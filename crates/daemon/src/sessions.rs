//! 会话管理：状态机、并发与排队、turn 合并、权限、idle 超时与 resume（docs/spec/daemon.md § 6–9）。
//!
//! 并发模型：
//! - 每个会话一把 `tokio::sync::Mutex<St>`：seq 编号、落库、广播都在锁内完成，所以事件顺序 = seq 顺序 = 广播顺序；
//! - 每个活着的 agent 进程由一个 *driver 任务* 独占 `AgentHandle`：它 `select!` 在 `handle.events()` 和命令通道上，
//!   事件回调 `Sessions::on_agent_event`（拿会话锁），命令由持锁方 **fire-and-forget** 投递——持锁时绝不等 driver 的回复
//!   （`set_settings` 例外，带 3 秒超时）；
//! - 全局并发 / 排队在 `Sched`（std Mutex，不跨 await）；释放名额后用 `kick_pump` 在新任务里启动队首，避免两个会话锁互等；
//! - driver 有「代」（`driver_gen`）：被 detach 的旧 driver 的迟到事件按代号丢弃；`Exited` 是终态，driver 收到后就收尾，
//!   `on_exited` 也会推进代号，所以一次退出只会被处理一次；
//! - 旧进程的关闭是异步的（`St::closing`）：下一次拉起前先等它关完，同一个 agent 会话不会同时有两个进程。
//!
//! **锁顺序**：`Workspace` 的命名空间锁（`ns_lock`，link / unlink / create / rename / promote_scratch…）在前，会话锁（`St`）在后。
//! 持有会话锁时**绝不**调用要取 `ns_lock` 的 `Workspace` 方法（`unlink` 持着 `ns_lock` 去逐个会话锁查「有没有运行中的会话」，
//! 反过来就是死锁）。`promote` 因此是三步：会话锁里检查并标 `promoting`、释放锁后改名（取 `ns_lock`）、再加锁写回。
//!
//! **worktree（ADR-34）沿用同一个三步写法**：`git` 操作（建 / 删 worktree、同步 base、查状态…）一律不在会话锁里跑——会话锁里只
//! 做检查、标 `wt_busy`（挡住并发的 `send` / 归档 / 同步）、读写 `St.wt`，释放锁后再跑 git，最后重新加锁写回结果并发事件。
//! 因此不会持着会话锁去取 `ns_lock`（`git` 子进程不碰 `ns_lock`；项目解析 `project_path` / `project_loc` 不取 `ns_lock`）。
//! worktree 相关的实现在子模块 `wt`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anycon_agent_api::{
    AdapterError, AgentEvent, AgentHandle, PermissionDecision, ResumeHandle, SettingsApply, SpawnRequest, UserInput,
};
use anycon_protocol::{
    methods, AgentStatus, AttachmentRef, ErrorCode, Event, ModeRisk, PermissionKind, PermissionListParams,
    PermissionListResult, PermissionOptionKind, PermissionRequest, PermissionResolved, PermissionRespondParams,
    RpcError, SearchHit, SessionChanged, SessionCounts, SessionCreateParams, SessionEvent, SessionHistoryParams,
    SessionHistoryResult, SessionListParams, SessionListResult, SessionPromoteParams, SessionSearchParams,
    SessionSendParams, SessionSendResult, SessionSetSettingsParams, SessionSetSettingsResult, SessionSettings,
    SessionStatus, SessionSummary, SessionWorkspace, TurnDoneReason, UsageTotals, WorktreeState,
};
use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex};

use crate::agents::Agents;
use crate::audit::Audit;
use crate::blobs::{attachment_label, Attached, Blobs, MAX_ATTACHMENTS_PER_MESSAGE};
use crate::clients::{Caller, Clients};
use crate::config::ConfigStore;
use crate::env::DaemonEnv;
use crate::limits;
use crate::notify::Notifier;
use crate::store::{SessionRow, Store, WorktreeRow};
use crate::util::{fmt_utc, new_session_id, now_ms, preview_of, truncate_title, warn_err};
use crate::workspace::Workspace;
use crate::worktree::{session_worktree, WorktreeMgr};

mod wt;

use wt::archived_error;
pub(crate) use wt::{COUNT_BYTES_MAX, COUNT_TIME_MAX};

type RpcResult<T> = Result<T, RpcError>;

fn err(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

/// `starting` 超过这么久没有任何事件 → `agent_start_timeout`（从 turn 开始算，拉起进程卡住也包含在内）。
const START_TIMEOUT: Duration = Duration::from_secs(60);
/// `cancel` 之后 adapter 这么久还没结束 turn → 强制收尾并重启进程。
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// `release_project` 最多等被取消的 turn 收尾多久。
const RELEASE_DEADLINE: Duration = Duration::from_secs(20);
/// 后台任务都结束后，等 agent 自己接着跑一轮等多久；没等到就补发推迟的 `turn_finished`（daemon.md § 14）。
/// Claude Code 实测在最后一个任务结束后几十毫秒内就开始那一轮。
const FINISH_NOTIFY_GRACE: Duration = Duration::from_secs(15);
const SWEEP_INTERVAL: Duration = Duration::from_millis(500);
/// 每扫描这么多次做一次「turn 进行中的关联项目复核」：500 ms × 4 = 2 秒。
const PROJECT_RECHECK_EVERY: u32 = 4;
/// 每扫描这么多次做一轮「PR 合并 / 关闭后自动归档」：500 ms × 1200 = 10 分钟。
const PR_ROUND_EVERY: u32 = 1200;
/// `permission.respond` 因为关联项目的目录变了而返回 `not_found` 时，`data.reason` 的值（协议文档 § 5.5）。
pub const PROJECT_DIR_CHANGED: &str = "project_dir_changed";
const CHANGED_MIN_INTERVAL: Duration = Duration::from_secs(1);
const SET_SETTINGS_TIMEOUT: Duration = Duration::from_secs(3);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(8);
const FTS_FLUSH_BYTES: usize = 32 * 1024;
const RESOLVED_MEMORY: usize = 2048;

// ---------------------------------------------------------------- 内部类型

enum DriverCmd {
    /// `retry`：这条是**插话**（`input.interject = true`）时带上原消息；插话失败（任何错误）daemon 把它放回队列，
    /// 不算 turn 失败（daemon.md § 7.5）。开新 turn 的 `Send` 没有它，失败照旧让 turn 失败。
    Send {
        input: UserInput,
        retry: Option<Backlog>,
    },
    Respond {
        request_id: String,
        decision: PermissionDecision,
    },
    Cancel,
    SetSettings {
        settings: SessionSettings,
        reply: oneshot::Sender<anycon_agent_api::Result<SettingsApply>>,
    },
    Shutdown {
        reply: oneshot::Sender<Option<ResumeHandle>>,
    },
}

struct Driver {
    tx: mpsc::UnboundedSender<DriverCmd>,
}

/// 一个正在关闭的旧进程；输出是它关闭前报告的 resume 信息。可以被多处等待。
type ClosingFut = Shared<BoxFuture<'static, Option<ResumeHandle>>>;

/// 批准 `mode_change` / `plan` 之后切到的模式（审计用）。
#[derive(Debug, Clone)]
pub struct ModeSwitch {
    pub mode: String,
    /// 按当前配置和 AgentInfo 判断的风险。
    pub dangerous: bool,
}

#[derive(Debug, Clone)]
struct Backlog {
    message_id: String,
    content: String,
    ts_ms: u64,
    /// 会话内的到达顺序。插话失败退回队列时按它插回原来的位置（同一毫秒内的消息 `ts_ms` 分不出先后）。
    ord: u64,
    /// 这条消息的附件（已核对并绑定到会话，ADR-35）；合并成一个 turn 时按消息顺序拼接，插话失败退回队列时跟着消息走。
    attachments: Vec<Attached>,
}

struct TurnRt {
    turn_id: u64,
    /// 输入已交给 driver。
    dispatched: bool,
    /// turn 开始（拿到名额）的时刻：`starting` 超时从这里算，覆盖拉起进程的时间。
    started_at: Instant,
    /// 已收到 dispatch 之后的第一个 agent 事件。
    first_event: bool,
    cancel_requested_at: Option<Instant>,
    /// 正在为这个 turn 建 / 重建 worktree（拉起 agent 之前）：`starting` 的 60 秒超时先不算，建好之后重新计时（daemon.md § 10.1）。
    preparing: bool,
    /// agent 自己开始的 turn（adapter 的 `TurnStarted`，没有用户消息，daemon.md § 7.5）。
    by_agent: bool,
}

struct PendingPerm {
    req: PermissionRequest,
    expires_at: Option<Instant>,
}

struct St {
    row: SessionRow,
    driver: Option<Driver>,
    driver_gen: u64,
    holds_slot: bool,
    queued: bool,
    turn: Option<TurnRt>,
    backlog: Vec<Backlog>,
    /// 下一条消息的 `Backlog.ord`。
    next_ord: u64,
    /// 已经交给 driver、还没有结果的插话条数。大于 0 时后来的消息只入队、不开新 turn：
    /// 否则插话失败退回的消息会排在它后面，agent 看到的顺序和用户发的相反。
    interjecting: u32,
    /// 进程拉起时项目目录的 canonical 路径（没有项目为 `None`）。复用活着的进程开新 turn 之前核对它没变。
    spawn_dir: Option<PathBuf>,
    /// 正在升格成项目（`promote` 的第二步，不持会话锁）：这段时间 `send` 返回 `busy`。
    promoting: bool,
    perms: Vec<PendingPerm>,
    needs_restart: bool,
    idle_since: Option<Instant>,
    /// 进入 `ended` 的时间（回合数 / 预算用完后进程可能还活着，要按 `idle_timeout` 回收）。
    ended_since: Option<Instant>,
    idle_notified: bool,
    watched_during_idle: bool,
    /// agent 进程里还在跑的后台任务数（adapter 的 `Background`）。大于 0 时不按 `idle_timeout` 回收进程，
    /// 也不为改设置重启进程——那样会连同后台任务一起杀掉（daemon.md § 7.3）。进程摘掉时清零。
    background_tasks: u32,
    /// 后台任务数最近一次降到 0 的时刻（推迟的 `turn_finished` 通知按它兜底补发）。
    background_drained_at: Option<Instant>,
    /// turn 结束时后台任务还在跑：`turn_finished` 推迟到它们都结束之后的那个 turn 结束时再发（daemon.md § 14）。
    finish_notify_pending: bool,
    text_buf: String,
    text_buf_seq: u64,
    last_position: Option<u32>,
    deleted: bool,
    /// 进入 safe（plan）模式之前的模式，plan 批准后切回去用。
    pre_plan_mode: Option<String>,
    /// 本 turn 里 adapter 已经报告过 `agent_exited`（避免 `Exited` 处理时重复发 error）。
    saw_exit_error: bool,
    /// 正在关闭的旧进程；下一次拉起前要等它关完。
    closing: Option<ClosingFut>,
    /// 每个工具调用的 `tool_output_delta` 转发预算（64 KiB 累计）。
    delta_budgets: HashMap<String, limits::DeltaBudget>,
    /// 会话的 worktree（`worktrees` 表那一行的内存副本；改它就要写透到库里）。
    wt: Option<WorktreeRow>,
    /// 正在跑的 worktree 操作（git 不在会话锁里跑，靠它挡住并发的 `send` / 归档 / 同步）。
    wt_busy: Option<WtBusy>,
    /// 请求创建 worktree 的设备（建好之后记审计 `worktree.create`）。
    wt_caller: Option<Caller>,
    /// 删除会话时拉起流程正在建 / 重建 worktree：建完之后按这个删除请求处置它（`run_create_worktree`）。
    deleted_with: Option<(Caller, anycon_protocol::SessionDeleteParams)>,
}

/// 会话上正在进行的 worktree 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WtBusy {
    /// 拉起流程里在建 / 重建 worktree（turn 在跑，`send` 照常入队）。
    Creating,
    /// `session.sync_base`：`send` 返回 `busy`。
    Syncing,
    /// 归档 / 取消归档 / 删除时处置 worktree：`send` 返回 `busy`。
    Archiving,
    /// 项目改名期间（`workspace.rename`）：目录已经挪了、worktree 的 `.git` 指针还没 `git worktree repair`，不能让周期检查 / `send`
    /// 在这个窗口里看到半截状态：`send` 返回 `busy`，周期检查跳过。
    Renaming,
}

impl St {
    fn new(row: SessionRow) -> St {
        St {
            row,
            driver: None,
            driver_gen: 0,
            holds_slot: false,
            queued: false,
            turn: None,
            backlog: vec![],
            next_ord: 0,
            interjecting: 0,
            spawn_dir: None,
            promoting: false,
            perms: vec![],
            needs_restart: false,
            idle_since: None,
            ended_since: None,
            idle_notified: false,
            watched_during_idle: false,
            background_tasks: 0,
            background_drained_at: None,
            finish_notify_pending: false,
            text_buf: String::new(),
            text_buf_seq: 0,
            last_position: None,
            deleted: false,
            pre_plan_mode: None,
            saw_exit_error: false,
            closing: None,
            delta_budgets: HashMap::new(),
            wt: None,
            wt_busy: None,
            wt_caller: None,
            deleted_with: None,
        }
    }
}

pub struct Session {
    id: String,
    st: AsyncMutex<St>,
}

#[derive(Default)]
struct Sched {
    running: usize,
    queue: VecDeque<String>,
}

enum Acquire {
    Granted,
    Queued(u32),
}

impl Sched {
    fn acquire(&mut self, id: &str, max: usize) -> Acquire {
        if let Some(p) = self.queue.iter().position(|x| x == id) {
            return Acquire::Queued(p as u32 + 1);
        }
        if self.running < max && self.queue.is_empty() {
            self.running += 1;
            Acquire::Granted
        } else {
            self.queue.push_back(id.to_string());
            Acquire::Queued(self.queue.len() as u32)
        }
    }
    fn release(&mut self) {
        self.running = self.running.saturating_sub(1);
    }
    fn remove(&mut self, id: &str) -> bool {
        let before = self.queue.len();
        self.queue.retain(|x| x != id);
        before != self.queue.len()
    }
    fn position(&self, id: &str) -> Option<u32> {
        self.queue.iter().position(|x| x == id).map(|p| p as u32 + 1)
    }
}

#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    latest: Option<SessionChanged>,
    timer: bool,
}

#[derive(Default)]
struct ResolvedSet {
    order: VecDeque<String>,
    set: HashSet<String>,
}

impl ResolvedSet {
    fn insert(&mut self, id: &str) {
        if self.set.insert(id.to_string()) {
            self.order.push_back(id.to_string());
            if self.order.len() > RESOLVED_MEMORY {
                if let Some(old) = self.order.pop_front() {
                    self.set.remove(&old);
                }
            }
        }
    }
    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
}

pub struct SessionDeps {
    pub store: Store,
    pub clients: Arc<Clients>,
    pub agents: Arc<Agents>,
    pub workspace: Arc<Workspace>,
    pub config: Arc<ConfigStore>,
    pub env: Arc<DaemonEnv>,
    pub notifier: Arc<Notifier>,
    pub worktrees: Arc<WorktreeMgr>,
    pub audit: Audit,
    pub blobs: Arc<Blobs>,
}

pub struct Sessions {
    d: SessionDeps,
    map: RwLock<HashMap<String, Arc<Session>>>,
    sched: Mutex<Sched>,
    perm_index: Mutex<HashMap<String, String>>,
    /// 正在收尾的项目（小写项目名 → 运行期间又收到的通知数）：`release_project_soon` 合并重复通知。
    releasing: Mutex<HashMap<String, u32>>,
    resolved: Mutex<ResolvedSet>,
    throttle: Mutex<HashMap<String, Throttle>>,
    /// 带 prompt 的 `session.create` 串行化，配合 `message_id` 做幂等。
    create_lock: AsyncMutex<()>,
    /// `starting` 超时（毫秒）；默认 [`START_TIMEOUT`]，测试里可以缩短。
    start_timeout_ms: std::sync::atomic::AtomicU64,
    /// 推迟的 `turn_finished` 兜底补发前等多久（毫秒）；默认 [`FINISH_NOTIFY_GRACE`]，测试里可以缩短。
    finish_notify_grace_ms: std::sync::atomic::AtomicU64,
    /// 周期复核每隔多少次扫描做一次（默认 [`PROJECT_RECHECK_EVERY`]；测试里可以调大来「关掉」它）。
    recheck_every: std::sync::atomic::AtomicU32,
    /// 每个会话上次查 PR 的时刻（`worktree.auto_archive_on_pr_close` 每轮挑最久没查的 20 个）。
    pr_checked: Mutex<HashMap<String, Instant>>,
    /// 已经有一轮「PR 合并 / 关闭后自动归档」在后台跑（同一时刻只有一轮）。
    pr_round_running: Arc<std::sync::atomic::AtomicBool>,
    /// 已经有一轮每小时的 `cleanup` 在后台跑。
    cleanup_running: Arc<std::sync::atomic::AtomicBool>,
    /// 每扫描这么多次起一轮 PR 自动归档（默认 [`PR_ROUND_EVERY`]；测试里可以调小）。
    pr_round_every: std::sync::atomic::AtomicU32,
    /// 每个会话当前 agent 进程的 `(driver 代号, pid)`（adapter 报告得出的才有；进程退出时清掉）。
    /// 预览按它判断监听端口是不是会话起的（docs/spec/daemon.md § 19.3），见 [`Sessions::preview_targets`]。
    agent_pids: Mutex<HashMap<String, (u64, u32)>>,
}

/// 「同一时刻只有一个」的标记：拿到它的人跑，drop 时放回去（任务 panic 也会放）。
struct RunFlag(Arc<std::sync::atomic::AtomicBool>);

impl RunFlag {
    fn try_take(flag: &Arc<std::sync::atomic::AtomicBool>) -> Option<RunFlag> {
        flag.compare_exchange(false, true, std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst)
            .ok()
            .map(|_| RunFlag(flag.clone()))
    }
}

impl Drop for RunFlag {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 把积压消息合并成一条用户输入（protocol.md § 7.3）：单条原样；多条按顺序拼接，带时间戳和分隔线。
pub fn merge_messages(msgs: &[(String, u64)]) -> String {
    if msgs.len() == 1 {
        return msgs[0].0.clone();
    }
    msgs.iter().map(|(c, ts)| format!("[{} UTC] {}", fmt_utc(*ts), c)).collect::<Vec<_>>().join("\n---\n")
}

fn reject_option_id(req: &PermissionRequest) -> Option<String> {
    req.options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::RejectOnce)
        .or_else(|| req.options.iter().find(|o| o.kind == PermissionOptionKind::RejectAlways))
        .map(|o| o.id.clone())
}

impl Sessions {
    // ============================================================ 构造与恢复

    pub async fn new(d: SessionDeps) -> Arc<Self> {
        let this = Arc::new(Sessions {
            d,
            map: RwLock::new(HashMap::new()),
            sched: Mutex::new(Sched::default()),
            perm_index: Mutex::new(HashMap::new()),
            releasing: Mutex::new(HashMap::new()),
            resolved: Mutex::new(ResolvedSet::default()),
            throttle: Mutex::new(HashMap::new()),
            create_lock: AsyncMutex::new(()),
            start_timeout_ms: std::sync::atomic::AtomicU64::new(START_TIMEOUT.as_millis() as u64),
            finish_notify_grace_ms: std::sync::atomic::AtomicU64::new(FINISH_NOTIFY_GRACE.as_millis() as u64),
            recheck_every: std::sync::atomic::AtomicU32::new(PROJECT_RECHECK_EVERY),
            pr_checked: Mutex::new(HashMap::new()),
            pr_round_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cleanup_running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pr_round_every: std::sync::atomic::AtomicU32::new(PR_ROUND_EVERY),
            agent_pids: Mutex::new(HashMap::new()),
        });
        this.load().await;
        this
    }

    async fn load(self: &Arc<Self>) {
        let rows = match self.d.store.load_sessions().await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "failed to load sessions");
                return;
            }
        };
        warn_err("close open turns", self.d.store.turns_close_open(now_ms()).await);
        for mut row in rows {
            let max_seq = match self.d.store.max_seq(&row.id).await {
                Ok(n) => n,
                Err(e) => {
                    tracing::warn!(session = %row.id, error = %e, "cannot read the last event seq");
                    0
                }
            };
            row.last_seq = row.last_seq.max(max_seq);
            // 以前（NFC 规范化）存下的项目名和盘上那一项的写法不同（NFD 的文件夹）：改成盘上的写法，
            // 这样客户端按 `workspace.list` 给的名字过滤会话能对上
            if let Some(p) = row.project.clone() {
                let on_disk = self.d.workspace.canonical_project_name(&p);
                if on_disk != p && self.d.workspace.same_project_name(&on_disk, &p) {
                    row.project = Some(on_disk);
                    warn_err("normalize the project name", self.d.store.upsert_session(&row).await);
                }
            }
            let was_active = matches!(
                row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            );
            let id = row.id.clone();
            let wt = match &row.worktree_id {
                Some(wid) => self.d.store.get_worktree(wid).await.unwrap_or_else(|e| {
                    tracing::warn!(session = %row.id, error = %e, "cannot load the worktree record of a session");
                    None
                }),
                None => None,
            };
            let mut wt = wt;
            // 旧记录（规范化 base 之前创建的短名字）：能规范的就规范；规范不了的读的时候按 `base_spec` 解释
            if let (Some(w), Some(project)) = (wt.as_mut(), row.project.as_deref()) {
                if w.state != WorktreeState::Failed && self.d.worktrees.available() {
                    if let Ok(dir) = self.d.workspace.project_path(project) {
                        if let Some(base) = self.d.worktrees.normalized_base_of(&dir, w).await {
                            w.base_display = crate::worktree::display_base(&base);
                            w.base = base;
                            warn_err("normalize the base of a worktree record", self.d.store.upsert_worktree(w).await);
                        }
                    }
                }
            }
            let mut st = St::new(row);
            st.wt = wt;
            if was_active {
                // 孤儿会话：把没有答复的权限请求在历史里收尾，状态改为 interrupted
                for rid in self.d.store.unresolved_permissions(&id).await.unwrap_or_default() {
                    self.emit(
                        &mut st,
                        Event::PermissionResolved { request_id: rid, option_id: None, by_device: "daemon".into() },
                    )
                    .await;
                }
                self.set_status(&mut st, SessionStatus::Interrupted, None).await;
            }
            // 注意：重启后不给已经 idle 的会话设 `idle_since`——否则 `session_idle_after` 之后会对所有会话（含已归档）批量发通知
            self.map.write().unwrap().insert(id.clone(), Arc::new(Session { id, st: AsyncMutex::new(st) }));
        }
    }

    fn get(&self, id: &str) -> RpcResult<Arc<Session>> {
        self.map
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| err(ErrorCode::NotFound, format!("session {id} not found")))
    }

    fn all(&self) -> Vec<Arc<Session>> {
        self.map.read().unwrap().values().cloned().collect()
    }

    // ============================================================ 事件与状态

    /// 分配 seq、落库、广播给 watcher。调用方持有会话锁。
    async fn emit(&self, st: &mut St, mut ev: Event) -> SessionEvent {
        // 工具的输入输出可能很大：存储和发送都限制在 64 KiB 内（前 48 KiB + 后 16 KiB）
        limits::limit_event(&mut ev);
        let turn_id = match &ev {
            Event::UserMessage { .. }
            | Event::Status { .. }
            | Event::SettingsChanged { .. }
            | Event::TitleChanged { .. } => None,
            _ => st.turn.as_ref().map(|t| t.turn_id),
        };
        st.row.last_seq += 1;
        let se =
            SessionEvent { session_id: st.row.id.clone(), seq: st.row.last_seq, ts_ms: now_ms(), turn_id, event: ev };
        if let Err(e) = self.d.store.append_event(&se).await {
            tracing::error!(session = %st.row.id, seq = se.seq, error = %e, "failed to persist event");
        }
        st.row.updated_ms = se.ts_ms;
        self.d.clients.to_watchers(&st.row.id, methods::SESSION_EVENT, &se);
        se
    }

    async fn persist(&self, st: &St) {
        if let Err(e) = self.d.store.upsert_session(&st.row).await {
            tracing::error!(session = %st.row.id, error = %e, "failed to persist session row");
        }
    }

    fn changed_payload(&self, st: &St) -> SessionChanged {
        SessionChanged {
            session_id: st.row.id.clone(),
            title: st.row.title.clone(),
            status: st.row.status,
            archived: st.row.archived,
            pending_permissions: st.perms.len() as u32,
            background_tasks: st.background_tasks,
            updated_at_ms: st.row.updated_ms,
            last_seq: st.row.last_seq,
            deleted: false,
            worktree: st.wt.as_ref().map(session_worktree),
        }
    }

    /// `session.changed`：发给所有在线客户端，每会话至多 1 次 / 秒（前沿立即发，窗口内的变化合并成后沿一次）。
    fn push_changed(self: &Arc<Self>, c: SessionChanged, immediate: bool) {
        let send_now = {
            let mut t = self.throttle.lock().unwrap();
            let e = t.entry(c.session_id.clone()).or_default();
            let due = e.last.map(|l| CHANGED_MIN_INTERVAL.saturating_sub(l.elapsed())).unwrap_or(Duration::ZERO);
            if immediate || due.is_zero() {
                e.last = Some(Instant::now());
                e.latest = None;
                true
            } else {
                e.latest = Some(c.clone());
                if !e.timer {
                    e.timer = true;
                    let this = self.clone();
                    let sid = c.session_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(due).await;
                        let latest = {
                            let mut t = this.throttle.lock().unwrap();
                            t.get_mut(&sid).and_then(|e| {
                                e.timer = false;
                                e.last = Some(Instant::now());
                                e.latest.take()
                            })
                        };
                        if let Some(l) = latest {
                            this.d.clients.broadcast(methods::SESSION_CHANGED, l);
                        }
                    });
                }
                false
            }
        };
        if send_now {
            self.d.clients.broadcast(methods::SESSION_CHANGED, c);
        }
    }

    fn notify_changed(self: &Arc<Self>, st: &St) {
        self.push_changed(self.changed_payload(st), false);
    }

    async fn set_status(self: &Arc<Self>, st: &mut St, new: SessionStatus, position: Option<u32>) {
        if st.row.status == new && st.last_position == position {
            return;
        }
        let was_idle = st.row.status == SessionStatus::Idle;
        st.row.status = new;
        st.last_position = position;
        // 只有 turn 已经交给 agent 之后积压的消息才算「待发送」；还没 dispatch 的会并入当前 turn
        let dispatched = st.turn.as_ref().is_some_and(|t| t.dispatched);
        let queued_messages = if dispatched && !st.backlog.is_empty() { Some(st.backlog.len() as u32) } else { None };
        self.emit(st, Event::Status { state: new, position, queued_messages }).await;
        if new == SessionStatus::Idle && !was_idle {
            st.idle_since = Some(Instant::now());
            st.idle_notified = false;
            // 一直在看这个会话的客户端也算「看过」（spec § 14）
            st.watched_during_idle = self.d.clients.has_watchers(&st.row.id);
        }
        if new != SessionStatus::Idle {
            st.idle_since = None;
        }
        if new == SessionStatus::Ended {
            st.ended_since.get_or_insert_with(Instant::now);
        } else {
            st.ended_since = None;
        }
        self.persist(st).await;
        self.notify_changed(st);
    }

    async fn flush_text(&self, st: &mut St) {
        if st.text_buf.is_empty() {
            return;
        }
        let text = std::mem::take(&mut st.text_buf);
        warn_err(
            "index assistant text",
            self.d.store.index_text(&st.row.id, st.text_buf_seq, "assistant", now_ms(), &text).await,
        );
    }

    // ============================================================ 创建

    pub async fn create(self: &Arc<Self>, caller: &Caller, p: SessionCreateParams) -> RpcResult<SessionSummary> {
        let agents = &self.d.agents;
        let info = agents
            .info(&p.agent)
            .ok_or_else(|| err(ErrorCode::AgentUnavailable, format!("unknown agent {}", p.agent)))?;
        if info.status != AgentStatus::Ready {
            return Err(unavailable(&info.status, &p.agent));
        }
        let p = SessionCreateParams { prompt: p.prompt.clone().filter(|x| !x.is_empty()), ..p };
        // 第一条消息：有文字、或者只有附件（ADR-35）
        let has_message = p.prompt.is_some() || !p.attachments.is_empty();
        if has_message && p.message_id.as_deref().is_none_or(str::is_empty) {
            return Err(err(ErrorCode::Invalid, "message_id is required when prompt or attachments are given"));
        }
        if p.prompt.as_ref().is_some_and(|t| t.len() > limits::SEND_CONTENT_MAX) {
            return Err(err(ErrorCode::Invalid, "the prompt is too large (at most 1 MiB)"));
        }
        // 带第一条消息的创建按 message_id 幂等：客户端没收到响应重试时，返回第一次创建的会话，不再开第二个
        let _create_guard = if has_message { Some(self.create_lock.lock().await) } else { None };
        if let (true, Some(mid)) = (has_message, p.message_id.as_deref()) {
            let found = self
                .d
                .store
                .find_session_by_message(mid)
                .await
                .map_err(|e| err(ErrorCode::Internal, format!("cannot check message_id: {e}")))?;
            if let Some(sid) = found {
                if let Ok(existing) = self.get(&sid) {
                    let st = existing.st.lock().await;
                    if !st.deleted {
                        // 同一个 message_id 配不同的内容不是重试，是客户端的 bug：别悄悄返回第一个会话
                        let same_project = st.row.project.as_deref()
                            == p.project
                                .as_deref()
                                .filter(|n| !n.is_empty())
                                .map(|n| self.d.workspace.canonical_project_name(n))
                                .as_deref();
                        let first = self
                            .d
                            .store
                            .user_message_first(&st.row.id, mid)
                            .await
                            .map_err(|e| err(ErrorCode::Internal, format!("cannot check message_id: {e}")))?;
                        let given_blobs: Vec<&str> = p.attachments.iter().map(|a| a.blob_id.as_str()).collect();
                        let same_message = first.as_ref().is_some_and(|(content, blob_ids)| {
                            content == p.prompt.as_deref().unwrap_or("")
                                && blob_ids.iter().map(String::as_str).eq(given_blobs.iter().copied())
                        });
                        if st.row.agent != p.agent || !same_project || !same_message {
                            return Err(err(
                                ErrorCode::Invalid,
                                format!("message_id {mid} was already used for a different session.create (another prompt, attachments, agent or project)"),
                            ));
                        }
                        return Ok(self.summary(&st));
                    }
                }
            }
        }
        let settings = agents.apply_defaults(&p.agent, &info, p.settings.clone());
        agents.validate_settings(&p.agent, &info, &settings)?;

        let id = new_session_id();
        // 附件在建会话之前先核对一遍（blob 不存在 / 信息对不上 / agent 不收这种图…）：不能留下一个因为附件不对而没有第一条
        // 消息的空会话。真正的绑定在 `send_locked` 里，和事件一起（它会再核对一次）
        if !p.attachments.is_empty() {
            self.d.blobs.resolve(&id, &p.attachments, &info.capabilities.attachments).await?;
        }
        // 项目：解析（missing / 被换成符号链接 / 规则不允许 → not_found）并记下它现在指向哪里
        let (project, project_loc) = match &p.project {
            Some(name) if !name.is_empty() => {
                self.d.workspace.project_path(name)?;
                let loc = self.d.workspace.project_loc(name)?;
                (Some(self.d.workspace.canonical_project_name(name)), Some(loc))
            }
            _ => {
                self.d.workspace.scratch_dir(&id).map_err(|e| err(ErrorCode::Internal, format!("scratch dir: {e}")))?;
                (None, None)
            }
        };
        // 每会话 worktree（ADR-34）：同步校验和名字分配在返回之前做；git worktree add 等在拉起流程里（finish_begin）
        let wt_plan = match &p.workspace {
            None | Some(SessionWorkspace::Local) => None,
            Some(SessionWorkspace::Worktree { base, branch }) => {
                let Some(name) = project.as_deref() else {
                    return Err(err(ErrorCode::Invalid, "scratch sessions cannot use a worktree"));
                };
                let project_dir = self.d.workspace.project_path(name)?;
                let plan = Box::pin(self.d.worktrees.plan(crate::worktree::PlanInput {
                    session_id: &id,
                    project: name,
                    project_loc: project_loc.clone(),
                    project_dir: &project_dir,
                    base: base.as_deref(),
                    branch: branch.as_deref(),
                    prompt: p.prompt.as_deref(),
                }))
                .await?;
                Some(plan)
            }
        };
        let now = now_ms();
        let row = SessionRow {
            id: id.clone(),
            project,
            project_loc,
            agent: p.agent.clone(),
            title: String::new(),
            title_locked: false,
            status: SessionStatus::Idle,
            archived: false,
            settings: settings.clone(),
            agent_session_id: None,
            launch_args: serde_json::Value::Null,
            created_ms: now,
            updated_ms: now,
            last_seq: 0,
            next_turn_id: 1,
            usage: UsageTotals::default(),
            preview: None,
            worktree_id: wt_plan.as_ref().map(|(w, _)| w.id.clone()),
        };
        let mut st = St::new(row);
        st.idle_since = Some(Instant::now());
        // 会话行和 worktrees 行一起写库（state = creating）；分配锁到这里才放，并发的创建看得到这个名字
        let _alloc_guard = match wt_plan {
            Some((wt, guard)) => {
                self.d
                    .store
                    .insert_session_with_worktree(&st.row, &wt)
                    .await
                    .map_err(|e| err(ErrorCode::Internal, format!("cannot record the session: {e}")))?;
                st.wt = Some(wt);
                st.wt_caller = Some(caller.clone());
                Some(guard)
            }
            None => {
                self.persist(&st).await;
                None
            }
        };
        self.emit(&mut st, Event::SettingsChanged { settings }).await;
        let sess = Arc::new(Session { id: id.clone(), st: AsyncMutex::new(st) });
        // 先持锁插入 map，再释放：别的调用在 create 完成前看不到半成品
        let mut guard = sess.st.lock().await;
        self.map.write().unwrap().insert(id.clone(), sess.clone());
        self.push_changed(self.changed_payload(&guard), true);
        if has_message {
            let message_id = p.message_id.clone().unwrap_or_default();
            self.send_locked(
                &sess,
                &mut guard,
                caller,
                message_id,
                p.prompt.clone().unwrap_or_default(),
                p.attachments,
            )
            .await?;
        }
        Ok(self.summary(&guard))
    }

    fn summary(&self, st: &St) -> SessionSummary {
        SessionSummary {
            id: st.row.id.clone(),
            project: st.row.project.clone(),
            agent: st.row.agent.clone(),
            title: st.row.title.clone(),
            status: st.row.status,
            archived: st.row.archived,
            settings: st.row.settings.clone(),
            created_at_ms: st.row.created_ms,
            updated_at_ms: st.row.updated_ms,
            last_seq: st.row.last_seq,
            pending_permissions: st.perms.len() as u32,
            background_tasks: st.background_tasks,
            queue_position: if st.row.status == SessionStatus::Queued {
                self.sched.lock().unwrap().position(&st.row.id)
            } else {
                None
            },
            usage: st.row.usage.clone(),
            preview: st.row.preview.clone(),
            agent_session_id: st.row.agent_session_id.clone(),
            worktree: st.wt.as_ref().map(session_worktree),
        }
    }

    pub async fn get_summary(&self, id: &str) -> RpcResult<SessionSummary> {
        let s = self.get(id)?;
        let st = s.st.lock().await;
        Ok(self.summary(&st))
    }

    pub async fn list(&self, p: SessionListParams) -> SessionListResult {
        let mut out = Vec::new();
        for s in self.all() {
            let st = s.st.lock().await;
            if st.deleted || (st.row.archived && !p.include_archived) {
                continue;
            }
            if let Some(proj) = &p.project {
                // 项目名按规范化的键比较（NFC / NFD、不区分大小写的卷上忽略大小写）
                if !st.row.project.as_deref().is_some_and(|x| self.d.workspace.same_project_name(x, proj)) {
                    continue;
                }
            }
            out.push(self.summary(&st));
        }
        out.sort_by(|a, b| b.updated_at_ms.cmp(&a.updated_at_ms).then(a.id.cmp(&b.id)));
        if let Some(limit) = p.limit {
            out.truncate(limit as usize);
        }
        SessionListResult { sessions: out }
    }

    // ============================================================ 发送与 turn

    pub async fn send(self: &Arc<Self>, caller: &Caller, p: SessionSendParams) -> RpcResult<SessionSendResult> {
        if p.message_id.is_empty() {
            return Err(err(ErrorCode::Invalid, "message_id is required"));
        }
        if p.content.len() > limits::SEND_CONTENT_MAX {
            return Err(err(ErrorCode::Invalid, "the message is too large (at most 1 MiB)"));
        }
        let sess = self.get(&p.session_id)?;
        let mut st = sess.st.lock().await;
        if st.deleted {
            return Err(err(ErrorCode::NotFound, "session was deleted"));
        }
        self.send_locked(&sess, &mut st, caller, p.message_id, p.content, p.attachments).await
    }

    async fn send_locked(
        self: &Arc<Self>,
        sess: &Arc<Session>,
        st: &mut St,
        caller: &Caller,
        message_id: String,
        content: String,
        attachments: Vec<AttachmentRef>,
    ) -> RpcResult<SessionSendResult> {
        // 幂等：同一 message_id 之前收到过。查询本身出错时不能当成新消息（会重复执行），直接报错让客户端重试
        let seen = self
            .d
            .store
            .find_user_message(&st.row.id, &message_id)
            .await
            .map_err(|e| err(ErrorCode::Internal, format!("cannot check message_id: {e}")))?;
        if seen.is_some() {
            // 同一个 message_id 配不同的内容（或不同的附件，按顺序比 `blob_id`）不是重试（ADR-35）
            let first = self
                .d
                .store
                .user_message_first(&st.row.id, &message_id)
                .await
                .map_err(|e| err(ErrorCode::Internal, format!("cannot check message_id: {e}")))?;
            if first.is_some_and(|(first_content, first_blobs)| {
                first_content != content
                    || !first_blobs.iter().map(String::as_str).eq(attachments.iter().map(|a| a.blob_id.as_str()))
            }) {
                return Err(err(
                    ErrorCode::Invalid,
                    format!("message_id {message_id} was already used for a different message in this session"),
                ));
            }
            let turn_id = match self.d.store.turn_of_message(&st.row.id, &message_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(session = %st.row.id, error = %e, "cannot look up the turn of a duplicate message");
                    None
                }
            };
            let queued = st.backlog.iter().any(|b| b.message_id == message_id);
            return Ok(SessionSendResult { turn_id, queued, duplicate: true });
        }
        if content.is_empty() && attachments.is_empty() {
            return Err(err(ErrorCode::Invalid, "content is empty"));
        }
        // 已归档的会话不能直接发消息（所有会话，ADR-34 修订）：归档时 worktree 可能已经删掉或保留着未提交的改动，
        // 归档状态下跑 turn 会绕开目录复核。先 `unarchive`
        if st.row.archived {
            return Err(archived_error());
        }
        if st.promoting {
            return Err(err(ErrorCode::Busy, "the session is being promoted to a project; try again in a moment"));
        }
        if matches!(st.wt_busy, Some(WtBusy::Syncing | WtBusy::Archiving | WtBusy::Renaming)) {
            return Err(err(
                ErrorCode::Busy,
                "the session's worktree is being synchronized, archived, restored or its project renamed; try again in a moment",
            ));
        }
        let turn_active = st.turn.is_some();
        // 没有活着的进程时要拉起：先检查 agent 现在能不能用
        if !turn_active && st.driver.is_none() {
            let info =
                self.d.agents.info(&st.row.agent).ok_or_else(|| err(ErrorCode::AgentUnavailable, "agent not found"))?;
            if info.status != AgentStatus::Ready {
                return Err(unavailable(&info.status, &st.row.agent));
            }
            if matches!(st.row.status, SessionStatus::Ended | SessionStatus::Interrupted) && !info.capabilities.resume {
                if st.row.status == SessionStatus::Interrupted {
                    self.set_status(st, SessionStatus::Ended, None).await;
                }
                return Err(err(ErrorCode::Unsupported, "session has ended and the agent cannot resume it"));
            }
        }
        // 附件：逐个核对（存在、信息和记录一致、没被别的会话引用、该 agent 收这种图），然后在写事件之前一次性绑定到会话——
        // 校验或绑定失败时什么都还没发生（ADR-35）
        let attached: Vec<Attached> = if attachments.is_empty() {
            vec![]
        } else {
            let allowed = self
                .d
                .agents
                .info(&st.row.agent)
                .ok_or_else(|| err(ErrorCode::AgentUnavailable, "agent not found"))?
                .capabilities
                .attachments
                .clone();
            let rows = self.d.blobs.resolve(&st.row.id, &attachments, &allowed).await?;
            self.d.blobs.bind(&st.row.id, &rows).await?;
            rows.iter().map(|r| self.d.blobs.attached(r)).collect()
        };
        // 事件里写 daemon 的记录（含宽高），不是客户端给的
        let refs: Vec<AttachmentRef> = attached.iter().map(|a| a.reference.clone()).collect();
        // 用户消息进事件流
        self.emit(
            st,
            Event::UserMessage {
                message_id: message_id.clone(),
                content: content.clone(),
                by_device: caller.device_id.clone(),
                attachments: refs.clone(),
            },
        )
        .await;
        // 全文索引只收文字；没有文字（只有图片）不索引
        warn_err(
            "index user text",
            self.d.store.index_text(&st.row.id, st.row.last_seq, "user", now_ms(), &content).await,
        );
        // 没有文字时，摘要和标题用附件的标签（`[image] 文件名`）
        let shown = if content.trim().is_empty() { attachment_label(&refs) } else { content.clone() };
        st.row.preview = Some(preview_of(&shown, 120));
        if st.row.title.is_empty() && !st.row.title_locked {
            let title = truncate_title(&shown, 60);
            if !title.is_empty() {
                st.row.title = title.clone();
                self.emit(st, Event::TitleChanged { title }).await;
            }
        }
        // 终态的会话再发消息 = 要重新跑起来
        st.idle_since = None;
        let ts = now_ms();

        // turn 进行中
        if let Some(turn) = &st.turn {
            let interject = self.d.agents.info(&st.row.agent).is_some_and(|i| i.capabilities.interject);
            // 取消已经发出、turn 还没收尾时不插话（插进一个正在被中断的 turn，消息可能丢）；
            // 有一条插话还没有结果时也只入队（顺序）
            let dispatched = turn.dispatched;
            let steerable = dispatched && turn.cancel_requested_at.is_none() && st.interjecting == 0;
            if interject && steerable {
                let turn_id = turn.turn_id;
                // 插话之前复核项目目录（ADR-33 第 5 条）：目录变了就不送出，消息退回队列，turn 以 not_found 结束、进程收掉
                if st.driver.is_some() {
                    // 项目目录（关联项目）和 worktree 目录都要复核；worktree 出问题只收这个会话的进程
                    let problem = match self.recheck_running_turn(st) {
                        Some(e) => Some((e, true)),
                        None => self.check_worktree(st).await.map(|e| (e, false)),
                    };
                    if let Some((e, whole_project)) = problem {
                        let msg = self.new_backlog(st, message_id, content, ts, attached);
                        st.backlog.push(msg);
                        if whole_project {
                            self.abort_turn_for_project(st, &e).await;
                        } else {
                            self.fail_turn_with(st, &stable_code_str(&e), e.message.clone(), true).await;
                        }
                        self.persist(st).await;
                        return Ok(SessionSendResult { turn_id: None, queued: true, duplicate: false });
                    }
                }
                if st.driver.is_some() {
                    // 插话：交给 adapter 送进当前 turn。失败（turn 刚好结束、或任何别的错误）不让 turn 失败，
                    // driver 报告后消息退回队列，作为下一个 turn 发出（`on_interject_failed`）
                    let msg = self.new_backlog(st, message_id, content, ts, attached);
                    // 插话同样带附件（ADR-35）；失败退回队列时附件跟着消息走（`retry` 里的 `Backlog`）
                    let input = UserInput {
                        message_id: msg.message_id.clone(),
                        content: msg.content.clone(),
                        attachments: msg.attachments.iter().map(|a| a.reference.clone()).collect(),
                        attachment_paths: msg.attachments.iter().map(|a| a.path.clone()).collect(),
                        interject: true,
                    };
                    st.interjecting += 1;
                    if let Some(d) = &st.driver {
                        let _ = d.tx.send(DriverCmd::Send { input, retry: Some(msg) });
                    }
                    return Ok(SessionSendResult { turn_id: Some(turn_id), queued: false, duplicate: false });
                }
            }
            let msg = self.new_backlog(st, message_id, content, ts, attached);
            st.backlog.push(msg);
            if dispatched {
                let (state, n) = (st.row.status, st.backlog.len() as u32);
                self.emit(st, Event::Status { state, position: None, queued_messages: Some(n) }).await;
            }
            self.notify_changed(st);
            self.persist(st).await;
            return Ok(SessionSendResult { turn_id: None, queued: true, duplicate: false });
        }

        let msg = self.new_backlog(st, message_id, content, ts, attached);
        st.backlog.push(msg);
        self.persist(st).await;
        self.try_begin(sess, st).await;
        Ok(SessionSendResult {
            turn_id: st.turn.as_ref().map(|t| t.turn_id),
            queued: st.turn.is_none(),
            duplicate: false,
        })
    }

    fn new_backlog(
        &self,
        st: &mut St,
        message_id: String,
        content: String,
        ts_ms: u64,
        attachments: Vec<Attached>,
    ) -> Backlog {
        st.next_ord += 1;
        Backlog { message_id, content, ts_ms, ord: st.next_ord, attachments }
    }

    /// backlog 非空且没有 turn 在跑时尝试开始一个 turn：先拿并发名额，拿不到就排队。
    async fn try_begin(self: &Arc<Self>, sess: &Arc<Session>, st: &mut St) {
        // 已归档的会话不开新 turn：排队的消息不会在归档之后作为新 turn 发出（`send` 本来就拒绝，这里是兜底）
        if st.turn.is_some() || st.backlog.is_empty() || st.deleted || st.interjecting > 0 || st.row.archived {
            return;
        }
        if !st.holds_slot {
            let max = self.d.config.get().sessions.max_concurrent;
            let acq = self.sched.lock().unwrap().acquire(&st.row.id, max);
            match acq {
                Acquire::Granted => {
                    st.holds_slot = true;
                    st.queued = false;
                }
                Acquire::Queued(pos) => {
                    st.queued = true;
                    self.set_status(st, SessionStatus::Queued, Some(pos)).await;
                    return;
                }
            }
        }
        self.begin(sess, st).await;
    }

    /// 已持有名额：开 turn。有活着的进程就直接投递；否则（或需要重启时）在后台任务里拉起再投递。
    /// 投递 / 拉起之前先用**当前**配置把设置完整复核一遍（配置可以热改：危险模式、会话 env 开关、模型表都可能变了）。
    async fn begin(self: &Arc<Self>, sess: &Arc<Session>, st: &mut St) {
        let turn_id = st.row.next_turn_id;
        st.row.next_turn_id += 1;
        let first_msg = st.backlog.first().map(|b| b.message_id.clone()).unwrap_or_default();
        st.saw_exit_error = false;
        st.delta_budgets.clear();
        // 用户又发消息了：之前推迟的「turn 完成」通知不再补发，这个 turn 结束时按当时的后台任务重新判断
        st.finish_notify_pending = false;
        // worktree 还没建好 / 不见了的会话：拉起流程里先建（重建）它，期间 `starting` 的超时不算
        let wt_ready = st.wt.as_ref().is_none_or(|w| w.state == WorktreeState::Active);
        st.turn = Some(TurnRt {
            turn_id,
            dispatched: false,
            started_at: Instant::now(),
            first_event: false,
            cancel_requested_at: None,
            preparing: !wt_ready,
            by_agent: false,
        });
        self.set_status(st, SessionStatus::Starting, None).await;
        self.emit(st, Event::TurnStart { turn_id, message_id: first_msg.clone() }).await;
        warn_err("record turn start", self.d.store.turn_start(&st.row.id, turn_id, &first_msg, now_ms()).await);
        if let Err(e) = self.recheck_settings(st) {
            st.backlog.clear();
            self.fail_turn_with(st, &stable_code_str(&e), e.message.clone(), true).await;
            return;
        }
        // 后台任务在跑时不为改设置重启进程（重启会连同后台任务一起杀掉）：沿用当前进程，需要重启才生效的设置
        // 推迟到后台任务都结束之后的 turn（`needs_restart` 留着）
        let restart = st.needs_restart && st.background_tasks == 0;
        if st.needs_restart && !restart {
            tracing::info!(session = %st.row.id, background_tasks = st.background_tasks, "deferring the agent restart for new settings until its background tasks finish");
        }
        if st.driver.is_some() && !restart && wt_ready {
            // ADR-33 第 5 条：复用活着的进程之前也复核项目目录——空闲进程上的会话不能在目录被换掉之后照样收消息
            if let Some(e) = self.recheck_live_project(st) {
                let project = st.row.project.clone();
                // 项目现在解析不了：消息留在队列里（不清掉），目录恢复之后的下一个 turn 一起发出；`session.cancel` 才清空
                self.fail_turn_with(st, &stable_code_str(&e), e.message.clone(), true).await;
                if let Some(p) = project {
                    self.release_project_soon(p);
                }
                return;
            }
            // worktree 目录同样每个 turn 开始前复核（ADR-34 第 9 条）：不见了 / 被换掉就停下，不在别的地方续接
            if let Some(e) = self.check_worktree(st).await {
                self.fail_turn_with(st, &stable_code_str(&e), e.message.clone(), true).await;
                return;
            }
            let left = self.dispatch(st);
            self.announce_leftover(st, left).await;
        } else {
            if let Some(d) = self.detach(st) {
                self.retire(st, d);
            }
            st.needs_restart = false;
            let this = self.clone();
            let sess = sess.clone();
            tokio::spawn(async move { this.finish_begin(sess, turn_id).await });
        }
    }

    /// 有项目的会话在复用活着的进程开新 turn 之前复核：项目现在还能解析、还是创建会话时的那个位置、而且和进程拉起时的目录一样。
    /// 不满足返回错误（`not_found`）。没有项目（scratch）的会话没有要复核的。
    fn recheck_live_project(&self, st: &St) -> Option<RpcError> {
        let project = st.row.project.as_deref()?;
        match self.resolve_session_cwd(project, st.row.project_loc.as_deref()) {
            Err(e) => Some(e),
            Ok((dir, _)) if st.spawn_dir.as_ref().is_some_and(|d| *d != dir) => Some(err(
                ErrorCode::NotFound,
                format!("the directory of project {project} changed since the agent process was started"),
            )),
            Ok(_) => None,
        }
    }

    /// turn 进行中的复核（放行权限请求之前、插话之前、周期检查）：只对关联项目的会话（根目录下的项目不会被这样换掉）。
    fn recheck_running_turn(&self, st: &St) -> Option<RpcError> {
        let project = st.row.project.as_deref()?;
        if !self.d.workspace.is_linked(project) {
            return None;
        }
        self.recheck_live_project(st)
    }

    /// 在新任务里收掉一个项目所有会话的进程（不能在持有会话锁的地方直接做）。同一个项目已经有一轮在跑时只记一笔
    /// 「再来一轮」（`workspace.list` / `fs.*` 每次发现问题都会通知，不能每次都起一个新任务）。
    pub fn release_project_soon(self: &Arc<Self>, project: String) {
        let key = project.to_lowercase();
        {
            let mut running = self.releasing.lock().unwrap();
            if let Some(again) = running.get_mut(&key) {
                *again += 1;
                return;
            }
            running.insert(key.clone(), 0);
        }
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                this.clone().release_project_boxed(project.clone()).await;
                let mut running = this.releasing.lock().unwrap();
                match running.get_mut(&key) {
                    Some(again) if *again > 0 => *again = 0,
                    _ => {
                        running.remove(&key);
                        break;
                    }
                }
            }
        });
    }

    /// 关联项目的会话在 turn **进行中**发现目录变了（放行权限请求之前、插话之前、每 2 秒一次的周期检查，ADR-33 第 5 条）：
    /// turn 以 `error { not_found }` + `turn_done` 结束，agent 进程收掉（待答复的权限请求按拒绝处理），同项目其它会话的进程一并收掉。
    /// 队列里的消息（包括被退回的插话）留着，下次 `send` 时项目还不能用就会再次 `not_found`。
    async fn abort_turn_for_project(self: &Arc<Self>, st: &mut St, e: &RpcError) {
        let project = st.row.project.clone();
        self.fail_turn_with(st, &stable_code_str(e), e.message.clone(), true).await;
        if let Some(p) = project {
            self.release_project_soon(p);
        }
    }

    /// 周期检查（共享的定时任务里每 [`PROJECT_RECHECK_EVERY`] 次扫描一次，约 2 秒）。
    ///
    /// - **关联项目**的会话，进程还活着的：有 turn 在跑的，也包括**没有 turn 但进程还在**（空闲 / 已结束）的。
    ///   目录变了就像放行前的复核一样收掉。
    /// - **有 worktree 的会话**（不只关联项目，ADR-34）：目录不见了 / 被换掉就标 `missing`、收这个会话的进程（有 turn 就以
    ///   `not_found` 结束）；`missing` 的目录自己回来了（身份一致）就恢复成 `active`。
    ///
    /// agent 不经审批自己执行的命令（包括它记住的「本会话都允许」之后的工具调用）在目录被换掉之后，最多还能持续
    /// 「一个检查间隔 + 收进程的时间」（SIGINT 后最多 5 秒宽限再 SIGKILL）；复核通过之后、答复真正送到 agent 之前换目录同样拦不住。
    /// 这是纵深防御，不是隔离。
    async fn recheck_running_projects(self: &Arc<Self>) {
        for sess in self.all() {
            // 正忙的会话下一轮再看；不能在这里等一把长时间被占着的锁
            let Ok(mut st) = tokio::time::timeout(Duration::from_millis(50), sess.st.lock()).await else { continue };
            if st.deleted {
                continue;
            }
            if st.wt.is_some() {
                self.recheck_worktree_periodic(&mut st).await;
            }
            if st.deleted || st.driver.is_none() {
                continue;
            }
            if let Some(e) = self.recheck_running_turn(&st) {
                if st.turn.is_some() {
                    self.abort_turn_for_project(&mut st, &e).await;
                } else if let Some(project) = st.row.project.clone() {
                    // 没有 turn、进程还活着（空闲 / 已结束）：agent 的后台任务仍可能在被换掉的目录里跑，收进程；
                    // 会话保持原状态，下一次 send 时同样的复核会给出 `not_found`
                    tracing::warn!(session = %st.row.id, project = %project, "the directory of a linked project changed under an idle agent process; closing it");
                    self.release_project_soon(project);
                }
            }
        }
    }

    /// 显式装箱，切断 begin → release_project → cancel → end_turn → … 的 auto-trait 递归。
    fn release_project_boxed(self: Arc<Self>, project: String) -> BoxFuture<'static, ()> {
        Box::pin(async move { self.release_project(&project).await })
    }

    /// 用当前配置复核会话设置（危险模式门禁、会话 env 策略、选项值、模型表）。
    fn recheck_settings(&self, st: &St) -> RpcResult<()> {
        let info = self
            .d
            .agents
            .info(&st.row.agent)
            .ok_or_else(|| err(ErrorCode::AgentUnavailable, format!("agent {} is not available", st.row.agent)))?;
        self.d.agents.validate_settings(&st.row.agent, &info, &st.row.settings)
    }

    /// 摘掉当前 driver（代号 +1，旧 driver 的迟到事件会被丢弃）。
    fn detach(&self, st: &mut St) -> Option<Driver> {
        st.driver_gen += 1;
        st.interjecting = 0;
        st.spawn_dir = None;
        // 后台任务和进程同生死；摘掉进程之后也不会再有推迟的「turn 完成」可补（进程是被 daemon 收掉或出错了）
        st.background_tasks = 0;
        st.background_drained_at = None;
        st.finish_notify_pending = false;
        st.driver.take()
    }

    /// 在后台关闭一个被摘掉的 driver，并记在 `st.closing` 上：之后要拉起新进程（resume）的人先等它关完。
    /// 返回的 future 可以被多处等待，输出是旧进程关闭前报告的 resume 信息。
    fn retire_tracked(&self, st: &mut St, d: Driver) -> ClosingFut {
        let prev = st.closing.take();
        let fut: BoxFuture<'static, Option<ResumeHandle>> = Box::pin(async move {
            let earlier = match prev {
                Some(p) => p.await,
                None => None,
            };
            shutdown_driver(d).await.or(earlier)
        });
        let shared = fut.shared();
        // 没人等也要推进到底
        tokio::spawn(shared.clone());
        st.closing = Some(shared.clone());
        shared
    }

    /// [`Sessions::retire_tracked`]，不关心关闭结果。
    fn retire(&self, st: &mut St, d: Driver) {
        drop(self.retire_tracked(st, d));
    }

    /// 把 backlog 合并后交给 driver。合并按消息顺序收：附件累计超过 20 个或 20 MiB（Claude 一次请求的体积有上限）
    /// 就停，剩下的留在队列里，本 turn 结束后自动作为下一个 turn 发出；第一条总是收（单条消息的附件在 `send` 时已经核对过上限）。
    /// 返回留在队列里的消息条数。
    fn dispatch(&self, st: &mut St) -> usize {
        if st.backlog.is_empty() {
            return 0;
        }
        let max_bytes = self.d.blobs.message_bytes_limit();
        let (mut count, mut bytes, mut take) = (0usize, 0u64, 0usize);
        for (i, m) in st.backlog.iter().enumerate() {
            let (n, b) = (m.attachments.len(), m.attachments.iter().map(|a| a.reference.bytes).sum::<u64>());
            if i > 0 && (count + n > MAX_ATTACHMENTS_PER_MESSAGE || bytes + b > max_bytes) {
                break;
            }
            count += n;
            bytes += b;
            take = i + 1;
        }
        let msgs: Vec<Backlog> = st.backlog.drain(..take).collect();
        let pairs: Vec<(String, u64)> = msgs.iter().map(|m| (m.content.clone(), m.ts_ms)).collect();
        // 几条消息合并成一个 turn 时，附件按消息顺序拼接；`attachments` 和 `attachment_paths` 一一对应（ADR-35）
        let attached: Vec<&Attached> = msgs.iter().flat_map(|m| m.attachments.iter()).collect();
        let input = UserInput {
            message_id: msgs[0].message_id.clone(),
            content: merge_messages(&pairs),
            attachments: attached.iter().map(|a| a.reference.clone()).collect(),
            attachment_paths: attached.iter().map(|a| a.path.clone()).collect(),
            interject: false,
        };
        if let Some(t) = st.turn.as_mut() {
            t.dispatched = true;
        }
        if let Some(d) = &st.driver {
            let _ = d.tx.send(DriverCmd::Send { input, retry: None });
        }
        st.backlog.len()
    }

    /// `dispatch` 之后还有消息留在队列里（附件超过了一个 turn 的上限）：告诉客户端有几条在排队。
    async fn announce_leftover(self: &Arc<Self>, st: &mut St, left: usize) {
        if left > 0 {
            let state = st.row.status;
            self.emit(st, Event::Status { state, position: None, queued_messages: Some(left as u32) }).await;
            self.notify_changed(st);
        }
    }

    async fn finish_begin(self: Arc<Self>, sess: Arc<Session>, turn_id: u64) {
        // 旧进程（重启 / idle 超时 / 出错后摘掉的）可能还在关：等它关完再 resume，
        // 同一个 agent 会话不允许两个进程同时跑；顺便拿到它关闭前报告的 resume 信息
        let closing = sess.st.lock().await.closing.take();
        let old_resume = match closing {
            Some(c) => c.await,
            None => None,
        };
        // 取拉起参数（持锁很短），拉起过程中不持锁，期间到达的消息进 backlog，最终一起合并
        let (agent, cwd_project, recorded_loc, scratch_id, settings, resume, gen, has_wt) = {
            let mut st = sess.st.lock().await;
            if st.deleted || st.turn.as_ref().map(|t| t.turn_id) != Some(turn_id) {
                return;
            }
            st.driver_gen += 1;
            if let Some(r) = old_resume {
                adopt_closing_resume(&mut st, r);
            }
            let resume = st
                .row
                .resume_handle()
                .filter(|_| self.d.agents.info(&st.row.agent).is_some_and(|i| i.capabilities.resume));
            // 配置在等待期间可能又变了：拉起前再复核一次
            if let Err(e) = self.recheck_settings(&st) {
                st.backlog.clear();
                self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                return;
            }
            (
                st.row.agent.clone(),
                st.row.project.clone(),
                st.row.project_loc.clone(),
                st.row.id.clone(),
                st.row.settings.clone(),
                resume,
                st.driver_gen,
                st.wt.is_some(),
            )
        };
        // 工作目录：项目每次都重新解析（missing / 被换成符号链接 / 按当前规则不再允许 → not_found），并且要和会话创建时
        // 记下的位置一致：解除关联后这个名字指向了别的目录，旧会话不能在新目录里续接
        let cwd: Result<(PathBuf, Option<String>), RpcError> = match &cwd_project {
            Some(name) => self.resolve_session_cwd(name, recorded_loc.as_deref()),
            None => self
                .d
                .workspace
                .scratch_dir(&scratch_id)
                .map(|d| (d, None))
                .map_err(|e| err(ErrorCode::Internal, e.to_string())),
        };
        // worktree 会话：在项目目录里的 worktree 里工作。先建（重建）它——在 `spawn_driver` 之前，agent 的启动超时之后才开始算；
        // 失败时 `prepare_worktree` 已经收尾了这个 turn
        let wt_dir: Option<PathBuf> = match (&cwd, has_wt) {
            (Ok((project_dir, _)), true) => {
                match Box::pin(self.prepare_worktree(&sess, turn_id, gen, project_dir)).await {
                    Ok(dir) => Some(dir),
                    Err(()) => return,
                }
            }
            _ => None,
        };
        let result: Result<(Driver, Option<String>, PathBuf), Result<AdapterError, RpcError>> = match cwd {
            Err(e) => Err(Err(e)),
            Ok((cwd, backfill)) => {
                let spawn_cwd = wt_dir.clone().unwrap_or_else(|| cwd.clone());
                match self.spawn_driver(&scratch_id, &agent, spawn_cwd, settings, resume, gen).await {
                    Ok(d) => Ok((d, backfill, cwd)),
                    Err(e) => Err(Ok(e)),
                }
            }
        };
        let mut st = sess.st.lock().await;
        let still_current = !st.deleted && st.turn.as_ref().map(|t| t.turn_id) == Some(turn_id) && st.driver_gen == gen;
        // 进程起来之后再复核一次项目目录：解析项目和进程真正启动之间目录被换掉了，就不让它收消息
        let swapped_during_spawn = match (&result, &cwd_project) {
            (Ok((_, backfill, cwd)), Some(name)) => {
                let loc = backfill.as_deref().or(recorded_loc.as_deref());
                match self.resolve_session_cwd(name, loc) {
                    Ok((dir, _)) if dir == *cwd => None,
                    Ok(_) => Some(err(
                        ErrorCode::NotFound,
                        format!("the directory of project {name} changed while the agent process was starting"),
                    )),
                    Err(e) => Some(e),
                }
            }
            _ => None,
        };
        match result {
            Ok((driver, _, _)) if still_current && swapped_during_spawn.is_some() => {
                let e = swapped_during_spawn.expect("checked above");
                self.retire(&mut st, driver);
                self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                if let Some(p) = cwd_project.clone() {
                    self.release_project_soon(p);
                }
            }
            Ok((driver, _, cwd)) if still_current && self.worktree_changed_since(&st, &cwd).is_some() => {
                // 进程起来的这一刻 worktree 目录被换掉 / 删掉了：不让它收消息
                self.retire(&mut st, driver);
                let why = self.worktree_changed_since(&st, &cwd).unwrap_or(wt::MissingWhy::Gone);
                let e = self.mark_worktree_missing(&mut st, why).await;
                self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
            }
            Ok((driver, backfill, cwd)) if still_current => {
                if let Some(loc) = backfill {
                    // 旧会话（迁移之前创建的）第一次拉起、或整个 workspace 根搬了盘：记下它现在的位置
                    st.row.project_loc = Some(loc);
                    self.persist(&st).await;
                }
                st.spawn_dir = cwd_project.is_some().then_some(cwd);
                st.driver = Some(driver);
                let left = self.dispatch(&mut st);
                self.announce_leftover(&mut st, left).await;
            }
            Ok((driver, _, _)) => {
                // 拉起期间 turn 已经结束（取消 / 超时 / 删除）：新进程没人要了
                self.retire(&mut st, driver);
            }
            Err(Ok(e)) if still_current => {
                self.fail_turn(&mut st, e).await;
            }
            Err(Err(e)) if still_current => {
                // 项目解析失败：不拉起，turn 以 `not_found` 之类的稳定错误码收尾，会话进入 error，修好以后可以再发；
                // 没送出去的消息留在队列里，下一个成功的 turn 把它们和新消息按顺序一起发出
                self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
            }
            Err(_) => {}
        }
    }

    /// 会话的工作目录：项目现在的目录，加上要（重新）记下的位置（旧会话还没记过、或整个 workspace 根搬了盘）。
    /// 记过位置而现在指向别处：`not_found`，消息带原来的目录 / 说明目录换了。
    fn resolve_session_cwd(
        &self,
        project: &str,
        recorded: Option<&str>,
    ) -> Result<(PathBuf, Option<String>), RpcError> {
        use crate::workspace::{LocCheck, Workspace};
        let ws = &self.d.workspace;
        let dir = ws.project_path(project)?;
        let now = ws.project_loc(project)?;
        let Some(rec) = recorded else { return Ok((dir, Some(now))) };
        match ws.check_loc(rec, &now) {
            LocCheck::Same => Ok((dir, None)),
            LocCheck::Refresh(loc) => Ok((dir, Some(loc))),
            LocCheck::Different if rec.starts_with("linked:") => Err(err(
                ErrorCode::NotFound,
                format!(
                    "project {project} no longer points to the directory this session was created in ({}); \
                     link that directory again (any name) to continue, or start a new session",
                    Workspace::describe_loc(rec)
                ),
            )),
            LocCheck::Different => Err(err(
                ErrorCode::NotFound,
                format!(
                    "project {project} is now a different directory from the one this session was created in ({} was replaced). \
                     If you replaced it on purpose (for example restored it from a backup or cloned it again), start a new session; \
                     this session's history can still be viewed. Otherwise restore the original folder to continue",
                    Workspace::describe_loc(rec)
                ),
            )),
        }
    }

    async fn spawn_driver(
        self: &Arc<Self>,
        session_id: &str,
        agent: &str,
        cwd: PathBuf,
        settings: SessionSettings,
        resume: Option<ResumeHandle>,
        gen: u64,
    ) -> Result<Driver, AdapterError> {
        let adapter = self
            .d
            .agents
            .adapter(agent)
            .ok_or_else(|| AdapterError::NotInstalled(format!("agent {agent} is not registered")))?;
        let info = self
            .d
            .agents
            .info(agent)
            .ok_or_else(|| AdapterError::NotInstalled(format!("agent {agent} has no probe result")))?;
        let cfg = self.d.config.get();
        let ac = cfg.adapter_config(agent).cloned().unwrap_or_default();
        let env = self.d.env.for_session(&cfg, agent, &settings.env);
        let req = SpawnRequest {
            session_id: session_id.to_string(),
            cwd,
            settings,
            env,
            config: ac,
            resume,
            agent_info: info,
        };
        let handle = adapter.spawn(req).await?;
        if let Some(pid) = handle.pid() {
            self.agent_pids.lock().unwrap().insert(session_id.to_string(), (gen, pid));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let this = self.clone();
        let sid = session_id.to_string();
        tokio::spawn(async move { driver_loop(this, sid, gen, handle, rx).await });
        Ok(Driver { tx })
    }

    /// turn 因为错误无法继续（拉起失败、send 失败）：收尾为 error。
    async fn fail_turn(self: &Arc<Self>, st: &mut St, e: AdapterError) {
        let code = match &e {
            AdapterError::NotInstalled(_) => "agent_not_found",
            AdapterError::NotAuthenticated(_) => "auth_failed",
            AdapterError::UnsupportedVersion(_) => "unsupported_version",
            AdapterError::InvalidSettings { .. } => "invalid_settings",
            AdapterError::Exited(_) => "agent_exited",
            _ => "agent_error",
        };
        if matches!(
            e,
            AdapterError::NotInstalled(_) | AdapterError::NotAuthenticated(_) | AdapterError::UnsupportedVersion(_)
        ) {
            self.d.agents.reprobe_soon(&st.row.agent);
        }
        self.fail_turn_with(st, code, e.to_string(), true).await;
    }

    /// 以一个 `error` 事件收尾当前 turn（会话进入 error 状态，可以修好设置后再 send）。
    async fn fail_turn_with(self: &Arc<Self>, st: &mut St, code: &str, message: String, recoverable: bool) {
        self.emit(st, Event::Error { code: code.into(), message, recoverable }).await;
        if let Some(d) = self.detach(st) {
            self.retire(st, d);
        }
        self.end_turn(st, TurnDoneReason::Error, None, Some(SessionStatus::Error)).await;
        self.d.notifier.error(&st.row.id);
    }

    /// 收尾当前 turn：发 `TurnDone`、记 turn 表和用量、释放名额，然后进入 idle / 下一个 turn / 指定状态。
    async fn end_turn(
        self: &Arc<Self>,
        st: &mut St,
        reason: TurnDoneReason,
        usage: Option<anycon_protocol::Usage>,
        force_status: Option<SessionStatus>,
    ) {
        let Some(turn) = st.turn.as_ref() else { return };
        let turn_id = turn.turn_id;
        let by_agent = turn.by_agent;
        self.deny_all_inner(st, "turn ended", false).await;
        self.flush_text(st).await;
        self.emit(st, Event::TurnDone { turn_id, reason, usage: usage.clone() }).await;
        let reason_s = match reason {
            TurnDoneReason::Completed => "completed",
            TurnDoneReason::Cancelled => "cancelled",
            TurnDoneReason::Error => "error",
            TurnDoneReason::MaxTurns => "max_turns",
            TurnDoneReason::Budget => "budget",
        };
        let usage_json = usage.as_ref().and_then(|u| serde_json::to_value(u).ok());
        warn_err(
            "record turn end",
            self.d.store.turn_end(&st.row.id, turn_id, now_ms(), reason_s, usage_json.as_ref()).await,
        );
        if let Some(u) = &usage {
            let t = &mut st.row.usage;
            t.input_tokens += u.input_tokens.unwrap_or(0);
            t.output_tokens += u.output_tokens.unwrap_or(0);
            t.cache_read_tokens += u.cache_read_tokens.unwrap_or(0);
            t.cache_write_tokens += u.cache_write_tokens.unwrap_or(0);
            if let Some(c) = u.cost_usd {
                t.cost_usd = Some(t.cost_usd.unwrap_or(0.0) + c);
            }
        }
        st.row.usage.turns += 1;
        st.turn = None;
        st.delta_budgets.clear();
        self.release_slot(st);
        // 回合数 / 预算用完 = 正常结束（spec § 7.1），不是 idle；但有积压消息时还是接着跑下一个 turn
        let force_status = force_status.or_else(|| {
            (matches!(reason, TurnDoneReason::MaxTurns | TurnDoneReason::Budget) && st.backlog.is_empty())
                .then_some(SessionStatus::Ended)
        });
        if let Some(s) = force_status {
            self.set_status(st, s, None).await;
        } else if !st.backlog.is_empty() && st.interjecting == 0 {
            // 积压的消息作为下一个 turn（排在其它排队会话之后）；还有插话没有结果时等它的结果（成功 / 失败的回调会接着开）
            // 这里没有 `sess` 句柄：通过 kick 让 pump 处理——把自己放进队列
            self.requeue_for_backlog(st).await;
        } else {
            self.set_status(st, SessionStatus::Idle, None).await;
        }
        if reason == TurnDoneReason::Completed || reason == TurnDoneReason::MaxTurns || reason == TurnDoneReason::Budget
        {
            // 后台任务还在跑：活还没干完，「完成」推迟到它们都结束之后的那个 turn（daemon.md § 14）。agent 自己开的 turn
            // 只在有推迟的通知时才发——没有的话这个 turn 是已经通知过的 turn 的后续，不再打扰一次
            if st.background_tasks > 0 {
                st.finish_notify_pending = true;
            } else if !by_agent || st.finish_notify_pending {
                st.finish_notify_pending = false;
                self.d.notifier.turn_finished(&st.row.id);
            }
        }
        self.persist(st).await;
    }

    /// 积压消息要开下一个 turn：排到队尾（FIFO 公平），由 pump 在新任务里启动。
    /// 不在这里直接 `begin`：避免 begin → finish_begin → end_turn → begin 的异步递归。
    async fn requeue_for_backlog(self: &Arc<Self>, st: &mut St) {
        let max = self.d.config.get().sessions.max_concurrent;
        let (will_wait, pos) = {
            let mut sc = self.sched.lock().unwrap();
            if !sc.queue.iter().any(|x| x == &st.row.id) {
                sc.queue.push_back(st.row.id.clone());
            }
            (!(sc.running < max && sc.queue.len() == 1), sc.position(&st.row.id).unwrap_or(1))
        };
        st.queued = true;
        if will_wait {
            self.set_status(st, SessionStatus::Queued, Some(pos)).await;
        }
        self.kick_pump();
    }

    fn release_slot(self: &Arc<Self>, st: &mut St) {
        if st.holds_slot {
            st.holds_slot = false;
            self.sched.lock().unwrap().release();
            self.kick_pump();
        }
    }

    fn kick_pump(self: &Arc<Self>) {
        tokio::spawn(self.clone().pump_boxed());
    }

    /// 显式装箱，切断 pump → begin → finish_begin → end_turn → kick_pump → pump 的 auto-trait 递归。
    fn pump_boxed(self: Arc<Self>) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move { self.pump().await })
    }

    /// 有空闲名额就启动队首会话；然后刷新其余排队会话的位置。
    async fn pump(self: &Arc<Self>) {
        loop {
            let next = {
                let max = self.d.config.get().sessions.max_concurrent;
                let mut s = self.sched.lock().unwrap();
                if s.running < max {
                    let id = s.queue.pop_front();
                    if id.is_some() {
                        s.running += 1;
                    }
                    id
                } else {
                    None
                }
            };
            let Some(id) = next else { break };
            let Ok(sess) = self.get(&id) else {
                self.sched.lock().unwrap().release();
                continue;
            };
            let mut st = sess.st.lock().await;
            if st.deleted || !st.queued || st.turn.is_some() {
                // 状态已经变了（取消 / 删除）：名额还回去
                drop(st);
                self.sched.lock().unwrap().release();
                continue;
            }
            st.queued = false;
            st.holds_slot = true;
            if st.backlog.is_empty() {
                st.holds_slot = false;
                self.sched.lock().unwrap().release();
                self.set_status(&mut st, SessionStatus::Idle, None).await;
                continue;
            }
            self.begin(&sess, &mut st).await;
        }
        // 位置变了的排队会话补发 status
        let ids: Vec<(String, u32)> =
            self.sched.lock().unwrap().queue.iter().enumerate().map(|(i, id)| (id.clone(), i as u32 + 1)).collect();
        for (id, pos) in ids {
            if let Ok(sess) = self.get(&id) {
                let mut st = sess.st.lock().await;
                if st.queued && st.row.status == SessionStatus::Queued {
                    self.set_status(&mut st, SessionStatus::Queued, Some(pos)).await;
                }
            }
        }
    }

    // ============================================================ 来自 agent 的事件

    async fn on_agent_event(self: &Arc<Self>, sid: &str, gen: u64, ev: AgentEvent) {
        let Ok(sess) = self.get(sid) else { return };
        let mut st = sess.st.lock().await;
        if st.deleted || st.driver_gen != gen {
            return;
        }
        match ev {
            AgentEvent::Started { resume } => {
                st.row.agent_session_id = Some(resume.agent_session_id);
                st.row.launch_args = resume.launch_args;
                self.persist(&st).await;
            }
            AgentEvent::Title(t) => {
                self.mark_first_event(&mut st).await;
                if !st.row.title_locked && !t.trim().is_empty() && t != st.row.title {
                    st.row.title = t.clone();
                    self.emit(&mut st, Event::TitleChanged { title: t }).await;
                    self.persist(&st).await;
                    self.notify_changed(&st);
                }
            }
            AgentEvent::Permission(p) => {
                self.mark_first_event(&mut st).await;
                self.on_permission(&mut st, p).await;
            }
            AgentEvent::Event(e) => {
                self.mark_first_event(&mut st).await;
                self.on_event(&mut st, e).await;
            }
            AgentEvent::TurnStarted => {
                self.begin_agent_turn(&mut st).await;
            }
            AgentEvent::Background { tasks } => {
                self.on_background(&mut st, tasks).await;
            }
            AgentEvent::Exited { code, error } => {
                self.on_exited(&mut st, code, error).await;
            }
        }
    }

    /// agent 自己开始了一个 turn（Claude Code 的后台任务结束后 CLI 自己跑一轮处理结果，daemon.md § 7.5）：开一个没有
    /// 用户消息的 turn，`turn_start.message_id` 为空串，结束照常是 adapter 的 `TurnDone`。已经有 turn 时忽略——那是
    /// `send` 撞上了这一轮，adapter 把它并进了用户的 turn。不占并发名额：进程已经在跑，daemon 拦不住它（和后台任务一样）。
    /// 已归档的会话不跑 turn（§ 7.1）：不开，这一轮的事件照旧记在 turn 之外。
    async fn begin_agent_turn(self: &Arc<Self>, st: &mut St) {
        if st.turn.is_some() || st.driver.is_none() || st.row.archived {
            return;
        }
        let turn_id = st.row.next_turn_id;
        st.row.next_turn_id += 1;
        st.saw_exit_error = false;
        st.delta_budgets.clear();
        st.turn = Some(TurnRt {
            turn_id,
            dispatched: true,
            started_at: Instant::now(),
            first_event: true,
            cancel_requested_at: None,
            preparing: false,
            by_agent: true,
        });
        self.set_status(st, SessionStatus::Running, None).await;
        self.emit(st, Event::TurnStart { turn_id, message_id: String::new() }).await;
        warn_err("record turn start", self.d.store.turn_start(&st.row.id, turn_id, "", now_ms()).await);
        self.persist(st).await;
    }

    /// adapter 报告进程里还在跑的后台任务数（daemon.md § 7.3）。降到 0 时会话才算真正空闲：空闲的会话从这时起
    /// 重新计 `idle_timeout` 和 `session_idle`，`ended` 的同理。
    async fn on_background(self: &Arc<Self>, st: &mut St, tasks: u32) {
        if st.background_tasks == tasks {
            return;
        }
        st.background_tasks = tasks;
        if tasks == 0 {
            let now = Instant::now();
            st.background_drained_at = Some(now);
            match st.row.status {
                SessionStatus::Idle => {
                    st.idle_since = Some(now);
                    st.idle_notified = false;
                    st.watched_during_idle = self.d.clients.has_watchers(&st.row.id);
                }
                SessionStatus::Ended => st.ended_since = Some(now),
                _ => {}
            }
        } else {
            st.background_drained_at = None;
        }
        self.notify_changed(st);
    }

    /// dispatch 之后的第一个 agent 事件：starting → running。
    async fn mark_first_event(self: &Arc<Self>, st: &mut St) {
        let Some(t) = st.turn.as_mut() else { return };
        if t.dispatched && !t.first_event {
            t.first_event = true;
            if st.row.status == SessionStatus::Starting {
                self.set_status(st, SessionStatus::Running, None).await;
            }
        }
    }

    async fn on_event(self: &Arc<Self>, st: &mut St, e: Event) {
        match e {
            // 这些由 daemon 生成，adapter 不该发
            Event::UserMessage { .. }
            | Event::TurnStart { .. }
            | Event::Status { .. }
            | Event::Permission { .. }
            | Event::PermissionResolved { .. }
            | Event::SettingsChanged { .. }
            | Event::TitleChanged { .. } => {
                tracing::warn!(session = %st.row.id, "adapter emitted a daemon-owned event type; dropped");
            }
            Event::TurnDone { reason, usage, .. } => {
                if st.turn.is_some() {
                    self.end_turn(st, reason, usage, None).await;
                    self.notify_changed(st);
                }
            }
            Event::TextDelta { text } => {
                if st.text_buf.is_empty() {
                    st.text_buf_seq = st.row.last_seq + 1;
                }
                st.text_buf.push_str(&text);
                let flush = st.text_buf.len() >= FTS_FLUSH_BYTES;
                self.emit(st, Event::TextDelta { text }).await;
                if flush {
                    self.flush_text(st).await;
                }
            }
            Event::TextDone {} => {
                self.flush_text(st).await;
                self.emit(st, Event::TextDone {}).await;
            }
            // 每个工具调用累计转发不超过 64 KiB：超出的丢弃，只补一条截断提示；最终结果在 `ToolResult` 里保留头尾
            Event::ToolOutputDelta { id, text } => {
                let verdict = st.delta_budgets.entry(id.clone()).or_default().offer(&text);
                match verdict {
                    limits::DeltaVerdict::Forward(t) => {
                        self.emit(st, Event::ToolOutputDelta { id, text: t }).await;
                    }
                    limits::DeltaVerdict::ForwardLast(t) => {
                        if !t.is_empty() {
                            self.emit(st, Event::ToolOutputDelta { id: id.clone(), text: t }).await;
                        }
                        self.emit(st, Event::ToolOutputDelta { id, text: limits::DELTA_CUT_NOTICE.to_string() }).await;
                    }
                    limits::DeltaVerdict::Drop => {}
                }
            }
            Event::ToolResult { id, output, is_error, mut detail } => {
                // 这次调用的流式输出因为超限停止过转发：在最终结果里结构化地标出来（`output` 本身仍是头尾截断的完整输出）
                if st.delta_budgets.remove(&id).is_some_and(|b| b.is_cut()) {
                    detail.get_or_insert_with(Default::default).live_output_truncated = Some(true);
                }
                self.emit(st, Event::ToolResult { id, output, is_error, detail }).await;
            }
            Event::Error { code, message, recoverable } => {
                if code == "agent_exited" {
                    st.saw_exit_error = true;
                }
                self.emit(st, Event::Error { code, message, recoverable }).await;
                if !recoverable {
                    self.d.notifier.error(&st.row.id);
                }
            }
            other => {
                self.emit(st, other).await;
            }
        }
    }

    async fn on_permission(self: &Arc<Self>, st: &mut St, p: anycon_agent_api::PendingPermission) {
        let cfg_timeout = self.d.config.get().request_timeout();
        let timeout = match (p.expires_in, cfg_timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let created = now_ms();
        let expires_at_ms = timeout.map(|d| created + d.as_millis() as u64);
        self.emit(
            st,
            Event::Permission {
                request_id: p.request_id.clone(),
                kind: p.kind,
                title: p.title.clone(),
                detail: p.detail.clone(),
                options: p.options.clone(),
                expires_at_ms,
            },
        )
        .await;
        let req = PermissionRequest {
            session_id: st.row.id.clone(),
            request_id: p.request_id.clone(),
            kind: p.kind,
            title: p.title,
            detail: p.detail,
            options: p.options,
            created_at_ms: created,
            expires_at_ms,
        };
        self.perm_index.lock().unwrap().insert(req.request_id.clone(), st.row.id.clone());
        self.d.clients.broadcast(methods::PERMISSION_REQUEST, &req);
        st.perms.push(PendingPerm { req, expires_at: timeout.map(|d| Instant::now() + d) });
        if matches!(st.row.status, SessionStatus::Running | SessionStatus::Starting) {
            self.set_status(st, SessionStatus::WaitingPermission, None).await;
        } else {
            self.notify_changed(st);
        }
        self.d.notifier.permission_requested(&st.row.id);
    }

    async fn on_exited(self: &Arc<Self>, st: &mut St, code: Option<i32>, error: Option<String>) {
        // 推进代号：这个 driver 之后的任何事件（包括通道关闭时合成的 Exited）都会被丢弃，一次退出只处理一次
        self.detach(st);
        if st.turn.is_some() {
            let failed = error.is_some() || code.is_some_and(|c| c != 0);
            let msg = error.clone().unwrap_or_else(|| format!("agent exited (code {code:?})"));
            if failed {
                if !st.saw_exit_error {
                    self.emit(st, Event::Error { code: "agent_exited".into(), message: msg, recoverable: true }).await;
                }
                self.end_turn(st, TurnDoneReason::Error, None, Some(SessionStatus::Error)).await;
                self.d.notifier.error(&st.row.id);
            } else {
                self.end_turn(st, TurnDoneReason::Completed, None, Some(SessionStatus::Ended)).await;
            }
        } else {
            // 没有 turn 在跑时 agent 自己退出了
            self.deny_all(st, "agent exited").await;
            if error.is_some() || code.is_some_and(|c| c != 0) {
                self.emit(
                    st,
                    Event::Error {
                        code: "agent_exited".into(),
                        message: error.unwrap_or_else(|| format!("agent exited (code {code:?})")),
                        recoverable: true,
                    },
                )
                .await;
                self.set_status(st, SessionStatus::Error, None).await;
                self.d.notifier.error(&st.row.id);
            } else {
                self.set_status(st, SessionStatus::Ended, None).await;
            }
        }
        self.persist(st).await;
    }

    /// driver 报告一次**插话**送进去了（`send` 返回 `Ok`）。如果这期间 turn 已经结束、积压里有消息在等它的结果
    /// （有插话没结果时后来的消息只入队），现在可以接着开下一个 turn 了。
    async fn on_interject_done(self: &Arc<Self>, sid: &str) {
        let Ok(sess) = self.get(sid) else { return };
        let mut st = sess.st.lock().await;
        st.interjecting = st.interjecting.saturating_sub(1);
        if !st.deleted && st.turn.is_none() && st.interjecting == 0 && !st.backlog.is_empty() {
            self.requeue_for_backlog(&mut st).await;
        }
    }

    /// driver 报告一次**插话**没送进去（adapter 返回 `TurnEnded`——turn 刚结束、`turn_done` 还没被读到，或者 turn 正在取消——
    /// 或任何其它错误）。这不是 turn 失败：消息按到达顺序放回队列（排在比它晚到的消息前面），发 `status { queued_messages }`，
    /// 不 retire 进程；turn 已经结束就排队开下一个 turn，没结束就等它结束，只发一次（daemon.md § 7.5）。
    async fn on_interject_failed(self: &Arc<Self>, sid: &str, gen: u64, msg: Backlog, e: AdapterError) {
        let Ok(sess) = self.get(sid) else { return };
        let mut st = sess.st.lock().await;
        st.interjecting = st.interjecting.saturating_sub(1);
        if st.deleted {
            return;
        }
        tracing::debug!(session = %sid, driver_current = st.driver_gen == gen, error = %e, "interjection was not delivered; queueing the message");
        let at = st.backlog.partition_point(|b| b.ord < msg.ord);
        st.backlog.insert(at, msg);
        if st.turn.is_some() {
            let (state, n) = (st.row.status, st.backlog.len() as u32);
            self.emit(&mut st, Event::Status { state, position: None, queued_messages: Some(n) }).await;
            self.notify_changed(&st);
            self.persist(&st).await;
        } else {
            self.persist(&st).await;
            // turn 已经结束（`turn_done` 先于这次失败被处理）：没人会再来取积压，现在就排队开下一个 turn
            // （和 `end_turn` 遇到积压消息时一样走 pump，不在这里直接 begin：driver → begin → driver 的递归）；
            // 还有别的插话没结果就等它们
            if st.interjecting == 0 {
                self.requeue_for_backlog(&mut st).await;
            }
        }
    }

    /// driver 报告 `AgentHandle::send` 失败。
    async fn on_send_failed(self: &Arc<Self>, sid: &str, gen: u64, e: AdapterError) {
        let Ok(sess) = self.get(sid) else { return };
        let mut st = sess.st.lock().await;
        if st.deleted || st.driver_gen != gen || st.turn.is_none() {
            return;
        }
        self.fail_turn(&mut st, e).await;
    }

    // ============================================================ 权限

    /// 自动拒绝全部未决请求（取消、turn 结束、agent 退出、删除）。
    async fn deny_all(self: &Arc<Self>, st: &mut St, message: &str) {
        self.deny_all_inner(st, message, true).await
    }

    async fn deny_all_inner(self: &Arc<Self>, st: &mut St, message: &str, reset_status: bool) {
        let perms = std::mem::take(&mut st.perms);
        for p in perms {
            let opt = reject_option_id(&p.req);
            self.perm_index.lock().unwrap().remove(&p.req.request_id);
            self.resolved.lock().unwrap().insert(&p.req.request_id);
            self.emit(
                st,
                Event::PermissionResolved {
                    request_id: p.req.request_id.clone(),
                    option_id: opt.clone(),
                    by_device: "daemon".into(),
                },
            )
            .await;
            self.d.clients.broadcast(
                methods::PERMISSION_RESOLVED,
                PermissionResolved {
                    session_id: st.row.id.clone(),
                    request_id: p.req.request_id.clone(),
                    option_id: opt.clone(),
                    by_device: "daemon".into(),
                },
            );
            if let Some(d) = &st.driver {
                let _ = d.tx.send(DriverCmd::Respond {
                    request_id: p.req.request_id,
                    decision: PermissionDecision { option_id: opt, message: Some(message.to_string()), answers: None },
                });
            }
        }
        if reset_status && st.row.status == SessionStatus::WaitingPermission && st.turn.is_some() {
            self.set_status(st, SessionStatus::Running, None).await;
        }
    }

    pub async fn respond_permission(
        self: &Arc<Self>,
        caller: &Caller,
        p: PermissionRespondParams,
    ) -> RpcResult<(String, Option<String>, Option<ModeSwitch>)> {
        let sid = self.perm_index.lock().unwrap().get(&p.request_id).cloned();
        let Some(sid) = sid else {
            if self.resolved.lock().unwrap().contains(&p.request_id)
                || self.d.store.permission_resolved(&p.request_id).await.unwrap_or(false)
            {
                return Err(err(ErrorCode::AlreadyResolved, "permission request was already resolved"));
            }
            return Err(err(ErrorCode::NotFound, format!("permission request {} not found", p.request_id)));
        };
        let sess = self.get(&sid)?;
        let mut st = sess.st.lock().await;
        let Some(idx) = st.perms.iter().position(|x| x.req.request_id == p.request_id) else {
            return Err(err(ErrorCode::AlreadyResolved, "permission request was already resolved"));
        };
        let req = st.perms[idx].req.clone();
        // 校验答复
        match req.kind {
            PermissionKind::Question => {
                if p.option_id.is_none() && p.answers.is_none() && p.message.is_none() {
                    return Err(err(ErrorCode::Invalid, "answers are required for a question"));
                }
                if let Some(o) = &p.option_id {
                    if !req.options.iter().any(|x| &x.id == o) {
                        return Err(err(ErrorCode::Invalid, format!("unknown option {o}")));
                    }
                }
            }
            _ => {
                let Some(o) = &p.option_id else { return Err(err(ErrorCode::Invalid, "option_id is required")) };
                if !req.options.iter().any(|x| &x.id == o) {
                    return Err(err(ErrorCode::Invalid, format!("unknown option {o}")));
                }
            }
        }
        // 同意 mode_change / plan 之后要切模式：先在消耗请求之前算好并过门禁，被拒绝就不消耗请求
        let chosen_kind = p.option_id.as_ref().and_then(|o| req.options.iter().find(|x| &x.id == o)).map(|o| o.kind);
        let allow = matches!(chosen_kind, Some(PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways));
        // 放行之前复核项目目录（ADR-33 第 5 条）：目录变了，这个请求改按拒绝答复 adapter，turn 以 not_found 结束，
        // 进程收掉；客户端的 `permission.respond` 得到 `not_found` 错误（请求已经被消耗：事件 `permission_resolved` 带的是拒绝选项）
        if allow {
            let problem = match self.recheck_running_turn(&st) {
                Some(e) => Some((e, true)),
                None => self.check_worktree(&mut st).await.map(|e| (e, false)),
            };
            if let Some((e, whole_project)) = problem {
                let deny = reject_option_id(&req);
                st.perms.remove(idx);
                self.perm_index.lock().unwrap().remove(&p.request_id);
                self.resolved.lock().unwrap().insert(&p.request_id);
                self.emit(
                    &mut st,
                    Event::PermissionResolved {
                        request_id: p.request_id.clone(),
                        option_id: deny.clone(),
                        by_device: "daemon".into(),
                    },
                )
                .await;
                self.d.clients.broadcast(
                    methods::PERMISSION_RESOLVED,
                    PermissionResolved {
                        session_id: sid.clone(),
                        request_id: p.request_id.clone(),
                        option_id: deny.clone(),
                        by_device: "daemon".into(),
                    },
                );
                if let Some(d) = &st.driver {
                    let _ = d.tx.send(DriverCmd::Respond {
                        request_id: p.request_id.clone(),
                        decision: PermissionDecision {
                            option_id: deny,
                            message: Some(e.message.clone()),
                            answers: None,
                        },
                    });
                }
                let mut e = e;
                if whole_project {
                    self.abort_turn_for_project(&mut st, &e).await;
                    // 带上原因，客户端据此区分「项目目录变了」和别的 `not_found`（未知的请求、找不到会话）
                    if let Some(d) = e.data.as_mut() {
                        d.reason = Some(PROJECT_DIR_CHANGED.into());
                    }
                } else {
                    // worktree 不见了 / 被换掉：只收这个会话（`reason = worktree_missing` 已经在错误里）
                    self.fail_turn_with(&mut st, &stable_code_str(&e), e.message.clone(), true).await;
                }
                return Err(e);
            }
        }
        let mode_switch = if !allow {
            None
        } else {
            match req.kind {
                PermissionKind::ModeChange => req.detail.mode.clone(),
                PermissionKind::Plan => self.plan_target_mode(&st, &req),
                _ => None,
            }
        };
        let mut new_settings = None;
        if let Some(mode) = &mode_switch {
            if let Some(info) = self.d.agents.info(&st.row.agent) {
                self.d.agents.validate_settings(
                    &st.row.agent,
                    &info,
                    &SessionSettings { mode: Some(mode.clone()), ..Default::default() },
                )?;
                if st.row.settings.mode.as_deref() != Some(mode.as_str()) {
                    let mut s = st.row.settings.clone();
                    s.mode = Some(mode.clone());
                    new_settings = Some(s);
                }
            }
        }
        // 消耗请求：先到先得
        st.perms.remove(idx);
        self.perm_index.lock().unwrap().remove(&p.request_id);
        self.resolved.lock().unwrap().insert(&p.request_id);
        self.emit(
            &mut st,
            Event::PermissionResolved {
                request_id: p.request_id.clone(),
                option_id: p.option_id.clone(),
                by_device: caller.device_id.clone(),
            },
        )
        .await;
        self.d.clients.broadcast(
            methods::PERMISSION_RESOLVED,
            PermissionResolved {
                session_id: sid.clone(),
                request_id: p.request_id.clone(),
                option_id: p.option_id.clone(),
                by_device: caller.device_id.clone(),
            },
        );
        if let Some(d) = &st.driver {
            let _ = d.tx.send(DriverCmd::Respond {
                request_id: p.request_id.clone(),
                decision: PermissionDecision {
                    option_id: p.option_id.clone(),
                    message: p.message.clone(),
                    answers: p.answers.clone(),
                },
            });
        }
        let mut switched_to = None;
        if let Some(s) = new_settings {
            let mode = s.mode.clone().unwrap_or_default();
            let dangerous = self
                .d
                .agents
                .info(&st.row.agent)
                .is_some_and(|info| self.d.agents.is_dangerous_mode(&st.row.agent, &info, &mode));
            switched_to = Some(ModeSwitch { mode, dangerous });
            st.row.settings = s.clone();
            self.emit(&mut st, Event::SettingsChanged { settings: s.clone() }).await;
            if let Some(d) = &st.driver {
                // 进程不能热改时 adapter 会回 `restart_required`：标记，下一个 turn 前重启
                let (tx, rx) = oneshot::channel();
                if d.tx.send(DriverCmd::SetSettings { settings: s, reply: tx }).is_ok() {
                    let (sess2, gen) = (sess.clone(), st.driver_gen);
                    tokio::spawn(async move {
                        let restart = match tokio::time::timeout(SET_SETTINGS_TIMEOUT, rx).await {
                            Ok(Ok(Ok(apply))) => apply.restart_required,
                            // 失败或超时：保守起见重启
                            _ => true,
                        };
                        if restart {
                            let mut st = sess2.st.lock().await;
                            if st.driver_gen == gen {
                                st.needs_restart = true;
                            }
                        }
                    });
                }
            }
            self.persist(&st).await;
        }
        if st.perms.is_empty() && st.row.status == SessionStatus::WaitingPermission {
            self.set_status(&mut st, SessionStatus::Running, None).await;
        } else {
            self.notify_changed(&st);
        }
        Ok((sid, p.option_id, switched_to))
    }

    /// plan 被批准后会话要切到哪个模式：adapter 在 `detail.mode` 里指明的 > 进入 plan 之前的模式（不是 dangerous 才回去）
    /// > 配置的 `default_mode` > AgentInfo 标 default 的模式。当前模式不是 safe（plan）时不动。
    fn plan_target_mode(&self, st: &St, req: &PermissionRequest) -> Option<String> {
        let info = self.d.agents.info(&st.row.agent)?;
        let risk = |m: &str| info.modes.iter().find(|x| x.id == m).map(|x| x.risk);
        if let Some(m) = &req.detail.mode {
            return Some(m.clone());
        }
        let cur = st.row.settings.mode.as_deref()?;
        if risk(cur) != Some(ModeRisk::Safe) {
            return None;
        }
        // 「危险」要把配置里的 `dangerous_modes` 也算上，不能只看 adapter 标的 risk
        let not_dangerous = |m: &String| {
            risk(m).is_some_and(|r| r != ModeRisk::Safe) && !self.d.agents.is_dangerous_mode(&st.row.agent, &info, m)
        };
        let cfg_default = self.d.config.get().adapter_config(&st.row.agent).and_then(|c| c.default_mode.clone());
        st.pre_plan_mode
            .clone()
            .filter(not_dangerous)
            .or(cfg_default.filter(not_dangerous))
            .or_else(|| info.modes.iter().find(|m| m.default).map(|m| m.id.clone()).filter(not_dangerous))
    }

    pub async fn list_permissions(&self, p: PermissionListParams) -> PermissionListResult {
        let mut requests = Vec::new();
        for s in self.all() {
            if p.session_id.as_deref().is_some_and(|x| x != s.id) {
                continue;
            }
            let st = s.st.lock().await;
            requests.extend(st.perms.iter().map(|x| x.req.clone()));
        }
        requests.sort_by_key(|r| r.created_at_ms);
        PermissionListResult { requests }
    }

    // ============================================================ 取消 / 设置 / 重启

    pub async fn cancel(self: &Arc<Self>, session_id: &str) -> RpcResult<()> {
        let sess = self.get(session_id)?;
        let mut st = sess.st.lock().await;
        match st.row.status {
            SessionStatus::Queued => {
                self.sched.lock().unwrap().remove(&st.row.id);
                st.queued = false;
                st.backlog.clear();
                self.set_status(&mut st, SessionStatus::Idle, None).await;
            }
            SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingPermission => {
                self.deny_all(&mut st, "cancelled").await;
                if st.driver.is_none() {
                    // 还在拉起进程：直接收尾，finish_begin 发现 turn 已变会把新进程关掉
                    st.backlog.clear();
                    self.end_turn(&mut st, TurnDoneReason::Cancelled, None, None).await;
                } else {
                    if let Some(t) = st.turn.as_mut() {
                        t.cancel_requested_at = Some(Instant::now());
                    }
                    if let Some(d) = &st.driver {
                        let _ = d.tx.send(DriverCmd::Cancel);
                    }
                }
            }
            _ => {
                // 没有 turn 在跑：留在队列里等下一个 turn 的消息（比如目录没恢复时被挡下的）由取消清空
                if st.turn.is_none() && !st.backlog.is_empty() {
                    st.backlog.clear();
                    self.persist(&st).await;
                    self.notify_changed(&st);
                }
            }
        }
        Ok(())
    }

    pub async fn set_settings(
        self: &Arc<Self>,
        p: SessionSetSettingsParams,
    ) -> RpcResult<(SessionSetSettingsResult, bool)> {
        let sess = self.get(&p.session_id)?;
        let mut st = sess.st.lock().await;
        if st.turn.is_some() || st.row.status == SessionStatus::Queued {
            return Err(err(ErrorCode::Busy, "a turn is in progress; settings are locked"));
        }
        let info =
            self.d.agents.info(&st.row.agent).ok_or_else(|| err(ErrorCode::AgentUnavailable, "agent not found"))?;
        let (mut merged, _reconciled) = Agents::merge_settings(&info, &st.row.settings, &p.settings, &p.clear);
        // `clear` 掉的 model / thinking / mode 回到默认（和 create 不传这些字段时一致），然后按清除后的完整设置重新校验
        self.d.agents.refill_cleared(&st.row.agent, &info, &mut merged, &p.clear);
        self.d.agents.validate_settings(&st.row.agent, &info, &merged)?;
        let dangerous =
            merged.mode.as_deref().is_some_and(|m| self.d.agents.is_dangerous_mode(&st.row.agent, &info, m))
                && merged.mode != st.row.settings.mode;
        {
            let risk =
                |m: &Option<String>| m.as_deref().and_then(|m| info.modes.iter().find(|x| x.id == m)).map(|x| x.risk);
            let (old_r, new_r) = (risk(&st.row.settings.mode), risk(&merged.mode));
            if new_r == Some(ModeRisk::Safe) && old_r != Some(ModeRisk::Safe) {
                st.pre_plan_mode = st.row.settings.mode.clone();
            } else if new_r != Some(ModeRisk::Safe) {
                st.pre_plan_mode = None;
            }
        }
        if merged == st.row.settings {
            return Ok((SessionSetSettingsResult { settings: merged }, dangerous));
        }
        st.row.settings = merged.clone();
        if st.driver.is_none() {
            // 没有存活进程（idle 关闭 / daemon 重启 / ended）：没有 adapter 会替我们更新 `launch_args`，它是上次进程启动时
            // 的快照，里面可能还有刚被清掉的值。同步改掉，adapter 回落到它也拿不到旧值
            sync_launch_args(&mut st.row.launch_args, &merged);
        }
        self.persist(&st).await;
        self.emit(&mut st, Event::SettingsChanged { settings: merged.clone() }).await;
        if let Some(d) = &st.driver {
            let (tx, rx) = oneshot::channel();
            let _ = d.tx.send(DriverCmd::SetSettings { settings: merged.clone(), reply: tx });
            match tokio::time::timeout(SET_SETTINGS_TIMEOUT, rx).await {
                Ok(Ok(Ok(apply))) => {
                    if apply.restart_required {
                        st.needs_restart = true;
                    }
                }
                // 失败或超时：保守起见下个 turn 前重启进程
                _ => st.needs_restart = true,
            }
        }
        self.notify_changed(&st);
        Ok((SessionSetSettingsResult { settings: merged }, dangerous))
    }

    /// 重启会话的 agent 进程（懒重启：下次 send 才拉起）。
    pub async fn restart(self: &Arc<Self>, session_id: &str) -> RpcResult<()> {
        let sess = self.get(session_id)?;
        let closing = {
            let mut st = sess.st.lock().await;
            self.deny_all(&mut st, "session restarted").await;
            let old = self.detach(&mut st);
            if st.turn.is_some() {
                st.backlog.clear();
                self.end_turn(&mut st, TurnDoneReason::Cancelled, None, None).await;
            } else if st.row.status == SessionStatus::Queued {
                self.sched.lock().unwrap().remove(&st.row.id);
                st.queued = false;
                self.set_status(&mut st, SessionStatus::Idle, None).await;
            } else if matches!(st.row.status, SessionStatus::Error | SessionStatus::Interrupted) {
                self.set_status(&mut st, SessionStatus::Idle, None).await;
            }
            st.needs_restart = false;
            // 关闭记在 `closing` 上：在它关完之前来的 send 会等着，不会同时 resume 出第二个进程
            old.map(|d| self.retire_tracked(&mut st, d))
        };
        if let Some(c) = closing {
            if let Some(r) = c.await {
                let mut st = sess.st.lock().await;
                adopt_closing_resume(&mut st, r);
                self.persist(&st).await;
            }
        }
        Ok(())
    }

    // ============================================================ 其它会话操作

    /// 历史分页。一页按**编码后字节数**封顶（192 KiB，`limits::HISTORY_PAGE_MAX`），截掉的部分留给下一页（`has_more`）；
    /// 单个事件超过上限时单独成页。旧数据库里存着的超大 `tool_result` 在读出时也一并截断。
    pub async fn history(&self, p: SessionHistoryParams) -> RpcResult<SessionHistoryResult> {
        self.get(&p.session_id)?;
        let limit = p.limit.unwrap_or(200).clamp(1, 1000);
        let store = &self.d.store;
        let fetch = limit + 1;
        let io = |e: crate::store::StoreError| err(ErrorCode::Internal, e.to_string());
        let (mut evs, mut has_more, keep_newest) = if let Some(before) = p.before_seq {
            let mut evs = store.events_before(&p.session_id, before, fetch).await.map_err(io)?;
            let has_more = evs.len() as u32 > limit;
            if has_more {
                evs.remove(0);
            }
            (evs, has_more, true)
        } else {
            let mut evs = store.events_after(&p.session_id, p.after_seq, fetch).await.map_err(io)?;
            let has_more = evs.len() as u32 > limit;
            evs.truncate(limit as usize);
            (evs, has_more, false)
        };
        for e in &mut evs {
            limits::limit_event(&mut e.event);
        }
        let (events, cut) = limits::cap_page(std::mem::take(&mut evs), limits::HISTORY_PAGE_MAX, keep_newest);
        has_more |= cut;
        Ok(SessionHistoryResult { events, has_more })
    }

    pub async fn rename(self: &Arc<Self>, session_id: &str, title: String) -> RpcResult<()> {
        let title = title.trim().to_string();
        if title.is_empty() || title.len() > 500 {
            return Err(err(ErrorCode::Invalid, "title must be 1-500 bytes"));
        }
        let sess = self.get(session_id)?;
        let mut st = sess.st.lock().await;
        st.row.title = title.clone();
        st.row.title_locked = true;
        self.emit(&mut st, Event::TitleChanged { title }).await;
        self.persist(&st).await;
        self.push_changed(self.changed_payload(&st), true);
        Ok(())
    }

    pub async fn set_archived(self: &Arc<Self>, session_id: &str, archived: bool) -> RpcResult<()> {
        let sess = self.get(session_id)?;
        let mut st = sess.st.lock().await;
        // spec § 7.1：queued / starting / running / waiting_permission 时不能归档（先停止或答复）：归档状态下不跑 turn
        if archived
            && matches!(
                st.row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            )
        {
            return Err(err(
                ErrorCode::Busy,
                "a turn is in progress or waiting for an answer; stop it or answer first",
            ));
        }
        if st.row.archived != archived {
            st.row.archived = archived;
            st.row.updated_ms = now_ms();
            self.persist(&st).await;
            self.push_changed(self.changed_payload(&st), true);
        }
        Ok(())
    }

    /// 删除（保留期清理等 daemon 自己的操作）：worktree 按 `auto` 处置，不删分支。
    pub async fn delete(self: &Arc<Self>, session_id: &str) -> RpcResult<()> {
        let p = anycon_protocol::SessionDeleteParams {
            session_id: session_id.to_string(),
            worktree: anycon_protocol::WorktreeDisposal::Auto,
            delete_branch: false,
        };
        self.delete_with(&Caller::daemon(), p).await
    }

    /// 删除：事件、附件、scratch 目录一并删；agent 自己的会话文件不动。有 worktree 的会话先收进程，再按 `p.worktree` /
    /// `p.delete_branch` 处置它（git 不在会话锁里跑，daemon.md § 10.1「归档、删除、取消归档」）：被保留下来的 worktree
    /// 变成「没有会话的 worktree」（`worktrees` 行的 `session_id` 置空）。
    pub async fn delete_with(
        self: &Arc<Self>,
        caller: &Caller,
        p: anycon_protocol::SessionDeleteParams,
    ) -> RpcResult<()> {
        let session_id = p.session_id.as_str();
        let sess = self.get(session_id)?;
        let (old, was_scratch, wt) = {
            let mut st = sess.st.lock().await;
            if st.deleted {
                return Err(err(ErrorCode::NotFound, "session was deleted"));
            }
            if matches!(st.wt_busy, Some(WtBusy::Syncing | WtBusy::Archiving | WtBusy::Renaming)) {
                return Err(err(ErrorCode::Busy, "the session's worktree is busy; try again in a moment"));
            }
            // 拉起流程正在建 worktree：它建完发现会话没了会自己清理（`run_create_worktree`，按这个删除请求处置），这里不碰
            let wt = if st.wt_busy == Some(WtBusy::Creating) {
                st.deleted_with = Some((caller.clone(), p.clone()));
                None
            } else {
                st.wt.clone()
            };
            st.deleted = true;
            // 未决的权限请求要告诉所有客户端「已处理」，否则它们的审批卡片会一直挂着
            let perms = std::mem::take(&mut st.perms);
            for p in perms {
                self.perm_index.lock().unwrap().remove(&p.req.request_id);
                self.resolved.lock().unwrap().insert(&p.req.request_id);
                self.d.clients.broadcast(
                    methods::PERMISSION_RESOLVED,
                    PermissionResolved {
                        session_id: st.row.id.clone(),
                        request_id: p.req.request_id.clone(),
                        option_id: reject_option_id(&p.req),
                        by_device: "daemon".into(),
                    },
                );
            }
            self.sched.lock().unwrap().remove(&st.row.id);
            self.release_slot(&mut st);
            st.turn = None;
            st.backlog.clear();
            (self.detach(&mut st), st.row.project.is_none(), wt)
        };
        if let Some(d) = old {
            shutdown_driver(d).await;
        }
        if let Some(wt) = wt {
            self.dispose_worktree_of_deleted(caller, &wt, &p).await;
        }
        let blob_files =
            self.d.store.delete_session(session_id).await.map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
        // 会话的附件（`blobs/<blob_id>.<ext>`）随会话删除
        self.d.blobs.remove_files(&blob_files);
        if was_scratch {
            self.d.workspace.remove_scratch(session_id);
        }
        self.map.write().unwrap().remove(session_id);
        self.throttle.lock().unwrap().remove(session_id);
        self.d.notifier.forget(session_id);
        self.d.clients.forget_session(session_id);
        self.d.clients.broadcast(
            methods::SESSION_CHANGED,
            SessionChanged {
                session_id: session_id.to_string(),
                title: String::new(),
                status: SessionStatus::Ended,
                archived: false,
                pending_permissions: 0,
                background_tasks: 0,
                updated_at_ms: now_ms(),
                last_seq: 0,
                deleted: true,
                worktree: None,
            },
        );
        Ok(())
    }

    pub async fn search(&self, p: SessionSearchParams) -> RpcResult<Vec<SearchHit>> {
        if p.query.trim().is_empty() {
            return Err(err(ErrorCode::Invalid, "query is empty"));
        }
        // 项目过滤：库里同一个项目可能有几种写法，凡是和过滤条件是「同一个名字」的都算
        let projects = match &p.project {
            None => None,
            Some(filter) => {
                let known =
                    self.d.store.distinct_projects().await.map_err(|e| err(ErrorCode::Internal, e.to_string()))?;
                Some(
                    known
                        .into_iter()
                        .filter(|n| self.d.workspace.same_project_name(n, filter))
                        .collect::<Vec<String>>(),
                )
            }
        };
        self.d
            .store
            .search_in(&p.query, projects.as_deref(), p.limit.unwrap_or(20).clamp(1, 200))
            .await
            .map_err(|e| err(ErrorCode::Internal, e.to_string()))
    }

    /// scratch 会话转项目。
    pub async fn promote(self: &Arc<Self>, p: SessionPromoteParams) -> RpcResult<()> {
        let sess = self.get(&p.session_id)?;
        let name = self.d.workspace.canonical_project_name(&p.name);
        crate::workspace::validate_name(&name)?;
        // 三步，**不持会话锁去取 `Workspace` 的命名空间锁**（锁顺序见模块说明；`unlink` 持着命名空间锁逐个锁会话）：
        // ① 会话锁里检查并标 `promoting`（期间 `send` 返回 busy，不会有 turn 在 scratch 目录里开始），然后释放锁；
        let session_id = {
            let mut st = sess.st.lock().await;
            if st.deleted {
                return Err(err(ErrorCode::NotFound, "session was deleted"));
            }
            if st.row.project.is_some() {
                return Err(err(ErrorCode::Invalid, "only scratch sessions can be promoted"));
            }
            if st.promoting || st.turn.is_some() || st.row.status == SessionStatus::Queued {
                return Err(err(ErrorCode::Busy, "a turn is in progress"));
            }
            st.promoting = true;
            st.row.id.clone()
        };
        // ② 不持会话锁改名（取命名空间锁）；
        let moved = self.d.workspace.promote_scratch(&session_id, &name).await;
        // ③ 重新加锁写回。
        let old = {
            let mut st = sess.st.lock().await;
            st.promoting = false;
            moved?;
            if st.deleted {
                return Ok(());
            }
            st.row.project_loc = self.d.workspace.project_loc(&name).ok();
            st.row.project = Some(self.d.workspace.canonical_project_name(&name));
            st.row.updated_ms = now_ms();
            self.persist(&st).await;
            self.push_changed(self.changed_payload(&st), true);
            // 工作目录变了：旧进程不能继续用，下次 send 重新拉起
            self.detach(&mut st).map(|d| self.retire_tracked(&mut st, d))
        };
        if let Some(c) = old {
            c.await;
        }
        Ok(())
    }

    // ============================================================ 统计与项目联动

    /// 每个项目的活跃会话数（非归档、非终态）。
    pub async fn active_counts(&self) -> HashMap<String, u32> {
        let mut m = HashMap::new();
        for s in self.all() {
            let st = s.st.lock().await;
            if st.row.archived || st.deleted {
                continue;
            }
            if matches!(st.row.status, SessionStatus::Ended | SessionStatus::Error | SessionStatus::Interrupted) {
                continue;
            }
            if let Some(p) = &st.row.project {
                *m.entry(p.clone()).or_insert(0) += 1;
            }
        }
        m
    }

    /// 清掉 driver 代号为 `gen` 的 agent 进程记下的 pid（更新的进程已经记了自己的就不动）。
    fn forget_agent_pid(&self, session_id: &str, gen: u64) {
        let mut m = self.agent_pids.lock().unwrap();
        if m.get(session_id).is_some_and(|(g, _)| *g == gen) {
            m.remove(session_id);
        }
    }

    /// 预览用的只读视图（docs/spec/daemon.md § 19.3）：每个会话当前的 agent 主进程 pid（进程不在了就是 `None`）和工作目录
    /// ——项目目录，worktree 会话是它的 worktree，scratch 会话是 scratch 目录。agent 进程因为空闲退出之后工作目录还在，
    /// 它起的 dev server 靠工作目录这条规则继续算在会话头上。目录尽量取 canonical 路径（和 `lsof` 报的 cwd 同一种写法）。
    pub async fn preview_targets(&self) -> Vec<crate::preview::ports::SessionTarget> {
        struct Row {
            id: String,
            project: Option<String>,
            rel_path: Option<String>,
        }
        let mut rows = Vec::new();
        for s in self.all() {
            let st = s.st.lock().await;
            if st.deleted {
                continue;
            }
            rows.push(Row {
                id: st.row.id.clone(),
                project: st.row.project.clone(),
                rel_path: st.wt.as_ref().map(|w| w.rel_path.clone()),
            });
        }
        let pids = self.agent_pids.lock().unwrap().clone();
        let mut out: Vec<crate::preview::ports::SessionTarget> = rows
            .into_iter()
            .map(|r| {
                let dir = match &r.project {
                    Some(name) => self
                        .d
                        .workspace
                        .project_path(name)
                        .ok()
                        .map(|d| r.rel_path.as_deref().map_or(d.clone(), |rel| d.join(rel))),
                    None => Some(self.d.workspace.scratch_path(&r.id)),
                };
                let workdir = dir.map(|d| std::fs::canonicalize(&d).unwrap_or(d));
                crate::preview::ports::SessionTarget {
                    pid: pids.get(&r.id).map(|(_, p)| *p),
                    session_id: r.id,
                    workdir,
                }
            })
            .collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }

    pub async fn counts(&self) -> (SessionCounts, u32) {
        let mut c = SessionCounts::default();
        let mut procs = 0;
        for s in self.all() {
            let st = s.st.lock().await;
            if st.driver.is_some() {
                procs += 1;
            }
            match st.row.status {
                SessionStatus::Queued => c.queued += 1,
                SessionStatus::Starting | SessionStatus::Running => c.running += 1,
                SessionStatus::WaitingPermission => c.waiting_permission += 1,
                SessionStatus::Idle => c.idle += 1,
                SessionStatus::Ended => c.ended += 1,
                SessionStatus::Error => c.error += 1,
                SessionStatus::Interrupted => {}
            }
        }
        (c, procs)
    }

    /// 项目上是否有进行中 / 活着进程 / 正在跑 git 操作的会话（改名、回收前的保护）。
    pub async fn project_busy(&self, project: &str) -> bool {
        for s in self.all() {
            let st = s.st.lock().await;
            // 会话的 git 忙标记也算忙：同步 base / 归档处置 / 删 worktree 进行中，项目目录不能被挪走
            if st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, project))
                && (st.turn.is_some()
                    || st.driver.is_some()
                    || st.row.status == SessionStatus::Queued
                    || st.wt_busy.is_some())
            {
                return true;
            }
        }
        false
    }

    /// 项目上有没有进行中（queued / starting / running / waiting_permission）的会话（解除关联前的保护）。
    pub async fn project_running(&self, project: &str) -> bool {
        for s in self.all() {
            let st = s.st.lock().await;
            if st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, project))
                && matches!(
                    st.row.status,
                    SessionStatus::Queued
                        | SessionStatus::Starting
                        | SessionStatus::Running
                        | SessionStatus::WaitingPermission
                )
            {
                return true;
            }
        }
        false
    }

    /// 收尾一个项目上的全部会话（`workspace.unlink` 之后，或者发现关联目录被换了 / 按当前规则不再允许）。
    /// 循环到收干净：
    /// - 还在跑 turn 的（检查之后、登记删掉之前才开始的）：**取消**，下一轮它收尾以后再关进程；
    /// - 空闲的会话：把 agent 进程关掉。活着的空闲进程会直接收下一条消息、绕过项目解析，所以必须收；
    /// - 每一轮之间的缝隙里新开始的 turn 也会在下一轮被看到；之后才开始的 turn 在 `begin` 里复核项目，起不来。
    pub async fn release_project(self: &Arc<Self>, project: &str) {
        let deadline = Instant::now() + RELEASE_DEADLINE;
        loop {
            let mut pending = false;
            for s in self.all() {
                let (cancel_id, closing) = {
                    let mut st = s.st.lock().await;
                    if !st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, project)) {
                        continue;
                    }
                    if st.turn.is_some() || st.row.status == SessionStatus::Queued {
                        pending = true;
                        (Some(st.row.id.clone()), None)
                    } else if st.driver.is_some() {
                        (None, self.detach(&mut st).map(|d| self.retire_tracked(&mut st, d)))
                    } else {
                        continue;
                    }
                };
                if let Some(id) = cancel_id {
                    if let Err(e) = self.cancel(&id).await {
                        tracing::warn!(session = %id, error = %e.message, "cannot cancel a turn in a released project");
                    }
                }
                if let Some(c) = closing {
                    c.await;
                }
            }
            if !pending {
                return;
            }
            if Instant::now() >= deadline {
                tracing::warn!(project = %project, "some turns in a released project did not stop in time");
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 项目改名之前（`workspace.rename`）：检查项目里没有 turn / 进程 / 排队 / git 操作在跑（和 [`Sessions::project_busy`]
    /// 一样，但检查和标记是一个整体，之后才开始的 turn 进不来），并给项目里有 worktree 的会话标 `WtBusy::Renaming`。
    /// 返回被标的会话 id；之后必须调 [`Sessions::finish_project_rename`]（改名成功）或 [`Sessions::abort_project_rename`]（失败）。
    pub async fn begin_project_rename(&self, project: &str) -> RpcResult<Vec<String>> {
        let mut marked: Vec<Arc<Session>> = Vec::new();
        for s in self.all() {
            let mut st = s.st.lock().await;
            if !st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, project)) {
                continue;
            }
            if st.turn.is_some()
                || st.driver.is_some()
                || st.row.status == SessionStatus::Queued
                || st.wt_busy.is_some()
            {
                drop(st);
                for m in marked {
                    let mut ms = m.st.lock().await;
                    if ms.wt_busy == Some(WtBusy::Renaming) {
                        ms.wt_busy = None;
                    }
                }
                return Err(err(ErrorCode::Busy, "the project has running sessions"));
            }
            if st.wt.is_some() {
                st.wt_busy = Some(WtBusy::Renaming);
                drop(st);
                marked.push(s);
            }
        }
        Ok(marked.iter().map(|s| s.id.clone()).collect())
    }

    /// [`Sessions::begin_project_rename`] 之后改名没成功：放开标记。
    pub async fn abort_project_rename(&self, ids: &[String]) {
        for id in ids {
            if let Ok(s) = self.get(id) {
                let mut st = s.st.lock().await;
                if st.wt_busy == Some(WtBusy::Renaming) {
                    st.wt_busy = None;
                }
            }
        }
    }

    /// 项目改名成功之后：worktree 的登记行改名 → **先 `git worktree repair`**（worktree 的 `.git` 文件和主仓库里的记录都是绝对路径）→
    /// 才让会话看到新名字、放开 `Renaming` 标记（不留「已改名、还没 repair」的窗口）；repair 失败只记 warn，
    /// 会话的指针核对会提示用户运行 `git worktree repair`。更新会话的 project 和记着的位置（根目录下的项目 `folder:<新名字>`；
    /// 关联项目的位置不变，因为目录没动）。
    pub async fn finish_project_rename(self: &Arc<Self>, ids: &[String], from: &str, to: &str) {
        let to = self.d.workspace.canonical_project_name(to);
        // worktree 的登记行（含没有会话的）也跟着改名；会话里的内存副本下面按 id 换成新的
        let loc = self.d.workspace.project_loc(&to).ok();
        let renamed_rows = self.d.worktrees.rename_project_rows(from, &to, loc.as_deref()).await;
        self.d.worktrees.repair_project(&to).await;
        for s in self.all() {
            let mut st = s.st.lock().await;
            if st.row.project.as_deref().is_some_and(|p| self.d.workspace.same_project_name(p, from)) {
                st.row.project = Some(to.clone());
                if let Some(loc) = &loc {
                    st.row.project_loc = Some(loc.clone());
                }
                if let Some(row) = st.wt.as_ref().and_then(|w| renamed_rows.iter().find(|r| r.id == w.id)) {
                    st.wt = Some(row.clone());
                }
                if ids.contains(&st.row.id) && st.wt_busy == Some(WtBusy::Renaming) {
                    st.wt_busy = None;
                }
                self.persist(&st).await;
            }
        }
        // 没找到的（会话在改名期间被删了）：标记随会话一起没了，不用处理
    }

    /// 一个目录刚被（重新）关联为项目 `name`：之前在**同一个目录**里创建、而它们的项目名现在已经不指向那里的会话
    /// （解除关联之后换了个名字关联回来）跟着改成新名字，这样同一个目录用任何名字关联回来，旧会话都能继续。
    /// 项目名现在仍然指向同一个目录的会话不动。返回改了多少个。
    pub async fn adopt_orphans(self: &Arc<Self>, dir: &str, name: &str) -> usize {
        let loc = format!("linked:{dir}");
        let mut n = 0;
        // 这个目录里的 worktree 行（含没有会话的）也改成新名字，会话里的内存副本下面按 id 换掉
        let adopted_rows = self.d.worktrees.adopt_rows(&loc, name).await;
        for s in self.all() {
            let mut st = s.st.lock().await;
            if st.deleted || st.row.project_loc.as_deref() != Some(loc.as_str()) {
                continue;
            }
            let Some(old) = st.row.project.clone() else { continue };
            if self.d.workspace.same_project_name(&old, name) {
                continue;
            }
            // 旧名字现在还指向别处（或根本不存在）才改；仍然指向这个目录的不动
            let still_here = self.d.workspace.project_loc(&old).is_ok_and(|l| l == loc);
            if still_here {
                continue;
            }
            st.row.project = Some(name.to_string());
            if let Some(row) = st.wt.as_ref().and_then(|w| adopted_rows.iter().find(|r| r.id == w.id)) {
                st.wt = Some(row.clone());
            }
            self.persist(&st).await;
            n += 1;
        }
        n
    }

    // ============================================================ 后台维护

    pub fn spawn_sweeper(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(SWEEP_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut maintenance = 0u32;
            let mut recheck = 0u32;
            let mut pr_round = 0u32;
            loop {
                tick.tick().await;
                this.sweep().await;
                recheck += 1;
                if recheck >= this.recheck_every.load(std::sync::atomic::Ordering::Relaxed) {
                    recheck = 0;
                    this.recheck_running_projects().await;
                }
                // PR 轮次（每个会话一次 `gh pr view`，最坏几十秒）和每小时的 `cleanup`（保留期删除要跑 git）都不在这条
                // 共享循环里同步跑：否则它们跑的时候启动超时、权限超时、空闲回收和 2 秒一次的目录复核全部停摆
                pr_round += 1;
                if pr_round >= this.pr_round_every.load(std::sync::atomic::Ordering::Relaxed) {
                    pr_round = 0;
                    this.spawn_auto_archive_round();
                }
                maintenance += 1;
                if maintenance >= 7200 {
                    maintenance = 0;
                    this.spawn_cleanup();
                }
            }
        })
    }

    /// 在后台任务里跑一轮 PR 自动归档；上一轮还没跑完就跳过这一轮（原子标记）。
    pub fn spawn_auto_archive_round(self: &Arc<Self>) {
        let Some(flag) = RunFlag::try_take(&self.pr_round_running) else { return };
        let this = self.clone();
        tokio::spawn(async move {
            let _flag = flag;
            this.auto_archive_round().await;
        });
    }

    /// 在后台任务里跑保留期清理；上一轮还没跑完就跳过。
    pub fn spawn_cleanup(self: &Arc<Self>) {
        let Some(flag) = RunFlag::try_take(&self.cleanup_running) else { return };
        let this = self.clone();
        tokio::spawn(async move {
            let _flag = flag;
            this.cleanup().await;
        });
    }

    /// 一次扫描：idle 超时、权限超时、启动超时、取消宽限、session_idle 通知、名额补位。
    pub async fn sweep(self: &Arc<Self>) {
        // 10 分钟没有新块的附件上传作废（内存里的小表，没有过期的就什么也不做）
        self.d.blobs.expire_uploads();
        let cfg = self.d.config.get();
        let idle_timeout = cfg.idle_timeout();
        let idle_after = cfg.session_idle_after();
        let start_timeout = Duration::from_millis(self.start_timeout_ms.load(std::sync::atomic::Ordering::Relaxed));
        // 不能晚于 idle 回收：回收会摘掉进程、清掉推迟的通知（同一次扫描里补发在回收之前）
        let mut finish_grace =
            Duration::from_millis(self.finish_notify_grace_ms.load(std::sync::atomic::Ordering::Relaxed));
        if !idle_timeout.is_zero() {
            finish_grace = finish_grace.min(idle_timeout);
        }
        let mut to_shutdown: Vec<(Arc<Session>, ClosingFut)> = Vec::new();
        for sess in self.all() {
            let Ok(mut st) = sess.st.try_lock() else { continue };
            if st.deleted {
                continue;
            }
            // 权限请求超时 -> 自动 deny
            let now = Instant::now();
            let expired: Vec<String> = st
                .perms
                .iter()
                .filter(|p| p.expires_at.is_some_and(|e| e <= now))
                .map(|p| p.req.request_id.clone())
                .collect();
            for rid in expired {
                self.auto_deny_one(&mut st, &rid, "timeout").await;
            }
            // starting 超时：从 turn 开始算，拉起进程（spawn）卡住也算在内
            if st.row.status == SessionStatus::Starting {
                let timed_out = st
                    .turn
                    .as_ref()
                    .is_some_and(|t| !t.first_event && !t.preparing && t.started_at.elapsed() > start_timeout);
                if timed_out {
                    self.emit(
                        &mut st,
                        Event::Error {
                            code: "agent_start_timeout".into(),
                            message: "agent did not respond within 60 seconds".into(),
                            recoverable: true,
                        },
                    )
                    .await;
                    if let Some(d) = self.detach(&mut st) {
                        self.retire(&mut st, d);
                    }
                    self.end_turn(&mut st, TurnDoneReason::Error, None, Some(SessionStatus::Error)).await;
                    self.d.notifier.error(&st.row.id);
                    continue;
                }
            }
            // 取消后 adapter 迟迟不结束 turn
            if let Some(at) = st.turn.as_ref().and_then(|t| t.cancel_requested_at) {
                if at.elapsed() > CANCEL_GRACE {
                    if let Some(d) = self.detach(&mut st) {
                        self.retire(&mut st, d);
                    }
                    st.backlog.clear();
                    self.end_turn(&mut st, TurnDoneReason::Cancelled, None, None).await;
                    continue;
                }
            }
            // 推迟的「turn 完成」：后台任务都结束了，CLI 却没有接着自己跑一轮（那一轮结束时会发）——过一会儿兜底补发
            if st.finish_notify_pending
                && st.turn.is_none()
                && st.background_tasks == 0
                && st.background_drained_at.is_some_and(|t| t.elapsed() >= finish_grace)
            {
                st.finish_notify_pending = false;
                self.d.notifier.turn_finished(&st.row.id);
            }
            // ended（回合数 / 预算用完）但进程还活着：和 idle 一样按 `idle_timeout` 回收，别让它一直占着进程
            if st.row.status == SessionStatus::Ended
                && st.driver.is_some()
                && st.background_tasks == 0
                && !idle_timeout.is_zero()
                && st.ended_since.is_some_and(|t| t.elapsed() >= idle_timeout)
                && self.d.agents.info(&st.row.agent).is_some_and(|i| i.capabilities.resume)
            {
                if let Some(d) = self.detach(&mut st) {
                    let closing = self.retire_tracked(&mut st, d);
                    to_shutdown.push((sess.clone(), closing));
                }
            }
            // idle：超时关进程；通知 session_idle（已归档的会话不提醒）
            if st.row.status == SessionStatus::Idle {
                if let Some(since) = st.idle_since {
                    // 后台任务还在跑（后台 subagent、后台命令）：关进程会连同它们一起杀掉，不回收
                    if !idle_timeout.is_zero()
                        && st.driver.is_some()
                        && st.background_tasks == 0
                        && since.elapsed() >= idle_timeout
                    {
                        let can_resume = self.d.agents.info(&st.row.agent).is_some_and(|i| i.capabilities.resume);
                        if can_resume {
                            if let Some(d) = self.detach(&mut st) {
                                let closing = self.retire_tracked(&mut st, d);
                                to_shutdown.push((sess.clone(), closing));
                            }
                        }
                    }
                    // 一直开着这个会话的客户端也算「看过」
                    if !st.watched_during_idle && self.d.clients.has_watchers(&st.row.id) {
                        st.watched_during_idle = true;
                    }
                    if !st.row.archived
                        && st.background_tasks == 0
                        && !st.idle_notified
                        && !st.watched_during_idle
                        && !idle_after.is_zero()
                        && since.elapsed() >= idle_after
                    {
                        st.idle_notified = true;
                        self.d.notifier.session_idle(&st.row.id);
                    }
                }
            }
        }
        for (sess, closing) in to_shutdown {
            let this = self.clone();
            tokio::spawn(async move {
                // 旧进程关闭前报告的 resume 信息要存下来（下次 send 用它 resume）
                if let Some(r) = closing.await {
                    let mut st = sess.st.lock().await;
                    if st.row.agent_session_id.as_deref() == Some(r.agent_session_id.as_str())
                        || st.row.agent_session_id.is_none()
                    {
                        adopt_closing_resume(&mut st, r);
                        this.persist(&st).await;
                    }
                }
            });
        }
        // max_concurrent 热加载后可能有空位
        let has_room = {
            let s = self.sched.lock().unwrap();
            !s.queue.is_empty() && s.running < cfg.sessions.max_concurrent
        };
        if has_room {
            self.kick_pump();
        }
    }

    async fn auto_deny_one(self: &Arc<Self>, st: &mut St, request_id: &str, message: &str) {
        let Some(idx) = st.perms.iter().position(|p| p.req.request_id == request_id) else { return };
        let p = st.perms.remove(idx);
        let opt = reject_option_id(&p.req);
        self.perm_index.lock().unwrap().remove(request_id);
        self.resolved.lock().unwrap().insert(request_id);
        self.emit(
            st,
            Event::PermissionResolved {
                request_id: request_id.to_string(),
                option_id: opt.clone(),
                by_device: "daemon".into(),
            },
        )
        .await;
        self.d.clients.broadcast(
            methods::PERMISSION_RESOLVED,
            PermissionResolved {
                session_id: st.row.id.clone(),
                request_id: request_id.to_string(),
                option_id: opt.clone(),
                by_device: "daemon".into(),
            },
        );
        if let Some(d) = &st.driver {
            let _ = d.tx.send(DriverCmd::Respond {
                request_id: request_id.to_string(),
                decision: PermissionDecision { option_id: opt, message: Some(message.to_string()), answers: None },
            });
        }
        if st.perms.is_empty() && st.row.status == SessionStatus::WaitingPermission {
            self.set_status(st, SessionStatus::Running, None).await;
        } else {
            self.notify_changed(st);
        }
    }

    /// 保留期清理：`history_retention` 到期的会话整条删，过期的 scratch 目录清掉，回收站超期条目清掉，
    /// 完成后 24 小时仍没被消息引用的附件（blob）删掉。
    pub async fn cleanup(self: &Arc<Self>) {
        let cfg = self.d.config.get();
        if let Some(ret) = cfg.history_retention() {
            let cutoff = now_ms().saturating_sub(ret.as_millis() as u64);
            let old = match self.d.store.sessions_older_than(cutoff).await {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(error = %e, "cannot list sessions past the retention period");
                    vec![]
                }
            };
            for id in old {
                if let Err(e) = self.delete(&id).await {
                    tracing::warn!(session = %id, error = %e.message, "cannot delete an expired session");
                }
            }
            warn_err("prune audit log", self.d.store.prune_audit(cutoff).await);
        }
        let days = cfg.workspace.scratch_retention_days;
        if days > 0 {
            let cutoff = now_ms().saturating_sub(days as u64 * 86_400_000);
            for sess in self.all() {
                let st = sess.st.lock().await;
                if st.row.project.is_none() && st.row.status == SessionStatus::Ended && st.row.updated_ms < cutoff {
                    self.d.workspace.remove_scratch(&st.row.id);
                }
            }
        }
        self.d.workspace.cleanup_trash().await;
        self.d.blobs.gc().await;
    }

    /// 某个 watcher 看过这个会话（抑制 session_idle 通知）。
    pub fn note_watched(self: &Arc<Self>, session_id: &str) {
        if let Ok(sess) = self.get(session_id) {
            tokio::spawn(async move {
                sess.st.lock().await.watched_during_idle = true;
            });
        }
    }

    /// daemon 退出：关掉所有 agent 进程（先礼后兵由 adapter 的 shutdown 负责），在跑的会话标 interrupted。
    pub async fn shutdown_all(self: &Arc<Self>) {
        let mut drivers = Vec::new();
        for sess in self.all() {
            let mut st = sess.st.lock().await;
            let active = matches!(
                st.row.status,
                SessionStatus::Queued
                    | SessionStatus::Starting
                    | SessionStatus::Running
                    | SessionStatus::WaitingPermission
            );
            if active {
                self.deny_all_inner(&mut st, "daemon shutting down", false).await;
            }
            if let Some(d) = self.detach(&mut st) {
                drivers.push(d);
            }
            if active {
                st.turn = None;
                st.queued = false;
                self.sched.lock().unwrap().remove(&st.row.id);
                self.set_status(&mut st, SessionStatus::Interrupted, None).await;
            }
        }
        let all = futures::future::join_all(drivers.into_iter().map(shutdown_driver));
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, all).await;
    }

    /// 测试用：周期复核每隔多少次扫描做一次（调得很大 = 测试期间「关掉」它，让 send / 放行权限时的复核可以单独验证）。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_recheck_every(&self, n: u32) {
        self.recheck_every.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：PR 自动归档每隔多少次扫描起一轮（缺省 1200 次 = 10 分钟）。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_pr_round_every(&self, n: u32) {
        self.pr_round_every.store(n.max(1), std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：缩短 `starting` 超时。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_start_timeout(&self, d: Duration) {
        self.start_timeout_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// 测试用：缩短推迟的 `turn_finished` 兜底补发前的等待。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_finish_notify_grace(&self, d: Duration) {
        self.finish_notify_grace_ms.store(d.as_millis() as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

/// RPC 错误的稳定码（snake_case），写进 `error` 事件的 `code`。
fn stable_code_str(e: &RpcError) -> String {
    e.stable_code()
        .and_then(|c| serde_json::to_value(c).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "error".into())
}

fn unavailable(status: &AgentStatus, agent: &str) -> RpcError {
    let mut e = err(ErrorCode::AgentUnavailable, format!("agent {agent} is not available ({status:?})"));
    if let Some(d) = e.data.as_mut() {
        d.reason = Some(format!("{status:?}").to_lowercase());
    }
    e
}

/// 关闭一个 driver，返回它在关闭前报告的 resume 信息。
async fn shutdown_driver(d: Driver) -> Option<ResumeHandle> {
    let (tx, rx) = oneshot::channel();
    if d.tx.send(DriverCmd::Shutdown { reply: tx }).is_err() {
        return None;
    }
    tokio::time::timeout(SHUTDOWN_TIMEOUT, rx).await.ok().and_then(|r| r.ok()).flatten()
}

/// driver 任务结束（正常收尾、被关闭、panic）时清掉会话记下的 agent pid。
struct AgentPidGuard {
    sessions: Arc<Sessions>,
    sid: String,
    gen: u64,
}

impl Drop for AgentPidGuard {
    fn drop(&mut self) {
        self.sessions.forget_agent_pid(&self.sid, self.gen);
    }
}

/// 独占 `AgentHandle` 的任务。
///
/// `Exited` 是终态：收到并交给状态机之后 driver 就收尾，不会再从已经关闭的事件通道里合成第二个 `Exited`。
/// `biased`：事件优先于命令，先把 adapter 已经发出的事件（包括 `Exited`）处理完。
async fn driver_loop(
    sessions: Arc<Sessions>,
    sid: String,
    gen: u64,
    mut handle: Box<dyn AgentHandle>,
    mut cmds: mpsc::UnboundedReceiver<DriverCmd>,
) {
    let _pid = AgentPidGuard { sessions: sessions.clone(), sid: sid.clone(), gen };
    loop {
        tokio::select! {
            biased;
            ev = handle.events().recv() => {
                match ev {
                    Some(ev) => {
                        let exited = matches!(ev, AgentEvent::Exited { .. });
                        sessions.on_agent_event(&sid, gen, ev).await;
                        if exited {
                            break;
                        }
                    }
                    None => {
                        // 通道关闭而没有 Exited：当作异常退出（只合成这一次）
                        sessions
                            .on_agent_event(&sid, gen, AgentEvent::Exited { code: None, error: Some("agent event channel closed".into()) })
                            .await;
                        break;
                    }
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else {
                    // 所有发送端都没了（会话被丢弃）：关进程
                    warn_err("agent shutdown", handle.shutdown().await);
                    return;
                };
                match cmd {
                    DriverCmd::Send { input, retry } => match (handle.send(input).await, retry) {
                        (Ok(()), Some(_)) => sessions.on_interject_done(&sid).await,
                        (Ok(()), None) => {}
                        (Err(e), Some(msg)) => sessions.on_interject_failed(&sid, gen, msg, e).await,
                        (Err(e), None) => sessions.on_send_failed(&sid, gen, e).await,
                    },
                    DriverCmd::Respond { request_id, decision } => {
                        if let Err(e) = handle.respond_permission(&request_id, decision).await {
                            tracing::warn!(session = %sid, error = %e, "respond_permission failed");
                        }
                    }
                    DriverCmd::Cancel => {
                        if let Err(e) = handle.cancel().await {
                            tracing::warn!(session = %sid, error = %e, "cancel failed");
                        }
                    }
                    DriverCmd::SetSettings { settings, reply } => {
                        let _ = reply.send(handle.set_settings(&settings).await);
                    }
                    DriverCmd::Shutdown { reply } => {
                        let resume = handle.resume_handle();
                        if let Err(e) = handle.shutdown().await {
                            tracing::warn!(session = %sid, error = %e, "agent shutdown failed");
                        }
                        let _ = reply.send(resume);
                        // 把剩下的事件读空，让 adapter 的读取任务正常收尾（事件已被代号过滤，不会再进状态机）
                        let _ = tokio::time::timeout(Duration::from_secs(2), async { while handle.events().recv().await.is_some() {} }).await;
                        return;
                    }
                }
            }
        }
    }
    // agent 已经退出：它的 pid 不再代表任何会话进程（预览据此判断端口归属，见 `Sessions::preview_targets`）
    sessions.forget_agent_pid(&sid, gen);
    // 回收残留（幂等；进程组里可能还有它派生的子进程）
    if let Err(e) = tokio::time::timeout(Duration::from_secs(5), handle.shutdown()).await.unwrap_or(Ok(())) {
        tracing::debug!(session = %sid, error = %e, "cleanup after the agent exited failed");
    }
}

/// 旧进程关闭前报告的 resume 信息：记下它的 agent 会话 id 和 launch_args。这份 `launch_args` 是关闭那一刻的快照，可能比
/// 现在的设置旧（关闭期间 / 关闭之后用户改过、清过设置），所以没有存活进程时再按**当前**设置同步一遍，
/// 否则 `set_settings` 刚清掉的值会被这个迟到的快照写回来。
fn adopt_closing_resume(st: &mut St, r: ResumeHandle) {
    st.row.agent_session_id = Some(r.agent_session_id);
    st.row.launch_args = r.launch_args;
    if st.driver.is_none() {
        sync_launch_args(&mut st.row.launch_args, &st.row.settings);
    }
}

/// `ResumeHandle.launch_args` 是 adapter 自己的 JSON（daemon 不解释它），但它按约定是「上次进程启动时有效设置」的快照，
/// 键名和 `SessionSettings` 的字段同名：`model` `thinking` `mode` `system_prompt_append` `max_turns` `max_budget_usd`
/// `env`，以及 `fast_mode`（= `options.fast_mode`）。daemon 在没有存活进程时改了设置，就把**已经出现在快照里**的这些键
/// 改成新值——新设置里没有（被 `clear` 或本来就没有）的直接删掉，这样即使 adapter 回落到快照，也拿不到旧值。
/// 快照里没有的键不加（那是 adapter 的事）。不是 JSON 对象的 `launch_args` 原样不动。
fn sync_launch_args(args: &mut serde_json::Value, new: &SessionSettings) {
    use serde_json::Value;
    let Some(obj) = args.as_object_mut() else { return };
    let mut put = |key: &str, value: Option<Value>| {
        if obj.contains_key(key) {
            match value {
                Some(v) => obj.insert(key.to_string(), v),
                None => obj.remove(key),
            };
        }
    };
    put("model", new.model.clone().map(Value::from));
    put("thinking", new.thinking.clone().map(Value::from));
    put("mode", new.mode.clone().map(Value::from));
    put("system_prompt_append", new.system_prompt_append.clone().map(Value::from));
    put("max_turns", new.max_turns.map(Value::from));
    put("max_budget_usd", new.max_budget_usd.map(Value::from));
    put("env", (!new.env.is_empty()).then(|| serde_json::to_value(&new.env).unwrap_or(Value::Null)));
    put("fast_mode", new.options.get("fast_mode").filter(|v| v.is_boolean()).cloned());
}

#[cfg(test)]
mod launch_args_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cleared_settings_disappear_from_the_launch_args_snapshot_and_changed_ones_follow() {
        let mut args = json!({
            "model": "opus", "thinking": "high", "mode": "plan",
            "system_prompt_append": "OLD SECRET", "max_turns": 7, "max_budget_usd": 1.5,
            "env": { "A": "1" }, "fast_mode": true,
            "resume_id": "kept", "unknown_adapter_key": 1
        });
        // 什么都没设置（全部被清掉）：同名的键全部删除，别的键原样
        sync_launch_args(&mut args, &SessionSettings::default());
        assert_eq!(args, json!({ "resume_id": "kept", "unknown_adapter_key": 1 }));

        // 设了值：快照里已有的键跟着改，没有的键不凭空加
        let mut args = json!({ "model": "opus", "system_prompt_append": "OLD", "max_turns": 7 });
        let mut s = SessionSettings {
            model: Some("sonnet".into()),
            system_prompt_append: Some("NEW".into()),
            max_budget_usd: Some(2.0),
            ..Default::default()
        };
        s.env.insert("K".into(), "v".into());
        sync_launch_args(&mut args, &s);
        assert_eq!(args, json!({ "model": "sonnet", "system_prompt_append": "NEW" }));

        // 非对象原样不动
        let mut null = serde_json::Value::Null;
        sync_launch_args(&mut null, &s);
        assert!(null.is_null());
    }
}

/// 插话与顺序的状态机用例：要把会话摆进「`turn_done` 已经处理、插话的结果还没回来」这个状态，
/// 这个状态在真实运行里只持续几个微秒，所以直接摆（用本模块的私有字段）再喂结果。
#[cfg(test)]
mod interject_order_tests {
    use super::*;
    use crate::testutil::TestDaemon;
    use serde_json::json;

    async fn send(
        td: &TestDaemon,
        c: &mut crate::testutil::TestClient,
        sid: &str,
        mid: &str,
        text: &str,
    ) -> SessionSendResult {
        c.call(td, methods::SESSION_SEND, json!({ "session_id": sid, "message_id": mid, "content": text }))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_message_that_arrives_while_an_interjection_has_no_result_waits_behind_it() {
        let td = TestDaemon::start().await;
        td.fake.interject.store(true, std::sync::atomic::Ordering::SeqCst);
        td.ctx.agents.refresh_all().await;
        let mut c = td.client("a");
        let s: SessionSummary = c
            .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": "m1" }))
            .await
            .unwrap();
        // 等第一个 turn 结束（进程还活着）
        let t = std::time::Instant::now();
        while c.call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap().status
            != SessionStatus::Idle
        {
            assert!(t.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let sessions = td.ctx.sessions.clone();
        let sess = sessions.get(&s.id).unwrap();
        // 摆出状态：turn 已经结束，但有一条插话（m2）的结果还没回来
        let gen = {
            let mut st = sess.st.lock().await;
            assert!(st.turn.is_none() && st.driver.is_some());
            st.interjecting = 1;
            st.driver_gen
        };
        let inputs_before = td.fake.last_handle().inputs.lock().unwrap().len();
        // m3 到达：只入队，不开新 turn（否则 m2 退回时会排在它后面）
        let r = send(&td, &mut c, &s.id, "m3", "third message").await;
        assert!(r.queued && r.turn_id.is_none(), "{r:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let st = sess.st.lock().await;
            assert!(st.turn.is_none(), "no turn may start while an interjection has no result");
            assert_eq!(st.backlog.len(), 1);
        }
        assert_eq!(td.fake.last_handle().inputs.lock().unwrap().len(), inputs_before);
        // 插话的结果回来了：没送进去（TurnEnded）。m2 按到达顺序排在 m3 前面，两条一起作为下一个 turn
        let m2 = Backlog {
            message_id: "m2".into(),
            content: "second message".into(),
            ts_ms: now_ms(),
            ord: 0,
            attachments: vec![],
        };
        sessions.on_interject_failed(&s.id, gen, m2, AdapterError::TurnEnded).await;
        let t = std::time::Instant::now();
        loop {
            if td.fake.last_handle().inputs.lock().unwrap().len() > inputs_before {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "the queued messages never started a turn");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let inputs = td.fake.last_handle().inputs.lock().unwrap().clone();
        let next = inputs.last().unwrap();
        assert!(!next.interject, "a new turn, not an interjection");
        let (a, b) = (next.content.find("second message").unwrap(), next.content.find("third message").unwrap());
        assert!(a < b, "the agent sees the messages in the order the user sent them: {}", next.content);
        assert_eq!(inputs.len(), inputs_before + 1, "exactly one new turn");
        // 对称的：插话「成功」的结果回来时，如果积压里有消息在等，接着开下一个 turn
        let t = std::time::Instant::now();
        while c.call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap().status
            != SessionStatus::Idle
        {
            assert!(t.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        {
            let mut st = sess.st.lock().await;
            st.interjecting = 1;
        }
        let n = td.fake.last_handle().inputs.lock().unwrap().len();
        send(&td, &mut c, &s.id, "m4", "fourth message").await;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(td.fake.last_handle().inputs.lock().unwrap().len(), n, "still waiting for the interjection result");
        sessions.on_interject_done(&s.id).await;
        let t = std::time::Instant::now();
        while td.fake.last_handle().inputs.lock().unwrap().len() == n {
            assert!(t.elapsed() < Duration::from_secs(10), "the queued message never started a turn");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        td.shutdown().await;
    }
}

/// 已归档的会话不开新 turn：`send` 本来就拒绝，`try_begin` 是兜底——排队的消息不会在归档之后作为新 turn 发出。
/// 要摆出「已归档、backlog 里有一条消息」这个状态（等审批时已经不能归档了），所以直接用本模块的私有字段。
#[cfg(test)]
mod archived_backlog_tests {
    use super::*;
    use crate::testutil::TestDaemon;
    use serde_json::json;

    #[tokio::test]
    async fn an_archived_session_never_starts_a_turn_from_its_backlog() {
        let td = TestDaemon::start().await;
        let mut c = td.client("a");
        let s: SessionSummary = c
            .call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "prompt": "hi", "message_id": "m1" }))
            .await
            .unwrap();
        let t = std::time::Instant::now();
        while c.call::<SessionSummary>(&td, methods::SESSION_GET, json!({ "session_id": s.id })).await.unwrap().status
            != SessionStatus::Idle
        {
            assert!(t.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _: anycon_protocol::Empty =
            c.call(&td, methods::SESSION_ARCHIVE, json!({ "session_id": s.id })).await.unwrap();
        let sessions = td.ctx.sessions.clone();
        let sess = sessions.get(&s.id).unwrap();
        let inputs_before = td.fake.last_handle().inputs.lock().unwrap().len();
        {
            let mut st = sess.st.lock().await;
            st.backlog.push(Backlog {
                message_id: "m2".into(),
                content: "queued".into(),
                ts_ms: now_ms(),
                ord: 1,
                attachments: vec![],
            });
            sessions.try_begin(&sess, &mut st).await;
            assert!(st.turn.is_none(), "an archived session must not start a turn");
            assert_eq!(st.backlog.len(), 1, "the message waits");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(td.fake.last_handle().inputs.lock().unwrap().len(), inputs_before, "nothing reached the agent");
        // 取消归档之后下一次开 turn 的机会（比如 `send`）会把它连同新消息一起发出
        let _: anycon_protocol::Empty =
            c.call(&td, methods::SESSION_UNARCHIVE, json!({ "session_id": s.id })).await.unwrap();
        {
            let mut st = sess.st.lock().await;
            sessions.try_begin(&sess, &mut st).await;
            assert!(st.turn.is_some(), "once unarchived the queued message starts a turn");
        }
        td.shutdown().await;
    }
}
