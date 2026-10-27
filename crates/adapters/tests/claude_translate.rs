//! Claude Code 翻译器（stream-json → 事件）对固定 JSONL 的回放测试。
//! `tests/fixtures/claude/*.jsonl`：`real_*` 是对真实 CLI 2.1.293 的录制（已脱敏），其余按事实表手写。

use std::path::PathBuf;

use anycon_adapters::claude_code::permissions::{self, NativePending};
use anycon_adapters::claude_code::translate::{Out, Translator, UserTurn};
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
fn background_command_gets_its_final_result_and_exit_code_from_the_task_notification() {
    // 形状取自 Claude Code 2.1.293：Bash 的 `tool_use_result.backgroundTaskId`、通知的 `summary` 末尾带 `(exit code N)`
    let call = |id: &str| {
        json!({"type": "assistant", "parent_tool_use_id": null, "session_id": "s", "message": {"id": format!("m-{id}"), "role": "assistant",
            "content": [{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "cargo test", "run_in_background": true}}]}})
    };
    let launched = |id: &str, tur: Value| {
        json!({"type": "user", "parent_tool_use_id": null, "session_id": "s", "tool_use_result": tur,
            "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": id, "content": "Command running in background with ID: b1."}]}})
    };
    let done = |id: &str, status: &str, summary: &str| json!({"type": "system", "subtype": "task_notification", "task_id": "b1", "tool_use_id": id, "status": status, "summary": summary, "session_id": "s"});
    // 任务快照（`Event::Task`）另有测试；这里只看工具结果
    let result =
        |r: &Replay| match r.events().into_iter().filter(|e| matches!(e, Event::ToolResult { .. })).collect::<Vec<_>>()
            [..]
        {
            [Event::ToolResult { output, is_error, detail, .. }] => (
                output.clone(),
                *is_error,
                detail.as_ref().and_then(|d| d.background),
                detail.as_ref().and_then(|d| d.exit_code),
            ),
            ref other => panic!("{other:?}"),
        };

    for (status, summary, want) in [
        ("completed", "Background command \"cargo test\" completed (exit code 0)", (false, Some(0))),
        ("completed", "Background command \"cargo test\" completed (exit code 101)", (true, Some(101))),
        ("failed", "Background command \"cargo test\" failed (exit code 1: killed by signal)", (true, Some(1))),
        ("killed", "Background command \"cargo test\" was stopped", (false, None)),
    ] {
        let mut t = Translator::new(CWD, json!({}), None);
        t.begin_turn();
        let mut feed = |v: Value| Replay { outs: t.feed_line(&v.to_string()) };
        feed(call("toolu_sh"));
        // 启动：还没有退出码，标 background
        let r = feed(launched(
            "toolu_sh",
            json!({"stdout": "", "stderr": "", "interrupted": false, "backgroundTaskId": "b1"}),
        ));
        assert_eq!(result(&r), (json!("Command running in background with ID: b1."), false, Some(true), None));
        // 结束：同一个 id 补上通知的摘要和退出码；只补一次
        let r = feed(done("toolu_sh", status, summary));
        assert_eq!(result(&r), (json!(summary), want.0, None, want.1), "{summary}");
        assert!(feed(done("toolu_sh", status, summary))
            .events()
            .iter()
            .all(|e| !matches!(e, Event::ToolResult { .. })));
    }

    // 没有 `backgroundTaskId` 的结果：`task_started { is_backgrounded }` 先报了这个调用，同样算后台
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let mut feed = |v: Value| Replay { outs: t.feed_line(&v.to_string()) };
    feed(call("toolu_sh"));
    feed(
        json!({"type": "system", "subtype": "task_started", "task_id": "b1", "tool_use_id": "toolu_sh", "is_backgrounded": true, "task_type": "local_bash"}),
    );
    let r = feed(launched("toolu_sh", Value::Null));
    assert_eq!(result(&r).2, Some(true));
    let r = feed(done("toolu_sh", "completed", "Background command \"cargo test\" completed (exit code 0)"));
    assert_eq!(result(&r).3, Some(0));

    // 前台命令照旧：退出码 0，不标 background，之后的通知不补结果
    feed(call("toolu_fg"));
    let r = feed(json!({"type": "user", "parent_tool_use_id": null, "session_id": "s",
        "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_fg", "content": "ok"}]}}));
    assert_eq!(result(&r), (json!("ok"), false, None, Some(0)));
    assert!(feed(done("toolu_fg", "completed", "")).events().is_empty());
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

/// 事件序列（含 `TurnStarted` / `Background`），工具调用带上名字，便于对照时间线。
fn timeline(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Event(AgentEvent::TurnStarted) => Some("TURN_STARTED".to_string()),
            Out::Event(AgentEvent::Background { tasks }) => Some(format!("BACKGROUND:{tasks}")),
            Out::Event(AgentEvent::Started { .. }) => Some("STARTED".to_string()),
            Out::Event(AgentEvent::Event(Event::ToolCall { name, .. })) => Some(format!("tool_call:{name}")),
            Out::Event(AgentEvent::Event(ev)) => {
                Some(serde_json::to_value(ev).unwrap()["type"].as_str().unwrap().into())
            }
            Out::Write(_) => Some("WRITE".to_string()),
            _ => None,
        })
        .collect()
}

fn feed_all(t: &mut Translator, lines: &[String]) -> Vec<Out> {
    lines.iter().flat_map(|l| t.feed_line(l)).collect()
}

#[test]
fn background_tasks_and_the_turns_the_cli_starts_on_its_own() {
    // 用户的一个 turn 派出后台 subagent 和后台命令后结束；之后每个任务结束，CLI 都自己再跑一轮（没有 `send`）
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let outs = feed_all(&mut t, &fixture("background_tasks.jsonl"));
    assert_eq!(
        timeline(&outs),
        [
            "STARTED",
            "tool_call:Agent",
            "BACKGROUND:1",
            // `task_started`：两个后台任务各出第一条快照
            "task",
            "tool_result",
            "tool_call:Bash",
            "BACKGROUND:2",
            "task",
            "tool_result",
            "text_delta",
            "text_done",
            "turn_done",
            // 主 turn 结束之后 subagent 还在干活：`task_progress` 一条快照
            "tool_call:Edit",
            "task",
            // 后台命令结束：`task_updated { completed }` 先到（终态快照、任务数减一），`task_notification` 补上摘要
            // 和最终结果，CLI 再自己开始一轮
            "task",
            "BACKGROUND:1",
            "task",
            "tool_result",
            "TURN_STARTED",
            "text_delta",
            "text_done",
            // subagent 的 Edit 跨了 turn：结果在这一轮到，工具信息还在，照样有 diff 和 file_changed
            "tool_result",
            "file_changed",
            "turn_done",
            // subagent 结束：`task_updated { completed }` 先到，后台任务数归零；`task_notification` 再给它的调用补上最终结果
            "task",
            "BACKGROUND:0",
            "task",
            "tool_result",
            "TURN_STARTED",
            "text_delta",
            "text_done",
            "turn_done",
        ]
    );
    let r = Replay { outs };
    let dones = r.turn_dones();
    assert_eq!(
        dones.iter().map(|d| (d.0, d.1)).collect::<Vec<_>>(),
        [(1, TurnDoneReason::Completed), (2, TurnDoneReason::Completed), (3, TurnDoneReason::Completed)]
    );
    // 费用照旧是累计值的差：自己开的那几轮也分到自己的份
    approx(dones[0].2.as_ref().unwrap().cost_usd, 0.01);
    approx(dones[1].2.as_ref().unwrap().cost_usd, 0.005);
    approx(dones[2].2.as_ref().unwrap().cost_usd, 0.005);
    let edit = r.events().into_iter().find_map(|e| match e {
        Event::ToolResult { id, detail, .. } if id == "toolu_sub_edit" => Some(detail.clone()),
        _ => None,
    });
    assert!(edit.flatten().and_then(|d| d.unified_diff).is_some_and(|d| d.contains("+b")));
    let agent_results: Vec<_> = r
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::ToolResult { id, output, detail, .. } if id == "toolu_agent" => {
                Some((output.clone(), detail.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(agent_results.len(), 2, "launched, then the final result");
    assert_eq!(agent_results[0].1.as_ref().and_then(|d| d.background), Some(true));
    assert_eq!(agent_results[1].0, json!("Tests fixed."));
    assert_eq!(agent_results[1].1.as_ref().and_then(|d| d.total_tokens), Some(4200));
    let bash_results: Vec<_> = r
        .events()
        .into_iter()
        .filter_map(|e| match e {
            Event::ToolResult { id, output, detail, .. } if id == "toolu_bg" => Some((output.clone(), detail.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(bash_results.len(), 2, "launched, then the final result");
    assert_eq!(bash_results[0].1.as_ref().map(|d| (d.background, d.exit_code)), Some((Some(true), None)));
    assert_eq!(bash_results[1].0, json!("Background command \"cargo build\" completed (exit code 0)"));
    assert_eq!(bash_results[1].1.as_ref().map(|d| (d.background, d.exit_code)), Some((None, Some(0))));
    assert!(!t.is_busy());
}

#[test]
fn background_count_falls_back_to_task_lifecycle_messages() {
    // 没有 `background_tasks_changed` 的 CLI：按 `task_started`（后台的）/ `task_notification` / `task_updated` 计数
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    let lines = [
        json!({"type":"system","subtype":"task_started","task_id":"fg","is_backgrounded":false}),
        json!({"type":"system","subtype":"task_started","task_id":"a","is_backgrounded":true}),
        json!({"type":"system","subtype":"task_started","task_id":"b","is_backgrounded":true}),
        json!({"type":"system","subtype":"task_updated","task_id":"a","patch":{"status":"running"}}),
        json!({"type":"system","subtype":"task_updated","task_id":"a","patch":{"status":"failed"}}),
        json!({"type":"system","subtype":"task_notification","task_id":"a","status":"failed"}),
        json!({"type":"system","subtype":"task_notification","task_id":"b","status":"completed"}),
    ];
    let outs: Vec<Out> = lines.iter().flat_map(|l| t.feed(l)).collect();
    // 前台任务（`fg`）不发快照；`task_updated { running }` 和重复的终态通知没带新内容，也不重发
    assert_eq!(
        timeline(&outs),
        ["task", "BACKGROUND:1", "task", "BACKGROUND:2", "task", "BACKGROUND:1", "task", "BACKGROUND:0"]
    );
}

fn line(v: Value) -> String {
    v.to_string()
}

fn init() -> String {
    line(json!({"type":"system","subtype":"init","session_id":"s","model":"m"}))
}

fn result(total: f64, input: u64) -> String {
    line(
        json!({"type":"result","subtype":"success","is_error":false,"duration_ms":10,"total_cost_usd":total,"usage":{"input_tokens":input}}),
    )
}

#[test]
fn a_user_message_during_the_clis_own_turn_waits_for_it_and_takes_it_over() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    // CLI 自己开始了一轮
    assert_eq!(timeline(&t.feed_line(&init())), ["TURN_STARTED"]);
    assert!(t.is_busy());
    // 这时到的用户消息暂存（写进去的话，这一轮的 result 会被当成用户 turn 的结束）；第二条照旧是「忙」
    assert!(!t.would_refuse_user_message());
    assert_eq!(t.submit_user_message(json!({"m": 1})), UserTurn::Deferred);
    assert!(t.would_refuse_user_message());
    assert_eq!(t.submit_user_message(json!({"m": 2})), UserTurn::Busy);
    // 这一轮结束：不发 TurnDone，把暂存的消息交出来写进 stdin
    let outs = t.feed_line(&result(0.01, 5));
    assert_eq!(timeline(&outs), ["WRITE"]);
    assert!(matches!(&outs[0], Out::Write(v) if *v == json!({"m": 1})));
    assert!(t.is_busy(), "the user's turn is now running");
    // 用户的 turn：init 不再算 CLI 自己开的；结束时报两轮合起来的用量
    assert!(timeline(&t.feed_line(&init())).is_empty());
    let r = Replay { outs: t.feed_line(&result(0.03, 7)) };
    let dones = r.turn_dones();
    assert_eq!(dones.len(), 1);
    assert_eq!(dones[0].1, TurnDoneReason::Completed);
    let u = dones[0].2.clone().unwrap();
    assert_eq!(u.input_tokens, Some(12));
    assert_eq!(u.duration_ms, 20);
    approx(u.cost_usd, 0.03);
    assert!(!t.is_busy());
    // 之后再来的 init 又是 CLI 自己开的一轮
    assert_eq!(timeline(&t.feed_line(&init())), ["TURN_STARTED"]);
}

#[test]
fn cancelling_while_the_message_waits_drops_it() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.feed_line(&init());
    assert_eq!(t.submit_user_message(json!({"m": 1})), UserTurn::Deferred);
    // 取消发出之后这一轮自己正常结束了：消息还没送出去，不再写，按取消收尾
    t.mark_cancel_requested();
    let r = Replay { outs: t.feed_line(&result(0.01, 5)) };
    assert!(!r.outs.iter().any(|o| matches!(o, Out::Write(_))));
    assert_eq!(r.turn_dones().iter().map(|d| d.1).collect::<Vec<_>>(), [TurnDoneReason::Cancelled]);
    assert!(!t.is_busy());
}

#[test]
fn the_process_dying_during_the_clis_own_turn_is_a_death_mid_turn() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.feed_line(&line(json!({"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"a"}]})));
    t.feed_line(&init());
    assert_eq!(t.submit_user_message(json!({"m": 1})), UserTurn::Deferred);
    let fin = t.finish();
    assert!(fin.died_mid_turn);
    assert!(!fin.out.iter().any(|o| matches!(o, Out::Write(_))), "the waiting message is dropped");
}

// ---------- 后台任务快照（Event::Task） ----------

fn feed(t: &mut Translator, v: Value) -> Replay {
    Replay { outs: t.feed_line(&v.to_string()) }
}

/// 某一类事件（JSON 形式，断言字段方便）。
fn events_of_type(r: &Replay, ty: &str) -> Vec<Value> {
    r.events().into_iter().map(|e| serde_json::to_value(e).unwrap()).filter(|v| v["type"] == ty).collect()
}

fn agent_states(task: &Value) -> Vec<&str> {
    task["agents"].as_array().map(|a| a.iter().map(|x| x["state"].as_str().unwrap()).collect()).unwrap_or_default()
}

#[test]
fn real_recording_workflow() {
    // 真实 CLI 2.1.308 的 `-p` 录制（haiku 5.5，2026-10-26）：模型写了一个两 agent 并行的 workflow 脚本，Workflow 工具先走
    // `can_use_tool` 权限请求，随后在后台跑；workflow 结束后 CLI 自己开一轮把结果告诉用户（没有 `send`）
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let r = Replay { outs: feed_all(&mut t, &fixture("real_workflow.jsonl")) };

    // 权限：普通的工具权限，标题用脚本里的一句话说明
    let perms = r.perms();
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0].0.kind, PermissionKind::Tool);
    assert_eq!(
        perms[0].0.title,
        "Run workflow: Run two trivial agents in parallel: one replies alpha, one replies beta"
    );
    assert_eq!(perms[0].0.detail.tool.as_ref().unwrap().tool, "Workflow");

    let tool = "toolu_01UWvw41XL9q9KrgmSpA6Fc8";
    let ts = events_of_type(&r, "task");
    // started + 5 条 task_progress + task_updated(completed) + task_notification(completed)
    assert_eq!(ts.len(), 8, "{ts:#?}");
    for t in &ts {
        assert_eq!(
            (t["id"].as_str(), t["kind"].as_str(), t["tool_id"].as_str()),
            (Some("whvf25gph"), Some("workflow"), Some(tool))
        );
        assert_eq!(t["title"], "alpha-beta", "标题是脚本里起的名字");
        // `last_tool_name` 在 workflow 里是 agent 的名字，不是工具
        assert!(t.get("last_tool").is_none());
    }
    // 1. task_started：只有名字和说明
    assert_eq!(ts[0]["status"], "running");
    assert_eq!(ts[0]["summary"], "Run two trivial agents in parallel: one replies alpha, one replies beta");
    assert!(ts[0].get("agents").is_none() && ts[0].get("usage").is_none());
    // 2. 第一份 workflow_progress：alpha 已经开始（有 startedAt），beta 还在排队（只有 queuedAt）
    assert_eq!(agent_states(&ts[1]), ["running", "queued"]);
    assert_eq!(ts[1]["phases"], json!([{"index": 1, "title": "Words"}]));
    assert_eq!(
        ts[1]["agents"][0],
        json!({"index": 1, "label": "alpha", "state": "running", "phase_index": 1, "model": "claude-haiku-5-5", "attempt": 1})
    );
    assert_eq!(ts[1]["usage"], json!({"total_tokens": 0, "tool_calls": 0, "duration_ms": 29}));
    // 3. beta 也开始了
    assert_eq!(agent_states(&ts[2]), ["running", "running"]);
    // 4. 这一帧没有 workflow_progress（CLI 对「全是 progress 更新」的批次限频）：agent 列表沿用上一份，用量是新的
    assert_eq!(ts[3]["agents"], ts[2]["agents"]);
    assert_eq!(ts[3]["usage"]["total_tokens"], 13627);
    // 5. alpha 完成（带 token 和耗时）
    assert_eq!(agent_states(&ts[4]), ["done", "running"]);
    assert_eq!(ts[4]["agents"][0]["tokens"], 13627);
    assert_eq!(ts[4]["agents"][0]["duration_ms"], 2453);
    // 6. 都完成了，任务本身还是 running，直到 task_updated
    assert_eq!(agent_states(&ts[5]), ["done", "done"]);
    assert_eq!((ts[5]["status"].as_str(), ts[5]["usage"]["total_tokens"].as_u64()), (Some("running"), Some(27253)));
    // 7. task_updated { status: completed }；8. task_notification 带的最终用量，摘要保留开头那句说明（通知里只有 `Dynamic workflow "…" completed`）
    assert_eq!(ts[6]["status"], "completed");
    assert_eq!(ts[7]["status"], "completed");
    assert_eq!(ts[7]["summary"], ts[0]["summary"]);
    assert_eq!(ts[7]["usage"], json!({"total_tokens": 27253, "tool_calls": 0, "duration_ms": 3715}));

    // Workflow 工具的调用结果只是「已在后台启动」，不算 background 结果（最终结果在后台任务的快照里）
    let launch = r.events().into_iter().find_map(|e| match e {
        Event::ToolResult { id, output, detail, .. } if id == tool => Some((output.clone(), detail.clone())),
        _ => None,
    });
    let (output, detail) = launch.expect("tool_result of the Workflow call");
    assert!(output.as_str().unwrap().starts_with("Workflow launched in background. Task ID: whvf25gph"));
    assert_eq!(detail, None);

    // 后台任务数照旧；CLI 自己的那一轮正常收尾，带着上下文用量
    let tl = timeline(&r.outs);
    assert_eq!(tl.iter().filter(|x| *x == "BACKGROUND:1").count(), 1);
    assert_eq!(tl.iter().filter(|x| *x == "BACKGROUND:0").count(), 1);
    assert_eq!(tl.iter().filter(|x| *x == "TURN_STARTED").count(), 1);
    let dones = r.turn_dones();
    assert_eq!(dones.len(), 2);
    let u0 = dones[0].2.clone().unwrap();
    // context_tokens = 最近一条顶层 assistant 消息的 input(2) + cache_read(30868) + cache_creation(842)
    assert_eq!((u0.context_window, u0.context_tokens), (Some(1_000_000), Some(2 + 30868 + 842)));
    let u1 = dones[1].2.clone().unwrap();
    assert_eq!((u1.context_window, u1.context_tokens), (Some(1_000_000), Some(4 + 31710 + 949)));
}

#[test]
fn real_recording_workflow_that_fails() {
    // 脚本最后 `throw`：agent 都完成了，workflow 本身失败。其中一个 agent 用了 Read 工具
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let r = Replay { outs: feed_all(&mut t, &fixture("real_workflow_failed.jsonl")) };
    let ts = events_of_type(&r, "task");
    assert_eq!(ts.len(), 8, "{ts:#?}");
    assert_eq!(ts[0]["title"], "two-phase");
    assert_eq!(ts[1]["phases"], json!([{"index": 1, "title": "Read"}, {"index": 2, "title": "Check"}]));
    // 第二个阶段的 agent 在第一个阶段结束后才出现
    assert_eq!(agent_states(&ts[1]), ["running"]);
    assert_eq!(agent_states(&ts[4]), ["done", "running"]);
    assert_eq!(agent_states(&ts[5]), ["done", "done"]);
    assert_eq!(
        ts[5]["agents"][0],
        json!({"index": 1, "label": "reader", "state": "done", "phase_index": 1, "model": "claude-haiku-5-5", "tokens": 14502,
               "tool_calls": 1, "duration_ms": 3765, "last_tool": "Read", "attempt": 1})
    );
    assert_eq!(ts[5]["agents"][1]["phase_index"], 2);
    assert_eq!(ts[5]["usage"], json!({"total_tokens": 28133, "tool_calls": 1, "duration_ms": 5254}));
    // task_updated 带着错误信息；task_notification 的摘要是完整的一句话
    assert_eq!((ts[6]["status"].as_str(), ts[7]["status"].as_str()), (Some("failed"), Some("failed")));
    assert!(ts[6]["summary"].as_str().unwrap().starts_with("Error: deliberate failure after hello ok"));
    assert!(ts[7]["summary"].as_str().unwrap().starts_with(
        "Dynamic workflow \"Read a file, check, then fail on purpose\" failed: Error: deliberate failure"
    ));
    // 终态之后任务数归零
    assert!(timeline(&r.outs).iter().any(|x| x == "BACKGROUND:0"));
}

/// `workflow_progress` 里的 agent / 阶段条目。
fn wf_agent(index: u64, extra: Value) -> Value {
    let mut v = json!({"type": "workflow_agent", "index": index, "label": format!("agent-{index}"), "state": "start"});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

fn task_started(id: &str, extra: Value) -> Value {
    let mut v = json!({"type": "system", "subtype": "task_started", "task_id": id, "run_id": "r1", "tool_use_id": "toolu_t", "description": "Review everything", "session_id": "s"});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

fn task_progress(id: &str, extra: Value) -> Value {
    let mut v = json!({"type": "system", "subtype": "task_progress", "task_id": id, "run_id": "r1", "tool_use_id": "toolu_t",
        "description": "Scan: agent-1", "usage": {"total_tokens": 100, "tool_uses": 2, "duration_ms": 10}, "session_id": "s"});
    v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    v
}

#[test]
fn workflow_agents_are_mapped_state_by_state_and_merged_across_frames() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.begin_turn();
    let r = feed(&mut t, task_started("w1", json!({"task_type": "local_workflow", "workflow_name": "review"})));
    assert_eq!(events_of_type(&r, "task").len(), 1);

    // 乱序的条目、没有 workflow_phase 条目的阶段、schema 之外的 state、workflow_log 条目
    let wp = json!([
        wf_agent(3, json!({"state": "progress", "phaseIndex": 2, "phaseTitle": "Verify", "tokens": 900, "toolCalls": 4, "lastToolName": "Grep",
            "lastToolSummary": "foo", "durationMs": 1200, "attempt": 2, "queuedAt": 1, "startedAt": 2})),
        wf_agent(1, json!({"phaseIndex": 1, "queuedAt": 5})),
        {"type": "workflow_phase", "index": 1, "title": "Scan"},
        wf_agent(2, json!({"phaseIndex": 1, "queuedAt": 5, "startedAt": 6, "model": "claude-haiku-5-5"})),
        wf_agent(4, json!({"state": "done", "cached": true, "resultPreview": "x"})),
        wf_agent(5, json!({"state": "error", "skipped": true, "error": "skipped by user"})),
        wf_agent(6, json!({"state": "error", "blocked": true, "error": "[agent-6] blocked: unreviewed"})),
        wf_agent(7, json!({"state": "paused"})),
        {"type": "workflow_log", "message": "hello"},
    ]);
    let r = feed(
        &mut t,
        task_progress(
            "w1",
            json!({"workflow_progress": wp, "last_tool_name": "agent-3", "summary": "Review everything"}),
        ),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!(ts.len(), 1);
    let task = &ts[0];
    // 按 index 升序；未知 state 的条目整条丢掉
    assert_eq!(agent_states(task), ["queued", "running", "running", "done", "skipped", "failed"]);
    assert_eq!(task["phases"], json!([{"index": 1, "title": "Scan"}, {"index": 2, "title": "Verify"}]));
    let a = |i: usize| &task["agents"][i];
    assert_eq!(a(0)["index"], 1);
    assert_eq!(
        a(2),
        &json!({"index": 3, "label": "agent-3", "state": "running", "phase_index": 2, "tokens": 900, "tool_calls": 4,
        "duration_ms": 1200, "last_tool": "Grep", "attempt": 2})
    );
    assert_eq!(a(3)["cached"], true);
    assert_eq!(a(4)["error"], "skipped by user");
    assert_eq!(a(5)["error"], "[agent-6] blocked: unreviewed");
    assert!(task.get("last_tool").is_none(), "workflow 的 last_tool_name 是 agent 名");
    assert_eq!(task["summary"], "Review everything");

    // 下一帧没带 workflow_progress：agent 列表沿用，用量更新
    let r =
        feed(&mut t, task_progress("w1", json!({"usage": {"total_tokens": 777, "tool_uses": 5, "duration_ms": 99}})));
    let ts = events_of_type(&r, "task");
    assert_eq!(ts[0]["agents"], task["agents"]);
    assert_eq!(ts[0]["usage"], json!({"total_tokens": 777, "tool_calls": 5, "duration_ms": 99}));
    // 同样的帧再来一次：内容没变，不重发
    assert!(events_of_type(
        &feed(&mut t, task_progress("w1", json!({"usage": {"total_tokens": 777, "tool_uses": 5, "duration_ms": 99}}))),
        "task"
    )
    .is_empty());

    // 带 workflow_progress 的帧是全量：没出现的 agent 就是没了
    let r = feed(&mut t, task_progress("w1", json!({"workflow_progress": [wf_agent(1, json!({"state": "done"}))]})));
    assert_eq!(agent_states(&events_of_type(&r, "task")[0]), ["done"]);

    // 失败：task_updated 先到，带 error；之后的进度帧被忽略（终态不再变）
    let r = feed(
        &mut t,
        json!({"type": "system", "subtype": "task_updated", "task_id": "w1", "run_id": "r1",
        "patch": {"status": "failed", "error": "Error: boom", "end_time": 1}}),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!((ts[0]["status"].as_str(), ts[0]["summary"].as_str()), (Some("failed"), Some("Error: boom")));
    assert!(events_of_type(
        &feed(&mut t, task_progress("w1", json!({"usage": {"total_tokens": 1, "tool_uses": 1, "duration_ms": 1}}))),
        "task"
    )
    .is_empty());
    // 通知和已有终态一致：并进用量与摘要；终态不一致的忽略
    let note = |status: &str| {
        json!({"type": "system", "subtype": "task_notification", "task_id": "w1", "run_id": "r1", "tool_use_id": "toolu_t",
        "status": status, "summary": "Dynamic workflow \"x\" failed: Error: boom", "usage": {"total_tokens": 800, "tool_uses": 6, "duration_ms": 100}})
    };
    assert!(events_of_type(&feed(&mut t, note("completed")), "task").is_empty());
    let ts = events_of_type(&feed(&mut t, note("failed")), "task");
    assert_eq!(ts[0]["summary"], "Dynamic workflow \"x\" failed: Error: boom");
    assert_eq!(ts[0]["usage"]["total_tokens"], 800);
}

#[test]
fn task_kinds_titles_and_sources() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    for (id, extra, kind, title) in [
        ("a", json!({"task_type": "local_agent", "description": "run tests"}), "subagent", "run tests"),
        ("b", json!({"task_type": "local_bash", "description": "cargo build"}), "shell", "cargo build"),
        ("c", json!({"task_type": "mcp_task", "description": "export"}), "other", "export"),
        (
            "d",
            json!({"task_type": "local_workflow", "workflow_name": "spec", "description": "Write the spec"}),
            "workflow",
            "spec",
        ),
        ("e", json!({"task_type": "local_workflow", "description": "Unnamed"}), "workflow", "Unnamed"),
        // 没有 task_type 的老 CLI
        ("f", json!({"workflow_name": "legacy"}), "workflow", "legacy"),
        ("g", json!({"subagent_type": "Explore", "description": "look"}), "subagent", "look"),
        ("h", json!({"description": "mystery"}), "other", "mystery"),
        ("i", json!({"description": null}), "other", "Background task"),
    ] {
        let r = feed(&mut t, task_started(id, extra));
        let ts = events_of_type(&r, "task");
        assert_eq!(ts.len(), 1, "{id}");
        assert_eq!((ts[0]["kind"].as_str(), ts[0]["title"].as_str()), (Some(kind), Some(title)), "{id}");
        assert_eq!(ts[0]["tool_id"], "toolu_t");
        assert_eq!(ts[0]["status"], "running");
    }
    // 子 agent 的标题就是描述，不重复放进摘要；workflow 的说明放摘要
    let r = feed(&mut t, task_started("j", json!({"task_type": "local_agent", "description": "same"})));
    assert!(events_of_type(&r, "task")[0].get("summary").is_none());
    let r = feed(
        &mut t,
        task_started(
            "k",
            json!({"task_type": "local_workflow", "workflow_name": "spec", "description": "Write the spec"}),
        ),
    );
    assert_eq!(events_of_type(&r, "task")[0]["summary"], "Write the spec");
}

#[test]
fn subagent_progress_carries_usage_last_tool_and_summary() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    feed(
        &mut t,
        task_started(
            "a1",
            json!({"task_type": "local_agent", "description": "run tests", "subagent_type": "general-purpose", "is_backgrounded": true}),
        ),
    );
    // `description` 是正在做的事；没有 `summary` 时拿它当进度
    let r = feed(
        &mut t,
        task_progress(
            "a1",
            json!({"description": "Editing src/lib.rs", "last_tool_name": "Edit",
        "usage": {"total_tokens": 13036, "tool_uses": 1, "duration_ms": 1679}}),
        ),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!(ts[0]["last_tool"], "Edit");
    assert_eq!(ts[0]["summary"], "Editing src/lib.rs");
    assert_eq!(ts[0]["usage"], json!({"total_tokens": 13036, "tool_calls": 1, "duration_ms": 1679}));
    assert!(ts[0].get("agents").is_none() && ts[0].get("phases").is_none());
    // 模型生成的进度摘要优先
    let r = feed(
        &mut t,
        task_progress(
            "a1",
            json!({"description": "Reading x", "summary": "Fixing the failing test", "last_tool_name": "Read"}),
        ),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!(
        (ts[0]["summary"].as_str(), ts[0]["last_tool"].as_str()),
        (Some("Fixing the failing test"), Some("Read"))
    );
    // 描述回到和标题一样（没有新信息）：摘要不被冲掉
    let r = feed(
        &mut t,
        task_progress(
            "a1",
            json!({"description": "run tests", "last_tool_name": "Read",
        "usage": {"total_tokens": 1, "tool_uses": 2, "duration_ms": 3}}),
        ),
    );
    assert_eq!(events_of_type(&r, "task")[0]["summary"], "Fixing the failing test");
    // 结束：摘要是子 agent 的结果；被停掉算 cancelled
    let r = feed(
        &mut t,
        json!({"type": "system", "subtype": "task_notification", "task_id": "a1", "run_id": "r1", "tool_use_id": "toolu_t",
        "status": "stopped", "summary": "Stopped early", "usage": {"total_tokens": 20, "tool_uses": 3, "duration_ms": 30}}),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!((ts[0]["status"].as_str(), ts[0]["summary"].as_str()), (Some("cancelled"), Some("Stopped early")));
    assert_eq!(ts[0]["usage"]["total_tokens"], 20);
    // killed（task_updated）同样是 cancelled
    feed(&mut t, task_started("a2", json!({"task_type": "local_agent", "description": "d"})));
    let r = feed(
        &mut t,
        json!({"type": "system", "subtype": "task_updated", "task_id": "a2", "patch": {"status": "killed"}}),
    );
    assert_eq!(events_of_type(&r, "task")[0]["status"], "cancelled");
}

#[test]
fn foreground_tasks_only_get_snapshots_once_they_move_to_the_background() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    // 前台子 agent / 被前台工具调用等着的续跑：什么都不发
    for extra in [json!({"is_backgrounded": false}), json!({"awaited": true})] {
        assert!(events_of_type(&feed(&mut t, task_started("fg", extra)), "task").is_empty());
        assert!(events_of_type(&feed(&mut t, task_progress("fg", json!({}))), "task").is_empty());
        // 没转过后台就结束：从头到尾没有快照
        let r = feed(
            &mut t,
            json!({"type": "system", "subtype": "task_notification", "task_id": "fg", "status": "completed", "summary": "x"}),
        );
        assert!(events_of_type(&r, "task").is_empty());
    }
    // 转到后台：`task_updated { patch.is_backgrounded: true }` 起带着已知的进度发快照
    feed(&mut t, task_started("mv", json!({"task_type": "local_agent", "is_backgrounded": false})));
    feed(&mut t, task_progress("mv", json!({"last_tool_name": "Edit", "description": "Editing"})));
    let r = feed(
        &mut t,
        json!({"type": "system", "subtype": "task_updated", "task_id": "mv", "patch": {"is_backgrounded": true}}),
    );
    let ts = events_of_type(&r, "task");
    assert_eq!(ts.len(), 1);
    assert_eq!(
        (ts[0]["status"].as_str(), ts[0]["last_tool"].as_str(), ts[0]["usage"]["total_tokens"].as_u64()),
        (Some("running"), Some("Edit"), Some(100))
    );
    let r = feed(
        &mut t,
        json!({"type": "system", "subtype": "task_updated", "task_id": "mv", "patch": {"status": "completed"}}),
    );
    assert_eq!(events_of_type(&r, "task")[0]["status"], "completed");

    // 杂务（skip_transcript / ambient）不进界面，它们后面的帧也被忽略
    for extra in [json!({"skip_transcript": true}), json!({"ambient": true})] {
        assert!(events_of_type(&feed(&mut t, task_started("amb", extra)), "task").is_empty());
        assert!(events_of_type(&feed(&mut t, task_progress("amb", json!({}))), "task").is_empty());
    }
    // 没见过 task_started 的任务：帧都忽略，不凭空造快照
    assert!(events_of_type(&feed(&mut t, task_progress("ghost", json!({}))), "task").is_empty());
    assert!(events_of_type(
        &feed(
            &mut t,
            json!({"type": "system", "subtype": "task_updated", "task_id": "ghost", "patch": {"status": "failed"}})
        ),
        "task"
    )
    .is_empty());
    assert!(events_of_type(
        &feed(
            &mut t,
            json!({"type": "system", "subtype": "task_notification", "task_id": "ghost", "status": "failed"})
        ),
        "task"
    )
    .is_empty());
}

#[test]
fn a_resumed_task_starts_over_and_late_frames_of_the_previous_run_are_ignored() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    let with_run = |mut v: Value, run: &str| {
        v["run_id"] = json!(run);
        v
    };
    feed(&mut t, with_run(task_started("a", json!({"task_type": "local_agent"})), "run-2"));
    // 上一次运行（run-1）迟到的帧
    assert!(events_of_type(&feed(&mut t, with_run(task_progress("a", json!({})), "run-1")), "task").is_empty());
    let late_done = with_run(
        json!({"type": "system", "subtype": "task_notification", "task_id": "a", "status": "completed", "summary": "old"}),
        "run-1",
    );
    assert!(events_of_type(&feed(&mut t, late_done), "task").is_empty());
    assert_eq!(events_of_type(&feed(&mut t, with_run(task_progress("a", json!({})), "run-2")), "task").len(), 1);
    // 结束之后续跑（同一个 task_id，更大的 run_id）：重新开始，状态回到 running
    feed(
        &mut t,
        with_run(
            json!({"type": "system", "subtype": "task_updated", "task_id": "a", "patch": {"status": "completed"}}),
            "run-2",
        ),
    );
    let r = feed(&mut t, with_run(task_started("a", json!({"task_type": "local_agent"})), "run-3"));
    assert_eq!(events_of_type(&r, "task")[0]["status"], "running");
    assert_eq!(events_of_type(&feed(&mut t, with_run(task_progress("a", json!({})), "run-3")), "task").len(), 1);
}

#[test]
fn only_the_latest_finished_tasks_are_remembered() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    for i in 0..40 {
        let id = format!("t{i}");
        feed(&mut t, task_started(&id, json!({"task_type": "local_bash"})));
        feed(
            &mut t,
            json!({"type": "system", "subtype": "task_updated", "task_id": id, "patch": {"status": "completed"}}),
        );
    }
    let note = |id: &str| json!({"type": "system", "subtype": "task_notification", "task_id": id, "status": "completed", "summary": "done"});
    // 最近结束的还能收到迟到的通知；很久以前的状态已经丢了
    assert_eq!(events_of_type(&feed(&mut t, note("t39")), "task").len(), 1);
    assert!(events_of_type(&feed(&mut t, note("t0")), "task").is_empty());
    // 进程退出后任务全忘
    let fin = t.finish();
    assert!(fin.out.is_empty());
    assert!(events_of_type(&feed(&mut t, note("t38")), "task").is_empty());
}

// ---------- Activity / Notice ----------

#[test]
fn api_retries_are_a_temporary_activity_that_ends_with_the_next_response() {
    let retry = |extra: Value| {
        let mut v = json!({"type": "system", "subtype": "api_retry", "attempt": 2, "max_retries": 10, "retry_delay_ms": 4000, "error_status": 529, "error": "overloaded", "session_id": "s"});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    // 没有 turn 在跑（后台 agent 在重试）：不发，客户端没有内容事件来清掉它
    assert!(feed(&mut t, retry(json!({}))).events().is_empty());

    t.begin_turn();
    let r = feed(&mut t, retry(json!({})));
    assert_eq!(
        events_of_type(&r, "activity"),
        [
            json!({"type": "activity", "state": "retrying", "attempt": 2, "max_attempts": 10, "delay_ms": 4000, "reason": "529 overloaded"})
        ]
    );
    let mut reason = |extra: Value| {
        events_of_type(&feed(&mut t, retry(extra)), "activity")[0]["reason"].as_str().unwrap().to_string()
    };
    assert_eq!(reason(json!({"error_status": null, "error": "unknown"})), "connection error");
    assert_eq!(reason(json!({"error_status": null, "error": "server_error"})), "server_error");
    assert_eq!(
        reason(json!({"error_status": 429, "error": "rate_limit", "retry_delay_ms": 3_600_000})),
        "429 rate_limit"
    );
    assert_eq!(
        reason(
            json!({"error_status": null, "error": "unknown", "no_response": {"waited_ms": 30000, "retry_wait_ms": 60000}})
        ),
        "no response from the API within 30s"
    );
    // 新的响应开始流式输出 = 重试成功：发一次 none；之后的 message_start 不再发
    let start = json!({"type": "stream_event", "parent_tool_use_id": null, "event": {"type": "message_start", "message": {"id": "m1"}}});
    assert_eq!(
        events_of_type(&feed(&mut t, start.clone()), "activity"),
        [json!({"type": "activity", "state": "none"})]
    );
    assert!(events_of_type(&feed(&mut t, start), "activity").is_empty());
}

#[test]
fn compaction_is_an_activity_followed_by_a_notice() {
    let status = |s: Value| json!({"type": "system", "subtype": "status", "status": s, "session_id": "s"});
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.begin_turn();
    // `requesting` 是每次请求开始时发的，与压缩无关
    assert!(feed(&mut t, status(json!("requesting"))).events().is_empty());
    let r = feed(&mut t, status(json!("compacting")));
    assert_eq!(events_of_type(&r, "activity"), [json!({"type": "activity", "state": "compacting"})]);
    // 自动压缩期间会反复发 compacting（compact_output_chars 在涨）：只发一次
    let mut again = status(json!("compacting"));
    again["compact_output_chars"] = json!(120);
    assert!(feed(&mut t, again).events().is_empty());
    // 回到 null = 结束
    let mut done = status(Value::Null);
    done["compact_result"] = json!("success");
    assert_eq!(events_of_type(&feed(&mut t, done.clone()), "activity"), [json!({"type": "activity", "state": "none"})]);
    // 没在压缩时的 null 不发（它在别的场合也会出现）
    assert!(feed(&mut t, done).events().is_empty());

    let boundary = json!({"type": "system", "subtype": "compact_boundary", "session_id": "s",
        "compact_metadata": {"trigger": "auto", "pre_tokens": 168000, "post_tokens": 12000, "duration_ms": 9000}});
    let r = feed(&mut t, boundary);
    assert_eq!(
        events_of_type(&r, "notice"),
        [
            json!({"type": "notice", "code": "context_compacted", "level": "info", "message": "Context compacted (auto): 168000 → 12000 tokens"})
        ]
    );
    assert!(events_of_type(&r, "activity").is_empty());
    // 只有 compact_boundary 没有 null（通知先到）：压缩中的状态也在这里清掉
    feed(&mut t, status(json!("compacting")));
    let manual = json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": "manual", "pre_tokens": 500}});
    let r = feed(&mut t, manual);
    assert_eq!(events_of_type(&r, "activity"), [json!({"type": "activity", "state": "none"})]);
    assert_eq!(events_of_type(&r, "notice")[0]["message"], "Context compacted (manual): was 500 tokens");
    // 新 turn 开始时压缩状态作废：旧的 null 不会凭空发 none
    feed(&mut t, status(json!("compacting")));
    t.begin_turn();
    assert!(feed(&mut t, status(Value::Null)).events().is_empty());
}

#[test]
fn rate_limit_notices_are_sent_once_per_state() {
    let rl = |status: &str, kind: &str, resets: u64, util: f64| {
        json!({"type": "rate_limit_event", "uuid": "u", "session_id": "s",
            "rate_limit_info": {"status": status, "resetsAt": resets, "rateLimitType": kind, "utilization": util}})
    };
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    assert!(feed(&mut t, rl("allowed", "five_hour", 1_791_721_200, 0.1)).events().is_empty());
    let r = feed(&mut t, rl("allowed_warning", "five_hour", 1_791_721_200, 0.82));
    assert_eq!(
        events_of_type(&r, "notice"),
        [
            json!({"type": "notice", "code": "rate_limit", "level": "warning", "message": "Approaching the usage limit (5-hour, 82% used)",
            "resets_at_ms": 1_791_721_200_000u64})
        ]
    );
    // 用量涨了一点：同一状态同一恢复时间，不重复提示
    assert!(feed(&mut t, rl("allowed_warning", "five_hour", 1_791_721_200, 0.9)).events().is_empty());
    let r = feed(&mut t, rl("rejected", "five_hour", 1_791_721_200, 1.0));
    assert_eq!(events_of_type(&r, "notice")[0]["message"], "Usage limit reached (5-hour)");
    assert!(feed(&mut t, rl("rejected", "five_hour", 1_791_721_200, 1.0)).events().is_empty());
    // 别的限额类型各记各的
    let r = feed(&mut t, rl("rejected", "seven_day", 1_792_000_000, 1.0));
    assert_eq!(events_of_type(&r, "notice")[0]["message"], "Usage limit reached (weekly)");
    // 恢复（allowed）之后再触发，要重新提示；恢复时间变了也算新的
    assert!(feed(&mut t, rl("allowed", "five_hour", 1_791_739_200, 0.1)).events().is_empty());
    assert_eq!(events_of_type(&feed(&mut t, rl("rejected", "five_hour", 1_791_739_200, 1.0)), "notice").len(), 1);
    assert_eq!(events_of_type(&feed(&mut t, rl("rejected", "five_hour", 1_791_757_200, 1.0)), "notice").len(), 1);
    // 没有 utilization / 限额类型也不出错
    let bare = json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed_warning"}});
    assert_eq!(events_of_type(&feed(&mut t, bare), "notice")[0]["message"], "Approaching the usage limit (usage)");
}

#[test]
fn notifications_and_informational_lines_become_agent_notices() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.begin_turn();
    let note = |key: &str, text: &str, priority: &str, color: Value| json!({"type": "system", "subtype": "notification", "key": key, "text": text, "priority": priority, "color": color, "session_id": "s"});
    let notice = |r: &Replay| {
        events_of_type(r, "notice")
            .into_iter()
            .map(|n| {
                (
                    n["code"].as_str().unwrap().to_string(),
                    n["level"].as_str().unwrap().to_string(),
                    n["message"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    let agent = |level: &str, msg: &str| ("agent_notice".to_string(), level.to_string(), msg.to_string());
    assert_eq!(
        notice(&feed(&mut t, note("k1", "Plugin sync failed", "high", json!("warning")))),
        [agent("warning", "Plugin sync failed")]
    );
    assert_eq!(
        notice(&feed(&mut t, note("k2", "Error compacting conversation", "immediate", json!("error")))),
        [agent("warning", "Error compacting conversation")]
    );
    assert_eq!(
        notice(&feed(&mut t, note("k3", "Switched to Sonnet", "medium", Value::Null))),
        [agent("info", "Switched to Sonnet")]
    );
    // 闲置提示（low）不转；同一 turn 里同 key 同文本只发一次，换了文本或换了 turn 再发
    assert!(feed(&mut t, note("tip", "Press esc to interrupt", "low", Value::Null)).events().is_empty());
    assert!(feed(&mut t, note("k1", "Plugin sync failed", "high", json!("warning"))).events().is_empty());
    assert_eq!(notice(&feed(&mut t, note("k1", "Plugin sync failed again", "high", json!("warning")))).len(), 1);
    t.begin_turn();
    assert_eq!(notice(&feed(&mut t, note("k1", "Plugin sync failed", "high", json!("warning")))).len(), 1);

    let info = |level: &str, content: &str| json!({"type": "system", "subtype": "informational", "level": level, "content": content, "session_id": "s"});
    // `info` 级别 CLI 自己只在转录视图里显示，不转
    assert!(feed(&mut t, info("info", "x")).events().is_empty());
    assert_eq!(notice(&feed(&mut t, info("notice", "Using the fallback"))), [agent("info", "Using the fallback")]);
    assert_eq!(notice(&feed(&mut t, info("suggestion", "Try /compact"))), [agent("info", "Try /compact")]);
    assert_eq!(
        notice(&feed(&mut t, info("warning", "Hook blocked the prompt"))),
        [agent("warning", "Hook blocked the prompt")]
    );
    assert!(feed(&mut t, info("warning", "")).events().is_empty());
}

#[test]
fn model_fallbacks_permission_denials_and_hook_failures_are_notices() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    let one = |t: &mut Translator, v: Value| {
        let ns = events_of_type(&feed(t, v), "notice");
        assert_eq!(ns.len(), 1, "{ns:?}");
        ns.into_iter().next().unwrap()
    };
    // 模型回退一族：字段取自 CLI 2.1.308 的 SDK schema
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "model_fallback", "trigger": "overloaded", "original_model": "claude-opus-5-5",
        "fallback_model": "claude-sonnet-5-5", "content": "Switched to Sonnet 5.5 due to high demand"}),
    );
    assert_eq!(
        n,
        json!({"type": "notice", "code": "model_fallback", "level": "warning", "message": "Switched to Sonnet 5.5 due to high demand",
            "model_from": "claude-opus-5-5", "model_to": "claude-sonnet-5-5"})
    );
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "model_refusal_fallback", "trigger": "refusal", "direction": "retry", "scope": "session",
        "original_model": "claude-opus-5-5", "fallback_model": "claude-sonnet-5-5", "request_id": null, "content": "Retrying on Sonnet"}),
    );
    assert_eq!(
        (n["model_from"].as_str(), n["model_to"].as_str(), n["code"].as_str()),
        (Some("claude-opus-5-5"), Some("claude-sonnet-5-5"), Some("model_fallback"))
    );
    // 没有回退目标：只有 model_from
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "model_refusal_no_fallback", "original_model": "claude-opus-5-5", "request_id": null, "content": "Refused"}),
    );
    assert_eq!(
        (n["model_from"].as_str(), n.get("model_to"), n["message"].as_str()),
        (Some("claude-opus-5-5"), None, Some("Refused"))
    );
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "model_consent_fallback", "choice": "switch", "original_model": "claude-opus-5-5",
        "original_model_name": "Opus 5.5", "fallback_model": "claude-sonnet-5-5", "persisted_as_default": false, "content": "Switched to Sonnet"}),
    );
    assert_eq!((n["model_to"].as_str(), n["level"].as_str()), (Some("claude-sonnet-5-5"), Some("warning")));

    // 没有权限提示就被拒绝的工具调用
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "permission_denied", "tool_name": "Bash", "tool_use_id": "toolu_1",
        "decision_reason_type": "mode", "decision_reason": "dontAsk mode denies Bash", "message": "Permission to use Bash has been denied."}),
    );
    assert_eq!(
        n,
        json!({"type": "notice", "code": "permission_denied", "level": "info", "message": "Bash: dontAsk mode denies Bash"})
    );
    let n = one(
        &mut t,
        json!({"type": "system", "subtype": "permission_denied", "tool_name": "Edit", "tool_use_id": "toolu_2", "message": "Denied by rule"}),
    );
    assert_eq!(n["message"], "Edit: Denied by rule");

    // hook：只有 outcome = error 才提示
    let hook = |outcome: &str, extra: Value| {
        let mut v = json!({"type": "system", "subtype": "hook_response", "hook_id": "h1", "hook_name": "format-check", "hook_event": "PreToolUse",
            "output": "", "stdout": "", "stderr": "", "outcome": outcome});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v
    };
    assert!(feed(&mut t, hook("success", json!({}))).events().is_empty());
    assert!(feed(&mut t, hook("cancelled", json!({}))).events().is_empty());
    let n = one(&mut t, hook("error", json!({"exit_code": 2, "stderr": "  lint failed: src/a.rs\n"})));
    assert_eq!(
        n,
        json!({"type": "notice", "code": "hook_failed", "level": "warning", "message": "Hook format-check (PreToolUse) failed with exit code 2: lint failed: src/a.rs"})
    );
    let n = one(&mut t, hook("error", json!({"output": "boom"})));
    assert_eq!(n["message"], "Hook format-check (PreToolUse) failed: boom");
}

#[test]
fn failed_mcp_servers_are_reported_once_per_server() {
    let init = |servers: Value| json!({"type": "system", "subtype": "init", "session_id": "s", "model": "m", "mcp_servers": servers});
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.begin_turn();
    let msgs = |r: &Replay| {
        events_of_type(r, "notice")
            .iter()
            .map(|n| {
                (
                    n["code"].as_str().unwrap().to_string(),
                    n["level"].as_str().unwrap().to_string(),
                    n["message"].as_str().unwrap().to_string(),
                )
            })
            .collect::<Vec<_>>()
    };
    let code = "mcp_server_failed".to_string();
    let r = feed(
        &mut t,
        init(
            json!([{"name": "a", "status": "failed"}, {"name": "b", "status": "connected"}, {"name": "c", "status": "pending"}, {"name": "d", "status": "needs-auth"}]),
        ),
    );
    assert_eq!(msgs(&r), [(code.clone(), "warning".to_string(), "MCP server \"a\" failed to connect".to_string())]);
    // init 每个 turn 都会再发：同一个 server 不重复提示；之前还在连接的 server 后来失败了，再提示
    t.begin_turn();
    let r = feed(&mut t, init(json!([{"name": "a", "status": "failed"}, {"name": "c", "status": "failed"}])));
    assert_eq!(msgs(&r), [(code, "warning".to_string(), "MCP server \"c\" failed to connect".to_string())]);
    assert!(feed(&mut t, init(json!([{"name": "a", "status": "failed"}, {"name": "c", "status": "failed"}])))
        .events()
        .iter()
        .all(|e| !matches!(e, Event::Notice { .. })));
}

#[test]
fn local_command_output_is_a_notice_not_assistant_text() {
    // 真实 CLI 2.1.308：`-p` 下 `/context` 的输出是一条模型为 `<synthetic>`、带 `local_command_source` 的 assistant 消息，
    // 随后 `result { num_turns: 0, local_command: "context" }`；没有模型调用
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let r = Replay { outs: feed_all(&mut t, &fixture("real_local_command.jsonl")) };
    assert!(r.text().is_empty(), "不是模型说的话: {:?}", r.text());
    let ns = events_of_type(&r, "notice");
    assert_eq!(ns.len(), 1);
    assert_eq!((ns[0]["code"].as_str(), ns[0]["level"].as_str()), (Some("command_output"), Some("info")));
    let msg = ns[0]["message"].as_str().unwrap();
    assert!(msg.starts_with("## Context Usage\n\n**Model:** claude-haiku-5-5"), "{msg}");
    assert!(
        msg.ends_with("| example-skill-3 | claude.ai sync | ~80 |"),
        "原文不带 <local-command-stdout> 标签，末尾换行也去掉"
    );
    assert!(!msg.contains("local-command"));
    // turn 正常收尾；没有模型调用，也就没有上下文用量
    let dones = r.turn_dones();
    assert_eq!(dones.len(), 1);
    assert_eq!(dones[0].1, TurnDoneReason::Completed);
    let u = dones[0].2.clone().unwrap();
    assert_eq!((u.context_window, u.context_tokens), (None, None));

    // system/local_command_output（schema 里有）同样处理；stderr 形式是警告
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    let ns = events_of_type(
        &feed(
            &mut t,
            json!({"type": "system", "subtype": "local_command_output", "content": "Usage: 12%\n", "uuid": "u", "session_id": "s"}),
        ),
        "notice",
    );
    assert_eq!(ns, [json!({"type": "notice", "code": "command_output", "level": "info", "message": "Usage: 12%"})]);
    let err = json!({"type": "assistant", "parent_tool_use_id": null, "local_command_source": "<local-command-stderr>boom</local-command-stderr>",
        "message": {"id": "m", "model": "<synthetic>", "role": "assistant", "content": [{"type": "text", "text": "boom"}]}});
    let ns = events_of_type(&feed(&mut t, err), "notice");
    assert_eq!((ns[0]["level"].as_str(), ns[0]["message"].as_str()), (Some("warning"), Some("boom")));
    assert!(feed(&mut t, json!({"type": "system", "subtype": "local_command_output", "content": "  \n"}))
        .events()
        .is_empty());

    // 同样是 `<synthetic>` 的 API 错误提示（没有 local_command_source）仍然是普通文本
    let api_error = json!({"type": "assistant", "parent_tool_use_id": null, "error": "billing_error", "isApiErrorMessage": true,
        "message": {"id": "m2", "model": "<synthetic>", "role": "assistant", "content": [{"type": "text", "text": "Credit balance is too low"}]}});
    let r = feed(&mut t, api_error);
    assert_eq!(r.text(), "Credit balance is too low");
    assert!(events_of_type(&r, "notice").is_empty());
}

// ---------- 用量里的上下文 ----------

#[test]
fn usage_carries_the_context_window_and_the_latest_context_size() {
    let assistant = |id: &str, model: &str, parent: Value, input: u64, read: u64, write: u64| {
        json!({"type": "assistant", "parent_tool_use_id": parent, "session_id": "s", "message": {"id": id, "model": model, "role": "assistant",
            "content": [{"type": "text", "text": "x"}],
            "usage": {"input_tokens": input, "cache_read_input_tokens": read, "cache_creation_input_tokens": write, "output_tokens": 7}}})
    };
    let result = |model_usage: Value| json!({"type": "result", "subtype": "success", "is_error": false, "duration_ms": 1, "total_cost_usd": 0.0, "usage": {"input_tokens": 1}, "modelUsage": model_usage});
    let usage_of = |r: &Replay| r.turn_dones().remove(0).2.unwrap();

    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.feed_line(&json!({"type": "system", "subtype": "init", "session_id": "s", "model": "claude-main"}).to_string());
    // 同一条请求的每个 content block 各带一条 assistant 消息（输入用量相同）；子 agent 的（parent_tool_use_id 非空）不算
    feed(&mut t, assistant("m1", "claude-main", Value::Null, 3, 100, 20));
    feed(&mut t, assistant("m2", "claude-main", Value::Null, 5, 400, 60));
    feed(&mut t, assistant("m2", "claude-main", Value::Null, 5, 400, 60));
    feed(&mut t, assistant("sub", "claude-sub", json!("toolu_agent"), 99_999, 99_999, 99_999));
    // 子 agent 用了别的模型：modelUsage 里不止一项，取主模型的窗口
    let r = feed(
        &mut t,
        result(json!({"claude-sub": {"contextWindow": 200_000}, "claude-main": {"contextWindow": 1_000_000}})),
    );
    let u = usage_of(&r);
    assert_eq!((u.context_window, u.context_tokens), (Some(1_000_000), Some(5 + 400 + 60)));

    // 下一个 turn 重新算：没有模型调用就没有 context_tokens；多个条目又对不上主模型时不猜
    t.begin_turn();
    let r = feed(&mut t, result(json!({"claude-x": {"contextWindow": 1}, "claude-y": {"contextWindow": 2}})));
    let u = usage_of(&r);
    assert_eq!((u.context_window, u.context_tokens), (None, None));
    // 只有一项：就是它
    t.begin_turn();
    feed(&mut t, assistant("m3", "claude-other", Value::Null, 1, 2, 3));
    let u = usage_of(&feed(&mut t, result(json!({"claude-other": {"contextWindow": 123_456}}))));
    assert_eq!((u.context_window, u.context_tokens), (Some(123_456), Some(6)));
    // 缺 usage 的 assistant 消息不把值冲成 0
    t.begin_turn();
    feed(&mut t, assistant("m4", "claude-main", Value::Null, 10, 0, 0));
    feed(
        &mut t,
        json!({"type": "assistant", "parent_tool_use_id": null, "message": {"id": "m5", "model": "claude-main", "content": [{"type": "text", "text": "y"}]}}),
    );
    let u = usage_of(&feed(&mut t, result(json!({}))));
    assert_eq!((u.context_window, u.context_tokens), (None, Some(10)));
}

// ---------- MCP elicitation ----------

#[test]
fn real_recording_elicitation_form() {
    // 真实 CLI 2.1.308（stream-json 宿主默认会收到 elicitation 请求）：MCP server 的表单
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let r = Replay { outs: feed_all(&mut t, &fixture("real_elicitation_form.jsonl")) };
    let perms = r.perms();
    assert_eq!(perms.len(), 2, "MCP 工具调用的权限，然后是 elicitation");
    assert_eq!((perms[0].0.kind, perms[0].0.title.as_str()), (PermissionKind::Tool, "Call example-elicit/ask_details"));
    let (p, n) = perms[1];
    assert_eq!(p.request_id, "54da5fa6-ed53-459d-812c-fbc08a83fbfa");
    assert_eq!((p.kind, p.title.as_str()), (PermissionKind::Question, "example-elicit: Please enter your details"));
    assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["accept", "decline"]);
    assert_eq!(
        p.detail
            .questions
            .iter()
            .map(|q| (q.id.as_str(), q.text.as_str(), q.free_text, q.choices.len()))
            .collect::<Vec<_>>(),
        [
            ("name", "Name — Your first name", true, 0),
            ("age", "Age (integer, 0–150)", true, 0),
            ("subscribe", "Subscribe to updates", false, 2),
            ("color", "Favorite color", false, 3),
        ]
    );
    // 表单里只有 name 在 `required` 里，其余可以留空
    assert_eq!(p.detail.questions.iter().map(|q| q.optional).collect::<Vec<_>>(), [false, true, true, true]);
    // 录制时这样答复，CLI 收下并把 `{action, content}` 原样转给了 MCP server（工具结果回显 `{"action": "accept", "content": {…}}`）
    let mut answers = serde_json::Map::new();
    for (k, v) in [("name", "Ada"), ("age", "36"), ("subscribe", "true"), ("color", "green")] {
        answers.insert(k.into(), json!(v));
    }
    let reply = permissions::elicitation_reply(
        n.elicitation.as_ref().unwrap(),
        &PermissionDecision { option_id: Some("accept".into()), answers: Some(answers), ..Default::default() },
    )
    .unwrap();
    assert_eq!(
        permissions::control_response(&p.request_id, reply.response),
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": "54da5fa6-ed53-459d-812c-fbc08a83fbfa",
            "response": {"action": "accept", "content": {"name": "Ada", "age": 36, "subscribe": true, "color": "green"}}}})
    );
    let echoed = fixture("real_elicitation_form.jsonl").into_iter().find(|l| l.contains("elicitation result")).unwrap();
    assert!(echoed.contains(r#"{\"action\": \"accept\", \"content\": {\"name\": \"Ada\", \"age\": 36, \"subscribe\": true, \"color\": \"green\"}}"#));
    assert!(r.errors().is_empty());
}

#[test]
fn real_recording_elicitation_url() {
    let mut t = Translator::new(CWD, json!({}), None);
    t.begin_turn();
    let r = Replay { outs: feed_all(&mut t, &fixture("real_elicitation_url.jsonl")) };
    let perms = r.perms();
    assert_eq!(perms.len(), 2);
    let (p, n) = perms[1];
    assert_eq!(p.request_id, "17bef9d6-44e2-4cf0-8266-062a691ae80a");
    assert_eq!(p.kind, PermissionKind::Other);
    assert_eq!(p.detail.text.as_deref(), Some("Sign in to Example to continue"));
    assert_eq!(p.detail.url.as_deref(), Some("https://example.com/signin?state=abc123"));
    assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), ["accept", "decline"]);
    // 录制时 `{action: accept}`（不带 content）被 CLI 收下
    let reply = permissions::elicitation_reply(
        n.elicitation.as_ref().unwrap(),
        &PermissionDecision { option_id: Some("accept".into()), ..Default::default() },
    )
    .unwrap();
    assert_eq!(reply.response, json!({"action": "accept"}));
}

#[test]
fn an_elicitation_link_that_is_not_http_is_declined_without_asking() {
    let mut t = Translator::new(CWD, json!({}), Some("s".into()));
    t.begin_turn();
    let req = json!({"type": "control_request", "request_id": "e9", "request": {"subtype": "elicitation", "mcp_server_name": "example-elicit",
        "message": "Sign in", "mode": "url", "url": "javascript:alert(1)", "elicitation_id": "x"}});
    let r = feed(&mut t, req);
    assert!(r.perms().is_empty(), "不交给用户");
    // 直接回 decline，并告诉用户发生了什么
    let reply =
        r.outs.iter().find_map(|o| if let Out::Reply(v) = o { Some(v.clone()) } else { None }).expect("a reply");
    assert_eq!(
        reply,
        json!({"type": "control_response", "response": {"subtype": "success", "request_id": "e9", "response": {"action": "decline"}}})
    );
    let errs = r.errors();
    assert_eq!(errs.len(), 1);
    assert_eq!((errs[0].0.as_str(), errs[0].2), ("elicitation_rejected", true));
    assert!(errs[0].1.contains("example-elicit") && errs[0].1.contains("http"));
}

// ---------- 手写的整条流（形状取自 CLI 2.1.308 二进制里的 schema / 转换代码） ----------

#[test]
fn api_retry_stream_shows_the_retries_and_clears_when_the_response_starts() {
    let r = Replay::run("api_retry.jsonl");
    assert_eq!(r.types(), ["STARTED", "activity", "activity", "activity", "text_delta", "text_done", "turn_done"]);
    assert_eq!(
        events_of_type(&r, "activity"),
        [
            json!({"type": "activity", "state": "retrying", "attempt": 1, "max_attempts": 10, "delay_ms": 520, "reason": "529 overloaded"}),
            json!({"type": "activity", "state": "retrying", "attempt": 2, "max_attempts": 10, "delay_ms": 1040, "reason": "529 overloaded"}),
            json!({"type": "activity", "state": "none"}),
        ]
    );
    let u = r.turn_dones().remove(0).2.unwrap();
    assert_eq!((u.context_window, u.context_tokens), (Some(200_000), Some(2 + 20_000 + 100)));
}

#[test]
fn compaction_stream_is_an_activity_then_a_notice() {
    let r = Replay::run("compaction.jsonl");
    // 第二条 compacting（compact_output_chars 在涨）不重复发
    assert_eq!(r.types(), ["STARTED", "activity", "activity", "notice", "text_delta", "text_done", "turn_done"]);
    assert_eq!(
        events_of_type(&r, "activity"),
        [json!({"type": "activity", "state": "compacting"}), json!({"type": "activity", "state": "none"})]
    );
    assert_eq!(
        events_of_type(&r, "notice"),
        [
            json!({"type": "notice", "code": "context_compacted", "level": "info", "message": "Context compacted (auto): 168000 → 12000 tokens"})
        ]
    );
    // 压缩之后的第一次请求：上下文只剩 12k
    assert_eq!(r.turn_dones().remove(0).2.unwrap().context_tokens, Some(3 + 12_000));
}

#[test]
fn rate_limit_stream_warns_once_per_state_and_the_turn_ends_with_the_429() {
    let r = Replay::run("rate_limit_rejected.jsonl");
    assert_eq!(r.types(), ["STARTED", "notice", "notice", "error", "turn_done"]);
    let ns = events_of_type(&r, "notice");
    assert_eq!(
        ns.iter()
            .map(|n| (n["code"].as_str().unwrap(), n["level"].as_str().unwrap(), n["message"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [
            ("rate_limit", "warning", "Approaching the usage limit (5-hour, 82% used)"),
            ("rate_limit", "warning", "Usage limit reached (5-hour)"),
        ]
    );
    assert!(ns.iter().all(|n| n["resets_at_ms"] == 1_791_721_200_000u64));
    assert_eq!(r.errors()[0].0, "rate_limited");
    assert_eq!(r.turn_dones()[0].1, TurnDoneReason::Error);
}
