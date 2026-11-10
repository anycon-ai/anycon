//! `anycon mcp` 的工具：每个工具映射到 daemon 已有的 RPC 方法，不增加业务接口（ADR-49）。
//!
//! | 工具 | RPC |
//! |---|---|
//! | `list_agents` | `agent.list` |
//! | `list_projects` | `workspace.list` |
//! | `list_sessions` | `session.list` |
//! | `read_session` | `session.get` + `session.history` |
//! | `search_sessions` | `session.search` |
//! | `create_session` | `agent.list` + `session.create`（等待时再加 `session.get` / `session.history` / `permission.list`） |
//! | `send_message` | `session.send`（同上） |
//! | `cancel_turn` | `session.cancel` |
//! | `get_diff` | `session.diff` |
//!
//! 安全边界（ADR-49）：
//! - **没有任何批准 / 拒绝权限请求的工具**。等待中的会话进入等待审批就立即返回 `waiting_permission`，由人在已配对设备上处理；
//! - `create_session` 拒绝 `risk = dangerous` 的权限模式（包括没指定模式、默认模式恰好是危险的情形），除非启动时带了
//!   `--allow-dangerous-modes`；schema 里没有会话级 `env`；
//! - 在 anycon 会话里启动的（环境里有 `ANYCON_AGENT`）默认拒绝会改状态的工具，`--allow-nested` 放开；
//! - 这里不保存任何状态、不调度、不在会话之间转发消息：每个工具调用都只是一次对 daemon 的查询或操作。
//!
//! 等一轮结束靠轮询 `session.history`（按 seq 增量）而不是 `session.watch`：watch 数有上限（每个连接 5 个），而一个 MCP 客户端
//! 可以并发调用多次；轮询对并发和断线重连都没有额外状态。

use std::future::Future;
use std::time::{Duration, Instant};

use anycon_protocol::{
    methods, AgentInfo, AgentListResult, AgentStatus, DiffAgainst, DiffFile, Event, ModeRisk, NodeInfo, PermissionKind,
    PermissionListResult, SearchHit, SessionDiffResult, SessionEvent, SessionHistoryResult, SessionListResult,
    SessionSearchResult, SessionSendResult, SessionStatus, SessionSummary, SessionWorkspaceMode, TurnDoneReason, Usage,
    WorkspaceListResult,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use super::protocol::{ToolDef, ToolHost, ToolOutput};
use crate::error::{CliError, Result};

/// `wait` 缺省的等待上限（秒）。比常见 MCP 客户端的工具调用超时（60 秒）短一点：先超时的是我们，调用方能拿到
/// 「仍在运行」而不是一个客户端超时错误。
pub const DEFAULT_WAIT_SECS: u64 = 50;
/// `timeout_seconds` 的上限。
pub const MAX_WAIT_SECS: u64 = 600;
/// 返回的最终文字最多这么多字符（超出保留末尾，最终答案通常在最后）。
const MAX_TEXT_CHARS: usize = 30_000;
const LIST_DEFAULT: u32 = 20;
const LIST_MAX: u32 = 100;
const SEARCH_DEFAULT: u32 = 20;
const SEARCH_MAX: u32 = 50;
const READ_DEFAULT: u32 = 20;
const READ_MAX: u32 = 100;
/// `read_session` 里每条消息最多这么多字符。
const READ_MESSAGE_CHARS: usize = 4_000;
/// `read_session` 一次返回的文字总量上限。
const READ_TOTAL_CHARS: usize = 60_000;
/// `read_session` 为凑够条数最多往前翻几页历史。
const READ_MAX_PAGES: usize = 10;
/// `get_diff` 最多列出几个文件。
const DIFF_MAX_FILES: usize = 200;
/// `get_diff` 单个文件的补丁最多这么多字符。
const DIFF_PATCH_CHARS: usize = 100_000;
const TITLE_CHARS: usize = 200;

/// 到 daemon 的 RPC 通道（真实实现在 `mcp.rs`，测试里是脚本化的假实现）。
pub trait Node: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> impl Future<Output = Result<Value>> + Send;
}

/// 启动选项。
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// `--allow-dangerous-modes`。
    pub allow_dangerous_modes: bool,
    /// 会改状态的工具被拒绝（在 anycon 会话里启动、且没有 `--allow-nested`）。
    pub block_mutations: bool,
}

pub struct AnyconTools<N: Node> {
    node: N,
    opts: Options,
}

/// 会改状态的工具。
const MUTATING: &[&str] = &["create_session", "send_message", "cancel_turn"];

/// 工具名，按展示顺序。
pub const TOOL_NAMES: &[&str] = &[
    "list_agents",
    "list_projects",
    "list_sessions",
    "read_session",
    "search_sessions",
    "create_session",
    "send_message",
    "cancel_turn",
    "get_diff",
];

fn clip(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    (s.chars().take(max).collect(), true)
}

fn clip_marked(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [{} more characters]", total - max)
}

/// 保留末尾 `max` 个字符。
fn tail(s: &str, max: usize) -> (String, bool) {
    let total = s.chars().count();
    if total <= max {
        return (s.to_string(), false);
    }
    (s.chars().skip(total - max).collect(), true)
}

fn status_str(s: SessionStatus) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn kind_str(k: PermissionKind) -> String {
    serde_json::to_value(k).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

/// RPC 错误的文字：稳定错误码 + 消息（`data.reason` 已经附在消息里）。
fn fmt_err(e: CliError) -> String {
    format!("{}: {}", e.code, e.message)
}

fn is_terminal(s: SessionStatus) -> bool {
    matches!(s, SessionStatus::Error | SessionStatus::Ended | SessionStatus::Interrupted)
}

// ============================================================ 参数

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StatusFilter {
    Running,
    Waiting,
    Idle,
    Ended,
    Error,
    Queued,
}

impl StatusFilter {
    fn matches(self, s: SessionStatus) -> bool {
        match self {
            StatusFilter::Running => matches!(s, SessionStatus::Running | SessionStatus::Starting),
            StatusFilter::Waiting => s == SessionStatus::WaitingPermission,
            StatusFilter::Idle => s == SessionStatus::Idle,
            StatusFilter::Ended => matches!(s, SessionStatus::Ended | SessionStatus::Interrupted),
            StatusFilter::Error => s == SessionStatus::Error,
            StatusFilter::Queued => s == SessionStatus::Queued,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSessionsArgs {
    project: Option<String>,
    status: Option<StatusFilter>,
    #[serde(default)]
    include_archived: bool,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadSessionArgs {
    session: String,
    limit: Option<u32>,
    before_seq: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    project: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    project: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    mode: Option<String>,
    worktree: Option<bool>,
    prompt: String,
    #[serde(default = "yes")]
    wait: bool,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    session: String,
    message: String,
    #[serde(default = "yes")]
    wait: bool,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    session: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    session: String,
    uncommitted: Option<bool>,
    path: Option<String>,
}

fn parse<T: DeserializeOwned>(args: Value) -> std::result::Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

fn wait_duration(t: Option<u64>) -> Duration {
    Duration::from_secs(t.unwrap_or(DEFAULT_WAIT_SECS).clamp(1, MAX_WAIT_SECS))
}

// ============================================================ 工具声明

fn session_prop() -> Value {
    json!({
        "type": "string",
        "description": "Session id: the full id (sess_...), a unique prefix of it, or `last` for the most recently updated session."
    })
}

fn wait_props() -> (Value, Value) {
    (
        json!({
            "type": "boolean",
            "default": true,
            "description": "Wait until the agent finishes this turn and return its final text. With false, return as soon as the message is accepted and check progress with read_session."
        }),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": MAX_WAIT_SECS,
            "default": DEFAULT_WAIT_SECS,
            "description": format!("How long to wait, in seconds (default {DEFAULT_WAIT_SECS}, at most {MAX_WAIT_SECS}). When it runs out the turn keeps running on the node and the result says `still_running`. Your MCP client may have its own, shorter tool-call timeout.")
        }),
    )
}

fn definitions() -> Vec<ToolDef> {
    let (wait, timeout) = wait_props();
    let obj = |props: Value, required: Vec<&str>| json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false });
    vec![
        ToolDef {
            name: "list_agents",
            title: "List agents",
            description: "List the coding agents installed on the anycon node, with their status, models (and thinking levels) and permission modes. \
                Use the ids it returns for create_session. Permission modes have a risk level (safe, normal, dangerous); create_session refuses dangerous ones unless this server was started with --allow-dangerous-modes."
                .into(),
            input_schema: obj(json!({}), vec![]),
            read_only: true,
        },
        ToolDef {
            name: "list_projects",
            title: "List projects",
            description: "List the projects (folders in the node's workspace) that sessions can work in: name, git branch and dirty state, and how many sessions are active in each."
                .into(),
            input_schema: obj(json!({}), vec![]),
            read_only: true,
        },
        ToolDef {
            name: "list_sessions",
            title: "List sessions",
            description: "List sessions on the node, most recently updated first. Each entry has the id, title, project, agent, status (queued, starting, running, waiting_permission, idle, ended, error, interrupted) and the number of pending permission requests."
                .into(),
            input_schema: obj(
                json!({
                    "project": { "type": "string", "description": "Only sessions in this project." },
                    "status": {
                        "type": "string",
                        "enum": ["running", "waiting", "idle", "ended", "error", "queued"],
                        "description": "Only sessions in this state (`waiting` = waiting for a permission decision; `ended` includes interrupted)."
                    },
                    "include_archived": { "type": "boolean", "default": false, "description": "Include archived sessions." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": LIST_MAX, "default": LIST_DEFAULT, "description": "Maximum number of sessions to return." }
                }),
                vec![],
            ),
            read_only: true,
        },
        ToolDef {
            name: "read_session",
            title: "Read a session",
            description: format!(
                "Read a session: its state (status, agent, model, mode, usage, pending permission requests) and the text of its most recent messages (user messages and the agent's replies; tool output is not included). \
                 Output is capped (each message at {READ_MESSAGE_CHARS} characters, about {READ_TOTAL_CHARS} in total). To read further back, pass the returned `next_before_seq` as `before_seq`."
            ),
            input_schema: obj(
                json!({
                    "session": session_prop(),
                    "limit": { "type": "integer", "minimum": 1, "maximum": READ_MAX, "default": READ_DEFAULT, "description": "How many messages to return, counting back from the newest (or from before_seq)." },
                    "before_seq": { "type": "integer", "minimum": 1, "description": "Return messages before this event sequence number (use next_before_seq from a previous result)." }
                }),
                vec!["session"],
            ),
            read_only: true,
        },
        ToolDef {
            name: "search_sessions",
            title: "Search sessions",
            description: "Full-text search over the messages of all sessions on the node. The query is whitespace-separated words; each word matches as a prefix and all must appear in the same piece of text. Returns the session id, the sequence number of the match, the title and a snippet (matches wrapped in **)."
                .into(),
            input_schema: obj(
                json!({
                    "query": { "type": "string", "minLength": 1, "description": "Words to search for." },
                    "project": { "type": "string", "description": "Only search sessions in this project." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": SEARCH_MAX, "default": SEARCH_DEFAULT, "description": "Maximum number of hits." }
                }),
                vec!["query"],
            ),
            read_only: true,
        },
        ToolDef {
            name: "create_session",
            title: "Create a session",
            description: "Start a new agent session on the node and send it a first message. Every permission request the agent raises waits for a human on a paired device: this server cannot approve anything, \
                 and a wait ends early with `waiting_permission` when that happens. Permission modes marked dangerous in list_agents are refused (this includes leaving `mode` out when the agent's default mode is dangerous). \
                 With wait=true (default) it waits up to timeout_seconds for the first turn and returns the agent's final text, the session status and a usage summary; otherwise it returns the new session right away. \
                 If `outcome` is `still_running`, check later with read_session."
            .into(),
            input_schema: obj(
                json!({
                    "prompt": { "type": "string", "minLength": 1, "description": "The first message to send to the agent." },
                    "project": { "type": "string", "description": "Project name (see list_projects). Leave out for a scratch session in its own empty folder." },
                    "agent": { "type": "string", "description": "Agent id (see list_agents). Optional when exactly one agent is ready." },
                    "model": { "type": "string", "description": "Model id from list_agents. Default: the agent's default." },
                    "thinking": { "type": "string", "description": "Thinking level of the chosen model, from list_agents." },
                    "mode": { "type": "string", "description": "Permission mode id from list_agents. Dangerous modes are refused." },
                    "worktree": { "type": "boolean", "description": "true: work in a new git worktree and branch (the project must be a git repository root with a commit). false: work in the project folder. Default: the node's setting." },
                    "wait": wait,
                    "timeout_seconds": timeout
                }),
                vec!["prompt"],
            ),
            read_only: false,
        },
        ToolDef {
            name: "send_message",
            title: "Send a message to a session",
            description: "Send a message to an existing session. With wait=true (default) it waits up to timeout_seconds for the agent to finish the turn that handles this message (when the session is busy, the message is queued and that is the next turn) and returns that turn's text (segments around tool calls separated by a blank line), the session status and a usage summary. \
                 Refused when the session is in a permission mode marked dangerous, unless this server was started with --allow-dangerous-modes. \
                 If the session starts waiting for a permission decision it returns immediately with outcome `waiting_permission` and the request's title and kind: that has to be decided by a person on a paired device, this server cannot do it. \
                 If the time runs out it returns `still_running`; the turn (or the queued message) stays on the node, so check later with read_session. If the queued message is dropped (someone cancelled the session's queue) it returns `dropped`."
            .into(),
            input_schema: obj(
                json!({
                    "session": session_prop(),
                    "message": { "type": "string", "minLength": 1, "description": "The message text." },
                    "wait": wait,
                    "timeout_seconds": timeout
                }),
                vec!["session", "message"],
            ),
            read_only: false,
        },
        ToolDef {
            name: "cancel_turn",
            title: "Cancel the current turn",
            description: "Interrupt the session's current turn. The session stays and can receive more messages.".into(),
            input_schema: obj(json!({ "session": session_prop() }), vec!["session"]),
            read_only: false,
        },
        ToolDef {
            name: "get_diff",
            title: "Get a session's changes",
            description: format!(
                "Show what a session changed in its working folder. Without `path`: the list of changed files with added/deleted line counts (at most {DIFF_MAX_FILES} files). \
                 With `path`: that file's unified diff as a second text block (capped at {DIFF_PATCH_CHARS} characters). \
                 For a session in its own git worktree the default is everything since its base; a session working in the project folder only has uncommitted changes."
            ),
            input_schema: obj(
                json!({
                    "session": session_prop(),
                    "uncommitted": { "type": "boolean", "description": "Only uncommitted changes (always the case for a session working in the project folder)." },
                    "path": { "type": "string", "description": "A changed file's path as listed without `path`; returns its diff." }
                }),
                vec!["session"],
            ),
            read_only: true,
        },
    ]
}

// ============================================================ 实现

impl<N: Node> AnyconTools<N> {
    pub fn new(node: N, opts: Options) -> Self {
        AnyconTools { node, opts }
    }

    async fn rpc<T: DeserializeOwned>(&self, method: &str, params: Value) -> std::result::Result<T, String> {
        let v = self.node.call(method, params).await.map_err(fmt_err)?;
        serde_json::from_value(v).map_err(|e| format!("unexpected response to {method}: {e}"))
    }

    async fn dispatch(&self, name: &str, args: Value) -> std::result::Result<ToolOutput, String> {
        if self.opts.block_mutations && MUTATING.contains(&name) {
            return Err(format!(
                "{name} is disabled: this MCP server was started inside an anycon agent session (ANYCON_AGENT is set in its environment), \
                 and an agent that starts or drives other sessions is refused by default. The read-only tools still work. \
                 If this is intended, a person can start the server with `anycon mcp --allow-nested`."
            ));
        }
        match name {
            "list_agents" => {
                parse::<NoArgs>(args)?;
                self.list_agents().await
            }
            "list_projects" => {
                parse::<NoArgs>(args)?;
                self.list_projects().await
            }
            "list_sessions" => self.list_sessions(parse(args)?).await,
            "read_session" => self.read_session(parse(args)?).await,
            "search_sessions" => self.search_sessions(parse(args)?).await,
            "create_session" => self.create_session(parse(args)?).await,
            "send_message" => self.send_message(parse(args)?).await,
            "cancel_turn" => self.cancel_turn(parse(args)?).await,
            "get_diff" => self.get_diff(parse(args)?).await,
            other => Err(format!("unknown tool: {other}")),
        }
    }

    /// 会话引用：完整 id、id 前缀（`sess_` 可省）、`last`。
    async fn resolve(&self, key: &str) -> std::result::Result<String, String> {
        let key = key.trim();
        if key.starts_with("sess_") && key.len() == 27 {
            return Ok(key.to_string());
        }
        let l: SessionListResult = self.rpc(methods::SESSION_LIST, json!({ "include_archived": true })).await?;
        if key == "last" {
            return l.sessions.first().map(|s| s.id.clone()).ok_or_else(|| "there are no sessions yet".to_string());
        }
        if key.is_empty() {
            return Err("session must not be empty".into());
        }
        let m: Vec<&SessionSummary> = l
            .sessions
            .iter()
            .filter(|s| s.id.starts_with(key) || s.id.strip_prefix("sess_").is_some_and(|r| r.starts_with(key)))
            .collect();
        match m.len() {
            1 => Ok(m[0].id.clone()),
            0 => Err(format!("no session matches {key:?}")),
            n => Err(format!("{key:?} matches {n} sessions; use a longer prefix")),
        }
    }

    fn mode_selectable(&self, risk: ModeRisk) -> bool {
        risk != ModeRisk::Dangerous || self.opts.allow_dangerous_modes
    }

    async fn list_agents(&self) -> std::result::Result<ToolOutput, String> {
        let r: AgentListResult = self.rpc(methods::AGENT_LIST, json!({ "refresh": false })).await?;
        let agents: Vec<Value> = r
            .agents
            .iter()
            .map(|a| {
                let mut v = json!({
                    "id": a.id,
                    "label": a.label,
                    "status": serde_json::to_value(a.status).unwrap_or(Value::Null),
                    "models": a.models.iter().map(|m| {
                        let mut mv = json!({ "id": m.id, "label": m.label, "default": m.default });
                        if let Some(t) = &m.thinking {
                            mv["thinking"] = json!({ "levels": t.options.iter().map(|o| &o.id).collect::<Vec<_>>(), "default": t.default });
                        }
                        mv
                    }).collect::<Vec<_>>(),
                    "modes": a.modes.iter().map(|m| {
                        let mut mv = json!({
                            "id": m.id,
                            "label": m.label,
                            "default": m.default,
                            "risk": serde_json::to_value(m.risk).unwrap_or(Value::Null),
                            "selectable": self.mode_selectable(m.risk),
                        });
                        if let Some(d) = &m.description {
                            mv["description"] = json!(clip(d, 300).0);
                        }
                        mv
                    }).collect::<Vec<_>>(),
                });
                if let Some(ver) = &a.version {
                    v["version"] = json!(ver);
                }
                if let Some(i) = &a.issue {
                    v["issue"] = json!({ "code": i.code, "message": clip(&i.message, 300).0 });
                }
                v
            })
            .collect();
        Ok(ToolOutput::json(&json!({ "agents": agents })))
    }

    async fn list_projects(&self) -> std::result::Result<ToolOutput, String> {
        let r: WorkspaceListResult = self.rpc(methods::WORKSPACE_LIST, json!({})).await?;
        let projects: Vec<Value> = r
            .projects
            .iter()
            .map(|p| {
                let mut v = json!({
                    "name": p.name,
                    "path": p.path,
                    "kind": serde_json::to_value(p.kind).unwrap_or(Value::Null),
                    "active_sessions": p.active_sessions,
                });
                if let Some(g) = &p.git {
                    v["git"] = json!({ "branch": g.branch, "dirty": g.dirty, "worktree_ready": g.worktree_ready });
                }
                if p.missing {
                    v["missing"] = json!(true);
                }
                v
            })
            .collect();
        Ok(ToolOutput::json(&json!({ "projects": projects })))
    }

    async fn list_sessions(&self, a: ListSessionsArgs) -> std::result::Result<ToolOutput, String> {
        let r: SessionListResult = self
            .rpc(methods::SESSION_LIST, json!({ "project": a.project, "include_archived": a.include_archived }))
            .await?;
        let limit = a.limit.unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX) as usize;
        let total = r.sessions.iter().filter(|s| a.status.is_none_or(|f| f.matches(s.status))).count();
        let sessions: Vec<Value> = r
            .sessions
            .iter()
            .filter(|s| a.status.is_none_or(|f| f.matches(s.status)))
            .take(limit)
            .map(summary_json)
            .collect();
        Ok(ToolOutput::json(&json!({ "sessions": sessions, "total_matching": total })))
    }

    async fn read_session(&self, a: ReadSessionArgs) -> std::result::Result<ToolOutput, String> {
        let sid = self.resolve(&a.session).await?;
        let s: SessionSummary = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await?;
        let limit = a.limit.unwrap_or(READ_DEFAULT).clamp(1, READ_MAX) as usize;
        let mut before = a.before_seq.unwrap_or(s.last_seq + 1);
        // 事件按 seq 升序；往前翻页直到凑够条数（一条助手消息由很多 text_delta 组成，一页事件对应的消息数不定）
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut has_more = true;
        for _ in 0..READ_MAX_PAGES {
            let page: SessionHistoryResult = self
                .rpc(methods::SESSION_HISTORY, json!({ "session_id": sid, "before_seq": before, "limit": 1000 }))
                .await?;
            has_more = page.has_more;
            let Some(first) = page.events.first().map(|e| e.seq) else {
                has_more = false;
                break;
            };
            let mut merged = page.events;
            merged.append(&mut events);
            events = merged;
            // 防死循环：节点说还有更多、页却没有带来更早的事件
            if first >= before {
                break;
            }
            before = first;
            if !has_more || build_messages(&events).len() > limit {
                break;
            }
        }
        let mut msgs = build_messages(&events);
        let mut more_before = has_more;
        if msgs.len() > limit {
            msgs.drain(..msgs.len() - limit);
            more_before = true;
        }
        // 总量上限：从最新的往回数
        let mut budget = READ_TOTAL_CHARS;
        let mut keep = 0;
        for m in msgs.iter().rev() {
            let n = m.text.chars().count().min(READ_MESSAGE_CHARS) + 20;
            if keep > 0 && n > budget {
                break;
            }
            budget = budget.saturating_sub(n);
            keep += 1;
        }
        if keep < msgs.len() {
            msgs.drain(..msgs.len() - keep);
            more_before = true;
        }
        let next_before_seq = msgs.first().map(|m| m.seq);
        let mut out = json!({
            "session": summary_json(&s),
            "messages": msgs.iter().map(|m| json!({
                "seq": m.seq,
                "role": m.role,
                "text": clip_marked(&m.text, READ_MESSAGE_CHARS),
            })).collect::<Vec<_>>(),
            "has_more_before": more_before,
        });
        if more_before {
            out["next_before_seq"] = json!(next_before_seq);
        }
        if s.status == SessionStatus::WaitingPermission {
            out["waiting_permission"] = self.pending_json(&sid).await;
        }
        Ok(ToolOutput::json(&out))
    }

    /// 会话在等的权限请求（标题和种类；不含 request_id 和细节）。
    async fn pending_json(&self, sid: &str) -> Value {
        let list: std::result::Result<PermissionListResult, String> =
            self.rpc(methods::PERMISSION_LIST, json!({ "session_id": sid })).await;
        let requests: Vec<Value> = list
            .map(|l| {
                l.requests
                    .iter()
                    .take(5)
                    .map(|r| json!({ "kind": kind_str(r.kind), "title": clip(&r.title, TITLE_CHARS).0 }))
                    .collect()
            })
            .unwrap_or_default();
        json!({ "requests": requests, "note": WAITING_NOTE })
    }

    async fn search_sessions(&self, a: SearchArgs) -> std::result::Result<ToolOutput, String> {
        if a.query.trim().is_empty() {
            return Err("query must not be empty".into());
        }
        let limit = a.limit.unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX);
        let r: SessionSearchResult = self
            .rpc(methods::SESSION_SEARCH, json!({ "query": a.query, "project": a.project, "limit": limit }))
            .await?;
        let hits: Vec<Value> = r
            .hits
            .iter()
            .map(|h: &SearchHit| {
                json!({
                    "session_id": h.session_id,
                    "seq": h.seq,
                    "title": clip(&h.title, TITLE_CHARS).0,
                    "snippet": clip(&plain_snippet(&h.snippet), 600).0,
                    "ts_ms": h.ts_ms,
                })
            })
            .collect();
        Ok(ToolOutput::json(&json!({ "hits": hits })))
    }

    async fn create_session(&self, a: CreateArgs) -> std::result::Result<ToolOutput, String> {
        if a.prompt.trim().is_empty() {
            return Err("prompt must not be empty".into());
        }
        let project = a.project.clone().filter(|p| !p.is_empty());
        let agents: AgentListResult = self.rpc(methods::AGENT_LIST, json!({ "refresh": false })).await?;
        let agent = self.pick_agent(&agents, a.agent.as_deref())?;
        self.check_mode(agent, a.mode.as_deref(), false)?;
        let workspace = self.choose_workspace(project.as_deref(), a.worktree).await?;
        let message_id = uuid::Uuid::new_v4().to_string();
        let mut params = json!({
            "project": project,
            "agent": agent.id,
            "prompt": a.prompt,
            "message_id": message_id,
            "settings": { "model": a.model, "thinking": a.thinking, "mode": a.mode },
        });
        if let Some(w) = &workspace.workspace {
            params["workspace"] = w.clone();
        }
        let created: std::result::Result<SessionSummary, String> =
            self.rpc(methods::SESSION_CREATE, params.clone()).await;
        let s = match created {
            // 按节点默认值选的 worktree，而项目开不了（不是仓库根目录 / 没有提交）：退回项目目录，不算错
            Err(e) if workspace.defaulted && e.contains("(not_worktree_ready)") => {
                params.as_object_mut().map(|m| m.remove("workspace"));
                self.rpc(methods::SESSION_CREATE, params).await?
            }
            other => other?,
        };
        if !a.wait {
            return Ok(ToolOutput::json(&json!({ "session": summary_json(&s), "message_id": message_id })));
        }
        let outcome = self.wait_turn(&s.id, 0, &message_id, None, wait_duration(a.timeout_seconds)).await?;
        Ok(ToolOutput::json(&outcome.to_json(&s.id)))
    }

    fn pick_agent<'a>(
        &self,
        agents: &'a AgentListResult,
        wanted: Option<&str>,
    ) -> std::result::Result<&'a AgentInfo, String> {
        let ids = || agents.agents.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ");
        if let Some(w) = wanted {
            let a = agents
                .agents
                .iter()
                .find(|a| a.id == w)
                .ok_or_else(|| format!("unknown agent {w:?}; the node has: {}", ids()))?;
            return Ok(a);
        }
        let ready: Vec<&AgentInfo> = agents.agents.iter().filter(|a| a.status == AgentStatus::Ready).collect();
        match ready.len() {
            1 => Ok(ready[0]),
            0 => Err(format!("no agent is ready on the node (agents: {})", ids())),
            _ => Err(format!(
                "several agents are ready ({}); choose one with `agent`",
                ready.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", ")
            )),
        }
    }

    /// 拒绝危险的权限模式，以及判断不了风险的模式（除非 `--allow-dangerous-modes`）。
    ///
    /// `existing = false`：`create_session` 要用的模式（`requested`，没指定就是 agent 的默认模式）；
    /// `existing = true`：`send_message` 要发消息的会话当前所处的模式。两处的规则一样：模式的 `risk = dangerous` 就拒绝；
    /// 模式不在 daemon 给的清单（`agent.list`）里也拒绝——风险未知就不放行，而不是按「没找到就当安全」处理。
    /// 清单来自 daemon，这里不内置任何 agent 的知识；daemon 自己的 `permissions.allow_dangerous_modes` 是另一道、更高的闸：
    /// 它关着的时候危险模式根本不在清单里。
    fn check_mode(
        &self,
        agent: &AgentInfo,
        requested: Option<&str>,
        existing: bool,
    ) -> std::result::Result<(), String> {
        if self.opts.allow_dangerous_modes {
            return Ok(());
        }
        let effective = match requested {
            Some(m) => agent.modes.iter().find(|x| x.id == m),
            None => agent.modes.iter().find(|x| x.default),
        };
        let safe: Vec<&str> =
            agent.modes.iter().filter(|m| m.risk != ModeRisk::Dangerous).map(|m| m.id.as_str()).collect();
        let hint = if safe.is_empty() { String::new() } else { format!(" (available: {})", safe.join(", ")) };
        let Some(mode) = effective else {
            // 这个 agent 根本没有权限模式：没有什么可选、也没有什么危险
            if requested.is_none() && agent.modes.is_empty() {
                return Ok(());
            }
            return Err(match requested {
                Some(m) => format!(
                    "permission mode {m:?} is not one of the modes agent {:?} offers (see list_agents); a mode the node does not list cannot be checked, so it is refused{}",
                    agent.id,
                    if existing { "" } else { &hint }
                ),
                None => format!(
                    "agent {:?} has no default permission mode in list_agents, so the risk of the mode it would run in cannot be checked; choose a mode with the `mode` argument{hint}",
                    agent.id
                ),
            });
        };
        if mode.risk != ModeRisk::Dangerous {
            return Ok(());
        }
        let flag = "This MCP server refuses dangerous modes unless a person starts it with `anycon mcp --allow-dangerous-modes`.";
        Err(if existing {
            format!(
                "this session is in permission mode {:?} of agent {:?}, which is marked dangerous: messages sent to it run tools without asking a person. {flag} A person can switch the session to another mode first.",
                mode.id, agent.id
            )
        } else if requested.is_some() {
            format!(
                "permission mode {:?} of agent {:?} is marked dangerous: it would let the agent act without asking a person. {flag} Choose another mode with the `mode` argument{hint}.",
                mode.id, agent.id
            )
        } else {
            format!(
                "the default permission mode ({:?}) of agent {:?} is marked dangerous and no `mode` was given: it would let the agent act without asking a person. {flag} Choose another mode with the `mode` argument{hint}.",
                mode.id, agent.id
            )
        })
    }

    async fn choose_workspace(
        &self,
        project: Option<&str>,
        worktree: Option<bool>,
    ) -> std::result::Result<WorkspaceChoice, String> {
        match worktree {
            Some(true) => {
                if project.is_none() {
                    return Err("worktree needs a project; scratch sessions work in their own folder".into());
                }
                return Ok(WorkspaceChoice { workspace: Some(json!({ "mode": "worktree" })), defaulted: false });
            }
            Some(false) => {
                return Ok(WorkspaceChoice { workspace: Some(json!({ "mode": "local" })), defaulted: false })
            }
            None => {}
        }
        if project.is_none() {
            return Ok(WorkspaceChoice { workspace: None, defaulted: false });
        }
        // 节点信息拿不到（老 daemon）就当没有这个功能
        let info: Option<NodeInfo> = self.rpc(methods::NODE_INFO, json!({})).await.ok();
        let wants = info
            .as_ref()
            .and_then(|i| i.worktree.as_ref().filter(|_| i.capabilities.worktree))
            .is_some_and(|w| w.default_mode == SessionWorkspaceMode::Worktree);
        Ok(if wants {
            WorkspaceChoice { workspace: Some(json!({ "mode": "worktree" })), defaulted: true }
        } else {
            WorkspaceChoice { workspace: None, defaulted: false }
        })
    }

    async fn send_message(&self, a: SendArgs) -> std::result::Result<ToolOutput, String> {
        if a.message.trim().is_empty() {
            return Err("message must not be empty".into());
        }
        let sid = self.resolve(&a.session).await?;
        let before: SessionSummary = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await?;
        if before.archived {
            return Err(format!("session {sid} is archived; a person has to unarchive it first (`anycon unarchive`)"));
        }
        // 不带 `--allow-dangerous-modes` 时不向处在危险模式的会话发消息：那等于驱动一个不问人就跑工具的 agent
        if !self.opts.allow_dangerous_modes {
            let agents: AgentListResult = self.rpc(methods::AGENT_LIST, json!({ "refresh": false })).await?;
            let agent = agents.agents.iter().find(|a| a.id == before.agent).ok_or_else(|| {
                format!(
                    "the agent of this session ({:?}) is not listed by the node, so the risk of the session's permission mode cannot be checked",
                    before.agent
                )
            })?;
            self.check_mode(agent, before.settings.mode.as_deref(), true)?;
        }
        let message_id = uuid::Uuid::new_v4().to_string();
        let sent: SessionSendResult = self
            .rpc(methods::SESSION_SEND, json!({ "session_id": sid, "message_id": message_id, "content": a.message }))
            .await?;
        if !a.wait {
            return Ok(ToolOutput::json(&json!({
                "session_id": sid,
                "sent": true,
                "queued": sent.queued,
                "message_id": message_id,
            })));
        }
        let outcome =
            self.wait_turn(&sid, before.last_seq, &message_id, sent.turn_id, wait_duration(a.timeout_seconds)).await?;
        Ok(ToolOutput::json(&outcome.to_json(&sid)))
    }

    async fn cancel_turn(&self, a: CancelArgs) -> std::result::Result<ToolOutput, String> {
        let sid = self.resolve(&a.session).await?;
        let _: Value = self.rpc(methods::SESSION_CANCEL, json!({ "session_id": sid })).await?;
        Ok(ToolOutput::json(&json!({ "session_id": sid, "cancelled": true })))
    }

    async fn get_diff(&self, a: DiffArgs) -> std::result::Result<ToolOutput, String> {
        let sid = self.resolve(&a.session).await?;
        let s: SessionSummary = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await?;
        // 在项目目录本身工作的会话没有 base，只能看未提交的
        let uncommitted = a.uncommitted.unwrap_or(false) || s.worktree.is_none();
        let against = if uncommitted { DiffAgainst::Uncommitted } else { DiffAgainst::Base };
        let against_s = if uncommitted { "uncommitted" } else { "base" };
        if let Some(path) = a.path.filter(|p| !p.is_empty()) {
            let r: SessionDiffResult =
                self.rpc(methods::SESSION_DIFF, json!({ "session_id": sid, "against": against, "path": path })).await?;
            let Some(file) = r.files.into_iter().next() else {
                return Err(format!("{path:?} is not among the session's changes"));
            };
            let (patch, clipped) = clip(file.patch.as_deref().unwrap_or(""), DIFF_PATCH_CHARS);
            let mut meta = diff_file_json(&file);
            meta["patch_truncated"] = json!(clipped || file.patch_truncated);
            let meta = json!({ "session_id": sid, "against": against_s, "from_sha": r.from_sha, "file": meta });
            let mut out = ToolOutput::json(&meta);
            if !patch.is_empty() {
                out.blocks.push(patch);
            }
            return Ok(out);
        }
        let r: SessionDiffResult =
            self.rpc(methods::SESSION_DIFF, json!({ "session_id": sid, "against": against })).await?;
        let total = r.files.len();
        let files: Vec<Value> = r.files.iter().take(DIFF_MAX_FILES).map(diff_file_json).collect();
        Ok(ToolOutput::json(&json!({
            "session_id": sid,
            "against": against_s,
            "from_sha": r.from_sha,
            "files": files,
            "files_truncated": r.truncated || total > DIFF_MAX_FILES,
            "total_files": total,
        })))
    }

    /// 等我发出的那条消息（`message_id`）被处理完。
    ///
    /// 要等的是**我的那一轮**，不是「下一个 `turn_done`」：会话忙着的时候，daemon 在 `send` 时就写了 `user_message`，
    /// 消息进队列，等当前 turn 结束才开新 turn（`turn_start`），所以我的消息之后第一个 `turn_done` 往往是上一轮的。认法：
    /// - `session.send` 返回了 `turn_id`（会话空闲、新 turn 立刻开了；或者插话进了正在跑的 turn）：等这个 turn；
    /// - 否则（排队，`create_session` 也一样）：等我的 `user_message` 之后第一个 `turn_start`，等它的 `turn_done`；
    /// - 助手文字只取我这一轮的（事件的 `turn_id`）。
    ///
    /// 每一轮：按 seq 增量拉历史（累积文字、记下还没答复的权限请求、看有没有我那一轮的 `turn_done`），再看会话状态。
    /// 权限请求一出现就返回 `waiting_permission`：等下去也只会等到超时，因为这里没有任何办法答复它。
    /// 消息被丢掉（排队时有人 `session.cancel`）的情形靠状态判断：见下面 `Dropped` 的条件。
    async fn wait_turn(
        &self,
        sid: &str,
        start_seq: u64,
        message_id: &str,
        known_turn: Option<u64>,
        timeout: Duration,
    ) -> std::result::Result<TurnOutcome, String> {
        let deadline = Instant::now() + timeout;
        let mut after = start_seq;
        let mut saw = false;
        // 我的那一轮；插话进正在跑的 turn 时 `send` 一返回就知道，排队的要等到它的 `turn_start`
        let mut my_turn = known_turn;
        // 看到了我那一轮的 `turn_start`（在我的 `user_message` 之后）。没看到而 `my_turn` 已知 = 插话进了别的 turn
        let mut started_after_me = false;
        let mut text = String::new();
        let mut text_truncated = false;
        // 上一段文字之后出现过工具调用 / 文字块结束：下一段文字前插一个空行，免得连成一片
        let mut break_before_next = false;
        let mut open: Vec<(String, PermissionKind, String)> = Vec::new();
        let mut last_error: Option<(String, String)> = None;
        // 插话的 turn 结束了，但插话可能失败（消息退回队列、作为下一个 turn 发出）：先把这个结果放着，
        // 看会话里还有没有动静，有就改等我的消息真正进的那个 turn，否则才用这个结果
        let mut pending_done: Option<TurnOutcome> = None;
        // 插话期间看到了 daemon 在插话失败时发的 `status { queued_messages }`：消息很可能退回了队列
        let mut requeued_seen = false;
        let mut pending_idle = 0u32;
        let mut idle_polls = 0u32;
        let mut poll = Duration::from_millis(150);
        loop {
            let page: SessionHistoryResult = self
                .rpc(methods::SESSION_HISTORY, json!({ "session_id": sid, "after_seq": after, "limit": 500 }))
                .await?;
            let progressed = !page.events.is_empty();
            for ev in page.events {
                after = ev.seq;
                let ev_turn = ev.turn_id;
                match ev.event {
                    Event::UserMessage { message_id: m, .. } if m == message_id => saw = true,
                    Event::TurnStart { turn_id: t, message_id: first } if saw => {
                        if pending_done.is_some() {
                            // 插话失败的话，消息退回队列、随后和积压的消息合并成下一个 turn（`turn_start` 记的是积压里的
                            // 第一条，不一定是我的）。有退回的迹象就认下一个 turn；没有迹象时只认第一条就是我的那种
                            if requeued_seen || first == message_id {
                                pending_done = None;
                                my_turn = Some(t);
                                started_after_me = true;
                                requeued_seen = false;
                                text.clear();
                                text_truncated = false;
                                break_before_next = false;
                                last_error = None;
                            }
                        } else if my_turn.is_none() {
                            my_turn = Some(t);
                            started_after_me = true;
                        } else if my_turn == Some(t) {
                            started_after_me = true;
                        }
                    }
                    Event::TextDelta { text: t } if my_turn.is_some() && ev_turn.is_none_or(|x| Some(x) == my_turn) => {
                        if break_before_next && !text.is_empty() && !text.ends_with("\n\n") {
                            text.push_str("\n\n");
                        }
                        break_before_next = false;
                        text.push_str(&t);
                        // 只留末尾：超出上限一倍时砍一次，免得无限增长
                        if text.chars().count() > MAX_TEXT_CHARS * 2 {
                            text = tail(&text, MAX_TEXT_CHARS).0;
                            text_truncated = true;
                        }
                    }
                    Event::ToolCall { .. } | Event::TextDone {} if my_turn.is_some() => break_before_next = true,
                    Event::Permission { request_id, kind, title, .. } => open.push((request_id, kind, title)),
                    Event::PermissionResolved { request_id, .. } => open.retain(|(id, _, _)| *id != request_id),
                    Event::Error { code, message, .. } if saw && my_turn.is_some() => {
                        last_error = Some((code, message))
                    }
                    Event::TurnDone { turn_id: t, reason, usage } if saw && my_turn == Some(t) => {
                        let (txt, cut) = tail(&text, MAX_TEXT_CHARS);
                        let done = TurnOutcome::Done {
                            reason,
                            usage,
                            text: txt,
                            text_truncated: text_truncated || cut,
                            error: last_error.clone(),
                            session: None,
                        };
                        if started_after_me {
                            return self.finish_done(sid, done).await;
                        }
                        pending_done = Some(done);
                    }
                    Event::Status { state, queued_messages, .. }
                        if saw
                            && my_turn.is_some()
                            && !started_after_me
                            && pending_done.is_none()
                            && queued_messages.is_some()
                            && !is_terminal(state) =>
                    {
                        requeued_seen = true;
                    }
                    Event::Status { state, .. } if saw && my_turn.is_some() && is_terminal(state) => {
                        let (t, cut) = tail(&text, MAX_TEXT_CHARS);
                        let s = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await.ok();
                        return Ok(TurnOutcome::Ended {
                            state,
                            text: t,
                            text_truncated: text_truncated || cut,
                            error: last_error,
                            session: s,
                        });
                    }
                    _ => {}
                }
            }
            if page.has_more {
                continue;
            }
            if let Some((_, kind, title)) = open.first() {
                let s = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await.ok();
                return Ok(TurnOutcome::WaitingPermission { kind: *kind, title: title.clone(), session: s });
            }
            let s: SessionSummary = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await?;
            if s.status == SessionStatus::WaitingPermission {
                // 开始之前就在等的请求（不在我们读到的事件里）
                let list: PermissionListResult =
                    self.rpc(methods::PERMISSION_LIST, json!({ "session_id": sid })).await?;
                if let Some(r) = list.requests.first() {
                    return Ok(TurnOutcome::WaitingPermission {
                        kind: r.kind,
                        title: r.title.clone(),
                        session: Some(s),
                    });
                }
            }
            // 插话的 turn 结束了：消息是不是退回队列了？
            // - 有退回的迹象：会话还在排队 / 启动 / 跑别的 turn，就一直等到我的消息进的那个 turn 开始（只受 `timeout` 限制：
            //   别的会话占着名额时新 turn 可以晚很久）；会话闲着了仍没有新 turn，消息就是被清掉了；
            // - 没有迹象：会话在排队 / 启动、或还有没读到的事件时再等等，其余就用这个 turn 的结果（连续两次确认）
            if pending_done.is_some() {
                let active = matches!(
                    s.status,
                    SessionStatus::Queued
                        | SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingPermission
                );
                let hold = s.last_seq > after
                    || if requeued_seen {
                        active
                    } else {
                        matches!(s.status, SessionStatus::Queued | SessionStatus::Starting)
                    };
                if hold {
                    pending_idle = 0;
                } else {
                    pending_idle += 1;
                    if pending_idle >= 2 {
                        return match pending_done.take() {
                            Some(_) if requeued_seen => Ok(TurnOutcome::Dropped { session: s }),
                            Some(done) => self.finish_done(sid, done).await,
                            None => unreachable!(),
                        };
                    }
                }
            }
            // 消息被丢掉：我的消息已经在事件流里，却没有开过 turn，会话也闲着、没有没读到的事件。
            // 连续两次看到才算（排除 turn 刚结束、下一个 turn 马上开的瞬间；那一瞬间会有新事件，`last_seq` 会超过 `after`）
            if saw && my_turn.is_none() && s.status == SessionStatus::Idle && s.last_seq <= after {
                idle_polls += 1;
                if idle_polls >= 2 {
                    return Ok(TurnOutcome::Dropped { session: s });
                }
            } else {
                idle_polls = 0;
            }
            if Instant::now() >= deadline {
                let (t, cut) = tail(&text, MAX_TEXT_CHARS);
                return Ok(TurnOutcome::StillRunning {
                    text: t,
                    text_truncated: text_truncated || cut,
                    turn_started: my_turn.is_some() && pending_done.is_none(),
                    session: s,
                });
            }
            if progressed {
                poll = Duration::from_millis(150);
            } else {
                poll = (poll + poll / 2).min(Duration::from_millis(1000));
            }
            tokio::time::sleep(poll.min(deadline.saturating_duration_since(Instant::now()))).await;
        }
    }

    /// `turn_done` 之后补上会话状态和累计用量。
    async fn finish_done(&self, sid: &str, done: TurnOutcome) -> std::result::Result<TurnOutcome, String> {
        let s = self.rpc(methods::SESSION_GET, json!({ "session_id": sid })).await.ok();
        Ok(match done {
            TurnOutcome::Done { reason, usage, text, text_truncated, error, .. } => {
                TurnOutcome::Done { reason, usage, text, text_truncated, error, session: s }
            }
            other => other,
        })
    }
}

const WAITING_NOTE: &str = "The agent is waiting for a permission decision. Only a person can give it: on a paired device (the anycon app or web UI), or with `anycon approve` / `anycon deny` on the node. This MCP server has no way to approve or deny.";

struct WorkspaceChoice {
    workspace: Option<Value>,
    defaulted: bool,
}

/// 一轮等待的结果。
enum TurnOutcome {
    Done {
        reason: TurnDoneReason,
        usage: Option<Usage>,
        text: String,
        text_truncated: bool,
        error: Option<(String, String)>,
        session: Option<SessionSummary>,
    },
    /// 会话在这一轮里进入终态（error / ended / interrupted），没有 `turn_done`。
    Ended {
        state: SessionStatus,
        text: String,
        text_truncated: bool,
        error: Option<(String, String)>,
        session: Option<SessionSummary>,
    },
    WaitingPermission {
        kind: PermissionKind,
        title: String,
        session: Option<SessionSummary>,
    },
    StillRunning {
        text: String,
        text_truncated: bool,
        /// 我那一轮已经开始了（否则消息还在排队）。
        turn_started: bool,
        session: SessionSummary,
    },
    /// 消息没有被处理：会话闲着，它没开过 turn，也不在队列里（排队时被 `session.cancel` 清掉了）。
    Dropped {
        session: SessionSummary,
    },
}

impl TurnOutcome {
    fn to_json(&self, sid: &str) -> Value {
        let mut v = json!({ "session_id": sid });
        let usage_of = |session: &Option<SessionSummary>, turn: Option<&Usage>| {
            let mut u = serde_json::Map::new();
            if let Some(t) = turn {
                u.insert("turn".into(), usage_json(t));
            }
            if let Some(s) = session {
                u.insert(
                    "session_total".into(),
                    json!({
                        "turns": s.usage.turns,
                        "input_tokens": s.usage.input_tokens,
                        "output_tokens": s.usage.output_tokens,
                        "cost_usd": s.usage.cost_usd,
                    }),
                );
            }
            Value::Object(u)
        };
        match self {
            TurnOutcome::Done { reason, usage, text, text_truncated, error, session } => {
                v["outcome"] = json!(match reason {
                    TurnDoneReason::Completed | TurnDoneReason::MaxTurns | TurnDoneReason::Budget => "completed",
                    TurnDoneReason::Cancelled => "cancelled",
                    TurnDoneReason::Error => "failed",
                });
                v["turn_end_reason"] = serde_json::to_value(reason).unwrap_or(Value::Null);
                v["text"] = json!(text);
                v["text_truncated"] = json!(text_truncated);
                v["usage"] = usage_of(session, usage.as_ref());
                if let Some((code, message)) = error {
                    v["error"] = json!({ "code": code, "message": clip(message, 500).0 });
                }
                if let Some(s) = session {
                    v["session_status"] = json!(status_str(s.status));
                }
            }
            TurnOutcome::Ended { state, text, text_truncated, error, session } => {
                v["outcome"] = json!("failed");
                v["session_status"] = json!(status_str(*state));
                v["text"] = json!(text);
                v["text_truncated"] = json!(text_truncated);
                v["usage"] = usage_of(session, None);
                if let Some((code, message)) = error {
                    v["error"] = json!({ "code": code, "message": clip(message, 500).0 });
                }
            }
            TurnOutcome::WaitingPermission { kind, title, session } => {
                v["outcome"] = json!("waiting_permission");
                v["permission"] = json!({ "kind": kind_str(*kind), "title": clip(title, TITLE_CHARS).0 });
                v["note"] = json!(WAITING_NOTE);
                if let Some(s) = session {
                    v["session_status"] = json!(status_str(s.status));
                }
            }
            TurnOutcome::StillRunning { text, text_truncated, turn_started, session } => {
                v["outcome"] = json!("still_running");
                v["session_status"] = json!(status_str(session.status));
                v["text_so_far"] = json!(text);
                v["text_truncated"] = json!(text_truncated);
                // 说明只讲真实情况：有 turn 在跑才说「在跑」，排队就说排队
                let running = matches!(
                    session.status,
                    SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingPermission
                );
                v["note"] = json!(if session.status == SessionStatus::Queued {
                    "The session is queued: the node runs at most sessions.max_concurrent turns at once, and this one starts when a slot is free. The message stays queued without this call. Check on it later with read_session."
                } else if running && *turn_started {
                    "The turn is still running on the node and keeps running without this call. Check on it later with read_session."
                } else if running {
                    "The message is queued behind the turn that is running now; the node sends it when that turn ends, with or without this call. Check on it later with read_session."
                } else {
                    "The wait ran out before the turn was reported finished. Check the state with read_session."
                });
            }
            TurnOutcome::Dropped { session } => {
                v["outcome"] = json!("dropped");
                v["session_status"] = json!(status_str(session.status));
                v["note"] = json!(
                    "The message was not processed: it was queued, but the session is idle and no turn was started for it, so the queue was cleared (for example by someone cancelling the session). Send the message again if it is still wanted."
                );
            }
        }
        v
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "output_tokens": u.output_tokens,
        "cost_usd": u.cost_usd,
        "duration_ms": u.duration_ms,
        "model": u.model,
    })
}

fn summary_json(s: &SessionSummary) -> Value {
    let mut v = json!({
        "id": s.id,
        "title": clip(&s.title, TITLE_CHARS).0,
        "project": s.project,
        "agent": s.agent,
        "status": status_str(s.status),
        "archived": s.archived,
        "pending_permissions": s.pending_permissions,
        "model": s.settings.model,
        "thinking": s.settings.thinking,
        "mode": s.settings.mode,
        "updated_at_ms": s.updated_at_ms,
        "last_seq": s.last_seq,
        "usage": {
            "turns": s.usage.turns,
            "input_tokens": s.usage.input_tokens,
            "output_tokens": s.usage.output_tokens,
            "cost_usd": s.usage.cost_usd,
        },
    });
    if let Some(w) = &s.worktree {
        v["branch"] = json!(w.branch);
    }
    if let Some(p) = &s.preview {
        v["preview"] = json!(clip(p, TITLE_CHARS).0);
    }
    if let Some(q) = s.queue_position {
        v["queue_position"] = json!(q);
    }
    v
}

fn diff_file_json(f: &DiffFile) -> Value {
    let mut v = json!({
        "path": f.path,
        "status": serde_json::to_value(f.status).unwrap_or(Value::Null),
        "additions": f.additions,
        "deletions": f.deletions,
    });
    if f.binary {
        v["binary"] = json!(true);
    }
    if f.untracked {
        v["untracked"] = json!(true);
    }
    if let Some(o) = &f.old_path {
        v["old_path"] = json!(o);
    }
    v
}

/// 搜索片段：`<mark>` 换成 `**`，还原被转义的 `& < >`。
fn plain_snippet(s: &str) -> String {
    s.replace("<mark>", "**").replace("</mark>", "**").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

struct Msg {
    seq: u64,
    role: &'static str,
    text: String,
}

/// 把一段（按 seq 升序的）事件折成消息：用户消息、助手文字（连续的 `text_delta`）、错误。工具调用和思考不进来。
fn build_messages(events: &[SessionEvent]) -> Vec<Msg> {
    let mut out: Vec<Msg> = Vec::new();
    let mut cur: Option<Msg> = None;
    let flush = |cur: &mut Option<Msg>, out: &mut Vec<Msg>| {
        if let Some(m) = cur.take() {
            if !m.text.trim().is_empty() {
                out.push(m);
            }
        }
    };
    for ev in events {
        match &ev.event {
            Event::UserMessage { content, attachments, .. } => {
                flush(&mut cur, &mut out);
                let mut text = content.clone();
                if !attachments.is_empty() {
                    text.push_str(&format!("\n[{} attachment(s)]", attachments.len()));
                }
                out.push(Msg { seq: ev.seq, role: "user", text });
            }
            Event::TextDelta { text } => match cur.as_mut() {
                Some(m) => m.text.push_str(text),
                None => cur = Some(Msg { seq: ev.seq, role: "assistant", text: text.clone() }),
            },
            Event::TextDone {} | Event::ToolCall { .. } | Event::TurnDone { .. } | Event::Permission { .. } => {
                flush(&mut cur, &mut out)
            }
            Event::Error { code, message, .. } => {
                flush(&mut cur, &mut out);
                out.push(Msg { seq: ev.seq, role: "error", text: format!("{code}: {message}") });
            }
            _ => {}
        }
    }
    flush(&mut cur, &mut out);
    out
}

impl<N: Node> ToolHost for AnyconTools<N> {
    fn tools(&self) -> Vec<ToolDef> {
        definitions()
    }

    fn instructions(&self) -> Option<String> {
        Some(
            "Tools to look at and drive coding-agent sessions on an anycon node (the user's machine). \
             Use list_agents / list_projects to see what is available, create_session to start work and send_message to continue it. \
             Waiting for a turn is bounded by timeout_seconds; a long turn keeps running on the node, check it with read_session. \
             Permission requests raised by those agents can only be decided by a person on a paired device: there is no tool for it, \
             and a call that hits one returns waiting_permission."
                .into(),
        )
    }

    async fn call(&self, name: &str, arguments: Value) -> Option<ToolOutput> {
        if !TOOL_NAMES.contains(&name) {
            return None;
        }
        Some(match self.dispatch(name, arguments).await {
            Ok(o) => o,
            Err(msg) => ToolOutput::error(msg),
        })
    }
}

/// 给测试：按方法名脚本化应答的假节点。
#[cfg(test)]
pub mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    type Handler = Box<dyn FnMut(&str, &Value) -> Result<Value> + Send>;

    #[derive(Clone)]
    pub struct FakeNode {
        pub calls: Arc<Mutex<Vec<(String, Value)>>>,
        handler: Arc<Mutex<Handler>>,
    }

    impl FakeNode {
        pub fn new(h: impl FnMut(&str, &Value) -> Result<Value> + Send + 'static) -> FakeNode {
            FakeNode { calls: Arc::new(Mutex::new(Vec::new())), handler: Arc::new(Mutex::new(Box::new(h))) }
        }

        pub fn methods(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
        }
    }

    impl Node for FakeNode {
        async fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.lock().unwrap().push((method.to_string(), params.clone()));
            (self.handler.lock().unwrap())(method, &params)
        }
    }

    /// 一个叫 `agent` 的 agent：`ask`（normal，默认）、`yolo`（dangerous）。
    pub fn agent_list(default_mode: &str) -> Value {
        let mode = |id: &str, risk: &str| json!({ "id": id, "label": id, "default": id == default_mode, "risk": risk });
        json!({
            "agents": [{
                "id": "agent", "label": "Agent", "driver": "mock", "enabled": true, "status": "ready",
                "models": [{ "id": "m1", "label": "M1", "default": true }],
                "modes": [mode("ask", "normal"), mode("yolo", "dangerous")],
                "options": [], "capabilities": {
                    "resume": true, "interrupt": true, "interject": false, "attachments": [], "thinking_stream": false,
                    "usage": true, "structured_output": false, "env": false, "rewind": false
                },
                "probed_at_ms": 1
            }],
            "etag": "e"
        })
    }

    pub fn session(id: &str, status: &str, last_seq: u64) -> Value {
        json!({
            "id": id, "agent": "agent", "title": "t", "status": status, "archived": false, "settings": {},
            "created_at_ms": 1, "updated_at_ms": 2, "last_seq": last_seq, "pending_permissions": 0
        })
    }

    pub fn event(session_id: &str, seq: u64, event: Value) -> Value {
        json!({ "session_id": session_id, "seq": seq, "ts_ms": 1, "event": event })
    }

    /// 带 `turn_id` 的事件（turn 里的内容事件）。
    pub fn event_in(session_id: &str, seq: u64, turn_id: u64, event: Value) -> Value {
        json!({ "session_id": session_id, "seq": seq, "ts_ms": 1, "turn_id": turn_id, "event": event })
    }

    /// 当前权限模式为 `mode` 的会话。
    pub fn session_in_mode(id: &str, status: &str, last_seq: u64, mode: &str) -> Value {
        let mut v = session(id, status, last_seq);
        v["settings"] = json!({ "mode": mode });
        v
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::fake::*;
    use super::*;

    const SID: &str = "sess_0123456789abcdefghijkl";

    fn tools(node: FakeNode, opts: Options) -> AnyconTools<FakeNode> {
        AnyconTools::new(node, opts)
    }

    fn text_of(o: &ToolOutput) -> Value {
        serde_json::from_str(&o.blocks[0]).unwrap_or_else(|_| Value::String(o.blocks[0].clone()))
    }

    #[test]
    fn the_tool_set_is_exactly_the_documented_one_and_has_no_way_to_approve() {
        let defs = definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            let n = d.name.to_lowercase();
            assert!(
                !n.contains("approve") && !n.contains("deny") && !n.contains("permission") && !n.contains("respond"),
                "{n}"
            );
        }
        // 会改状态的工具和 MUTATING 一致，其余都是只读
        for d in &defs {
            assert_eq!(!d.read_only, MUTATING.contains(&d.name), "{}", d.name);
        }
    }

    #[test]
    fn schemas_are_valid_json_schema_and_never_offer_a_session_env() {
        for d in definitions() {
            let s = &d.input_schema;
            assert_eq!(s["type"], "object", "{}", d.name);
            assert_eq!(s["additionalProperties"], false, "{}: unknown arguments are rejected", d.name);
            assert!(jsonschema::meta::is_valid(s), "{}: not a valid JSON Schema: {s}", d.name);
            let props = s["properties"].as_object().unwrap();
            for req in s["required"].as_array().unwrap() {
                assert!(props.contains_key(req.as_str().unwrap()), "{}: required {req} is not a property", d.name);
            }
            for forbidden in ["env", "settings", "options", "system_prompt_append", "max_turns", "max_budget_usd"] {
                assert!(!props.contains_key(forbidden), "{} must not offer {forbidden}", d.name);
            }
            assert!(!d.description.is_empty() && d.description.is_ascii(), "{}: English description", d.name);
        }
    }

    #[tokio::test]
    async fn dangerous_modes_are_refused_unless_the_flag_is_given() {
        let node = FakeNode::new(|m, p| match m {
            "agent.list" => Ok(agent_list("ask")),
            "node.info" => Err(CliError::general("no")),
            "session.create" => {
                assert_eq!(p["settings"]["mode"], "yolo", "{p}");
                Ok(session(SID, "running", 1))
            }
            other => panic!("unexpected {other}"),
        });
        let t = tools(node.clone(), Options::default());
        let args = json!({ "prompt": "hi", "mode": "yolo", "wait": false });
        let r = t.call("create_session", args.clone()).await.unwrap();
        assert!(r.is_error, "{r:?}");
        assert!(r.blocks[0].contains("dangerous") && r.blocks[0].contains("--allow-dangerous-modes"), "{r:?}");
        assert!(r.blocks[0].contains("ask"), "names the modes that are allowed: {r:?}");
        assert!(!node.methods().contains(&"session.create".to_string()), "nothing was created");

        let t = tools(node.clone(), Options { allow_dangerous_modes: true, ..Default::default() });
        let r = t.call("create_session", args).await.unwrap();
        assert!(!r.is_error, "{r:?}");
        assert!(node.methods().contains(&"session.create".to_string()));
    }

    #[tokio::test]
    async fn a_dangerous_default_mode_counts_when_no_mode_is_given() {
        let node = FakeNode::new(|m, _| match m {
            "agent.list" => Ok(agent_list("yolo")),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node.clone(), Options::default());
        let r = t.call("create_session", json!({ "prompt": "hi", "wait": false })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("default permission mode"), "{r:?}");
        // 明确选了安全的模式就可以
        let node = FakeNode::new(|m, p| match m {
            "agent.list" => Ok(agent_list("yolo")),
            "node.info" => Err(CliError::general("no")),
            "session.create" => {
                assert_eq!(p["settings"]["mode"], "ask");
                Ok(session(SID, "running", 1))
            }
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let r = t.call("create_session", json!({ "prompt": "hi", "mode": "ask", "wait": false })).await.unwrap();
        assert!(!r.is_error, "{r:?}");
    }

    #[tokio::test]
    async fn unknown_or_forbidden_arguments_are_tool_errors_not_ignored() {
        let node = FakeNode::new(|m, _| panic!("no rpc expected, got {m}"));
        let t = tools(node, Options::default());
        // 会话级 env 不在 schema 里，传了也不会被转给 daemon
        let r = t.call("create_session", json!({ "prompt": "hi", "env": { "A": "1" } })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("env"), "{r:?}");
        let r = t.call("create_session", json!({ "prompt": "hi", "settings": { "mode": "yolo" } })).await.unwrap();
        assert!(r.is_error, "{r:?}");
        let r = t.call("send_message", json!({ "message": "hi" })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("session"), "{r:?}");
        let r = t.call("list_sessions", json!({ "status": "bogus" })).await.unwrap();
        assert!(r.is_error, "{r:?}");
        assert!(t.call("approve_permission", json!({})).await.is_none(), "there is no such tool");
    }

    #[tokio::test]
    async fn nested_use_blocks_the_mutating_tools_only() {
        let node = FakeNode::new(|m, _| match m {
            "session.list" => Ok(json!({ "sessions": [] })),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node.clone(), Options { block_mutations: true, ..Default::default() });
        for (name, args) in [
            ("create_session", json!({ "prompt": "hi" })),
            ("send_message", json!({ "session": "last", "message": "hi" })),
            ("cancel_turn", json!({ "session": "last" })),
        ] {
            let r = t.call(name, args).await.unwrap();
            assert!(r.is_error && r.blocks[0].contains("--allow-nested"), "{name}: {r:?}");
        }
        assert!(node.methods().is_empty(), "the node was not contacted: {:?}", node.methods());
        let r = t.call("list_sessions", json!({})).await.unwrap();
        assert!(!r.is_error, "{r:?}");
    }

    #[tokio::test]
    async fn a_turn_that_outlasts_the_timeout_is_reported_as_still_running() {
        let t = tools(scripted(vec![], "running"), Options::default());
        let started = Instant::now();
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 1 })).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        let v = text_of(&r);
        assert_eq!(
            (v["outcome"].as_str(), v["session_status"].as_str()),
            (Some("still_running"), Some("running")),
            "{v}"
        );
        assert!(v["note"].as_str().unwrap().contains("read_session"));
    }

    /// 脚本化一个会话：`session.send` 记下 message_id，之后每次 `session.history` 依次吐出 `pages` 里的一页事件
    /// （用 `{mid}` 占位我的 message_id），吐完了就是空页；`session.get` 一直是 `status` / `last_seq` / `mode`。
    fn scripted_pages(
        pages: Vec<Vec<Value>>,
        send_result: Value,
        status: &'static str,
        last_seq: u64,
        mode: &'static str,
    ) -> FakeNode {
        let mid = Arc::new(Mutex::new(String::new()));
        let served = Arc::new(Mutex::new(0usize));
        FakeNode::new(move |m, p| match m {
            "agent.list" => Ok(agent_list("ask")),
            "session.get" => Ok(session_in_mode(SID, status, last_seq, mode)),
            "session.send" => {
                *mid.lock().unwrap() = p["message_id"].as_str().unwrap().to_string();
                Ok(send_result.clone())
            }
            "session.history" => {
                let mut served = served.lock().unwrap();
                let i = *served;
                *served += 1;
                let Some(page) = pages.get(i) else { return Ok(json!({ "events": [], "has_more": false })) };
                let mid = mid.lock().unwrap().clone();
                let events: Vec<Value> =
                    page.iter().map(|e| serde_json::from_str(&e.to_string().replace("{mid}", &mid)).unwrap()).collect();
                Ok(json!({ "events": events, "has_more": false }))
            }
            "permission.list" => Ok(json!({ "requests": [] })),
            other => panic!("unexpected {other}"),
        })
    }

    /// 一页事件、新 turn 立刻开始（`turn_id: 2`）的常见情形。
    fn scripted(script: Vec<Value>, final_status: &'static str) -> FakeNode {
        scripted_pages(vec![script], json!({ "turn_id": 2, "queued": false }), final_status, 9, "ask")
    }

    #[tokio::test]
    async fn send_message_waits_for_turn_done_and_reports_text_and_usage() {
        let script = vec![
            event(
                SID,
                5,
                json!({ "type": "user_message", "message_id": "{mid}", "content": "hi", "by_device": "local" }),
            ),
            event(SID, 6, json!({ "type": "turn_start", "turn_id": 2, "message_id": "{mid}" })),
            event(SID, 7, json!({ "type": "text_delta", "text": "Hello " })),
            event(SID, 8, json!({ "type": "text_delta", "text": "there" })),
            event(
                SID,
                9,
                json!({ "type": "turn_done", "turn_id": 2, "reason": "completed",
                        "usage": { "input_tokens": 10, "output_tokens": 3, "duration_ms": 50, "model": "m1" } }),
            ),
        ];
        let t = tools(scripted(script, "idle"), Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap();
        assert!(!r.is_error, "{r:?}");
        let v = text_of(&r);
        assert_eq!(v["outcome"], "completed", "{v}");
        assert_eq!(v["text"], "Hello there");
        assert_eq!(v["usage"]["turn"]["output_tokens"], 3);
        assert_eq!(v["session_status"], "idle");
    }

    #[tokio::test]
    async fn a_permission_request_ends_the_wait_with_waiting_permission() {
        let script = vec![
            event(
                SID,
                5,
                json!({ "type": "user_message", "message_id": "{mid}", "content": "hi", "by_device": "local" }),
            ),
            event(
                SID,
                6,
                json!({ "type": "permission", "request_id": "r1", "kind": "tool", "title": "Run command",
                        "detail": {}, "options": [] }),
            ),
        ];
        let node = scripted(script, "waiting_permission");
        let t = tools(node.clone(), Options::default());
        let started = Instant::now();
        let r =
            t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 30 })).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "returns immediately, does not wait for the timeout");
        let v = text_of(&r);
        assert_eq!(v["outcome"], "waiting_permission", "{v}");
        assert_eq!(v["permission"], json!({ "kind": "tool", "title": "Run command" }));
        assert!(v["note"].as_str().unwrap().contains("person"), "{v}");
        assert!(!node.methods().iter().any(|m| m.starts_with("permission.respond")), "never answers");
    }

    #[tokio::test]
    async fn a_resolved_permission_does_not_end_the_wait() {
        let script = vec![
            event(
                SID,
                5,
                json!({ "type": "user_message", "message_id": "{mid}", "content": "hi", "by_device": "local" }),
            ),
            event(
                SID,
                6,
                json!({ "type": "permission", "request_id": "r1", "kind": "tool", "title": "Run command",
                        "detail": {}, "options": [] }),
            ),
            event(
                SID,
                7,
                json!({ "type": "permission_resolved", "request_id": "r1", "option_id": "allow", "by_device": "phone" }),
            ),
            event(SID, 8, json!({ "type": "text_delta", "text": "done" })),
            event(SID, 9, json!({ "type": "turn_done", "turn_id": 2, "reason": "completed" })),
        ];
        let t = tools(scripted(script, "idle"), Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap();
        let v = text_of(&r);
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("done")), "{v}");
    }

    #[tokio::test]
    async fn a_failed_turn_and_wait_false_are_reported() {
        let script = vec![
            event(
                SID,
                5,
                json!({ "type": "user_message", "message_id": "{mid}", "content": "hi", "by_device": "local" }),
            ),
            event(
                SID,
                6,
                json!({ "type": "error", "code": "agent_exited", "message": "it died", "recoverable": false }),
            ),
            event(SID, 7, json!({ "type": "status", "state": "error" })),
        ];
        let t = tools(scripted(script, "error"), Options::default());
        let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
        assert_eq!(v["outcome"], "failed", "{v}");
        assert_eq!(v["error"]["code"], "agent_exited");

        let node = scripted(vec![], "running");
        let t = tools(node.clone(), Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        let v = text_of(&r);
        assert_eq!((v["sent"].clone(), v["queued"].clone()), (json!(true), json!(false)), "{v}");
        assert!(!node.methods().contains(&"session.history".to_string()), "no polling without wait");
    }

    #[test]
    fn messages_are_folded_from_events() {
        let ev = |seq, e: Value| serde_json::from_value::<SessionEvent>(event(SID, seq, e)).unwrap();
        let events = vec![
            ev(1, json!({ "type": "user_message", "message_id": "a", "content": "q1", "by_device": "x" })),
            ev(2, json!({ "type": "thinking_delta", "text": "hm" })),
            ev(3, json!({ "type": "text_delta", "text": "a" })),
            ev(4, json!({ "type": "text_delta", "text": "b" })),
            ev(5, json!({ "type": "text_done" })),
            ev(6, json!({ "type": "tool_call", "id": "t", "name": "Bash", "category": "shell", "title": "ls" })),
            ev(7, json!({ "type": "text_delta", "text": "c" })),
            ev(8, json!({ "type": "turn_done", "turn_id": 1, "reason": "completed" })),
        ];
        let msgs = build_messages(&events);
        let got: Vec<(u64, &str, &str)> = msgs.iter().map(|m| (m.seq, m.role, m.text.as_str())).collect();
        assert_eq!(got, vec![(1, "user", "q1"), (3, "assistant", "ab"), (7, "assistant", "c")]);
    }

    #[tokio::test]
    async fn read_session_pages_backwards_and_caps_the_text() {
        let long = "x".repeat(READ_MESSAGE_CHARS + 500);
        // 6 条消息：用户 / 助手交替，seq 1..=12（每条助手消息两个 delta）
        let mut all = Vec::new();
        for i in 0..3u64 {
            let base = i * 4;
            all.push(event(SID, base + 1, json!({ "type": "user_message", "message_id": format!("m{i}"), "content": format!("q{i}"), "by_device": "x" })));
            all.push(event(SID, base + 2, json!({ "type": "text_delta", "text": long.clone() })));
            all.push(event(SID, base + 3, json!({ "type": "text_delta", "text": "tail" })));
            all.push(event(SID, base + 4, json!({ "type": "turn_done", "turn_id": i + 1, "reason": "completed" })));
        }
        let node = FakeNode::new(move |m, p| match m {
            "session.get" => Ok(session(SID, "idle", 12)),
            "session.history" => {
                let before = p["before_seq"].as_u64().unwrap();
                let evs: Vec<Value> = all.iter().filter(|e| e["seq"].as_u64().unwrap() < before).cloned().collect();
                Ok(json!({ "events": evs, "has_more": false }))
            }
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let v = text_of(&t.call("read_session", json!({ "session": SID, "limit": 2 })).await.unwrap());
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        // 最新的两条，按时间顺序：第三轮的用户消息（seq 9）和助手回复（seq 10）
        let got: Vec<(&str, u64)> =
            msgs.iter().map(|m| (m["role"].as_str().unwrap(), m["seq"].as_u64().unwrap())).collect();
        assert_eq!(got, vec![("user", 9), ("assistant", 10)]);
        assert!(msgs[1]["text"].as_str().unwrap().contains("more characters"), "the long reply is clipped");
        assert!(msgs.iter().all(|m| m["text"].as_str().unwrap().chars().count() <= READ_MESSAGE_CHARS + 60));
        assert_eq!(v["has_more_before"], true);
        let next = v["next_before_seq"].as_u64().unwrap();
        // 往前翻
        let v2 =
            text_of(&t.call("read_session", json!({ "session": SID, "limit": 2, "before_seq": next })).await.unwrap());
        let seqs: Vec<u64> = v2["messages"].as_array().unwrap().iter().map(|m| m["seq"].as_u64().unwrap()).collect();
        assert!(seqs.iter().all(|s| *s < next), "{seqs:?} before {next}");
        assert!(!seqs.is_empty());
    }

    #[tokio::test]
    async fn get_diff_caps_the_patch_and_picks_the_right_base() {
        let patch = "+".repeat(DIFF_PATCH_CHARS + 10);
        let node = FakeNode::new(move |m, p| match m {
            "session.get" => Ok(session(SID, "idle", 3)),
            "session.diff" => {
                assert_eq!(
                    p["against"], "uncommitted",
                    "a session without a worktree only has uncommitted changes: {p}"
                );
                if p.get("path").is_some() {
                    Ok(
                        json!({ "from_sha": "abc", "files": [{ "path": "a.txt", "status": "modified", "additions": 1, "deletions": 0, "patch": patch.clone() }] }),
                    )
                } else {
                    Ok(
                        json!({ "from_sha": "abc", "files": [{ "path": "a.txt", "status": "modified", "additions": 1, "deletions": 0 }] }),
                    )
                }
            }
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let v = text_of(&t.call("get_diff", json!({ "session": SID })).await.unwrap());
        assert_eq!(v["files"][0]["path"], "a.txt");
        assert!(v["files"][0].get("patch").is_none());
        let r = t.call("get_diff", json!({ "session": SID, "path": "a.txt" })).await.unwrap();
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(r.blocks[1].chars().count(), DIFF_PATCH_CHARS);
        assert_eq!(text_of(&r)["file"]["patch_truncated"], true);
    }

    fn user_msg(seq: u64) -> Value {
        event(SID, seq, json!({ "type": "user_message", "message_id": "{mid}", "content": "hi", "by_device": "local" }))
    }

    fn done_event(seq: u64, turn: u64, out_tokens: u64) -> Value {
        event_in(
            SID,
            seq,
            turn,
            json!({ "type": "turn_done", "turn_id": turn, "reason": "completed",
                    "usage": { "output_tokens": out_tokens, "duration_ms": 5 } }),
        )
    }

    fn text_in(seq: u64, turn: u64, text: &str) -> Value {
        event_in(SID, seq, turn, json!({ "type": "text_delta", "text": text }))
    }

    /// 会话正忙：我的消息在上一轮进行中被写进事件流（排队），上一轮的 `turn_done` 先到，
    /// 我的那一轮（`turn_start.message_id` 是我的）随后才开始。返回的必须是第二轮的文字和用量，不是第一轮的。
    #[tokio::test]
    async fn a_message_sent_to_a_busy_session_gets_the_answer_of_its_own_turn() {
        let first = vec![
            event_in(SID, 3, 1, json!({ "type": "turn_start", "turn_id": 1, "message_id": "other" })),
            text_in(4, 1, "first answer"),
            user_msg(5),
            done_event(6, 1, 111),
        ];
        let second = vec![
            event_in(SID, 7, 2, json!({ "type": "turn_start", "turn_id": 2, "message_id": "{mid}" })),
            text_in(8, 2, "second answer"),
            done_event(9, 2, 222),
        ];
        // 两页（轮询到两次）和一页（一次读到）都一样
        for pages in [vec![first.clone(), second.clone()], vec![[first.clone(), second.clone()].concat()]] {
            let t = tools(scripted_pages(pages, json!({ "queued": true }), "idle", 9, "ask"), Options::default());
            let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
            assert_eq!(v["outcome"], "completed", "{v}");
            assert_eq!(v["text"], "second answer", "must not return the previous turn's result: {v}");
            assert_eq!(v["usage"]["turn"]["output_tokens"], 222, "{v}");
        }
    }

    /// 同样的情形发生在 `create_session` 的等待里没有意义（新会话没有上一轮），但 turn 的认法一样：按我的 `user_message` 之后的 `turn_start`。
    #[tokio::test]
    async fn create_session_waits_for_the_turn_that_starts_after_its_message() {
        let node = FakeNode::new({
            let mid = Arc::new(Mutex::new(String::new()));
            move |m, p| match m {
                "agent.list" => Ok(agent_list("ask")),
                "session.create" => {
                    *mid.lock().unwrap() = p["message_id"].as_str().unwrap().to_string();
                    Ok(session(SID, "running", 3))
                }
                "session.get" => Ok(session(SID, "idle", 6)),
                "session.history" => {
                    let mid = mid.lock().unwrap().clone();
                    let evs = vec![
                        event(
                            SID,
                            1,
                            json!({ "type": "user_message", "message_id": mid, "content": "hi", "by_device": "x" }),
                        ),
                        event_in(SID, 2, 1, json!({ "type": "turn_start", "turn_id": 1, "message_id": mid })),
                        text_in(3, 1, "created"),
                        done_event(4, 1, 7),
                    ];
                    Ok(json!({ "events": evs, "has_more": false }))
                }
                other => panic!("unexpected {other}"),
            }
        });
        let t = tools(node, Options::default());
        let v = text_of(&t.call("create_session", json!({ "prompt": "hi" })).await.unwrap());
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("created")), "{v}");
    }

    /// 插话进了正在跑的 turn：等的就是这个 turn；文字是这个 turn 的全部（从发出消息前的 `start_seq` 之后算起）。
    #[tokio::test]
    async fn an_interjected_message_waits_for_the_running_turn() {
        let script = vec![text_in(4, 1, "partial "), user_msg(5), text_in(6, 1, "rest"), done_event(7, 1, 9)];
        let t = tools(
            scripted_pages(vec![script], json!({ "turn_id": 1, "queued": false }), "idle", 7, "ask"),
            Options::default(),
        );
        let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("partial rest")), "{v}");
    }

    /// 插话失败：消息退回队列，作为下一个 turn 的第一条发出。那个 turn 才是我的。
    #[tokio::test]
    async fn a_failed_interjection_is_followed_into_the_turn_that_really_ran_it() {
        let script = vec![
            text_in(4, 1, "old turn"),
            user_msg(5),
            done_event(6, 1, 1),
            event_in(SID, 7, 2, json!({ "type": "turn_start", "turn_id": 2, "message_id": "{mid}" })),
            text_in(8, 2, "answer to mine"),
            done_event(9, 2, 2),
        ];
        let t = tools(
            scripted_pages(vec![script], json!({ "turn_id": 1, "queued": false }), "idle", 9, "ask"),
            Options::default(),
        );
        let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("answer to mine")), "{v}");
    }

    /// 排队的消息被丢掉（有人 `session.cancel` 清了队列）：会话闲着、我的消息没开过 turn，要尽快说明，而不是等满超时再谎报「在跑」。
    #[tokio::test]
    async fn a_dropped_message_is_reported_instead_of_waiting_for_the_timeout() {
        let node = scripted_pages(vec![vec![user_msg(5)]], json!({ "queued": true }), "idle", 5, "ask");
        let t = tools(node, Options::default());
        let started = Instant::now();
        let r =
            t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 30 })).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "does not wait for the timeout");
        let v = text_of(&r);
        assert_eq!(v["outcome"], "dropped", "{v}");
        assert!(!v["note"].as_str().unwrap().contains("keeps running"), "{v}");

        // 会话里还有没读到的事件（比如下一个 turn 马上要开）：不算丢
        let node = scripted_pages(vec![vec![user_msg(5)]], json!({ "queued": true }), "idle", 6, "ask");
        let t = tools(node, Options::default());
        let v = text_of(
            &t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 1 })).await.unwrap(),
        );
        assert_eq!(v["outcome"], "still_running", "{v}");
    }

    /// 超时的说明只讲真实情况：因 `max_concurrent` 排队就说排队；排在别人的 turn 后面就说排队；真有 turn 在跑才说「在跑」。
    #[tokio::test]
    async fn the_timeout_note_matches_what_is_really_happening() {
        let cases = [
            ("queued", "max_concurrent"),
            ("running", "queued behind"), // 我的消息排在另一个 turn 后面，我的 turn 还没开始
        ];
        for (status, want) in cases {
            let node = scripted_pages(vec![vec![user_msg(5)]], json!({ "queued": true }), status, 5, "ask");
            let t = tools(node, Options::default());
            let v = text_of(
                &t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 1 }))
                    .await
                    .unwrap(),
            );
            assert_eq!(v["outcome"], "still_running", "{v}");
            let note = v["note"].as_str().unwrap();
            assert!(note.contains(want) && !note.contains("keeps running"), "{status}: {note}");
        }
        // 我的 turn 已经开始并且在跑
        let node = scripted(vec![user_msg(5)], "running");
        let t = tools(node, Options::default());
        let v = text_of(
            &t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 1 })).await.unwrap(),
        );
        assert!(v["note"].as_str().unwrap().contains("keeps running"), "{v}");
    }

    /// 文字在工具调用前后分成几段：段与段之间留一个空行，不连成一片。
    #[tokio::test]
    async fn text_around_tool_calls_is_separated() {
        let script = vec![
            user_msg(5),
            event_in(SID, 6, 2, json!({ "type": "turn_start", "turn_id": 2, "message_id": "{mid}" })),
            text_in(7, 2, "Let me look."),
            event_in(
                SID,
                8,
                2,
                json!({ "type": "tool_call", "id": "t1", "name": "Bash", "category": "shell", "title": "ls" }),
            ),
            text_in(9, 2, "Found it."),
            text_in(10, 2, " Done."),
            done_event(11, 2, 1),
        ];
        let t = tools(scripted(script, "idle"), Options::default());
        let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
        assert_eq!(v["text"], "Let me look.\n\nFound it. Done.", "{v}");
    }

    /// 向处在危险模式的会话发消息：不带 `--allow-dangerous-modes` 拒绝（`cancel_turn` 和只读工具不受限）。
    #[tokio::test]
    async fn messages_to_a_session_in_a_dangerous_mode_are_refused_unless_the_flag_is_given() {
        let node = || {
            FakeNode::new(|m, p| match m {
                "agent.list" => Ok(agent_list("ask")),
                "session.get" => Ok(session_in_mode(SID, "idle", 4, "yolo")),
                "session.send" => {
                    assert!(p["content"].is_string());
                    Ok(json!({ "queued": true }))
                }
                "session.cancel" => Ok(json!({})),
                "session.history" => Ok(json!({ "events": [], "has_more": false })),
                other => panic!("unexpected {other}"),
            })
        };
        let n = node();
        let t = tools(n.clone(), Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        assert!(r.is_error, "{r:?}");
        assert!(r.blocks[0].contains("dangerous") && r.blocks[0].contains("--allow-dangerous-modes"), "{r:?}");
        assert!(!n.methods().contains(&"session.send".to_string()), "nothing was sent");
        // cancel_turn 不受限
        let r = t.call("cancel_turn", json!({ "session": SID })).await.unwrap();
        assert!(!r.is_error, "{r:?}");
        // 带标志放行
        let n = node();
        let t = tools(n.clone(), Options { allow_dangerous_modes: true, ..Default::default() });
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        assert!(!r.is_error, "{r:?}");
        assert!(n.methods().contains(&"session.send".to_string()));
    }

    /// 没有记录模式的会话按 agent 的默认模式判断；模式不在清单里（风险未知）一律不放行，创建和发消息都一样。
    #[tokio::test]
    async fn a_mode_that_the_node_does_not_list_is_refused() {
        // 创建：请求的模式不在清单里
        let node = FakeNode::new(|m, _| match m {
            "agent.list" => Ok(agent_list("ask")),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node.clone(), Options::default());
        let r = t.call("create_session", json!({ "prompt": "hi", "mode": "ghost", "wait": false })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("ghost") && r.blocks[0].contains("not one of the modes"), "{r:?}");
        assert!(!node.methods().contains(&"session.create".to_string()));

        // 发消息：会话的模式不在清单里
        let node = FakeNode::new(|m, _| match m {
            "agent.list" => Ok(agent_list("ask")),
            "session.get" => Ok(session_in_mode(SID, "idle", 4, "ghost")),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("ghost"), "{r:?}");

        // 会话没记录模式：按 agent 的默认模式（ask，普通）判断，放行
        let node = FakeNode::new(|m, _| match m {
            "agent.list" => Ok(agent_list("ask")),
            "session.get" => Ok(session(SID, "idle", 4)),
            "session.send" => Ok(json!({ "queued": true })),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        assert!(!r.is_error, "{r:?}");

        // 会话没记录模式、默认模式是危险的：拒绝
        let node = FakeNode::new(|m, _| match m {
            "agent.list" => Ok(agent_list("yolo")),
            "session.get" => Ok(session(SID, "idle", 4)),
            other => panic!("unexpected {other}"),
        });
        let t = tools(node, Options::default());
        let r = t.call("send_message", json!({ "session": SID, "message": "hi", "wait": false })).await.unwrap();
        assert!(r.is_error && r.blocks[0].contains("dangerous"), "{r:?}");
    }

    fn requeued_status(seq: u64) -> Value {
        event(SID, seq, json!({ "type": "status", "state": "running", "queued_messages": 1 }))
    }

    /// 插话失败（daemon 发了 `status { queued_messages }`，消息退回队列），新 turn 因为别的会话占着名额而很久以后才开始：
    /// 一直等下去，不拿上一个 turn 的结果交差。
    #[tokio::test]
    async fn a_failed_interjection_waits_as_long_as_the_new_turn_takes_to_start() {
        let first = vec![text_in(4, 1, "old turn"), user_msg(5), requeued_status(6), done_event(7, 1, 1)];
        let late = vec![
            event_in(SID, 8, 2, json!({ "type": "turn_start", "turn_id": 2, "message_id": "{mid}" })),
            text_in(9, 2, "answer to mine"),
            done_event(10, 2, 2),
        ];
        // 中间好几次轮询什么都没有，会话一直是 queued
        let mut pages = vec![first];
        pages.extend(std::iter::repeat_n(vec![], 8));
        pages.push(late);
        let node = scripted_pages(pages, json!({ "turn_id": 1, "queued": false }), "queued", 7, "ask");
        let t = tools(node, Options::default());
        let started = Instant::now();
        let v = text_of(
            &t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 60 })).await.unwrap(),
        );
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("answer to mine")), "{v}");
        assert!(started.elapsed() > Duration::from_secs(2), "it really waited");
    }

    /// 插话失败、前面还有更早的积压消息：下一个 turn 的 `turn_start.message_id` 是积压里的第一条，不是我的；
    /// 积压合并成一个 turn，我的消息就在里面。
    #[tokio::test]
    async fn a_failed_interjection_behind_older_backlog_follows_the_merged_turn() {
        let script = vec![
            text_in(4, 1, "old turn"),
            user_msg(5),
            requeued_status(6),
            done_event(7, 1, 1),
            event_in(
                SID,
                8,
                2,
                json!({ "type": "turn_start", "turn_id": 2, "message_id": "someone-elses-older-message" }),
            ),
            text_in(9, 2, "merged answer"),
            done_event(10, 2, 2),
        ];
        let node = scripted_pages(vec![script], json!({ "turn_id": 1, "queued": false }), "idle", 10, "ask");
        let t = tools(node, Options::default());
        let v = text_of(&t.call("send_message", json!({ "session": SID, "message": "hi" })).await.unwrap());
        assert_eq!((v["outcome"].as_str(), v["text"].as_str()), (Some("completed"), Some("merged answer")), "{v}");
    }

    /// 插话失败、消息退回队列后队列被清掉：没有新 turn，会话闲着。要报 `dropped`，不是拿旧 turn 的结果。
    #[tokio::test]
    async fn a_failed_interjection_whose_queue_was_cleared_is_reported_as_dropped() {
        let script = vec![text_in(4, 1, "old turn"), user_msg(5), requeued_status(6), done_event(7, 1, 1)];
        let node = scripted_pages(vec![script], json!({ "turn_id": 1, "queued": false }), "idle", 7, "ask");
        let t = tools(node, Options::default());
        let v = text_of(
            &t.call("send_message", json!({ "session": SID, "message": "hi", "timeout_seconds": 30 })).await.unwrap(),
        );
        assert_eq!(v["outcome"], "dropped", "{v}");
    }

    #[test]
    fn helpers() {
        assert_eq!(plain_snippet("a <mark>b</mark> &lt;c&gt; &amp;"), "a **b** <c> &");
        assert_eq!(tail("abcdef", 3), ("def".to_string(), true));
        assert_eq!(clip("héllo", 2), ("hé".to_string(), true));
        assert_eq!(wait_duration(None), Duration::from_secs(DEFAULT_WAIT_SECS));
        assert_eq!(wait_duration(Some(100_000)), Duration::from_secs(MAX_WAIT_SECS));
        assert_eq!(wait_duration(Some(0)), Duration::from_secs(1));
    }
}
