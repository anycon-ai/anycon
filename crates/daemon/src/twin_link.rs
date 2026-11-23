//! 使用别的节点这一侧（A）：分身设备身份和长连接、`twin.link` / `twin.links` / `twin.unlink`、分拣的节点表、派发到别的节点、
//! 取回结果（docs/spec/daemon.md § 27.1、§ 27.2、§ 27.4）。
//!
//! - **身份**：存在数据目录的 `twin/` 下，和 CLI 的设备身份同一种文件（[`HubState`]），第一次 `twin.link` 时生成；已配对的节点以
//!   `twin/nodes.json` 为准，库里的表 `twin_links` 只存它的元数据（名称、分组、上一次的 `twin.peer_status`），启动时对账。
//! - **连接**：至少有一条分身连接时 daemon 起一个 [`Client`]（一条 hub 连接对多个节点，设备种类 `twin`），没有就不起，关停时
//!   [`TwinLinks::shutdown`]。`[twin] enabled = false` 不影响它。
//! - **没有代理**：[`Client`] 只在这个模块里，不存进别处、不经任何 RPC 方法递出去。A 上的任何调用方都没有「经 A 调 B」的办法：
//!   B 上的动作只有两处——分拣的计划被 [`TwinLinks::dispatch`] 执行（`todo.add`、`todo.dispatch`），和后台任务对远程项做的只读同步
//!   （`todo.get`、`todo.history`、`twin.peer_status`）。
//! - **取回结果**：B 上远程项的 `todo.event` / `todo.changed` 只是「去同步」的信号；同步读 B 的 `todo.history`，把 `fetched_seq` 之后的
//!   `result` 按顺序记进 A 这一项的「经过」，所以重连、重启之后漏掉的也会补上，不会重复。
//!
//! B 上项的内容（标题、会话的结果）是外来的文字：不进日志、不进审计；日志和审计只带 id、数字和枚举。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anycon_protocol::{
    methods, DeviceKind, Empty, ErrorCode, RpcError, SessionIdParams, SessionStatus, SessionSummary, Todo,
    TodoActivity, TodoActor, TodoChanged, TodoDispatchParams, TodoDispatchResult, TodoEvent, TodoEventData,
    TodoEventNotice, TodoHandler, TodoHistoryParams, TodoHistoryResult, TodoIdParams, TwinLink, TwinLinkParams,
    TwinLinksResult, TwinPeerStatus, TwinTrigger, TwinUnlinkParams,
};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;

use crate::client::{
    client_info, host_of, sanitize_label, Client, ClientError, ClientEvent, ClientOptions, ErrorClass, HubState,
    NodeStatus,
};
use crate::clients::Caller;
use crate::daemon::Ctx;
use crate::store::{LinkRow, RemoteRow};
use crate::todo_dispatch::{truncate_utf8, MAX_NOTE_BYTES, RESULT_MAX_BYTES};
use crate::worktree::err_reason;

type Result<T> = std::result::Result<T, RpcError>;

/// `twin.peer_status` 等多久。
const PEER_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
/// 对 B 的 `todo.add` / `todo.dispatch` 等多久。
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 对 B 的只读同步（`todo.get`、`todo.history`）等多久。
const SYNC_TIMEOUT: Duration = Duration::from_secs(15);
/// 分拣输入里节点表的字节上限（§ 27.4）。
const TABLE_BYTES: usize = 4 * 1024;
/// 节点表里每个节点最多列几个项目、几个 agent；名字和项目名按字符截断。
const TABLE_PROJECTS: usize = 20;
const TABLE_AGENTS: usize = 10;
const LABEL_CHARS: usize = 64;
/// 补取一个远程项的「经过」最多翻几页（每页 200 条）。
const HISTORY_PAGES: usize = 10;
/// 分拣输入里每个远程项带几条最近的远程结果。
pub(crate) const PROMPT_REMOTE_RESULTS: u32 = 3;

fn internal(e: impl std::fmt::Display) -> RpcError {
    tracing::warn!(error = %e, "twin link storage error");
    RpcError::new(ErrorCode::Internal, "twin link storage error")
}

/// 分身设备在 hub 上的名字：「<节点名>'s twin」（B 的设备列表里照这个显示，客户端按设备种类本地化）。
fn device_name(node_name: &str) -> String {
    format!("{}'s twin", sanitize_label(node_name, 48, "node"))
}

/// 派发到别的节点失败的两种：B 不在线 / 调不通 / 超时，和 B 明确拒绝了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// B 不在线、握手没成功、超时、传输出错。带一个机器可读的原因（`offline`、`timeout`……）。
    Unavailable(String),
    /// B 回了错误：带 B 的 `data.reason`（没有就用错误码），已经按 `[a-z0-9_]{1,40}` 清理过。
    Refused(String),
}

fn failure_of(e: ClientError) -> Failure {
    match e.rpc {
        Some(rpc) => {
            let reason = rpc.data.as_ref().and_then(|d| d.reason.clone()).unwrap_or(e.code);
            let clean: String =
                reason.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_').take(40).collect();
            Failure::Refused(if clean.is_empty() { "error".into() } else { clean })
        }
        None => Failure::Unavailable(e.code),
    }
}

/// `twin.link` 的错误：说得出原因的都带 `data.reason`。
///
/// 这里的连接只为分身设备而开，所以 hub 以 `protocol` 拒绝认证就是它不认识 `twin` 设备种类（0.4 之前的 hub）；
/// 别处的 `protocol` 拒绝不一定是这个原因，不在这里解释。
fn link_error(e: ClientError) -> RpcError {
    let reason = e.reason.clone().unwrap_or_else(|| e.code.clone());
    if reason == crate::client::HUB_PROTOCOL_REASON {
        return err_reason(
            ErrorCode::Unsupported,
            "the hub does not accept twin devices; it may be older than anycon 0.4, so update the hub first",
            "hub_unsupported",
        );
    }
    // hub 限速了这个地址上的新连接（等过几次仍然不行）。协议没有专门的限速码：`busy`（暂时做不了、稍后再试）
    // 最接近，和 `node.pair_begin` 对 hub 限速的约定一致；文字里已经带着建议等多久。
    if e.code == crate::client::RATE_LIMITED_CODE {
        return err_reason(ErrorCode::Busy, e.message, crate::client::HUB_RATE_LIMITED_REASON);
    }
    let (code, hint) = match (e.class, e.code.as_str()) {
        (ErrorClass::Usage, _) => (ErrorCode::Invalid, ""),
        (ErrorClass::Offline, _) => (ErrorCode::Unsupported, ""),
        (_, "node_mismatch") => (ErrorCode::Invalid, ""),
        (_, "pair_failed") => match reason.as_str() {
            "not_found" => (
                ErrorCode::NotFound,
                " The other node must be on the same hub as this one, and the code must be fresh and from \
                 `node.twin_link_code`.",
            ),
            "timeout" => (
                ErrorCode::Unsupported,
                " The other node may have turned off [twin] accept_links, or is not accepting twin links.",
            ),
            _ => (ErrorCode::Forbidden, ""),
        },
        _ => (ErrorCode::Internal, ""),
    };
    err_reason(code, format!("{}{hint}", e.message), &reason)
}

fn label(s: &str) -> String {
    sanitize_label(s, LABEL_CHARS, "")
}

/// 分拣输入里的节点表：每个节点一行 JSON，合计 ≤ [`TABLE_BYTES`]。
pub(crate) struct PeerTable {
    /// 节点行，已经 JSON 化；放进随机边界里。
    pub rows: String,
}

/// 一个节点这一次唤醒的情况；`status = None` 表示不可用（不在线或调不通）。
struct PeerRow {
    id: String,
    name: String,
    status: Option<TwinPeerStatus>,
}

fn row_json(r: &PeerRow) -> Value {
    match &r.status {
        Some(s) => json!({
            "id": r.id,
            "name": label(&r.name),
            "group": s.group.as_deref().map(label),
            "available": true,
            "projects": s.projects.iter().take(TABLE_PROJECTS).map(|p| label(p)).collect::<Vec<_>>(),
            "agents": s.agents.iter().take(TABLE_AGENTS)
                .map(|a| json!({ "id": label(&a.id), "ready": a.ready })).collect::<Vec<_>>(),
            "remaining_sessions": s.remaining_sessions,
            "remaining_dispatches": s.remaining_dispatches,
            "paused": s.paused,
        }),
        None => json!({ "id": r.id, "name": label(&r.name), "available": false }),
    }
}

/// 按节点名排序，一行一个节点，装不下的节点丢掉。
fn render(mut rows: Vec<PeerRow>) -> Option<PeerTable> {
    if rows.is_empty() {
        return None;
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    let mut out = String::new();
    for r in &rows {
        let line = row_json(r).to_string();
        if out.len() + line.len() + 1 > TABLE_BYTES {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    Some(PeerTable { rows: out.trim_end().to_string() })
}

/// 这一项在 B 上的新 id：由 A 上这一项的 id 和已经派出的次数定出来。`todo.add` 按 id 幂等，所以「B 上记好了、派发没成功」之后
/// 重试用的是同一个项，B 上不会留下多余的项；派发成功、A 记下之后次数加一，下一次是新的项。
fn remote_id(todo_id: &str, n: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("anycon-twin-remote:{todo_id}:{n}"));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid().hyphenated().to_string()
}

/// A 上所有分身连接的状态：一个 [`Client`] 和它的事件任务。
#[derive(Default)]
pub struct TwinLinks {
    /// `twin.link` / `twin.unlink` / 启动 / 关停互相串行。
    ops: tokio::sync::Mutex<()>,
    running: Mutex<Option<(Client, tokio::task::JoinHandle<()>)>>,
    /// 事件任务和补取要在后台用到上下文：弱引用，不让 `Ctx` 和它自己互相持有。
    ctx: Mutex<Weak<Ctx>>,
    /// 已经关停：不再开连接。
    closed: std::sync::atomic::AtomicBool,
    /// 同步远程项的「经过」一次只做一个（事件任务和刚派发完的补取都走它）。
    sync_gate: Arc<tokio::sync::Mutex<()>>,
    /// 远程会话「没有进展」的计时（§ 26.4）：只在内存里，A 重启、B 重新连上、刚派发完都重新起算。
    stalls: Mutex<HashMap<(String, String), Stall>>,
}

/// 2 小时没有事件算卡住（§ 26.4），和本机会话同一个数。
const STALL_MS: u64 = 120 * 60_000;

/// 一个远程会话最近看到的 `last_seq`，和从什么时候起它没变过。时间是 A 自己的时钟，不比较两台机器的时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stall {
    last_seq: Option<u64>,
    since_ms: u64,
}

impl Stall {
    /// 又看到一次 `last_seq`：变了（或第一次看到）就重新起算；返回是否已经满 2 小时没变。
    fn observe(&mut self, last_seq: u64, now_ms: u64) -> bool {
        match self.last_seq {
            // 派发时留的基线：第一次读到 seq 只补上数字，起算点还是派发的时候
            None => self.last_seq = Some(last_seq),
            Some(prev) if prev != last_seq => *self = Stall { last_seq: Some(last_seq), since_ms: now_ms },
            Some(_) => {}
        }
        now_ms.saturating_sub(self.since_ms) >= STALL_MS
    }
}

impl TwinLinks {
    fn client(&self) -> Option<Client> {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|(c, _)| c.clone())
    }

    /// daemon 启动时：库里和 `twin/nodes.json` 对账；有分身连接就起长连接。
    pub async fn start(&self, c: &Arc<Ctx>) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(c);
        let _op = self.ops.lock().await;
        let Some(url) = hub_url(c) else { return };
        let state = HubState::load(&c.paths.twin_dir(), &url);
        if state.problem.is_some() || state.nodes.is_empty() {
            if let Some(p) = &state.problem {
                tracing::warn!(problem = %p, "the twin device state is unusable; no twin link is started");
            }
            // 没有已配对的节点：库里残留的元数据也不要
            if state.problem.is_none() {
                for l in c.store.twin_links().await.unwrap_or_default() {
                    let _ = c.store.twin_link_delete(&l.node_id).await;
                }
            }
            return;
        }
        if let Err(e) = self.reconcile(c, &state).await {
            tracing::warn!(error = %e.message, "cannot reconcile the twin links");
        }
        if let Err(e) = self.ensure_client(c).await {
            tracing::warn!(error = %e.message, "the twin connection did not start");
        }
    }

    /// 关停：断开 hub 连接、结束事件任务。不等正在进行的 `twin.link`（它可能在等对端完成配对）：关停之后不再开新的连接。
    pub async fn shutdown(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        self.stop().await;
    }

    async fn stop(&self) {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some((client, listener)) = running {
            client.shutdown().await;
            listener.abort();
            let _ = listener.await;
        }
    }

    /// 库里的元数据和 `nodes.json` 对齐：文件里有、库里没有的补一行，库里有、文件里没有的删掉。
    async fn reconcile(&self, c: &Ctx, state: &HubState) -> Result<()> {
        let rows = c.store.twin_links().await.map_err(internal)?;
        for n in &state.nodes {
            if !rows.iter().any(|r| r.node_id == n.id) {
                let row = LinkRow {
                    node_id: n.id.clone(),
                    name: label(&n.name),
                    group: None,
                    pubkey: n.pubkey.clone(),
                    paired_ms: n.paired_at_ms,
                    status: None,
                };
                c.store.twin_link_save(&row).await.map_err(internal)?;
            }
        }
        for r in rows.iter().filter(|r| !state.nodes.iter().any(|n| n.id == r.node_id)) {
            c.store.twin_link_delete(&r.node_id).await.map_err(internal)?;
        }
        Ok(())
    }

    /// 长连接还没起就起（调用方持 `ops`）。
    async fn ensure_client(&self, c: &Ctx) -> Result<Client> {
        if let Some(client) = self.client() {
            return Ok(client);
        }
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(err_reason(ErrorCode::Unsupported, "the daemon is shutting down", "shutting_down"));
        }
        let url = hub_url(c).ok_or_else(|| {
            err_reason(
                ErrorCode::Unsupported,
                "this node has no hub configured; twin links go through the hub",
                "no_hub",
            )
        })?;
        let mut state = HubState::load(&c.paths.twin_dir(), &url);
        state.check().map_err(link_error)?;
        if state.nodes.is_empty() {
            // 还没有分身连接：身份属于现在的 hub
            state.hub_url = url.clone();
        } else if host_of(&state.hub_url).ok() != host_of(&url).ok() {
            return Err(err_reason(
                ErrorCode::Unsupported,
                "the existing twin links were made on another hub; unlink them before linking on this hub",
                "hub_changed",
            ));
        }
        let opts = ClientOptions::new(client_info(&device_name(&c.config.get().node_name()), DeviceKind::Twin));
        let client = Client::start(state, opts).map_err(link_error)?;
        let rx = client.subscribe();
        let weak = self.ctx.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let listener = tokio::spawn(events(weak, client.clone(), rx, self.sync_gate.clone()));
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some((client.clone(), listener));
        Ok(client)
    }

    // ------------------------------------------------------------ RPC

    /// `twin.link`：用 B 的短码以分身设备的身份和 B 配对。
    pub async fn link(&self, c: &Ctx, caller: &Caller, p: TwinLinkParams) -> Result<TwinLink> {
        crate::twin::human(caller)?;
        if p.node_id.trim().is_empty() {
            return Err(err_reason(ErrorCode::Invalid, "node_id is required", "node_id"));
        }
        let _op = self.ops.lock().await;
        let client = self.ensure_client(c).await?;
        let entry = match client.pair(&p.code, Some(&p.node_id)).await {
            Ok(e) => e,
            Err(e) => {
                if client.nodes().is_empty() {
                    self.stop().await;
                }
                return Err(link_error(e));
            }
        };
        let row = LinkRow {
            node_id: entry.id.clone(),
            name: label(&entry.name),
            group: None,
            pubkey: entry.pubkey.clone(),
            paired_ms: entry.paired_at_ms,
            status: None,
        };
        c.store.twin_link_save(&row).await.map_err(internal)?;
        c.audit.record(caller, "twin.link", Some(&entry.id), json!({ "name": row.name })).await;
        // 尽力取一次 B 的 `twin.peer_status`（名称、分组、开放了什么）；取不到不影响建立
        refresh_status(c, &client, &entry.id).await;
        crate::twin::announce_links(c).await;
        let row = c.store.twin_links().await.map_err(internal)?.into_iter().find(|r| r.node_id == entry.id);
        Ok(view(&client, &row.unwrap_or(LinkRow { status: None, ..fallback_row(&entry.id) })))
    }

    /// `twin.links`。
    pub async fn links(&self, c: &Ctx) -> Result<TwinLinksResult> {
        let client = self.client();
        let rows = c.store.twin_links().await.map_err(internal)?;
        let links = rows
            .iter()
            .map(|r| match &client {
                Some(client) => view(client, r),
                None => view_offline(r),
            })
            .collect();
        Ok(TwinLinksResult { links })
    }

    /// `twin.unlink`：只在 A 上忘掉这一条（关掉传输、删记录）。B 上的设备记录要在 B 上吊销。
    pub async fn unlink(&self, c: &Ctx, caller: &Caller, p: TwinUnlinkParams) -> Result<Empty> {
        crate::twin::human(caller)?;
        let _op = self.ops.lock().await;
        let removed = match self.client() {
            Some(client) => {
                let removed = client.remove_node(&p.node_id).await.map_err(link_error)?;
                if client.nodes().is_empty() {
                    self.stop().await;
                }
                removed
            }
            None => {
                // 没有长连接（比如 hub 换了之后没起来）：直接改文件
                let dir = c.paths.twin_dir();
                let mut state = HubState::load(&dir, &hub_url(c).unwrap_or_default());
                state.check().map_err(link_error)?;
                let before = state.nodes.len();
                state.nodes.retain(|n| n.id != p.node_id);
                let removed = state.nodes.len() != before;
                if removed {
                    state.save().map_err(link_error)?;
                }
                removed
            }
        };
        let had_row = c.store.twin_link_delete(&p.node_id).await.map_err(internal)?;
        if !removed && !had_row {
            return Err(RpcError::new(ErrorCode::NotFound, "no such twin link"));
        }
        c.audit.record(caller, "twin.unlink", Some(&p.node_id), json!({})).await;
        crate::twin::announce_links(c).await;
        Ok(Empty {})
    }

    // ------------------------------------------------------------ 分拣用

    fn stalls(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Stall>> {
        self.stalls.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// B 重新连上：这个节点上所有远程会话的计时重新起算，B 离线的时间不算进「没有进展」。
    fn stall_reset_node(&self, node: &str) {
        self.stalls().retain(|(n, _), _| n != node);
    }

    /// 这一项交给分身的远程会话里，有没有在运行中满 2 小时 `last_seq` 没变的（§ 26.4）。返回「在哪个节点上」的说明，没有就 `None`。
    ///
    /// 只看 B 最近知道的 `activity` 是 `working`、B 在线的远程项；`session.get` 调不通这次不判断（不算失败），计时也重新起算，
    /// 所以断断续续连不上的时间不会被当成「没有进展」。
    pub(crate) async fn stalled(&self, c: &Ctx, todo_id: &str) -> Option<String> {
        let client = self.client()?;
        let rows = c.store.todo_remote_rows(todo_id).await.ok()?;
        for row in rows {
            if row.activity != Some(Some(TodoActivity::Working))
                || client.node_status(&row.node_id) != NodeStatus::Ready
            {
                continue;
            }
            let Ok(Some(sid)) = c.store.todo_remote_session(todo_id, &row.node_id, &row.remote_todo_id).await else {
                continue;
            };
            let key = (row.node_id.clone(), sid.clone());
            let summary = client
                .request(&row.node_id, methods::SESSION_GET, SessionIdParams { session_id: sid }, SYNC_TIMEOUT)
                .await
                .ok()
                .and_then(|v| serde_json::from_value::<SessionSummary>(v).ok());
            let now = c.judge.clock().now();
            let stalled = match summary {
                Some(s)
                    if matches!(s.status, SessionStatus::Running | SessionStatus::Starting | SessionStatus::Queued) =>
                {
                    self.stalls().entry(key).or_insert(Stall { last_seq: None, since_ms: now }).observe(s.last_seq, now)
                }
                _ => {
                    self.stalls().remove(&key);
                    false
                }
            };
            if stalled {
                let name = c.store.twin_links().await.ok()?.into_iter().find(|l| l.node_id == row.node_id)?.name;
                return Some(format!(
                    "The dispatched session on node {} has had no events for two hours",
                    label(&name)
                ));
            }
        }
        None
    }

    /// 这个 id 是不是一条分身连接（分拣的 `dispatch.node` 要在这里面）。
    pub(crate) fn knows(&self, node_id: &str) -> bool {
        self.client().is_some_and(|c| c.nodes().iter().any(|n| n.id == node_id))
    }

    /// 节点表（§ 27.4）：对每条在线的连接调一次 `twin.peer_status`（并发，各 5 秒），不在线或调不通的标成不可用。没有连接返回 `None`。
    pub(crate) async fn table(&self, c: &Ctx) -> Option<PeerTable> {
        let client = self.client()?;
        let rows = c.store.twin_links().await.ok()?;
        let fetched = futures::future::join_all(rows.iter().map(|r| async {
            let status = if client.node_status(&r.node_id) == NodeStatus::Ready {
                fetch_status(c, &client, &r.node_id).await
            } else {
                None
            };
            PeerRow {
                id: r.node_id.clone(),
                name: status.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| r.name.clone()),
                status,
            }
        }))
        .await;
        render(fetched)
    }

    /// 分拣的计划里的一次远程派发：在 B 上 `todo.add`、`todo.dispatch`，成功后 A 这一项记下 `Todo.remote` 和 `remote_dispatched`。
    pub(crate) async fn dispatch(
        &self,
        c: &Ctx,
        run_id: &str,
        t: &Todo,
        node: &str,
        project: &str,
        note: Option<&str>,
    ) -> std::result::Result<(), Failure> {
        let client = self.client().ok_or_else(|| Failure::Unavailable("offline".into()))?;
        if !client.nodes().iter().any(|n| n.id == node) {
            return Err(Failure::Unavailable("not_linked".into()));
        }
        let n = c.store.todo_remotes(&t.id).await.map_err(|_| Failure::Unavailable("storage".into()))?.len();
        let rid = remote_id(&t.id, n);
        client
            .request(
                node,
                methods::TODO_ADD,
                json!({ "id": rid, "title": t.title, "body": t.body, "project": project }),
                CALL_TIMEOUT,
            )
            .await
            .map_err(failure_of)?;
        let params = TodoDispatchParams {
            id: rid.clone(),
            agent: String::new(),
            project: None,
            workspace: None,
            settings: Default::default(),
            no_push: None,
            message_id: Some(format!("twin-remote-{rid}")),
            note: note.map(|n| truncate_utf8(n, MAX_NOTE_BYTES).0),
        };
        let value = client.request(node, methods::TODO_DISPATCH, params, CALL_TIMEOUT).await.map_err(failure_of)?;
        let session_id = serde_json::from_value::<TodoDispatchResult>(value)
            .map(|r| r.session.id)
            .map_err(|_| Failure::Unavailable("bad_reply".into()))?;
        record_dispatch(c, run_id, &t.id, node, &rid, &session_id)
            .await
            .map_err(|_| Failure::Unavailable("storage".into()))?;
        // 刚派发完：从现在起算「没有进展」
        self.stalls()
            .insert((node.to_string(), session_id.clone()), Stall { last_seq: None, since_ms: c.judge.clock().now() });
        // 刚派出的项可能很快就有结果：B 的 `todo.event` 比 A 记下这一行早到的话，事件任务会找不到它，所以现在补取一次
        let c2 = self.ctx.lock().unwrap_or_else(|e| e.into_inner()).upgrade();
        let (client2, gate) = (client.clone(), self.sync_gate.clone());
        let row = RemoteRow {
            todo_id: t.id.clone(),
            node_id: node.to_string(),
            remote_todo_id: rid.clone(),
            fetched_seq: 0,
            activity: None,
        };
        if let Some(c2) = c2 {
            tokio::spawn(async move {
                let _gate = gate.lock().await;
                sync_remote(&c2, &client2, &row).await;
            });
        }
        Ok(())
    }
}

fn hub_url(c: &Ctx) -> Option<String> {
    c.config.get().hub.url.clone().filter(|u| !u.trim().is_empty())
}

fn fallback_row(node_id: &str) -> LinkRow {
    LinkRow {
        node_id: node_id.into(),
        name: String::new(),
        group: None,
        pubkey: String::new(),
        paired_ms: 0,
        status: None,
    }
}

fn fingerprint(pubkey: &str) -> String {
    hex::decode(pubkey)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .map(|k| anycon_crypto::fingerprint(&k))
        .unwrap_or_default()
}

fn view_offline(r: &LinkRow) -> TwinLink {
    TwinLink {
        node_id: r.node_id.clone(),
        name: r.name.clone(),
        group: r.group.clone(),
        fingerprint: fingerprint(&r.pubkey),
        paired_ms: r.paired_ms,
        online: false,
        status: r.status.clone(),
    }
}

fn view(client: &Client, r: &LinkRow) -> TwinLink {
    TwinLink { online: client.node_status(&r.node_id) == NodeStatus::Ready, ..view_offline(r) }
}

// ====================================================================== B 的 `twin.peer_status`

async fn fetch_status(c: &Ctx, client: &Client, node: &str) -> Option<TwinPeerStatus> {
    let value = client.request(node, methods::TWIN_PEER_STATUS, json!({}), PEER_STATUS_TIMEOUT).await.ok()?;
    let status: TwinPeerStatus = serde_json::from_value(value).ok()?;
    let name = label(&status.name);
    if let Err(e) = c.store.twin_link_status(node, &name, &status).await {
        tracing::warn!(error = %e, "cannot save the peer status of a twin link");
    }
    Some(status)
}

async fn refresh_status(c: &Ctx, client: &Client, node: &str) {
    fetch_status(c, client, node).await;
}

// ====================================================================== 事件任务

async fn events(
    c: Weak<Ctx>,
    client: Client,
    mut rx: tokio::sync::broadcast::Receiver<ClientEvent>,
    gate: Arc<tokio::sync::Mutex<()>>,
) {
    // 订阅之前就已经握好手的节点（启动之后立刻 `NodeReady` 的可能先于订阅）
    for n in client.nodes() {
        if client.node_status(&n.id) == NodeStatus::Ready {
            let Some(c) = c.upgrade() else { return };
            on_ready(&c, &client, &n.id, &gate).await;
        }
    }
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(RecvError::Lagged(_)) => {
                // 丢了一些事件：对在线的节点重新同步一遍
                let Some(c) = c.upgrade() else { return };
                for n in client.nodes() {
                    if client.node_status(&n.id) == NodeStatus::Ready {
                        on_ready(&c, &client, &n.id, &gate).await;
                    }
                }
                continue;
            }
            Err(RecvError::Closed) => return,
        };
        let Some(c) = c.upgrade() else { return };
        match event {
            ClientEvent::NodeReady { node_id } => on_ready(&c, &client, &node_id, &gate).await,
            ClientEvent::NodeOffline { .. } | ClientEvent::HubConnected | ClientEvent::HubDisconnected => {
                crate::twin::announce_links(&c).await;
            }
            ClientEvent::Notification { node_id, notification } => {
                on_notification(&c, &client, &node_id, &notification.method, notification.params, &gate).await;
            }
        }
    }
}

async fn on_ready(c: &Arc<Ctx>, client: &Client, node: &str, gate: &tokio::sync::Mutex<()>) {
    c.twin_links.stall_reset_node(node);
    refresh_status(c, client, node).await;
    let rows = c.store.todo_remotes_on(node).await.unwrap_or_default();
    for row in rows {
        let _gate = gate.lock().await;
        sync_remote(c, client, &row).await;
    }
    crate::twin::announce_links(c).await;
}

async fn on_notification(
    c: &Arc<Ctx>,
    client: &Client,
    node: &str,
    method: &str,
    params: Value,
    gate: &tokio::sync::Mutex<()>,
) {
    match method {
        // 这一项的「经过」多了一条：是结果或者有人接手了就去同步（同步读 B 的 `todo.history`，通知本身不当作数据）
        methods::TODO_EVENT => {
            let Ok(n) = serde_json::from_value::<TodoEventNotice>(params) else { return };
            if !matches!(n.event.data, TodoEventData::Result { .. } | TodoEventData::TakenOver { .. }) {
                return;
            }
            for row in c.store.todo_remote_by_remote(node, &n.todo_id).await.unwrap_or_default() {
                let _gate = gate.lock().await;
                sync_remote(c, client, &row).await;
            }
        }
        methods::TODO_CHANGED => {
            let Ok(n) = serde_json::from_value::<TodoChanged>(params) else { return };
            let Some(todo) = n.todo.filter(|_| !n.deleted) else { return };
            for row in c.store.todo_remote_by_remote(node, &n.id).await.unwrap_or_default() {
                store_activity(c, &row, todo.activity).await;
            }
        }
        _ => {}
    }
}

/// 存 B 上这一项最近一次的 `activity`；变了就让 A 的这一项重新算 `activity`。
async fn store_activity(c: &Ctx, row: &RemoteRow, activity: Option<TodoActivity>) {
    match c.store.todo_remote_set_activity(&row.todo_id, &row.node_id, &row.remote_todo_id, activity).await {
        Ok(true) => c.sessions.todo_signal().mark_dirty(&row.todo_id),
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, "cannot save the activity of a remote todo"),
    }
}

/// 同步一个远程项：读 B 上这一项现在的 `activity`，再把 `fetched_seq` 之后的 `result` 按顺序记进 A 这一项的「经过」。
/// 调用方持 `sync_gate`。B 不可达或回了错误就放弃这一次，下一次重连或通知时再来。
async fn sync_remote(c: &Arc<Ctx>, client: &Client, row: &RemoteRow) {
    if let Ok(value) = client
        .request(&row.node_id, methods::TODO_GET, TodoIdParams { id: row.remote_todo_id.clone() }, SYNC_TIMEOUT)
        .await
    {
        if let Ok(t) = serde_json::from_value::<Todo>(value) {
            store_activity(c, row, t.activity).await;
        }
    }
    // 进度以库里的为准（调用方给的行可能是刚派发时的快照）
    let fetched = match c.store.todo_remote_rows(&row.todo_id).await {
        Ok(rows) => match rows.into_iter().find(|r| r.node_id == row.node_id && r.remote_todo_id == row.remote_todo_id)
        {
            Some(r) => r.fetched_seq,
            None => return,
        },
        Err(_) => return,
    };
    let mut newer: Vec<TodoEvent> = Vec::new();
    let mut before = None;
    for _ in 0..HISTORY_PAGES {
        let params = TodoHistoryParams { id: row.remote_todo_id.clone(), before_seq: before, limit: Some(200) };
        let Ok(value) = client.request(&row.node_id, methods::TODO_HISTORY, params, SYNC_TIMEOUT).await else { return };
        let Ok(page) = serde_json::from_value::<TodoHistoryResult>(value) else { return };
        let mut reached = false;
        for ev in page.events {
            if ev.seq <= fetched {
                reached = true;
                break;
            }
            before = Some(ev.seq);
            newer.push(ev);
        }
        if reached || !page.has_more {
            break;
        }
    }
    newer.sort_by_key(|e| e.seq);
    let mut wake = false;
    for ev in &newer {
        let recorded = match &ev.data {
            TodoEventData::Result { .. } => record_result(c, row, ev).await,
            TodoEventData::TakenOver { by, .. } => record_taken_over(c, row, ev, by).await.map(|_| false),
            _ => continue,
        };
        match recorded {
            Ok(recorded) => wake |= recorded,
            Err(e) => {
                tracing::warn!(error = %e.message, "cannot record a remote event");
                return;
            }
        }
    }
    if let Some(last) = newer.last() {
        if let Ok(mut conn) = c.store.pool().acquire().await {
            let _ = crate::store::twin_link::set_fetched_seq(
                &mut conn,
                &row.todo_id,
                &row.node_id,
                &row.remote_todo_id,
                last.seq,
            )
            .await;
        }
    }
    if wake {
        // 像本机会话的结果一样：A 的分身再看一眼这一项（只在它还交给分身、没关闭时）
        let twin_item = matches!(
            c.store.todo_get(&row.todo_id).await,
            Ok(Some(t)) if t.handler == TodoHandler::Twin && !t.state.is_closed()
        );
        if twin_item {
            let _ = c.twin.trigger(c, TwinTrigger::Session).await;
        }
        c.sessions.todo_signal().mark_dirty(&row.todo_id);
    }
}

/// 把 B 上的一条 `result` 记成 A 这一项的 `remote_result`，同一个事务里推进 `fetched_seq`。不是 `result` 的事件什么也不记。
/// 返回有没有新记一条。
async fn record_result(c: &Ctx, row: &RemoteRow, ev: &TodoEvent) -> Result<bool> {
    let TodoEventData::Result { session_id, text, .. } = &ev.data else { return Ok(false) };
    let _guard = c.store.todo_lock().lock().await;
    // A 上这一项没了：不记
    if c.store.todo_get(&row.todo_id).await.map_err(internal)?.is_none() {
        return Ok(false);
    }
    let data = TodoEventData::RemoteResult {
        node_id: row.node_id.clone(),
        session_id: session_id.clone(),
        text: truncate_utf8(text, RESULT_MAX_BYTES).0,
    };
    let mut tx = c.store.pool().begin().await.map_err(internal)?;
    let event = crate::todo_dispatch::event_append_on(&mut tx, &row.todo_id, c.judge.clock().now(), data)
        .await
        .map_err(internal)?;
    crate::store::twin_link::set_fetched_seq(&mut tx, &row.todo_id, &row.node_id, &row.remote_todo_id, ev.seq)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    crate::todo_dispatch::broadcast_event(&c.clients, &row.todo_id, event);
    Ok(true)
}

/// B 上有人在这个远程项的会话里接手了：A 这一项还交给分身，就照本机的「你接手了」处理（`twin::take_over_on`，和本机共用），
/// 同一个事务里推进 `fetched_seq`。`handler` 已经不是 `twin` 的（人在 A 上已经接手或收回了）只推进进度、不记；这一项后来又派了
/// 新的远程项时，旧远程项上迟到的接手同样不记：它说的不是现在这一次。
async fn record_taken_over(c: &Ctx, row: &RemoteRow, ev: &TodoEvent, by: &TodoActor) -> Result<()> {
    let _guard = c.store.todo_lock().lock().await;
    let Some(t) = c.store.todo_get(&row.todo_id).await.map_err(internal)? else { return Ok(()) };
    let latest = c.store.todo_remotes(&row.todo_id).await.map_err(internal)?.pop();
    let current = latest.is_some_and(|r| r.node_id == row.node_id && r.todo_id == row.remote_todo_id);
    if t.handler != TodoHandler::Twin || !current {
        return Ok(());
    }
    let mut tx = c.store.pool().begin().await.map_err(internal)?;
    let (t, events) =
        crate::twin::take_over_on(&mut tx, t, by.clone(), Some(row.node_id.clone()), c.judge.clock().now()).await?;
    crate::store::twin_link::set_fetched_seq(&mut tx, &row.todo_id, &row.node_id, &row.remote_todo_id, ev.seq)
        .await
        .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    let detail = json!({ "node_id": row.node_id, "remote_todo_id": row.remote_todo_id, "by": by });
    crate::twin::take_over_done(c, t, events, &Caller::twin("local"), detail).await
}

/// 远程派发成功之后在 A 上记下来：`Todo.remote`（只追加）、`remote_dispatched`、这一项的 `updated_ms`。同一个事务。
async fn record_dispatch(c: &Ctx, run_id: &str, todo_id: &str, node: &str, rid: &str, session_id: &str) -> Result<()> {
    let _guard = c.store.todo_lock().lock().await;
    let Some(mut t) = c.store.todo_get(todo_id).await.map_err(internal)? else { return Ok(()) };
    let now = c.judge.clock().now();
    let mut tx = c.store.pool().begin().await.map_err(internal)?;
    crate::store::twin_link::remote_add_on(&mut tx, todo_id, node, rid, now).await.map_err(internal)?;
    t.updated_ms = crate::todos::next_updated_ms(now, t.updated_ms);
    crate::todos::save_todo_on(&mut *tx, &t).await.map_err(internal)?;
    let event = crate::todo_dispatch::event_append_on(
        &mut tx,
        todo_id,
        now,
        TodoEventData::RemoteDispatched {
            node_id: node.to_string(),
            todo_id: rid.to_string(),
            session_id: session_id.to_string(),
        },
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    crate::todo_dispatch::broadcast_event(&c.clients, todo_id, event);
    // 连续失败的计数到此为止：一次成功的派发打断「连续」
    let _ =
        sqlx::query("UPDATE twin_items SET failures = 0 WHERE todo_id = ?").bind(todo_id).execute(c.store.pool()).await;
    let t = c.store.todo_get(todo_id).await.map_err(internal)?.unwrap_or(t);
    crate::twin::announce_todo(c, t).await;
    c.audit
        .record(
            &Caller::twin("local"),
            "twin.remote_dispatch",
            Some(todo_id),
            json!({ "run_id": run_id, "node_id": node, "remote_todo_id": rid }),
        )
        .await;
    Ok(())
}

#[cfg(test)]
mod tests;
