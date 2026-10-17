//! mock adapter 的完整行为矩阵：每个关键词路径、取消、续接、设置回显、权限往返。
//! 行为表见 `src/mock/README.md`；E2E 与 web 测试依赖这些行为，改动要同步改 README。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::MockAdapter;
use anycon_agent_api::{
    Adapter, AdapterConfig, AdapterError, AgentEvent, AgentHandle, PendingPermission, PermissionDecision, ProbeContext,
    ResumeHandle, SpawnRequest, UserInput,
};
use anycon_protocol::{
    AgentInfo, AgentStatus, AttachmentRef, EditKind, Event, FileChangeKind, PermissionKind, PermissionOptionKind,
    PlanStatus, SessionSettings, ToolCategory, TurnDoneReason,
};
use serde_json::json;

fn probe_ctx(enabled: bool) -> ProbeContext {
    ProbeContext {
        env: Arc::new(HashMap::new()),
        config: AdapterConfig { enabled, ..Default::default() },
        deadline: Duration::from_secs(5),
        cheap: false,
        previous: None,
        scratch_dir: std::env::temp_dir(),
    }
}

async fn info() -> Arc<AgentInfo> {
    Arc::new(MockAdapter::new().probe(&probe_ctx(true)).await)
}

fn request(info: Arc<AgentInfo>, settings: SessionSettings, resume: Option<ResumeHandle>) -> SpawnRequest {
    SpawnRequest {
        session_id: "sess_test".into(),
        cwd: PathBuf::from("/tmp/mock-project"),
        settings,
        env: Arc::new(HashMap::new()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        resume,
        agent_info: info,
    }
}

async fn spawn_with(settings: SessionSettings) -> Box<dyn AgentHandle> {
    let info = info().await;
    MockAdapter::new().spawn(request(info, settings, None)).await.expect("spawn")
}

/// cwd 指向真实目录的会话（`write` 关键词会在里面写文件）。
async fn spawn_in(cwd: &Path, settings: SessionSettings) -> Box<dyn AgentHandle> {
    let info = info().await;
    let mut req = request(info, settings, None);
    req.cwd = cwd.to_path_buf();
    MockAdapter::new().spawn(req).await.expect("spawn")
}

async fn spawn_default() -> Box<dyn AgentHandle> {
    spawn_with(SessionSettings::default()).await
}

fn input(text: &str) -> UserInput {
    UserInput {
        message_id: "m1".into(),
        interject: false,
        content: text.into(),
        attachments: vec![],
        attachment_paths: vec![],
    }
}

async fn next(h: &mut Box<dyn AgentHandle>) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), h.events().recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

/// 收事件直到 `TurnDone`（含）。遇到权限请求用 `on_permission` 决定答复；返回全部事件。
async fn run_turn(
    h: &mut Box<dyn AgentHandle>,
    prompt: &str,
    mut on_permission: impl FnMut(&PendingPermission) -> PermissionDecision,
) -> Vec<AgentEvent> {
    h.send(input(prompt)).await.expect("send");
    let mut out = Vec::new();
    loop {
        let ev = next(h).await;
        if let AgentEvent::Permission(p) = &ev {
            let d = on_permission(p);
            let rid = p.request_id.clone();
            out.push(ev);
            h.respond_permission(&rid, d).await.expect("respond");
            continue;
        }
        let done = matches!(&ev, AgentEvent::Event(Event::TurnDone { .. }));
        out.push(ev);
        if done {
            return out;
        }
    }
}

fn no_permission(_: &PendingPermission) -> PermissionDecision {
    panic!("unexpected permission request")
}

fn choose(id: &'static str) -> impl FnMut(&PendingPermission) -> PermissionDecision {
    move |_| PermissionDecision { option_id: Some(id.into()), ..Default::default() }
}

fn events_of(evs: &[AgentEvent]) -> Vec<&Event> {
    evs.iter().filter_map(|e| if let AgentEvent::Event(ev) = e { Some(ev) } else { None }).collect()
}

fn text_of(evs: &[AgentEvent]) -> String {
    events_of(evs)
        .into_iter()
        .filter_map(|e| if let Event::TextDelta { text } = e { Some(text.as_str()) } else { None })
        .collect()
}

fn type_names(evs: &[AgentEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            AgentEvent::Event(ev) => serde_json::to_value(ev).unwrap()["type"].as_str().unwrap().to_string(),
            AgentEvent::Permission(_) => "PERMISSION".to_string(),
            AgentEvent::Started { .. } => "STARTED".to_string(),
            AgentEvent::Title(_) => "TITLE".to_string(),
            AgentEvent::Exited { .. } => "EXITED".to_string(),
        })
        .collect()
}

fn turn_done(evs: &[AgentEvent]) -> (u64, TurnDoneReason, anycon_protocol::Usage) {
    match evs.last() {
        Some(AgentEvent::Event(Event::TurnDone { turn_id, reason, usage })) => {
            (*turn_id, *reason, usage.clone().expect("usage"))
        }
        other => panic!("last event is not TurnDone: {other:?}"),
    }
}

// ---------- 图片附件（ADR-35） ----------

/// 在 `dir` 里写一个文件，返回 (引用, 路径)。引用里的 `bytes` 故意写成 999：回显里的大小必须是读文件得到的。
fn attached(dir: &Path, name: &str, mime: &str, content: &[u8]) -> (AttachmentRef, PathBuf) {
    let path = dir.join(format!("blob-{name}"));
    std::fs::write(&path, content).unwrap();
    (
        AttachmentRef {
            blob_id: "0123456789abcdef0123456789abcdef".into(),
            name: name.into(),
            mime: mime.into(),
            bytes: 999,
            width: None,
            height: None,
        },
        path,
    )
}

fn input_with(text: &str, files: Vec<(AttachmentRef, PathBuf)>) -> UserInput {
    let (attachments, attachment_paths) = files.into_iter().unzip();
    UserInput { attachments, attachment_paths, ..input(text) }
}

async fn run_with(h: &mut Box<dyn AgentHandle>, inp: UserInput) -> Vec<AgentEvent> {
    h.send(inp).await.expect("send");
    let mut out = Vec::new();
    loop {
        let ev = next(h).await;
        let done = matches!(&ev, AgentEvent::Event(Event::TurnDone { .. }));
        out.push(ev);
        if done {
            return out;
        }
    }
}

#[tokio::test]
async fn probe_advertises_exactly_the_four_image_types() {
    let info = info().await;
    assert_eq!(info.capabilities.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp"]);
}

#[tokio::test]
async fn attachments_add_one_text_delta_between_the_echo_and_text_done() {
    let files = tempfile::tempdir().unwrap();
    let mut h = spawn_default().await;
    let _ = next(&mut h).await; // Started
    let img = attached(files.path(), "shot.png", "image/png", b"abc");
    let evs = run_with(&mut h, input_with("look at this", vec![img])).await;
    assert_eq!(
        type_names(&evs),
        vec![
            "thinking_delta",
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_delta", // 多出来的这一个
            "text_done",
            "turn_done"
        ]
    );
    let deltas: Vec<&str> = events_of(&evs)
        .into_iter()
        .filter_map(|e| if let Event::TextDelta { text } = e { Some(text.as_str()) } else { None })
        .collect();
    assert_eq!(deltas[2], " You said: look at this");
    // sha256("abc") = ba7816bf8f01cfea…；大小是读文件得到的 3，不是引用里的 999
    assert_eq!(deltas[3], " Attachments: shot.png (image/png, 3 bytes, sha256 ba7816bf8f01)");
    // 用量把多出来的字符算进 output_tokens
    let (_, reason, usage) = turn_done(&evs);
    assert_eq!(reason, TurnDoneReason::Completed);
    assert_eq!(usage.output_tokens, Some(text_of(&evs).chars().count() as u64));
    assert_eq!(usage.input_tokens, Some("look at this".chars().count() as u64));
    // 没有附件的下一个 turn 回到原来的事件序列
    let evs = run_turn(&mut h, "hello there", no_permission).await;
    assert_eq!(text_of(&evs), "Mock reply: [model=mock-large thinking=high mode=ask] You said: hello there");
    assert_eq!(type_names(&evs).iter().filter(|n| *n == "text_delta").count(), 3);
}

#[tokio::test]
async fn several_attachments_are_joined_in_order_and_read_from_their_own_paths() {
    let files = tempfile::tempdir().unwrap();
    let mut h = spawn_default().await;
    let a = attached(files.path(), "a.png", "image/png", b"abc");
    let b = attached(files.path(), "b.jpg", "image/jpeg", b"hello");
    let c = attached(files.path(), "empty.webp", "image/webp", b"");
    // 引用在，文件不在（比如被回收了）：unreadable，不报错、不影响其它
    let gone = (attached(files.path(), "gone.gif", "image/gif", b"x").0, files.path().join("does-not-exist"));
    let evs = run_with(&mut h, input_with("pics", vec![a, b, c, gone])).await;
    let expected = [
        "Mock reply: [model=mock-large thinking=high mode=ask] You said: pics Attachments: a.png (image/png, 3 bytes, sha256 ba7816bf8f01)",
        "b.jpg (image/jpeg, 5 bytes, sha256 2cf24dba5fb0)", // sha256("hello")
        "empty.webp (image/webp, 0 bytes, sha256 e3b0c44298fc)", // sha256("")
        "gone.gif (unreadable)",
    ]
    .join("; ");
    assert_eq!(text_of(&evs), expected);
    // 仍然只多一个 text_delta（不是每个附件一个）
    assert_eq!(type_names(&evs).iter().filter(|n| *n == "text_delta").count(), 4);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
}

#[tokio::test]
async fn attachments_do_not_change_what_the_keywords_do() {
    let files = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let mut h = spawn_in(cwd.path(), SessionSettings::default()).await;
    let _ = next(&mut h).await; // Started
    let img = attached(files.path(), "p.png", "image/png", b"abc");
    // 回显（含附件那一段）先于 permission 请求，text_done 在请求之前
    h.send(input_with("run permission", vec![img])).await.unwrap();
    let mut names = Vec::new();
    loop {
        match next(&mut h).await {
            AgentEvent::Permission(p) => {
                names.push("PERMISSION".to_string());
                h.respond_permission(
                    &p.request_id,
                    PermissionDecision { option_id: Some("deny".into()), ..Default::default() },
                )
                .await
                .unwrap();
            }
            AgentEvent::Event(ev) => {
                let done = matches!(ev, Event::TurnDone { .. });
                names.push(serde_json::to_value(&ev).unwrap()["type"].as_str().unwrap().to_string());
                if done {
                    break;
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        names,
        vec![
            "thinking_delta",
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_done",
            "PERMISSION",
            "text_delta", // skipped
            "text_done",
            "turn_done"
        ]
    );
    // 附件在 cwd 之外：mock 只读它们，cwd 里什么都没有多出来
    assert!(dir_entries(cwd.path()).is_empty());
}

#[tokio::test]
async fn mismatched_attachment_paths_are_refused_without_opening_a_turn() {
    let files = tempfile::tempdir().unwrap();
    let mut h = spawn_default().await;
    let _ = next(&mut h).await; // Started
    let (att, path) = attached(files.path(), "a.png", "image/png", b"abc");
    let few = UserInput { attachments: vec![att.clone()], attachment_paths: vec![], ..input("t") };
    assert!(matches!(h.send(few).await, Err(AdapterError::Protocol(_))));
    let many = UserInput { attachments: vec![], attachment_paths: vec![path], ..input("t") };
    assert!(matches!(h.send(many).await, Err(AdapterError::Protocol(_))));
    assert!(!h.is_busy());
    // 没有半截的 turn：下一次正常发送是第 1 个 turn
    let evs = run_turn(&mut h, "fine", no_permission).await;
    assert_eq!(turn_done(&evs).0, 1);
}

// ---------- 基本形状 ----------

#[tokio::test]
async fn started_event_comes_first_with_session_derived_id() {
    let mut h = spawn_default().await;
    match next(&mut h).await {
        AgentEvent::Started { resume } => assert_eq!(resume.agent_session_id, "mock-sess_test"),
        other => panic!("expected Started, got {other:?}"),
    }
    assert_eq!(h.resume_handle().unwrap().agent_session_id, "mock-sess_test");
    assert!(!h.is_busy());
}

#[tokio::test]
async fn plain_prompt_default_settings() {
    let mut h = spawn_default().await;
    let _ = next(&mut h).await; // Started
    let evs = run_turn(&mut h, "hello there", no_permission).await;
    assert_eq!(
        type_names(&evs),
        vec![
            "thinking_delta",
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_done",
            "turn_done"
        ]
    );
    assert_eq!(text_of(&evs), "Mock reply: [model=mock-large thinking=high mode=ask] You said: hello there");
    let (turn_id, reason, usage) = turn_done(&evs);
    assert_eq!(turn_id, 1);
    assert_eq!(reason, TurnDoneReason::Completed);
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(text_of(&evs).chars().count() as u64));
    assert_eq!(usage.cache_read_tokens, Some(0));
    assert_eq!(usage.cache_write_tokens, Some(0));
    assert_eq!(usage.duration_ms, 100);
    assert_eq!(usage.model.as_deref(), Some("mock-large"));
    assert!(usage.cost_usd.unwrap() > 0.0);
    assert!(!h.is_busy());
}

#[tokio::test]
async fn usage_is_deterministic_across_runs() {
    let mut a = spawn_default().await;
    let mut b = spawn_default().await;
    let ea = run_turn(&mut a, "same prompt", no_permission).await;
    let eb = run_turn(&mut b, "same prompt", no_permission).await;
    assert_eq!(turn_done(&ea).2, turn_done(&eb).2);
    assert_eq!(text_of(&ea), text_of(&eb));
}

#[tokio::test]
async fn turn_ids_increase_per_send() {
    let mut h = spawn_default().await;
    let e1 = run_turn(&mut h, "one", no_permission).await;
    let e2 = run_turn(&mut h, "two", no_permission).await;
    assert_eq!(turn_done(&e1).0, 1);
    assert_eq!(turn_done(&e2).0, 2);
}

#[tokio::test]
async fn small_model_has_no_thinking_events() {
    let mut h = spawn_with(SessionSettings { model: Some("mock-small".into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "hi", no_permission).await;
    assert!(!type_names(&evs).iter().any(|t| t.starts_with("thinking")));
    assert!(text_of(&evs).contains("[model=mock-small thinking=none mode=ask]"));
}

#[tokio::test]
async fn explicit_settings_are_echoed() {
    let mut h = spawn_with(SessionSettings {
        model: Some("mock-large".into()),
        thinking: Some("low".into()),
        mode: Some("plan".into()),
        ..Default::default()
    })
    .await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert!(text_of(&evs).contains("[model=mock-large thinking=low mode=plan]"));
}

#[tokio::test]
async fn verbose_option_adds_marker() {
    let mut options = serde_json::Map::new();
    options.insert("verbose".into(), json!(true));
    let mut h = spawn_with(SessionSettings { options, ..Default::default() }).await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert!(text_of(&evs).contains("mode=ask] verbose=on You said: x"), "{}", text_of(&evs));
}

#[tokio::test]
async fn invalid_settings_rejected_at_spawn() {
    let info = info().await;
    for s in [
        SessionSettings { model: Some("nope".into()), ..Default::default() },
        SessionSettings { model: Some("mock-small".into()), thinking: Some("low".into()), ..Default::default() },
        SessionSettings { thinking: Some("medium".into()), ..Default::default() },
        SessionSettings { mode: Some("bogus".into()), ..Default::default() },
    ] {
        assert!(MockAdapter::new().spawn(request(info.clone(), s.clone(), None)).await.is_err(), "{s:?}");
    }
}

#[tokio::test]
async fn disabled_mock_refuses_to_spawn() {
    let info = info().await;
    let mut req = request(info, SessionSettings::default(), None);
    req.config.enabled = false;
    assert!(MockAdapter::new().spawn(req).await.is_err());
}

// ---------- 权限 ----------

#[tokio::test]
async fn permission_keyword_allow_runs_tool() {
    let mut h = spawn_default().await;
    let mut seen: Vec<PendingPermission> = Vec::new();
    let evs = run_turn(&mut h, "run permission", |p| {
        seen.push(p.clone());
        PermissionDecision { option_id: Some("allow".into()), ..Default::default() }
    })
    .await;
    assert_eq!(seen.len(), 1);
    let p = &seen[0];
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.request_id, "sess_test-perm-1");
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!(tool.tool, "Bash");
    assert_eq!(tool.category, ToolCategory::Shell);
    assert_eq!(tool.input, json!({"command": "echo hi"}));
    assert_eq!(tool.detail.as_ref().unwrap().command.as_deref(), Some("echo hi"));
    assert_eq!(tool.cwd.as_deref(), Some("/tmp/mock-project"));
    let opts: Vec<(&str, PermissionOptionKind, bool)> =
        p.options.iter().map(|o| (o.id.as_str(), o.kind, o.needs_message)).collect();
    assert_eq!(
        opts,
        vec![
            ("allow", PermissionOptionKind::AllowOnce, false),
            ("allow_session", PermissionOptionKind::AllowAlways, false),
            ("deny", PermissionOptionKind::RejectOnce, true),
        ]
    );
    let names = type_names(&evs);
    assert_eq!(&names[names.len() - 5..], ["PERMISSION", "tool_call", "tool_output_delta", "tool_result", "turn_done"]);
    let calls: Vec<&Event> = events_of(&evs)
        .into_iter()
        .filter(|e| matches!(e, Event::ToolCall { .. } | Event::ToolResult { .. }))
        .collect();
    match (&calls[0], &calls[1]) {
        (Event::ToolCall { id: a, .. }, Event::ToolResult { id: b, is_error, output, detail }) => {
            assert_eq!(a, b);
            assert!(!is_error);
            assert_eq!(output, &json!("hi\n"));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn big_keyword_emits_one_huge_tool_result_without_asking() {
    for mode in ["ask", "yolo", "plan"] {
        let mut h = spawn_with(SessionSettings { mode: Some(mode.into()), ..Default::default() }).await;
        let evs = run_turn(&mut h, "give me something BIG", no_permission).await;
        let names = type_names(&evs);
        // 回显之后、turn_done 之前；没有 tool_output_delta
        assert_eq!(
            &names[names.len() - 4..],
            ["text_done", "tool_call", "tool_result", "turn_done"],
            "{mode}: {names:?}"
        );
        assert!(!names.contains(&"tool_output_delta".to_string()));
        let calls: Vec<&Event> = events_of(&evs)
            .into_iter()
            .filter(|e| matches!(e, Event::ToolCall { .. } | Event::ToolResult { .. }))
            .collect();
        match (&calls[0], &calls[1]) {
            (
                Event::ToolCall { id: a, name, category, input, .. },
                Event::ToolResult { id: b, is_error, output, detail },
            ) => {
                assert_eq!(a, b);
                assert_eq!(a, "sess_test-tool-1");
                assert_eq!((name.as_str(), *category), ("Bash", ToolCategory::Shell));
                assert_eq!(input, &json!({"command": "cat big.log"}));
                assert!(!is_error);
                assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
                let text = output.as_str().expect("a string output");
                assert!(text.starts_with("BIGSTART\n") && text.ends_with("BIGEND"));
                assert!(text.is_ascii());
                assert!((299_000..=301_000).contains(&text.len()), "{}", text.len());
                // 确定性：每次一样
                assert_eq!(text, anycon_adapters::mock::big_output_text());
            }
            other => panic!("{other:?}"),
        }
        // 不含 big 的 prompt 没有这些事件
        let evs = run_turn(&mut h, "hello", no_permission).await;
        assert!(!type_names(&evs).contains(&"tool_call".to_string()));
    }
}

#[tokio::test]
async fn run_keyword_asks_only_in_ask_mode() {
    // ask 模式：run 触发权限
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "please run it", choose("allow")).await;
    assert!(type_names(&evs).contains(&"tool_call".to_string()));

    // plan 模式：run 不触发任何东西
    let mut h = spawn_with(SessionSettings { mode: Some("plan".into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "please run it", no_permission).await;
    assert!(!type_names(&evs).contains(&"tool_call".to_string()));

    // yolo 模式：不问，直接跑
    let mut h = spawn_with(SessionSettings { mode: Some("yolo".into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "please run it", no_permission).await;
    assert!(type_names(&evs).contains(&"tool_result".to_string()));

    // permission 关键词在任何模式下都问
    let mut h = spawn_with(SessionSettings { mode: Some("yolo".into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "permission", choose("allow")).await;
    assert!(type_names(&evs).contains(&"PERMISSION".to_string()));
}

#[tokio::test]
async fn deny_skips_tool_and_says_skipped() {
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "permission please", |_| PermissionDecision {
        option_id: Some("deny".into()),
        message: Some("no thanks".into()),
        ..Default::default()
    })
    .await;
    assert!(!type_names(&evs).contains(&"tool_call".to_string()));
    assert!(text_of(&evs).ends_with("skipped"));
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
}

#[tokio::test]
async fn allow_session_silences_later_requests() {
    let mut h = spawn_default().await;
    let e1 = run_turn(&mut h, "permission", choose("allow_session")).await;
    assert!(type_names(&e1).contains(&"tool_result".to_string()));
    let e2 = run_turn(&mut h, "permission again", no_permission).await;
    assert!(type_names(&e2).contains(&"tool_result".to_string()));
    assert!(!type_names(&e2).contains(&"PERMISSION".to_string()));
}

#[tokio::test]
async fn plan_keyword_emits_plan_and_plan_permission() {
    let mut h = spawn_default().await;
    let mut kinds = Vec::new();
    let evs = run_turn(&mut h, "make a plan", |p| {
        kinds.push((p.kind, p.detail.text.clone(), p.options.iter().map(|o| o.id.clone()).collect::<Vec<_>>()));
        PermissionDecision { option_id: Some("allow".into()), ..Default::default() }
    })
    .await;
    assert_eq!(kinds.len(), 1);
    assert_eq!(kinds[0].0, PermissionKind::Plan);
    assert!(kinds[0].1.as_deref().unwrap().starts_with("## Plan"));
    assert_eq!(kinds[0].2, vec!["allow", "deny"]);
    let plan = events_of(&evs)
        .into_iter()
        .find_map(|e| if let Event::Plan { entries } = e { Some(entries.clone()) } else { None })
        .unwrap();
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0].status, PlanStatus::Completed);
    assert_eq!(plan[1].status, PlanStatus::InProgress);
    assert_eq!(plan[2].status, PlanStatus::Pending);
    assert!(text_of(&evs).ends_with("Plan approved."));

    // 拒绝并带意见
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "plan", |_| PermissionDecision {
        option_id: Some("deny".into()),
        message: Some("add tests".into()),
        ..Default::default()
    })
    .await;
    assert!(text_of(&evs).ends_with("Plan rejected: add tests"));
}

#[tokio::test]
async fn question_keyword_asks_and_echoes_answers() {
    let mut h = spawn_default().await;
    let mut seen = None;
    let evs = run_turn(&mut h, "a question for you", |p| {
        seen = Some(p.clone());
        let mut answers = serde_json::Map::new();
        answers.insert("q1".into(), json!("b"));
        PermissionDecision { answers: Some(answers), ..Default::default() }
    })
    .await;
    let p = seen.unwrap();
    assert_eq!(p.kind, PermissionKind::Question);
    let q = &p.detail.questions[0];
    assert_eq!(q.id, "q1");
    assert_eq!(q.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
    assert!(q.free_text && !q.multi);
    assert!(text_of(&evs).ends_with("You answered: q1=b"));

    // 跳过
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "question", choose("deny")).await;
    assert!(text_of(&evs).ends_with("Question skipped."));

    // 多选答案（数组）用 `", "` 连接，和 claude-code adapter 一致；多个题目之间用 `"; "`
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "question", |_| {
        let mut answers = serde_json::Map::new();
        answers.insert("q1".into(), json!(["a", "b"]));
        answers.insert("q2".into(), json!("free text"));
        PermissionDecision { answers: Some(answers), ..Default::default() }
    })
    .await;
    assert!(text_of(&evs).ends_with("You answered: q1=a, b; q2=free text"), "{}", text_of(&evs));
}

#[tokio::test]
async fn error_keyword_emits_error_and_error_reason() {
    let mut h = spawn_default().await;
    let evs = run_turn(&mut h, "trigger an error", no_permission).await;
    let names = type_names(&evs);
    assert_eq!(&names[names.len() - 2..], ["error", "turn_done"]);
    match events_of(&evs).into_iter().find(|e| matches!(e, Event::Error { .. })).unwrap() {
        Event::Error { code, recoverable, .. } => {
            assert_eq!(code, "mock_error");
            assert!(*recoverable);
        }
        _ => unreachable!(),
    }
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Error);
}

// ---------- write：mock 唯一会写文件的关键词 ----------

const WRITE_NAME: &str = "anycon-mock.txt";

/// 取 `tool_result` 的 (output 文本, is_error, unified_diff)：只看最后一个 tool_result。
fn last_tool_result(evs: &[AgentEvent]) -> (String, bool, Option<String>) {
    match events_of(evs).into_iter().rev().find(|e| matches!(e, Event::ToolResult { .. })) {
        Some(Event::ToolResult { output, is_error, detail, .. }) => (
            output.as_str().expect("a string output").to_string(),
            *is_error,
            detail.as_ref().and_then(|d| d.unified_diff.clone()),
        ),
        _ => panic!("no tool_result in {:?}", type_names(evs)),
    }
}

fn dir_entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[test]
fn write_constants_match_the_readme() {
    assert_eq!(anycon_adapters::mock::WRITE_FILE_NAME, WRITE_NAME);
    assert_eq!(anycon_adapters::mock::write_line(7), "mock write turn 7");
}

#[tokio::test]
async fn write_keyword_creates_then_appends() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(WRITE_NAME);
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let _ = next(&mut h).await; // Started

    // 第 1 个 turn：新建
    let evs = run_turn(&mut h, "write", no_permission).await;
    assert_eq!(
        type_names(&evs),
        vec![
            "thinking_delta",
            "thinking_delta",
            "thinking_done",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_done",
            "tool_call",
            "tool_result",
            "file_changed",
            "turn_done"
        ]
    );
    let wev = events_of(&evs);
    match (&wev[7], &wev[8], &wev[9]) {
        (
            Event::ToolCall { id: call_id, name, category, title, input, detail },
            Event::ToolResult { id: result_id, output, is_error, detail: rdetail },
            Event::FileChanged { path, kind },
        ) => {
            assert_eq!(call_id, "sess_test-tool-1");
            assert_eq!(result_id, call_id);
            assert_eq!((name.as_str(), *category), ("Write", ToolCategory::FileEdit));
            assert_eq!(title, "Write anycon-mock.txt");
            assert_eq!(input, &json!({"path": WRITE_NAME, "content": "mock write turn 1\n"}));
            let d = detail.as_ref().expect("tool_call detail");
            assert_eq!(d.path.as_deref(), Some(WRITE_NAME));
            assert_eq!(d.edit_kind, Some(EditKind::Write));
            assert_eq!(output, &json!("Created anycon-mock.txt"));
            assert!(!is_error);
            assert_eq!(
                rdetail.as_ref().unwrap().unified_diff.as_deref(),
                Some("--- /dev/null\n+++ b/anycon-mock.txt\n@@ -0,0 +1,1 @@\n+mock write turn 1\n")
            );
            assert_eq!((path.as_str(), *kind), (WRITE_NAME, FileChangeKind::Created));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mock write turn 1\n");

    // 第 2 个 turn：追加一行，kind = modified，diff 带上一行做上下文
    let evs = run_turn(&mut h, "write again", no_permission).await;
    let (output, is_error, diff) = last_tool_result(&evs);
    assert_eq!((output.as_str(), is_error), ("Modified anycon-mock.txt", false));
    assert_eq!(
        diff.as_deref(),
        Some("--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -1,1 +1,2 @@\n mock write turn 1\n+mock write turn 2\n")
    );
    assert!(events_of(&evs)
        .iter()
        .any(|e| matches!(e, Event::FileChanged { path, kind: FileChangeKind::Modified } if path == WRITE_NAME)));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mock write turn 1\nmock write turn 2\n");
    // 工具 id 在会话里接着往下编号
    assert!(events_of(&evs).iter().any(|e| matches!(e, Event::ToolCall { id, .. } if id == "sess_test-tool-2")));

    // 上下文最多 3 行
    run_turn(&mut h, "write", no_permission).await;
    run_turn(&mut h, "write", no_permission).await;
    let evs = run_turn(&mut h, "write", no_permission).await;
    assert_eq!(
        last_tool_result(&evs).2.as_deref(),
        Some(
            "--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -2,3 +2,4 @@\n mock write turn 2\n mock write turn 3\n mock write turn 4\n+mock write turn 5\n"
        )
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        (1..=5).map(|n| format!("mock write turn {n}\n")).collect::<String>()
    );
    // 只写了这一个文件
    assert_eq!(dir_entries(dir.path()), vec![WRITE_NAME.to_string()]);
}

#[tokio::test]
async fn write_keyword_ignores_the_mode_and_never_asks() {
    for mode in ["ask", "plan", "yolo"] {
        let dir = tempfile::tempdir().unwrap();
        let mut h = spawn_in(dir.path(), SessionSettings { mode: Some(mode.into()), ..Default::default() }).await;
        let evs = run_turn(&mut h, "write", no_permission).await;
        let names = type_names(&evs);
        assert_eq!(&names[names.len() - 4..], ["tool_call", "tool_result", "file_changed", "turn_done"], "{mode}");
        assert_eq!(std::fs::read_to_string(dir.path().join(WRITE_NAME)).unwrap(), "mock write turn 1\n", "{mode}");
    }
}

#[tokio::test]
async fn write_combines_with_the_other_keywords_in_fixed_order() {
    // permission：先走 Bash 的权限请求（步骤 5），write 在它之后；答复只管 Bash
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write permission", choose("allow")).await;
    let names = type_names(&evs);
    assert_eq!(
        &names[names.len() - 8..],
        [
            "PERMISSION",
            "tool_call",
            "tool_output_delta",
            "tool_result",
            "tool_call",
            "tool_result",
            "file_changed",
            "turn_done"
        ]
    );
    let ids: Vec<String> = events_of(&evs)
        .into_iter()
        .filter_map(|e| if let Event::ToolCall { id, name, .. } = e { Some(format!("{id}:{name}")) } else { None })
        .collect();
    assert_eq!(ids, vec!["sess_test-tool-1:Bash", "sess_test-tool-2:Write"]);
    assert_eq!(std::fs::read_to_string(dir.path().join(WRITE_NAME)).unwrap(), "mock write turn 1\n");

    // 拒绝 Bash 的请求：Bash 被跳过（"skipped"），write 照常写（它不依赖那个答复）
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write permission", choose("deny")).await;
    let names = type_names(&evs);
    assert_eq!(
        &names[names.len() - 7..],
        ["PERMISSION", "text_delta", "text_done", "tool_call", "tool_result", "file_changed", "turn_done"]
    );
    assert!(text_of(&evs).ends_with("skipped"));
    assert_eq!(std::fs::read_to_string(dir.path().join(WRITE_NAME)).unwrap(), "mock write turn 1\n");

    // big 在 write 之前；error 在 write 之后，turn 以 error 结束
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write big error", no_permission).await;
    let names = type_names(&evs);
    assert_eq!(
        &names[names.len() - 7..],
        ["tool_call", "tool_result", "tool_call", "tool_result", "file_changed", "error", "turn_done"]
    );
    let calls: Vec<String> = events_of(&evs)
        .into_iter()
        .filter_map(|e| if let Event::ToolCall { name, .. } = e { Some(name.clone()) } else { None })
        .collect();
    assert_eq!(calls, vec!["Bash", "Write"]);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Error);
    assert_eq!(std::fs::read_to_string(dir.path().join(WRITE_NAME)).unwrap(), "mock write turn 1\n");
}

#[tokio::test]
async fn other_keywords_still_never_touch_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    for prompt in ["hello", "run permission", "big output", "make a plan", "trigger an error"] {
        run_turn(&mut h, prompt, choose("allow")).await;
    }
    assert!(dir_entries(dir.path()).is_empty());
    // 关键词不区分大小写、按子串匹配（"Overwrite" 也算）
    run_turn(&mut h, "Overwrite it", no_permission).await;
    assert_eq!(dir_entries(dir.path()), vec![WRITE_NAME.to_string()]);
}

#[tokio::test]
async fn write_appends_to_a_file_that_has_no_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(WRITE_NAME);
    // 没有结尾换行：新内容独占一行，diff 里最后一行按 git 的写法改写
    std::fs::write(&file, "a\nb\nc\nd\nexisting").unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write", no_permission).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "a\nb\nc\nd\nexisting\nmock write turn 1\n");
    assert_eq!(
        last_tool_result(&evs).2.as_deref(),
        Some(
            "--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -2,4 +2,5 @@\n b\n c\n d\n-existing\n\\ No newline at end of file\n+existing\n+mock write turn 1\n"
        )
    );
    // 只有一行、没有换行
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(WRITE_NAME);
    std::fs::write(&file, "only").unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write", no_permission).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "only\nmock write turn 1\n");
    assert_eq!(
        last_tool_result(&evs).2.as_deref(),
        Some(
            "--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -1,1 +1,2 @@\n-only\n\\ No newline at end of file\n+only\n+mock write turn 1\n"
        )
    );
    // 已存在的空文件：算 modified，diff 从 0 行开始
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(WRITE_NAME);
    std::fs::write(&file, "").unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write", no_permission).await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "mock write turn 1\n");
    assert_eq!(
        last_tool_result(&evs).2.as_deref(),
        Some("--- a/anycon-mock.txt\n+++ b/anycon-mock.txt\n@@ -0,0 +1,1 @@\n+mock write turn 1\n")
    );
    assert!(events_of(&evs).iter().any(|e| matches!(e, Event::FileChanged { kind: FileChangeKind::Modified, .. })));
}

/// 写不了的情况：`tool_result { is_error: true }`，没有 `file_changed`，turn 照常 `completed`，目标一个字节都没变。
async fn assert_write_refused(dir: &Path, expect_in_output: &str) {
    let mut h = spawn_in(dir, SessionSettings::default()).await;
    let evs = run_turn(&mut h, "write", no_permission).await;
    let names = type_names(&evs);
    assert_eq!(&names[names.len() - 3..], ["tool_call", "tool_result", "turn_done"], "{names:?}");
    let (output, is_error, diff) = last_tool_result(&evs);
    assert!(is_error, "{output}");
    assert!(
        output.starts_with("refusing to write anycon-mock.txt") || output.starts_with("anycon-mock.txt:"),
        "{output}"
    );
    assert!(output.contains(expect_in_output), "{output}");
    assert!(diff.is_none());
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn write_refuses_to_follow_a_symbolic_link() {
    use std::os::unix::fs::symlink;
    // 指向已有文件的链接：不写目标，链接保持原样
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, "keep\n").unwrap();
    symlink(&victim, dir.path().join(WRITE_NAME)).unwrap();
    assert_write_refused(dir.path(), "symbolic link").await;
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep\n");
    assert!(std::fs::symlink_metadata(dir.path().join(WRITE_NAME)).unwrap().file_type().is_symlink());

    // 悬空链接：不会借链接把目标文件创建出来
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("not-yet.txt");
    symlink(&target, dir.path().join(WRITE_NAME)).unwrap();
    assert_write_refused(dir.path(), "symbolic link").await;
    assert!(!target.exists());

    // 指向目录外的链接
    let outside = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    symlink(outside.path().join("elsewhere.txt"), dir.path().join(WRITE_NAME)).unwrap();
    assert_write_refused(dir.path(), "symbolic link").await;
    assert!(dir_entries(outside.path()).is_empty());
}

#[tokio::test]
async fn write_refuses_what_is_not_a_small_utf8_regular_file() {
    // 目标是目录
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(WRITE_NAME)).unwrap();
    assert_write_refused(dir.path(), "not a regular file").await;

    // 已有内容太大（> 64 KiB）
    let dir = tempfile::tempdir().unwrap();
    let big = vec![b'x'; 64 * 1024 + 1];
    std::fs::write(dir.path().join(WRITE_NAME), &big).unwrap();
    assert_write_refused(dir.path(), "64 KiB").await;
    assert_eq!(std::fs::read(dir.path().join(WRITE_NAME)).unwrap(), big);

    // 不是 UTF-8 文本
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(WRITE_NAME), [0xff, 0xfe, 0x00]).unwrap();
    assert_write_refused(dir.path(), "UTF-8").await;
    assert_eq!(std::fs::read(dir.path().join(WRITE_NAME)).unwrap(), vec![0xff, 0xfe, 0x00]);

    // cwd 不存在：I/O 错误原样报出，也不会把 cwd 建出来
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone");
    assert_write_refused(&missing, "No such file").await;
    assert!(!missing.exists());
}

#[tokio::test]
async fn write_can_be_cancelled_before_it_touches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = spawn_in(dir.path(), SessionSettings::default()).await;
    h.send(input("slow write")).await.unwrap();
    // 回显之前就取消：什么都没写
    let _ = next(&mut h).await; // Started
    let _ = next(&mut h).await; // 第一个事件（慢速下每个事件前停 200ms）
    h.cancel().await.unwrap();
    loop {
        if let AgentEvent::Event(Event::TurnDone { reason, .. }) = next(&mut h).await {
            assert_eq!(reason, TurnDoneReason::Cancelled);
            break;
        }
    }
    assert!(dir_entries(dir.path()).is_empty());
}

// ---------- 取消、并发、关闭 ----------

#[tokio::test]
async fn cancel_during_slow_turn_ends_with_cancelled() {
    let mut h = spawn_default().await;
    let _ = next(&mut h).await;
    h.send(input("slow response")).await.unwrap();
    assert!(h.is_busy());
    // 等第一个事件，确认 turn 在跑
    let first = next(&mut h).await;
    assert!(matches!(first, AgentEvent::Event(Event::ThinkingDelta { .. })));
    let started = std::time::Instant::now();
    h.cancel().await.unwrap();
    h.cancel().await.unwrap(); // 幂等
    let last = loop {
        let ev = next(&mut h).await;
        if let AgentEvent::Event(Event::TurnDone { reason, usage, .. }) = ev {
            break Some((reason, usage));
        }
    };
    assert!(started.elapsed() < Duration::from_millis(1000), "cancel was not prompt: {:?}", started.elapsed());
    let (reason, usage) = last.unwrap();
    assert_eq!(reason, TurnDoneReason::Cancelled);
    assert!(usage.is_some());
    assert!(!h.is_busy());
    // 取消后能继续用
    let evs = run_turn(&mut h, "again", no_permission).await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    // 空闲时 cancel 是 no-op
    h.cancel().await.unwrap();
}

#[tokio::test]
async fn cancel_while_waiting_for_permission() {
    let mut h = spawn_default().await;
    h.send(input("permission")).await.unwrap();
    let rid = loop {
        if let AgentEvent::Permission(p) = next(&mut h).await {
            break p.request_id;
        }
    };
    h.cancel().await.unwrap();
    let mut reason = None;
    while reason.is_none() {
        if let AgentEvent::Event(Event::TurnDone { reason: r, .. }) = next(&mut h).await {
            reason = Some(r);
        }
    }
    assert_eq!(reason, Some(TurnDoneReason::Cancelled));
    // daemon 随后自动 deny 未决请求：必须是 no-op 而不是报错
    h.respond_permission(&rid, PermissionDecision { option_id: Some("deny".into()), ..Default::default() })
        .await
        .unwrap();
}

#[tokio::test]
async fn send_during_turn_is_unsupported() {
    let mut h = spawn_default().await;
    h.send(input("slow")).await.unwrap();
    let err = h.send(input("second")).await.unwrap_err();
    assert!(matches!(err, anycon_agent_api::AdapterError::Unsupported(_)), "{err:?}");
    h.cancel().await.unwrap();
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_is_unsupported_and_never_opens_a_turn() {
    let mut h = spawn_default().await;
    let interject = |text: &str| UserInput { interject: true, ..input(text) };
    // 没有进行中的 turn
    let err = h.send(interject("hello")).await.unwrap_err();
    assert!(matches!(err, anycon_agent_api::AdapterError::Unsupported(_)), "{err:?}");
    assert!(!h.is_busy());
    // turn 进行中
    h.send(input("slow")).await.unwrap();
    let err = h.send(interject("more")).await.unwrap_err();
    assert!(matches!(err, anycon_agent_api::AdapterError::Unsupported(_)), "{err:?}");
    h.cancel().await.unwrap();
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_emits_exited_then_closes_channel() {
    let mut h = spawn_default().await;
    let _ = next(&mut h).await;
    h.shutdown().await.unwrap();
    h.shutdown().await.unwrap(); // 幂等
    match next(&mut h).await {
        AgentEvent::Exited { code, error } => {
            assert_eq!(code, Some(0));
            assert!(error.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(h.events().recv().await.is_none());
    assert!(h.send(input("after")).await.is_err());
}

#[tokio::test]
async fn shutdown_mid_turn_cancels_it_first() {
    let mut h = spawn_default().await;
    let _ = next(&mut h).await;
    h.send(input("slow")).await.unwrap();
    h.shutdown().await.unwrap();
    let mut names = Vec::new();
    while let Some(ev) = h.events().recv().await {
        names.push(match ev {
            AgentEvent::Event(Event::TurnDone { reason, .. }) => format!("turn_done:{reason:?}"),
            AgentEvent::Exited { .. } => "exited".to_string(),
            _ => "other".to_string(),
        });
    }
    let n = names.len();
    assert_eq!(&names[n - 2..], ["turn_done:Cancelled", "exited"]);
}

// ---------- 设置与续接 ----------

#[tokio::test]
async fn set_settings_applies_immediately() {
    let mut h = spawn_default().await;
    let e1 = run_turn(&mut h, "first", no_permission).await;
    assert!(text_of(&e1).contains("[model=mock-large thinking=high mode=ask]"));

    // 改 thinking（set_settings 收到的是完整目标设置，所以其它字段都要写上）
    let r = h.set_settings(&SessionSettings { thinking: Some("low".into()), ..Default::default() }).await.unwrap();
    assert!(r.applied_now && !r.restart_required);
    let e2 = run_turn(&mut h, "second", no_permission).await;
    assert!(text_of(&e2).contains("[model=mock-large thinking=low mode=ask]"));

    // 换到无 thinking 的模型，thinking 回落为 none；mode 同时改
    h.set_settings(&SessionSettings {
        model: Some("mock-small".into()),
        mode: Some("yolo".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let e3 = run_turn(&mut h, "third", no_permission).await;
    assert!(text_of(&e3).contains("[model=mock-small thinking=none mode=yolo]"));
    assert!(!type_names(&e3).iter().any(|t| t.starts_with("thinking")));

    // 换回大模型，thinking 回到该模型的默认
    h.set_settings(&SessionSettings {
        model: Some("mock-large".into()),
        mode: Some("yolo".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let e4 = run_turn(&mut h, "fourth", no_permission).await;
    assert!(text_of(&e4).contains("[model=mock-large thinking=high mode=yolo]"));

    // 非法值被拒绝且不改变现有设置
    assert!(h.set_settings(&SessionSettings { model: Some("nope".into()), ..Default::default() }).await.is_err());
    assert!(h.set_settings(&SessionSettings { mode: Some("nope".into()), ..Default::default() }).await.is_err());
    let e5 = run_turn(&mut h, "fifth", no_permission).await;
    assert!(text_of(&e5).contains("[model=mock-large thinking=high mode=yolo]"));
}

#[tokio::test]
async fn set_settings_is_the_complete_target_so_missing_fields_are_cleared() {
    let mut options = serde_json::Map::new();
    options.insert("verbose".into(), json!(true));
    let full = SessionSettings {
        model: Some("mock-large".into()),
        thinking: Some("low".into()),
        mode: Some("plan".into()),
        options,
        system_prompt_append: Some("Be terse.".into()),
        max_turns: Some(3),
        max_budget_usd: Some(1.5),
        ..Default::default()
    };
    let mut h = spawn_with(full.clone()).await;
    let e1 = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(
        text_of(&e1),
        "Mock reply: [model=mock-large thinking=low mode=plan] verbose=on prompt_append=Be terse. max_turns=3 budget=1.5 You said: x"
    );

    // 清掉 system_prompt_append / max_* / verbose：回显里不再出现，模型 / 思考 / 模式按给的值
    let cleared = SessionSettings {
        model: full.model.clone(),
        thinking: full.thinking.clone(),
        mode: full.mode.clone(),
        ..Default::default()
    };
    let r = h.set_settings(&cleared).await.unwrap();
    assert!(r.applied_now && !r.restart_required);
    let e2 = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&e2), "Mock reply: [model=mock-large thinking=low mode=plan] You said: x");
    for k in ["verbose", "prompt_append", "max_turns", "budget"] {
        assert!(!text_of(&e2).contains(k), "{k} survived the clear: {}", text_of(&e2));
    }
    let la = h.resume_handle().unwrap().launch_args;
    for k in ["system_prompt_append", "max_turns", "max_budget_usd"] {
        assert!(la[k].is_null(), "{k}: {la}");
    }
    assert_eq!(la["verbose"], false);

    // 什么都不给 = 全部回到默认（模型 → mock-large，思考 → high，模式 → ask），不沿用当前值
    h.set_settings(&SessionSettings::default()).await.unwrap();
    let e3 = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&e3), "Mock reply: [model=mock-large thinking=high mode=ask] You said: x");

    // 再设回去：值回来了，不是上次清掉之前的残留
    h.set_settings(&SessionSettings { system_prompt_append: Some("again".into()), ..Default::default() })
        .await
        .unwrap();
    let e4 = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&e4), "Mock reply: [model=mock-large thinking=high mode=ask] prompt_append=again You said: x");

    // 非法的目标设置被拒绝，现有设置（含 prompt_append）不变
    let bad = SessionSettings { max_turns: Some(9), mode: Some("nope".into()), ..Default::default() };
    assert!(h.set_settings(&bad).await.is_err());
    let e5 = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&e5), "Mock reply: [model=mock-large thinking=high mode=ask] prompt_append=again You said: x");
}

#[tokio::test]
async fn prompt_append_echo_is_clipped_to_32_chars_on_one_line() {
    let long = "0123456789abcdefghijklmnopqrstuvwxyz";
    let mut h = spawn_with(SessionSettings { system_prompt_append: Some(long.into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert!(text_of(&evs).contains(" prompt_append=0123456789abcdefghijklmnopqrstuv You said: x"), "{}", text_of(&evs));

    let mut h =
        spawn_with(SessionSettings { system_prompt_append: Some("a\nb\tc\r\nd".into()), ..Default::default() }).await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert!(text_of(&evs).contains(" prompt_append=a b c  d You said: x"), "{}", text_of(&evs));

    // 只设预算 / 只设轮数：只出现对应的一项
    let mut h = spawn_with(SessionSettings { max_budget_usd: Some(2.0), ..Default::default() }).await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&evs), "Mock reply: [model=mock-large thinking=high mode=ask] budget=2 You said: x");
    let mut h = spawn_with(SessionSettings { max_turns: Some(7), ..Default::default() }).await;
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert_eq!(text_of(&evs), "Mock reply: [model=mock-large thinking=high mode=ask] max_turns=7 You said: x");
}

#[tokio::test]
async fn additional_models_from_the_config_are_selectable() {
    use anycon_protocol::{Choice, ModelInfo, ThinkingOptions};
    let extra = ModelInfo {
        id: "mock-custom".into(),
        label: "Mock Custom".into(),
        description: None,
        default: false,
        thinking: Some(ThinkingOptions {
            options: vec![Choice { id: "deep".into(), label: "Deep".into(), description: None }],
            default: Some("deep".into()),
        }),
        tags: vec![],
    };
    let cfg = AdapterConfig { enabled: true, additional_models: vec![extra], ..Default::default() };
    let mut ctx = probe_ctx(true);
    ctx.config = cfg.clone();
    let info = Arc::new(MockAdapter::new().probe(&ctx).await);
    assert!(info.models.iter().any(|m| m.id == "mock-custom"));

    let mut req =
        request(info.clone(), SessionSettings { model: Some("mock-custom".into()), ..Default::default() }, None);
    req.config = cfg;
    let mut h = MockAdapter::new().spawn(req).await.expect("an additional model can be spawned with");
    // 它自己的 thinking 默认值
    let evs = run_turn(&mut h, "x", no_permission).await;
    assert!(text_of(&evs).contains("[model=mock-custom thinking=deep mode=ask]"), "{}", text_of(&evs));
    // set_settings 也认它；换回内置模型之后不再沿用 deep
    h.set_settings(&SessionSettings { model: Some("mock-small".into()), ..Default::default() }).await.unwrap();
    h.set_settings(&SessionSettings {
        model: Some("mock-custom".into()),
        thinking: Some("deep".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    // 配置里没有的模型仍然被拒绝
    assert!(h.set_settings(&SessionSettings { model: Some("nope".into()), ..Default::default() }).await.is_err());
}

#[tokio::test]
async fn resume_keeps_the_same_agent_session_id() {
    let info = info().await;
    let mut first = MockAdapter::new().spawn(request(info.clone(), SessionSettings::default(), None)).await.unwrap();
    let id1 = match next(&mut first).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("{other:?}"),
    };
    first.shutdown().await.unwrap();

    let mut second = MockAdapter::new()
        .spawn(request(info, SessionSettings { mode: Some("yolo".into()), ..Default::default() }, Some(id1.clone())))
        .await
        .unwrap();
    match next(&mut second).await {
        AgentEvent::Started { resume } => assert_eq!(resume.agent_session_id, id1.agent_session_id),
        other => panic!("{other:?}"),
    }
    assert_eq!(second.resume_handle().unwrap().agent_session_id, id1.agent_session_id);
    // 续接后设置按新请求生效
    let evs = run_turn(&mut second, "hi", no_permission).await;
    assert!(text_of(&evs).contains("mode=yolo"));
}

#[tokio::test]
async fn respond_to_unknown_request_is_a_noop() {
    let mut h = spawn_default().await;
    h.respond_permission("nope", PermissionDecision::default()).await.unwrap();
}

#[tokio::test]
async fn probe_status_values() {
    let off = MockAdapter::new().probe(&probe_ctx(false)).await;
    assert_eq!(off.status, AgentStatus::Disabled);
    let on = MockAdapter::new().probe(&probe_ctx(true)).await;
    assert_eq!(on.status, AgentStatus::Ready);
}

#[tokio::test]
async fn diagnose_entry_point_works_for_mock_and_unknown_id() {
    let ctx = probe_ctx(true);
    let (text, fields) = anycon_adapters::diagnose("mock", &ctx).await.expect("mock is diagnosable");
    assert!(text.contains("Mock Agent"));
    assert!(fields.iter().any(|f| f.label == "models" && f.value == "2"));
    assert!(anycon_adapters::diagnose("nope", &ctx).await.is_none());
}
