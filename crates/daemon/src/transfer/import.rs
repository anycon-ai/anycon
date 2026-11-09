//! 导入：读 [`crate::transfer`] 格式的 zip，严格校验之后写进这台 daemon（docs/spec/daemon.md § 20）。
//!
//! 两层校验：
//! - **压缩包层**（任何一条不满足就整个拒绝，什么都不写）：manifest 的 `format` / `version`；条目名只认格式里定义的路径
//!   （拒绝 zip-slip、绝对路径、反斜杠、多余的文件）、不能是符号链接 / 加密 / 重复 / 重叠的条目；条目个数、单个条目和总的
//!   解压大小的上限；`session_count` 和实际的会话目录数一致。
//! - **会话层**（只让这个会话失败，别的照常导入，报告里列出原因）：`session.json` 的每个字段、`events.jsonl` 的每一行
//!   （能解析成 `SessionEvent`、会话 id 一致、seq 严格递增、数量对得上）、每个附件的大小和 SHA-256。
//!
//! 每个会话一个数据库事务（会话行、worktree 行、附件记录、事件、全文索引、turn 汇总），附件文件先落到 `blobs/tmp/` 下的
//! 临时目录，数据库都写成功之后才 rename 进 `blobs/`，最后提交；任何一步失败都回滚并删掉已经放好的文件，不留下半个会话。
//! 会话 id 已经存在就跳过，不覆盖。
//!
//! 导入的会话：没有 agent 进程；`agent_session_id` 和 `launch_args` 不收（`--resume` 在别的机器上必然失败，且 `launch_args`
//! 来自不可信的文件）；`project_loc` 为空（第一次拉起时按本机同名项目补记）；有 worktree 的会话的 worktree 记录标成 `missing`
//! （目录不存在，下一条消息按分支重建；导出时已经是 `removed` / `failed` 的保持原样）；导出时在跑的会话变成 `interrupted`，
//! 没有答复的权限请求在历史里收尾，和 daemon 崩溃之后重启恢复一样。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_protocol::{ErrorCode, Event, RpcError, RpcNotification, SessionEvent, SessionStatus, WorktreeState};
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use zip::{CompressionMethod, ZipArchive};

use super::export::cancel_on_disconnect;
use super::*;
use crate::clients::{ClientHandle, NotificationExt};
use crate::daemon::Ctx;
use crate::sessions::{Sessions, FTS_FLUSH_BYTES};
use crate::store::{BlobRow, ImportTx, SessionRow, Store, StoreError, WorktreeRow};
use crate::sys::OwnerOnly;
use crate::util::{new_worktree_id, now_ms};
use crate::workspace::Workspace;
use crate::worktree::err_reason;

const PROGRESS_EVERY: Duration = Duration::from_millis(150);
/// 一个会话最多这么多附件。
const MAX_BLOBS_PER_SESSION: usize = 100_000;

/// 为什么失败。
#[derive(Debug, thiserror::Error)]
enum Fail {
    /// 压缩包或会话的内容不合格。
    #[error("{0}")]
    Bad(String),
    /// manifest 的 `format` / `version` 不认识。
    #[error("{0}")]
    Unsupported(String),
    #[error("cancelled")]
    Cancelled,
    /// 写入时发现 id 已经被占用（检查之后、写入之前别的写入抢先了）：不是失败，当作跳过。
    #[error("already exists")]
    AlreadyThere,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Store(#[from] StoreError),
}

impl From<zip::result::ZipError> for Fail {
    fn from(e: zip::result::ZipError) -> Fail {
        Fail::Bad(format!("the file is not a readable zip archive: {e}"))
    }
}

impl From<serde_json::Error> for Fail {
    fn from(e: serde_json::Error) -> Fail {
        Fail::Bad(e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> Fail {
    Fail::Bad(msg.into())
}

/// 读压缩包条目时的 I/O 错误：数据本身不对（校验和不符、提前结束、deflate 流损坏）算压缩包的问题，其它是真的 I/O 错误。
fn read_err(name: &str, e: std::io::Error) -> Fail {
    use std::io::ErrorKind::{InvalidData, UnexpectedEof};
    if matches!(e.kind(), InvalidData | UnexpectedEof) {
        Fail::Bad(format!("{name}: {e}"))
    } else {
        Fail::Io(e)
    }
}

/// 压缩包里读出来的字符串（条目名、manifest 里的节点名、format）放进错误消息之前过一遍：只留 ASCII 可见字符和空格，
/// 其它换成 `?`，不把控制字符带进终端或日志。
fn printable(name: &str) -> String {
    name.chars().take(80).map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '?' }).collect()
}

impl From<Fail> for RpcError {
    /// 压缩包层的失败。
    fn from(f: Fail) -> RpcError {
        match f {
            Fail::Bad(m) => err_reason(ErrorCode::Invalid, m, "bad_archive"),
            Fail::Unsupported(m) => err_reason(ErrorCode::Invalid, m, "unsupported_version"),
            Fail::Cancelled | Fail::AlreadyThere => RpcError::new(ErrorCode::Internal, "cancelled"),
            Fail::Io(e) => RpcError::new(ErrorCode::Internal, format!("import failed: {e}")),
            Fail::Store(e) => RpcError::new(ErrorCode::Internal, format!("import failed: {e}")),
        }
    }
}

/// `local.import`。
pub async fn import(ctx: &Arc<Ctx>, client: &Arc<ClientHandle>, p: ImportParams) -> Result<ImportResult, RpcError> {
    let file = PathBuf::from(&p.file);
    if p.file.is_empty() || p.file.contains('\0') || !file.is_absolute() {
        return Err(RpcError::new(ErrorCode::Invalid, "file must be an absolute path"));
    }
    match std::fs::metadata(&file) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Err(RpcError::new(ErrorCode::Invalid, format!("{} is not a regular file", file.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(RpcError::new(ErrorCode::NotFound, format!("{} does not exist", file.display())))
        }
        Err(e) => return Err(RpcError::new(ErrorCode::Internal, format!("cannot read {}: {e}", file.display()))),
    }
    let Some(_gate) = ctx.transfer.begin() else {
        return Err(RpcError::new(ErrorCode::Busy, "another export or import is already running"));
    };
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (cancel, watcher) = cancel_on_disconnect(client);
    let progress: ProgressSink = {
        let client = client.clone();
        Arc::new(move |pr| {
            if let Some(n) = RpcNotification::new(PROGRESS_METHOD, pr) {
                client.notify(&n);
            }
        })
    };
    let env = Env {
        rt: Handle::current(),
        store: ctx.store.clone(),
        sessions: ctx.sessions.clone(),
        workspace: ctx.workspace.clone(),
        blobs_dir: ctx.blobs.dir().to_path_buf(),
        disk_reserve: ctx.blobs.disk_reserve(),
        limits: ctx.transfer.limits(),
        history_retention: ctx.config.get().history_retention(),
        dry_run: p.dry_run,
        cancel,
        progress,
    };
    let result = tokio::task::spawn_blocking(move || run(&env, &file)).await;
    watcher.abort();
    let result = match result {
        Ok(r) => r?,
        Err(e) => return Err(RpcError::new(ErrorCode::Internal, format!("import task failed: {e}"))),
    };
    tracing::info!(
        dry_run = result.dry_run,
        imported = result.imported.len(),
        skipped = result.skipped.len(),
        failed = result.failed.len(),
        events = result.events,
        blobs = result.blobs,
        "import finished"
    );
    if !p.dry_run {
        // 审计只记数量和文件名，不记内容
        ctx.audit
            .record(
                &client.caller(),
                "import",
                Some(&name),
                serde_json::json!({
                    "imported": result.imported.len(),
                    "skipped": result.skipped.len(),
                    "failed": result.failed.len(),
                    "events": result.events,
                    "blobs": result.blobs,
                }),
            )
            .await;
    }
    Ok(result)
}

struct Env {
    rt: Handle,
    store: Store,
    sessions: Arc<Sessions>,
    workspace: Arc<Workspace>,
    blobs_dir: PathBuf,
    disk_reserve: u64,
    limits: Limits,
    history_retention: Option<Duration>,
    dry_run: bool,
    cancel: Arc<AtomicBool>,
    progress: ProgressSink,
}

impl Env {
    fn check_cancel(&self) -> Result<(), Fail> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(Fail::Cancelled)
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- 压缩包层

struct SessionEntries {
    json: usize,
    events: usize,
    /// 文件名 → 条目序号。
    blobs: BTreeMap<String, usize>,
}

struct Archive {
    zip: ZipArchive<BufReader<File>>,
    manifest: Manifest,
    sessions: BTreeMap<String, SessionEntries>,
    /// 所有条目声明的解压后大小之和。
    total_bytes: u64,
}

/// 读一个条目的全部内容：声明的大小不能超过 `limit`，实际读到的必须恰好等于声明的大小。
fn read_entry(zip: &mut ZipArchive<BufReader<File>>, idx: usize, name: &str, limit: u64) -> Result<Vec<u8>, Fail> {
    let mut e = zip.by_index(idx)?;
    let declared = e.size();
    if declared > limit {
        return Err(bad(format!("{name} is larger than the limit ({limit} bytes)")));
    }
    let mut buf = Vec::with_capacity(declared as usize);
    (&mut e).take(declared + 1).read_to_end(&mut buf).map_err(|er| read_err(name, er))?;
    if buf.len() as u64 != declared {
        return Err(bad(format!("{name}: its content does not match the size in the archive")));
    }
    Ok(buf)
}

fn open_archive(file: &Path, limits: &Limits) -> Result<Archive, Fail> {
    let f = File::open(file)?;
    let mut zip = ZipArchive::new(BufReader::with_capacity(256 * 1024, f))?;
    if zip.len() > limits.max_entries {
        return Err(bad(format!("the archive has too many entries ({} at most)", limits.max_entries)));
    }
    if zip.has_duplicate_names() {
        return Err(bad("the archive has two entries with the same name"));
    }
    // manifest 先看：版本不认识时，条目的结构本来就可能不一样，报「版本不支持」比报「多出的文件」有用
    let Some(mi) = zip.index_for_name(MANIFEST_NAME) else {
        return Err(bad("this is not an anycon export: manifest.json is missing"));
    };
    let manifest: Manifest =
        serde_json::from_slice(&read_entry(&mut zip, mi, MANIFEST_NAME, limits.max_manifest_bytes)?)
            .map_err(|e| bad(format!("manifest.json: {e}")))?;
    if manifest.format != FORMAT {
        return Err(bad(format!("this is not an anycon export (format {:?})", printable(&manifest.format))));
    }
    if manifest.version != FORMAT_VERSION {
        return Err(Fail::Unsupported(format!(
            "this export has format version {}, this anycon reads version {FORMAT_VERSION}",
            manifest.version
        )));
    }
    if zip.has_overlapping_files()? {
        return Err(bad("entries in the archive overlap"));
    }
    let mut sessions: BTreeMap<String, SessionEntries> = BTreeMap::new();
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let e = zip.by_index_raw(i)?;
        let name = e.name().map_err(|_| bad("an entry name is not valid UTF-8"))?.into_owned();
        let Some(kind) = parse_entry_name(&name) else {
            return Err(bad(format!(
                "unexpected entry {:?} (it is not a path of the export format)",
                printable(&name)
            )));
        };
        if e.encrypted() {
            return Err(bad(format!("{}: encrypted entries are not supported", printable(&name))));
        }
        if !matches!(e.compression(), CompressionMethod::Stored | CompressionMethod::Deflated) {
            return Err(bad(format!("{}: unsupported compression method", printable(&name))));
        }
        // 符号链接、设备、管道、套接字都不是普通文件
        if let Some(mode) = e.unix_mode() {
            let ty = mode & 0o170000;
            if e.is_symlink() || !matches!(ty, 0 | 0o100000 | 0o040000) {
                return Err(bad(format!("{}: not a regular file (symbolic link or special file)", printable(&name))));
            }
        }
        if e.is_dir() != matches!(kind, Entry::Dir) || (e.is_dir() && e.size() != 0) {
            return Err(bad(format!("{}: a file where a directory belongs, or the other way round", printable(&name))));
        }
        let size = e.size();
        let limit = match &kind {
            Entry::Manifest => limits.max_manifest_bytes,
            Entry::SessionJson(_) => limits.max_session_json_bytes,
            Entry::Events(_) => limits.max_events_bytes,
            Entry::Blob(..) => limits.max_blob_bytes,
            Entry::Dir => 0,
        };
        if size > limit {
            return Err(bad(format!("{} is larger than the limit ({limit} bytes)", printable(&name))));
        }
        total = total.saturating_add(size);
        if total > limits.max_total_bytes {
            return Err(bad(format!(
                "the archive expands to more than {} bytes; refusing to unpack it",
                limits.max_total_bytes
            )));
        }
        match kind {
            Entry::Manifest | Entry::Dir => {}
            Entry::SessionJson(sid) => {
                sessions
                    .entry(sid.to_string())
                    .or_insert_with(|| SessionEntries { json: usize::MAX, events: usize::MAX, blobs: BTreeMap::new() })
                    .json = i;
            }
            Entry::Events(sid) => {
                sessions
                    .entry(sid.to_string())
                    .or_insert_with(|| SessionEntries { json: usize::MAX, events: usize::MAX, blobs: BTreeMap::new() })
                    .events = i;
            }
            Entry::Blob(sid, file) => {
                sessions
                    .entry(sid.to_string())
                    .or_insert_with(|| SessionEntries { json: usize::MAX, events: usize::MAX, blobs: BTreeMap::new() })
                    .blobs
                    .insert(file.to_string(), i);
            }
        }
    }
    for (sid, s) in &sessions {
        if s.json == usize::MAX || s.events == usize::MAX {
            return Err(bad(format!("session {sid}: session.json or events.jsonl is missing")));
        }
    }
    if manifest.session_count != sessions.len() as u64 {
        return Err(bad(format!(
            "manifest.json says {} sessions but the archive holds {} (truncated or modified?)",
            manifest.session_count,
            sessions.len()
        )));
    }
    Ok(Archive { zip, manifest, sessions, total_bytes: total })
}

// ---------------------------------------------------------------- 主流程

fn run(env: &Env, file: &Path) -> Result<ImportResult, RpcError> {
    let mut archive = open_archive(file, &env.limits)?;
    if !env.dry_run {
        if let Some(free) = crate::sys::disk_free(&env.blobs_dir) {
            let need = archive.total_bytes.saturating_add(env.disk_reserve);
            if free < need {
                return Err(err_reason(
                    ErrorCode::Busy,
                    format!(
                        "not enough free disk space: the archive expands to {} bytes and {} must stay free, {free} are available",
                        archive.total_bytes, env.disk_reserve
                    ),
                    "disk_full",
                ));
            }
        }
    }
    let staging = if env.dry_run { None } else { Some(Staging::create(&env.blobs_dir).map_err(Fail::Io)?) };
    let total = archive.sessions.len() as u64;
    let ids: Vec<String> = archive.sessions.keys().cloned().collect();
    let mut result = ImportResult {
        dry_run: env.dry_run,
        total,
        imported: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
        events: 0,
        blobs: 0,
        source_node: printable(&archive.manifest.node_name),
        source_version: printable(&archive.manifest.anycon_version),
        exported_at_ms: archive.manifest.exported_at_ms,
        warnings: Vec::new(),
    };
    let mut missing_projects: BTreeSet<String> = BTreeSet::new();
    let mut expired = 0u64;
    let mut last_progress = Instant::now();
    let mut bytes = 0u64;
    for (i, id) in ids.iter().enumerate() {
        env.check_cancel().map_err(RpcError::from)?;
        let entries = archive.sessions.remove(id).expect("listed above");
        match import_session(env, &mut archive.zip, staging.as_ref(), id, &entries, &mut bytes) {
            Ok(Outcome::Imported(done)) => {
                result.events += done.events;
                result.blobs += done.blobs;
                if let Some(p) = &done.project {
                    if env.workspace.project_path(p).is_err() {
                        missing_projects.insert(printable(p));
                    }
                }
                if let Some(ret) = env.history_retention {
                    if done.updated_ms < now_ms().saturating_sub(ret.as_millis() as u64) {
                        expired += 1;
                    }
                }
                result.imported.push(id.clone());
            }
            Ok(Outcome::Skipped) => {
                result.skipped.push(ImportSkipped { id: id.clone(), reason: "already_exists".into() })
            }
            Err(Fail::Cancelled) => return Err(Fail::Cancelled.into()),
            Err(f @ (Fail::Bad(_) | Fail::Unsupported(_))) => {
                result.failed.push(ImportFailed { id: id.clone(), error: f.to_string() })
            }
            Err(f) => {
                tracing::warn!(session = %id, error = %f, "cannot import a session");
                result.failed.push(ImportFailed { id: id.clone(), error: f.to_string() })
            }
        }
        if i as u64 + 1 == total || last_progress.elapsed() >= PROGRESS_EVERY {
            last_progress = Instant::now();
            (env.progress)(Progress { op: "import".into(), done: i as u64 + 1, total, bytes });
        }
    }
    if !missing_projects.is_empty() {
        result.warnings.push(format!(
            "projects that do not exist on this node: {}; their sessions are imported and can be read, \
             create or link a project with the same name to continue them",
            missing_projects.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    if expired > 0 && !env.dry_run {
        result.warnings.push(format!(
            "sessions.history_retention is set and {expired} of the imported sessions are older than it; \
             the next cleanup deletes them"
        ));
    }
    Ok(result)
}

/// 导入期间附件的临时目录（`blobs/tmp/import-<随机>/`，0700）。`blobs/tmp/` 在 daemon 启动时会清空，崩溃留下的不会一直在。
struct Staging {
    dir: PathBuf,
}

impl Staging {
    fn create(blobs_dir: &Path) -> std::io::Result<Staging> {
        let dir = blobs_dir.join("tmp").join(format!("import-{:016x}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir)?;
        crate::sys::restrict_to_owner(&dir)?;
        Ok(Staging { dir })
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

enum Outcome {
    Imported(Done),
    Skipped,
}

struct Done {
    events: u64,
    blobs: u64,
    project: Option<String>,
    updated_ms: u64,
}

// ---------------------------------------------------------------- 会话层

fn import_session(
    env: &Env,
    zip: &mut ZipArchive<BufReader<File>>,
    staging: Option<&Staging>,
    id: &str,
    entries: &SessionEntries,
    bytes: &mut u64,
) -> Result<Outcome, Fail> {
    let lim = &env.limits;
    let raw = read_entry(zip, entries.json, &session_json_name(id), lim.max_session_json_bytes)?;
    let file: SessionFile = serde_json::from_slice(&raw).map_err(|e| bad(format!("{}: {e}", session_json_name(id))))?;
    validate_session(&file, id, lim).map_err(|m| bad(format!("{}: {m}", session_json_name(id))))?;
    // session.json 列出的附件和 blobs/ 下的文件必须一一对应
    let listed: BTreeSet<&str> = file.blobs.iter().map(|b| b.file.as_str()).collect();
    let present: BTreeSet<&str> = entries.blobs.keys().map(String::as_str).collect();
    if listed != present {
        return Err(bad("the attachment files in blobs/ do not match the list in session.json"));
    }
    if env.rt.block_on(env.store.session_exists(id))? {
        return Ok(Outcome::Skipped);
    }
    env.check_cancel()?;

    // 附件：校验大小和 SHA-256；导入时顺便落到临时目录
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new(); // (临时文件, 正式文件)
    let cleanup_staged = |staged: &[(PathBuf, PathBuf)]| {
        for (tmp, _) in staged {
            let _ = std::fs::remove_file(tmp);
        }
    };
    for b in &file.blobs {
        let idx = entries.blobs[&b.file];
        let name = blob_entry_name(id, &b.file);
        let e = zip.by_index(idx)?;
        if e.size() != b.bytes {
            return Err(bad(format!("{}: the size in the archive differs from session.json", printable(&name))));
        }
        let res = match staging {
            Some(s) => {
                let tmp = s.dir.join(&b.file);
                let r = copy_to_file(e, &tmp, b, env, &name, bytes);
                if r.is_ok() {
                    staged.push((tmp, env.blobs_dir.join(&b.file)));
                } else {
                    let _ = std::fs::remove_file(&tmp);
                }
                r
            }
            None => copy_hashed(e, &mut std::io::sink(), b, env, &name, bytes),
        };
        if let Err(f) = res {
            cleanup_staged(&staged);
            return Err(f);
        }
    }

    let result = if env.dry_run {
        scan_events(env, zip, entries.events, id, &file, |_| Ok(()), bytes).map(|s| Done {
            events: s.count,
            blobs: file.blobs.len() as u64,
            project: file.project.clone(),
            updated_ms: file.updated_ms,
        })
    } else {
        apply(env, zip, entries, id, &file, &staged, bytes)
    };
    match result {
        Ok(done) => {
            if !env.dry_run {
                if let Err(e) = env.rt.block_on(env.sessions.adopt_imported(id)) {
                    tracing::warn!(session = %id, error = %e.message, "an imported session was written but could not be loaded");
                }
            }
            Ok(Outcome::Imported(done))
        }
        Err(Fail::AlreadyThere) => {
            cleanup_staged(&staged);
            Ok(Outcome::Skipped)
        }
        Err(f) => {
            cleanup_staged(&staged);
            Err(f)
        }
    }
}

/// 写数据库和放附件文件。成功时事务已经提交。
fn apply(
    env: &Env,
    zip: &mut ZipArchive<BufReader<File>>,
    entries: &SessionEntries,
    id: &str,
    file: &SessionFile,
    staged: &[(PathBuf, PathBuf)],
    bytes: &mut u64,
) -> Result<Done, Fail> {
    let rt = &env.rt;
    let in_flight = matches!(
        file.status,
        SessionStatus::Queued | SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingPermission
    );
    let final_status = if in_flight { SessionStatus::Interrupted } else { file.status };
    let project = file.project.as_ref().map(|p| env.workspace.canonical_project_name(p));
    let wt_row = file.worktree.as_ref().map(|w| worktree_row(w, project.as_deref().unwrap_or_default(), id));
    let row = SessionRow {
        id: id.to_string(),
        project: project.clone(),
        project_loc: None,
        agent: file.agent.clone(),
        title: file.title.clone(),
        title_locked: file.title_locked,
        status: final_status,
        archived: file.archived,
        settings: file.settings.clone(),
        agent_session_id: None,
        launch_args: serde_json::Value::Null,
        created_ms: file.created_ms,
        updated_ms: file.updated_ms,
        last_seq: 0,
        next_turn_id: 1,
        usage: file.usage.clone(),
        preview: file.preview.clone(),
        worktree_id: wt_row.as_ref().map(|w| w.id.clone()),
    };
    let mut tx = rt.block_on(env.store.begin_import())?;
    let body = (|| -> Result<Done, Fail> {
        // 第一条语句：拿写锁，同时检测 id 冲突
        if !rt.block_on(tx.insert_session(&row))? {
            return Err(Fail::AlreadyThere);
        }
        if let Some(w) = &wt_row {
            rt.block_on(tx.insert_worktree(w))?;
        }
        for b in &file.blobs {
            let blob = BlobRow {
                id: b.blob_id.clone(),
                session_id: Some(id.to_string()),
                name: b.name.clone(),
                mime: b.mime.clone(),
                size: b.bytes,
                sha256: b.sha256.clone(),
                width: b.width,
                height: b.height,
                path: b.file.clone(),
                created_ms: b.created_ms,
            };
            if !rt.block_on(tx.insert_blob(&blob))? {
                return Err(bad(format!("attachment {} already exists on this node under another session", b.blob_id)));
            }
        }
        let mut st = Derived::default();
        let summary = scan_events(
            env,
            zip,
            entries.events,
            id,
            file,
            |ev| {
                rt.block_on(st.apply(&mut tx, ev))?;
                Ok(())
            },
            bytes,
        )?;
        // 尾部：和 daemon 崩溃重启后的恢复一样——没有答复的权限请求在历史里收尾，在跑的会话记一条 interrupted
        let now = now_ms();
        let mut seq = summary.last_seq;
        let mut tail: Vec<Event> = st
            .open_permissions
            .iter()
            .map(|rid| Event::PermissionResolved {
                request_id: rid.clone(),
                option_id: None,
                by_device: "daemon".into(),
                answers: None,
                message: None,
            })
            .collect();
        if in_flight {
            tail.push(Event::Status { state: SessionStatus::Interrupted, position: None, queued_messages: None });
        }
        rt.block_on(st.flush_events(&mut tx))?;
        let tail: Vec<SessionEvent> = tail
            .into_iter()
            .map(|event| {
                seq += 1;
                SessionEvent { session_id: id.to_string(), seq, ts_ms: now, turn_id: None, event }
            })
            .collect();
        rt.block_on(tx.insert_events(&tail))?;
        rt.block_on(st.finish(&mut tx, id, now))?;
        let next_turn_id = file.next_turn_id.max(st.max_turn_id + 1).max(1);
        rt.block_on(tx.finish_session(id, seq, next_turn_id, final_status))?;
        Ok(Done { events: summary.count, blobs: file.blobs.len() as u64, project, updated_ms: file.updated_ms })
    })();
    let done = match body {
        Ok(d) => d,
        Err(e) => {
            let _ = rt.block_on(tx.rollback());
            return Err(e);
        }
    };
    // 数据库都写成功了：附件放到正式位置。放不下就撤回已经放的、回滚
    let mut placed: Vec<&Path> = Vec::new();
    for (tmp, dest) in staged {
        if let Err(e) = std::fs::rename(tmp, dest) {
            for p in &placed {
                let _ = std::fs::remove_file(p);
            }
            let _ = rt.block_on(tx.rollback());
            return Err(Fail::Io(e));
        }
        placed.push(dest);
    }
    if let Err(e) = rt.block_on(tx.commit()) {
        for p in &placed {
            let _ = std::fs::remove_file(p);
        }
        return Err(e.into());
    }
    Ok(done)
}

/// 一次插进数据库的事件数。
const EVENT_BATCH: usize = 256;

/// 事件流边读边得出的东西：全文索引的缓冲、turn 的最大编号、还没答复的权限请求；事件自己攒成批再写。
#[derive(Default)]
struct Derived {
    fts: Fts,
    max_turn_id: u64,
    /// 按出现顺序。
    open_permissions: Vec<String>,
    batch: Vec<SessionEvent>,
}

impl Derived {
    async fn apply(&mut self, tx: &mut ImportTx, ev: SessionEvent) -> Result<(), Fail> {
        for (seq, role, ts, text) in self.fts.on_event(&ev) {
            tx.index_text(&ev.session_id, seq, role, ts, &text).await?;
        }
        match &ev.event {
            Event::TurnStart { turn_id, message_id } => {
                self.max_turn_id = self.max_turn_id.max(*turn_id);
                tx.turn_start(&ev.session_id, *turn_id, message_id, ev.ts_ms).await?;
            }
            Event::TurnDone { turn_id, reason, usage } => {
                self.max_turn_id = self.max_turn_id.max(*turn_id);
                let reason = serde_json::to_value(reason)?;
                let usage = usage.as_ref().map(serde_json::to_value).transpose()?;
                tx.turn_end(&ev.session_id, *turn_id, ev.ts_ms, reason.as_str().unwrap_or("error"), usage.as_ref())
                    .await?;
            }
            Event::Permission { request_id, .. } => self.open_permissions.push(request_id.clone()),
            Event::PermissionResolved { request_id, .. } => self.open_permissions.retain(|r| r != request_id),
            _ => {}
        }
        self.batch.push(ev);
        if self.batch.len() >= EVENT_BATCH {
            tx.insert_events(&self.batch).await?;
            self.batch.clear();
        }
        Ok(())
    }

    /// 还没写的事件写进去。
    async fn flush_events(&mut self, tx: &mut ImportTx) -> Result<(), Fail> {
        tx.insert_events(&self.batch).await?;
        self.batch.clear();
        Ok(())
    }

    /// 事件流读完：缓冲里剩的 assistant 文字收进索引，没结束的 turn 标 `error`。
    async fn finish(&mut self, tx: &mut ImportTx, id: &str, now: u64) -> Result<(), Fail> {
        if let Some((seq, role, ts, text)) = self.fts.flush(now) {
            tx.index_text(id, seq, role, ts, &text).await?;
        }
        tx.close_open_turns(id, now).await?;
        Ok(())
    }
}

/// 全文索引的行，和 daemon 平时写的一样（`Sessions::on_event` / `flush_text`）：用户消息一条一行；assistant 的文字按
/// `text_delta` 累积，在 `text_done`、`turn_done` 或累积到 [`FTS_FLUSH_BYTES`] 时收成一行，seq 是第一个 delta 的 seq。
#[derive(Default)]
struct Fts {
    buf: String,
    seq: u64,
}

type FtsRow = (u64, &'static str, u64, String);

impl Fts {
    fn on_event(&mut self, ev: &SessionEvent) -> Vec<FtsRow> {
        let mut out = Vec::new();
        match &ev.event {
            Event::UserMessage { content, .. } => out.push((ev.seq, "user", ev.ts_ms, content.clone())),
            Event::TextDelta { text } => {
                if self.buf.is_empty() {
                    self.seq = ev.seq;
                }
                self.buf.push_str(text);
                if self.buf.len() >= FTS_FLUSH_BYTES {
                    out.extend(self.flush(ev.ts_ms));
                }
            }
            Event::TextDone {} | Event::TurnDone { .. } => out.extend(self.flush(ev.ts_ms)),
            _ => {}
        }
        out
    }

    fn flush(&mut self, ts: u64) -> Option<FtsRow> {
        (!self.buf.is_empty()).then(|| (self.seq, "assistant", ts, std::mem::take(&mut self.buf)))
    }
}

struct EventsSummary {
    count: u64,
    last_seq: u64,
}

/// 逐行读 `events.jsonl`，每个事件先校验再交给 `sink`。读完核对行数和 `last_seq`。
fn scan_events(
    env: &Env,
    zip: &mut ZipArchive<BufReader<File>>,
    idx: usize,
    id: &str,
    file: &SessionFile,
    mut sink: impl FnMut(SessionEvent) -> Result<(), Fail>,
    bytes: &mut u64,
) -> Result<EventsSummary, Fail> {
    let name = events_name(id);
    let e = zip.by_index(idx)?;
    let declared = e.size();
    let max_line = env.limits.max_event_line_bytes;
    let mut reader = BufReader::with_capacity(256 * 1024, e.take(declared + 1));
    let mut line = Vec::new();
    let (mut total, mut count, mut last_seq, mut line_no) = (0u64, 0u64, 0u64, 0u64);
    loop {
        line.clear();
        let n = (&mut reader).take(max_line as u64 + 1).read_until(b'\n', &mut line).map_err(|e| read_err(&name, e))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        total += n as u64;
        *bytes += n as u64;
        if total > declared {
            return Err(bad(format!("{name}: its content does not match the size in the archive")));
        }
        if line.len() > max_line {
            return Err(bad(format!("{name} line {line_no}: longer than {max_line} bytes")));
        }
        let text = line.trim_ascii();
        if text.is_empty() {
            continue;
        }
        let ev: SessionEvent =
            serde_json::from_slice(text).map_err(|e| bad(format!("{name} line {line_no}: not a valid event: {e}")))?;
        if ev.session_id != id {
            return Err(bad(format!("{name} line {line_no}: the event belongs to another session")));
        }
        if ev.seq == 0 || ev.seq <= last_seq || ev.seq > MAX_SEQ {
            return Err(bad(format!("{name} line {line_no}: seq {} is not increasing", ev.seq)));
        }
        if ev.ts_ms > MAX_TS_MS {
            return Err(bad(format!("{name} line {line_no}: not a plausible time")));
        }
        last_seq = ev.seq;
        count += 1;
        sink(ev)?;
        if count % 256 == 0 {
            env.check_cancel()?;
        }
    }
    if total != declared {
        return Err(bad(format!("{name}: its content does not match the size in the archive")));
    }
    if count != file.event_count || last_seq != file.last_seq {
        return Err(bad(format!(
            "{name}: holds {count} events up to seq {last_seq}, session.json says {} up to seq {}",
            file.event_count, file.last_seq
        )));
    }
    Ok(EventsSummary { count, last_seq })
}

/// 附件内容拷到临时文件，边拷边核对大小和 SHA-256。
fn copy_to_file(e: impl Read, dest: &Path, b: &BlobFile, env: &Env, name: &str, bytes: &mut u64) -> Result<(), Fail> {
    let mut oo = OpenOptions::new();
    oo.write(true).create_new(true).owner_only();
    let mut out = oo.open(dest)?;
    copy_hashed(e, &mut out, b, env, name, bytes)?;
    out.flush()?;
    out.sync_all()?;
    Ok(())
}

fn copy_hashed(
    e: impl Read,
    out: &mut impl Write,
    b: &BlobFile,
    env: &Env,
    name: &str,
    bytes: &mut u64,
) -> Result<(), Fail> {
    let mut r = e.take(b.bytes + 1);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf).map_err(|er| read_err(name, er))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > b.bytes {
            return Err(bad(format!("{}: its content does not match the size in the archive", printable(name))));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        *bytes += n as u64;
        env.check_cancel()?;
    }
    if total != b.bytes {
        return Err(bad(format!("{}: its content does not match the size in the archive", printable(name))));
    }
    if hex::encode(hasher.finalize()) != b.sha256 {
        return Err(bad(format!("{}: SHA-256 does not match session.json", printable(name))));
    }
    Ok(())
}

// ---------------------------------------------------------------- 字段校验

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `image/png`、`application/pdf`、`text/plain` 这样的：两段都是 token 字符。不带参数。
fn is_mime(s: &str) -> bool {
    let tok = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b));
    s.len() <= 255 && s.split_once('/').is_some_and(|(a, b)| tok(a) && tok(b))
}

/// git 引用名的保守子集：`[A-Za-z0-9._/-]`，不以 `-` `.` `/` 开头，没有 `..` `//` `/.`，不以 `/` `.` `.lock` 结尾。
/// 它们之后会被拼进 `git worktree add -b <分支> <路径> <base>`，所以只收肯定不会被当成选项、也不含特殊语法的写法。
fn is_safe_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        && !s.starts_with(['-', '.', '/'])
        && !s.ends_with(['/', '.'])
        && !s.ends_with(".lock")
        && !s.contains("..")
        && !s.contains("//")
        && !s.contains("/.")
}

fn validate_worktree(w: &WorktreeFile, has_project: bool) -> Result<(), String> {
    if !has_project {
        return Err("worktree: a session without a project cannot have a worktree".into());
    }
    let seg = w.rel_path.strip_prefix(".anycon/worktrees/").unwrap_or("");
    if seg.is_empty()
        || seg.len() > 200
        || !seg.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || seg.bytes().all(|b| b == b'.')
    {
        return Err("worktree.rel_path must be .anycon/worktrees/<name>".into());
    }
    if !is_safe_ref(&w.branch) {
        return Err("worktree.branch is not a plain branch name".into());
    }
    if !(is_safe_ref(&w.base) || crate::worktree::is_hex_oid(&w.base)) {
        return Err("worktree.base is not a plain ref name or commit".into());
    }
    if !crate::worktree::is_hex_oid(&w.base_sha) {
        return Err("worktree.base_sha is not a commit id".into());
    }
    if w.base_display.len() > 255 || w.base_display.chars().any(char::is_control) {
        return Err("worktree.base_display is not a plain name".into());
    }
    if w.created_ms > MAX_TS_MS {
        return Err("worktree.created_ms is not a plausible time".into());
    }
    Ok(())
}

fn validate_session(f: &SessionFile, dir_id: &str, limits: &Limits) -> Result<(), String> {
    if f.id != dir_id {
        return Err("id does not match the directory".into());
    }
    if !(1..=64).contains(&f.agent.len()) || !f.agent.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
    {
        return Err("agent is not a plain agent id".into());
    }
    if f.title.len() > 4096 || f.preview.as_ref().is_some_and(|p| p.len() > 4096) {
        return Err("title or preview is too long".into());
    }
    if f.agent_session_id.as_ref().is_some_and(|a| a.len() > 1024) {
        return Err("agent_session_id is too long".into());
    }
    if let Some(p) = &f.project {
        crate::workspace::validate_name(p).map_err(|e| format!("project: {}", e.message))?;
    }
    for (what, v) in [("created_ms", f.created_ms), ("updated_ms", f.updated_ms)] {
        if v > MAX_TS_MS {
            return Err(format!("{what} is not a plausible time"));
        }
    }
    if f.last_seq > MAX_SEQ || f.event_count > MAX_SEQ || f.next_turn_id > MAX_SEQ {
        return Err("last_seq, event_count or next_turn_id is out of range".into());
    }
    if serde_json::to_vec(&f.settings).map_or(true, |s| s.len() > 64 * 1024) {
        return Err("settings is too large".into());
    }
    if let Some(w) = &f.worktree {
        validate_worktree(w, f.project.is_some())?;
    }
    if f.blobs.len() > MAX_BLOBS_PER_SESSION {
        return Err("too many attachments".into());
    }
    let (mut ids, mut files) = (HashSet::new(), HashSet::new());
    for b in &f.blobs {
        if !matches!(parse_blob_file(&b.file), Some((id, _)) if id == b.blob_id) {
            return Err(format!("attachment {:?}: file must be <blob_id>.<extension>", printable(&b.blob_id)));
        }
        if !ids.insert(b.blob_id.as_str()) || !files.insert(b.file.as_str()) {
            return Err(format!("attachment {} is listed twice", b.blob_id));
        }
        if b.name.len() > 255 || b.name.chars().any(crate::blobs::is_forbidden_in_name) {
            return Err(format!("attachment {}: name is not a plain file name", b.blob_id));
        }
        if !is_mime(&b.mime) {
            return Err(format!("attachment {}: mime is not a media type", b.blob_id));
        }
        if !is_sha256(&b.sha256) {
            return Err(format!("attachment {}: sha256 is not 64 lowercase hex digits", b.blob_id));
        }
        if b.bytes > limits.max_blob_bytes || b.created_ms > MAX_TS_MS {
            return Err(format!("attachment {}: size or time out of range", b.blob_id));
        }
        if b.width.is_some_and(|w| w > 1_000_000) || b.height.is_some_and(|h| h > 1_000_000) {
            return Err(format!("attachment {}: width or height out of range", b.blob_id));
        }
    }
    Ok(())
}

/// worktree 记录：目录不存在（`missing`，下一条消息按分支重建）；导出时已经是 `removed`（归档时删掉的）/ `failed`
/// 的保持原样。设备号 / inode、`.git` 指针、项目位置都不收：对这台机器没有意义，空着就不会把别的目录认成它的。
fn worktree_row(w: &WorktreeFile, project: &str, session_id: &str) -> WorktreeRow {
    let state = match w.state {
        WorktreeState::Removed | WorktreeState::Failed => w.state,
        _ => WorktreeState::Missing,
    };
    WorktreeRow {
        id: new_worktree_id(),
        project_loc: None,
        project: project.to_string(),
        rel_path: w.rel_path.clone(),
        branch: w.branch.clone(),
        base: w.base.clone(),
        base_display: w.base_display.clone(),
        base_sha: w.base_sha.clone(),
        session_id: Some(session_id.to_string()),
        state,
        identity: None,
        git_pointer: None,
        created_ms: w.created_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, event: Event) -> SessionEvent {
        SessionEvent { session_id: "s".into(), seq, ts_ms: 1000 + seq, turn_id: None, event }
    }

    fn user(seq: u64, content: &str) -> SessionEvent {
        ev(
            seq,
            Event::UserMessage {
                message_id: format!("m{seq}"),
                content: content.into(),
                by_device: "d".into(),
                attachments: vec![],
            },
        )
    }

    fn delta(seq: u64, text: &str) -> SessionEvent {
        ev(seq, Event::TextDelta { text: text.into() })
    }

    /// 和 `Sessions::on_event` / `flush_text` 写的行一致：用户消息一条一行，assistant 文字在 text_done / turn_done / 够大时收成一行，
    /// seq 是第一个 delta 的 seq。
    #[test]
    fn fts_rows_follow_what_the_daemon_indexes() {
        let mut f = Fts::default();
        assert_eq!(f.on_event(&user(1, "hello zebra")), vec![(1, "user", 1001, "hello zebra".to_string())]);
        assert!(f.on_event(&delta(2, "Hel")).is_empty());
        assert!(f.on_event(&delta(3, "lo")).is_empty());
        assert_eq!(f.on_event(&ev(4, Event::TextDone {})), vec![(2, "assistant", 1004, "Hello".to_string())]);
        // text_done 已经收过了：turn_done 不重复
        assert!(f
            .on_event(&ev(
                5,
                Event::TurnDone { turn_id: 1, reason: anycon_protocol::TurnDoneReason::Completed, usage: None }
            ))
            .is_empty());
        // 没有 text_done 的文字在 turn_done 时收
        f.on_event(&delta(6, "tail"));
        let rows = f.on_event(&ev(
            7,
            Event::TurnDone { turn_id: 2, reason: anycon_protocol::TurnDoneReason::Cancelled, usage: None },
        ));
        assert_eq!(rows, vec![(6, "assistant", 1007, "tail".to_string())]);
        // 累积到阈值就收，下一段从新的 seq 开始
        let big = "x".repeat(FTS_FLUSH_BYTES);
        let rows = f.on_event(&delta(8, &big));
        assert_eq!((rows.len(), rows[0].0), (1, 8));
        f.on_event(&delta(9, "more"));
        assert_eq!(f.flush(2000), Some((9, "assistant", 2000, "more".to_string())));
        assert_eq!(f.flush(2000), None);
        // 只有图片的用户消息（空文字）也交给 index_text，由它丢掉空白
        assert_eq!(f.on_event(&user(10, "")).len(), 1);
    }

    #[test]
    fn refs_are_a_plain_subset_of_what_git_allows() {
        for ok in
            ["main", "anycon/fix-login", "origin/main", "refs/heads/main", "refs/tags/v1.0", "a.b/c_d-e", "release-1.2"]
        {
            assert!(is_safe_ref(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            "--upload-pack=x",
            ".hidden",
            "/abs",
            "a..b",
            "a//b",
            "a/.b",
            "a/",
            "a.",
            "x.lock",
            "has space",
            "a:b",
            "a~1",
            "a^",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            "a@{u}",
            "é",
            "a\nb",
        ] {
            assert!(!is_safe_ref(bad), "{bad:?}");
        }
        assert!(!is_safe_ref(&"a".repeat(256)));
    }

    fn wt(rel: &str, branch: &str, base: &str, sha: &str) -> WorktreeFile {
        WorktreeFile {
            branch: branch.into(),
            base: base.into(),
            base_display: "main".into(),
            base_sha: sha.into(),
            rel_path: rel.into(),
            state: WorktreeState::Active,
            created_ms: 1,
        }
    }

    #[test]
    fn worktree_records_cannot_point_outside_the_anycon_directory() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let ok = wt(".anycon/worktrees/fix-login", "anycon/fix-login", "refs/heads/main", sha);
        assert_eq!(validate_worktree(&ok, true), Ok(()));
        assert!(validate_worktree(&ok, false).is_err(), "a scratch session has no worktree");
        for rel in [
            "../../etc",
            ".anycon/worktrees/../../x",
            ".anycon/worktrees/..",
            ".anycon/worktrees/.",
            ".anycon/worktrees/a/b",
            ".anycon/worktrees/",
            "/abs/path",
            ".anycon/worktrees/a b",
            "worktrees/x",
        ] {
            assert!(validate_worktree(&WorktreeFile { rel_path: rel.into(), ..ok.clone() }, true).is_err(), "{rel}");
        }
        assert!(validate_worktree(&WorktreeFile { branch: "-D".into(), ..ok.clone() }, true).is_err());
        assert!(validate_worktree(&WorktreeFile { base: "--output=x".into(), ..ok.clone() }, true).is_err());
        assert!(validate_worktree(&WorktreeFile { base_sha: "main".into(), ..ok.clone() }, true).is_err());
        assert!(
            validate_worktree(&WorktreeFile { base: sha.into(), ..ok }, true).is_ok(),
            "a commit id is a fine base"
        );
    }

    #[test]
    fn media_types_are_checked_by_shape_not_by_a_list_of_images() {
        for ok in ["image/png", "application/pdf", "text/plain", "application/vnd.api+json", "x-world/x-vrml"] {
            assert!(is_mime(ok), "{ok}");
        }
        for bad in ["", "png", "image/", "/png", "image/png; charset=x", "a b/c", "a/b/c", "image/\npng"] {
            assert!(!is_mime(bad), "{bad:?}");
        }
    }
}
