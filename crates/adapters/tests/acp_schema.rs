//! 用官方 ACP v1 的 JSON Schema（`tests/fixtures/acp/schema.v1.json`，原样拷贝，见同目录的 README）校验 adapter 发给 agent 的
//! **每一条消息**：请求 / 通知的 `params`，以及对 agent 请求（权限、elicitation）的答复。字段名和形状以 schema 为准，不凭记忆。
//!
//! 对着假 ACP agent 跑一遍覆盖各种请求的场景，再把假 agent 记下的全部 stdin 消息逐条过 schema。
//! 假 CLI 是带 shebang 的 python 脚本，Windows 上不能直接执行：这个文件只在 unix 上跑。
#![cfg(unix)]

mod common;

use std::collections::BTreeSet;

use anycon_agent_api::{AgentEvent, AgentHandle, PermissionDecision};
use anycon_protocol::SessionSettings;
use common::*;
use serde_json::{json, Value};

fn schema() -> Value {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp/schema.v1.json");
    serde_json::from_str(&std::fs::read_to_string(p).expect("schema fixture")).expect("schema json")
}

fn check(defs: &Value, def: &str, instance: &Value) -> Vec<String> {
    let doc = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": defs,
        "$ref": format!("#/$defs/{def}"),
    });
    let validator = jsonschema::validator_for(&doc).unwrap_or_else(|e| panic!("{def}: bad schema: {e}"));
    validator.iter_errors(instance).map(|e| format!("{} (at {})", e, e.instance_path())).collect()
}

/// 假 agent 记下的每条 stdin 消息过 schema；返回见过的 method（和答复的种类）。
fn validate_log(h: &AcpHarness) -> BTreeSet<String> {
    let s = schema();
    let defs = &s["$defs"];
    let mut seen = BTreeSet::new();
    for e in h.stdin_messages() {
        let msg = &e["msg"];
        assert_eq!(msg["jsonrpc"], "2.0", "every message carries the JSON-RPC version: {msg}");
        if let Some(method) = msg["method"].as_str() {
            let (def, notification) = match method {
                "initialize" => ("InitializeRequest", false),
                "session/new" => ("NewSessionRequest", false),
                "session/load" => ("LoadSessionRequest", false),
                "session/resume" => ("ResumeSessionRequest", false),
                "session/prompt" => ("PromptRequest", false),
                "session/close" => ("CloseSessionRequest", false),
                "session/set_mode" => ("SetSessionModeRequest", false),
                "session/set_config_option" => ("SetSessionConfigOptionRequest", false),
                "session/cancel" => ("CancelNotification", true),
                other => panic!("the adapter sent a method it should not: {other}"),
            };
            assert_eq!(msg.get("id").is_none(), notification, "{method}: notifications have no id, requests do: {msg}");
            let errs = check(defs, def, &msg["params"]);
            assert!(errs.is_empty(), "{method} params do not match {def}:\n  {}\n  {msg}", errs.join("\n  "));
            seen.insert(method.to_string());
        } else if let Some(result) = msg.get("result") {
            // 对 agent 请求的答复
            let def = if result.get("outcome").is_some() {
                "RequestPermissionResponse"
            } else if result.get("action").is_some() {
                "CreateElicitationResponse"
            } else {
                panic!("an answer of an unknown shape: {msg}")
            };
            let errs = check(defs, def, result);
            assert!(errs.is_empty(), "answer does not match {def}:\n  {}\n  {msg}", errs.join("\n  "));
            seen.insert(format!(
                "answer:{def}:{}",
                result["outcome"]["outcome"].as_str().or(result["action"].as_str()).unwrap_or("")
            ));
        } else if let Some(err) = msg.get("error") {
            let errs = check(defs, "Error", err);
            assert!(errs.is_empty(), "error does not match Error:\n  {}\n  {msg}", errs.join("\n  "));
            seen.insert("answer:error".to_string());
        } else {
            panic!("a message of an unknown shape: {msg}");
        }
    }
    seen
}

/// 假 agent 写出去的每条消息也过 schema：它得是合法的 ACP，adapter 才算对着真实形状的流量测过。
fn validate_agent_output(h: &AcpHarness) {
    let s = schema();
    let defs = &s["$defs"];
    let mut checked = 0;
    for e in h.log_entries().into_iter().filter(|e| e["kind"] == "out") {
        let msg = &e["msg"];
        if let Some(method) = msg["method"].as_str() {
            let def = match method {
                "session/update" => "SessionNotification",
                "session/request_permission" => "RequestPermissionRequest",
                "elicitation/create" => "CreateElicitationRequest",
                "fs/read_text_file" => "ReadTextFileRequest",
                other => panic!("the fake agent sent a method this test does not know: {other}"),
            };
            let errs = check(defs, def, &msg["params"]);
            assert!(
                errs.is_empty(),
                "the fake agent's {method} does not match {def}:\n  {}\n  {msg}",
                errs.join("\n  ")
            );
        } else if let Some(result) = msg.get("result") {
            let Some(method) = e["for"].as_str() else { continue };
            let def = match method {
                "initialize" => "InitializeResponse",
                "session/new" => "NewSessionResponse",
                "session/load" => "LoadSessionResponse",
                "session/resume" => "ResumeSessionResponse",
                "session/prompt" => "PromptResponse",
                "session/close" => "CloseSessionResponse",
                "session/set_mode" => "SetSessionModeResponse",
                "session/set_config_option" => "SetSessionConfigOptionResponse",
                other => panic!("the fake agent answered a method this test does not know: {other}"),
            };
            let errs = check(defs, def, result);
            assert!(
                errs.is_empty(),
                "the fake agent's {method} result does not match {def}:\n  {}\n  {msg}",
                errs.join("\n  ")
            );
        } else if let Some(err) = msg.get("error") {
            let errs = check(defs, "Error", err);
            assert!(errs.is_empty(), "the fake agent's error does not match Error:\n  {}\n  {msg}", errs.join("\n  "));
        }
        checked += 1;
    }
    assert!(checked > 5, "only {checked} messages from the agent were checked");
}

async fn started(h: &mut Box<dyn AgentHandle>) -> anycon_agent_api::ResumeHandle {
    match next_event(h).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("expected Started, got {other:?}"),
    }
}

async fn run_turn(h: &mut Box<dyn AgentHandle>, text: &str) -> Vec<AgentEvent> {
    h.send(input(text)).await.expect("send");
    collect_until(h, is_turn_done).await
}

fn accept(answers: Value) -> PermissionDecision {
    PermissionDecision { option_id: Some("accept".into()), answers: answers.as_object().cloned(), ..Default::default() }
}

#[tokio::test]
async fn the_schema_fixture_is_what_we_think_it_is() {
    let s = schema();
    assert_eq!(s["title"], "Agent Client Protocol");
    for def in [
        "InitializeRequest",
        "PromptRequest",
        "SetSessionConfigOptionRequest",
        "RequestPermissionResponse",
        "CreateElicitationResponse",
        "Error",
    ] {
        assert!(s["$defs"].get(def).is_some(), "{def}");
    }
    // 校验器不是摆设：坏的消息要被挡住
    assert!(!check(&s["$defs"], "NewSessionRequest", &json!({"cwd": "/w"})).is_empty(), "mcpServers is required");
    assert!(!check(
        &s["$defs"],
        "PromptRequest",
        &json!({"sessionId": "s", "prompt": [{"type": "image", "mimeType": "image/png"}]})
    )
    .is_empty());
    assert!(check(
        &s["$defs"],
        "PromptRequest",
        &json!({"sessionId": "s", "prompt": [{"type": "text", "text": "hi"}]})
    )
    .is_empty());
}

#[tokio::test]
async fn every_message_the_client_sends_matches_the_official_schema() {
    let h = AcpHarness::new();
    let file = h.dir.path().join("a.png");
    std::fs::write(&file, [0x89, b'P', b'N', b'G']).unwrap();

    // 新会话 + 全部设置（字符串选项、布尔选项、模型 → 思考等级、模式）
    let mut settings = SessionSettings {
        model: Some("smart".into()),
        thinking: Some("medium".into()),
        mode: Some("code".into()),
        ..Default::default()
    };
    settings.options.insert("brave".into(), json!(true));
    settings.options.insert("ctx".into(), json!("1m"));
    let mut a = h.spawn(settings, None).await;
    let first = started(&mut a).await;

    // 带图片的 prompt、普通 prompt
    let mut inp = input("images please");
    inp.attachments = vec![anycon_protocol::AttachmentRef {
        blob_id: "b".into(),
        name: "a.png".into(),
        mime: "image/png".into(),
        bytes: 4,
        width: None,
        height: None,
    }];
    inp.attachment_paths = vec![file.clone()];
    a.send(inp).await.unwrap();
    collect_until(&mut a, is_turn_done).await;
    run_turn(&mut a, "hello").await;

    // 权限：选一项、取消；elicitation：接受、拒绝、取消；agent 要我们没声明的 fs：错误答复
    for option in ["allow-always", "reject-once"] {
        a.send(input("permission")).await.unwrap();
        let p = collect_until(&mut a, is_permission).await;
        let id = match p.last() {
            Some(AgentEvent::Permission(p)) => p.request_id.clone(),
            other => panic!("{other:?}"),
        };
        a.respond_permission(&id, PermissionDecision { option_id: Some(option.into()), ..Default::default() })
            .await
            .unwrap();
        collect_until(&mut a, is_turn_done).await;
    }
    a.send(input("permission")).await.unwrap();
    collect_until(&mut a, is_permission).await;
    a.cancel().await.unwrap();
    collect_until(&mut a, is_turn_done).await;
    for decision in [
        accept(json!({"strategy": "fast", "count": "3", "note": "x"})),
        PermissionDecision { option_id: Some("decline".into()), ..Default::default() },
    ] {
        a.send(input("elicit")).await.unwrap();
        let p = collect_until(&mut a, is_permission).await;
        let id = match p.last() {
            Some(AgentEvent::Permission(p)) => p.request_id.clone(),
            other => panic!("{other:?}"),
        };
        a.respond_permission(&id, decision).await.unwrap();
        collect_until(&mut a, is_turn_done).await;
    }
    a.send(input("elicit")).await.unwrap();
    collect_until(&mut a, is_permission).await;
    a.cancel().await.unwrap();
    collect_until(&mut a, is_turn_done).await;
    run_turn(&mut a, "fsread").await;

    // 取消一个跑着的 turn；会话中改设置；脱离（session/close）
    a.send(input("hold")).await.unwrap();
    collect_until(&mut a, |e| matches!(e, AgentEvent::Event(anycon_protocol::Event::ToolCall { .. }))).await;
    a.cancel().await.unwrap();
    collect_until(&mut a, is_turn_done).await;
    let mut s2 = SessionSettings { model: Some("fast".into()), mode: Some("plan".into()), ..Default::default() };
    s2.options.insert("brave".into(), json!(false));
    a.set_settings(&s2).await.unwrap();
    let resume = a.resume_handle().unwrap();
    assert_eq!(resume.agent_session_id, first.agent_session_id);
    a.shutdown().await.unwrap();

    // 续接（session/resume）
    let mut b = h.spawn(SessionSettings::default(), Some(resume)).await;
    started(&mut b).await;
    b.shutdown().await.unwrap();

    validate_agent_output(&h);
    let seen = validate_log(&h);
    for expected in [
        "initialize",
        "session/new",
        "session/resume",
        "session/prompt",
        "session/cancel",
        "session/close",
        "session/set_config_option",
        "answer:RequestPermissionResponse:selected",
        "answer:RequestPermissionResponse:cancelled",
        "answer:CreateElicitationResponse:accept",
        "answer:CreateElicitationResponse:decline",
        "answer:CreateElicitationResponse:cancel",
        "answer:error",
    ] {
        assert!(seen.contains(expected), "the scenario never sent {expected}: {seen:?}");
    }
}

#[tokio::test]
async fn legacy_modes_and_session_load_match_the_official_schema() {
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "legacy").var("FAKE_ACP_CAPS", "load,image");
    let settings = SessionSettings { mode: Some("code".into()), ..Default::default() };
    let mut a = h.spawn(settings.clone(), None).await;
    started(&mut a).await;
    run_turn(&mut a, "hello").await;
    let resume = a.resume_handle().unwrap();
    a.shutdown().await.unwrap();
    let mut b = h.spawn(settings, Some(resume)).await;
    started(&mut b).await;
    b.shutdown().await.unwrap();
    validate_agent_output(&h);
    let seen = validate_log(&h);
    for expected in ["session/set_mode", "session/load"] {
        assert!(seen.contains(expected), "the scenario never sent {expected}: {seen:?}");
    }
}
