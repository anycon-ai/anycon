//! ACP 工具调用的归一化（docs/spec/daemon.md § 8.1）：`kind` → category，`rawInput` / `locations` / `diff` 内容 →
//! `ToolCallDetail`，`content` / `rawOutput` / `diff` → `tool_result` 的输出和 `ToolResultDetail`。
//!
//! ACP 不规定 `rawInput` 的形状（每个 agent 自己的工具参数），所以每个字段都是「找几个常见的键名」的尽力而为；
//! 找不到就留空，客户端退回显示标题和原始输入。
//!
//! | ACP `kind` | category |
//! |---|---|
//! | `execute` | `shell` |
//! | `edit` / `delete` / `move` | `file_edit` |
//! | `read` | `file_read` |
//! | `search` | `search` |
//! | `fetch` | `web` |
//! | `think` / `switch_mode` / `other` / 不认识的 | `other` |

use anycon_protocol::{EditKind, ToolCallDetail, ToolCategory, ToolResultDetail};
use serde_json::Value;

use super::diff;

/// 一个文件的修改（`diff` 内容块）。
#[derive(Debug, Clone, PartialEq)]
pub struct DiffBlock {
    pub path: String,
    /// 修改前的内容；新建的文件没有。
    pub old: Option<String>,
    pub new: String,
}

pub fn category(kind: &str) -> ToolCategory {
    match kind {
        "execute" => ToolCategory::Shell,
        "edit" | "delete" | "move" => ToolCategory::FileEdit,
        "read" => ToolCategory::FileRead,
        "search" => ToolCategory::Search,
        "fetch" => ToolCategory::Web,
        _ => ToolCategory::Other,
    }
}

fn str_field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| v.get(*k).and_then(Value::as_str)).map(str::trim).filter(|s| !s.is_empty())
}

/// `execute` 的命令：字符串，或者 `["bash", "-lc", "…"]` 这样的数组（拼成一行）。
fn command_of(raw: &Value) -> Option<String> {
    for key in ["command", "cmd", "script"] {
        match raw.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.trim().to_string()),
            Some(Value::Array(a)) => {
                let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
                if !parts.is_empty() {
                    return Some(parts.join(" "));
                }
            }
            _ => {}
        }
    }
    None
}

/// `content[]` 里的文字（`type = content` 的文本块、内嵌的文本资源），按顺序拼接。
pub fn text_of_content(content: &[Value]) -> String {
    let mut out = String::new();
    for c in content {
        if c.get("type").and_then(Value::as_str) != Some("content") {
            continue;
        }
        let Some(inner) = c.get("content") else { continue };
        let piece = match inner.get("type").and_then(Value::as_str) {
            Some("text") => inner.get("text").and_then(Value::as_str),
            Some("resource") => inner.pointer("/resource/text").and_then(Value::as_str),
            _ => None,
        };
        if let Some(p) = piece {
            out.push_str(p);
        }
    }
    out
}

/// `content[]` 里的 `diff` 块。
pub fn diffs_of_content(content: &[Value]) -> Vec<DiffBlock> {
    content
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("diff"))
        .filter_map(|c| {
            Some(DiffBlock {
                path: c.get("path")?.as_str()?.to_string(),
                old: c.get("oldText").and_then(Value::as_str).map(str::to_string),
                new: c.get("newText").and_then(Value::as_str).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

/// `tool_call.detail`。
pub fn call_detail(
    kind: &str,
    raw_input: &Value,
    locations: &[String],
    diffs: &[DiffBlock],
    cwd: &str,
) -> Option<ToolCallDetail> {
    let mut d = ToolCallDetail::default();
    match category(kind) {
        ToolCategory::Shell => {
            d.command = command_of(raw_input);
            d.cwd = str_field(raw_input, &["cwd", "workdir", "workingDirectory", "working_directory"])
                .map(str::to_string)
                .or_else(|| (d.command.is_some()).then(|| cwd.to_string()));
        }
        ToolCategory::FileEdit => {
            d.path = locations.first().cloned().or_else(|| diffs.first().map(|x| x.path.clone())).or_else(|| {
                str_field(raw_input, &["path", "file_path", "filePath", "filepath", "file"]).map(str::to_string)
            });
            d.edit_kind = Some(match kind {
                "delete" => EditKind::Delete,
                "move" => EditKind::Move,
                _ if diffs.iter().any(|x| x.old.is_none()) => EditKind::Write,
                _ => EditKind::Edit,
            });
        }
        ToolCategory::FileRead => {
            d.path = locations.first().cloned().or_else(|| {
                str_field(raw_input, &["path", "file_path", "filePath", "filepath", "file"]).map(str::to_string)
            });
        }
        ToolCategory::Search => {
            d.query = str_field(raw_input, &["query", "pattern", "regex", "q", "search"]).map(str::to_string);
            d.path = str_field(raw_input, &["path", "directory", "dir"]).map(str::to_string);
        }
        ToolCategory::Web => {
            d.url = str_field(raw_input, &["url", "uri"]).map(str::to_string);
        }
        _ => {}
    }
    (d != ToolCallDetail::default()).then_some(d)
}

/// 文件头里的名字：在 cwd 里面写相对路径（`a/…` / `b/…`），cwd 之外保持绝对路径。
fn diff_name(cwd: &str, path: &str, prefix: &str) -> String {
    match std::path::Path::new(path).strip_prefix(cwd) {
        Ok(rel) if !cwd.is_empty() && !rel.as_os_str().is_empty() => format!("{prefix}/{}", rel.to_string_lossy()),
        _ => path.to_string(),
    }
}

/// 一次工具调用目前为止合并出来的样子（`tool_call` 加上之后所有 `tool_call_update` 的补丁；权限请求里的 `toolCall`
/// 也是同一个形状）。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolView {
    pub id: String,
    /// ACP 的 `kind`；agent 没给时是 `other`。
    pub kind: String,
    pub title: String,
    /// `name`（agent 给的工具名）。
    pub name: Option<String>,
    pub raw_input: Value,
    pub content: Vec<Value>,
    /// `locations[].path`。
    pub locations: Vec<String>,
}

impl ToolView {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: "other".to_string(),
            title: String::new(),
            name: None,
            raw_input: Value::Null,
            content: Vec::new(),
            locations: Vec::new(),
        }
    }

    /// 把一条 `tool_call` / `tool_call_update` / 权限请求里的 `toolCall` 合进来：只覆盖出现了的（非 `null` 的）字段。
    /// `content` / `locations` 是整个替换，`rawInput` 同理。
    pub fn merge(&mut self, v: &Value) {
        if let Some(k) = v.get("kind").and_then(Value::as_str).filter(|k| !k.is_empty()) {
            self.kind = k.to_string();
        }
        if let Some(t) = v.get("title").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
            self.title = t.to_string();
        }
        if let Some(n) = v.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
            self.name = Some(n.to_string());
        }
        if let Some(i) = v.get("rawInput").filter(|i| !i.is_null()) {
            self.raw_input = i.clone();
        }
        if let Some(c) = v.get("content").and_then(Value::as_array) {
            self.content = c.clone();
        }
        if let Some(l) = v.get("locations").and_then(Value::as_array) {
            self.locations =
                l.iter().filter_map(|x| x.get("path").and_then(Value::as_str)).map(str::to_string).collect();
        }
    }

    pub fn category(&self) -> ToolCategory {
        category(&self.kind)
    }

    /// `tool_call.name`：agent 给的工具名，没有就用 `kind`。
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.kind.clone())
    }

    pub fn display_title(&self) -> String {
        if self.title.is_empty() {
            self.display_name()
        } else {
            self.title.clone()
        }
    }

    /// `tool_call.input`：`rawInput`，没有就是空对象。
    pub fn input(&self) -> Value {
        if self.raw_input.is_null() {
            Value::Object(Default::default())
        } else {
            self.raw_input.clone()
        }
    }

    pub fn text(&self) -> String {
        text_of_content(&self.content)
    }

    pub fn diffs(&self) -> Vec<DiffBlock> {
        diffs_of_content(&self.content)
    }

    pub fn detail(&self, cwd: &str) -> Option<ToolCallDetail> {
        call_detail(&self.kind, &self.raw_input, &self.locations, &self.diffs(), cwd)
    }

    /// 有没有足够的信息可以发 `tool_call` 了：已经有输入、内容或位置（agent 往往先发一个空壳的 `pending`，参数随后才补上）。
    pub fn has_substance(&self) -> bool {
        !is_empty_json(&self.raw_input) || !self.content.is_empty() || !self.locations.is_empty()
    }
}

fn is_empty_json(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Object(o) => o.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// 第一行，最多 `max_chars` 个字符（超出补 `…`）。
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

/// 一组 diff 块的 unified diff（每个文件一段）。`deleted`：这次调用删文件，新文件头写 `/dev/null`。
/// 文本太大算不出来的文件跳过；一个都算不出来 = `None`。
pub fn unified_diff(cwd: &str, diffs: &[DiffBlock], deleted: bool) -> Option<String> {
    let mut out = String::new();
    for d in diffs {
        let old_name = if d.old.is_some() || deleted { diff_name(cwd, &d.path, "a") } else { "/dev/null".to_string() };
        let new_name = if deleted { "/dev/null".to_string() } else { diff_name(cwd, &d.path, "b") };
        if let Some(text) = diff::unified_diff(&old_name, &new_name, d.old.as_deref(), &d.new) {
            out.push_str(&text);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `tool_result.output`：文字内容优先，其次 `rawOutput`，最后是 diff 的一句话概述。
pub fn output_value(text: &str, raw_output: &Value, diffs: &[DiffBlock]) -> Value {
    if !text.is_empty() {
        return Value::String(text.to_string());
    }
    if !raw_output.is_null() {
        return raw_output.clone();
    }
    match diffs {
        [] => Value::Null,
        [d] => Value::String(format!("Edited {}", d.path)),
        many => Value::String(format!("Edited {} files", many.len())),
    }
}

/// 命令的退出码：`rawOutput` 里常见的几种写法。
fn exit_code_of(raw_output: &Value) -> Option<i32> {
    ["exit_code", "exitCode", "exit", "code", "status"]
        .iter()
        .find_map(|k| raw_output.get(*k).and_then(Value::as_i64))
        .or_else(|| raw_output.pointer("/metadata/exit").and_then(Value::as_i64))
        .and_then(|c| i32::try_from(c).ok())
}

/// `tool_result.detail`。
pub fn result_detail(kind: &str, raw_output: &Value, unified_diff: Option<String>) -> Option<ToolResultDetail> {
    let mut d = ToolResultDetail::default();
    if category(kind) == ToolCategory::Shell {
        d.exit_code = exit_code_of(raw_output);
    }
    d.unified_diff = unified_diff;
    (d != ToolResultDetail::default()).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_map_to_categories() {
        for (k, c) in [
            ("execute", ToolCategory::Shell),
            ("edit", ToolCategory::FileEdit),
            ("delete", ToolCategory::FileEdit),
            ("move", ToolCategory::FileEdit),
            ("read", ToolCategory::FileRead),
            ("search", ToolCategory::Search),
            ("fetch", ToolCategory::Web),
            ("think", ToolCategory::Other),
            ("switch_mode", ToolCategory::Other),
            ("other", ToolCategory::Other),
            ("something-new", ToolCategory::Other),
        ] {
            assert_eq!(category(k), c, "{k}");
        }
    }

    #[test]
    fn shell_detail_finds_the_command_in_common_shapes() {
        let d = call_detail("execute", &json!({"command": "ls -la", "cwd": "/w/sub"}), &[], &[], "/w").unwrap();
        assert_eq!((d.command.as_deref(), d.cwd.as_deref()), (Some("ls -la"), Some("/w/sub")));
        let d = call_detail("execute", &json!({"command": ["bash", "-lc", "echo hi"]}), &[], &[], "/w").unwrap();
        assert_eq!((d.command.as_deref(), d.cwd.as_deref()), (Some("bash -lc echo hi"), Some("/w")));
        let d = call_detail("execute", &json!({"cmd": "make"}), &[], &[], "/w").unwrap();
        assert_eq!(d.command.as_deref(), Some("make"));
        // 认不出命令：没有 detail（不凭空造一个 cwd）
        assert_eq!(call_detail("execute", &json!({"weird": 1}), &[], &[], "/w"), None);
        assert_eq!(call_detail("execute", &Value::Null, &[], &[], "/w"), None);
    }

    #[test]
    fn file_and_search_and_web_details() {
        let diff = DiffBlock { path: "/w/a.txt".into(), old: None, new: "x".into() };
        let d = call_detail("edit", &Value::Null, &[], std::slice::from_ref(&diff), "/w").unwrap();
        assert_eq!((d.path.as_deref(), d.edit_kind), (Some("/w/a.txt"), Some(EditKind::Write)));
        let modified = DiffBlock { old: Some("o".into()), ..diff.clone() };
        let d = call_detail("edit", &Value::Null, &["/w/loc.txt".into()], &[modified], "/w").unwrap();
        assert_eq!((d.path.as_deref(), d.edit_kind), (Some("/w/loc.txt"), Some(EditKind::Edit)), "locations first");
        let d = call_detail("delete", &json!({"path": "/w/old.txt"}), &[], &[], "/w").unwrap();
        assert_eq!((d.path.as_deref(), d.edit_kind), (Some("/w/old.txt"), Some(EditKind::Delete)));
        let d = call_detail("move", &json!({"file_path": "/w/b"}), &[], &[], "/w").unwrap();
        assert_eq!(d.edit_kind, Some(EditKind::Move));
        let d = call_detail("read", &json!({"filePath": "/w/r.rs"}), &[], &[], "/w").unwrap();
        assert_eq!(d.path.as_deref(), Some("/w/r.rs"));
        let d = call_detail("search", &json!({"pattern": "TODO", "path": "src"}), &[], &[], "/w").unwrap();
        assert_eq!((d.query.as_deref(), d.path.as_deref()), (Some("TODO"), Some("src")));
        let d = call_detail("fetch", &json!({"url": "https://example.com"}), &[], &[], "/w").unwrap();
        assert_eq!(d.url.as_deref(), Some("https://example.com"));
        assert_eq!(call_detail("think", &json!({"x": 1}), &[], &[], "/w"), None);
    }

    #[test]
    fn content_text_and_diffs_are_extracted() {
        let content = vec![
            json!({"type": "content", "content": {"type": "text", "text": "hello "}}),
            json!({"type": "content", "content": {"type": "image", "data": "AA==", "mimeType": "image/png"}}),
            json!({"type": "diff", "path": "/w/a.txt", "oldText": "a\n", "newText": "b\n"}),
            json!({"type": "content", "content": {"type": "resource", "resource": {"uri": "file:///x", "text": "world"}}}),
            json!({"type": "terminal", "terminalId": "t1"}),
        ];
        assert_eq!(text_of_content(&content), "hello world");
        assert_eq!(
            diffs_of_content(&content),
            vec![DiffBlock { path: "/w/a.txt".into(), old: Some("a\n".into()), new: "b\n".into() }]
        );
        // `oldText: null` = 新文件
        let d = diffs_of_content(&[json!({"type": "diff", "path": "/w/n", "oldText": null, "newText": "x"})]);
        assert_eq!(d[0].old, None);
    }

    #[test]
    fn unified_diff_names_files_relative_to_the_cwd() {
        let inside = DiffBlock { path: "/w/src/a.txt".into(), old: Some("a\n".into()), new: "b\n".into() };
        let created = DiffBlock { path: "/w/new.txt".into(), old: None, new: "x\n".into() };
        let outside = DiffBlock { path: "/etc/hosts".into(), old: Some("h\n".into()), new: "g\n".into() };
        let d = unified_diff("/w", &[inside, created, outside], false).unwrap();
        assert!(d.contains("--- a/src/a.txt\n+++ b/src/a.txt\n@@ -1 +1 @@\n-a\n+b\n"), "{d}");
        assert!(d.contains("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+x\n"), "{d}");
        assert!(d.contains("--- /etc/hosts\n+++ /etc/hosts\n"), "{d}");
        let gone = DiffBlock { path: "/w/gone.txt".into(), old: Some("bye\n".into()), new: String::new() };
        assert_eq!(unified_diff("/w", &[gone], true).unwrap(), "--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n");
        assert_eq!(unified_diff("/w", &[], false), None);
    }

    #[test]
    fn output_prefers_text_then_raw_output_then_a_diff_summary() {
        let d = DiffBlock { path: "/w/a".into(), old: None, new: "x".into() };
        assert_eq!(output_value("out", &json!({"x": 1}), &[]), json!("out"));
        assert_eq!(output_value("", &json!({"x": 1}), std::slice::from_ref(&d)), json!({"x": 1}));
        assert_eq!(output_value("", &Value::Null, std::slice::from_ref(&d)), json!("Edited /w/a"));
        assert_eq!(output_value("", &Value::Null, &[d.clone(), d]), json!("Edited 2 files"));
        assert_eq!(output_value("", &Value::Null, &[]), Value::Null);
    }

    #[test]
    fn exit_codes_come_from_common_raw_output_shapes() {
        assert_eq!(result_detail("execute", &json!({"exit_code": 2}), None).unwrap().exit_code, Some(2));
        assert_eq!(result_detail("execute", &json!({"exitCode": 0}), None).unwrap().exit_code, Some(0));
        assert_eq!(result_detail("execute", &json!({"metadata": {"exit": 1}}), None).unwrap().exit_code, Some(1));
        assert_eq!(result_detail("execute", &json!("plain text"), None), None);
        // 不是 shell：不猜退出码
        assert_eq!(result_detail("read", &json!({"exit_code": 2}), None), None);
        let r = result_detail("edit", &Value::Null, Some("--- a\n".into())).unwrap();
        assert_eq!(r.unified_diff.as_deref(), Some("--- a\n"));
    }

    #[test]
    fn tool_views_merge_updates_field_by_field() {
        let mut v = ToolView::new("c1");
        assert!(!v.has_substance());
        assert_eq!((v.display_name().as_str(), v.display_title().as_str()), ("other", "other"));
        v.merge(
            &json!({"toolCallId": "c1", "title": "Run ls", "kind": "execute", "status": "pending", "rawInput": {}}),
        );
        assert_eq!((v.kind.as_str(), v.title.as_str()), ("execute", "Run ls"));
        assert!(!v.has_substance(), "an empty rawInput is not substance yet");
        assert_eq!(v.input(), json!({}));
        v.merge(&json!({"status": "in_progress", "rawInput": {"command": "ls"}, "name": "bash"}));
        assert!(v.has_substance());
        assert_eq!((v.display_name().as_str(), v.title.as_str()), ("bash", "Run ls"), "unmentioned fields stay");
        // null 不覆盖
        v.merge(&json!({"title": null, "kind": null, "rawInput": null, "content": null}));
        assert_eq!((v.kind.as_str(), v.title.as_str()), ("execute", "Run ls"));
        assert_eq!(v.raw_input, json!({"command": "ls"}));
        v.merge(&json!({"content": [{"type": "content", "content": {"type": "text", "text": "x"}}], "locations": [{"path": "/w/a", "line": 3}]}));
        assert_eq!((v.text().as_str(), v.locations.clone()), ("x", vec!["/w/a".to_string()]));
        assert_eq!(v.detail("/w").unwrap().command.as_deref(), Some("ls"));
        // 内容整个替换
        v.merge(&json!({"content": []}));
        assert_eq!(v.text(), "");
        assert_eq!(first_line_clipped("one\ntwo", 10), "one");
        assert_eq!(first_line_clipped(&"x".repeat(20), 5), "xxxxx…");
    }
}
