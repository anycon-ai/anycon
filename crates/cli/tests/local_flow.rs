//! `anycon` 二进制对本机 daemon（真 mock adapter）的端到端：init、daemon、全部会话命令、权限交互、退出码。

mod common;

use std::time::Duration;

use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exit_codes_before_and_after_init() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    // 没初始化：4
    let r = anycon(&home, &["ls"], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    assert!(r.stderr.contains("error: not_initialized:"), "{}", r.stderr);
    let r = anycon(&home, &["daemon"], None, &[]).await;
    assert_eq!(r.code, 4, "{}", r.stderr);
    // 参数错误：2
    assert_eq!(anycon(&home, &["new"], None, &[]).await.code, 2);
    assert_eq!(anycon(&home, &["no-such-command"], None, &[]).await.code, 2);
    // 初始化
    init(&home, &d.path().join("ws"), "box", Some("http://127.0.0.1:1")).await;
    let again =
        anycon(&home, &["init", "--workspace", d.path().join("ws").to_str().unwrap(), "--yes"], None, &[]).await;
    assert_eq!(again.code, 1);
    assert!(again.stderr.contains("already initialized"), "{}", again.stderr);
    let forced =
        anycon(&home, &["init", "--workspace", d.path().join("ws").to_str().unwrap(), "--yes", "--force"], None, &[])
            .await;
    assert_eq!(forced.code, 0, "{}", forced.stderr);
    assert!(forced.stdout.contains("paired device must be paired again"));
    // 已初始化但 daemon 没跑：3
    let r = anycon(&home, &["ls"], None, &[]).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(r.stderr.contains("error: offline:"));
    // doctor 在 daemon 没起来时也能跑（离线版本），有失败项所以退出码 1，--json 仍是合法 JSON
    let r = anycon(&home, &["doctor", "--json"], None, &[("ANYCON_AGENTS__ENV_FROM_LOGIN_SHELL", "false")]).await;
    assert_eq!(r.code, 1);
    let j = r.json();
    assert_eq!(j["ok"], false);
    let ids: Vec<&str> = j["checks"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"config") && ids.contains(&"daemon") && ids.contains(&"workspace"));
    assert!(j["checks"].as_array().unwrap().iter().find(|c| c["id"] == "daemon").unwrap()["ok"] == false);
    // config 不需要 daemon
    assert_eq!(anycon(&home, &["config", "get", "sessions.max_concurrent"], None, &[]).await.stdout.trim(), "3");
    let r = anycon(&home, &["config", "set", "sessions.max_concurrent", "5"], None, &[]).await;
    assert!(r.stdout.contains("daemon is not running"), "{}", r.stdout);
    assert_eq!(anycon(&home, &["config", "get", "sessions.max_concurrent"], None, &[]).await.stdout.trim(), "5");
    assert_eq!(
        anycon(&home, &["config", "set", "sessions.max_concurrent", "0"], None, &[]).await.code,
        2,
        "invalid value is rejected"
    );
    assert!(anycon(&home, &["config", "path"], None, &[]).await.stdout.trim().ends_with("config.toml"));
    assert_eq!(anycon(&home, &["config", "get", "nope.key"], None, &[]).await.code, 1);
    let v = anycon(&home, &["version", "--json"], None, &[]).await.json();
    assert_eq!(v["protocol_version"], 1);
    let c = anycon(&home, &["completions", "zsh"], None, &[]).await;
    assert_eq!(c.code, 0);
    assert!(c.stdout.contains("anycon"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_workflow_with_the_mock_agent() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    // 内置的真实 agent（claude-code、codex）没有配置段也会探测：都关掉，测试不碰真 CLI / 网络
    assert_eq!(anycon(&home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await.code, 0);
    assert_eq!(anycon(&home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await.code, 0);
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    macro_rules! a {
        ($($x:expr),* $(,)?) => {
            anycon(&home, &[$($x),*], None, &[])
        };
    }

    // agents：mock ready，claude-code disabled
    let ag = a!("agents", "--json").await.json();
    let agents = ag["agents"].as_array().unwrap();
    let mock = agents.iter().find(|x| x["id"] == "mock").unwrap();
    assert_eq!(mock["status"], "ready");
    assert_eq!(agents.iter().find(|x| x["id"] == "claude-code").unwrap()["status"], "disabled");
    let human = a!("agents").await;
    assert!(
        human.stdout.contains("mock (Mock Agent) — ready") && human.stdout.contains("mock-large"),
        "{}",
        human.stdout
    );

    // projects
    assert_eq!(a!("projects", "create", "demo", "--git").await.code, 0);
    assert!(a!("projects").await.stdout.contains("demo"));
    let e = a!("projects", "create", "demo").await;
    assert_eq!((e.code, e.stderr.contains("error: exists:")), (1, true), "{}", e.stderr);
    // 参数不合法 = 退出码 2（spec/cli.md § 4）
    let e = a!("projects", "create", "../x").await;
    assert_eq!(e.code, 2, "{}", e.stderr);
    let e = a!("projects", "clone", "file:///etc/passwd").await;
    assert_eq!(e.code, 2, "{}", e.stderr);
    assert!(e.stderr.contains("unsupported clone url"), "{}", e.stderr);

    // new + 权限交互：回答 a（允许）
    let r =
        anycon(&home, &["new", "--agent", "mock", "--project", "demo", "run permission now"], Some("a\n"), &[]).await;
    assert_eq!(r.code, 0, "stderr: {}\nstdout: {}", r.stderr, r.stdout);
    assert!(r.stdout.contains("> run permission now"), "{}", r.stdout);
    assert!(r.stdout.contains("Mock reply: [model=mock-large thinking=high mode=ask]"), "{}", r.stdout);
    assert!(r.stdout.contains("▸ bash: echo hi"), "{}", r.stdout);
    assert!(r.stdout.contains("── completed · "), "{}", r.stdout);
    assert!(
        r.stderr.contains("[a] Allow")
            && r.stderr.contains("[s] Allow for this session")
            && r.stderr.contains("[d] Deny"),
        "{}",
        r.stderr
    );
    assert!(!r.stdout.contains("(thinking)"), "thinking is hidden by default");

    // 拒绝（带说明）
    let r = anycon(&home, &["new", "--agent", "mock", "run permission please"], Some("d\nnot now\n"), &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("skipped"), "{}", r.stdout);
    assert!(r.stderr.contains("message for the agent"), "{}", r.stderr);

    // --auto-approve + --show-thinking
    let r = anycon(
        &home,
        &["new", "--agent", "mock", "--auto-approve", "--show-thinking", "run permission auto"],
        None,
        &[],
    )
    .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stderr.contains("--auto-approve is on"));
    assert!(r.stdout.contains("▸ bash: echo hi") && r.stdout.contains("(thinking) "), "{}", r.stdout);

    // 选项：模型 / thinking / 模式 / -o；非法值给出字段名
    let r = anycon(
        &home,
        &["new", "--agent", "mock", "--model", "mock-small", "--mode", "plan", "-o", "verbose=true", "hello opts"],
        None,
        &[],
    )
    .await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("[model=mock-small thinking=none mode=plan] verbose=on"), "{}", r.stdout);
    let r = anycon(&home, &["new", "--agent", "mock", "--model", "nope", "x"], None, &[]).await;
    assert_eq!(r.code, 2, "invalid settings are a usage error");
    assert!(r.stderr.contains("error: invalid_settings: model: unknown model nope"), "{}", r.stderr);
    let r = anycon(&home, &["new", "--agent", "ghost", "x"], None, &[]).await;
    assert_eq!(r.code, 1);
    assert!(r.stderr.contains("agent_unavailable"), "{}", r.stderr);
    // 只有一个 ready 的 agent 时 --agent 可省略
    let r = anycon(&home, &["new", "--no-wait", "--json", "implicit agent"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["agent"], "mock");

    // ls / history / send / search / rename / archive / delete
    let ls = a!("ls", "--json").await.json();
    let sessions = ls["sessions"].as_array().unwrap();
    assert!(sessions.len() >= 5);
    let first = sessions.iter().find(|s| s["title"] == "run permission now").unwrap();
    let sid = first["id"].as_str().unwrap().to_string();
    assert_eq!(first["project"], "demo");
    let table = a!("ls").await.stdout;
    assert!(table.contains("STATUS") && table.contains("run permission now") && table.contains("demo"), "{table}");
    let short = &sid[..12];
    let h = a!("history", short, "--json").await;
    let evs = h.json_lines();
    assert!(evs.iter().any(|e| e["event"]["type"] == "permission"));
    assert!(evs.windows(2).all(|w| w[0]["seq"].as_u64() < w[1]["seq"].as_u64()));
    let human = a!("history", short).await;
    assert!(human.stdout.contains("▸ bash: echo hi"), "{}", human.stdout);
    let r = anycon(&home, &["send", short, "a follow up"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout.contains("You said: a follow up") && !r.stdout.contains("run permission now"),
        "send only streams the new turn: {}",
        r.stdout
    );
    let r = a!("send", "last", "to the last one", "--no-wait").await;
    assert_eq!(r.code, 0);
    assert!(r.stdout.contains("sent") || r.stdout.contains("queued"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let r = a!("search", "follow").await;
    assert!(r.stdout.contains("a follow up"), "{}", r.stdout);
    let r = a!("search", "--json", "zzzzqqqq").await;
    assert_eq!(r.json()["hits"].as_array().unwrap().len(), 0);
    assert_eq!(a!("rename", short, "Renamed one").await.code, 0);
    assert!(a!("ls").await.stdout.contains("Renamed one"));
    assert_eq!(a!("archive", short).await.code, 0);
    assert!(!a!("ls").await.stdout.contains("Renamed one"));
    assert!(a!("ls", "--all").await.stdout.contains("Renamed one"));
    assert_eq!(a!("unarchive", short).await.code, 0);
    let e = a!("ls", "--status", "bogus").await;
    assert_eq!(e.code, 2, "{}", e.stderr);
    let e = a!("history", "sess_doesnotexist").await;
    assert_eq!(e.code, 1);
    assert!(e.stderr.contains("not_found"), "{}", e.stderr);

    // pending / approve / deny（另一个进程回答）
    let created = anycon(&home, &["new", "--no-wait", "--json", "--agent", "mock", "run permission remote"], None, &[])
        .await
        .json();
    let psid = created["id"].as_str().unwrap().to_string();
    let t = std::time::Instant::now();
    let pend = loop {
        let p = a!("pending", "--json").await.json();
        if !p["requests"].as_array().unwrap().is_empty() {
            break p;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let rid = pend["requests"][0]["request_id"].as_str().unwrap().to_string();
    assert!(a!("pending").await.stdout.contains(&rid));
    let r = anycon(&home, &["approve", &rid[..16]], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("approved"));
    let e = anycon(&home, &["approve", &rid], None, &[]).await;
    assert_eq!(e.code, 1, "second approve fails");
    let t = std::time::Instant::now();
    while a!("ls", "--json").await.json()["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == psid.as_str())
        .unwrap()["status"]
        != "idle"
    {
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let created =
        anycon(&home, &["new", "--no-wait", "--json", "--agent", "mock", "run permission deny me"], None, &[])
            .await
            .json();
    let dsid = created["id"].as_str().unwrap().to_string();
    let t = std::time::Instant::now();
    let rid2 = loop {
        let p = a!("pending", "--json").await.json();
        if let Some(r) = p["requests"].as_array().unwrap().first() {
            break r["request_id"].as_str().unwrap().to_string();
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let r = anycon(&home, &["deny", &rid2, "-m", "nope"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let t = std::time::Instant::now();
    loop {
        let h = a!("history", &dsid[..12]).await.stdout;
        if h.contains("skipped") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "{h}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // attach 看到另一个进程正在进行的 turn；cancel 终止
    let slow =
        anycon(&home, &["new", "--no-wait", "--json", "--agent", "mock", "slow slow slow"], None, &[]).await.json();
    let ssid = slow["id"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(a!("cancel", &ssid[..12]).await.code, 0);
    let t = std::time::Instant::now();
    loop {
        let h = a!("history", &ssid[..12], "--json").await;
        if h.json_lines().iter().any(|e| e["event"]["reason"] == "cancelled") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // stats / audit / devices / logs / doctor
    let st = a!("stats", "--json").await.json();
    assert!(st["stats"]["sessions"]["idle"].as_u64().unwrap() >= 3);
    assert_eq!(st["status"]["hub"]["connected"], false);
    assert!(a!("stats").await.stdout.contains("hub:"));
    let au = a!("audit", "--json", "-n", "500").await.json();
    let actions: Vec<&str> = au["entries"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    for want in ["session.create", "session.send", "permission.respond", "workspace.create", "session.cancel"] {
        assert!(actions.contains(&want), "audit lacks {want}: {actions:?}");
    }
    assert!(a!("audit", "--since", "1h").await.stdout.contains("session.create"));
    assert_eq!(a!("audit", "--since", "garbage").await.code, 2);
    assert!(a!("devices").await.stdout.contains("no paired devices"));
    assert_eq!(a!("devices", "--json").await.json()["devices"].as_array().unwrap().len(), 0);
    let pair = a!("pair").await;
    assert_eq!(pair.code, 3, "pairing needs the hub: {}", pair.stderr);
    let logs = a!("logs", "-n", "5").await;
    assert_eq!(logs.code, 0, "{}", logs.stderr);
    let doc = a!("doctor", "--json").await;
    assert_eq!(doc.code, 1);
    let dj = doc.json();
    assert!(dj["checks"].as_array().unwrap().iter().any(|c| c["id"] == "daemon" && c["ok"] == true));
    assert!(dj["checks"].as_array().unwrap().iter().any(|c| c["id"] == "agent:mock" && c["ok"] == true));

    // delete
    assert_eq!(a!("delete", &dsid[..12]).await.code, 0);
    assert_eq!(a!("history", &dsid[..12]).await.code, 1);
    // config set 热加载：daemon 在跑时立即生效
    let r = a!("config", "set", "sessions.max_concurrent", "2").await;
    assert!(r.stdout.contains("saved and applied"), "{}", r.stdout);
    let r = a!("config", "set", "hub.name", "renamed-box").await;
    assert!(r.stdout.contains("restart"), "{}", r.stdout);
    // projects: rename/trash
    assert_eq!(a!("projects", "trash", "demo").await.code, 1, "the project still has sessions with live processes");
    daemon.stop().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_detaches_without_cancelling() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    anycon(&home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await;
    anycon(&home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await;
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;
    let created =
        anycon(&home, &["new", "--no-wait", "--json", "--agent", "mock", "slow turn"], None, &[]).await.json();
    let sid = created["id"].as_str().unwrap().to_string();
    let mut c = tokio::process::Command::new(BIN);
    c.args(["attach", &sid])
        .env("ANYCON_HOME", &home)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = c.spawn().unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    let first = tokio::time::timeout(Duration::from_secs(10), err.next_line()).await.unwrap().unwrap().unwrap();
    assert!(first.contains("attached to"), "{first}");
    // 等看到输出再 Ctrl+C
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = tokio::time::timeout(Duration::from_secs(10), out.next_line()).await.unwrap().unwrap().unwrap();
    assert!(!line.is_empty());
    // SAFETY: 给自己起的子进程发 SIGINT。
    unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) };
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(130));
    // 会话没被取消：最终正常完成
    let t = std::time::Instant::now();
    loop {
        let h = anycon(&home, &["history", &sid[..12], "--json"], None, &[]).await;
        let evs = h.json_lines();
        if let Some(done) = evs.iter().find(|e| e["event"]["type"] == "turn_done") {
            assert_eq!(done["event"]["reason"], "completed");
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20));
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_approve_warns_up_front_and_only_grants_one_time_tool_permissions() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    anycon(&home, &["config", "set", "agents.claude-code.enabled", "false"], None, &[]).await;
    anycon(&home, &["config", "set", "agents.codex.enabled", "false"], None, &[]).await;
    let daemon = start_daemon(&home, &[("ANYCON_MOCK_AGENT", "1")]).await;

    // 普通工具权限：自动批准「这一次」；启动时（不是第一次自动答复时）就在 stderr 警告
    let r = anycon(&home, &["new", "--agent", "mock", "--auto-approve", "run permission now"], None, &[]).await;
    assert_eq!(r.code, 0, "stdout: {}\nstderr: {}", r.stdout, r.stderr);
    assert!(r.stderr.contains("warning: --auto-approve"), "{}", r.stderr);
    assert!(r.stdout.contains("▸ bash: echo hi"), "approved once: {}", r.stdout);

    // 计划批准不在自动批准的范围内：请求留着，直到人来决定
    let mut c = tokio::process::Command::new(BIN);
    c.args(["new", "--agent", "mock", "--auto-approve", "make a plan"])
        .env("ANYCON_HOME", &home)
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let mut child = c.spawn().unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap()).lines();
    // stderr 上先是 `session …`，随后（在任何权限请求到达之前）就是警告
    let mut warned = false;
    for _ in 0..3 {
        let l = tokio::time::timeout(Duration::from_secs(10), err.next_line()).await.unwrap().unwrap().unwrap();
        if l.starts_with("warning: --auto-approve") {
            warned = true;
            break;
        }
    }
    assert!(warned, "the warning is printed at startup");
    let t = std::time::Instant::now();
    let rid = loop {
        let p = anycon(&home, &["pending", "--json"], None, &[]).await.json();
        if let Some(r) = p["requests"].as_array().unwrap().first() {
            assert_eq!(r["kind"], "plan");
            break r["request_id"].as_str().unwrap().to_string();
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(child.try_wait().unwrap().is_none(), "the plan request was not auto-answered; the turn is still waiting");
    let p = anycon(&home, &["pending", "--json"], None, &[]).await.json();
    assert_eq!(p["requests"].as_array().unwrap().len(), 1, "still pending");
    // 人来批准之后 turn 才结束
    assert_eq!(anycon(&home, &["approve", &rid], None, &[]).await.code, 0);
    let status = tokio::time::timeout(Duration::from_secs(15), child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(0));
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_gives_up_when_the_node_claims_more_pages_but_sends_nothing_new() {
    use tokio::io::AsyncWriteExt;
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "").unwrap();
    // 假 daemon：session.history 永远回 `has_more: true` 和一页空事件
    let listener = tokio::net::UnixListener::bind(home.join("daemon.sock")).unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((s, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let (r, mut w) = s.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    let v: serde_json::Value = serde_json::from_str(&l).unwrap();
                    let resp = serde_json::json!({ "jsonrpc": "2.0", "id": v["id"], "result": { "events": [], "has_more": true } });
                    let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                }
            });
        }
    });
    let sid = format!("sess_{}", "a".repeat(22));
    let r = anycon(&home, &["--local", "--timeout", "5", "history", &sid], None, &[]).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    assert!(r.stderr.contains("returned no new events"), "{}", r.stderr);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn json_output_covers_config_path_and_edit_and_logs() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join("home");
    init(&home, &d.path().join("ws"), "box", None).await;
    let j = anycon(&home, &["config", "path", "--json"], None, &[]).await.json();
    assert!(j["path"].as_str().unwrap().ends_with("config.toml"));
    let r = anycon(&home, &["config", "edit", "--json"], None, &[("EDITOR", "true"), ("VISUAL", "true")]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.json()["valid"], true);
    // 日志：每行一个 JSON 对象
    std::fs::create_dir_all(home.join("logs")).unwrap();
    std::fs::write(home.join("logs").join("daemon.log.2026-01-01"), "first line\nsecond line\n").unwrap();
    let r = anycon(&home, &["logs", "--json", "-n", "5"], None, &[]).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let lines = r.json_lines();
    assert_eq!(
        lines.iter().map(|l| l["line"].as_str().unwrap()).collect::<Vec<_>>(),
        vec!["first line", "second line"]
    );
    // 超时参数：--timeout 在全局选项里
    let v = anycon(&home, &["--timeout", "5", "version", "--json"], None, &[]).await.json();
    assert_eq!(v["protocol_version"], 1);
}
