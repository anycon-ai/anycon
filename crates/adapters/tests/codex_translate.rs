//! 把 `tests/fixtures/codex/*.jsonl` 里的 app-server 流量直接喂给 `Translator`（不起进程）。
//!
//! `real_*.jsonl` 是用本机真实的 `codex app-server` 录下来的：协议层是真的，模型输出要么是脚本（0.157.1 + 本地的假模型服务），
//! 要么是真模型的一次很短的调用（0.164.0：`real_subagent_*`、`real_mcp_*`、`real_context_compaction`、`real_startup_notices`）。
//! 其余是按 schema 手写的（`tools_and_plan`、`command_failed`、`reconnecting`、`unsupported_requests`、`long_text`、`two_turns`、
//! `subagent_collab`、`notices`、`mcp_elicitation_forms`；`unknown_things` 故意带着不认识的东西）。录制入库前清洗过
//! （路径 → `/work/proj`，id → `FIXTURE_*`，模型名 → `fixture-model*`）。

use std::path::PathBuf;

use anycon_adapters::codex::permissions::{answer_problem, build_response, NativePending};
use anycon_adapters::codex::translate::{Out, Translator};
use anycon_agent_api::{AgentEvent, PendingPermission, PermissionDecision};
use anycon_protocol::{
    ActivityState, Event, FileChangeKind, NoticeLevel, PermissionKind, PermissionOptionKind, PlanStatus, TaskKind,
    TaskStatus, ToolCategory, TurnDoneReason,
};
use serde_json::{json, Value};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

fn lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixtures_dir().join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// 回放一个 fixture：返回全部输出。
fn replay(name: &str) -> (Translator, Vec<Out>) {
    let mut t = Translator::new("/work/proj", Some("fixture-model".to_string()));
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines(name) {
        outs.extend(t.feed_line(&l));
    }
    (t, outs)
}

fn events(outs: &[Out]) -> Vec<Event> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Event(AgentEvent::Event(e)) => Some(e.clone()),
            _ => None,
        })
        .collect()
}

fn names(outs: &[Out]) -> Vec<String> {
    events(outs).iter().map(|e| serde_json::to_value(e).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn permissions(outs: &[Out]) -> Vec<(PendingPermission, NativePending)> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Permission { pending, native } => Some((pending.clone(), native.clone())),
            _ => None,
        })
        .collect()
}

fn done(outs: &[Out]) -> (TurnDoneReason, anycon_protocol::Usage) {
    match events(outs).last().expect("events") {
        Event::TurnDone { reason, usage, .. } => (*reason, usage.clone().expect("usage")),
        other => panic!("expected TurnDone, got {other:?}"),
    }
}

#[test]
fn every_fixture_line_is_valid_json_and_classifies() {
    let mut files = 0;
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".jsonl") {
            files += 1;
            for (i, l) in lines(&name).iter().enumerate() {
                let v: Value = serde_json::from_str(l).unwrap_or_else(|e| panic!("{name}:{}: {e}", i + 1));
                assert!(
                    anycon_adapters::codex::rpc::classify_value(&v).is_some(),
                    "{name}:{} is neither a notification nor a request",
                    i + 1
                );
            }
        } else if name.ends_with(".json") {
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
    assert!(files >= 15, "found {files} fixtures");
}

#[test]
fn recordings_carry_no_local_environment() {
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        // 一类一类地拦：用户目录 / 临时目录、回环地址、密钥前缀、安装 id；具体的人名和机器名不写在这里（仓库是公开的）
        for bad in ["/Users/", "-Users-", "/private/", "/var/folders/", "127.0.0.1", "sk-", "installationId"] {
            assert!(!text.contains(bad), "{name} contains {bad:?}");
        }
        // 其它 /home/<名字>：只允许 /home/user
        for (i, _) in text.match_indices("/home/") {
            assert!(
                text[i..].starts_with("/home/user"),
                "{name} has a foreign home dir: {}",
                &text[i..(i + 30).min(text.len())]
            );
        }
        let leaks = local_hostnames(&text);
        assert!(leaks.is_empty(), "{name} has a local hostname: {leaks:?}");
        let leaks = real_emails(&text);
        assert!(leaks.is_empty(), "{name} has an email address: {leaks:?}");
    }
}

/// 本机主机名：`<名字>.local` 后面不再跟字母数字（`~/.local/…` 前面是 `/`，不算）。
fn local_hostnames(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices(".local") {
        let before = text[..i].chars().next_back();
        let after = text[i + ".local".len()..].chars().next();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '-')
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
        {
            let start =
                text[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.')).map_or(0, |p| p + 1);
            out.push(text[start..i + ".local".len()].to_string());
        }
    }
    out
}

/// 邮箱地址：`@` 两侧都紧挨字母数字，且域名里没有 `example` 这一段（补丁里的 `@@` 行不会误报）。
fn real_emails(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (i, _) in text.match_indices('@') {
        if !text[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let domain: String =
            text[i + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
        if !domain.is_empty() && !domain.split('.').any(|label| label == "example") {
            let start =
                text[..i].rfind(|c: char| !(c.is_ascii_alphanumeric() || "._+-".contains(c))).map_or(0, |p| p + 1);
            out.push(format!("{}@{domain}", &text[start..i]));
        }
    }
    out
}

#[test]
fn the_local_environment_guard_catches_what_it_claims_to() {
    assert_eq!(
        local_hostnames(r#"{"host":"build-box.local","dir":"~/.local/share","ok":"a.localhost"}"#),
        vec!["build-box.local"]
    );
    assert!(local_hostnames("see ~/.local/bin and local.example").is_empty());
    assert_eq!(real_emails("mail alice@corp.io, bob.x+y@mail.corp.io"), vec!["alice@corp.io", "bob.x+y@mail.corp.io"]);
    assert!(real_emails("user@example.com and user@example.org").is_empty());
    assert!(real_emails("diff\\n@@ -1 +1 @@\\n@@").is_empty());
}

#[test]
fn text_and_reasoning() {
    let (t, outs) = replay("real_text_reasoning.jsonl");
    assert_eq!(
        names(&outs),
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
    let evs = events(&outs);
    assert_eq!(evs[0], Event::ThinkingDelta { text: "Thinking about a greeting".into() });
    let text: String =
        evs.iter().filter_map(|e| if let Event::TextDelta { text } = e { Some(text.as_str()) } else { None }).collect();
    assert_eq!(text.trim(), "Hello from the fake model");
    let (reason, usage) = done(&outs);
    assert_eq!(reason, TurnDoneReason::Completed);
    // 120 输入里 40 缓存命中；30 输出（含 8 推理）
    assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (Some(80), Some(40), Some(30)));
    assert_eq!((usage.cost_usd, usage.duration_ms), (None, 26));
    assert_eq!(usage.model.as_deref(), Some("fixture-model"));
    assert!(!t.is_busy());
}

#[test]
fn command_with_approval_accepted() {
    let (_, outs) = replay("real_exec_approval.jsonl");
    // 事件顺序：reasoning → tool_call → (权限请求) → 输出 → tool_result → 文本 → turn_done
    assert_eq!(
        names(&outs),
        vec![
            "thinking_delta",
            "thinking_done",
            "tool_call",
            "tool_output_delta",
            "tool_result",
            "text_delta",
            "text_delta",
            "text_delta",
            "text_done",
            "turn_done"
        ]
    );
    let evs = events(&outs);
    match &evs[2] {
        Event::ToolCall { id, name, category, title, input, detail, .. } => {
            assert_eq!((id.as_str(), name.as_str(), *category), ("call_0_1", "commandExecution", ToolCategory::Shell));
            // 壳 `/bin/zsh -lc '…'` 被拆掉；input 保留原样
            assert_eq!(title, "echo hello > out.txt && cat out.txt");
            assert_eq!(input["command"], "/bin/zsh -lc 'echo hello > out.txt && cat out.txt'");
            assert_eq!(input["commandActions"][0]["type"], "unknown");
            let d = detail.as_ref().unwrap();
            assert_eq!(
                (d.command.as_deref(), d.cwd.as_deref()),
                (Some("echo hello > out.txt && cat out.txt"), Some("/work/proj"))
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(evs[3], Event::ToolOutputDelta { id: "call_0_1".into(), text: "hello\n".into() });
    match &evs[4] {
        Event::ToolResult { id, output, is_error, detail } => {
            assert_eq!((id.as_str(), output, *is_error), ("call_0_1", &json!("hello\n"), false));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
        }
        other => panic!("{other:?}"),
    }
    // 两次 `thread/tokenUsage/updated`（两个模型请求）：用量是 `last` 之和，不是线程累计
    let (reason, usage) = done(&outs);
    assert_eq!(reason, TurnDoneReason::Completed);
    assert_eq!((usage.input_tokens, usage.cache_read_tokens, usage.output_tokens), (Some(160), Some(80), Some(60)));
    // `serverRequest/resolved` 被转给 handle
    assert!(outs.iter().any(|o| matches!(o, Out::Resolved { rpc_id } if *rpc_id == json!(0))));
    // 这个录制里恰好有一条审批请求（`replay` 用 `feed_line`，请求走 `Out::Permission`，见下一个测试）
    assert_eq!(lines("real_exec_approval.jsonl").iter().filter(|l| l.contains("requestApproval")).count(), 1);
}

#[test]
fn command_approval_request_becomes_a_tool_permission_with_the_reason_as_description() {
    // 逐条喂，把「请求」也走一遍 feed_message，拿到 Permission
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_exec_approval.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 1);
    let (p, native) = &perms[0];
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.title, "Run command");
    assert_eq!(native.rpc_id, json!(0));
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("commandExecution", ToolCategory::Shell));
    assert_eq!(tool.cwd.as_deref(), Some("/work/proj"));
    let d = tool.detail.as_ref().unwrap();
    assert_eq!(d.command.as_deref(), Some("echo hello > out.txt && cat out.txt"));
    // web 的审批卡片在命令下面显示 `detail.description`
    assert_eq!(d.description.as_deref(), Some("Do you want to run this command outside the sandbox?"));
    assert_eq!(tool.input["reason"], "Do you want to run this command outside the sandbox?");
    assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["allow", "allow_session", "deny"]);
}

#[test]
fn every_approval_kind_puts_its_reason_in_detail_description() {
    let reason = "Because the sandbox blocks it";
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    // 命令
    let cmd = t.feed_request(
        json!(1),
        "item/commandExecution/requestApproval",
        &json!({"kind": "command", "threadId": "t", "turnId": "u", "itemId": "c", "command": "make", "cwd": "/work/proj", "reason": reason}),
    );
    // 补丁（内容在先前的 item/started 里）
    t.feed_message(&json!({"method": "item/started", "params": {"threadId": "t", "turnId": "u", "item": {"type": "fileChange", "id": "f", "status": "inProgress",
        "changes": [{"path": "/work/proj/a.txt", "kind": {"type": "add"}, "diff": "x\n"}]}}}));
    let file = t.feed_request(
        json!(2),
        "item/fileChange/requestApproval",
        &json!({"threadId": "t", "turnId": "u", "itemId": "f", "reason": reason}),
    );
    // 额外权限
    let perm = t.feed_request(
        json!(3),
        "item/permissions/requestApproval",
        &json!({"threadId": "t", "turnId": "u", "itemId": "p", "cwd": "/work/proj", "reason": reason, "permissions": {"network": {"enabled": true}}}),
    );
    for (what, outs) in [("command", cmd), ("file change", file), ("permissions", perm)] {
        let perms = permissions(&outs);
        assert_eq!(perms.len(), 1, "{what}");
        let tool = perms[0].0.detail.tool.as_ref().unwrap();
        assert_eq!(tool.detail.as_ref().unwrap().description.as_deref(), Some(reason), "{what}");
        assert_eq!(tool.input["reason"], reason, "{what}");
        // 没有 reason 就没有 description
    }
    let none = t.feed_request(
        json!(4),
        "item/commandExecution/requestApproval",
        &json!({"threadId": "t", "turnId": "u", "itemId": "c2", "command": "ls"}),
    );
    assert_eq!(permissions(&none)[0].0.detail.tool.as_ref().unwrap().detail.as_ref().unwrap().description, None);
}

#[test]
fn declined_command() {
    let (_, outs) = replay("real_exec_declined.jsonl");
    let evs = events(&outs);
    let result = evs.iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap();
    match result {
        Event::ToolResult { output, is_error, detail, .. } => {
            assert_eq!((output, *is_error), (&json!("Declined by the user"), true));
            assert_eq!(detail, &None);
        }
        _ => unreachable!(),
    }
    // 被拒绝之后 agent 照常把 turn 走完
    assert_eq!(done(&outs).0, TurnDoneReason::Completed);
}

#[test]
fn cancel_decision_interrupts_the_turn() {
    // 回 `cancel`：命令是 declined，turn 以 interrupted 结束，items 为空
    let (_, outs) = replay("real_exec_cancelled.jsonl");
    let n = names(&outs);
    assert!(n.contains(&"tool_call".to_string()) && n.contains(&"tool_result".to_string()), "{n:?}");
    assert_eq!(done(&outs).0, TurnDoneReason::Cancelled);
}

#[test]
fn patch_with_approval() {
    let (_, outs) = replay("real_patch_approval.jsonl");
    let evs = events(&outs);
    let call = evs.iter().find(|e| matches!(e, Event::ToolCall { .. })).unwrap();
    match call {
        Event::ToolCall { name, category, title, detail, input, .. } => {
            assert_eq!((name.as_str(), *category), ("fileChange", ToolCategory::FileEdit));
            assert_eq!(title, "Create hello.txt");
            let d = detail.as_ref().unwrap();
            assert_eq!(d.path.as_deref(), Some("/work/proj/hello.txt"));
            assert_eq!(d.edit_kind, Some(anycon_protocol::EditKind::Write));
            assert_eq!(input["changes"][0]["diff"], "hi there\nsecond line\n");
        }
        _ => unreachable!(),
    }
    let changed: Vec<_> = evs
        .iter()
        .filter_map(|e| if let Event::FileChanged { path, kind } = e { Some((path.as_str(), *kind)) } else { None })
        .collect();
    assert_eq!(changed, vec![("/work/proj/hello.txt", FileChangeKind::Created)]);
    let diff = evs
        .iter()
        .find_map(|e| match e {
            Event::ToolResult { detail, .. } => detail.as_ref().and_then(|d| d.unified_diff.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(diff, "--- /dev/null\n+++ b/hello.txt\n@@ -0,0 +1,2 @@\n+hi there\n+second line\n");
    // 审批请求里没有补丁内容，要靠前面的 item/started 补出标题
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_patch_approval.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 1);
    assert_eq!(perms[0].0.title, "Create hello.txt");
    assert_eq!(
        perms[0].0.detail.tool.as_ref().unwrap().detail.as_ref().unwrap().path.as_deref(),
        Some("/work/proj/hello.txt")
    );
}

#[test]
fn add_update_move_delete_patches() {
    let (_, outs) = replay("real_patch_edits.jsonl");
    let evs = events(&outs);
    let titles: Vec<&str> = evs
        .iter()
        .filter_map(|e| if let Event::ToolCall { title, .. } = e { Some(title.as_str()) } else { None })
        .collect();
    assert_eq!(titles, vec!["Edit hello.txt", "Create a.txt", "Move a.txt to b.txt", "Delete hello.txt"]);
    let changed: Vec<(&str, FileChangeKind)> = evs
        .iter()
        .filter_map(|e| if let Event::FileChanged { path, kind } = e { Some((path.as_str(), *kind)) } else { None })
        .collect();
    assert_eq!(
        changed,
        vec![
            ("/work/proj/hello.txt", FileChangeKind::Modified),
            ("/work/proj/a.txt", FileChangeKind::Created),
            ("/work/proj/a.txt", FileChangeKind::Deleted),
            ("/work/proj/b.txt", FileChangeKind::Created),
            ("/work/proj/hello.txt", FileChangeKind::Deleted),
        ]
    );
    let diffs: Vec<String> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ToolResult { detail, .. } => detail.as_ref().and_then(|d| d.unified_diff.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(diffs[0], "--- a/hello.txt\n+++ b/hello.txt\n@@ -1,2 +1,2 @@\n-hi there\n+hello there\n second line\n");
    // 移动：头里是新旧两个名字，尾巴上的 "Moved to:" 不是 diff 的一部分
    assert_eq!(diffs[2], "--- a/a.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n");
    assert_eq!(diffs[3], "--- a/hello.txt\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-hello there\n-second line\n");
}

#[test]
fn errors_become_coded_error_events() {
    let (_, outs) = replay("real_error_usage_limit.jsonl");
    assert_eq!(names(&outs), vec!["error", "turn_done"]);
    match &events(&outs)[0] {
        Event::Error { code, message, recoverable } => {
            assert_eq!((code.as_str(), *recoverable), ("rate_limited", true));
            assert_eq!(message, "Quota exceeded. Check your plan and billing details.");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(done(&outs).0, TurnDoneReason::Error);

    let (_, outs) = replay("real_error_http_401.jsonl");
    match &events(&outs)[0] {
        Event::Error { code, recoverable, .. } => assert_eq!((code.as_str(), *recoverable), ("auth_failed", false)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn permission_profile_and_user_input_requests() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_permissions_request.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 1);
    let (p, _) = &perms[0];
    assert_eq!(p.kind, PermissionKind::Tool);
    assert_eq!(p.title, "Grant network access; read /usr/share; write /work/proj/sub");
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("requestPermissions", ToolCategory::Other));
    assert_eq!(tool.detail.as_ref().unwrap().description.as_deref(), Some("Need network access to fetch a package"));
    assert_eq!(tool.input["permissions"]["fileSystem"]["entries"][1]["access"], "write");

    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_user_input_request.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 1);
    let (p, _) = &perms[0];
    assert_eq!(p.kind, PermissionKind::Question);
    assert_eq!(p.title, "Agent has a question");
    let q = &p.detail.questions[0];
    assert_eq!((q.id.as_str(), q.text.as_str()), ("which_db", "Database: Which database should I use?"));
    assert_eq!(q.choices[0].label, "SQLite (Recommended) — Zero setup");
}

#[test]
fn plan_search_mcp_and_sub_agent() {
    let (_, outs) = replay("tools_and_plan.jsonl");
    let evs = events(&outs);
    let plan =
        evs.iter().find_map(|e| if let Event::Plan { entries } = e { Some(entries.clone()) } else { None }).unwrap();
    assert_eq!(
        plan.iter().map(|e| (e.content.as_str(), e.status)).collect::<Vec<_>>(),
        vec![
            ("Read the failing test", PlanStatus::Completed),
            ("Fix the parser", PlanStatus::InProgress),
            ("Run the suite", PlanStatus::Pending)
        ]
    );
    let calls: Vec<(String, ToolCategory, String)> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { name, category, title, .. } => Some((name.clone(), *category, title.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        vec![
            ("webSearch".to_string(), ToolCategory::Search, "Search: rust serde untagged enum".to_string()),
            ("mcp__docs__lookup".to_string(), ToolCategory::Mcp, "docs/lookup".to_string()),
            (
                "spawnAgent".to_string(),
                ToolCategory::Subagent,
                "Survey the test directory and report the slowest tests".to_string()
            ),
        ]
    );
    let results: Vec<(Value, bool)> = evs
        .iter()
        .filter_map(|e| {
            if let Event::ToolResult { output, is_error, .. } = e {
                Some((output.clone(), *is_error))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(results[1], (json!("Parser::parse(&str) -> Result<Ast>"), false));
    assert!(!results[2].1);
    assert_eq!(done(&outs).0, TurnDoneReason::Completed);
}

#[test]
fn failing_command() {
    let (_, outs) = replay("command_failed.jsonl");
    let evs = events(&outs);
    match evs.iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { output, is_error, detail, .. } => {
            assert!(*is_error);
            assert!(output.as_str().unwrap().ends_with("FAILED\n"));
            assert_eq!(detail.as_ref().unwrap().exit_code, Some(101));
        }
        _ => unreachable!(),
    }
    let deltas = evs.iter().filter(|e| matches!(e, Event::ToolOutputDelta { .. })).count();
    assert_eq!(deltas, 2);
}

#[test]
fn reconnect_notices_are_recoverable_and_do_not_end_the_turn() {
    let (_, outs) = replay("reconnecting.jsonl");
    let errs: Vec<Event> = events(&outs).into_iter().filter(|e| matches!(e, Event::Error { .. })).collect();
    assert_eq!(errs.len(), 2);
    match &errs[0] {
        Event::Error { code, message, recoverable } => {
            assert_eq!((code.as_str(), *recoverable), ("agent_reconnecting", true));
            assert!(message.starts_with("Reconnecting... 1/5") && message.contains("tls handshake eof"), "{message}");
        }
        _ => unreachable!(),
    }
    assert_eq!(done(&outs).0, TurnDoneReason::Completed);
}

#[test]
fn unsupported_requests_are_each_answered() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("unsupported_requests.jsonl") {
        outs.extend(t.feed_line(&l));
    }
    let replies: Vec<(Value, Result<Value, i64>)> = outs
        .iter()
        .filter_map(|o| match o {
            Out::Reply { id, result } => Some((id.clone(), result.clone().map_err(|e| e.code))),
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0].0, json!(1));
    assert_eq!(replies[0].1.as_ref().unwrap()["success"], false);
    assert_eq!(replies[1], (json!(2), Ok(json!({"decision": "abort"}))));
    assert_eq!((replies[2].0.clone(), replies[2].1.clone()), (json!(3), Err(-32603)));
    // 每个都给用户留了一条可见的说明，没有一个变成权限请求
    assert!(permissions(&outs).is_empty());
    let msgs: Vec<String> = events(&outs)
        .into_iter()
        .filter_map(|e| {
            if let Event::Error { code, message, .. } = e {
                assert_eq!(code, "unsupported_request");
                Some(message)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(msgs.len(), 3);
    assert!(
        msgs[0].contains("dynamic tool") && msgs[1].contains("legacy") && msgs[2].contains("codex login"),
        "{msgs:?}"
    );
    assert_eq!(done(&outs).0, TurnDoneReason::Completed);
}

#[test]
fn unknown_notifications_and_item_types_are_ignored() {
    let (_, outs) = replay("unknown_things.jsonl");
    assert_eq!(names(&outs), vec!["text_delta", "text_delta", "text_done", "turn_done"]);
}

#[test]
fn two_turns_in_one_stream() {
    let mut t = Translator::new("/work/proj", Some("m".into()));
    let mut all = Vec::new();
    // 第二个 turn 的开头也是 turn/started，所以按 turn/completed 切开
    let mut chunk: Vec<String> = Vec::new();
    let mut chunks: Vec<Vec<String>> = Vec::new();
    for l in lines("two_turns.jsonl") {
        let end = l.contains("\"method\":\"turn/completed\"");
        chunk.push(l);
        if end {
            chunks.push(std::mem::take(&mut chunk));
        }
    }
    assert_eq!(chunks.len(), 2);
    for c in chunks {
        t.begin_turn();
        let mut outs = Vec::new();
        for l in c {
            outs.extend(t.feed_line(&l));
        }
        all.push(outs);
    }
    let (_, u1) = done(&all[0]);
    let (_, u2) = done(&all[1]);
    // 每个 turn 只算自己的：100-20 / 220-120
    assert_eq!((u1.input_tokens, u1.cache_read_tokens), (Some(80), Some(20)));
    assert_eq!((u2.input_tokens, u2.cache_read_tokens), (Some(100), Some(120)));
    match events(&all[1]).last().unwrap() {
        Event::TurnDone { turn_id, .. } => assert_eq!(*turn_id, 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn interrupted_stream_closes_open_blocks() {
    // 把 long_text 在中途截断，再补一个 interrupted 的 turn/completed（和真实的取消一样：没有 item/completed）
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("long_text.jsonl").into_iter().take(12) {
        outs.extend(t.feed_line(&l));
    }
    t.mark_cancel_requested();
    outs.extend(t.feed_message(&json!({"method": "turn/completed", "params": {"threadId": "FIXTURE_THREAD",
        "turn": {"id": "FIXTURE_TURN", "items": [], "itemsView": "notLoaded", "status": "interrupted", "error": null, "startedAt": 1, "completedAt": 2, "durationMs": 25}}})));
    let n = names(&outs);
    assert_eq!(&n[n.len() - 2..], ["text_done", "turn_done"]);
    assert_eq!(done(&outs).0, TurnDoneReason::Cancelled);
}

#[test]
fn every_recorded_item_and_notification_parses_into_the_typed_subset() {
    use anycon_adapters::codex::wire::{ErrorNotification, ThreadItem, TokenUsageNotification, TurnNotification};
    let mut items = 0;
    let mut kinds = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(fixtures_dir()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if !name.starts_with("real_") || !name.ends_with(".jsonl") {
            continue;
        }
        for l in lines(&name) {
            let v: Value = serde_json::from_str(&l).unwrap();
            let params = &v["params"];
            match v["method"].as_str() {
                Some("item/started" | "item/completed") => {
                    let item = serde_json::from_value::<ThreadItem>(params["item"].clone())
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                    assert!(!matches!(item, ThreadItem::Other), "{name}: unknown item {}", params["item"]["type"]);
                    kinds.insert(params["item"]["type"].as_str().unwrap().to_string());
                    items += 1;
                }
                Some("turn/started" | "turn/completed") => {
                    serde_json::from_value::<TurnNotification>(params.clone())
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                Some("thread/tokenUsage/updated") => {
                    serde_json::from_value::<TokenUsageNotification>(params.clone())
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                Some("error") => {
                    serde_json::from_value::<ErrorNotification>(params.clone())
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                _ => {}
            }
        }
    }
    assert!(items > 40, "{items}");
    for want in ["userMessage", "agentMessage", "reasoning", "commandExecution", "fileChange"] {
        assert!(kinds.contains(want), "no recorded {want} item; have {kinds:?}");
    }
}

#[test]
fn paths_outside_the_working_directory_stay_absolute() {
    // 录制里的补丁改的是 /work/proj/hello.txt。会话的 cwd 是别的地方时，它就是「项目外」的文件：标题、detail.path、diff 头都是绝对路径
    let mut t = Translator::new("/somewhere/else", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_patch_approval.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let evs = events(&outs);
    match evs.iter().find(|e| matches!(e, Event::ToolCall { .. })).unwrap() {
        Event::ToolCall { title, detail, .. } => {
            assert_eq!(title, "Create /work/proj/hello.txt");
            assert_eq!(detail.as_ref().unwrap().path.as_deref(), Some("/work/proj/hello.txt"));
        }
        _ => unreachable!(),
    }
    let perms = permissions(&outs);
    assert_eq!(perms[0].0.title, "Create /work/proj/hello.txt");
    let diff = evs
        .iter()
        .find_map(|e| match e {
            Event::ToolResult { detail, .. } => detail.as_ref().and_then(|d| d.unified_diff.clone()),
            _ => None,
        })
        .unwrap();
    assert!(diff.starts_with("--- /dev/null\n+++ /work/proj/hello.txt\n"), "{diff}");
    match evs.iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
        Event::ToolResult { output, .. } => assert_eq!(output, &json!("Created /work/proj/hello.txt")),
        _ => unreachable!(),
    }
}

#[test]
fn a_multi_file_patch_approval_lists_every_path_with_the_reason_first() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    t.feed_message(&json!({"method": "item/started", "params": {"threadId": "t", "turnId": "u", "item": {"type": "fileChange", "id": "f", "status": "inProgress",
        "changes": [
            {"path": "/work/proj/src/a.rs", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-a\n+b\n"},
            {"path": "/home/me/.ssh/authorized_keys", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-x\n+y\n"}]}}}));
    let outs = t.feed_request(
        json!(1),
        "item/fileChange/requestApproval",
        &json!({"threadId": "t", "turnId": "u", "itemId": "f", "reason": "needs write access outside the project", "grantRoot": "/home/me/.ssh"}),
    );
    let (p, _) = &permissions(&outs)[0];
    assert_eq!(
        p.title,
        "Edit 2 files (src/a.rs, /home/me/.ssh/authorized_keys) and allow writing under /home/me/.ssh for this session"
    );
    let d = p.detail.tool.as_ref().unwrap().detail.as_ref().unwrap();
    assert_eq!(
        d.description.as_deref(),
        Some("needs write access outside the project\n\n- edit src/a.rs\n- edit /home/me/.ssh/authorized_keys\n\nThis also grants write access under /home/me/.ssh for the rest of the session.")
    );
    // 路径也在 input.changes 里（CLI 按它列全部）
    let paths: Vec<&str> = p.detail.tool.as_ref().unwrap().input["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["/work/proj/src/a.rs", "/home/me/.ssh/authorized_keys"]);
}

#[test]
fn a_patch_approval_that_arrives_before_its_item_says_the_contents_are_unknown() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let outs = t.feed_request(
        json!(1),
        "item/fileChange/requestApproval",
        &json!({"threadId": "t", "turnId": "u", "itemId": "never-started", "reason": "extra write access"}),
    );
    let (p, _) = &permissions(&outs)[0];
    assert_eq!(p.title, "Edit files (patch contents unknown)");
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!(tool.input["contentUnknown"], true);
    assert!(tool
        .detail
        .as_ref()
        .unwrap()
        .description
        .as_deref()
        .unwrap()
        .starts_with("extra write access\n\nThe patch contents are unknown"));
}

#[test]
fn a_sub_command_approval_shows_the_sub_command_not_the_whole_script() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let started = lines("real_exec_approval.jsonl")
        .into_iter()
        .find(|l| l.contains("\"method\":\"item/started\"") && l.contains("commandExecution"))
        .unwrap();
    t.feed_line(&started);
    // 同一个 item（call_0_1）的审批回调，带 approvalId 和子命令
    let outs = t.feed_request(
        json!(5),
        "item/commandExecution/requestApproval",
        &json!({"kind": "command", "threadId": "t", "turnId": "u", "itemId": "call_0_1", "approvalId": "b6f1-sub", "command": "rm -f out.txt", "cwd": "/work/proj", "reason": "deletes a file"}),
    );
    let (p, _) = &permissions(&outs)[0];
    assert_eq!(p.title, "Run sub-command");
    let d = p.detail.tool.as_ref().unwrap().detail.as_ref().unwrap();
    assert_eq!(d.command.as_deref(), Some("rm -f out.txt"));
    assert_eq!(d.description.as_deref(), Some("deletes a file"));
    assert_eq!(p.detail.tool.as_ref().unwrap().input["approvalId"], "b6f1-sub");
}

// ---------------------------------------------------------------- 子 agent

fn tasks(outs: &[Out]) -> Vec<Event> {
    events(outs).into_iter().filter(|e| matches!(e, Event::Task { .. })).collect()
}

fn texts(outs: &[Out]) -> Vec<String> {
    let mut all = vec![String::new()];
    for e in events(outs) {
        match e {
            Event::TextDelta { text } => all.last_mut().unwrap().push_str(&text),
            Event::TextDone {} => all.push(String::new()),
            _ => {}
        }
    }
    all.retain(|t| !t.is_empty());
    all
}

fn notices(outs: &[Out]) -> Vec<(String, NoticeLevel, String)> {
    events(outs)
        .into_iter()
        .filter_map(|e| match e {
            Event::Notice { code, level, message, .. } => Some((code, level, message)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_sub_agents_turn_completion_does_not_end_the_main_turn() {
    // 主 thread 是握手时告诉的，或者从第一个 `turn/started` 学的：两种都一样
    for told in [Some("FIXTURE_THREAD"), None] {
        let mut t = Translator::new("/work/proj", None);
        if let Some(m) = told {
            t.set_main_thread(m);
        }
        t.begin_turn();
        let mut outs = Vec::new();
        let mut child_turns_seen = 0;
        for l in lines("real_subagent_spawn.jsonl") {
            let v: Value = serde_json::from_str(&l).unwrap();
            let child_done = v["method"] == "turn/completed" && v["params"]["threadId"] == "FIXTURE_CHILD";
            let child_started = v["method"] == "turn/started" && v["params"]["threadId"] == "FIXTURE_CHILD";
            outs.extend(t.feed_line(&l));
            if child_started || child_done {
                child_turns_seen += 1;
                // 子 agent 的 turn 开始 / 结束都和主 turn 无关
                assert!(t.is_busy(), "{told:?}: the main turn ended at {l}");
                assert!(!names(&outs).contains(&"turn_done".to_string()), "{told:?}");
                assert_eq!(t.codex_turn_id(), Some("FIXTURE_TURN"));
            }
        }
        assert_eq!(child_turns_seen, 2);
        let n = names(&outs);
        assert_eq!(n.iter().filter(|x| *x == "turn_done").count(), 1);
        assert_eq!(n.last().map(String::as_str), Some("turn_done"));
        assert!(!t.is_busy());
        // 用量只算主 thread 的：4 次主 thread 的请求（子 agent 的 23551 输入不在里面）
        let (reason, usage) = done(&outs);
        assert_eq!(reason, TurnDoneReason::Completed);
        assert_eq!((usage.input_tokens, usage.output_tokens), (Some(14732), Some(98)));
    }
}

#[test]
fn a_sub_agents_text_stays_out_of_the_main_stream() {
    let (_, outs) = replay("real_subagent_spawn.jsonl");
    // 子 agent 回复的 `PONG` 不在主流里，只有主 agent 自己的两段话
    assert_eq!(
        texts(&outs),
        vec![
            "I’ll delegate exactly that one-word response, wait for completion, and report it back.".to_string(),
            "The sub-agent replied: PONG".to_string()
        ]
    );
    assert!(!names(&outs).contains(&"thinking_delta".to_string()));
}

#[test]
fn starting_a_sub_agent_is_a_subagent_tool_call_with_a_task_attached() {
    let (_, outs) = replay("real_subagent_spawn.jsonl");
    let evs = events(&outs);
    // 启动：`subAgentActivity(kind = started)` 是模型那次调用，没有 `collabAgentToolCall`
    let call = evs.iter().find(|e| matches!(e, Event::ToolCall { id, .. } if id == "call_1")).unwrap();
    match call {
        Event::ToolCall { name, category, title, parent_id, input, .. } => {
            assert_eq!(
                (name.as_str(), *category, title.as_str()),
                ("spawnAgent", ToolCategory::Subagent, "Start sub-agent pong")
            );
            assert_eq!(parent_id, &None);
            assert_eq!(input["agentThreadId"], "FIXTURE_CHILD");
        }
        _ => unreachable!(),
    }
    let result = evs.iter().find_map(|e| match e {
        Event::ToolResult { id, output, is_error, .. } if id == "call_1" => Some((output.clone(), *is_error)),
        _ => None,
    });
    assert_eq!(result, Some((json!("Started sub-agent pong (fixture-model-sub)"), false)));
    // `task` 紧跟在 `tool_call` 后面，`tool_id` 指向它
    let pos = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).unwrap();
    assert_eq!(
        pos(&|e| matches!(e, Event::Task { .. })),
        pos(&|e| matches!(e, Event::ToolCall { id, .. } if id == "call_1")) + 1
    );
    let snaps = tasks(&outs);
    match &snaps[0] {
        Event::Task { id, kind, tool_id, title, status, summary, .. } => {
            assert_eq!((id.as_str(), *kind, tool_id.as_deref()), ("FIXTURE_CHILD", TaskKind::Subagent, Some("call_1")));
            assert_eq!((title.as_str(), *status, summary), ("pong", TaskStatus::Running, &None));
        }
        _ => unreachable!(),
    }
    // 最后一条：完成，带上它的回复、用量和耗时
    match snaps.last().unwrap() {
        Event::Task { status, summary, usage, last_tool, .. } => {
            assert_eq!(*status, TaskStatus::Completed);
            assert_eq!(summary.as_deref(), Some("PONG"));
            let u = usage.unwrap();
            assert_eq!((u.total_tokens, u.tool_calls, u.duration_ms), (Some(23557), Some(0), Some(5513)));
            assert_eq!(last_tool, &None);
        }
        _ => unreachable!(),
    }
    // 没有变化就不重复发：每条快照都和前一条不同
    for w in snaps.windows(2) {
        assert_ne!(w[0], w[1]);
    }
    // `wait` 不是启动子 agent 的调用：category other，标题是人话
    let wait = evs.iter().find(|e| matches!(e, Event::ToolCall { id, .. } if id == "call_2")).unwrap();
    match wait {
        Event::ToolCall { name, category, title, .. } => {
            assert_eq!((name.as_str(), *category, title.as_str()), ("wait", ToolCategory::Other, "Wait for sub-agents"))
        }
        _ => unreachable!(),
    }
}

#[test]
fn a_sub_agents_tool_calls_point_at_the_call_that_started_it() {
    let (_, outs) = replay("real_subagent_command.jsonl");
    let evs = events(&outs);
    match evs.iter().find(|e| matches!(e, Event::ToolCall { id, .. } if id == "exec_1")).unwrap() {
        Event::ToolCall { name, category, parent_id, detail, .. } => {
            assert_eq!((name.as_str(), *category), ("commandExecution", ToolCategory::Shell));
            assert_eq!(parent_id.as_deref(), Some("call_1"));
            assert_eq!(detail.as_ref().unwrap().command.as_deref(), Some("echo hello-from-child"));
        }
        _ => unreachable!(),
    }
    // 主 agent 自己的调用不带 parent_id
    for e in &evs {
        if let Event::ToolCall { id, parent_id, .. } = e {
            assert_eq!(parent_id.is_some(), id == "exec_1", "{id}");
        }
    }
    let result = evs.iter().find_map(|e| match e {
        Event::ToolResult { id, output, detail, .. } if id == "exec_1" => Some((output.clone(), detail.clone())),
        _ => None,
    });
    let (output, detail) = result.unwrap();
    assert_eq!((output, detail.unwrap().exit_code), (json!("hello-from-child\n"), Some(0)));
    // 子 agent 的话不进主流
    assert_eq!(
        texts(&outs),
        vec![
            "I’m delegating the command exactly as requested and will wait for the child’s result.".to_string(),
            "The output was `hello-from-child`.".to_string()
        ]
    );
    // task：调用数、最近的工具、它的回复、累计 token
    match tasks(&outs).last().unwrap() {
        Event::Task { title, status, usage, last_tool, summary, .. } => {
            assert_eq!((title.as_str(), *status), ("echo_child", TaskStatus::Completed));
            let u = usage.unwrap();
            assert_eq!((u.tool_calls, u.total_tokens), (Some(1), Some(47280)));
            assert_eq!(last_tool.as_deref(), Some("commandExecution"));
            assert_eq!(summary.as_deref(), Some("The command output was `hello-from-child`."));
        }
        _ => unreachable!(),
    }
}

#[test]
fn a_sub_agents_approval_says_who_is_asking_and_is_answered_like_any_other() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_subagent_command.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 1);
    let (p, native) = &perms[0];
    assert_eq!((p.kind, p.title.as_str()), (PermissionKind::Tool, "Run command (sub-agent echo_child)"));
    // 子 thread 里的 item 也能给审批补上命令（item id 全局唯一）
    assert_eq!(
        p.detail.tool.as_ref().unwrap().detail.as_ref().unwrap().command.as_deref(),
        Some("echo hello-from-child")
    );
    let d = PermissionDecision { option_id: Some("allow".into()), ..Default::default() };
    assert_eq!(build_response(native, &d, false).unwrap(), json!({"decision": "accept"}));
}

#[test]
fn collab_agent_calls_register_their_receivers_and_report_readable_results() {
    let (t, outs) = replay("subagent_collab.jsonl");
    let evs = events(&outs);
    let result_of = |want: &str| {
        evs.iter().find_map(|e| match e {
            Event::ToolResult { id, output, is_error, .. } if id == want => Some((output.clone(), *is_error)),
            _ => None,
        })
    };
    // 启动：结果是「名字: 状态」，不是 `agentsStates` 的原始 JSON
    assert_eq!(
        result_of("collab_1"),
        Some((json!("Survey the test directory and report the slowest tests: running"), false))
    );
    // 等待：每个子 agent 一段，带上它们的消息
    assert_eq!(
        result_of("collab_3"),
        Some((
            json!("Survey the test directory and report the slowest tests: completed\nSlowest: parser_bench (4.2s)\n\nCheck that the lockfile is up to date: interrupted"),
            false
        ))
    );
    // 子 agent 里的调用指向各自的启动调用；子 agent 的思考 / 文本不进主流
    let parents: Vec<(String, Option<String>)> = evs
        .iter()
        .filter_map(|e| match e {
            Event::ToolCall { id, parent_id, .. } => Some((id.clone(), parent_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        parents,
        vec![
            ("collab_1".to_string(), None),
            ("collab_2".to_string(), None),
            ("exec_c1".to_string(), Some("collab_1".to_string())),
            ("exec_c2".to_string(), Some("collab_2".to_string())),
            ("collab_3".to_string(), None),
        ]
    );
    assert_eq!(
        texts(&outs),
        vec!["Spawning two helpers.".to_string(), "The slowest test is parser_bench.".to_string()]
    );
    assert!(!names(&outs).contains(&"thinking_delta".to_string()));
    // 被打断的子 agent 没完成的命令补一条出错的结果，task 是 cancelled
    assert_eq!(result_of("exec_c2"), Some((json!("The sub-agent's turn ended before this finished"), true)));
    let last_of = |want: &str| {
        tasks(&outs)
            .into_iter()
            .filter_map(|e| match e {
                Event::Task { id, status, summary, usage, .. } if id == want => Some((status, summary, usage)),
                _ => None,
            })
            .next_back()
            .unwrap()
    };
    let (status, summary, usage) = last_of("FIXTURE_CHILD");
    assert_eq!(
        (status, summary.as_deref(), usage.unwrap().duration_ms),
        (TaskStatus::Completed, Some("Slowest: parser_bench (4.2s)"), Some(4200))
    );
    let (status, _, _) = last_of("FIXTURE_CHILD_2");
    assert_eq!(status, TaskStatus::Cancelled);
    // 两个子 agent 的 turn 都结束了，主 turn 还在：只有最后那一个 `turn_done`
    assert_eq!(names(&outs).iter().filter(|x| *x == "turn_done").count(), 1);
    assert_eq!(done(&outs).0, TurnDoneReason::Completed);
    assert!(!t.is_busy());
}

/// 事件流里的 task 状态和 `Background` 数按先后排成一串（`task:<status>`、`BACKGROUND:<n>`、`turn_done`），看它们的相对顺序。
fn background_timeline(outs: &[Out]) -> Vec<String> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Event(AgentEvent::Background { tasks }) => Some(format!("BACKGROUND:{tasks}")),
            Out::Event(AgentEvent::Event(Event::Task { status, .. })) => Some(format!("task:{status:?}")),
            Out::Event(AgentEvent::Event(Event::TurnDone { .. })) => Some("turn_done".to_string()),
            _ => None,
        })
        .collect()
}

fn background_counts(outs: &[Out]) -> Vec<u32> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Event(AgentEvent::Background { tasks }) => Some(*tasks),
            _ => None,
        })
        .collect()
}

#[test]
fn a_running_sub_agent_counts_as_a_background_task_until_its_turn_ends() {
    // 真实录制：子 agent 启动 -> 1，它的 turn 结束 -> 0，都在主 turn 结束之前
    for name in ["real_subagent_spawn.jsonl", "real_subagent_command.jsonl"] {
        let (_, outs) = replay(name);
        let tl = background_timeline(&outs);
        assert_eq!(background_counts(&outs), [1, 0], "{name}");
        // 每次变化紧跟在引起它的快照后面
        let first = tl.iter().position(|x| x == "BACKGROUND:1").unwrap();
        assert_eq!(tl[first - 1], "task:Running", "{tl:?}");
        let zero = tl.iter().position(|x| x == "BACKGROUND:0").unwrap();
        assert_eq!(tl[zero - 1], "task:Completed", "{tl:?}");
        assert!(zero < tl.iter().position(|x| x == "turn_done").unwrap(), "{tl:?}");
    }
}

#[test]
fn two_sub_agents_are_counted_individually_and_the_count_returns_to_zero() {
    // 手写的流：两个 spawnAgent（都在跑）-> 2，一个完成 -> 1，另一个的 turn 被打断 -> 0
    let (_, outs) = replay("subagent_collab.jsonl");
    assert_eq!(background_counts(&outs), [1, 2, 1, 0], "{:?}", background_timeline(&outs));
}

/// 手动喂几条通知：主 thread `main`，`begin_turn` 之后主 turn `t1` 在跑。
struct Scenario {
    t: Translator,
    outs: Vec<Out>,
}

impl Scenario {
    fn new() -> Self {
        let mut t = Translator::new("/work/proj", None);
        t.set_main_thread("main");
        t.begin_turn();
        let mut s = Self { t, outs: Vec::new() };
        s.note("turn/started", json!({"threadId": "main", "turn": {"id": "t1", "status": "inProgress", "items": []}}));
        s
    }

    fn note(&mut self, method: &str, params: Value) -> &mut Self {
        let outs = self.t.feed_message(&json!({"method": method, "params": params}));
        self.outs.extend(outs);
        self
    }

    /// 主 thread 里启动一个子 agent（`subAgentActivity` started）。
    fn spawn(&mut self, call: &str, child: &str) -> &mut Self {
        let item = json!({"type": "subAgentActivity", "id": call, "kind": "started", "agentThreadId": child, "agentPath": format!("/root/{child}"), "model": "m"});
        self.note("item/started", json!({"threadId": "main", "turnId": "t1", "item": item.clone()}));
        self.note("item/completed", json!({"threadId": "main", "turnId": "t1", "item": item}))
    }

    fn child_turn(&mut self, child: &str, status: &str) -> &mut Self {
        self.note(
            "turn/completed",
            json!({"threadId": child, "turn": {"id": format!("{child}-t"), "status": status, "items": []}}),
        )
    }

    fn main_done(&mut self, status: &str) -> &mut Self {
        self.note("turn/completed", json!({"threadId": "main", "turn": {"id": "t1", "status": status, "items": []}}))
    }

    fn counts(&self) -> Vec<u32> {
        background_counts(&self.outs)
    }
}

#[test]
fn every_way_a_sub_agent_can_end_brings_the_count_back_to_zero() {
    for status in ["completed", "failed", "interrupted"] {
        let mut s = Scenario::new();
        s.spawn("c1", "kid").child_turn("kid", status);
        assert_eq!(s.counts(), [1, 0], "child turn {status}");
    }
    // 启动它的 subAgentActivity 之后的状态标记
    for kind in ["completed", "interrupted"] {
        let mut s = Scenario::new();
        s.spawn("c1", "kid");
        s.note("item/started", json!({"threadId": "main", "turnId": "t1", "item": {"type": "subAgentActivity", "id": "x", "kind": kind, "agentThreadId": "kid", "agentPath": "/root/kid"}}));
        assert_eq!(s.counts(), [1, 0], "subAgentActivity {kind}");
    }
    // 子 thread 被关掉
    let mut s = Scenario::new();
    s.spawn("c1", "kid").note("thread/closed", json!({"threadId": "kid"}));
    assert_eq!(s.counts(), [1, 0], "thread/closed");
    // collabAgentToolCall 的 agentsStates 说它结束了（或出错、不存在了）
    for state in ["completed", "errored", "interrupted", "shutdown", "notFound"] {
        let mut s = Scenario::new();
        s.spawn("c1", "kid");
        let item = json!({"type": "collabAgentToolCall", "id": "w", "tool": "wait", "status": "completed", "senderThreadId": "main",
            "receiverThreadIds": [], "agentsStates": {"kid": {"status": state, "message": null}}});
        s.note("item/completed", json!({"threadId": "main", "turnId": "t1", "item": item}));
        assert_eq!(s.counts(), [1, 0], "agentsStates {state}");
    }
}

#[test]
fn a_sub_agent_that_outlives_the_main_turn_keeps_the_count_up_until_it_ends() {
    let mut s = Scenario::new();
    s.spawn("c1", "kid").main_done("completed");
    // 主 turn 结束了，子 agent 还在跑：仍然是 1（daemon 据此不回收进程）
    assert_eq!(s.counts(), [1]);
    assert!(!s.t.is_busy());
    // 之后它自己结束
    s.child_turn("kid", "completed");
    assert_eq!(s.counts(), [1, 0]);
    // 主 turn 失败也一样：子 agent 的状态只由它自己的通知决定
    let mut s = Scenario::new();
    s.spawn("c1", "kid").main_done("failed");
    assert_eq!(s.counts(), [1]);
}

#[test]
fn cancelling_the_main_turn_stops_counting_the_sub_agents_even_if_codex_sends_no_wrap_up() {
    let mut s = Scenario::new();
    s.spawn("c1", "kid-a").spawn("c2", "kid-b").child_turn("kid-a", "completed");
    assert_eq!(s.counts(), [1, 2, 1]);
    s.t.mark_cancel_requested();
    s.main_done("interrupted");
    // 没有给 kid-b 发任何收尾：它被一并标成 cancelled，数降到 0，而且在 `turn_done` 之后
    assert_eq!(s.counts(), [1, 2, 1, 0]);
    let tl = background_timeline(&s.outs);
    assert_eq!(&tl[tl.len() - 3..], ["task:Cancelled", "turn_done", "BACKGROUND:0"], "{tl:?}");
    // 它如果其实还在跑，之后自己的通知会把状态改回来（并重新计数）
    s.note(
        "turn/started",
        json!({"threadId": "kid-b", "turn": {"id": "kid-b-t2", "status": "inProgress", "items": []}}),
    );
    assert_eq!(s.counts(), [1, 2, 1, 0, 1]);
    s.child_turn("kid-b", "completed");
    assert_eq!(s.counts(), [1, 2, 1, 0, 1, 0]);
}

#[test]
fn a_sub_agent_that_gets_new_input_counts_again_after_it_completed() {
    let mut s = Scenario::new();
    s.spawn("c1", "kid").child_turn("kid", "completed");
    assert_eq!(s.counts(), [1, 0]);
    // 主 agent 给它发了新消息（`interacted`），随后它开始新 turn、结束
    s.note("item/started", json!({"threadId": "main", "turnId": "t1", "item": {"type": "subAgentActivity", "id": "i1", "kind": "interacted", "agentThreadId": "kid", "agentPath": "/root/kid"}}));
    assert_eq!(s.counts(), [1, 0, 1]);
    s.note("turn/started", json!({"threadId": "kid", "turn": {"id": "kid-t2", "status": "inProgress", "items": []}}));
    assert_eq!(s.counts(), [1, 0, 1], "already running: no duplicate report");
    s.child_turn("kid", "completed");
    assert_eq!(s.counts(), [1, 0, 1, 0]);
    // 同一个 task 的快照：completed -> running -> completed
    let statuses: Vec<TaskStatus> = tasks(&s.outs)
        .into_iter()
        .filter_map(|e| match e {
            Event::Task { status, .. } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [TaskStatus::Running, TaskStatus::Completed, TaskStatus::Running, TaskStatus::Completed]);
}

#[test]
fn without_sub_agents_no_background_count_is_ever_reported() {
    for name in ["real_context_compaction.jsonl", "real_mcp_elicitation.jsonl", "notices.jsonl", "two_turns.jsonl"] {
        let (_, outs) = replay(name);
        assert!(background_counts(&outs).is_empty(), "{name}");
    }
}

#[test]
fn threads_that_were_never_announced_as_sub_agents_do_not_show_up() {
    // 别的内部 thread（比如自动审批的审查 thread）也可能把通知发在这条连接上：既不能进主流，也不该冒充子 agent
    let mut t = Translator::new("/work/proj", None);
    t.set_main_thread("FIXTURE_THREAD");
    t.begin_turn();
    let feed =
        |t: &mut Translator, method: &str, params: Value| t.feed_message(&json!({"method": method, "params": params}));
    let mut outs = Vec::new();
    outs.extend(feed(
        &mut t,
        "turn/started",
        json!({"threadId": "FIXTURE_THREAD", "turn": {"id": "T", "status": "inProgress", "items": []}}),
    ));
    outs.extend(feed(
        &mut t,
        "turn/started",
        json!({"threadId": "REVIEWER", "turn": {"id": "R", "status": "inProgress", "items": []}}),
    ));
    outs.extend(feed(&mut t, "item/started", json!({"threadId": "REVIEWER", "turnId": "R", "item": {"type": "commandExecution", "id": "rc", "command": "ls", "commandActions": []}})));
    outs.extend(feed(
        &mut t,
        "item/agentMessage/delta",
        json!({"threadId": "REVIEWER", "turnId": "R", "itemId": "m", "delta": "approve?"}),
    ));
    outs.extend(feed(&mut t, "thread/tokenUsage/updated", json!({"threadId": "REVIEWER", "turnId": "R", "tokenUsage": {"total": {"totalTokens": 9}, "last": {"totalTokens": 9, "inputTokens": 9}, "modelContextWindow": 10}})));
    outs.extend(feed(
        &mut t,
        "turn/completed",
        json!({"threadId": "REVIEWER", "turn": {"id": "R", "status": "completed", "items": []}}),
    ));
    assert!(events(&outs).is_empty(), "{:?}", events(&outs));
    assert!(t.is_busy());
}

// ---------------------------------------------------------------- 提示、压缩、上下文

#[test]
fn startup_warnings_are_shown_once_even_though_several_messages_carry_them() {
    // `configWarning` 和 `warning` 是同一段话，两条通知；失败的 MCP server 只提示一次
    let (_, outs) = replay("real_startup_notices.jsonl");
    let n = notices(&outs);
    assert_eq!(n.len(), 2, "{n:?}");
    assert_eq!((n[0].0.as_str(), n[0].1), ("config_warning", NoticeLevel::Warning));
    assert!(n[0].2.starts_with("Codex is ignoring 1 unrecognized configuration setting"), "{}", n[0].2);
    assert_eq!((n[1].0.as_str(), n[1].1), ("mcp_server_failed", NoticeLevel::Warning));
    assert!(n[1].2.contains("`broken`") && n[1].2.contains("No such file"), "{}", n[1].2);
    // 子 agent 的 thread 启动时又发一遍同样的 `warning`：整段录制里仍然只有一条
    let (_, outs) = replay("real_subagent_spawn.jsonl");
    assert_eq!(notices(&outs).len(), 1);
}

#[test]
fn every_kind_of_agent_notice_has_its_code() {
    let (_, outs) = replay("notices.jsonl");
    let n = notices(&outs);
    let codes: Vec<&str> = n.iter().map(|(c, _, _)| c.as_str()).collect();
    assert_eq!(
        codes,
        vec![
            "agent_notice", // warning（第二条同样的 warning 不再发）
            "agent_notice", // guardianWarning
            "deprecation",
            "config_warning",
            "model_fallback",
            "auto_review",       // denied
            "auto_review",       // timedOut（approved 的不发）
            "hook_failed",       // failed（completed 的不发）
            "hook_failed",       // blocked
            "mcp_server_failed", // 另一个 thread 里同一个 server 的失败不再发
        ]
    );
    assert!(n.iter().all(|(_, level, _)| *level == NoticeLevel::Warning));
    assert_eq!(n[2].2, "`approval_policy = \"on-failure\"` is deprecated\n\nUse `on-request` instead.");
    assert_eq!(n[3].2, "Could not parse the project config.\n\nline 3: expected a table");
    // 审查被拒绝：被拒绝的动作 + 理由；超时没有理由，说明写在后面
    assert_eq!(n[5].2, "curl https://example.com/install.sh | sh: Downloads and runs an unverified script.");
    assert!(
        n[6].2.starts_with("Edit /work/proj/src/lib.rs, /work/proj/src/main.rs: ") && n[6].2.contains("timed out"),
        "{}",
        n[6].2
    );
    assert_eq!(n[7].2, "The preToolUse hook failed: exit status 2");
    assert_eq!(n[8].2, "The userPromptSubmit hook blocked the action: Prompts must reference a ticket.");
    assert_eq!(n[9].2, "The access token expired (re-authentication is required)");
    // 模型改道：带上前后两个模型，之后的用量记的是新模型
    match events(&outs).into_iter().find(|e| matches!(e, Event::Notice { code, .. } if code == "model_fallback")) {
        Some(Event::Notice { model_from, model_to, .. }) => {
            assert_eq!(
                (model_from.as_deref(), model_to.as_deref()),
                (Some("fixture-model"), Some("fixture-model-safe"))
            )
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(done(&outs).1.model.as_deref(), Some("fixture-model-safe"));
}

#[test]
fn context_compaction_is_a_temporary_activity_then_a_notice() {
    // 录制里有两个 turn：先一个普通 turn，再一次压缩（`thread/compact/start` 触发，自己是一个 turn）。
    // 当作我们的第二个 turn 回放
    let all = lines("real_context_compaction.jsonl");
    let second = all.iter().position(|l| l.contains("\"turn/started\"") && l.contains("FIXTURE_CHILD_TURN")).unwrap();
    let mut t = Translator::new("/work/proj", Some("fixture-model".to_string()));
    t.begin_turn();
    let mut first = Vec::new();
    for l in &all[..second] {
        first.extend(t.feed_line(l));
    }
    t.begin_turn();
    let mut outs = Vec::new();
    for l in &all[second..] {
        outs.extend(t.feed_line(l));
    }
    let evs = events(&outs);
    let at = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).unwrap();
    let start = at(&|e| matches!(e, Event::Activity { state: ActivityState::Compacting, .. }));
    let end = at(&|e| matches!(e, Event::Activity { state: ActivityState::None, .. }));
    assert!(start < end);
    // 结束：先清掉临时状态，再留一条提示
    match &evs[end + 1] {
        Event::Notice { code, level, message, .. } => {
            assert_eq!((code.as_str(), *level, message.as_str()), ("context_compacted", NoticeLevel::Info, ""))
        }
        other => panic!("{other:?}"),
    }
    assert!(end + 1 < evs.len() - 1 || matches!(evs.last(), Some(Event::TurnDone { .. })));
    // 窗口大小；上下文占用：第一个 turn 是最近一次请求的输入 18150（已含缓存命中，不再加 3712），
    // 压缩后那条更新的 `inputTokens` 是 0，取 `totalTokens` 里压缩后的估计 5985
    let (_, u1) = done(&first);
    assert_eq!((u1.context_window, u1.context_tokens), (Some(258400), Some(18150)));
    let (reason, u2) = done(&outs);
    assert_eq!(reason, TurnDoneReason::Completed);
    assert_eq!((u2.context_window, u2.context_tokens), (Some(258400), Some(5985)));
}

#[test]
fn context_tokens_are_the_latest_request_not_the_sum() {
    let (_, outs) = replay("real_subagent_command.jsonl");
    let (_, u) = done(&outs);
    // 主 thread 的最后一次请求：输入 18378；子 agent 的 47231 不算
    assert_eq!((u.context_window, u.context_tokens), (Some(258400), Some(18378)));
    // 累计用量仍是各次请求之和（输入扣掉缓存命中）
    assert_eq!(u.input_tokens, Some(15043));
}

// ---------------------------------------------------------------- MCP elicitation

/// 把 fixture 里的服务端请求逐条喂进去，按请求的 id 取出权限请求。
fn elicitations(name: &str) -> Vec<(i64, Vec<Out>)> {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut got = Vec::new();
    for l in lines(name) {
        let v: Value = serde_json::from_str(&l).unwrap();
        let outs = t.feed_message(&v);
        if let Some(id) = v.get("id").and_then(Value::as_i64) {
            got.push((id, outs));
        }
    }
    got
}

fn decide(option: Option<&str>, answers: Value) -> PermissionDecision {
    PermissionDecision { option_id: option.map(str::to_string), answers: answers.as_object().cloned(), message: None }
}

fn the_permission(outs: &[Out]) -> (PendingPermission, NativePending) {
    let mut p = permissions(outs);
    assert_eq!(p.len(), 1, "{outs:?}");
    p.remove(0)
}

#[test]
fn an_mcp_form_becomes_questions_and_the_answers_are_typed_by_the_schema() {
    let mut forms = elicitations("mcp_elicitation_forms.jsonl");
    let (_, outs) = forms.remove(0);
    let (p, native) = the_permission(&outs);
    assert_eq!(p.kind, PermissionKind::Question);
    assert_eq!(p.title, "forms: Tell us about the deployment");
    assert_eq!(
        p.options.iter().map(|o| (o.id.as_str(), o.kind)).collect::<Vec<_>>(),
        vec![("accept", PermissionOptionKind::AllowOnce), ("decline", PermissionOptionKind::RejectOnce)]
    );
    let q: Vec<(&str, bool, bool, Vec<&str>)> = p
        .detail
        .questions
        .iter()
        .map(|q| (q.id.as_str(), q.free_text, q.multi, q.choices.iter().map(|c| c.id.as_str()).collect()))
        .collect();
    assert_eq!(
        q,
        vec![
            ("budget", true, false, vec![]),
            ("dry_run", false, false, vec!["true", "false"]),
            ("environment", false, false, vec!["staging", "prod"]),
            ("note", true, false, vec![]),
            ("regions", false, true, vec!["eu", "us", "ap"]),
            ("replicas", true, false, vec![]),
            ("tags", false, true, vec!["a", "b"]),
        ]
    );
    let text = |id: &str| p.detail.questions.iter().find(|q| q.id == id).unwrap().text.clone();
    // 说明、数值范围跟在后面；必答是默认，不在题干里标，没列进 `required` 的是 `optional`
    assert_eq!(text("environment"), "Environment");
    assert_eq!(text("replicas"), "Replicas\nEnter an integer between 1 and 10.");
    let optional: Vec<(&str, bool)> = p.detail.questions.iter().map(|q| (q.id.as_str(), q.optional)).collect();
    assert_eq!(
        optional,
        vec![
            ("budget", true),
            ("dry_run", true),
            ("environment", false),
            ("note", true),
            ("regions", true),
            ("replicas", false),
            ("tags", true)
        ]
    );
    assert_eq!(text("budget"), "Budget (USD)\nEnter a number, at least 0.");
    assert_eq!(text("note"), "Note\nAnything else?");
    assert_eq!(text("regions"), "Regions\nPick at least one");
    // 单选的候选用 title 做标签、const 做 id
    let env = p.detail.questions.iter().find(|q| q.id == "environment").unwrap();
    assert_eq!(env.choices[1].label, "Production");

    // 接受：按 schema 的类型转换（整数 / 小数 / 布尔 / 多选数组），没答的非必填项省略
    let answers = json!({"environment": "prod", "replicas": "3", "budget": "12.5", "dry_run": "true", "regions": ["eu", "ap"], "note": ""});
    let r = build_response(&native, &decide(Some("accept"), answers.clone()), false).unwrap();
    assert_eq!(
        r,
        json!({"action": "accept", "content": {"environment": "prod", "replicas": 3, "budget": 12.5, "dry_run": true, "regions": ["eu", "ap"]}})
    );
    // 提交问题卡片没带 option_id，只带了回答
    assert_eq!(build_response(&native, &decide(None, answers), false).unwrap()["action"], "accept");
    // 拒绝 / 没回答（超时）/ 会话取消：decline
    let decline = json!({"action": "decline"});
    assert_eq!(build_response(&native, &decide(Some("decline"), json!({"replicas": "3"})), false).unwrap(), decline);
    assert_eq!(build_response(&native, &decide(None, json!({})), false).unwrap(), decline);
    assert_eq!(build_response(&native, &PermissionDecision::default(), false).unwrap(), decline);
    assert_eq!(
        build_response(&native, &decide(Some("accept"), json!({"replicas": "3", "environment": "prod"})), true)
            .unwrap(),
        decline
    );
    // 不认识的选项是协议错误（请求仍然未决）
    assert!(build_response(&native, &decide(Some("allow"), json!({})), false).is_err());
}

#[test]
fn unanswered_optional_properties_are_left_out_and_only_the_required_ones_have_to_be_answered() {
    let (_, outs) = elicitations("mcp_elicitation_forms.jsonl").remove(0);
    let (_, native) = the_permission(&outs);
    // environment / replicas 是 required，其余留空（空串、空数组、没给）：accept，content 里只有答了的
    let answers = json!({"environment": "prod", "replicas": "2", "note": "  ", "regions": [], "dry_run": ""});
    assert_eq!(answer_problem(&native, &decide(Some("accept"), answers.clone()), false), None);
    assert_eq!(
        build_response(&native, &decide(Some("accept"), answers), false).unwrap(),
        json!({"action": "accept", "content": {"environment": "prod", "replicas": 2}})
    );
    // 必填项留空仍然不行
    let d = decide(Some("accept"), json!({"environment": "prod", "replicas": ""}));
    assert!(answer_problem(&native, &d, false).unwrap().contains("\"Replicas\" is required"));
    assert_eq!(build_response(&native, &d, false).unwrap(), json!({"action": "decline"}));
}

#[test]
fn answers_that_do_not_fit_the_schema_decline_the_form_and_say_why() {
    let (_, outs) = elicitations("mcp_elicitation_forms.jsonl").remove(0);
    let (_, native) = the_permission(&outs);
    let decline = json!({"action": "decline"});
    for (answers, why) in [
        (json!({"environment": "prod", "replicas": "three"}), "\"Replicas\" is not an integer"),
        (json!({"environment": "prod", "replicas": "11"}), "\"Replicas\" is out of range"),
        (json!({"environment": "prod", "replicas": "2", "budget": "-1"}), "\"Budget (USD)\" is out of range"),
        (json!({"environment": "dev", "replicas": "2"}), "\"Environment\" is not one of the choices"),
        (json!({"environment": "prod", "replicas": "2", "dry_run": "maybe"}), "\"Dry run\" is not true or false"),
        (
            json!({"environment": "prod", "replicas": "2", "regions": ["eu", "mars"]}),
            "\"Regions\" has a value that is not one of the choices",
        ),
        (json!({"replicas": "2"}), "\"Environment\" is required but was not answered"),
    ] {
        let d = decide(Some("accept"), answers.clone());
        assert_eq!(build_response(&native, &d, false).unwrap(), decline, "{answers}");
        let problem = answer_problem(&native, &d, false).unwrap_or_else(|| panic!("no problem reported for {answers}"));
        assert!(problem.contains(why), "{problem:?} vs {why:?}");
    }
    // 没问题的、拒绝的、取消的都不报
    assert_eq!(
        answer_problem(&native, &decide(Some("accept"), json!({"environment": "prod", "replicas": "2"})), false),
        None
    );
    assert_eq!(answer_problem(&native, &decide(Some("decline"), json!({})), false), None);
    assert_eq!(answer_problem(&native, &decide(Some("accept"), json!({})), true), None);
}

#[test]
fn an_mcp_url_request_carries_the_link_and_only_http_links_are_shown() {
    let mut forms = elicitations("mcp_elicitation_forms.jsonl");
    let (_, outs) = forms.remove(1);
    let (p, native) = the_permission(&outs);
    assert_eq!((p.kind, p.title.as_str()), (PermissionKind::Other, "auth needs you to open a link"));
    assert_eq!(p.detail.text.as_deref(), Some("Sign in to continue"));
    assert_eq!(p.detail.url.as_deref(), Some("https://example.com/oauth/start?code=1"));
    assert_eq!(
        p.options.iter().map(|o| (o.id.as_str(), o.label.as_str())).collect::<Vec<_>>(),
        vec![("accept", "I've completed it"), ("decline", "Decline")]
    );
    assert_eq!(
        build_response(&native, &decide(Some("accept"), json!({})), false).unwrap(),
        json!({"action": "accept"})
    );
    assert_eq!(
        build_response(&native, &decide(Some("decline"), json!({})), false).unwrap(),
        json!({"action": "decline"})
    );
    assert_eq!(build_response(&native, &decide(None, json!({})), false).unwrap(), json!({"action": "decline"}));
    // javascript: / ftp: 链接、不认识的表单模式：不给用户看，回 decline 并留一条可见的可恢复错误
    let refused: Vec<_> = forms.into_iter().filter(|(id, _)| matches!(id, 2..=4)).collect();
    assert_eq!(refused.len(), 3);
    for (_, outs) in refused {
        assert!(permissions(&outs).is_empty());
        assert_eq!(outs[0], Out::Reply { id: outs_id(&outs), result: Ok(json!({"action": "decline"})) });
        match &outs[1] {
            Out::Event(AgentEvent::Event(Event::Error { code, recoverable, message })) => {
                assert_eq!((code.as_str(), *recoverable), ("elicitation_rejected", true));
                assert!(message.contains("declined"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
}

fn outs_id(outs: &[Out]) -> Value {
    match &outs[0] {
        Out::Reply { id, .. } => id.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_mcp_tool_approval_is_a_tool_permission_with_codexs_persistence_options() {
    // 录制：`wipe_cache` 不是只读工具，Codex 用 elicitation 要审批，`_meta` 里给出 `persist: [session, always]`
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    let mut outs = Vec::new();
    for l in lines("real_mcp_elicitation.jsonl") {
        outs.extend(t.feed_message(&serde_json::from_str(&l).unwrap()));
    }
    let perms = permissions(&outs);
    assert_eq!(perms.len(), 3);
    let (p, native) = &perms[2];
    assert_eq!((p.kind, p.title.as_str()), (PermissionKind::Tool, "Run MCP tool demo/wipe_cache"));
    let tool = p.detail.tool.as_ref().unwrap();
    assert_eq!((tool.tool.as_str(), tool.category), ("mcp__demo__wipe_cache", ToolCategory::Mcp));
    assert_eq!(tool.input, json!({"target": "tmp"}));
    let d = tool.detail.as_ref().unwrap();
    assert_eq!((d.server.as_deref(), d.tool.as_deref()), (Some("demo"), Some("wipe_cache")));
    assert_eq!(d.description.as_deref(), Some("Delete the demo cache. Destructive."));
    assert_eq!(
        p.options.iter().map(|o| (o.id.as_str(), o.kind)).collect::<Vec<_>>(),
        vec![
            ("allow", PermissionOptionKind::AllowOnce),
            ("allow_session", PermissionOptionKind::AllowAlways),
            ("allow_always", PermissionOptionKind::AllowAlways),
            ("deny", PermissionOptionKind::RejectOnce)
        ]
    );
    let opt = |o: &str| build_response(native, &decide(Some(o), json!({})), false).unwrap();
    assert_eq!(opt("allow"), json!({"action": "accept"}));
    assert_eq!(opt("allow_session"), json!({"action": "accept", "_meta": {"persist": "session"}}));
    assert_eq!(opt("allow_always"), json!({"action": "accept", "_meta": {"persist": "always"}}));
    assert_eq!(opt("deny"), json!({"action": "decline"}));
    assert_eq!(build_response(native, &PermissionDecision::default(), false).unwrap(), json!({"action": "decline"}));
    assert_eq!(build_response(native, &decide(Some("allow"), json!({})), true).unwrap(), json!({"action": "decline"}));
    assert!(build_response(native, &decide(Some("accept"), json!({})), false).is_err());
    // 表单和 URL 两个请求也在同一个 turn 里
    assert_eq!(perms[0].0.kind, PermissionKind::Question);
    assert_eq!(perms[1].0.detail.url.as_deref(), Some("https://example.com/demo/authorize?state=abc123"));
}

#[test]
fn an_mcp_tool_approval_only_offers_the_persistence_codex_allows() {
    // 请求 5：`persist` 只有 session，调用的 item 没出现过，工具名从提示语里取
    let (_, outs) = elicitations("mcp_elicitation_forms.jsonl").remove(5);
    let (p, native) = the_permission(&outs);
    assert_eq!(p.title, "Run MCP tool docs/publish");
    assert_eq!(p.detail.tool.as_ref().unwrap().tool, "mcp__docs__publish");
    assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["allow", "allow_session", "deny"]);
    assert!(build_response(&native, &decide(Some("allow_always"), json!({})), false).is_err());
    // 没有属性的表单（别的确认）：kind other，只有接受 / 拒绝
    let (_, outs) = elicitations("mcp_elicitation_forms.jsonl").remove(6);
    let (p, native) = the_permission(&outs);
    assert_eq!((p.kind, p.detail.text.as_deref()), (PermissionKind::Other, Some("Continue with the deployment?")));
    assert_eq!(
        build_response(&native, &decide(Some("accept"), json!({})), false).unwrap(),
        json!({"action": "accept"})
    );
    // 多行的说明：标题只放第一行，完整的话并进第一题
    let (_, outs) = elicitations("mcp_elicitation_forms.jsonl").remove(7);
    let (p, _) = the_permission(&outs);
    assert_eq!(p.title, "forms: Line one");
    assert_eq!(p.detail.questions[0].text, "Line one\nLine two\n\nanswer");
}

#[test]
fn an_mcp_approval_in_a_sub_agent_says_which_agent_is_asking() {
    let mut t = Translator::new("/work/proj", None);
    t.begin_turn();
    t.feed_message(&json!({"method": "turn/started", "params": {"threadId": "FIXTURE_THREAD", "turn": {"id": "T", "status": "inProgress", "items": []}}}));
    t.feed_message(&json!({"method": "item/started", "params": {"threadId": "FIXTURE_THREAD", "turnId": "T",
        "item": {"type": "subAgentActivity", "id": "call_1", "kind": "started", "agentThreadId": "FIXTURE_CHILD", "agentPath": "/root/publisher"}}}));
    let outs = t.feed_message(&json!({"method": "mcpServer/elicitation/request", "id": 4, "params": {
        "threadId": "FIXTURE_CHILD", "turnId": "C", "serverName": "docs", "mode": "form",
        "_meta": {"codex_approval_kind": "mcp_tool_call", "persist": ["session"], "tool_params": {}},
        "message": "Allow the docs MCP server to run tool \"publish\"?", "requestedSchema": {"type": "object", "properties": {}}}}));
    let (p, _) = the_permission(&outs);
    assert_eq!(p.title, "Run MCP tool docs/publish (sub-agent publisher)");
}
