//! 图片附件（ADR-35，docs/spec/daemon.md § 7.10，protocol.md § 5.6）：blob 通道的上传 / 校验 / 下载 / 丢弃 / 回收，
//! 以及 `session.send` / `session.create` 对附件的核对、绑定、合并和交给 adapter。
//!
//! 大部分用例用假 adapter（`td.fake.handle(n).inputs` 能看到交给 adapter 的 `UserInput`）；
//! 最后一组用真的 mock adapter 验证它读到的是 daemon 给的路径（回复里回显文件名 / 大小 / sha256 前 12 位）。

use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::blobs::CHUNK_SIZE;
use anycon_daemon::testutil::{eventually, InterjectFailure, TestClient, TestDaemon};
use anycon_protocol::*;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const T: Duration = Duration::from_secs(10);
const PNG: &str = "image/png";

// ---------------------------------------------------------------- 夹具

/// 文件头合法的 PNG（IHDR 里是给定的宽高），后面补确定性的伪随机字节到 `total` 字节。
fn png(w: u32, h: u32, total: usize, seed: u8) -> Vec<u8> {
    let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut x = u32::from(seed) + 1;
    while v.len() < total {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 24) as u8);
    }
    v
}

fn small_png(seed: u8) -> Vec<u8> {
    png(640, 480, 3000, seed)
}

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn reason(e: &RpcError) -> Option<&str> {
    e.data.as_ref().and_then(|d| d.reason.as_deref())
}

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

async fn begin(td: &TestDaemon, c: &mut TestClient, name: &str, mime: &str, bytes: &[u8]) -> BlobPutBeginResult {
    c.call(
        td,
        methods::BLOB_PUT_BEGIN,
        json!({ "name": name, "mime": mime, "size": bytes.len(), "sha256": sha(bytes) }),
    )
    .await
    .unwrap()
}

async fn chunk(td: &TestDaemon, c: &mut TestClient, id: &str, index: u32, data: &[u8]) -> Result<Empty, RpcError> {
    c.call(td, methods::BLOB_PUT_CHUNK, json!({ "blob_id": id, "index": index, "data": b64(data) })).await
}

async fn end(td: &TestDaemon, c: &mut TestClient, id: &str) -> Result<AttachmentRef, RpcError> {
    c.call(td, methods::BLOB_PUT_END, json!({ "blob_id": id })).await
}

async fn upload_as(td: &TestDaemon, c: &mut TestClient, name: &str, mime: &str, bytes: &[u8]) -> AttachmentRef {
    let b = begin(td, c, name, mime, bytes).await;
    assert_eq!(b.chunk_size, CHUNK_SIZE);
    for (i, part) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        chunk(td, c, &b.blob_id, i as u32, part).await.unwrap();
    }
    end(td, c, &b.blob_id).await.unwrap()
}

async fn upload(td: &TestDaemon, c: &mut TestClient, name: &str, bytes: &[u8]) -> AttachmentRef {
    upload_as(td, c, name, PNG, bytes).await
}

async fn get(
    td: &TestDaemon,
    c: &mut TestClient,
    id: &str,
    offset: u64,
    max: Option<u32>,
) -> Result<BlobGetResult, RpcError> {
    c.call(td, methods::BLOB_GET, json!({ "blob_id": id, "offset": offset, "max_bytes": max })).await
}

async fn discard(td: &TestDaemon, c: &mut TestClient, id: &str) -> Result<Empty, RpcError> {
    c.call(td, methods::BLOB_DISCARD, json!({ "blob_id": id })).await
}

async fn new_session(td: &TestDaemon, c: &mut TestClient) -> SessionSummary {
    c.call(td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap()
}

async fn send(
    td: &TestDaemon,
    c: &mut TestClient,
    sid: &str,
    mid: &str,
    text: &str,
    atts: &[&AttachmentRef],
) -> Result<SessionSendResult, RpcError> {
    c.call(
        td,
        methods::SESSION_SEND,
        json!({ "session_id": sid, "message_id": mid, "content": text, "attachments": atts }),
    )
    .await
}

async fn summary(td: &TestDaemon, c: &mut TestClient, sid: &str) -> SessionSummary {
    c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap()
}

async fn wait_status(td: &TestDaemon, c: &mut TestClient, sid: &str, want: SessionStatus) {
    let t = std::time::Instant::now();
    loop {
        let s = summary(td, c, sid).await.status;
        if s == want {
            return;
        }
        assert!(t.elapsed() < T, "the session never reached {want:?}; it is {s:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let r: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    r.events
}

/// 历史里所有 `user_message` 事件：`(message_id, content, attachments)`。
async fn user_messages(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<(String, String, Vec<AttachmentRef>)> {
    history(td, c, sid)
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::UserMessage { message_id, content, attachments, .. } => Some((message_id, content, attachments)),
            _ => None,
        })
        .collect()
}

fn blob_file(td: &TestDaemon, a: &AttachmentRef, ext: &str) -> std::path::PathBuf {
    td.paths.blobs_dir().join(format!("{}.{ext}", a.blob_id))
}

fn part_file(td: &TestDaemon, id: &str) -> std::path::PathBuf {
    td.paths.blobs_tmp().join(format!("{id}.part"))
}

async fn bound_session(td: &TestDaemon, id: &str) -> Option<String> {
    td.ctx.store.blob_get(id).await.unwrap().and_then(|r| r.session_id)
}

// ---------------------------------------------------------------- 主流程

#[tokio::test]
async fn upload_send_and_the_agent_receives_the_right_files() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.blobs, "the daemon advertises the blob channel");

    let bytes = png(1280, 800, 5000, 1);
    let a = upload(&td, &mut c, "shot.png", &bytes).await;
    assert_eq!((a.name.as_str(), a.mime.as_str(), a.bytes), ("shot.png", PNG, 5000));
    assert_eq!((a.width, a.height), (Some(1280), Some(800)), "the daemon reads the size from the header");
    assert_eq!(a.blob_id.len(), 32);

    // 落盘：<数据目录>/blobs/<id>.png，文件 0600、目录 0700，内容一致；上传中的文件不在了
    let file = blob_file(&td, &a, "png");
    assert_eq!(std::fs::read(&file).unwrap(), bytes);
    anycon_daemon::testutil::assert_mode(&file, 0o600);
    anycon_daemon::testutil::assert_mode(&td.paths.blobs_dir(), 0o700);
    assert!(!part_file(&td, &a.blob_id).exists());
    // 还没被引用
    assert_eq!(bound_session(&td, &a.blob_id).await, None);

    let s = new_session(&td, &mut c).await;
    // 客户端原样放回 `put_end` 的结果；也可以不带宽高（以 daemon 的记录为准）
    let mut sloppy = a.clone();
    sloppy.width = None;
    sloppy.height = None;
    let r = send(&td, &mut c, &s.id, "m1", "what is this?", &[&sloppy]).await.unwrap();
    assert!(!r.duplicate);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    // adapter 收到：附件和路径一一对应，路径是正式文件的绝对路径
    let inputs = td.fake.handle(0).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].attachments, vec![a.clone()]);
    assert_eq!(inputs[0].attachment_paths, vec![file.clone()]);
    assert!(inputs[0].attachment_paths[0].is_absolute());
    assert_eq!(std::fs::read(&inputs[0].attachment_paths[0]).unwrap(), bytes);
    assert_eq!(inputs[0].content, "what is this?");

    // 事件里带 daemon 记录的引用（含宽高）
    let msgs = user_messages(&td, &mut c, &s.id).await;
    assert_eq!(msgs, vec![("m1".to_string(), "what is this?".to_string(), vec![a.clone()])]);
    // 绑定到会话
    assert_eq!(bound_session(&td, &a.blob_id).await.as_deref(), Some(s.id.as_str()));
    // 绑定之后的 blob 仍可下载
    let g = get(&td, &mut c, &a.blob_id, 0, None).await.unwrap();
    assert_eq!(g.blob, a);
    td.shutdown().await;
}

#[tokio::test]
async fn chunks_may_arrive_out_of_order_and_be_resent() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 1.3 个块：3 块（512 KiB、512 KiB、余下的）
    let total = CHUNK_SIZE as usize * 2 + 1000;
    let bytes = png(100, 50, total, 3);
    let parts: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE as usize).collect();
    assert_eq!(parts.len(), 3);
    let b = begin(&td, &mut c, "big.png", PNG, &bytes).await;
    // 2, 0, 重发 0, 1, 重发 2
    chunk(&td, &mut c, &b.blob_id, 2, parts[2]).await.unwrap();
    chunk(&td, &mut c, &b.blob_id, 0, parts[0]).await.unwrap();
    chunk(&td, &mut c, &b.blob_id, 0, parts[0]).await.unwrap();
    chunk(&td, &mut c, &b.blob_id, 1, parts[1]).await.unwrap();
    chunk(&td, &mut c, &b.blob_id, 2, parts[2]).await.unwrap();
    let a = end(&td, &mut c, &b.blob_id).await.unwrap();
    assert_eq!(a.bytes, total as u64);
    assert_eq!(std::fs::read(blob_file(&td, &a, "png")).unwrap(), bytes);
    // 重复的 put_end（响应丢了、客户端超时重试）返回同一个结果，不重复写库、不动文件
    assert_eq!(end(&td, &mut c, &b.blob_id).await.unwrap(), a);
    assert_eq!(end(&td, &mut c, &b.blob_id).await.unwrap(), a);
    assert_eq!(std::fs::read(blob_file(&td, &a, "png")).unwrap(), bytes);
    td.shutdown().await;
}

#[tokio::test]
async fn chunks_are_validated() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let total = CHUNK_SIZE as usize + 10;
    let bytes = png(10, 10, total, 4);
    let b = begin(&td, &mut c, "x.png", PNG, &bytes).await;
    // 长度不对（第一块必须正好 512 KiB，最后一块是余数）
    let e = chunk(&td, &mut c, &b.blob_id, 0, &bytes[..100]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let e = chunk(&td, &mut c, &b.blob_id, 1, &bytes[CHUNK_SIZE as usize..CHUNK_SIZE as usize + 9]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // index 越界
    let e = chunk(&td, &mut c, &b.blob_id, 2, &bytes[..10]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // base64 不合法
    let e = c
        .call::<Empty>(
            &td,
            methods::BLOB_PUT_CHUNK,
            json!({ "blob_id": b.blob_id, "index": 0, "data": "!!!not base64!!!" }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 上传不存在
    let e = chunk(&td, &mut c, &"0".repeat(32), 0, &bytes[..10]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // 上面的失败都没有留下半截数据：补齐正确的块之后能成功
    for (i, part) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        chunk(&td, &mut c, &b.blob_id, i as u32, part).await.unwrap();
    }
    end(&td, &mut c, &b.blob_id).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn a_missing_chunk_keeps_the_upload_and_can_be_completed_later() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let bytes = png(10, 10, CHUNK_SIZE as usize * 2, 5);
    let parts: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE as usize).collect();
    let b = begin(&td, &mut c, "x.png", PNG, &bytes).await;
    chunk(&td, &mut c, &b.blob_id, 0, parts[0]).await.unwrap();
    let e = end(&td, &mut c, &b.blob_id).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("incomplete")));
    // 上传保留：补发之后再 put_end
    assert!(part_file(&td, &b.blob_id).exists());
    chunk(&td, &mut c, &b.blob_id, 1, parts[1]).await.unwrap();
    let a = end(&td, &mut c, &b.blob_id).await.unwrap();
    assert_eq!(std::fs::read(blob_file(&td, &a, "png")).unwrap(), bytes);
    td.shutdown().await;
}

#[tokio::test]
async fn a_hash_mismatch_discards_the_upload() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let bytes = small_png(6);
    // 声明的 sha256 是别的数据的
    let b: BlobPutBeginResult = c
        .call(
            &td,
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "x.png", "mime": PNG, "size": bytes.len(), "sha256": sha(b"something else") }),
        )
        .await
        .unwrap();
    chunk(&td, &mut c, &b.blob_id, 0, &bytes).await.unwrap();
    let e = end(&td, &mut c, &b.blob_id).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("hash_mismatch")));
    // 上传作废：文件删了，也没有留下正式文件
    assert!(!part_file(&td, &b.blob_id).exists());
    assert!(!td.paths.blobs_dir().join(format!("{}.png", b.blob_id)).exists());
    assert_eq!(chunk(&td, &mut c, &b.blob_id, 0, &bytes).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    assert_eq!(end(&td, &mut c, &b.blob_id).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    td.shutdown().await;
}

fn dummy_ref() -> AttachmentRef {
    AttachmentRef { blob_id: String::new(), name: "x".into(), mime: PNG.into(), bytes: 1, width: None, height: None }
}

#[tokio::test]
async fn magic_bytes_must_match_the_declared_mime() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 声明 JPEG，实际是 PNG
    let bytes = small_png(7);
    let b = begin(&td, &mut c, "x.jpg", "image/jpeg", &bytes).await;
    chunk(&td, &mut c, &b.blob_id, 0, &bytes).await.unwrap();
    let e = end(&td, &mut c, &b.blob_id).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("mime_mismatch")));
    assert!(!part_file(&td, &b.blob_id).exists(), "the upload is discarded");
    // 声明 PNG，实际是 SVG 文本
    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>".to_vec();
    let b = begin(&td, &mut c, "x.png", PNG, &svg).await;
    chunk(&td, &mut c, &b.blob_id, 0, &svg).await.unwrap();
    let e = end(&td, &mut c, &b.blob_id).await.unwrap_err();
    assert_eq!(reason(&e), Some("mime_mismatch"));
    td.shutdown().await;
}

#[tokio::test]
async fn each_whitelisted_type_is_accepted_with_its_extension_and_size() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0x40, 0x01, 0xC8, 0x00]); // 320 × 200
    gif.extend_from_slice(&[0; 20]);
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0];
    jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8, 0x02, 0x58, 0x03, 0x20, 1, 1, 0x11, 0]); // 800 × 600
    jpeg.extend_from_slice(&[0xFF, 0xD9]);
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    webp.extend_from_slice(&[10, 0, 0, 0, 0, 0, 0, 0, 0x7F, 0x02, 0x00, 0xDF, 0x01, 0x00]); // 640 × 480
    for (name, mime, ext, bytes, dims) in [
        ("a.gif", "image/gif", "gif", gif, Some((320, 200))),
        ("b.jpg", "image/jpeg", "jpg", jpeg, Some((800, 600))),
        ("c.webp", "image/webp", "webp", webp, Some((640, 480))),
        ("d.png", PNG, "png", small_png(8), Some((640, 480))),
    ] {
        let a = upload_as(&td, &mut c, name, mime, &bytes).await;
        assert_eq!(a.mime, mime);
        assert_eq!((a.width, a.height), dims.map_or((None, None), |(w, h)| (Some(w), Some(h))), "{name}");
        assert_eq!(std::fs::read(blob_file(&td, &a, ext)).unwrap(), bytes, "{name}");
    }
    // 文件头合法但读不出宽高：照样收，宽高省略
    let odd = b"GIF89a".to_vec();
    let a = upload_as(&td, &mut c, "odd.gif", "image/gif", &odd).await;
    assert_eq!((a.width, a.height), (None, None));
    td.shutdown().await;
}

/// 用一台全新的「设备」调 `blob.put_begin`：合法的基础参数，覆盖 `patch` 里的字段。
async fn begin_with(td: &TestDaemon, device: &str, patch: Value) -> Result<BlobPutBeginResult, RpcError> {
    let mut p = json!({ "name": "a.png", "mime": PNG, "size": 100, "sha256": sha(b"x") });
    for (k, v) in patch.as_object().unwrap() {
        p[k] = v.clone();
    }
    td.client(device).call(td, methods::BLOB_PUT_BEGIN, p).await
}

#[tokio::test]
async fn put_begin_validates_name_mime_size_and_sha() {
    let td = TestDaemon::start_with(&[("sessions.max_attachment_bytes", "\"4KiB\"")], false).await;
    // mime 的格式不对
    for mime in ["", "text", "text/plain; charset=utf-8", "*/*", "a/b/c", "text/pl ain"] {
        let e = begin_with(&td, "m", json!({ "mime": mime })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{mime:?}");
    }
    // 任意类型都收（四种图片之外的不看内容）
    for mime in ["image/svg+xml", "application/pdf", "image/bmp", "text/plain", "IMAGE/PNG", "application/octet-stream"]
    {
        begin_with(&td, &format!("m-{mime}"), json!({ "mime": mime })).await.unwrap();
    }
    // 名字
    for name in ["", "a/b.png", "..\\x.png", "a\nb.png", "a\u{7}.png", &"x".repeat(256)] {
        let e = begin_with(&td, "n", json!({ "name": name })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{name:?}");
    }
    begin_with(&td, "n", json!({ "name": "x".repeat(255) })).await.unwrap();
    // 大小：0 和超过上限（config 里 4 KiB）
    for size in [0u64, 4097, 50 * 1024 * 1024] {
        let e = begin_with(&td, "s", json!({ "size": size })).await.unwrap_err();
        assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")), "{size}");
    }
    begin_with(&td, "s", json!({ "size": 4096 })).await.unwrap();
    // sha256 不是 64 位小写 hex
    for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(63), &"a".repeat(65)] {
        let e = begin_with(&td, "h", json!({ "sha256": bad })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{bad:?}");
    }
    // 失败的 put_begin 不占名额，也不留文件：只有上面成功的（6 种 mime + 名字 + 大小）留下了 .part
    assert_eq!(std::fs::read_dir(td.paths.blobs_tmp()).unwrap().count(), 8);
    td.shutdown().await;
}

#[tokio::test]
async fn at_most_eight_uploads_per_device_at_the_same_time() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let bytes = small_png(9);
    let mut ids = vec![];
    for _ in 0..8 {
        ids.push(begin(&td, &mut a, "x.png", PNG, &bytes).await.blob_id);
    }
    let e = a
        .call::<BlobPutBeginResult>(
            &td,
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "x.png", "mime": PNG, "size": bytes.len(), "sha256": sha(&bytes) }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Busy);
    // 另一台设备不受影响
    begin(&td, &mut b, "x.png", PNG, &bytes).await;
    // 完成或丢弃一个就腾出一个名额
    discard(&td, &mut a, &ids[0]).await.unwrap();
    chunk(&td, &mut a, &ids[1], 0, &bytes).await.unwrap();
    end(&td, &mut a, &ids[1]).await.unwrap();
    begin(&td, &mut a, "x.png", PNG, &bytes).await;
    begin(&td, &mut a, "x.png", PNG, &bytes).await;
    assert_eq!(
        a.call::<BlobPutBeginResult>(
            &td,
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "x.png", "mime": PNG, "size": bytes.len(), "sha256": sha(&bytes) }),
        )
        .await
        .unwrap_err()
        .stable_code(),
        Some(ErrorCode::Busy)
    );
    td.shutdown().await;
}

#[tokio::test]
async fn malformed_blob_ids_are_rejected_at_every_entry_before_touching_the_filesystem() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 一个藏在 blobs 目录外面的「正式文件」：任何入口都不能被诱导去读 / 删它
    let victim = td.dir.path().join("home").join("victim.png");
    std::fs::write(&victim, small_png(1)).unwrap();
    for bad in [
        "../victim",
        "../../victim.png",
        "../victim.png/0123456789abcdef0123456789ab",
        "0123456789abcdef0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdef.png",
        "/etc/passwd",
        "",
        "tmp/0123456789abcdef0123456789abcdef",
    ] {
        let e = chunk(&td, &mut c, bad, 0, b"x").await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "put_chunk {bad:?}");
        let e = end(&td, &mut c, bad).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "put_end {bad:?}");
        let e = get(&td, &mut c, bad, 0, None).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "get {bad:?}");
        let e = discard(&td, &mut c, bad).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "discard {bad:?}");
    }
    assert!(victim.exists());
    // 消息里的附件也一样先校验格式
    let s = new_session(&td, &mut c).await;
    let mut a = dummy_ref();
    a.blob_id = "../victim".into();
    let e = send(&td, &mut c, &s.id, "m1", "x", &[&a]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    td.shutdown().await;
}

#[tokio::test]
async fn get_pages_through_the_file_and_reports_eof() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let total = CHUNK_SIZE as usize + 1000;
    let bytes = png(100, 100, total, 10);
    let a = upload(&td, &mut c, "x.png", &bytes).await;
    // 每一页都带完整的 `blob`（`bytes` 是整个文件的大小，不是这一段的）；`max_bytes = 524288` 就是上限本身，合法
    let exact = get(&td, &mut c, &a.blob_id, 0, Some(524_288)).await.unwrap();
    assert_eq!((exact.blob.bytes, exact.eof), (total as u64, false));
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&exact.data).unwrap().len(), 524_288);
    let tail = get(&td, &mut c, &a.blob_id, 524_288, Some(524_288)).await.unwrap();
    assert_eq!((tail.blob.clone(), tail.eof), (a.clone(), true), "the second page repeats the whole record");
    // 缺省每段 512 KiB
    let g1 = get(&td, &mut c, &a.blob_id, 0, None).await.unwrap();
    assert_eq!((g1.offset, g1.eof), (0, false));
    assert_eq!(g1.blob, a);
    let d1 = base64::engine::general_purpose::STANDARD.decode(&g1.data).unwrap();
    assert_eq!(d1.len(), CHUNK_SIZE as usize);
    let g2 = get(&td, &mut c, &a.blob_id, d1.len() as u64, None).await.unwrap();
    assert!(g2.eof);
    let d2 = base64::engine::general_purpose::STANDARD.decode(&g2.data).unwrap();
    assert_eq!(d2.len(), 1000);
    assert_eq!([d1, d2].concat(), bytes);
    // max_bytes 小于上限就用它；大于上限被压到上限
    let g = get(&td, &mut c, &a.blob_id, 10, Some(100)).await.unwrap();
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&g.data).unwrap(), bytes[10..110]);
    assert!(!g.eof);
    let g = get(&td, &mut c, &a.blob_id, 0, Some(10 * 1024 * 1024)).await.unwrap();
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(&g.data).unwrap().len(), CHUNK_SIZE as usize);
    // 刚好读到末尾 -> eof；offset == size -> 空数据 + eof；offset > size -> invalid
    let g = get(&td, &mut c, &a.blob_id, total as u64 - 5, Some(5)).await.unwrap();
    assert!(g.eof);
    let g = get(&td, &mut c, &a.blob_id, total as u64, None).await.unwrap();
    assert_eq!((g.data.as_str(), g.eof), ("", true));
    let e = get(&td, &mut c, &a.blob_id, total as u64 + 1, None).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 不存在、还没 put_end 的上传：not_found
    let e = get(&td, &mut c, &"1".repeat(32), 0, None).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    let pending = begin(&td, &mut c, "p.png", PNG, &bytes).await;
    let e = get(&td, &mut c, &pending.blob_id, 0, None).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    // 任何设备都能读（不要求是上传者）
    let mut other = td.client("other");
    let g = get(&td, &mut other, &a.blob_id, 0, Some(16)).await.unwrap();
    assert_eq!(g.blob, a);
    td.shutdown().await;
}

#[tokio::test]
async fn discard_handles_the_three_cases() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let bytes = small_png(11);
    // 1. 进行中的上传：删 .part，之后的块 / put_end 都是 not_found
    let b = begin(&td, &mut c, "x.png", PNG, &bytes).await;
    chunk(&td, &mut c, &b.blob_id, 0, &bytes).await.unwrap();
    assert!(part_file(&td, &b.blob_id).exists());
    discard(&td, &mut c, &b.blob_id).await.unwrap();
    assert!(!part_file(&td, &b.blob_id).exists());
    assert_eq!(end(&td, &mut c, &b.blob_id).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    // 2. 已完成但没被引用：删文件和行
    let a = upload(&td, &mut c, "x.png", &bytes).await;
    assert!(blob_file(&td, &a, "png").exists());
    discard(&td, &mut c, &a.blob_id).await.unwrap();
    assert!(!blob_file(&td, &a, "png").exists());
    assert!(td.ctx.store.blob_get(&a.blob_id).await.unwrap().is_none());
    assert_eq!(get(&td, &mut c, &a.blob_id, 0, None).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    // 3. 已被消息引用：invalid / in_use，文件还在
    let a = upload(&td, &mut c, "y.png", &bytes).await;
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m1", "see", &[&a]).await.unwrap();
    let e = discard(&td, &mut c, &a.blob_id).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("in_use")));
    assert!(blob_file(&td, &a, "png").exists());
    // 不存在：成功（幂等）；重复 discard 也成功
    discard(&td, &mut c, &"2".repeat(32)).await.unwrap();
    discard(&td, &mut c, &b.blob_id).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- 过期和回收

#[tokio::test]
async fn uploads_expire_after_ten_minutes_without_a_chunk() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let bytes = png(10, 10, CHUNK_SIZE as usize * 2, 12);
    let parts: Vec<&[u8]> = bytes.chunks(CHUNK_SIZE as usize).collect();
    let b = begin(&td, &mut c, "x.png", PNG, &bytes).await;
    let blobs = &td.ctx.blobs;
    // 每收到一块就顺延：6 分钟 + 收块 + 6 分钟，总共 12 分钟，上传还活着
    blobs.advance_clock(Duration::from_secs(6 * 60));
    chunk(&td, &mut c, &b.blob_id, 0, parts[0]).await.unwrap();
    blobs.advance_clock(Duration::from_secs(6 * 60));
    chunk(&td, &mut c, &b.blob_id, 1, parts[1]).await.unwrap();
    end(&td, &mut c, &b.blob_id).await.unwrap();

    // 10 分钟没有新块：作废，删 .part
    let b2 = begin(&td, &mut c, "y.png", PNG, &bytes).await;
    chunk(&td, &mut c, &b2.blob_id, 0, parts[0]).await.unwrap();
    assert!(part_file(&td, &b2.blob_id).exists());
    blobs.advance_clock(Duration::from_secs(10 * 60 + 1));
    // 周期任务（sweep）顺手清
    td.ctx.sessions.sweep().await;
    assert!(!part_file(&td, &b2.blob_id).exists(), "the sweeper removes the expired upload");
    assert_eq!(
        chunk(&td, &mut c, &b2.blob_id, 1, parts[1]).await.unwrap_err().stable_code(),
        Some(ErrorCode::NotFound)
    );
    // 另一个过期的上传：不等周期任务，下一次调用就清
    let b3 = begin(&td, &mut c, "z.png", PNG, &bytes).await;
    blobs.advance_clock(Duration::from_secs(11 * 60));
    assert_eq!(
        chunk(&td, &mut c, &b3.blob_id, 0, parts[0]).await.unwrap_err().stable_code(),
        Some(ErrorCode::NotFound)
    );
    assert!(!part_file(&td, &b3.blob_id).exists());
    // 过期的上传不再占每设备 8 个的名额
    for _ in 0..8 {
        begin(&td, &mut c, "x.png", PNG, &bytes).await;
    }
    blobs.advance_clock(Duration::from_secs(11 * 60));
    begin(&td, &mut c, "x.png", PNG, &bytes).await;
    td.shutdown().await;
}

#[tokio::test]
async fn unreferenced_blobs_are_collected_after_a_day_and_referenced_ones_are_not() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let old_orphan = upload(&td, &mut c, "orphan.png", &small_png(13)).await;
    let kept = upload(&td, &mut c, "kept.png", &small_png(14)).await;
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m1", "hi", &[&kept]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    // 23 小时：谁都不动
    td.ctx.blobs.advance_clock(Duration::from_secs(23 * 3600));
    td.ctx.sessions.cleanup().await;
    assert!(blob_file(&td, &old_orphan, "png").exists());
    // 刚上传的新 blob，之后再过 2 小时也不到 24 小时
    let fresh = upload(&td, &mut c, "fresh.png", &small_png(15)).await;
    td.ctx.blobs.advance_clock(Duration::from_secs(2 * 3600));
    // 每小时的清理任务（`Sessions::cleanup`）顺带回收
    td.ctx.sessions.cleanup().await;
    assert!(!blob_file(&td, &old_orphan, "png").exists(), "an unreferenced blob older than 24 hours is removed");
    assert!(td.ctx.store.blob_get(&old_orphan.blob_id).await.unwrap().is_none());
    assert!(blob_file(&td, &fresh, "png").exists(), "younger than 24 hours");
    assert!(blob_file(&td, &kept, "png").exists(), "referenced by a message");
    assert_eq!(bound_session(&td, &kept.blob_id).await.as_deref(), Some(s.id.as_str()));
    // 再过一天：fresh 也没了，kept 仍在
    td.ctx.blobs.advance_clock(Duration::from_secs(24 * 3600));
    assert_eq!(td.ctx.blobs.gc().await, 1);
    assert!(!blob_file(&td, &fresh, "png").exists());
    assert!(blob_file(&td, &kept, "png").exists());
    // 被回收的 blob 在消息里引用 -> not_found（reason = blob），客户端要重新添加
    let e = send(&td, &mut c, &s.id, "m2", "again", &[&old_orphan]).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("blob")));
    td.shutdown().await;
}

#[tokio::test]
async fn startup_clears_tmp_and_orphan_files_but_keeps_recorded_blobs() {
    let mut td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "kept.png", &small_png(16)).await;
    let half = begin(&td, &mut c, "half.png", PNG, &small_png(17)).await;
    assert!(part_file(&td, &half.blob_id).exists());
    // 崩溃留下的：没有记录的正式文件
    let orphan = td.paths.blobs_dir().join(format!("{}.png", "ab".repeat(16)));
    std::fs::write(&orphan, b"x").unwrap();
    // 名字不符合 `<32 hex>.<ext>` 的文件不碰
    let stranger = td.paths.blobs_dir().join("notes.txt");
    std::fs::write(&stranger, b"keep me").unwrap();
    // 不是图片的扩展名也一样认
    let orphan_pdf = td.paths.blobs_dir().join(format!("{}.pdf", "cd".repeat(16)));
    std::fs::write(&orphan_pdf, b"x").unwrap();
    td.restart().await;
    assert!(!part_file(&td, &half.blob_id).exists(), "uploads in flight do not survive a restart");
    assert!(td.paths.blobs_tmp().is_dir());
    assert!(!orphan.exists(), "a file without a record is removed");
    assert!(!orphan_pdf.exists());
    assert!(stranger.exists());
    assert!(blob_file(&td, &a, "png").exists());
    // 记录还在，重启后仍可下载
    let mut c = td.client("a");
    let g = get(&td, &mut c, &a.blob_id, 0, Some(16)).await.unwrap();
    assert_eq!(g.blob, a);
    assert_eq!(end(&td, &mut c, &half.blob_id).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    td.shutdown().await;
}

#[tokio::test]
async fn deleting_a_session_removes_its_attachment_files_only() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let mine = upload(&td, &mut c, "mine.png", &small_png(18)).await;
    let other = upload(&td, &mut c, "other.png", &small_png(19)).await;
    let loose = upload(&td, &mut c, "loose.png", &small_png(20)).await;
    let s1 = new_session(&td, &mut c).await;
    let s2 = new_session(&td, &mut c).await;
    send(&td, &mut c, &s1.id, "m1", "x", &[&mine]).await.unwrap();
    send(&td, &mut c, &s2.id, "m2", "y", &[&other]).await.unwrap();
    wait_status(&td, &mut c, &s1.id, SessionStatus::Idle).await;
    wait_status(&td, &mut c, &s2.id, SessionStatus::Idle).await;
    let _: Empty = c.call(&td, methods::SESSION_DELETE, json!({ "session_id": s1.id })).await.unwrap();
    assert!(!blob_file(&td, &mine, "png").exists());
    assert!(td.ctx.store.blob_get(&mine.blob_id).await.unwrap().is_none());
    assert_eq!(get(&td, &mut c, &mine.blob_id, 0, None).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    assert!(blob_file(&td, &other, "png").exists(), "another session's attachment stays");
    assert!(blob_file(&td, &loose, "png").exists(), "an unreferenced blob is not tied to the session");
    td.shutdown().await;
}

// ---------------------------------------------------------------- 发送时的核对

#[tokio::test]
async fn a_blob_of_another_session_cannot_be_referenced_and_nothing_is_half_done() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(21)).await;
    let b = upload(&td, &mut c, "b.png", &small_png(22)).await;
    let s1 = new_session(&td, &mut c).await;
    let s2 = new_session(&td, &mut c).await;
    send(&td, &mut c, &s1.id, "m1", "mine", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s1.id, SessionStatus::Idle).await;
    // 别的会话引用 a -> invalid / blob_in_use
    let e = send(&td, &mut c, &s2.id, "m2", "steal", &[&a]).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("blob_in_use")));
    // 先 b 后 a：b 不能被绑上（没有半截状态），消息也没有进事件流
    let e = send(&td, &mut c, &s2.id, "m3", "mixed", &[&b, &a]).await.unwrap_err();
    assert_eq!(reason(&e), Some("blob_in_use"));
    assert_eq!(bound_session(&td, &b.blob_id).await, None, "the valid attachment was not bound either");
    assert!(user_messages(&td, &mut c, &s2.id).await.is_empty(), "no user_message event");
    assert_eq!(td.fake.spawn_count(), 1, "no agent process for the rejected message");
    // 同一个会话里再次引用自己的 blob 没问题
    send(&td, &mut c, &s1.id, "m4", "again", &[&a]).await.unwrap();
    assert_eq!(user_messages(&td, &mut c, &s1.id).await.len(), 2);
    // 被拒绝之后 b 仍然可用：discard 成功（没被引用）
    discard(&td, &mut c, &b.blob_id).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn attachments_are_checked_against_the_records() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(23)).await;
    let s = new_session(&td, &mut c).await;
    // name / mime / bytes 和记录不一致 -> invalid
    for bad in [
        AttachmentRef { name: "other.png".into(), ..a.clone() },
        AttachmentRef { mime: "image/jpeg".into(), ..a.clone() },
        AttachmentRef { bytes: a.bytes + 1, ..a.clone() },
    ] {
        let e = send(&td, &mut c, &s.id, "m1", "x", &[&bad]).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{bad:?}");
    }
    // 不存在的 blob -> not_found / blob
    let ghost = AttachmentRef { blob_id: "3".repeat(32), ..a.clone() };
    let e = send(&td, &mut c, &s.id, "m1", "x", &[&ghost]).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("blob")));
    // 还没 put_end 的上传也不行
    let pending = begin(&td, &mut c, "p.png", PNG, &small_png(24)).await;
    let early = AttachmentRef { blob_id: pending.blob_id.clone(), ..a.clone() };
    let e = send(&td, &mut c, &s.id, "m1", "x", &[&early]).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("blob")));
    // 同一条消息里 blob_id 重复
    let e = send(&td, &mut c, &s.id, "m1", "x", &[&a, &a]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 超过 20 个
    let many: Vec<AttachmentRef> =
        (0..21).map(|i| AttachmentRef { blob_id: format!("{i:032x}"), ..a.clone() }).collect();
    let refs: Vec<&AttachmentRef> = many.iter().collect();
    let e = send(&td, &mut c, &s.id, "m1", "x", &refs).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    assert!(user_messages(&td, &mut c, &s.id).await.is_empty());
    assert_eq!(bound_session(&td, &a.blob_id).await, None);
    // 正确的那条没问题，一条消息 20 个也可以
    let mut all = vec![];
    for i in 0..20u8 {
        all.push(upload(&td, &mut c, &format!("{i}.png"), &small_png(100 + i)).await);
    }
    let refs: Vec<&AttachmentRef> = all.iter().collect();
    send(&td, &mut c, &s.id, "m2", "twenty", &refs).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let input = td.fake.handle(0).inputs.lock().unwrap()[0].clone();
    assert_eq!(input.attachments.len(), 20);
    assert_eq!(input.attachment_paths.len(), 20);
    td.shutdown().await;
}

#[tokio::test]
async fn the_agent_must_accept_the_attachment_mime() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let png_a = upload(&td, &mut c, "a.png", &small_png(25)).await;
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[1, 0, 1, 0, 0, 0]);
    let gif_a = upload_as(&td, &mut c, "a.gif", "image/gif", &gif).await;
    // agent 只收 PNG
    *td.fake.attachments.lock().unwrap() = vec![PNG.to_string()];
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    let s = new_session(&td, &mut c).await;
    let e = send(&td, &mut c, &s.id, "m1", "gif", &[&gif_a]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    assert_eq!(bound_session(&td, &gif_a.blob_id).await, None);
    send(&td, &mut c, &s.id, "m2", "png", &[&png_a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    // agent 不收任何附件
    *td.fake.attachments.lock().unwrap() = vec![];
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    let e = send(&td, &mut c, &s.id, "m3", "png", &[&png_a]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    // create 带附件同样检查，而且被拒绝时不留下会话
    let fresh = upload(&td, &mut c, "fresh.png", &small_png(37)).await;
    let before: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "hi", "message_id": "c1", "attachments": [fresh] }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    let after: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(before.sessions.len(), after.sessions.len(), "a rejected create leaves no empty session behind");
    td.shutdown().await;
}

#[tokio::test]
async fn any_file_is_stored_as_is_and_handed_to_an_agent_that_accepts_files() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 不是图片：不看文件头、没有宽高，mime 记成小写，名字原样保留，落盘的扩展名取名字的
    let pdf_bytes = b"%PDF-1.7 not really".to_vec();
    let pdf = upload_as(&td, &mut c, "Spec v2.PDF", "Application/PDF", &pdf_bytes).await;
    assert_eq!(
        (pdf.name.as_str(), pdf.mime.as_str(), pdf.bytes),
        ("Spec v2.PDF", "application/pdf", pdf_bytes.len() as u64)
    );
    assert_eq!((pdf.width, pdf.height), (None, None));
    assert_eq!(std::fs::read(blob_file(&td, &pdf, "pdf")).unwrap(), pdf_bytes);
    // PNG 的内容声明成别的类型也行（当作普通文件）；没有像样扩展名的落成 `.bin`
    let raw = upload_as(&td, &mut c, "Makefile", "application/octet-stream", &small_png(30)).await;
    assert_eq!((raw.width, raw.height), (None, None));
    assert!(blob_file(&td, &raw, "bin").is_file());
    let g = get(&td, &mut c, &pdf.blob_id, 0, None).await.unwrap();
    let data = base64::engine::general_purpose::STANDARD.decode(&g.data).unwrap();
    assert_eq!((g.blob.clone(), data, g.eof), (pdf.clone(), pdf_bytes.clone(), true));

    // agent 只收图片：文件 → unsupported
    let s = new_session(&td, &mut c).await;
    let e = send(&td, &mut c, &s.id, "m1", "", &[&pdf]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Unsupported);
    assert_eq!(bound_session(&td, &pdf.blob_id).await, None);
    // agent 收 `*/*`：文件和图片都交给 adapter，路径就是落盘的文件
    *td.fake.attachments.lock().unwrap() = vec![PNG.to_string(), "*/*".to_string()];
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    let png = upload(&td, &mut c, "shot.png", &small_png(31)).await;
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m2", "", &[&pdf, &png, &raw]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let last = td.fake.handles.lock().unwrap().last().cloned().unwrap();
    let input = last.inputs.lock().unwrap()[0].clone();
    assert_eq!(input.attachments, vec![pdf.clone(), png.clone(), raw.clone()]);
    assert_eq!(
        input.attachment_paths,
        vec![blob_file(&td, &pdf, "pdf"), blob_file(&td, &png, "png"), blob_file(&td, &raw, "bin")]
    );
    assert_eq!(bound_session(&td, &pdf.blob_id).await.as_deref(), Some(s.id.as_str()));
    // 有不是图片的附件时，标题用 `[file]`
    assert_eq!(summary(&td, &mut c, &s.id).await.title, "[file] Spec v2.PDF (+2)");
    td.shutdown().await;
}

#[tokio::test]
async fn the_same_message_id_with_different_attachments_is_not_a_retry() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(26)).await;
    let b = upload(&td, &mut c, "b.png", &small_png(27)).await;
    let s = new_session(&td, &mut c).await;
    let first = send(&td, &mut c, &s.id, "m1", "hello", &[&a]).await.unwrap();
    assert!(!first.duplicate);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    // 真正的重发（同样的内容和附件）：duplicate，不再产生事件 / turn / 输入
    let again = send(&td, &mut c, &s.id, "m1", "hello", &[&a]).await.unwrap();
    assert!(again.duplicate);
    assert_eq!(user_messages(&td, &mut c, &s.id).await.len(), 1);
    assert_eq!(td.fake.handle(0).inputs.lock().unwrap().len(), 1);
    // 重发时附件的 blob_id 变了 / 少了 / 多了 / 顺序不同 -> invalid
    for atts in [vec![&b], vec![], vec![&a, &b]] {
        let e = send(&td, &mut c, &s.id, "m1", "hello", &atts).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{atts:?}");
    }
    assert_eq!(bound_session(&td, &b.blob_id).await, None, "the rejected retries did not bind anything");
    // 顺序：两个附件的消息，换了顺序重发 -> invalid
    send(&td, &mut c, &s.id, "m2", "two", &[&a, &b]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let e = send(&td, &mut c, &s.id, "m2", "two", &[&b, &a]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    assert!(send(&td, &mut c, &s.id, "m2", "two", &[&a, &b]).await.unwrap().duplicate);
    td.shutdown().await;
}

#[tokio::test]
async fn a_message_may_have_only_attachments_and_is_not_indexed_for_search() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "diagram.png", &small_png(28)).await;
    let s = new_session(&td, &mut c).await;
    // 都为空 -> invalid（照旧）
    let e = send(&td, &mut c, &s.id, "m0", "", &[]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    // 只有附件
    send(&td, &mut c, &s.id, "m1", "", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let msgs = user_messages(&td, &mut c, &s.id).await;
    assert_eq!(msgs, vec![("m1".to_string(), String::new(), vec![a.clone()])]);
    // adapter 收到空文字 + 一个附件
    let input = td.fake.handle(0).inputs.lock().unwrap()[0].clone();
    assert_eq!((input.content.as_str(), input.attachments.len()), ("", 1));
    // 标题和摘要用附件的标签
    let sum = summary(&td, &mut c, &s.id).await;
    assert_eq!(sum.title, "[image] diagram.png");
    assert_eq!(sum.preview.as_deref(), Some("[image] diagram.png"));
    // 全文索引只收文字：文件名搜不到；文字照常能搜到
    let hits: SessionSearchResult = c.call(&td, methods::SESSION_SEARCH, json!({ "query": "diagram" })).await.unwrap();
    assert!(hits.hits.is_empty(), "{hits:?}");
    send(&td, &mut c, &s.id, "m2", "describe the zebra", &[]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let hits: SessionSearchResult = c.call(&td, methods::SESSION_SEARCH, json!({ "query": "zebra" })).await.unwrap();
    assert!(!hits.hits.is_empty(), "text is indexed as before");
    // 标题已经有了，之后的消息不改它
    assert_eq!(summary(&td, &mut c, &s.id).await.title, "[image] diagram.png");
    td.shutdown().await;
}

#[tokio::test]
async fn create_may_carry_attachments_with_or_without_a_prompt() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(29)).await;
    let b = upload(&td, &mut c, "b.png", &small_png(30)).await;
    // 带 prompt + 附件
    let s1: SessionSummary = c
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "look at this", "message_id": "c1", "attachments": [a] }),
        )
        .await
        .unwrap();
    wait_status(&td, &mut c, &s1.id, SessionStatus::Idle).await;
    assert_eq!(user_messages(&td, &mut c, &s1.id).await, vec![("c1".into(), "look at this".into(), vec![a.clone()])]);
    assert_eq!(td.fake.handle(0).inputs.lock().unwrap()[0].attachment_paths, vec![blob_file(&td, &a, "png")]);
    assert_eq!(bound_session(&td, &a.blob_id).await.as_deref(), Some(s1.id.as_str()));
    // 只有附件（没有 prompt）：也发第一条消息，message_id 必填
    let e = c
        .call::<SessionSummary>(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "attachments": [b] }))
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let s2: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "message_id": "c2", "attachments": [b] }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s2.id, SessionStatus::Idle).await;
    assert_eq!(user_messages(&td, &mut c, &s2.id).await, vec![("c2".into(), String::new(), vec![b.clone()])]);
    assert_eq!(summary(&td, &mut c, &s2.id).await.title, "[image] b.png");
    // 创建的幂等：同一个 message_id + 同样的附件 -> 返回同一个会话；附件不同 -> invalid
    let again: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "message_id": "c2", "attachments": [b] }))
        .await
        .unwrap();
    assert_eq!(again.id, s2.id);
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "message_id": "c2", "attachments": [a] }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let all: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 2);
    // 不存在 / 已过期的 blob：`not_found` + `reason = "blob"`（Web 靠它提示「附件已过期」），也不留空会话
    let ghost = AttachmentRef { blob_id: "5".repeat(32), ..a.clone() };
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "message_id": "c4", "attachments": [ghost] }),
        )
        .await
        .unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::NotFound, Some("blob")));
    // 引用别的会话的 blob：创建被拒绝，不留空会话
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "x", "message_id": "c3", "attachments": [a] }),
        )
        .await
        .unwrap_err();
    assert_eq!(reason(&e), Some("blob_in_use"));
    let all: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 2);
    td.shutdown().await;
}

// ---------------------------------------------------------------- 积压合并和插话

#[tokio::test]
async fn backlog_messages_with_attachments_merge_in_message_order() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a1 = upload(&td, &mut c, "1.png", &small_png(31)).await;
    let a2 = upload(&td, &mut c, "2.png", &small_png(32)).await;
    let a3 = upload(&td, &mut c, "3.png", &small_png(33)).await;
    let s = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }),
        )
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    // turn 进行中、agent 不支持插话：两条消息进积压（第二条带两个附件，第三条只有一个）
    assert!(send(&td, &mut c, &s.id, "m1", "first", &[&a1]).await.unwrap().queued);
    assert!(send(&td, &mut c, &s.id, "m2", "", &[&a2, &a3]).await.unwrap().queued);
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 2 }).await);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let inputs = td.fake.handle(0).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 2, "the two queued messages became one turn");
    assert!(inputs[0].attachments.is_empty() && inputs[0].attachment_paths.is_empty());
    let merged = &inputs[1];
    assert_eq!(merged.attachments, vec![a1.clone(), a2.clone(), a3.clone()], "message order");
    assert_eq!(
        merged.attachment_paths,
        vec![blob_file(&td, &a1, "png"), blob_file(&td, &a2, "png"), blob_file(&td, &a3, "png")],
        "paths line up with the attachments"
    );
    assert_eq!(merged.attachments.len(), merged.attachment_paths.len(), "equal length");
    assert!(merged.attachment_paths.iter().all(|p| p.is_absolute()));
    assert!(merged.content.contains("first") && merged.content.contains("---"));
    td.shutdown().await;
}

#[tokio::test]
async fn interjected_messages_carry_their_attachments_and_keep_them_when_requeued() {
    let td = TestDaemon::start().await;
    td.fake.interject.store(true, Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    let mut c = td.client("a");
    let a1 = upload(&td, &mut c, "1.png", &small_png(34)).await;
    let a2 = upload(&td, &mut c, "2.png", &small_png(35)).await;
    let s = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }),
        )
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    // 插话成功：adapter 收到的 UserInput 是 interject = true，带附件
    let r = send(&td, &mut c, &s.id, "m1", "look", &[&a1]).await.unwrap();
    assert!(!r.queued, "handed to the running turn");
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 2 }).await);
    let ij = td.fake.handle(0).inputs.lock().unwrap()[1].clone();
    assert!(ij.interject);
    assert_eq!(ij.attachments, vec![a1.clone()]);
    assert_eq!(ij.attachment_paths, vec![blob_file(&td, &a1, "png")]);
    // 插话失败：消息（连附件）退回队列，作为下一个 turn 发出
    td.fake.last_handle().release();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let s2 = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "hold again", "message_id": "n0" }),
        )
        .await
        .unwrap();
    wait_status(&td, &mut c, &s2.id, SessionStatus::Running).await;
    *td.fake.interject_failure.lock().unwrap() = Some(InterjectFailure::TurnEnded);
    let r = send(&td, &mut c, &s2.id, "n1", "retry me", &[&a2]).await.unwrap();
    assert!(!r.queued, "optimistic: it was handed to the running turn");
    let h = td.fake.last_handle();
    let t = std::time::Instant::now();
    while !history(&td, &mut c, &s2.id)
        .await
        .iter()
        .any(|e| matches!(e.event, Event::Status { queued_messages: Some(1), .. }))
    {
        assert!(t.elapsed() < T, "the failed interjection never went back to the queue");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    *td.fake.interject_failure.lock().unwrap() = None;
    h.release();
    assert!(eventually(T, || async { h.inputs.lock().unwrap().len() >= 2 }).await);
    wait_status(&td, &mut c, &s2.id, SessionStatus::Idle).await;
    let next = h.inputs.lock().unwrap()[1].clone();
    assert!(!next.interject, "the requeued message starts a new turn");
    assert_eq!(next.attachments, vec![a2.clone()]);
    assert_eq!(next.attachment_paths, vec![blob_file(&td, &a2, "png")]);
    td.shutdown().await;
}

/// 积压的消息出队时 adapter 才读附件文件（Claude Code 的做法）：文件这时读不了，`send` 返回 `Io`，这是这条消息的失败——
/// turn 以 `error` 收尾，会话进入 error，消息不会无限重试；之后的消息照常能发。
#[tokio::test]
async fn a_file_that_cannot_be_read_when_the_turn_starts_fails_that_turn_once() {
    let td = TestDaemon::start().await;
    td.fake.read_attachments.store(true, Ordering::SeqCst);
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "gone.png", &small_png(38)).await;
    let s = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }),
        )
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    assert!(send(&td, &mut c, &s.id, "m1", "look", &[&a]).await.unwrap().queued);
    // 文件在排队期间不见了（磁盘被动过）
    std::fs::remove_file(blob_file(&td, &a, "png")).unwrap();
    td.fake.last_handle().release();
    wait_status(&td, &mut c, &s.id, SessionStatus::Error).await;
    let evs = history(&td, &mut c, &s.id).await;
    let errors: Vec<(&str, &str)> = evs
        .iter()
        .filter_map(|e| match &e.event {
            Event::Error { code, message, .. } => Some((code.as_str(), message.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "reported once: {errors:?}");
    assert_eq!(errors[0].0, "agent_error");
    // 没有无限重试：没有第三个 turn，adapter 没有收到这条输入
    tokio::time::sleep(Duration::from_millis(300)).await;
    let turn_starts =
        history(&td, &mut c, &s.id).await.iter().filter(|e| matches!(e.event, Event::TurnStart { .. })).count();
    assert_eq!(turn_starts, 2);
    assert_eq!(td.fake.handle(0).inputs.lock().unwrap().len(), 1, "only the first message reached the agent");
    // 会话修好之后（发一条别的消息）照常工作
    send(&td, &mut c, &s.id, "m2", "never mind", &[]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    td.shutdown().await;
}

// ---------------------------------------------------------------- 审计、socket、schema

#[tokio::test]
async fn blob_transfers_are_not_audited_but_session_send_still_is() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(36)).await;
    get(&td, &mut c, &a.blob_id, 0, None).await.unwrap();
    discard(&td, &mut c, &"4".repeat(32)).await.unwrap();
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m1", "x", &[&a]).await.unwrap();
    let audit: NodeAuditResult = c.call(&td, methods::NODE_AUDIT, json!({ "limit": 200 })).await.unwrap();
    let actions: Vec<&str> = audit.entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.iter().all(|a| !a.starts_with("blob")), "{actions:?}");
    assert!(actions.contains(&"session.send"), "{actions:?}");
    td.shutdown().await;
}

/// 最小的 socket 客户端：一行一条 JSON-RPC。
struct Sock {
    r: tokio::io::BufReader<tokio::io::ReadHalf<anycon_daemon::local::ClientStream>>,
    w: tokio::io::WriteHalf<anycon_daemon::local::ClientStream>,
    next: u64,
}

impl Sock {
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        self.next += 1;
        let id = self.next;
        let mut line = RpcMessage::request(id, method, params).encode();
        line.push(b'\n');
        self.w.write_all(&line).await.unwrap();
        loop {
            let mut l = String::new();
            tokio::time::timeout(T, self.r.read_line(&mut l)).await.expect("timed out").unwrap();
            assert!(!l.is_empty(), "socket closed");
            if let RpcMessage::Response(resp) = RpcMessage::decode(l.trim().as_bytes()).unwrap() {
                assert_eq!(resp.id, RpcId::Num(id));
                return resp.error.map(Err).unwrap_or_else(|| Ok(resp.result.unwrap_or(Value::Null)));
            }
        }
    }
}

#[tokio::test]
async fn the_blob_methods_work_over_the_local_socket() {
    let td = TestDaemon::start_with(&[], true).await;
    let stream = anycon_daemon::local::connect(&td.paths.sock()).await.unwrap();
    let (r, w) = tokio::io::split(stream);
    let mut sock = Sock { r: tokio::io::BufReader::new(r), w, next: 0 };
    let bytes = png(33, 44, 2000, 40);
    let begin_r = sock
        .call(
            methods::BLOB_PUT_BEGIN,
            json!({ "name": "s.png", "mime": PNG, "size": bytes.len(), "sha256": sha(&bytes) }),
        )
        .await
        .unwrap();
    let id = begin_r["blob_id"].as_str().unwrap().to_string();
    assert_eq!(begin_r["chunk_size"], json!(CHUNK_SIZE));
    sock.call(methods::BLOB_PUT_CHUNK, json!({ "blob_id": id, "index": 0, "data": b64(&bytes) })).await.unwrap();
    let done = sock.call(methods::BLOB_PUT_END, json!({ "blob_id": id })).await.unwrap();
    assert_eq!(
        (done["width"].clone(), done["height"].clone(), done["bytes"].clone()),
        (json!(33), json!(44), json!(2000))
    );
    let got = sock.call(methods::BLOB_GET, json!({ "blob_id": id, "offset": 0 })).await.unwrap();
    assert_eq!(got["eof"], json!(true));
    assert_eq!(got["data"], json!(b64(&bytes)));
    let e = sock.call(methods::BLOB_GET, json!({ "blob_id": "../x" })).await.unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::Invalid));
    sock.call(methods::BLOB_DISCARD, json!({ "blob_id": id })).await.unwrap();
    td.shutdown().await;
}

// ---------------------------------------------------------------- 真的 mock adapter

/// mock 在回复里回显附件（读 daemon 给的路径算出来的名字 / 大小 / sha256 前 12 位）——证明 daemon 给 adapter 的路径是对的。
async fn mock_text(td: &TestDaemon, c: &mut TestClient, sid: &str) -> String {
    wait_status(td, c, sid, SessionStatus::Idle).await;
    history(td, c, sid)
        .await
        .iter()
        .filter_map(|e| if let Event::TextDelta { text } = &e.event { Some(text.as_str()) } else { None })
        .collect()
}

#[tokio::test]
async fn the_mock_agent_reads_the_files_the_daemon_hands_over() {
    let td = TestDaemon::start_mock(&[], false).await;
    let mut c = td.client("a");
    let b1 = png(10, 20, 2500, 50);
    let b2 = png(30, 40, 4000, 51);
    let a1 = upload(&td, &mut c, "one.png", &b1).await;
    let a2 = upload(&td, &mut c, "two.png", &b2).await;
    let s: SessionSummary = c
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "mock", "prompt": "hello", "message_id": "m1", "attachments": [a1, a2] }),
        )
        .await
        .unwrap();
    let text = mock_text(&td, &mut c, &s.id).await;
    assert!(text.contains(" You said: hello"), "{text}");
    let want = format!(
        " Attachments: one.png (image/png, 2500 bytes, sha256 {}); two.png (image/png, 4000 bytes, sha256 {})",
        &sha(&b1)[..12],
        &sha(&b2)[..12]
    );
    assert!(text.contains(&want), "want {want:?} in {text:?}");
    // 只有附件
    let a3 = upload(&td, &mut c, "three.png", &b1).await;
    send(&td, &mut c, &s.id, "m2", "", &[&a3]).await.unwrap();
    let text = mock_text(&td, &mut c, &s.id).await;
    assert!(
        text.contains(&format!(" Attachments: three.png (image/png, 2500 bytes, sha256 {})", &sha(&b1)[..12])),
        "{text}"
    );
    // 没有附件时回复里没有 Attachments
    let s2: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "mock", "prompt": "plain", "message_id": "m3" }))
        .await
        .unwrap();
    assert!(!mock_text(&td, &mut c, &s2.id).await.contains("Attachments"));
    td.shutdown().await;
}

// ================================================================ 附件上限、收尾、配额、名字与文件头、幂等重试、重启与保留期

/// 上传 `n` 张互不相同的小 PNG（名字 `<prefix>-<i>.png`）。
async fn upload_many(td: &TestDaemon, c: &mut TestClient, prefix: &str, n: usize, seed: u8) -> Vec<AttachmentRef> {
    let mut v = vec![];
    for i in 0..n {
        v.push(upload(td, c, &format!("{prefix}-{i}.png"), &small_png(seed.wrapping_add(i as u8))).await);
    }
    v
}

/// 分页拉回一个 blob 的全部字节。
async fn fetch(td: &TestDaemon, c: &mut TestClient, id: &str) -> Vec<u8> {
    let mut out = vec![];
    loop {
        let r = get(td, c, id, out.len() as u64, None).await.unwrap();
        out.extend(base64::engine::general_purpose::STANDARD.decode(r.data).unwrap());
        if r.eof {
            return out;
        }
    }
}

/// 文件头是 JPEG 的 `FF D8 FF`、后面是别的东西：文件头核对只比前缀。
fn jpeg_prefix_with(tail: &[u8]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
    v.extend_from_slice(tail);
    v
}

fn refs(v: &[AttachmentRef]) -> Vec<&AttachmentRef> {
    v.iter().collect()
}

// ---------------------------------------------------------------- 单张 10 MiB、一条消息 20 MiB / 20 张、积压分 turn

#[tokio::test]
async fn a_single_attachment_may_be_at_most_ten_mib_by_default() {
    let td = TestDaemon::start().await;
    assert_eq!(td.ctx.config.get().max_attachment_bytes(), 10 * 1024 * 1024);
    let ten = 10 * 1024 * 1024u64;
    begin_with(&td, "d1", json!({ "size": ten })).await.unwrap();
    let e = begin_with(&td, "d2", json!({ "size": ten + 1 })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    td.shutdown().await;
}

#[tokio::test]
async fn the_attachments_of_one_message_add_up_to_at_most_twenty_mib() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 默认值：三张 7 MiB（21 MiB）超过一条消息的 20 MiB
    assert_eq!(td.ctx.blobs.message_bytes_limit(), 20 * 1024 * 1024);
    let seven = 7 * 1024 * 1024;
    let mut big = vec![];
    for i in 0..3u8 {
        big.push(upload(&td, &mut c, &format!("big-{i}.png"), &png(4000, 3000, seven, i)).await);
    }
    let s = new_session(&td, &mut c).await;
    let e = send(&td, &mut c, &s.id, "m1", "too much", &refs(&big)).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    // 被拒绝的消息没有留下半截状态：没有事件、没有绑定，blob 还能用
    assert!(user_messages(&td, &mut c, &s.id).await.is_empty());
    for a in &big {
        assert_eq!(bound_session(&td, &a.blob_id).await, None);
    }
    // 创建会话带这三张也一样，而且不留空会话
    let before: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fake", "message_id": "c1", "attachments": big }),
        )
        .await
        .unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    let after: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(after.sessions.len(), before.sessions.len());
    // 少一张（14 MiB）就行
    send(&td, &mut c, &s.id, "m2", "fits", &[&big[0], &big[1]]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(user_messages(&td, &mut c, &s.id).await.len(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn the_per_message_byte_limit_is_inclusive_and_applies_to_the_sum() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let imgs = upload_many(&td, &mut c, "p", 4, 1).await; // 每张 3000 字节
    let s = new_session(&td, &mut c).await;
    td.ctx.blobs.set_message_bytes_limit(9000);
    // 三张正好 9000：可以；再加一张 12000：不行
    let e = send(&td, &mut c, &s.id, "m1", "four", &refs(&imgs)).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    send(&td, &mut c, &s.id, "m2", "three", &refs(&imgs[..3])).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    td.shutdown().await;
}

#[tokio::test]
async fn a_backlog_is_split_into_turns_when_its_attachments_exceed_twenty() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let g1 = upload_many(&td, &mut c, "one", 8, 10).await;
    let g2 = upload_many(&td, &mut c, "two", 8, 40).await;
    let g3 = upload_many(&td, &mut c, "three", 8, 70).await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    // turn 进行中：三条消息各带 8 张进积压（每条都在单条的上限之内）
    assert!(send(&td, &mut c, &s.id, "m1", "first", &refs(&g1)).await.unwrap().queued);
    assert!(send(&td, &mut c, &s.id, "m2", "second", &refs(&g2)).await.unwrap().queued);
    assert!(send(&td, &mut c, &s.id, "m3", "third", &refs(&g3)).await.unwrap().queued);
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 3 }).await);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let inputs = td.fake.handle(0).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 3, "the opening message, the first two queued messages, then the third on its own");
    // 第二个 turn：前两条（16 张）；累计会超过 20 的第三条留在队列里
    let t2 = &inputs[1];
    let want: Vec<AttachmentRef> = g1.iter().chain(g2.iter()).cloned().collect();
    assert_eq!(t2.attachments, want, "message order");
    assert_eq!(t2.attachment_paths.len(), 16);
    assert!(t2.content.contains("first") && t2.content.contains("second") && !t2.content.contains("third"));
    // 第三个 turn：第三条，自动发出
    let t3 = &inputs[2];
    assert_eq!(t3.attachments, g3);
    assert_eq!(t3.attachment_paths, g3.iter().map(|a| blob_file(&td, a, "png")).collect::<Vec<_>>());
    assert_eq!(t3.content, "third");
    // 第二个 turn 一交给 agent 就告诉客户端还有一条在排队（不等到 agent 的第一个事件）
    let evs = history(&td, &mut c, &s.id).await;
    let turn2 =
        evs.iter().position(|e| matches!(e.event, Event::TurnStart { turn_id: 2, .. })).expect("turn 2 started");
    assert!(
        evs[turn2..]
            .iter()
            .any(|e| matches!(e.event, Event::Status { state: SessionStatus::Starting, queued_messages: Some(1), .. })),
        "one message is still queued as soon as turn 2 is handed over"
    );
    td.shutdown().await;
}

#[tokio::test]
async fn a_backlog_is_split_into_turns_when_its_attachments_exceed_the_byte_limit() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    td.ctx.blobs.set_message_bytes_limit(8000); // 每张 3000 字节
    let img = upload_many(&td, &mut c, "i", 4, 20).await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    // m1 两张（6000）、m2 一张、m3 一张：m1 + m2 = 9000 > 8000，所以 m2 要等下一个 turn，m2 + m3 = 6000 一起
    send(&td, &mut c, &s.id, "m1", "first", &refs(&img[..2])).await.unwrap();
    send(&td, &mut c, &s.id, "m2", "second", &[&img[2]]).await.unwrap();
    send(&td, &mut c, &s.id, "m3", "third", &[&img[3]]).await.unwrap();
    // 单条消息超过上限（9000 > 8000）在 send 时就被拒绝
    let e = send(&td, &mut c, &s.id, "m4", "fat", &[&img[0], &img[1], &img[2]]).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 3 }).await);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let inputs = td.fake.handle(0).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[1].attachments, img[..2].to_vec());
    assert!(inputs[1].content.contains("first") && !inputs[1].content.contains("second"));
    assert_eq!(inputs[2].attachments, img[2..].to_vec());
    assert!(inputs[2].content.contains("second") && inputs[2].content.contains("third"));
    td.shutdown().await;
}

#[tokio::test]
async fn the_first_queued_message_is_always_taken_even_if_it_alone_exceeds_the_limit() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let img = upload_many(&td, &mut c, "i", 3, 30).await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    send(&td, &mut c, &s.id, "m1", "first", &refs(&img[..2])).await.unwrap();
    send(&td, &mut c, &s.id, "m2", "second", &[&img[2]]).await.unwrap();
    // 排队之后上限被调低到连 m1 自己也装不下：m1 仍然单独成一个 turn（不能卡住队列），m2 再一个 turn
    td.ctx.blobs.set_message_bytes_limit(1000);
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 3 }).await);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let inputs = td.fake.handle(0).inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 3);
    assert_eq!(inputs[1].attachments, img[..2].to_vec());
    assert_eq!(inputs[2].attachments, img[2..].to_vec());
    td.shutdown().await;
}

// ---------------------------------------------------------------- 收尾

/// 给可能因回归而卡住的调用一个期限：卡住时用例变红，而不是挂住整个测试进程。
async fn within<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f).await.unwrap_or_else(|_| panic!("{what} did not finish in time"))
}

/// 上传好所有块，但不 `put_end`。
async fn upload_chunks_only(td: &TestDaemon, c: &mut TestClient, name: &str, bytes: &[u8]) -> BlobPutBeginResult {
    let b = begin(td, c, name, PNG, bytes).await;
    for (i, part) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        chunk(td, c, &b.blob_id, i as u32, part).await.unwrap();
    }
    b
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_put_end_calls_get_the_same_result_every_time() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    for round in 0..20u8 {
        let bytes = png(64, 64, CHUNK_SIZE as usize * 2 + 100, round);
        let up = upload_chunks_only(&td, &mut a, "x.png", &bytes).await;
        let (x, y) = tokio::join!(end(&td, &mut a, &up.blob_id), end(&td, &mut b, &up.blob_id));
        let (x, y) = (
            x.unwrap_or_else(|e| panic!("round {round}: {e:?}")),
            y.unwrap_or_else(|e| panic!("round {round}: {e:?}")),
        );
        assert_eq!(x, y, "round {round}: both callers get the same AttachmentRef");
        assert_eq!(std::fs::read(blob_file(&td, &x, "png")).unwrap(), bytes);
    }
    td.shutdown().await;
}

#[tokio::test]
async fn a_second_put_end_waits_for_the_first_and_gets_the_same_result_or_the_same_error() {
    for good in [true, false] {
        let td = TestDaemon::start().await;
        let mut a = td.client("a");
        let mut b = td.client("b");
        let bytes = png(50, 50, 3000, 3);
        // 失败的一组：声明的 sha256 是别的数据的
        let declared = if good { sha(&bytes) } else { sha(b"something else") };
        let up: BlobPutBeginResult = a
            .call(
                &td,
                methods::BLOB_PUT_BEGIN,
                json!({ "name": "x.png", "mime": PNG, "size": bytes.len(), "sha256": declared }),
            )
            .await
            .unwrap();
        chunk(&td, &mut a, &up.blob_id, 0, &bytes).await.unwrap();
        let hold = td.ctx.blobs.hold_next_finalize();
        let (first, second) = tokio::join!(end(&td, &mut a, &up.blob_id), async {
            within("the finalizer", hold.reached()).await;
            // 第一个已经接手、停在校验之前：第二个不能看到 `not_found`，要等
            let mut second = Box::pin(end(&td, &mut b, &up.blob_id));
            assert!(
                tokio::time::timeout(Duration::from_millis(250), &mut second).await.is_err(),
                "the second put_end waits for the first"
            );
            hold.release();
            within("the second put_end", second).await
        });
        if good {
            let (first, second) = (first.unwrap(), second.unwrap());
            assert_eq!(first, second, "the same AttachmentRef");
            assert_eq!(std::fs::read(blob_file(&td, &first, "png")).unwrap(), bytes);
            // 事后再来的 put_end（响应丢了的重试）也是同一个结果
            assert_eq!(end(&td, &mut b, &up.blob_id).await.unwrap(), first);
        } else {
            let (e1, e2) = (first.unwrap_err(), second.unwrap_err());
            assert_eq!((code(&e1), reason(&e1)), (ErrorCode::Invalid, Some("hash_mismatch")));
            assert_eq!(e1, e2, "the same error, not not_found");
            // 上传作废了：事后再来的才是 not_found
            assert_eq!(end(&td, &mut b, &up.blob_id).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
            assert!(!part_file(&td, &up.blob_id).exists());
        }
        td.shutdown().await;
    }
}

#[tokio::test]
async fn a_chunk_that_arrives_while_put_end_is_finalizing_is_refused() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let bytes = png(100, 100, CHUNK_SIZE as usize + 4000, 70); // 两块
    let parts: Vec<Vec<u8>> = bytes.chunks(CHUNK_SIZE as usize).map(<[u8]>::to_vec).collect();
    let up = upload_chunks_only(&td, &mut a, "x.png", &bytes).await;
    let hold = td.ctx.blobs.hold_next_finalize();
    let evil = vec![0u8; parts[1].len()];
    let (finished, ()) = tokio::join!(end(&td, &mut a, &up.blob_id), async {
        within("the finalizer", hold.reached()).await;
        // 收尾已经接手：换了内容的重发被拒绝，内容一样的重发也被拒绝（不留「碰巧一样」的窗口）
        for data in [&evil, &parts[1]] {
            let e = within("a chunk during finalizing", chunk(&td, &mut b, &up.blob_id, 1, data)).await.unwrap_err();
            assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("finalizing")));
        }
        hold.release();
    });
    let a_ref = finished.unwrap();
    // 落盘的内容就是校验过的内容
    let on_disk = std::fs::read(blob_file(&td, &a_ref, "png")).unwrap();
    assert_eq!(sha(&on_disk), sha(&bytes));
    assert_eq!(on_disk, bytes);
    // 完成之后到的块：上传已经不存在
    assert_eq!(chunk(&td, &mut b, &up.blob_id, 1, &evil).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    assert_eq!(std::fs::read(blob_file(&td, &a_ref, "png")).unwrap(), bytes, "the late chunk changed nothing");
    td.shutdown().await;
}

#[tokio::test]
async fn a_cancelled_put_end_still_finishes_the_upload() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let bytes = small_png(73);
    let up = upload_chunks_only(&td, &mut a, "x.png", &bytes).await;
    let hold = td.ctx.blobs.hold_next_finalize();
    {
        // 客户端在 `put_end` 收尾的当口断开：请求的 future 被丢弃
        let mut first = Box::pin(end(&td, &mut a, &up.blob_id));
        tokio::select! {
            r = &mut first => panic!("put_end returned before the finalizer was released: {r:?}"),
            () = within("the finalizer", hold.reached()) => {}
        }
    }
    hold.release();
    // 收尾在自己的任务里，不跟着请求一起死：记录写好了，不会卡在「收尾中」
    assert!(eventually(T, || async { td.ctx.store.blob_get(&up.blob_id).await.unwrap().is_some() }).await);
    let a_ref = within("put_end", end(&td, &mut b, &up.blob_id)).await.unwrap();
    assert_eq!(std::fs::read(blob_file(&td, &a_ref, "png")).unwrap(), bytes);
    assert!(!part_file(&td, &up.blob_id).exists());
    td.shutdown().await;
}

#[tokio::test]
async fn put_end_waits_for_a_chunk_write_that_is_already_in_flight() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let bytes = png(100, 100, CHUNK_SIZE as usize + 100, 71);
    let up = upload_chunks_only(&td, &mut a, "x.png", &bytes).await;
    // 有一块已经拿到文件句柄、写到一半
    let write = td.ctx.blobs.hold_chunk_writes(&up.blob_id).await.expect("the upload exists");
    let mut finishing = Box::pin(end(&td, &mut a, &up.blob_id));
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut finishing).await.is_err(),
        "put_end does not start verifying while a write is in flight"
    );
    assert!(part_file(&td, &up.blob_id).exists(), "nothing was renamed yet");
    // 但它已经接手了：新来的块不再进来
    let e = within("a chunk during finalizing", chunk(&td, &mut b, &up.blob_id, 0, &bytes[..CHUNK_SIZE as usize]))
        .await
        .unwrap_err();
    assert_eq!(reason(&e), Some("finalizing"));
    drop(write);
    let a_ref = within("put_end", finishing).await.unwrap();
    assert_eq!(std::fs::read(blob_file(&td, &a_ref, "png")).unwrap(), bytes);
    td.shutdown().await;
}

#[tokio::test]
async fn a_discard_during_finalizing_waits_and_then_removes_the_finished_blob() {
    let td = TestDaemon::start().await;
    let mut a = td.client("a");
    let mut b = td.client("b");
    let bytes = small_png(72);
    let up = upload_chunks_only(&td, &mut a, "x.png", &bytes).await;
    let hold = td.ctx.blobs.hold_next_finalize();
    let (finished, discarded) = tokio::join!(end(&td, &mut a, &up.blob_id), async {
        within("the finalizer", hold.reached()).await;
        let mut d = Box::pin(discard(&td, &mut b, &up.blob_id));
        assert!(tokio::time::timeout(Duration::from_millis(200), &mut d).await.is_err(), "discard waits");
        hold.release();
        within("discard", d).await
    });
    let a_ref = finished.unwrap();
    discarded.unwrap();
    // put_end 成功了（它的调用者拿到了引用），随后 discard 把这个还没被引用的 blob 删掉了
    assert!(!blob_file(&td, &a_ref, "png").exists());
    assert!(td.ctx.store.blob_get(&a_ref.blob_id).await.unwrap().is_none());
    td.shutdown().await;
}

// ---------------------------------------------------------------- 配额、剩余磁盘、统计

#[tokio::test]
async fn unreferenced_blobs_have_a_node_wide_quota_that_counts_uploads_in_progress() {
    let td = TestDaemon::start().await;
    td.ctx.blobs.set_unreferenced_quota(10_000);
    let (mut a, mut b) = (td.client("a"), td.client("b"));
    let x1 = upload(&td, &mut a, "x1.png", &small_png(1)).await; // 每张 3000 字节
    let x2 = upload(&td, &mut b, "x2.png", &small_png(2)).await; // 另一台设备的也算：全节点合计
    let _x3 = upload(&td, &mut a, "x3.png", &small_png(3)).await;
    // 9000 已用：再来 3000 超了；正好 1000 刚好到上限
    let e = begin_with(&td, "c", json!({ "size": 3000 })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Busy, Some("quota")));
    let in_progress = begin_with(&td, "c", json!({ "size": 1000 })).await.unwrap();
    // 进行中的上传也占着：满了
    let e = begin_with(&td, "d", json!({ "size": 1 })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Busy, Some("quota")));
    // 丢掉一个完成的 blob 腾出空间；丢掉进行中的也一样
    discard(&td, &mut a, &x1.blob_id).await.unwrap();
    let again = begin_with(&td, "d", json!({ "size": 3000 })).await.unwrap();
    discard(&td, &mut a, &again.blob_id).await.unwrap();
    discard(&td, &mut a, &in_progress.blob_id).await.unwrap();
    // 被消息引用之后不再算「未引用」：x2 发出去以后又有 3000 的空间
    let s = new_session(&td, &mut a).await;
    begin_with(&td, "e", json!({ "size": 4000 })).await.unwrap(); // 6000 + 4000 = 10000
    let e = begin_with(&td, "f", json!({ "size": 3000 })).await.unwrap_err();
    assert_eq!(reason(&e), Some("quota"));
    send(&td, &mut a, &s.id, "m1", "x2", &[&x2]).await.unwrap();
    begin_with(&td, "f", json!({ "size": 3000 })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn the_default_quota_is_one_gib_and_the_default_disk_reserve_is_one_gib() {
    assert_eq!(anycon_daemon::blobs::UNREFERENCED_QUOTA_BYTES, 1024 * 1024 * 1024);
    assert_eq!(anycon_daemon::blobs::DISK_RESERVE_BYTES, 1024 * 1024 * 1024);
    assert_eq!(anycon_daemon::blobs::MAX_MESSAGE_BYTES, 20 * 1024 * 1024);
    // 一次上传的大小本身也受它们约束：10 MiB 的单张在默认配额下没问题
    let td = TestDaemon::start().await;
    begin_with(&td, "a", json!({ "size": 10 * 1024 * 1024 })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn uploads_are_refused_when_the_data_volume_is_nearly_full() {
    let td = TestDaemon::start().await;
    // 底线高得不可能满足：剩余空间 < size + 底线
    td.ctx.blobs.set_disk_reserve(u64::MAX / 2);
    let e = begin_with(&td, "a", json!({ "size": 100 })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Busy, Some("disk_full")));
    assert_eq!(std::fs::read_dir(td.paths.blobs_tmp()).unwrap().count(), 0, "nothing was created");
    // 底线放下来就收（这台机器的剩余空间 > 0）
    td.ctx.blobs.set_disk_reserve(0);
    begin_with(&td, "a", json!({ "size": 100 })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn blob_statistics_count_files_bytes_and_uploads() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let zero = td.ctx.blobs.stats().await.unwrap();
    assert_eq!((zero.count, zero.bytes, zero.unreferenced_bytes, zero.uploads), (0, 0, 0, 0));
    assert!(zero.disk_free > 0);
    let a = upload(&td, &mut c, "a.png", &small_png(1)).await;
    let _b = upload(&td, &mut c, "b.png", &small_png(2)).await;
    let _pending = begin(&td, &mut c, "c.png", PNG, &small_png(3)).await;
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m1", "a", &[&a]).await.unwrap();
    let st = td.ctx.blobs.stats().await.unwrap();
    assert_eq!((st.count, st.bytes, st.unreferenced_bytes, st.uploads), (2, 6000, 3000, 1));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 名字、文件头

#[tokio::test]
async fn names_with_bidirectional_controls_or_line_separators_are_refused() {
    let td = TestDaemon::start().await;
    for (i, bad) in [
        "invoice\u{202E}gnp.exe",
        "a\u{200E}b.png",
        "a\u{200F}b.png",
        "x\u{2066}y\u{2069}.png",
        "line\u{2028}break.png",
        "para\u{2029}break.png",
        "c1\u{85}control.png",
    ]
    .iter()
    .enumerate()
    {
        let e = begin_with(&td, &format!("n{i}"), json!({ "name": bad })).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::Invalid, "{bad:?}");
    }
    // 普通的非 ASCII 名字、零宽连接符（表情序列用）不受影响
    begin_with(&td, "ok", json!({ "name": "截图 😀.png" })).await.unwrap();
    begin_with(&td, "ok", json!({ "name": "family\u{200D}pic.png" })).await.unwrap();
    td.shutdown().await;
}

#[tokio::test]
async fn the_recorded_name_carries_the_extension_of_the_mime_type() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "photo", &small_png(1)).await;
    assert_eq!(a.name, "photo.png");
    let jpeg = jpeg_prefix_with(b"jpeg body");
    let h = upload_as(&td, &mut c, "x.html", "image/jpeg", &jpeg).await;
    assert_eq!(h.name, "x.html.jpg", "an extension that does not match the type is not trusted");
    assert!(blob_file(&td, &h, "jpg").exists());
    // 对得上的原样保留（不分大小写；JPEG 认 jpg 和 jpeg）
    assert_eq!(upload_as(&td, &mut c, "PHOTO.JPEG", "image/jpeg", &jpeg).await.name, "PHOTO.JPEG");
    assert_eq!(upload(&td, &mut c, "Shot.PNG", &small_png(2)).await.name, "Shot.PNG");
    // 消息里要用 `put_end` 返回的记录；原来的名字对不上记录
    let s = new_session(&td, &mut c).await;
    let mut original = a.clone();
    original.name = "photo".into();
    let e = send(&td, &mut c, &s.id, "m1", "", &[&original]).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    send(&td, &mut c, &s.id, "m2", "", &[&a]).await.unwrap();
    assert_eq!(user_messages(&td, &mut c, &s.id).await[0].2[0].name, "photo.png");
    assert_eq!(summary(&td, &mut c, &s.id).await.title, "[image] photo.png");
    td.shutdown().await;
}

#[tokio::test]
async fn the_header_check_compares_a_prefix_and_does_not_decode_the_image() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // JPEG 的前三个字节 + HTML：文件头只是前缀比对，不是解码——收下，宽高读不出来就省略。
    // 记录下来的名字被补上了 `.jpg`，下载 / 展示都不会再是 `.html`
    let html = jpeg_prefix_with(b"<html><script>alert(1)</script></html>");
    let a = upload_as(&td, &mut c, "evil.html", "image/jpeg", &html).await;
    assert_eq!((a.mime.as_str(), a.name.as_str(), a.width, a.height), ("image/jpeg", "evil.html.jpg", None, None));
    // 完全不像图片的内容 / 别的类型的头仍然被拒绝
    let b = begin(&td, &mut c, "x.png", "image/jpeg", b"<svg xmlns='http://www.w3.org/2000/svg'/>").await;
    chunk(&td, &mut c, &b.blob_id, 0, b"<svg xmlns='http://www.w3.org/2000/svg'/>").await.unwrap();
    let e = end(&td, &mut c, &b.blob_id).await.unwrap_err();
    assert_eq!(reason(&e), Some("mime_mismatch"));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 创建的幂等重试

#[tokio::test]
async fn a_create_retried_after_a_timeout_returns_the_session_the_first_attempt_made() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let imgs = upload_many(&td, &mut c, "r", 3, 5).await;
    // 带文字 + 两张图；第一次的 turn 还在跑（hold）时重试
    let first = json!({ "agent": "fake", "prompt": "hold on", "message_id": "retry-1", "attachments": &imgs[..2] });
    let s1: SessionSummary = c.call(&td, methods::SESSION_CREATE, first.clone()).await.unwrap();
    let again: SessionSummary =
        c.call(&td, methods::SESSION_CREATE, first.clone()).await.expect("a retry gets the session, not blob_in_use");
    assert_eq!(again.id, s1.id);
    td.fake.last_handle().release();
    wait_status(&td, &mut c, &s1.id, SessionStatus::Idle).await;
    // turn 结束以后才重试，同样
    let later: SessionSummary = c.call(&td, methods::SESSION_CREATE, first).await.unwrap();
    assert_eq!(later.id, s1.id);
    // 只有附件、没有文字的创建
    let only = json!({ "agent": "fake", "message_id": "retry-2", "attachments": &imgs[2..] });
    let s2: SessionSummary = c.call(&td, methods::SESSION_CREATE, only.clone()).await.unwrap();
    let again: SessionSummary = c.call(&td, methods::SESSION_CREATE, only).await.unwrap();
    assert_eq!(again.id, s2.id);
    // 没有多出会话、没有多出消息；附件仍然绑在第一次的会话上
    let all: SessionListResult = c.call(&td, methods::SESSION_LIST, json!({})).await.unwrap();
    assert_eq!(all.sessions.len(), 2);
    assert_eq!(user_messages(&td, &mut c, &s1.id).await.len(), 1);
    assert_eq!(bound_session(&td, &imgs[0].blob_id).await.as_deref(), Some(s1.id.as_str()));
    assert_eq!(bound_session(&td, &imgs[2].blob_id).await.as_deref(), Some(s2.id.as_str()));
    td.shutdown().await;
}

// ---------------------------------------------------------------- 重启、promote / 归档、保留期

#[tokio::test]
async fn attachments_in_the_history_survive_a_daemon_restart() {
    let mut td = TestDaemon::start().await;
    let mut c = td.client("a");
    let bytes = png(800, 600, CHUNK_SIZE as usize + 500, 90); // blob.get 要分两页
    let a = upload(&td, &mut c, "keep.png", &bytes).await;
    let loose_bytes = small_png(91);
    let loose = upload(&td, &mut c, "loose.png", &loose_bytes).await;
    let s = new_session(&td, &mut c).await;
    send(&td, &mut c, &s.id, "m1", "look", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    td.restart().await;
    let mut c = td.client("a");
    // 历史里的附件（含宽高）、绑定、文件、`blob.get`
    assert_eq!(user_messages(&td, &mut c, &s.id).await, vec![("m1".to_string(), "look".to_string(), vec![a.clone()])]);
    assert_eq!(bound_session(&td, &a.blob_id).await.as_deref(), Some(s.id.as_str()));
    assert_eq!(fetch(&td, &mut c, &a.blob_id).await, bytes);
    assert_eq!(get(&td, &mut c, &a.blob_id, 0, None).await.unwrap().blob, a);
    // 没被引用的 blob 也在
    assert_eq!(fetch(&td, &mut c, &loose.blob_id).await, loose_bytes);
    // 同一个会话里换个 message_id 再引用同一个 blob：agent 读到的还是那个文件
    send(&td, &mut c, &s.id, "m2", "again", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let inputs = td.fake.last_handle().inputs.lock().unwrap().clone();
    let last = inputs.last().unwrap();
    assert_eq!(
        (last.attachments.clone(), last.attachment_paths.clone()),
        (vec![a.clone()], vec![blob_file(&td, &a, "png")])
    );
    assert_eq!(std::fs::read(&last.attachment_paths[0]).unwrap(), bytes);
    td.shutdown().await;
}

#[tokio::test]
async fn promote_archive_and_unarchive_keep_the_images_of_a_session() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.png", &small_png(1)).await;
    let b = upload(&td, &mut c, "b.png", &small_png(2)).await;
    let s = new_session(&td, &mut c).await; // scratch 会话
    send(&td, &mut c, &s.id, "m1", "first", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    async fn intact(td: &TestDaemon, c: &mut TestClient, sid: &str, a: &AttachmentRef, when: &str) {
        let msgs = user_messages(td, c, sid).await;
        assert_eq!(msgs[0].2, vec![a.clone()], "{when}: the history still names the attachment");
        assert_eq!(bound_session(td, &a.blob_id).await.as_deref(), Some(sid), "{when}: still bound");
        assert!(blob_file(td, a, "png").exists(), "{when}: the file is there");
        assert_eq!(get(td, c, &a.blob_id, 0, None).await.unwrap().blob, *a, "{when}: blob.get works");
    }
    // promote（scratch → 项目）
    let _: Empty =
        c.call(&td, methods::SESSION_PROMOTE, json!({ "session_id": s.id, "name": "promoted" })).await.unwrap();
    intact(&td, &mut c, &s.id, &a, "after promote").await;
    // 归档
    let _: Empty = c.call(&td, methods::SESSION_ARCHIVE, json!({ "session_id": s.id })).await.unwrap();
    intact(&td, &mut c, &s.id, &a, "archived").await;
    // 已归档的会话不收消息；被拒绝的消息没有把新 blob 绑上
    let e = send(&td, &mut c, &s.id, "m2", "nope", &[&b]).await.unwrap_err();
    assert_eq!(reason(&e), Some("archived"));
    assert_eq!(bound_session(&td, &b.blob_id).await, None);
    // 取消归档之后：同一个 blob 换个 message_id 再引用、新的 blob 也行
    let _: Empty = c.call(&td, methods::SESSION_UNARCHIVE, json!({ "session_id": s.id })).await.unwrap();
    intact(&td, &mut c, &s.id, &a, "unarchived").await;
    send(&td, &mut c, &s.id, "m3", "same image again", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    send(&td, &mut c, &s.id, "m4", "and a new one", &[&b]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(bound_session(&td, &b.blob_id).await.as_deref(), Some(s.id.as_str()));
    let inputs = td.fake.handle(td.fake.spawn_count() - 1).inputs.lock().unwrap().clone();
    assert!(inputs
        .iter()
        .any(|i| i.attachments == vec![b.clone()] && i.attachment_paths == vec![blob_file(&td, &b, "png")]));
    td.shutdown().await;
}

#[tokio::test]
async fn the_retention_cleanup_removes_the_files_of_the_sessions_it_deletes() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let old_img = upload(&td, &mut c, "old.png", &small_png(1)).await;
    let new_img = upload(&td, &mut c, "new.png", &small_png(2)).await;
    let loose = upload(&td, &mut c, "loose.png", &small_png(3)).await;
    let old = new_session(&td, &mut c).await;
    send(&td, &mut c, &old.id, "m1", "old", &[&old_img]).await.unwrap();
    wait_status(&td, &mut c, &old.id, SessionStatus::Idle).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let fresh = new_session(&td, &mut c).await;
    send(&td, &mut c, &fresh.id, "m2", "new", &[&new_img]).await.unwrap();
    wait_status(&td, &mut c, &fresh.id, SessionStatus::Idle).await;

    anycon_daemon::config::set_value(&td.paths, "sessions.history_retention", "\"1s\"").unwrap();
    td.ctx.config.reload().unwrap();
    td.ctx.sessions.cleanup().await;

    // 过期的会话连同它的附件（文件和记录）一起删了
    let e = c.call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": old.id })).await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::NotFound);
    assert!(!blob_file(&td, &old_img, "png").exists());
    assert!(td.ctx.store.blob_get(&old_img.blob_id).await.unwrap().is_none());
    assert_eq!(get(&td, &mut c, &old_img.blob_id, 0, None).await.unwrap_err().stable_code(), Some(ErrorCode::NotFound));
    // 没到期的会话和没被引用的 blob 不受影响
    assert!(blob_file(&td, &new_img, "png").exists());
    assert_eq!(bound_session(&td, &new_img.blob_id).await.as_deref(), Some(fresh.id.as_str()));
    assert!(blob_file(&td, &loose, "png").exists());
    td.shutdown().await;
}
