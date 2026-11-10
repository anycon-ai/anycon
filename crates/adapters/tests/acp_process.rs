//! ACP 进程级测试：真的拉起假 ACP agent（`tests/bin/fake-acp-agent`），驱动 `AcpAdapter::spawn` 出来的 `AgentHandle`，
//! 检查握手、请求参数、事件、权限答复、取消、设置、一个进程服务多个会话、续接、崩溃、关闭。
//!
//! 假 CLI（`tests/bin/fake-*`）是带 shebang 的 python 脚本，Windows 上不能直接执行：这个文件只在 unix 上跑。
#![cfg(unix)]

mod common;

use std::time::Duration;

use anycon_agent_api::{Adapter, AdapterError, AgentEvent, AgentHandle, PermissionDecision, ResumeHandle};
use anycon_protocol::{
    AttachmentRef, Event, FileChangeKind, PermissionKind, SessionSettings, ToolCategory, TurnDoneReason,
};
use common::*;
use serde_json::{json, Value};

fn decide(option: &str) -> PermissionDecision {
    PermissionDecision { option_id: Some(option.to_string()), ..Default::default() }
}

async fn started(h: &mut Box<dyn AgentHandle>) -> ResumeHandle {
    match next_event(h).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("expected Started, got {other:?}"),
    }
}

/// 发一条消息，一路收到 `TurnDone`。
async fn run_turn(h: &mut Box<dyn AgentHandle>, text: &str) -> Vec<AgentEvent> {
    h.send(input(text)).await.expect("send");
    collect_until(h, is_turn_done).await
}

fn events_of(evs: &[AgentEvent]) -> Vec<Event> {
    evs.iter()
        .filter_map(|e| match e {
            AgentEvent::Event(ev) => Some(ev.clone()),
            _ => None,
        })
        .collect()
}

fn turn_done(evs: &[AgentEvent]) -> (TurnDoneReason, Option<anycon_protocol::Usage>) {
    match evs.last().expect("events") {
        AgentEvent::Event(Event::TurnDone { reason, usage, .. }) => (*reason, usage.clone()),
        other => panic!("expected TurnDone, got {other:?}"),
    }
}

fn permission_of(evs: &[AgentEvent]) -> anycon_agent_api::PendingPermission {
    evs.iter()
        .find_map(|e| match e {
            AgentEvent::Permission(p) => Some(p.clone()),
            _ => None,
        })
        .expect("a permission request")
}

fn error_codes(evs: &[AgentEvent]) -> Vec<String> {
    events_of(evs)
        .into_iter()
        .filter_map(|e| match e {
            Event::Error { code, .. } => Some(code),
            _ => None,
        })
        .collect()
}

fn settings(f: impl FnOnce(&mut SessionSettings)) -> SessionSettings {
    let mut s = SessionSettings::default();
    f(&mut s);
    s
}

fn tool_result_of(evs: &[AgentEvent]) -> (Value, bool) {
    events_of(evs)
        .into_iter()
        .find_map(|e| match e {
            Event::ToolResult { output, is_error, .. } => Some((output, is_error)),
            _ => None,
        })
        .expect("a tool_result")
}

/// 没有更多事件（等一小会儿）。
async fn assert_quiet(h: &mut Box<dyn AgentHandle>) {
    let r = tokio::time::timeout(Duration::from_millis(400), h.events().recv()).await;
    assert!(r.is_err(), "unexpected event: {r:?}");
}

// ---------------------------------------------------------------- 握手与参数

#[tokio::test]
async fn new_session_handshake_settings_and_started() {
    let h = AcpHarness::new();
    let s = settings(|s| {
        s.model = Some("smart".into());
        s.thinking = Some("medium".into());
        s.mode = Some("code".into());
        s.options.insert("brave".into(), json!(true));
        s.options.insert("ctx".into(), json!("1m"));
    });
    let mut handle = h.spawn(s, None).await;
    let resume = started(&mut handle).await;
    assert!(resume.agent_session_id.starts_with("fake-"), "{resume:?}");
    assert_eq!(
        resume.launch_args,
        json!({"model": "smart", "thinking": "medium", "mode": "code", "options": {"brave": true, "ctx": "1m"}})
    );
    assert_eq!(handle.resume_handle().unwrap(), resume);
    assert!(handle.pid().is_some());

    // 进程：参数、cwd（中性目录，不是会话目录）
    let launches = h.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0]["args"], json!(["acp"]));
    assert!(!same_path(std::path::Path::new(launches[0]["cwd"].as_str().unwrap()), &h.project), "{}", launches[0]);

    // 请求顺序和参数
    assert_eq!(
        h.methods(),
        vec![
            "initialize",
            "session/new",
            "session/set_config_option", // model
            "session/set_config_option", // effort（换成 smart 之后 agent 把等级改成了 high，要的是 medium）
            "session/set_config_option", // mode
            "session/set_config_option", // brave
            "session/set_config_option", // ctx
        ]
    );
    let init = &h.requests("initialize")[0]["params"];
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["clientInfo"]["name"], "anycon");
    // 声明的 client 能力：不声明 fs / terminal，声明 elicitation 和布尔选项
    let caps = &init["clientCapabilities"];
    assert_eq!(caps["fs"], json!({"readTextFile": false, "writeTextFile": false}));
    assert_eq!(caps["terminal"], false);
    assert_eq!(caps["elicitation"], json!({"form": {}, "url": {}}));
    assert_eq!(caps["session"]["configOptions"]["boolean"], json!({}));
    assert!(caps.get("auth").is_none());
    let new = &h.requests("session/new")[0]["params"];
    assert!(same_path(std::path::Path::new(new["cwd"].as_str().unwrap()), &h.project));
    assert_eq!(new["mcpServers"], json!([]));
    let sets = h.requests("session/set_config_option");
    let sid = resume.agent_session_id.as_str();
    assert_eq!(sets[0]["params"], json!({"sessionId": sid, "configId": "model", "value": "smart"}));
    assert_eq!(sets[1]["params"], json!({"sessionId": sid, "configId": "effort", "value": "medium"}));
    assert_eq!(sets[2]["params"], json!({"sessionId": sid, "configId": "mode", "value": "code"}));
    assert_eq!(sets[3]["params"], json!({"sessionId": sid, "configId": "brave", "type": "boolean", "value": true}));
    assert_eq!(sets[4]["params"], json!({"sessionId": sid, "configId": "ctx", "value": "1m"}));
    assert_quiet(&mut handle).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_equal_to_the_agents_values_send_no_requests() {
    let h = AcpHarness::new();
    // 探测看到的默认值：fast / medium / ask
    let s = settings(|s| {
        s.model = Some("fast".into());
        s.thinking = Some("medium".into());
        s.mode = Some("ask".into());
        s.options.insert("brave".into(), json!(false));
    });
    let mut handle = h.spawn(s, None).await;
    started(&mut handle).await;
    assert_eq!(h.methods(), vec!["initialize", "session/new"]);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn legacy_modes_are_set_with_session_set_mode() {
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "legacy");
    let info = h.info().await;
    assert_eq!(info.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["ask", "code"]);
    let mut handle = h.spawn(settings(|s| s.mode = Some("code".into())), None).await;
    let resume = started(&mut handle).await;
    assert_eq!(
        h.requests("session/set_mode")[0]["params"],
        json!({"sessionId": resume.agent_session_id, "modeId": "code"})
    );
    assert!(h.requests("session/set_config_option").is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_the_agent_cannot_take_are_reported_but_do_not_fail_the_session() {
    // 旧式的配置没有思考等级选择器
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "legacy");
    let mut handle = h.spawn(settings(|s| s.thinking = Some("high".into())), None).await;
    started(&mut handle).await;
    match next_event(&mut handle).await {
        AgentEvent::Event(Event::Error { code, message, recoverable, .. }) => {
            assert_eq!(code, "agent_setting_failed");
            assert!(recoverable && message.contains("thinking"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // 会话照常能用
    let evs = run_turn(&mut handle, "hello").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Completed);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_settings_are_rejected_by_validation() {
    let h = AcpHarness::new();
    let info = h.info().await;
    let a = &h.adapter;
    let bad = |f: &dyn Fn(&mut SessionSettings)| {
        let mut s = SessionSettings::default();
        f(&mut s);
        match a.validate_settings(&info, &s) {
            Err(AdapterError::InvalidSettings { field, .. }) => field,
            other => panic!("expected invalid settings, got {other:?}"),
        }
    };
    assert_eq!(
        bad(&|s| {
            s.env.insert("A".into(), "b".into());
        }),
        "env"
    );
    assert_eq!(bad(&|s| s.system_prompt_append = Some("x".into())), "system_prompt_append");
    assert_eq!(bad(&|s| s.max_turns = Some(3)), "max_turns");
    assert_eq!(bad(&|s| s.max_budget_usd = Some(1.0)), "max_budget_usd");
    assert_eq!(bad(&|s| s.model = Some("nope".into())), "model");
    assert_eq!(bad(&|s| s.mode = Some("nope".into())), "mode");
    assert_eq!(
        bad(&|s| {
            s.options.insert("nope".into(), json!(1));
        }),
        "options.nope"
    );
    // 通过：探测里有的模型 / 等级 / 模式
    let ok = settings(|s| {
        s.model = Some("smart".into());
        s.thinking = Some("medium".into());
        s.mode = Some("plan".into());
    });
    a.validate_settings(&info, &ok).unwrap();
    // spawn 也会拒绝
    let r = h.try_spawn(settings(|s| s.max_turns = Some(2)), None).await;
    assert!(matches!(r, Err(AdapterError::InvalidSettings { .. })));
}

// ---------------------------------------------------------------- turn 与事件

#[tokio::test]
async fn text_thinking_usage_and_title() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "stream and title").await;
    assert_eq!(
        names(&evs),
        vec![
            "TITLE",
            "thinking_delta",
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_done",
            "text_delta",
            "text_done",
            "turn_done"
        ]
    );
    assert_eq!(text_of(&evs), "Hello worldSecond message");
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Title(t) if t == "A better title")));
    let (reason, usage) = turn_done(&evs);
    assert_eq!(reason, TurnDoneReason::Completed);
    let u = usage.expect("usage");
    assert_eq!((u.context_tokens, u.context_window), (Some(1234), Some(200000)));
    assert!((u.cost_usd.unwrap() - 0.05).abs() < 1e-9, "{u:?}");
    assert_eq!(u.model.as_deref(), Some("fast"));
    assert!(!handle.is_busy());
    // 第二个 turn：累计费用 0.10，增量还是 0.05
    let evs = run_turn(&mut handle, "stream").await;
    assert!((turn_done(&evs).1.unwrap().cost_usd.unwrap() - 0.05).abs() < 1e-9);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn tool_calls_are_announced_with_their_input_and_completed() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "tool").await;
    assert_eq!(names(&evs), vec!["tool_call", "tool_result", "text_delta", "text_done", "turn_done"]);
    let ev = events_of(&evs);
    match &ev[0] {
        Event::ToolCall { id, category, title, input, detail, .. } => {
            assert_eq!((id.as_str(), *category, title.as_str()), ("t1", ToolCategory::Shell, "Run ls"));
            assert_eq!(input, &json!({"command": "ls -la"}));
            assert_eq!(detail.as_ref().unwrap().command.as_deref(), Some("ls -la"));
        }
        other => panic!("{other:?}"),
    }
    match &ev[1] {
        Event::ToolResult { output, is_error, detail, .. } => {
            assert_eq!((output, *is_error), (&json!("file-a\nfile-b\n"), false));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
        }
        other => panic!("{other:?}"),
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn edits_carry_a_unified_diff_and_file_changed() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "edit").await;
    assert_eq!(names(&evs), vec!["tool_call", "tool_result", "file_changed", "file_changed", "turn_done"]);
    let ev = events_of(&evs);
    match &ev[1] {
        Event::ToolResult { detail, .. } => {
            let d = detail.as_ref().unwrap().unified_diff.as_deref().unwrap();
            assert!(d.contains("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"), "{d}");
            assert!(d.contains("--- /dev/null\n+++ b/new.txt\n"), "{d}");
        }
        other => panic!("{other:?}"),
    }
    let changed: Vec<(String, FileChangeKind)> = ev
        .iter()
        .filter_map(|e| if let Event::FileChanged { path, kind } = e { Some((path.clone(), *kind)) } else { None })
        .collect();
    let p = h.project.to_string_lossy();
    assert_eq!(
        changed,
        vec![(format!("{p}/a.txt"), FileChangeKind::Modified), (format!("{p}/new.txt"), FileChangeKind::Created)]
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn plans_stop_reasons_and_prompt_errors() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "plan").await;
    match &events_of(&evs)[0] {
        Event::Plan { entries } => assert_eq!(entries.len(), 3),
        other => panic!("{other:?}"),
    }
    // refusal → error（先报一条可恢复的错误）
    let evs = run_turn(&mut handle, "refuse").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Error);
    assert_eq!(error_codes(&evs), vec!["agent_refusal"]);
    // max_tokens → max_turns
    let evs = run_turn(&mut handle, "maxtokens").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::MaxTurns);
    // JSON-RPC 错误
    let evs = run_turn(&mut handle, "error").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Error);
    assert_eq!(error_codes(&evs), vec!["agent_error"]);
    // 认证错误
    let evs = run_turn(&mut handle, "authfail").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Error);
    assert_eq!(error_codes(&evs), vec!["auth_failed"]);
    // 出错之后会话照常能用
    let evs = run_turn(&mut handle, "hello").await;
    assert_eq!(text_of(&evs), "echo: hello");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_and_a_second_turn_while_busy_are_refused() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    assert!(matches!(handle.send(interject("x")).await, Err(AdapterError::Unsupported(_))));
    handle.send(input("hold")).await.unwrap();
    assert!(handle.is_busy());
    assert!(matches!(handle.send(input("again")).await, Err(AdapterError::Unsupported(_))));
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Cancelled);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn requests_the_client_did_not_advertise_get_method_not_found() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "fsread").await;
    assert_eq!(text_of(&evs), "fs: -32601");
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 权限

#[tokio::test]
async fn permission_options_are_passed_through_and_the_option_id_comes_back() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("permission")).await.unwrap();
    let evs = collect_until(&mut handle, is_permission).await;
    assert_eq!(names(&evs), vec!["tool_call", "PERMISSION"]);
    let p = permission_of(&evs);
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.title, "Run `rm -rf build`");
    assert_eq!(
        p.options.iter().map(|o| (o.id.as_str(), o.label.as_str())).collect::<Vec<_>>(),
        vec![
            ("allow-once", "Allow once"),
            ("allow-always", "Always allow rm"),
            ("reject-once", "Reject"),
            ("reject-always", "Never allow rm")
        ]
    );
    let t = p.detail.tool.as_ref().unwrap();
    assert_eq!(t.category, ToolCategory::Shell);
    assert_eq!(t.detail.as_ref().unwrap().command.as_deref(), Some("rm -rf build"));
    assert!(same_path(std::path::Path::new(t.cwd.as_deref().unwrap()), &h.project));
    assert!(handle.is_busy());
    handle.respond_permission(&p.request_id, decide("allow-always")).await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(names(&evs), vec!["tool_result", "text_delta", "text_done", "turn_done"]);
    assert_eq!(tool_result_of(&evs), (json!("ran after allow-always"), false));
    assert_eq!(text_of(&evs), "permission answered: allow-always");
    let outcome = h.log_entries().into_iter().find(|e| e["kind"] == "permission_outcome").unwrap();
    assert_eq!(outcome["outcome"], json!({"outcome": "selected", "optionId": "allow-always"}));
    // 重复答复是幂等的
    handle.respond_permission(&p.request_id, decide("allow-always")).await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejecting_fails_the_tool_and_no_option_picks_a_reject_option() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("permission")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    handle.respond_permission(&p.request_id, decide("reject-once")).await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(tool_result_of(&evs), (json!("refused: reject-once"), true));
    // 没带选项的答复（超时之类的自动拒绝）：挑 agent 给的 reject_once
    handle.send(input("permission")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    handle.respond_permission(&p.request_id, PermissionDecision::default()).await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(tool_result_of(&evs), (json!("refused: reject-once"), true));
    // 不在选项里的答复是错误，请求仍然未决
    handle.send(input("permission")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    assert!(matches!(handle.respond_permission(&p.request_id, decide("nope")).await, Err(AdapterError::Protocol(_))));
    handle.respond_permission(&p.request_id, decide("allow-once")).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_answers_pending_requests_with_cancelled_and_ends_the_turn() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    let resume = started(&mut handle).await;
    handle.send(input("permission")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Cancelled);
    // agent 看到的：cancelled 的答复，和一条 session/cancel 通知
    let outcome = h.log_entries().into_iter().find(|e| e["kind"] == "permission_outcome").unwrap();
    assert_eq!(outcome["outcome"], json!({"outcome": "cancelled"}));
    assert_eq!(h.requests("session/cancel")[0]["params"], json!({"sessionId": resume.agent_session_id}));
    // daemon 随后按拒绝答复同一个请求：已经答复过，什么也不发生
    handle.respond_permission(&p.request_id, decide("reject-once")).await.unwrap();
    // 取消之后会话还能继续
    let evs = run_turn(&mut handle, "hello").await;
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Completed);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_ends_a_running_turn_and_closes_open_tool_calls() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    let resume = started(&mut handle).await;
    handle.send(input("hold")).await.unwrap();
    collect_until(&mut handle, |e| matches!(e, AgentEvent::Event(Event::ToolCall { .. }))).await;
    // 没有 turn 时取消是空操作；有 turn 时发通知，不杀进程
    handle.cancel().await.unwrap();
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(names(&evs), vec!["tool_result", "turn_done"]);
    assert_eq!(tool_result_of(&evs), (json!("Cancelled before it finished"), true));
    assert_eq!(turn_done(&evs).0, TurnDoneReason::Cancelled);
    assert!(h.requests("session/cancel").iter().all(|m| m["params"]["sessionId"] == resume.agent_session_id.as_str()));
    let pid = handle.pid().unwrap() as u64;
    assert!(pid_alive(pid), "cancel does not kill the process");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn elicitation_forms_become_questions_and_answers_are_typed() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("elicit")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!(p.kind, PermissionKind::Question);
    assert_eq!(p.title, "How should I proceed?");
    let mut ids: Vec<&str> = p.detail.questions.iter().map(|q| q.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, vec!["count", "note", "strategy"]);
    let answers = json!({"strategy": "fast", "count": "3"}).as_object().cloned().unwrap();
    handle
        .respond_permission(
            &p.request_id,
            PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), ..Default::default() },
        )
        .await
        .unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    let echoed = text_of(&evs);
    assert_eq!(echoed, r#"elicitation: {"action": "accept", "content": {"count": 3, "strategy": "fast"}}"#);

    // 回答和 schema 对不上：按拒绝答复，并说明原因
    handle.send(input("elicit")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    let bad = json!({"strategy": "fast"}).as_object().cloned().unwrap();
    handle
        .respond_permission(
            &p.request_id,
            PermissionDecision { option_id: Some("accept".into()), answers: Some(bad), ..Default::default() },
        )
        .await
        .unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(error_codes(&evs), vec!["elicitation_invalid_answer"]);
    assert_eq!(text_of(&evs), r#"elicitation: {"action": "decline"}"#);

    // 取消：agent 收到 `cancel`
    handle.send(input("elicit")).await.unwrap();
    collect_until(&mut handle, is_permission).await;
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert!(text_of(&evs).contains(r#""action": "cancel""#), "{}", text_of(&evs));
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 设置（会话中）

#[tokio::test]
async fn set_settings_applies_now_and_only_what_changed() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    let first = started(&mut handle).await;
    let before = h.requests("session/set_config_option").len();
    assert_eq!(before, 0);
    let r = handle.set_settings(&settings(|s| s.mode = Some("code".into()))).await.unwrap();
    assert!(r.applied_now && !r.restart_required);
    let sets = h.requests("session/set_config_option");
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0]["params"], json!({"sessionId": first.agent_session_id, "configId": "mode", "value": "code"}));
    // launch_args 跟着变，补发了一个 Started 让 daemon 落库
    match next_event(&mut handle).await {
        AgentEvent::Started { resume } => assert_eq!(resume.launch_args["mode"], "code"),
        other => panic!("{other:?}"),
    }
    // 同样的设置再来一次：不发请求
    handle.set_settings(&settings(|s| s.mode = Some("code".into()))).await.unwrap();
    assert_eq!(h.requests("session/set_config_option").len(), 1);
    // 换模型：响应里的完整选项列表更新了思考等级的候选，之后可以设新的等级
    handle
        .set_settings(&settings(|s| {
            s.mode = Some("code".into());
            s.model = Some("smart".into());
            s.thinking = Some("max".into());
        }))
        .await
        .unwrap();
    let sets = h.requests("session/set_config_option");
    assert_eq!(sets.len(), 3);
    assert_eq!(sets[1]["params"]["value"], "smart");
    assert_eq!(sets[2]["params"], json!({"sessionId": first.agent_session_id, "configId": "effort", "value": "max"}));
    // 不合法的设置：错误，不改状态
    let e = handle.set_settings(&settings(|s| s.mode = Some("nope".into()))).await.unwrap_err();
    assert!(matches!(&e, AdapterError::InvalidSettings { field, .. } if field == "mode"), "{e:?}");
    let e = handle
        .set_settings(&settings(|s| {
            s.env.insert("A".into(), "b".into());
        }))
        .await
        .unwrap_err();
    assert!(matches!(&e, AdapterError::InvalidSettings { field, .. } if field == "env"), "{e:?}");
    // 当前模型不提供的等级（smart 的等级是 medium / high / max）：错误
    let e = handle
        .set_settings(&settings(|s| {
            s.model = Some("smart".into());
            s.thinking = Some("low".into());
        }))
        .await
        .unwrap_err();
    assert!(matches!(&e, AdapterError::InvalidSettings { field, .. } if field == "thinking"), "{e:?}");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_agent_changing_its_own_mode_is_not_overwritten_by_the_next_settings_change() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(settings(|s| s.mode = Some("ask".into())), None).await;
    started(&mut handle).await;
    // agent 自己把模式切成了 code（比如批准计划之后）
    let evs = run_turn(&mut handle, "switchmode").await;
    assert_eq!(text_of(&evs), "switched");
    // daemon 记着的模式还是 ask；之后只改 thinking：mode 和上一次**请求**的一样，不发 mode 的请求，agent 的选择不会被冲掉
    handle
        .set_settings(&settings(|s| {
            s.mode = Some("ask".into());
            s.thinking = Some("high".into());
        }))
        .await
        .unwrap();
    let sets = h.requests("session/set_config_option");
    assert_eq!(sets.len(), 1);
    assert_eq!(sets[0]["params"]["configId"], "effort");
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 附件

fn image(name: &str, path: &std::path::Path) -> (AttachmentRef, std::path::PathBuf) {
    (
        AttachmentRef {
            blob_id: "b".into(),
            name: name.into(),
            mime: "image/png".into(),
            bytes: 4,
            width: None,
            height: None,
        },
        path.to_path_buf(),
    )
}

#[tokio::test]
async fn images_go_first_as_image_blocks() {
    let h = AcpHarness::new();
    let info = h.info().await;
    assert_eq!(info.capabilities.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp"]);
    let file = h.dir.path().join("a.png");
    std::fs::write(&file, [0x89, b'P', b'N', b'G']).unwrap();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let (a, p) = image("a.png", &file);
    let mut inp = input("images please");
    inp.attachments = vec![a.clone()];
    inp.attachment_paths = vec![p.clone()];
    handle.send(inp).await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(text_of(&evs), "blocks: image/png,text");
    let prompt = h.requests("session/prompt").last().unwrap()["params"]["prompt"].clone();
    assert_eq!(prompt[0]["type"], "image");
    assert_eq!(prompt[0]["mimeType"], "image/png");
    assert_eq!(prompt[0]["data"], "iVBORw==");
    assert_eq!(prompt[1], json!({"type": "text", "text": "images please"}));
    // 只有图片：不发空文本
    let mut inp = input("");
    inp.attachments = vec![a];
    inp.attachment_paths = vec![p];
    handle.send(inp).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    let prompt = h.requests("session/prompt").last().unwrap()["params"]["prompt"].clone();
    assert_eq!(prompt.as_array().unwrap().len(), 1);
    // 非图片
    let mut inp = input("x");
    let (mut pdf, p) = image("a.pdf", &file);
    pdf.mime = "application/pdf".into();
    inp.attachments = vec![pdf];
    inp.attachment_paths = vec![p];
    assert!(matches!(handle.send(inp).await, Err(AdapterError::Unsupported(_))));
    assert!(!handle.is_busy());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_without_image_support_declares_no_attachments_and_refuses_them() {
    let h = AcpHarness::new().var("FAKE_ACP_CAPS", "resume,close");
    let info = h.info().await;
    assert!(info.capabilities.attachments.is_empty());
    let file = h.dir.path().join("a.png");
    std::fs::write(&file, [1, 2, 3]).unwrap();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let (a, p) = image("a.png", &file);
    let mut inp = input("look");
    inp.attachments = vec![a];
    inp.attachment_paths = vec![p];
    assert!(matches!(handle.send(inp).await, Err(AdapterError::Unsupported(_))));
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 一个进程服务多个会话

#[tokio::test]
async fn one_process_serves_all_sessions_and_exits_with_the_last_one() {
    let h = AcpHarness::new();
    let mut a = h.spawn(SessionSettings::default(), None).await;
    let mut b = h.spawn(SessionSettings::default(), None).await;
    let ra = started(&mut a).await;
    let rb = started(&mut b).await;
    assert_ne!(ra.agent_session_id, rb.agent_session_id);
    assert_eq!(a.pid(), b.pid(), "one process for the agent");
    let pid = a.pid().unwrap() as u64;
    assert_eq!(h.launches().len(), 1, "only one agent process was started");
    assert_eq!(h.requests("session/new").len(), 2);

    // 两个会话各自独立
    a.send(input("hold")).await.unwrap();
    let evs_b = run_turn(&mut b, "hello b").await;
    assert_eq!(text_of(&evs_b), "echo: hello b");
    assert!(a.is_busy() && !b.is_busy());
    a.cancel().await.unwrap();
    let evs_a = collect_until(&mut a, is_turn_done).await;
    assert_eq!(turn_done(&evs_a).0, TurnDoneReason::Cancelled);
    assert!(!text_of(&evs_a).contains("hello b"));

    // 第一个脱离：agent 收到 session/close，进程还在，另一个会话照常用
    a.shutdown().await.unwrap();
    assert_eq!(h.requests("session/close")[0]["params"], json!({"sessionId": ra.agent_session_id}));
    assert!(pid_alive(pid));
    let evs = run_turn(&mut b, "still here").await;
    assert_eq!(text_of(&evs), "echo: still here");
    // 最后一个走了，进程退出
    b.shutdown().await.unwrap();
    assert!(wait_gone(pid).await, "the agent process should exit with its last session");
    // 之后再 spawn 起新进程
    let mut c = h.spawn(SessionSettings::default(), None).await;
    started(&mut c).await;
    assert_ne!(c.pid().unwrap() as u64, pid);
    assert_eq!(h.launches().len(), 2);
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_is_idempotent_and_closes_the_event_stream() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.shutdown().await.unwrap();
    handle.shutdown().await.unwrap();
    // 读完剩下的事件：最后是 Exited，然后通道关闭
    let mut last = None;
    while let Some(e) =
        tokio::time::timeout(Duration::from_secs(5), handle.events().recv()).await.expect("stream should close")
    {
        last = Some(e);
    }
    assert!(matches!(last, Some(AgentEvent::Exited { error: None, .. })), "{last:?}");
}

#[tokio::test]
async fn an_agent_without_session_close_keeps_a_detached_session_for_the_next_resume() {
    let h = AcpHarness::new().var("FAKE_ACP_CAPS", "load,image");
    let mut a = h.spawn(SessionSettings::default(), None).await;
    let mut b = h.spawn(SessionSettings::default(), None).await;
    let ra = started(&mut a).await;
    started(&mut b).await;
    a.shutdown().await.unwrap();
    assert!(h.requests("session/close").is_empty(), "this agent has no session/close");
    // 进程里还有这个会话（b 挂着，进程没退）：续接直接接上，不发 session/load
    let mut a2 = h.spawn(SessionSettings::default(), Some(ra.clone())).await;
    assert_eq!(started(&mut a2).await.agent_session_id, ra.agent_session_id);
    assert!(h.requests("session/load").is_empty());
    let evs = run_turn(&mut a2, "hello again").await;
    assert_eq!(text_of(&evs), "echo: hello again");
    a2.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 续接

#[tokio::test]
async fn resume_uses_session_resume_in_a_new_process() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    let first = started(&mut handle).await;
    run_turn(&mut handle, "stream").await;
    let pid = handle.pid().unwrap() as u64;
    let resume = handle.resume_handle().unwrap();
    handle.shutdown().await.unwrap();
    assert!(wait_gone(pid).await);

    let mut again = h.spawn(settings(|s| s.mode = Some("code".into())), Some(resume)).await;
    let second = started(&mut again).await;
    assert_eq!(second.agent_session_id, first.agent_session_id);
    assert_ne!(again.pid().unwrap() as u64, pid, "a new process");
    let r = &h.requests("session/resume")[0]["params"];
    assert_eq!(r["sessionId"], first.agent_session_id.as_str());
    assert!(same_path(std::path::Path::new(r["cwd"].as_str().unwrap()), &h.project));
    assert_eq!(r["mcpServers"], json!([]));
    assert!(h.requests("session/load").is_empty());
    // 设置照新的完整设置重新应用（模式在新进程里是 ask）
    assert_eq!(h.requests("session/set_config_option").last().unwrap()["params"]["value"], "code");
    // 这是续接的会话：不知道费用基线，第一个 turn 不报费用
    let evs = run_turn(&mut again, "stream").await;
    assert_eq!(turn_done(&evs).1.unwrap().cost_usd, None);
    assert!(error_codes(&evs).is_empty());
    again.shutdown().await.unwrap();
}

#[tokio::test]
async fn resume_falls_back_to_session_load_and_swallows_the_replayed_history() {
    let h = AcpHarness::new().var("FAKE_ACP_CAPS", "load,image");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    let first = started(&mut handle).await;
    run_turn(&mut handle, "remember this").await;
    let resume = handle.resume_handle().unwrap();
    let pid = handle.pid().unwrap() as u64;
    handle.shutdown().await.unwrap();
    assert!(wait_gone(pid).await);

    let mut again = h.spawn(SessionSettings::default(), Some(resume)).await;
    assert_eq!(started(&mut again).await.agent_session_id, first.agent_session_id);
    assert_eq!(h.requests("session/load").len(), 1);
    assert!(h.requests("session/resume").is_empty());
    // agent 重放了整段历史（user / agent 的消息块），一条都不能出现在事件流里
    assert_quiet(&mut again).await;
    let evs = run_turn(&mut again, "next").await;
    assert_eq!(text_of(&evs), "echo: next");
    again.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_session_that_is_gone_is_replaced_and_the_user_is_told() {
    let h = AcpHarness::new().var("FAKE_ACP_RESUME_FAIL", "notfound");
    let old = ResumeHandle { agent_session_id: "fake-old-1".into(), launch_args: Value::Null };
    let mut handle = h.spawn(SessionSettings::default(), Some(old)).await;
    let s = started(&mut handle).await;
    assert_ne!(s.agent_session_id, "fake-old-1");
    match next_event(&mut handle).await {
        AgentEvent::Event(Event::Error { code, message, recoverable, .. }) => {
            assert_eq!(code, "agent_context_reset");
            assert!(recoverable && message.contains("Resource not found"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(h.requests("session/new").len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_that_can_neither_resume_nor_load_starts_a_new_session_with_a_notice() {
    let h = AcpHarness::new().var("FAKE_ACP_CAPS", "image");
    let old = ResumeHandle { agent_session_id: "fake-old-1".into(), launch_args: Value::Null };
    let info = h.info().await;
    assert!(!info.capabilities.resume);
    let mut handle = h.spawn(SessionSettings::default(), Some(old)).await;
    started(&mut handle).await;
    assert!(
        matches!(next_event(&mut handle).await, AgentEvent::Event(Event::Error { code, .. }) if code == "agent_context_reset")
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn other_resume_errors_fail_the_spawn() {
    let h = AcpHarness::new().var("FAKE_ACP_RESUME_FAIL", "other");
    let old = ResumeHandle { agent_session_id: "fake-old-1".into(), launch_args: Value::Null };
    let e = h.try_spawn(SessionSettings::default(), Some(old)).await.err().expect("spawn should fail");
    assert!(matches!(&e, AdapterError::Protocol(m) if m.contains("disk on fire")), "{e:?}");
}

// ---------------------------------------------------------------- 失败

#[tokio::test]
async fn a_crash_ends_every_session_of_the_process_and_the_next_spawn_starts_a_new_one() {
    let h = AcpHarness::new();
    let mut a = h.spawn(SessionSettings::default(), None).await;
    let mut b = h.spawn(SessionSettings::default(), None).await;
    started(&mut a).await;
    started(&mut b).await;
    let pid = a.pid().unwrap() as u64;
    a.send(input("crash now")).await.unwrap();
    // 中途崩溃的会话：先收尾还开着的文字，再报 agent_exited，最后 Exited（带错误）；不补 turn_done
    let evs = collect_until(&mut a, is_exited).await;
    assert_eq!(names(&evs), vec!["text_delta", "text_done", "error", "EXITED"]);
    assert_eq!(error_codes(&evs), vec!["agent_exited"]);
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, Some(3));
            assert!(error.as_deref().unwrap().contains("exited unexpectedly"), "{error:?}");
        }
        other => panic!("{other:?}"),
    }
    // 空闲的会话同样得到 Exited（daemon 据此标成 error，下一条消息走 resume）
    let evs = collect_until(&mut b, is_exited).await;
    assert_eq!(names(&evs), vec!["EXITED"]);
    assert!(wait_gone(pid).await);
    assert!(!a.is_busy());
    assert!(matches!(a.send(input("x")).await, Err(AdapterError::Exited(_))));
    // 句柄收尾（daemon 在 Exited 之后还会调 shutdown）
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
    // 之后的 spawn 起新进程
    let mut c = h.spawn(SessionSettings::default(), None).await;
    started(&mut c).await;
    assert_ne!(c.pid().unwrap() as u64, pid);
    let evs = run_turn(&mut c, "hello").await;
    assert_eq!(text_of(&evs), "echo: hello");
    c.shutdown().await.unwrap();
}

#[tokio::test]
async fn login_required_fails_the_spawn_with_the_login_methods() {
    let h = AcpHarness::new().var("FAKE_ACP_AUTH", "required");
    let e = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn should fail");
    match e {
        AdapterError::NotAuthenticated(m) => {
            assert!(m.contains("Log in with Fake") && m.contains("not logged in"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    // 失败的 spawn 不留进程：池里没有挂着的句柄，进程已经被关掉
    let pid = h.launches().last().and_then(|l| l["pid"].as_u64()).expect("a launch");
    assert!(wait_gone(pid).await);
}

#[tokio::test]
async fn startup_failures_are_reported_with_the_agents_own_words() {
    let h = AcpHarness::new().var("FAKE_ACP_INIT", "fail");
    let e = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn should fail");
    assert!(matches!(&e, AdapterError::Spawn(m) if m.contains("missing config")), "{e:?}");
    let h = AcpHarness::new().var("FAKE_ACP_INIT", "v2");
    let e = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn should fail");
    assert!(matches!(&e, AdapterError::UnsupportedVersion(m) if m.contains("v2")), "{e:?}");
}

#[tokio::test]
async fn a_disabled_agent_or_a_missing_command_cannot_be_spawned() {
    let mut h = AcpHarness::new();
    h.config.enabled = false;
    let e = h.try_spawn(SessionSettings::default(), None).await.err().unwrap();
    assert!(matches!(e, AdapterError::Spawn(m) if m.contains("disabled")));
    let mut h = AcpHarness::new();
    h.config.command = None;
    let e = h.try_spawn(SessionSettings::default(), None).await.err().unwrap();
    assert!(matches!(&e, AdapterError::Spawn(m) if m.contains("agents.acp.fakeacp.command")), "{e:?}");
    // 工作目录不存在
    let h = AcpHarness::new();
    let mut req = h.request(SessionSettings::default(), None).await;
    req.cwd = h.dir.path().join("missing");
    let e = h.adapter.spawn(req).await.err().unwrap();
    assert!(matches!(e, AdapterError::Spawn(m) if m.contains("working directory")));
}

#[tokio::test]
async fn dropping_a_handle_without_shutdown_does_not_leave_the_process() {
    let h = AcpHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let pid = handle.pid().unwrap() as u64;
    drop(handle);
    assert!(wait_gone(pid).await);
}

#[tokio::test]
async fn a_changed_launch_environment_starts_another_process() {
    let h = AcpHarness::new();
    let mut a = h.spawn(SessionSettings::default(), None).await;
    started(&mut a).await;
    let mut req = h.request(SessionSettings::default(), None).await;
    let mut env = (*req.env).clone();
    env.insert("SOMETHING_NEW".into(), "1".into());
    req.env = std::sync::Arc::new(env);
    let mut b = h.adapter.spawn(req).await.unwrap();
    started(&mut b).await;
    assert_ne!(a.pid(), b.pid(), "the environment is part of what a process was started with");
    a.shutdown().await.unwrap();
    b.shutdown().await.unwrap();
}
