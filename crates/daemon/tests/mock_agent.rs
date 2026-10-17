//! 用 `crates/adapters` 里真正的 mock adapter 跑一遍（README 快速开始那条手工链路的自动化版本）。
//! daemon 本身没有任何 mock 特判：mock 只是 `[agents.mock] enabled = true`。

use std::time::Duration;

use anycon_daemon::testutil::{TestClient, TestDaemon};
use anycon_protocol::*;
use serde_json::json;

async fn wait_idle(td: &TestDaemon, c: &mut TestClient, sid: &str) {
    let t = std::time::Instant::now();
    loop {
        let s: SessionSummary = c.call(td, methods::SESSION_GET, json!({ "session_id": sid })).await.unwrap();
        if s.status == SessionStatus::Idle {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "stuck in {:?}", s.status);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn mock_agent_is_hidden_unless_configured_and_runs_the_happy_path() {
    let td = TestDaemon::start_mock(&[], false).await;
    let mut c = td.client("a");
    let agents: AgentListResult = c.call(&td, methods::AGENT_LIST, json!({})).await.unwrap();
    let ids: Vec<&str> = agents.agents.iter().map(|a| a.id.as_str()).collect();
    // 内置的真实 agent 没有配置段也列出（这里被测试夹具关掉了，所以是 disabled，不会去探测机器上真装着的 CLI）
    assert!(ids.contains(&"claude-code") && ids.contains(&"mock"), "{ids:?}");
    for a in agents.agents.iter().filter(|a| a.id != "mock") {
        assert_eq!(a.status, AgentStatus::Disabled, "{}", a.id);
    }
    let mock = agents.agents.iter().find(|a| a.id == "mock").unwrap();
    assert_eq!(mock.status, AgentStatus::Ready);
    assert_eq!(mock.models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["mock-large", "mock-small"]);

    let s: SessionSummary = c
        .call(&td, methods::SESSION_CREATE, json!({ "agent": "mock", "prompt": "run permission", "message_id": "m1" }))
        .await
        .unwrap();
    assert_eq!(s.settings.model.as_deref(), Some("mock-large"));
    assert_eq!(s.settings.thinking.as_deref(), Some("high"));
    assert_eq!(s.settings.mode.as_deref(), Some("ask"));
    // 权限请求（tool，三个 option）
    let t = std::time::Instant::now();
    let req = loop {
        let l: PermissionListResult = c.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            break r;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(req.kind, PermissionKind::Tool);
    assert_eq!(req.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["allow", "allow_session", "deny"]);
    let _: Empty = c
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    let h: SessionHistoryResult =
        c.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "limit": 1000 })).await.unwrap();
    let text: String = h
        .events
        .iter()
        .filter_map(|e| if let Event::TextDelta { text } = &e.event { Some(text.as_str()) } else { None })
        .collect();
    assert!(text.contains("[model=mock-large thinking=high mode=ask]"), "{text}");
    assert!(h.events.iter().any(|e| matches!(e.event, Event::ToolResult { .. })));
    assert!(h.events.iter().any(|e| matches!(&e.event, Event::ThinkingDelta { .. })));
    let done = h
        .events
        .iter()
        .find_map(|e| {
            if let Event::TurnDone { turn_id, usage, .. } = &e.event {
                Some((*turn_id, usage.clone()))
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(done.0, 1, "the daemon's turn id replaces the adapter's own counter");
    assert!(done.1.unwrap().input_tokens.unwrap() > 0);
    let after: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(after.usage.turns, 1);
    assert!(after.usage.output_tokens > 0);

    // 换模型 / 思考 / 模式，各一次（turn 之外）
    let r: SessionSetSettingsResult = c
        .call(&td, methods::SESSION_SET_SETTINGS, json!({ "session_id": s.id, "settings": { "model": "mock-small" } }))
        .await
        .unwrap();
    assert_eq!(r.settings.thinking, None, "mock-small has no thinking; falls back");
    let r: SessionSetSettingsResult = c
        .call(
            &td,
            methods::SESSION_SET_SETTINGS,
            json!({ "session_id": s.id, "settings": { "model": "mock-large", "thinking": "low", "mode": "plan" } }),
        )
        .await
        .unwrap();
    assert_eq!((r.settings.thinking.as_deref(), r.settings.mode.as_deref()), (Some("low"), Some("plan")));
    let _: SessionSendResult = c
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m2", "content": "hello again" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    let h: SessionHistoryResult = c
        .call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "limit": 1000, "after_seq": after.last_seq }))
        .await
        .unwrap();
    let text: String = h
        .events
        .iter()
        .filter_map(|e| if let Event::TextDelta { text } = &e.event { Some(text.as_str()) } else { None })
        .collect();
    assert!(text.contains("[model=mock-large thinking=low mode=plan]"), "{text}");
    td.shutdown().await;
}

#[tokio::test]
async fn mock_cancel_error_question_plan_and_resume_after_restart() {
    let mut td = TestDaemon::start_mock(&[("sessions.idle_timeout", "\"1s\"")], false).await;
    let mut c = td.client("a");
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({ "agent": "mock" })).await.unwrap();
    let send = |text: &'static str, id: &'static str| json!({ "session_id": s.id, "message_id": id, "content": text });
    // slow + cancel
    let _: SessionSendResult = c.call(&td, methods::SESSION_SEND, send("slow thing", "a")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _: Empty = c.call(&td, methods::SESSION_CANCEL, json!({ "session_id": s.id })).await.unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    // error
    let _: SessionSendResult = c.call(&td, methods::SESSION_SEND, send("error please", "b")).await.unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    // question: answers only
    let _: SessionSendResult = c.call(&td, methods::SESSION_SEND, send("a question", "c")).await.unwrap();
    let t = std::time::Instant::now();
    let req = loop {
        let l: PermissionListResult = c.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            break r;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(req.kind, PermissionKind::Question);
    let _: Empty = c
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "answers": { "q1": "b" } }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    // plan 批准
    let _: SessionSendResult = c.call(&td, methods::SESSION_SEND, send("make a plan", "d")).await.unwrap();
    let t = std::time::Instant::now();
    let req = loop {
        let l: PermissionListResult = c.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            break r;
        }
        assert!(t.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(req.kind, PermissionKind::Plan);
    let _: Empty = c
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    let h: SessionHistoryResult =
        c.call(&td, methods::SESSION_HISTORY, json!({ "session_id": s.id, "limit": 1000 })).await.unwrap();
    let k = |f: &dyn Fn(&Event) -> bool| h.events.iter().filter(|e| f(&e.event)).count();
    assert_eq!(k(&|e| matches!(e, Event::TurnDone { reason: TurnDoneReason::Cancelled, .. })), 1);
    assert_eq!(k(&|e| matches!(e, Event::TurnDone { reason: TurnDoneReason::Error, .. })), 1);
    assert_eq!(k(&|e| matches!(e, Event::Plan { .. })), 1);
    assert!(h
        .events
        .iter()
        .any(|e| matches!(&e.event, Event::TextDelta { text } if text.contains("You answered: q1=b"))));
    let turns: Vec<u64> = h
        .events
        .iter()
        .filter_map(|e| if let Event::TurnDone { turn_id, .. } = &e.event { Some(*turn_id) } else { None })
        .collect();
    assert_eq!(turns, vec![1, 2, 3, 4]);
    // idle 超时关进程后 resume，agent_session_id 不变
    let id_before = c
        .call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": s.id }))
        .await
        .unwrap()
        .agent_session_id;
    assert!(id_before.as_deref().is_some_and(|i| i.starts_with("mock-")));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(td.ctx.sessions.counts().await.1, 0, "idle process was shut down");
    let _: SessionSendResult = c.call(&td, methods::SESSION_SEND, send("hello after idle", "e")).await.unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    // daemon 重启后历史还在，继续聊
    td.restart().await;
    let mut c = td.client("a");
    let after: SessionSummary = c.call(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap();
    assert_eq!(after.status, SessionStatus::Idle);
    assert_eq!(after.agent_session_id, id_before);
    let _: SessionSendResult = c
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "f", "content": "still here" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    td.shutdown().await;
}

#[tokio::test]
async fn mock_is_not_listed_without_config_and_env_alias_enables_it() {
    use anycon_daemon::config::{Config, Paths};
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("home"));
    anycon_daemon::setup::init(
        &paths,
        &anycon_daemon::setup::InitOptions {
            workspace: Some(dir.path().join("ws").to_string_lossy().into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let cfg = Config::load(&paths).unwrap();
    assert!(cfg.adapter_config("mock").is_none(), "an ordinary install has no mock section");
    assert!(cfg.adapter_config("claude-code").is_some());
}

/// 关联一个临时目录（ADR-33）：真正的 mock adapter 的会话 cwd 就是那个目录（权限请求里的 `cwd` 是 spawn 的 cwd）。
#[tokio::test]
async fn a_mock_session_on_a_linked_directory_runs_in_that_directory() {
    let td = TestDaemon::start_mock(&[], false).await;
    let machine = std::fs::canonicalize(td.dir.path()).unwrap().join("machine");
    std::fs::create_dir_all(machine.join("proj")).unwrap();
    let roots = serde_json::to_string(&[machine.to_string_lossy()]).unwrap();
    anycon_daemon::config::set_value(&td.paths, "workspace.link_roots", &roots).unwrap();
    td.ctx.config.reload().unwrap();
    let mut c = td.client("a");
    let p: Project =
        c.call(&td, methods::WORKSPACE_LINK, json!({ "path": machine.join("proj"), "name": "linked" })).await.unwrap();
    assert_eq!(p.kind, ProjectKind::Linked);
    let s: SessionSummary = c
        .call(
            &td,
            methods::SESSION_CREATE,
            json!({ "agent": "mock", "project": "linked", "prompt": "run permission", "message_id": "m1" }),
        )
        .await
        .unwrap();
    let t = std::time::Instant::now();
    let req = loop {
        let l: PermissionListResult = c.call(&td, methods::PERMISSION_LIST, json!({})).await.unwrap();
        if let Some(r) = l.requests.into_iter().next() {
            break r;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "no permission request");
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let cwd = req.detail.tool.as_ref().and_then(|t| t.cwd.clone());
    assert_eq!(cwd.as_deref(), Some(machine.join("proj").to_string_lossy().as_ref()));
    let _: Empty = c
        .call(&td, methods::PERMISSION_RESPOND, json!({ "request_id": req.request_id, "option_id": "allow" }))
        .await
        .unwrap();
    wait_idle(&td, &mut c, &s.id).await;
    td.shutdown().await;
}
