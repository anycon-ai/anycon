//! 附件 / blob 通道（[ADR-35](../../../docs/decisions.md)，docs/spec/daemon.md § 7.10）：文件（图片或任意文件）分块上传、校验、落盘、
//! 分页下载、回收，以及 `session.send` / `session.create` 对 `AttachmentRef` 的核对与绑定。
//!
//! - **存放**：`<数据目录>/blobs/<blob_id>.<ext>`（目录 0700、文件 0600），上传中的写 `blobs/tmp/<blob_id>.part`，启动时清空 `tmp/`。
//!   `blob_id` 是 32 位小写 hex，**每个入口先校验格式**（它会进路径）。`ext` 对图片由 mime 决定；其它文件取 `name` 的扩展名，
//!   但只认 1–16 个 ASCII 字母数字（[`stored_ext`]），否则是 `bin`——路径里没有其它客户端给的字符。
//! - **上传状态**在内存里（`blob_id` → 期望大小、mime、sha256、已收的块、发起设备、最后活动时间）；同一设备同时进行中的上传
//!   至多 8 个；10 分钟没有新块就作废。`put_end` 才写 `blobs` 表（`session_id` 为空 = 还没被消息引用）。
//! - **`put_end` 是收尾，不是又一块**：它在锁内把上传标成「收尾中」，之后的 `put_chunk` 一律 `invalid`（`finalizing`）；
//!   已经拿到文件句柄的写先做完（每个上传一把读写锁：块拿读锁、收尾拿写锁），然后才算 SHA-256 / 读文件头，所以落盘的内容就是校验过的
//!   内容。并发的第二个 `put_end`（重连后旧传输上的请求和新传输上的重试）等第一个的结果，拿到同一个 `AttachmentRef` 或同一个错误。
//!   收尾在独立的任务里跑：请求被取消（客户端断开）也不会把上传留在半截状态。
//! - **回收**：24 小时前完成、仍没被引用的 blob 由每小时的清理任务删除（[`Blobs::gc`]）；第一次被消息引用时绑定到会话，
//!   随会话删除。
//! - **总量**：完成但没被引用的 blob（加上进行中的上传）全节点合计 ≤ 1 GiB，`put_begin` 超了 → `busy`（`quota`）；
//!   数据目录所在卷剩余空间 < `size` + 1 GiB → `busy`（`disk_full`）。
//! - **校验点只有这里**：mime 的格式、大小、SHA-256；四种图片类型（[`ext_of`]）的文件头（magic bytes）必须和声明的 mime 一致，
//!   宽高由文件头读出，读不出就省略。文件头只是**前缀比对**（PNG 8 字节、JPEG 3、GIF 6、WebP 12），不解码、不校验图片结构。
//!   其它类型不看内容：它们以路径交给 agent，由 agent 自己读（ADR-35「任意文件」）。
//! - **名字**只用于展示：拒绝 `/` `\`、控制字符（Cc）、双向控制符（U+200E / U+200F / U+202A–U+202E / U+2066–U+2069）和
//!   U+2028 / U+2029；`put_end` 记录的名字扩展名和 mime 对不上时在后面补 `.<ext>`。
//!
//! 这个模块只在 daemon ↔ 客户端之间流动，hub 看到的只是密文信封。**不记录文件内容**（日志里最多有 blob_id 和字节数）。
//! 时钟和两个阈值（上传 TTL、未引用回收）是原子变量，测试里可以调（[`Blobs::advance_clock`] 等）。

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_protocol::{
    accepts_attachment, AttachmentRef, BlobGetParams, BlobGetResult, BlobIdParams, BlobPutBeginParams,
    BlobPutBeginResult, BlobPutChunkParams, ErrorCode, RpcError,
};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Notify, OwnedRwLockReadGuard, RwLock as AsyncRwLock};

use crate::config::{ConfigStore, Paths};
use crate::store::{BlobRow, Store, StoreError};
use crate::sys::{read_exact_at, write_all_at, OwnerOnly};
use crate::util::now_ms;
use crate::worktree::err_reason;

type RpcResult<T> = Result<T, RpcError>;

/// 分块字节数。除最后一块外每块恰好这么大；`blob.get` 每段的上限也是它。
pub const CHUNK_SIZE: u32 = 512 * 1024;
/// 一条消息至多带这么多个附件。
pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 20;
/// 同一设备同时进行中的上传上限。
pub const MAX_UPLOADS_PER_DEVICE: usize = 8;
/// 进行中的上传多久没有新块就作废。
const UPLOAD_TTL: Duration = Duration::from_secs(10 * 60);
/// 完成后没被消息引用的 blob 保留多久。
const ORPHAN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// 一条消息（积压合并成一个 turn 时是一个 turn）的附件字节合计上限：Claude 一次请求的体积有 32 MB 的上限，
/// 图片再经 base64 涨三分之一。
pub const MAX_MESSAGE_BYTES: u64 = 20 * 1024 * 1024;
/// 完成但没被引用的 blob（加上进行中的上传）全节点合计上限。
pub const UNREFERENCED_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
/// 数据目录所在卷至少要剩这么多（再加上这次上传的大小）才收新的上传。
pub const DISK_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;
/// `name` 的字节上限。
const NAME_MAX: usize = 255;
/// 计算 SHA-256 时每次读多少。
const HASH_BUF: usize = 256 * 1024;
/// `put_end` 校验失败时 `data.reason` 的取值（protocol.md § 5.6）。
pub const REASON_INCOMPLETE: &str = "incomplete";
pub const REASON_HASH_MISMATCH: &str = "hash_mismatch";
pub const REASON_MIME_MISMATCH: &str = "mime_mismatch";
pub const REASON_TOO_LARGE: &str = "too_large";
pub const REASON_IN_USE: &str = "in_use";
pub const REASON_BLOB_IN_USE: &str = "blob_in_use";
/// `put_chunk` 在 `put_end` 接手之后到达。
pub const REASON_FINALIZING: &str = "finalizing";
/// `put_begin`：未引用的 blob 合计超过全节点的配额（`busy`）。
pub const REASON_QUOTA: &str = "quota";
/// `put_begin`：数据目录所在卷剩余空间不够（`busy`）。
pub const REASON_DISK_FULL: &str = "disk_full";
/// `not_found` 的 `data.reason`：附件指向的 blob 不存在（过期被回收了…）。
pub const REASON_BLOB: &str = "blob";

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("store: {0}")]
    Store(#[from] StoreError),
}

/// 四种图片类型对应的扩展名（Codex 按扩展名判断图片类型）；不是这四种返回 `None`。
pub fn ext_of(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

/// 是不是会核对文件头、作为图片交给模型的四种类型之一。
pub fn is_image_mime(mime: &str) -> bool {
    ext_of(mime).is_some()
}

/// mime 的字节上限（RFC 6838：type 和 subtype 各 ≤ 127，这里合起来限 127）。
const MIME_MAX: usize = 127;

/// 规整 `put_begin` 的 mime：`type/subtype`，两段都是 RFC 6838 的 restricted-name（字母数字开头，之后是字母数字和
/// `!#$&-^_.+`），不带参数，合计 ≤ 127 字节；转成小写。不合格返回 `None`。
pub fn normalize_mime(mime: &str) -> Option<String> {
    let ok = |part: &str| {
        part.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric())
            && part.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'!' | b'#' | b'$' | b'&' | b'-' | b'^' | b'_' | b'.' | b'+')
            })
    };
    let (ty, sub) = mime.split_once('/')?;
    (mime.len() <= MIME_MAX && ok(ty) && ok(sub)).then(|| mime.to_ascii_lowercase())
}

/// 落盘文件的扩展名：图片按 mime（[`ext_of`]）；其它文件取 `name` 最后一个 `.` 之后的部分，1–16 个 ASCII 字母数字才用
/// （转小写，agent 和它的工具靠扩展名认文件类型），否则 `bin`。
pub fn stored_ext(name: &str, mime: &str) -> String {
    if let Some(e) = ext_of(mime) {
        return e.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty() && (1..=16).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            ext.to_ascii_lowercase()
        }
        _ => "bin".to_string(),
    }
}

/// 落盘的文件名是不是 `<blob_id>.<ext>` 的样子（[`stored_ext`] 能产生的扩展名）。
fn is_stored_file_name(name: &str) -> bool {
    name.split_once('.').is_some_and(|(id, ext)| {
        is_blob_id(id)
            && (1..=16).contains(&ext.len())
            && ext.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// 32 位小写 hex（会进路径，所以每个入口都先过它）。
pub fn is_blob_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 读文件头判断真实类型：PNG `89 50 4E 47 0D 0A 1A 0A`、JPEG `FF D8 FF`、GIF `GIF87a` / `GIF89a`、WebP `RIFF????WEBP`。
pub fn sniff_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn be16(b: &[u8]) -> u32 {
    u32::from(b[0]) << 8 | u32::from(b[1])
}
fn be32(b: &[u8]) -> u32 {
    u32::from(b[0]) << 24 | u32::from(b[1]) << 16 | u32::from(b[2]) << 8 | u32::from(b[3])
}
fn le16(b: &[u8]) -> u32 {
    u32::from(b[1]) << 8 | u32::from(b[0])
}
fn le24(b: &[u8]) -> u32 {
    u32::from(b[2]) << 16 | u32::from(b[1]) << 8 | u32::from(b[0])
}
fn le32(b: &[u8]) -> u32 {
    u32::from(b[3]) << 24 | le24(b)
}

/// 读图片宽高（只看文件头，不解码）。`read(offset, n)` 读 `[offset, offset + n)`，读不满返回 `None`。
/// 读不出（格式特殊、文件头残缺）返回 `None`——不算错。
pub fn image_size(mime: &str, read: &dyn Fn(u64, usize) -> Option<Vec<u8>>) -> Option<(u32, u32)> {
    let nonzero = |w: u32, h: u32| (w > 0 && h > 0).then_some((w, h));
    match mime {
        "image/png" => {
            let b = read(0, 24)?;
            if &b[12..16] != b"IHDR" {
                return None;
            }
            nonzero(be32(&b[16..20]), be32(&b[20..24]))
        }
        "image/gif" => {
            let b = read(0, 10)?;
            nonzero(le16(&b[6..8]), le16(&b[8..10]))
        }
        "image/webp" => {
            let b = read(0, 30)?;
            match &b[12..16] {
                // 有损：帧头里 14 位宽高
                b"VP8 " => {
                    if b[23..26] != [0x9D, 0x01, 0x2A] {
                        return None;
                    }
                    nonzero(le16(&b[26..28]) & 0x3FFF, le16(&b[28..30]) & 0x3FFF)
                }
                // 无损：签名 0x2F 之后 14 位（宽 - 1）+ 14 位（高 - 1）
                b"VP8L" => {
                    if b[20] != 0x2F {
                        return None;
                    }
                    let bits = le32(&b[21..25]);
                    nonzero((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
                }
                // 扩展格式：画布宽高 - 1，各 24 位
                b"VP8X" => nonzero(le24(&b[24..27]) + 1, le24(&b[27..30]) + 1),
                _ => None,
            }
        }
        "image/jpeg" => {
            // 逐段跳到 SOFn（EXIF / ICC 可能很大，所以不能只看前几十个字节）
            let mut pos = 2u64;
            for _ in 0..1024 {
                let h = read(pos, 4)?;
                if h[0] != 0xFF {
                    return None;
                }
                let m = h[1];
                match m {
                    0xFF => pos += 1,
                    // SOI / TEM / RSTn：没有长度
                    0xD8 | 0x01 | 0xD0..=0xD7 => pos += 2,
                    // EOI / SOS 之前还没见到 SOF
                    0xD9 | 0xDA => return None,
                    _ => {
                        let seg = u64::from(be16(&h[2..4]));
                        if seg < 2 {
                            return None;
                        }
                        // SOF0..SOF15，除了 DHT (C4)、JPG (C8)、DAC (CC)
                        if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                            let s = read(pos + 4, 5)?;
                            return nonzero(be16(&s[3..5]), be16(&s[1..3]));
                        }
                        pos += 2 + seg;
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// 文件名里不允许的字符：路径分隔符、控制字符（Unicode Cc，含 C1）、双向控制符（会让显示出来的文件名和真实的不一样，
/// 例如 `invoice\u{202E}gnp.exe`）、行 / 段分隔符 U+2028 / U+2029。
fn is_forbidden_in_name(c: char) -> bool {
    c == '/'
        || c == '\\'
        || c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}')
}

/// 校验展示用文件名：1–255 字节，不含 [`is_forbidden_in_name`] 里的字符。
fn validate_name(name: &str) -> RpcResult<()> {
    if name.is_empty() || name.len() > NAME_MAX {
        return Err(RpcError::new(ErrorCode::Invalid, "name must be 1 to 255 bytes"));
    }
    if name.chars().any(is_forbidden_in_name) {
        return Err(RpcError::new(
            ErrorCode::Invalid,
            "name must not contain '/', '\\', control characters or bidirectional-control characters",
        ));
    }
    Ok(())
}

/// 记录下来的名字：图片的扩展名和 mime 对不上（没有扩展名、`.html` 配 `image/jpeg`…）就在后面补 `.<ext>`（`png`；
/// `jpg` / `jpeg`；`gif`；`webp`，不分大小写）。补完超过 255 字节时截短原来的名字（在字符边界上）。其它类型原样保留。
pub fn normalize_name(name: &str, mime: &str) -> String {
    let Some(want) = ext_of(mime) else { return name.to_string() };
    let ok_exts: &[&str] = if want == "jpg" { &["jpg", "jpeg"] } else { std::slice::from_ref(&want) };
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if !stem.is_empty() && ok_exts.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return name.to_string();
        }
    }
    let joined = if name.ends_with('.') { format!("{name}{want}") } else { format!("{name}.{want}") };
    if joined.len() <= NAME_MAX {
        return joined;
    }
    // 太长：截短原来的名字，给 `.<ext>` 留位置
    let mut cut = NAME_MAX - 1 - want.len();
    while !name.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}.{want}", name[..cut].trim_end_matches('.'))
}

fn invalid_id() -> RpcError {
    RpcError::new(ErrorCode::Invalid, "blob_id must be 32 lowercase hex characters")
}

fn internal(what: &str, e: impl std::fmt::Display) -> RpcError {
    tracing::warn!(error = %e, "{what}");
    RpcError::new(ErrorCode::Internal, format!("{what}: {e}"))
}

/// `put_end` 的结果。只有收尾任务写它（`None` = 还没完成），`put_end` 的调用者和 `discard` 在上面等。
type Outcome = Option<RpcResult<AttachmentRef>>;

/// 一个正在上传的 blob。
struct Upload {
    name: String,
    mime: String,
    size: u64,
    sha256: String,
    /// 发起上传的设备（每设备并发上限）。
    device: String,
    chunk_count: u32,
    received: Vec<bool>,
    last_active_ms: u64,
    /// `blobs/tmp/<id>.part`，创建时 `set_len(size)`（稀疏文件），分块按偏移写。
    file: Arc<File>,
    /// 块拿读锁（持有到写完、记账完），收尾拿写锁：收尾开始校验时，已经拿到文件句柄的写都做完了，之后没有新的写。
    gate: Arc<AsyncRwLock<()>>,
    /// `Some` = `put_end` 已经接手（收尾中）：`put_chunk` → `invalid`（`finalizing`）；并发的 `put_end` / `discard` 在这里等结果。
    finalizing: Option<watch::Receiver<Outcome>>,
}

/// `put_end` 交给收尾任务的东西（上传的快照）。
struct FinalizeJob {
    name: String,
    mime: String,
    size: u64,
    sha256: String,
    file: Arc<File>,
    gate: Arc<AsyncRwLock<()>>,
}

/// 等收尾任务的结果。任务没留下结果就结束（panic）时返回 `None`。
async fn await_outcome(mut rx: watch::Receiver<Outcome>) -> Outcome {
    match rx.wait_for(Option::is_some).await {
        Ok(v) => v.clone(),
        Err(_) => None,
    }
}

/// 测试用：让下一个 `put_end` 的收尾在「已经接手、还没开始校验」处停住（见 [`Blobs::hold_next_finalize`]）。
pub struct FinalizeHold {
    reached: Arc<Notify>,
    release: Arc<Notify>,
}

impl FinalizeHold {
    /// 收尾任务停在了断点上（此时 `put_chunk` 已经会被拒绝，第二个 `put_end` 会在等第一个）。
    pub async fn reached(&self) {
        self.reached.notified().await;
    }

    /// 放行。
    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// 测试用：模拟「有一块已经拿到文件句柄、正在写」——持有这个上传的读锁，直到被丢弃（见 [`Blobs::hold_chunk_writes`]）。
pub struct ChunkWriteHold {
    _guard: OwnedRwLockReadGuard<()>,
}

/// 统计（`anycon doctor`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobStats {
    /// 已完成的 blob 个数和字节数（含已被引用的）。
    pub count: u64,
    pub bytes: u64,
    /// 其中还没被消息引用的字节数。
    pub unreferenced_bytes: u64,
    /// 进行中的上传。
    pub uploads: usize,
    /// 数据目录所在卷的剩余字节数。
    pub disk_free: u64,
}

/// 核对通过的附件：daemon 记录的元数据加正式文件的绝对路径。排队的消息带着它，开 turn 时交给 adapter。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub reference: AttachmentRef,
    pub path: PathBuf,
}

/// 附件存储：上传状态、校验、落盘、回收。
pub struct Blobs {
    dir: PathBuf,
    tmp: PathBuf,
    store: Store,
    config: Arc<ConfigStore>,
    uploads: Mutex<HashMap<String, Upload>>,
    /// 测试里往前拨的时钟偏移（毫秒）。
    clock_offset_ms: AtomicU64,
    /// 上传多久没有新块就作废（毫秒）；默认 [`UPLOAD_TTL`]。
    upload_ttl_ms: AtomicU64,
    /// 完成后没被引用的 blob 保留多久（毫秒）；默认 [`ORPHAN_TTL`]。
    orphan_ttl_ms: AtomicU64,
    /// 一条消息的附件字节合计上限；默认 [`MAX_MESSAGE_BYTES`]。
    message_bytes_limit: AtomicU64,
    /// 未引用 blob 的全节点配额；默认 [`UNREFERENCED_QUOTA_BYTES`]。
    unreferenced_quota: AtomicU64,
    /// 剩余磁盘空间的底线；默认 [`DISK_RESERVE_BYTES`]。
    disk_reserve: AtomicU64,
    /// 测试断点（[`Blobs::hold_next_finalize`]）。
    finalize_hold: Mutex<Option<(Arc<Notify>, Arc<Notify>)>>,
}

impl Blobs {
    /// 建目录（0700）、清空 `blobs/tmp/`（上次没传完的上传作废）、清掉没有数据库记录的孤儿文件（崩溃在 rename 和写库之间）。
    pub async fn open(paths: &Paths, store: Store, config: Arc<ConfigStore>) -> Result<Arc<Blobs>, BlobError> {
        // 交给 adapter 的是绝对路径（`ANYCON_HOME` 可能是相对的，agent 的工作目录是别处）
        let dir = std::path::absolute(paths.blobs_dir())?;
        let tmp = dir.join("tmp");
        if tmp.exists() {
            std::fs::remove_dir_all(&tmp)?;
        }
        std::fs::create_dir_all(&tmp)?;
        for d in [&dir, &tmp] {
            crate::sys::restrict_to_owner(d)?;
        }
        let this = Arc::new(Blobs {
            dir,
            tmp,
            store,
            config,
            uploads: Mutex::new(HashMap::new()),
            clock_offset_ms: AtomicU64::new(0),
            upload_ttl_ms: AtomicU64::new(UPLOAD_TTL.as_millis() as u64),
            orphan_ttl_ms: AtomicU64::new(ORPHAN_TTL.as_millis() as u64),
            message_bytes_limit: AtomicU64::new(MAX_MESSAGE_BYTES),
            unreferenced_quota: AtomicU64::new(UNREFERENCED_QUOTA_BYTES),
            disk_reserve: AtomicU64::new(DISK_RESERVE_BYTES),
            finalize_hold: Mutex::new(None),
        });
        this.sweep_orphan_files().await;
        Ok(this)
    }

    /// 当前时间（毫秒），加上测试拨的偏移。
    fn now(&self) -> u64 {
        now_ms() + self.clock_offset_ms.load(Ordering::Relaxed)
    }

    /// 测试用：把 blob 的时钟往前拨（过期 / 回收的用例不用真的等 10 分钟 / 24 小时）。
    pub fn advance_clock(&self, d: Duration) {
        self.clock_offset_ms.fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// 测试用：上传多久没有新块就作废。
    pub fn set_upload_ttl(&self, d: Duration) {
        self.upload_ttl_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// 测试用：完成后没被引用的 blob 保留多久。
    pub fn set_orphan_ttl(&self, d: Duration) {
        self.orphan_ttl_ms.store(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// 测试用：一条消息的附件字节合计上限（缺省 20 MiB）。
    pub fn set_message_bytes_limit(&self, bytes: u64) {
        self.message_bytes_limit.store(bytes, Ordering::Relaxed);
    }

    /// 一条消息（积压合并成一个 turn 时是一个 turn）的附件字节合计上限。
    pub fn message_bytes_limit(&self) -> u64 {
        self.message_bytes_limit.load(Ordering::Relaxed)
    }

    /// 测试用：未引用 blob 的全节点配额（缺省 1 GiB）。
    pub fn set_unreferenced_quota(&self, bytes: u64) {
        self.unreferenced_quota.store(bytes, Ordering::Relaxed);
    }

    /// 测试用：剩余磁盘空间的底线（缺省 1 GiB）。
    pub fn set_disk_reserve(&self, bytes: u64) {
        self.disk_reserve.store(bytes, Ordering::Relaxed);
    }

    /// 测试用：让下一个 `put_end` 的收尾停在「已经接手、还没开始校验」处，直到 [`FinalizeHold::release`]。
    pub fn hold_next_finalize(&self) -> FinalizeHold {
        let (reached, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        *self.finalize_hold.lock().unwrap() = Some((reached.clone(), release.clone()));
        FinalizeHold { reached, release }
    }

    /// 测试用：让这个上传看起来有一块写到一半（持有它的读锁）。`put_end` 的收尾要等它被丢弃才开始校验。
    pub async fn hold_chunk_writes(&self, blob_id: &str) -> Option<ChunkWriteHold> {
        let gate = self.uploads.lock().unwrap().get(blob_id).map(|u| u.gate.clone())?;
        Some(ChunkWriteHold { _guard: gate.read_owned().await })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 个数、字节数、配额占用、剩余磁盘（`anycon doctor` 的 blobs 一行）。
    pub async fn stats(&self) -> Result<BlobStats, BlobError> {
        let (count, bytes) = self.store.blobs_totals().await?;
        let unreferenced_bytes = self.store.blobs_unreferenced_bytes().await?;
        Ok(BlobStats {
            count,
            bytes,
            unreferenced_bytes,
            uploads: self.uploads.lock().unwrap().len(),
            disk_free: crate::doctor::disk_free(&self.dir),
        })
    }

    /// 某条记录的正式文件路径（`path` 列只存文件名）。
    pub fn path_of(&self, row: &BlobRow) -> PathBuf {
        self.dir.join(&row.path)
    }

    fn part_path(&self, id: &str) -> PathBuf {
        self.tmp.join(format!("{id}.part"))
    }

    /// 数据库记录对外的样子（以 daemon 的记录为准，含宽高）。
    pub fn reference(row: &BlobRow) -> AttachmentRef {
        AttachmentRef {
            blob_id: row.id.clone(),
            name: row.name.clone(),
            mime: row.mime.clone(),
            bytes: row.size,
            width: row.width,
            height: row.height,
        }
    }

    pub fn attached(&self, row: &BlobRow) -> Attached {
        Attached { reference: Self::reference(row), path: self.path_of(row) }
    }

    /// 启动时：`blobs/` 里文件名合法、但数据库里没有记录的文件是崩溃留下的孤儿，删掉。只认 `<32 hex>.<ext>` 这种名字，别的不碰。
    async fn sweep_orphan_files(&self) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_stored_file_name(&name) {
                continue;
            }
            let Some((id, _)) = name.split_once('.') else { continue };
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            match self.store.blob_get(id).await {
                Ok(None) => {
                    tracing::warn!(blob = %id, "removing an attachment file that has no record");
                    let _ = std::fs::remove_file(entry.path());
                }
                Ok(Some(_)) => {}
                Err(e) => tracing::warn!(error = %e, "cannot check an attachment file against the database"),
            }
        }
    }

    // ============================================================ 上传

    /// `blob.put_begin`。`device` 是发起设备的 id（每设备并发上限）。
    pub async fn put_begin(&self, device: &str, p: BlobPutBeginParams) -> RpcResult<BlobPutBeginResult> {
        self.expire_uploads();
        validate_name(&p.name)?;
        let Some(mime) = normalize_mime(&p.mime) else {
            return Err(RpcError::new(
                ErrorCode::Invalid,
                "mime must be a type/subtype without parameters, at most 127 bytes",
            ));
        };
        let image = is_image_mime(&mime);
        let max = self.config.get().max_attachment_bytes();
        if p.size == 0 || p.size > max {
            return Err(err_reason(
                ErrorCode::Invalid,
                format!("size must be between 1 and {max} bytes"),
                REASON_TOO_LARGE,
            ));
        }
        if !is_sha256_hex(&p.sha256) {
            return Err(RpcError::new(ErrorCode::Invalid, "sha256 must be 64 lowercase hex characters"));
        }
        let busy = || {
            RpcError::new(
                ErrorCode::Busy,
                format!("at most {MAX_UPLOADS_PER_DEVICE} uploads can be in progress at the same time"),
            )
        };
        if self.uploads_of(device) >= MAX_UPLOADS_PER_DEVICE {
            return Err(busy());
        }
        // 磁盘：数据目录所在卷剩余空间要装得下这次上传，还要留出底线
        let reserve = self.disk_reserve.load(Ordering::Relaxed);
        let free = crate::doctor::disk_free(&self.dir);
        if free < p.size.saturating_add(reserve) {
            return Err(err_reason(
                ErrorCode::Busy,
                "the node is low on disk space; try again later",
                REASON_DISK_FULL,
            ));
        }
        // 配额：完成但没被引用的 blob（库里）加上进行中的上传（内存里）加上这一次，全节点合计有上限
        let quota = self.unreferenced_quota.load(Ordering::Relaxed);
        let committed =
            self.store.blobs_unreferenced_bytes().await.map_err(|e| internal("cannot read the blob records", e))?;
        let over_quota = |up: &HashMap<String, Upload>| {
            committed.saturating_add(up.values().map(|u| u.size).sum::<u64>()).saturating_add(p.size) > quota
        };
        let quota_error = || {
            err_reason(
                ErrorCode::Busy,
                "too many attachments are waiting to be sent; send or remove some and try again",
                REASON_QUOTA,
            )
        };
        if over_quota(&self.uploads.lock().unwrap()) {
            return Err(quota_error());
        }
        let id = format!("{:032x}", rand::thread_rng().gen::<u128>());
        let part = self.part_path(&id);
        let size = p.size;
        let file = {
            let part = part.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<File> {
                let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).owner_only().open(&part)?;
                f.set_len(size)?;
                Ok(f)
            })
            .await
            .map_err(|e| internal("cannot create the upload file", e))?
            .map_err(|e| internal("cannot create the upload file", e))?
        };
        let chunk_count = u32::try_from(p.size.div_ceil(u64::from(CHUNK_SIZE))).unwrap_or(u32::MAX);
        let now = self.now();
        {
            let mut up = self.uploads.lock().unwrap();
            // 创建文件期间别的请求可能也在开始上传：放进去之前再数一次
            if up.values().filter(|u| u.device == device).count() >= MAX_UPLOADS_PER_DEVICE {
                drop(up);
                let _ = std::fs::remove_file(&part);
                return Err(busy());
            }
            if over_quota(&up) {
                drop(up);
                let _ = std::fs::remove_file(&part);
                return Err(quota_error());
            }
            up.insert(
                id.clone(),
                Upload {
                    name: p.name,
                    mime,
                    size: p.size,
                    sha256: p.sha256,
                    device: device.to_string(),
                    chunk_count,
                    received: vec![false; chunk_count as usize],
                    last_active_ms: now,
                    file: Arc::new(file),
                    gate: Arc::new(AsyncRwLock::new(())),
                    finalizing: None,
                },
            );
        }
        tracing::debug!(blob = %id, bytes = p.size, image, "attachment upload started");
        Ok(BlobPutBeginResult {
            blob_id: id,
            chunk_size: CHUNK_SIZE,
            expires_at_ms: now + self.upload_ttl_ms.load(Ordering::Relaxed),
        })
    }

    fn uploads_of(&self, device: &str) -> usize {
        self.uploads.lock().unwrap().values().filter(|u| u.device == device).count()
    }

    /// `put_chunk` 看一眼上传：块的序号合法、没有在收尾，返回文件句柄、这一块应有的字节数和读写锁。
    fn chunk_plan(&self, id: &str, index: u32) -> RpcResult<(Arc<File>, u64, Arc<AsyncRwLock<()>>)> {
        let up = self.uploads.lock().unwrap();
        let u = up.get(id).ok_or_else(|| not_found_upload(id))?;
        if u.finalizing.is_some() {
            return Err(err_reason(
                ErrorCode::Invalid,
                "the upload is being finished; no more chunks are accepted",
                REASON_FINALIZING,
            ));
        }
        if index >= u.chunk_count {
            return Err(RpcError::new(
                ErrorCode::Invalid,
                format!("index {index} is out of range (the blob has {} chunks)", u.chunk_count),
            ));
        }
        let expected = if index + 1 == u.chunk_count {
            u.size - u64::from(index) * u64::from(CHUNK_SIZE)
        } else {
            u64::from(CHUNK_SIZE)
        };
        Ok((u.file.clone(), expected, u.gate.clone()))
    }

    /// `blob.put_chunk`：按偏移写一块。可乱序、并发、重发（幂等）。`put_end` 接手之后到达的块 → `invalid`（`finalizing`）。
    pub async fn put_chunk(&self, p: BlobPutChunkParams) -> RpcResult<()> {
        if !is_blob_id(&p.blob_id) {
            return Err(invalid_id());
        }
        self.expire_uploads();
        let (_, expected, gate) = self.chunk_plan(&p.blob_id, p.index)?;
        // 编码后的长度先于解码检查，免得为一个肯定不对的大字符串白白解码
        if (p.data.len() as u64) > expected.div_ceil(3) * 4 {
            return Err(RpcError::new(ErrorCode::Invalid, "the chunk is longer than expected"));
        }
        // 块拿读锁，持有到写完、记完账；拿到锁之后再看一遍：等锁的时候 `put_end` 可能已经接手了
        let guard = gate.read_owned().await;
        let (file, expected, _) = self.chunk_plan(&p.blob_id, p.index)?;
        let offset = u64::from(p.index) * u64::from(CHUNK_SIZE);
        let data = p.data;
        // 读锁跟着写走进阻塞线程再带回来：请求在中途被取消，锁也要等写做完才放
        let guard = tokio::task::spawn_blocking(move || -> RpcResult<OwnedRwLockReadGuard<()>> {
            let bytes = B64
                .decode(data.as_bytes())
                .map_err(|_| RpcError::new(ErrorCode::Invalid, "data is not valid base64"))?;
            if bytes.len() as u64 != expected {
                return Err(RpcError::new(
                    ErrorCode::Invalid,
                    format!("the chunk must be exactly {expected} bytes, got {}", bytes.len()),
                ));
            }
            write_all_at(&file, &bytes, offset).map_err(|e| internal("cannot write an upload chunk", e))?;
            Ok(guard)
        })
        .await
        .map_err(|e| internal("cannot write an upload chunk", e))??;
        let now = self.now();
        {
            let mut up = self.uploads.lock().unwrap();
            // 写的期间上传被丢弃 / 过期了
            let u = up.get_mut(&p.blob_id).ok_or_else(|| not_found_upload(&p.blob_id))?;
            u.received[p.index as usize] = true;
            u.last_active_ms = now;
        }
        drop(guard);
        Ok(())
    }

    /// `blob.put_end`：校验大小、SHA-256、文件头，落成正式文件，写库，返回 `AttachmentRef`。
    /// 对已完成的 blob 重复调用返回同一个结果；并发的第二个调用等第一个的结果（同一个 `AttachmentRef` 或同一个错误）。
    pub async fn put_end(self: &Arc<Self>, p: BlobIdParams) -> RpcResult<AttachmentRef> {
        if !is_blob_id(&p.blob_id) {
            return Err(invalid_id());
        }
        self.expire_uploads();
        let id = p.blob_id;
        // 在锁内决定自己是谁：已经有人在收尾 -> 等它；没有 -> 标成「收尾中」并接手；不在表里 -> 看库
        let rx = {
            let mut up = self.uploads.lock().unwrap();
            match up.get_mut(&id) {
                None => None,
                Some(u) => Some(match &u.finalizing {
                    Some(rx) => rx.clone(),
                    None => {
                        let missing = u.received.iter().filter(|r| !**r).count();
                        if missing > 0 {
                            return Err(err_reason(
                                ErrorCode::Invalid,
                                format!("{missing} chunk(s) have not been received"),
                                REASON_INCOMPLETE,
                            ));
                        }
                        let (tx, rx) = watch::channel(None);
                        u.finalizing = Some(rx.clone());
                        // 收尾期间不过期
                        u.last_active_ms = self.now();
                        let job = FinalizeJob {
                            name: u.name.clone(),
                            mime: u.mime.clone(),
                            size: u.size,
                            sha256: u.sha256.clone(),
                            file: u.file.clone(),
                            gate: u.gate.clone(),
                        };
                        // 收尾在自己的任务里跑：请求被取消也不会把上传留在半截
                        tokio::spawn(self.clone().finalize(id.clone(), job, tx));
                        rx
                    }
                }),
            }
        };
        let Some(rx) = rx else {
            // 不在进行中的上传里：已经完成的返回同一个结果，否则 `not_found`
            return match self.store.blob_get(&id).await.map_err(|e| internal("cannot read the blob record", e))? {
                Some(row) => Ok(Self::reference(&row)),
                None => Err(not_found_upload(&id)),
            };
        };
        match await_outcome(rx).await {
            Some(res) => res,
            None => {
                // 收尾任务没留下结果就没了（不应发生）：丢掉这个上传，客户端重新上传，不要让它一直卡在「收尾中」
                self.drop_upload(&id);
                Err(RpcError::new(ErrorCode::Internal, "the upload could not be finished"))
            }
        }
    }

    /// 收尾任务：等已经开始的块写完、校验、改名、写库。结果经 `tx` 交给所有在等的 `put_end`。
    async fn finalize(self: Arc<Self>, id: String, job: FinalizeJob, tx: watch::Sender<Outcome>) {
        let res = self.finalize_inner(&id, job).await;
        tx.send_replace(Some(res));
    }

    async fn finalize_inner(&self, id: &str, job: FinalizeJob) -> RpcResult<AttachmentRef> {
        // 写锁：拿到的时候，已经拿到文件句柄的块都写完了；之后到的块在 `chunk_plan` 里看到 `finalizing` 被拒绝
        let _writes_done = job.gate.clone().write_owned().await;
        let hold = self.finalize_hold.lock().unwrap().take();
        if let Some((reached, release)) = hold {
            reached.notify_one();
            release.notified().await;
        }
        // 算 SHA-256、（图片）读文件头和宽高：在阻塞线程里，一个 10 MiB 的文件不能占着运行时
        let verdict = {
            let (file, mime, sha, size) = (job.file.clone(), job.mime.clone(), job.sha256.clone(), job.size);
            tokio::task::spawn_blocking(move || verify(&file, size, &mime, &sha))
                .await
                .unwrap_or_else(|e| Err(Verdict::Io(std::io::Error::other(e.to_string()))))
        };
        let (width, height) = match verdict {
            Ok(dims) => dims,
            Err(Verdict::Hash) => {
                self.drop_upload(id);
                return Err(err_reason(
                    ErrorCode::Invalid,
                    "the SHA-256 of the data does not match",
                    REASON_HASH_MISMATCH,
                ));
            }
            Err(Verdict::Mime) => {
                self.drop_upload(id);
                return Err(err_reason(
                    ErrorCode::Invalid,
                    "the file header does not match the declared mime type",
                    REASON_MIME_MISMATCH,
                ));
            }
            Err(Verdict::Io(e)) => {
                // 读不出来不是客户端的错：上传留着，退出「收尾中」，客户端可以再 `put_end`
                if let Some(u) = self.uploads.lock().unwrap().get_mut(id) {
                    u.finalizing = None;
                }
                return Err(internal("cannot read the upload", e));
            }
        };
        let file_name = format!("{id}.{}", stored_ext(&job.name, &job.mime));
        let (part, dest) = (self.part_path(id), self.dir.join(&file_name));
        if let Err(e) = std::fs::rename(&part, &dest) {
            let _ = std::fs::remove_file(&part);
            self.drop_upload(id);
            return Err(internal("cannot store the attachment", e));
        }
        let row = BlobRow {
            id: id.to_string(),
            session_id: None,
            name: normalize_name(&job.name, &job.mime),
            mime: job.mime,
            size: job.size,
            sha256: job.sha256,
            width,
            height,
            path: file_name,
            created_ms: self.now(),
        };
        if let Err(e) = self.store.blob_insert(&row).await {
            let _ = std::fs::remove_file(&dest);
            self.drop_upload(id);
            return Err(internal("cannot record the attachment", e));
        }
        // 记录写好了才把上传从表里拿掉：之后任何一个看不到上传的 `put_end` 都能在库里查到它
        self.uploads.lock().unwrap().remove(id);
        tracing::debug!(blob = %id, bytes = row.size, "attachment stored");
        Ok(Self::reference(&row))
    }

    /// 丢掉一个进行中的上传和它的 `.part` 文件。
    fn drop_upload(&self, id: &str) {
        self.uploads.lock().unwrap().remove(id);
        let _ = std::fs::remove_file(self.part_path(id));
    }

    /// 作废 10 分钟没有新块的上传（每次调用顺手清，另外清理任务也周期性调用）。返回清掉的个数。
    pub fn expire_uploads(&self) -> usize {
        let now = self.now();
        let ttl = self.upload_ttl_ms.load(Ordering::Relaxed);
        let dead: Vec<String> = {
            let mut up = self.uploads.lock().unwrap();
            let ids: Vec<String> =
                up.iter().filter(|(_, u)| u.last_active_ms + ttl <= now).map(|(id, _)| id.clone()).collect();
            for id in &ids {
                up.remove(id);
            }
            ids
        };
        for id in &dead {
            tracing::debug!(blob = %id, "attachment upload expired");
            let _ = std::fs::remove_file(self.part_path(id));
        }
        dead.len()
    }

    // ============================================================ 下载 / 丢弃

    /// `blob.get`：按偏移读一段（base64）。
    pub async fn get(&self, p: BlobGetParams) -> RpcResult<BlobGetResult> {
        if !is_blob_id(&p.blob_id) {
            return Err(invalid_id());
        }
        let row = self
            .store
            .blob_get(&p.blob_id)
            .await
            .map_err(|e| internal("cannot read the blob record", e))?
            .ok_or_else(|| not_found_upload(&p.blob_id))?;
        if p.offset > row.size {
            return Err(RpcError::new(
                ErrorCode::Invalid,
                format!("offset {} is past the end of the blob ({} bytes)", p.offset, row.size),
            ));
        }
        let want = u64::from(p.max_bytes.unwrap_or(CHUNK_SIZE).clamp(1, CHUNK_SIZE));
        let len = want.min(row.size - p.offset) as usize;
        let path = self.path_of(&row);
        let offset = p.offset;
        let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            let f = File::open(&path)?;
            let mut buf = vec![0u8; len];
            read_exact_at(&f, &mut buf, offset)?;
            Ok(buf)
        })
        .await
        .map_err(|e| internal("cannot read the attachment", e))?
        .map_err(|e| {
            tracing::warn!(blob = %row.id, error = %e, "cannot read an attachment file");
            err_reason(ErrorCode::NotFound, "the attachment file is missing", REASON_BLOB)
        })?;
        Ok(BlobGetResult {
            blob: Self::reference(&row),
            offset: p.offset,
            data: B64.encode(&bytes),
            eof: p.offset + bytes.len() as u64 >= row.size,
        })
    }

    /// `blob.discard`：丢掉进行中的上传，或已完成但还没被引用的 blob；已被引用 → `invalid`（`in_use`）；不存在也成功。
    /// 上传正在收尾时先等它收完（成功了就按「已完成未引用」处理，丢掉）：不能在改名的当口删它的文件。
    pub async fn discard(&self, p: BlobIdParams) -> RpcResult<()> {
        if !is_blob_id(&p.blob_id) {
            return Err(invalid_id());
        }
        let id = p.blob_id;
        loop {
            let step = {
                let mut up = self.uploads.lock().unwrap();
                match up.get(&id).map(|u| u.finalizing.clone()) {
                    None => None,
                    Some(Some(rx)) => Some(Some(rx)),
                    Some(None) => {
                        up.remove(&id);
                        Some(None)
                    }
                }
            };
            match step {
                None => break,
                Some(None) => {
                    let _ = std::fs::remove_file(self.part_path(&id));
                    return Ok(());
                }
                Some(Some(rx)) => {
                    // 收尾结束后上传不在表里了（成功 / 失败）或者退回了「收件中」（读盘出错）：再看一遍
                    if await_outcome(rx).await.is_none() {
                        // 收尾任务没留下结果就没了（不应发生）：当作普通的上传丢掉，免得一直卡在「收尾中」
                        self.drop_upload(&id);
                        return Ok(());
                    }
                }
            }
        }
        let Some(row) = self.store.blob_get(&id).await.map_err(|e| internal("cannot read the blob record", e))? else {
            return Ok(());
        };
        let in_use = || err_reason(ErrorCode::Invalid, "the attachment is used by a message", REASON_IN_USE);
        if row.session_id.is_some() {
            return Err(in_use());
        }
        // 条件删除：和 `session.send` 的绑定竞争时，谁先谁赢
        if !self.store.blob_delete_unreferenced(&id).await.map_err(|e| internal("cannot delete the blob record", e))? {
            return match self.store.blob_get(&id).await.map_err(|e| internal("cannot read the blob record", e))? {
                Some(_) => Err(in_use()),
                None => Ok(()),
            };
        }
        self.remove_file(&row.path);
        Ok(())
    }

    fn remove_file(&self, name: &str) {
        if let Err(e) = std::fs::remove_file(self.dir.join(name)) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(file = %name, error = %e, "cannot remove an attachment file");
            }
        }
    }

    /// 删一批 blob 文件（`session.delete` 从库里取到的文件名）。
    pub fn remove_files(&self, names: &[String]) {
        for n in names {
            self.remove_file(n);
        }
    }

    // ============================================================ 回收

    /// 回收：作废过期的上传，删掉完成超过 24 小时仍没被引用的 blob（文件和行）。返回删掉的 blob 个数。
    /// 每小时的清理任务（`Sessions::cleanup`）调用。
    pub async fn gc(&self) -> usize {
        self.expire_uploads();
        let cutoff = self.now().saturating_sub(self.orphan_ttl_ms.load(Ordering::Relaxed));
        let old = match self.store.blobs_unreferenced_before(cutoff).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "cannot list unreferenced attachments");
                return 0;
            }
        };
        let mut n = 0;
        for (id, path) in old {
            match self.store.blob_delete_unreferenced(&id).await {
                Ok(true) => {
                    self.remove_file(&path);
                    n += 1;
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(blob = %id, error = %e, "cannot delete an unreferenced attachment"),
            }
        }
        if n > 0 {
            tracing::info!(count = n, "removed unreferenced attachments");
        }
        n
    }

    // ============================================================ 发送时的核对与绑定

    /// 核对一条消息的 `attachments`（protocol.md § 5.6），返回 daemon 的记录。`allowed` 是该会话 agent 的
    /// `capabilities.attachments`。`session_id` 是消息要发进的会话（新建会话时是还没存在的新 id）。
    ///
    /// - 至多 20 个，`blob_id` 不重复，字节合计 ≤ 20 MiB（`invalid`；合计超了是 `too_large`）；
    /// - blob 不存在 / 还没 `put_end` / 文件不见了 → `not_found`（`data.reason = "blob"`）；
    /// - `name` / `mime` / `bytes` 和记录不一致 → `invalid`；已被**别的**会话引用 → `invalid`（`blob_in_use`）；
    /// - mime 不在 `allowed` 里（也没有 `*/*`）→ `unsupported`。
    pub async fn resolve(
        &self,
        session_id: &str,
        refs: &[AttachmentRef],
        allowed: &[String],
    ) -> RpcResult<Vec<BlobRow>> {
        if refs.len() > MAX_ATTACHMENTS_PER_MESSAGE {
            return Err(RpcError::new(
                ErrorCode::Invalid,
                format!("a message can carry at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments"),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(refs.len());
        for r in refs {
            if !is_blob_id(&r.blob_id) {
                return Err(invalid_id());
            }
            if !seen.insert(r.blob_id.as_str()) {
                return Err(RpcError::new(ErrorCode::Invalid, "the same blob_id appears twice in one message"));
            }
            let missing = || {
                err_reason(
                    ErrorCode::NotFound,
                    format!("attachment {} does not exist (expired?)", r.blob_id),
                    REASON_BLOB,
                )
            };
            let row = self
                .store
                .blob_get(&r.blob_id)
                .await
                .map_err(|e| internal("cannot read the blob record", e))?
                .ok_or_else(missing)?;
            if !self.path_of(&row).is_file() {
                return Err(missing());
            }
            if r.name != row.name || r.mime != row.mime || r.bytes != row.size {
                return Err(RpcError::new(
                    ErrorCode::Invalid,
                    format!("attachment {} does not match the uploaded file (name, mime or bytes)", r.blob_id),
                ));
            }
            if row.session_id.as_deref().is_some_and(|s| s != session_id) {
                return Err(err_reason(
                    ErrorCode::Invalid,
                    format!("attachment {} already belongs to another session", r.blob_id),
                    REASON_BLOB_IN_USE,
                ));
            }
            if !accepts_attachment(allowed, &row.mime) {
                return Err(RpcError::new(
                    ErrorCode::Unsupported,
                    format!("this agent does not accept {} attachments", row.mime),
                ));
            }
            out.push(row);
        }
        // 一条消息的附件合计：Claude 一次请求有体积上限，超了整个会话的历史都会被它拖垮
        let total: u64 = out.iter().map(|r| r.size).sum();
        let limit = self.message_bytes_limit();
        if total > limit {
            return Err(err_reason(
                ErrorCode::Invalid,
                format!("the attachments of one message add up to {total} bytes; at most {limit} are accepted"),
                REASON_TOO_LARGE,
            ));
        }
        Ok(out)
    }

    /// 消息被接受时把 blob 绑定到会话（一个事务：要么全部，要么一个都不绑）。别的会话抢先了 → `invalid`（`blob_in_use`）；
    /// 校验之后 blob 被丢弃了 → `not_found`。
    pub async fn bind(&self, session_id: &str, rows: &[BlobRow]) -> RpcResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        match self.store.blobs_bind(session_id, &ids).await.map_err(|e| internal("cannot bind the attachments", e))? {
            Ok(()) => Ok(()),
            Err(id) => match self.store.blob_get(&id).await.map_err(|e| internal("cannot read the blob record", e))? {
                Some(_) => Err(err_reason(
                    ErrorCode::Invalid,
                    format!("attachment {id} already belongs to another session"),
                    REASON_BLOB_IN_USE,
                )),
                None => Err(err_reason(
                    ErrorCode::NotFound,
                    format!("attachment {id} does not exist (expired?)"),
                    REASON_BLOB,
                )),
            },
        }
    }
}

fn not_found_upload(id: &str) -> RpcError {
    RpcError::new(ErrorCode::NotFound, format!("upload {id} does not exist (finished, discarded or expired)"))
}

/// `put_end` 的校验结果。
enum Verdict {
    Hash,
    Mime,
    Io(std::io::Error),
}

/// 整个文件的 SHA-256 必须等于声明的值；四种图片类型的文件头必须和 mime 一致，返回读到的宽高（读不出、不是图片就是 `None`）。
fn verify(file: &File, size: u64, mime: &str, want_sha: &str) -> Result<(Option<u32>, Option<u32>), Verdict> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUF];
    let mut off = 0u64;
    while off < size {
        let n = ((size - off) as usize).min(HASH_BUF);
        read_exact_at(file, &mut buf[..n], off).map_err(Verdict::Io)?;
        hasher.update(&buf[..n]);
        off += n as u64;
    }
    if hex::encode(hasher.finalize()) != want_sha {
        return Err(Verdict::Hash);
    }
    if !is_image_mime(mime) {
        return Ok((None, None));
    }
    let mut head = [0u8; 16];
    let n = (size as usize).min(head.len());
    read_exact_at(file, &mut head[..n], 0).map_err(Verdict::Io)?;
    if sniff_mime(&head[..n]) != Some(mime) {
        return Err(Verdict::Mime);
    }
    let read = |off: u64, n: usize| -> Option<Vec<u8>> {
        if off.checked_add(n as u64)? > size {
            return None;
        }
        let mut b = vec![0u8; n];
        read_exact_at(file, &mut b, off).ok()?;
        Some(b)
    };
    let dims = image_size(mime, &read);
    Ok((dims.map(|d| d.0), dims.map(|d| d.1)))
}

/// 没有文字时，标题 / 摘要用的标签：`[image] 文件名`（多个时 `[image] 文件名 (+N)`）；有不是图片的附件时是 `[file]`。
pub fn attachment_label(refs: &[AttachmentRef]) -> String {
    let tag = if refs.iter().all(|r| is_image_mime(&r.mime)) { "[image]" } else { "[file]" };
    match refs {
        [] => String::new(),
        [one] => format!("{tag} {}", one.name),
        [first, rest @ ..] => format!("{tag} {} (+{})", first.name, rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一块内存当成「随机读」的来源。
    fn mem(bytes: &[u8]) -> impl Fn(u64, usize) -> Option<Vec<u8>> + '_ {
        move |off, n| {
            let (off, end) = (off as usize, off as usize + n);
            (end <= bytes.len()).then(|| bytes[off..end].to_vec())
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    #[test]
    fn blob_ids_are_32_lowercase_hex() {
        assert!(is_blob_id("0123456789abcdef0123456789abcdef"));
        for bad in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "../../../etc/passwd/0123456789abcd",
            "0123456789abcdef0123456789abcd/.",
            "0123456789abcdef0123456789abcd\0\0",
        ] {
            assert!(!is_blob_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn magic_bytes_decide_the_type() {
        assert_eq!(sniff_mime(&png(1, 1)), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF87a\x01\x00\x01\x00"), Some("image/gif"));
        assert_eq!(sniff_mime(b"GIF89a\x01\x00\x01\x00"), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("image/webp"));
        // RIFF 但不是 WebP（例如 WAV）
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        assert_eq!(sniff_mime(b"<svg xmlns="), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn png_gif_and_webp_sizes_come_from_the_header() {
        let p = png(1280, 800);
        assert_eq!(image_size("image/png", &mem(&p)), Some((1280, 800)));
        // 零宽高、残缺的头：读不出来，不算错
        assert_eq!(image_size("image/png", &mem(&png(0, 5))), None);
        assert_eq!(image_size("image/png", &mem(&p[..20])), None);

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x40, 0x01, 0xC8, 0x00]); // 320 × 200
        assert_eq!(image_size("image/gif", &mem(&gif)), Some((320, 200)));

        // VP8X：画布宽高 - 1（24 位）
        let mut vp8x = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        vp8x.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0]);
        vp8x.extend_from_slice(&[0x7F, 0x02, 0x00]); // 639 + 1
        vp8x.extend_from_slice(&[0xDF, 0x01, 0x00]); // 479 + 1
        assert_eq!(image_size("image/webp", &mem(&vp8x)), Some((640, 480)));

        // VP8L：签名 0x2F 之后 14 位 + 14 位
        let mut vp8l = b"RIFF\0\0\0\0WEBPVP8L".to_vec();
        vp8l.extend_from_slice(&[5, 0, 0, 0, 0x2F]);
        let bits: u32 = 99 | (49 << 14); // 100 × 50
        vp8l.extend_from_slice(&bits.to_le_bytes());
        vp8l.extend_from_slice(&[0; 5]);
        assert_eq!(image_size("image/webp", &mem(&vp8l)), Some((100, 50)));

        // VP8：帧头里的 14 位宽高（高 2 位是缩放）
        let mut vp8 = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        vp8.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0x9D, 0x01, 0x2A]);
        vp8.extend_from_slice(&[0x20, 0x01, 0x58, 0x00]); // 288 × 88
        assert_eq!(image_size("image/webp", &mem(&vp8)), Some((288, 88)));
    }

    #[test]
    fn jpeg_size_skips_other_segments_to_reach_sof() {
        let mut j = vec![0xFF, 0xD8];
        // APP1（EXIF）：长度 300
        j.extend_from_slice(&[0xFF, 0xE1, 0x01, 0x2C]);
        j.extend(std::iter::repeat_n(0u8, 298));
        // DHT 不是 SOF，要跳过
        j.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x04, 0, 0]);
        // SOF2（渐进式）：精度 8、高 600、宽 800
        j.extend_from_slice(&[
            0xFF, 0xC2, 0x00, 0x11, 8, 0x02, 0x58, 0x03, 0x20, 3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1,
        ]);
        assert_eq!(image_size("image/jpeg", &mem(&j)), Some((800, 600)));
        // 没有 SOF 就到了 SOS：读不出来
        let mut no_sof = vec![0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02];
        no_sof.extend_from_slice(&[0; 8]);
        assert_eq!(image_size("image/jpeg", &mem(&no_sof)), None);
        // 截断的文件不 panic
        assert_eq!(image_size("image/jpeg", &mem(&j[..10])), None);
    }

    #[test]
    fn names_are_checked() {
        assert!(validate_name("shot.png").is_ok());
        assert!(validate_name("截图 2026.png").is_ok());
        for bad in ["", "a/b.png", "a\\b.png", "a\nb.png", "a\0.png", &"x".repeat(256)] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name(&"x".repeat(255)).is_ok());
    }

    #[test]
    fn bidirectional_controls_and_line_separators_are_not_allowed_in_names() {
        // 双向控制符（让 `invoice\u{202E}gnp.exe` 看起来像 `invoiceexe.png`）、行 / 段分隔符
        for c in [
            '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}',
            '\u{2068}', '\u{2069}', '\u{2028}', '\u{2029}',
        ] {
            let name = format!("invoice{c}gnp.exe.png");
            assert!(validate_name(&name).is_err(), "U+{:04X}", c as u32);
        }
        // C1 控制符（Cc）本来就拒绝；边界上的相邻字符不受影响
        assert!(validate_name("a\u{85}b.png").is_err());
        for ok in
            ["a\u{200D}b.png", "a\u{202F}b.png", "a\u{2065}b.png", "a\u{206A}b.png", "カタカナ.png", "emoji 😀.png"]
        {
            assert!(validate_name(ok).is_ok(), "{ok:?}");
        }
    }

    #[test]
    fn the_recorded_name_gets_the_extension_of_its_mime() {
        // 已经对得上的原样保留（不分大小写；JPEG 认 jpg 和 jpeg）
        for (name, mime) in [
            ("shot.png", "image/png"),
            ("SHOT.PNG", "image/png"),
            ("a.b.png", "image/png"),
            ("p.jpg", "image/jpeg"),
            ("p.JPEG", "image/jpeg"),
            ("p.jpeg", "image/jpeg"),
            ("g.gif", "image/gif"),
            ("w.WebP", "image/webp"),
        ] {
            assert_eq!(normalize_name(name, mime), name, "{name} / {mime}");
        }
        // 对不上的在后面补：没有扩展名、别的扩展名、HTML 冒充、扩展名配错 mime
        for (name, mime, want) in [
            ("photo", "image/png", "photo.png"),
            ("photo.", "image/png", "photo.png"),
            ("x.html", "image/jpeg", "x.html.jpg"),
            ("x.svg", "image/gif", "x.svg.gif"),
            ("invoice.exe", "image/png", "invoice.exe.png"),
            ("pic.png", "image/jpeg", "pic.png.jpg"),
            ("pic.jpg", "image/webp", "pic.jpg.webp"),
            ("截图", "image/png", "截图.png"),
            (".png", "image/png", ".png.png"),
        ] {
            assert_eq!(normalize_name(name, mime), want, "{name} / {mime}");
        }
        // 补完超过 255 字节时截短原来的名字（在字符边界上），结果仍然合法
        let long = "x".repeat(255);
        let fixed = normalize_name(&long, "image/png");
        assert_eq!(fixed.len(), 255);
        assert!(fixed.ends_with("x.png") && validate_name(&fixed).is_ok());
        let wide = "图".repeat(85); // 255 字节
        let fixed = normalize_name(&wide, "image/webp");
        assert!(fixed.len() <= 255 && fixed.ends_with("图.webp") && validate_name(&fixed).is_ok(), "{fixed}");
    }

    #[test]
    fn labels_for_messages_without_text() {
        let a = |n: &str| AttachmentRef {
            blob_id: "x".into(),
            name: n.into(),
            mime: "image/png".into(),
            bytes: 1,
            width: None,
            height: None,
        };
        assert_eq!(attachment_label(&[]), "");
        assert_eq!(attachment_label(&[a("shot.png")]), "[image] shot.png");
        assert_eq!(attachment_label(&[a("a.png"), a("b.png"), a("c.png")]), "[image] a.png (+2)");
        let pdf = AttachmentRef { mime: "application/pdf".into(), ..a("spec.pdf") };
        assert_eq!(attachment_label(std::slice::from_ref(&pdf)), "[file] spec.pdf");
        assert_eq!(attachment_label(&[a("a.png"), pdf]), "[file] a.png (+1)");
    }

    #[test]
    fn mimes_are_checked_and_lowercased() {
        for (raw, want) in [
            ("image/png", "image/png"),
            ("application/pdf", "application/pdf"),
            ("Text/Plain", "text/plain"),
            (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("image/svg+xml", "image/svg+xml"),
            ("application/octet-stream", "application/octet-stream"),
        ] {
            assert_eq!(normalize_mime(raw).as_deref(), Some(want), "{raw}");
        }
        for bad in [
            "",
            "text",
            "/plain",
            "text/",
            "text/plain; charset=utf-8",
            "text/pl ain",
            "a/b/c",
            "текст/plain",
            "*/*",
            "text/*",
            ".x/y",
            "x/+y",
        ] {
            assert_eq!(normalize_mime(bad), None, "{bad:?}");
        }
        assert_eq!(normalize_mime(&format!("application/{}", "x".repeat(115))).map(|m| m.len()), Some(127));
        assert_eq!(normalize_mime(&format!("application/{}", "x".repeat(116))), None);
        assert!(is_image_mime("image/webp"));
        assert!(!is_image_mime("image/svg+xml"));
        assert!(!is_image_mime("application/pdf"));
    }

    #[test]
    fn stored_extension_comes_from_the_mime_or_a_plain_name_extension() {
        assert_eq!(stored_ext("x.html", "image/jpeg"), "jpg");
        assert_eq!(stored_ext("spec.PDF", "application/pdf"), "pdf");
        assert_eq!(stored_ext("archive.tar.gz", "application/gzip"), "gz");
        assert_eq!(stored_ext("Makefile", "text/plain"), "bin");
        assert_eq!(stored_ext(".bashrc", "text/plain"), "bin");
        assert_eq!(stored_ext("a.", "text/plain"), "bin");
        assert_eq!(stored_ext("a.tar-gz", "application/gzip"), "bin");
        assert_eq!(stored_ext("a.文档", "text/plain"), "bin");
        assert_eq!(stored_ext(&format!("a.{}", "x".repeat(17)), "text/plain"), "bin");
        assert_eq!(stored_ext(&format!("a.{}", "x".repeat(16)), "text/plain"), "x".repeat(16));

        let id = "0123456789abcdef0123456789abcdef";
        for ok in ["png", "pdf", "bin", "mp4", "x".repeat(16).as_str()] {
            assert!(is_stored_file_name(&format!("{id}.{ok}")), "{ok}");
        }
        for bad in [
            id.to_string(),
            format!("{id}.PDF"),
            format!("{id}.tar.gz"),
            format!("{id}."),
            format!("{id}.{}", "x".repeat(17)),
            "notanid.png".into(),
        ] {
            assert!(!is_stored_file_name(&bad), "{bad}");
        }
    }
}
