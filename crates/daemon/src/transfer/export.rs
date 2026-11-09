//! 导出：把选中的会话写成一个 zip（格式见 [`crate::transfer`] 和 docs/spec/daemon.md § 20）。
//!
//! 整个写入在一个阻塞线程里跑（zip 和文件 I/O 是同步的），数据库读取用 `Handle::block_on`。每个会话的行、worktree、
//! 附件记录和事件取自同一个只读快照事务（[`crate::store::SnapshotTx`]），所以正在跑的会话也能导出一致的一份，
//! 不会挡住它写事件。输出先写到同目录的临时文件（0600），成功后 rename，失败 / 取消时删掉。

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_protocol::{ErrorCode, RpcError, RpcNotification};
use sha2::{Digest, Sha256};
use tokio::runtime::Handle;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use super::*;
use crate::blobs::Blobs;
use crate::clients::{ClientHandle, NotificationExt};
use crate::daemon::Ctx;
use crate::store::{BlobRow, SessionRow, Store, StoreError, WorktreeRow};
use crate::sys::OwnerOnly;
use crate::util::{civil_from_ms, now_ms};
use crate::worktree::err_reason;

/// 一页读多少个事件。
const EVENT_PAGE: u32 = 200;
/// 进度通知的最小间隔。
const PROGRESS_EVERY: Duration = Duration::from_millis(150);
/// 超过它的条目要写 zip64 头。
const ZIP64_THRESHOLD: u64 = 3 * 1024 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
enum ExportError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("database: {0}")]
    Store(#[from] StoreError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cancelled")]
    Cancelled,
}

impl From<ExportError> for RpcError {
    fn from(e: ExportError) -> RpcError {
        RpcError::new(ErrorCode::Internal, format!("export failed: {e}"))
    }
}

fn invalid(msg: impl Into<String>) -> RpcError {
    RpcError::new(ErrorCode::Invalid, msg)
}

/// `local.export`。
pub async fn export(ctx: &Arc<Ctx>, client: &Arc<ClientHandle>, p: ExportParams) -> Result<ExportResult, RpcError> {
    let out = PathBuf::from(&p.out);
    if p.out.is_empty() || p.out.contains('\0') || !out.is_absolute() {
        return Err(invalid("out must be an absolute path"));
    }
    let Some(name) = out.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Err(invalid("out must name a file"));
    };
    match out.parent().map(std::fs::metadata) {
        Some(Ok(m)) if m.is_dir() => {}
        _ => return Err(invalid(format!("the directory of {} does not exist", out.display()))),
    }
    if out.is_dir() {
        return Err(invalid(format!("{} is a directory", out.display())));
    }
    if out.exists() && !p.overwrite {
        return Err(err_reason(
            ErrorCode::Exists,
            format!("{} already exists; choose another file or allow overwriting it", out.display()),
            "output_exists",
        ));
    }
    if let Some(since) = p.since_ms {
        if since > MAX_TS_MS {
            return Err(invalid("since is not a plausible time"));
        }
    }
    for id in &p.sessions {
        if !crate::util::is_session_id(id) {
            return Err(invalid(format!("{id:?} is not a session id")));
        }
    }
    let Some(_gate) = ctx.transfer.begin() else {
        return Err(RpcError::new(ErrorCode::Busy, "another export or import is already running"));
    };

    let (ids, skipped_archived) = select(ctx, &p).await?;
    let started = now_ms();
    let manifest = Manifest {
        format: FORMAT.into(),
        version: FORMAT_VERSION,
        exported_at_ms: started,
        anycon_version: env!("CARGO_PKG_VERSION").into(),
        node_name: ctx.config.get().node_name(),
        session_count: 0,
        filters: ExportFilters {
            project: p.project.clone(),
            since_ms: p.since_ms,
            sessions: (!p.sessions.is_empty()).then(|| p.sessions.clone()),
            exclude_archived: p.exclude_archived,
        },
    };
    let (cancel, watcher) = cancel_on_disconnect(client);
    let progress: ProgressSink = {
        let client = client.clone();
        Arc::new(move |pr| {
            if let Some(n) = RpcNotification::new(PROGRESS_METHOD, pr) {
                client.notify(&n);
            }
        })
    };
    let job = Job {
        rt: Handle::current(),
        store: ctx.store.clone(),
        blobs: ctx.blobs.clone(),
        out: out.clone(),
        overwrite: p.overwrite,
        ids,
        manifest,
        skipped_archived,
        cancel: cancel.clone(),
        progress,
    };
    let result = tokio::task::spawn_blocking(move || job.run()).await;
    watcher.abort();
    let result = match result {
        Ok(r) => r?,
        Err(e) => return Err(RpcError::new(ErrorCode::Internal, format!("export task failed: {e}"))),
    };
    tracing::info!(
        sessions = result.sessions,
        events = result.events,
        blobs = result.blobs,
        bytes = result.bytes,
        "export finished"
    );
    // 审计只记数量和文件名，不记路径和内容
    ctx.audit
        .record(
            &client.caller(),
            "export",
            Some(&name),
            serde_json::json!({
                "sessions": result.sessions,
                "events": result.events,
                "blobs": result.blobs,
                "bytes": result.bytes,
            }),
        )
        .await;
    Ok(result)
}

/// 取消标志：发起的连接断开（CLI 被 Ctrl+C 或被杀掉）就置位。调用时连接已经断开的，标志一开始就是置位的。
/// 返回的任务在请求结束时 `abort`。
pub(super) fn cancel_on_disconnect(client: &Arc<ClientHandle>) -> (Arc<AtomicBool>, tokio::task::JoinHandle<()>) {
    let cancel = Arc::new(AtomicBool::new(client.is_gone()));
    let (c, flag) = (client.clone(), cancel.clone());
    let watcher = tokio::spawn(async move {
        loop {
            if c.is_gone() {
                flag.store(true, Ordering::SeqCst);
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    (cancel, watcher)
}

/// 按筛选条件选出要导出的会话 id（创建顺序），以及因为已归档而被排除的个数。
async fn select(ctx: &Arc<Ctx>, p: &ExportParams) -> Result<(Vec<String>, u64), RpcError> {
    let internal = |e: StoreError| RpcError::new(ErrorCode::Internal, format!("cannot list sessions: {e}"));
    let rows = ctx.store.load_sessions().await.map_err(internal)?;
    let named: Option<HashSet<&str>> =
        (!p.sessions.is_empty()).then(|| p.sessions.iter().map(String::as_str).collect());
    if let Some(named) = &named {
        let known: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        let mut missing: Vec<&str> = named.iter().copied().filter(|id| !known.contains(id)).collect();
        missing.sort_unstable();
        if let Some(id) = missing.first() {
            return Err(RpcError::new(ErrorCode::NotFound, format!("session {id} not found")));
        }
    }
    let mut ids = Vec::new();
    let mut skipped_archived = 0;
    for row in &rows {
        if named.as_ref().is_some_and(|n| !n.contains(row.id.as_str())) {
            continue;
        }
        if let Some(project) = &p.project {
            if !row.project.as_deref().is_some_and(|x| ctx.workspace.same_project_name(x, project)) {
                continue;
            }
        }
        if let Some(since) = p.since_ms {
            // 行上的 updated_ms 可能落后于事件流：它够新就不用查事件
            let mut activity = row.updated_ms;
            if activity < since {
                activity = activity.max(ctx.store.last_event_ts(&row.id).await.map_err(internal)?.unwrap_or(0));
            }
            if activity < since {
                continue;
            }
        }
        // 点名的会话不受 `exclude_archived` 的限制
        if row.archived && p.exclude_archived && named.is_none() {
            skipped_archived += 1;
            continue;
        }
        ids.push(row.id.clone());
    }
    Ok((ids, skipped_archived))
}

struct Job {
    rt: Handle,
    store: Store,
    blobs: Arc<Blobs>,
    out: PathBuf,
    overwrite: bool,
    ids: Vec<String>,
    manifest: Manifest,
    skipped_archived: u64,
    cancel: Arc<AtomicBool>,
    progress: ProgressSink,
}

/// 临时文件：没有 [`Temp::persist`] 就在 drop 时删掉（出错、取消、panic）。
struct Temp {
    path: PathBuf,
    keep: bool,
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 写 zip 的状态。
struct Writer<'a> {
    job: &'a Job,
    zip: ZipWriter<BufWriter<File>>,
    when: DateTime,
    events: u64,
    blobs: u64,
    sessions: u64,
    bytes: u64,
    warnings: Vec<String>,
    last_progress: Instant,
}

impl Job {
    fn run(self) -> Result<ExportResult, ExportError> {
        let dir = self.out.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = self.out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp_path = dir.join(format!(".{name}.{:08x}.partial", rand::random::<u32>()));
        let mut oo = OpenOptions::new();
        oo.write(true).create_new(true).owner_only();
        let file = oo.open(&tmp_path)?;
        let mut tmp = Temp { path: tmp_path, keep: false };
        // unix 上创建时已经是 0600；Windows 上给这个文件设只有当前用户（和 SYSTEM / Administrators）能访问的 DACL
        crate::sys::restrict_to_owner(&tmp.path)?;
        let (y, mo, d, h, mi, s) = civil_from_ms(self.manifest.exported_at_ms);
        let when =
            DateTime::from_date_and_time(y as u16, mo as u8, d as u8, h as u8, mi as u8, s as u8).unwrap_or_default();
        let mut w = Writer {
            job: &self,
            zip: ZipWriter::new(BufWriter::with_capacity(1 << 20, file)),
            when,
            events: 0,
            blobs: 0,
            sessions: 0,
            bytes: 0,
            warnings: Vec::new(),
            last_progress: Instant::now(),
        };
        let total = self.ids.len() as u64;
        for (i, id) in self.ids.iter().enumerate() {
            self.check_cancel()?;
            w.export_session(id)?;
            w.report(i as u64 + 1, total, i as u64 + 1 == total);
        }
        let manifest = Manifest { session_count: w.sessions, ..self.manifest.clone() };
        w.put_json(MANIFEST_NAME, &manifest, CompressionMethod::Deflated)?;
        let Writer { zip, sessions, events, blobs, mut warnings, .. } = w;
        let mut buf = zip.finish()?;
        buf.flush()?;
        let file = buf.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        self.check_cancel()?;
        if !self.overwrite && self.out.exists() {
            return Err(ExportError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} appeared while exporting", self.out.display()),
            )));
        }
        std::fs::rename(&tmp.path, &self.out)?;
        tmp.keep = true;
        let bytes = std::fs::metadata(&self.out).map(|m| m.len()).unwrap_or(0);
        warnings.dedup();
        Ok(ExportResult {
            path: self.out.to_string_lossy().into_owned(),
            bytes,
            sessions,
            events,
            blobs,
            skipped_archived: self.skipped_archived,
            warnings,
        })
    }

    fn check_cancel(&self) -> Result<(), ExportError> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(ExportError::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn opts(when: DateTime, method: CompressionMethod, large: bool) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(method)
        .last_modified_time(when)
        .unix_permissions(0o600)
        .large_file(large)
}

/// 附件在压缩包里用什么压缩方式：图片本来就是压缩过的，存原样；其它类型压一压。
fn method_for(mime: &str) -> CompressionMethod {
    if mime.starts_with("image/") || mime.starts_with("video/") || mime.starts_with("audio/") || mime.ends_with("zip") {
        CompressionMethod::Stored
    } else {
        CompressionMethod::Deflated
    }
}

/// 附件在压缩包里的文件名：`<blob_id>.<daemon 存的扩展名>`；扩展名不是 1–16 个字母数字（不应该发生）时用 `bin`。
fn blob_file_name(row: &BlobRow) -> Option<String> {
    if !crate::blobs::is_blob_id(&row.id) {
        return None;
    }
    let ext = Path::new(&row.path).extension().and_then(|e| e.to_str()).filter(|e| super::is_ext(e)).unwrap_or("bin");
    Some(format!("{}.{ext}", row.id))
}

/// `blobs` 表里的 `path` 必须是 `blobs/` 下的一个普通文件名，才能拼成路径。
fn plain_file_name(p: &str) -> bool {
    !p.is_empty() && p != "." && p != ".." && !p.contains(['/', '\\', '\0'])
}

impl Writer<'_> {
    fn put_json<T: serde::Serialize>(
        &mut self,
        name: &str,
        v: &T,
        method: CompressionMethod,
    ) -> Result<(), ExportError> {
        let body = serde_json::to_vec_pretty(v)?;
        self.zip.start_file(name, opts(self.when, method, false))?;
        self.zip.write_all(&body)?;
        self.zip.write_all(b"\n")?;
        self.bytes += body.len() as u64 + 1;
        Ok(())
    }

    fn report(&mut self, done: u64, total: u64, last: bool) {
        if last || self.last_progress.elapsed() >= PROGRESS_EVERY {
            self.last_progress = Instant::now();
            (self.job.progress)(Progress { op: "export".into(), done, total, bytes: self.bytes });
        }
    }

    fn export_session(&mut self, id: &str) -> Result<(), ExportError> {
        let job = self.job;
        let rt = &job.rt;
        let mut snap = rt.block_on(job.store.begin_snapshot())?;
        let Some(row) = rt.block_on(snap.session(id))? else {
            self.warnings.push(format!("session {id} was deleted while exporting; it is not in the file"));
            return Ok(());
        };
        let wt: Option<WorktreeRow> = match &row.worktree_id {
            Some(wid) => rt.block_on(snap.worktree(wid))?,
            None => None,
        };
        let blob_rows = rt.block_on(snap.blobs_of(id))?;

        // 事件流
        self.zip.start_file(events_name(id), opts(self.when, CompressionMethod::Deflated, true))?;
        let (mut after, mut count, mut last_ts) = (0u64, 0u64, 0u64);
        loop {
            job.check_cancel()?;
            let page = rt.block_on(snap.events_after(id, after, EVENT_PAGE))?;
            let Some(last) = page.last() else { break };
            (after, last_ts) = (last.seq, last.ts_ms);
            let mut buf = Vec::with_capacity(64 * 1024);
            for ev in &page {
                serde_json::to_writer(&mut buf, ev)?;
                buf.push(b'\n');
            }
            self.zip.write_all(&buf)?;
            self.bytes += buf.len() as u64;
            count += page.len() as u64;
        }

        // 附件
        let mut blob_files = Vec::new();
        for b in &blob_rows {
            job.check_cancel()?;
            if let Some(f) = self.export_blob(id, b)? {
                blob_files.push(f);
            }
        }

        let file = session_file(&row, wt.as_ref(), blob_files, count, after, last_ts);
        self.put_json(&session_json_name(id), &file, CompressionMethod::Deflated)?;
        rt.block_on(snap.finish())?;
        self.sessions += 1;
        self.events += count;
        self.blobs += file.blobs.len() as u64;
        Ok(())
    }

    /// 写一个附件。文件不在 / 大小或 SHA-256 和记录对不上（磁盘损坏）时不写，记一条警告。
    fn export_blob(&mut self, session_id: &str, b: &BlobRow) -> Result<Option<BlobFile>, ExportError> {
        let (Some(file_name), true) = (blob_file_name(b), plain_file_name(&b.path)) else {
            self.warnings.push(format!("attachment {} of session {session_id} has an unusable record; skipped", b.id));
            return Ok(None);
        };
        let path = self.job.blobs.path_of(b);
        let mut f = match File::open(&path) {
            Ok(f) if f.metadata().map(|m| m.is_file() && m.len() == b.size).unwrap_or(false) => f,
            _ => {
                self.warnings.push(format!(
                    "attachment {} of session {session_id}: the file is missing or its size differs from the record; skipped",
                    b.id
                ));
                return Ok(None);
            }
        };
        // 先整个读一遍核对 SHA-256：压缩包里的条目写出去就撤不回来了
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            self.job.check_cancel()?;
        }
        if hex::encode(hasher.finalize()) != b.sha256 {
            self.warnings.push(format!(
                "attachment {} of session {session_id}: the file does not match its recorded SHA-256 (corrupt on disk?); skipped",
                b.id
            ));
            return Ok(None);
        }
        use std::io::Seek;
        f.rewind()?;
        self.zip.start_file(
            blob_entry_name(session_id, &file_name),
            opts(self.when, method_for(&b.mime), b.size >= ZIP64_THRESHOLD),
        )?;
        let mut left = b.size;
        while left > 0 {
            let want = (left as usize).min(buf.len());
            f.read_exact(&mut buf[..want])?;
            self.zip.write_all(&buf[..want])?;
            left -= want as u64;
            self.bytes += want as u64;
            self.job.check_cancel()?;
        }
        Ok(Some(BlobFile {
            blob_id: b.id.clone(),
            file: file_name,
            name: b.name.clone(),
            mime: b.mime.clone(),
            bytes: b.size,
            sha256: b.sha256.clone(),
            width: b.width,
            height: b.height,
            created_ms: b.created_ms,
        }))
    }
}

/// 会话行（加上事件流得出的数）→ `session.json`。
fn session_file(
    row: &SessionRow,
    wt: Option<&WorktreeRow>,
    blobs: Vec<BlobFile>,
    event_count: u64,
    last_seq: u64,
    last_ts: u64,
) -> SessionFile {
    SessionFile {
        id: row.id.clone(),
        project: row.project.clone(),
        agent: row.agent.clone(),
        title: row.title.clone(),
        title_locked: row.title_locked,
        status: row.status,
        archived: row.archived,
        settings: row.settings.clone(),
        agent_session_id: row.agent_session_id.clone(),
        created_ms: row.created_ms,
        updated_ms: row.updated_ms.max(last_ts),
        last_seq,
        next_turn_id: row.next_turn_id,
        usage: row.usage.clone(),
        preview: row.preview.clone(),
        worktree: wt.map(|w| WorktreeFile {
            branch: w.branch.clone(),
            base: w.base.clone(),
            base_display: w.base_display.clone(),
            base_sha: w.base_sha.clone(),
            rel_path: w.rel_path.clone(),
            state: w.state,
            created_ms: w.created_ms,
        }),
        blobs,
        event_count,
    }
}
