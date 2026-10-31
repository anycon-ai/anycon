//! 导出 / 导入（`anycon export` / `anycon import`，docs/spec/daemon.md § 20）：把会话历史（含附件）打成一个 zip，
//! 或者把这样的 zip 导入这台 daemon，用于备份和搬迁。
//!
//! - **通道**：只有本机 socket 上的 `local.export` / `local.import`（[`crate::local_methods`]），经 hub 调用一律 `forbidden`。
//!   daemon 拥有数据库，所以由它来读写（避免和正在跑的会话并发写）；路径是 daemon 所在机器上的绝对路径，
//!   远程客户端不能让 daemon 读写任意文件。同一时刻只跑一个导出或导入（[`Transfer::begin`]）。
//! - **进度**：请求本身是长请求，进度以 `local.progress` 通知只发给发起的那个连接（不像 `job.progress` 广播给所有客户端）；
//!   连接断开（CLI 被 Ctrl+C）就取消，导出删掉临时文件，导入回滚正在写的那个会话。
//! - **格式**是长期契约：这里是类型和名字语法，[`export`] 写、[`import`] 读并严格校验。
//! - 不导出：节点密钥、配对设备、配置、审计、进行中的权限请求、worktree 目录里的文件、会话的 `project_loc` /
//!   `launch_args`（只对原来那台机器有意义）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anycon_protocol::{SessionSettings, SessionStatus, UsageTotals, WorktreeState};
use serde::{Deserialize, Serialize};

pub mod export;
pub mod import;

/// `manifest.json` 的 `format`。
pub const FORMAT: &str = "anycon-export";
/// 当前格式版本。加字段（读的一方忽略不认识的字段）不升版本；改语义或删字段才升。
pub const FORMAT_VERSION: u32 = 1;
pub const MANIFEST_NAME: &str = "manifest.json";
/// 进度通知的方法名（只发给发起的连接）。
pub const PROGRESS_METHOD: &str = "local.progress";
/// 文件里的时间戳上限（公元 3000 年）：再大的不是真的时间，也避免 `as i64` 之后变成负数。
pub const MAX_TS_MS: u64 = 32_503_680_000_000;
/// 文件里的 seq / 计数上限：落库时是 SQLite 的 64 位有符号整数。
pub const MAX_SEQ: u64 = 1 << 53;

/// 导入时的上限（防 zip 炸弹）。默认值按「单个用户机器上能有的最大历史」放得很宽，目的只是挡住声明的大小和实际对不上的
/// 压缩包、和一次性把磁盘写满的压缩包；真正的内容校验在后面。
#[derive(Debug, Clone)]
pub struct Limits {
    /// 条目个数。
    pub max_entries: usize,
    /// 全部条目解压后的字节数之和（按条目头里声明的算，读的时候再强制每个条目不超过自己声明的大小）。
    pub max_total_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_session_json_bytes: u64,
    /// 一个会话的 `events.jsonl`。
    pub max_events_bytes: u64,
    /// `events.jsonl` 的一行。
    pub max_event_line_bytes: usize,
    /// 一个附件文件。
    pub max_blob_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        Limits {
            max_entries: 1_000_000,
            max_total_bytes: 128 * GIB,
            max_manifest_bytes: 1024 * 1024,
            max_session_json_bytes: 16 * 1024 * 1024,
            max_events_bytes: 16 * GIB,
            max_event_line_bytes: 8 * 1024 * 1024,
            max_blob_bytes: 4 * GIB,
        }
    }
}

/// 一台 daemon 一份：导出 / 导入的互斥和导入上限。
pub struct Transfer {
    busy: Arc<AtomicBool>,
    limits: Mutex<Limits>,
}

impl Default for Transfer {
    fn default() -> Self {
        Transfer { busy: Arc::new(AtomicBool::new(false)), limits: Mutex::new(Limits::default()) }
    }
}

/// 拿到它的人在跑导出 / 导入，drop 时放回去（任务 panic 也会放）。
pub struct TransferGuard(Arc<AtomicBool>);

impl Drop for TransferGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Transfer {
    /// 已经有一个在跑时返回 `None`。
    pub fn begin(&self) -> Option<TransferGuard> {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| TransferGuard(self.busy.clone()))
    }

    pub fn limits(&self) -> Limits {
        self.limits.lock().unwrap().clone()
    }

    /// 测试用：调小导入上限，不用真的造出几 GiB 的压缩包。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_limits(&self, l: Limits) {
        *self.limits.lock().unwrap() = l;
    }
}

// ---------------------------------------------------------------- 文件内容

/// `manifest.json`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// 固定 [`FORMAT`]。
    pub format: String,
    /// [`FORMAT_VERSION`]。
    pub version: u32,
    pub exported_at_ms: u64,
    /// 导出它的 anycon 版本（信息，不参与校验）。
    pub anycon_version: String,
    /// 导出时的节点名（信息）。
    pub node_name: String,
    /// 压缩包里的会话个数；导入时和实际的会话目录数核对（对不上说明压缩包被截断或改过）。
    pub session_count: u64,
    #[serde(default)]
    pub filters: ExportFilters,
}

/// 导出时的筛选条件（记在 manifest 里，说明这个压缩包是全部还是一部分）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExportFilters {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// 点名的会话 id。
    #[serde(default)]
    pub sessions: Option<Vec<String>>,
    /// 是否排除了已归档的会话（缺省不排除：导出是备份手段，默认带上全部）。
    #[serde(default)]
    pub exclude_archived: bool,
}

/// `sessions/<id>/session.json`：会话行里值得带走的列、worktree 记录和附件记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFile {
    /// 和目录名一致。
    pub id: String,
    /// 项目名；scratch 会话没有。
    #[serde(default)]
    pub project: Option<String>,
    pub agent: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub title_locked: bool,
    /// 导出时的状态（导入时 queued / starting / running / waiting_permission 变成 interrupted）。
    pub status: SessionStatus,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub settings: SessionSettings,
    /// agent 自己的会话 id。导出，但**导入时不收**：在别的机器上它对应的会话文件不存在，`--resume` 必然失败。
    #[serde(default)]
    pub agent_session_id: Option<String>,
    pub created_ms: u64,
    /// 最后活动时间：`sessions.updated_ms` 和最后一个事件的时间取较大的。
    pub updated_ms: u64,
    /// 最后一个事件的 seq（没有事件为 0）；导入时和 `events.jsonl` 核对。
    pub last_seq: u64,
    pub next_turn_id: u64,
    #[serde(default)]
    pub usage: UsageTotals,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub worktree: Option<WorktreeFile>,
    /// 绑定在这个会话上的附件，`blobs/` 目录里每个文件一条。
    #[serde(default)]
    pub blobs: Vec<BlobFile>,
    /// `events.jsonl` 的行数；导入时核对。
    pub event_count: u64,
}

/// 会话的 worktree 记录。只有元数据：目录里的文件不导出，目录身份（设备号 / inode）、`.git` 指针、
/// 项目位置对别的机器没有意义，也不导出。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorktreeFile {
    pub branch: String,
    pub base: String,
    pub base_display: String,
    pub base_sha: String,
    /// 相对项目目录的路径，`.anycon/worktrees/<名字>`。
    pub rel_path: String,
    /// 导出时的状态。
    pub state: WorktreeState,
    pub created_ms: u64,
}

/// 一个附件的元数据（文件本身在 `blobs/<file>`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlobFile {
    pub blob_id: String,
    /// `blobs/` 下的文件名：`<blob_id>.<扩展名>`，扩展名取自 daemon 存的文件名。
    pub file: String,
    /// 展示用文件名。
    pub name: String,
    pub mime: String,
    pub bytes: u64,
    /// 文件内容的 SHA-256（64 位小写 hex）。导入时按它核对。
    pub sha256: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    pub created_ms: u64,
}

// ---------------------------------------------------------------- 条目名

/// 压缩包里允许出现的条目。
#[derive(Debug, PartialEq, Eq)]
pub enum Entry<'a> {
    Manifest,
    /// `sessions/`、`sessions/<id>/`、`sessions/<id>/blobs/`：目录条目（可有可无，没有内容）。
    Dir,
    SessionJson(&'a str),
    Events(&'a str),
    /// 会话 id、文件名。
    Blob(&'a str, &'a str),
}

/// 附件文件名里的扩展名：1–16 个 ASCII 字母数字。
fn is_ext(s: &str) -> bool {
    (1..=16).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `<32 位小写 hex>.<扩展名>`：返回 (blob_id, 扩展名)。
pub fn parse_blob_file(file: &str) -> Option<(&str, &str)> {
    let (id, ext) = file.split_once('.')?;
    (crate::blobs::is_blob_id(id) && is_ext(ext)).then_some((id, ext))
}

/// 条目名只认格式里定义的这几种：
/// `manifest.json`、`sessions/<会话 id>/session.json`、`.../events.jsonl`、`.../blobs/<blob_id>.<扩展名>`，
/// 以及它们的目录条目。其余一律不认——绝对路径、`..`、反斜杠、非 ASCII、多余的目录层级、符号链接指向的名字都落在这里。
pub fn parse_entry_name(name: &str) -> Option<Entry<'_>> {
    if name.is_empty() || name.len() > 200 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b)) {
        return None;
    }
    if name == MANIFEST_NAME {
        return Some(Entry::Manifest);
    }
    let parts: Vec<&str> = name.split('/').collect();
    let sid_ok = |s: &str| crate::util::is_session_id(s);
    match parts.as_slice() {
        ["sessions", ""] => Some(Entry::Dir),
        ["sessions", sid, ""] if sid_ok(sid) => Some(Entry::Dir),
        ["sessions", sid, "blobs", ""] if sid_ok(sid) => Some(Entry::Dir),
        ["sessions", sid, "session.json"] if sid_ok(sid) => Some(Entry::SessionJson(sid)),
        ["sessions", sid, "events.jsonl"] if sid_ok(sid) => Some(Entry::Events(sid)),
        ["sessions", sid, "blobs", file] if sid_ok(sid) && parse_blob_file(file).is_some() => {
            Some(Entry::Blob(sid, file))
        }
        _ => None,
    }
}

pub fn session_json_name(id: &str) -> String {
    format!("sessions/{id}/session.json")
}

pub fn events_name(id: &str) -> String {
    format!("sessions/{id}/events.jsonl")
}

pub fn blob_entry_name(id: &str, file: &str) -> String {
    format!("sessions/{id}/blobs/{file}")
}

// ---------------------------------------------------------------- RPC 参数和结果

/// `local.export`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportParams {
    /// 输出文件，daemon 所在机器上的绝对路径。
    pub out: String,
    #[serde(default)]
    pub project: Option<String>,
    /// 只要最后活动时间不早于它的会话。
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// 点名的会话（完整 id）。点名的会话不受 `exclude_archived` 的限制，其它条件照样要满足。
    #[serde(default)]
    pub sessions: Vec<String>,
    /// 不要已归档的会话（缺省带上）。
    #[serde(default)]
    pub exclude_archived: bool,
    /// 输出文件已经存在时覆盖它；缺省拒绝（`exists`）。
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportResult {
    pub path: String,
    /// 压缩包的大小。
    pub bytes: u64,
    pub sessions: u64,
    pub events: u64,
    pub blobs: u64,
    /// 因为 `exclude_archived` 而没有导出的已归档会话数（没给 `exclude_archived` 时为 0）。
    pub skipped_archived: u64,
    /// 没能完整导出的东西（附件文件丢了 / 对不上、会话在导出过程中被删了）。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `local.import`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportParams {
    /// 压缩包，daemon 所在机器上的绝对路径。
    pub file: String,
    /// 只校验，不写任何东西。
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportSkipped {
    pub id: String,
    /// 目前只有 `already_exists`。
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportFailed {
    pub id: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub dry_run: bool,
    /// 压缩包里的会话数。
    pub total: u64,
    /// 导入了的（`dry_run` 时是「会导入」的）会话 id。
    pub imported: Vec<String>,
    pub skipped: Vec<ImportSkipped>,
    pub failed: Vec<ImportFailed>,
    /// 导入了的事件数和附件数。
    pub events: u64,
    pub blobs: u64,
    /// 压缩包 manifest 里的来源信息。
    pub source_node: String,
    pub source_version: String,
    pub exported_at_ms: u64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// `local.progress` 通知的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// `export` 或 `import`。
    pub op: String,
    /// 处理完的会话数和总数。
    pub done: u64,
    pub total: u64,
    /// 目前写出 / 读入的字节数。
    pub bytes: u64,
}

/// 发进度通知的出口：只发给发起的连接。
pub type ProgressSink = Arc<dyn Fn(Progress) + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "sess_0123456789abcdefABCDEF";
    const BID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn only_the_paths_the_format_defines_are_accepted() {
        assert_eq!(parse_entry_name("manifest.json"), Some(Entry::Manifest));
        assert_eq!(parse_entry_name("sessions/"), Some(Entry::Dir));
        assert_eq!(parse_entry_name(&format!("sessions/{SID}/")), Some(Entry::Dir));
        assert_eq!(parse_entry_name(&format!("sessions/{SID}/blobs/")), Some(Entry::Dir));
        assert_eq!(parse_entry_name(&format!("sessions/{SID}/session.json")), Some(Entry::SessionJson(SID)));
        assert_eq!(parse_entry_name(&format!("sessions/{SID}/events.jsonl")), Some(Entry::Events(SID)));
        let file = format!("{BID}.png");
        assert_eq!(parse_entry_name(&format!("sessions/{SID}/blobs/{file}")), Some(Entry::Blob(SID, file.as_str())));
        // 扩展名不限图片，但只收安全字符
        assert!(parse_entry_name(&format!("sessions/{SID}/blobs/{BID}.pdf")).is_some());
        assert!(parse_entry_name(&format!("sessions/{SID}/blobs/{BID}.TXT")).is_some());
        for bad in [
            "",
            "/manifest.json",
            "manifest.json/",
            "./manifest.json",
            "../manifest.json",
            "sessions/../manifest.json",
            "sessions/..",
            "sessions",
            "sessions//session.json",
            "other.txt",
            "__MACOSX/._manifest.json",
            ".DS_Store",
            "sessions/sess_short/session.json",
            &format!("sessions/{SID}/../{SID}/session.json"),
            &format!("sessions/{SID}/session.json/"),
            &format!("sessions/{SID}/notes.txt"),
            &format!("sessions/{SID}/blobs/{BID}"),
            &format!("sessions/{SID}/blobs/{BID}."),
            &format!("sessions/{SID}/blobs/{BID}.a.b"),
            &format!("sessions/{SID}/blobs/{BID}.p ng"),
            &format!("sessions/{SID}/blobs/{BID}.{}", "a".repeat(17)),
            &format!("sessions/{SID}/blobs/../../x.png"),
            &format!("sessions/{SID}/blobs/sub/{BID}.png"),
            &format!("sessions/{SID}\\session.json"),
            &format!("sessions/{SID}/session.json\0"),
            &format!("sessions/{SID}/blobs/{}.png", BID.to_uppercase()),
            "C:/Windows/x",
            "sessions/sess_0123456789abcdefABCDEé/session.json",
        ] {
            assert_eq!(parse_entry_name(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_gate_allows_one_transfer_at_a_time() {
        let t = Transfer::default();
        let g = t.begin().expect("first");
        assert!(t.begin().is_none(), "a second one is refused");
        drop(g);
        assert!(t.begin().is_some(), "released on drop");
    }
}
