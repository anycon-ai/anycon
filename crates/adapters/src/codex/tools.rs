//! Codex 的 thread item → 协议里的 category / 标题 / 渲染字段（docs/spec/daemon.md § 8）。
//! `translate`（tool_call / tool_result）和 `permissions`（权限请求）共用。
//!
//! | item `type` | category | 说明 |
//! |---|---|---|
//! | `commandExecution` | `shell` | 命令是 `/bin/zsh -lc '<cmd>'` 这种壳时，标题和 `detail.command` 显示里面的 `<cmd>`，`input.command` 保留原样 |
//! | `fileChange` | `file_edit` | 每个 change 一个 `file_changed`；`unified_diff` 由 change 的 `diff` 补上文件头 |
//! | `mcpToolCall` | `mcp` | 名字 `mcp__<server>__<tool>` |
//! | `webSearch` | `search` | |
//! | `subAgentActivity`（`kind = started`） | `subagent` | 0.164 里启动子 agent 就是它（没有对应的 `collabAgentToolCall`）；`id` 是模型那次调用的 id，名字 `spawnAgent` |
//! | `collabAgentToolCall` | `spawnAgent` 是 `subagent`，其余（`wait`、`sendInput`、`closeAgent`…）是 `other` | |
//! | `imageView` | `file_read` | |
//! | `dynamicToolCall` / `imageGeneration` | `other` | |

use anycon_protocol::{EditKind, ToolCallDetail, ToolCategory};
use serde_json::{json, Value};

use super::wire::{FileUpdateChange, PatchChangeKind, ThreadItem};

/// 一个工具调用在协议里的样子。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCallParts {
    pub name: String,
    pub category: ToolCategory,
    pub title: String,
    pub input: Value,
    pub detail: Option<ToolCallDetail>,
}

/// 单个工具输出（文本）保留的上限。历史是唯一副本，但一次 `cat` 大文件不该撑爆事件流。
pub const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

pub fn clip_text(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated {} bytes]", &s[..end], s.len() - end)
}

pub fn first_line_clipped(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// `/bin/zsh -lc 'echo hi'` → `echo hi`。认不出壳的形状就原样返回。
pub fn unwrap_shell(command: &str) -> String {
    let Ok(words) = shell_words::split(command) else { return command.to_string() };
    if words.len() != 3 {
        return command.to_string();
    }
    let shell = words[0].rsplit('/').next().unwrap_or("");
    let flag = &words[1];
    let is_shell = matches!(shell, "bash" | "zsh" | "sh" | "dash" | "ksh" | "fish");
    let runs_script = flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c');
    if is_shell && runs_script {
        words[2].clone()
    } else {
        command.to_string()
    }
}

/// 显示用的路径：在工作目录里面的写成相对路径，**cwd 之外的保持绝对路径**（标题、`detail.path`、diff 头都用它）。
/// 以前把 `/etc/hosts` 显示成 `etc/hosts`，读起来像项目里的文件——而需要审批的恰好是写项目外的补丁。
/// 含 `..` 的（`/proj/../x`）也保持原样，免得看起来在项目里。
pub fn rel_path(cwd: &str, path: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    match path.strip_prefix(cwd) {
        Some(rest) if !cwd.is_empty() && rest.starts_with('/') && !rest.split('/').any(|c| c == "..") => {
            rest.trim_start_matches('/').to_string()
        }
        _ => path.to_string(),
    }
}

/// diff 头里的文件名：工作目录里面的是 `a/<相对路径>` / `b/<相对路径>`，cwd 之外的是绝对路径本身（不加前缀）。
fn diff_name(cwd: &str, path: &str, prefix: &str) -> String {
    let rel = rel_path(cwd, path);
    if rel == path {
        path.to_string()
    } else {
        format!("{prefix}/{rel}")
    }
}

fn kind_name(k: &PatchChangeKind) -> &'static str {
    match k {
        PatchChangeKind::Add => "add",
        PatchChangeKind::Delete => "delete",
        PatchChangeKind::Update { .. } => "update",
        PatchChangeKind::Unknown => "unknown",
    }
}

fn move_path(k: &PatchChangeKind) -> Option<&str> {
    match k {
        PatchChangeKind::Update { move_path } => move_path.as_deref().filter(|p| !p.is_empty()),
        _ => None,
    }
}

fn edit_kind(k: &PatchChangeKind) -> EditKind {
    match k {
        PatchChangeKind::Add => EditKind::Write,
        PatchChangeKind::Delete => EditKind::Delete,
        PatchChangeKind::Update { .. } if move_path(k).is_some() => EditKind::Move,
        _ => EditKind::Edit,
    }
}

/// `fileChange` 的 `tool_call.input` / 权限请求的 input：每个 change 的路径、种类、diff。
pub fn file_change_input(changes: &[FileUpdateChange]) -> Value {
    let list: Vec<Value> = changes
        .iter()
        .map(|c| {
            let mut o = json!({ "path": c.path, "kind": kind_name(&c.kind), "diff": c.diff });
            if let Some(m) = move_path(&c.kind) {
                o["move_path"] = json!(m);
            }
            o
        })
        .collect();
    json!({ "changes": list })
}

pub fn file_change_title(cwd: &str, changes: &[FileUpdateChange]) -> String {
    match changes {
        [] => "Edit files".to_string(),
        [c] => {
            let p = rel_path(cwd, &c.path);
            match &c.kind {
                PatchChangeKind::Add => format!("Create {p}"),
                PatchChangeKind::Delete => format!("Delete {p}"),
                k => match move_path(k) {
                    Some(m) => format!("Move {p} to {}", rel_path(cwd, m)),
                    None => format!("Edit {p}"),
                },
            }
        }
        many => {
            // 标题要写清文件数；名字也列几个（CLI 的通知只显示标题）
            let names: Vec<String> = many.iter().take(3).map(|c| rel_path(cwd, &c.path)).collect();
            let more = if many.len() > 3 { ", …" } else { "" };
            first_line_clipped(&format!("Edit {} files ({}{more})", many.len(), names.join(", ")), 120)
        }
    }
}

/// 多文件补丁的文件清单（每行 `- <动作> <路径>`）；单文件没有（路径已经在标题和 `detail.path` 里）。
pub fn file_change_list(cwd: &str, changes: &[FileUpdateChange]) -> Option<String> {
    if changes.len() < 2 {
        return None;
    }
    let lines: Vec<String> = changes
        .iter()
        .map(|c| {
            let p = rel_path(cwd, &c.path);
            match &c.kind {
                PatchChangeKind::Add => format!("- create {p}"),
                PatchChangeKind::Delete => format!("- delete {p}"),
                k => match move_path(k) {
                    Some(m) => format!("- move {p} to {}", rel_path(cwd, m)),
                    None => format!("- edit {p}"),
                },
            }
        })
        .collect();
    Some(lines.join("\n"))
}

pub fn file_change_detail(cwd: &str, changes: &[FileUpdateChange]) -> Option<ToolCallDetail> {
    let first = changes.first()?;
    Some(ToolCallDetail {
        path: Some(first.path.clone()),
        edit_kind: Some(edit_kind(&first.kind)),
        // 多文件补丁：`detail.path` 只放得下第一个，全部路径列在说明里
        description: file_change_list(cwd, changes),
        ..Default::default()
    })
}

/// 一个 change 的 unified diff：补上 `---` / `+++` 头。Codex 给的 `diff` 对新增 / 删除是整个文件内容，对修改是 `@@` 开头的 hunk。
pub fn unified_diff(cwd: &str, c: &FileUpdateChange) -> String {
    match &c.kind {
        PatchChangeKind::Add => {
            let n = c.diff.lines().count();
            let mut s = format!("--- /dev/null\n+++ {}\n", diff_name(cwd, &c.path, "b"));
            if n > 0 {
                s.push_str(&format!("@@ -0,0 +1,{n} @@\n"));
                for l in c.diff.lines() {
                    s.push('+');
                    s.push_str(l);
                    s.push('\n');
                }
            }
            s
        }
        PatchChangeKind::Delete => {
            let n = c.diff.lines().count();
            let mut s = format!("--- {}\n+++ /dev/null\n", diff_name(cwd, &c.path, "a"));
            if n > 0 {
                s.push_str(&format!("@@ -1,{n} +0,0 @@\n"));
                for l in c.diff.lines() {
                    s.push('-');
                    s.push_str(l);
                    s.push('\n');
                }
            }
            s
        }
        k => {
            let new_path = move_path(k).unwrap_or(&c.path);
            // 带 move_path 时 Codex 在 hunk 后面追加 "\n\nMoved to: <path>"，那不是 diff 的一部分
            let body = match c.diff.find("\n\nMoved to: ") {
                Some(i) => &c.diff[..i],
                None => c.diff.as_str(),
            };
            let mut s = format!("--- {}\n+++ {}\n{body}", diff_name(cwd, &c.path, "a"), diff_name(cwd, new_path, "b"));
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s
        }
    }
}

/// 所有 change 的 diff 拼在一起。
pub fn file_change_unified_diff(cwd: &str, changes: &[FileUpdateChange]) -> String {
    clip_text(&changes.iter().map(|c| unified_diff(cwd, c)).collect::<String>())
}

/// MCP 工具结果 → `tool_result.output`：内容全是文本块就拼成字符串，否则整个结果原样。
pub fn mcp_output(result: &Value) -> Value {
    if let Some(blocks) = result.get("content").and_then(Value::as_array) {
        if !blocks.is_empty() && blocks.iter().all(|b| b.get("type").and_then(Value::as_str) == Some("text")) {
            let texts: Vec<&str> = blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect();
            return Value::String(clip_text(&texts.join("\n")));
        }
    }
    result.clone()
}

/// 动态工具的输出：`inputText` 块拼起来。
pub fn dynamic_output(items: &[Value]) -> String {
    let texts: Vec<&str> = items
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("inputText"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect();
    clip_text(&texts.join("\n"))
}

/// 没有 prompt 的 collab 调用的标题。
fn collab_title(tool: &str) -> String {
    match tool {
        "wait" => "Wait for sub-agents",
        "closeAgent" => "Close a sub-agent",
        "sendInput" | "sendMessage" | "followupTask" => "Send a message to a sub-agent",
        "resumeAgent" => "Resume a sub-agent",
        "interruptAgent" => "Interrupt a sub-agent",
        "listAgents" => "List sub-agents",
        "spawnAgent" => "Start a sub-agent",
        other => other,
    }
    .to_string()
}

/// 开始时的工具调用描述。不是工具类的 item 返回 `None`。
pub fn describe_item(item: &ThreadItem, cwd: &str) -> Option<ToolCallParts> {
    match item {
        ThreadItem::CommandExecution { command, cwd: item_cwd, command_actions, source, .. } => {
            let shown = unwrap_shell(command);
            let run_cwd = item_cwd.clone().unwrap_or_else(|| cwd.to_string());
            let mut input = json!({ "command": command, "cwd": run_cwd });
            if !command_actions.is_empty() {
                input["commandActions"] = Value::Array(command_actions.clone());
            }
            if let Some(s) = source.as_deref().filter(|s| !s.is_empty()) {
                input["source"] = json!(s);
            }
            Some(ToolCallParts {
                name: "commandExecution".to_string(),
                category: ToolCategory::Shell,
                title: if shown.trim().is_empty() {
                    "Run command".to_string()
                } else {
                    first_line_clipped(&shown, 120)
                },
                input,
                detail: Some(ToolCallDetail { command: Some(shown), cwd: Some(run_cwd), ..Default::default() }),
            })
        }
        ThreadItem::FileChange { changes, .. } => Some(ToolCallParts {
            name: "fileChange".to_string(),
            category: ToolCategory::FileEdit,
            title: file_change_title(cwd, changes),
            input: file_change_input(changes),
            detail: file_change_detail(cwd, changes),
        }),
        ThreadItem::McpToolCall { server, tool, arguments, .. } => Some(ToolCallParts {
            name: format!("mcp__{server}__{tool}"),
            category: ToolCategory::Mcp,
            title: format!("{server}/{tool}"),
            input: arguments.clone(),
            detail: Some(ToolCallDetail {
                server: Some(server.clone()),
                tool: Some(tool.clone()),
                ..Default::default()
            }),
        }),
        ThreadItem::DynamicToolCall { tool, namespace, arguments, .. } => {
            let name = match namespace.as_deref().filter(|n| !n.is_empty()) {
                Some(ns) => format!("{ns}.{tool}"),
                None => tool.clone(),
            };
            Some(ToolCallParts {
                title: name.clone(),
                name,
                category: ToolCategory::Other,
                input: arguments.clone(),
                detail: None,
            })
        }
        ThreadItem::CollabAgentToolCall { tool, prompt, model, receiver_thread_ids, .. } => {
            let description = prompt.clone().filter(|p| !p.trim().is_empty());
            Some(ToolCallParts {
                name: tool.clone(),
                // 只有启动子 agent 的那次调用是 `subagent`：子 agent 里的调用的 `parent_id` 指向它
                category: if tool == "spawnAgent" { ToolCategory::Subagent } else { ToolCategory::Other },
                title: match &description {
                    Some(p) => first_line_clipped(p, 120),
                    None => collab_title(tool),
                },
                input: json!({ "tool": tool, "prompt": prompt, "model": model, "receiverThreadIds": receiver_thread_ids }),
                detail: description.map(|d| ToolCallDetail { description: Some(d), ..Default::default() }),
            })
        }
        ThreadItem::SubAgentActivity { kind, agent_thread_id, agent_path, model, reasoning_effort, .. }
            if kind == "started" =>
        {
            let path = agent_path.trim().trim_start_matches('/');
            let path = path.strip_prefix("root/").unwrap_or(path);
            Some(ToolCallParts {
                name: "spawnAgent".to_string(),
                category: ToolCategory::Subagent,
                title: if path.is_empty() || path == "root" {
                    "Start a sub-agent".to_string()
                } else {
                    format!("Start sub-agent {path}")
                },
                input: json!({
                    "agentPath": agent_path, "agentThreadId": agent_thread_id,
                    "model": model, "reasoningEffort": reasoning_effort,
                }),
                detail: None,
            })
        }
        ThreadItem::WebSearch { query, action, .. } => Some(ToolCallParts {
            name: "webSearch".to_string(),
            category: ToolCategory::Search,
            title: if query.is_empty() { "Search the web".to_string() } else { format!("Search: {query}") },
            input: json!({ "query": query, "action": action }),
            detail: Some(ToolCallDetail { query: Some(query.clone()).filter(|q| !q.is_empty()), ..Default::default() }),
        }),
        ThreadItem::ImageView { path, .. } => Some(ToolCallParts {
            name: "imageView".to_string(),
            category: ToolCategory::FileRead,
            title: format!("View {}", rel_path(cwd, path)),
            input: json!({ "path": path }),
            detail: Some(ToolCallDetail { path: Some(path.clone()), ..Default::default() }),
        }),
        ThreadItem::ImageGeneration { revised_prompt, .. } => Some(ToolCallParts {
            name: "imageGeneration".to_string(),
            category: ToolCategory::Other,
            title: "Generate image".to_string(),
            input: json!({ "prompt": revised_prompt }),
            detail: None,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(path: &str, kind: PatchChangeKind, diff: &str) -> FileUpdateChange {
        FileUpdateChange { path: path.into(), kind, diff: diff.into() }
    }

    #[test]
    fn shell_wrappers_are_unwrapped() {
        assert_eq!(
            unwrap_shell("/bin/zsh -lc 'echo hello > out.txt && cat out.txt'"),
            "echo hello > out.txt && cat out.txt"
        );
        assert_eq!(unwrap_shell("bash -c \"ls -la\""), "ls -la");
        assert_eq!(unwrap_shell("/usr/bin/env zsh -lc 'x'"), "/usr/bin/env zsh -lc 'x'");
        assert_eq!(unwrap_shell("git status"), "git status");
        assert_eq!(unwrap_shell("python3 -c 'print(1)'"), "python3 -c 'print(1)'");
        assert_eq!(unwrap_shell("sh --version x"), "sh --version x");
        // 引号不闭合：原样
        assert_eq!(unwrap_shell("bash -c 'oops"), "bash -c 'oops");
    }

    #[test]
    fn relative_paths() {
        assert_eq!(rel_path("/work/proj", "/work/proj/src/a.rs"), "src/a.rs");
        assert_eq!(rel_path("/work/proj/", "/work/proj/a"), "a");
        // cwd 之外的保持绝对路径（以前会被写成 `etc/hosts`，像是项目里的文件）
        assert_eq!(rel_path("/work/proj", "/work/project2/a"), "/work/project2/a");
        assert_eq!(rel_path("/work/proj", "/etc/hosts"), "/etc/hosts");
        assert_eq!(rel_path("", "/a/b"), "/a/b");
        assert_eq!(rel_path("/work/proj", "/work/proj"), "/work/proj");
        // 含 `..` 的不算在项目里
        assert_eq!(rel_path("/work/proj", "/work/proj/../secrets/key"), "/work/proj/../secrets/key");
    }

    #[test]
    fn command_item_description() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "commandExecution", "id": "c1", "command": "/bin/zsh -lc 'echo hi\nls'", "cwd": "/work/proj",
            "status": "inProgress", "commandActions": [{"type": "unknown", "command": "echo hi"}], "source": "agent"
        }))
        .unwrap();
        let p = describe_item(&item, "/fallback").unwrap();
        assert_eq!((p.name.as_str(), p.category), ("commandExecution", ToolCategory::Shell));
        assert_eq!(p.title, "echo hi");
        let d = p.detail.unwrap();
        assert_eq!((d.command.as_deref(), d.cwd.as_deref()), (Some("echo hi\nls"), Some("/work/proj")));
        assert_eq!(p.input["command"], "/bin/zsh -lc 'echo hi\nls'");
        assert_eq!(p.input["cwd"], "/work/proj");
        assert_eq!(p.input["source"], "agent");
        assert_eq!(p.input["commandActions"][0]["type"], "unknown");
    }

    #[test]
    fn file_change_titles_and_details() {
        let add = change("/work/proj/a.txt", PatchChangeKind::Add, "x\n");
        let upd = change("/work/proj/src/b.rs", PatchChangeKind::Update { move_path: None }, "@@ -1 +1 @@\n-a\n+b\n");
        let mv =
            change("/work/proj/c.txt", PatchChangeKind::Update { move_path: Some("/work/proj/d.txt".into()) }, "@@\n");
        let del = change("/work/proj/e.txt", PatchChangeKind::Delete, "bye\n");
        assert_eq!(file_change_title("/work/proj", std::slice::from_ref(&add)), "Create a.txt");
        assert_eq!(file_change_title("/work/proj", std::slice::from_ref(&upd)), "Edit src/b.rs");
        assert_eq!(file_change_title("/work/proj", std::slice::from_ref(&mv)), "Move c.txt to d.txt");
        assert_eq!(file_change_title("/work/proj", std::slice::from_ref(&del)), "Delete e.txt");
        assert_eq!(file_change_title("/work/proj", &[add.clone(), upd.clone()]), "Edit 2 files (a.txt, src/b.rs)");
        // 项目外的文件写绝对路径；超过 3 个只列前 3 个
        let outside = change("/etc/hosts", PatchChangeKind::Update { move_path: None }, "@@\n");
        let four = [add.clone(), outside.clone(), upd.clone(), del.clone()];
        assert_eq!(file_change_title("/work/proj", &four), "Edit 4 files (a.txt, /etc/hosts, src/b.rs, …)");
        assert_eq!(file_change_title("/work/proj", std::slice::from_ref(&outside)), "Edit /etc/hosts");
        assert_eq!(file_change_title("/work/proj", &[]), "Edit files");
        let kinds: Vec<_> = [&add, &upd, &mv, &del]
            .iter()
            .map(|c| file_change_detail("/work/proj", std::slice::from_ref(c)).unwrap().edit_kind.unwrap())
            .collect();
        assert_eq!(kinds, vec![EditKind::Write, EditKind::Edit, EditKind::Move, EditKind::Delete]);
        assert_eq!(
            file_change_detail("/work/proj", std::slice::from_ref(&add)).unwrap().path.as_deref(),
            Some("/work/proj/a.txt")
        );
        assert!(file_change_detail("/work/proj", &[]).is_none());
        // 多文件补丁：全部路径列在 detail.description 里
        let list = file_change_list("/work/proj", &[add.clone(), outside, del.clone()]).unwrap();
        assert_eq!(list, "- create a.txt\n- edit /etc/hosts\n- delete e.txt");
        assert_eq!(
            file_change_detail("/work/proj", &[add.clone(), del.clone()]).unwrap().description.as_deref(),
            Some("- create a.txt\n- delete e.txt")
        );
        assert_eq!(file_change_detail("/work/proj", std::slice::from_ref(&add)).unwrap().description, None);
        assert!(file_change_list("/work/proj", &[]).is_none());
        let input = file_change_input(&[mv]);
        assert_eq!(input["changes"][0]["kind"], "update");
        assert_eq!(input["changes"][0]["move_path"], "/work/proj/d.txt");
    }

    #[test]
    fn unified_diffs_get_headers() {
        let add = change("/p/a.txt", PatchChangeKind::Add, "hi there\nsecond line\n");
        assert_eq!(unified_diff("/p", &add), "--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1,2 @@\n+hi there\n+second line\n");
        let upd = change(
            "/p/a.txt",
            PatchChangeKind::Update { move_path: None },
            "@@ -1,2 +1,2 @@\n-hi there\n+hello there\n second line\n",
        );
        assert_eq!(
            unified_diff("/p", &upd),
            "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n-hi there\n+hello there\n second line\n"
        );
        let mv = change(
            "/p/a.txt",
            PatchChangeKind::Update { move_path: Some("/p/b.txt".into()) },
            "@@ -1 +1 @@\n-x\n+y\n\n\nMoved to: /p/b.txt",
        );
        assert_eq!(unified_diff("/p", &mv), "--- a/a.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n");
        let del = change("/p/a.txt", PatchChangeKind::Delete, "bye\n");
        assert_eq!(unified_diff("/p", &del), "--- a/a.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye\n");
        // 空内容的新增文件：只有头
        assert_eq!(unified_diff("/p", &change("/p/empty", PatchChangeKind::Add, "")), "--- /dev/null\n+++ b/empty\n");
        // cwd 之外：头里是绝对路径，不加 a/ b/ 前缀
        let outside_add = change("/etc/new.conf", PatchChangeKind::Add, "k=v\n");
        assert_eq!(unified_diff("/p", &outside_add), "--- /dev/null\n+++ /etc/new.conf\n@@ -0,0 +1,1 @@\n+k=v\n");
        let outside_upd = change("/etc/hosts", PatchChangeKind::Update { move_path: None }, "@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(unified_diff("/p", &outside_upd), "--- /etc/hosts\n+++ /etc/hosts\n@@ -1 +1 @@\n-a\n+b\n");
        let outside_del = change("/etc/old", PatchChangeKind::Delete, "x\n");
        assert_eq!(unified_diff("/p", &outside_del), "--- /etc/old\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-x\n");
        let both = file_change_unified_diff("/p", &[add, del]);
        assert!(both.contains("+++ b/a.txt") && both.contains("+++ /dev/null"));
    }

    #[test]
    fn other_item_kinds() {
        let mcp: ThreadItem = serde_json::from_value(json!({
            "type": "mcpToolCall", "id": "m", "server": "github", "tool": "create_issue", "status": "inProgress",
            "arguments": {"title": "x"}
        }))
        .unwrap();
        let p = describe_item(&mcp, "/p").unwrap();
        assert_eq!(
            (p.name.as_str(), p.category, p.title.as_str()),
            ("mcp__github__create_issue", ToolCategory::Mcp, "github/create_issue")
        );
        assert_eq!(p.input, json!({"title": "x"}));
        let d = p.detail.unwrap();
        assert_eq!((d.server.as_deref(), d.tool.as_deref()), (Some("github"), Some("create_issue")));

        let web: ThreadItem =
            serde_json::from_value(json!({"type": "webSearch", "id": "w", "query": "rust async"})).unwrap();
        let p = describe_item(&web, "/p").unwrap();
        assert_eq!((p.category, p.title.as_str()), (ToolCategory::Search, "Search: rust async"));
        assert_eq!(p.detail.unwrap().query.as_deref(), Some("rust async"));

        let agent: ThreadItem = serde_json::from_value(json!({
            "type": "collabAgentToolCall", "id": "a", "tool": "spawnAgent", "status": "inProgress",
            "prompt": "Explore the repo\nthoroughly", "receiverThreadIds": ["t2"], "agentsStates": {}
        }))
        .unwrap();
        let p = describe_item(&agent, "/p").unwrap();
        assert_eq!((p.category, p.title.as_str()), (ToolCategory::Subagent, "Explore the repo"));
        assert_eq!(p.detail.unwrap().description.as_deref(), Some("Explore the repo\nthoroughly"));

        let img: ThreadItem =
            serde_json::from_value(json!({"type": "imageView", "id": "i", "path": "/p/a.png"})).unwrap();
        let p = describe_item(&img, "/p").unwrap();
        assert_eq!((p.category, p.title.as_str()), (ToolCategory::FileRead, "View a.png"));

        let dynamic: ThreadItem = serde_json::from_value(json!({
            "type": "dynamicToolCall", "id": "d", "tool": "lookup", "namespace": "ns", "arguments": {}, "status": "inProgress"
        }))
        .unwrap();
        let p = describe_item(&dynamic, "/p").unwrap();
        assert_eq!((p.name.as_str(), p.category), ("ns.lookup", ToolCategory::Other));

        let msg: ThreadItem = serde_json::from_value(json!({"type": "agentMessage", "id": "x", "text": ""})).unwrap();
        assert!(describe_item(&msg, "/p").is_none());
    }

    #[test]
    fn mcp_and_dynamic_outputs() {
        assert_eq!(
            mcp_output(&json!({"content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]})),
            json!("a\nb")
        );
        let mixed = json!({"content": [{"type": "text", "text": "a"}, {"type": "image", "data": "x"}]});
        assert_eq!(mcp_output(&mixed), mixed);
        assert_eq!(mcp_output(&json!({"content": []})), json!({"content": []}));
        assert_eq!(
            dynamic_output(&[
                json!({"type": "inputText", "text": "hi"}),
                json!({"type": "inputImage", "imageUrl": "u"})
            ]),
            "hi"
        );
    }

    #[test]
    fn big_output_is_truncated_on_a_char_boundary() {
        let big = "é".repeat(MAX_TOOL_OUTPUT_BYTES);
        let s = clip_text(&big);
        assert!(s.len() < MAX_TOOL_OUTPUT_BYTES + 64);
        assert!(s.contains("[truncated"));
        assert_eq!(clip_text("short"), "short");
    }
}
