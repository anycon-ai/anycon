//! 把 `tests/fixtures/codex/*.jsonl` 里的 app-server 流量直接喂给 `Translator`（不起进程）。
//!
//! `real_*.jsonl` 是用本机真实的 `codex app-server`（0.157.1）加一个本地的假模型服务录下来的：协议层是真的，
//! 模型输出是脚本。其余是按 schema 手写的（`tools_and_plan`、`command_failed`、`reconnecting`、`unsupported_requests`、
//! `long_text`、`two_turns`；`unknown_things` 故意带着不认识的东西）。录制入库前清洗过（路径 → `/work/proj`，id → `FIXTURE_*`）。

use std::path::PathBuf;

use anycon_adapters::codex::permissions::NativePending;
use anycon_adapters::codex::translate::{Out, Translator};
use anycon_agent_api::{AgentEvent, PendingPermission};
use anycon_protocol::{Event, FileChangeKind, PermissionKind, PlanStatus, ToolCategory, TurnDoneReason};
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
        Event::ToolCall { id, name, category, title, input, detail } => {
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
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0], (json!(0), Ok(json!({"action": "decline"}))));
    assert_eq!(replies[1].0, json!(1));
    assert_eq!(replies[1].1.as_ref().unwrap()["success"], false);
    assert_eq!(replies[2], (json!(2), Ok(json!({"decision": "abort"}))));
    assert_eq!((replies[3].0.clone(), replies[3].1.clone()), (json!(3), Err(-32603)));
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
    assert_eq!(msgs.len(), 4);
    assert!(
        msgs[0].contains("MCP")
            && msgs[1].contains("dynamic tool")
            && msgs[2].contains("legacy")
            && msgs[3].contains("codex login"),
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
