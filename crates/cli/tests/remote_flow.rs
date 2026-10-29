//! `anycon` 作为客户端设备经真实 hub（进程内）连 daemon（进程内，假 adapter）：connect / nodes / 远程命令 / 吊销 / 节点离线。

mod common;

use std::time::Duration;

use anycon_crypto::Keypair;
use anycon_daemon::testutil::{FakeWeb, TestDaemon};
use anycon_protocol::*;
use common::*;
use serde_json::json;

struct TestHub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
}

impl TestHub {
    async fn start() -> TestHub {
        let dir = tempfile::tempdir().unwrap();
        let mut c = anycon_hub::Config::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        let handle = anycon_hub::run(c).await.expect("hub starts");
        TestHub { handle, _dir: dir }
    }
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.handle.addr.port())
    }
    async fn admin_cookie(&self) -> String {
        let res = reqwest::Client::new()
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": self.handle.generated_admin_token.clone().unwrap() }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_pairs_with_a_node_and_drives_it_through_the_hub() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async { td.ctx.hub_state.get().connected })
            .await
    );
    // 第一次配对必须来自有账号的 web（daemon 和 CLI 都还没有账号归属）
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Setup Browser", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async {
            td.ctx.identity.device_token().is_some()
        })
        .await
    );

    // CLI 机器：没有 init 也能作为客户端
    let d = tempfile::tempdir().unwrap();
    let chome = d.path().join("cli-home");
    let url = hub.url();
    let bad = anycon(&chome, &["connect", &url, "--code", "ZZZZ-ZZZZ"], None, &[]).await;
    assert_eq!(bad.code, 4, "{}", bad.stderr);
    let malformed = anycon(&chome, &["connect", &url, "--code", "1"], None, &[]).await;
    assert_eq!(malformed.code, 2, "{}", malformed.stderr);

    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let r = anycon(&chome, &["connect", &url, "--code", &begin.code, "--name", "cli-box"], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stdout.contains("Paired with node test-node"), "{}", r.stdout);
    // 本地状态：0600 的密钥和 token
    let client_dir = std::fs::read_dir(chome.join("client")).unwrap().next().unwrap().unwrap().path();
    for f in ["client.key", "device-token"] {
        anycon_daemon::testutil::assert_mode(&client_dir.join(f), 0o600);
    }
    // daemon 侧：设备名和类型来自第 3 条握手消息
    let devices: NodeDevicesResult = local.call(&td, methods::NODE_DEVICES, json!({})).await.unwrap();
    let cli_dev = devices.devices.iter().find(|d| d.name == "cli-box").expect("cli device registered by name");
    assert_eq!(cli_dev.kind, DeviceKind::Cli);

    // nodes
    let n = anycon(&chome, &["nodes", "--json"], None, &[]).await.json();
    assert_eq!(n["nodes"][0]["name"], "test-node");
    assert_eq!(n["nodes"][0]["status"], "online");
    assert!(anycon(&chome, &["nodes"], None, &[]).await.stdout.contains("test-node"));

    // 远程命令（Noise E2E）：agents / projects / new（含权限交互）/ ls / history / pending
    let ag = anycon(&chome, &["agents", "--json"], None, &[]).await;
    assert_eq!(ag.code, 0, "{}", ag.stderr);
    assert_eq!(ag.json()["agents"][0]["id"], "fake");
    assert_eq!(anycon(&chome, &["projects", "create", "remote-proj"], None, &[]).await.code, 0);
    assert!(td.workspace.join("remote-proj").is_dir());
    let r = anycon(
        &chome,
        &["--node", "test-node", "new", "--project", "remote-proj", "run a permission check"],
        Some("a\n"),
        &[],
    )
    .await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stdout.contains("> run a permission check"), "{}", r.stdout);
    assert!(r.stdout.contains("▸ bash: echo hi"), "{}", r.stdout);
    assert!(r.stdout.contains("── completed"), "{}", r.stdout);
    let ls = anycon(&chome, &["ls", "--json"], None, &[]).await.json();
    assert_eq!(ls["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(ls["sessions"][0]["project"], "remote-proj");
    let sid = ls["sessions"][0]["id"].as_str().unwrap().to_string();
    let h = anycon(&chome, &["history", "last"], None, &[]).await;
    assert!(h.stdout.contains("> run a permission check"), "{}", h.stdout);
    // 审计里记的是这台 CLI 设备
    let audit: NodeAuditResult = local.call(&td, methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    assert!(audit
        .entries
        .iter()
        .any(|e| e.action == "session.create" && e.device_name == "cli-box" && e.device_id == cli_dev.device_id));
    // 另一个远程进程回答权限：先让会话停在权限请求上
    let created = anycon(&chome, &["new", "--no-wait", "--json", "--agent", "fake", "another permission"], None, &[])
        .await
        .json();
    let sid2 = created["id"].as_str().unwrap().to_string();
    let t = std::time::Instant::now();
    let rid = loop {
        let p = anycon(&chome, &["pending", "--json"], None, &[]).await.json();
        if let Some(r) = p["requests"].as_array().unwrap().first() {
            break r["request_id"].as_str().unwrap().to_string();
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let r = anycon(&chome, &["approve", &rid], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let _ = (sid, sid2);

    // --node 不认识 -> 4
    let r = anycon(&chome, &["--node", "ghost", "agents"], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    // 吊销：daemon 删公钥并通知 hub，之后 CLI 连不上（4）
    let revoked = td.ctx.peers.resolve("cli-box").unwrap();
    let _: Empty = local.call(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": revoked })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = anycon(&chome, &["agents"], None, &[]).await;
    assert_eq!(r.code, 4, "a revoked device is told to re-pair: {}", r.stderr);
    // disconnect 删本地配对
    assert_eq!(anycon(&chome, &["disconnect", "test-node"], None, &[]).await.code, 0);
    assert_eq!(anycon(&chome, &["disconnect", "test-node"], None, &[]).await.code, 4);
    assert!(anycon(&chome, &["nodes"], None, &[]).await.stdout.contains("no paired nodes"));
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offline_node_gives_exit_code_3_and_local_daemon_is_preferred() {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async { td.ctx.hub_state.get().connected })
            .await
    );
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Browser", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async {
            td.ctx.identity.device_token().is_some()
        })
        .await
    );
    let d = tempfile::tempdir().unwrap();
    let chome = d.path().join("cli-home");
    let url = hub.url();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_eq!(anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await.code, 0);
    assert_eq!(anycon(&chome, &["agents", "--json"], None, &[]).await.code, 0);
    // 把 CLI 的 home 指到 daemon 的 home 之外时走远程；daemon 下线后：3
    td.shutdown().await;
    let r = anycon(&chome, &["agents"], None, &[]).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(r.stderr.contains("error: offline:"), "{}", r.stderr);
    let n = anycon(&chome, &["nodes", "--json"], None, &[]).await.json();
    assert_eq!(n["nodes"][0]["status"], "offline");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_json_prints_the_code_first_then_blocks_until_a_device_joins() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async { td.ctx.hub_state.get().connected })
            .await
    );
    let cookie = hub.admin_cookie().await;

    let mut c = tokio::process::Command::new(BIN);
    c.args(["pair", "--json"])
        .env("ANYCON_HOME", &td.paths.home)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = c.spawn().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = tokio::time::timeout(Duration::from_secs(10), out.next_line()).await.unwrap().unwrap().unwrap();
    let j: serde_json::Value = serde_json::from_str(&first).unwrap();
    let code = j["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 19, "`anycon pair` defaults to a 16-character code: {first}");
    assert!(j["link"].as_str().unwrap().contains(&code));
    assert!(j["expires_at_ms"].as_u64().unwrap() > anycon_daemon::util::now_ms());
    // 还没人加入：进程在阻塞
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), child.wait()).await.is_err(),
        "pair must block until a device joins"
    );
    // 新设备加入
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Joined Browser", Keypair::generate()).await;
    web.pair(&code).await;
    let last = tokio::time::timeout(Duration::from_secs(10), out.next_line()).await.unwrap().unwrap().unwrap();
    let j: serde_json::Value = serde_json::from_str(&last).unwrap();
    assert_eq!(j["paired"]["name"], "Joined Browser", "{last}");
    assert_eq!(j["paired"]["kind"], "web");
    assert_eq!(j["paired"]["device_id"], web.device_id.as_str());
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(0));
    td.shutdown().await;
}

/// 起 hub + 带 hub 的 daemon，并用 web（带管理员 cookie）完成第一次配对，返回 (hub, daemon, 本机客户端)。
async fn hub_with_a_paired_daemon() -> (TestHub, TestDaemon, anycon_daemon::testutil::TestClient) {
    let hub = TestHub::start().await;
    let td = TestDaemon::start_full(&[], true, Some(&hub.url())).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async { td.ctx.hub_state.get().connected })
            .await
    );
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("setup");
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), "Setup Browser", Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(
        anycon_daemon::testutil::eventually(Duration::from_secs(10), || async {
            td.ctx.identity.device_token().is_some()
        })
        .await
    );
    (hub, td, local)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connect_accepts_pairing_links_and_long_codes() {
    let (hub, td, mut local) = hub_with_a_paired_daemon().await;
    let d = tempfile::tempdir().unwrap();
    let url = hub.url();
    // 链接形式（`anycon pair` 打印的 `…/pair#code=…`）
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_eq!(begin.link, format!("{url}/pair#code={}", begin.code));
    let home1 = d.path().join("one");
    let r = anycon(&home1, &["connect", &url, "--code", &begin.link, "--name", "via-link"], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stdout.contains("Paired with node test-node"), "{}", r.stdout);
    assert_eq!(anycon(&home1, &["agents", "--json"], None, &[]).await.code, 0);
    // 16 位长码
    let long: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({ "long": true })).await.unwrap();
    assert_eq!(long.code.len(), 19);
    let home2 = d.path().join("two");
    let r = anycon(&home2, &["connect", &url, "--code", &long.code], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    // 垃圾 / 过期的码：4；不是码的东西：2
    assert_eq!(anycon(&d.path().join("three"), &["connect", &url, "--code", "ZZZZ-ZZZZ"], None, &[]).await.code, 4);
    assert_eq!(
        anycon(&d.path().join("three"), &["connect", &url, "--code", &format!("{url}/pair")], None, &[]).await.code,
        2
    );
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_cli_can_pair_again_and_a_key_mismatch_is_never_overwritten() {
    let (hub, td, mut local) = hub_with_a_paired_daemon().await;
    let d = tempfile::tempdir().unwrap();
    let chome = d.path().join("cli-home");
    let url = hub.url();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_eq!(anycon(&chome, &["connect", &url, "--code", &begin.code, "--name", "cli-box"], None, &[]).await.code, 0);
    assert_eq!(anycon(&chome, &["agents", "--json"], None, &[]).await.code, 0);
    let client_dir = std::fs::read_dir(chome.join("client")).unwrap().next().unwrap().unwrap().path();
    let old_token = std::fs::read_to_string(client_dir.join("device-token")).unwrap();

    // 吊销：CLI 被告知重新配对（4），而且重新 `connect` 能成功（以前会陷入「请先 connect」的死循环）
    let revoked = td.ctx.peers.resolve("cli-box").unwrap();
    let _: Empty = local.call(&td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": revoked })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = anycon(&chome, &["agents"], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("anycon connect"), "{}", r.stderr);
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let r = anycon(&chome, &["connect", &url, "--code", &begin.code, "--name", "cli-box"], None, &[]).await;
    assert_eq!(r.code, 0, "re-pairing after a revoke works: {}\n{}", r.stdout, r.stderr);
    assert_ne!(std::fs::read_to_string(client_dir.join("device-token")).unwrap(), old_token, "a fresh identity");
    assert_eq!(anycon(&chome, &["agents", "--json"], None, &[]).await.code, 0);

    // 本地保存的节点公钥和这次配对得到的不同：拒绝覆盖，提示先 disconnect
    let nodes_path = client_dir.join("nodes.json");
    let mut nodes: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&nodes_path).unwrap()).unwrap();
    nodes["nodes"][0]["pubkey"] = json!("11".repeat(32));
    std::fs::write(&nodes_path, serde_json::to_string_pretty(&nodes).unwrap()).unwrap();
    let before = std::fs::read_to_string(&nodes_path).unwrap();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let r = anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("key_mismatch") && r.stderr.contains("disconnect"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(&nodes_path).unwrap(), before, "the pinned key was not replaced");
    // disconnect 之后可以重新配对
    assert_eq!(anycon(&chome, &["disconnect", "test-node"], None, &[]).await.code, 0);
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    assert_eq!(anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await.code, 0);

    // 损坏的状态文件不会被当作「没有」：connect 拒绝并保持原样
    std::fs::write(&nodes_path, "{ corrupt").unwrap();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let key_before = std::fs::read_to_string(client_dir.join("client.key")).unwrap();
    let r = anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("nodes.json"), "{}", r.stderr);
    assert_eq!(std::fs::read_to_string(client_dir.join("client.key")).unwrap(), key_before, "the key was not replaced");
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pair_defaults_to_a_long_code_and_short_is_opt_in() {
    let (_hub, td, _local) = hub_with_a_paired_daemon().await;
    let home = td.paths.home.clone();
    // 默认 16 位；`--long` 是无操作；链接、`--json` 跟随当前长度
    for args in [&["pair", "--json", "--no-wait"][..], &["pair", "--json", "--no-wait", "--long"][..]] {
        let r = anycon(&home, args, None, &[]).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        let j = r.json();
        assert_eq!(j["code"].as_str().unwrap().len(), 19, "{args:?}: {j}");
        assert!(j["link"].as_str().unwrap().ends_with(j["code"].as_str().unwrap()));
        assert!(!r.stderr.contains("8-character"), "{}", r.stderr);
    }
    // `--short`：8 位；本机 / 私网的 hub 不警告
    let r = anycon(&home, &["pair", "--json", "--no-wait", "--short"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["code"].as_str().unwrap().len(), 9);
    assert!(!r.stderr.contains("only suitable"), "no warning for a local hub: {}", r.stderr);
    // 二者互斥
    let r = anycon(&home, &["pair", "--short", "--long"], None, &[]).await;
    assert_eq!(r.code, 2, "{}", r.stderr);
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cli_reads_a_multi_mib_history_through_the_hub() {
    let (hub, td, mut local) = hub_with_a_paired_daemon().await;
    let d = tempfile::tempdir().unwrap();
    let chome = d.path().join("cli-home");
    let url = hub.url();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({ "long": true })).await.unwrap();
    assert_eq!(anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await.code, 0);
    // 40 个回合各 150 KB 的 text_delta -> 约 6 MB 的历史，分成许多页（每页 <= 192 KiB）经 hub 发给 CLI
    let s: SessionSummary = local.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    for i in 0..40 {
        let _: SessionSendResult = local
            .call(
                &td,
                methods::SESSION_SEND,
                json!({ "session_id": s.id, "message_id": format!("m{i}"), "content": "bigtext" }),
            )
            .await
            .unwrap();
        assert!(
            anycon_daemon::testutil::eventually(Duration::from_secs(10), || async {
                td.ctx.sessions.get_summary(&s.id).await.unwrap().status == SessionStatus::Idle
            })
            .await
        );
    }
    let r = anycon(&chome, &["history", &s.id, "--json"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let big = r
        .json_lines()
        .iter()
        .filter(|e| {
            e["event"]["type"] == "text_delta" && e["event"]["text"].as_str().is_some_and(|t| t.len() == 150_000)
        })
        .count();
    assert_eq!(big, 40, "every big event arrived intact, in order");
    td.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_approval_prompt_escapes_control_characters_clips_only_past_the_threshold_and_v_shows_everything() {
    let (hub, td, mut local) = hub_with_a_paired_daemon().await;
    let d = tempfile::tempdir().unwrap();
    let chome = d.path().join("cli-home");
    let url = hub.url();
    let begin: NodePairBeginResult = local.call(&td, methods::NODE_PAIR_BEGIN, json!({ "long": true })).await.unwrap();
    assert_eq!(anycon(&chome, &["connect", &url, "--code", &begin.code], None, &[]).await.code, 0);
    // 被注入的命令：`\r` / ESC[2K 改写显示，危险的尾巴在 2000 个字符之后。先按 v 看全文，再按 d 拒绝（拒绝会追问一句话，回车跳过）
    let r = anycon(&chome, &["new", "--agent", "fake", "evilcommand permission"], Some("v\nd\n\n"), &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(
        !r.stderr.contains('\x1b') && !r.stderr.contains('\r'),
        "no raw control characters on the terminal: {:?}",
        r.stderr
    );
    assert!(
        r.stderr.contains("echo harmless\\r"),
        "the carriage return is shown as an escape: {}",
        &r.stderr[..300.min(r.stderr.len())]
    );
    assert!(r.stderr.contains("press v to view all"), "{}", &r.stderr[..300.min(r.stderr.len())]);
    // 截断的提示里没有尾巴；按 v 之后的全文里有，且转义序列显示成 \x1b[2K
    let first_clip = r.stderr.find("press v to view all").unwrap();
    assert!(!r.stderr[..first_clip].contains("TAILMARK-DO-NOT-HIDE"), "the dangerous tail is not in the clipped view");
    assert!(r.stderr[first_clip..].contains("\\x1b[2K; echo TAILMARK-DO-NOT-HIDE"), "v prints the whole command");
    assert!(r.stdout.contains("skipped"), "the request was denied: {}", r.stdout);
    td.shutdown().await;
}
