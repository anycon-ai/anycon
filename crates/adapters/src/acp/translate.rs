//! 一个会话里 agent 发来的东西 → [`AgentEvent`]（docs/spec/daemon.md § 8 的 ACP 一条）。
//!
//! 纯同步状态机：`on_*` 进、`Vec<Out>` 出，不碰进程和 IO，所以可以直接拿 JSON 回放测试。要点：
//! - `agent_message_chunk` → `text_delta`，`agent_thought_chunk` → `thinking_delta`；一段文字 / 思考在被别的内容打断、`messageId`
//!   变了、turn 结束时收口（`text_done` / `thinking_done`）
//! - `tool_call` / `tool_call_update` 按 `toolCallId` 合并成一次调用。**`tool_call` 事件要等到有内容才发**：很多 agent 先发一个空壳的
//!   `pending`（`rawInput` 是 `{}`），参数随后的 `in_progress` 更新才补上，而 `tool_call` 事件一旦发出就改不了（CLI 每个事件打一行）。
//!   所以第一次看到有输入 / 内容 / 位置、或者状态不再是 `pending`、或者权限请求要引用它、或者后面出现了别的内容（文字、计划）、
//!   或者 turn 结束时才发；发的时候先把排在它前面还没发的补上，保持顺序。`completed` / `failed` → `tool_result`（带 `diff` 内容的
//!   编辑类调用另有 `unified_diff` 和 `file_changed`）。`in_progress` 期间内容文字在末尾增长的部分 → `tool_output_delta`
//! - `plan` → `plan`；`session_info_update.title` → `AgentEvent::Title`（同一个标题只发一次）
//! - `usage_update`（上下文占用 `used` / `size`，累计费用 `cost`）累积进 `turn_done.usage`：`context_tokens` / `context_window` 取最近一次，
//!   `cost_usd` 是累计费用相对上一个 turn 结束时的增量（只认 USD；续接的会话不知道基线，第一个 turn 不报费用）。ACP v1 稳定版没有 token 数，
//!   所以 `input_tokens` / `output_tokens` 为空；`PromptResponse.usage` 在草案里（语义是单 turn 还是累计还没定），不读
//! - `session/prompt` 的 `stopReason` → `turn_done.reason`：`end_turn` → completed，`cancelled`，`max_tokens` / `max_turn_requests` →
//!   max_turns，`refusal` → error（先发一条可恢复的 `error`）；JSON-RPC 错误：`-32800` 或者已经要求过取消 → cancelled，
//!   `-32000` → `error { auth_failed }`，其余 `error { agent_error }`
//! - turn 结束时还没收尾的工具调用补一条 `tool_result`（取消 / 出错的标 `is_error`），客户端的转圈才停得下来
//! - 权限请求 / elicitation → `Out::Permission`（见 `permissions.rs`）；会话正在取消、或者没有 turn 在跑时到达的直接按取消答复
//!
//! 不翻译：`user_message_chunk`（用户消息由 daemon 生成）、`available_commands_update`、`notice` / `compaction_*`（我们没有声明这些能力，
//! agent 不该发）。`current_mode_update` / `config_option_update` 改的是会话的选项状态，由 `conn` 处理，不进事件流。

use std::collections::HashMap;
use std::time::Instant;

use anycon_agent_api::{AgentEvent, PendingPermission};
use anycon_protocol::{
    Event, FileChangeKind, PlanEntry, PlanStatus, ToolCategory, ToolResultDetail, TurnDoneReason, Usage,
};
use serde_json::{json, Value};

use super::permissions::{self, NativePending};
use super::rpc::{RpcError, AUTH_REQUIRED, REQUEST_CANCELLED};
use super::tools::{self, ToolView};

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Event(AgentEvent),
    /// 权限请求：`native` 由 handle 记下，答复时用。
    Permission {
        pending: PendingPermission,
        native: NativePending,
    },
    /// 直接答复 agent 的一个请求（取消中的、读不懂的）。
    Reply {
        id: Value,
        result: Result<Value, RpcError>,
    },
}

fn ev(e: Event) -> Out {
    Out::Event(AgentEvent::Event(e))
}

/// `session/prompt` 的结局。
#[derive(Debug, Clone, PartialEq)]
pub enum PromptOutcome {
    StopReason(String),
    Error(RpcError),
}

/// 进程退出时的收尾结果。
pub struct Finish {
    pub out: Vec<Out>,
    /// 退出时 turn 还在进行，且不是我们要求的取消——上层应补一个 `Error` 事件。
    pub died_mid_turn: bool,
    /// 退出前我们已经要求了取消，所以这次退出是预期内的。
    pub cancelled: bool,
}

struct OpenText {
    message_id: Option<String>,
}

struct OpenThought {
    message_id: Option<String>,
    started: Instant,
}

struct ToolState {
    view: ToolView,
    status: String,
    raw_output: Value,
    emitted: bool,
    finished: bool,
}

impl ToolState {
    fn new(id: &str) -> Self {
        Self {
            view: ToolView::new(id),
            status: "pending".to_string(),
            raw_output: Value::Null,
            emitted: false,
            finished: false,
        }
    }

    fn terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed")
    }
}

struct TurnState {
    started: Instant,
    cancel_requested: bool,
    text: Option<OpenText>,
    thought: Option<OpenThought>,
    tools: HashMap<String, ToolState>,
    /// 工具调用第一次出现的顺序。
    order: Vec<String>,
    /// 最近一次 `usage_update` 的 `(used, size)`。
    context: Option<(u64, u64)>,
    /// 最近一次 `usage_update` 里的累计费用（USD）。
    cost: Option<f64>,
}

impl TurnState {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            cancel_requested: false,
            text: None,
            thought: None,
            tools: HashMap::new(),
            order: Vec::new(),
            context: None,
            cost: None,
        }
    }
}

pub struct Translator {
    cwd: String,
    model: Option<String>,
    turn_counter: u64,
    /// 上一个 turn 结束时的累计费用；`None` = 不知道（续接的会话，第一个 turn 不报费用）。
    cost_baseline: Option<f64>,
    title: Option<String>,
    turn: Option<TurnState>,
}

// ---------- 收口 / 发工具调用（作用在一个 turn 上） ----------

fn close_text(turn: &mut TurnState, out: &mut Vec<Out>) {
    if turn.text.take().is_some() {
        out.push(ev(Event::TextDone {}));
    }
}

fn close_thought(turn: &mut TurnState, out: &mut Vec<Out>) {
    if let Some(t) = turn.thought.take() {
        out.push(ev(Event::ThinkingDone { duration_ms: t.started.elapsed().as_millis() as u64 }));
    }
}

fn close_open(turn: &mut TurnState, out: &mut Vec<Out>) {
    close_text(turn, out);
    close_thought(turn, out);
}

/// 把还没发的 `tool_call` 按出现顺序发出去；`upto = Some(id)` 时发到这一个为止（含）。
fn emit_calls(turn: &mut TurnState, cwd: &str, upto: Option<&str>, out: &mut Vec<Out>) {
    let ids: Vec<String> = turn.order.clone();
    let mut first = true;
    for id in ids {
        let Some(st) = turn.tools.get_mut(&id) else { continue };
        if !st.emitted {
            if first {
                // 这个调用是要发的第一个：先把还开着的文字 / 思考收口，保持顺序
                close_text_and_thought(&mut turn.text, &mut turn.thought, out);
                first = false;
            }
            st.emitted = true;
            let v = &st.view;
            out.push(ev(Event::ToolCall {
                id: id.clone(),
                name: v.display_name(),
                category: v.category(),
                title: v.display_title(),
                input: v.input(),
                detail: v.detail(cwd),
                parent_id: None,
            }));
        }
        if upto == Some(id.as_str()) {
            break;
        }
    }
}

fn close_text_and_thought(text: &mut Option<OpenText>, thought: &mut Option<OpenThought>, out: &mut Vec<Out>) {
    if text.take().is_some() {
        out.push(ev(Event::TextDone {}));
    }
    if let Some(t) = thought.take() {
        out.push(ev(Event::ThinkingDone { duration_ms: t.started.elapsed().as_millis() as u64 }));
    }
}

/// 一次调用结束：发 `tool_result`，成功的编辑再发 `file_changed`。`forced`：turn 结束时被迫收尾的，`Some((是否出错, 没有任何
/// 输出时显示的文字))`。
fn finish_tool(turn: &mut TurnState, cwd: &str, id: &str, forced: Option<(bool, &str)>, out: &mut Vec<Out>) {
    emit_calls(turn, cwd, Some(id), out);
    let Some(st) = turn.tools.get_mut(id) else { return };
    if st.finished {
        return;
    }
    st.finished = true;
    let v = &st.view;
    let diffs = v.diffs();
    let failed = match forced {
        Some((errored, _)) => errored,
        None => st.status == "failed",
    };
    let unified = (v.category() == ToolCategory::FileEdit && !diffs.is_empty())
        .then(|| tools::unified_diff(cwd, &diffs, v.kind == "delete"))
        .flatten();
    let mut output = tools::output_value(&v.text(), &st.raw_output, &diffs);
    if let (Some((_, fallback)), true) = (forced, output.is_null()) {
        output = Value::String(fallback.to_string());
    }
    let detail: Option<ToolResultDetail> = tools::result_detail(&v.kind, &st.raw_output, unified);
    out.push(ev(Event::ToolResult { id: id.to_string(), output, is_error: failed, detail }));
    if !failed && st.status == "completed" && v.category() == ToolCategory::FileEdit {
        if diffs.is_empty() {
            let kind = match v.kind.as_str() {
                "edit" => Some(FileChangeKind::Modified),
                "delete" => Some(FileChangeKind::Deleted),
                _ => None,
            };
            if let Some(kind) = kind {
                for p in &v.locations {
                    out.push(ev(Event::FileChanged { path: p.clone(), kind }));
                }
            }
        }
        for d in &diffs {
            let kind = if v.kind == "delete" {
                FileChangeKind::Deleted
            } else if d.old.is_none() {
                FileChangeKind::Created
            } else {
                FileChangeKind::Modified
            };
            out.push(ev(Event::FileChanged { path: d.path.clone(), kind }));
        }
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn plan_status(s: &str) -> PlanStatus {
    match s {
        "in_progress" => PlanStatus::InProgress,
        "completed" => PlanStatus::Completed,
        _ => PlanStatus::Pending,
    }
}

impl Translator {
    /// `fresh`：这是刚 `session/new` 出来的会话（累计费用从 0 算起）；续接的会话不知道基线。
    pub fn new(cwd: String, model: Option<String>, fresh: bool) -> Self {
        Self { cwd, model, turn_counter: 0, cost_baseline: fresh.then_some(0.0), title: None, turn: None }
    }

    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    pub fn is_busy(&self) -> bool {
        self.turn.is_some()
    }

    pub fn is_cancelling(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.cancel_requested)
    }

    /// `send` 之前调用：开始新 turn。
    pub fn begin_turn(&mut self) {
        self.turn_counter += 1;
        self.turn = Some(TurnState::new());
    }

    /// `session/prompt` 没发出去：这个 turn 没有开始。
    pub fn abort_turn(&mut self) {
        self.turn = None;
    }

    /// 我们发出了 `session/cancel`（或要关闭会话）：此后 turn 按 cancelled 算，新来的权限请求直接按取消答复。
    pub fn mark_cancel_requested(&mut self) {
        if let Some(t) = self.turn.as_mut() {
            t.cancel_requested = true;
        }
    }

    /// 一条 `session/update` 的 `params.update`。
    pub fn on_update(&mut self, update: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let kind = str_of(update, "sessionUpdate").unwrap_or("");
        if kind == "session_info_update" {
            // 标题在没有 turn 的时候也会变（agent 在后台给会话起名）
            if let Some(t) = str_of(update, "title").map(str::trim).filter(|t| !t.is_empty()) {
                if self.title.as_deref() != Some(t) {
                    self.title = Some(t.to_string());
                    out.push(Out::Event(AgentEvent::Title(t.to_string())));
                }
            }
            return out;
        }
        let cwd = self.cwd.as_str();
        let Some(turn) = self.turn.as_mut() else {
            tracing::debug!(kind, "acp: dropping a session update that arrived outside a turn");
            return out;
        };
        match kind {
            "agent_message_chunk" => on_text(turn, cwd, update, &mut out),
            "agent_thought_chunk" => on_thought(turn, cwd, update, &mut out),
            "tool_call" | "tool_call_update" => on_tool(turn, cwd, update, &mut out),
            "plan" => {
                emit_calls(turn, cwd, None, &mut out);
                close_open(turn, &mut out);
                let entries: Vec<PlanEntry> = update
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| {
                                Some(PlanEntry {
                                    content: str_of(e, "content")?.to_string(),
                                    status: plan_status(str_of(e, "status").unwrap_or("pending")),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(ev(Event::Plan { entries }));
            }
            "usage_update" => {
                if let (Some(used), Some(size)) =
                    (update.get("used").and_then(Value::as_u64), update.get("size").and_then(Value::as_u64))
                {
                    turn.context = Some((used, size));
                }
                if let Some(c) = update.get("cost") {
                    let usd = str_of(c, "currency").is_some_and(|cur| cur.eq_ignore_ascii_case("USD"));
                    if let (true, Some(amount)) = (usd, c.get("amount").and_then(Value::as_f64)) {
                        turn.cost = Some(amount);
                    }
                }
            }
            // 用户消息由 daemon 生成；这些要么是 `session/load` 重放的历史，要么 agent 不该发（我们没有声明相应的能力）
            "user_message_chunk" | "available_commands_update" | "current_mode_update" | "config_option_update" => {}
            other => tracing::debug!(kind = other, "acp: ignoring an unknown session update"),
        }
        out
    }

    /// `session/request_permission`（agent 发来的请求，`id` 原样回传）。
    pub fn on_permission(&mut self, rpc_id: Value, params: &Value) -> Vec<Out> {
        let cancelled = || json!({ "outcome": { "outcome": "cancelled" } });
        let cwd = self.cwd.as_str();
        let mut out = Vec::new();
        let Some(turn) = self.turn.as_mut().filter(|t| !t.cancel_requested) else {
            // 没有 turn 在跑 / 正在取消：没人能答复，按取消答复，免得 agent 一直等
            out.push(Out::Reply { id: rpc_id, result: Ok(cancelled()) });
            return out;
        };
        let tc = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let Some(id) = str_of(&tc, "toolCallId").filter(|s| !s.is_empty()).map(str::to_string) else {
            out.push(Out::Reply { id: rpc_id, result: Ok(cancelled()) });
            return out;
        };
        // 权限请求里的 `toolCall` 和 `tool_call_update` 同一个形状：合进来，并且这个调用现在就要出现在事件流里（卡片引用它）
        let existed = turn.tools.contains_key(&id);
        let st = turn.tools.entry(id.clone()).or_insert_with(|| ToolState::new(&id));
        if !existed {
            turn.order.push(id.clone());
        }
        if !st.finished {
            st.view.merge(&tc);
            if let Some(s) = str_of(&tc, "status").filter(|s| !s.is_empty()) {
                st.status = s.to_string();
            }
        }
        emit_calls(turn, cwd, Some(&id), &mut out);
        let view = turn.tools[&id].view.clone();
        let request_id = uuid::Uuid::new_v4().to_string();
        match permissions::from_request_permission(&request_id, rpc_id.clone(), params, &view, cwd) {
            Some((pending, native)) => out.push(Out::Permission { pending, native }),
            None => {
                out.push(ev(Event::Error {
                    code: "unsupported_request".to_string(),
                    message: "The agent asked for permission without any option to choose from, so the request was cancelled."
                        .to_string(),
                    recoverable: true,
                }));
                out.push(Out::Reply { id: rpc_id, result: Ok(cancelled()) });
            }
        }
        out
    }

    /// `elicitation/create`（带 `sessionId`）。
    pub fn on_elicitation(&mut self, rpc_id: Value, params: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let busy = self.turn.as_ref().filter(|t| !t.cancel_requested);
        if busy.is_none() {
            let action = if self.is_cancelling() { "cancel" } else { "decline" };
            out.push(Out::Reply { id: rpc_id, result: Ok(json!({ "action": action })) });
            return out;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        match permissions::from_elicitation(&request_id, rpc_id.clone(), params) {
            Ok((pending, native)) => {
                if let Some(turn) = self.turn.as_mut() {
                    emit_calls(turn, &self.cwd, None, &mut out);
                    close_open(turn, &mut out);
                }
                out.push(Out::Permission { pending, native });
            }
            Err(message) => {
                out.push(ev(Event::Error { code: "elicitation_rejected".to_string(), message, recoverable: true }));
                out.push(Out::Reply { id: rpc_id, result: Ok(json!({ "action": "decline" })) });
            }
        }
        out
    }

    /// `session/prompt` 有了结局（响应或错误）：收尾这个 turn。没有 turn 在跑（进程退出时已经收尾了）= 什么也不做。
    pub fn finish_turn(&mut self, outcome: PromptOutcome) -> Vec<Out> {
        let mut out = Vec::new();
        let Some(mut turn) = self.turn.take() else { return out };
        let cwd = self.cwd.clone();
        let (reason, error): (TurnDoneReason, Option<(&str, String)>) = match &outcome {
            PromptOutcome::StopReason(s) => match s.as_str() {
                "cancelled" => (TurnDoneReason::Cancelled, None),
                "max_tokens" | "max_turn_requests" => (TurnDoneReason::MaxTurns, None),
                "refusal" => {
                    (TurnDoneReason::Error, Some(("agent_refusal", "The agent refused to continue.".to_string())))
                }
                "end_turn" => (TurnDoneReason::Completed, None),
                other => {
                    tracing::debug!(stop_reason = other, "acp: unknown stop reason, treating the turn as completed");
                    (TurnDoneReason::Completed, None)
                }
            },
            // 要求过取消之后 agent 回的错误（API 库被中止时常见）按取消算
            PromptOutcome::Error(e) if e.code == REQUEST_CANCELLED || turn.cancel_requested => {
                (TurnDoneReason::Cancelled, None)
            }
            PromptOutcome::Error(e) if e.code == AUTH_REQUIRED => (
                TurnDoneReason::Error,
                Some(("auth_failed", format!("The agent needs you to log in: {}", error_text(e)))),
            ),
            PromptOutcome::Error(e) => (TurnDoneReason::Error, Some(("agent_error", error_text(e)))),
        };
        let (errored, fallback) = match reason {
            TurnDoneReason::Cancelled => (true, "Cancelled before it finished"),
            TurnDoneReason::Error => (true, "Interrupted"),
            _ => (false, ""),
        };
        close_open(&mut turn, &mut out);
        emit_calls(&mut turn, &cwd, None, &mut out);
        let unfinished: Vec<String> =
            turn.order.iter().filter(|id| turn.tools.get(*id).is_some_and(|t| !t.finished)).cloned().collect();
        for id in unfinished {
            finish_tool(&mut turn, &cwd, &id, Some((errored, fallback)), &mut out);
        }
        if let Some((code, message)) = error {
            out.push(ev(Event::Error { code: code.to_string(), message, recoverable: true }));
        }
        let cost_usd = match (turn.cost, self.cost_baseline) {
            (Some(c), Some(b)) => {
                self.cost_baseline = Some(c);
                Some((c - b).max(0.0))
            }
            (Some(c), None) => {
                self.cost_baseline = Some(c);
                None
            }
            (None, _) => None,
        };
        let usage = Usage {
            duration_ms: turn.started.elapsed().as_millis() as u64,
            model: self.model.clone(),
            context_window: turn.context.map(|(_, size)| size),
            context_tokens: turn.context.map(|(used, _)| used),
            cost_usd,
            ..Default::default()
        };
        out.push(ev(Event::TurnDone { turn_id: self.turn_counter, reason, usage: Some(usage) }));
        out
    }

    /// 进程退出：收掉还开着的文字 / 思考 / 工具调用。**不发 `turn_done`**：daemon 在处理 `Exited` 时自己收尾 turn。
    pub fn finish_process(&mut self) -> Finish {
        let mut out = Vec::new();
        let Some(mut turn) = self.turn.take() else {
            return Finish { out, died_mid_turn: false, cancelled: false };
        };
        let cwd = self.cwd.clone();
        close_open(&mut turn, &mut out);
        emit_calls(&mut turn, &cwd, None, &mut out);
        let unfinished: Vec<String> =
            turn.order.iter().filter(|id| turn.tools.get(*id).is_some_and(|t| !t.finished)).cloned().collect();
        for id in unfinished {
            finish_tool(&mut turn, &cwd, &id, Some((true, "Interrupted")), &mut out);
        }
        Finish { out, died_mid_turn: !turn.cancel_requested, cancelled: turn.cancel_requested }
    }
}

fn error_text(e: &RpcError) -> String {
    let m = e.message.trim();
    if m.is_empty() {
        format!("the agent returned error {}", e.code)
    } else {
        m.to_string()
    }
}

fn on_text(turn: &mut TurnState, cwd: &str, update: &Value, out: &mut Vec<Out>) {
    let Some(content) = update.get("content") else { return };
    if str_of(content, "type") != Some("text") {
        return;
    }
    let Some(text) = str_of(content, "text").filter(|t| !t.is_empty()) else { return };
    let mid = str_of(update, "messageId").map(str::to_string);
    emit_calls(turn, cwd, None, out);
    close_thought(turn, out);
    if let Some(open) = &turn.text {
        if mid.is_some() && open.message_id.is_some() && mid != open.message_id {
            close_text(turn, out);
        }
    }
    match &mut turn.text {
        Some(open) => {
            if open.message_id.is_none() {
                open.message_id = mid;
            }
        }
        None => turn.text = Some(OpenText { message_id: mid }),
    }
    out.push(ev(Event::TextDelta { text: text.to_string() }));
}

fn on_thought(turn: &mut TurnState, cwd: &str, update: &Value, out: &mut Vec<Out>) {
    let Some(content) = update.get("content") else { return };
    if str_of(content, "type") != Some("text") {
        return;
    }
    let text = str_of(content, "text").unwrap_or_default();
    let mid = str_of(update, "messageId").map(str::to_string);
    emit_calls(turn, cwd, None, out);
    close_text(turn, out);
    if let Some(open) = &turn.thought {
        if mid.is_some() && open.message_id.is_some() && mid != open.message_id {
            close_thought(turn, out);
        }
    }
    match &mut turn.thought {
        Some(open) => {
            if open.message_id.is_none() {
                open.message_id = mid;
            }
        }
        None => turn.thought = Some(OpenThought { message_id: mid, started: Instant::now() }),
    }
    // 空文本也发：模型只给「正在思考」不给内容时，客户端要看到一个思考块
    out.push(ev(Event::ThinkingDelta { text: text.to_string() }));
}

fn on_tool(turn: &mut TurnState, cwd: &str, update: &Value, out: &mut Vec<Out>) {
    let Some(id) = str_of(update, "toolCallId").filter(|s| !s.is_empty()) else { return };
    let existed = turn.tools.contains_key(id);
    let st = turn.tools.entry(id.to_string()).or_insert_with(|| ToolState::new(id));
    if !existed {
        turn.order.push(id.to_string());
    }
    if st.finished {
        return;
    }
    // 内容文字在末尾增长的部分当作流式输出
    let prev_text = (st.emitted && update.get("content").is_some_and(Value::is_array)).then(|| st.view.text());
    st.view.merge(update);
    if let Some(s) = str_of(update, "status").filter(|s| !s.is_empty()) {
        st.status = s.to_string();
    }
    if let Some(o) = update.get("rawOutput").filter(|o| !o.is_null()) {
        st.raw_output = o.clone();
    }
    let terminal = st.terminal();
    if let (Some(prev), false) = (prev_text, terminal) {
        let now = st.view.text();
        if now.len() > prev.len() && now.starts_with(&prev) {
            out.push(ev(Event::ToolOutputDelta { id: id.to_string(), text: now[prev.len()..].to_string() }));
        }
    }
    let need_emit = !st.emitted && (st.view.has_substance() || st.status != "pending");
    if need_emit {
        emit_calls(turn, cwd, Some(id), out);
    }
    if terminal {
        finish_tool(turn, cwd, id, None, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::PermissionKind;

    fn tr() -> Translator {
        let mut t = Translator::new("/w".into(), Some("m1".into()), true);
        t.begin_turn();
        t
    }

    fn events(out: &[Out]) -> Vec<Event> {
        out.iter()
            .filter_map(|o| match o {
                Out::Event(AgentEvent::Event(e)) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn names(out: &[Out]) -> Vec<String> {
        out.iter()
            .map(|o| match o {
                Out::Event(AgentEvent::Event(e)) => {
                    serde_json::to_value(e).unwrap()["type"].as_str().unwrap().to_string()
                }
                Out::Event(AgentEvent::Title(t)) => format!("TITLE:{t}"),
                Out::Event(_) => "OTHER".into(),
                Out::Permission { .. } => "PERMISSION".into(),
                Out::Reply { .. } => "REPLY".into(),
            })
            .collect()
    }

    fn feed(t: &mut Translator, updates: &[Value]) -> Vec<Out> {
        updates.iter().flat_map(|u| t.on_update(u)).collect()
    }

    fn text(s: &str) -> Value {
        json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": s}})
    }

    fn done(out: &[Out]) -> (TurnDoneReason, Usage) {
        match events(out).last().expect("events") {
            Event::TurnDone { reason, usage, .. } => (*reason, usage.clone().expect("usage")),
            other => panic!("expected turn_done, got {other:?}"),
        }
    }

    #[test]
    fn text_and_thinking_stream_and_close_when_interrupted() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hmm"}}),
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "..."}}),
                text("Hel"),
                text("lo"),
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "again"}}),
            ],
        );
        assert_eq!(
            names(&out),
            vec![
                "thinking_delta",
                "thinking_delta",
                "thinking_done",
                "text_delta",
                "text_delta",
                "text_done",
                "thinking_delta"
            ]
        );
        let out = t.finish_turn(PromptOutcome::StopReason("end_turn".into()));
        assert_eq!(names(&out), vec!["thinking_done", "turn_done"]);
        assert_eq!(done(&out).0, TurnDoneReason::Completed);
    }

    #[test]
    fn a_new_message_id_starts_a_new_text_block() {
        let mut t = tr();
        let with_id = |id: &str, s: &str| json!({"sessionUpdate": "agent_message_chunk", "messageId": id, "content": {"type": "text", "text": s}});
        let out = feed(&mut t, &[with_id("a", "one"), with_id("a", " two"), with_id("b", "three")]);
        assert_eq!(names(&out), vec!["text_delta", "text_delta", "text_done", "text_delta"]);
        // 非文本内容和空文本不产生事件
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "AA==", "mimeType": "image/png"}}),
                text(""),
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "echo"}}),
            ],
        );
        assert!(out.is_empty());
    }

    #[test]
    fn updates_outside_a_turn_are_dropped_except_the_title() {
        let mut t = Translator::new("/w".into(), None, true);
        assert!(t.on_update(&text("late")).is_empty());
        let out = t.on_update(&json!({"sessionUpdate": "session_info_update", "title": "Fix the bug"}));
        assert_eq!(names(&out), vec!["TITLE:Fix the bug"]);
        // 同一个标题只发一次；null / 空串不清掉
        assert!(t.on_update(&json!({"sessionUpdate": "session_info_update", "title": "Fix the bug"})).is_empty());
        assert!(t.on_update(&json!({"sessionUpdate": "session_info_update", "title": null})).is_empty());
        assert!(t.on_update(&json!({"sessionUpdate": "session_info_update", "title": "  "})).is_empty());
    }

    #[test]
    fn a_tool_call_is_announced_once_it_has_substance() {
        let mut t = tr();
        // 空壳的 pending：先不发
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run ls", "kind": "execute", "status": "pending", "rawInput": {}}),
            ],
        );
        assert!(out.is_empty());
        // 参数补上、开始执行：现在发，带完整的输入和 detail
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "in_progress", "rawInput": {"command": "ls -la"}}),
            ],
        );
        match &events(&out)[..] {
            [Event::ToolCall { id, name, category, title, input, detail, parent_id }] => {
                assert_eq!(
                    (id.as_str(), name.as_str(), *category, title.as_str()),
                    ("c1", "execute", ToolCategory::Shell, "Run ls")
                );
                assert_eq!(input, &json!({"command": "ls -la"}));
                assert_eq!(detail.as_ref().unwrap().command.as_deref(), Some("ls -la"));
                assert_eq!(detail.as_ref().unwrap().cwd.as_deref(), Some("/w"));
                assert_eq!(parent_id, &None);
            }
            other => panic!("{other:?}"),
        }
        // 完成：tool_result，输出是内容文字，退出码取自 rawOutput
        let out = feed(
            &mut t,
            &[json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed",
                     "content": [{"type": "content", "content": {"type": "text", "text": "a\nb\n"}}], "rawOutput": {"exit_code": 0}})],
        );
        match &events(&out)[..] {
            [Event::ToolResult { id, output, is_error, detail }] => {
                assert_eq!((id.as_str(), output, *is_error), ("c1", &json!("a\nb\n"), false));
                assert_eq!(detail.as_ref().unwrap().exit_code, Some(0));
            }
            other => panic!("{other:?}"),
        }
        // 结束之后的更新忽略
        assert!(feed(&mut t, &[json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed"})])
            .is_empty());
    }

    #[test]
    fn a_complete_tool_call_is_announced_immediately_and_failures_are_errors() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Read a.rs", "kind": "read", "status": "pending",
                     "locations": [{"path": "/w/a.rs", "line": 1}], "name": "read_file"}),
            ],
        );
        match &events(&out)[..] {
            [Event::ToolCall { name, category, detail, .. }] => {
                assert_eq!((name.as_str(), *category), ("read_file", ToolCategory::FileRead));
                assert_eq!(detail.as_ref().unwrap().path.as_deref(), Some("/w/a.rs"));
            }
            other => panic!("{other:?}"),
        }
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed", "rawOutput": "ENOENT"}),
            ],
        );
        match &events(&out)[..] {
            [Event::ToolResult { output, is_error, .. }] => assert_eq!((output, *is_error), (&json!("ENOENT"), true)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_tool_that_completes_without_ever_having_substance_still_gets_both_events() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Think", "kind": "think", "status": "pending"}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "completed"}),
            ],
        );
        assert_eq!(names(&out), vec!["tool_call", "tool_result"]);
    }

    #[test]
    fn earlier_unannounced_calls_are_announced_first_to_keep_the_order() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "a", "title": "A", "kind": "other", "status": "pending"}),
                json!({"sessionUpdate": "tool_call", "toolCallId": "b", "title": "B", "kind": "other", "status": "pending"}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "b", "status": "in_progress", "rawInput": {"x": 1}}),
            ],
        );
        let ids: Vec<String> = events(&out)
            .into_iter()
            .filter_map(|e| if let Event::ToolCall { id, .. } = e { Some(id) } else { None })
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
        // 文字出现之前，还没发的调用先发
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c", "title": "C", "kind": "other", "status": "pending"}),
                text("after"),
            ],
        );
        assert_eq!(names(&out), vec!["tool_call", "text_delta"]);
    }

    #[test]
    fn an_open_text_block_is_closed_before_a_tool_call() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                text("let me look"),
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "ls", "kind": "execute", "status": "in_progress", "rawInput": {"command": "ls"}}),
            ],
        );
        assert_eq!(names(&out), vec!["text_delta", "text_done", "tool_call"]);
    }

    #[test]
    fn growing_content_while_in_progress_streams_as_output_deltas() {
        let mut t = tr();
        let upd = |s: &str, status: &str| {
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": status,
                   "content": [{"type": "content", "content": {"type": "text", "text": s}}]})
        };
        feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "run", "kind": "execute", "status": "in_progress", "rawInput": {"command": "x"}}),
            ],
        );
        let out = feed(
            &mut t,
            &[upd("one\n", "in_progress"), upd("one\ntwo\n", "in_progress"), upd("one\ntwo\n", "in_progress")],
        );
        let deltas: Vec<String> = events(&out)
            .into_iter()
            .filter_map(|e| if let Event::ToolOutputDelta { text, .. } = e { Some(text) } else { None })
            .collect();
        assert_eq!(deltas, vec!["one\n", "two\n"], "an unchanged update adds nothing");
        // 内容被换成别的文字（不是追加）：不发 delta
        let out = feed(&mut t, &[upd("replaced", "in_progress")]);
        assert!(events(&out).is_empty());
        // 最终结果带完整的输出
        let out = feed(&mut t, &[upd("replaced and done", "completed")]);
        match &events(&out)[..] {
            [Event::ToolResult { output, .. }] => assert_eq!(output, &json!("replaced and done")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn edits_with_diffs_get_a_unified_diff_and_file_changed() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "e1", "title": "Edit a.txt", "kind": "edit", "status": "in_progress",
                       "locations": [{"path": "/w/a.txt"}]}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "completed",
                "content": [
                  {"type": "diff", "path": "/w/a.txt", "oldText": "x\n", "newText": "y\n"},
                  {"type": "diff", "path": "/w/new.txt", "oldText": null, "newText": "n\n"}
                ]}),
            ],
        );
        assert_eq!(names(&out), vec!["tool_call", "tool_result", "file_changed", "file_changed"]);
        let evs = events(&out);
        match &evs[1] {
            Event::ToolResult { output, is_error, detail, .. } => {
                assert_eq!((output, *is_error), (&json!("Edited 2 files"), false));
                let d = detail.as_ref().unwrap().unified_diff.as_deref().unwrap();
                assert!(d.contains("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-x\n+y\n"), "{d}");
                assert!(d.contains("--- /dev/null\n+++ b/new.txt\n"), "{d}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(evs[2], Event::FileChanged { path: "/w/a.txt".into(), kind: FileChangeKind::Modified });
        assert_eq!(evs[3], Event::FileChanged { path: "/w/new.txt".into(), kind: FileChangeKind::Created });
    }

    #[test]
    fn edits_without_diffs_use_the_locations_and_failed_edits_report_nothing() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "e1", "title": "Delete b", "kind": "delete", "status": "in_progress",
                       "locations": [{"path": "/w/b.txt"}]}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "completed"}),
                json!({"sessionUpdate": "tool_call", "toolCallId": "e2", "title": "Edit c", "kind": "edit", "status": "in_progress",
                       "locations": [{"path": "/w/c.txt"}]}),
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "e2", "status": "failed"}),
            ],
        );
        let changed: Vec<Event> = events(&out).into_iter().filter(|e| matches!(e, Event::FileChanged { .. })).collect();
        assert_eq!(changed, vec![Event::FileChanged { path: "/w/b.txt".into(), kind: FileChangeKind::Deleted }]);
    }

    #[test]
    fn plans_replace_the_whole_list() {
        let mut t = tr();
        let out = feed(
            &mut t,
            &[
                text("planning"),
                json!({"sessionUpdate": "plan", "entries": [
                    {"content": "a", "priority": "high", "status": "completed"},
                    {"content": "b", "priority": "low", "status": "in_progress"},
                    {"content": "c", "priority": "low", "status": "weird"},
                    {"priority": "low", "status": "pending"}
                ]}),
            ],
        );
        assert_eq!(names(&out), vec!["text_delta", "text_done", "plan"]);
        match events(&out).last().unwrap() {
            Event::Plan { entries } => assert_eq!(
                entries.iter().map(|e| (e.content.as_str(), e.status)).collect::<Vec<_>>(),
                vec![("a", PlanStatus::Completed), ("b", PlanStatus::InProgress), ("c", PlanStatus::Pending)]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stop_reasons_map_to_turn_done_reasons() {
        for (stop, reason, errors) in [
            ("end_turn", TurnDoneReason::Completed, vec![]),
            ("cancelled", TurnDoneReason::Cancelled, vec![]),
            ("max_tokens", TurnDoneReason::MaxTurns, vec![]),
            ("max_turn_requests", TurnDoneReason::MaxTurns, vec![]),
            ("refusal", TurnDoneReason::Error, vec!["agent_refusal"]),
            ("something_new", TurnDoneReason::Completed, vec![]),
        ] {
            let mut t = tr();
            let out = t.finish_turn(PromptOutcome::StopReason(stop.into()));
            assert_eq!(done(&out).0, reason, "{stop}");
            let codes: Vec<String> = events(&out)
                .into_iter()
                .filter_map(|e| {
                    if let Event::Error { code, recoverable, .. } = e {
                        assert!(recoverable);
                        Some(code)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(codes, errors, "{stop}");
            assert!(!t.is_busy());
        }
    }

    #[test]
    fn prompt_errors_map_to_cancelled_auth_or_agent_errors() {
        let err = |code: i64, m: &str| PromptOutcome::Error(RpcError::new(code, m));
        let mut t = tr();
        assert_eq!(done(&t.finish_turn(err(REQUEST_CANCELLED, "Request cancelled"))).0, TurnDoneReason::Cancelled);
        let mut t = tr();
        let out = t.finish_turn(err(AUTH_REQUIRED, "Authentication required"));
        assert_eq!(done(&out).0, TurnDoneReason::Error);
        assert!(events(&out).iter().any(|e| matches!(e, Event::Error { code, .. } if code == "auth_failed")));
        let mut t = tr();
        let out = t.finish_turn(err(-32603, "boom"));
        assert!(events(&out)
            .iter()
            .any(|e| matches!(e, Event::Error { code, message, .. } if code == "agent_error" && message == "boom")));
        // 要求过取消之后任何错误都按取消算
        let mut t = tr();
        t.mark_cancel_requested();
        let out = t.finish_turn(err(-32603, "AbortError"));
        assert_eq!(done(&out).0, TurnDoneReason::Cancelled);
        assert!(!events(&out).iter().any(|e| matches!(e, Event::Error { .. })));
        // 没有 turn 在跑：什么也不发（进程退出时已经收尾）
        assert!(t.finish_turn(err(-32603, "late")).is_empty());
    }

    #[test]
    fn unfinished_tools_are_closed_when_the_turn_ends() {
        let mut t = tr();
        feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "sleep", "kind": "execute", "status": "in_progress", "rawInput": {"command": "sleep 99"}}),
                json!({"sessionUpdate": "tool_call", "toolCallId": "c2", "title": "quiet", "kind": "other", "status": "pending"}),
            ],
        );
        t.mark_cancel_requested();
        let out = t.finish_turn(PromptOutcome::StopReason("cancelled".into()));
        // c2 从没发过 tool_call：先补发，再收尾
        assert_eq!(names(&out), vec!["tool_call", "tool_result", "tool_result", "turn_done"]);
        for e in events(&out) {
            if let Event::ToolResult { is_error, output, .. } = e {
                assert!(is_error);
                assert_eq!(output, json!("Cancelled before it finished"));
            }
        }
        // 正常结束但 agent 没发完成通知：不标错误
        let mut t = tr();
        feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "x", "kind": "execute", "status": "in_progress", "rawInput": {"command": "x"}}),
            ],
        );
        let out = t.finish_turn(PromptOutcome::StopReason("end_turn".into()));
        assert!(events(&out).iter().any(|e| matches!(e, Event::ToolResult { is_error: false, .. })));
    }

    #[test]
    fn usage_carries_context_and_the_cost_delta() {
        let mut t = Translator::new("/w".into(), Some("m1".into()), true);
        let usage = |used: u64, amount: f64, currency: &str| json!({"sessionUpdate": "usage_update", "used": used, "size": 200000, "cost": {"amount": amount, "currency": currency}});
        t.begin_turn();
        feed(&mut t, &[usage(1000, 0.01, "USD"), usage(1500, 0.02, "USD")]);
        let (_, u) = done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into())));
        assert_eq!((u.context_tokens, u.context_window), (Some(1500), Some(200000)));
        assert!((u.cost_usd.unwrap() - 0.02).abs() < 1e-9);
        assert_eq!(u.model.as_deref(), Some("m1"));
        assert_eq!((u.input_tokens, u.output_tokens), (None, None));
        // 第二个 turn 的累计费用是 0.05：增量 0.03
        t.begin_turn();
        feed(&mut t, &[usage(2500, 0.05, "USD")]);
        let (_, u) = done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into())));
        assert!((u.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        // 没有 usage_update 的 turn：没有上下文也没有费用
        t.begin_turn();
        let (_, u) = done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into())));
        assert_eq!((u.context_tokens, u.cost_usd), (None, None));
        // 不是美元：不报费用
        t.begin_turn();
        feed(&mut t, &[usage(10, 9.0, "credits")]);
        assert_eq!(done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into()))).1.cost_usd, None);
        // 续接的会话不知道基线：第一个 turn 不报费用，之后的才有
        let mut t = Translator::new("/w".into(), None, false);
        t.begin_turn();
        feed(&mut t, &[usage(10, 1.00, "USD")]);
        assert_eq!(done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into()))).1.cost_usd, None);
        t.begin_turn();
        feed(&mut t, &[usage(20, 1.25, "USD")]);
        assert!(
            (done(&t.finish_turn(PromptOutcome::StopReason("end_turn".into()))).1.cost_usd.unwrap() - 0.25).abs()
                < 1e-9
        );
    }

    fn perm_params(tool_call: Value) -> Value {
        json!({"sessionId": "s", "toolCall": tool_call, "options": [
            {"optionId": "y", "name": "Allow", "kind": "allow_once"}, {"optionId": "n", "name": "Reject", "kind": "reject_once"}]})
    }

    #[test]
    fn a_permission_request_announces_its_tool_call_first() {
        let mut t = tr();
        feed(
            &mut t,
            &[
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run rm", "kind": "execute", "status": "pending", "rawInput": {}}),
            ],
        );
        let out = t.on_permission(
            json!(42),
            &perm_params(json!({"toolCallId": "c1", "rawInput": {"command": "rm -rf build"}})),
        );
        assert_eq!(names(&out), vec!["tool_call", "PERMISSION"]);
        match &events(&out)[0] {
            Event::ToolCall { input, detail, .. } => {
                assert_eq!(input, &json!({"command": "rm -rf build"}));
                assert_eq!(detail.as_ref().unwrap().command.as_deref(), Some("rm -rf build"));
            }
            other => panic!("{other:?}"),
        }
        match &out[1] {
            Out::Permission { pending, native } => {
                assert_eq!(pending.kind, PermissionKind::Tool);
                assert_eq!(native.rpc_id, json!(42));
                assert_eq!(pending.options.len(), 2);
            }
            other => panic!("{other:?}"),
        }
        // 已经发过的调用不再重复发
        let out = t.on_permission(json!(43), &perm_params(json!({"toolCallId": "c1"})));
        assert_eq!(names(&out), vec!["PERMISSION"]);
        // 一个没见过的调用：照样发 tool_call 再问
        let out =
            t.on_permission(json!(44), &perm_params(json!({"toolCallId": "c9", "title": "Write", "kind": "edit"})));
        assert_eq!(names(&out), vec!["tool_call", "PERMISSION"]);
    }

    #[test]
    fn permission_requests_with_nobody_to_answer_are_cancelled_at_once() {
        let cancelled = json!({"outcome": {"outcome": "cancelled"}});
        // 没有 turn
        let mut t = Translator::new("/w".into(), None, true);
        let out = t.on_permission(json!(1), &perm_params(json!({"toolCallId": "c1"})));
        assert_eq!(out, vec![Out::Reply { id: json!(1), result: Ok(cancelled.clone()) }]);
        // 正在取消
        let mut t = tr();
        t.mark_cancel_requested();
        let out = t.on_permission(json!(2), &perm_params(json!({"toolCallId": "c1"})));
        assert_eq!(out, vec![Out::Reply { id: json!(2), result: Ok(cancelled.clone()) }]);
        // 一个选项都没有 / 没有 toolCallId：取消，并说明
        let mut t = tr();
        let out = t.on_permission(json!(3), &json!({"toolCall": {"toolCallId": "c1"}, "options": []}));
        assert_eq!(names(&out), vec!["tool_call", "error", "REPLY"]);
        let out = t.on_permission(
            json!(4),
            &json!({"toolCall": {}, "options": [{"optionId": "y", "name": "Y", "kind": "allow_once"}]}),
        );
        assert_eq!(out, vec![Out::Reply { id: json!(4), result: Ok(cancelled) }]);
    }

    #[test]
    fn elicitations_become_questions_or_are_declined() {
        let mut t = tr();
        let form = json!({"sessionId": "s", "mode": "form", "message": "Pick one",
                          "requestedSchema": {"type": "object", "properties": {"x": {"type": "string", "enum": ["a", "b"]}}, "required": ["x"]}});
        let out = t.on_elicitation(json!("e1"), &form);
        assert_eq!(names(&out), vec!["PERMISSION"]);
        // 不认识的模式：拒绝并说明
        let out = t.on_elicitation(json!(7), &json!({"sessionId": "s", "mode": "_x", "message": "m"}));
        assert_eq!(names(&out), vec!["error", "REPLY"]);
        assert!(matches!(&out[1], Out::Reply { result: Ok(v), .. } if v == &json!({"action": "decline"})));
        // 没有 turn：decline；取消中：cancel
        let mut idle = Translator::new("/w".into(), None, true);
        assert_eq!(
            idle.on_elicitation(json!(8), &form),
            vec![Out::Reply { id: json!(8), result: Ok(json!({"action": "decline"})) }]
        );
        let mut cancelling = tr();
        cancelling.mark_cancel_requested();
        assert_eq!(
            cancelling.on_elicitation(json!(9), &form),
            vec![Out::Reply { id: json!(9), result: Ok(json!({"action": "cancel"})) }]
        );
    }

    #[test]
    fn process_exit_closes_open_things_without_a_turn_done() {
        let mut t = tr();
        feed(
            &mut t,
            &[
                text("working"),
                json!({"sessionUpdate": "tool_call", "toolCallId": "c1", "title": "x", "kind": "execute", "status": "in_progress", "rawInput": {"command": "x"}}),
            ],
        );
        let fin = t.finish_process();
        assert!(fin.died_mid_turn && !fin.cancelled);
        assert_eq!(names(&fin.out), vec!["tool_result"]);
        assert!(!t.is_busy());
        // 要求过取消 / 关闭之后退出是预期内的
        let mut t = tr();
        t.mark_cancel_requested();
        let fin = t.finish_process();
        assert!(!fin.died_mid_turn && fin.cancelled);
        // 没有 turn：什么都不是
        let fin = t.finish_process();
        assert!(!fin.died_mid_turn && !fin.cancelled && fin.out.is_empty());
    }

    #[test]
    fn turn_ids_count_up() {
        let mut t = Translator::new("/w".into(), None, true);
        for expected in 1..=3u64 {
            t.begin_turn();
            let out = t.finish_turn(PromptOutcome::StopReason("end_turn".into()));
            match events(&out).last().unwrap() {
                Event::TurnDone { turn_id, .. } => assert_eq!(*turn_id, expected),
                other => panic!("{other:?}"),
            }
        }
    }
}
