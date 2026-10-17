//! 附件（ADR-35）在 CLI 里的样子：`anycon history` 把用户消息的附件列出来；`anycon send --image <path>` 在本地看过
//! 文件头和大小之后经 `blob.*` 上传、`session.send` 带上，和 Web / SDK 走同一套方法。这里验证真实的 daemon 子进程
//! （mock agent，本机 socket）加真实的 `anycon` 二进制。

mod common;

use anycon_daemon::testutil::{png_fixture, sha256_hex};
use base64::Engine as _;
use common::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// 一行一条 JSON-RPC 的最小 socket 客户端。
struct Sock {
    r: BufReader<tokio::net::unix::OwnedReadHalf>,
    w: tokio::net::unix::OwnedWriteHalf,
    next: u64,
}

impl Sock {
    async fn connect(home: &std::path::Path) -> Sock {
        let (r, w) = UnixStream::connect(home.join("daemon.sock")).await.unwrap().into_split();
        Sock { r: BufReader::new(r), w, next: 0 }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let line = json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params });
        self.w.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        loop {
            let mut l = String::new();
            self.r.read_line(&mut l).await.unwrap();
            assert!(!l.is_empty(), "socket closed");
            let v: Value = serde_json::from_str(l.trim()).unwrap();
            if v["id"] == json!(self.next) {
                assert!(v.get("error").is_none(), "{method} failed: {v}");
                return v["result"].clone();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_lists_the_attachments_of_a_user_message() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    for agent in ["claude-code", "codex"] {
        assert_eq!(
            anycon(&home, &["config", "set", &format!("agents.{agent}.enabled"), "false"], None, &[]).await.code,
            0
        );
    }
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;

    // 经 socket 上传一张图，再带着它创建会话
    let mut sock = Sock::connect(&home).await;
    let bytes = png_fixture(1280, 800, 4096, 7);
    let begin = sock
        .call(
            "blob.put_begin",
            json!({ "name": "shot.png", "mime": "image/png", "size": bytes.len(), "sha256": sha256_hex(&bytes) }),
        )
        .await;
    let id = begin["blob_id"].as_str().unwrap().to_string();
    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    sock.call("blob.put_chunk", json!({ "blob_id": id, "index": 0, "data": data })).await;
    let att = sock.call("blob.put_end", json!({ "blob_id": id })).await;
    assert_eq!((att["width"].clone(), att["height"].clone()), (json!(1280), json!(800)));
    let session = sock
        .call(
            "session.create",
            json!({ "agent": "mock", "prompt": "what is in the picture?", "message_id": "m1", "attachments": [att] }),
        )
        .await;
    let sid = session["id"].as_str().unwrap().to_string();
    let short = &sid[..12];
    // 等 mock 答完
    let t = std::time::Instant::now();
    loop {
        let r = anycon(&home, &["history", short, "--json"], None, &[]).await;
        if r.stdout.contains("turn_done") {
            break;
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "the mock never finished: {}", r.stdout);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // 人看的输出：文字，然后每个附件一行
    let human = anycon(&home, &["history", short], None, &[]).await;
    assert_eq!(human.code, 0, "{}", human.stderr);
    assert!(
        human.stdout.contains("> what is in the picture?\n  [attachment] shot.png (image/png, 4.0 KiB, 1280×800)\n"),
        "{}",
        human.stdout
    );
    // mock 回显的附件说明也在（证明 daemon 把文件交给了 agent）
    assert!(human.stdout.contains(&format!("shot.png (image/png, 4096 bytes, sha256 {})", &sha256_hex(&bytes)[..12])));
    // --json：原始事件里的 attachments 带 daemon 记录的元数据
    let json_lines = anycon(&home, &["history", short, "--json"], None, &[]).await.json_lines();
    let um = json_lines.iter().find(|e| e["event"]["type"] == "user_message").expect("a user_message event");
    assert_eq!(um["event"]["attachments"][0]["name"], "shot.png");
    assert_eq!(um["event"]["attachments"][0]["width"], 1280);
    daemon.stop().await;
}

/// `blobs/` 下的正式文件数（不含上传中的 `tmp/`）。
fn blob_files(home: &std::path::Path) -> usize {
    std::fs::read_dir(home.join("blobs"))
        .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count())
        .unwrap_or(0)
}

/// 等会话完成第 `n` 个 turn。
async fn wait_turns(home: &std::path::Path, short: &str, n: usize) -> Vec<Value> {
    let t = std::time::Instant::now();
    loop {
        let lines = anycon(home, &["history", short, "--json"], None, &[]).await.json_lines();
        if lines.iter().filter(|e| e["event"]["type"] == "turn_done").count() >= n {
            return lines;
        }
        assert!(t.elapsed() < std::time::Duration::from_secs(20), "turn {n} never finished: {lines:?}");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_image_uploads_over_the_socket_and_attaches_it() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    for agent in ["claude-code", "codex"] {
        assert_eq!(
            anycon(&home, &["config", "set", &format!("agents.{agent}.enabled"), "false"], None, &[]).await.code,
            0
        );
    }
    // 每张上限调到 1 MiB：700 KiB 的图能过（两块），1.6 MiB 的由 daemon 拒绝（CLI 本地只拦每条消息固定的 20 MiB）
    let daemon =
        start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1"), ("ANYCON_SESSIONS__MAX_ATTACHMENT_BYTES", "1MiB")]).await;
    let created = anycon(&home, &["new", "--agent", "mock", "--no-wait", "--json", "hello"], None, &[]).await;
    assert_eq!(created.code, 0, "{}", created.stderr);
    let sid = created.json()["id"].as_str().unwrap().to_string();
    let short = &sid[..12];
    wait_turns(&home, short, 1).await;

    let shot = d.path().join("shot.png");
    let bytes = png_fixture(1280, 800, 700 * 1024, 3);
    std::fs::write(&shot, &bytes).unwrap();
    let shot_s = shot.to_string_lossy().into_owned();

    // 文字 + 图，跟到 turn 结束：mock 回显的大小和 sha256 证明 daemon 拿到的就是这个文件（经过两块重组）
    let r = anycon(&home, &["send", short, "--image", &shot_s, "what is in the picture?"], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(
        r.stdout.contains(&format!(
            "shot.png (image/png, {} bytes, sha256 {})",
            bytes.len(),
            &sha256_hex(&bytes)[..12]
        )),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("> what is in the picture?\n  [attachment] shot.png (image/png, 700.0 KiB, 1280×800)"),
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("warning"), "a small image gets no advice: {}", r.stderr);
    assert_eq!(blob_files(&home), 1);

    // 只有图、没有文字，--no-wait
    let r = anycon(&home, &["send", short, "--image", &shot_s, "--no-wait"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(matches!(r.stdout.trim(), "sent" | "queued"), "{}", r.stdout);
    let lines = wait_turns(&home, short, 3).await;
    let with_images: Vec<&Value> =
        lines.iter().filter(|e| e["event"]["type"] == "user_message" && e["event"]["attachments"].is_array()).collect();
    assert_eq!(with_images.len(), 2, "{lines:?}");
    let last = &with_images[1]["event"];
    assert_eq!(last["content"], "");
    assert_eq!(last["attachments"][0]["name"], "shot.png");
    assert_eq!(last["attachments"][0]["bytes"], json!(bytes.len()));
    assert_eq!(
        (last["attachments"][0]["width"].clone(), last["attachments"][0]["height"].clone()),
        (json!(1280), json!(800))
    );
    assert_ne!(
        last["attachments"][0]["blob_id"], with_images[0]["event"]["attachments"][0]["blob_id"],
        "each send uploads its own copy"
    );
    assert_eq!(blob_files(&home), 2);

    // 本地就拒的：文件头不是图片（扩展名不算）、空文件、不存在；什么都没上传
    let fake = d.path().join("notes.png");
    std::fs::write(&fake, b"just some text").unwrap();
    let r = anycon(&home, &["send", short, "--image", &fake.to_string_lossy(), "x"], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("not a PNG, JPEG, GIF or WebP image"), "{}", r.stderr);
    let empty = d.path().join("empty.png");
    std::fs::write(&empty, b"").unwrap();
    let r = anycon(&home, &["send", short, "--image", &empty.to_string_lossy(), "x"], None, &[]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("is empty"), "{}", r.stderr);
    let r =
        anycon(&home, &["send", short, "--image", &d.path().join("nope.png").to_string_lossy(), "x"], None, &[]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("cannot read"), "{}", r.stderr);
    // 既没有文字也没有图：参数错误
    assert_eq!(anycon(&home, &["send", short], None, &[]).await.code, 2);
    assert_eq!(blob_files(&home), 2);

    // daemon 拒的：超过节点的每张上限 → invalid / too_large（退出码 2），错误里说是哪张图；大图先有提醒
    let big = d.path().join("big.png");
    std::fs::write(&big, png_fixture(4032, 3024, 1600 * 1024, 5)).unwrap();
    let r =
        anycon(&home, &["send", short, "--image", &shot_s, "--image", &big.to_string_lossy(), "two"], None, &[]).await;
    assert_eq!(r.code, 2, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stderr.contains("too_large") && r.stderr.contains("big.png"), "{}", r.stderr);
    assert!(r.stderr.contains("4032×3024") && r.stderr.contains("does not resize"), "{}", r.stderr);
    // 第一张已经传完、没被引用：随失败一起丢掉，不留到 24 小时后
    assert_eq!(blob_files(&home), 2);
    assert_eq!(wait_turns(&home, short, 3).await.iter().filter(|e| e["event"]["type"] == "user_message").count(), 3);

    // 已归档的会话：在上传之前就拒（退出码 2，提示 unarchive）
    assert_eq!(anycon(&home, &["archive", short], None, &[]).await.code, 0);
    let r = anycon(&home, &["send", short, "--image", &shot_s, "late"], None, &[]).await;
    assert_eq!(r.code, 2);
    assert!(r.stderr.contains("anycon unarchive"), "{}", r.stderr);
    assert_eq!(blob_files(&home), 2);
    daemon.stop().await;
}
