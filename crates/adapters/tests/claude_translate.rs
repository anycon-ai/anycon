//! Claude Code 翻译器（stream-json → 事件）对固定 JSONL 的回放测试。
//! `tests/fixtures/claude/*.jsonl`：`real_*` 是对真实 CLI 2.1.293 的录制（已脱敏），其余按事实表手写。

use std::path::PathBuf;

use anycon_adapters::claude_code::permissions::{self, NativePending};
use anycon_adapters::claude_code::translate::{Out, Translator};
use anycon_agent_api::{AgentEvent, PendingPermission, PermissionDecision};
use anycon_protocol::{
    EditKind, Event, FileChangeKind, PermissionKind, PlanStatus, ToolCategory, TurnDoneReason, Usage,
};
use serde_json::{json, Value};

const CWD: &str = "/work/proj";

fn fixture(name: &str) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

#[derive(Default)]
struct Replay {
    outs: Vec<Out>,
}

impl Replay {
    /// 回放全文。第一行之前、以及每个 `result` 之后的下一行之前开一个新 turn。
    fn run(name: &str) -> Self {
        let mut t = Translator::new(CWD, json!({"model": "m"}), None);
        Self::run_with(&mut t, name, true)
    }

    fn run_with(t: &mut Translator, name: &str, begin: bool) -> Self {
        let mut outs = Vec::new();
        let mut need_turn = begin;
        for line in fixture(name) {
            if need_turn && line.trim_start().starts_with('{') {
                t.begin_turn();
                need_turn = false;
            }
            outs.extend(t.feed_line(&line));
            if line.contains("\"type\":\"result\"") {
                need_turn = true;
            }
        }
        Self { outs }
    }

    fn agent_events(&self) -> Vec<&AgentEvent> {
        self.outs.iter().filter_map(|o| if let Out::Event(e) = o { Some(e) } else { None }).collect()
    }

    fn events(&self) -> Vec<&Event> {
        self.agent_events()
            .into_iter()
            .filter_map(|e| if let AgentEvent::Event(ev) = e { Some(ev) } else { None })
            .collect()
    }

    fn perms(&self) -> Vec<(&PendingPermission, &NativePending)> {
        self.outs
            .iter()
            .filter_map(|o| if let Out::Permission { pending, native } = o { Some((pending, native)) } else { None })
            .collect()
    }

    fn types(&self) -> Vec<String> {
        self.outs
            .iter()
            .filter_map(|o| match o {
                Out::Event(AgentEvent::Event(ev)) => {
                    Some(serde_json::to_value(ev).unwrap()["type"].as_str().unwrap().to_string())
                }
                Out::Event(AgentEvent::Started { .. }) => Some("STARTED".into()),
                Out::Permission { .. } => Some("PERMISSION".into()),
                _ => None,
            })
            .collect()
    }

    fn text(&self) -> String {
        self.events()
            .into_iter()
            .filter_map(|e| if let Event::TextDelta { text } = e { Some(text.as_str()) } else { None })
            .collect()
    }

    fn turn_dones(&self) -> Vec<(u64, TurnDoneReason, Option<Usage>)> {
        self.events()
            .into_iter()
            .filter_map(|e| {
                if let Event::TurnDone { turn_id, reason, usage } = e {
                    Some((*turn_id, *reason, usage.clone()))
                } else {
                    None
                }
            })
            .collect()
    }

    fn errors(&self) -> Vec<(String, String, bool)> {
        self.events()
            .into_iter()
            .filter_map(|e| {
                if let Event::Error { code, message, recoverable } = e {
                    Some((code.clone(), message.clone(), *recoverable))
                } else {
                    None
                }
            })
            .collect()
    }
}

fn approx(a: Option<f64>, b: f64) {
    let a = a.expect("cost");
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

#[test]
fn text_stream_ignores_noise_and_dedups_assistant_text() {
    let r = Replay::run("text_stream.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "text_delta", "text_delta", "text_delta", "text_done", "turn_done"]);
    assert_eq!(r.text(), "Hello  world");
    match r.agent_events()[0] {
        AgentEvent::Started { resume } => {
            assert_eq!(resume.agent_session_id, "11111111-2222-3333-4444-555555555555");
            assert_eq!(resume.launch_args, json!({"model": "m"}));
        }
        other => panic!("{other:?}"),
    }
    let (turn_id, reason, usage) = r.turn_dones().remove(0);
    assert_eq!((turn_id, reason), (1, TurnDoneReason::Completed));
    let u = usage.unwrap();
    assert_eq!(u.input_tokens, Some(2));
    assert_eq!(u.output_tokens, Some(4));
    assert_eq!(u.cache_read_tokens, Some(21870));
    assert_eq!(u.cache_write_tokens, Some(10984));
    assert_eq!(u.duration_ms, 7497);
    assert_eq!(u.model.as_deref(), Some("claude-haiku-5-5"));
    approx(u.cost_usd, 0.0024177);
}

#[test]
fn started_is_not_repeated_when_session_id_was_announced_at_spawn() {
    let mut t = Translator::new(CWD, json!({}), Some("11111111-2222-3333-4444-555555555555".into()));
    let r = Replay::run_with(&mut t, "text_stream.jsonl", true);
    assert!(!r.types().contains(&"STARTED".to_string()));
    // 换了 id（CLI 自己 fork 了会话）才再发
    let mut t = Translator::new(CWD, json!({}), Some("other".into()));
    let r = Replay::run_with(&mut t, "text_stream.jsonl", true);
    assert_eq!(r.types().iter().filter(|x| *x == "STARTED").count(), 1);
}

#[test]
fn empty_thinking_emits_a_single_marker_delta() {
    let r = Replay::run("thinking_empty.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "thinking_delta", "thinking_done", "text_delta", "text_done", "turn_done"]);
    assert!(matches!(r.events()[0], Event::ThinkingDelta { text } if text.is_empty()));
    assert!(matches!(r.events()[1], Event::ThinkingDone { .. }));
    assert_eq!(r.text(), "42");
}

#[test]
fn visible_thinking_streams_text() {
    let r = Replay::run("thinking_visible.jsonl");
    assert_eq!(
        r.types(),
        vec!["STARTED", "thinking_delta", "thinking_delta", "thinking_done", "text_delta", "text_done", "turn_done"]
    );
    let th: String = r
        .events()
        .into_iter()
        .filter_map(|e| if let Event::ThinkingDelta { text } = e { Some(text.as_str()) } else { None })
        .collect();
    assert_eq!(th, "Let me think.");
}

#[test]
fn without_partial_messages_assistant_blocks_are_used() {
    let r = Replay::run("no_partial.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "thinking_delta", "thinking_done", "text_delta", "text_done", "turn_done"]);
    assert_eq!(r.text(), "Plain text without stream events");
}

#[test]
fn bash_tool_call_and_result() {
    let r = Replay::run("tool_bash.jsonl");
    assert_eq!(
        r.types(),
        vec!["STARTED", "text_delta", "text_done", "tool_call", "tool_result", "text_delta", "text_done", "turn_done"]
    );
    let call = r.events().into_iter().find(|e| matches!(e, Event::ToolCall { .. })).unwrap();
    match call {
        Event::ToolCall { id, name, category, title, input, detail, parent_id } => {
            assert_eq!(
                (id.as_str(), name.as_str(), *category, title.as_str()),
                ("toolu_bash1", "Bash", ToolCategory::Shell, "echo hi")
            );
            // 主 agent 自己的调用不带 parent_id
            assert_eq!(parent_id, &None);
            assert_eq!(input, &json!({"command": "echo hi", "description": "Print hi"}));
            let d = detail.as_ref().unwrap();
            assert_eq!((d.command.as_deref(), d.cwd.as_deref()), (Some("echo hi"), Some(CWD)));
        }
        _ => unreachable!(),
    }
    match r.events().into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { id, output, is_error, detail } => {
            assert_eq!(id, "toolu_bash1");
            assert_eq!(output, &json!("hi"));
            assert!(!is_error);
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
        }
        _ => unreachable!(),
    }
    assert_eq!(r.text(), "I'll run it.The `echo hi` command printed \"hi\".");
    approx(r.turn_dones()[0].2.as_ref().unwrap().cost_usd, 0.00468399);
}

#[test]
fn failing_bash_reports_exit_code() {
    let r = Replay::run("tool_bash_error.jsonl");
    match r.events().into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { is_error, detail, output, .. } => {
            assert!(is_error);
            assert_eq!(output, &json!("Exit code 1"));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(1));
        }
        _ => unreachable!(),
    }
}

#[test]
fn file_tools_search_and_web() {
    let r = Replay::run("tools_edit_read_search.jsonl");
    let evs = r.events();
    let calls: Vec<(&str, ToolCategory, &str)> = evs
        .iter()
        .filter_map(|e| {
            if let Event::ToolCall { name, category, title, .. } = e {
                Some((name.as_str(), *category, title.as_str()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("Edit", ToolCategory::FileEdit, "Edit /work/proj/src/a.rs"),
            ("Write", ToolCategory::FileEdit, "Write /work/proj/new.txt"),
            ("Read", ToolCategory::FileRead, "Read /work/proj/README.md"),
            ("Grep", ToolCategory::Search, "Grep fn main"),
            ("WebFetch", ToolCategory::Web, "Fetch https://example.com"),
            ("Edit", ToolCategory::FileEdit, "Edit /work/proj/src/a.rs"),
        ]
    );
    // 详情
    let details: Vec<_> = evs
        .iter()
        .filter_map(|e| if let Event::ToolCall { detail, .. } = e { Some(detail.clone().unwrap()) } else { None })
        .collect();
    assert_eq!((details[0].path.as_deref(), details[0].edit_kind), (Some("/work/proj/src/a.rs"), Some(EditKind::Edit)));
    assert_eq!(details[1].edit_kind, Some(EditKind::Write));
    assert_eq!(details[2].path.as_deref(), Some("/work/proj/README.md"));
    assert_eq!((details[3].query.as_deref(), details[3].path.as_deref()), (Some("fn main"), Some("src")));
    assert_eq!(details[4].url.as_deref(), Some("https://example.com"));

    let results: Vec<(&Value, bool, Option<&anycon_protocol::ToolResultDetail>)> = evs
        .iter()
        .filter_map(|e| {
            if let Event::ToolResult { output, is_error, detail, .. } = e {
                Some((output, *is_error, detail.as_ref()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results.len(), 6);
    // Edit：unified diff 相对 cwd
    assert_eq!(
        results[0].2.unwrap().unified_diff.as_deref(),
        Some("--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n fn main() {\n-    a();\n+    b();\n")
    );
    // Write 新建：合成 diff
    assert_eq!(
        results[1].2.unwrap().unified_diff.as_deref(),
        Some("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hi\n+there\n")
    );
    // Read：多个 text block 拼成一个字符串
    assert_eq!(results[2].0, &json!("     1\t# Title\n     2\tbody"));
    // Grep：match_count
    assert_eq!(results[3].2.unwrap().match_count, Some(1));
    // 失败的 Edit：不带 diff，也不产生 FileChanged
    assert!(results[5].1);
    assert!(results[5].2.is_none());

    let changed: Vec<(&str, FileChangeKind)> = evs
        .iter()
        .filter_map(|e| if let Event::FileChanged { path, kind } = e { Some((path.as_str(), *kind)) } else { None })
        .collect();
    assert_eq!(
        changed,
        vec![("/work/proj/src/a.rs", FileChangeKind::Modified), ("/work/proj/new.txt", FileChangeKind::Created)]
    );
}

#[test]
fn permission_round_trip_maps_request_and_builds_response() {
    let r = Replay::run("permission_roundtrip.jsonl");
    let perms = r.perms();
    assert_eq!(perms.len(), 1);
    let (p, native) = perms[0];
    assert_eq!(p.request_id, "req-perm-1");
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.title, "Write /work/proj/hello.txt");
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("Write", ToolCategory::FileEdit));
    assert_eq!(tool.input["content"], "hi\n");
    assert_eq!(tool.detail.as_ref().unwrap().path.as_deref(), Some("/work/proj/hello.txt"));
    assert_eq!(native.tool_use_id.as_deref(), Some("toolu_perm1"));

    // allow / allow_session / deny 对应的 control_response
    let allow = permissions::build_decision(
        native,
        &PermissionDecision { option_id: Some("allow".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(allow["behavior"], "allow");
    assert_eq!(allow["updatedInput"]["file_path"], "/work/proj/hello.txt");
    let envelope = permissions::control_response("req-perm-1", allow);
    assert_eq!(envelope["response"]["request_id"], "req-perm-1");
    assert_eq!(envelope["response"]["subtype"], "success");
    let sess = permissions::build_decision(
        native,
        &PermissionDecision { option_id: Some("allow_session".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(sess["updatedPermissions"][0]["rules"], json!([{"toolName": "Write"}]));
    assert_eq!(sess["updatedPermissions"][0]["destination"], "session");

    // 之后 tool_result 与 FileChanged 照常出来
    let types = r.types();
    let after = types.iter().position(|t| t == "PERMISSION").unwrap();
    assert_eq!(&types[after + 1..], ["tool_result", "file_changed", "text_delta", "text_done", "turn_done"]);
}

#[test]
fn permission_deny_flow() {
    let r = Replay::run("permission_deny.jsonl");
    let (p, native) = r.perms()[0];
    assert_eq!(p.title, "Run command");
    let d = permissions::build_decision(
        native,
        &PermissionDecision { option_id: Some("deny".into()), message: Some("not now".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(d, json!({"behavior": "deny", "message": "not now"}));
    match r.events().into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { is_error, detail, .. } => {
            assert!(is_error);
            assert!(detail.is_none(), "denied command has no exit code");
        }
        _ => unreachable!(),
    }
}

#[test]
fn todowrite_becomes_plan_and_hides_the_tool() {
    let r = Replay::run("todowrite.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "plan", "turn_done"]);
    match r.events()[0] {
        Event::Plan { entries } => {
            let got: Vec<(&str, PlanStatus)> = entries.iter().map(|e| (e.content.as_str(), e.status)).collect();
            assert_eq!(
                got,
                vec![
                    ("Read the code", PlanStatus::Completed),
                    ("Write the fix", PlanStatus::InProgress),
                    ("Run tests", PlanStatus::Pending)
                ]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn exit_plan_mode_is_a_plan_permission() {
    let r = Replay::run("exit_plan_mode.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "PERMISSION", "text_delta", "text_done", "turn_done"]);
    let (p, native) = r.perms()[0];
    assert_eq!(p.kind, PermissionKind::Plan);
    assert_eq!(p.detail.text.as_deref(), Some("## Plan\n1. Add the module\n2. Write tests"));
    let ok = permissions::build_decision(
        native,
        &PermissionDecision { option_id: Some("allow".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(ok["behavior"], "allow");
    assert_eq!(ok["updatedInput"]["plan"], "## Plan\n1. Add the module\n2. Write tests");
}

#[test]
fn ask_user_question_is_a_question_permission() {
    let r = Replay::run("ask_question.jsonl");
    assert_eq!(r.types(), vec!["STARTED", "PERMISSION", "text_delta", "text_done", "turn_done"]);
    let (p, native) = r.perms()[0];
    assert_eq!(p.kind, PermissionKind::Question);
    let q = &p.detail.questions[0];
    assert_eq!((q.id.as_str(), q.text.as_str()), ("q1", "Which database?"));
    assert_eq!(q.choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), vec!["Postgres", "SQLite"]);
    let mut answers = serde_json::Map::new();
    answers.insert("q1".into(), json!("SQLite"));
    let d = permissions::build_decision(native, &PermissionDecision { answers: Some(answers), ..Default::default() })
        .unwrap();
    assert_eq!(d["updatedInput"]["answers"], json!({"Which database?": "SQLite"}));
    assert!(d["updatedInput"]["questions"].is_array());
}

#[test]
fn result_subtypes_map_to_reasons() {
    let r = Replay::run("result_max_turns.jsonl");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::MaxTurns);
    assert!(r.errors().is_empty());

    let r = Replay::run("result_budget.jsonl");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Budget);
    assert!(r.errors().is_empty());

    let r = Replay::run("result_error.jsonl");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Error);
    assert_eq!(r.errors(), vec![("agent_error".to_string(), "Tool execution failed: boom".to_string(), true)]);
    // Error 事件在 TurnDone 之前
    assert_eq!(r.types(), vec!["STARTED", "error", "turn_done"]);

    let r = Replay::run("result_auth_error.jsonl");
    assert_eq!(r.errors(), vec![("auth_failed".to_string(), "Invalid API key · Please run /login".to_string(), false)]);
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Error);

    let r = Replay::run("result_rate_limited.jsonl");
    assert_eq!(r.errors()[0].0, "rate_limited");
}

#[test]
fn interrupted_result_is_cancelled_only_when_we_asked_for_it() {
    // 没要求取消：这是个错误
    let r = Replay::run("interrupted.jsonl");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Error);
    assert_eq!(r.errors().len(), 1);

    // 要求了取消：cancelled，不报错
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut outs = Vec::new();
    for (i, line) in fixture("interrupted.jsonl").iter().enumerate() {
        if i == 2 {
            t.mark_cancel_requested();
        }
        outs.extend(t.feed_line(line));
    }
    let r = Replay { outs };
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Cancelled);
    assert!(r.errors().is_empty());
    assert!(!t.is_busy());
}

#[test]
fn real_recording_interrupt_during_thinking() {
    // 真实 CLI：interrupt 的回执 `{still_queued: []}`，一条 `[Request interrupted by user]` 用户消息，
    // 然后 `result { subtype: error_during_execution, is_error: true, terminal_reason: aborted_streaming }`，用量全 0。
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut outs = Vec::new();
    for line in fixture("real_interrupted.jsonl") {
        if line.contains("\"type\":\"control_response\"") {
            t.mark_cancel_requested();
        }
        outs.extend(t.feed_line(&line));
    }
    let r = Replay { outs };
    assert_eq!(r.types(), vec!["STARTED", "thinking_delta", "thinking_done", "turn_done"]);
    let (_, reason, usage) = r.turn_dones().remove(0);
    assert_eq!(reason, TurnDoneReason::Cancelled);
    assert_eq!(usage.unwrap().output_tokens, Some(0));
    assert!(r.errors().is_empty());
    // 回执本身被识别为 ControlAck
    assert!(r.outs.iter().any(|o| matches!(o, Out::ControlAck(a) if a.ok && a.request_id == "int-1")));

    // 没要求取消的同一份输出，则是一个错误
    let r = Replay::run("real_interrupted.jsonl");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Error);
    assert_eq!(r.errors()[0].0, "agent_error");
}

#[test]
fn real_recording_question_and_plan() {
    // 真实 CLI（plan 模式，--permission-prompt-tool stdio）：AskUserQuestion、ExitPlanMode 都走 can_use_tool，
    // 且用我们假设的答复形状（updatedInput 带 answers / 原样回传 input）CLI 接受了——录制时的 tool_result 可以印证。
    let r = Replay::run("real_question_plan.jsonl");
    let kinds: Vec<PermissionKind> = r.perms().iter().map(|(p, _)| p.kind).collect();
    assert_eq!(kinds, vec![PermissionKind::Question, PermissionKind::Plan, PermissionKind::Tool]);

    let (q, qn) = r.perms()[0];
    assert_eq!(q.detail.questions[0].text, "Which color do you prefer?");
    assert_eq!(q.detail.questions[0].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["red", "blue"]);
    let mut answers = serde_json::Map::new();
    answers.insert("q1".into(), json!("blue"));
    let d =
        permissions::build_decision(qn, &PermissionDecision { answers: Some(answers), ..Default::default() }).unwrap();
    assert_eq!(d["behavior"], "allow");
    assert_eq!(d["updatedInput"]["answers"], json!({"Which color do you prefer?": "blue"}));

    let (p, pn) = r.perms()[1];
    assert!(p.detail.text.as_deref().unwrap().starts_with("# Plan: Create hello.txt"));
    let d =
        permissions::build_decision(pn, &PermissionDecision { option_id: Some("allow".into()), ..Default::default() })
            .unwrap();
    assert!(d["updatedInput"]["planFilePath"].as_str().unwrap().ends_with(".md"), "the whole input is echoed back");

    // 这两个工具不再出现为 tool_call / tool_result（权限请求已经表达了它们）
    let call_names: Vec<&str> = r
        .events()
        .into_iter()
        .filter_map(|e| if let Event::ToolCall { name, .. } = e { Some(name.as_str()) } else { None })
        .collect();
    assert!(!call_names.contains(&"AskUserQuestion") && !call_names.contains(&"ExitPlanMode"), "{call_names:?}");
    assert!(call_names.contains(&"ToolSearch"));
    assert_eq!(r.turn_dones().len(), 2);
    assert!(r.turn_dones().iter().all(|d| d.1 == TurnDoneReason::Completed));
}

#[test]
fn mcp_and_subagent() {
    let r = Replay::run("mcp_and_subagent.jsonl");
    let evs = r.events();
    let calls: Vec<(&str, ToolCategory)> = evs
        .iter()
        .filter_map(|e| {
            if let Event::ToolCall { name, category, .. } = e {
                Some((name.as_str(), *category))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("mcp__github__create_issue", ToolCategory::Mcp),
            ("Task", ToolCategory::Subagent),
            ("Read", ToolCategory::FileRead)
        ]
    );
    match evs.iter().find(|e| matches!(e, Event::ToolCall { name, .. } if name.starts_with("mcp__"))).unwrap() {
        Event::ToolCall { title, detail, .. } => {
            assert_eq!(title, "github/create_issue");
            let d = detail.as_ref().unwrap();
            assert_eq!((d.server.as_deref(), d.tool.as_deref()), (Some("github"), Some("create_issue")));
        }
        _ => unreachable!(),
    }
    match evs.iter().find(|e| matches!(e, Event::ToolCall { name, .. } if name == "Task")).unwrap() {
        Event::ToolCall { detail, .. } => {
            assert_eq!(detail.as_ref().unwrap().description.as_deref(), Some("explore the repo"))
        }
        _ => unreachable!(),
    }
    // 子 agent 的文本不混进主文本流
    assert!(!r.text().contains("SUBAGENT TEXT"));
    // 子 agent 内部的调用指向启动它的那次 Task 调用；主 agent 自己的调用不带
    let parents: Vec<(&str, Option<&str>)> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { id, parent_id, .. } => Some((id.as_str(), parent_id.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(parents, vec![("toolu_mcp1", None), ("toolu_task1", None), ("toolu_inner1", Some("toolu_task1"))]);
    assert!(r.types().iter().filter(|t| *t == "tool_result").count() == 3);
}

#[test]
fn background_subagent_gets_its_final_result_from_the_task_notification() {
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut feed = |v: Value| Replay { outs: t.feed_line(&v.to_string()) };
    let call = |id: &str, desc: &str| {
        json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s", "message": {"id": format!("m-{id}"), "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": "Agent", "input": {"description": desc, "prompt": "p", "run_in_background": true}}]}})
    };
    feed(call("toolu_bg", "backend API"));
    feed(call("toolu_fg", "quick look"));

    // 后台启动：马上返回 async_launched，结果标 background
    let r = feed(json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_bg", "content": "Async agent launched successfully."}]},
        "tool_use_result": {"status": "async_launched", "agentId": "a1", "description": "backend API"}}));
    match r.events()[..] {
        [Event::ToolResult { id, is_error, detail, .. }] => {
            assert_eq!((id.as_str(), *is_error), ("toolu_bg", false));
            assert_eq!(detail.as_ref().and_then(|d| d.background), Some(true));
        }
        ref other => panic!("{other:?}"),
    }
    // 前台子 agent 正常返回，不标 background；它的 task_notification 不再补结果
    let r = feed(json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_fg", "content": "done"}]},
        "tool_use_result": {"status": "completed", "agentId": "a2"}}));
    assert!(matches!(r.events()[..], [Event::ToolResult { detail: None, .. }]));
    let r = feed(
        json!({"type": "system", "subtype": "task_notification", "task_id": "a2", "tool_use_id": "toolu_fg", "status": "completed", "summary": "x", "session_id": "s"}),
    );
    assert!(r.events().is_empty());

    // 后台子 agent 还在跑：它的调用带 parent_id
    let r = feed(
        json!({"type": "assistant", "parent_tool_use_id": "toolu_bg", "session_id": "s", "message": {"id": "m-in", "role": "assistant",
        "content": [{"type": "tool_use", "id": "toolu_in", "name": "Bash", "input": {"command": "ls"}}]}}),
    );
    assert!(matches!(r.events()[..], [Event::ToolCall { parent_id: Some(p), .. }] if p == "toolu_bg"));

    // 结束：同一个 id 再来一条结果（总结），不带 background；只补一次
    let done = json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "tool_use_id": "toolu_bg", "status": "completed", "summary": "API done", "session_id": "s"});
    let r = feed(done.clone());
    match r.events()[..] {
        [Event::ToolResult { id, output, is_error, detail }] => {
            assert_eq!((id.as_str(), output, *is_error, detail), ("toolu_bg", &json!("API done"), false, &None));
        }
        ref other => panic!("{other:?}"),
    }
    assert!(feed(done).events().is_empty());
}

#[test]
fn subagent_token_totals_come_from_the_result_or_the_task_notification() {
    // 形状取自 Claude Code 2.1.293 的实际输出
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut feed = |v: Value| Replay { outs: t.feed_line(&v.to_string()) };
    for (id, bg) in [("toolu_fg", false), ("toolu_bg", true)] {
        feed(
            json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s", "message": {"id": format!("m-{id}"), "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": "Agent", "input": {"description": id, "prompt": "p", "run_in_background": bg}}]}}),
        );
    }
    let tokens = |r: &Replay| match r.events()[..] {
        [Event::ToolResult { detail, .. }] => detail.as_ref().and_then(|d| d.total_tokens),
        ref other => panic!("{other:?}"),
    };

    // 前台：tool_use_result.totalTokens
    let r = feed(json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_fg", "content": [{"type": "text", "text": "ALPHA"}]}]},
        "tool_use_result": {"status": "completed", "agentId": "a1", "totalDurationMs": 1669, "totalTokens": 14275, "totalToolUseCount": 0}}));
    assert_eq!(tokens(&r), Some(14275));

    // 后台：启动时还没有用量，结束时取 task_notification.usage.total_tokens
    let r = feed(json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_bg", "content": "launched"}]},
        "tool_use_result": {"isAsync": true, "status": "async_launched", "agentId": "a2"}}));
    assert_eq!(tokens(&r), None);
    let r = feed(
        json!({"type": "system", "subtype": "task_notification", "task_id": "a2", "tool_use_id": "toolu_bg", "status": "completed",
        "summary": "BETA", "usage": {"total_tokens": 13009, "tool_uses": 0, "duration_ms": 1589}, "session_id": "s"}),
    );
    assert_eq!(tokens(&r), Some(13009));
}

#[test]
fn failed_background_subagent_is_an_error_but_a_stopped_one_is_not() {
    for (status, is_err) in [("failed", true), ("stopped", false)] {
        let mut t = Translator::new(CWD, json!({}), None);
        t.begin_turn();
        t.feed_line(&json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s", "message": {"id": "m", "role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_bg", "name": "Agent", "input": {"description": "d", "prompt": "p"}}]}}).to_string());
        t.feed_line(&json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_bg", "content": "launched"}]},
            "tool_use_result": {"status": "async_launched"}}).to_string());
        let r = Replay { outs: t.feed_line(&json!({"type": "system", "subtype": "task_notification", "tool_use_id": "toolu_bg", "status": status, "summary": ""}).to_string()) };
        assert!(matches!(r.events()[..], [Event::ToolResult { is_error, .. }] if *is_error == is_err), "{status}");
    }
}

#[test]
fn cost_is_a_per_turn_delta_of_the_cumulative_total() {
    let r = Replay::run("two_turns.jsonl");
    let dones = r.turn_dones();
    assert_eq!(dones.len(), 2);
    assert_eq!((dones[0].0, dones[1].0), (1, 2));
    let (u1, u2) = (dones[0].2.clone().unwrap(), dones[1].2.clone().unwrap());
    approx(u1.cost_usd, 0.002);
    approx(u2.cost_usd, 0.0015);
    assert_eq!((u1.input_tokens, u2.input_tokens), (Some(2), Some(5)));
    assert_eq!((u1.cache_read_tokens, u2.cache_read_tokens), (Some(200), Some(300)));
    assert_eq!((u1.cache_write_tokens, u2.cache_write_tokens), (Some(100), Some(7)));
    assert_eq!((u1.duration_ms, u2.duration_ms), (1000, 2000));
    // 每个 turn 都有 system/init，但 session id 没变，只发一次 Started
    assert_eq!(r.types().iter().filter(|t| *t == "STARTED").count(), 1);
}

#[test]
fn real_recording_pong_and_bash() {
    let r = Replay::run("real_pong_and_bash.jsonl");
    let types = r.types();
    assert_eq!(types[0], "STARTED");
    let dones = r.turn_dones();
    assert_eq!(dones.len(), 2);
    assert!(dones.iter().all(|d| d.1 == TurnDoneReason::Completed));
    assert!(r.text().starts_with("pong"));
    assert!(r.text().contains("echo hi"));
    // 第二个 turn 的费用是增量：0.00468399 - 0.0024177
    approx(dones[0].2.as_ref().unwrap().cost_usd, 0.0024177);
    approx(dones[1].2.as_ref().unwrap().cost_usd, 0.00468399 - 0.0024177);
    assert_eq!(dones[1].2.as_ref().unwrap().cache_read_tokens, Some(58009));
    // Bash 被 CLI 自己按规则放行：没有权限请求，只有 tool_call / tool_result
    assert!(r.perms().is_empty());
    assert!(types.contains(&"tool_call".to_string()) && types.contains(&"tool_result".to_string()));
    assert_eq!(dones[0].2.as_ref().unwrap().model.as_deref(), Some("claude-haiku-5-5"));
}

#[test]
fn real_recording_permission_deny_then_allow() {
    let r = Replay::run("real_permission.jsonl");
    let perms = r.perms();
    assert_eq!(perms.len(), 2);
    for (p, native) in &perms {
        assert_eq!(p.kind, PermissionKind::Tool);
        assert_eq!(native.tool_name, "Write");
        assert_eq!(p.title, "Write /work/proj/hello.txt");
        assert_eq!(p.request_id.len(), 36, "request ids are UUIDs");
    }
    let results: Vec<bool> = r
        .events()
        .into_iter()
        .filter_map(|e| if let Event::ToolResult { is_error, .. } = e { Some(*is_error) } else { None })
        .collect();
    assert_eq!(results, vec![true, false], "first write was declined, second one succeeded");
    assert!(r.events().iter().any(|e| matches!(e, Event::FileChanged { kind: FileChangeKind::Created, .. })));
    assert_eq!(r.turn_dones().len(), 2);
    // 真实 CLI 的思考块只有空文本
    assert!(r.events().iter().any(|e| matches!(e, Event::ThinkingDelta { text } if text.is_empty())));
}

/// `real_*` 录制会随仓库公开，里面不能有开发者本机的环境（已装的技能 / 斜杠命令 / 插件 / MCP 服务器名、
/// 本机路径、用户名、邮箱、密钥）。新录的样本入库前按 README 里的说明清洗，这个测试兜底。
#[test]
fn real_recordings_carry_no_local_environment() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("real_") && n.ends_with(".jsonl"))
        })
        .collect();
    files.sort();
    assert!(files.len() >= 4, "{files:?}");

    // 本机路径 / 临时目录 / 账号标记 / 密钥前缀。模型输出里的路径只允许占位的 `/work/proj` 和 `/home/user`。
    const FORBIDDEN: &[&str] = &[
        "/Users/",
        "/private/",
        "/var/folders",
        "/tmp/",
        "C:\\Users",
        "scratch",
        "sk-ant-",
        "sk-proj-",
        "AKIA",
        "ghp_",
    ];
    let names_are_placeholders = |v: &Value| {
        v.as_array()
            .map(|a| {
                a.iter().all(|x| {
                    let name = x.as_str().or_else(|| x.get("name").and_then(Value::as_str)).unwrap_or("");
                    name.starts_with("example-")
                })
            })
            .unwrap_or(false)
    };

    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        // 流式 delta 把长字符串切碎了：按 content block 拼回去再查，免得路径正好被切开躲过检查
        let mut haystacks = vec![text.clone()];
        let mut block: Option<String> = None;
        for line in text.lines() {
            let v: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}: {e}"));
            if v["type"] == "system" && v["subtype"] == "init" {
                for k in ["slash_commands", "skills", "plugins", "mcp_servers"] {
                    assert!(names_are_placeholders(&v[k]), "{name}: init.{k} must be placeholders, got {}", v[k]);
                }
            }
            if v["type"] != "stream_event" {
                continue;
            }
            match v["event"]["type"].as_str() {
                Some("content_block_start") => block = Some(String::new()),
                Some("content_block_delta") => {
                    let d = &v["event"]["delta"];
                    let piece = d["partial_json"].as_str().or_else(|| d["text"].as_str()).unwrap_or("");
                    if let Some(b) = block.as_mut() {
                        b.push_str(piece);
                    }
                }
                Some("content_block_stop") => haystacks.extend(block.take()),
                _ => {}
            }
        }
        for hay in &haystacks {
            for bad in FORBIDDEN {
                assert!(
                    !hay.contains(bad),
                    "{name}: contains {bad:?} near {:?}",
                    hay.split(bad).next().map(|s| &s[s.len().saturating_sub(40)..])
                );
            }
            // `/home/<x>` 只允许占位用户名 `user`
            for (i, _) in hay.match_indices("/home/") {
                assert!(
                    hay[i + 6..].starts_with("user/"),
                    "{name}: unexpected home directory in {:?}",
                    &hay[i..(i + 30).min(hay.len())]
                );
            }
            // 没有邮箱（`@builtin` 是 CLI 的插件来源标记，占位名后面带它）
            assert!(!hay.replace("@builtin", "").contains('@'), "{name}: contains an `@` (an email address?)");
        }
    }
}

#[test]
fn control_messages() {
    let mut t = Translator::new(CWD, json!({}), None);
    // 成功回执
    let outs = t.feed_line(
        r#"{"type":"control_response","response":{"subtype":"success","request_id":"r2","response":{"mode":"plan"}}}"#,
    );
    match &outs[0] {
        Out::ControlAck(a) => {
            assert!(a.ok);
            assert_eq!(a.request_id, "r2");
            assert_eq!(a.response["mode"], "plan");
        }
        other => panic!("{other:?}"),
    }
    // 失败回执（真实 CLI 的措辞）
    let outs = t.feed_line(
        r#"{"type":"control_response","response":{"subtype":"error","request_id":"r4","error":"Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions","error_code":"bypass_not_launched"}}"#,
    );
    match &outs[0] {
        Out::ControlAck(a) => {
            assert!(!a.ok);
            assert_eq!(a.error_code.as_deref(), Some("bypass_not_launched"));
        }
        other => panic!("{other:?}"),
    }
    // CLI 撤回未决请求
    let outs = t.feed_line(r#"{"type":"control_cancel_request","request_id":"req-1"}"#);
    assert_eq!(outs, vec![Out::ControlCancel { request_id: "req-1".into() }]);
    // 不认识的 control_request
    let outs = t.feed_line(
        r#"{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"x"}}"#,
    );
    assert_eq!(outs, vec![Out::Unsupported { request_id: "h1".into(), subtype: "hook_callback".into() }]);
    // 垃圾
    assert!(t.feed_line("").is_empty());
    assert!(t.feed_line("not json").is_empty());
    assert!(t.feed_line("{broken").is_empty());
    assert!(t.feed_line(r#"{"type":"something_new","x":1}"#).is_empty());
}

#[test]
fn result_without_a_turn_only_reports_the_error() {
    // 例如 `--resume` 一个不存在的会话：CLI 直接吐一个 error result，没有任何 send
    let mut t = Translator::new(CWD, json!({}), None);
    let outs = t.feed_line(
        r#"{"type":"result","subtype":"error_during_execution","duration_ms":0,"is_error":true,"num_turns":0,"session_id":"s","total_cost_usd":0,"usage":{},"errors":["No conversation found with session ID: s"]}"#,
    );
    let r = Replay { outs };
    assert_eq!(r.types(), vec!["error"]);
    assert!(r.turn_dones().is_empty());
}

#[test]
fn finish_reports_how_the_process_died() {
    // 空闲时退出：什么都不用补
    let mut t = Translator::new(CWD, json!({}), None);
    let f = t.finish();
    assert!(f.out.is_empty() && !f.died_mid_turn && !f.cancelled);

    // turn 中途死掉
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let f = t.finish();
    assert!(f.died_mid_turn && f.out.is_empty());

    // 我们要求取消后进程退出：补一个 cancelled 的 TurnDone
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    t.mark_cancel_requested();
    let f = t.finish();
    assert!(!f.died_mid_turn && f.cancelled);
    match &f.out[0] {
        Out::Event(AgentEvent::Event(Event::TurnDone {
            turn_id: 1,
            reason: TurnDoneReason::Cancelled,
            usage: None,
        })) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn echoed_user_messages_with_images_never_become_events() {
    // 带 `--replay-user-messages` 之类的情形下 CLI 会把用户消息（含 image block 的 base64）回显出来：
    // `on_user` 只看 tool_result，其它 block 一概不管，base64 不会变成任何事件
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
    let echo = json!({
        "type": "user",
        "message": {"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64}},
            {"type": "text", "text": "what color is this?"},
        ]},
        "parent_tool_use_id": null, "session_id": "s", "isReplay": true,
    });
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    assert!(t.feed_line(&echo.to_string()).is_empty());
    // 字符串 content 的回显同样没有事件
    let plain = json!({"type": "user", "message": {"role": "user", "content": "hi"}, "session_id": "s"});
    assert!(t.feed_line(&plain.to_string()).is_empty());
}

#[test]
fn tool_result_images_are_replaced_by_a_placeholder_in_the_event() {
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==";
    let call = json!({
        "type": "assistant",
        "message": {"model": "m", "id": "msg_1", "type": "message", "role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_read1", "name": "Read", "input": {"file_path": "/work/proj/shot.png"}},
        ], "stop_reason": null, "usage": {}},
        "parent_tool_use_id": null, "session_id": "s",
    });
    let result = json!({
        "type": "user",
        "message": {"role": "user", "content": [{
            "tool_use_id": "toolu_read1", "type": "tool_result", "is_error": false,
            "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64}}],
        }]},
        "parent_tool_use_id": null, "session_id": "s",
    });
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut outs = t.feed_line(&call.to_string());
    outs.extend(t.feed_line(&result.to_string()));
    let r = Replay { outs };
    match r.events().into_iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { id, output, is_error, .. } => {
            assert_eq!((id.as_str(), *is_error), ("toolu_read1", false));
            assert_eq!(
                output,
                &json!([{"type": "image", "source": {
                    "type": "base64", "media_type": "image/png",
                    "data": format!("[omitted: {} bytes of base64]", b64.len()),
                }}])
            );
        }
        _ => unreachable!(),
    }
    let all = serde_json::to_string(&r.events()).unwrap();
    assert!(!all.contains("iVBOR"), "image base64 leaked into the events: {all}");
}
