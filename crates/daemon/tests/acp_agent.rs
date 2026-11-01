//! ACP agent 经 daemon 跑一遍：`[agents.acp.<name>]` 配的命令是假 ACP agent（`crates/adapters/tests/bin/fake-acp-agent`，
//! python 脚本，所以只在 unix 上跑）。daemon 里没有 ACP 特判：它就是又一个 adapter。覆盖 `agent.list`、新建会话和默认值、
//! 事件、权限、取消、会话选项、一个进程服务多个会话、idle 回收后用 `session/resume` 接上、配置里增减 agent 不用重启。
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::{json, Value};

fn fake_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../adapters/tests/bin/fake-acp-agent")
}

/// `agents.acp.<name>` 的配置：命令、环境（日志、状态目录、python 冷启动慢时放宽 `--version` 的超时）。
fn acp_overrides(name: &str, log: &std::path::Path, state: &std::path::Path) -> Vec<(String, String)> {
    vec![
        (format!("agents.acp.{name}.command"), format!("\"{} acp\"", fake_agent().display())),
        (
            format!("agents.acp.{name}.env"),
            format!(
                "{{ FAKE_ACP_LOG = \"{}\", FAKE_ACP_STATE = \"{}\", ANYCON_VERSION_TIMEOUT_MS = \"20000\" }}",
                log.display(),
                state.display()
            ),
        ),
    ]
}

async fn start(extra: &[(&str, &str)]) -> (TestDaemon, tempfile::TempDir) {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("fake-acp.log");
    let state = d.path().join("state");
    let mut ov: Vec<(String, String)> = acp_overrides("fakeacp", &log, &state);
    ov.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let refs: Vec<(&str, &str)> = ov.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let td = TestDaemon::start_acp(&refs, "fakeacp").await;
    (td, d)
}

fn log_entries(d: &tempfile::TempDir) -> Vec<Value> {
    std::fs::read_to_string(d.path().join("fake-acp.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// 会话（不是探测）在 agent 里开的 session/new 所在的进程 pid。探测的 cwd 是 daemon 的探测目录。
fn session_pids(d: &tempfile::TempDir, td: &TestDaemon) -> Vec<u64> {
    let probe_cwd = td.paths.probe_scratch();
    log_entries(d)
        .into_iter()
        .filter(|e| e["kind"] == "in" && e["msg"]["method"] == "session/new")
        .filter(|e| {
            let cwd = e["msg"]["params"]["cwd"].as_str().unwrap_or("");
            std::path::Path::new(cwd).canonicalize().ok() != probe_cwd.canonicalize().ok()
        })
        .filter_map(|e| e["pid"].as_u64())
        .collect()
}

async fn wait_idle(td: &TestDaemon, c: &mut TestClient, sid: &str) {
    let t = Instant::now();
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if s.status == SessionStatus::Idle {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "stuck in {:?}", s.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_permission(td: &TestDaemon, c: &mut TestClient) -> PermissionRequest {
    let t = Instant::now();
    loop {
        let l: PermissionListResult = c.call(td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            return r;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "no permission request");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn history(td: &TestDaemon, c: &mut TestClient, sid: &str) -> Vec<Event> {
    let h: SessionHistoryResult =
        c.call(td, methods::SESSION_HISTORY, json!({ "session_id": sid, "limit": 1000 })).await.unwrap();
    h.events.into_iter().map(|e| e.event).collect()
}

fn text(evs: &[Event]) -> String {
    evs.iter().filter_map(|e| if let Event::TextDelta { text } = e { Some(text.as_str()) } else { None }).collect()
}

async fn send(td: &TestDaemon, c: &mut TestClient, sid: &str, id: &str, content: &str) {
    let _: SessionSendResult = c
        .call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": id, "content": content }))
        .await
        .unwrap();
}

async fn agent_list(td: &TestDaemon, c: &mut TestClient) -> Vec<AgentInfo> {
    let l: AgentListResult = c.call(td, methods::AGENT_LIST, json!({})).await.unwrap();
    l.agents
}

#[tokio::test]
async fn an_acp_agent_is_listed_like_any_other_and_runs_sessions() {
    let (td, dir) = start(&[("sessions.idle_timeout", "\"2s\"")]).await;
    let mut c = td.client("a");

    // agent.list：id 是它的名字，driver = acp；注册表里的 agent 被夹具关掉了（用户动过它们的配置，所以列出为 disabled）
    let agents = agent_list(&td, &mut c).await;
    let fake = agents.iter().find(|a| a.id == "fakeacp").expect("the configured ACP agent is listed");
    assert_eq!((fake.driver.as_str(), fake.status), ("acp", AgentStatus::Ready), "{:?}", fake.issue);
    assert_eq!((fake.label.as_str(), fake.version.as_deref()), ("Fake ACP", Some("1.2.3")));
    assert_eq!(fake.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["fast", "smart"]);
    assert_eq!(fake.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["ask", "code", "plan"]);
    assert!(fake.capabilities.resume && !fake.capabilities.interject && !fake.capabilities.env);
    for id in ["opencode", "gemini", "goose", "kimi", "qwen", "cursor", "copilot", "pi"] {
        let a = agents.iter().find(|a| a.id == id).unwrap_or_else(|| panic!("{id} is listed as disabled"));
        assert_eq!((a.driver.as_str(), a.status), ("acp", AgentStatus::Disabled), "{id}");
    }

    // agent.diagnose 用 adapter 自己的诊断：协议版本、登录方式、能力
    let d: AgentDiagnoseResult = c.call(&td, methods::AGENT_DIAGNOSE, json!({ "id": "fakeacp" })).await.unwrap();
    let field = |label: &str| {
        d.fields.iter().find(|f| f.label == label).unwrap_or_else(|| panic!("{label}: {}", d.text)).value.clone()
    };
    assert_eq!(field("ACP protocol version"), "1");
    assert!(field("agent capabilities").contains("resume=true"));
    assert_eq!(field("status"), "ready");
    assert!(d.text.contains("fakeacp"));

    // 新建会话：默认值来自探测（fast / medium / ask），agent 给的标题优先
    let s: SessionSummary = c
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fakeacp", "prompt": "stream and title", "message_id": "m1" }),
        )
        .await
        .unwrap();
    assert_eq!(s.agent, "fakeacp");
    assert_eq!(
        (s.settings.model.as_deref(), s.settings.thinking.as_deref(), s.settings.mode.as_deref()),
        (Some("fast"), Some("medium"), Some("ask"))
    );
    wait_idle(&td, &mut c, &s.id).await;
    let evs = history(&td, &mut c, &s.id).await;
    assert_eq!(text(&evs), "Hello worldSecond message");
    assert!(evs.iter().any(|e| matches!(e, Event::ThinkingDelta { .. })));
    let usage = evs
        .iter()
        .find_map(|e| {
            if let Event::TurnDone { usage, reason: TurnDoneReason::Completed, .. } = e {
                usage.clone()
            } else {
                None
            }
        })
        .expect("a completed turn with usage");
    assert_eq!((usage.context_tokens, usage.context_window), (Some(1234), Some(200000)));
    let after: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(after.title, "A better title");
    assert!(after.agent_session_id.as_deref().is_some_and(|i| i.starts_with("fake-")));

    // 工具调用 + 权限：agent 的四个选项原样出现，选的 optionId 原样回
    send(&td, &mut c, &s.id, "m2", "permission please").await;
    let req = wait_permission(&td, &mut c).await;
    assert_eq!(req.kind, PermissionKind::Tool);
    assert_eq!(
        req.options.iter().map(|o| (o.id.as_str(), o.kind)).collect::<Vec<_>>(),
        vec![
            ("allow-once", PermissionOptionKind::AllowOnce),
            ("allow-always", PermissionOptionKind::AllowAlways),
            ("reject-once", PermissionOptionKind::RejectOnce),
            ("reject-always", PermissionOptionKind::RejectAlways)
        ]
    );
    let _: Empty = c
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow-always" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    let evs = history(&td, &mut c, &s.id).await;
    assert!(evs.iter().any(
        |e| matches!(e, Event::ToolResult { is_error: false, output, .. } if output == &json!("ran after allow-always"))
    ));
    let outcome = log_entries(&dir).into_iter().find(|e| e["kind"] == "permission_outcome").unwrap();
    assert_eq!(outcome["outcome"], json!({"outcome": "selected", "optionId": "allow-always"}));

    // 取消一个跑着的 turn
    send(&td, &mut c, &s.id, "m3", "hold on").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _: Empty = c.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    let evs = history(&td, &mut c, &s.id).await;
    assert!(evs.iter().any(|e| matches!(e, Event::TurnDone { reason: TurnDoneReason::Cancelled, .. })));

    // 会话中改选项：模式走 `session/set_config_option`；ACP 不支持的设置在创建 / 修改时就被拒绝
    let r: SessionSetSettingsResult = c
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "code", "thinking": "high" } }),
        )
        .await
        .unwrap();
    assert_eq!((r.settings.mode.as_deref(), r.settings.thinking.as_deref()), (Some("code"), Some("high")));
    let e = c
        .call::<SessionSetSettingsResult>(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "mode": "nope" } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
    let e = c
        .call::<SessionSummary>(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "fakeacp", "settings": { "max_turns": 3 } }),
        )
        .await
        .unwrap_err();
    assert_eq!(e.stable_code(), Some(ErrorCode::InvalidSettings));
    let sets: Vec<Value> = log_entries(&dir)
        .into_iter()
        .filter(|e| e["kind"] == "in" && e["msg"]["method"] == "session/set_config_option")
        .map(|e| e["msg"]["params"].clone())
        .collect();
    assert!(sets.iter().any(|p| p["configId"] == "mode" && p["value"] == "code"), "{sets:?}");
    assert!(sets.iter().any(|p| p["configId"] == "effort" && p["value"] == "high"), "{sets:?}");

    // 第二个会话和第一个共用同一个 agent 进程
    let s2: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fakeacp", "prompt": "hello second", "message_id": "n1" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s2.id).await;
    assert_eq!(text(&history(&td, &mut c, &s2.id).await), "echo: hello second");
    let pids = session_pids(&dir, &td);
    assert_eq!(pids.len(), 2, "{pids:?}");
    assert_eq!(pids[0], pids[1], "one process serves both sessions");

    // 两个会话都空闲超过 idle_timeout：agent 进程退出（最后一个会话脱离时），下一条消息用 session/resume 接上原来的会话
    let before = after.agent_session_id.clone();
    let t = Instant::now();
    while td.ctx.sessions.counts().await.1 != 0 {
        assert!(t.elapsed() < Duration::from_secs(20), "the idle sessions were not recycled");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    send(&td, &mut c, &s.id, "m4", "hello after idle").await;
    wait_idle(&td, &mut c, &s.id).await;
    let resumes: Vec<Value> = log_entries(&dir)
        .into_iter()
        .filter(|e| e["kind"] == "in" && e["msg"]["method"] == "session/resume")
        .map(|e| e["msg"]["params"].clone())
        .collect();
    assert_eq!(resumes.len(), 1, "{resumes:?}");
    assert_eq!(resumes[0]["sessionId"].as_str(), before.as_deref());
    let evs = history(&td, &mut c, &s.id).await;
    assert!(text(&evs).ends_with("echo: hello after idle"), "{}", text(&evs));
    let now: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(now.agent_session_id, before);
    // 重启后设置（含会话中改过的）照新进程重新应用
    assert_eq!((now.settings.mode.as_deref(), now.settings.thinking.as_deref()), (Some("code"), Some("high")));
    td.shutdown().await;
}

#[tokio::test]
async fn a_crash_marks_the_session_failed_and_the_next_message_starts_over_with_resume() {
    let (td, dir) = start(&[]).await;
    let mut c = td.client("a");
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "fakeacp", "prompt": "hello", "message_id": "m1" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    send(&td, &mut c, &s.id, "m2", "crash now").await;
    let t = Instant::now();
    loop {
        let now: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
        if now.status == SessionStatus::Error {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "stuck in {:?}", now.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let evs = history(&td, &mut c, &s.id).await;
    assert!(evs.iter().any(|e| matches!(e, Event::Error { code, .. } if code == "agent_exited")));
    // 下一条消息：新进程里 session/resume 接上
    send(&td, &mut c, &s.id, "m3", "back again").await;
    wait_idle(&td, &mut c, &s.id).await;
    assert!(text(&history(&td, &mut c, &s.id).await).ends_with("echo: back again"));
    let resumed =
        log_entries(&dir).into_iter().filter(|e| e["kind"] == "in" && e["msg"]["method"] == "session/resume").count();
    assert_eq!(resumed, 1);
    td.shutdown().await;
}

#[tokio::test]
async fn acp_agents_come_and_go_with_the_config_without_a_restart() {
    let (td, dir) = start(&[]).await;
    let mut c = td.client("a");
    let log = dir.path().join("fake-acp.log");
    let state = dir.path().join("state");
    assert!(agent_list(&td, &mut c).await.iter().all(|a| a.id != "second"));

    // 加一个：配置文件里出现 `[agents.acp.second]`，几秒内就被探测并出现在列表里
    for (k, v) in acp_overrides("second", &log, &state) {
        anycon_daemon::config::set_value(&td.paths, &k, &v).unwrap();
    }
    let t = Instant::now();
    loop {
        let list = agent_list(&td, &mut c).await;
        if list.iter().any(|a| a.id == "second" && a.status == AgentStatus::Ready) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{list:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // 能用它建会话
    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "second", "prompt": "hello", "message_id": "m1" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;

    // 关掉它（热加载的 `enabled`）：列表里变成 disabled
    anycon_daemon::config::set_value(&td.paths, "agents.acp.second.enabled", "false").unwrap();
    let t = Instant::now();
    loop {
        let list = agent_list(&td, &mut c).await;
        if list.iter().any(|a| a.id == "second" && a.status == AgentStatus::Disabled) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{list:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 删掉那一段：agent 从列表里消失
    let file = td.paths.config_toml();
    let text = std::fs::read_to_string(&file).unwrap();
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.starts_with('[') {
            skipping = line.starts_with("[agents.acp.second");
        }
        if !skipping {
            out.push(line);
        }
    }
    std::fs::write(&file, out.join("\n") + "\n").unwrap();
    let t = Instant::now();
    loop {
        let list = agent_list(&td, &mut c).await;
        if list.iter().all(|a| a.id != "second") {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(20), "{list:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    td.shutdown().await;
}

#[tokio::test]
async fn defaults_for_an_acp_agent_are_written_under_agents_acp() {
    let (td, _dir) = start(&[]).await;
    let mut c = td.client("a");
    let _: Empty = c
        .call(&td, methods::NODE_SET_AGENT_DEFAULTS, json!({ "agent": "fakeacp", "mode": "plan", "model": "smart" }))
        .await
        .unwrap();
    let cfg = anycon_daemon::config::Config::load(&td.paths).unwrap();
    let ac = cfg.adapter_config("fakeacp").unwrap();
    assert_eq!((ac.default_mode.as_deref(), ac.default_model.as_deref()), (Some("plan"), Some("smart")));
    assert!(!cfg.agents.agent.contains_key("fakeacp"), "not under [agents.fakeacp]");
    // 新会话用这些默认值，客户端看到的列表里 `default` 标记也跟着变
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fakeacp" })).await.unwrap();
    assert_eq!((s.settings.mode.as_deref(), s.settings.model.as_deref()), (Some("plan"), Some("smart")));
    let agents = agent_list(&td, &mut c).await;
    let fake = agents.iter().find(|a| a.id == "fakeacp").unwrap();
    assert!(fake.models.iter().find(|m| m.id == "smart").unwrap().default);
    td.shutdown().await;
}
