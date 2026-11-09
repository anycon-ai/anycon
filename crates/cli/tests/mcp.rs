//! `anycon mcp` 对真实 daemon（mock agent）的端到端：用 stdio 说 MCP，覆盖工具、安全边界和审计身份。

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// 一个 `anycon mcp` 子进程，测试扮演 MCP 客户端。
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: tokio::io::Lines<BufReader<ChildStdout>>,
    next: u64,
}

impl Mcp {
    async fn start(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Mcp {
        let mut c = base_command(home);
        c.arg("mcp").args(args).envs(env.iter().copied());
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = c.spawn().expect("spawn anycon mcp");
        let stdin = child.stdin.take();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut m = Mcp { child, stdin, lines, next: 0 };
        let init = m
            .request(
                "initialize",
                json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }),
            )
            .await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18", "{init}");
        m.notify("notifications/initialized", json!({})).await;
        m
    }

    async fn write(&mut self, v: Value) {
        let si = self.stdin.as_mut().expect("stdin open");
        si.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        si.flush().await.unwrap();
    }

    async fn notify(&mut self, method: &str, params: Value) {
        self.write(json!({ "jsonrpc": "2.0", "method": method, "params": params })).await;
    }

    /// 发请求，等同 id 的响应。stdout 的每一行都必须是 JSON-RPC 消息（否则解析就失败）。
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.send_request(method, params).await;
        self.response(id).await
    }

    /// 只发请求，不等响应（之后用 [`Mcp::response`] 取）。同一时间只放一个在途请求。
    async fn send_request(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let id = self.next;
        self.write(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
        id
    }

    async fn response(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(60), self.lines.next_line())
                .await
                .expect("timed out waiting for the MCP server")
                .unwrap()
                .expect("the MCP server closed stdout");
            let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("stdout is not JSON: {line:?}: {e}"));
            assert_eq!(v["jsonrpc"], "2.0", "{line}");
            if v["id"] == id {
                return v;
            }
        }
    }

    /// `tools/call`：返回 (isError, 第一段文字解析出的 JSON 或原文, 全部文字段)。
    async fn call(&mut self, name: &str, args: Value) -> (bool, Value, Vec<String>) {
        let r = self.request("tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(r.get("error").is_none(), "{name}: protocol error: {r}");
        let res = &r["result"];
        let blocks: Vec<String> =
            res["content"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect();
        let first = serde_json::from_str(&blocks[0]).unwrap_or_else(|_| Value::String(blocks[0].clone()));
        (res["isError"].as_bool().unwrap(), first, blocks)
    }

    /// 成功的调用：返回第一段文字解析出的 JSON。
    async fn ok(&mut self, name: &str, args: Value) -> Value {
        let (err, v, _) = self.call(name, args).await;
        assert!(!err, "{name} failed: {v}");
        v
    }

    /// 关掉 stdin 并等进程退出，返回退出码。
    async fn close(mut self) -> i32 {
        drop(self.stdin.take());
        let st = tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("anycon mcp exits when stdin closes")
            .unwrap();
        st.code().unwrap_or(-1)
    }
}

struct Node {
    _dir: tempfile::TempDir,
    ws: std::path::PathBuf,
    home: std::path::PathBuf,
    daemon: Option<DaemonProc>,
}

async fn node() -> Node {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    // 内置的真实 agent 没有配置段也会探测：都关掉，测试不碰真 CLI / 网络
    assert_eq!(anycon(&home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await.code, 0);
    assert_eq!(anycon(&home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await.code, 0);
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    Node { ws: d.path().join("ws"), _dir: d, home, daemon: Some(daemon) }
}

impl Node {
    async fn cli(&self, args: &[&str]) -> Run {
        anycon(&self.home, args, None, &[]).await
    }

    async fn stop(mut self) {
        if let Some(d) = self.daemon.take() {
            d.stop().await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_work_end_to_end_against_a_real_daemon() {
    let n = node().await;
    assert_eq!(n.cli(&["projects", "create", "demo", "--git"]).await.code, 0);
    // get_diff 要仓库里至少有一个提交
    let commit = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "--allow-empty", "-m", "init"])
        .current_dir(n.ws.join("demo"))
        .output()
        .unwrap();
    assert!(commit.status.success(), "{}", String::from_utf8_lossy(&commit.stderr));
    let mut m = Mcp::start(&n.home, &[], &[]).await;

    // tools/list：九个工具，没有任何批准 / 拒绝权限的工具，create_session 的 schema 里没有 env
    let list = m.request("tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "list_agents",
            "list_projects",
            "list_sessions",
            "read_session",
            "search_sessions",
            "create_session",
            "send_message",
            "cancel_turn",
            "get_diff"
        ]
    );
    let create = tools.iter().find(|t| t["name"] == "create_session").unwrap();
    assert!(create["inputSchema"]["properties"].get("env").is_none());

    // list_agents / list_projects：值来自 daemon
    let agents = m.ok("list_agents", json!({})).await;
    let mock = agents["agents"].as_array().unwrap().iter().find(|a| a["id"] == "mock").expect("mock agent");
    assert_eq!(mock["status"], "ready");
    let yolo = mock["modes"].as_array().unwrap().iter().find(|x| x["id"] == "yolo").unwrap();
    assert_eq!((yolo["risk"].as_str(), yolo["selectable"].as_bool()), (Some("dangerous"), Some(false)));
    let projects = m.ok("list_projects", json!({})).await;
    assert!(projects["projects"].as_array().unwrap().iter().any(|p| p["name"] == "demo"));

    // create_session + 等第一轮结束：拿到最终文字、状态和用量
    let r = m.ok("create_session", json!({ "agent": "mock", "project": "demo", "prompt": "hello there" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("Mock reply:") && text.contains("You said: hello there"), "{text}");
    assert_eq!(r["session_status"], "idle");
    assert_eq!(r["usage"]["session_total"]["turns"], 1, "{r}");
    let sid = r["session_id"].as_str().unwrap().to_string();

    // send_message(wait) 用前缀 / last 引用会话
    let r = m.ok("send_message", json!({ "session": &sid[..12], "message": "second follow-up" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    assert!(r["text"].as_str().unwrap().contains("You said: second follow-up"), "{r}");
    assert!(!r["text"].as_str().unwrap().contains("hello there"), "only the new turn's text: {r}");
    let r = m.ok("send_message", json!({ "session": "last", "message": "third", "wait": false })).await;
    assert_eq!((r["sent"].clone(), r["session_id"].as_str()), (json!(true), Some(sid.as_str())), "{r}");

    // list_sessions / read_session / search_sessions
    let ls = m.ok("list_sessions", json!({ "project": "demo" })).await;
    let found = ls["sessions"].as_array().unwrap();
    assert!(found.iter().any(|s| s["id"] == sid.as_str()), "{ls}");
    let rs = m.ok("read_session", json!({ "session": &sid })).await;
    let roles: Vec<(&str, &str)> = rs["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["role"].as_str().unwrap(), x["text"].as_str().unwrap()))
        .collect();
    assert!(roles.iter().any(|(r, t)| *r == "user" && *t == "hello there"), "{roles:?}");
    assert!(roles.iter().any(|(r, t)| *r == "assistant" && t.contains("You said: second follow-up")), "{roles:?}");
    assert_eq!(rs["session"]["project"], "demo");
    let rs2 = m.ok("read_session", json!({ "session": &sid, "limit": 1 })).await;
    assert_eq!(rs2["messages"].as_array().unwrap().len(), 1);
    assert_eq!(rs2["has_more_before"], true);
    let before = rs2["next_before_seq"].as_u64().unwrap();
    let older = m.ok("read_session", json!({ "session": &sid, "limit": 1, "before_seq": before })).await;
    assert!(older["messages"][0]["seq"].as_u64().unwrap() < before, "{older}");
    let hits = m.ok("search_sessions", json!({ "query": "second follow-up" })).await;
    assert!(hits["hits"].as_array().unwrap().iter().any(|h| h["session_id"] == sid.as_str()), "{hits}");

    // get_diff：mock 的 `write` 在项目目录里写了一个文件
    let r = m
        .ok(
            "create_session",
            json!({ "agent": "mock", "project": "demo", "prompt": "write the file", "worktree": false }),
        )
        .await;
    assert_eq!(r["outcome"], "completed", "{r}");
    let wsid = r["session_id"].as_str().unwrap().to_string();
    let d = m.ok("get_diff", json!({ "session": &wsid })).await;
    assert_eq!(d["against"], "uncommitted");
    assert!(d["files"].as_array().unwrap().iter().any(|f| f["path"] == "anycon-mock.txt"), "{d}");
    let (err, meta, blocks) = m.call("get_diff", json!({ "session": &wsid, "path": "anycon-mock.txt" })).await;
    assert!(!err, "{meta}");
    assert!(blocks.len() == 2 && blocks[1].contains("anycon-mock.txt"), "{blocks:?}");

    // 工具失败是 isError，不是协议错误；参数多余的字段（比如 env）被拒绝
    let (err, v, _) = m.call("send_message", json!({ "session": "sess_nonexistent", "message": "x" })).await;
    assert!(err, "{v}");
    let (err, v, _) = m.call("create_session", json!({ "prompt": "x", "env": { "A": "1" } })).await;
    assert!(err && v.as_str().unwrap().contains("env"), "{v}");
    let r = m.request("tools/call", json!({ "name": "approve_permission", "arguments": {} })).await;
    assert_eq!(r["error"]["code"], -32602, "there is no approval tool: {r}");

    // cancel_turn：取消一个慢 turn
    let r = m.ok("create_session", json!({ "agent": "mock", "prompt": "slow answer please", "wait": false })).await;
    let slow = r["session"]["id"].as_str().unwrap().to_string();
    let r = m.ok("cancel_turn", json!({ "session": &slow })).await;
    assert_eq!(r["cancelled"], true);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = m.ok("read_session", json!({ "session": &slow })).await;
        if s["session"]["status"] == "idle" {
            break;
        }
        assert!(Instant::now() < deadline, "the cancelled turn never finished: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 审计：MCP 做的是 local-mcp，人敲的 CLI 是 local-cli
    let audit = n.cli(&["audit", "--json", "-n", "500"]).await.json();
    let entries = audit["entries"].as_array().unwrap();
    let by =
        |name: &str, action: &str| entries.iter().filter(|e| e["device_name"] == name && e["action"] == action).count();
    assert!(by("local-mcp", "session.create") >= 3, "{entries:?}");
    assert!(by("local-mcp", "session.send") >= 2, "{entries:?}");
    assert!(by("local-mcp", "session.cancel") >= 1, "{entries:?}");
    assert_eq!(by("local-cli", "session.create"), 0, "the CLI created no sessions: {entries:?}");
    assert!(entries.iter().all(|e| e["device_id"] == "local"));

    assert_eq!(m.close().await, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_request_returns_waiting_permission_and_only_a_person_can_answer() {
    let n = node().await;
    let mut m = Mcp::start(&n.home, &[], &[]).await;
    let started = Instant::now();
    let r = m
        .ok("create_session", json!({ "agent": "mock", "prompt": "run permission now", "timeout_seconds": 120 }))
        .await;
    assert!(started.elapsed() < Duration::from_secs(30), "does not wait for the timeout");
    assert_eq!(r["outcome"], "waiting_permission", "{r}");
    assert_eq!(r["permission"]["kind"], "tool", "{r}");
    assert_eq!(r["permission"]["title"], "Run command", "{r}");
    assert!(r["note"].as_str().unwrap().contains("person"), "{r}");
    let sid = r["session_id"].as_str().unwrap().to_string();

    // 请求仍然挂着：MCP 没有答复它
    let pending = n.cli(&["pending", "--json"]).await.json();
    let reqs = pending["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 1, "{pending}");
    let s = m.ok("read_session", json!({ "session": &sid })).await;
    assert_eq!(s["session"]["status"], "waiting_permission");
    assert_eq!(s["session"]["pending_permissions"], 1);
    assert_eq!(s["waiting_permission"]["requests"][0]["title"], "Run command", "{s}");
    // 给正在等审批的会话再发一条：同样马上返回 waiting_permission
    let r = m.ok("send_message", json!({ "session": &sid, "message": "more", "timeout_seconds": 60 })).await;
    assert_eq!(r["outcome"], "waiting_permission", "{r}");

    // 人在 CLI 里批准之后，会话继续跑完
    let id = reqs[0]["request_id"].as_str().unwrap();
    assert_eq!(n.cli(&["approve", id]).await.code, 0);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let s = m.ok("read_session", json!({ "session": &sid })).await;
        if s["session"]["status"] == "idle" {
            break;
        }
        assert!(Instant::now() < deadline, "the session never finished after approval: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let audit = n.cli(&["audit", "--json", "-n", "500"]).await.json();
    let respond: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "permission.respond")
        .map(|e| e["device_name"].as_str().unwrap())
        .collect();
    assert_eq!(respond, ["local-cli"], "the only answer came from the person's CLI");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dangerous_modes_are_refused_unless_the_server_is_started_with_the_flag() {
    let n = node().await;
    let mut m = Mcp::start(&n.home, &[], &[]).await;
    let (err, v, _) = m.call("create_session", json!({ "agent": "mock", "prompt": "run it", "mode": "yolo" })).await;
    assert!(err, "{v}");
    let msg = v.as_str().unwrap();
    assert!(msg.contains("dangerous") && msg.contains("--allow-dangerous-modes"), "{msg}");
    assert!(
        n.cli(&["ls", "--all", "--json"]).await.json()["sessions"].as_array().unwrap().is_empty(),
        "nothing was created"
    );
    // 安全的模式可以
    let r = m.ok("create_session", json!({ "agent": "mock", "prompt": "hi", "mode": "plan" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    assert_eq!(m.close().await, 0);

    // 带标志：放行（daemon 的 permissions.allow_dangerous_modes 默认开着）
    let mut m = Mcp::start(&n.home, &["--allow-dangerous-modes"], &[]).await;
    let r = m.ok("create_session", json!({ "agent": "mock", "prompt": "run it", "mode": "yolo" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    let s = m.ok("read_session", json!({ "session": r["session_id"].as_str().unwrap() })).await;
    assert_eq!(s["session"]["mode"], "yolo");
    assert_eq!(m.close().await, 0);

    // 第二道闸：daemon 自己关掉 allow_dangerous_modes 时，带了标志也建不出来
    assert_eq!(n.cli(&["config", "set", "permissions.allow_dangerous_modes", "false"]).await.code, 0);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut m = Mcp::start(&n.home, &["--allow-dangerous-modes"], &[]).await;
    let (err, v, _) = m.call("create_session", json!({ "agent": "mock", "prompt": "run it", "mode": "yolo" })).await;
    assert!(err, "the daemon's own ceiling still applies: {v}");
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inside_an_anycon_session_the_mutating_tools_are_refused_unless_allowed() {
    let n = node().await;
    // 先用一个不受限的 server 造一个会话，嵌套的 server 要能读到它
    let mut free = Mcp::start(&n.home, &[], &[]).await;
    let r = free.ok("create_session", json!({ "agent": "mock", "prompt": "seed" })).await;
    let sid = r["session_id"].as_str().unwrap().to_string();
    assert_eq!(free.close().await, 0);
    let before = n.cli(&["ls", "--all", "--json"]).await.json()["sessions"].as_array().unwrap().len();

    let mut m = Mcp::start(&n.home, &[], &[("ANYCON_AGENT", "1")]).await;
    for (name, args) in [
        ("create_session", json!({ "agent": "mock", "prompt": "nested" })),
        ("send_message", json!({ "session": &sid, "message": "nested" })),
        ("cancel_turn", json!({ "session": &sid })),
    ] {
        let (err, v, _) = m.call(name, args).await;
        assert!(err && v.as_str().unwrap().contains("--allow-nested"), "{name}: {v}");
    }
    // 只读工具照常
    assert!(!m.ok("list_agents", json!({})).await["agents"].as_array().unwrap().is_empty());
    assert_eq!(m.ok("list_sessions", json!({})).await["sessions"].as_array().unwrap().len(), before);
    let rs = m.ok("read_session", json!({ "session": &sid })).await;
    assert_eq!(rs["session"]["id"], sid.as_str());
    m.ok("search_sessions", json!({ "query": "seed" })).await;
    assert_eq!(m.close().await, 0);
    let after = n.cli(&["ls", "--all", "--json"]).await.json()["sessions"].as_array().unwrap().len();
    assert_eq!(after, before, "the refused calls changed nothing");

    // --allow-nested 放开
    let mut m = Mcp::start(&n.home, &["--allow-nested"], &[("ANYCON_AGENT", "1")]).await;
    let r = m.ok("send_message", json!({ "session": &sid, "message": "allowed now" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    assert_eq!(m.close().await, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_starts_without_a_daemon_and_reports_it_per_call() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    let mut m = Mcp::start(&home, &[], &[]).await;
    assert!(!m.request("tools/list", json!({})).await["result"]["tools"].as_array().unwrap().is_empty());
    let (err, v, _) = m.call("list_agents", json!({})).await;
    assert!(err && v.as_str().unwrap().contains("not running"), "{v}");
    // 未知方法、畸形输入：服务端照常
    let r = m.request("resources/list", json!({})).await;
    assert_eq!(r["error"]["code"], -32601);
    m.write(json!("garbage")).await;
    let ping = m.request("ping", json!({})).await;
    assert_eq!(ping["result"], json!({}));
    assert_eq!(m.close().await, 0);
}

/// 轮询 CLI 的 `ls`，直到会话到了某个状态。
async fn wait_status(n: &Node, session: &str, status: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let ls = n.cli(&["ls", "--all", "--json"]).await.json();
        let now = ls["sessions"].as_array().unwrap().iter().find(|s| s["id"] == session).map(|s| s["status"].clone());
        if now == Some(json!(status)) {
            return;
        }
        assert!(Instant::now() < deadline, "session {session} never became {status}: {now:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_sent_while_the_session_is_busy_gets_its_own_answer() {
    let n = node().await;
    let mut m = Mcp::start(&n.home, &[], &[]).await;
    // 第一轮很慢（mock 每个事件之间停 200 ms）；第二条消息在它进行中发出，排在它后面
    let r = m
        .ok("create_session", json!({ "agent": "mock", "prompt": "slow answer please, first turn", "wait": false }))
        .await;
    let sid = r["session"]["id"].as_str().unwrap().to_string();
    let started = Instant::now();
    let r = m.ok("send_message", json!({ "session": &sid, "message": "SECOND message" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    let text = r["text"].as_str().unwrap();
    assert!(text.contains("You said: SECOND message"), "{r}");
    assert!(!text.contains("first turn"), "the answer of the previous turn must not come back: {r}");
    assert_eq!(r["usage"]["session_total"]["turns"], 2, "both turns were over when it returned: {r}");
    assert!(started.elapsed() > Duration::from_millis(500), "it waited for the first turn to end first");
    assert_eq!(m.close().await, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_message_that_is_cancelled_is_reported_as_dropped() {
    let n = node().await;
    assert_eq!(n.cli(&["config", "set", "sessions.max_concurrent", "1"]).await.code, 0);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let mut m = Mcp::start(&n.home, &[], &[]).await;
    // B 先跑完一轮、闲下来；A 随后占着唯一的名额（等人批准）
    let b = m.ok("create_session", json!({ "agent": "mock", "prompt": "hello b" })).await;
    let bid = b["session_id"].as_str().unwrap().to_string();
    let a = m.ok("create_session", json!({ "agent": "mock", "prompt": "run permission now" })).await;
    assert_eq!(a["outcome"], "waiting_permission", "{a}");
    // 给 B 发消息：没有名额，B 排队；这个调用还在等的时候有人把队列清了
    let id = m.send_request("tools/call", json!({ "name": "send_message", "arguments": { "session": &bid, "message": "queued one", "timeout_seconds": 60 } })).await;
    wait_status(&n, &bid, "queued").await;
    assert_eq!(n.cli(&["cancel", &bid]).await.code, 0);
    let started = Instant::now();
    let r = m.response(id).await;
    assert!(started.elapsed() < Duration::from_secs(10), "reported soon after the cancel, not at the timeout");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    let v: Value = serde_json::from_str(text).unwrap();
    assert_eq!(v["outcome"], "dropped", "{v}");
    assert!(!text.contains("keeps running"), "{v}");
    assert_eq!(m.close().await, 0);
    n.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_in_a_dangerous_mode_cannot_be_driven_without_the_flag() {
    let n = node().await;
    // 带标志的 server 开一个危险模式的会话
    let mut dangerous = Mcp::start(&n.home, &["--allow-dangerous-modes"], &[]).await;
    let r = dangerous.ok("create_session", json!({ "agent": "mock", "prompt": "run it", "mode": "yolo" })).await;
    assert_eq!(r["outcome"], "completed", "{r}");
    let sid = r["session_id"].as_str().unwrap().to_string();
    assert_eq!(dangerous.close().await, 0);

    // 不带标志的 server：读、取消可以，发消息不行
    let mut m = Mcp::start(&n.home, &[], &[]).await;
    let (err, v, _) = m.call("send_message", json!({ "session": &sid, "message": "more", "wait": false })).await;
    assert!(err, "{v}");
    let msg = v.as_str().unwrap();
    assert!(msg.contains("dangerous") && msg.contains("--allow-dangerous-modes"), "{msg}");
    let rs = m.ok("read_session", json!({ "session": &sid })).await;
    assert_eq!(rs["session"]["mode"], "yolo");
    m.ok("cancel_turn", json!({ "session": &sid })).await;
    // 没有多出一条用户消息
    let users = rs["messages"].as_array().unwrap().iter().filter(|x| x["role"] == "user").count();
    let after = m.ok("read_session", json!({ "session": &sid })).await;
    assert_eq!(after["messages"].as_array().unwrap().iter().filter(|x| x["role"] == "user").count(), users);
    // 请求的模式不在清单里：创建被拒
    let (err, v, _) = m.call("create_session", json!({ "agent": "mock", "prompt": "x", "mode": "ghost" })).await;
    assert!(err && v.as_str().unwrap().contains("ghost"), "{v}");
    assert_eq!(m.close().await, 0);
    n.stop().await;
}
