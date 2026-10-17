//! Claude Code 工具名 → 协议里的 category / 标题 / 渲染字段（docs/spec/daemon.md § 8）。
//! `translate`（tool_call）和 `permissions`（权限请求）共用。

use anycon_protocol::{EditKind, ToolCallDetail, ToolCategory};
use serde_json::Value;

/// 工具名 → category。
pub fn category_of(name: &str) -> ToolCategory {
    match name {
        "Bash" => ToolCategory::Shell,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => ToolCategory::FileEdit,
        "Read" | "Glob" | "LS" => ToolCategory::FileRead,
        "Grep" | "WebSearch" => ToolCategory::Search,
        "WebFetch" => ToolCategory::Web,
        "Task" | "Agent" => ToolCategory::Subagent,
        n if n.starts_with("mcp__") => ToolCategory::Mcp,
        _ => ToolCategory::Other,
    }
}

/// `mcp__server__tool` → `(server, tool)`。工具名里允许再有 `__`。
pub fn split_mcp(name: &str) -> Option<(String, String)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    Some((server.to_string(), tool.to_string()))
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// 文件类工具操作的路径。
pub fn file_path_of(name: &str, input: &Value) -> Option<String> {
    let key = if name == "NotebookEdit" { "notebook_path" } else { "file_path" };
    str_field(input, key).or_else(|| str_field(input, "path")).map(str::to_string)
}

fn first_line_clipped(s: &str, max_chars: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let mut out: String = line.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// 按 category 归一化的渲染字段。`cwd` 是会话的工作目录（Bash 在那里跑）。
pub fn detail_of(name: &str, input: &Value, cwd: &str) -> Option<ToolCallDetail> {
    let mut d = ToolCallDetail::default();
    match category_of(name) {
        ToolCategory::Shell => {
            d.command = str_field(input, "command").map(str::to_string);
            d.cwd = Some(cwd.to_string());
        }
        ToolCategory::FileEdit => {
            d.path = file_path_of(name, input);
            d.edit_kind = Some(if name == "Write" { EditKind::Write } else { EditKind::Edit });
        }
        ToolCategory::FileRead => match name {
            "Read" => d.path = file_path_of(name, input),
            "Glob" => {
                d.query = str_field(input, "pattern").map(str::to_string);
                d.path = str_field(input, "path").map(str::to_string);
            }
            _ => d.path = str_field(input, "path").map(str::to_string),
        },
        ToolCategory::Search => {
            if name == "Grep" {
                d.query = str_field(input, "pattern").map(str::to_string);
                d.path = str_field(input, "path").map(str::to_string);
            } else {
                d.query = str_field(input, "query").map(str::to_string);
            }
        }
        ToolCategory::Web => d.url = str_field(input, "url").map(str::to_string),
        ToolCategory::Subagent => d.description = str_field(input, "description").map(str::to_string),
        ToolCategory::Mcp => {
            if let Some((server, tool)) = split_mcp(name) {
                d.server = Some(server);
                d.tool = Some(tool);
            }
        }
        ToolCategory::Other => return None,
    }
    if d == ToolCallDetail::default() {
        None
    } else {
        Some(d)
    }
}

/// `tool_call.title`：一行人能读的摘要。
pub fn title_of(name: &str, input: &Value) -> String {
    let with = |prefix: &str, v: Option<String>| match v {
        Some(v) => format!("{prefix} {v}"),
        None => prefix.to_string(),
    };
    match category_of(name) {
        ToolCategory::Shell => match str_field(input, "command") {
            Some(c) => first_line_clipped(c, 120),
            None => name.to_string(),
        },
        ToolCategory::FileEdit | ToolCategory::FileRead if name != "Glob" && name != "LS" => {
            with(name, file_path_of(name, input))
        }
        _ => match name {
            "Glob" => with("Glob", str_field(input, "pattern").map(str::to_string)),
            "LS" => with("LS", str_field(input, "path").map(str::to_string)),
            "Grep" => with("Grep", str_field(input, "pattern").map(str::to_string)),
            "WebSearch" => with("Search:", str_field(input, "query").map(str::to_string)),
            "WebFetch" => with("Fetch", str_field(input, "url").map(str::to_string)),
            "Task" | "Agent" => str_field(input, "description").map(str::to_string).unwrap_or_else(|| name.to_string()),
            n => match split_mcp(n) {
                Some((s, t)) => format!("{s}/{t}"),
                None => n.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn categories() {
        for (n, c) in [
            ("Bash", ToolCategory::Shell),
            ("Edit", ToolCategory::FileEdit),
            ("Write", ToolCategory::FileEdit),
            ("MultiEdit", ToolCategory::FileEdit),
            ("NotebookEdit", ToolCategory::FileEdit),
            ("Read", ToolCategory::FileRead),
            ("Glob", ToolCategory::FileRead),
            ("LS", ToolCategory::FileRead),
            ("Grep", ToolCategory::Search),
            ("WebSearch", ToolCategory::Search),
            ("WebFetch", ToolCategory::Web),
            ("Task", ToolCategory::Subagent),
            ("Agent", ToolCategory::Subagent),
            ("mcp__github__create_issue", ToolCategory::Mcp),
            ("TodoWrite", ToolCategory::Other),
            ("Whatever", ToolCategory::Other),
        ] {
            assert_eq!(category_of(n), c, "{n}");
        }
    }

    #[test]
    fn mcp_split_keeps_inner_double_underscores() {
        assert_eq!(split_mcp("mcp__srv__do__thing"), Some(("srv".into(), "do__thing".into())));
        assert_eq!(split_mcp("mcp__only"), None);
        assert_eq!(split_mcp("Bash"), None);
    }

    #[test]
    fn details() {
        let d = detail_of("Bash", &json!({"command": "ls -la"}), "/p").unwrap();
        assert_eq!((d.command.as_deref(), d.cwd.as_deref()), (Some("ls -la"), Some("/p")));
        let d = detail_of("Write", &json!({"file_path": "/p/a.txt"}), "/p").unwrap();
        assert_eq!((d.path.as_deref(), d.edit_kind), (Some("/p/a.txt"), Some(EditKind::Write)));
        let d = detail_of("MultiEdit", &json!({"file_path": "/p/a.txt"}), "/p").unwrap();
        assert_eq!(d.edit_kind, Some(EditKind::Edit));
        let d = detail_of("NotebookEdit", &json!({"notebook_path": "/p/n.ipynb"}), "/p").unwrap();
        assert_eq!(d.path.as_deref(), Some("/p/n.ipynb"));
        let d = detail_of("Read", &json!({"file_path": "/p/r"}), "/p").unwrap();
        assert_eq!(d.path.as_deref(), Some("/p/r"));
        let d = detail_of("Grep", &json!({"pattern": "foo", "path": "src"}), "/p").unwrap();
        assert_eq!((d.query.as_deref(), d.path.as_deref()), (Some("foo"), Some("src")));
        let d = detail_of("WebSearch", &json!({"query": "rust"}), "/p").unwrap();
        assert_eq!(d.query.as_deref(), Some("rust"));
        let d = detail_of("WebFetch", &json!({"url": "https://x.dev"}), "/p").unwrap();
        assert_eq!(d.url.as_deref(), Some("https://x.dev"));
        let d = detail_of("Task", &json!({"description": "explore"}), "/p").unwrap();
        assert_eq!(d.description.as_deref(), Some("explore"));
        let d = detail_of("mcp__srv__tool", &json!({}), "/p").unwrap();
        assert_eq!((d.server.as_deref(), d.tool.as_deref()), (Some("srv"), Some("tool")));
        assert!(detail_of("Whatever", &json!({"x": 1}), "/p").is_none());
    }

    #[test]
    fn titles() {
        assert_eq!(title_of("Bash", &json!({"command": "echo hi\nsecond"})), "echo hi");
        assert_eq!(title_of("Edit", &json!({"file_path": "/a/b.rs"})), "Edit /a/b.rs");
        assert_eq!(title_of("Read", &json!({})), "Read");
        assert_eq!(title_of("Grep", &json!({"pattern": "x"})), "Grep x");
        assert_eq!(title_of("WebFetch", &json!({"url": "u"})), "Fetch u");
        assert_eq!(title_of("Task", &json!({"description": "do it"})), "do it");
        assert_eq!(title_of("mcp__s__t", &json!({})), "s/t");
        assert_eq!(title_of("Foo", &json!({})), "Foo");
        let long = "x".repeat(300);
        assert!(title_of("Bash", &json!({ "command": long })).chars().count() <= 121);
    }
}
