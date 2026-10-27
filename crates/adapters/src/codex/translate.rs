//! app-server 的通知 / 服务端请求 → [`AgentEvent`]（docs/spec/daemon.md § 8 的 Codex 一条）。
//!
//! 纯同步状态机：`feed_*` 进、`Vec<Out>` 出，不碰进程和 IO，所以可以直接拿录制的 JSONL 回放测试。要点：
//! - 未知通知、未知 item 类型丢弃并 `debug!`
//! - `item/started` → `tool_call`（命令 / 补丁 / MCP / 搜索…），`item/commandExecution/outputDelta` → `tool_output_delta`，
//!   `item/completed` → `tool_result`（`declined` / `failed` / 退出码非 0 视为 `is_error`；补丁另发 `file_changed`）
//! - 文本 / 思考按 item 流式：`item/agentMessage/delta`、`item/reasoning/summaryTextDelta` + `textDelta`；
//!   摘要的第 2 段起前面补一个空行；没有任何文本的思考块发一条空的 `thinking_delta`（和 Claude Code 一致）
//! - **被打断 / 失败的 turn 不会为进行中的 item 发 `item/completed`**：`turn/completed` 时由这里补上 `text_done` /
//!   `thinking_done` / 带 `is_error` 的 `tool_result`，否则客户端的转圈停不下来
//! - `turn/completed` → `turn_done`（`interrupted` → `cancelled`，`failed` → `error`，失败时先发一条 `error` 事件，除非
//!   `error` 通知已经发过）；用量是这个 turn 里每次 `thread/tokenUsage/updated` 的 `last` 之和（`total` 是整个线程的累计）。
//!   `input_tokens` 不含缓存命中（OpenAI 的 `inputTokens` 包含 `cachedInputTokens`，这里减掉，与 Claude Code 的口径一致，
//!   这样 `input + cache_read` 不会重复计数），没有费用。`context_window` 取 `modelContextWindow`；`context_tokens` 取
//!   **最近一次** `last` 的 `inputTokens`（已含缓存命中，所以不再加 `cachedInputTokens`）；压缩上下文后那条更新的 `inputTokens`
//!   是 0、只有 `totalTokens` 带着压缩后的估计，此时取 `totalTokens`
//! - **线程路由**：app-server 把子 agent（`subAgentActivity` / `collabAgentToolCall` 启动的 thread）的通知发在**同一条连接**上，
//!   带各自的 `threadId`。主 thread 之外的通知**绝不**碰主 turn 的状态（子 agent 的 `turn/completed` 不会结束主 turn、
//!   用量不进主 turn、`error` 不算主 turn 的错、`plan` 不覆盖主计划），它们的文本 / 思考不进主流（和 Claude Code 一致），
//!   工具调用带 `parent_id`（启动这个子 agent 的那次调用）；子 agent 的状态以 `task` 快照（`kind = subagent`，`id` = 子 thread id）发出
//! - 提示：`warning` / `guardianWarning` / `configWarning` / `deprecationNotice` / `mcpServer/startupStatus/updated`（失败）/
//!   `model/rerouted` / `item/autoApprovalReview/completed`（拒绝）/ `hook/completed`（失败）→ `notice`（同样的文字每个进程只发一次，
//!   子 agent 的 thread 会把启动时的警告再发一遍）；`contextCompaction` item 开始 → `activity { compacting }`，结束 →
//!   `activity { none }` + `notice { context_compacted }`
//! - `error { willRetry: true }`（重连中）发一条可恢复的 `error` 事件（`agent_reconnecting`），不收尾；`willRetry: false`
//!   发 `error` 事件，随后的 `turn/completed { status: failed }` 收尾
//! - 审批 / 提问 / MCP elicitation 请求 → `Out::Permission`（见 `permissions.rs`）；不支持的服务端请求按 schema 回拒绝并发一条可见的 `error`
//!   （`unsupported_request`；交不给用户的 MCP elicitation 是 `elicitation_rejected`）；会话正在取消时新来的审批直接回 `cancel`

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anycon_agent_api::{AgentEvent, PendingPermission, PermissionDecision};
use anycon_protocol::{
    ActivityState, Event, FileChangeKind, NoticeLevel, PlanEntry, PlanStatus, TaskKind, TaskStatus, TaskUsage,
    ToolCategory, ToolResultDetail, TurnDoneReason, Usage,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use super::permissions::{self, NativePending};
use super::rpc::{self, Incoming, RpcError};
use super::tools::{self, ToolCallParts};
use super::wire::{
    AutoApprovalReviewCompleted, CommandApproval, ConfigWarning, DeltaNotification, DeprecationNotice,
    ErrorNotification, FileApproval, HookCompleted, ItemNotification, ItemStatus, McpElicitation, McpServerStatus,
    ModelRerouted, PatchChangeKind, PermissionsApproval, PlanUpdated, ServerRequestResolved, SummaryPartAdded,
    ThreadItem, TokenUsageNotification, Turn, TurnError, TurnNotification, TurnStatus, UserInputRequest,
    WarningNotification,
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

/// 一个子 agent（主 thread 启动的另一个 thread）。
struct Child {
    /// 给人看的名字：`agentPath` 去掉 `/root/` 的部分（`/root/pong` → `pong`），没有就用启动它的 prompt 的第一行；
    /// 都没有（只见过它的通知）时为 `None`，显示 `sub-agent <thread id 前 8 位>`。
    name: Option<String>,
    /// 启动它的那次调用（`subAgentActivity` 的 id / `collabAgentToolCall` 的 id），子 agent 里的工具调用的 `parent_id`。
    tool_id: Option<String>,
    status: TaskStatus,
    tool_calls: u64,
    last_tool: Option<String>,
    /// 它最近一条完整的回复（运行中是它说到哪了，结束后是结果）。
    summary: Option<String>,
    /// 子 thread 累计用掉的 token（`total.totalTokens`）。
    total_tokens: Option<u64>,
    /// 它最近一个 turn 的耗时（turn 结束时才知道）。
    duration_ms: Option<u64>,
    /// 上一次发出去的快照：没变就不再发。
    last_snapshot: Option<Event>,
}

impl Child {
    fn new(name: Option<String>, tool_id: Option<String>) -> Self {
        Self {
            name,
            tool_id,
            status: TaskStatus::Running,
            tool_calls: 0,
            last_tool: None,
            summary: None,
            total_tokens: None,
            duration_ms: None,
            last_snapshot: None,
        }
    }

    /// 给人看的名字。
    fn title(&self, thread_id: &str) -> String {
        match &self.name {
            Some(n) => n.clone(),
            None => format!("sub-agent {}", short_id(thread_id)),
        }
    }
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
    /// 主 thread 的 id。`handle` 在握手后告诉我们；回放时没人告诉，就从第一个我们自己的 turn 的 `turn/started` 学。
    /// 还不知道时所有通知都算主 thread 的。
    main_thread: Option<String>,
    /// 子 agent，key 是子 thread 的 id。
    children: HashMap<String, Child>,
    /// 子 agent 里还没完成的工具调用 → 它所在的 thread（子 agent 的 turn 结束时据此收尾）。
    tool_threads: HashMap<String, String>,
    /// 已经发过的提示（文字 / `mcp:<server>`）：同一条每个进程只发一次。
    seen_notices: HashSet<String>,
    /// 模型的上下文窗口（跨 turn 保留）。
    context_window: Option<u64>,
    /// 当前 turn 里最近一次模型请求之后上下文的占用。
    context_tokens: Option<u64>,
    /// 上次通过 `Background` 报告的数目（还在跑的子 agent 数）。
    bg_reported: u32,
}

fn push(out: &mut Vec<Out>, e: Event) {
    out.push(Out::Event(AgentEvent::Event(e)));
}

fn item_id(item: &ThreadItem) -> Option<&str> {
    match item {
        ThreadItem::AgentMessage { id, .. }
        | ThreadItem::Reasoning { id, .. }
        | ThreadItem::Plan { id, .. }
        | ThreadItem::SubAgentActivity { id, .. }
        | ThreadItem::ContextCompaction { id, .. }
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

/// 记住多少条已经提示过的文字。
const MAX_SEEN_NOTICES: usize = 256;

/// 子 agent 摘要的上限（daemon 也会截，这里先截，免得每个快照都带一大段）。
const MAX_SUMMARY_BYTES: usize = 4096;

fn clip_summary(s: &str) -> String {
    crate::common::truncate_bytes(s, MAX_SUMMARY_BYTES)
}

fn activity(state: ActivityState) -> Event {
    Event::Activity { state, attempt: None, max_attempts: None, delay_ms: None, reason: None }
}

/// 子 thread id 的前 8 位（没有名字时的标识）。
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// `/root/pong` → `pong`；嵌套的 `/root/a/b` → `a/b`；没有路径返回 `None`。
fn agent_name(path: &str) -> Option<String> {
    let p = path.trim().trim_start_matches('/');
    let p = p.strip_prefix("root/").unwrap_or(p);
    (!p.is_empty() && p != "root").then(|| p.to_string())
}

/// `summary` + 可选的 `details`（空行隔开）。
fn join_details(summary: &str, details: Option<&str>) -> String {
    match details.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{}\n\n{d}", summary.trim()),
        None => summary.trim().to_string(),
    }
}

/// `agentsStates` 里的状态 → task 状态；不认识的返回 `None`（不动）。
fn collab_status(s: &str) -> Option<TaskStatus> {
    match s {
        "pendingInit" | "running" => Some(TaskStatus::Running),
        "completed" => Some(TaskStatus::Completed),
        "errored" => Some(TaskStatus::Failed),
        "interrupted" | "shutdown" | "notFound" => Some(TaskStatus::Cancelled),
        _ => None,
    }
}

/// 给人看的状态词。
fn collab_status_word(s: &str) -> &str {
    match s {
        "pendingInit" => "starting",
        "errored" => "failed",
        "shutdown" => "closed",
        "notFound" => "not found",
        other => other,
    }
}

/// 自动审查被拒绝的动作的一句话描述（给 notice 的 `message` 打头）。
fn review_action_summary(action: &Value) -> Option<String> {
    let s = |k: &str| action.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let text = match action.get("type").and_then(Value::as_str)? {
        "command" => s("command")?.to_string(),
        "execve" => {
            let argv: Vec<&str> = action.get("argv")?.as_array()?.iter().filter_map(Value::as_str).collect();
            if argv.is_empty() {
                s("program")?.to_string()
            } else {
                argv.join(" ")
            }
        }
        "applyPatch" => {
            let files: Vec<&str> = action.get("files")?.as_array()?.iter().filter_map(Value::as_str).collect();
            if files.is_empty() {
                return None;
            }
            format!("Edit {}", files.join(", "))
        }
        "networkAccess" => format!("Network access to {}", s("host").or_else(|| s("target"))?),
        "mcpToolCall" => format!("MCP tool {}/{}", s("server")?, s("toolName")?),
        _ => return None,
    };
    Some(tools::first_line_clipped(&text, 160))
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
            main_thread: None,
            children: HashMap::new(),
            tool_threads: HashMap::new(),
            seen_notices: HashSet::new(),
            context_window: None,
            context_tokens: None,
            bg_reported: 0,
        }
    }

    /// 握手拿到主 thread 的 id 之后调用：此后其它 thread 的通知都不碰主 turn。
    pub fn set_main_thread(&mut self, id: &str) {
        self.main_thread = Some(id.to_string());
    }

    /// `send` 之前调用：开始新 turn。
    pub fn begin_turn(&mut self) {
        self.turn_counter += 1;
        self.busy = true;
        self.cancel_requested = false;
        self.codex_turn = None;
        self.turn_started = Some(Instant::now());
        self.usage = UsageAcc::default();
        self.context_tokens = None;
        self.error_emitted = false;
        // item 不会跨 turn，清掉各种表，长期保活的会话里不会无限增长
        self.texts.clear();
        self.text_order.clear();
        self.thinks.clear();
        self.think_order.clear();
        self.tools.clear();
        self.tool_order.clear();
        self.tool_threads.clear();
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
        let mut out = self.notification(method, params);
        self.report_background(&mut out);
        out
    }

    /// 还在跑的子 agent 数（`task` 快照是 `running` 的）有变化就发 `Background`（含降到 0）：daemon 据此不按 `idle_timeout`
    /// 回收进程——回收会连同还在跑的子 agent 一起杀掉。所有改子 agent 状态的通知都经过 `feed_notification`，所以在它的末尾统一对一次数，
    /// 不用在每个改状态的地方记着发。降回 0 靠子 agent 自己的收尾（`turn/completed`、`subAgentActivity`、`agentsStates`、
    /// `thread/closed`）和主 turn 被取消时的统一收尾（`on_turn_completed`）；进程退出由 daemon 清零。
    fn report_background(&mut self, out: &mut Vec<Out>) {
        let running = self.children.values().filter(|c| c.status == TaskStatus::Running).count();
        let n = u32::try_from(running).unwrap_or(u32::MAX);
        if n != self.bg_reported {
            self.bg_reported = n;
            out.push(Out::Event(AgentEvent::Background { tasks: n }));
        }
    }

    fn notification(&mut self, method: &str, params: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        match method {
            "turn/started" => {
                if let Some(n) = parse::<TurnNotification>(method, params) {
                    if self.is_main(n.thread_id.as_deref()) {
                        self.learn_main_thread(n.thread_id.as_deref());
                        self.on_turn_started(&n.turn, &mut out);
                    } else if let Some(t) = n.thread_id.as_deref() {
                        self.on_child_turn_started(t, &mut out);
                    }
                }
            }
            "turn/completed" => {
                if let Some(n) = parse::<TurnNotification>(method, params) {
                    if self.is_main(n.thread_id.as_deref()) {
                        self.on_turn_completed(&n.turn, &mut out);
                    } else if let Some(t) = n.thread_id.as_deref() {
                        self.on_child_turn_completed(t, &n.turn, &mut out);
                    }
                }
            }
            "item/started" => {
                if let Some(n) = parse::<ItemNotification>(method, params) {
                    self.on_item(&n.item, false, n.thread_id.as_deref(), &mut out);
                }
            }
            "item/completed" => {
                if let Some(n) = parse::<ItemNotification>(method, params) {
                    self.on_item(&n.item, true, n.thread_id.as_deref(), &mut out);
                }
            }
            "item/agentMessage/delta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    // 子 agent 的文本不进主流
                    if self.is_main(n.thread_id.as_deref()) {
                        self.on_text_delta(n, &mut out);
                    }
                }
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    if self.is_main(n.thread_id.as_deref()) {
                        self.on_thinking_delta(n, &mut out);
                    }
                }
            }
            "item/reasoning/summaryPartAdded" => {
                if let Some(n) = parse::<SummaryPartAdded>(method, params) {
                    if n.summary_index > 0 && self.is_main(n.thread_id.as_deref()) {
                        if let Some(t) = self.thinks.get_mut(&n.item_id) {
                            t.separate_next = t.emitted;
                        }
                    }
                }
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                if let Some(n) = parse::<DeltaNotification>(method, params) {
                    // 子 agent 的工具调用只在它的 `tool_call` 发出去之后才有输出可附
                    let known = self.is_main(n.thread_id.as_deref()) || self.tools.contains_key(&n.item_id);
                    if known && !n.delta.is_empty() {
                        push(&mut out, Event::ToolOutputDelta { id: n.item_id, text: n.delta });
                    }
                }
            }
            "turn/plan/updated" => {
                if let Some(n) = parse::<PlanUpdated>(method, params) {
                    // 子 agent 自己的计划不能顶掉主 agent 的
                    if self.is_main(n.thread_id.as_deref()) {
                        let entries = n
                            .plan
                            .into_iter()
                            .map(|s| PlanEntry { content: s.step, status: plan_status(&s.status) })
                            .collect();
                        push(&mut out, Event::Plan { entries });
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                if let Some(n) = parse::<TokenUsageNotification>(method, params) {
                    if self.is_main(n.thread_id.as_deref()) {
                        self.on_token_usage(&n);
                    } else if let Some(t) = n.thread_id.as_deref() {
                        self.on_child_token_usage(t, &n, &mut out);
                    }
                }
            }
            "error" => {
                if let Some(n) = parse::<ErrorNotification>(method, params) {
                    if self.is_main(n.thread_id.as_deref()) {
                        self.on_error(&n, &mut out);
                    } else {
                        // 子 agent 的错误由它自己的 `turn/completed`（failed）反映到它的 task 上，不算主 turn 的
                        tracing::debug!(thread = ?n.thread_id, message = %n.error.message, "codex: a sub-agent reported an error");
                    }
                }
            }
            "serverRequest/resolved" => {
                if let Some(n) = parse::<ServerRequestResolved>(method, params) {
                    out.push(Out::Resolved { rpc_id: n.request_id });
                }
            }
            "model/rerouted" => {
                if let Some(n) = parse::<ModelRerouted>(method, params) {
                    self.on_model_rerouted(&n, &mut out);
                }
            }
            "warning" | "guardianWarning" => {
                if let Some(n) = parse::<WarningNotification>(method, params) {
                    self.notice_once(&mut out, "agent_notice", NoticeLevel::Warning, n.message.trim(), None);
                }
            }
            "configWarning" => {
                if let Some(n) = parse::<ConfigWarning>(method, params) {
                    let message = join_details(&n.summary, n.details.as_deref());
                    self.notice_once(&mut out, "config_warning", NoticeLevel::Warning, &message, None);
                }
            }
            "deprecationNotice" => {
                if let Some(n) = parse::<DeprecationNotice>(method, params) {
                    let message = join_details(&n.summary, n.details.as_deref());
                    self.notice_once(&mut out, "deprecation", NoticeLevel::Warning, &message, None);
                }
            }
            "mcpServer/startupStatus/updated" => {
                if let Some(n) = parse::<McpServerStatus>(method, params) {
                    self.on_mcp_status(&n, &mut out);
                }
            }
            "item/autoApprovalReview/completed" => {
                if let Some(n) = parse::<AutoApprovalReviewCompleted>(method, params) {
                    self.on_auto_review(&n, &mut out);
                }
            }
            "thread/closed" => {
                // 子 agent 的 thread 被关了：还没收尾的任务不会再有后续
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct Closed {
                    thread_id: String,
                }
                if let Some(n) = parse::<Closed>(method, params) {
                    if self.children.get(&n.thread_id).is_some_and(|c| c.status == TaskStatus::Running) {
                        self.set_child_status(&n.thread_id, TaskStatus::Cancelled, &mut out);
                    }
                }
            }
            "hook/completed" => {
                if let Some(n) = parse::<HookCompleted>(method, params) {
                    self.on_hook_completed(&n, &mut out);
                }
            }
            other => tracing::debug!(method = other, "codex: ignoring a notification"),
        }
        out
    }

    // ---------- 线程路由 ----------

    /// 这条消息属于主 thread 吗。没有 `threadId`（老格式）或还不知道主 thread 是谁时都算主 thread 的。
    fn is_main(&self, thread_id: Option<&str>) -> bool {
        match (thread_id, self.main_thread.as_deref()) {
            (Some(t), Some(main)) => t == main,
            _ => true,
        }
    }

    /// 没人告诉过我们主 thread 是谁（回放）：我们自己开的 turn 的 `turn/started` 所在的 thread 就是。
    fn learn_main_thread(&mut self, thread_id: Option<&str>) {
        if self.main_thread.is_none() && self.busy {
            self.main_thread = thread_id.map(str::to_string);
        }
    }

    /// 非主 thread 里的消息，要挂在哪个调用下面：启动那个子 agent 的调用。
    fn parent_of(&self, thread_id: Option<&str>) -> Option<String> {
        let t = thread_id.filter(|t| !self.is_main(Some(t)))?;
        self.children.get(t).and_then(|c| c.tool_id.clone())
    }

    /// 非主 thread 里的请求（审批 / 提问）要让用户看得出是哪个子 agent 在问：返回它的显示名。
    fn child_label(&self, thread_id: Option<&str>) -> Option<String> {
        let t = thread_id.filter(|t| !self.is_main(Some(t)))?;
        Some(self.children.get(t).and_then(|c| c.name.clone()).unwrap_or_else(|| short_id(t)))
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

        // 用户取消了主 turn：还在跑的子 agent 一并算取消（Stop 的意思是都停；子 agent 若其实还在跑，它自己后面的通知会把状态改回来）。
        // 不这样的话，Codex 要是没有给它们发收尾通知，`Background` 就永远降不回 0，进程不会被回收
        if reason == TurnDoneReason::Cancelled {
            self.cancel_running_children(out);
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
            context_window: self.context_window,
            context_tokens: self.context_tokens,
        };
        self.busy = false;
        self.cancel_requested = false;
        self.codex_turn = None;
        push(out, Event::TurnDone { turn_id: self.turn_counter, reason, usage: Some(usage) });
    }

    /// 还在跑的子 agent 全部标成 cancelled（它们没完成的工具调用补一条出错的结果）。
    fn cancel_running_children(&mut self, out: &mut Vec<Out>) {
        let mut running: Vec<String> =
            self.children.iter().filter(|(_, c)| c.status == TaskStatus::Running).map(|(t, _)| t.clone()).collect();
        running.sort();
        for t in running {
            self.close_thread_tools(&t, out, "The sub-agent was stopped before this finished");
            self.set_child_status(&t, TaskStatus::Cancelled, out);
        }
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
        // 窗口大小是模型的属性，resume 重放的用量里也一样可信
        if let Some(w) = n.token_usage.model_context_window.filter(|w| *w > 0) {
            self.context_window = Some(w);
        }
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
        // 上下文占用 = 最近一次模型请求的输入（`inputTokens` 已含缓存命中）。压缩上下文之后那条更新的 `inputTokens` 是 0，
        // 压缩后的估计只在 `totalTokens` 里
        let ctx = if l.input_tokens > 0 { l.input_tokens } else { l.total_tokens };
        if ctx > 0 {
            self.context_tokens = Some(ctx);
        }
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

    fn on_item(&mut self, raw: &Value, completed: bool, thread_id: Option<&str>, out: &mut Vec<Out>) {
        let item = match serde_json::from_value::<ThreadItem>(raw.clone()) {
            Ok(i) => i,
            Err(e) => {
                tracing::debug!(error = %e, item_type = ThreadItem::raw_type(raw), "codex: ignoring an item with an unexpected shape");
                return;
            }
        };
        let main = self.is_main(thread_id);
        match &item {
            ThreadItem::AgentMessage { id, text, .. } => {
                if !main {
                    // 子 agent 的文本不进主流；它的回复记作它的 task 摘要
                    if completed {
                        self.on_child_message(thread_id, text, out);
                    }
                } else if completed {
                    self.finish_text(id, text, out);
                } else {
                    self.text_state(id);
                }
            }
            ThreadItem::Reasoning { id, summary, content } => {
                if !main {
                    // 同上：子 agent 的思考不进主流
                } else if completed {
                    self.finish_thinking(id, summary, content, out);
                } else {
                    self.think_state(id);
                }
            }
            ThreadItem::UserMessage {} | ThreadItem::Plan { .. } => {}
            ThreadItem::ContextCompaction { .. } => {
                // 临时状态只在 turn 进行中有意义（客户端在 `turn_done` 时清掉它）；压缩完成的提示不挑时候
                if main && completed {
                    if self.busy {
                        push(out, activity(ActivityState::None));
                    }
                    self.push_notice(out, "context_compacted", NoticeLevel::Info, "", None);
                } else if main && self.busy {
                    push(out, activity(ActivityState::Compacting));
                }
            }
            ThreadItem::SubAgentActivity { .. } => self.on_sub_agent_activity(&item, completed, thread_id, out),
            ThreadItem::Other => {
                tracing::debug!(item_type = ThreadItem::raw_type(raw), "codex: ignoring an item of an unknown type");
            }
            ThreadItem::CollabAgentToolCall { .. } => {
                // 先登记接收方，`tool_result` 里才有它们的名字；`task` 快照排在 `tool_call` 之后
                self.register_collab_children(&item);
                self.on_tool_item(&item, completed, thread_id, out);
                self.on_collab_states(&item, out);
            }
            tool => self.on_tool_item(tool, completed, thread_id, out),
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

    fn on_tool_item(&mut self, item: &ThreadItem, completed: bool, thread_id: Option<&str>, out: &mut Vec<Out>) {
        let Some(id) = item_id(item).map(str::to_string) else { return };
        let main = self.is_main(thread_id);
        // 子 agent 的工具调用只在主 turn 进行中才展示（turn 之外发出去的 `tool_call` 没有地方收尾），
        // 而且只展示登记过的子 agent 的
        if !main && (!self.busy || !thread_id.is_some_and(|t| self.children.contains_key(t))) {
            return;
        }
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
                    parent_id: self.parent_of(thread_id),
                },
            );
            if let Some(t) = thread_id.filter(|_| !main) {
                self.tool_threads.insert(id.clone(), t.to_string());
                if let Some(c) = self.children.get_mut(t) {
                    c.tool_calls += 1;
                    c.last_tool = Some(parts.name.clone());
                }
                self.emit_task(t, out);
            }
            self.tool_order.push(id.clone());
            self.tools.insert(id.clone(), parts);
        }
        if !completed {
            return;
        }
        self.tools.remove(&id);
        self.tool_threads.remove(&id);
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
                let output = Value::String(self.collab_output(agents_states));
                push(out, Event::ToolResult { id, output, is_error, detail: None });
            }
            ThreadItem::SubAgentActivity { agent_thread_id, agent_path, model, .. } => {
                let name = self
                    .children
                    .get(agent_thread_id)
                    .map(|c| c.title(agent_thread_id))
                    .or_else(|| agent_name(agent_path))
                    .unwrap_or_else(|| format!("sub-agent {}", short_id(agent_thread_id)));
                let mut text = format!("Started sub-agent {name}");
                if let Some(m) = model.as_deref().filter(|m| !m.is_empty()) {
                    text.push_str(&format!(" ({m})"));
                }
                push(out, Event::ToolResult { id, output: Value::String(text), is_error: false, detail: None });
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

    // ---------- 子 agent ----------

    /// 子 thread 的登记项（没见过就新建）。只给**明确说了"这是子 agent"的消息**用（`subAgentActivity`、
    /// `collabAgentToolCall` 的接收方 / `agentsStates`）；光是某个 thread 发来了通知不算：别的内部 thread（比如自动审批的
    /// 审查 thread）也可能出现在这条连接上，它们有各自的通知，不该冒充子 agent。
    fn child_entry(&mut self, thread_id: &str) -> &mut Child {
        self.children.entry(thread_id.to_string()).or_insert_with(|| Child::new(None, None))
    }

    /// 发这个子 agent 的 `task` 快照（和上一次发的一样就不发）。
    fn emit_task(&mut self, thread_id: &str, out: &mut Vec<Out>) {
        let Some(c) = self.children.get_mut(thread_id) else { return };
        let snapshot = Event::Task {
            id: thread_id.to_string(),
            kind: TaskKind::Subagent,
            tool_id: c.tool_id.clone(),
            title: c.title(thread_id),
            status: c.status,
            usage: Some(TaskUsage {
                total_tokens: c.total_tokens,
                tool_calls: Some(c.tool_calls),
                duration_ms: c.duration_ms,
            }),
            last_tool: c.last_tool.clone(),
            summary: c.summary.clone(),
            phases: Vec::new(),
            agents: Vec::new(),
        };
        if c.last_snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        c.last_snapshot = Some(snapshot.clone());
        push(out, snapshot);
    }

    fn set_child_status(&mut self, thread_id: &str, status: TaskStatus, out: &mut Vec<Out>) {
        let c = self.child_entry(thread_id);
        c.status = status;
        if status == TaskStatus::Running {
            // 又开始跑了：上一个 turn 的耗时不再适用
            c.duration_ms = None;
        }
        self.emit_task(thread_id, out);
    }

    fn on_child_turn_started(&mut self, thread_id: &str, out: &mut Vec<Out>) {
        // 子 agent 的 turn 不是主 turn：不动主 turn 的任何状态
        if self.children.contains_key(thread_id) {
            self.set_child_status(thread_id, TaskStatus::Running, out);
        } else {
            tracing::debug!(thread = thread_id, "codex: a turn started in a thread that is not a known sub-agent");
        }
    }

    fn on_child_turn_completed(&mut self, thread_id: &str, turn: &Turn, out: &mut Vec<Out>) {
        let Some(c) = self.children.get_mut(thread_id) else {
            tracing::debug!(thread = thread_id, "codex: a turn ended in a thread that is not a known sub-agent");
            return;
        };
        let status = match turn.status {
            TurnStatus::Completed => TaskStatus::Completed,
            TurnStatus::Interrupted => TaskStatus::Cancelled,
            TurnStatus::Failed | TurnStatus::Unknown => TaskStatus::Failed,
            TurnStatus::InProgress => TaskStatus::Running,
        };
        c.status = status;
        c.duration_ms = turn.duration_ms.filter(|d| *d >= 0).map(|d| d as u64);
        if status == TaskStatus::Failed {
            if let Some(e) = &turn.error {
                c.summary = Some(clip_summary(&error_message(e)));
            }
        }
        self.close_thread_tools(thread_id, out, "The sub-agent's turn ended before this finished");
        self.emit_task(thread_id, out);
    }

    fn on_child_token_usage(&mut self, thread_id: &str, n: &TokenUsageNotification, out: &mut Vec<Out>) {
        let Some(c) = self.children.get_mut(thread_id) else { return };
        c.total_tokens = Some(n.token_usage.total.total_tokens);
        self.emit_task(thread_id, out);
    }

    /// 子 agent 说了一段完整的话：记作它的摘要。
    fn on_child_message(&mut self, thread_id: Option<&str>, text: &str, out: &mut Vec<Out>) {
        let (Some(t), text) = (thread_id, text.trim()) else { return };
        if text.is_empty() {
            return;
        }
        let Some(c) = self.children.get_mut(t) else { return };
        c.summary = Some(clip_summary(text));
        self.emit_task(t, out);
    }

    /// 这个 thread 里还没完成的工具调用补一条出错的结果（子 agent 的 turn 结束了）。
    fn close_thread_tools(&mut self, thread_id: &str, out: &mut Vec<Out>, why: &str) {
        let open: Vec<String> = self
            .tool_order
            .iter()
            .filter(|id| self.tool_threads.get(*id).is_some_and(|t| t == thread_id))
            .cloned()
            .collect();
        for id in open {
            self.tool_order.retain(|x| x != &id);
            self.tool_threads.remove(&id);
            if self.tools.remove(&id).is_some() {
                push(
                    out,
                    Event::ToolResult { id, output: Value::String(why.to_string()), is_error: true, detail: None },
                );
            }
        }
    }

    /// `subAgentActivity`：`started` 是启动子 agent（它同时是模型那次调用的"工具调用"：发 `tool_call` / `tool_result`，
    /// 子 agent 里的调用以它为 `parent_id`），其余三种只是状态标记。
    fn on_sub_agent_activity(
        &mut self,
        item: &ThreadItem,
        completed: bool,
        thread_id: Option<&str>,
        out: &mut Vec<Out>,
    ) {
        let ThreadItem::SubAgentActivity { id, kind, agent_thread_id, agent_path, .. } = item else { return };
        if agent_thread_id.is_empty() {
            return;
        }
        match kind.as_str() {
            "started" => {
                let name = agent_name(agent_path);
                let c = self.children.entry(agent_thread_id.clone()).or_insert_with(|| Child::new(None, None));
                if name.is_some() {
                    c.name = name;
                }
                c.tool_id = Some(id.clone());
                c.status = TaskStatus::Running;
                // 先有 `tool_call` 再有挂在它上面的 `task`
                self.on_tool_item(item, completed, thread_id, out);
                self.emit_task(agent_thread_id, out);
            }
            // 给已有的子 agent 发了新消息：它又在跑了
            "interacted" => self.set_child_status(agent_thread_id, TaskStatus::Running, out),
            "interrupted" => self.set_child_status(agent_thread_id, TaskStatus::Cancelled, out),
            "completed" => self.set_child_status(agent_thread_id, TaskStatus::Completed, out),
            other => tracing::debug!(kind = other, "codex: ignoring a sub-agent activity of an unknown kind"),
        }
    }

    /// `collabAgentToolCall` 启动（`spawnAgent`）时把接收方 thread 登记为子 agent：名字取 prompt 的第一行，
    /// 子 agent 里的调用以这次调用为 `parent_id`。
    fn register_collab_children(&mut self, item: &ThreadItem) {
        let ThreadItem::CollabAgentToolCall { id, tool, prompt, receiver_thread_ids, .. } = item else { return };
        if tool != "spawnAgent" {
            return;
        }
        for t in receiver_thread_ids {
            let name = prompt.as_deref().filter(|p| !p.trim().is_empty()).map(|p| tools::first_line_clipped(p, 80));
            let c = self.children.entry(t.clone()).or_insert_with(|| Child::new(None, None));
            if c.name.is_none() {
                c.name = name;
            }
            if c.tool_id.is_none() {
                c.tool_id = Some(id.clone());
            }
        }
    }

    /// `collabAgentToolCall`：接收方的 `task` 快照，以及 `agentsStates` 给出的各子 agent 的最新状态 / 消息。
    fn on_collab_states(&mut self, item: &ThreadItem, out: &mut Vec<Out>) {
        let ThreadItem::CollabAgentToolCall { tool, receiver_thread_ids, agents_states, .. } = item else { return };
        if tool == "spawnAgent" {
            for t in receiver_thread_ids {
                self.emit_task(t, out);
            }
        }
        let Some(states) = agents_states.as_object() else { return };
        for (t, st) in states {
            let Some(status) = st.get("status").and_then(Value::as_str).and_then(collab_status) else { continue };
            let message = st.get("message").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty());
            let c = self.child_entry(t);
            c.status = status;
            if let Some(m) = message {
                c.summary = Some(clip_summary(m));
            }
            self.emit_task(t, out);
        }
    }

    /// `collabAgentToolCall` 的结果：每个子 agent 一段「名字: 状态」，有消息的跟在后面。
    fn collab_output(&self, agents_states: &Value) -> String {
        let Some(states) = agents_states.as_object() else { return String::new() };
        states
            .iter()
            .map(|(t, st)| {
                let name =
                    self.children.get(t).map(|c| c.title(t)).unwrap_or_else(|| format!("sub-agent {}", short_id(t)));
                let status = st.get("status").and_then(Value::as_str).map(collab_status_word).unwrap_or("unknown");
                match st.get("message").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()) {
                    Some(m) => format!("{name}: {status}\n{m}"),
                    None => format!("{name}: {status}"),
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    // ---------- 提示 ----------

    fn push_notice(
        &self,
        out: &mut Vec<Out>,
        code: &str,
        level: NoticeLevel,
        message: &str,
        models: Option<(&str, &str)>,
    ) {
        push(
            out,
            Event::Notice {
                code: code.to_string(),
                level,
                message: message.to_string(),
                model_from: models.map(|(f, _)| f.to_string()).filter(|f| !f.is_empty()),
                model_to: models.map(|(_, t)| t.to_string()).filter(|t| !t.is_empty()),
                resets_at_ms: None,
            },
        );
    }

    /// 同样的文字每个进程只发一次：`warning` 和 `configWarning` 会各发一遍启动时的配置警告，每个子 agent 的 thread 又会
    /// 再发一遍。空的不发。
    fn notice_once(
        &mut self,
        out: &mut Vec<Out>,
        code: &str,
        level: NoticeLevel,
        message: &str,
        models: Option<(&str, &str)>,
    ) {
        if message.trim().is_empty() || !self.mark_seen(message) {
            return;
        }
        self.push_notice(out, code, level, message, models);
    }

    /// 记下这段文字已经提示过；返回它是不是第一次出现。记的条数有上限（超过就从头记），免得长寿的进程里无限增长。
    fn mark_seen(&mut self, key: &str) -> bool {
        if self.seen_notices.len() >= MAX_SEEN_NOTICES {
            self.seen_notices.clear();
        }
        self.seen_notices.insert(key.to_string())
    }

    fn on_model_rerouted(&mut self, n: &ModelRerouted, out: &mut Vec<Out>) {
        let to = n.to_model.as_deref().filter(|m| !m.is_empty());
        // 主 thread 的模型变了，之后的 `turn_done.usage.model` 才是真的；子 agent 的不算
        if let Some(m) = to.filter(|_| self.is_main(n.thread_id.as_deref())) {
            tracing::info!(model = %m, "codex: the model was rerouted");
            self.model = Some(m.to_string());
        }
        let from = n.from_model.as_deref().unwrap_or("");
        let why = match n.reason.as_deref() {
            Some("highRiskCyberActivity") => " because high-risk cyber activity was detected".to_string(),
            Some(r) if !r.is_empty() => format!(" ({r})"),
            _ => String::new(),
        };
        let message = match (from.is_empty(), to) {
            (false, Some(t)) => format!("Codex rerouted this request from {from} to {t}{why}"),
            (true, Some(t)) => format!("Codex rerouted this request to {t}{why}"),
            _ => format!("Codex rerouted this request to a different model{why}"),
        };
        self.push_notice(out, "model_fallback", NoticeLevel::Warning, &message, Some((from, to.unwrap_or(""))));
    }

    fn on_mcp_status(&mut self, n: &McpServerStatus, out: &mut Vec<Out>) {
        if n.status != "failed" {
            return;
        }
        // 每个 server 每个进程只提示一次（子 agent 的 thread 会再启动一遍）
        if !self.mark_seen(&format!("mcp:{}", n.name)) {
            return;
        }
        let mut message = match n.error.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            Some(e) => e.to_string(),
            None => format!("MCP server {} failed to start", n.name),
        };
        if n.failure_reason.as_deref() == Some("reauthenticationRequired") {
            message.push_str(" (re-authentication is required)");
        }
        self.push_notice(out, "mcp_server_failed", NoticeLevel::Warning, &message, None);
    }

    fn on_auto_review(&mut self, n: &AutoApprovalReviewCompleted, out: &mut Vec<Out>) {
        let fallback = match n.review.status.as_str() {
            "denied" => "The automatic approval review denied this action.",
            "timedOut" => "The automatic approval review timed out, so the action was not approved.",
            _ => return,
        };
        let what = review_action_summary(&n.action);
        let message = match (n.review.rationale.as_deref().map(str::trim).filter(|r| !r.is_empty()), what) {
            (Some(r), Some(w)) => format!("{w}: {r}"),
            (Some(r), None) => r.to_string(),
            (None, Some(w)) => format!("{w}: {fallback}"),
            (None, None) => fallback.to_string(),
        };
        self.push_notice(out, "auto_review", NoticeLevel::Warning, &message, None);
    }

    fn on_hook_completed(&mut self, n: &HookCompleted, out: &mut Vec<Out>) {
        let verb = match n.run.status.as_str() {
            "failed" => "failed",
            "blocked" => "blocked the action",
            _ => return,
        };
        let detail =
            n.run.status_message.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string).or_else(
                || {
                    let text: Vec<&str> = n
                        .run
                        .entries
                        .iter()
                        .filter(|e| matches!(e.kind.as_str(), "error" | "stop" | "warning" | "feedback"))
                        .map(|e| e.text.trim())
                        .filter(|t| !t.is_empty())
                        .collect();
                    (!text.is_empty()).then(|| text.join("\n"))
                },
            );
        let event = if n.run.event_name.is_empty() { "hook".to_string() } else { format!("{} hook", n.run.event_name) };
        let message = match detail {
            Some(d) => format!("The {event} {verb}: {d}"),
            None => format!("The {event} {verb}"),
        };
        self.push_notice(out, "hook_failed", NoticeLevel::Warning, &message, None);
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
                    parent_id: None,
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

    /// 和这条审批对得上的、正在进行的 MCP 工具调用（同一个 server、参数一样）：只用来取工具名。
    fn matching_mcp_call(&self, p: &McpElicitation) -> Option<&ToolCallParts> {
        let params = p.meta.as_ref()?.get("tool_params").cloned().unwrap_or_else(|| json!({}));
        self.tool_order.iter().rev().filter_map(|id| self.tools.get(id)).find(|t| {
            t.category == ToolCategory::Mcp
                && t.input == params
                && t.detail.as_ref().and_then(|d| d.server.as_deref()) == Some(p.server_name.as_str())
        })
    }

    pub fn feed_request(&mut self, id: Value, method: &str, params: &Value) -> Vec<Out> {
        let mut out = Vec::new();
        let request_id = uuid::Uuid::new_v4().to_string();
        // 请求所属的 thread：子 agent 的请求要在标题里写明是谁在问
        let mut thread_id: Option<String> = None;
        let built: Option<(PendingPermission, NativePending)> = match method {
            permissions::M_COMMAND_APPROVAL => parse::<CommandApproval>(method, params).map(|p| {
                thread_id = p.thread_id.clone();
                let started = self.tools.get(&p.item_id);
                permissions::from_command(&request_id, id.clone(), &p, &self.cwd, started)
            }),
            permissions::M_FILE_APPROVAL => parse::<FileApproval>(method, params).map(|p| {
                thread_id = p.thread_id.clone();
                let started = self.tools.get(&p.item_id);
                permissions::from_file_change(&request_id, id.clone(), &p, &self.cwd, started)
            }),
            permissions::M_PERMISSIONS_APPROVAL => parse::<PermissionsApproval>(method, params).map(|p| {
                thread_id = p.thread_id.clone();
                permissions::from_permissions(&request_id, id.clone(), &p, &self.cwd)
            }),
            permissions::M_USER_INPUT => parse::<UserInputRequest>(method, params).map(|p| {
                thread_id = p.thread_id.clone();
                permissions::from_user_input(&request_id, id.clone(), &p)
            }),
            permissions::M_ELICITATION => match parse::<McpElicitation>(method, params) {
                None => None,
                Some(p) => {
                    thread_id = p.thread_id.clone();
                    match permissions::from_elicitation(
                        &request_id,
                        id.clone(),
                        &p,
                        &self.cwd,
                        self.matching_mcp_call(&p),
                    ) {
                        Ok(b) => Some(b),
                        Err(message) => {
                            // 交不给用户的 elicitation：回拒绝，并留一条可见的说明
                            tracing::warn!(method, "codex: declining an MCP elicitation anycon cannot show");
                            out.push(Out::Reply { id, result: Ok(json!({ "action": "decline" })) });
                            push(
                                &mut out,
                                Event::Error { code: "elicitation_rejected".to_string(), message, recoverable: true },
                            );
                            return out;
                        }
                    }
                }
            },
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
            Some((mut pending, native)) => {
                if self.cancel_requested {
                    // 会话正在取消：别把新的审批摆到用户面前，直接 cancel
                    let result = permissions::build_response(&native, &PermissionDecision::default(), true)
                        .map_err(|e| RpcError::new(-32603, e.to_string()));
                    out.push(Out::Reply { id, result });
                } else {
                    if let Some(label) = self.child_label(thread_id.as_deref()) {
                        pending.title = format!("{} (sub-agent {label})", pending.title);
                    }
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
        let outs = t.feed_request(
            json!("e-1"),
            "item/tool/call",
            &json!({"threadId": "th", "turnId": "t1", "callId": "d1", "tool": "lookup", "arguments": {}}),
        );
        assert!(matches!(&outs[0], Out::Reply { id, result: Ok(r) } if *id == json!("e-1") && r["success"] == false));
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
    fn elicitations_are_permissions_not_refusals() {
        let mut t = Translator::new("/w", None);
        turn_started(&mut t, "t1");
        let params = json!({"serverName": "s", "threadId": "th", "mode": "form", "message": "x", "requestedSchema": {"type": "object", "properties": {"a": {"type": "string"}}}});
        let outs = t.feed_request(json!("e-1"), "mcpServer/elicitation/request", &params);
        assert!(
            matches!(&outs[0], Out::Permission { pending, .. } if pending.kind == anycon_protocol::PermissionKind::Question)
        );
        // 会话正在取消：别摆到用户面前，直接 decline
        t.mark_cancel_requested();
        let outs = t.feed_request(json!("e-2"), "mcpServer/elicitation/request", &params);
        assert_eq!(outs, vec![Out::Reply { id: json!("e-2"), result: Ok(json!({"action": "decline"})) }]);
        // 形状不对
        let outs = t.feed_request(json!("e-3"), "mcpServer/elicitation/request", &json!({"threadId": 7}));
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
            Event::ToolCall { id, name, category, title, input, detail, .. } => {
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
    // ---------- 子 agent / 线程路由 ----------

    /// 主 thread `th`，子 agent `kid`（`/root/helper`，由 `call_1` 启动）。
    fn with_sub_agent() -> Translator {
        let mut t = Translator::new("/w", Some("m".into()));
        t.set_main_thread("th");
        turn_started(&mut t, "t1");
        let outs = feed(
            &mut t,
            "item/started",
            json!({"threadId": "th", "turnId": "t1", "item": {"type": "subAgentActivity", "id": "call_1", "kind": "started", "agentThreadId": "kid", "agentPath": "/root/helper"}}),
        );
        assert_eq!(names(&outs), vec!["tool_call", "task"]);
        // 启动这个动作本身马上就完成了；子 agent 继续在后台跑
        let outs = feed(
            &mut t,
            "item/completed",
            json!({"threadId": "th", "turnId": "t1", "item": {"type": "subAgentActivity", "id": "call_1", "kind": "started", "agentThreadId": "kid", "agentPath": "/root/helper"}}),
        );
        assert_eq!(names(&outs), vec!["tool_result"]);
        t
    }

    fn task_status(outs: &[Out]) -> Vec<TaskStatus> {
        events(outs)
            .into_iter()
            .filter_map(|e| if let Event::Task { status, .. } = e { Some(status) } else { None })
            .collect()
    }

    #[test]
    fn sub_agent_markers_drive_the_task_status() {
        let mut t = with_sub_agent();
        let marker = |t: &mut Translator, kind: &str| {
            feed(
                t,
                "item/completed",
                json!({"threadId": "th", "turnId": "t1", "item": {"type": "subAgentActivity", "id": format!("m-{kind}"), "kind": kind, "agentThreadId": "kid", "agentPath": "/root/helper"}}),
            )
        };
        assert_eq!(task_status(&marker(&mut t, "completed")), vec![TaskStatus::Completed]);
        // 给它发了新消息：又在跑了（task 不是一次性的）
        assert_eq!(task_status(&marker(&mut t, "interacted")), vec![TaskStatus::Running]);
        assert_eq!(task_status(&marker(&mut t, "interrupted")), vec![TaskStatus::Cancelled]);
        // 没变就不重复发
        assert!(marker(&mut t, "interrupted").is_empty());
        // 标记本身不是工具调用
        assert!(!names(&marker(&mut t, "interacted")).contains(&"tool_call".to_string()));
    }

    #[test]
    fn a_failed_sub_agent_turn_fails_the_task_with_the_reason_and_leaves_the_main_turn_alone() {
        let mut t = with_sub_agent();
        feed(
            &mut t,
            "turn/started",
            json!({"threadId": "kid", "turn": {"id": "k1", "status": "inProgress", "items": []}}),
        );
        let outs = feed(
            &mut t,
            "turn/completed",
            json!({"threadId": "kid", "turn": {"id": "k1", "status": "failed", "items": [], "durationMs": 7, "error": {"message": "boom", "codexErrorInfo": "other"}}}),
        );
        // 没有 `error` 事件、没有 `turn_done`
        assert_eq!(names(&outs), vec!["task"]);
        match &events(&outs)[0] {
            Event::Task { status, summary, usage, .. } => {
                assert_eq!(
                    (*status, summary.as_deref(), usage.unwrap().duration_ms),
                    (TaskStatus::Failed, Some("boom"), Some(7))
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(t.is_busy());
        // 子 agent 的 `error` 通知也不算主 turn 的错
        let outs = feed(
            &mut t,
            "error",
            json!({"threadId": "kid", "turnId": "k1", "willRetry": false, "error": {"message": "boom"}}),
        );
        assert!(outs.is_empty());
        let outs = completed(&mut t, "t1", "completed", json!({}));
        assert_eq!(names(&outs), vec!["turn_done"]);
    }

    #[test]
    fn a_sub_agents_plan_model_and_usage_do_not_touch_the_main_turn() {
        let mut t = with_sub_agent();
        let outs = feed(
            &mut t,
            "turn/plan/updated",
            json!({"threadId": "kid", "turnId": "k1", "plan": [{"step": "x", "status": "pending"}]}),
        );
        assert!(outs.is_empty());
        // 子 agent 的模型被改道：提示照发，但主 thread 的模型不变
        let outs = feed(
            &mut t,
            "model/rerouted",
            json!({"threadId": "kid", "turnId": "k1", "fromModel": "a", "toModel": "b", "reason": "highRiskCyberActivity"}),
        );
        assert_eq!(names(&outs), vec!["notice"]);
        assert_eq!(t.model(), Some("m"));
        let outs = feed(
            &mut t,
            "thread/tokenUsage/updated",
            json!({"threadId": "kid", "turnId": "k1", "tokenUsage": {"total": {"totalTokens": 500, "inputTokens": 400, "outputTokens": 100}, "last": {"totalTokens": 500, "inputTokens": 400, "outputTokens": 100}, "modelContextWindow": 1000}}),
        );
        assert_eq!(names(&outs), vec!["task"]);
        let outs = completed(&mut t, "t1", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => {
                assert_eq!(
                    (u.input_tokens, u.output_tokens, u.context_window, u.context_tokens),
                    (None, None, None, None)
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sub_agent_tool_calls_are_only_shown_during_the_main_turn_but_the_task_always_updates() {
        let mut t = with_sub_agent();
        completed(&mut t, "t1", "completed", json!({}));
        assert!(!t.is_busy());
        // 主 turn 结束后子 agent 还在跑（后台）：它的命令没有地方收尾，不发；task 照常更新
        let outs = feed(
            &mut t,
            "item/started",
            json!({"threadId": "kid", "turnId": "k1", "item": {"type": "commandExecution", "id": "late", "command": "ls", "commandActions": []}}),
        );
        assert!(outs.is_empty());
        let outs = completed(&mut t, "k1", "completed", json!({}));
        assert!(outs.is_empty(), "main turn is over: a stray completion is ignored");
        let outs = feed(
            &mut t,
            "turn/completed",
            json!({"threadId": "kid", "turn": {"id": "k1", "status": "completed", "items": []}}),
        );
        assert_eq!(task_status(&outs), vec![TaskStatus::Completed]);
    }

    #[test]
    fn closing_a_sub_agents_thread_cancels_its_open_task() {
        let mut t = with_sub_agent();
        let outs = feed(&mut t, "thread/closed", json!({"threadId": "kid"}));
        assert_eq!(task_status(&outs), vec![TaskStatus::Cancelled]);
        // 已经收尾的、不认识的 thread 都不动
        assert!(feed(&mut t, "thread/closed", json!({"threadId": "kid"})).is_empty());
        assert!(feed(&mut t, "thread/closed", json!({"threadId": "other"})).is_empty());
    }

    #[test]
    fn a_collab_spawn_with_no_receivers_yet_does_not_invent_a_task() {
        let mut t = Translator::new("/w", None);
        t.set_main_thread("th");
        turn_started(&mut t, "t1");
        let outs = feed(
            &mut t,
            "item/started",
            json!({"threadId": "th", "turnId": "t1", "item": {"type": "collabAgentToolCall", "id": "c1", "tool": "spawnAgent", "status": "inProgress", "senderThreadId": "th", "receiverThreadIds": [], "prompt": "Do it", "agentsStates": {}}}),
        );
        assert_eq!(names(&outs), vec!["tool_call"]);
        // 状态里提到的 thread（`wait` 之类）明确是子 agent：登记并发 task
        let outs = feed(
            &mut t,
            "item/completed",
            json!({"threadId": "th", "turnId": "t1", "item": {"type": "collabAgentToolCall", "id": "c1", "tool": "spawnAgent", "status": "completed", "senderThreadId": "th", "receiverThreadIds": ["k"], "prompt": "Do it", "agentsStates": {"k": {"status": "errored", "message": "no luck"}}}}),
        );
        assert_eq!(names(&outs), vec!["tool_result", "task", "task"]);
        assert_eq!(task_status(&outs), vec![TaskStatus::Running, TaskStatus::Failed]);
        match &events(&outs)[0] {
            Event::ToolResult { output, .. } => assert_eq!(output, &json!("Do it: failed\nno luck")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn child_thread_requests_are_answerable_even_before_the_agent_is_announced() {
        let mut t = Translator::new("/w", None);
        t.set_main_thread("th");
        turn_started(&mut t, "t1");
        let outs = t.feed_request(
            json!(3),
            "item/commandExecution/requestApproval",
            &json!({"threadId": "abcdef0123456789", "turnId": "k1", "itemId": "c", "command": "ls", "cwd": "/w"}),
        );
        match &outs[0] {
            Out::Permission { pending, .. } => assert_eq!(pending.title, "Run command (sub-agent abcdef01)"),
            other => panic!("{other:?}"),
        }
    }

    // ---------- 用量、压缩、提示 ----------

    #[test]
    fn context_tokens_follow_the_latest_request_and_the_window_is_kept() {
        let mut t = Translator::new("/w", Some("m".into()));
        let update = |t: &mut Translator, turn: &str, input: u64, cached: u64, output: u64, window: Value| {
            feed(
                t,
                "thread/tokenUsage/updated",
                json!({"threadId": "th", "turnId": turn, "tokenUsage": {
                    "total": {"totalTokens": 9999, "inputTokens": 9000, "cachedInputTokens": 1, "outputTokens": 9},
                    "last": {"totalTokens": input + output, "inputTokens": input, "cachedInputTokens": cached, "outputTokens": output},
                    "modelContextWindow": window}}),
            );
        };
        turn_started(&mut t, "t1");
        update(&mut t, "t1", 100, 80, 10, json!(1000));
        update(&mut t, "t1", 160, 150, 20, json!(1000));
        let outs = completed(&mut t, "t1", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => {
                // 累计：输入各扣缓存命中再加起来；占用：最近一次请求的输入（已含缓存命中）
                assert_eq!((u.input_tokens, u.cache_read_tokens, u.output_tokens), (Some(30), Some(230), Some(30)));
                assert_eq!((u.context_window, u.context_tokens), (Some(1000), Some(160)));
            }
            other => panic!("{other:?}"),
        }
        // 下一个 turn 还没有用量更新：窗口沿用，占用没有
        turn_started(&mut t, "t2");
        let outs = completed(&mut t, "t2", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => {
                assert_eq!((u.context_window, u.context_tokens), (Some(1000), None))
            }
            other => panic!("{other:?}"),
        }
        // 窗口缺省 / 为 0 不覆盖已知的
        turn_started(&mut t, "t3");
        update(&mut t, "t3", 5, 0, 1, json!(null));
        update(&mut t, "t3", 5, 0, 1, json!(0));
        let outs = completed(&mut t, "t3", "completed", json!({}));
        match events(&outs).last().unwrap() {
            Event::TurnDone { usage: Some(u), .. } => {
                assert_eq!((u.context_window, u.context_tokens), (Some(1000), Some(5)))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn compaction_outside_a_turn_is_a_notice_only() {
        let mut t = Translator::new("/w", None);
        let item =
            |_: &str| json!({"threadId": "th", "turnId": "x", "item": {"type": "contextCompaction", "id": "cc"}});
        // 没有我们的 turn：不发临时状态（没人会清它），压缩完成的提示照留
        assert!(feed(&mut t, "item/started", item("s")).is_empty());
        let outs = feed(&mut t, "item/completed", item("c"));
        assert_eq!(names(&outs), vec!["notice"]);
    }

    #[test]
    fn startup_status_other_than_failed_is_not_a_notice() {
        let mut t = Translator::new("/w", None);
        for status in ["starting", "ready", "cancelled"] {
            let outs = feed(&mut t, "mcpServer/startupStatus/updated", json!({"name": "s", "status": status}));
            assert!(outs.is_empty(), "{status}");
        }
        let outs =
            feed(&mut t, "mcpServer/startupStatus/updated", json!({"name": "s", "status": "failed", "error": null}));
        match &events(&outs)[0] {
            Event::Notice { code, message, .. } => {
                assert_eq!((code.as_str(), message.as_str()), ("mcp_server_failed", "MCP server s failed to start"))
            }
            other => panic!("{other:?}"),
        }
        // 同一个 server 不再提示
        assert!(feed(&mut t, "mcpServer/startupStatus/updated", json!({"name": "s", "status": "failed"})).is_empty());
    }

    #[test]
    fn empty_and_repeated_warnings_are_dropped() {
        let mut t = Translator::new("/w", None);
        assert!(feed(&mut t, "warning", json!({"message": "  "})).is_empty());
        assert_eq!(names(&feed(&mut t, "warning", json!({"message": "careful"}))), vec!["notice"]);
        assert!(feed(&mut t, "warning", json!({"message": "careful"})).is_empty());
        // 同样的话换一种通知来也是同一条
        assert!(feed(&mut t, "guardianWarning", json!({"threadId": "th", "message": "careful"})).is_empty());
        assert_eq!(names(&feed(&mut t, "warning", json!({"message": "careful, again"}))), vec!["notice"]);
    }
}
