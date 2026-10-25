//! README 快速开始的 happy path，全程走真实组件，只有浏览器用 `FakeWeb` 代替：
//!
//! ```text
//! 进程内 hub ← 真实 `anycon daemon` 子进程（mock agent）← 首次配对（带管理员 cookie 的 FakeWeb，给 daemon 一个账号）
//!            ← `anycon connect`（CLI 作为第二台设备，没有 cookie，被归到 daemon 的账号）
//!            ← `anycon nodes / agents / projects / new / approve / history / cancel` 全部经 hub + Noise
//! ```
//!
//! 账号规则（docs/spec/hub.md § 4）：全新 daemon 和无 cookie 的 CLI/SDK 配对 = 两边都没有账号 = `forbidden`，
//! 所以第一次配对由「带登录 cookie 的 web」来做；daemon 有了账号之后 CLI 才能 `connect`。
//!
//! 约 10 秒，确定性（只有 mock agent，不碰网络、不碰真实 `~/.anycon`）。

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anycon_crypto::Keypair;
use anycon_daemon::testutil::FakeWeb;
use common::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdout, Command};

const NODE_NAME: &str = "test-node"; // FakeWeb::pair 校验节点名

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

    /// 管理员登录，返回 `anycon_session=…` cookie。
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

/// 流式运行 `anycon <args>`（CLI 客户端，`cli_home`），stdout 逐行读。
struct Streaming {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Streaming {
    fn spawn(cli_home: &Path, args: &[&str]) -> Streaming {
        let mut c = Command::new(BIN);
        for (k, _) in std::env::vars() {
            if k.starts_with("ANYCON_") {
                c.env_remove(k);
            }
        }
        c.args(args)
            .env("ANYCON_HOME", cli_home)
            .env("NO_COLOR", "1")
            .env("ANYCON_LOG", "warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let mut child = c.spawn().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Streaming { child, lines }
    }

    async fn stderr(&mut self) -> String {
        use tokio::io::AsyncReadExt;
        let mut out = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            let _ = e.read_to_string(&mut out).await;
        }
        out
    }

    async fn next_json(&mut self) -> Option<Value> {
        let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
            .await
            .expect("timed out waiting for the next NDJSON line")
            .unwrap()?;
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON: {line:?}: {e}")))
    }
}

fn event_type(line: &Value) -> &str {
    line["event"]["type"].as_str().unwrap_or("")
}

/// 按顺序在 haystack 里找 needle（允许中间夹别的事件，比如 `status`）。
fn assert_subsequence(haystack: &[String], needle: &[&str]) {
    let mut at = 0;
    for h in haystack {
        if at < needle.len() && h == needle[at] {
            at += 1;
        }
    }
    assert_eq!(at, needle.len(), "expected {needle:?} in order, got {haystack:?}");
}

async fn eventually_json(cli_home: &Path, args: &[&str], what: &str, pred: impl Fn(&Run) -> bool) -> Run {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let r = anycon(cli_home, args, None, &[]).await;
        if pred(&r) {
            return r;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {}\n{}", r.stdout, r.stderr);
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

struct Stack {
    hub: TestHub,
    daemon: Option<DaemonProc>,
    daemon_home: PathBuf,
    cli_home: PathBuf,
    _dir: tempfile::TempDir,
}

/// hub + 真实 daemon（mock agent）+ 已配对的 CLI 客户端。
async fn paired_stack() -> Stack {
    let dir = tempfile::tempdir().unwrap();
    let daemon_home = dir.path().join("daemon-home");
    let cli_home = dir.path().join("cli-home");
    let hub = TestHub::start().await;
    let url = hub.url();

    // daemon：init → 不碰真实 claude → 前台起来（mock agent 由 ANYCON_MOCK_AGENT=1 打开）
    init(&daemon_home, &dir.path().join("workspace"), NODE_NAME, Some(&url)).await;
    let r = anycon(&daemon_home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let r = anycon(&daemon_home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let daemon = start_daemon(&daemon_home, &[("ANYCON_MOCK_AGENT", "1")]).await;

    // 第一次配对：带管理员 cookie 的 web，daemon 因此有了账号和 device token
    let cookie = hub.admin_cookie().await;
    let first = PairProc::begin(&daemon_home).await;
    let mut web = FakeWeb::connect(&url, None, Some(&cookie), "Setup Browser", Keypair::generate()).await;
    web.pair(&first.code).await;
    let done = first.finish().await;
    assert_eq!(done["paired"]["name"], "Setup Browser");

    // CLI 作为客户端：没有 cookie，被 hub 归到 daemon 的账号下
    let second = PairProc::begin(&daemon_home).await;
    let r = anycon(&cli_home, &["connect", &url, "--code", &second.code, "--name", "cli-test"], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stdout.contains(&format!("Paired with node {NODE_NAME}")), "{}", r.stdout);
    let done = second.finish().await;
    assert_eq!(done["paired"]["name"], "cli-test");
    assert_eq!(done["paired"]["kind"], "cli");

    Stack { hub, daemon: Some(daemon), daemon_home, cli_home, _dir: dir }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_drives_a_real_daemon_through_a_real_hub_end_to_end() {
    let stack = paired_stack().await;
    let url = stack.hub.url();
    let home = stack.cli_home.as_path();
    let daemon_home = stack.daemon_home.as_path();

    // nodes / agents
    let nodes = anycon(home, &["--hub", &url, "nodes", "--json"], None, &[]).await.json();
    assert_eq!(nodes["nodes"][0]["name"], NODE_NAME, "{nodes}");
    assert_eq!(nodes["nodes"][0]["status"], "online", "{nodes}");

    let agents = anycon(home, &["--node", NODE_NAME, "agents", "--json"], None, &[]).await;
    assert_eq!(agents.code, 0, "{}", agents.stderr);
    let agents = agents.json();
    let list = agents["agents"].as_array().unwrap();
    let mock = list.iter().find(|a| a["id"] == "mock").expect("mock agent listed");
    assert_eq!(mock["status"], "ready");
    assert_eq!(list.iter().find(|a| a["id"] == "claude-code").map(|a| a["status"].clone()), Some(json!("disabled")));

    // projects
    let made = anycon(home, &["projects", "create", "happy-proj", "--json"], None, &[]).await;
    assert_eq!(made.code, 0, "{}", made.stderr);

    // new：流式 NDJSON；权限请求从另一个进程（另一次 CLI 调用）答复
    let mut new = Streaming::spawn(
        home,
        &[
            "--hub",
            &url,
            "--node",
            NODE_NAME,
            "new",
            "--agent",
            "mock",
            "--project",
            "happy-proj",
            "--model",
            "mock-large",
            "--thinking",
            "low",
            "--mode",
            "ask",
            "--json",
            "run permission",
        ],
    );
    let mut streamed: Vec<Value> = Vec::new();
    let mut approved = false;
    let mut session_id = String::new();
    while let Some(line) = new.next_json().await {
        session_id = line["session_id"].as_str().expect("every line is a session event").to_string();
        if event_type(&line) == "permission" && !approved {
            let rid = line["event"]["request_id"].as_str().unwrap().to_string();
            assert_eq!(line["event"]["kind"], "tool");
            // 另一个进程答复：在 daemon 所在机器上的终端 `anycon approve`（本机 socket）。
            // 不能再起一个「同一台 CLI 设备」的远程进程：hub 对单设备只允许一条连接（4001），
            // 第二个进程会把流式的 `anycon new` 顶掉（它以退出码 3 结束）。
            let r = anycon(daemon_home, &["approve", &rid, "--json"], None, &[]).await;
            assert_eq!(r.code, 0, "{}\n{}", r.stdout, r.stderr);
            let j = r.json();
            assert_eq!(j["request_id"], rid.as_str());
            assert_eq!(j["option_id"], "allow");
            approved = true;
        }
        let done = event_type(&line) == "turn_done";
        streamed.push(line);
        if done {
            break;
        }
    }
    assert!(approved, "a permission request showed up in the stream");
    let status = tokio::time::timeout(Duration::from_secs(20), new.child.wait()).await.unwrap().unwrap();
    let stderr = new.stderr().await;
    assert_eq!(status.code(), Some(0), "anycon new exits 0 after a completed turn; stderr: {stderr}");

    let types: Vec<String> = streamed.iter().map(|l| event_type(l).to_string()).collect();
    assert_subsequence(
        &types,
        &[
            "user_message",
            "turn_start",
            "thinking_delta",
            "text_delta",
            "permission",
            "permission_resolved",
            "tool_call",
            "tool_result",
            "turn_done",
        ],
    );
    let text: String = streamed
        .iter()
        .filter(|l| event_type(l) == "text_delta")
        .map(|l| l["event"]["text"].as_str().unwrap())
        .collect();
    assert!(text.contains("[model=mock-large thinking=low mode=ask]"), "settings echo missing: {text}");
    let resolved = streamed.iter().find(|l| event_type(l) == "permission_resolved").unwrap();
    assert_eq!(resolved["event"]["option_id"], "allow");
    let done = streamed.last().unwrap();
    assert_eq!(done["event"]["reason"], "completed");
    assert!(done["event"]["usage"]["input_tokens"].as_u64().unwrap() > 0, "turn_done carries usage: {done}");
    assert!(done["event"]["usage"]["output_tokens"].as_u64().unwrap() > 0);

    // history：同一会话的完整事件，和刚才流里的逐条一致（按 seq 对账）
    let history = anycon(home, &["history", &session_id, "--json"], None, &[]).await;
    assert_eq!(history.code, 0, "{}", history.stderr);
    let hist = history.json_lines();
    assert_eq!(hist[0]["event"]["type"], "settings_changed", "the first event carries the effective settings");
    assert_eq!(hist[0]["event"]["settings"]["model"], "mock-large");
    assert_eq!(hist[0]["event"]["settings"]["thinking"], "low");
    assert_eq!(hist[0]["event"]["settings"]["mode"], "ask");
    for s in &streamed {
        let h = hist
            .iter()
            .find(|h| h["seq"] == s["seq"])
            .unwrap_or_else(|| panic!("seq {} ({}) missing from history", s["seq"], event_type(s)));
        assert_eq!(h["event"]["type"], s["event"]["type"]);
        if event_type(s) != "turn_done" {
            // 费用浮点数在落库后可能差最后几位，turn_done 单独比关键字段
            assert_eq!(h["event"], s["event"], "seq {}", s["seq"]);
        } else {
            assert_eq!(h["event"]["reason"], s["event"]["reason"]);
            assert_eq!(h["event"]["usage"]["input_tokens"], s["event"]["usage"]["input_tokens"]);
        }
    }
    // 人类可读的历史也能读
    let human = anycon(home, &["history", &session_id], None, &[]).await;
    assert!(human.stdout.contains("> run permission"), "{}", human.stdout);

    // 远程答复（顺序执行，同一时刻只有一个 CLI 进程连着 hub）：`new --no-wait` 建会话，`pending` 查请求，`approve` 答复
    let created = anycon(
        home,
        &["new", "--no-wait", "--json", "--agent", "mock", "--project", "happy-proj", "another permission"],
        None,
        &[],
    )
    .await;
    assert_eq!(created.code, 0, "{}", created.stderr);
    let second_id = created.json()["id"].as_str().unwrap().to_string();
    let pending = eventually_json(home, &["pending", "--json"], "the second permission request", |r| {
        r.code == 0 && !r.json()["requests"].as_array().unwrap().is_empty()
    })
    .await
    .json();
    let rid = pending["requests"][0]["request_id"].as_str().unwrap().to_string();
    assert_eq!(pending["requests"][0]["session_id"], second_id.as_str());
    let approved = anycon(home, &["approve", &rid, "--json"], None, &[]).await;
    assert_eq!(approved.code, 0, "{}", approved.stderr);
    let r = eventually_json(home, &["history", &second_id, "--json"], "the second turn to finish", |r| {
        r.code == 0 && r.json_lines().iter().any(|l| event_type(l) == "turn_done")
    })
    .await;
    let kinds: Vec<String> = r.json_lines().iter().map(|l| event_type(l).to_string()).collect();
    assert_subsequence(&kinds, &["permission", "permission_resolved", "tool_call", "tool_result", "turn_done"]);
    // 默认设置（没传 --model/--thinking/--mode）：mock 的默认值
    let echo: String = r
        .json_lines()
        .iter()
        .filter(|l| event_type(l) == "text_delta")
        .map(|l| l["event"]["text"].as_str().unwrap().to_string())
        .collect();
    assert!(echo.contains("[model=mock-large thinking=high mode=ask]"), "{echo}");

    // 取消：慢 turn 开始后 cancel，turn_done(cancelled)
    let created = anycon(
        home,
        &["new", "--no-wait", "--json", "--agent", "mock", "--project", "happy-proj", "--mode", "yolo", "slow please"],
        None,
        &[],
    )
    .await;
    assert_eq!(created.code, 0, "{}", created.stderr);
    let slow_id = created.json()["id"].as_str().unwrap().to_string();
    // 等第一段文本出现（turn 确实在跑）
    eventually_json(home, &["history", &slow_id, "--json"], "the slow turn to start streaming", |r| {
        r.code == 0 && r.json_lines().iter().any(|l| event_type(l) == "text_delta")
    })
    .await;
    let cancelled = anycon(home, &["cancel", &slow_id], None, &[]).await;
    assert_eq!(cancelled.code, 0, "{}", cancelled.stderr);
    let r = eventually_json(home, &["history", &slow_id, "--json"], "turn_done(cancelled)", |r| {
        r.code == 0 && r.json_lines().iter().any(|l| event_type(l) == "turn_done")
    })
    .await;
    let done = r.json_lines().into_iter().rev().find(|l| event_type(l) == "turn_done").unwrap();
    assert_eq!(done["event"]["reason"], "cancelled");

    // ls：三个会话，都在 happy-proj 下
    let ls = anycon(home, &["ls", "--json"], None, &[]).await.json();
    assert_eq!(ls["sessions"].as_array().unwrap().len(), 3, "{ls}");
    assert!(ls["sessions"].as_array().unwrap().iter().all(|s| s["project"] == "happy-proj"));

    // 清场：先停 daemon（SIGTERM，优雅），hub 随 Stack drop
    if let Some(d) = stack.daemon {
        d.stop().await;
    }
    stack.hub.handle.shutdown().await;
}
