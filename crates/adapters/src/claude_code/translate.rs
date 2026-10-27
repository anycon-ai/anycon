//! stream-json（CLI 的 stdout，一行一个 JSON）→ [`AgentEvent`]。
//!
//! 纯同步状态机：`feed_line` 进、`Vec<Out>` 出，不碰进程和 IO，所以可以直接拿录制的 JSONL 回放测试。
//! 要点（部分来自对真实 CLI 2.1.x 的观察，见 README.md）：
//! - 非 `{` 开头的行忽略；不认识的 `type` / `subtype` 忽略（`commands_changed`、`thinking_tokens` …）
//! - `stream_event` 只取文本 / 思考的增量；**tool_call 在 `assistant` 消息里发**（那里有完整 input）
//! - 每个 content block 会各自带一条 `assistant` 消息；文本已经流式发过的，不再从 `assistant` 重发
//! - 思考内容在较新的 CLI 里是空串（只有 `estimated_tokens`）：每个思考块只发一条空的 `ThinkingDelta`
//! - `result.total_cost_usd` 是**进程内累计值**，`usage` / `duration_ms` 是**本 turn 的**；这里把费用换成增量
//! - `system/init` 每个 turn 都会再发一次；只在 `session_id` 变化时再发 `Started`
//! - 没有 turn 在跑时出现的 `system/init` = CLI **自己**开始了一轮（后台任务结束后处理结果）：发 `TurnStarted`，
//!   这一轮照常以 `result` → `TurnDone` 结束。这一轮进行中到达的用户消息先暂存，`result` 之后以 [`Out::Write`] 交还
//! - 后台任务（后台 subagent、后台命令）以 `system/background_tasks_changed` 为准，`task_started` / `task_notification` /
//!   `task_updated` 兜底；数目变化时发 `Background`
//! - `TodoWrite` → `Plan`；`ExitPlanMode` / `AskUserQuestion` 由权限请求表达，不再发 tool_call / tool_result
//! - 子 agent 内部的调用（`parent_tool_use_id` 非空）带 `parent_id`
//! - 转到后台的调用（子 agent 的 `tool_use_result.status = async_launched`、Bash 的 `tool_use_result.backgroundTaskId`，
//!   或者 `task_started { is_backgrounded }` 指向的调用）的结果标 `background`，等 `system/task_notification` 再给同一个 id
//!   补最终结果（后台命令从通知的 `summary` 里取退出码）
//! - 子 agent 用掉的 token：前台的取 `tool_use_result.totalTokens`，后台的取 `task_notification.usage.total_tokens`
//! - 后台任务的状态快照（`Event::Task`）：`system/task_started` / `task_progress` / `task_updated` / `task_notification` 是增量，
//!   这里按 `task_id` 合并成整条快照再发（workflow 的 `workflow_progress` 是全量数组，没带的帧沿用上一份）；
//!   只有后台任务才发（前台任务 `is_backgrounded: false` 转到后台之前只记状态）；终态之后的帧忽略
//! - `system/api_retry` / `system/status { compacting }` → `Activity`；`system/compact_boundary`、`rate_limit_event`、
//!   `notification` / `informational`、模型回退、权限拒绝、hook 失败、MCP server 启动失败 → `Notice`（code 见 docs/spec/daemon.md § 8.3）
//! - 本地斜杠命令（`/context`、`/cost` …）的输出：带 `local_command_source` 的 `assistant` 消息（`-p` 下实测）和
//!   `system/local_command_output` 都发 `Notice { code: command_output }`，不当作模型的文本
//! - `result.modelUsage.<模型>.contextWindow` → `Usage.context_window`；`context_tokens` 取本 turn 最近一条顶层 assistant 消息的
//!   `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`
//! - `control_request { subtype: elicitation }`（MCP server 向用户要输入）→ 权限请求，见 `permissions::from_elicitation`

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use anycon_agent_api::{AgentEvent, PendingPermission, ResumeHandle};
use anycon_protocol::{
    ActivityState, Event, FileChangeKind, NoticeLevel, PlanEntry, PlanStatus, TaskAgent, TaskAgentState, TaskKind,
    TaskPhase, TaskStatus, TaskUsage, ToolCategory, ToolResultDetail, TurnDoneReason, Usage,
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
    /// 要写进 stdin 的一行：agent 自己的一轮进行中暂存的用户消息，这一轮结束了，现在写进去（[`Translator::submit_user_message`]）。
    Write(Value),
    /// 要立刻写进 stdin 的 `control_response`（不碰未决权限表）：translator 自己能答复的控制请求，如不能交给用户的 elicitation。
    Reply(Value),
}

/// [`Translator::submit_user_message`] 的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum UserTurn {
    /// 已经开始新 turn：调用方现在把这条消息写进 stdin。
    Write(Value),
    /// CLI 自己开的一轮正在跑：消息暂存，这一轮的 `result` 之后由 `feed` 以 [`Out::Write`] 交还，
    /// 用户的 turn 接着这一轮算（这一轮的事件、用量都归它）。
    Deferred,
    /// 用户的 turn 正在跑，或者已经有一条暂存的消息。
    Busy,
}

/// `task_updated.patch.status` 里表示任务还没结束的取值；其它取值都当作结束。
const TASK_LIVE_STATUSES: &[&str] = &["running", "pending"];

/// 已经结束的任务保留多少个状态：终态之后迟到的 `task_notification`（带最终用量和摘要）还要和它合并。
const MAX_FINISHED_TASKS: usize = 32;

/// `system/notification` 的 `priority`：`low` 是 REPL 里的闲置提示（小贴士之类），CLI 自己也只在没有别的通知时才显示，不转成 `Notice`。
const NOTIFICATION_LOW_PRIORITY: &str = "low";

/// 后台任务的状态：adapter 把 `task_*` 的增量合并成整条快照，每次变化发一条 `Event::Task`（后一条整条取代前一条）。
struct TaskState {
    /// `run_id`：任务被续跑（resume）时 `task_id` 不变、`run_id` 变；比当前小的帧是迟到的上一次运行的，忽略。
    run_id: Option<String>,
    kind: TaskKind,
    tool_id: Option<String>,
    title: String,
    status: TaskStatus,
    usage: Option<TaskUsage>,
    last_tool: Option<String>,
    summary: Option<String>,
    phases: Vec<TaskPhase>,
    agents: Vec<TaskAgent>,
    /// 后台任务才对外发快照。前台任务（`is_backgrounded: false`、`awaited`）只记状态，转到后台（`task_updated.patch.is_backgrounded`）时才开始发。
    announced: bool,
    /// 上一次发出去的快照：内容没变不重发。
    sent: Option<Event>,
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
    /// 当前的 turn 是 CLI 自己开始的（没有 turn 时收到了 `system/init`），不是 `send` 触发的。
    agent_turn: bool,
    /// agent 自己的一轮进行中收到的用户消息，等这一轮的 `result` 之后写进 stdin。
    deferred: Option<Value>,
    /// 被并进用户 turn 的那一轮的用量，加到用户 turn 的 `TurnDone` 上。
    carry: Option<Usage>,
    /// 还在跑的后台任务的 task_id。
    bg_tasks: HashSet<String>,
    /// 上次通过 `Background` 报告的数目。
    bg_reported: u32,
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
    /// 结果已标 `background`、还在后台跑的调用（子 agent、后台命令）和它的类别：等 `task_notification` 补最终结果。
    backgrounded: HashMap<String, ToolCategory>,
    /// `task_started { is_backgrounded: true }` 报过、结果还没到的调用：结果到了标 `background`。
    bg_started: HashSet<String>,
    /// 任务快照的状态，按 `task_id`。
    tasks: HashMap<String, TaskState>,
    /// 已经到终态、状态还留着的任务（旧的在前），超过 [`MAX_FINISHED_TASKS`] 就丢最旧的。
    tasks_done: VecDeque<String>,
    /// 正在发 `Activity { compacting }`（`system/status` 回到 `null` 时才发 `none`）。
    compacting: bool,
    /// 正在发 `Activity { retrying }`（下一次响应开始流式输出时发 `none`）。
    retrying: bool,
    /// `rate_limit_event` 已经通知过的状态：按限额类型记（状态，恢复时间），同一状态同一恢复时间只发一次。
    rate_limit_seen: HashMap<String, (String, Option<u64>)>,
    /// 已经为它发过 `mcp_server_failed` 的 server（每个进程每个 server 一次）。
    mcp_failed: HashSet<String>,
    /// 本 turn 已经发过的 `notification`（key + 文本）。
    notified: HashSet<(String, String)>,
    /// 本 turn 最近一条顶层 assistant 消息的上下文占用（输入 + 缓存读写）。
    context_tokens: Option<u64>,
    /// 本 turn 最近一条顶层 assistant 消息的模型：从 `result.modelUsage` 里挑主模型的依据。
    last_model: Option<String>,
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

/// 两轮的用量相加（并进用户 turn 的那一轮 + 用户 turn 本身）。模型取后一轮的。
fn merge_usage(a: Usage, b: Usage) -> Usage {
    fn add<T: std::ops::Add<Output = T>>(x: Option<T>, y: Option<T>) -> Option<T> {
        match (x, y) {
            (Some(x), Some(y)) => Some(x + y),
            (x, y) => x.or(y),
        }
    }
    Usage {
        input_tokens: add(a.input_tokens, b.input_tokens),
        output_tokens: add(a.output_tokens, b.output_tokens),
        cache_read_tokens: add(a.cache_read_tokens, b.cache_read_tokens),
        cache_write_tokens: add(a.cache_write_tokens, b.cache_write_tokens),
        cost_usd: add(a.cost_usd, b.cost_usd),
        duration_ms: a.duration_ms + b.duration_ms,
        model: b.model.or(a.model),
        context_window: b.context_window.or(a.context_window),
        context_tokens: b.context_tokens.or(a.context_tokens),
    }
}

fn parse_exit_code(text: &str) -> Option<i32> {
    let rest = text.strip_prefix("Exit code ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

/// 后台命令结束通知的 `summary`（`Background command "…" completed (exit code 1)`）里的退出码。
fn summary_exit_code(summary: &str) -> Option<i32> {
    const MARK: &str = "(exit code ";
    let rest = &summary[summary.find(MARK)? + MARK.len()..];
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

/// 非空字符串字段。
fn non_empty<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn u32_field(v: &Value, key: &str) -> Option<u32> {
    v.get(key).and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok())
}

/// 摘要类文字（任务摘要、错误信息）在 adapter 这一层保留的上限；daemon 还会按规格再截一次。
const MAX_SUMMARY_CHARS: usize = 8 * 1024;

/// 按字符数截断（落在字符边界上），超出的补 `…`。
fn clip_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

/// `task_progress` / `task_notification` 的 `usage`。
fn task_usage(u: &Value) -> Option<TaskUsage> {
    if !u.is_object() {
        return None;
    }
    Some(TaskUsage {
        total_tokens: u64_of(u, "total_tokens"),
        tool_calls: u64_of(u, "tool_uses"),
        duration_ms: u64_of(u, "duration_ms"),
    })
}

/// `task_updated.patch.status` → 任务状态。`pending` / `running` / `paused` 都还没结束；不认识的取值按「被取消」处理
/// （后台任务计数同样把它们当作已结束）。
fn task_status_of_patch(s: &str) -> TaskStatus {
    match s {
        "pending" | "running" | "paused" => TaskStatus::Running,
        "completed" => TaskStatus::Completed,
        "failed" => TaskStatus::Failed,
        _ => TaskStatus::Cancelled,
    }
}

/// workflow 里一个 agent 的条目（`workflow_progress[]` 里 `type = workflow_agent`）。字段含义取自 CLI 2.1.308 二进制里的 schema 和它自己的进度统计：
/// `state`：`start` 带 `queuedAt` 而没有 `startedAt` = 还在排队（并发上限、等限额恢复），有 `startedAt` 才算在跑；`progress` = 在跑（带最新用量）；
/// `done`；`error`（`skipped` = 被用户跳过，其余是失败，`blocked` = 被安全分类器拦下）。schema 之外的 `state` 整条丢掉（CLI 自己的消费端也这样）。
fn parse_workflow_agent(e: &Value) -> Option<TaskAgent> {
    let index = u32_field(e, "index")?;
    let present = |k: &str| e.get(k).is_some_and(|v| !v.is_null());
    let state = match e["state"].as_str()? {
        "start" if present("queuedAt") && !present("startedAt") => TaskAgentState::Queued,
        "start" | "progress" => TaskAgentState::Running,
        "done" => TaskAgentState::Done,
        "error" if e["skipped"].as_bool() == Some(true) => TaskAgentState::Skipped,
        "error" => TaskAgentState::Failed,
        _ => return None,
    };
    Some(TaskAgent {
        index,
        label: non_empty(e, "label").map(str::to_string).unwrap_or_else(|| format!("agent {index}")),
        state,
        phase_index: u32_field(e, "phaseIndex"),
        model: non_empty(e, "model").map(str::to_string),
        tokens: u64_of(e, "tokens"),
        tool_calls: u64_of(e, "toolCalls"),
        duration_ms: u64_of(e, "durationMs"),
        last_tool: non_empty(e, "lastToolName").map(str::to_string),
        error: non_empty(e, "error").map(|m| clip_chars(m, MAX_SUMMARY_CHARS)),
        attempt: u32_field(e, "attempt"),
        cached: (e["cached"].as_bool() == Some(true)).then_some(true),
    })
}

/// `task_progress.workflow_progress`（全量数组）→ 阶段和 agent，各按 `index` 升序。
fn parse_workflow_progress(items: &[Value]) -> (Vec<TaskPhase>, Vec<TaskAgent>) {
    let mut phases: BTreeMap<u32, String> = BTreeMap::new();
    let mut agents: BTreeMap<u32, TaskAgent> = BTreeMap::new();
    for item in items {
        match item["type"].as_str() {
            Some("workflow_phase") => {
                if let (Some(index), Some(title)) = (u32_field(item, "index"), item["title"].as_str()) {
                    phases.insert(index, title.to_string());
                }
            }
            Some("workflow_agent") => {
                if let Some(a) = parse_workflow_agent(item) {
                    // agent 指向的阶段没有 `workflow_phase` 条目（脚本里的 `phase()` 没写进 `meta.phases`）：用它自带的阶段名补上
                    if let (Some(pi), Some(title)) = (a.phase_index, non_empty(item, "phaseTitle")) {
                        phases.entry(pi).or_insert_with(|| title.to_string());
                    }
                    agents.insert(a.index, a);
                }
            }
            _ => {}
        }
    }
    (phases.into_iter().map(|(index, title)| TaskPhase { index, title }).collect(), agents.into_values().collect())
}

/// 提示类事件的构造。
fn notice_event(code: &str, level: NoticeLevel, message: impl Into<String>) -> Event {
    Event::Notice {
        code: code.to_string(),
        level,
        message: message.into(),
        model_from: None,
        model_to: None,
        resets_at_ms: None,
    }
}

/// 限额类型 → 给人看的窗口名。
fn rate_limit_window(kind: Option<&str>) -> &str {
    match kind {
        Some("five_hour") => "5-hour",
        Some("seven_day") => "weekly",
        Some("seven_day_opus") => "weekly Opus",
        Some("seven_day_sonnet") => "weekly Sonnet",
        Some("seven_day_overage_included") => "weekly",
        Some("overage") => "overage",
        Some(other) => other,
        None => "usage",
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
            agent_turn: false,
            deferred: None,
            carry: None,
            bg_tasks: HashSet::new(),
            bg_reported: 0,
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
            backgrounded: HashMap::new(),
            bg_started: HashSet::new(),
            tasks: HashMap::new(),
            tasks_done: VecDeque::new(),
            compacting: false,
            retrying: false,
            rate_limit_seen: HashMap::new(),
            mcp_failed: HashSet::new(),
            notified: HashSet::new(),
            context_tokens: None,
            last_model: None,
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
        self.agent_turn = false;
        self.cancel_requested = false;
        self.reset_turn_tables();
    }

    /// 用户消息要开 turn（`send`）：没有 turn 在跑就开新 turn，返回 [`UserTurn::Write`]；CLI 自己开的一轮正在跑就暂存
    /// （[`UserTurn::Deferred`]）——这时写进 stdin 的话，CLI 会把它排到这一轮之后，先到的是这一轮的 `result`，会被当成
    /// 用户这个 turn 的结束；其它情况 [`UserTurn::Busy`]。
    pub fn submit_user_message(&mut self, msg: Value) -> UserTurn {
        if !self.busy {
            self.begin_turn();
            return UserTurn::Write(msg);
        }
        if self.agent_turn && self.deferred.is_none() {
            self.deferred = Some(msg);
            return UserTurn::Deferred;
        }
        UserTurn::Busy
    }

    /// [`Translator::submit_user_message`] 会不会返回 [`UserTurn::Busy`]（读附件之前先问，忙的时候不读文件）。
    pub fn would_refuse_user_message(&self) -> bool {
        self.busy && !(self.agent_turn && self.deferred.is_none())
    }

    /// 新 turn 开始时清掉按消息去重的表，长期保活的会话里不会无限增长。工具调用的表只在没有后台任务时清：
    /// 后台 subagent 的工具调用会跨 turn（调用在上一轮，结果在这一轮），清掉就丢了结果要用的工具名和路径。
    fn reset_turn_tables(&mut self) {
        self.blocks.clear();
        self.text_streamed.clear();
        self.thinking_streamed.clear();
        self.fallback_done.clear();
        self.compacting = false;
        self.retrying = false;
        self.notified.clear();
        self.context_tokens = None;
        self.last_model = None;
        if self.bg_tasks.is_empty() {
            self.seen_tool_use.clear();
            self.tools.clear();
            self.suppressed.clear();
            self.bg_started.clear();
        }
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
            Some("rate_limit_event") => self.on_rate_limit(v, &mut out),
            _ => {}
        }
        out
    }

    /// stdout 关闭（进程退出）时调用。
    pub fn finish(&mut self) -> Finish {
        let mut out = Vec::new();
        let mut died = false;
        let cancelled = self.cancel_requested;
        // 暂存的用户消息不会再有机会写进去了；后台任务随进程一起没了（daemon 在进程退出时自己清零）
        self.deferred = None;
        self.carry = None;
        self.agent_turn = false;
        self.bg_tasks.clear();
        // 任务随进程一起没了；daemon 在进程退出时给还没到终态的任务补 `cancelled` 快照
        self.tasks.clear();
        self.tasks_done.clear();
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
            Some("init") => self.on_init(v, out),
            // 全量列表，以它为准
            Some("background_tasks_changed") => {
                self.bg_tasks = v["tasks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["task_id"].as_str().map(str::to_string))
                    .collect();
                self.report_background(out);
            }
            Some("task_started") => {
                self.on_task_started(v, out);
                if v["is_backgrounded"].as_bool() == Some(true) {
                    if let Some(tool_id) = v["tool_use_id"].as_str() {
                        self.bg_started.insert(tool_id.to_string());
                    }
                    if let Some(id) = v["task_id"].as_str() {
                        self.bg_tasks.insert(id.to_string());
                        self.report_background(out);
                    }
                }
            }
            Some("task_progress") => self.on_task_progress(v, out),
            Some("task_notification") => {
                // 任务快照先到终态；后台调用的最终结果再补上，最后报任务数的变化
                self.on_task_finished(v, out);
                self.on_task_notification(v, out);
                if let Some(id) = v["task_id"].as_str() {
                    self.bg_tasks.remove(id);
                    self.report_background(out);
                }
            }
            Some("task_updated") => {
                self.on_task_updated(v, out);
                let ended = v["patch"]["status"].as_str().is_some_and(|s| !TASK_LIVE_STATUSES.contains(&s));
                if let (true, Some(id)) = (ended, v["task_id"].as_str()) {
                    self.bg_tasks.remove(id);
                    self.report_background(out);
                }
            }
            Some("api_retry") => self.on_api_retry(v, out),
            Some("status") => self.on_status(v, out),
            Some("compact_boundary") => self.on_compact_boundary(v, out),
            Some("notification") => self.on_notification(v, out),
            Some("informational") => self.on_informational(v, out),
            Some("local_command_output") => {
                if let Some(text) = v["content"].as_str() {
                    self.command_output(text, NoticeLevel::Info, out);
                }
            }
            Some(
                "model_fallback" | "model_refusal_fallback" | "model_refusal_no_fallback" | "model_consent_fallback",
            ) => self.on_model_fallback(v, out),
            Some("permission_denied") => self.on_permission_denied(v, out),
            Some("hook_response") => self.on_hook_response(v, out),
            _ => {}
        }
    }

    fn report_background(&mut self, out: &mut Vec<Out>) {
        let n = u32::try_from(self.bg_tasks.len()).unwrap_or(u32::MAX);
        if n != self.bg_reported {
            self.bg_reported = n;
            out.push(Out::Event(AgentEvent::Background { tasks: n }));
        }
    }

    fn on_init(&mut self, v: &Value, out: &mut Vec<Out>) {
        // `init` 只在 CLI 开始处理一轮时发（第一条用户消息之前不发）。没有 turn 在跑却来了 init：CLI 自己开始了一轮
        // （后台任务结束后处理结果），它同样以 `result` 结束
        if !self.busy {
            self.turn_counter += 1;
            self.busy = true;
            self.agent_turn = true;
            self.cancel_requested = false;
            self.reset_turn_tables();
            out.push(Out::Event(AgentEvent::TurnStarted));
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
        self.on_mcp_servers(v, out);
    }

    /// 后台任务结束（`completed` / `failed` / `stopped` / `killed`）。只认我们标过 `background` 的调用：
    /// 前台子 agent 也会发这个通知，它的结果已经发过了。`failed`、退出码非 0 算错误，被停掉的不算。
    fn on_task_notification(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v.get("tool_use_id").and_then(Value::as_str) else { return };
        self.bg_started.remove(id);
        let Some(category) = self.backgrounded.remove(id) else { return };
        let summary = v["summary"].as_str().unwrap_or("");
        let mut detail = ToolResultDetail::default();
        match category {
            ToolCategory::Subagent => detail.total_tokens = u64_of(&v["usage"], "total_tokens"),
            ToolCategory::Shell => detail.exit_code = summary_exit_code(summary),
            _ => {}
        }
        let is_error = v["status"].as_str() == Some("failed") || detail.exit_code.is_some_and(|c| c != 0);
        Self::push(
            out,
            Event::ToolResult {
                id: id.to_string(),
                output: json!(clip_text(summary)),
                is_error,
                detail: if detail == ToolResultDetail::default() { None } else { Some(detail) },
            },
        );
    }

    // ---------- 后台任务快照（Event::Task） ----------

    /// `task_type` → 任务种类。没有 `task_type` 的老 CLI：带 `workflow_name` 的是 workflow，带 `subagent_type` 的是子 agent。
    fn task_kind(v: &Value) -> TaskKind {
        match v["task_type"].as_str() {
            Some("local_workflow") => TaskKind::Workflow,
            Some("local_agent") => TaskKind::Subagent,
            Some("local_bash") => TaskKind::Shell,
            Some(_) => TaskKind::Other,
            None if non_empty(v, "workflow_name").is_some() => TaskKind::Workflow,
            None if non_empty(v, "subagent_type").is_some() => TaskKind::Subagent,
            None => TaskKind::Other,
        }
    }

    /// 这一帧是不是上一次运行的迟到帧：任务续跑后 `task_id` 不变、`run_id` 变大（字符串比较，CLI 保证按开始顺序递增）。
    fn is_late_frame(&self, id: &str, v: &Value) -> bool {
        match (self.tasks.get(id).and_then(|t| t.run_id.as_deref()), v["run_id"].as_str()) {
            (Some(cur), Some(frame)) => frame < cur,
            _ => false,
        }
    }

    /// `task_started`：（重新）建立这个任务的状态。`skip_transcript` / `ambient` 的杂务不进界面；前台任务只记状态，不发快照。
    fn on_task_started(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v["task_id"].as_str() else { return };
        if v["skip_transcript"].as_bool() == Some(true) || v["ambient"].as_bool() == Some(true) {
            return;
        }
        if self.is_late_frame(id, v) {
            return;
        }
        let kind = Self::task_kind(v);
        let description = non_empty(v, "description");
        let name = non_empty(v, "workflow_name");
        let title = match kind {
            TaskKind::Workflow => name.or(description),
            _ => description.or(name),
        }
        .unwrap_or("Background task");
        // workflow 的标题是脚本里起的名字，`description`（`meta.description`）是一句话说明，放进摘要
        let summary = description.filter(|d| *d != title).map(|d| clip_chars(d, MAX_SUMMARY_CHARS));
        let foreground = v["is_backgrounded"].as_bool() == Some(false) || v["awaited"].as_bool() == Some(true);
        self.tasks_done.retain(|x| x != id);
        self.tasks.insert(
            id.to_string(),
            TaskState {
                run_id: v["run_id"].as_str().map(str::to_string),
                kind,
                tool_id: non_empty(v, "tool_use_id").map(str::to_string),
                title: title.to_string(),
                status: TaskStatus::Running,
                usage: None,
                last_tool: None,
                summary,
                phases: Vec::new(),
                agents: Vec::new(),
                announced: !foreground,
                sent: None,
            },
        );
        self.emit_task(id, out);
    }

    /// `task_progress`：用量、最近的工具、进度摘要；workflow 另带 `workflow_progress`（全量数组，没带的帧沿用上一份）。
    fn on_task_progress(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v["task_id"].as_str() else { return };
        if self.is_late_frame(id, v) {
            return;
        }
        let Some(st) = self.tasks.get_mut(id) else { return };
        if st.status.is_terminal() {
            return;
        }
        if let Some(u) = task_usage(&v["usage"]) {
            st.usage = Some(u);
        }
        let workflow = st.kind == TaskKind::Workflow;
        // workflow 的 `last_tool_name` 是最近有动静的那个 agent 的名字，不是工具：它的工具在各个 agent 条目里
        if let (false, Some(t)) = (workflow, non_empty(v, "last_tool_name")) {
            st.last_tool = Some(clip_chars(t, MAX_SUMMARY_CHARS));
        }
        // `summary` 是 agent 给的一句话进度；子 agent 没有它时 `description` 是当前在做的事（如 `Editing src/lib.rs`）
        let line = non_empty(v, "summary").or(if workflow { None } else { non_empty(v, "description") });
        if let Some(line) = line.filter(|l| *l != st.title) {
            st.summary = Some(clip_chars(line, MAX_SUMMARY_CHARS));
        }
        if let Some(items) = v["workflow_progress"].as_array() {
            let (phases, agents) = parse_workflow_progress(items);
            st.phases = phases;
            st.agents = agents;
        }
        self.emit_task(id, out);
    }

    /// `task_updated`：`patch.status` 进终态，`patch.is_backgrounded` 把前台任务转成后台任务。
    fn on_task_updated(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v["task_id"].as_str() else { return };
        if self.is_late_frame(id, v) {
            return;
        }
        let patch = &v["patch"];
        let Some(st) = self.tasks.get_mut(id) else { return };
        if st.status.is_terminal() {
            return;
        }
        if patch["is_backgrounded"].as_bool() == Some(true) {
            st.announced = true;
        }
        if let Some(status) = patch["status"].as_str().map(task_status_of_patch) {
            st.status = status;
            if let (TaskStatus::Failed, Some(e)) = (status, non_empty(patch, "error")) {
                st.summary = Some(clip_chars(e, MAX_SUMMARY_CHARS));
            }
        }
        self.emit_task(id, out);
        self.retire_task(id);
    }

    /// `task_notification`：任务结束（`completed` / `failed` / `stopped`），带最终用量和摘要。`task_updated` 通常先到，
    /// 这里把用量和摘要并进去再发一次；两者的终态不一致时以先到的为准（和 CLI 自己的消费端一样）。
    fn on_task_finished(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(id) = v["task_id"].as_str() else { return };
        if self.is_late_frame(id, v) {
            return;
        }
        let Some(st) = self.tasks.get_mut(id) else { return };
        let status = match v["status"].as_str() {
            Some("completed") => TaskStatus::Completed,
            Some("failed") => TaskStatus::Failed,
            _ => TaskStatus::Cancelled,
        };
        if st.status.is_terminal() && st.status != status {
            return;
        }
        st.status = status;
        if let Some(u) = task_usage(&v["usage"]) {
            st.usage = Some(u);
        }
        // 子 agent / 后台命令的通知摘要就是它的结果（`Background command "…" completed (exit code 0)`）；
        // workflow 正常结束时只有一句 `Dynamic workflow "…" completed`，不如开头那句 `meta.description` 有用，失败 / 被停时才换成通知的
        let informative = !(st.kind == TaskKind::Workflow && status == TaskStatus::Completed);
        if let (true, Some(sum)) = (informative, non_empty(v, "summary")) {
            st.summary = Some(clip_chars(sum, MAX_SUMMARY_CHARS));
        }
        self.emit_task(id, out);
        self.retire_task(id);
    }

    /// 发这个任务的快照（后台任务才发；和上一次发出去的一样就不发）。
    fn emit_task(&mut self, id: &str, out: &mut Vec<Out>) {
        let Some(st) = self.tasks.get_mut(id) else { return };
        if !st.announced {
            return;
        }
        let ev = Event::Task {
            id: id.to_string(),
            kind: st.kind,
            tool_id: st.tool_id.clone(),
            title: st.title.clone(),
            status: st.status,
            usage: st.usage,
            last_tool: st.last_tool.clone(),
            summary: st.summary.clone(),
            phases: st.phases.clone(),
            agents: st.agents.clone(),
        };
        if st.sent.as_ref() == Some(&ev) {
            return;
        }
        st.sent = Some(ev.clone());
        Self::push(out, ev);
    }

    /// 任务到了终态：前台任务的状态直接丢；后台任务的留一阵（迟到的 `task_notification` 还要并进去），只留最近的 [`MAX_FINISHED_TASKS`] 个。
    fn retire_task(&mut self, id: &str) {
        let Some(st) = self.tasks.get(id) else { return };
        if !st.status.is_terminal() {
            return;
        }
        if !st.announced {
            self.tasks.remove(id);
            return;
        }
        if !self.tasks_done.iter().any(|x| x == id) {
            self.tasks_done.push_back(id.to_string());
        }
        while self.tasks_done.len() > MAX_FINISHED_TASKS {
            if let Some(old) = self.tasks_done.pop_front() {
                self.tasks.remove(&old);
            }
        }
    }

    // ---------- Activity / Notice ----------

    /// 临时状态只在 turn 进行中发：没有 turn 时（后台 agent 在重试）发出去，客户端没有内容事件或 `turn_done` 来清掉它。
    fn activity(&self, out: &mut Vec<Out>, state: ActivityState) -> bool {
        if !self.busy {
            return false;
        }
        Self::push(out, Event::Activity { state, attempt: None, max_attempts: None, delay_ms: None, reason: None });
        true
    }

    /// `system/api_retry`：API 请求失败，过一会儿重试。`error_status` 是 HTTP 状态码（连接错误为 `null`），`error` 是错误类别
    /// （`overloaded`、`rate_limit`、`server_error` …）；`no_response` = 首字节超时。
    fn on_api_retry(&mut self, v: &Value, out: &mut Vec<Out>) {
        if !self.busy {
            return;
        }
        let status = v["error_status"].as_u64();
        let class = non_empty(v, "error");
        let reason = if let Some(waited) = u64_of(&v["no_response"], "waited_ms") {
            format!("no response from the API within {}s", waited / 1000)
        } else {
            match (status, class) {
                (Some(s), Some(c)) => format!("{s} {c}"),
                (Some(s), None) => s.to_string(),
                (None, Some(c)) if c != "unknown" => c.to_string(),
                (None, _) => "connection error".to_string(),
            }
        };
        self.retrying = true;
        Self::push(
            out,
            Event::Activity {
                state: ActivityState::Retrying,
                attempt: u32_field(v, "attempt"),
                max_attempts: u32_field(v, "max_retries"),
                delay_ms: u64_of(v, "retry_delay_ms"),
                reason: Some(reason),
            },
        );
    }

    /// `system/status`：`compacting` = 正在压缩上下文，`null` = 回到普通状态（`requesting` 是每次请求开始时发的，与压缩无关）。
    fn on_status(&mut self, v: &Value, out: &mut Vec<Out>) {
        match v.get("status") {
            Some(Value::String(s)) if s == "compacting" => {
                if !self.compacting && self.activity(out, ActivityState::Compacting) {
                    self.compacting = true;
                }
            }
            Some(Value::Null) if self.compacting => {
                self.compacting = false;
                self.activity(out, ActivityState::None);
            }
            _ => {}
        }
    }

    fn on_compact_boundary(&mut self, v: &Value, out: &mut Vec<Out>) {
        if self.compacting {
            self.compacting = false;
            self.activity(out, ActivityState::None);
        }
        let m = &v["compact_metadata"];
        let trigger = non_empty(m, "trigger").unwrap_or("auto");
        let message = match (u64_of(m, "pre_tokens"), u64_of(m, "post_tokens")) {
            (Some(pre), Some(post)) => format!("Context compacted ({trigger}): {pre} → {post} tokens"),
            (Some(pre), None) => format!("Context compacted ({trigger}): was {pre} tokens"),
            _ => format!("Context compacted ({trigger})"),
        };
        Self::push(out, notice_event("context_compacted", NoticeLevel::Info, message));
    }

    /// `rate_limit_event`：`status` 是 `allowed` 时什么都不发（并忘掉这个限额类型之前的通知，下次再触发还要提醒）；
    /// `allowed_warning`（快到了）/ `rejected`（到了）各发一次——同一限额类型、同一状态、同一恢复时间只发一次，
    /// 这个事件在用量每变一点就会发一次。
    fn on_rate_limit(&mut self, v: &Value, out: &mut Vec<Out>) {
        let info = &v["rate_limit_info"];
        let Some(status) = info["status"].as_str() else { return };
        let kind = info["rateLimitType"].as_str();
        let slot = kind.unwrap_or("").to_string();
        if status == "allowed" {
            self.rate_limit_seen.remove(&slot);
            return;
        }
        let resets_at = info["resetsAt"].as_u64();
        let key = (status.to_string(), resets_at);
        if self.rate_limit_seen.get(&slot) == Some(&key) {
            return;
        }
        self.rate_limit_seen.insert(slot, key);
        let window = rate_limit_window(kind);
        let message = if status == "rejected" {
            format!("Usage limit reached ({window})")
        } else {
            match info["utilization"].as_f64() {
                Some(u) => format!("Approaching the usage limit ({window}, {}% used)", (u * 100.0).round() as u64),
                None => format!("Approaching the usage limit ({window})"),
            }
        };
        Self::push(
            out,
            Event::Notice {
                code: "rate_limit".to_string(),
                level: NoticeLevel::Warning,
                message,
                model_from: None,
                model_to: None,
                resets_at_ms: resets_at.map(|s| s.saturating_mul(1000)),
            },
        );
    }

    /// `system/notification`（REPL 通知队列的文本）。`color` 为 `warning` / `error` 的当警告；
    /// 优先级 `low` 的是闲置提示，不转；同一个 turn 里同 key 同文本只发一次。
    fn on_notification(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(text) = non_empty(v, "text") else { return };
        if v["priority"].as_str() == Some(NOTIFICATION_LOW_PRIORITY) {
            return;
        }
        let key = v["key"].as_str().unwrap_or("").to_string();
        if !self.notified.insert((key, text.to_string())) {
            return;
        }
        let level = match v["color"].as_str() {
            Some("warning" | "error") => NoticeLevel::Warning,
            _ => NoticeLevel::Info,
        };
        Self::push(out, notice_event("agent_notice", level, text));
    }

    /// `system/informational`：`level` 为 `warning` 的当警告，`notice` / `suggestion` 当提示；`info` 级别 CLI 自己也只在
    /// 转录视图里显示，不转。
    fn on_informational(&mut self, v: &Value, out: &mut Vec<Out>) {
        let Some(content) = non_empty(v, "content") else { return };
        let level = match v["level"].as_str() {
            Some("warning") => NoticeLevel::Warning,
            Some("notice" | "suggestion") => NoticeLevel::Info,
            _ => return,
        };
        Self::push(out, notice_event("agent_notice", level, content));
    }

    /// 本地斜杠命令（`/context`、`/cost` …）的输出。
    fn command_output(&mut self, text: &str, level: NoticeLevel, out: &mut Vec<Out>) {
        if text.trim().is_empty() {
            return;
        }
        Self::push(out, notice_event("command_output", level, text.trim_end()));
    }

    /// 模型回退一族：`model_fallback`、`model_refusal_fallback`、`model_refusal_no_fallback`（没有回退目标）、`model_consent_fallback`。
    fn on_model_fallback(&mut self, v: &Value, out: &mut Vec<Out>) {
        Self::push(
            out,
            Event::Notice {
                code: "model_fallback".to_string(),
                level: NoticeLevel::Warning,
                message: v["content"].as_str().unwrap_or("").to_string(),
                model_from: non_empty(v, "original_model").map(str::to_string),
                model_to: non_empty(v, "fallback_model").map(str::to_string),
                resets_at_ms: None,
            },
        );
    }

    /// `system/permission_denied`：没有权限提示就被拒绝的工具调用（auto 模式分类器、dontAsk 模式、deny 规则）。
    fn on_permission_denied(&mut self, v: &Value, out: &mut Vec<Out>) {
        let reason = non_empty(v, "decision_reason").or(non_empty(v, "message")).unwrap_or("denied");
        let message = match non_empty(v, "tool_name") {
            Some(tool) => format!("{tool}: {reason}"),
            None => reason.to_string(),
        };
        Self::push(out, notice_event("permission_denied", NoticeLevel::Info, message));
    }

    /// `system/hook_response`：只有 `outcome = error`（hook 失败或阻止了操作）才提示；`cancelled` 是我们自己中断的。
    /// 这个消息只在 CLI 开了 hook 事件时才发（`--include-hook-events`，anycon 不加），所以多数会话里看不到。
    fn on_hook_response(&mut self, v: &Value, out: &mut Vec<Out>) {
        if v["outcome"].as_str() != Some("error") {
            return;
        }
        let name = non_empty(v, "hook_name").unwrap_or("hook");
        let mut message = match non_empty(v, "hook_event") {
            Some(ev) => format!("Hook {name} ({ev}) failed"),
            None => format!("Hook {name} failed"),
        };
        if let Some(code) = v["exit_code"].as_i64() {
            message.push_str(&format!(" with exit code {code}"));
        }
        if let Some(detail) = non_empty(v, "stderr").or(non_empty(v, "output")) {
            message.push_str(&format!(": {}", clip_chars(detail.trim(), 1000)));
        }
        Self::push(out, notice_event("hook_failed", NoticeLevel::Warning, message));
    }

    /// `system/init.mcp_servers[]`：连接失败的 server 提示一次（`init` 每个 turn 都发，同一个 server 不重复）。
    fn on_mcp_servers(&mut self, v: &Value, out: &mut Vec<Out>) {
        for server in v["mcp_servers"].as_array().into_iter().flatten() {
            let (Some(name), Some("failed")) = (non_empty(server, "name"), server["status"].as_str()) else { continue };
            if self.mcp_failed.insert(name.to_string()) {
                Self::push(
                    out,
                    notice_event(
                        "mcp_server_failed",
                        NoticeLevel::Warning,
                        format!("MCP server \"{name}\" failed to connect"),
                    ),
                );
            }
        }
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
                // 新的响应开始流式输出 = 重试成功了
                if self.retrying {
                    self.retrying = false;
                    self.activity(out, ActivityState::None);
                }
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
        // 本地斜杠命令（`/context`、`/cost`）的输出：`-p` 下是一条模型为 `<synthetic>`、带 `local_command_source` 的 assistant 消息
        // （`<local-command-stdout>…` 原文），不是模型说的话
        if let Some(source) = v["local_command_source"].as_str() {
            let text = blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
            let level =
                if source.starts_with("<local-command-stderr>") { NoticeLevel::Warning } else { NoticeLevel::Info };
            self.command_output(&text, level, out);
            return;
        }
        if top && msg["model"].as_str() != Some("<synthetic>") {
            // 这条请求发给模型的上下文大小：输入 + 缓存读写（同一个 `message.id` 的每个 content block 都带同样的输入用量）
            let u = &msg["usage"];
            if u["input_tokens"].is_number() {
                self.context_tokens = Some(
                    u64_of(u, "input_tokens").unwrap_or(0)
                        + u64_of(u, "cache_read_input_tokens").unwrap_or(0)
                        + u64_of(u, "cache_creation_input_tokens").unwrap_or(0),
                );
            }
            if let Some(m) = non_empty(msg, "model") {
                self.last_model = Some(m.to_string());
            }
        }
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

            // 调用马上返回、任务转到后台：结果标 background（后台命令还没有退出码），等 `task_notification` 补最终结果
            let bg_started = self.bg_started.remove(&id);
            let background = !is_error
                && (bg_started
                    || tur["backgroundTaskId"].is_string()
                    || (category == ToolCategory::Subagent && tur["status"].as_str() == Some("async_launched")));
            let mut detail = ToolResultDetail::default();
            match category {
                _ if background => {
                    detail.background = Some(true);
                    self.backgrounded.insert(id.clone(), category);
                }
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
                ToolCategory::Subagent => detail.total_tokens = u64_of(tur, "totalTokens"),
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
            context_window: self.context_window_of(&v["modelUsage"]),
            context_tokens: self.context_tokens,
        };
        let usage = match self.carry.take() {
            Some(c) => merge_usage(c, usage),
            None => usage,
        };
        // CLI 自己的一轮结束了，有用户消息在等它：上层眼里这一轮已经是用户的 turn（`send` 已经返回），所以不发 `TurnDone`，
        // 把消息写进去、用量留到用户 turn 结束时一起报。用户在这期间要求了取消的（这一轮自己正常结束了也一样），
        // 消息不再写，按取消收尾——用户的消息还没送出去
        if let Some(msg) = self.deferred.take() {
            self.agent_turn = false;
            if !self.cancel_requested {
                self.carry = Some(usage);
                self.reset_turn_tables();
                out.push(Out::Write(msg));
                return;
            }
            reason = TurnDoneReason::Cancelled;
        }
        self.busy = false;
        self.agent_turn = false;
        self.cancel_requested = false;
        Self::push(out, Event::TurnDone { turn_id: self.turn_counter, reason, usage: Some(usage) });
    }

    /// `result.modelUsage`（模型 → 用量，含 `contextWindow`）里主模型的上下文窗口：先看本 turn 最近一条顶层 assistant 消息的模型，
    /// 再看 `system/init` 的模型，最后只有一个条目时取它。子 agent 用别的模型时 `modelUsage` 里不止一项，不能随便取。
    fn context_window_of(&self, model_usage: &Value) -> Option<u64> {
        let map = model_usage.as_object()?;
        let by_name = |name: Option<&str>| name.and_then(|n| map.get(n)).and_then(|m| u64_of(m, "contextWindow"));
        by_name(self.last_model.as_deref()).or_else(|| by_name(self.model.as_deref())).or_else(|| {
            if map.len() == 1 {
                map.values().next().and_then(|m| u64_of(m, "contextWindow"))
            } else {
                None
            }
        })
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
            // MCP server 向用户要输入（CLI 2.1.308 起 stream-json 宿主默认会收到它；不答的话 CLI 一直等）
            Some("elicitation") => match permissions::from_elicitation(request_id, req) {
                Ok((pending, native)) => out.push(Out::Permission { pending, native }),
                Err(why) => {
                    // 不能交给用户的请求（URL 不是 http(s)）：直接 decline，并告诉用户发生了什么
                    out.push(Out::Reply(permissions::control_response(request_id, json!({ "action": "decline" }))));
                    Self::push(
                        out,
                        Event::Error { code: "elicitation_rejected".to_string(), message: why, recoverable: true },
                    );
                }
            },
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
