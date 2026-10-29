//! 账号内接入（ADR-36）的本机命令：`anycon passwd` 的各个分支、`anycon pair --enroll`、`anycon connect` 的参数与退出码、
//! doctor 里的节点口令一项。真 daemon 子进程 + 本机 socket；`pair --enroll` 等待完成那条用假 daemon socket（接入要经 hub，
//! 这里没有）。经真实 hub + daemon 的端到端见 `crates/cli/tests/enroll_real_hub.rs` 和 `e2e/tests/enroll.spec.ts`。

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PW: &str = "correct horse battery";

/// 起一个只有 mock 也不探测真 agent 的 daemon。
async fn node(d: &std::path::Path) -> (std::path::PathBuf, DaemonProc) {
    let home = d.join("home");
    init(&home, &d.join("ws"), "box", None).await;
    for a in ["claude-code", "codex"] {
        let key = format!("agents.{a}.enabled");
        assert_eq!(anycon(&home, &["config", "set", &key, "false"], None, &[]).await.code, 0);
    }
    let daemon = start_daemon(&home, &[]).await;
    (home, daemon)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn passwd_sets_changes_disables_enables_and_clears_through_the_local_socket() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    // 口令不接受命令行参数；不是终端又没有 --stdin：参数错误（2），和 daemon 在不在无关（这里它还没起来）
    let r = anycon(&home, &["passwd"], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("--stdin"), "{}", r.stderr);
    for bad in [&["passwd", "hunter2hunter2"][..], &["passwd", "--password", "x"], &["passwd", "--clear", "--enable"]] {
        assert_eq!(anycon(&home, bad, None, &[]).await.code, 2, "{bad:?}");
    }
    let (home, daemon) = node(d.path()).await;
    // 没设：状态
    let st = anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json();
    assert_eq!(st["approval"], json!(true));
    assert_eq!(st["password"]["set"], json!(false));
    assert_eq!(st["password"]["state"], json!("enabled"));
    // 太短（本地先拦，不发给 daemon）；空的 stdin
    let r = anycon(&home, &["passwd", "--stdin"], Some("short\n"), &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("at least 8"), "{}", r.stderr);
    assert!(!anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json()["password"]["set"]
        .as_bool()
        .unwrap());
    // 设置（stdin，行尾 \r\n 去掉）
    let r = anycon(&home, &["passwd", "--stdin"], Some(&format!("{PW}\r\n")), &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("node password set"), "{}", r.stdout);
    assert!(!r.stdout.contains(PW) && !r.stderr.contains(PW));
    let st = anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json();
    assert_eq!(st["password"]["set"], json!(true));
    assert_eq!(st["password"]["set_by"], json!("local"), "the local socket counts as `local`");
    assert_eq!(st["password"]["failures"], json!(0));
    let human = anycon(&home, &["passwd", "--status"], None, &[]).await;
    assert!(human.stdout.contains("set just now, by local") && human.stdout.contains("enabled"), "{}", human.stdout);
    // 停用 / 启用
    assert_eq!(anycon(&home, &["passwd", "--disable"], None, &[]).await.code, 0);
    let st = anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json();
    assert_eq!(st["password"]["state"], json!("disabled"));
    assert_eq!(st["password"]["set"], json!(true), "the password itself is kept");
    let r = anycon(&home, &["passwd", "--enable"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json()["password"]["state"],
        json!("enabled")
    );
    // 清除：脚本里必须带 --yes（不然要交互确认）
    let r = anycon(&home, &["passwd", "--clear"], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json()["password"]["set"]
        .as_bool()
        .unwrap());
    let r = anycon(&home, &["passwd", "--clear", "--yes", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json(), json!({ "password": "cleared" }));
    assert!(!anycon(&home, &["passwd", "--status", "--json"], None, &[]).await.json()["password"]["set"]
        .as_bool()
        .unwrap());
    // 改动进了审计，而且不含口令
    let audit = anycon(&home, &["audit", "--json"], None, &[]).await;
    let text = audit.stdout.clone();
    assert!(
        text.contains("enroll.password_set")
            && text.contains("enroll.password_cleared")
            && text.contains("enroll.settings"),
        "{text}"
    );
    assert!(!text.contains(PW));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn doctor_lists_the_node_password_line_and_it_is_optional() {
    let d = tempfile::tempdir().unwrap();
    let (home, daemon) = node(d.path()).await;
    let doctor = anycon(&home, &["doctor", "--json"], None, &[("ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL", "false")]).await;
    let j = doctor.json();
    let line =
        j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "enroll_password").expect("a node password line");
    assert_eq!(line["ok"], json!(true), "an unset password is a hint, not a failure: {line}");
    assert!(line["detail"].as_str().unwrap().contains("anycon passwd"), "{line}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_enroll_prints_a_join_code_and_rejects_the_short_code_options() {
    let d = tempfile::tempdir().unwrap();
    let (home, daemon) = node(d.path()).await;
    // 机器可读：第一行（也是唯一一行）是 {"code","expires_at_ms"}
    let r = anycon(&home, &["pair", "--enroll", "--no-wait", "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let j = r.json();
    let code = j["code"].as_str().unwrap();
    assert!(code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()), "{j}");
    let ttl = j["expires_at_ms"].as_u64().unwrap().saturating_sub(now_ms());
    assert!(ttl > 290_000 && ttl <= 300_000, "5 minutes: {ttl}");
    assert!(
        r.stderr.contains("not connected to the hub"),
        "a code that cannot be used yet comes with a warning: {}",
        r.stderr
    );
    // 人类可读：123-456
    let r = anycon(&home, &["pair", "--enroll", "--no-wait"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let line = r.stdout.lines().find(|l| l.starts_with("Join code: ")).expect("Join code line");
    let shown = line.trim_start_matches("Join code: ");
    assert!(shown.len() == 7 && shown.as_bytes()[3] == b'-', "{line}");
    assert!(r.stdout.contains("5 minutes") && r.stdout.contains("--via code"), "{}", r.stdout);
    // 新的顶掉旧的：数字不同（几乎一定；一百万分之一撞车就再来一次）
    // 不能和短码的选项同用：参数错误
    for bad in ["--short", "--long", "--qr"] {
        let r = anycon(&home, &["pair", "--enroll", bad], None, &[]).await;
        assert_eq!(r.code, 2, "{bad}: {}", r.stderr);
    }
    // 关掉之后不能再生成接入码：5，提示怎么打开
    assert_eq!(anycon(&home, &["config", "set", "enroll.approval", "false"], None, &[]).await.code, 0);
    tokio::time::sleep(Duration::from_millis(1200)).await; // 热加载（文件监视 500 ms 一次）
    let r = anycon(&home, &["pair", "--enroll", "--no-wait"], None, &[]).await;
    assert_eq!(r.code, 5, "{}", r.stderr);
    assert!(r.stderr.contains("enroll.approval"), "{}", r.stderr);
    daemon.stop().await;
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

/// 假 daemon：本机 socket 上的 JSON-RPC。`node.enroll_code` 答完之后推 `node.security_event`（先一个别的方式、再一个 code）。
async fn fake_daemon(home: &std::path::Path, calls: Arc<Mutex<Vec<(String, Value)>>>) -> tokio::task::JoinHandle<()> {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("config.toml"), "").unwrap();
    let mut l = anycon_daemon::local::bind(&anycon_daemon::config::Paths::new(home).sock()).unwrap();
    tokio::spawn(async move {
        let s = l.accept().await.unwrap();
        let (r, mut w) = tokio::io::split(s);
        let mut lines = BufReader::new(r).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let v: Value = serde_json::from_str(&line).unwrap();
            let (id, method, params) =
                (v["id"].clone(), v["method"].as_str().unwrap().to_string(), v["params"].clone());
            calls.lock().unwrap().push((method.clone(), params));
            let result = match method.as_str() {
                "local.status" => json!({ "hub": { "connected": true, "url": "http://hub.test" } }),
                "node.enroll_code" => json!({ "code": "123456", "expires_at_ms": now_ms() + 300_000 }),
                "node.devices" => json!({ "devices": [{
                    "device_id": "dev1", "name": "Pixel", "kind": "cli", "fingerprint": "ab", "paired_at_ms": 1, "online": true
                }] }),
                _ => json!({}),
            };
            let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
            w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
            if method == "node.enroll_code" {
                tokio::time::sleep(Duration::from_millis(200)).await;
                for ev in [
                    json!({ "kind": "device_enrolled", "ts_ms": now_ms(), "device_id": "other", "device_name": "Elsewhere", "method": "password" }),
                    json!({ "kind": "password_failures", "ts_ms": now_ms(), "failures": 3 }),
                    json!({ "kind": "device_enrolled", "ts_ms": now_ms(), "device_id": "dev1", "device_name": "Pixel", "method": "code" }),
                ] {
                    let n = json!({ "jsonrpc": "2.0", "method": "node.security_event", "params": ev });
                    w.write_all(format!("{n}\n").as_bytes()).await.unwrap();
                }
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_enroll_waits_for_a_device_that_used_this_code_and_can_rename_it() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let srv = fake_daemon(&home, calls.clone()).await;
    let r = anycon(&home, &["pair", "--enroll", "--json", "--name", "kitchen tablet"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines = r.json_lines();
    assert_eq!(lines.len(), 2, "{:?}", lines);
    assert_eq!(lines[0]["code"], json!("123456"));
    // 完成：只认用这个码（method = code）接入的设备，别的方式的接入不算
    assert_eq!(lines[1]["enrolled"]["device_id"], json!("dev1"));
    assert_eq!(lines[1]["enrolled"]["name"], json!("kitchen tablet"));
    assert_eq!(lines[1]["enrolled"]["kind"], json!("cli"));
    let calls = calls.lock().unwrap().clone();
    let rename = calls.iter().find(|(m, _)| m == "local.rename_device").expect("--name renames the new device");
    assert_eq!(rename.1, json!({ "device": "dev1", "name": "kitchen tablet" }));
    srv.abort();
    // 人类可读的输出
    let d2 = tempfile::tempdir().unwrap();
    let home2 = d2.path().join("home");
    let srv = fake_daemon(&home2, Arc::new(Mutex::new(Vec::new()))).await;
    let r = anycon(&home2, &["pair", "--enroll"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("Join code: 123-456") && r.stdout.contains("http://hub.test"), "{}", r.stdout);
    assert!(r.stdout.contains("Joined: Pixel"), "{}", r.stdout);
    srv.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_without_a_code_needs_a_terminal_and_does_not_mix_with_the_short_code_options() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    // 账号内接入要有人在终端前（登录码、批准码都得人看）：不是终端，退出码 2，提示改用短码
    let r = anycon(&home, &["connect", "http://127.0.0.1:1", "--via", "password"], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(r.stderr.contains("--code"), "{}", r.stderr);
    // 短码配对和账号内接入的选项互斥
    for bad in [
        &["connect", "http://127.0.0.1:1", "--code", "AAAA-BBBB", "--via", "scan"][..],
        &["connect", "http://127.0.0.1:1", "--code", "AAAA-BBBB", "--email", "a@b.c"],
        &["connect", "http://127.0.0.1:1", "--via", "telepathy"],
    ] {
        assert_eq!(anycon(&home, bad, None, &[]).await.code, 2, "{bad:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn init_mentions_the_node_password() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    let ws = d.path().join("ws");
    let r = anycon(&home, &["init", "--workspace", ws.to_str().unwrap(), "--yes"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("anycon passwd"), "init ends with a one-line hint about the node password: {}", r.stdout);
    // 机器可读的输出不变
    let d2 = tempfile::tempdir().unwrap();
    let r = anycon(
        &d2.path().join("home"),
        &["init", "--workspace", d2.path().join("ws").to_str().unwrap(), "--yes", "--json"],
        None,
        &[],
    )
    .await;
    assert!(!r.stdout.contains("passwd"), "{}", r.stdout);
    r.json();
}
