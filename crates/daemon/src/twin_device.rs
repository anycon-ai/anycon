//! 被使用的节点（B）这一侧：Twin 设备（`DeviceKind::Twin`，另一个节点的 Twin）能做什么（docs/spec/daemon.md § 27.3）。
//!
//! 一条边界，三个集中的地方：
//!
//! - **方法白名单**（[`ALLOWED`] + [`authorize`]）：RPC 分发处在进入任何处理之前，按调用方的设备种类集中检查一次。名单之外一律
//!   `forbidden`（`data.reason = "twin_device"`），新增的方法缺省不在名单里；名单里「只针对它自己的项 / 会话」的方法，
//!   归属不对同样 `forbidden`——`forbidden` 先于 `not_found`，所以 Twin 设备探测不出别人的项和会话存不存在。
//! - **推送过滤**（[`pushable`]）：每条通知最终都经 [`ClientHandle::notify`](crate::clients::ClientHandle::notify) 送到一个客户端，
//!   过滤就在那里做；对 Twin 设备只放行 [`PUSHED`] 里的四种、并且只限它自己的项和会话，新增的通知缺省不推。
//! - **范围、名额和每日次数**：按请求方（这台 Twin 设备的设备 id）存在 `twin_scopes`，实现和本机 Twin（§ 26）共用
//!   [`crate::twin`] 里的函数；这里只放 Twin 设备特有的部分（归属、`twin.peer_status`、记一项和派发的强制）。
//!
//! 项和会话的归属存在库里（项的 `source`、表 `twin_sessions`）；推送要同步判断，所以另有一份内存里的 [`TwinOwners`]，
//! 启动时从库里读、写库成功之后更新，只用于推送，方法的检查读库。
//!
//! 项的标题、正文和会话内容是用户内容：本模块的日志和审计只带 id、数字和枚举。

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anycon_protocol::{
    methods, AgentStatus, DeviceKind, ErrorCode, RpcError, RpcNotification, SessionSettings, SessionStatus,
    SessionWorkspace, Todo, TodoDispatchParams, TodoSourceKind, TwinPeerAgent, TwinPeerStatus, TwinScope,
};
use serde_json::Value;

use crate::clients::Caller;
use crate::daemon::Ctx;
use crate::judge::service::local_day_start;
use crate::store::Store;

type Result<T> = std::result::Result<T, RpcError>;

/// 一个方法对 Twin 设备开放到什么程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 不看参数（节点和 agent 的信息、它自己的 `twin.peer_status`）。
    Open,
    /// `todo.add`：新建；`id` 已经存在而不是它自己建的，`forbidden`（`todo.add` 按 `id` 幂等，不能借它读到别人的项）。
    NewTodo,
    /// `todo.list`：处理函数只返回它自己建的项。
    OwnTodoList,
    /// 参数里的 `id` 必须是它自己建的项。
    OwnTodo,
    /// 参数里的 `session_id` 必须是它自己派出的会话。
    OwnSession,
}

/// 方法白名单：Twin 设备只能调这些。**新增的方法缺省不在名单里**；加进来之前要想清楚它能碰什么。
/// 守护测试（`tests/twin_device.rs`）遍历方法注册表，要求每个请求方法恰好出现在这里或者测试里的「被拒绝」表里。
pub const ALLOWED: &[(&str, Scope)] = &[
    (methods::NODE_INFO, Scope::Open),
    (methods::AGENT_LIST, Scope::Open),
    (methods::TWIN_PEER_STATUS, Scope::Open),
    (methods::TODO_ADD, Scope::NewTodo),
    (methods::TODO_GET, Scope::OwnTodo),
    (methods::TODO_LIST, Scope::OwnTodoList),
    (methods::TODO_HISTORY, Scope::OwnTodo),
    (methods::TODO_DISPATCH, Scope::OwnTodo),
    (methods::SESSION_GET, Scope::OwnSession),
    (methods::SESSION_HISTORY, Scope::OwnSession),
    (methods::SESSION_WATCH, Scope::OwnSession),
    (methods::SESSION_UNWATCH, Scope::OwnSession),
    (methods::SESSION_CANCEL, Scope::OwnSession),
    // 看改动：只有 diff（别的会话状态方法会触发 `gh` 之类的外部调用，不开放）
    (methods::SESSION_DIFF, Scope::OwnSession),
];

/// 推给 Twin 设备的通知：只有这四种，并且只限它自己的项和会话。别的一律不推（会话列表、权限请求、清单、接入、安全事件、
/// 项目和 agent 的变化…），也不给它发 `notify`。**新增的通知缺省不推。**
pub const PUSHED: &[&str] =
    &[methods::TODO_CHANGED, methods::TODO_EVENT, methods::SESSION_CHANGED, methods::SESSION_EVENT];

pub fn scope_of(method: &str) -> Option<Scope> {
    ALLOWED.iter().find(|(m, _)| *m == method).map(|(_, s)| *s)
}

use crate::worktree::err_reason;

pub(crate) fn twin_device_forbidden() -> RpcError {
    err_reason(ErrorCode::Forbidden, "this method is not available to a twin device", "twin_device")
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    tracing::warn!(error = %e, "twin device storage error");
    RpcError::new(ErrorCode::Internal, "twin storage error")
}

// ====================================================================== 归属（推送用的内存索引）

#[derive(Default)]
struct Owned {
    todos: HashSet<String>,
    sessions: HashSet<String>,
}

/// 每台 Twin 设备建的项和派出的会话（推送过滤用）。库是真源：启动时 [`TwinOwners::load`]，之后每次写库成功才更新。
#[derive(Default)]
pub struct TwinOwners {
    inner: Mutex<HashMap<String, Owned>>,
}

impl TwinOwners {
    pub fn add_todo(&self, device: &str, id: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).entry(device.into()).or_default().todos.insert(id.into());
    }

    pub fn add_session(&self, device: &str, id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(device.into())
            .or_default()
            .sessions
            .insert(id.into());
    }

    /// 这一项没了（删除、保留期清理）。
    pub fn forget_todo(&self, id: &str) {
        for o in self.inner.lock().unwrap_or_else(|e| e.into_inner()).values_mut() {
            o.todos.remove(id);
        }
    }

    pub fn owns_todo(&self, device: &str, id: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).get(device).is_some_and(|o| o.todos.contains(id))
    }

    pub fn owns_session(&self, device: &str, id: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).get(device).is_some_and(|o| o.sessions.contains(id))
    }

    /// 启动时从库里读。
    pub async fn load(&self, store: &Store) {
        match store.twin_device_todos().await {
            Ok(rows) => rows.iter().for_each(|(device, id)| self.add_todo(device, id)),
            Err(e) => tracing::warn!(error = %e, "cannot read the todos of twin devices"),
        }
        match store.twin_device_sessions().await {
            Ok(rows) => rows.iter().for_each(|(device, id)| self.add_session(device, id)),
            Err(e) => tracing::warn!(error = %e, "cannot read the sessions of twin devices"),
        }
    }
}

/// 这个调用方是不是经 hub 来的 Twin 设备。
pub fn is_twin_device(caller: &Caller) -> bool {
    !caller.local && caller.kind == DeviceKind::Twin
}

// ====================================================================== 推送过滤

/// 这条通知能不能推给 Twin 设备 `device`。`watching`：这个设备现在是不是 watch 着通知里的会话。
/// 不在 [`PUSHED`] 里的方法一律 `false`；在里面的要通知里的 id 属于这台设备（`session.event` 还要它 watch 着）。
pub fn pushable(owners: &TwinOwners, device: &str, watching: impl Fn(&str) -> bool, n: &RpcNotification) -> bool {
    let id_of = |field: &str| n.params.get(field).and_then(Value::as_str);
    match n.method.as_str() {
        methods::TODO_CHANGED => id_of("id").is_some_and(|id| owners.owns_todo(device, id)),
        methods::TODO_EVENT => id_of("todo_id").is_some_and(|id| owners.owns_todo(device, id)),
        methods::SESSION_CHANGED => id_of("session_id").is_some_and(|id| owners.owns_session(device, id)),
        methods::SESSION_EVENT => id_of("session_id").is_some_and(|id| owners.owns_session(device, id) && watching(id)),
        _ => false,
    }
}

// ====================================================================== 方法白名单

/// 在 RPC 分发处调用（调用方是 Twin 设备时）：方法在不在名单里，名单里的归属对不对。通过了才会进处理函数。
pub async fn authorize(c: &Ctx, caller: &Caller, method: &str, params: &Value) -> Result<()> {
    let Some(scope) = scope_of(method) else { return Err(twin_device_forbidden()) };
    let text = |field: &str| params.get(field).and_then(Value::as_str);
    let owned = match scope {
        Scope::Open | Scope::OwnTodoList => return Ok(()),
        // 参数缺了或不是字符串：交给处理函数按 `invalid` 回；这里不替它们编一个存在与否的答案
        Scope::OwnTodo | Scope::NewTodo => {
            let Some(id) = text("id") else { return Ok(()) };
            let source = c.store.todo_source(id).await.map_err(internal)?;
            match source {
                Some((kind, device)) => {
                    kind == crate::todos::source_kind_str(TodoSourceKind::Twin)
                        && device.as_deref() == Some(caller.device_id.as_str())
                }
                // 不存在：新建可以；别的方法和「别人的项」同样 `forbidden`
                None => scope == Scope::NewTodo,
            }
        }
        Scope::OwnSession => {
            let Some(id) = text("session_id") else { return Ok(()) };
            c.store.twin_session_owner(id).await.map_err(internal)?.as_deref() == Some(caller.device_id.as_str())
        }
    };
    if owned {
        Ok(())
    } else {
        Err(twin_device_forbidden())
    }
}

// ====================================================================== 范围

/// 这台 Twin 设备的范围（`twin_scopes`，请求方 = 设备 id）。缺省 `projects = []`：什么都不开放。
pub async fn scope_of_device(c: &Ctx, device: &str) -> Result<TwinScope> {
    c.store.twin_scope(device).await.map_err(internal)
}

/// 这台设备在这个本地日里已经派发的次数。
pub async fn dispatches_today(c: &Ctx, device: &str) -> Result<u32> {
    let start = local_day_start(c.judge.clock().now()).ok_or_else(|| internal("local day"))?;
    c.store.twin_dispatches_since(device, start).await.map_err(internal)
}

/// 这台设备派出的、还在处理中的会话数（同本机 Twin 的「处理中」：在跑、排队、等审批、还有后台任务）。
pub async fn working_sessions(c: &Ctx, device: &str) -> Result<usize> {
    let mut n = 0;
    for (sid, _) in c.store.twin_sessions_of(device).await.map_err(internal)? {
        if c.sessions.status_of(&sid).await.is_some_and(|(status, tasks)| processing(status, tasks)) {
            n += 1;
        }
    }
    Ok(n)
}

/// 「处理中」：和本机 Twin 判断一项有没有在干活是同一个条件。
pub(crate) fn processing(status: SessionStatus, background_tasks: u32) -> bool {
    background_tasks > 0
        || matches!(
            status,
            SessionStatus::Running | SessionStatus::Starting | SessionStatus::Queued | SessionStatus::WaitingPermission
        )
}

/// `twin.peer_status`：B 对这条连接开放了什么、还剩多少。只给调用的这条连接自己的。
pub async fn peer_status(c: &Ctx, caller: &Caller) -> Result<TwinPeerStatus> {
    if !is_twin_device(caller) {
        return Err(err_reason(
            ErrorCode::Forbidden,
            "twin.peer_status is for the twin devices of other nodes",
            "not_twin_device",
        ));
    }
    let scope = scope_of_device(c, &caller.device_id).await?;
    let cfg = c.config.get();
    let working = working_sessions(c, &caller.device_id).await?;
    let today = dispatches_today(c, &caller.device_id).await?;
    let (counts, _) = c.sessions.counts().await;
    let active = (counts.running + counts.queued + counts.waiting_permission) as usize;
    let for_humans = cfg.sessions.max_concurrent.saturating_sub(1);
    let room =
        (scope.max_sessions as usize).min(for_humans).saturating_sub(working).min(for_humans.saturating_sub(active));
    let agents = c
        .agents
        .list()
        .agents
        .iter()
        .map(|a| TwinPeerAgent { id: a.id.clone(), label: a.label.clone(), ready: a.status == AgentStatus::Ready })
        .collect();
    Ok(TwinPeerStatus {
        name: cfg.node_name(),
        group: cfg.node_group(),
        projects: scope.projects.clone(),
        agents,
        remaining_sessions: room as u32,
        remaining_dispatches: scope.daily_dispatches.saturating_sub(today),
        paused: scope.paused,
        accept_links: cfg.twin.accept_links,
    })
}

// ====================================================================== 记一项和派发

/// 这一项是不是这台设备建的。
pub fn owns(todo: &Todo, device: &str) -> bool {
    todo.source.kind == TodoSourceKind::Twin && todo.source.device_id.as_deref() == Some(device)
}

/// `todo.add` 来自 Twin 设备：`project` 必须在这条连接开放的项目里（`forbidden`，`data.reason = "out_of_scope"`）。
/// 返回规范化的项目名。
pub async fn check_project_in_scope(c: &Ctx, device: &str, project: Option<&str>) -> Result<String> {
    let Some(project) = project.map(str::trim).filter(|p| !p.is_empty()) else {
        return Err(err_reason(ErrorCode::Invalid, "a twin device must name the project of the todo", "project"));
    };
    let project = crate::todos::check_project(project)?;
    let scope = scope_of_device(c, device).await?;
    if !scope.projects.iter().any(|p| c.workspace.same_project_name(p, &project)) {
        return Err(out_of_scope());
    }
    Ok(project)
}

fn out_of_scope() -> RpcError {
    err_reason(ErrorCode::Forbidden, "the project is not open to this twin device", "out_of_scope")
}

fn invalid_field(field: &str, reason: &str) -> RpcError {
    let mut e = RpcError::new(ErrorCode::Invalid, format!("invalid dispatch {field}: {reason}"));
    if let Some(d) = &mut e.data {
        d.field = Some(field.into());
        d.reason = Some(reason.into());
    }
    e
}

/// `todo.dispatch` 来自 Twin 设备：调用方已持清单锁，`t` 是读到的最新的项。
///
/// - 只派它自己建的项（再确认一次：分发处的检查在锁外）；
/// - 项目（请求里的，缺省项自己的）必须还在范围里；
/// - agent / 模型 / 模式由范围定，`no_push` 恒为 true，`workspace` 恒为 worktree：请求里带的这些字段只能落在这些值里，否则 `invalid`；
/// - `paused`、名额、每日次数用和本机 Twin 同一个实现（[`crate::twin::capacity_skip`]），超出时 `invalid`
///   （`data.reason` 是 `paused` / `no_capacity` / `budget`）；同一个 `message_id` 的重试不再占名额和次数。
pub async fn dispatch_settings_locked(
    c: &Ctx,
    caller: &Caller,
    t: &Todo,
    p: &TodoDispatchParams,
) -> Result<(String, SessionSettings)> {
    if !owns(t, &caller.device_id) {
        return Err(twin_device_forbidden());
    }
    let scope = scope_of_device(c, &caller.device_id).await?;
    let project = p.project.as_deref().map(str::trim).filter(|s| !s.is_empty()).or(t.project.as_deref());
    let in_scope = project.is_some_and(|p| scope.projects.iter().any(|a| c.workspace.same_project_name(a, p)));
    if !in_scope {
        return Err(out_of_scope());
    }
    if scope.paused {
        return Err(invalid_field("dispatch", "paused"));
    }
    let (agent, settings) = crate::twin::settings(c, &scope)?;
    // 请求里带了就要和范围定的一样
    if !p.agent.is_empty() && p.agent != agent {
        return Err(invalid_field("agent", "not_allowed"));
    }
    let wants = &p.settings;
    if wants.model.as_ref().is_some_and(|m| settings.model.as_ref() != Some(m))
        || wants.mode.as_ref().is_some_and(|m| settings.mode.as_ref() != Some(m))
        || wants.thinking.is_some()
        || !wants.env.is_empty()
        || wants.options.iter().any(|(k, v)| settings.options.get(k) != Some(v))
    {
        return Err(invalid_field("settings", "not_allowed"));
    }
    if p.no_push == Some(false) {
        return Err(invalid_field("no_push", "not_allowed"));
    }
    if p.workspace.as_ref().is_some_and(|w| !matches!(w, SessionWorkspace::Worktree { base: None, branch: None })) {
        return Err(invalid_field("workspace", "not_allowed"));
    }
    // 同一个 `message_id` 的重试（回执丢了）会拿回第一次的结果，不是新的派发：不再要名额和次数
    let retry = match p.message_id.as_deref() {
        Some(m) => {
            c.store.find_session_by_message(m).await.map_err(internal)?.is_some_and(|sid| t.sessions.contains(&sid))
        }
        None => false,
    };
    if !retry {
        let working = working_sessions(c, &caller.device_id).await?;
        let today = dispatches_today(c, &caller.device_id).await?;
        if let Some(skip) = crate::twin::capacity_skip(c, &scope, today, working, false).await {
            return Err(invalid_field("dispatch", crate::twin::skip_reason_str(skip)));
        }
    }
    Ok((agent, settings))
}
