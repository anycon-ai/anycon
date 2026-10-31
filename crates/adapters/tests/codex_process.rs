//! Codex 进程级测试：真的拉起假 `codex app-server`（`tests/bin/fake-codex`，回放 `tests/fixtures/codex` 里的录制），
//! 驱动 `CodexAdapter::spawn` 出来的 `AgentHandle`，检查握手、请求参数、事件、权限答复、取消 / 转向、设置、续接、关闭。
//!
//! 假 CLI（`tests/bin/fake-*`）是带 shebang 的 python 脚本，Windows 上不能直接执行：这个文件只在 unix 上跑。
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use anycon_adapters::CodexAdapter;
use anycon_agent_api::{
    Adapter, AdapterError, AgentEvent, AgentHandle, PermissionDecision, ResumeHandle, SettingsApply, UserInput,
};
use anycon_protocol::{
    AttachmentRef, Event, FileChangeKind, PermissionKind, SessionSettings, ToolCategory, TurnDoneReason,
};
use common::*;
use serde_json::{json, Value};

fn decide(option: &str) -> PermissionDecision {
    PermissionDecision { option_id: Some(option.to_string()), ..Default::default() }
}

async fn started_event(h: &mut Box<dyn AgentHandle>) -> ResumeHandle {
    started(h).await
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

fn turn_done(evs: &[AgentEvent]) -> (u64, TurnDoneReason, Option<anycon_protocol::Usage>) {
    match evs.last().expect("events") {
        AgentEvent::Event(Event::TurnDone { turn_id, reason, usage }) => (*turn_id, *reason, usage.clone()),
        other => panic!("expected TurnDone, got {other:?}"),
    }
}

fn events_of(evs: &[AgentEvent]) -> Vec<Event> {
    evs.iter()
        .filter_map(|e| match e {
            AgentEvent::Event(ev) => Some(ev.clone()),
            _ => None,
        })
        .collect()
}

fn permission_of(evs: &[AgentEvent]) -> anycon_agent_api::PendingPermission {
    evs.iter()
        .find_map(|e| match e {
            AgentEvent::Permission(p) => Some(p.clone()),
            _ => None,
        })
        .expect("a permission request")
}

fn is_text_delta(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::Event(Event::TextDelta { .. }))
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

// ---------------------------------------------------------------- 握手与参数

#[tokio::test]
async fn new_thread_handshake_and_started() {
    let h = CodexHarness::new();
    let s = settings(|s| {
        s.model = Some("gpt-6-sol".into());
        s.thinking = Some("high".into());
        s.mode = Some("read_only".into());
        s.system_prompt_append = Some("Be terse.".into());
    });
    let mut handle = h.spawn(s, None).await;
    let resume = started(&mut handle).await;
    assert_eq!(resume.agent_session_id, "thr-0001");
    assert_eq!(resume.launch_args["model"], "gpt-6-sol");
    assert_eq!(resume.launch_args["thinking"], "high");
    assert_eq!(resume.launch_args["mode"], "read_only");
    assert_eq!(resume.launch_args["system_prompt_append"], "Be terse.");
    assert_eq!(resume.launch_args["instructions_applied"], "Be terse.");
    assert_eq!(handle.resume_handle().unwrap(), resume);

    let a = h.argv_entry();
    assert_eq!(a["args"], json!(["app-server"]));
    assert!(same_path(std::path::Path::new(a["cwd"].as_str().unwrap()), &h.project), "{a}");

    let methods: Vec<String> =
        h.stdin_messages().iter().filter_map(|m| m["method"].as_str().map(str::to_string)).collect();
    assert_eq!(methods, vec!["initialize", "initialized", "thread/start"]);
    let init = &h.requests("initialize")[0]["params"];
    assert_eq!(init["clientInfo"]["name"], "anycon");
    assert_eq!(init["capabilities"]["experimentalApi"], false);
    let p = &h.requests("thread/start")[0]["params"];
    assert_eq!(p["model"], "gpt-6-sol");
    assert_eq!(p["approvalPolicy"], "on-request");
    assert_eq!(p["approvalsReviewer"], "user");
    assert_eq!(p["sandbox"], "read-only");
    assert_eq!(p["developerInstructions"], "Be terse.");
    assert!(same_path(std::path::Path::new(p["cwd"].as_str().unwrap()), &h.project));
    // 没有设置 fast_mode：不指定服务档位
    assert!(p.get("serviceTier").is_none());
    assert!(!handle.is_busy());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn defaults_come_from_the_probe_and_are_always_explicit() {
    let h = CodexHarness::new().script("two_turns.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let p = &h.requests("thread/start")[0]["params"];
    // 默认模型 / 模式来自探测（真实 model/list 的 isDefault、四档里标 default 的）
    assert_eq!(p["model"], "gpt-6-astra");
    assert_eq!(p["sandbox"], "workspace-write");
    assert_eq!(p["approvalPolicy"], "on-request");
    run_turn(&mut handle, "hi").await;
    let t = &h.requests("turn/start")[0]["params"];
    assert_eq!(t["threadId"], "thr-0001");
    assert_eq!(t["input"], json!([{"type": "text", "text": "hi"}]));
    assert_eq!(t["model"], "gpt-6-astra");
    assert_eq!(t["effort"], "low"); // 该模型的默认思考等级
    assert_eq!(t["approvalPolicy"], "on-request");
    assert_eq!(t["approvalsReviewer"], "user");
    // 沙箱与线程创建时相同：不传 sandboxPolicy，保留用户 config.toml 里的沙箱细节
    assert!(t.get("sandboxPolicy").is_none());
    assert!(t.get("serviceTier").is_none() && t.get("summary").is_none());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn env_reaches_the_server_process_unchanged() {
    let h = CodexHarness::new().var("ANYCON_TEST_VAR", "per-session");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    assert_eq!(h.argv_entry()["env"]["ANYCON_TEST_VAR"], "per-session");
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 一个 turn

#[tokio::test]
async fn text_and_thinking_turn_with_usage() {
    let h = CodexHarness::new().script("real_text_reasoning.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "hi").await;
    assert_eq!(
        names(&evs),
        vec![
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_done",
            "turn_done"
        ]
    );
    assert_eq!(text_of(&evs).trim(), "Hello from the fake model");
    let (turn_id, reason, usage) = turn_done(&evs);
    assert_eq!((turn_id, reason), (1, TurnDoneReason::Completed));
    let u = usage.unwrap();
    // 120 个输入里 40 个是缓存命中
    assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (Some(80), Some(40), Some(30)));
    assert_eq!((u.cost_usd, u.cache_write_tokens), (None, None));
    assert_eq!(u.model.as_deref(), Some("gpt-6-astra"));
    assert!(!handle.is_busy());
    // 同一个进程里还能再来一轮（脚本用完后是一个空 turn）
    let evs = run_turn(&mut handle, "again").await;
    assert_eq!(turn_done(&evs).0, 2);
    handle.shutdown().await.unwrap();
    let evs = collect_until(&mut handle, is_exited).await;
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => assert_eq!((*code, error.as_deref()), (Some(0), None)),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn command_that_fails_and_plan_and_other_tools() {
    let h = CodexHarness::new().script("command_failed.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "run the tests").await;
    let evs = events_of(&evs);
    match evs.iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { output, is_error, detail, .. } => {
            assert!(*is_error);
            assert!(output.as_str().unwrap().contains("FAILED"));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(101));
        }
        _ => unreachable!(),
    }
    match &evs[0] {
        Event::ToolCall { category, title, .. } => {
            assert_eq!((*category, title.as_str()), (ToolCategory::Shell, "cargo test -q"))
        }
        other => panic!("{other:?}"),
    }
    handle.shutdown().await.unwrap();

    let h = CodexHarness::new().script("tools_and_plan.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "go").await;
    let n = names(&evs);
    assert!(n.contains(&"plan".to_string()), "{n:?}");
    assert_eq!(n.iter().filter(|x| *x == "tool_call").count(), 3);
    assert_eq!(n.iter().filter(|x| *x == "tool_result").count(), 3);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn errors_end_the_turn_with_a_coded_error_event() {
    for (fixture, code, recoverable) in
        [("real_error_usage_limit.jsonl", "rate_limited", true), ("real_error_http_401.jsonl", "auth_failed", false)]
    {
        let h = CodexHarness::new().script(fixture);
        let mut handle = h.spawn(SessionSettings::default(), None).await;
        started(&mut handle).await;
        let evs = run_turn(&mut handle, "hi").await;
        assert_eq!(names(&evs), vec!["error", "turn_done"], "{fixture}");
        match &events_of(&evs)[0] {
            Event::Error { code: c, recoverable: r, .. } => {
                assert_eq!((c.as_str(), *r), (code, recoverable), "{fixture}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(turn_done(&evs).1, TurnDoneReason::Error);
        assert!(!handle.is_busy());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn reconnect_notices_do_not_end_the_turn() {
    let h = CodexHarness::new().script("reconnecting.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "hi").await;
    assert_eq!(error_codes(&evs), vec!["agent_reconnecting", "agent_reconnecting"]);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_notifications_and_items_are_dropped() {
    let h = CodexHarness::new().script("unknown_things.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "hi").await;
    assert_eq!(names(&evs), vec!["text_delta", "text_delta", "text_done", "turn_done"]);
    assert_eq!(text_of(&evs), "Fine anyway.");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failing_turn_start_is_reported_by_send() {
    let h = CodexHarness::new().var("FAKE_CODEX_TURN_START_ERROR", "model `nope` is not available");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let err = handle.send(input("hi")).await.unwrap_err();
    assert!(matches!(&err, AdapterError::Protocol(m) if m.contains("not available")), "{err:?}");
    assert!(!handle.is_busy());
    // 没有半截的 turn：不会冒出 TurnDone
    assert!(tokio::time::timeout(Duration::from_millis(300), handle.events().recv()).await.is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn images_are_sent_as_local_images_and_other_files_by_path() {
    let h = CodexHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let att = |name: &str, mime: &str| AttachmentRef {
        blob_id: "b".into(),
        name: name.into(),
        mime: mime.into(),
        bytes: 1,
        width: None,
        height: None,
    };
    let with_image = UserInput {
        message_id: "m".into(),
        interject: false,
        content: "what is this?".into(),
        attachments: vec![att("a.png", "image/png")],
        attachment_paths: vec!["/tmp/anycon-a.png".into()],
    };
    handle.send(with_image).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    let t = &h.requests("turn/start")[0]["params"];
    assert_eq!(
        t["input"],
        json!([{"type": "text", "text": "what is this?"}, {"type": "localImage", "path": "/tmp/anycon-a.png"}])
    );

    let pdf = UserInput {
        message_id: "m".into(),
        interject: false,
        content: "read".into(),
        attachments: vec![att("a.pdf", "application/pdf")],
        attachment_paths: vec!["/tmp/a.pdf".into()],
    };
    handle.send(pdf).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    let starts = h.requests("turn/start");
    assert_eq!(starts.len(), 2);
    let input = &starts[1]["params"]["input"];
    assert_eq!(input.as_array().unwrap().len(), 2, "{input}");
    let listed = input[0]["text"].as_str().unwrap();
    assert!(listed.contains("- a.pdf (application/pdf, 1 bytes): /tmp/a.pdf"), "{listed}");
    assert_eq!(input[1], json!({"type": "text", "text": "read"}));
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 审批

#[tokio::test]
async fn command_approval_round_trip_allow() {
    let h = CodexHarness::new().script("real_exec_approval.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("run a command")).await.unwrap();
    let before = collect_until(&mut handle, is_permission).await;
    // tool_call 在权限请求之前
    assert!(names(&before).contains(&"tool_call".to_string()));
    let p = permission_of(&before);
    assert_eq!(p.kind, PermissionKind::Tool);
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("commandExecution", ToolCategory::Shell));
    let d = tool.detail.as_ref().unwrap();
    assert_eq!(d.command.as_deref(), Some("echo hello > out.txt && cat out.txt"));
    // 请求带的 reason 放在 detail.description，让审批卡片直接显示
    assert_eq!(d.description.as_deref(), Some("Do you want to run this command outside the sandbox?"));
    assert_eq!(tool.input["reason"], "Do you want to run this command outside the sandbox?");
    assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["allow", "allow_session", "deny"]);

    handle.respond_permission(&p.request_id, decide("allow")).await.unwrap();
    let rest = collect_until(&mut handle, is_turn_done).await;
    assert!(names(&rest).contains(&"tool_output_delta".to_string()));
    let result = events_of(&rest).into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap();
    match result {
        Event::ToolResult { output, is_error, detail, .. } => {
            assert_eq!((output, is_error), (json!("hello\n"), false));
            assert_eq!(detail.unwrap().exit_code, Some(0));
        }
        _ => unreachable!(),
    }
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Completed);
    assert_eq!(
        h.answers().iter().map(|a| a["result"].clone()).collect::<Vec<_>>(),
        vec![json!({"decision": "accept"})]
    );
    // 答复用的是服务端请求的原 id
    assert_eq!(h.answers()[0]["id"], 100);
    // 重复答复是幂等的
    handle.respond_permission(&p.request_id, decide("allow")).await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_options_map_to_the_codex_decisions() {
    for (option, want) in [("allow_session", "acceptForSession"), ("deny", "decline")] {
        let script = if option == "deny" { "real_exec_declined.jsonl" } else { "real_exec_approval.jsonl" };
        let h = CodexHarness::new().script(script);
        let mut handle = h.spawn(SessionSettings::default(), None).await;
        started(&mut handle).await;
        handle.send(input("run a command")).await.unwrap();
        let evs = collect_until(&mut handle, is_permission).await;
        let p = permission_of(&evs);
        handle.respond_permission(&p.request_id, decide(option)).await.unwrap();
        let rest = collect_until(&mut handle, is_turn_done).await;
        assert_eq!(h.answers()[0]["result"], json!({"decision": want}));
        if option == "deny" {
            // 被拒绝的命令是出错的结果，turn 照常结束（agent 自己接着往下走）
            let r = events_of(&rest).into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap();
            match r {
                Event::ToolResult { is_error, output, .. } => {
                    assert_eq!((is_error, output), (true, json!("Declined by the user")))
                }
                _ => unreachable!(),
            }
            assert_eq!(turn_done(&rest).1, TurnDoneReason::Completed);
        }
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn bad_option_leaves_the_request_pending() {
    let h = CodexHarness::new().script("real_exec_approval.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    let err = handle.respond_permission(&p.request_id, decide("maybe")).await.unwrap_err();
    assert!(matches!(err, AdapterError::Protocol(_)));
    assert!(h.answers().is_empty());
    // 之后还能正常答复
    handle.respond_permission(&p.request_id, decide("allow")).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn patch_approval_shows_the_diff_and_reports_file_changes() {
    let h = CodexHarness::new().script("real_patch_approval.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("create a file")).await.unwrap();
    let evs = collect_until(&mut handle, is_permission).await;
    let p = permission_of(&evs);
    // 录制里的路径在 /work/proj 下，而这个会话的 cwd 是临时目录：cwd 之外的路径保持绝对路径
    assert_eq!(p.title, "Create /work/proj/hello.txt");
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("fileChange", ToolCategory::FileEdit));
    assert_eq!(tool.input["changes"][0]["kind"], "add");
    assert_eq!(tool.input["changes"][0]["diff"], "hi there\nsecond line\n");
    handle.respond_permission(&p.request_id, decide("allow")).await.unwrap();
    let rest = collect_until(&mut handle, is_turn_done).await;
    let evs = events_of(&rest);
    let changed: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            Event::FileChanged { path, kind } => Some((path.clone(), *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(changed, vec![("/work/proj/hello.txt".to_string(), FileChangeKind::Created)]);
    let diff = evs
        .iter()
        .find_map(|e| match e {
            Event::ToolResult { detail, .. } => detail.as_ref().and_then(|d| d.unified_diff.clone()),
            _ => None,
        })
        .expect("a unified diff");
    assert!(
        diff.starts_with("--- /dev/null\n+++ /work/proj/hello.txt\n@@ -0,0 +1,2 @@\n+hi there\n+second line\n"),
        "{diff}"
    );
    assert_eq!(h.answers()[0]["result"], json!({"decision": "accept"}));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_profile_request_round_trip() {
    let h = CodexHarness::new().script("real_permissions_request.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("ask permissions")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!(p.kind, PermissionKind::Tool);
    assert!(p.title.starts_with("Grant network access"), "{}", p.title);
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!(tool.detail.as_ref().unwrap().description.as_deref(), Some("Need network access to fetch a package"));
    handle.respond_permission(&p.request_id, decide("allow_session")).await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    let answer = &h.answers()[0]["result"];
    assert_eq!(answer["scope"], "session");
    assert_eq!(answer["permissions"]["network"]["enabled"], true);
    assert!(answer["permissions"]["fileSystem"]["read"].is_array());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn user_input_request_becomes_a_question() {
    let h = CodexHarness::new().script("real_user_input_request.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("ask me")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!(p.kind, PermissionKind::Question);
    let q = &p.detail.questions[0];
    assert_eq!(q.id, "which_db");
    assert!(q.free_text);
    assert_eq!(q.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["SQLite (Recommended)", "Postgres"]);
    let answers = serde_json::Map::from_iter([("which_db".to_string(), json!("SQLite (Recommended)"))]);
    handle
        .respond_permission(
            &p.request_id,
            PermissionDecision { option_id: Some("answer".into()), answers: Some(answers), message: None },
        )
        .await
        .unwrap();
    collect_until(&mut handle, is_turn_done).await;
    assert_eq!(h.answers()[0]["result"], json!({"answers": {"which_db": {"answers": ["SQLite (Recommended)"]}}}));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_server_requests_are_declined_and_surfaced() {
    let h = CodexHarness::new().script("unsupported_requests.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "go").await;
    // 每一个都发了一条可见的错误，turn 没有被卡住
    assert_eq!(error_codes(&evs), vec!["unsupported_request"; 3]);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    let answers = h.answers();
    assert_eq!(answers.len(), 3);
    assert_eq!(answers[0]["result"]["success"], false);
    assert_eq!(answers[0]["result"]["contentItems"][0]["type"], "inputText");
    assert_eq!(answers[1]["result"], json!({"decision": "abort"}));
    assert_eq!(answers[2]["error"]["code"], -32603);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn mcp_elicitations_round_trip_through_the_handle() {
    // 一个 turn 里三个 elicitation：表单、URL、MCP 工具审批
    let h = CodexHarness::new().script("real_mcp_elicitation.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();

    let form = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!(form.kind, PermissionKind::Question);
    let answers = serde_json::Map::from_iter([
        ("name".to_string(), json!("Ada")),
        ("color".to_string(), json!("green")),
        ("age".to_string(), json!("36")),
        ("subscribe".to_string(), json!("true")),
    ]);
    handle
        .respond_permission(
            &form.request_id,
            PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), message: None },
        )
        .await
        .unwrap();

    let url = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!(
        (url.kind, url.detail.url.as_deref()),
        (PermissionKind::Other, Some("https://example.com/demo/authorize?state=abc123"))
    );
    handle.respond_permission(&url.request_id, decide("accept")).await.unwrap();

    let tool = permission_of(&collect_until(&mut handle, is_permission).await);
    assert_eq!((tool.kind, tool.title.as_str()), (PermissionKind::Tool, "Run MCP tool demo/wipe_cache"));
    handle.respond_permission(&tool.request_id, decide("allow_session")).await.unwrap();

    let rest = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Completed);
    let sent = h.answers();
    assert_eq!(sent.len(), 3);
    assert_eq!(
        sent[0]["result"],
        json!({"action": "accept", "content": {"age": 36, "color": "green", "name": "Ada", "subscribe": true}})
    );
    assert_eq!(sent[1]["result"], json!({"action": "accept"}));
    assert_eq!(sent[2]["result"], json!({"action": "accept", "_meta": {"persist": "session"}}));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_form_answer_of_the_wrong_type_declines_it_and_tells_the_user() {
    let h = CodexHarness::new().script("real_mcp_elicitation.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let form = permission_of(&collect_until(&mut handle, is_permission).await);
    let answers = serde_json::Map::from_iter([
        ("name".to_string(), json!("Ada")),
        ("color".to_string(), json!("green")),
        ("age".to_string(), json!("thirty-six")),
    ]);
    handle
        .respond_permission(
            &form.request_id,
            PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), message: None },
        )
        .await
        .unwrap();
    // 答复 codex 的是 decline，用户另外看到一条可恢复的错误说明原因
    let evs = collect_until(&mut handle, is_permission).await;
    let problem = events_of(&evs).into_iter().find_map(|e| match e {
        Event::Error { code, recoverable, message } => Some((code, recoverable, message)),
        _ => None,
    });
    let (code, recoverable, message) = problem.expect("an error event");
    assert_eq!((code.as_str(), recoverable), ("elicitation_invalid_answer", true));
    assert!(message.contains("\"Age\" is not an integer"), "{message}");
    assert_eq!(h.answers()[0]["result"], json!({"action": "decline"}));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_declines_a_pending_elicitation() {
    let h = CodexHarness::new().script("real_mcp_elicitation.jsonl").var("FAKE_CODEX_DELAY_MS", "30");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let form = permission_of(&collect_until(&mut handle, is_permission).await);
    handle.cancel().await.unwrap();
    let rest = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Cancelled);
    assert_eq!(h.answers()[0]["result"], json!({"action": "decline"}));
    // 之后到来的 elicitation 也不再摆给用户
    assert!(h.answers().iter().all(|a| a["result"] == json!({"action": "decline"})));
    handle.respond_permission(&form.request_id, decide("accept")).await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_sub_agents_turn_does_not_end_the_turn_and_its_calls_hang_under_the_spawn() {
    // 子 agent 的 thread id（FIXTURE_CHILD）不会被 fake 改写，主 thread 的是握手拿到的
    let h = CodexHarness::new().script("real_subagent_command.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let mut all = collect_until(&mut handle, is_permission).await;
    let p = permission_of(&all);
    assert_eq!(p.title, "Run command (sub-agent echo_child)");
    handle.respond_permission(&p.request_id, decide("allow")).await.unwrap();
    all.extend(collect_until(&mut handle, is_turn_done).await);
    // 只有最后一个 turn_done，子 agent 的 turn/completed 在它前面已经过去了
    assert_eq!(names(&all).iter().filter(|n| *n == "turn_done").count(), 1);
    let evs = events_of(&all);
    assert!(evs.iter().any(|e| matches!(e, Event::Task { id, status, .. } if id == "FIXTURE_CHILD" && *status == anycon_protocol::TaskStatus::Completed)));
    assert!(evs.iter().any(|e| matches!(e, Event::ToolCall { id, parent_id, .. } if id == "exec_1" && parent_id.as_deref() == Some("call_1"))));
    assert_eq!(h.answers()[0]["result"], json!({"decision": "accept"}));
    assert!(!handle.is_busy());
    // 子 agent 在跑的时候 handle 报了 1 个后台任务，它结束后降回 0（daemon 据此推迟 / 恢复回收进程）
    let background: Vec<u32> = all
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Background { tasks } => Some(*tasks),
            _ => None,
        })
        .collect();
    assert_eq!(background, [1, 0]);
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 取消 / 转向

#[tokio::test]
async fn cancel_answers_pending_approvals_with_cancel_then_interrupts() {
    let h = CodexHarness::new().script("real_exec_approval.jsonl").var("FAKE_CODEX_DELAY_MS", "60");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let p = permission_of(&collect_until(&mut handle, is_permission).await);
    handle.cancel().await.unwrap();
    let rest = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Cancelled);
    assert_eq!(h.answers()[0]["result"], json!({"decision": "cancel"}));
    let interrupt = &h.requests("turn/interrupt")[0]["params"];
    assert_eq!(interrupt, &json!({"threadId": "thr-0001", "turnId": "turn-0001"}));
    // daemon 随后补发的拒绝是幂等的
    handle.respond_permission(&p.request_id, decide("deny")).await.unwrap();
    assert_eq!(h.answers().len(), 1);
    assert!(!handle.is_busy());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_mid_stream_closes_open_blocks_and_keeps_the_process() {
    let h = CodexHarness::new().script("long_text.jsonl").var("FAKE_CODEX_DELAY_MS", "40");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("write a lot")).await.unwrap();
    let mut evs = collect_until(&mut handle, is_text_delta).await;
    handle.cancel().await.unwrap();
    evs.extend(collect_until(&mut handle, is_turn_done).await);
    let n = names(&evs);
    assert!(n.contains(&"text_done".to_string()), "{n:?}");
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    assert!(!handle.is_busy());
    // 幂等
    handle.cancel().await.unwrap();
    // 取消不杀进程：还能继续
    let evs = run_turn(&mut handle, "next").await;
    assert_eq!((turn_done(&evs).0, turn_done(&evs).1), (2, TurnDoneReason::Completed));
    assert_eq!(h.requests("turn/start").len(), 2);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_during_a_turn_steers_it() {
    let h = CodexHarness::new().script("long_text.jsonl").var("FAKE_CODEX_DELAY_MS", "40");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("write a lot")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    assert!(handle.is_busy());
    handle.send(interject("actually, be brief")).await.unwrap();
    let steer = &h.requests("turn/steer")[0]["params"];
    assert_eq!(steer["threadId"], "thr-0001");
    assert_eq!(steer["expectedTurnId"], "turn-0001");
    assert_eq!(steer["input"], json!([{"type": "text", "text": "actually, be brief"}]));
    // 没有开第二个 turn
    assert_eq!(h.requests("turn/start").len(), 1);
    assert!(handle.is_busy());
    handle.cancel().await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_interjection_carries_its_images_as_local_images_too() {
    let h = CodexHarness::new().script("long_text.jsonl").var("FAKE_CODEX_DELAY_MS", "40");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("write a lot")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    let att = |name: &str, mime: &str| AttachmentRef {
        blob_id: "b".into(),
        name: name.into(),
        mime: mime.into(),
        bytes: 1,
        width: None,
        height: None,
    };

    // 文字 + 两张图，按顺序；只有图片时不放空文本
    let two = UserInput {
        attachments: vec![att("a.png", "image/png"), att("b.jpg", "image/jpeg")],
        attachment_paths: vec!["/tmp/anycon-a.png".into(), "/tmp/anycon-b.jpg".into()],
        ..interject("also look at these")
    };
    handle.send(two).await.unwrap();
    let only_image = UserInput {
        attachments: vec![att("c.webp", "image/webp")],
        attachment_paths: vec!["/tmp/anycon-c.webp".into()],
        ..interject("")
    };
    handle.send(only_image).await.unwrap();
    // 非图片的插话：路径清单在前
    let pdf = UserInput {
        attachments: vec![att("a.pdf", "application/pdf")],
        attachment_paths: vec!["/tmp/anycon-a.pdf".into()],
        ..interject("read this")
    };
    handle.send(pdf).await.unwrap();
    let steers = h.requests("turn/steer");
    assert_eq!(steers.len(), 3);
    let listed = steers[2]["params"]["input"][0]["text"].as_str().unwrap();
    assert!(listed.contains("/tmp/anycon-a.pdf"), "{listed}");
    assert_eq!(steers[2]["params"]["input"][1], json!({"type": "text", "text": "read this"}));
    assert_eq!(
        steers[0]["params"]["input"],
        json!([
            {"type": "text", "text": "also look at these"},
            {"type": "localImage", "path": "/tmp/anycon-a.png"},
            {"type": "localImage", "path": "/tmp/anycon-b.jpg"},
        ])
    );
    assert_eq!(steers[1]["params"]["input"], json!([{"type": "localImage", "path": "/tmp/anycon-c.webp"}]));
    assert_eq!(h.requests("turn/start").len(), 1);
    handle.cancel().await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn ignored_interrupt_falls_back_to_sigint() {
    let h = CodexHarness::new()
        .script("long_text.jsonl")
        .var("FAKE_CODEX_DELAY_MS", "40")
        .var("FAKE_CODEX_IGNORE_INTERRUPT", "1");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("write a lot")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    let began = Instant::now();
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_exited).await;
    assert!(began.elapsed() < Duration::from_secs(8));
    assert_eq!(turn_done(&evs[..evs.len() - 1]).1, TurnDoneReason::Cancelled);
    match evs.last().unwrap() {
        // 我们要求了取消，所以退出是预期内的
        AgentEvent::Exited { error, .. } => assert!(error.is_none(), "{error:?}"),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- 设置

#[tokio::test]
async fn settings_apply_from_the_next_turn_without_a_restart() {
    let h = CodexHarness::new().script("two_turns.jsonl");
    let mut full = SessionSettings::default();
    let mut handle = h.spawn(full.clone(), None).await;
    let first = started(&mut handle).await;
    run_turn(&mut handle, "one").await;
    let t1 = &h.requests("turn/start")[0]["params"];
    assert_eq!((t1["model"].clone(), t1["effort"].clone()), (json!("gpt-6-astra"), json!("low")));

    full.model = Some("gpt-6-luna".into());
    full.thinking = Some("max".into());
    full.mode = Some("full_access".into());
    full.options.insert("fast_mode".into(), json!(true));
    full.options.insert("model_reasoning_summary".into(), json!("concise"));
    let apply = handle.set_settings(&full).await.unwrap();
    assert_eq!(apply, SettingsApply { applied_now: true, restart_required: false });
    // launch_args 跟上了，daemon 会收到一个新的 Started 落库
    let updated = match next_event(&mut handle).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("{other:?}"),
    };
    assert_eq!(updated.agent_session_id, first.agent_session_id);
    assert_eq!(updated.launch_args["mode"], "full_access");
    assert_eq!(updated.launch_args["model"], "gpt-6-luna");
    assert_eq!(updated.launch_args["fast_mode"], true);
    assert_eq!(handle.resume_handle().unwrap().launch_args["thinking"], "max");

    run_turn(&mut handle, "two").await;
    let t2 = &h.requests("turn/start")[1]["params"];
    assert_eq!(t2["model"], "gpt-6-luna");
    assert_eq!(t2["effort"], "max");
    assert_eq!(t2["approvalPolicy"], "never");
    assert_eq!(t2["approvalsReviewer"], "user");
    assert_eq!(t2["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
    assert_eq!(t2["summary"], "concise");
    assert_eq!(t2["serviceTier"], "priority");

    // 第三个 turn 没有再变：沙箱和档位不重复传
    run_turn(&mut handle, "three").await;
    let t3 = &h.requests("turn/start")[2]["params"];
    assert_eq!(t3["approvalPolicy"], "never");
    assert!(t3.get("sandboxPolicy").is_none() && t3.get("serviceTier").is_none());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn changing_env_asks_for_a_restart_and_nothing_else_does() {
    let h = CodexHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let mut s = SessionSettings::default();
    s.env.insert("FEATURE_X".into(), "1".into());
    let apply = handle.set_settings(&s).await.unwrap();
    assert_eq!(apply, SettingsApply { applied_now: false, restart_required: true });
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_instructions_reach_the_thread_through_inject_items() {
    let h = CodexHarness::new();
    let mut s = settings(|s| s.system_prompt_append = Some("Be terse.".into()));
    let mut handle = h.spawn(s.clone(), None).await;
    started(&mut handle).await;
    assert!(h.requests("thread/inject_items").is_empty());

    s.system_prompt_append = Some("Answer in French.".into());
    let apply = handle.set_settings(&s).await.unwrap();
    assert!(apply.applied_now && !apply.restart_required);
    let inject = h.requests("thread/inject_items");
    assert_eq!(inject.len(), 1);
    assert_eq!(inject[0]["params"]["threadId"], "thr-0001");
    let item = &inject[0]["params"]["items"][0];
    assert_eq!((item["type"].clone(), item["role"].clone()), (json!("message"), json!("developer")));
    assert!(item["content"][0]["text"].as_str().unwrap().contains("Answer in French."));
    assert_eq!(handle.resume_handle().unwrap().launch_args["instructions_applied"], "Answer in French.");

    // 没变就不再注入
    handle.set_settings(&s).await.unwrap();
    assert_eq!(h.requests("thread/inject_items").len(), 1);
    // 清掉：告诉 thread 之前的指令作废
    s.system_prompt_append = None;
    handle.set_settings(&s).await.unwrap();
    let inject = h.requests("thread/inject_items");
    assert_eq!(inject.len(), 2);
    assert!(inject[1]["params"]["items"][0]["content"][0]["text"].as_str().unwrap().contains("no longer apply"));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_injection_fails_set_settings_and_is_retried_on_resume() {
    let h = CodexHarness::new().var("FAKE_CODEX_INJECT_FAIL", "1");
    let mut s = settings(|s| s.system_prompt_append = Some("Old.".into()));
    let mut handle = h.spawn(s.clone(), None).await;
    let first = started(&mut handle).await;
    s.system_prompt_append = Some("New.".into());
    assert!(handle.set_settings(&s).await.is_err());
    // 状态没有被改写：thread 仍然只知道旧指令
    let launch = handle.resume_handle().unwrap().launch_args;
    assert_eq!(launch["instructions_applied"], "Old.");
    handle.shutdown().await.unwrap();

    // 重启（resume）时按 `instructions_applied` 再试一次
    let h2 = CodexHarness::new();
    let mut resumed = h2
        .spawn(s.clone(), Some(ResumeHandle { agent_session_id: first.agent_session_id.clone(), launch_args: launch }))
        .await;
    let r = started(&mut resumed).await;
    assert_eq!(r.launch_args["instructions_applied"], "New.");
    let inject = h2.requests("thread/inject_items");
    assert_eq!(inject.len(), 1);
    assert!(inject[0]["params"]["items"][0]["content"][0]["text"].as_str().unwrap().contains("New."));
    resumed.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_settings_are_rejected_before_anything_is_sent() {
    let h = CodexHarness::new();
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let before = h.stdin_messages().len();
    for bad in [
        settings(|s| s.mode = Some("plan".into())),
        settings(|s| s.model = Some("--evil".into())),
        settings(|s| s.max_turns = Some(3)),
        settings(|s| {
            s.options.insert("fast_mode".into(), json!("yes"));
        }),
    ] {
        let err = handle.set_settings(&bad).await.unwrap_err();
        assert!(matches!(err, AdapterError::InvalidSettings { .. }), "{err:?}");
    }
    assert_eq!(h.stdin_messages().len(), before);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn adapter_validates_settings_for_the_daemon() {
    let h = CodexHarness::new();
    let info = h.info().await;
    let a = CodexAdapter::new();
    assert!(a.validate_settings(&info, &settings(|s| s.mode = Some("auto_review".into()))).is_ok());
    let field = |s: SessionSettings| match a.validate_settings(&info, &s) {
        Err(AdapterError::InvalidSettings { field, .. }) => field,
        other => panic!("{other:?}"),
    };
    assert_eq!(field(settings(|s| s.mode = Some("acceptEdits".into()))), "mode");
    // `plan_mode` 在这个版本没有声明（collaborationMode 是 experimental）
    assert_eq!(
        field(settings(|s| {
            s.options.insert("plan_mode".into(), json!(true));
        })),
        "options.plan_mode"
    );
    assert_eq!(field(settings(|s| s.max_budget_usd = Some(1.0))), "max_budget_usd");
    assert_eq!(field(settings(|s| s.thinking = Some("not a level".into()))), "thinking");
}

// ---------------------------------------------------------------- 续接

#[tokio::test]
async fn resume_reuses_the_thread_and_ignores_replayed_usage() {
    let h = CodexHarness::new().script("two_turns.jsonl");
    let resume =
        ResumeHandle { agent_session_id: "thr-from-before".into(), launch_args: json!({"instructions_applied": null}) };
    let mut handle = h.spawn(SessionSettings::default(), Some(resume)).await;
    let r = started(&mut handle).await;
    assert_eq!(r.agent_session_id, "thr-from-before");
    assert!(h.requests("thread/start").is_empty());
    let p = &h.requests("thread/resume")[0]["params"];
    assert_eq!(p["threadId"], "thr-from-before");
    assert_eq!(p["excludeTurns"], true);
    assert_eq!(p["approvalPolicy"], "on-request");
    assert_eq!(p["sandbox"], "workspace-write");
    assert!(p.get("developerInstructions").is_none());
    // 恢复后 app-server 重放了上一个 turn 的用量；不能算进新 turn
    let evs = run_turn(&mut handle, "next").await;
    let u = turn_done(&evs).2.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (Some(80), Some(10)));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn resume_without_a_rollout_starts_a_new_thread() {
    let h = CodexHarness::new().var("FAKE_CODEX_RESUME_FAIL", "none");
    let resume = ResumeHandle { agent_session_id: "thr-never-had-a-turn".into(), launch_args: Value::Null };
    let mut handle = h.spawn(SessionSettings::default(), Some(resume)).await;
    let r = started(&mut handle).await;
    assert_eq!(r.agent_session_id, "thr-0001");
    assert_eq!(h.requests("thread/resume").len(), 1);
    assert_eq!(h.requests("thread/start").len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn other_resume_failures_fail_the_spawn() {
    let h = CodexHarness::new().var("FAKE_CODEX_RESUME_FAIL", "corrupt");
    let resume = ResumeHandle { agent_session_id: "thr-x".into(), launch_args: Value::Null };
    let err = h.try_spawn(SessionSettings::default(), Some(resume)).await.err().expect("spawn must fail");
    assert!(
        matches!(&err, AdapterError::Protocol(m) if m.contains("thread/resume") && m.contains("corrupt")),
        "{err:?}"
    );
    assert!(h.requests("thread/start").is_empty());
}

#[tokio::test]
async fn resume_only_trusts_the_thread_id_and_the_instructions_marker() {
    // launch_args 里的旧模型 / 模式不会复活
    let h = CodexHarness::new();
    let resume = ResumeHandle {
        agent_session_id: "thr-9".into(),
        launch_args: json!({"model": "gpt-5.5", "mode": "full_access", "thinking": "ultra", "instructions_applied": "Be terse."}),
    };
    let mut handle = h.spawn(settings(|s| s.system_prompt_append = Some("Be terse.".into())), Some(resume)).await;
    let r = started(&mut handle).await;
    assert_eq!(r.launch_args["model"], "gpt-6-astra");
    assert_eq!(r.launch_args["mode"], "workspace");
    assert_eq!(h.requests("thread/resume")[0]["params"]["sandbox"], "workspace-write");
    // 指令没变：不注入
    assert!(h.requests("thread/inject_items").is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn resume_tells_the_thread_about_changed_instructions() {
    let h = CodexHarness::new();
    let resume =
        ResumeHandle { agent_session_id: "thr-9".into(), launch_args: json!({"instructions_applied": "Be terse."}) };
    let mut handle = h.spawn(settings(|s| s.system_prompt_append = Some("Be chatty.".into())), Some(resume)).await;
    let r = started(&mut handle).await;
    assert_eq!(r.launch_args["instructions_applied"], "Be chatty.");
    let inject = h.requests("thread/inject_items");
    assert_eq!(inject.len(), 1);
    assert!(inject[0]["params"]["items"][0]["content"][0]["text"].as_str().unwrap().contains("Be chatty."));
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 失败与关闭

#[tokio::test]
async fn startup_failure_carries_the_stderr_tail() {
    let h = CodexHarness::new().var("FAKE_CODEX_INIT", "fail");
    let err = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn must fail");
    assert!(matches!(&err, AdapterError::Spawn(m) if m.contains("cannot open the state database")), "{err:?}");
}

#[tokio::test]
async fn silent_server_times_out_and_is_killed() {
    let mut h = CodexHarness::new();
    h.timings.handshake = Duration::from_millis(800);
    // 探测用正常的假 codex；只有会话进程是哑的
    let mut req = h.request(SessionSettings::default(), None).await;
    let mut env = (*req.env).clone();
    env.insert("FAKE_CODEX_INIT".into(), "hang".into());
    req.env = std::sync::Arc::new(env);
    let began = Instant::now();
    let err = CodexAdapter::with_timings(h.timings).spawn(req).await.err().expect("spawn must fail");
    assert!(matches!(&err, AdapterError::Protocol(m) if m.contains("timed out")), "{err:?}");
    assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
    // 进程被清掉了
    let pid = h.argv_entry()["pid"].as_u64().unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pid_alive(pid));
}

#[tokio::test]
async fn crash_in_the_middle_of_a_turn_is_an_error_and_exit() {
    // 握手 4 行（initialize、thread/start 响应、thread/started …）之后在回放中途崩溃
    let h = CodexHarness::new()
        .script("long_text.jsonl")
        .var("FAKE_CODEX_CRASH_AFTER", "12")
        .var("FAKE_CODEX_DELAY_MS", "10");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let evs = collect_until(&mut handle, is_exited).await;
    assert_eq!(error_codes(&evs), vec!["agent_exited"]);
    assert!(!names(&evs).contains(&"turn_done".to_string()));
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, Some(3));
            let e = error.as_deref().unwrap();
            assert!(e.contains("exited unexpectedly") && e.contains("simulated crash"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!handle.is_busy());
    // 进程没了：再 send 是 Exited 错误
    assert!(matches!(handle.send(input("again")).await, Err(AdapterError::Exited(_))));
}

#[tokio::test]
async fn overlong_stdout_line_is_a_protocol_error() {
    let h = CodexHarness::new().var("FAKE_CODEX_BIG_LINE", "9000000");
    let err = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn must fail");
    assert!(matches!(&err, AdapterError::Spawn(m) if m.contains("protocol error")), "{err:?}");
}

#[tokio::test]
async fn shutdown_escalates_and_clears_the_process_group() {
    let pid_file = tempfile::NamedTempFile::new().unwrap();
    let h = CodexHarness::new()
        .var("FAKE_CODEX_STUBBORN", "1")
        .var("FAKE_CODEX_BG_PID_FILE", &pid_file.path().to_string_lossy());
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let bg: u64 = std::fs::read_to_string(pid_file.path()).unwrap().trim().parse().unwrap();
    assert!(pid_alive(bg));
    let began = Instant::now();
    handle.shutdown().await.unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
    let evs = collect_until(&mut handle, is_exited).await;
    assert!(matches!(evs.last().unwrap(), AgentEvent::Exited { .. }));
    // 后台进程随进程组一起没了
    let end = Instant::now() + Duration::from_secs(3);
    while pid_alive(bg) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pid_alive(bg), "background process {bg} survived");
}

#[tokio::test]
async fn dropping_the_handle_does_not_leave_the_server_behind() {
    let h = CodexHarness::new();
    let handle = h.spawn(SessionSettings::default(), None).await;
    let pid = h.argv_entry()["pid"].as_u64().unwrap();
    assert!(pid_alive(pid));
    drop(handle);
    let end = Instant::now() + Duration::from_secs(5);
    while pid_alive(pid) && Instant::now() < end {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pid_alive(pid));
}

#[tokio::test]
async fn disabled_adapter_refuses_to_spawn() {
    let mut h = CodexHarness::new();
    let req = h.request(SessionSettings::default(), None).await;
    h.config.enabled = false;
    let req = anycon_agent_api::SpawnRequest { config: h.config.clone(), ..req };
    let err = CodexAdapter::new().spawn(req).await.err().expect("spawn must fail");
    assert!(matches!(err, AdapterError::Spawn(m) if m.contains("disabled")));
}

// ---------------------------------------------------------------- 插话

#[tokio::test]
async fn interjecting_without_a_running_turn_is_turn_ended_and_never_opens_a_turn() {
    let h = CodexHarness::new().script("two_turns.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    // 还没有任何 turn
    assert!(matches!(handle.send(interject("too early")).await, Err(AdapterError::TurnEnded)));
    assert!(h.requests("turn/start").is_empty() && h.requests("turn/steer").is_empty());
    // 一个 turn 刚结束
    run_turn(&mut handle, "one").await;
    assert!(matches!(handle.send(interject("too late")).await, Err(AdapterError::TurnEnded)));
    assert_eq!(h.requests("turn/start").len(), 1, "an interjection must never start a turn by itself");
    assert!(h.requests("turn/steer").is_empty());
    assert!(!handle.is_busy());
    // 下一个 turn 照常开
    let evs = run_turn(&mut handle, "two").await;
    assert_eq!(turn_done(&evs).0, 2);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_after_the_turn_finished_but_before_turn_done_was_read_is_turn_ended() {
    // 竞态：turn 已经结束、`turn_done` 还在通道里没被 daemon 读到，用户发了一条消息
    let h = CodexHarness::new().script("two_turns.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("one")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await; // 回放早就结束了，TurnDone 在通道里，没人读
    assert!(matches!(handle.send(interject("sneaky")).await, Err(AdapterError::TurnEnded)));
    assert_eq!(h.requests("turn/start").len(), 1);
    assert!(h.requests("turn/steer").is_empty());
    // TurnDone 还在，且是这个 turn 的；之后没有冒出第二个 turn 的事件
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&evs).0, 1);
    assert!(tokio::time::timeout(Duration::from_millis(300), handle.events().recv()).await.is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn steer_rejected_with_no_active_turn_is_turn_ended_and_the_turn_carries_on() {
    // adapter 以为 turn 还在，app-server 说没有进行中的 turn：不能自己开新 turn
    let h = CodexHarness::new()
        .script("long_text.jsonl")
        .var("FAKE_CODEX_DELAY_MS", "30")
        .var("FAKE_CODEX_STEER_REJECT", "1");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    assert!(matches!(handle.send(interject("late")).await, Err(AdapterError::TurnEnded)));
    assert_eq!(h.requests("turn/start").len(), 1);
    assert_eq!(h.requests("turn/steer").len(), 1);
    // 正在进行的 turn 没被连累，照常跑完
    assert!(handle.is_busy());
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    handle.shutdown().await.unwrap();
}

/// 已经 `cancel`、turn 还没收尾时再插话 = 没有进行中的 turn。真实 codex 会接受这样的 `turn/steer`，
/// 然后随着 interrupt 把它连同 turn 一起丢掉（见 README），所以 adapter 一条 steer 都不发，让 daemon 把消息退回队列。
#[tokio::test]
async fn interjecting_while_a_cancel_is_pending_is_turn_ended_and_sends_no_steer() {
    let h = CodexHarness::new()
        .script("long_text.jsonl")
        .var("FAKE_CODEX_DELAY_MS", "40")
        .var("FAKE_CODEX_IGNORE_INTERRUPT", "1");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("write a lot")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    handle.cancel().await.unwrap();
    // 假 codex 不理 interrupt：turn 还在跑，cancel 的兜底（SIGINT）也还没到
    assert!(handle.is_busy(), "the cancel is pending, the turn has not finished yet");
    assert!(matches!(handle.send(interject("never mind, do this instead")).await, Err(AdapterError::TurnEnded)));
    assert!(h.requests("turn/steer").is_empty(), "no turn/steer may be sent while a cancel is pending");
    assert_eq!(h.requests("turn/start").len(), 1, "an interjection must never start a turn by itself");
    assert_eq!(h.requests("turn/interrupt").len(), 1);
    // 取消照常收尾
    let evs = collect_until(&mut handle, is_exited).await;
    assert_eq!(turn_done(&evs[..evs.len() - 1]).1, TurnDoneReason::Cancelled);
}

#[tokio::test]
async fn a_finished_cancel_does_not_stop_later_turns_from_being_steered() {
    // 两个 turn 都是长回答：把录制的那一个连放两遍
    let one = std::fs::read_to_string(codex_fixture_path("long_text.jsonl")).unwrap();
    let h = CodexHarness::new().var("FAKE_CODEX_DELAY_MS", "40");
    let twice = h.dir.path().join("long_text_twice.jsonl");
    std::fs::write(&twice, format!("{}\n{}\n", one.trim_end(), one.trim_end())).unwrap();
    let h = h.var("FAKE_CODEX_SCRIPT", &twice.to_string_lossy());
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("first")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    handle.cancel().await.unwrap();
    let evs = collect_until(&mut handle, is_turn_done).await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    // 下一个 turn：取消的标记不能留着，插话照常是 turn/steer
    handle.send(input("second")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    // 等过上一次取消的 SIGINT 兜底时间：兜底只认被取消的那个 turn，不能把这个 turn 一起打断
    tokio::time::sleep(h.timings.interrupt_fallback + Duration::from_millis(200)).await;
    handle.send(interject("and one more thing")).await.unwrap();
    assert_eq!(h.requests("turn/steer").len(), 1);
    handle.cancel().await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_message_without_interject_cannot_go_into_a_running_turn() {
    let h = CodexHarness::new().script("long_text.jsonl").var("FAKE_CODEX_DELAY_MS", "30");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    collect_until(&mut handle, is_text_delta).await;
    assert!(matches!(handle.send(input("a second turn?")).await, Err(AdapterError::Unsupported(_))));
    assert_eq!(h.requests("turn/start").len(), 1);
    handle.cancel().await.unwrap();
    collect_until(&mut handle, is_turn_done).await;
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 不阻塞约定

/// 一个 turn：开始、一个命令、`n` 条输出，然后一直停着等客户端打断。
fn write_flood(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
    let mut lines = vec![
        r#"{"method":"turn/started","params":{"threadId":"FIXTURE_THREAD","turn":{"id":"FIXTURE_TURN","items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":1,"completedAt":null,"durationMs":null}}}"#.to_string(),
        r#"{"method":"item/started","params":{"item":{"type":"commandExecution","id":"c1","command":"/bin/zsh -lc 'yes'","cwd":"/work/proj","status":"inProgress","commandActions":[]},"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","startedAtMs":1}}"#.to_string(),
    ];
    for i in 0..n {
        lines.push(format!(
            r#"{{"method":"item/commandExecution/outputDelta","params":{{"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","itemId":"c1","delta":"line {i}\n"}}}}"#
        ));
    }
    lines.push(r#"{"_hold":true}"#.to_string());
    let path = dir.join("flood.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

async fn flooding_session(n: usize) -> (CodexHarness, Box<dyn AgentHandle>, tempfile::TempDir) {
    let scripts = tempfile::tempdir().unwrap();
    let flood = write_flood(scripts.path(), n);
    let h = CodexHarness::new().var("FAKE_CODEX_SCRIPT", &flood.to_string_lossy());
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    // 洪水在飞，没有人读 `events()`：有界通道早就满了
    tokio::time::sleep(Duration::from_millis(800)).await;
    (h, handle, scripts)
}

fn count_output_deltas(evs: &[AgentEvent]) -> usize {
    events_of(evs).iter().filter(|e| matches!(e, Event::ToolOutputDelta { .. })).count()
}

#[tokio::test]
async fn cancel_is_not_blocked_by_thousands_of_unread_events() {
    let (h, mut handle, _scripts) = flooding_session(5000).await;
    let began = Instant::now();
    tokio::time::timeout(Duration::from_secs(5), handle.cancel())
        .await
        .expect("cancel() must not wait for the event channel to drain")
        .unwrap();
    assert!(began.elapsed() < Duration::from_secs(5), "{:?}", began.elapsed());
    // 然后读到的是全部 5000 条输出和 turn_done { cancelled }
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut handle, is_turn_done)).await.unwrap();
    assert_eq!(count_output_deltas(&evs), 5000);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    assert_eq!(h.requests("turn/interrupt").len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_is_not_blocked_by_thousands_of_unread_events() {
    let (h, mut handle, _scripts) = flooding_session(5000).await;
    tokio::time::timeout(Duration::from_secs(5), handle.send(interject("while you are busy")))
        .await
        .expect("an interjection must not wait for the event channel to drain")
        .unwrap();
    assert_eq!(h.requests("turn/steer").len(), 1);
    tokio::time::timeout(Duration::from_secs(5), handle.cancel()).await.expect("cancel").unwrap();
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut handle, is_turn_done)).await.unwrap();
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn settings_and_permission_replies_are_not_blocked_either() {
    let (h, mut handle, _scripts) = flooding_session(5000).await;
    // 要等 app-server 响应的 set_settings（改指令 = thread/inject_items）
    let s = settings(|s| s.system_prompt_append = Some("Be terse.".into()));
    tokio::time::timeout(Duration::from_secs(5), handle.set_settings(&s)).await.expect("set_settings").unwrap();
    assert_eq!(h.requests("thread/inject_items").len(), 1);
    // 答复一个不存在的权限请求是幂等的，也不能卡
    tokio::time::timeout(Duration::from_secs(5), handle.respond_permission("nope", decide("allow")))
        .await
        .expect("respond_permission")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.cancel()).await.expect("cancel").unwrap();
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut handle, is_turn_done)).await.unwrap();
    // 事件一条没丢、顺序没乱：洪水先到，改设置时补发的 Started 排在它们后面、turn_done 前面
    assert_eq!(count_output_deltas(&evs), 5000);
    let n = names(&evs);
    assert_eq!(n.iter().filter(|x| *x == "STARTED").count(), 1);
    let started_at = n.iter().position(|x| x == "STARTED").unwrap();
    assert!(n.iter().rposition(|x| x == "tool_output_delta").unwrap() < started_at);
    assert_eq!(n.last().map(String::as_str), Some("turn_done"));
    handle.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 超长的行

fn big(n: usize) -> String {
    "y".repeat(n)
}

fn write_lines(dir: &std::path::Path, name: &str, lines: &[String]) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

const TURN_STARTED: &str = r#"{"method":"turn/started","params":{"threadId":"FIXTURE_THREAD","turn":{"id":"FIXTURE_TURN","items":[],"itemsView":"notLoaded","status":"inProgress","error":null,"startedAt":1,"completedAt":null,"durationMs":null}}}"#;
const TURN_COMPLETED: &str = r#"{"method":"turn/completed","params":{"threadId":"FIXTURE_THREAD","turn":{"id":"FIXTURE_TURN","items":[],"itemsView":"notLoaded","status":"completed","error":null,"startedAt":1,"completedAt":2,"durationMs":9}}}"#;

#[tokio::test]
async fn oversize_notifications_are_dropped_not_fatal() {
    // 真实 codex 0.157.1 上实测：一个 11 MB 的新文件补丁会产生 11 MB 的 item/started、item/completed 和（三条）turn/diff/updated。
    // 键的顺序和真实输出一样（type、id 在前，大的 diff 在后）。
    let huge = big(9 * 1024 * 1024);
    let started = format!(
        r#"{{"method":"item/started","params":{{"item":{{"type":"fileChange","id":"big_1","changes":[{{"path":"/work/proj/big.txt","kind":{{"type":"add"}},"diff":"{huge}"}}],"status":"inProgress"}},"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","startedAtMs":1}},"emittedAtMs":1}}"#
    );
    let diff = format!(
        r#"{{"method":"turn/diff/updated","params":{{"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","diff":"{huge}"}},"emittedAtMs":2}}"#
    );
    let done = started
        .replace("item/started", "item/completed")
        .replace(r#""status":"inProgress""#, r#""status":"completed""#);
    let text = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"m1","text":"Done."},"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","completedAtMs":3}}"#;
    let dir = tempfile::tempdir().unwrap();
    let script = write_lines(
        dir.path(),
        "big_patch.jsonl",
        &[TURN_STARTED.into(), started, diff, done, text.into(), TURN_COMPLETED.into()],
    );
    let h = CodexHarness::new().var("FAKE_CODEX_SCRIPT", &script.to_string_lossy());
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started_event(&mut handle).await;
    let evs = run_turn(&mut handle, "write a big file").await;
    let events = events_of(&evs);
    // 进程没被杀，turn 正常结束，没有错误
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    assert!(error_codes(&evs).is_empty(), "{:?}", error_codes(&evs));
    // 补丁的调用和结果还是出现了：路径从前缀里抠出来，内容标注「太大没显示」
    match events.iter().find(|e| matches!(e, Event::ToolCall { .. })).expect("a tool_call") {
        Event::ToolCall { id, name, category, title, input, detail, .. } => {
            assert_eq!((id.as_str(), name.as_str(), *category), ("big_1", "fileChange", ToolCategory::FileEdit));
            assert!(title.contains("/work/proj/big.txt") && title.contains("too large"), "{title}");
            assert!(input["omitted"].as_str().unwrap().contains("over the"), "{input}");
            assert_eq!(detail.as_ref().unwrap().path.as_deref(), Some("/work/proj/big.txt"));
        }
        _ => unreachable!(),
    }
    match events.iter().find(|e| matches!(e, Event::ToolResult { .. })).expect("a tool_result") {
        Event::ToolResult { id, output, is_error, .. } => {
            assert_eq!(id, "big_1");
            assert!(output.as_str().unwrap().starts_with("[result not shown"), "{output}");
            assert!(!is_error);
        }
        _ => unreachable!(),
    }
    // 之后的 turn 照常
    let evs = run_turn(&mut handle, "again").await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    handle.shutdown().await.unwrap();
    let evs = collect_until(&mut handle, is_exited).await;
    match evs.last().unwrap() {
        AgentEvent::Exited { error, .. } => assert!(error.is_none(), "{error:?}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_oversize_server_request_is_a_protocol_error() {
    // 待答复的服务端请求超长：丢了它 turn 就卡死，所以这才是真正的协议错误
    let huge = big(9 * 1024 * 1024);
    let request = format!(
        r#"{{"method":"item/commandExecution/requestApproval","id":0,"params":{{"threadId":"FIXTURE_THREAD","turnId":"FIXTURE_TURN","itemId":"c1","command":"{huge}"}}}}"#
    );
    let dir = tempfile::tempdir().unwrap();
    let script = write_lines(dir.path(), "big_request.jsonl", &[TURN_STARTED.into(), request, TURN_COMPLETED.into()]);
    let h = CodexHarness::new().var("FAKE_CODEX_SCRIPT", &script.to_string_lossy());
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started_event(&mut handle).await;
    handle.send(input("go")).await.unwrap();
    let evs = collect_until(&mut handle, is_exited).await;
    assert_eq!(error_codes(&evs), vec!["agent_exited"]);
    assert!(!names(&evs).contains(&"turn_done".to_string()));
    match events_of(&evs).iter().find(|e| matches!(e, Event::Error { .. })).unwrap() {
        Event::Error { message, .. } => {
            assert!(message.contains("server request") && message.contains("requestApproval"), "{message}")
        }
        _ => unreachable!(),
    }
    match evs.last().unwrap() {
        AgentEvent::Exited { error, .. } => {
            let e = error.as_deref().unwrap();
            assert!(e.contains("protocol error") && e.contains("over the"), "{e}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!handle.is_busy());
}

#[tokio::test]
async fn an_unidentifiable_oversize_line_is_a_protocol_error() {
    // 前缀里认不出是什么：保守地当协议错误（已有的 `overlong_stdout_line_is_a_protocol_error` 也覆盖）
    let h = CodexHarness::new().var("FAKE_CODEX_BIG_LINE", "9000000");
    let err = h.try_spawn(SessionSettings::default(), None).await.err().expect("spawn must fail");
    assert!(
        matches!(&err, AdapterError::Spawn(m) if m.contains("protocol error") && m.contains("could not be identified")),
        "{err:?}"
    );
}

// ---------------------------------------------------------------- 续接回退

#[tokio::test]
async fn context_reset_is_announced_when_the_thread_cannot_be_resumed() {
    for (mode, reason) in [("none", "no rollout found"), ("notfound", "thread not found")] {
        let h = CodexHarness::new().var("FAKE_CODEX_RESUME_FAIL", mode);
        let resume = ResumeHandle { agent_session_id: "thr-gone".into(), launch_args: Value::Null };
        let mut handle = h.spawn(SessionSettings::default(), Some(resume)).await;
        let r = started(&mut handle).await;
        assert_eq!(r.agent_session_id, "thr-0001", "{mode}: a new thread was started");
        // 紧跟着一条可恢复的提示（不让 turn 失败）
        match next_event(&mut handle).await {
            AgentEvent::Event(Event::Error { code, message, recoverable }) => {
                assert_eq!(code, "agent_context_reset", "{mode}");
                assert!(recoverable);
                assert!(message.contains(reason) && message.contains("new thread"), "{mode}: {message}");
            }
            other => panic!("{mode}: expected the context-reset notice, got {other:?}"),
        }
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn no_context_reset_notice_for_new_or_successfully_resumed_threads() {
    let h = CodexHarness::new().script("two_turns.jsonl");
    let mut handle = h.spawn(SessionSettings::default(), None).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "one").await;
    assert!(error_codes(&evs).is_empty());
    handle.shutdown().await.unwrap();
    let h = CodexHarness::new().script("two_turns.jsonl");
    let resume = ResumeHandle { agent_session_id: "thr-9".into(), launch_args: json!({"instructions_applied": null}) };
    let mut handle = h.spawn(SessionSettings::default(), Some(resume)).await;
    started(&mut handle).await;
    let evs = run_turn(&mut handle, "next").await;
    assert!(error_codes(&evs).is_empty(), "{:?}", error_codes(&evs));
    handle.shutdown().await.unwrap();
}
