//! Claude Code 进程层：用 `tests/bin/fake-claude` 回放固定 JSONL，端到端测 spawn / send / respond / cancel / set_settings / shutdown。
//!
//! 假 CLI（`tests/bin/fake-*`）是带 shebang 的 python 脚本，Windows 上不能直接执行：这个文件只在 unix 上跑。
#![cfg(unix)]

mod common;

use std::time::{Duration, Instant};

use anycon_adapters::ClaudeCodeAdapter;
use anycon_agent_api::{Adapter, AdapterError, AgentEvent, AgentHandle, PermissionDecision, ResumeHandle, UserInput};
use anycon_protocol::{AttachmentRef, Event, PermissionKind, SessionSettings, TurnDoneReason};
use common::*;
use serde_json::{json, Value};

fn decision(id: &str) -> PermissionDecision {
    PermissionDecision { option_id: Some(id.into()), ..Default::default() }
}

fn turn_done(evs: &[AgentEvent]) -> (u64, TurnDoneReason) {
    match evs.iter().rev().find_map(|e| {
        if let AgentEvent::Event(Event::TurnDone { turn_id, reason, .. }) = e {
            Some((*turn_id, *reason))
        } else {
            None
        }
    }) {
        Some(x) => x,
        None => panic!("no TurnDone in {:?}", names(evs)),
    }
}

#[tokio::test]
async fn spawn_send_and_stream_text() {
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;

    let first = next_event(&mut h).await;
    let sid = match &first {
        AgentEvent::Started { resume } => resume.agent_session_id.clone(),
        other => panic!("expected Started, got {other:?}"),
    };
    assert!(uuid_like(&sid), "fresh sessions get a generated --session-id: {sid}");
    assert_eq!(h.resume_handle().unwrap().agent_session_id, sid);

    h.send(input("hello")).await.unwrap();
    assert!(h.is_busy());
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(names(&evs), vec!["text_delta", "text_delta", "text_delta", "text_done", "turn_done"]);
    assert_eq!(text_of(&evs), "Hello  world");
    assert_eq!(turn_done(&evs), (1, TurnDoneReason::Completed));
    assert!(!h.is_busy(), "busy is cleared before TurnDone is delivered");

    // 用户消息按 stream-json 格式送到 stdin
    let msgs = fx.stdin_messages();
    assert_eq!(msgs[0], json!({"type":"user","message":{"role":"user","content":"hello"},"parent_tool_use_id":null}));

    // argv：固定部分 + 显式的默认设置 + 我们生成的 session id；带值的选项一律 `--flag=value`
    let argv = fx.argv();
    assert_eq!(
        &argv[..6],
        [
            "-p",
            "--output-format=stream-json",
            "--input-format=stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool=stdio"
        ]
    );
    assert!(argv.contains(&"--model=claude-opus-5-5".to_string()));
    assert!(argv.contains(&"--effort=medium".to_string()));
    assert!(argv.contains(&"--permission-mode=default".to_string()));
    assert!(argv.contains(&format!("--session-id={sid}")));
    assert!(!argv.contains(&"--dangerously-skip-permissions".to_string()));
    for flag in ["--output-format", "--input-format", "--permission-prompt-tool", "--model", "--effort", "--session-id"]
    {
        assert!(!argv.iter().any(|a| a == flag), "{flag} must not be followed by a separate value");
    }
    let started = fx.log_entries().into_iter().find(|e| e["kind"] == "argv").unwrap();
    assert!(same_path(std::path::Path::new(started["cwd"].as_str().unwrap()), &fx.project), "cwd = req.cwd");

    // 关闭：关 stdin，进程自己退出，没有错误
    h.shutdown().await.unwrap();
    let evs = collect_until(&mut h, is_exited).await;
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => assert_eq!((*code, error.as_deref()), (Some(0), None)),
        _ => unreachable!(),
    }
    assert!(h.events().recv().await.is_none(), "channel closes after Exited");
    assert!(matches!(h.send(input("again")).await, Err(AdapterError::Exited(_))));
}

fn uuid_like(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}

#[tokio::test]
async fn two_turns_in_one_process() {
    let fx = Harness::new().script("two_turns.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await; // Started
    h.send(input("one")).await.unwrap();
    let e1 = collect_until(&mut h, is_turn_done).await;
    h.send(input("two")).await.unwrap();
    let e2 = collect_until(&mut h, is_turn_done).await;
    assert_eq!(text_of(&e1), "one");
    assert_eq!(text_of(&e2), "two");
    assert_eq!(turn_done(&e1).0, 1);
    assert_eq!(turn_done(&e2).0, 2);
    // 第二个 turn 的 system/init 带同一个 session id：不会再发 Started
    assert!(!names(&e2).contains(&"STARTED".to_string()));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn send_during_turn_is_unsupported() {
    let fx = Harness::new().script("long_turn.jsonl").var("FAKE_CLAUDE_DELAY_MS", "30");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("go")).await.unwrap();
    let err = h.send(input("second")).await.unwrap_err();
    assert!(matches!(err, AdapterError::Unsupported(_)), "{err:?}");
    h.shutdown().await.unwrap();
}

// ---------------------------------------------------------------- 图片附件（ADR-35）

fn attachment(name: &str, mime: &str, bytes: usize) -> AttachmentRef {
    AttachmentRef {
        blob_id: "0123456789abcdef0123456789abcdef".into(),
        name: name.into(),
        mime: mime.into(),
        bytes: bytes as u64,
        width: None,
        height: None,
    }
}

/// 在 harness 的临时目录里放一个文件，返回 (引用, 路径)。
fn put_file(fx: &Harness, name: &str, mime: &str, bytes: &[u8]) -> (AttachmentRef, std::path::PathBuf) {
    let path = fx.dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    (attachment(name, mime, bytes.len()), path)
}

fn with_files(text: &str, files: Vec<(AttachmentRef, std::path::PathBuf)>) -> UserInput {
    let (attachments, attachment_paths) = files.into_iter().unzip();
    UserInput { attachments, attachment_paths, ..input(text) }
}

fn user_messages(fx: &Harness) -> Vec<Value> {
    fx.stdin_messages().into_iter().filter(|m| m["type"] == "user").collect()
}

#[tokio::test]
async fn plain_text_messages_keep_the_string_content() {
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("hello there")).await.unwrap();
    let msg = fx.wait_stdin("user message", |m| m["type"] == "user").await;
    assert_eq!(
        msg,
        json!({"type": "user", "message": {"role": "user", "content": "hello there"}, "parent_tool_use_id": null})
    );
    collect_until(&mut h, is_turn_done).await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn images_go_into_the_stdin_message_as_content_blocks_images_first() {
    let fx = Harness::new().script("two_turns.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
    let webp: &[u8] = b"RIFF\x04\x00\x00\x00WEBPxx";
    let files = vec![put_file(&fx, "a.png", "image/png", png), put_file(&fx, "b.webp", "image/webp", webp)];
    h.send(with_files("what is in these?", files)).await.unwrap();

    let msg = fx.wait_stdin("user message", |m| m["type"] == "user").await;
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    assert_eq!(
        msg,
        json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64(png)}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": b64(webp)}},
                {"type": "text", "text": "what is in these?"},
            ]},
            "parent_tool_use_id": null,
        })
    );
    // 正常跑完一个 turn，之后不带附件的消息又回到字符串 content
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&evs), (1, TurnDoneReason::Completed));
    h.send(input("and now just text")).await.unwrap();
    collect_until(&mut h, is_turn_done).await;
    let users = user_messages(&fx);
    assert_eq!(users.len(), 2);
    assert_eq!(users[1]["message"]["content"], "and now just text");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_image_only_message_has_no_text_block() {
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let files = vec![put_file(&fx, "only.gif", "image/gif", b"GIF89a-bytes")];
    h.send(with_files("", files)).await.unwrap();
    let msg = fx.wait_stdin("user message", |m| m["type"] == "user").await;
    let content = msg["message"]["content"].as_array().expect("content is an array when there are images");
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["source"]["media_type"], "image/gif");
    assert_eq!(content[0]["source"]["type"], "base64");
    collect_until(&mut h, is_turn_done).await;
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_multi_mib_image_goes_through_stdin_in_one_line() {
    // 6 MiB 文件 → 8 MiB base64 的一行，超过 stdout 的单行上限（8 MiB）：stdin 这一侧没有行长限制
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let big: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
    let files = vec![put_file(&fx, "big.png", "image/png", &big)];
    h.send(with_files("big one", files)).await.unwrap();
    let msg = fx.wait_stdin("user message", |m| m["type"] == "user").await;
    use base64::Engine;
    let data = msg["message"]["content"][0]["source"]["data"].as_str().expect("image data");
    assert_eq!(data.len(), 8 * 1024 * 1024);
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), big);
    assert_eq!(msg["message"]["content"][1], json!({"type": "text", "text": "big one"}));
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&evs), (1, TurnDoneReason::Completed));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn bad_attachments_are_refused_before_anything_is_sent() {
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let good = put_file(&fx, "good.png", "image/png", b"\x89PNG-ish");

    // 路径比附件少 / 多
    let few = UserInput { attachments: vec![good.0.clone()], attachment_paths: vec![], ..input("t") };
    assert!(matches!(h.send(few).await, Err(AdapterError::Protocol(_))));
    let many = UserInput { attachments: vec![], attachment_paths: vec![good.1.clone()], ..input("t") };
    assert!(matches!(h.send(many).await, Err(AdapterError::Protocol(_))));

    // 不支持的类型（哪怕同时带着一张好图）
    let pdf = put_file(&fx, "doc.pdf", "application/pdf", b"%PDF-1.7");
    let err = h.send(with_files("t", vec![good.clone(), pdf])).await.unwrap_err();
    assert!(matches!(&err, AdapterError::Unsupported(m) if m.contains("doc.pdf")), "{err:?}");
    let bmp = put_file(&fx, "x.bmp", "image/bmp", b"BM");
    assert!(matches!(h.send(with_files("t", vec![bmp])).await, Err(AdapterError::Unsupported(_))));

    // 文件读不了：缺失 / 是目录
    let missing = (attachment("gone.png", "image/png", 3), fx.dir.path().join("gone.png"));
    let err = h.send(with_files("t", vec![missing])).await.unwrap_err();
    assert!(matches!(&err, AdapterError::Io(e) if e.kind() == std::io::ErrorKind::NotFound), "{err:?}");
    let dir = (attachment("dir.png", "image/png", 3), fx.dir.path().to_path_buf());
    assert!(matches!(h.send(with_files("t", vec![dir])).await, Err(AdapterError::Io(_))));

    // 全都没改状态：没有半截的 turn，下一条有效消息是第 1 个 turn，stdin 里只有它一条用户消息
    assert!(!h.is_busy());
    h.send(with_files("fine", vec![good])).await.unwrap();
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&evs), (1, TurnDoneReason::Completed));
    assert_eq!(user_messages(&fx).len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_with_images_is_still_unsupported() {
    let fx = Harness::new().script("long_turn.jsonl").var("FAKE_CLAUDE_DELAY_MS", "30");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("go")).await.unwrap();
    let img = put_file(&fx, "a.png", "image/png", b"\x89PNG-ish");
    let err = h.send(UserInput { interject: true, ..with_files("look", vec![img.clone()]) }).await.unwrap_err();
    assert!(matches!(err, AdapterError::Unsupported(_)), "{err:?}");
    // turn 进行中不带 interject 的带图消息：同样 Unsupported，且不会去读文件改状态
    let err = h.send(with_files("look", vec![img])).await.unwrap_err();
    assert!(matches!(err, AdapterError::Unsupported(_)), "{err:?}");
    // 只有第一条 "go" 到了 stdin（先等它落进日志，再数）
    fx.wait_stdin("the first user message", |m| m["type"] == "user").await;
    assert_eq!(user_messages(&fx).len(), 1);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_allow_session_round_trip() {
    let fx = Harness::new().script("permission_roundtrip.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("write a file")).await.unwrap();
    let evs = collect_until(&mut h, is_permission).await;
    let p = match evs.last().unwrap() {
        AgentEvent::Permission(p) => p.clone(),
        _ => unreachable!(),
    };
    assert_eq!(p.request_id, "req-perm-1");
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.title, "Write /work/proj/hello.txt");
    // 答复之前 fake 一直在等：后面的事件还没出来
    assert!(h.is_busy());

    h.respond_permission("req-perm-1", decision("allow_session")).await.unwrap();
    let rest = collect_until(&mut h, is_turn_done).await;
    assert_eq!(names(&rest), vec!["tool_result", "file_changed", "text_delta", "text_done", "turn_done"]);

    let reply = fx.wait_stdin("control_response", |m| m["type"] == "control_response").await;
    assert_eq!(reply["response"]["subtype"], "success");
    assert_eq!(reply["response"]["request_id"], "req-perm-1");
    let inner = &reply["response"]["response"];
    assert_eq!(inner["behavior"], "allow");
    assert_eq!(inner["updatedInput"], json!({"file_path": "/work/proj/hello.txt", "content": "hi\n"}));
    assert_eq!(
        inner["updatedPermissions"],
        json!([{"type": "addRules", "rules": [{"toolName": "Write"}], "behavior": "allow", "destination": "session"}])
    );
    // 重复答复：幂等，不报错
    h.respond_permission("req-perm-1", decision("allow")).await.unwrap();
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_deny_carries_the_message() {
    let fx = Harness::new().script("permission_deny.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("clean")).await.unwrap();
    let _ = collect_until(&mut h, is_permission).await;
    h.respond_permission(
        "req-deny-1",
        PermissionDecision {
            option_id: Some("deny".into()),
            message: Some("use cargo clean instead".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let rest = collect_until(&mut h, is_turn_done).await;
    assert_eq!(names(&rest)[0], "tool_result");
    let reply = fx.wait_stdin("control_response", |m| m["type"] == "control_response").await;
    assert_eq!(reply["response"]["response"], json!({"behavior": "deny", "message": "use cargo clean instead"}));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_permission_option_keeps_the_request_pending() {
    let fx = Harness::new().script("permission_roundtrip.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("x")).await.unwrap();
    let _ = collect_until(&mut h, is_permission).await;
    assert!(h.respond_permission("req-perm-1", decision("bogus")).await.is_err());
    // 请求还在：正确答复仍然有效
    h.respond_permission("req-perm-1", decision("allow")).await.unwrap();
    let rest = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&rest), (1, TurnDoneReason::Completed));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn plan_permission_response() {
    let fx = Harness::new().script("exit_plan_mode.jsonl");
    let mut h = fx.spawn(SessionSettings { mode: Some("plan".into()), ..Default::default() }, None).await;
    h.send(input("plan it")).await.unwrap();
    let evs = collect_until(&mut h, is_permission).await;
    match evs.last().unwrap() {
        AgentEvent::Permission(p) => {
            assert_eq!(p.kind, PermissionKind::Plan);
            assert!(p.detail.text.as_deref().unwrap().starts_with("## Plan"));
        }
        _ => unreachable!(),
    }
    h.respond_permission("req-plan-1", decision("allow")).await.unwrap();
    let _ = collect_until(&mut h, is_turn_done).await;
    let reply = fx.wait_stdin("control_response", |m| m["type"] == "control_response").await;
    assert_eq!(reply["response"]["response"]["behavior"], "allow");
    assert_eq!(reply["response"]["response"]["updatedInput"]["plan"], "## Plan\n1. Add the module\n2. Write tests");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn question_permission_response_uses_question_text_as_key() {
    let fx = Harness::new().script("ask_question.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("ask me")).await.unwrap();
    let evs = collect_until(&mut h, is_permission).await;
    match evs.last().unwrap() {
        AgentEvent::Permission(p) => assert_eq!(p.kind, PermissionKind::Question),
        _ => unreachable!(),
    }
    let mut answers = serde_json::Map::new();
    answers.insert("q1".into(), json!("SQLite"));
    h.respond_permission("req-q-1", PermissionDecision { answers: Some(answers), ..Default::default() }).await.unwrap();
    let _ = collect_until(&mut h, is_turn_done).await;
    let reply = fx.wait_stdin("control_response", |m| m["type"] == "control_response").await;
    let inner = &reply["response"]["response"];
    assert_eq!(inner["behavior"], "allow");
    assert_eq!(inner["updatedInput"]["answers"], json!({"Which database?": "SQLite"}));
    assert_eq!(inner["updatedInput"]["questions"][0]["question"], "Which database?");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn question_deny_option_denies_even_when_answers_are_sent() {
    let fx = Harness::new().script("ask_question.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("ask me")).await.unwrap();
    let _ = collect_until(&mut h, is_permission).await;
    let mut answers = serde_json::Map::new();
    answers.insert("q1".into(), json!("SQLite"));
    h.respond_permission(
        "req-q-1",
        PermissionDecision { option_id: Some("deny".into()), message: Some("skip it".into()), answers: Some(answers) },
    )
    .await
    .unwrap();
    let _ = collect_until(&mut h, is_turn_done).await;
    let reply = fx.wait_stdin("control_response", |m| m["type"] == "control_response").await;
    let inner = &reply["response"]["response"];
    assert_eq!(inner["behavior"], "deny", "{inner}");
    assert_eq!(inner["message"], "skip it");
    assert!(inner.get("updatedInput").is_none());
    h.shutdown().await.unwrap();
}

/// 回放 `real_elicitation_*.jsonl`：先答复 MCP 工具的权限，返回随后到来的 elicitation 请求。
async fn elicitation_request(h: &mut Box<dyn AgentHandle>) -> anycon_agent_api::PendingPermission {
    let perm = |evs: &[AgentEvent]| match evs.last() {
        Some(AgentEvent::Permission(p)) => p.clone(),
        other => panic!("{other:?}"),
    };
    let tool = perm(&collect_until(h, is_permission).await);
    assert_eq!(tool.kind, PermissionKind::Tool);
    h.respond_permission(&tool.request_id, decision("allow")).await.unwrap();
    perm(&collect_until(h, is_permission).await)
}

#[tokio::test]
async fn mcp_elicitation_form_is_answered_with_typed_content() {
    let fx = Harness::new().script("real_elicitation_form.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("collect my details")).await.unwrap();
    let p = elicitation_request(&mut h).await;
    assert_eq!((p.kind, p.title.as_str()), (PermissionKind::Question, "example-elicit: Please enter your details"));
    // 答复之前 fake（和真实 CLI）一直在等
    assert!(h.is_busy());

    let mut answers = serde_json::Map::new();
    for (k, v) in [("name", "Ada"), ("age", "36"), ("subscribe", "true"), ("color", "green")] {
        answers.insert(k.into(), json!(v));
    }
    let accept = PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), ..Default::default() };
    h.respond_permission(&p.request_id, accept).await.unwrap();
    let rest = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Completed);
    assert!(!names(&rest).contains(&"error".to_string()), "{:?}", names(&rest));

    // 这就是录制时 CLI 收下、并原样转给 MCP server 的 control_response
    let reply = fx.wait_stdin("elicitation reply", |m| m["response"]["request_id"] == json!(p.request_id)).await;
    assert_eq!(
        reply,
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": p.request_id,
            "response": {"action": "accept", "content": {"name": "Ada", "age": 36, "subscribe": true, "color": "green"}}}})
    );
    // 重复答复：幂等
    h.respond_permission(&p.request_id, decision("decline")).await.unwrap();
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn mcp_elicitation_declined_cancelled_or_unanswerable_all_decline() {
    for (answer, want_error) in [
        // 用户拒绝
        (PermissionDecision { option_id: Some("decline".into()), ..Default::default() }, false),
        // daemon 在取消 / 超时时自动答复的是 reject 选项，同样是 decline
        (
            PermissionDecision {
                option_id: Some("decline".into()),
                message: Some("turn cancelled".into()),
                ..Default::default()
            },
            false,
        ),
        // 提交了，但年龄不是整数：改按 decline 答复（请求在 daemon 那边已经算答复过），并说明原因
        (
            PermissionDecision {
                option_id: Some("accept".into()),
                answers: Some(serde_json::Map::from_iter([
                    ("name".to_string(), json!("Ada")),
                    ("age".to_string(), json!("old")),
                ])),
                ..Default::default()
            },
            true,
        ),
        // 必填项没填
        (
            PermissionDecision {
                option_id: Some("accept".into()),
                answers: Some(serde_json::Map::new()),
                ..Default::default()
            },
            true,
        ),
    ] {
        let fx = Harness::new().script("real_elicitation_form.jsonl");
        let mut h = fx.spawn(SessionSettings::default(), None).await;
        h.send(input("collect my details")).await.unwrap();
        let p = elicitation_request(&mut h).await;
        h.respond_permission(&p.request_id, answer).await.unwrap();
        let rest = collect_until(&mut h, is_turn_done).await;
        let errors: Vec<_> = rest
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Event(Event::Error { code, message, recoverable }) => {
                    Some((code.as_str(), message.as_str(), *recoverable))
                }
                _ => None,
            })
            .collect();
        if want_error {
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert_eq!((errors[0].0, errors[0].2), ("elicitation_invalid_answer", true));
            assert!(errors[0].1.contains("declined"), "{}", errors[0].1);
        } else {
            assert!(errors.is_empty(), "{errors:?}");
        }
        let reply = fx.wait_stdin("elicitation reply", |m| m["response"]["request_id"] == json!(p.request_id)).await;
        assert_eq!(reply["response"]["response"], json!({"action": "decline"}));
        h.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn mcp_elicitation_url_mode_asks_for_the_link_and_accepts() {
    let fx = Harness::new().script("real_elicitation_url.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("sign me in")).await.unwrap();
    let p = {
        // 这条录制里模型先用了 ToolSearch（自动放行），然后才是 MCP 工具的权限
        let tool = match collect_until(&mut h, is_permission).await.last() {
            Some(AgentEvent::Permission(p)) => p.clone(),
            other => panic!("{other:?}"),
        };
        h.respond_permission(&tool.request_id, decision("allow")).await.unwrap();
        match collect_until(&mut h, is_permission).await.last() {
            Some(AgentEvent::Permission(p)) => p.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(p.kind, PermissionKind::Other);
    assert_eq!(p.detail.url.as_deref(), Some("https://example.com/signin?state=abc123"));
    h.respond_permission(&p.request_id, decision("accept")).await.unwrap();
    let rest = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&rest).1, TurnDoneReason::Completed);
    let reply = fx.wait_stdin("elicitation reply", |m| m["response"]["request_id"] == json!(p.request_id)).await;
    assert_eq!(reply["response"]["response"], json!({"action": "accept"}));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn mcp_elicitation_with_a_bad_option_stays_pending_and_a_non_web_link_is_declined_at_once() {
    let fx = Harness::new().script("real_elicitation_form.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("x")).await.unwrap();
    let p = elicitation_request(&mut h).await;
    assert!(h.respond_permission(&p.request_id, decision("zzz")).await.is_err());
    h.respond_permission(&p.request_id, decision("decline")).await.unwrap();
    let _ = collect_until(&mut h, is_turn_done).await;
    h.shutdown().await.unwrap();

    // 链接不是 http(s)：不问用户，直接回 decline 并报一个可恢复的 error，turn 照常往下走
    let fx = Harness::new();
    let script = fx.dir.path().join("elicit-bad-link.jsonl");
    std::fs::write(
        &script,
        concat!(
            r#"{"type":"control_request","request_id":"el-bad","request":{"subtype":"elicitation","mcp_server_name":"example-elicit","message":"Sign in","mode":"url","url":"javascript:alert(1)","elicitation_id":"x"}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"num_turns":1,"session_id":"s","total_cost_usd":0,"usage":{}}"#, "\n",
        ),
    )
    .unwrap();
    let fx = fx.var("FAKE_CLAUDE_SCRIPT", &script.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("hi")).await.unwrap();
    let evs = collect_until(&mut h, is_turn_done).await;
    assert!(!names(&evs).contains(&"PERMISSION".to_string()));
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Event(Event::Error { code, recoverable: true, .. }) if code == "elicitation_rejected")));
    let reply = fx.wait_stdin("decline", |m| m["type"] == "control_response").await;
    assert_eq!(reply["response"]["request_id"], "el-bad");
    assert_eq!(reply["response"]["response"], json!({"action": "decline"}));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_control_request_gets_an_error_reply() {
    let fx = Harness::new();
    let script = fx.dir.path().join("hook.jsonl");
    std::fs::write(
        &script,
        concat!(
            r#"{"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"x"}}"#, "\n",
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"num_turns":1,"session_id":"s","total_cost_usd":0,"usage":{}}"#, "\n",
        ),
    )
    .unwrap();
    let fx = fx.var("FAKE_CLAUDE_SCRIPT", &script.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("hi")).await.unwrap();
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    let reply = fx.wait_stdin("error reply", |m| m["type"] == "control_response").await;
    assert_eq!(reply["response"]["subtype"], "error");
    assert_eq!(reply["response"]["request_id"], "hook-1");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_sends_interrupt_and_keeps_the_process() {
    let fx = Harness::new().script("long_turn.jsonl").var("FAKE_CLAUDE_DELAY_MS", "30");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await;
    h.send(input("write a lot")).await.unwrap();
    // 等到真的开始流式输出
    let _ = collect_until(&mut h, |e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).await;
    let t0 = Instant::now();
    h.cancel().await.unwrap();
    h.cancel().await.unwrap(); // 幂等
    let evs = collect_until(&mut h, is_turn_done).await;
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    assert!(t0.elapsed() < Duration::from_secs(2));
    assert!(!h.is_busy());

    let interrupts: Vec<Value> =
        fx.stdin_messages().into_iter().filter(|m| m["request"]["subtype"] == "interrupt").collect();
    assert!(!interrupts.is_empty());
    assert!(interrupts[0]["request_id"].as_str().unwrap().len() >= 8);

    // 进程没被杀：之后的控制请求仍然有回执
    let r = h.set_settings(&SessionSettings { mode: Some("plan".into()), ..Default::default() }).await.unwrap();
    assert!(r.applied_now);
    // 空闲时 cancel 是 no-op
    h.cancel().await.unwrap();
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_falls_back_to_sigint() {
    let fx = Harness::new()
        .script("long_turn.jsonl")
        .var("FAKE_CLAUDE_DELAY_MS", "30")
        .var("FAKE_CLAUDE_IGNORE_INTERRUPT", "1");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await;
    h.send(input("write a lot")).await.unwrap();
    let _ = collect_until(&mut h, |e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).await;
    h.cancel().await.unwrap();
    // interrupt 没人理 → 400ms 后 SIGINT → fake 以 130 退出 → 补 TurnDone(cancelled) 再 Exited
    let evs = collect_until(&mut h, is_exited).await;
    let n = names(&evs);
    assert_eq!(&n[n.len() - 2..], ["turn_done", "EXITED"]);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, Some(130));
            assert!(error.is_none(), "an exit we asked for is not an error: {error:?}");
        }
        _ => unreachable!(),
    }
    assert!(!h.is_busy());
}

/// 默认启动时的有效设置（`set_settings` 收的是完整的目标设置，测试里从它出发改）。
fn effective() -> SessionSettings {
    SessionSettings {
        model: Some("claude-opus-5-5".into()),
        thinking: Some("medium".into()),
        mode: Some("default".into()),
        ..Default::default()
    }
}

fn control_requests(fx: &Harness, subtype: &str) -> Vec<Value> {
    fx.stdin_messages().into_iter().filter(|m| m["request"]["subtype"] == subtype).collect()
}

#[tokio::test]
async fn set_settings_uses_control_requests() {
    let fx = Harness::new();
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let mut d = effective();

    // 模型：set_model
    d.model = Some("claude-sonnet-5-5".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now && !r.restart_required);
    // 权限模式：set_permission_mode
    d.mode = Some("acceptEdits".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now && !r.restart_required);
    // 同样的值再设一次：什么也不用做
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now);

    let msgs = fx.stdin_messages();
    let model_req = msgs.iter().find(|m| m["request"]["subtype"] == "set_model").unwrap();
    assert_eq!(model_req["request"]["model"], "claude-sonnet-5-5");
    let mode_reqs: Vec<&Value> = msgs.iter().filter(|m| m["request"]["subtype"] == "set_permission_mode").collect();
    assert_eq!(mode_reqs.len(), 1);
    assert_eq!(mode_reqs[0]["request"]["mode"], "acceptEdits");
    // request_id 各不相同
    let mut ids: Vec<&str> =
        msgs.iter().filter(|m| m["type"] == "control_request").map(|m| m["request_id"].as_str().unwrap()).collect();
    let n = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), n);

    // 思考等级只能启动时给 → 需要重启
    d.thinking = Some("high".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.restart_required && !r.applied_now);
    // 其它启动参数同理
    d.max_turns = Some(5);
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.restart_required);
    d.options.insert("fast_mode".into(), json!(true));
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.restart_required);

    // CLI 拒绝的模型 → InvalidSettings
    d.model = Some("bogus-model".into());
    let err = h.set_settings(&d).await.unwrap_err();
    assert!(matches!(err, AdapterError::InvalidSettings { ref field, .. } if field == "model"), "{err:?}");
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn set_settings_can_clear_values_and_resume_does_not_bring_them_back() {
    use std::collections::BTreeMap;
    let fx = Harness::new();
    let mut options = serde_json::Map::new();
    options.insert("fast_mode".into(), json!(true));
    let full = SessionSettings {
        model: Some("claude-sonnet-5-5".into()),
        thinking: Some("high".into()),
        mode: Some("acceptEdits".into()),
        options,
        env: BTreeMap::from([("FOO".to_string(), "1".to_string())]),
        system_prompt_append: Some("Be terse.".into()),
        max_turns: Some(3),
        max_budget_usd: Some(2.0),
    };
    let mut h = fx.spawn(full.clone(), None).await;
    match next_event(&mut h).await {
        AgentEvent::Started { resume } => {
            assert_eq!(resume.launch_args["system_prompt_append"], "Be terse.");
            assert_eq!(resume.launch_args["max_turns"], 3);
        }
        other => panic!("{other:?}"),
    }
    fx.wait_stdin_or_argv().await;
    assert!(fx.argv().contains(&"--append-system-prompt=Be terse.".to_string()));

    // 清掉 system_prompt_append / max_* / fast_mode / env，模型 / 思考 / 模式不变
    let cleared = SessionSettings {
        model: full.model.clone(),
        thinking: full.thinking.clone(),
        mode: full.mode.clone(),
        ..Default::default()
    };
    let r = h.set_settings(&cleared).await.unwrap();
    assert!(r.restart_required && !r.applied_now);
    assert!(fx.stdin_messages().is_empty(), "only launch-time settings changed: no control requests");
    let launch_args = h.resume_handle().unwrap().launch_args;
    for k in ["system_prompt_append", "max_turns", "max_budget_usd"] {
        assert!(launch_args[k].is_null(), "{k} was cleared in launch_args: {launch_args}");
    }
    assert_eq!(launch_args["fast_mode"], false);
    assert_eq!(launch_args["model"], "claude-sonnet-5-5");
    // 同样的目标再设一次：没有变化，也不用重启
    let r = h.set_settings(&cleared).await.unwrap();
    assert!(r.applied_now && !r.restart_required);

    // daemon 会收到一个带新 launch_args 的 `Started`，落库后崩溃重启也不会复活旧值
    match next_event(&mut h).await {
        AgentEvent::Started { resume } => {
            assert!(resume.launch_args["system_prompt_append"].is_null());
            assert!(resume.launch_args["max_turns"].is_null());
        }
        other => panic!("{other:?}"),
    }
    let resume = h.resume_handle().unwrap();
    h.shutdown().await.unwrap();

    // 重启：用新的完整设置 + resume_handle() 再拉起 → argv 里没有被清除的选项
    let fx2 = Harness::new();
    let mut h2 = fx2.spawn(cleared.clone(), Some(resume)).await;
    fx2.wait_stdin_or_argv().await;
    let argv = fx2.argv();
    for flag in ["--append-system-prompt", "--max-turns", "--max-budget-usd", "--settings"] {
        assert!(!argv.iter().any(|a| a.starts_with(flag)), "{flag} came back after being cleared: {argv:?}");
    }
    assert!(argv.contains(&"--model=claude-sonnet-5-5".to_string()));
    assert!(argv.iter().any(|a| a.starts_with("--resume=")));
    h2.shutdown().await.unwrap();
}

#[tokio::test]
async fn clearing_the_model_goes_back_to_the_default_or_null() {
    let fx = Harness::new();
    let mut h = fx
        .spawn(
            SessionSettings {
                model: Some("claude-sonnet-5-5".into()),
                thinking: Some("high".into()),
                ..Default::default()
            },
            None,
        )
        .await;
    // model = None → 回到默认模型（AgentInfo 里标 default 的）；思考等级 high 在默认模型上也成立
    let r = h
        .set_settings(&SessionSettings {
            thinking: Some("high".into()),
            mode: Some("default".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(r.applied_now);
    let reqs = control_requests(&fx, "set_model");
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["request"]["model"], "claude-opus-5-5");
    h.shutdown().await.unwrap();

    // 没有任何默认可回：set_model 带 null（让 CLI 用它自己的默认）
    let fx = Harness::new();
    let mut req =
        fx.request(SessionSettings { model: Some("claude-sonnet-5-5".into()), ..Default::default() }, None).await;
    let mut info = (*req.agent_info).clone();
    for m in &mut info.models {
        m.default = false;
    }
    req.agent_info = std::sync::Arc::new(info);
    let mut h = ClaudeCodeAdapter::with_timings(fast_timings()).spawn(req).await.unwrap();
    h.set_settings(&SessionSettings::default()).await.unwrap();
    let reqs = control_requests(&fx, "set_model");
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0]["request"]["model"].is_null(), "{}", reqs[0]);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_set_settings_failure_keeps_state_consistent() {
    let fx = Harness::new().var("FAKE_CLAUDE_REJECT_MODE", "plan");
    let mut h = fx.spawn(SessionSettings::default(), None).await;

    // 模型成功、模式被 CLI 拒绝：模型保留，错误里说清楚
    let mut d = effective();
    d.model = Some("claude-sonnet-5-5".into());
    d.mode = Some("plan".into());
    let err = h.set_settings(&d).await.unwrap_err();
    match &err {
        AdapterError::InvalidSettings { field, reason } => {
            assert_eq!(field, "mode");
            assert!(reason.contains("applied: model; not applied: mode"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(control_requests(&fx, "set_model").len(), 1);
    assert_eq!(control_requests(&fx, "set_permission_mode").len(), 1);

    // 状态一致：模型已经是 sonnet、模式还是 default，所以回到「sonnet + default」什么都不用发
    d.mode = Some("default".into());
    let r = h.set_settings(&d).await.unwrap();
    assert!(r.applied_now);
    assert_eq!(control_requests(&fx, "set_model").len(), 1);
    assert_eq!(control_requests(&fx, "set_permission_mode").len(), 1);

    // 第一步就失败：后面的步骤不发，也不记
    d.model = Some("bogus-model".into());
    d.mode = Some("acceptEdits".into());
    let err = h.set_settings(&d).await.unwrap_err();
    match &err {
        AdapterError::InvalidSettings { field, reason } => {
            assert_eq!(field, "model");
            assert!(reason.contains("applied: nothing; not applied: model, mode"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(control_requests(&fx, "set_model").len(), 2, "the rejected attempt was sent");
    assert_eq!(control_requests(&fx, "set_permission_mode").len(), 1, "mode was not attempted");
    // 模型回到已经生效的 sonnet（被拒的那次没改状态），再改模式：只发模式那一条
    d.model = Some("claude-sonnet-5-5".into());
    assert!(h.set_settings(&d).await.unwrap().applied_now);
    assert_eq!(control_requests(&fx, "set_model").len(), 2);
    assert_eq!(control_requests(&fx, "set_permission_mode").len(), 2);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn switching_into_bypass_needs_a_dangerous_launch() {
    // 没带 --dangerously-skip-permissions 启动：切 bypassPermissions 要重启，而且根本不发控制请求
    let fx = Harness::new();
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let r = h
        .set_settings(&SessionSettings { mode: Some("bypassPermissions".into()), ..Default::default() })
        .await
        .unwrap();
    assert!(r.restart_required && !r.applied_now);
    assert!(!fx.stdin_messages().iter().any(|m| m["request"]["subtype"] == "set_permission_mode"));
    h.shutdown().await.unwrap();

    // 以 bypass 启动：带危险标志，之后可以来回切
    let fx = Harness::new();
    let mut h = fx.spawn(SessionSettings { mode: Some("bypassPermissions".into()), ..Default::default() }, None).await;
    fx.wait_stdin_or_argv().await;
    let argv = fx.argv();
    assert!(argv.contains(&"--permission-mode=bypassPermissions".to_string()));
    assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
    assert!(
        h.set_settings(&SessionSettings { mode: Some("default".into()), ..Default::default() })
            .await
            .unwrap()
            .applied_now
    );
    assert!(
        h.set_settings(&SessionSettings { mode: Some("bypassPermissions".into()), ..Default::default() })
            .await
            .unwrap()
            .applied_now
    );
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn resume_passes_resume_flag_and_keeps_session_id() {
    let fx = Harness::new().script("text_stream.jsonl");
    // 落库的 launch_args 是旧的（和下面的目标设置不一样）：resume 只从里面取会话 id
    let prior = ResumeHandle {
        agent_session_id: "0d9d5f1e-6b1c-4a52-9a43-1d6f3a2b7c11".into(),
        launch_args: json!({"model": "claude-opus-5-5", "thinking": "medium", "mode": "plan", "max_turns": 99}),
    };
    let target = SessionSettings {
        model: Some("claude-haiku-5-5".into()),
        thinking: Some("low".into()),
        mode: Some("acceptEdits".into()),
        max_turns: Some(4),
        ..Default::default()
    };
    let mut h = fx.spawn(target, Some(prior.clone())).await;
    match next_event(&mut h).await {
        AgentEvent::Started { resume } => {
            assert_eq!(resume.agent_session_id, prior.agent_session_id);
            // 新的 launch_args 是这次的有效设置，不是旧快照
            assert_eq!(resume.launch_args["model"], "claude-haiku-5-5");
            assert_eq!(resume.launch_args["mode"], "acceptEdits");
            assert_eq!(resume.launch_args["max_turns"], 4);
        }
        other => panic!("{other:?}"),
    }
    h.send(input("continue")).await.unwrap();
    let evs = collect_until(&mut h, is_turn_done).await;
    // CLI 回显同一个 id → 没有第二个 Started
    assert!(!names(&evs).contains(&"STARTED".to_string()));
    assert_eq!(h.resume_handle().unwrap().agent_session_id, prior.agent_session_id);
    let argv = fx.argv();
    assert!(argv.contains(&format!("--resume={}", prior.agent_session_id)));
    assert!(!argv.iter().any(|a| a.starts_with("--session-id")));
    assert!(argv.contains(&"--model=claude-haiku-5-5".to_string()));
    assert!(argv.contains(&"--effort=low".to_string()));
    assert!(argv.contains(&"--permission-mode=acceptEdits".to_string()));
    assert!(argv.contains(&"--max-turns=4".to_string()));
    assert!(!argv.contains(&"--max-turns=99".to_string()), "{argv:?}");
    h.shutdown().await.unwrap();
}

/// daemon 在没有存活进程时（idle 关闭 / daemon 重启 / ended）`set_settings` 只改库里的 `settings`，
/// 不一定同步改 `launch_args`，所以 resume 时手里是「新的目标设置 + 旧的 launch_args」。
/// 设值 → 进程退出 → 清掉 → resume：argv 里不能有旧值。
#[tokio::test]
async fn resume_after_clear_does_not_resurrect_values_from_a_stale_launch_args() {
    use std::collections::BTreeMap;
    let mut options = serde_json::Map::new();
    options.insert("fast_mode".into(), json!(true));
    let with_values = SessionSettings {
        model: Some("claude-sonnet-5-5".into()),
        thinking: Some("high".into()),
        mode: Some("acceptEdits".into()),
        options,
        env: BTreeMap::from([("FOO".to_string(), "1".to_string())]),
        system_prompt_append: Some("Be terse.".into()),
        max_turns: Some(3),
        max_budget_usd: Some(2.5),
    };

    // 1. 带值拉起，进程自己退出；daemon 手里留下这时的 resume_handle()（launch_args 里有那些值）
    let fx = Harness::new();
    let mut h = fx.spawn(with_values.clone(), None).await;
    fx.wait_stdin_or_argv().await;
    let first = fx.argv();
    for want in
        ["--append-system-prompt=Be terse.", "--max-turns=3", "--max-budget-usd=2.5", "--settings={\"fastMode\":true}"]
    {
        assert!(first.contains(&want.to_string()), "{want} missing from {first:?}");
    }
    let stale = h.resume_handle().unwrap();
    assert_eq!(stale.launch_args["system_prompt_append"], "Be terse.");
    assert_eq!(stale.launch_args["max_budget_usd"], 2.5);
    h.shutdown().await.unwrap();
    let _ = collect_until(&mut h, is_exited).await;

    // 2. 进程没了，用户清掉 system_prompt_append / max_* / fast_mode / env（daemon 只改了库里的 settings，
    //    `stale` 原封不动）；再 resume
    let cleared = SessionSettings {
        model: with_values.model.clone(),
        thinking: with_values.thinking.clone(),
        mode: with_values.mode.clone(),
        ..Default::default()
    };
    let fx2 = Harness::new();
    let mut h2 = fx2.spawn(cleared.clone(), Some(stale.clone())).await;
    fx2.wait_stdin_or_argv().await;
    let argv = fx2.argv();
    for flag in ["--append-system-prompt", "--max-turns", "--max-budget-usd", "--settings"] {
        assert!(!argv.iter().any(|a| a.starts_with(flag)), "{flag} came back after being cleared: {argv:?}");
    }
    for old in ["Be terse.", "2.5"] {
        assert!(!argv.iter().any(|a| a.contains(old)), "{old} came back after being cleared: {argv:?}");
    }
    // 会话 id 来自 resume，其余没丢
    assert!(argv.contains(&format!("--resume={}", stale.agent_session_id)), "{argv:?}");
    assert!(!argv.iter().any(|a| a.starts_with("--session-id")));
    assert!(argv.contains(&"--model=claude-sonnet-5-5".to_string()));
    assert!(argv.contains(&"--effort=high".to_string()));
    assert!(argv.contains(&"--permission-mode=acceptEdits".to_string()));
    // 新进程宣布的 launch_args 是清掉之后的
    match next_event(&mut h2).await {
        AgentEvent::Started { resume } => {
            assert_eq!(resume.agent_session_id, stale.agent_session_id);
            for k in ["system_prompt_append", "max_turns", "max_budget_usd"] {
                assert!(resume.launch_args[k].is_null(), "{k}: {}", resume.launch_args);
            }
            assert_eq!(resume.launch_args["fast_mode"], false);
        }
        other => panic!("{other:?}"),
    }
    h2.shutdown().await.unwrap();

    // 3. 对照：resume 时 settings 仍带值 → argv 有值（取的是 settings 的，不是 launch_args 的）
    let newer = SessionSettings {
        system_prompt_append: Some("Be verbose.".into()),
        max_turns: Some(8),
        max_budget_usd: Some(7.0),
        ..cleared.clone()
    };
    let fx3 = Harness::new();
    let mut h3 = fx3.spawn(newer, Some(stale)).await;
    fx3.wait_stdin_or_argv().await;
    let argv = fx3.argv();
    for want in ["--append-system-prompt=Be verbose.", "--max-turns=8", "--max-budget-usd=7"] {
        assert!(argv.contains(&want.to_string()), "{want} missing from {argv:?}");
    }
    for old in ["Be terse.", "--max-turns=3", "--max-budget-usd=2.5"] {
        assert!(!argv.iter().any(|a| a.contains(old)), "stale {old} leaked into {argv:?}");
    }
    assert!(!argv.iter().any(|a| a.starts_with("--settings")), "fast_mode was not set in the new settings");
    h3.shutdown().await.unwrap();

    // 4. 对照：settings 原样带着全部值 resume → 全部都在
    let fx4 = Harness::new();
    let stale4 = ResumeHandle {
        agent_session_id: "0d9d5f1e-6b1c-4a52-9a43-1d6f3a2b7c11".into(),
        launch_args: serde_json::Value::Null,
    };
    let mut h4 = fx4.spawn(with_values, Some(stale4)).await;
    fx4.wait_stdin_or_argv().await;
    let argv = fx4.argv();
    for want in [
        "--append-system-prompt=Be terse.",
        "--max-turns=3",
        "--max-budget-usd=2.5",
        "--settings={\"fastMode\":true}",
        "--resume=0d9d5f1e-6b1c-4a52-9a43-1d6f3a2b7c11",
    ] {
        assert!(argv.contains(&want.to_string()), "{want} missing from {argv:?}");
    }
    h4.shutdown().await.unwrap();
}

#[tokio::test]
async fn resume_of_an_unknown_session_reports_the_failure() {
    let fx = Harness::new().var("FAKE_CLAUDE_RESUME_FAIL", "1");
    let prior =
        ResumeHandle { agent_session_id: "ffffffff-0000-0000-0000-000000000000".into(), launch_args: Value::Null };
    let mut h = fx.spawn(SessionSettings::default(), Some(prior)).await;
    let evs = collect_until(&mut h, is_exited).await;
    assert_eq!(names(&evs), vec!["STARTED", "error", "EXITED"]);
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, Some(1));
            assert!(error.as_deref().unwrap().contains("No conversation found"), "{error:?}");
        }
        _ => unreachable!(),
    }
    assert!(matches!(h.send(input("hi")).await, Err(AdapterError::Exited(_))));
}

#[tokio::test]
async fn crash_mid_turn_is_reported() {
    let fx = Harness::new().script("text_stream.jsonl").var("FAKE_CLAUDE_CRASH_AFTER", "3");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("hi")).await.unwrap();
    let evs = collect_until(&mut h, is_exited).await;
    let n = names(&evs);
    assert_eq!(&n[n.len() - 2..], ["error", "EXITED"], "{n:?}");
    match &evs[evs.len() - 2] {
        AgentEvent::Event(Event::Error { code, recoverable, .. }) => {
            assert_eq!((code.as_str(), *recoverable), ("agent_exited", true))
        }
        other => panic!("{other:?}"),
    }
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, Some(3));
            assert!(error.as_deref().unwrap().contains("simulated crash"), "stderr tail is attached: {error:?}");
        }
        _ => unreachable!(),
    }
    assert!(!h.is_busy());
}

#[tokio::test]
async fn shutdown_mid_turn_cancels_it_then_exits() {
    let fx = Harness::new().script("long_turn.jsonl").var("FAKE_CLAUDE_DELAY_MS", "30");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await;
    h.send(input("go")).await.unwrap();
    let _ = collect_until(&mut h, |e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).await;
    h.shutdown().await.unwrap();
    let evs = collect_until(&mut h, is_exited).await;
    let n = names(&evs);
    assert_eq!(&n[n.len() - 2..], ["turn_done", "EXITED"]);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    match evs.last().unwrap() {
        AgentEvent::Exited { error, .. } => assert!(error.is_none()),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn stubborn_process_is_killed_after_the_grace_period() {
    let fx = Harness::new().var("FAKE_CLAUDE_STUBBORN", "1");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await;
    // 等 fake 起来
    fx.wait_stdin_or_argv().await;
    let t0 = Instant::now();
    h.shutdown().await.unwrap();
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let evs = collect_until(&mut h, is_exited).await;
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, None, "killed by a signal has no exit code");
            assert!(error.is_none());
        }
        _ => unreachable!(),
    }
}

impl Harness {
    /// 等 fake-claude 写出 argv 记录（说明它已经在跑、信号处理器已装好）。
    async fn wait_stdin_or_argv(&self) {
        let start = Instant::now();
        while !self.log_entries().iter().any(|e| e["kind"] == "argv") {
            assert!(start.elapsed() < Duration::from_secs(10), "fake-claude never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // 再给一点时间让它跑到 signal 安装之后（argv 日志在安装之后写，这里只是保险）
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn env_is_passed_through_and_thinking_off_is_an_env_var() {
    let fx = Harness::new()
        .var("CLAUDECODE", "1")
        .var("CLAUDE_CODE_ENTRYPOINT", "cli")
        .var("ANYCON_TEST_VAR", "kept")
        .script("text_stream.jsonl");
    let mut h = fx
        .spawn(
            SessionSettings {
                model: Some("claude-opus-4-6".into()),
                thinking: Some("off".into()),
                ..Default::default()
            },
            None,
        )
        .await;
    h.send(input("hi")).await.unwrap();
    let _ = collect_until(&mut h, is_turn_done).await;
    let entry = fx.log_entries().into_iter().find(|e| e["kind"] == "argv").unwrap();
    let env = &entry["env"];
    // 剔除嵌套会话标记是 daemon 的事（env.rs，只做一次）：adapter 收到什么就原样交给子进程
    assert_eq!(env["CLAUDECODE"], "1", "the adapter does not strip ENV_STRIP any more: {env}");
    assert_eq!(env["CLAUDE_CODE_ENTRYPOINT"], "cli");
    assert_eq!(env["ANYCON_TEST_VAR"], "kept");
    assert_eq!(env["MAX_THINKING_TOKENS"], "0");
    assert!(!entry["args"].as_array().unwrap().iter().any(|a| a.as_str().unwrap().starts_with("--effort")));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_the_handle_kills_the_process() {
    let fx = Harness::new().var("FAKE_CLAUDE_STUBBORN", "1");
    let h = fx.spawn(SessionSettings::default(), None).await;
    fx.wait_stdin_or_argv().await;
    let pid = fx.log_entries().into_iter().find(|e| e["kind"] == "argv").unwrap()["pid"].as_u64().unwrap();
    assert!(pid_alive(pid));
    drop(h);
    let start = Instant::now();
    while pid_alive(pid) {
        assert!(start.elapsed() < Duration::from_secs(5), "process {pid} survived the handle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn spawn_errors() {
    // cwd 不存在
    let fx = Harness::new();
    let mut req = fx.request(SessionSettings::default(), None).await;
    req.cwd = fx.dir.path().join("missing");
    let err = ClaudeCodeAdapter::new().spawn(req).await.err().unwrap();
    assert!(matches!(err, AdapterError::Spawn(_)), "{err:?}");

    // 命令不存在
    let mut req = fx.request(SessionSettings::default(), None).await;
    req.config.command = Some("/no/such/claude".into());
    req.agent_info = std::sync::Arc::new({
        let mut i = (*req.agent_info).clone();
        i.path = None;
        i
    });
    let err = ClaudeCodeAdapter::new().spawn(req).await.err().unwrap();
    assert!(matches!(err, AdapterError::NotInstalled(_)), "{err:?}");

    // 已禁用
    let mut req = fx.request(SessionSettings::default(), None).await;
    req.config.enabled = false;
    assert!(ClaudeCodeAdapter::new().spawn(req).await.is_err());

    // 非法设置在拉起进程之前就被拒绝
    let req = fx
        .request(
            SessionSettings {
                model: Some("claude-opus-5-5".into()),
                thinking: Some("off".into()),
                ..Default::default()
            },
            None,
        )
        .await;
    let err = ClaudeCodeAdapter::new().spawn(req).await.err().unwrap();
    assert!(matches!(err, AdapterError::InvalidSettings { .. }), "{err:?}");
    assert!(!fx.log.exists(), "no process was started");
}

#[tokio::test]
async fn dangerous_values_are_rejected_before_a_process_starts() {
    let fx = Harness::new();
    let info = fx.info().await;
    let adapter = ClaudeCodeAdapter::new();
    // 以 - 开头的模型 id 能通过 `allow_custom_model` 的通用校验，但 adapter 自己的校验会拦住
    for bad in ["--dangerously-skip-permissions", "-p", "a b", "m\nx"] {
        let settings = SessionSettings { model: Some(bad.into()), ..Default::default() };
        let err = adapter.validate_settings(&info, &settings).unwrap_err();
        assert!(matches!(err, AdapterError::InvalidSettings { ref field, .. } if field == "model"), "{bad:?}: {err:?}");
        let req = fx.request(settings, None).await;
        let err = adapter.spawn(req).await.err().unwrap();
        assert!(matches!(err, AdapterError::InvalidSettings { ref field, .. } if field == "model"), "{bad:?}: {err:?}");
    }
    for settings in [
        SessionSettings { max_budget_usd: Some(f64::NAN), ..Default::default() },
        SessionSettings { max_budget_usd: Some(f64::INFINITY), ..Default::default() },
        SessionSettings { max_budget_usd: Some(-1.0), ..Default::default() },
        SessionSettings { system_prompt_append: Some("a\0b".into()), ..Default::default() },
        SessionSettings { system_prompt_append: Some("x".repeat(64 * 1024)), ..Default::default() },
    ] {
        assert!(adapter.validate_settings(&info, &settings).is_err(), "{settings:?}");
        let req = fx.request(settings, None).await;
        assert!(adapter.spawn(req).await.is_err());
    }
    // 会话 id 同理（resume 的 id 来自 daemon 的库）
    let bad_resume =
        ResumeHandle { agent_session_id: "--dangerously-skip-permissions".into(), launch_args: Value::Null };
    let req = fx.request(SessionSettings::default(), Some(bad_resume)).await;
    assert!(matches!(adapter.spawn(req).await.err().unwrap(), AdapterError::InvalidSettings { .. }));
    assert!(!fx.log.exists(), "no process was started");

    // 合法的自定义模型照常通过，而且在 argv 里是单个元素
    let settings = SessionSettings { model: Some("sonnet[1m]".into()), ..Default::default() };
    adapter.validate_settings(&info, &settings).unwrap();
    let mut h = fx.spawn(settings, None).await;
    fx.wait_stdin_or_argv().await;
    assert!(fx.argv().contains(&"--model=sonnet[1m]".to_string()));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_overlong_stdout_line_is_a_protocol_error_and_stops_the_process() {
    // 空闲时（没有 turn）：Exited 带协议错误，进程被杀
    let fx = Harness::new().var("FAKE_CLAUDE_BIG_LINE", &(9 * 1024 * 1024).to_string());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let evs = collect_until(&mut h, is_exited).await;
    match evs.last().unwrap() {
        AgentEvent::Exited { code, error } => {
            assert_eq!(*code, None, "killed by a signal");
            let e = error.as_deref().expect("a protocol error is reported");
            assert!(e.contains("protocol error") && e.contains("longer than"), "{e}");
        }
        _ => unreachable!(),
    }
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_overlong_line_in_the_middle_of_a_turn_reports_an_error_event_first() {
    let fx = Harness::new();
    let script = fx.dir.path().join("big.jsonl");
    let init = r#"{"type":"system","subtype":"init","session_id":"11111111-2222-3333-4444-555555555555","model":"claude-opus-5-5"}"#;
    let result = r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"num_turns":1,"session_id":"11111111-2222-3333-4444-555555555555","total_cost_usd":0,"usage":{}}"#;
    std::fs::write(&script, format!("{init}\n{}\n{result}\n", "x".repeat(9 * 1024 * 1024))).unwrap();
    let fx = fx.var("FAKE_CLAUDE_SCRIPT", &script.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("go")).await.unwrap();
    let evs = collect_until(&mut h, is_exited).await;
    let n = names(&evs);
    assert_eq!(&n[n.len() - 2..], ["error", "EXITED"], "{n:?}");
    match &evs[evs.len() - 2] {
        AgentEvent::Event(Event::Error { code, message, .. }) => {
            assert_eq!(code, "agent_exited");
            assert!(message.contains("limit"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    match evs.last().unwrap() {
        AgentEvent::Exited { error, .. } => assert!(error.as_deref().unwrap().contains("protocol error")),
        _ => unreachable!(),
    }
    assert!(!h.is_busy());
}

#[tokio::test]
async fn processes_started_by_claude_do_not_outlive_it() {
    let pid_file_dir = tempfile::tempdir().unwrap();
    let pid_file = pid_file_dir.path().join("bg.pid");
    let fx = Harness::new().var("FAKE_CLAUDE_BG_PID_FILE", &pid_file.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let start = Instant::now();
    let bg: u64 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|t| t.trim().parse().ok()) {
            break pid;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "fake-claude never started its background process");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(pid_alive(bg));
    // 优雅关闭：claude 读到 stdin EOF 自己退出，它留下的后台进程也要被清掉
    h.shutdown().await.unwrap();
    let start = Instant::now();
    while pid_alive(bg) {
        assert!(start.elapsed() < Duration::from_secs(5), "background process {bg} survived the shutdown");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 直接丢弃句柄也一样
    let pid_file2 = pid_file_dir.path().join("bg2.pid");
    let fx = Harness::new().var("FAKE_CLAUDE_BG_PID_FILE", &pid_file2.to_string_lossy());
    let h = fx.spawn(SessionSettings::default(), None).await;
    let start = Instant::now();
    let bg: u64 = loop {
        if let Some(pid) = std::fs::read_to_string(&pid_file2).ok().and_then(|t| t.trim().parse().ok()) {
            break pid;
        }
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    drop(h);
    let start = Instant::now();
    while pid_alive(bg) {
        assert!(start.elapsed() < Duration::from_secs(5), "background process {bg} survived the drop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------- 不阻塞约定

/// 一个 turn：`n` 条文本 delta，然后（`hold` = 一直停着等打断，否则正常结束）。
fn write_flood(dir: &std::path::Path, n: usize, hold: bool) -> std::path::PathBuf {
    const SID: &str = "11111111-2222-3333-4444-555555555555";
    let ev =
        |e: &str| format!(r#"{{"type":"stream_event","event":{e},"session_id":"{SID}","parent_tool_use_id":null}}"#);
    let mut lines = vec![
        ev(
            r#"{"type":"message_start","message":{"model":"claude-haiku-5-5","id":"msg_flood","type":"message","role":"assistant","content":[]}}"#,
        ),
        ev(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#),
    ];
    for i in 0..n {
        lines.push(ev(&format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"w{i} "}}}}"#
        )));
    }
    if hold {
        lines.push("__HOLD__".to_string());
    } else {
        lines.push(ev(r#"{"type":"content_block_stop","index":0}"#));
        lines.push(format!(
            r#"{{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"num_turns":1,"result":"","session_id":"{SID}","total_cost_usd":0.001,"usage":{{"input_tokens":1,"output_tokens":1}}}}"#
        ));
    }
    let path = dir.join(if hold { "flood_hold.jsonl" } else { "flood.jsonl" });
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

#[tokio::test]
async fn control_requests_are_not_blocked_by_thousands_of_unread_events() {
    // 一个 turn 输出了 5000 条 delta 就结束了，没有人读 `events()`：有界通道早就满了。
    // `set_model` 的回执和这些事件在同一条 stdout 上，读任务不能被事件挡住，否则一直等到控制请求超时。
    let dir = tempfile::tempdir().unwrap();
    let flood = write_flood(dir.path(), 5000, false);
    let fx = Harness::new().var("FAKE_CLAUDE_SCRIPT", &flood.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("go")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let began = Instant::now();
    let s = SessionSettings { model: Some("claude-sonnet-5-5".into()), ..Default::default() };
    let r = tokio::time::timeout(Duration::from_secs(5), h.set_settings(&s))
        .await
        .expect("set_settings() must not wait for the event channel to drain")
        .unwrap();
    assert!(r.applied_now, "{r:?}");
    assert!(began.elapsed() < Duration::from_secs(5));
    // 事件一条没丢：5000 条 delta，然后是 turn_done（改设置时补发的 Started 夹在中间也行）
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut h, is_turn_done)).await.unwrap();
    let deltas = evs.iter().filter(|e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).count();
    assert_eq!(deltas, 5000);
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Completed);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_under_a_flood_still_ends_with_a_cancelled_turn() {
    let dir = tempfile::tempdir().unwrap();
    let flood = write_flood(dir.path(), 5000, true);
    let fx = Harness::new().var("FAKE_CLAUDE_SCRIPT", &flood.to_string_lossy());
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    h.send(input("go")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    tokio::time::timeout(Duration::from_secs(5), h.cancel()).await.expect("cancel() must not block").unwrap();
    let evs = tokio::time::timeout(Duration::from_secs(30), collect_until(&mut h, is_turn_done)).await.unwrap();
    assert_eq!(turn_done(&evs).1, TurnDoneReason::Cancelled);
    let deltas = evs.iter().filter(|e| matches!(e, AgentEvent::Event(Event::TextDelta { .. }))).count();
    assert_eq!(deltas, 5000);
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn interjecting_is_unsupported() {
    // Claude Code 的 stream-json 在 turn 中收到的用户消息会排到下一个 turn：不支持真正的插话（`capabilities.interject = false`）
    let fx = Harness::new().script("text_stream.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    // 没有进行中的 turn 时也一样：`interject = true` 绝不自己开 turn
    assert!(matches!(h.send(interject("x")).await, Err(AdapterError::Unsupported(_))));
    assert!(fx.stdin_messages().iter().all(|m| m["type"] != "user"));
    h.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_message_sent_during_the_clis_own_turn_is_written_after_that_turn_ends() {
    // 第一个 turn 派出后台任务后结束；任务结束时 fake 自己开始一轮（`__AUTO__`，中间停 600 ms）
    let fx = Harness::new().script("agent_turn_defer.jsonl");
    let mut h = fx.spawn(SessionSettings::default(), None).await;
    let _ = next_event(&mut h).await; // Started
    h.send(input("go")).await.unwrap();
    let e1 = collect_until(&mut h, is_turn_done).await;
    assert!(names(&e1).contains(&"BACKGROUND:1".to_string()), "{:?}", names(&e1));

    let e2 = collect_until(&mut h, |e| matches!(e, AgentEvent::TurnStarted)).await;
    assert!(names(&e2).contains(&"BACKGROUND:0".to_string()), "{:?}", names(&e2));
    assert!(h.is_busy(), "the CLI's own turn counts as busy (cancel can interrupt it)");
    // 这一轮进行中发消息：被接受，但先不写进 stdin
    h.send(input("next")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!fx.stdin_messages().iter().any(|m| m["message"]["content"] == "next"), "held back during the CLI's turn");

    // 这一轮结束后才写进去，两轮合成一个 turn 交给上层：中间没有 TurnDone
    let e3 = collect_until(&mut h, is_turn_done).await;
    assert_eq!(text_of(&e3), "worker donereply");
    assert_eq!(e3.iter().filter(|e| is_turn_done(e)).count(), 1);
    let usage = e3.iter().find_map(|e| match e {
        AgentEvent::Event(Event::TurnDone { usage, .. }) => usage.clone(),
        _ => None,
    });
    assert_eq!(usage.and_then(|u| u.input_tokens), Some(11), "both rounds' usage");
    let order: Vec<String> = fx
        .log_entries()
        .into_iter()
        .filter_map(|e| match e["kind"].as_str() {
            Some("stdin") => e["msg"]["message"]["content"].as_str().map(|c| format!("stdin:{c}")),
            Some("result") => Some("result".into()),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["stdin:go", "result", "result", "stdin:next", "result"]);
    assert!(!h.is_busy());
    h.shutdown().await.unwrap();
}
