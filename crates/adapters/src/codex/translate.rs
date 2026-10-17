//! app-server 的通知 / 服务端请求 → [`AgentEvent`]（docs/spec/daemon.md § 8 的 Codex 一条）。
//!
//! 纯同步状态机：`feed_*` 进、`Vec<Out>` 出，不碰进程和 IO，所以可以直接拿录制的 JSONL 回放测试。要点：
//! - 未知通知、未知 item 类型丢弃并 `debug!`；`warning` / `configWarning` 等只进日志
//! - `item/started` → `tool_call`（命令 / 补丁 / MCP / 搜索…），`item/commandExecution/outputDelta` → `tool_output_delta`，
//!   `item/completed` → `tool_result`（`declined` / `failed` / 退出码非 0 视为 `is_error`；补丁另发 `file_changed`）
//! - 文本 / 思考按 item 流式：`item/agentMessage/delta`、`item/reasoning/summaryTextDelta` + `textDelta`；
//!   摘要的第 2 段起前面补一个空行；没有任何文本的思考块发一条空的 `thinking_delta`（和 Claude Code 一致）
//! - **被打断 / 失败的 turn 不会为进行中的 item 发 `item/completed`**：`turn/completed` 时由这里补上 `text_done` /
//!   `thinking_done` / 带 `is_error` 的 `tool_result`，否则客户端的转圈停不下来
//! - `turn/completed` → `turn_done`（`interrupted` → `cancelled`，`failed` → `error`，失败时先发一条 `error` 事件，除非
//!   `error` 通知已经发过）；用量是这个 turn 里每次 `thread/tokenUsage/updated` 的 `last` 之和（`total` 是整个线程的累计）。
//!   `input_tokens` 不含缓存命中（OpenAI 的 `inputTokens` 包含 `cachedInputTokens`，这里减掉，与 Claude Code 的口径一致，
//!   这样 `input + cache_read` 不会重复计数），没有费用
//! - `error { willRetry: true }`（重连中）发一条可恢复的 `error` 事件（`agent_reconnecting`），不收尾；`willRetry: false`
//!   发 `error` 事件，随后的 `turn/completed { status: failed }` 收尾
//! - 审批 / 提问请求 → `Out::Permission`（见 `permissions.rs`）；不支持的服务端请求按 schema 回拒绝并发一条可见的 `error`
//!   （`unsupported_request`）；会话正在取消时新来的审批直接回 `cancel`

use std::collections::HashMap;
use std::time::Instant;

use anycon_agent_api::{AgentEvent, PendingPermission, PermissionDecision};
use anycon_protocol::{Event, FileChangeKind, PlanEntry, PlanStatus, ToolResultDetail, TurnDoneReason, Usage};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::permissions::{self, NativePending};
use super::rpc::{self, Incoming, RpcError};
use super::tools::{self, ToolCallParts};
use super::wire::{
    CommandApproval, DeltaNotification, ErrorNotification, FileApproval, ItemNotification, ItemStatus, ModelRerouted,
    PatchChangeKind, PermissionsApproval, PlanUpdated, ServerRequestResolved, SummaryPartAdded, ThreadItem,
    TokenUsageNotification, Turn, TurnError, TurnNotification, TurnStatus, UserInputRequest,
};

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Event(AgentEvent),
    /// 权限请求：`native` 由 handle 记下，答复时用。
    Permission {
        pending: PendingPermission,
        native: NativePending,
    },
    /// 直接答复一个服务端请求（不支持的、解析不了的、取消中的）。
    Reply {
        id: Value,
        result: Result<Value, RpcError>,
    },
    /// app-server 说某个请求已经了结（`serverRequest/resolved`）：handle 可以丢掉对应的未决项。
    Resolved {
        rpc_id: Value,
    },
    /// `turn/started`：codex 的 turn id。
    TurnStarted {
        codex_turn_id: String,
    },
}

/// 进程退出时的收尾结果。
pub struct Finish {
    pub out: Vec<Out>,
    /// 退出时 turn 还在进行，且不是我们要求的取消——上层应补一个 `Error` 事件。
    pub died_mid_turn: bool,
    /// 退出前我们已经要求了取消（interrupt / shutdown），所以这次退出是预期内的。
    pub cancelled: bool,
}

struct TextState {
    emitted: bool,
}

struct ThinkState {
    started: Instant,
    emitted: bool,
    /// 下一段摘要前要补一个空行。
    separate_next: bool,
}

#[derive(Default)]
struct UsageAcc {
    seen: bool,
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

pub struct Translator {
    cwd: String,
    turn_counter: u64,
    busy: bool,
    cancel_requested: bool,
    codex_turn: Option<String>,
    turn_started: Option<Instant>,
    model: Option<String>,
    usage: UsageAcc,
    error_emitted: bool,
    texts: HashMap<String, TextState>,
    text_order: Vec<String>,
    thinks: HashMap<String, ThinkState>,
    think_order: Vec<String>,
    tools: HashMap<String, ToolCallParts>,
    tool_order: Vec<String>,
}

fn push(out: &mut Vec<Out>, e: Event) {
    out.push(Out::Event(AgentEvent::Event(e)));
}

fn item_id(item: &ThreadItem) -> Option<&str> {
    match item {
        ThreadItem::AgentMessage { id, .. }
        | ThreadItem::Reasoning { id, .. }
        | ThreadItem::Plan { id, .. }
        | ThreadItem::CommandExecution { id, .. }
        | ThreadItem::FileChange { id, .. }
        | ThreadItem::McpToolCall { id, .. }
        | ThreadItem::DynamicToolCall { id, .. }
        | ThreadItem::CollabAgentToolCall { id, .. }
        | ThreadItem::WebSearch { id, .. }
        | ThreadItem::ImageView { id, .. }
        | ThreadItem::ImageGeneration { id, .. } => Some(id.as_str()),
        ThreadItem::UserMessage {} | ThreadItem::Other => None,
    }
}

fn plan_status(s: &str) -> PlanStatus {
    match s {
        "completed" => PlanStatus::Completed,
        "inProgress" | "in_progress" => PlanStatus::InProgress,
        _ => PlanStatus::Pending,
    }
}

/// `TurnError` → `(事件 code, recoverable)`。
pub fn error_code(e: &TurnError) -> (&'static str, bool) {
    let lower = e.message.to_ascii_lowercase();
    match e.info_name().as_deref() {
        Some("unauthorized") => ("auth_failed", false),
        Some("usageLimitExceeded" | "rateLimitExceeded" | "serverOverloaded" | "sessionBudgetExceeded") => {
            ("rate_limited", true)
        }
        Some("contextWindowExceeded") => ("context_window_exceeded", true),
        Some("cyberPolicy" | "misalignmentPolicyViolation") => ("policy_violation", true),
        Some("sandboxError") => ("sandbox_error", true),
        _ => match e.http_status() {
            Some(401 | 403) => ("auth_failed", false),
            Some(429) => ("rate_limited", true),
            _ if lower.contains("401 unauthorized") || lower.contains("not logged in") => ("auth_failed", false),
            _ if lower.contains("429 too many requests") => ("rate_limited", true),
            _ => ("agent_error", true),
        },
    }
}

fn error_message(e: &TurnError) -> String {
    let mut m = if e.message.trim().is_empty() { "Codex reported an error".to_string() } else { e.message.clone() };
    if let Some(d) = e.additional_details.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        m.push_str(&format!(" ({d})"));
    }
    m
}

fn parse<T: DeserializeOwned>(method: &str, params: &Value) -> Option<T> {
    match serde_json::from_value::<T>(params.clone()) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!(method, error = %e, "codex: ignoring a message with an unexpected shape");
            None
        }
    }
}

impl Translator {
    pub fn new(cwd: impl Into<String>, model: Option<String>) -> Self {
        Self {
            cwd: cwd.into(),
            turn_counter: 0,
            busy: false,
            cancel_requested: false,
            codex_turn: None,
            turn_started: None,
            model,
            usage: UsageAcc::default(),
            error_emitted: false,
            texts: HashMap::new(),
            text_order: Vec::new(),
            thinks: HashMap::new(),
            think_order: Vec::new(),
            tools: HashMap::new(),
            tool_order: Vec::new(),
        }
    }

    /// `send` 之前调用：开始新 turn。
    pub fn begin_turn(&mut self) {
        self.turn_counter += 1;
        self.busy = true;
        self.cancel_requested = false;
        self.codex_turn = None;
        self.turn_started = Some(Instant::now());
        self.usage = UsageAcc::default();
        self.error_emitted = false;
        // item 不会跨 turn，清掉各种表，长期保活的会话里不会无限增长
        self.texts.clear();
        self.text_order.clear();
        self.thinks.clear();
        self.think_order.clear();
        self.tools.clear();
        self.tool_order.clear();
    }

    /// `turn/start` 失败了：这个 turn 没有开始。
    pub fn abort_turn(&mut self) {
        self.busy = false;
        self.codex_turn = None;
    }

    /// 我们发出了 interrupt（或要关闭进程）：此后 turn 按 cancelled 算，新来的审批直接回 `cancel`。
    pub fn mark_cancel_requested(&mut self) {
        if self.busy {
            self.cancel_requested = true;
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn is_cancelling(&self) -> bool {
        self.cancel_requested
    }

    pub fn turn_id(&self) -> u64 {
        self.turn_counter
    }

    pub fn codex_turn_id(&self) -> Option<&str> {
        self.codex_turn.as_deref()
    }

    /// `turn/start` 的响应带回 turn id；`turn/started` 通知可能更晚才到。
    pub fn set_codex_turn_id(&mut self, id: &str) {
        if self.busy && self.codex_turn.is_none() {
            self.codex_turn = Some(id.to_string());
        }
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    pub fn feed_line(&mut self, line: &str) -> Vec<Out> {
        match rpc::classify(line) {
            Some(i) => self.feed_incoming(i),
            None => Vec::new(),
        }
    }

    pub fn feed_message(&mut self, v: &Value) -> Vec<Out> {
        match rpc::classify_value(v) {
            Some(i) => self.feed_incoming(i),
            None => Vec::new(),
        }
    }

    pub fn feed_incoming(&mut self, i: Incoming) -> Vec<Out> {
        match i {
            Incoming::Notification { method, params } => self.feed_notification(&method, &params),
            Incoming::Request { id, method, params } => self.feed_request(id, &method, &params),
            // 响应归 `RpcClient`
            Incoming::Response { .. } => Vec::new(),
        }
    }

    /// 进程退出（stdout 关闭）时调用。
    pub fn finish(&mut self) -> Finish {
        let mut out = Vec::new();
        let cancelled = self.cancel_requested;
        let mut died = false;
        if self.busy {
            if self.cancel_requested {
                self.close_open_blocks(&mut out, "Interrupted");
                self.busy = false;
                push(
                    &mut out,
                    Event::TurnDone { turn_id: self.turn_counter, reason: TurnDoneReason::Cancelled, usage: None },
                );
            } else {
                died = true;
                self.busy = false;
            }
        }
        Finish { out, died_mid_turn: died, cancelled }
    }

    // ---------- 通知 ----------

    pub fn feed_notification(&mut self, method: &str, params: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        match method {
            "turn/started" => {
                if let Some(n) = parse::<TurnNotification>(method, params) {
                    self.on_turn_started(&n.turn, &mut out);
                }
            }
            "turn/completed" => {
                if let Some(n) = parse::<TurnNotification>(method, params) {
                    self.on_turn_completed(&n.turn, &mut out);
                }
            }
            "item/started" => {
                if let Some(n) = parse::<ItemNotification>(method, params) {
                    self.on_item(&n.item, false, &mut out);
                }
            }
            "item/completed" => {
                if let Some(n) = parse::<ItemNotification>(method, params) {
                    self.on_item(&n.item, true, &mut out);
                }
            }
            "item/agentMessage/delta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    self.on_text_delta(n, &mut out);
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    self.on_thinking_delta(n, &mut out);
                }
            }
            "item/reasoning/summaryPartAdded" => {
                if let Some(n) = parse::<SummaryPartAdded>(method, params) {
                    if n.summary_index > 0 {
                        if let Some(t) = self.thinks.get_mut(&n.item_id) {
                            t.separate_next = t.emitted;
                        }
                    }
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    if !n.delta.is_empty() {
                        push(&mut out, Event::ToolOutputDelta { id: n.item_id, text: n.delta });
                    }
                }
            }
            "turn/plan/updated" => {
                if let Some(n) = parse::<PlanUpdated>(method, params) {
                    let entries = n
                        .plan
                        .into_iter()
                        .map(|s| PlanEntry { content: s.step, status: plan_status(&s.status) })
                        .collect();
                    push(&mut out, Event::Plan { entries });
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(n) = parse::<TokenUsageNotification>(method, params) {
                    self.on_token_usage(&n);
                }
            }
            "error" => {
                if let Some(n) = parse::<ErrorNotification>(method, params) {
                    self.on_error(&n, &mut out);
                }
            }
            "serverRequest/resolved" => {
                if let Some(n) = parse::<ServerRequestResolved>(method, params) {
                    out.push(Out::Resolved { rpc_id: n.request_id });
                }
            }
            "model/rerouted" => {
                if let Some(n) = parse::<ModelRerouted>(method, params) {
                    if let Some(m) = n.to_model {
                        tracing::info!(model = %m, "codex: the model was rerouted");
                        self.model = Some(m);
                    }
                }
            }
            "warning" | "configWarning" | "guardianWarning" | "deprecationNotice" => {
                tracing::warn!(method, params = %params, "codex: notice from app-server");
            }
            other => tracing::debug!(method = other, "codex: ignoring a notification"),
        }
        out
    }

    fn on_turn_started(&mut self, turn: &Turn, out: &mut Vec<Out>) {
        if !self.busy {
            tracing::debug!(turn = %turn.id, "codex: a turn started that we did not start");
        }
        if self.codex_turn.is_none() {
            self.codex_turn = Some(turn.id.clone());
        }
        out.push(Out::TurnStarted { codex_turn_id: turn.id.clone() });
    }

    fn on_turn_completed(&mut self, turn: &Turn, out: &mut Vec<Out>) {
        if !self.busy {
            tracing::debug!(turn = %turn.id, "codex: a turn completed that we did not start");
            return;
        }
        if self.codex_turn.as_deref().is_some_and(|cur| cur != turn.id) {
            tracing::debug!(turn = %turn.id, current = ?self.codex_turn, "codex: ignoring the completion of another turn");
            return;
        }
        self.close_open_blocks(out, "The turn ended before this finished");

        let mut reason = match turn.status {
            TurnStatus::Completed => TurnDoneReason::Completed,
            TurnStatus::Interrupted => TurnDoneReason::Cancelled,
            TurnStatus::Failed | TurnStatus::Unknown | TurnStatus::InProgress => TurnDoneReason::Error,
        };
        if reason == TurnDoneReason::Error && self.cancel_requested {
            reason = TurnDoneReason::Cancelled;
        }
        if reason == TurnDoneReason::Error && !self.error_emitted {
            let (code, recoverable, message) = match &turn.error {
                Some(e) => {
                    let (c, r) = error_code(e);
                    (c, r, error_message(e))
                }
                None => ("agent_error", true, "Codex ended the turn with an error".to_string()),
            };
            push(out, Event::Error { code: code.to_string(), message, recoverable });
        }

        let u = &self.usage;
        let elapsed = self.turn_started.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
        let usage = Usage {
            input_tokens: u.seen.then(|| u.input.saturating_sub(u.cached)),
            output_tokens: u.seen.then_some(u.output),
            cache_read_tokens: u.seen.then_some(u.cached),
            cache_write_tokens: (u.cache_write > 0).then_some(u.cache_write),
            cost_usd: None,
            duration_ms: turn.duration_ms.filter(|d| *d >= 0).map(|d| d as u64).unwrap_or(elapsed),
            model: self.model.clone(),
        };
        self.busy = false;
        self.cancel_requested = false;
        self.codex_turn = None;
        push(out, Event::TurnDone { turn_id: self.turn_counter, reason, usage: Some(usage) });
    }

    /// turn 结束时还没收尾的块：文本 / 思考补 done，工具调用补一条出错的结果。
    fn close_open_blocks(&mut self, out: &mut Vec<Out>, why: &str) {
        for id in std::mem::take(&mut self.text_order) {
            if let Some(t) = self.texts.remove(&id) {
                if t.emitted {
                    push(out, Event::TextDone {});
                }
            }
        }
        for id in std::mem::take(&mut self.think_order) {
            if let Some(t) = self.thinks.remove(&id) {
                if t.emitted {
                    push(out, Event::ThinkingDone { duration_ms: t.started.elapsed().as_millis() as u64 });
                }
            }
        }
        for id in std::mem::take(&mut self.tool_order) {
            if self.tools.remove(&id).is_some() {
                push(
                    out,
                    Event::ToolResult { id, output: Value::String(why.to_string()), is_error: true, detail: None },
                );
            }
        }
    }

    fn on_token_usage(&mut self, n: &TokenUsageNotification) {
        if !self.busy {
            return;
        }
        // resume 时 app-server 会在响应之后立刻重放上一个 turn 的用量（可能和我们的下一个 `turn/start` 赛跑）：
        // 只认「当前 turn」的。用量总是在 `turn/started` 之后才来，所以此时 turn id 一定已知
        if let Some(tid) = &n.turn_id {
            if self.codex_turn.as_deref() != Some(tid.as_str()) {
                return;
            }
        }
        let l = &n.token_usage.last;
        self.usage.seen = true;
        self.usage.input += l.input_tokens;
        self.usage.cached += l.cached_input_tokens;
        self.usage.cache_write += l.cache_write_input_tokens;
        self.usage.output += l.output_tokens;
    }

    fn on_error(&mut self, n: &ErrorNotification, out: &mut Vec<Out>) {
        let message = error_message(&n.error);
        if n.will_retry {
            push(out, Event::Error { code: "agent_reconnecting".to_string(), message, recoverable: true });
            return;
        }
        let (code, recoverable) = error_code(&n.error);
        if self.busy {
            self.error_emitted = true;
        }
        push(out, Event::Error { code: code.to_string(), message, recoverable });
    }

    // ---------- 文本 / 思考 ----------

    fn text_state(&mut self, id: &str) -> &mut TextState {
        if !self.texts.contains_key(id) {
            self.text_order.push(id.to_string());
            self.texts.insert(id.to_string(), TextState { emitted: false });
        }
        self.texts.get_mut(id).expect("just inserted")
    }

    fn think_state(&mut self, id: &str) -> &mut ThinkState {
        if !self.thinks.contains_key(id) {
            self.think_order.push(id.to_string());
            self.thinks
                .insert(id.to_string(), ThinkState { started: Instant::now(), emitted: false, separate_next: false });
        }
        self.thinks.get_mut(id).expect("just inserted")
    }

    fn on_text_delta(&mut self, n: DeltaNotification, out: &mut Vec<Out>) {
        if n.delta.is_empty() {
            return;
        }
        self.text_state(&n.item_id).emitted = true;
        push(out, Event::TextDelta { text: n.delta });
    }

    fn on_thinking_delta(&mut self, n: DeltaNotification, out: &mut Vec<Out>) {
        if n.delta.is_empty() {
            return;
        }
        let t = self.think_state(&n.item_id);
        let mut text = n.delta;
        if t.separate_next {
            text.insert_str(0, "\n\n");
            t.separate_next = false;
        }
        t.emitted = true;
        push(out, Event::ThinkingDelta { text });
    }

    // ---------- item ----------

    fn on_item(&mut self, raw: &Value, completed: bool, out: &mut Vec<Out>) {
        let item = match serde_json::from_value::<ThreadItem>(raw.clone()) {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!(error = %e, item_type = ThreadItem::raw_type(raw), "codex: ignoring an item with an unexpected shape");
                return;
            }
        };
        match &item {
            ThreadItem::AgentMessage { id, text } => {
                if completed {
                    self.finish_text(id, text, out);
                } else {
                    self.text_state(id);
                }
            }
            ThreadItem::Reasoning { id, summary, content } => {
                if completed {
                    self.finish_thinking(id, summary, content, out);
                } else {
                    self.think_state(id);
                }
            }
            ThreadItem::UserMessage {} | ThreadItem::Plan { .. } => {}
            ThreadItem::Other => {
                tracing::debug!(item_type = ThreadItem::raw_type(raw), "codex: ignoring an item of an unknown type");
            }
            tool => self.on_tool_item(tool, completed, out),
        }
    }

    fn finish_text(&mut self, id: &str, text: &str, out: &mut Vec<Out>) {
        let state = self.texts.remove(id);
        self.text_order.retain(|x| x != id);
        let emitted = state.is_some_and(|s| s.emitted);
        if !emitted && !text.is_empty() {
            // 没有收到过 delta（比如整段一次给）：补发
            push(out, Event::TextDelta { text: text.to_string() });
            push(out, Event::TextDone {});
        } else if emitted {
            push(out, Event::TextDone {});
        }
    }

    fn finish_thinking(&mut self, id: &str, summary: &[String], content: &[String], out: &mut Vec<Out>) {
        let state = self.thinks.remove(id);
        self.think_order.retain(|x| x != id);
        let (started, emitted) = match &state {
            Some(s) => (s.started, s.emitted),
            None => (Instant::now(), false),
        };
        if !emitted {
            let joined = if !summary.is_empty() { summary.join("\n\n") } else { content.join("\n\n") };
            push(out, Event::ThinkingDelta { text: joined });
        }
        push(out, Event::ThinkingDone { duration_ms: started.elapsed().as_millis() as u64 });
    }

    fn on_tool_item(&mut self, item: &ThreadItem, completed: bool, out: &mut Vec<Out>) {
        let Some(id) = item_id(item).map(str::to_string) else { return };
        if !self.tools.contains_key(&id) {
            let Some(parts) = tools::describe_item(item, &self.cwd) else { return };
            push(
                out,
                Event::ToolCall {
                    id: id.clone(),
                    name: parts.name.clone(),
                    category: parts.category,
                    title: parts.title.clone(),
                    input: parts.input.clone(),
                    detail: parts.detail.clone(),
                },
            );
            self.tool_order.push(id.clone());
            self.tools.insert(id.clone(), parts);
        }
        if !completed {
            return;
        }
        self.tools.remove(&id);
        self.tool_order.retain(|x| x != &id);
        self.push_tool_result(&id, item, out);
    }

    fn push_tool_result(&self, id: &str, item: &ThreadItem, out: &mut Vec<Out>) {
        let id = id.to_string();
        match item {
            ThreadItem::CommandExecution { status, aggregated_output, exit_code, .. } => {
                let mut text = tools::clip_text(aggregated_output.as_deref().unwrap_or(""));
                let declined = *status == ItemStatus::Declined;
                if declined && text.is_empty() {
                    text = "Declined by the user".to_string();
                }
                let is_error =
                    matches!(status, ItemStatus::Failed | ItemStatus::Declined) || exit_code.is_some_and(|c| c != 0);
                let detail = exit_code.map(|c| ToolResultDetail { exit_code: Some(c), ..Default::default() });
                push(out, Event::ToolResult { id, output: Value::String(text), is_error, detail });
            }
            ThreadItem::FileChange { status, changes, .. } => {
                let is_error = matches!(status, ItemStatus::Failed | ItemStatus::Declined);
                let applied = *status == ItemStatus::Completed;
                let output = match status {
                    ItemStatus::Declined => "Declined by the user".to_string(),
                    ItemStatus::Failed => "The patch could not be applied".to_string(),
                    _ => changes
                        .iter()
                        .map(|c| {
                            let p = tools::rel_path(&self.cwd, &c.path);
                            match &c.kind {
                                PatchChangeKind::Add => format!("Created {p}"),
                                PatchChangeKind::Delete => format!("Deleted {p}"),
                                PatchChangeKind::Update { move_path: Some(m) } if !m.is_empty() => {
                                    format!("Moved {p} to {}", tools::rel_path(&self.cwd, m))
                                }
                                _ => format!("Modified {p}"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                let diff =
                    (!changes.is_empty() && !is_error).then(|| tools::file_change_unified_diff(&self.cwd, changes));
                let detail = diff.map(|d| ToolResultDetail { unified_diff: Some(d), ..Default::default() });
                push(out, Event::ToolResult { id, output: Value::String(output), is_error, detail });
                if applied {
                    for c in changes {
                        match &c.kind {
                            PatchChangeKind::Add => {
                                push(out, Event::FileChanged { path: c.path.clone(), kind: FileChangeKind::Created })
                            }
                            PatchChangeKind::Delete => {
                                push(out, Event::FileChanged { path: c.path.clone(), kind: FileChangeKind::Deleted })
                            }
                            PatchChangeKind::Update { move_path: Some(m) } if !m.is_empty() => {
                                push(out, Event::FileChanged { path: c.path.clone(), kind: FileChangeKind::Deleted });
                                push(out, Event::FileChanged { path: m.clone(), kind: FileChangeKind::Created });
                            }
                            _ => push(out, Event::FileChanged { path: c.path.clone(), kind: FileChangeKind::Modified }),
                        }
                    }
                }
            }
            ThreadItem::McpToolCall { status, result, error, .. } => {
                let err_text = error.as_ref().map(|e| {
                    e.get("message").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| e.to_string())
                });
                let is_error = *status == ItemStatus::Failed || err_text.is_some();
                let output = match (&err_text, result) {
                    (Some(e), _) => Value::String(e.clone()),
                    (None, Some(r)) => tools::mcp_output(r),
                    (None, None) => Value::String(String::new()),
                };
                push(out, Event::ToolResult { id, output, is_error, detail: None });
            }
            ThreadItem::DynamicToolCall { status, content_items, success, .. } => {
                let is_error = *status == ItemStatus::Failed || *success == Some(false);
                let text = content_items.as_deref().map(tools::dynamic_output).unwrap_or_default();
                push(out, Event::ToolResult { id, output: Value::String(text), is_error, detail: None });
            }
            ThreadItem::CollabAgentToolCall { status, agents_states, .. } => {
                let is_error = matches!(status, ItemStatus::Failed);
                let output = if agents_states.is_null() { Value::String(String::new()) } else { agents_states.clone() };
                push(out, Event::ToolResult { id, output, is_error, detail: None });
            }
            ThreadItem::ImageGeneration { status, saved_path, .. } => {
                let is_error = status.as_deref() == Some("failed");
                let text = saved_path.clone().unwrap_or_else(|| "Image generated".to_string());
                push(out, Event::ToolResult { id, output: Value::String(text), is_error, detail: None });
            }
            _ => {
                push(out, Event::ToolResult { id, output: Value::String(String::new()), is_error: false, detail: None })
            }
        }
    }

    // ---------- 超长的行 ----------

    /// 一条超长的通知被丢弃了（`prefix` 是它的前几 KiB，`len` 是整行长度）。`item/started` / `item/completed` 的
    /// 补丁整份 diff、MCP 的大结果会这样：这里用前缀里读得到的 `type` / `id`（和第一个文件的路径等）补出一个「内容没显示」的
    /// `tool_call` / `tool_result`，客户端的转圈才停得下来，也不会把 turn 收尾时才补的「出错」安到一个其实成功了的调用头上。
    /// 其它通知（`turn/diff/updated` 等）直接丢。
    pub fn feed_oversize(&mut self, method: &str, prefix: &str, len: usize, limit: usize) -> Vec<Out> {
        let mut out = Vec::new();
        match method {
            "item/started" | "item/completed" => match rpc::oversize_item(prefix) {
                Some((kind, id)) => {
                    self.on_oversize_item(&kind, &id, prefix, method == "item/completed", len, limit, &mut out)
                }
                None => {
                    tracing::warn!(method, len, "codex: dropped an oversize item notification we could not identify")
                }
            },
            other => tracing::warn!(method = other, len, "codex: dropped an oversize notification"),
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn on_oversize_item(
        &mut self,
        kind: &str,
        id: &str,
        prefix: &str,
        completed: bool,
        len: usize,
        limit: usize,
        out: &mut Vec<Out>,
    ) {
        match kind {
            "agentMessage" => {
                if completed {
                    self.finish_text(id, "", out);
                } else {
                    self.text_state(id);
                }
                return;
            }
            "reasoning" => {
                if completed {
                    self.finish_thinking(id, &[], &[], out);
                } else {
                    self.think_state(id);
                }
                return;
            }
            _ => {}
        }
        let note = format!("{len} bytes, over the {limit}-byte limit");
        if !self.tools.contains_key(id) {
            let s = |k: &str| rpc::string_at(prefix, &[k]);
            let item = match kind {
                "fileChange" => {
                    let change_kind = rpc::string_at(prefix, &["kind", "type"]).unwrap_or_else(|| "update".into());
                    let changes = match s("path") {
                        Some(path) => json!([{ "path": path, "kind": { "type": change_kind }, "diff": "" }]),
                        None => json!([]),
                    };
                    json!({ "type": kind, "id": id, "changes": changes })
                }
                "commandExecution" => {
                    json!({ "type": kind, "id": id, "command": s("command").unwrap_or_default(), "cwd": s("cwd") })
                }
                "mcpToolCall" => {
                    json!({ "type": kind, "id": id, "server": s("server").unwrap_or_default(), "tool": s("tool").unwrap_or_default() })
                }
                "webSearch" => json!({ "type": kind, "id": id, "query": s("query").unwrap_or_default() }),
                _ => json!({ "type": kind, "id": id }),
            };
            let Some(mut parts) =
                serde_json::from_value::<ThreadItem>(item).ok().and_then(|i| tools::describe_item(&i, &self.cwd))
            else {
                tracing::warn!(kind, len, "codex: dropped an oversize item of a kind we do not show");
                return;
            };
            parts.title = format!("{} (details too large to show)", parts.title);
            if let Value::Object(m) = &mut parts.input {
                m.insert("omitted".into(), json!(format!("the item was {note}")));
            }
            push(
                out,
                Event::ToolCall {
                    id: id.to_string(),
                    name: parts.name.clone(),
                    category: parts.category,
                    title: parts.title.clone(),
                    input: parts.input.clone(),
                    detail: parts.detail.clone(),
                },
            );
            self.tool_order.push(id.to_string());
            self.tools.insert(id.to_string(), parts);
        }
        if completed && self.tools.remove(id).is_some() {
            self.tool_order.retain(|x| x != id);
            push(
                out,
                Event::ToolResult {
                    id: id.to_string(),
                    output: Value::String(format!("[result not shown: the message was {note}]")),
                    is_error: false,
                    detail: None,
                },
            );
        }
    }

    // ---------- 服务端请求 ----------

    pub fn feed_request(&mut self, id: Value, method: &str, params: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let request_id = uuid::Uuid::new_v4().to_string();
        let built: Option<(PendingPermission, NativePending)> = match method {
            permissions::M_COMMAND_APPROVAL => parse::<CommandApproval>(method, params).map(|p| {
                let started = self.tools.get(&p.item_id);
                permissions::from_command(&request_id, id.clone(), &p, &self.cwd, started)
            }),
            permissions::M_FILE_APPROVAL => parse::<FileApproval>(method, params).map(|p| {
                let started = self.tools.get(&p.item_id);
                permissions::from_file_change(&request_id, id.clone(), &p, &self.cwd, started)
            }),
            permissions::M_PERMISSIONS_APPROVAL => parse::<PermissionsApproval>(method, params)
                .map(|p| permissions::from_permissions(&request_id, id.clone(), &p, &self.cwd)),
            permissions::M_USER_INPUT => parse::<UserInputRequest>(method, params)
                .map(|p| permissions::from_user_input(&request_id, id.clone(), &p)),
            other => {
                let u = permissions::unsupported(other);
                tracing::warn!(method = other, "codex: declining an unsupported server request");
                out.push(Out::Reply { id, result: u.result });
                push(
                    &mut out,
                    Event::Error { code: "unsupported_request".to_string(), message: u.message, recoverable: true },
                );
                return out;
            }
        };
        match built {
            None => {
                // 形状不对：不能挂着不回
                out.push(Out::Reply { id, result: Err(RpcError::new(-32602, "invalid params")) });
            }
            Some((pending, native)) => {
                if self.cancel_requested {
                    // 会话正在取消：别把新的审批摆到用户面前，直接 cancel
                    let result = permissions::build_response(&native, &PermissionDecision::default(), true)
                        .map_err(|e| RpcError::new(-32603, e.to_string()));
                    out.push(Out::Reply { id, result });
                } else {
                    out.push(Out::Permission { pending, native });
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use anycon_protocol::ToolCategory;
    use serde_json::json;

    use super::*;

    /// 构造一条通知。
    fn note(method: &str, params: Value) -> Value {
        json!({ "method": method, "params": params })
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

    fn feed(t: &mut Translator, method: &str, params: Value) -> Vec<Out> {
        t.feed_message(&note(method, params))
    }

    fn turn_started(t: &mut Translator, id: &str) {
        t.begin_turn();
        feed(t, "turn/started", json!({"threadId": "th", "turn": {"id": id, "status": "inProgress", "items": []}}));
    }

    fn completed(t: &mut Translator, id: &str, status: &str, extra: Value) -> Vec<Out> {
        let mut turn = json!({"id": id, "status": status, "items": []});
        if let (Value::Object(t), Value::Object(e)) = (&mut turn, extra) {
            t.extend(e);
        }
        feed(t, "turn/completed", json!({"threadId": "th", "turn": turn}))
    }

    #[test]
    fn text_streams_and_closes() {
        let mut t = Translator::new("/w", Some("m".into()));
        turn_started(&mut t, "t1");
        let mut all = Vec::new();
        all.extend(feed(&mut t, "item/started", json!({"item": {"type": "agentMessage", "id": "a", "text": ""}})));
        all.extend(feed(&mut t, "item/agentMessage/delta", json!({"itemId": "a", "delta": "Hel"})));
        all.extend(feed(&mut t, "item/agentMessage/delta", json!({"itemId": "a", "delta": "lo"})));
        all.extend(feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "agentMessage", "id": "a", "text": "Hello"}}),
        ));
        all.extend(completed(&mut t, "t1", "completed", json!({"durationMs": 42})));
        assert_eq!(names(&all), vec!["text_delta", "text_delta", "text_done", "turn_done"]);
        match events(&all).last().unwrap() {
            Event::TurnDone { turn_id, reason, usage } => {
                assert_eq!((*turn_id, *reason), (1, TurnDoneReason::Completed));
                let u = usage.as_ref().unwrap();
                assert_eq!((u.duration_ms, u.model.as_deref(), u.cost_usd), (42, Some("m"), None));
                assert_eq!(u.input_tokens, None);
            }
            other => panic!("{other:?}"),
        }
        assert!(!t.is_busy());
    }

    #[test]
    fn message_without_deltas_is_emitted_whole_on_completion() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs =
            feed(&mut t, "item/completed", json!({"item": {"type": "agentMessage", "id": "a", "text": "All at once"}}));
        assert_eq!(events(&outs), vec![Event::TextDelta { text: "All at once".into() }, Event::TextDone {}]);
        // 空消息什么都不发
        let outs = feed(&mut t, "item/completed", json!({"item": {"type": "agentMessage", "id": "b", "text": ""}}));
        assert!(events(&outs).is_empty());
    }

    #[test]
    fn reasoning_summary_parts_are_separated() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let mut all = Vec::new();
        all.extend(feed(
            &mut t,
            "item/started",
            json!({"item": {"type": "reasoning", "id": "r", "summary": [], "content": []}}),
        ));
        all.extend(feed(&mut t, "item/reasoning/summaryPartAdded", json!({"itemId": "r", "summaryIndex": 0})));
        all.extend(feed(
            &mut t,
            "item/reasoning/summaryTextDelta",
            json!({"itemId": "r", "delta": "first", "summaryIndex": 0}),
        ));
        all.extend(feed(&mut t, "item/reasoning/summaryPartAdded", json!({"itemId": "r", "summaryIndex": 1})));
        all.extend(feed(
            &mut t,
            "item/reasoning/summaryTextDelta",
            json!({"itemId": "r", "delta": "second", "summaryIndex": 1}),
        ));
        all.extend(feed(&mut t, "item/reasoning/textDelta", json!({"itemId": "r", "delta": "raw", "contentIndex": 0})));
        all.extend(feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "reasoning", "id": "r", "summary": ["first", "second"], "content": []}}),
        ));
        let evs = events(&all);
        assert_eq!(
            evs[..3],
            [
                Event::ThinkingDelta { text: "first".into() },
                Event::ThinkingDelta { text: "\n\nsecond".into() },
                Event::ThinkingDelta { text: "raw".into() }
            ]
        );
        assert!(matches!(evs[3], Event::ThinkingDone { .. }));
        assert_eq!(evs.len(), 4);
    }

    #[test]
    fn empty_reasoning_still_gets_one_empty_block() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let mut all = feed(
            &mut t,
            "item/started",
            json!({"item": {"type": "reasoning", "id": "r", "summary": [], "content": []}}),
        );
        all.extend(feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "reasoning", "id": "r", "summary": [], "content": []}}),
        ));
        let evs = events(&all);
        assert_eq!(evs[0], Event::ThinkingDelta { text: String::new() });
        assert!(matches!(evs[1], Event::ThinkingDone { .. }));
        // 只在 completed 里带了摘要（没有 delta）
        let outs = feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "reasoning", "id": "r2", "summary": ["a", "b"], "content": []}}),
        );
        assert_eq!(events(&outs)[0], Event::ThinkingDelta { text: "a\n\nb".into() });
    }

    #[test]
    fn command_lifecycle() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let item = |status: &str, extra: Value| {
            let mut i = json!({"type": "commandExecution", "id": "c1", "command": "/bin/zsh -lc 'ls'", "cwd": "/w",
                "status": status, "commandActions": [], "aggregatedOutput": null, "exitCode": null});
            if let (Value::Object(i), Value::Object(e)) = (&mut i, extra) {
                i.extend(e);
            }
            i
        };
        let mut all = feed(&mut t, "item/started", json!({"item": item("inProgress", json!({}))}));
        all.extend(feed(&mut t, "item/commandExecution/outputDelta", json!({"itemId": "c1", "delta": "a.txt\n"})));
        all.extend(feed(&mut t, "item/commandExecution/outputDelta", json!({"itemId": "c1", "delta": ""})));
        all.extend(feed(
            &mut t,
            "item/completed",
            json!({"item": item("completed", json!({"aggregatedOutput": "a.txt\n", "exitCode": 0}))}),
        ));
        let evs = events(&all);
        assert_eq!(names(&all), vec!["tool_call", "tool_output_delta", "tool_result"]);
        match &evs[0] {
            Event::ToolCall { id, name, category, title, detail, .. } => {
                assert_eq!(
                    (id.as_str(), name.as_str(), *category, title.as_str()),
                    ("c1", "commandExecution", ToolCategory::Shell, "ls")
                );
                assert_eq!(detail.as_ref().unwrap().command.as_deref(), Some("ls"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(evs[1], Event::ToolOutputDelta { id: "c1".into(), text: "a.txt\n".into() });
        match &evs[2] {
            Event::ToolResult { output, is_error, detail, .. } => {
                assert_eq!((output, *is_error), (&json!("a.txt\n"), false));
                assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failing_and_declined_commands_are_errors() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let done = |t: &mut Translator, id: &str, status: &str, code: Value, out: &str| {
            feed(
                t,
                "item/completed",
                json!({"item": {"type": "commandExecution", "id": id, "command": "x", "cwd": "/w",
                "status": status, "commandActions": [], "aggregatedOutput": out, "exitCode": code}}),
            )
        };
        let result = |outs: &[Out]| match events(outs).into_iter().find(|e| matches!(e, Event::ToolResult { .. })) {
            Some(Event::ToolResult { output, is_error, detail, .. }) => {
                (output, is_error, detail.and_then(|d| d.exit_code))
            }
            other => panic!("{other:?}"),
        };
        // 退出码非 0
        assert_eq!(result(&done(&mut t, "a", "completed", json!(2), "boom")), (json!("boom"), true, Some(2)));
        // 失败
        assert_eq!(result(&done(&mut t, "b", "failed", Value::Null, "")), (json!(""), true, None));
        // 被拒绝：给个说明
        assert_eq!(
            result(&done(&mut t, "c", "declined", Value::Null, "")),
            (json!("Declined by the user"), true, None)
        );
    }

    #[test]
    fn patch_lifecycle_emits_diff_and_file_changes() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let changes = json!([
            {"path": "/w/a.txt", "kind": {"type": "add"}, "diff": "x\n"},
            {"path": "/w/b.txt", "kind": {"type": "update", "move_path": null}, "diff": "@@ -1 +1 @@\n-a\n+b\n"},
            {"path": "/w/c.txt", "kind": {"type": "delete"}, "diff": "bye\n"},
            {"path": "/w/d.txt", "kind": {"type": "update", "move_path": "/w/e.txt"}, "diff": "@@ -1 +1 @@\n-1\n+2\n\n\nMoved to: /w/e.txt"}
        ]);
        let item = |status: &str| json!({"type": "fileChange", "id": "f1", "changes": changes, "status": status});
        let mut all = feed(&mut t, "item/started", json!({"item": item("inProgress")}));
        all.extend(feed(&mut t, "item/completed", json!({"item": item("completed")})));
        let evs = events(&all);
        match &evs[0] {
            Event::ToolCall { name, category, title, .. } => {
                assert_eq!(
                    (name.as_str(), *category, title.as_str()),
                    ("fileChange", ToolCategory::FileEdit, "Edit 4 files (a.txt, b.txt, c.txt, …)")
                );
            }
            other => panic!("{other:?}"),
        }
        match &evs[1] {
            Event::ToolResult { output, is_error, detail, .. } => {
                assert!(!is_error);
                assert_eq!(output, &json!("Created a.txt\nModified b.txt\nDeleted c.txt\nMoved d.txt to e.txt"));
                let diff = detail.as_ref().unwrap().unified_diff.as_ref().unwrap();
                assert!(diff.contains("--- /dev/null\n+++ b/a.txt"));
                assert!(diff.contains("--- a/d.txt\n+++ b/e.txt"));
                assert!(!diff.contains("Moved to"));
            }
            other => panic!("{other:?}"),
        }
        let changed: Vec<(String, FileChangeKind)> = evs
            .iter()
            .filter_map(|e| match e {
                Event::FileChanged { path, kind } => Some((path.clone(), *kind)),
                _ => None,
            })
            .collect();
        assert_eq!(
            changed,
            vec![
                ("/w/a.txt".to_string(), FileChangeKind::Created),
                ("/w/b.txt".to_string(), FileChangeKind::Modified),
                ("/w/c.txt".to_string(), FileChangeKind::Deleted),
                ("/w/d.txt".to_string(), FileChangeKind::Deleted),
                ("/w/e.txt".to_string(), FileChangeKind::Created),
            ]
        );
        // 被拒绝的补丁：出错、没有 file_changed、没有 diff
        let outs = feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "fileChange", "id": "f2", "changes": changes, "status": "declined"}}),
        );
        let evs = events(&outs);
        assert!(evs.iter().all(|e| !matches!(e, Event::FileChanged { .. })));
        match evs.iter().find(|e| matches!(e, Event::ToolResult { .. })).unwrap() {
            Event::ToolResult { is_error, detail, output, .. } => {
                assert!(*is_error);
                assert!(detail.is_none());
                assert_eq!(output, &json!("Declined by the user"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn mcp_and_search_and_unknown_items() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let mut all = feed(
            &mut t,
            "item/started",
            json!({"item": {"type": "mcpToolCall", "id": "m", "server": "gh", "tool": "issue", "status": "inProgress", "arguments": {"n": 1}}}),
        );
        all.extend(feed(&mut t, "item/completed", json!({"item": {"type": "mcpToolCall", "id": "m", "server": "gh", "tool": "issue", "status": "completed", "arguments": {"n": 1}, "result": {"content": [{"type": "text", "text": "ok"}]}}})));
        all.extend(feed(&mut t, "item/started", json!({"item": {"type": "mcpToolCall", "id": "m2", "server": "gh", "tool": "issue", "status": "inProgress", "arguments": {}}})));
        all.extend(feed(&mut t, "item/completed", json!({"item": {"type": "mcpToolCall", "id": "m2", "server": "gh", "tool": "issue", "status": "failed", "arguments": {}, "error": {"message": "nope"}}})));
        all.extend(feed(&mut t, "item/started", json!({"item": {"type": "webSearch", "id": "w", "query": "rust"}})));
        all.extend(feed(&mut t, "item/completed", json!({"item": {"type": "webSearch", "id": "w", "query": "rust"}})));
        // 未知类型、未知通知：丢弃
        all.extend(feed(&mut t, "item/started", json!({"item": {"type": "holographicThing", "id": "h"}})));
        all.extend(feed(&mut t, "some/new/notification", json!({"x": 1})));
        all.extend(feed(&mut t, "item/started", json!({"item": 5})));
        assert_eq!(
            names(&all),
            vec!["tool_call", "tool_result", "tool_call", "tool_result", "tool_call", "tool_result"]
        );
        let evs = events(&all);
        match &evs[1] {
            Event::ToolResult { output, is_error, .. } => assert_eq!((output, *is_error), (&json!("ok"), false)),
            other => panic!("{other:?}"),
        }
        match &evs[3] {
            Event::ToolResult { output, is_error, .. } => assert_eq!((output, *is_error), (&json!("nope"), true)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn completed_without_started_still_produces_a_call() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs = feed(
            &mut t,
            "item/completed",
            json!({"item": {"type": "commandExecution", "id": "late", "command": "ls", "cwd": "/w", "status": "completed", "commandActions": [], "aggregatedOutput": "", "exitCode": 0}}),
        );
        assert_eq!(names(&outs), vec!["tool_call", "tool_result"]);
    }

    #[test]
    fn interrupted_turn_closes_everything_that_was_open() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        feed(&mut t, "item/started", json!({"item": {"type": "agentMessage", "id": "a", "text": ""}}));
        feed(&mut t, "item/agentMessage/delta", json!({"itemId": "a", "delta": "par"}));
        feed(&mut t, "item/started", json!({"item": {"type": "reasoning", "id": "r", "summary": [], "content": []}}));
        feed(&mut t, "item/reasoning/summaryTextDelta", json!({"itemId": "r", "delta": "hm", "summaryIndex": 0}));
        feed(
            &mut t,
            "item/started",
            json!({"item": {"type": "commandExecution", "id": "c", "command": "sleep 30", "cwd": "/w", "status": "inProgress", "commandActions": []}}),
        );
        t.mark_cancel_requested();
        let outs = completed(&mut t, "t1", "interrupted", json!({}));
        assert_eq!(names(&outs), vec!["text_done", "thinking_done", "tool_result", "turn_done"]);
        let evs = events(&outs);
        match &evs[2] {
            Event::ToolResult { id, is_error, .. } => assert_eq!((id.as_str(), *is_error), ("c", true)),
            other => panic!("{other:?}"),
        }
        match &evs[3] {
            Event::TurnDone { reason, .. } => assert_eq!(*reason, TurnDoneReason::Cancelled),
            other => panic!("{other:?}"),
        }
        // 下一个 turn 是干净的
        turn_started(&mut t, "t2");
        let outs = completed(&mut t, "t2", "completed", json!({}));
        assert_eq!(names(&outs), vec!["turn_done"]);
        match &events(&outs)[0] {
            Event::TurnDone { turn_id, .. } => assert_eq!(*turn_id, 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn interrupted_status_is_cancelled_even_without_us_asking() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs = completed(&mut t, "t1", "interrupted", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { reason, .. } => assert_eq!(*reason, TurnDoneReason::Cancelled),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn usage_is_summed_per_turn_and_excludes_cache_from_input() {
        let mut t = Translator::new("/w", Some("gpt-x".into()));
        turn_started(&mut t, "t1");
        let usage = |turn: &str, last: Value| json!({"threadId": "th", "turnId": turn, "tokenUsage": {"total": {}, "last": last, "modelContextWindow": 1000}});
        let l1 = json!({"totalTokens": 150, "inputTokens": 120, "cachedInputTokens": 40, "cacheWriteInputTokens": 0, "outputTokens": 30, "reasoningOutputTokens": 8});
        let l2 = json!({"totalTokens": 250, "inputTokens": 200, "cachedInputTokens": 100, "cacheWriteInputTokens": 5, "outputTokens": 50, "reasoningOutputTokens": 0});
        feed(&mut t, "thread/tokenUsage/updated", usage("t1", l1));
        feed(&mut t, "thread/tokenUsage/updated", usage("t1", l2));
        // 别的 turn 的重放不算
        feed(
            &mut t,
            "thread/tokenUsage/updated",
            usage(
                "old",
                json!({"inputTokens": 999, "outputTokens": 999, "cachedInputTokens": 0, "reasoningOutputTokens": 0, "totalTokens": 0}),
            ),
        );
        let outs = completed(&mut t, "t1", "completed", json!({"durationMs": 1500}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => {
                assert_eq!(u.input_tokens, Some(120 + 200 - 40 - 100));
                assert_eq!(u.output_tokens, Some(80));
                assert_eq!(u.cache_read_tokens, Some(140));
                assert_eq!(u.cache_write_tokens, Some(5));
                assert_eq!((u.cost_usd, u.duration_ms, u.model.as_deref()), (None, 1500, Some("gpt-x")));
            }
            other => panic!("{other:?}"),
        }
        // 不在 turn 里的用量（resume 重放）不进下一个 turn
        feed(
            &mut t,
            "thread/tokenUsage/updated",
            usage(
                "t1",
                json!({"inputTokens": 7, "outputTokens": 7, "cachedInputTokens": 0, "reasoningOutputTokens": 0, "totalTokens": 14}),
            ),
        );
        turn_started(&mut t, "t2");
        let outs = completed(&mut t, "t2", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => assert_eq!((u.input_tokens, u.output_tokens), (None, None)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn failed_turn_reports_the_error_once() {
        // 先有 `error` 通知（willRetry: false），随后 turn/completed: failed
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let e = json!({"message": "Quota exceeded. Check your plan and billing details.", "codexErrorInfo": "usageLimitExceeded", "additionalDetails": null});
        let mut all = feed(&mut t, "error", json!({"error": e, "willRetry": false, "threadId": "th", "turnId": "t1"}));
        all.extend(completed(&mut t, "t1", "failed", json!({"error": e})));
        assert_eq!(names(&all), vec!["error", "turn_done"]);
        match &events(&all)[0] {
            Event::Error { code, message, recoverable } => {
                assert_eq!((code.as_str(), *recoverable), ("rate_limited", true));
                assert!(message.starts_with("Quota exceeded"));
            }
            other => panic!("{other:?}"),
        }
        match &events(&all)[1] {
            Event::TurnDone { reason, .. } => assert_eq!(*reason, TurnDoneReason::Error),
            other => panic!("{other:?}"),
        }
        // 没有 `error` 通知时由 turn/completed 补一条
        turn_started(&mut t, "t2");
        let outs = completed(
            &mut t,
            "t2",
            "failed",
            json!({"error": {"message": "x 401 Unauthorized y", "codexErrorInfo": "other"}}),
        );
        assert_eq!(names(&outs), vec!["error", "turn_done"]);
        match &events(&outs)[0] {
            Event::Error { code, recoverable, .. } => assert_eq!((code.as_str(), *recoverable), ("auth_failed", false)),
            other => panic!("{other:?}"),
        }
        // 我们要求过取消，失败也算取消
        turn_started(&mut t, "t3");
        t.mark_cancel_requested();
        let outs = completed(&mut t, "t3", "failed", json!({"error": {"message": "aborted"}}));
        assert_eq!(names(&outs), vec!["turn_done"]);
    }

    #[test]
    fn error_codes() {
        let e = |v: Value| serde_json::from_value::<TurnError>(v).unwrap();
        assert_eq!(error_code(&e(json!({"message": "m", "codexErrorInfo": "unauthorized"}))), ("auth_failed", false));
        assert_eq!(
            error_code(&e(json!({"message": "m", "codexErrorInfo": "rateLimitExceeded"}))),
            ("rate_limited", true)
        );
        assert_eq!(
            error_code(&e(json!({"message": "m", "codexErrorInfo": "contextWindowExceeded"}))),
            ("context_window_exceeded", true)
        );
        assert_eq!(error_code(&e(json!({"message": "m", "codexErrorInfo": "sandboxError"}))), ("sandbox_error", true));
        assert_eq!(
            error_code(&e(
                json!({"message": "m", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 403}}})
            )),
            ("auth_failed", false)
        );
        assert_eq!(
            error_code(&e(
                json!({"message": "m", "codexErrorInfo": {"responseStreamConnectionFailed": {"httpStatusCode": 429}}})
            )),
            ("rate_limited", true)
        );
        assert_eq!(error_code(&e(json!({"message": "Not logged in"}))), ("auth_failed", false));
        assert_eq!(
            error_code(&e(json!({"message": "something odd", "codexErrorInfo": "other"}))),
            ("agent_error", true)
        );
        assert_eq!(
            error_message(&e(json!({"message": "stream broke", "additionalDetails": "tls handshake eof"}))),
            "stream broke (tls handshake eof)"
        );
        assert_eq!(error_message(&e(json!({"message": ""}))), "Codex reported an error");
    }

    #[test]
    fn reconnecting_errors_are_recoverable_notices() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs = feed(
            &mut t,
            "error",
            json!({"error": {"message": "Reconnecting... 2/5", "codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": null}}, "additionalDetails": "tls handshake eof"}, "willRetry": true, "threadId": "th", "turnId": "t1"}),
        );
        match &events(&outs)[0] {
            Event::Error { code, recoverable, message } => {
                assert_eq!((code.as_str(), *recoverable), ("agent_reconnecting", true));
                assert!(message.contains("Reconnecting") && message.contains("tls handshake eof"));
            }
            other => panic!("{other:?}"),
        }
        // 之后正常完成：不会再冒出错误
        let outs = completed(&mut t, "t1", "completed", json!({}));
        assert_eq!(names(&outs), vec!["turn_done"]);
    }

    #[test]
    fn plan_updates() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs = feed(
            &mut t,
            "turn/plan/updated",
            json!({"threadId": "th", "turnId": "t1", "explanation": null, "plan": [
                {"step": "Read the code", "status": "completed"},
                {"step": "Write the fix", "status": "inProgress"},
                {"step": "Run tests", "status": "pending"}]}),
        );
        assert_eq!(
            events(&outs),
            vec![Event::Plan {
                entries: vec![
                    PlanEntry { content: "Read the code".into(), status: PlanStatus::Completed },
                    PlanEntry { content: "Write the fix".into(), status: PlanStatus::InProgress },
                    PlanEntry { content: "Run tests".into(), status: PlanStatus::Pending },
                ]
            }]
        );
    }

    #[test]
    fn approvals_become_permissions_and_resolved_is_forwarded() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        feed(
            &mut t,
            "item/started",
            json!({"item": {"type": "commandExecution", "id": "c1", "command": "/bin/zsh -lc 'make'", "cwd": "/w", "status": "inProgress", "commandActions": []}}),
        );
        let outs = t.feed_request(
            json!(0),
            "item/commandExecution/requestApproval",
            &json!({"kind": "command", "threadId": "th", "turnId": "t1", "itemId": "c1", "command": "/bin/zsh -lc 'make'", "cwd": "/w", "reason": "needs the network"}),
        );
        match &outs[0] {
            Out::Permission { pending, native } => {
                assert_eq!(pending.kind, anycon_protocol::PermissionKind::Tool);
                let d = pending.detail.tool.as_ref().unwrap().detail.as_ref().unwrap();
                assert_eq!(d.command.as_deref(), Some("make"));
                assert_eq!(d.description.as_deref(), Some("needs the network"));
                assert_eq!(native.rpc_id, json!(0));
            }
            other => panic!("{other:?}"),
        }
        let outs = feed(&mut t, "serverRequest/resolved", json!({"threadId": "th", "requestId": 0}));
        assert_eq!(outs, vec![Out::Resolved { rpc_id: json!(0) }]);
    }

    #[test]
    fn unsupported_requests_are_refused_and_surfaced() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let outs = t.feed_request(json!("e-1"), "mcpServer/elicitation/request", &json!({"serverName": "s", "threadId": "th", "mode": "form", "message": "x", "requestedSchema": {"type": "object", "properties": {}}}));
        assert_eq!(outs[0], Out::Reply { id: json!("e-1"), result: Ok(json!({"action": "decline"})) });
        match &outs[1] {
            Out::Event(AgentEvent::Event(Event::Error { code, recoverable, .. })) => {
                assert_eq!((code.as_str(), *recoverable), ("unsupported_request", true))
            }
            other => panic!("{other:?}"),
        }
        // 形状不对的审批也要回
        let outs = t.feed_request(json!(5), "item/commandExecution/requestApproval", &json!({"nonsense": true}));
        assert!(matches!(&outs[0], Out::Reply { result: Err(e), .. } if e.code == -32602));
    }

    #[test]
    fn approvals_during_cancellation_are_cancelled_at_once() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        t.mark_cancel_requested();
        let outs = t.feed_request(
            json!(9),
            "item/fileChange/requestApproval",
            &json!({"itemId": "f", "threadId": "th", "turnId": "t1"}),
        );
        assert_eq!(outs, vec![Out::Reply { id: json!(9), result: Ok(json!({"decision": "cancel"})) }]);
    }

    #[test]
    fn stale_completion_of_another_turn_is_ignored() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t2");
        let outs = completed(&mut t, "t1-old", "completed", json!({}));
        assert!(outs.is_empty());
        assert!(t.is_busy());
        // 没有进行中的 turn 时的 turn/completed 也不发 turn_done
        let mut idle = Translator::new("/w", None);
        assert!(completed(&mut idle, "x", "completed", json!({})).is_empty());
    }

    #[test]
    fn finish_distinguishes_cancel_from_crash() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let f = t.finish();
        assert!(f.died_mid_turn && !f.cancelled && f.out.is_empty());
        turn_started(&mut t, "t2");
        t.mark_cancel_requested();
        let f = t.finish();
        assert!(!f.died_mid_turn && f.cancelled);
        assert_eq!(names(&f.out), vec!["turn_done"]);
        let f = Translator::new("/w", None).finish();
        assert!(!f.died_mid_turn && f.out.is_empty());
    }

    #[test]
    fn rerouted_model_shows_up_in_usage() {
        let mut t = Translator::new("/w", Some("a".into()));
        turn_started(&mut t, "t1");
        feed(
            &mut t,
            "model/rerouted",
            json!({"fromModel": "a", "toModel": "b", "reason": "x", "threadId": "th", "turnId": "t1"}),
        );
        let outs = completed(&mut t, "t1", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => assert_eq!(u.model.as_deref(), Some("b")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversize_item_notifications_become_a_call_and_a_result_without_the_content() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let prefix = |kind: &str, status: &str| {
            format!(
                r#"{{"method":"item/{kind}","params":{{"item":{{"type":"fileChange","id":"f9","changes":[{{"path":"/w/big.txt","kind":{{"type":"add"}},"diff":"aaaaaaaa"#
            ) + status
        };
        let limit = 8 * 1024 * 1024;
        let started = t.feed_oversize("item/started", &prefix("started", ""), 11_329_324, limit);
        match &events(&started)[0] {
            Event::ToolCall { id, name, category, title, input, detail } => {
                assert_eq!((id.as_str(), name.as_str(), *category), ("f9", "fileChange", ToolCategory::FileEdit));
                assert_eq!(title, "Create big.txt (details too large to show)");
                assert!(input["omitted"].as_str().unwrap().contains("11329324 bytes"), "{input}");
                assert_eq!(detail.as_ref().unwrap().path.as_deref(), Some("/w/big.txt"));
            }
            other => panic!("{other:?}"),
        }
        let done = t.feed_oversize("item/completed", &prefix("completed", ""), 11_329_327, limit);
        match &events(&done)[0] {
            Event::ToolResult { id, output, is_error, .. } => {
                assert_eq!(id, "f9");
                assert!(output.as_str().unwrap().starts_with("[result not shown"), "{output}");
                assert!(!is_error);
            }
            other => panic!("{other:?}"),
        }
        // 结果已经补过了：turn 结束时不会再为它补一个「出错」的结果
        let end = completed(&mut t, "t1", "completed", json!({}));
        assert_eq!(names(&end), vec!["turn_done"]);
        // 只有 completed（started 也超长但已经丢了的话这里就是全部）：先补 tool_call 再补 tool_result
        let only = t.feed_oversize("item/completed", &prefix("completed", "").replace("f9", "f10"), 9_000_000, limit);
        assert_eq!(names(&only), vec!["tool_call", "tool_result"]);
    }

    #[test]
    fn oversize_messages_of_other_kinds_are_dropped_quietly() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let limit = 8 * 1024 * 1024;
        // 整个 turn 的累计 diff：adapter 不用
        assert!(t
            .feed_oversize(
                "turn/diff/updated",
                r#"{"method":"turn/diff/updated","params":{"diff":"x"#,
                9_000_000,
                limit
            )
            .is_empty());
        // 认不出的 item、前缀里没有 id
        assert!(t
            .feed_oversize(
                "item/started",
                r#"{"method":"item/started","params":{"item":{"type":"fileCh"#,
                9_000_000,
                limit
            )
            .is_empty());
        // 不展示的类型
        let odd = r#"{"method":"item/completed","params":{"item":{"type":"holographicProjection","id":"h1","beam":"x"#;
        assert!(t.feed_oversize("item/completed", odd, 9_000_000, limit).is_empty());
        // 超长的文本消息：补上 text_done，别让块一直开着
        t.feed_message(&note("item/started", json!({"item": {"type": "agentMessage", "id": "a1", "text": ""}})));
        t.feed_message(&note("item/agentMessage/delta", json!({"itemId": "a1", "delta": "partial"})));
        let msg = r#"{"method":"item/completed","params":{"item":{"type":"agentMessage","id":"a1","text":"partial"#;
        assert_eq!(names(&t.feed_oversize("item/completed", msg, 9_000_000, limit)), vec!["text_done"]);
        // 超长的 MCP 结果：调用和结果都有，名字从前缀里来
        let mcp = r#"{"method":"item/completed","params":{"item":{"type":"mcpToolCall","id":"m1","server":"shots","tool":"screenshot","status":"completed","arguments":{},"result":{"content":[{"type":"image","data":"AAAA"#;
        let outs = t.feed_oversize("item/completed", mcp, 12_000_000, limit);
        assert_eq!(names(&outs), vec!["tool_call", "tool_result"]);
        match &events(&outs)[0] {
            Event::ToolCall { name, .. } => assert_eq!(name, "mcp__shots__screenshot"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn turn_start_ids_are_tracked() {
        let mut t = Translator::new("/w", None);
        t.begin_turn();
        // turn/start 的响应先到
        t.set_codex_turn_id("abc");
        assert_eq!(t.codex_turn_id(), Some("abc"));
        let outs = feed(
            &mut t,
            "turn/started",
            json!({"threadId": "th", "turn": {"id": "abc", "status": "inProgress", "items": []}}),
        );
        assert_eq!(outs, vec![Out::TurnStarted { codex_turn_id: "abc".into() }]);
        // 不在 turn 里时不记
        let mut idle = Translator::new("/w", None);
        idle.set_codex_turn_id("x");
        assert_eq!(idle.codex_turn_id(), None);
    }
}
