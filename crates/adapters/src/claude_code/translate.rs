//! stream-json（CLI 的 stdout，一行一个 JSON）→ [`AgentEvent`]。
//!
//! 纯同步状态机：`feed_line` 进、`Vec<Out>` 出，不碰进程和 IO，所以可以直接拿录制的 JSONL 回放测试。
//! 要点（部分来自对真实 CLI 2.1.x 的观察，见 README.md）：
//! - 非 `{` 开头的行忽略；未知 `type` / `subtype` 忽略（`status`、`commands_changed`、`rate_limit_event` …）
//! - `stream_event` 只取文本 / 思考的增量；**tool_call 在 `assistant` 消息里发**（那里有完整 input）
//! - 每个 content block 会各自带一条 `assistant` 消息；文本已经流式发过的，不再从 `assistant` 重发
//! - 思考内容在较新的 CLI 里是空串（只有 `estimated_tokens`）：每个思考块只发一条空的 `ThinkingDelta`
//! - `result.total_cost_usd` 是**进程内累计值**，`usage` / `duration_ms` 是**本 turn 的**；这里把费用换成增量
//! - `system/init` 每个 turn 都会再发一次；只在 `session_id` 变化时再发 `Started`
//! - `TodoWrite` → `Plan`；`ExitPlanMode` / `AskUserQuestion` 由权限请求表达，不再发 tool_call / tool_result
//! - 子 agent 内部的调用（`parent_tool_use_id` 非空）带 `parent_id`；后台启动的子 agent（`tool_use_result.status =
//!   async_launched`）的结果标 `background`，等 `system/task_notification` 再给同一个 id 补最终结果

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anycon_agent_api::{AgentEvent, PendingPermission, ResumeHandle};
use anycon_protocol::{
    Event, FileChangeKind, PlanEntry, PlanStatus, ToolCategory, ToolResultDetail, TurnDoneReason, Usage,
};
use serde_json::{json, Value};

use super::permissions::{self, NativePending};
use super::tools;

/// 单个工具输出（文本）保留的上限。历史是唯一副本，但一次 `cat` 大文件不该撑爆事件流。
pub const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ControlAck {
    pub request_id: String,
    pub ok: bool,
    /// 成功时 CLI 回的 `response` 对象（可能为 `Null`）。
    pub response: Value,
    pub error: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Event(AgentEvent),
    /// 权限请求：`native` 由 handle 记下，答复时用。
    Permission {
        pending: PendingPermission,
        native: NativePending,
    },
    /// CLI 对我们发出的 control_request 的答复。
    ControlAck(ControlAck),
    /// CLI 撤回了一个 control_request（如中断时未决的权限请求）。
    ControlCancel {
        request_id: String,
    },
    /// 不认识的 control_request：handle 应回一个错误，免得 CLI 干等。
    Unsupported {
        request_id: String,
        subtype: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
    Other,
}

struct Block {
    kind: BlockKind,
    started: Instant,
    /// 已经给上层发过东西（才需要发对应的 Done）。
    emitted: bool,
    top_level: bool,
}

struct ToolMeta {
    name: String,
    path: Option<String>,
}

/// 进程退出时的收尾结果。
pub struct Finish {
    pub out: Vec<Out>,
    /// 退出时 turn 还在进行，且不是我们要求的取消——上层应补一个 `Error` 事件。
    pub died_mid_turn: bool,
    /// 退出前我们已经要求了取消（interrupt / shutdown），所以这次退出是预期内的。
    pub cancelled: bool,
}

pub struct Translator {
    cwd: String,
    launch_args: Value,
    started_session_id: Option<String>,
    model: Option<String>,
    turn_counter: u64,
    busy: bool,
    cancel_requested: bool,
    prev_total_cost: f64,
    cur_msg_id: String,
    blocks: HashMap<u64, Block>,
    text_streamed: HashSet<String>,
    thinking_streamed: HashSet<String>,
    fallback_done: HashSet<(String, usize)>,
    seen_tool_use: HashSet<String>,
    tools: HashMap<String, ToolMeta>,
    suppressed: HashSet<String>,
    /// 已返回 `async_launched`、还在后台跑的子 agent 调用：等 `task_notification` 补最终结果。
    backgrounded: HashSet<String>,
}

fn clip_text(s: &str) -> String {
    if s.len() <= MAX_TOOL_OUTPUT_BYTES {
        return s.to_string();
    }
    let mut end = MAX_TOOL_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…[truncated {} bytes]", &s[..end], s.len() - end)
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

fn parse_exit_code(text: &str) -> Option<i32> {
    let rest = text.strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

fn plan_status(s: &str) -> PlanStatus {
    match s {
        "completed" => PlanStatus::Completed,
        "in_progress" => PlanStatus::InProgress,
        _ => PlanStatus::Pending,
    }
}

/// 图片 block 里的 base64 不放进事件（一张图可能有几 MiB，事件要进库、进历史页、被所有设备拉）：
/// 保留 block 的结构和 `media_type`，`source.data` 换成一句说明。其它 block 原样。
fn redact_image(block: &Value) -> Value {
    let mut b = block.clone();
    if b["type"] == "image" {
        if let Some(len) = b["source"]["data"].as_str().map(str::len) {
            b["source"]["data"] = Value::String(format!("[omitted: {len} bytes of base64]"));
        }
    }
    b
}

/// 工具结果的 `content`：字符串，或 `[{type:"text",text}…]`（也可能夹图片，图片的 base64 会被去掉，见 [`redact_image`]）。
/// 返回 `(放进事件的 output, 纯文本)`；纯文本只含 text block。
fn tool_output(content: &Value) -> (Value, String) {
    match content {
        Value::String(s) => (Value::String(clip_text(s)), s.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> =
                blocks.iter().filter_map(|b| if b["type"] == "text" { b["text"].as_str() } else { None }).collect();
            let joined = texts.join("\n");
            if blocks.iter().all(|b| b["type"] == "text") {
                (Value::String(clip_text(&joined)), joined)
            } else {
                (Value::Array(blocks.iter().map(redact_image).collect()), joined)
            }
        }
        Value::Null => (Value::String(String::new()), String::new()),
        other => (other.clone(), other.to_string()),
    }
}

fn unified_diff(cwd: &str, path: &str, hunks: &[Value]) -> String {
    let rel = path.strip_prefix(cwd).map(|r| r.trim_start_matches('/')).unwrap_or_else(|| path.trim_start_matches('/'));
    let mut s = format!("--- a/{rel}\n+++ b/{rel}\n");
    for h in hunks {
        s.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            u64_of(h, "oldStart").unwrap_or(0),
            u64_of(h, "oldLines").unwrap_or(0),
            u64_of(h, "newStart").unwrap_or(0),
            u64_of(h, "newLines").unwrap_or(0),
        ));
        for l in h.get("lines").and_then(Value::as_array).into_iter().flatten() {
            if let Some(l) = l.as_str() {
                s.push_str(l);
                s.push('\n');
            }
        }
    }
    clip_text(&s)
}

fn created_diff(cwd: &str, path: &str, content: &str) -> String {
    let rel = path.strip_prefix(cwd).map(|r| r.trim_start_matches('/')).unwrap_or_else(|| path.trim_start_matches('/'));
    let n = content.lines().count();
    let mut s = format!("--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{n} @@\n");
    for l in content.lines() {
        s.push('+');
        s.push_str(l);
        s.push('\n');
    }
    clip_text(&s)
}

impl Translator {
    /// `started_session_id`：spawn 时已经告诉上层的 agent 会话 id（`--session-id` / `--resume`），
    /// `system/init` 带同一个 id 就不重复发 `Started`。
    pub fn new(cwd: impl Into<String>, launch_args: Value, started_session_id: Option<String>) -> Self {
        Self {
            cwd: cwd.into(),
            launch_args,
            started_session_id,
            model: None,
            turn_counter: 0,
            busy: false,
            cancel_requested: false,
            prev_total_cost: 0.0,
            cur_msg_id: String::new(),
            blocks: HashMap::new(),
            text_streamed: HashSet::new(),
            thinking_streamed: HashSet::new(),
            fallback_done: HashSet::new(),
            seen_tool_use: HashSet::new(),
            tools: HashMap::new(),
            suppressed: HashSet::new(),
            backgrounded: HashSet::new(),
        }
    }

    /// `set_settings` 改了启动参数之后更新：之后因 `system/init` 带出新会话 id 而发的 `Started` 要带最新的 `launch_args`，
    /// 否则 daemon 存下的快照里还是被清除之前的值（resume 路径不读它，但快照不该过期）。
    pub fn set_launch_args(&mut self, launch_args: Value) {
        self.launch_args = launch_args;
    }

    /// `send` 之前调用：开始新 turn。
    pub fn begin_turn(&mut self) {
        self.turn_counter += 1;
        self.busy = true;
        self.cancel_requested = false;
        // 消息 / 工具调用不会跨 turn，清掉各种去重表，长期保活的会话里不会无限增长。
        self.blocks.clear();
        self.text_streamed.clear();
        self.thinking_streamed.clear();
        self.fallback_done.clear();
        self.seen_tool_use.clear();
        self.tools.clear();
        self.suppressed.clear();
    }

    /// 我们发出了 interrupt（或要关闭进程）：此后 `result` 按 cancelled 算。
    pub fn mark_cancel_requested(&mut self) {
        if self.busy {
            self.cancel_requested = true;
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn turn_id(&self) -> u64 {
        self.turn_counter
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn feed_line(&mut self, line: &str) -> Vec<Out> {
        let line = line.trim();
        if !line.starts_with('{') {
            return Vec::new();
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => self.feed(&v),
            Err(e) => {
                tracing::debug!(error = %e, "claude: ignoring malformed JSON line");
                Vec::new()
            }
        }
    }

    pub fn feed(&mut self, v: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        match v.get("type").and_then(Value::as_str) {
            Some("system") => self.on_system(v, &mut out),
            Some("stream_event") => self.on_stream_event(v, &mut out),
            Some("assistant") => self.on_assistant(v, &mut out),
            Some("user") => self.on_user(v, &mut out),
            Some("result") => self.on_result(v, &mut out),
            Some("control_request") => self.on_control_request(v, &mut out),
            Some("control_response") => self.on_control_response(v, &mut out),
            Some("control_cancel_request") => {
                if let Some(id) = v.get("request_id").and_then(Value::as_str) {
                    out.push(Out::ControlCancel { request_id: id.to_string() });
                }
            }
            _ => {}
        }
        out
    }

    /// stdout 关闭（进程退出）时调用。
    pub fn finish(&mut self) -> Finish {
        let mut out = Vec::new();
        let mut died = false;
        let cancelled = self.cancel_requested;
        if self.busy {
            if self.cancel_requested {
                self.busy = false;
                out.push(Out::Event(AgentEvent::Event(Event::TurnDone {
                    turn_id: self.turn_counter,
                    reason: TurnDoneReason::Cancelled,
                    usage: None,
                })));
            } else {
                died = true;
                self.busy = false;
            }
        }
        Finish { out, died_mid_turn: died, cancelled }
    }

    fn push(out: &mut Vec<Out>, e: Event) {
        out.push(Out::Event(AgentEvent::Event(e)));
    }

    // ---------- system ----------

    fn on_system(&mut self, v: &Value, out: &mut Vec<Out>) {
        match v.get("subtype").and_then(Value::as_str) {
            Some("init") => {}
            Some("task_notification") => return self.on_task_notification(v, out),
            _ => return,
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = Some(m.to_string());
        }
        if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
            if self.started_session_id.as_deref() != Some(sid) {
                self.started_session_id = Some(sid.to_string());
                out.push(Out::Event(AgentEvent::Started {
                    resume: ResumeHandle { agent_session_id: sid.to_string(), launch_args: self.launch_args.clone() },
                }));
            }
        }
    }

    /// 后台任务结束（`completed` / `failed` / `stopped`）。只认我们标过 `background` 的子 agent 调用：
    /// 前台子 agent 和后台 shell 也会发这个通知，它们的结果已经发过了。
    fn on_task_notification(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v.get("tool_use_id").and_then(Value::as_str) else { return };
        if !self.backgrounded.remove(id) {
            return;
        }
        let summary = v["summary"].as_str().unwrap_or("");
        Self::push(
            out,
            Event::ToolResult {
                id: id.to_string(),
                output: json!(clip_text(summary)),
                is_error: v["status"].as_str() == Some("failed"),
                detail: None,
            },
        );
    }

    // ---------- stream_event ----------

    fn on_stream_event(&mut self, v: &Value, out: &mut Vec<Out>) {
        let top = v.get("parent_tool_use_id").map(Value::is_null).unwrap_or(true);
        let ev = &v["event"];
        let index = u64_of(ev, "index").unwrap_or(0);
        match ev.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                self.cur_msg_id = ev["message"]["id"].as_str().unwrap_or("").to_string();
                self.blocks.clear();
            }
            Some("content_block_start") => {
                let kind = match ev["content_block"]["type"].as_str() {
                    Some("text") => BlockKind::Text,
                    Some("thinking") => BlockKind::Thinking,
                    Some("tool_use") => BlockKind::ToolUse,
                    _ => BlockKind::Other,
                };
                self.blocks.insert(index, Block { kind, started: Instant::now(), emitted: false, top_level: top });
            }
            Some("content_block_delta") => {
                let delta = &ev["delta"];
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or("");
                        if top && !text.is_empty() {
                            self.text_streamed.insert(self.cur_msg_id.clone());
                            if let Some(b) = self.blocks.get_mut(&index) {
                                b.emitted = true;
                            }
                            Self::push(out, Event::TextDelta { text: text.to_string() });
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or("");
                        if !top {
                            return;
                        }
                        self.thinking_streamed.insert(self.cur_msg_id.clone());
                        let block = self.blocks.get_mut(&index);
                        let already = block.as_ref().map(|b| b.emitted).unwrap_or(false);
                        if !text.is_empty() || !already {
                            if let Some(b) = block {
                                b.emitted = true;
                            }
                            Self::push(out, Event::ThinkingDelta { text: text.to_string() });
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(b) = self.blocks.remove(&index) {
                    if b.top_level && b.emitted {
                        match b.kind {
                            BlockKind::Text => Self::push(out, Event::TextDone {}),
                            BlockKind::Thinking => Self::push(
                                out,
                                Event::ThinkingDone { duration_ms: b.started.elapsed().as_millis() as u64 },
                            ),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ---------- assistant ----------

    fn on_assistant(&mut self, v: &Value, out: &mut Vec<Out>) {
        let parent = v.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let top = parent.is_none();
        let msg = &v["message"];
        let mid = msg["id"].as_str().unwrap_or("").to_string();
        let Some(blocks) = msg["content"].as_array() else { return };
        for (i, block) in blocks.iter().enumerate() {
            match block["type"].as_str() {
                Some("text") if top => {
                    let text = block["text"].as_str().unwrap_or("");
                    if !text.is_empty()
                        && !self.text_streamed.contains(&mid)
                        && self.fallback_done.insert((mid.clone(), i))
                    {
                        Self::push(out, Event::TextDelta { text: text.to_string() });
                        Self::push(out, Event::TextDone {});
                    }
                }
                Some("thinking") if top => {
                    let text = block["thinking"].as_str().unwrap_or("");
                    if !text.is_empty()
                        && !self.thinking_streamed.contains(&mid)
                        && self.fallback_done.insert((mid.clone(), i))
                    {
                        Self::push(out, Event::ThinkingDelta { text: text.to_string() });
                        Self::push(out, Event::ThinkingDone { duration_ms: 0 });
                    }
                }
                Some("tool_use") => self.on_tool_use(block, parent.clone(), out),
                _ => {}
            }
        }
    }

    fn on_tool_use(&mut self, block: &Value, parent_id: Option<String>, out: &mut Vec<Out>) {
        let id = block["id"].as_str().unwrap_or("").to_string();
        let name = block["name"].as_str().unwrap_or("").to_string();
        if id.is_empty() || !self.seen_tool_use.insert(id.clone()) {
            return;
        }
        let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
        match name.as_str() {
            "TodoWrite" => {
                self.suppressed.insert(id);
                let entries: Vec<PlanEntry> = input["todos"]
                    .as_array()
                    .map(|todos| {
                        todos
                            .iter()
                            .map(|t| PlanEntry {
                                content: t["content"].as_str().unwrap_or("").to_string(),
                                status: plan_status(t["status"].as_str().unwrap_or("pending")),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Self::push(out, Event::Plan { entries });
            }
            "ExitPlanMode" | "AskUserQuestion" => {
                self.suppressed.insert(id);
            }
            _ => {
                let category = tools::category_of(&name);
                self.tools
                    .insert(id.clone(), ToolMeta { name: name.clone(), path: tools::file_path_of(&name, &input) });
                Self::push(
                    out,
                    Event::ToolCall {
                        id,
                        title: tools::title_of(&name, &input),
                        detail: tools::detail_of(&name, &input, &self.cwd),
                        category,
                        name,
                        input,
                        parent_id,
                    },
                );
            }
        }
    }

    // ---------- user（tool_result） ----------

    fn on_user(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(blocks) = v["message"]["content"].as_array() else { return };
        let tur = v.get("tool_use_result").unwrap_or(&Value::Null);
        for block in blocks {
            if block["type"] != "tool_result" {
                continue;
            }
            let id = block["tool_use_id"].as_str().unwrap_or("").to_string();
            if self.suppressed.remove(&id) {
                continue;
            }
            let is_error = block["is_error"].as_bool().unwrap_or(false);
            let (output, text) = tool_output(&block["content"]);
            let meta = self.tools.remove(&id);
            let name = meta.as_ref().map(|m| m.name.as_str()).unwrap_or("");
            let category = tools::category_of(name);

            let mut detail = ToolResultDetail::default();
            match category {
                ToolCategory::Shell => detail.exit_code = if is_error { parse_exit_code(&text) } else { Some(0) },
                ToolCategory::FileEdit if !is_error => {
                    let path = tur["filePath"]
                        .as_str()
                        .map(str::to_string)
                        .or_else(|| meta.as_ref().and_then(|m| m.path.clone()));
                    if let Some(p) = &path {
                        let hunks = tur["structuredPatch"].as_array().filter(|h| !h.is_empty());
                        detail.unified_diff = match (hunks, tur["type"].as_str(), tur["content"].as_str()) {
                            (Some(h), _, _) => Some(unified_diff(&self.cwd, p, h)),
                            (None, Some("create"), Some(c)) => Some(created_diff(&self.cwd, p, c)),
                            _ => None,
                        };
                    }
                }
                ToolCategory::FileRead | ToolCategory::Search if !is_error && (name == "Glob" || name == "Grep") => {
                    detail.match_count = u64_of(tur, "numFiles").map(|n| n as u32);
                }
                ToolCategory::Subagent if !is_error && tur["status"].as_str() == Some("async_launched") => {
                    detail.background = Some(true);
                    self.backgrounded.insert(id.clone());
                }
                _ => {}
            }
            let detail = if detail == ToolResultDetail::default() { None } else { Some(detail) };
            Self::push(out, Event::ToolResult { id, output, is_error, detail });

            if category == ToolCategory::FileEdit && !is_error {
                let path =
                    tur["filePath"].as_str().map(str::to_string).or_else(|| meta.as_ref().and_then(|m| m.path.clone()));
                if let Some(path) = path {
                    let kind = if tur["type"].as_str() == Some("create") {
                        FileChangeKind::Created
                    } else {
                        FileChangeKind::Modified
                    };
                    Self::push(out, Event::FileChanged { path, kind });
                }
            }
        }
    }

    // ---------- result ----------

    fn on_result(&mut self, v: &Value, out: &mut Vec<Out>) {
        let subtype = v["subtype"].as_str().unwrap_or("");
        let is_error = v["is_error"].as_bool().unwrap_or(false);
        let cancelled = self.cancel_requested && (is_error || subtype != "success");

        let mut reason = match subtype {
            "success" if !is_error => TurnDoneReason::Completed,
            "error_max_turns" => TurnDoneReason::MaxTurns,
            "error_max_budget_usd" => TurnDoneReason::Budget,
            _ => TurnDoneReason::Error,
        };
        if cancelled {
            reason = TurnDoneReason::Cancelled;
        }

        if reason == TurnDoneReason::Error {
            let status = u64_of(v, "api_error_status");
            let code = match status {
                Some(401) | Some(403) => "auth_failed",
                Some(429) => "rate_limited",
                _ if subtype == "error_max_structured_output_retries" => "structured_output_failed",
                _ => "agent_error",
            };
            let message = v["result"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    v["errors"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; "))
                        .filter(|s| !s.is_empty())
                })
                .unwrap_or_else(|| format!("Claude Code reported {subtype}"));
            Self::push(out, Event::Error { code: code.to_string(), message, recoverable: code != "auth_failed" });
        }

        if !self.busy {
            // 没有进行中的 turn（如 `--resume` 一个不存在的会话）：只报错，不发 TurnDone。
            return;
        }

        let u = &v["usage"];
        let total = v["total_cost_usd"].as_f64();
        let cost = total.map(|t| {
            let delta = ((t - self.prev_total_cost).max(0.0) * 1e9).round() / 1e9;
            if t > self.prev_total_cost {
                self.prev_total_cost = t;
            }
            delta
        });
        let usage = Usage {
            input_tokens: u64_of(u, "input_tokens"),
            output_tokens: u64_of(u, "output_tokens"),
            cache_read_tokens: u64_of(u, "cache_read_input_tokens"),
            cache_write_tokens: u64_of(u, "cache_creation_input_tokens"),
            cost_usd: cost,
            duration_ms: u64_of(v, "duration_ms").unwrap_or(0),
            model: self.model.clone(),
        };
        self.busy = false;
        self.cancel_requested = false;
        Self::push(out, Event::TurnDone { turn_id: self.turn_counter, reason, usage: Some(usage) });
    }

    // ---------- control ----------

    fn on_control_request(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(request_id) = v.get("request_id").and_then(Value::as_str) else { return };
        let req = &v["request"];
        match req.get("subtype").and_then(Value::as_str) {
            Some("can_use_tool") => {
                let (pending, native) = permissions::from_can_use_tool(request_id, req, &self.cwd);
                out.push(Out::Permission { pending, native });
            }
            other => out.push(Out::Unsupported {
                request_id: request_id.to_string(),
                subtype: other.unwrap_or("").to_string(),
            }),
        }
    }

    fn on_control_response(&mut self, v: &Value, out: &mut Vec<Out>) {
        let r = &v["response"];
        let Some(request_id) = r.get("request_id").and_then(Value::as_str) else { return };
        let ok = r.get("subtype").and_then(Value::as_str) == Some("success");
        out.push(Out::ControlAck(ControlAck {
            request_id: request_id.to_string(),
            ok,
            response: r.get("response").cloned().unwrap_or(Value::Null),
            error: r.get("error").and_then(Value::as_str).map(str::to_string),
            error_code: r.get("error_code").and_then(Value::as_str).map(str::to_string),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_parsing() {
        assert_eq!(parse_exit_code("Exit code 2\nboom"), Some(2));
        assert_eq!(parse_exit_code("Exit code 127"), Some(127));
        assert_eq!(parse_exit_code("something else"), None);
    }

    #[test]
    fn output_extraction() {
        assert_eq!(tool_output(&json!("hi")).0, json!("hi"));
        assert_eq!(
            tool_output(&json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])).0,
            json!("a\nb")
        );
        let with_image = json!([{"type": "text", "text": "a"}, {"type": "image", "source": {}}]);
        assert_eq!(tool_output(&with_image).0, with_image);
        assert_eq!(tool_output(&Value::Null).0, json!(""));
    }

    #[test]
    fn image_base64_never_reaches_the_event_or_the_plain_text() {
        let content = json!([
            {"type": "text", "text": "see below"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUg=="}},
        ]);
        let (output, text) = tool_output(&content);
        assert_eq!(text, "see below");
        assert_eq!(
            output,
            json!([
                {"type": "text", "text": "see below"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "[omitted: 24 bytes of base64]"}},
            ])
        );
        assert!(!output.to_string().contains("iVBOR"));
    }

    #[test]
    fn big_output_is_truncated_on_char_boundary() {
        let big = "é".repeat(MAX_TOOL_OUTPUT_BYTES);
        let (v, _) = tool_output(&json!(big));
        let s = v.as_str().unwrap();
        assert!(s.len() < MAX_TOOL_OUTPUT_BYTES + 64);
        assert!(s.contains("[truncated"));
    }

    #[test]
    fn diff_headers_are_relative_to_cwd() {
        let d = unified_diff(
            "/proj",
            "/proj/src/a.rs",
            &[json!({"oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1, "lines": ["-a", "+b"]})],
        );
        assert_eq!(d, "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n");
        let c = created_diff("/proj", "/proj/new.txt", "x\ny\n");
        assert_eq!(c, "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+x\n+y\n");
    }
}
