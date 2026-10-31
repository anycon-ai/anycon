//! 任意文件附件（ADR-44，docs/spec/daemon.md § 7.10，protocol.md § 5.6）：`blob.put_begin` 接受任意 mime；agent 不原生收的附件
//! 复制进会话工作目录的 `.anycon/attachments/<session_id>/`，路径清单追加在交给 adapter 的文字末尾；落盘的安全约束
//! （真目录、权限、`info/exclude`）、积压合并、插话、失败处理、`session.delete` 清理。图片的校验和原生传递在 `attachments.rs`。

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use anycon_daemon::blobs::CHUNK_SIZE;
use anycon_daemon::testutil::{assert_mode, eventually, InterjectFailure, TestClient, TestDaemon};
use anycon_daemon::util::human_bytes;
use anycon_protocol::*;
use base64::Engine as _;
use common::wt::Fx;
use serde_json::json;
use sha2::{Digest, Sha256};

const T: Duration = Duration::from_secs(10);
const HEADING: &str = "[Attached files, saved in the working directory]";

// ---------------------------------------------------------------- 夹具

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("a stable error code")
}

fn reason(e: &RpcError) -> Option<&str> {
    e.data.as_ref().and_then(|d| d.reason.as_deref())
}

fn png(seed: u8) -> Vec<u8> {
    let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&640u32.to_be_bytes());
    v.extend_from_slice(&480u32.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    v.resize(2000, seed);
    v
}

/// 一段文本内容（不是任何已知格式）。
fn blob(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| seed.wrapping_add((i % 251) as u8)).collect()
}

async fn put(
    td: &TestDaemon,
    c: &mut TestClient,
    name: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<AttachmentRef, RpcError> {
    let b: BlobPutBeginResult = c
        .call(
            td,
            methods::BLOB_PUT_BEGIN,
            json!({ "name": name, "mime": mime, "size": bytes.len(), "sha256": sha(bytes) }),
        )
        .await?;
    for (i, part) in bytes.chunks(CHUNK_SIZE as usize).enumerate() {
        let data = base64::engine::general_purpose::STANDARD.encode(part);
        let _: Empty =
            c.call(td, methods::BLOB_PUT_CHUNK, json!({ "blob_id": b.blob_id, "index": i, "data": data })).await?;
    }
    c.call(td, methods::BLOB_PUT_END, json!({ "blob_id": b.blob_id })).await
}

async fn upload(td: &TestDaemon, c: &mut TestClient, name: &str, mime: &str, bytes: &[u8]) -> AttachmentRef {
    put(td, c, name, mime, bytes).await.unwrap()
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

async fn create(td: &TestDaemon, c: &mut TestClient, project: Option<&str>) -> SessionSummary {
    let mut p = json!({ "agent": "fake" });
    if let Some(project) = project {
        p["project"] = json!(project);
    }
    c.call(td, methods::SESSION_CREATE, p).await.unwrap()
}

async fn wait_status(td: &TestDaemon, c: &mut TestClient, sid: &str, want: SessionStatus) {
    let t = std::time::Instant::now();
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if s.status == want {
            return;
        }
        assert!(t.elapsed() < T, "the session never reached {want:?}; it is {:?}", s.status);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<SessionEvent> {
    let r: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    r.events
}

/// 历史里 `error` 事件的 `(code, message)`。
async fn errors(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<(String, String)> {
    history(td, c, sid)
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::Error { code, message, .. } => Some((code, message)),
            _ => None,
        })
        .collect()
}

fn blob_file(td: &TestDaemon, a: &AttachmentRef, ext: &str) -> PathBuf {
    td.paths.blobs_dir().join(format!("{}.{ext}", a.blob_id))
}

fn attach_dir(cwd: &Path, sid: &str) -> PathBuf {
    cwd.join(".anycon").join("attachments").join(sid)
}

fn note(sid: &str, files: &[(&str, &str, u64)]) -> String {
    let mut s = HEADING.to_string();
    for (name, mime, bytes) in files {
        s.push_str(&format!("\n- .anycon/attachments/{sid}/{name} ({mime}, {})", human_bytes(*bytes)));
    }
    s
}

fn input(td: &TestDaemon, handle: usize, n: usize) -> anycon_agent_api::UserInput {
    td.fake.handle(handle).inputs.lock().unwrap()[n].clone()
}

fn scratch(td: &TestDaemon, sid: &str) -> PathBuf {
    td.ctx.workspace.scratch_path(sid)
}

// ---------------------------------------------------------------- 上传

#[tokio::test]
async fn node_info_advertises_file_attachments_and_the_limits() {
    let td = TestDaemon::start_with(&[("sessions.max_attachment_bytes", "\"4KiB\"")], false).await;
    let mut c = td.client("a");
    let info: NodeInfo = c.call(&td, methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.blobs && info.capabilities.file_attachments);
    let limits = info.attachment_limits.expect("limits are given when blobs are supported");
    assert_eq!((limits.max_bytes, limits.max_per_message, limits.max_message_bytes), (4096, 20, 20 * 1024 * 1024));
    td.shutdown().await;
}

#[tokio::test]
async fn any_mime_is_accepted_and_odd_ones_become_octet_stream() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // (声明的 mime, 文件名, 记录的 mime, blobs/ 里的扩展名)
    for (declared, name, mime, ext) in [
        ("application/pdf", "report.pdf", "application/pdf", "pdf"),
        ("TEXT/Plain; charset=utf-8", "notes.TXT", "text/plain", "txt"),
        ("", "data", "application/octet-stream", "bin"),
        ("nonsense", "x.c++", "application/octet-stream", "bin"),
        ("a/b/c", ".bashrc", "application/octet-stream", "bin"),
        ("image/", "archive.tar.gz", "application/octet-stream", "gz"),
        ("image/svg+xml", "pic.svg", "image/svg+xml", "svg"),
        ("image/bmp", "pic.bmp", "image/bmp", "bmp"),
        ("application/x-weird", "x.abcdefghijklmnopq", "application/x-weird", "bin"),
        ("application/zip", "x.", "application/zip", "bin"),
    ] {
        let bytes = blob(7, 300);
        let r = upload(&td, &mut c, name, declared, &bytes).await;
        assert_eq!((r.name.as_str(), r.mime.as_str(), r.bytes), (name, mime, 300), "{declared:?} {name:?}");
        assert_eq!((r.width, r.height), (None, None), "only images have a size");
        let file = blob_file(&td, &r, ext);
        assert_eq!(std::fs::read(&file).unwrap(), bytes, "{declared:?} {name:?}: expected {}", file.display());
        assert_mode(&file, 0o600);
        // 下载原样
        let got: BlobGetResult = c.call(&td, methods::BLOB_GET, json!({ "blob_id": r.blob_id })).await.unwrap();
        assert_eq!((got.blob.mime.as_str(), got.eof), (mime, true));
    }
    td.shutdown().await;
}

#[tokio::test]
async fn only_the_four_image_types_are_checked_for_their_header() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    // 声明为图片（大小写不论）而文件头不对：拒绝
    for mime in ["image/png", "IMAGE/PNG", "image/jpeg; q=1"] {
        let e = put(&td, &mut c, "x.png", mime, b"not an image at all").await.unwrap_err();
        assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("mime_mismatch")), "{mime}");
    }
    // 不是图片就不看内容：PNG 的字节可以声明成 PDF（daemon 不解释非图片）
    let r = upload(&td, &mut c, "a.pdf", "application/pdf", &png(1)).await;
    assert_eq!((r.mime.as_str(), r.width), ("application/pdf", None));
    // 真图片照旧读宽高、补扩展名
    let r = upload(&td, &mut c, "shot", "IMAGE/PNG", &png(2)).await;
    assert_eq!((r.mime.as_str(), r.name.as_str(), r.width, r.height), ("image/png", "shot.png", Some(640), Some(480)));
    // 非图片的名字不补扩展名
    let r = upload(&td, &mut c, "x.html", "application/pdf", b"%PDF").await;
    assert_eq!(r.name, "x.html");
    td.shutdown().await;
}

#[tokio::test]
async fn the_size_limit_and_name_rules_apply_to_every_file() {
    let td = TestDaemon::start_with(&[("sessions.max_attachment_bytes", "\"4KiB\"")], false).await;
    let mut c = td.client("a");
    let e = put(&td, &mut c, "big.pdf", "application/pdf", &blob(1, 4097)).await.unwrap_err();
    assert_eq!((code(&e), reason(&e)), (ErrorCode::Invalid, Some("too_large")));
    put(&td, &mut c, "ok.pdf", "application/pdf", &blob(1, 4096)).await.unwrap();
    let e = put(&td, &mut c, "a\nb.pdf", "application/pdf", b"x").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    let e = put(&td, &mut c, "a/b.pdf", "application/pdf", b"x").await.unwrap_err();
    assert_eq!(code(&e), ErrorCode::Invalid);
    td.shutdown().await;
}

#[tokio::test]
async fn startup_keeps_recorded_files_of_any_extension_and_removes_orphans() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let kept = upload(&td, &mut c, "keep.pdf", "application/pdf", b"%PDF-1").await;
    let orphan = td.paths.blobs_dir().join(format!("{:032x}.pdf", 0xabcdefu64));
    std::fs::write(&orphan, b"leftover").unwrap();
    let stranger = td.paths.blobs_dir().join("notes.txt");
    std::fs::write(&stranger, b"not ours").unwrap();
    let mut td = td;
    td.restart().await;
    assert!(blob_file(&td, &kept, "pdf").exists(), "a recorded blob stays");
    assert!(!orphan.exists(), "a file with a blob-shaped name and no record is removed");
    assert!(stranger.exists(), "other names are not touched");
    td.shutdown().await;
}

// ---------------------------------------------------------------- 落到工作目录（scratch 会话）

#[tokio::test]
async fn a_non_native_attachment_is_saved_in_the_working_directory_and_listed_in_the_text() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let pdf_bytes = blob(3, 3000);
    let pdf = upload(&td, &mut c, "report.pdf", "application/pdf", &pdf_bytes).await;
    let img = upload(&td, &mut c, "shot.png", "image/png", &png(4)).await;
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "see these", &[&pdf, &img]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    // adapter：图片原生（`attachments` / `attachment_paths` 一一对应），文件只出现在文字里
    let got = input(&td, 0, 0);
    assert_eq!(got.attachments, vec![img.clone()]);
    assert_eq!(got.attachment_paths, vec![blob_file(&td, &img, "png")]);
    assert_eq!(got.content, format!("see these\n\n{}", note(&s.id, &[("report.pdf", "application/pdf", 3000)])));
    // 文件在工作目录里：内容一致，目录 0700、文件 0600
    let dir = attach_dir(&scratch(&td, &s.id), &s.id);
    assert_eq!(std::fs::read(dir.join("report.pdf")).unwrap(), pdf_bytes);
    assert_mode(&dir.join("report.pdf"), 0o600);
    assert_mode(&dir, 0o700);
    assert_mode(dir.parent().unwrap(), 0o700);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "the native image is not copied");
    // 事件流里是用户的原文和附件元数据，没有追加的文字
    let users: Vec<(String, Vec<AttachmentRef>)> = history(&td, &mut c, &s.id)
        .await
        .into_iter()
        .filter_map(|e| match e.event {
            Event::UserMessage { content, attachments, .. } => Some((content, attachments)),
            _ => None,
        })
        .collect();
    assert_eq!(users, vec![("see these".to_string(), vec![pdf.clone(), img.clone()])]);
    td.shutdown().await;
}

#[tokio::test]
async fn a_message_with_only_a_file_has_the_list_alone_as_its_text() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let f = upload(&td, &mut c, "a.csv", "text/csv", b"a,b\n1,2\n").await;
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "", &[&f]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(input(&td, 0, 0).content, note(&s.id, &[("a.csv", "text/csv", 8)]));
    // 标题 / 摘要用 `[file] 名字`
    let sum: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(sum.title, "[file] a.csv");
    td.shutdown().await;
}

#[tokio::test]
async fn files_with_the_same_name_are_numbered_and_one_blob_is_saved_once() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "report.pdf", "application/pdf", &blob(1, 100)).await;
    let b = upload(&td, &mut c, "report.pdf", "application/pdf", &blob(2, 200)).await;
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "two", &[&a, &b]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let dir = attach_dir(&scratch(&td, &s.id), &s.id);
    assert_eq!(std::fs::read(dir.join("report.pdf")).unwrap(), blob(1, 100));
    assert_eq!(std::fs::read(dir.join("report (2).pdf")).unwrap(), blob(2, 200));
    assert_eq!(
        input(&td, 0, 0).content,
        format!(
            "two\n\n{}",
            note(&s.id, &[("report.pdf", "application/pdf", 100), ("report (2).pdf", "application/pdf", 200)])
        )
    );
    // 同一个 blob 再发一次：沿用已有的文件，不再复制出 `(3)`
    send(&td, &mut c, &s.id, "m2", "again", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(
        input(&td, 0, 1).content,
        format!("again\n\n{}", note(&s.id, &[("report.pdf", "application/pdf", 100)]))
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    td.shutdown().await;
}

#[tokio::test]
async fn images_the_agent_does_not_take_natively_are_saved_like_any_file() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let img = upload(&td, &mut c, "shot.png", "image/png", &png(5)).await;
    *td.fake.attachments.lock().unwrap() = vec![];
    let _: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({ "refresh": true })).await.unwrap();
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "look", &[&img]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let got = input(&td, 0, 0);
    assert!(got.attachments.is_empty() && got.attachment_paths.is_empty());
    assert_eq!(got.content, format!("look\n\n{}", note(&s.id, &[("shot.png", "image/png", 2000)])));
    assert_eq!(std::fs::read(attach_dir(&scratch(&td, &s.id), &s.id).join("shot.png")).unwrap(), png(5));
    td.shutdown().await;
}

#[tokio::test]
async fn a_message_without_files_is_handed_over_unchanged() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let img = upload(&td, &mut c, "shot.png", "image/png", &png(6)).await;
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "just text", &[]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    send(&td, &mut c, &s.id, "m2", "native image", &[&img]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(input(&td, 0, 0).content, "just text");
    assert_eq!(input(&td, 0, 1).content, "native image");
    assert!(!scratch(&td, &s.id).join(".anycon").exists(), "nothing is created when there is nothing to save");
    td.shutdown().await;
}

// ---------------------------------------------------------------- 积压合并和插话

#[tokio::test]
async fn backlog_messages_merge_into_one_turn_with_one_list_in_message_order() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.pdf", "application/pdf", &blob(1, 10)).await;
    let b = upload(&td, &mut c, "b.txt", "text/plain", &blob(2, 20)).await;
    let img = upload(&td, &mut c, "c.png", "image/png", &png(7)).await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    assert!(send(&td, &mut c, &s.id, "m1", "first", &[&a]).await.unwrap().queued);
    assert!(send(&td, &mut c, &s.id, "m2", "second", &[&b, &img]).await.unwrap().queued);
    td.fake.last_handle().release();
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 2 }).await);
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    let merged = input(&td, 0, 1);
    assert_eq!(merged.attachments, vec![img.clone()]);
    assert!(merged.content.contains("first") && merged.content.contains("second") && merged.content.contains("---"));
    assert!(
        merged.content.ends_with(&format!(
            "\n\n{}",
            note(&s.id, &[("a.pdf", "application/pdf", 10), ("b.txt", "text/plain", 20)])
        )),
        "one list at the end, in message order: {}",
        merged.content
    );
    assert_eq!(merged.content.matches(HEADING).count(), 1);
    td.shutdown().await;
}

#[tokio::test]
async fn an_interjected_message_gets_its_files_saved_and_listed_once_even_when_requeued() {
    let td = TestDaemon::start().await;
    td.fake.interject.store(true, Ordering::SeqCst);
    td.ctx.agents.refresh_all().await;
    let mut c = td.client("a");
    let f1 = upload(&td, &mut c, "one.pdf", "application/pdf", &blob(1, 10)).await;
    let f2 = upload(&td, &mut c, "two.pdf", "application/pdf", &blob(2, 20)).await;
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold on", "message_id": "m0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Running).await;
    // 插话成功：带清单
    assert!(!send(&td, &mut c, &s.id, "m1", "look", &[&f1]).await.unwrap().queued);
    assert!(eventually(T, || async { td.fake.handle(0).inputs.lock().unwrap().len() >= 2 }).await);
    let ij = input(&td, 0, 1);
    assert!(ij.interject);
    assert_eq!(ij.content, format!("look\n\n{}", note(&s.id, &[("one.pdf", "application/pdf", 10)])));
    td.fake.last_handle().release();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;

    // 插话失败退回队列：下一个 turn 沿用已经保存的文件，不出现 `two (2).pdf`
    let s2: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hold again", "message_id": "n0" }))
        .await
        .unwrap();
    wait_status(&td, &mut c, &s2.id, SessionStatus::Running).await;
    *td.fake.interject_failure.lock().unwrap() = Some(InterjectFailure::TurnEnded);
    assert!(!send(&td, &mut c, &s2.id, "n1", "retry me", &[&f2]).await.unwrap().queued);
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
    assert!(!next.interject);
    assert_eq!(next.content, format!("retry me\n\n{}", note(&s2.id, &[("two.pdf", "application/pdf", 20)])));
    let saved: Vec<String> = std::fs::read_dir(attach_dir(&scratch(&td, &s2.id), &s2.id))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(saved, vec!["two.pdf".to_string()]);
    td.shutdown().await;
}

// ---------------------------------------------------------------- 项目会话：git、符号链接、失败

#[tokio::test]
async fn in_a_git_project_the_directory_is_excluded_and_the_work_tree_stays_clean() {
    let mut f = Fx::new().await;
    let f1 = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 50)).await;
    let s = create(&f.td, &mut f.c, Some("proj")).await;
    send(&f.td, &mut f.c, &s.id, "m1", "here", &[&f1]).await.unwrap();
    wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Idle).await;
    let dir = attach_dir(&f.repo, &s.id);
    assert_eq!(std::fs::read(dir.join("a.pdf")).unwrap(), blob(1, 50));
    let exclude = std::fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| l.trim() == "/.anycon/").count(), 1, "{exclude}");
    assert_eq!(f.td.git(&f.repo, &["status", "--porcelain"]), "", "the saved files are not untracked changes");
    // 再发一条：不重复写 exclude
    let f2 = upload(&f.td, &mut f.c, "b.pdf", "application/pdf", &blob(2, 60)).await;
    send(&f.td, &mut f.c, &s.id, "m2", "more", &[&f2]).await.unwrap();
    wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Idle).await;
    let exclude = std::fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| l.trim() == "/.anycon/").count(), 1, "{exclude}");
    assert!(dir.join("b.pdf").is_file());
    f.td.shutdown().await;
}

#[tokio::test]
async fn a_worktree_session_gets_its_files_in_the_worktree_after_it_is_created() {
    let mut f = Fx::new().await;
    let pdf = upload(&f.td, &mut f.c, "spec.pdf", "application/pdf", &blob(9, 400)).await;
    let s: SessionSummary = f
        .call(
            methods::SESSION_CREATE,
            json!({
                "agent": "fake", "project": "proj", "prompt": "implement the spec", "message_id": "m1",
                "attachments": [pdf], "workspace": { "mode": "worktree" }
            }),
        )
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let wt = f.dir_of(&s);
    // 工作目录是 worktree，不是项目目录
    assert_eq!(std::fs::read(attach_dir(&wt, &s.id).join("spec.pdf")).unwrap(), blob(9, 400));
    assert!(!f.repo.join(".anycon/attachments").exists());
    assert_eq!(
        input(&f.td, 0, 0).content,
        format!("implement the spec\n\n{}", note(&s.id, &[("spec.pdf", "application/pdf", 400)]))
    );
    assert_eq!(f.td.git(&wt, &["status", "--porcelain"]), "");
    let exclude = std::fs::read_to_string(f.repo.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| l.trim() == "/.anycon/").count(), 1, "{exclude}");
    f.td.shutdown().await;
}

#[tokio::test]
async fn without_git_a_project_session_still_gets_its_files() {
    let td = TestDaemon::start_without_git().await;
    let plain = td.workspace.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let mut c = td.client("a");
    let f = upload(&td, &mut c, "a.pdf", "application/pdf", &blob(1, 30)).await;
    let s = create(&td, &mut c, Some("plain")).await;
    send(&td, &mut c, &s.id, "m1", "x", &[&f]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(std::fs::read(attach_dir(&plain, &s.id).join("a.pdf")).unwrap(), blob(1, 30));
    assert!(!plain.join(".git").exists());
    td.shutdown().await;
}

/// `.anycon`、`attachments`、`<session_id>` 任何一级是符号链接都拒绝：turn 以 `attachment_failed` 收尾，链接指向的目录里
/// 什么都没有写，消息没有交给 agent；链接拿掉之后重发（同一个 blob）正常。
#[cfg(unix)]
#[tokio::test]
async fn a_symbolic_link_at_any_level_refuses_the_files() {
    for level in 0..3 {
        let mut f = Fx::new().await;
        let outside = f.td.dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let file = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 40)).await;
        let s = create(&f.td, &mut f.c, Some("proj")).await;
        // 把第 `level` 级（0 = `.anycon`，1 = `attachments`，2 = `<session_id>`）换成指向 `outside` 的符号链接
        let any = f.repo.join(".anycon");
        let link = match level {
            0 => any.clone(),
            1 => any.join("attachments"),
            _ => any.join("attachments").join(&s.id),
        };
        if level > 0 {
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        }
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        send(&f.td, &mut f.c, &s.id, "m1", "here", &[&file]).await.unwrap();
        wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Error).await;
        let errs = errors(&f.td, &mut f.c, &s.id).await;
        assert_eq!(errs.len(), 1, "level {level}: {errs:?}");
        assert_eq!(errs[0].0, "attachment_failed", "level {level}");
        assert!(errs[0].1.contains("symbolic link"), "level {level}: {}", errs[0].1);
        assert_eq!(
            std::fs::read_dir(&outside).unwrap().count(),
            0,
            "level {level}: nothing was written through the link"
        );
        assert!(f.td.fake.handles.lock().unwrap().iter().all(|h| h.inputs.lock().unwrap().is_empty()), "level {level}");
        // 用户的消息在历史里，turn 以 error 结束
        let kinds = history(&f.td, &mut f.c, &s.id).await;
        assert!(kinds.iter().any(|e| matches!(e.event, Event::TurnDone { reason: TurnDoneReason::Error, .. })));

        // 拿掉链接，重发同一个附件
        std::fs::remove_file(&link).unwrap();
        send(&f.td, &mut f.c, &s.id, "m2", "again", &[&file]).await.unwrap();
        wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Idle).await;
        assert!(attach_dir(&f.repo, &s.id).join("a.pdf").is_file(), "level {level}");
        f.td.shutdown().await;
    }
}

/// agent 在 daemon「决定打开」`attachments` 和真正打开它之间把它换成指向别处的符号链接（钩子在这一刻动手）：逐级 `O_NOFOLLOW` 的
/// 打开拒绝，turn 以 `attachment_failed` 收尾，链接指向的目录里什么都没有。
#[cfg(unix)]
#[tokio::test]
async fn a_level_swapped_for_a_link_just_before_it_is_opened_fails_the_turn_and_writes_nothing_behind_the_link() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let mut f = Fx::new().await;
    let outside = f.td.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let file = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 40)).await;
    let s = create(&f.td, &mut f.c, Some("proj")).await;
    let attachments = f.repo.join(".anycon/attachments");
    std::fs::create_dir_all(attachments.join(&s.id)).unwrap();
    // 链接的另一头有一个同名的 `<session_id>` 目录：路径版的实现会把文件写进去
    std::fs::create_dir_all(outside.join(&s.id)).unwrap();
    // 钩子是进程范围的（测试并行跑）：只认这个测试自己的临时目录里的 `.anycon/attachments`
    let tmp_name = f.td.dir.path().file_name().unwrap().to_string_lossy().into_owned();
    let fired = Arc::new(AtomicBool::new(false));
    let (flag, link_target, swapped) = (fired.clone(), outside.clone(), attachments.clone());
    let _hook = anycon_daemon::fsopen::add_before_open_hook(Box::new(move |p: &Path| {
        if p.ends_with(".anycon/attachments")
            && p.to_string_lossy().contains(&tmp_name)
            && !flag.swap(true, Ordering::SeqCst)
        {
            std::fs::rename(&swapped, swapped.with_file_name("attachments.moved")).unwrap();
            std::os::unix::fs::symlink(&link_target, &swapped).unwrap();
        }
    }));
    send(&f.td, &mut f.c, &s.id, "m1", "here", &[&file]).await.unwrap();
    wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Error).await;
    assert!(fired.load(Ordering::SeqCst), "the swap happened");
    let errs = errors(&f.td, &mut f.c, &s.id).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "attachment_failed");
    assert!(errs[0].1.contains("symbolic link"), "{}", errs[0].1);
    assert_eq!(std::fs::read_dir(outside.join(&s.id)).unwrap().count(), 0, "nothing was written behind the link");
    assert!(f.td.fake.handles.lock().unwrap().iter().all(|h| h.inputs.lock().unwrap().is_empty()));
    f.td.shutdown().await;
}

/// 目录写不进去（这里是 0500）：同样以 `attachment_failed` 收尾，消息里有文件名；修好之后重发正常。
#[cfg(unix)]
#[tokio::test]
async fn a_directory_that_cannot_be_written_fails_the_turn_with_the_file_name() {
    use std::os::unix::fs::PermissionsExt;
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let file = upload(&td, &mut c, "report.pdf", "application/pdf", &blob(1, 40)).await;
    let s = create(&td, &mut c, None).await;
    let dir = attach_dir(&scratch(&td, &s.id), &s.id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(dir.join("probe"), b"x").is_ok() {
        // root 不受权限位限制：这个用例没有意义
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        td.shutdown().await;
        return;
    }
    send(&td, &mut c, &s.id, "m1", "here", &[&file]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Error).await;
    let errs = errors(&td, &mut c, &s.id).await;
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].0, "attachment_failed");
    assert!(errs[0].1.contains("report.pdf"), "{}", errs[0].1);
    assert!(td.fake.handles.lock().unwrap().iter().all(|h| h.inputs.lock().unwrap().is_empty()));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no partial file is left");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    send(&td, &mut c, &s.id, "m2", "again", &[&file]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert_eq!(input(&td, 1, 0).content, format!("again\n\n{}", note(&s.id, &[("report.pdf", "application/pdf", 40)])));
    td.shutdown().await;
}

// ---------------------------------------------------------------- session.delete

#[tokio::test]
async fn deleting_a_session_removes_only_its_own_attachment_directory() {
    let mut f = Fx::new().await;
    let a = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 10)).await;
    let b = upload(&f.td, &mut f.c, "b.pdf", "application/pdf", &blob(2, 10)).await;
    let s1 = create(&f.td, &mut f.c, Some("proj")).await;
    let s2 = create(&f.td, &mut f.c, Some("proj")).await;
    send(&f.td, &mut f.c, &s1.id, "m1", "x", &[&a]).await.unwrap();
    send(&f.td, &mut f.c, &s2.id, "m2", "y", &[&b]).await.unwrap();
    wait_status(&f.td, &mut f.c, &s1.id, SessionStatus::Idle).await;
    wait_status(&f.td, &mut f.c, &s2.id, SessionStatus::Idle).await;
    std::fs::write(f.repo.join(".anycon/keep.txt"), b"mine").unwrap();
    std::fs::write(f.repo.join(".anycon/attachments/loose.txt"), b"mine").unwrap();
    let _: Empty = f.call(methods::SESSION_DELETE, json!({ "session_id": s1.id })).await.unwrap();
    assert!(!attach_dir(&f.repo, &s1.id).exists());
    assert!(attach_dir(&f.repo, &s2.id).join("b.pdf").is_file(), "another session's files stay");
    assert!(f.repo.join(".anycon/keep.txt").is_file() && f.repo.join(".anycon/attachments/loose.txt").is_file());
    // 删除之后的文件内容也一起没了（blob 本身随会话删除，规则不变）
    assert!(f.td.ctx.store.blob_get(&a.blob_id).await.unwrap().is_none());
    f.td.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn deleting_a_session_does_not_follow_a_symbolic_link_in_the_chain() {
    let mut f = Fx::new().await;
    let a = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 10)).await;
    let s = create(&f.td, &mut f.c, Some("proj")).await;
    send(&f.td, &mut f.c, &s.id, "m1", "x", &[&a]).await.unwrap();
    wait_status(&f.td, &mut f.c, &s.id, SessionStatus::Idle).await;
    // agent（或别的什么）把 `attachments` 换成了指向别处的链接，别处恰好有同名的目录
    let outside = f.td.dir.path().join("outside");
    std::fs::create_dir_all(outside.join(&s.id)).unwrap();
    std::fs::write(outside.join(&s.id).join("precious.txt"), b"keep me").unwrap();
    let attachments = f.repo.join(".anycon/attachments");
    std::fs::rename(&attachments, f.repo.join(".anycon/attachments.bak")).unwrap();
    std::os::unix::fs::symlink(&outside, &attachments).unwrap();
    let _: Empty = f.call(methods::SESSION_DELETE, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(std::fs::read(outside.join(&s.id).join("precious.txt")).unwrap(), b"keep me");
    f.td.shutdown().await;
}

#[tokio::test]
async fn deleting_a_worktree_session_removes_its_files_even_when_the_worktree_is_kept() {
    let mut f = Fx::new().await;
    let a = upload(&f.td, &mut f.c, "a.pdf", "application/pdf", &blob(1, 10)).await;
    let s: SessionSummary = f
        .call(
            methods::SESSION_CREATE,
            json!({
                "agent": "fake", "project": "proj", "prompt": "go", "message_id": "m1",
                "attachments": [a], "workspace": { "mode": "worktree" }
            }),
        )
        .await
        .unwrap();
    f.wait_wt_state(&s.id, WorktreeState::Active).await;
    f.wait_status(&s.id, SessionStatus::Idle).await;
    let wt = f.dir_of(&s);
    assert!(attach_dir(&wt, &s.id).join("a.pdf").is_file());
    let _: Empty = f.call(methods::SESSION_DELETE, json!({ "session_id": s.id, "worktree": "keep" })).await.unwrap();
    assert!(wt.is_dir(), "the worktree is kept");
    assert!(!attach_dir(&wt, &s.id).exists(), "the session's attachment directory is removed");
    f.td.shutdown().await;
}

#[tokio::test]
async fn deleting_a_scratch_session_removes_its_directory_with_the_files() {
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let a = upload(&td, &mut c, "a.pdf", "application/pdf", &blob(1, 10)).await;
    let s = create(&td, &mut c, None).await;
    send(&td, &mut c, &s.id, "m1", "x", &[&a]).await.unwrap();
    wait_status(&td, &mut c, &s.id, SessionStatus::Idle).await;
    assert!(attach_dir(&scratch(&td, &s.id), &s.id).join("a.pdf").is_file());
    let _: Empty = c.call(&td, methods::SESSION_DELETE, json!({ "session_id": s.id })).await.unwrap();
    assert!(!scratch(&td, &s.id).exists());
    td.shutdown().await;
}
