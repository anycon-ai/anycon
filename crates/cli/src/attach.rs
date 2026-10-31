//! `anycon send --image` / `--file`（ADR-35）：先在本地看一眼附件（文件头、大小、个数），再经 blob 通道传给 daemon
//! （`blob.put_begin` → 每块 `chunk_size`、最多 2 块在途的 `blob.put_chunk` → `blob.put_end`），消息只带 `AttachmentRef`。
//!
//! - `--image`：文件头必须是 PNG / JPEG / GIF / WebP，类型按文件头定。
//! - `--file`：任意文件。文件头是四种图片之一就按图片传（agent 把它当图片看）；否则按扩展名猜类型（[`guess_mime`]），
//!   猜不出是 `application/octet-stream`。agent 收到的是 daemon 上的路径，由它自己读。
//!
//! CLI 不缩放、不重编码图片。daemon 是唯一的校验点（每个上限 `sessions.max_attachment_bytes`、SHA-256、图片的文件头），
//! 这里的检查只是让明显不对的输入在上传之前就得到看得懂的错误：不是文件、空文件、`--image` 的文件头不对、
//! 超过每条消息固定的 20 个 / 20 MiB。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_daemon::blobs::{image_size, is_image_mime, sniff_mime, MAX_ATTACHMENTS_PER_MESSAGE, MAX_MESSAGE_BYTES};
use anycon_daemon::util::human_bytes;
use anycon_protocol::{methods, AttachmentRef, BlobPutBeginResult};
use base64::Engine as _;
use futures::{StreamExt as _, TryStreamExt as _};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::conn::Conn;
use crate::error::{CliError, Result};

/// 同时在途的块数（和 Web / SDK 一样）。
const MAX_CHUNKS_IN_FLIGHT: usize = 2;
/// daemon 给的 `chunk_size` 超出这个范围当作协议错误，不照着切。
const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// 每块（和 `put_end`）至少等这么久：经 hub 的慢速上行一块就要十几秒。
const MIN_UPLOAD_TIMEOUT: Duration = Duration::from_secs(90);
/// 超过就在 stderr 提醒（不拦）：Claude 对长边 > 2000 px 的图要先缩，而进了会话历史的图每个 turn 都会重发。
const ADVISED_EDGE: u32 = 2000;
const ADVISED_BYTES: u64 = 1536 * 1024;

/// 按扩展名猜的类型（不分大小写）。四种图片不在表里：图片只认文件头，叫 `.png` 的别的东西按普通文件传。
const MIME_BY_EXT: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("log", "text/plain"),
    ("md", "text/markdown"),
    ("markdown", "text/markdown"),
    ("csv", "text/csv"),
    ("tsv", "text/tab-separated-values"),
    ("json", "application/json"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("xml", "application/xml"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("toml", "application/toml"),
    ("svg", "image/svg+xml"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tgz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("cjs", "text/javascript"),
    ("jsx", "text/javascript"),
    ("ts", "text/x-typescript"),
    ("tsx", "text/x-typescript"),
    ("py", "text/x-python"),
    ("rs", "text/x-rust"),
    ("go", "text/x-go"),
    ("java", "text/x-java"),
    ("c", "text/x-c"),
    ("h", "text/x-c"),
    ("cpp", "text/x-c++"),
    ("hpp", "text/x-c++"),
    ("sh", "application/x-sh"),
    ("sql", "application/sql"),
    ("diff", "text/x-diff"),
    ("patch", "text/x-diff"),
];

/// 不知道类型的文件。
pub const DEFAULT_FILE_MIME: &str = "application/octet-stream";

/// `--file` 的类型：文件头是四种图片之一就是那种图片；否则按扩展名查 [`MIME_BY_EXT`]；再不行是 `application/octet-stream`。
/// 和 TS SDK 的 `guessMime` 同一套规则。
pub fn guess_mime(bytes: &[u8], name: &str) -> &'static str {
    if let Some(m) = sniff_mime(bytes) {
        return m;
    }
    let ext = name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty()).map(|(_, e)| e.to_ascii_lowercase());
    ext.and_then(|e| MIME_BY_EXT.iter().find(|(x, _)| *x == e).map(|(_, m)| *m)).unwrap_or(DEFAULT_FILE_MIME)
}

/// 读进内存、看过文件头的一个附件。
pub struct LocalFile {
    pub path: PathBuf,
    pub name: String,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
    /// 图片的宽高（不是图片、或读不出来时 `None`）。
    pub dims: Option<(u32, u32)>,
}

impl LocalFile {
    /// 图片太大 / 太高时给用户的提醒（`None` = 没问题，或者不是图片）。
    pub fn advice(&self) -> Option<String> {
        if !is_image_mime(self.mime) {
            return None;
        }
        let long_edge = self.dims.map(|(w, h)| w.max(h)).unwrap_or(0);
        let big = self.bytes.len() as u64 > ADVISED_BYTES;
        if long_edge <= ADVISED_EDGE && !big {
            return None;
        }
        let dims = self.dims.map(|(w, h)| format!("{w}×{h}, ")).unwrap_or_default();
        Some(format!(
            "warning: {} is {dims}{}; anycon does not resize images, and an agent resends every image in the \
             conversation with each turn. Images up to {ADVISED_EDGE} px on the long edge and about 1.5 MiB work best.",
            self.path.display(),
            human_bytes(self.bytes.len() as u64)
        ))
    }
}

/// 读一张图并检查：是普通文件、非空、不超过每条消息的总上限、文件头是四种图片之一。
pub fn load_image(path: &Path) -> Result<LocalFile> {
    load(path, true)
}

/// 读一个任意文件并检查：是普通文件、非空、不超过每条消息的总上限。类型见 [`guess_mime`]。
pub fn load_file(path: &Path) -> Result<LocalFile> {
    load(path, false)
}

fn load(path: &Path, image_only: bool) -> Result<LocalFile> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| CliError::usage(format!("cannot read {shown}: {e}")))?;
    if !meta.is_file() {
        return Err(CliError::usage(format!("{shown} is not a file")));
    }
    if meta.len() == 0 {
        return Err(CliError::usage(format!("{shown} is empty")));
    }
    if meta.len() > MAX_MESSAGE_BYTES {
        return Err(CliError::usage(format!(
            "{shown} is {}; the attachments of one message can add up to at most {}",
            human_bytes(meta.len()),
            human_bytes(MAX_MESSAGE_BYTES)
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| CliError::usage(format!("cannot read {shown}: {e}")))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| if image_only { "image" } else { "file" }.to_string());
    let mime = match sniff_mime(&bytes) {
        Some(m) => m,
        None if image_only => {
            return Err(CliError::usage(format!(
                "{shown} is not a PNG, JPEG, GIF or WebP image (use --file to attach other files)"
            )))
        }
        None => guess_mime(&bytes, &name),
    };
    let read = |off: u64, n: usize| {
        let start = usize::try_from(off).ok()?;
        bytes.get(start..start.checked_add(n)?).map(<[u8]>::to_vec)
    };
    let dims = image_size(mime, &read);
    Ok(LocalFile { path: path.to_path_buf(), name, mime, bytes, dims })
}

/// 读一条消息的全部附件（`--image` 的在前，`--file` 的在后）：个数和合计大小不超过每条消息的上限（20 个 / 20 MiB，固定）。
pub fn load_attachments(images: &[PathBuf], files: &[PathBuf]) -> Result<Vec<LocalFile>> {
    let count = images.len() + files.len();
    if count > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(CliError::usage(format!(
            "at most {MAX_ATTACHMENTS_PER_MESSAGE} attachments can go with one message ({count} given)"
        )));
    }
    let loaded =
        images.iter().map(|p| load_image(p)).chain(files.iter().map(|p| load_file(p))).collect::<Result<Vec<_>>>()?;
    let total: u64 = loaded.iter().map(|i| i.bytes.len() as u64).sum();
    if total > MAX_MESSAGE_BYTES {
        return Err(CliError::usage(format!(
            "the attachments add up to {}; one message can carry at most {}",
            human_bytes(total),
            human_bytes(MAX_MESSAGE_BYTES)
        )));
    }
    Ok(loaded)
}

/// 一个一个上传（同一节点同时只有一个在传）。任何一张失败：丢掉已经传完的，返回错误。
pub async fn upload_all(conn: &Conn, files: &[LocalFile]) -> Result<Vec<AttachmentRef>> {
    let mut refs = Vec::with_capacity(files.len());
    for img in files {
        match upload(conn, img).await {
            Ok(r) => refs.push(r),
            Err(e) => {
                discard_all(conn, &refs).await;
                return Err(e);
            }
        }
    }
    Ok(refs)
}

/// 尽力丢掉没被消息引用的 blob（失败不管：daemon 24 小时后自己回收；已被引用的会得到 `in_use`，也不管）。
pub async fn discard_all(conn: &Conn, refs: &[AttachmentRef]) {
    for r in refs {
        let _ = conn.call_raw(methods::BLOB_DISCARD, json!({ "blob_id": r.blob_id })).await;
    }
}

async fn upload(conn: &Conn, img: &LocalFile) -> Result<AttachmentRef> {
    let sha256 = hex::encode(Sha256::digest(&img.bytes));
    let size = img.bytes.len();
    let begin: BlobPutBeginResult = conn
        .call(methods::BLOB_PUT_BEGIN, json!({ "name": img.name, "mime": img.mime, "size": size, "sha256": sha256 }))
        .await
        .map_err(|e| about(img, e))?;
    let id = begin.blob_id.clone();
    let done = async {
        let chunk = usize::try_from(begin.chunk_size).unwrap_or(0);
        if chunk == 0 || chunk > MAX_CHUNK_BYTES {
            return Err(CliError::general(format!("blob.put_begin returned an unusable chunk_size ({chunk})")));
        }
        let timeout = conn.timeout().max(MIN_UPLOAD_TIMEOUT);
        futures::stream::iter(img.bytes.chunks(chunk).enumerate())
            .map(|(index, part)| {
                let data = base64::engine::general_purpose::STANDARD.encode(part);
                conn.call_timeout(
                    methods::BLOB_PUT_CHUNK,
                    json!({ "blob_id": id, "index": index, "data": data }),
                    timeout,
                )
            })
            .buffer_unordered(MAX_CHUNKS_IN_FLIGHT)
            .try_collect::<Vec<_>>()
            .await?;
        let v = conn.call_timeout(methods::BLOB_PUT_END, json!({ "blob_id": id }), timeout).await?;
        serde_json::from_value::<AttachmentRef>(v)
            .map_err(|e| CliError::general(format!("unexpected response to {}: {e}", methods::BLOB_PUT_END)))
    }
    .await;
    if done.is_err() {
        let _ = conn.call_raw(methods::BLOB_DISCARD, json!({ "blob_id": id })).await;
    }
    done.map_err(|e| about(img, e))
}

/// 错误里带上是哪个文件。
fn about(img: &LocalFile, e: CliError) -> CliError {
    CliError { message: format!("{}: {}", img.path.display(), e.message), ..e }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, total: usize) -> Vec<u8> {
        let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        v.extend_from_slice(b"IHDR");
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.resize(total.max(v.len()), 7);
        v
    }

    #[test]
    fn local_checks_catch_the_obvious_mistakes_before_uploading() {
        let d = tempfile::tempdir().unwrap();
        let ok = d.path().join("shot.png");
        std::fs::write(&ok, png(1280, 800, 4096)).unwrap();
        let img = load_image(&ok).unwrap();
        assert_eq!(
            (img.name.as_str(), img.mime, img.dims, img.bytes.len()),
            ("shot.png", "image/png", Some((1280, 800)), 4096)
        );
        assert!(img.advice().is_none());

        // 文件头说了算，扩展名不算：叫 .png 的 SVG 被拒，叫 .bin 的 JPEG 照收
        let svg = d.path().join("x.png");
        std::fs::write(&svg, b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
        let e = load_image(&svg).err().unwrap();
        assert_eq!((e.exit, e.code.as_str()), (crate::error::EXIT_USAGE, "invalid"));
        assert!(e.message.contains("not a PNG, JPEG, GIF or WebP image"), "{}", e.message);
        // `--file` 收同一个文件：文件头不是图片，按扩展名猜，猜不出是 octet-stream
        let f = load_file(&svg).unwrap();
        assert_eq!((f.name.as_str(), f.mime, f.dims), ("x.png", DEFAULT_FILE_MIME, None));
        assert!(f.advice().is_none());
        assert_eq!(load_file(&ok).unwrap().mime, "image/png", "a real image stays an image");
        let jpg = d.path().join("photo.bin");
        std::fs::write(&jpg, [0xFF, 0xD8, 0xFF, 0xE0, 0, 16]).unwrap();
        assert_eq!(load_image(&jpg).unwrap().mime, "image/jpeg");

        let empty = d.path().join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(load_image(&empty).err().unwrap().message.contains("is empty"));
        assert!(load_image(d.path()).err().unwrap().message.contains("is not a file"));
        assert!(load_image(&d.path().join("missing.png")).err().unwrap().message.contains("cannot read"));

        // 每条消息的固定上限：单张超过 20 MiB 在读之前就拒；合计超过也拒；最多 20 张
        let huge = d.path().join("huge.png");
        std::fs::write(&huge, png(10, 10, MAX_MESSAGE_BYTES as usize + 1)).unwrap();
        assert!(load_image(&huge).err().unwrap().message.contains("at most 20.0 MiB"));
        let half = d.path().join("half.png");
        std::fs::write(&half, png(10, 10, MAX_MESSAGE_BYTES as usize / 2 + 1)).unwrap();
        let e = load_attachments(std::slice::from_ref(&half), std::slice::from_ref(&half)).err().unwrap();
        assert!(e.message.contains("add up to"), "{}", e.message);
        let many = vec![ok.clone(); MAX_ATTACHMENTS_PER_MESSAGE];
        assert!(load_attachments(&many, std::slice::from_ref(&ok))
            .err()
            .unwrap()
            .message
            .contains("at most 20 attachments"));
        let both = load_attachments(std::slice::from_ref(&ok), &[svg, ok.clone()]).unwrap();
        assert_eq!(both.iter().map(|f| f.mime).collect::<Vec<_>>(), ["image/png", DEFAULT_FILE_MIME, "image/png"]);
    }

    #[test]
    fn file_types_come_from_the_header_then_the_extension() {
        assert_eq!(guess_mime(&png(1, 1, 64), "whatever.txt"), "image/png");
        assert_eq!(guess_mime(b"%PDF-1.7", "Spec.PDF"), "application/pdf");
        assert_eq!(guess_mime(b"fn main() {}", "main.rs"), "text/x-rust");
        assert_eq!(guess_mime(b"x", "archive.tar.gz"), "application/gzip");
        assert_eq!(guess_mime(b"x", "fake.png"), DEFAULT_FILE_MIME);
        assert_eq!(guess_mime(b"x", ".bashrc"), DEFAULT_FILE_MIME);
        assert_eq!(guess_mime(b"x", "Makefile"), DEFAULT_FILE_MIME);
        for (_, m) in MIME_BY_EXT {
            assert_eq!(anycon_daemon::blobs::normalize_mime(m).as_deref(), Some(*m), "{m}");
        }
    }

    #[test]
    fn big_or_tall_images_get_advice_but_are_not_refused() {
        let d = tempfile::tempdir().unwrap();
        let tall = d.path().join("tall.png");
        std::fs::write(&tall, png(1200, 4000, 2048)).unwrap();
        let advice = load_image(&tall).unwrap().advice().unwrap();
        assert!(advice.contains("1200×4000") && advice.contains("does not resize"), "{advice}");
        let heavy = d.path().join("heavy.png");
        std::fs::write(&heavy, png(800, 600, 2 * 1024 * 1024)).unwrap();
        assert!(load_image(&heavy).unwrap().advice().unwrap().contains("2.0 MiB"));
    }
}
