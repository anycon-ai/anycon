//! 一个 ACP agent 的进程和上面的会话（docs/spec/daemon.md § 7.3）。
//!
//! **一个 agent 一个进程，服务它的所有会话**：[`Pool`] 管这个进程，每个 daemon 会话经 [`Connection::open_session`]
//! 拿到一个 [`Session`]，`AcpHandle` 是它的一个视图。进程按「挂着的句柄数」计数：最后一个句柄 `shutdown` 时才关进程
//! （关 stdin → 等 → SIGINT → 宽限期 → SIGKILL，整个进程组），所以 daemon 的 `idle_timeout` 回收一个会话只是让它脱离，
//! 其它会话照常跑；该 agent 的会话都回收完了，进程才退出。agent 声明了 `session/resume` 或 `session/load` 时 daemon 才会回收
//! （`capabilities.resume`）；两者都没有的 agent，daemon 不回收会话，进程就一直活到会话被删除 / 归档。
//!
//! 一个 reader 任务读 stdout，按行分类：响应交给 [`Rpc`]；`session/update` 按 `sessionId` 交给会话（选项状态更新、
//! 重放期间吞掉、翻译成事件）；agent 发来的请求（权限、elicitation）交给会话，**不认识的一律答 `-32601`**（我们没有声明
//! `fs` / `terminal`，agent 用它自己的工具）。读任务只管读、分类、处理响应和请求，事件进每个会话一个不限长的内部队列，
//! 由另一个任务往有界的 `events()` 通道（1024）里送：daemon 在等 `send` / `cancel` 返回期间不读 `events()`，
//! 通道满了也不能卡住别的会话（`AgentHandle` 的不阻塞约定）。
//!
//! 进程崩溃 = stdout 关闭：每个还挂着句柄的会话收尾（还开着的文字 / 工具调用补上结果），中途崩溃的先发一条
//! `error { agent_exited }`，然后 `Exited { error }`；daemon 据此把各个会话标成 error，下一条消息走 resume（agent 有
//! `session/resume` / `session/load` 时接上原来的会话，否则开新会话并说明上下文没了）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::{AdapterError, AgentEvent, ResumeHandle, SpawnRequest};
use anycon_protocol::Event;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::codex::rpc::{classify_oversize, OversizeKind};
use crate::common::{ExitInfo, JsonlChild, LineEvent, ShutdownTimings, SpawnSpec, DEFAULT_MAX_LINE_BYTES};

use super::options::ConfigState;
use super::permissions::{self, NativePending};
use super::rpc::{classify, Incoming, Rpc, RpcError, RpcFailure, AUTH_REQUIRED, METHOD_NOT_FOUND, RESOURCE_NOT_FOUND};
use super::translate::{Out, PromptOutcome, Translator};
use super::wire::{AgentCaps, InitializeResult};

/// 各种等待时间。生产用默认值；测试里缩短。
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// `initialize` 的上限。
    pub handshake: Duration,
    /// `session/new` / `session/load` / `session/resume` 的上限（agent 可能要先连它的 MCP server、拉模型列表）。
    pub session: Duration,
    /// `session/set_config_option` / `session/set_mode` 的上限。
    pub request: Duration,
    /// `session/close` 的上限。
    pub close: Duration,
    pub shutdown: ShutdownTimings,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(30),
            session: Duration::from_secs(60),
            request: Duration::from_secs(30),
            close: Duration::from_secs(10),
            shutdown: ShutdownTimings::default(),
        }
    }
}

/// 怎么拉起 agent。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// 子进程的完整环境（daemon 环境 + `agents.env` + `agents.<id>.env`）。ACP 的进程按 agent 共享，所以没有会话级的环境。
    pub env: Arc<HashMap<String, String>>,
}

impl LaunchSpec {
    /// 同一个进程能不能服务这个请求：程序、参数、环境都一样。
    pub fn fingerprint(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.program.to_string_lossy().as_bytes());
        for a in &self.args {
            h.update([0]);
            h.update(a.as_bytes());
        }
        let mut env: Vec<(&String, &String)> = self.env.iter().collect();
        env.sort();
        for (k, v) in env {
            h.update([1]);
            h.update(k.as_bytes());
            h.update([2]);
            h.update(v.as_bytes());
        }
        hex_prefix(&h.finalize())
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// `initialize` 的参数（探测和正式连接共用）。
///
/// 声明的 client 能力：**不声明 `fs` 和 `terminal`**（agent 用它自己的工具读写文件、跑命令，权限走 `session/request_permission`）；
/// 声明 `elicitation`（form / url）——agent 向用户要结构化输入时走权限通道；声明 `session.configOptions.boolean`——不声明的话 agent 不会给
/// 布尔选项。不声明 `auth.terminal`（我们没有交互式终端）、`session.notices`、`session.compaction`（规格里 ACP 不产生 notice / task / activity 事件）。
pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
            "elicitation": { "form": {}, "url": {} },
            "session": { "configOptions": { "boolean": {} } }
        },
        "clientInfo": { "name": "anycon", "title": "anycon", "version": env!("CARGO_PKG_VERSION") }
    })
}

fn map_spawn_error(program: &std::path::Path, e: std::io::Error) -> AdapterError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AdapterError::NotInstalled(format!("{}: {e}", program.display())),
        _ => AdapterError::Spawn(format!("{}: {e}", program.display())),
    }
}

pub fn map_failure(what: &str, e: RpcFailure) -> AdapterError {
    match e {
        RpcFailure::Rpc(err) => AdapterError::Protocol(format!("{what}: {}", err.message)),
        RpcFailure::Timeout => AdapterError::Protocol(format!("{what}: timed out waiting for the agent")),
        RpcFailure::Closed => AdapterError::Exited(format!("{what}: the agent is not running")),
    }
}

fn text_has(s: &str, needles: &[&str]) -> bool {
    let l = s.to_ascii_lowercase();
    needles.iter().any(|n| l.contains(n))
}

/// 续接 / 加载一个会话时，哪些错误说明「这个会话已经不在了」（回退成新会话）：`-32002`，或者消息里这么说。
fn is_session_gone(e: &RpcError) -> bool {
    e.code == RESOURCE_NOT_FOUND
        || text_has(&e.message, &["not found", "no such session", "unknown session", "does not exist", "no session"])
}

// ---------- 每个 agent 的进程池 ----------

/// 一个 agent 的当前进程。
pub struct Pool {
    /// 起进程要串行（第二个会话等第一个把进程起好，不会起两个）。
    start_lock: tokio::sync::Mutex<()>,
    current: Mutex<Option<Arc<Connection>>>,
}

impl Pool {
    pub fn new() -> Self {
        Self { start_lock: tokio::sync::Mutex::new(()), current: Mutex::new(None) }
    }

    /// 拿到能服务 `spec` 的进程（没有就起一个），并把调用方算作一个挂着的句柄。**出错 / 不再需要时必须 [`Pool::release`]。**
    /// 当前进程已经死了，或者启动方式变了（命令 / 环境改了）：另起一个；旧的在它的句柄都走完后退出。
    pub async fn acquire(
        &self,
        spec: &LaunchSpec,
        label: &str,
        timings: &Timings,
    ) -> Result<Arc<Connection>, AdapterError> {
        let _start = self.start_lock.lock().await;
        let fingerprint = spec.fingerprint();
        {
            let cur = self.current.lock().expect("pool lock");
            if let Some(c) = cur.as_ref().filter(|c| c.usable_for(&fingerprint)) {
                c.attached.fetch_add(1, Ordering::SeqCst);
                return Ok(c.clone());
            }
        }
        let conn = Connection::start(spec, label, fingerprint, timings).await?;
        let mut cur = self.current.lock().expect("pool lock");
        conn.attached.fetch_add(1, Ordering::SeqCst);
        *cur = Some(conn.clone());
        Ok(conn)
    }

    /// 一个句柄走了。返回 `true` = 它是最后一个，调用方要关进程。计数和「从池里摘掉」在同一把锁里，
    /// 所以不会和并发的 `acquire` 撞上（要么 `acquire` 先拿到它、计数不是 0，要么它已经不在池里）。
    fn release_count(&self, conn: &Arc<Connection>) -> bool {
        let mut cur = self.current.lock().expect("pool lock");
        let left = conn.attached.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
        if left == 0 {
            if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                *cur = None;
            }
            true
        } else {
            false
        }
    }

    /// 句柄走了；最后一个走的负责把进程关掉（等它退完，daemon 据此知道旧进程收尾了）。
    pub async fn release(&self, conn: Arc<Connection>, timings: &Timings) {
        if self.release_count(&conn) {
            conn.shutdown(timings.shutdown).await;
        }
    }

    /// 没走 `shutdown` 就被丢弃的句柄：同步地放手，最后一个直接杀进程，别留孤儿。
    pub fn release_nowait(&self, conn: &Arc<Connection>) {
        if self.release_count(conn) {
            conn.state.shutting_down.store(true, Ordering::SeqCst);
            conn.child.close_stdin();
            conn.child.kill();
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ConnState {
    dead: AtomicBool,
    shutting_down: AtomicBool,
    /// 读任务发现的、必须结束这个进程的协议错误（比如待答复的请求超长）。
    fatal: Mutex<Option<String>>,
}

pub struct Connection {
    pub child: Arc<JsonlChild>,
    pub rpc: Arc<Rpc>,
    /// `initialize` 的结果。
    pub init: InitializeResult,
    pub label: String,
    fingerprint: String,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    state: Arc<ConnState>,
    /// 挂着的句柄数（含正在 `open_session` 的）。
    attached: AtomicUsize,
    reader: Mutex<Option<JoinHandle<()>>>,
}

/// `open_session` 怎么拿到的会话。
#[derive(Debug, Clone, PartialEq)]
pub enum Opened {
    New,
    /// 接上了原来的会话（进程里还在 / `session/resume` / `session/load`）。
    Resumed,
    /// 要续接但原来的会话已经不在了，开了新的；带 agent 的说法。
    ContextReset(String),
}

impl Connection {
    /// 拉起进程并 `initialize`。进程的 cwd 是中性目录（会话的目录在 `session/new` 里给，规格要求 agent 按它而不是进程 cwd）。
    pub async fn start(
        spec: &LaunchSpec,
        label: &str,
        fingerprint: String,
        timings: &Timings,
    ) -> Result<Arc<Self>, AdapterError> {
        let sp = SpawnSpec {
            program: spec.program.clone(),
            args: spec.args.clone(),
            cwd: crate::common::neutral_cwd(),
            env: (*spec.env).clone(),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        };
        let (child, lines) = JsonlChild::spawn_events(&sp).map_err(|e| map_spawn_error(&spec.program, e))?;
        let child = Arc::new(child);
        let rpc = Arc::new(Rpc::new(child.clone()));
        let sessions: Arc<Mutex<HashMap<String, Arc<Session>>>> = Arc::new(Mutex::new(HashMap::new()));
        let state = Arc::new(ConnState {
            dead: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            fatal: Mutex::new(None),
        });
        let reader = tokio::spawn(read_loop(
            label.to_string(),
            child.clone(),
            rpc.clone(),
            sessions.clone(),
            state.clone(),
            lines,
        ));
        let init = match rpc.request("initialize", initialize_params(), timings.handshake).await {
            Ok(v) => InitializeResult::from_value(&v),
            Err(e) => {
                let err = startup_failure(&child, &state, e, "initialize").await;
                child.close_stdin();
                child.kill();
                return Err(err);
            }
        };
        match init.protocol_version {
            Some(1) => {}
            Some(other) => {
                child.close_stdin();
                child.kill();
                return Err(AdapterError::UnsupportedVersion(format!(
                    "the agent speaks ACP v{other}; anycon supports v1"
                )));
            }
            None => tracing::warn!(agent = label, "acp: the initialize response has no protocolVersion, assuming 1"),
        }
        Ok(Arc::new(Connection {
            child,
            rpc,
            init,
            label: label.to_string(),
            fingerprint,
            sessions,
            state,
            attached: AtomicUsize::new(0),
            reader: Mutex::new(Some(reader)),
        }))
    }

    pub fn caps(&self) -> &AgentCaps {
        &self.init.agent_capabilities
    }

    pub fn is_dead(&self) -> bool {
        self.state.dead.load(Ordering::SeqCst) || self.child.has_exited()
    }

    fn usable_for(&self, fingerprint: &str) -> bool {
        !self.is_dead() && !self.state.shutting_down.load(Ordering::SeqCst) && self.fingerprint == fingerprint
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.pid()
    }

    pub fn stderr_tail(&self) -> String {
        self.child.stderr_tail()
    }

    /// 在 agent 里新开 / 接上一个会话。`resume`：daemon 记着的 agent 会话 id。
    ///
    /// - 进程里还有这个会话（之前脱离时 agent 没有 `session/close`，没关）：直接接上，不发请求；
    /// - 否则 agent 有 `session/resume`（不重放）就用它，其次 `session/load`（重放全部历史，期间的更新一律吞掉）；
    /// - 会话已经不在了（`-32002` / 消息里说找不到）或者两个能力都没有：开新会话，返回 [`Opened::ContextReset`]；
    /// - 认证错误 = `NotAuthenticated`。
    pub async fn open_session(
        self: &Arc<Self>,
        req: &SpawnRequest,
        timings: &Timings,
    ) -> Result<(Arc<Session>, Opened), AdapterError> {
        let cwd = req.cwd.to_string_lossy().into_owned();
        let model = req.settings.model.clone();
        let mut reset: Option<String> = None;
        if let Some(r) = &req.resume {
            let sid = r.agent_session_id.clone();
            if let Some(existing) = self.sessions.lock().expect("sessions lock").get(&sid).cloned() {
                if existing.is_attached() {
                    return Err(AdapterError::Protocol(format!(
                        "session {sid} is still attached to a running handle; it must be shut down before it is resumed"
                    )));
                }
                return Ok((existing, Opened::Resumed));
            }
            let caps = self.caps();
            let params = json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] });
            let result = if caps.can_resume() {
                Some(self.rpc.request("session/resume", params, timings.session).await)
            } else if caps.load_session {
                // 重放期间的更新一律吞掉：先登记，响应回来才放行
                let s = Session::new(&sid, &cwd, model.clone(), false, self.rpc.clone());
                s.replaying.store(true, Ordering::SeqCst);
                self.sessions.lock().expect("sessions lock").insert(sid.clone(), s.clone());
                let r = self.rpc.request("session/load", params, timings.session).await;
                match &r {
                    Ok(v) => {
                        s.finish_replay(v);
                        return Ok((s, Opened::Resumed));
                    }
                    Err(_) => {
                        self.sessions.lock().expect("sessions lock").remove(&sid);
                    }
                }
                Some(r)
            } else {
                None
            };
            match result {
                Some(Ok(v)) => {
                    let s = Session::new(&sid, &cwd, model, false, self.rpc.clone());
                    s.set_state(ConfigState::from_response(&v));
                    self.sessions.lock().expect("sessions lock").insert(sid, s.clone());
                    return Ok((s, Opened::Resumed));
                }
                Some(Err(RpcFailure::Rpc(e))) if is_session_gone(&e) => {
                    tracing::warn!(session = %sid, error = %e.message, "acp: the session cannot be resumed; starting a new one");
                    reset = Some(e.message);
                }
                Some(Err(RpcFailure::Rpc(e))) if e.code == AUTH_REQUIRED => {
                    return Err(AdapterError::NotAuthenticated(auth_message(&self.label, &self.init, &e)));
                }
                Some(Err(e)) => return Err(startup_failure(&self.child, &self.state, e, "session/resume").await),
                None => {
                    reset = Some("the agent can neither resume nor load sessions".to_string());
                }
            }
        }
        let v = match self.rpc.request("session/new", json!({ "cwd": cwd, "mcpServers": [] }), timings.session).await {
            Ok(v) => v,
            Err(RpcFailure::Rpc(e)) if e.code == AUTH_REQUIRED => {
                return Err(AdapterError::NotAuthenticated(auth_message(&self.label, &self.init, &e)));
            }
            Err(e) => return Err(startup_failure(&self.child, &self.state, e, "session/new").await),
        };
        let Some(sid) = v.get("sessionId").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
            return Err(AdapterError::Protocol("session/new: the response has no sessionId".into()));
        };
        let s = Session::new(&sid, &cwd, model, true, self.rpc.clone());
        s.set_state(ConfigState::from_response(&v));
        self.sessions.lock().expect("sessions lock").insert(sid, s.clone());
        Ok((s, reset.map(Opened::ContextReset).unwrap_or(Opened::New)))
    }

    /// 会话脱离（句柄 `shutdown`）。agent 有 `session/close` 就关掉（取消还在跑的活、释放资源）；没有就让它留在进程里
    /// （之后 resume 时直接接上），除非它正在忙（留着一个还在跑 turn 的会话没法再接）。
    pub async fn detach_session(&self, session: &Arc<Session>, busy: bool, timings: &Timings) {
        session.detach();
        if self.is_dead() {
            return;
        }
        let close = self.caps().can_close();
        if close {
            match self.rpc.request("session/close", json!({ "sessionId": session.id }), timings.close).await {
                Ok(_) => {}
                // 幂等：会话已经不在了也算关掉了
                Err(RpcFailure::Rpc(e)) if e.code == RESOURCE_NOT_FOUND => {}
                Err(e) => tracing::warn!(session = %session.id, error = %e, "acp: session/close failed"),
            }
        }
        if close || busy {
            self.sessions.lock().expect("sessions lock").remove(&session.id);
        }
    }

    /// 关进程：关 stdin → 等 → SIGINT → 宽限期 → SIGKILL（整个进程组）。等它退完。
    pub async fn shutdown(&self, timings: ShutdownTimings) {
        self.state.shutting_down.store(true, Ordering::SeqCst);
        self.child.shutdown(timings).await;
        let reader = self.reader.lock().expect("reader lock").take();
        if let Some(r) = reader {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
    }

    /// 探测用：不登记会话地发一个请求。
    pub async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, RpcFailure> {
        self.rpc.request(method, params, timeout).await
    }
}

/// 握手阶段的失败：进程自己退了就带上它 stderr 的尾巴（让人看得出为什么）。
pub async fn startup_failure(child: &JsonlChild, state: &ConnState, e: RpcFailure, what: &str) -> AdapterError {
    match e {
        RpcFailure::Closed => {
            // 让 stderr 读任务把最后几行读完
            child.wait_exit(Duration::from_millis(500)).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            if let Some(f) = state.fatal.lock().expect("fatal lock").clone() {
                return AdapterError::Spawn(format!("ACP protocol error: {f}, the process was stopped"));
            }
            let tail = crate::common::truncate_bytes(child.stderr_tail().trim(), 400);
            AdapterError::Spawn(format!(
                "the ACP agent exited during {what}{}",
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            ))
        }
        other => map_failure(what, other),
    }
}

/// 认证错误的说明：agent 声明的登录方式。
pub fn auth_message(label: &str, init: &InitializeResult, e: &RpcError) -> String {
    let base =
        if e.message.trim().is_empty() { "authentication required".to_string() } else { e.message.trim().to_string() };
    let names: Vec<&str> = init.auth_methods.iter().map(|m| m.name.as_str()).filter(|n| !n.is_empty()).collect();
    if names.is_empty() {
        format!("{label} is not logged in: {base}")
    } else {
        format!("{label} is not logged in: {base} (login methods: {})", names.join(", "))
    }
}

// ---------- 一个会话 ----------

/// 内部事件队列（不限长）→ 有界的 `events()` 通道。这是唯一会因为消费者不读而阻塞的地方，读任务不会被它拖住。
async fn forward_events(mut queued: mpsc::UnboundedReceiver<AgentEvent>, tx: mpsc::Sender<AgentEvent>) {
    while let Some(ev) = queued.recv().await {
        if tx.send(ev).await.is_err() {
            break;
        }
    }
}

/// agent 里的一个会话，读任务和 `AcpHandle` 共用。
pub struct Session {
    pub id: String,
    pub cwd: String,
    rpc: Arc<Rpc>,
    pub translator: Mutex<Translator>,
    pub state: Mutex<ConfigState>,
    events: Mutex<Option<mpsc::UnboundedSender<AgentEvent>>>,
    /// anycon 的 request_id → 原生上下文。
    pub pending: Mutex<HashMap<String, NativePending>>,
    pub busy: watch::Sender<bool>,
    /// `session/load` 在重放历史：更新一律吞掉。
    replaying: AtomicBool,
    exited: AtomicBool,
}

impl Session {
    fn new(id: &str, cwd: &str, model: Option<String>, fresh: bool, rpc: Arc<Rpc>) -> Arc<Self> {
        let (busy, _) = watch::channel(false);
        Arc::new(Session {
            id: id.to_string(),
            cwd: cwd.to_string(),
            rpc,
            translator: Mutex::new(Translator::new(cwd.to_string(), model, fresh)),
            state: Mutex::new(ConfigState::default()),
            events: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            busy,
            replaying: AtomicBool::new(false),
            exited: AtomicBool::new(false),
        })
    }

    fn set_state(&self, state: ConfigState) {
        let model = state.current_model();
        *self.state.lock().expect("state lock") = state;
        if let Some(m) = model {
            self.translator.lock().expect("translator lock").set_model(Some(m));
        }
    }

    /// `session/load` 响应回来：重放结束。响应里有选项状态就换上（重放期间 agent 发的更新已经应用过了）。
    fn finish_replay(&self, response: &Value) {
        let fresh = ConfigState::from_response(response);
        if !fresh.options.is_empty() || !fresh.modes.is_empty() {
            self.set_state(fresh);
        }
        self.replaying.store(false, Ordering::SeqCst);
    }

    pub fn is_attached(&self) -> bool {
        self.events.lock().expect("events lock").is_some()
    }

    /// 挂上一个句柄：返回它的事件接收端。
    pub fn attach(&self) -> mpsc::Receiver<AgentEvent> {
        let (tx, rx) = mpsc::channel(1024);
        let (queue, queued) = mpsc::unbounded_channel();
        tokio::spawn(forward_events(queued, tx));
        *self.events.lock().expect("events lock") = Some(queue);
        rx
    }

    /// 句柄走了：之后的事件丢弃，内部队列关闭（`events()` 读完就是 `None`）。
    pub fn detach(&self) {
        self.events.lock().expect("events lock").take();
    }

    pub fn emit(&self, ev: AgentEvent) {
        if let Some(tx) = self.events.lock().expect("events lock").as_ref() {
            let _ = tx.send(ev);
        }
    }

    pub fn emit_event(&self, e: Event) {
        self.emit(AgentEvent::Event(e));
    }

    pub fn is_busy(&self) -> bool {
        *self.busy.borrow()
    }

    /// 把翻译器的输出送出去。`TurnDone` 之前先清 busy：消费者收到后可以立刻 `send` 下一条。
    pub fn dispatch(&self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Event(ev) => {
                    if let AgentEvent::Event(Event::TurnDone { .. }) = &ev {
                        self.pending.lock().expect("pending lock").clear();
                        self.busy.send_replace(false);
                    }
                    self.emit(ev);
                }
                Out::Permission { pending, native } => {
                    self.pending.lock().expect("pending lock").insert(pending.request_id.clone(), native);
                    self.emit(AgentEvent::Permission(pending));
                }
                Out::Reply { id, result } => {
                    let ok = match result {
                        Ok(v) => self.rpc.reply(&id, v),
                        Err(e) => self.rpc.reply_error(&id, &e),
                    };
                    if !ok {
                        tracing::debug!("acp: could not answer an agent request (the process is gone)");
                    }
                }
            }
        }
    }

    /// 一条 `session/update` 的 `update`。
    fn on_update(&self, update: &Value) {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("config_option_update") => {
                if let Some(opts) = update.get("configOptions") {
                    let model = {
                        let mut st = self.state.lock().expect("state lock");
                        st.replace_options(opts);
                        st.current_model()
                    };
                    if let Some(m) = model {
                        self.translator.lock().expect("translator lock").set_model(Some(m));
                    }
                }
            }
            Some("current_mode_update") => {
                if let Some(id) = update.get("currentModeId").and_then(Value::as_str) {
                    self.state.lock().expect("state lock").set_current_mode(id);
                }
            }
            _ => {}
        }
        if self.replaying.load(Ordering::SeqCst) {
            return;
        }
        let outs = self.translator.lock().expect("translator lock").on_update(update);
        self.dispatch(outs);
    }

    fn on_request(&self, id: Value, method: &str, params: &Value) {
        if self.replaying.load(Ordering::SeqCst) {
            let result = if method == permissions::M_ELICITATION {
                json!({ "action": "cancel" })
            } else {
                json!({ "outcome": { "outcome": "cancelled" } })
            };
            self.rpc.reply(&id, result);
            return;
        }
        let outs = {
            let mut t = self.translator.lock().expect("translator lock");
            if method == permissions::M_ELICITATION {
                t.on_elicitation(id, params)
            } else {
                t.on_permission(id, params)
            }
        };
        self.dispatch(outs);
    }

    /// agent 取消了它自己发的请求（`$/cancel_request`）：按取消答复，从未决里摘掉。
    fn on_agent_cancelled(&self, request_id: &Value) {
        let found = {
            let mut p = self.pending.lock().expect("pending lock");
            let key = p.iter().find(|(_, n)| &n.rpc_id == request_id).map(|(k, _)| k.clone());
            key.and_then(|k| p.remove(&k))
        };
        if let Some(native) = found {
            if let Ok(v) = permissions::build_response(&native, &Default::default(), true) {
                self.rpc.reply(&native.rpc_id, v);
            }
        }
    }

    /// `session/prompt` 有了结局。
    pub fn finish_prompt(&self, result: Result<Value, RpcFailure>) {
        let outcome = match result {
            Ok(v) => {
                PromptOutcome::StopReason(v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn").to_string())
            }
            Err(RpcFailure::Rpc(e)) => PromptOutcome::Error(e),
            // 进程没了：收尾由读任务的退出路径做
            Err(RpcFailure::Closed | RpcFailure::Timeout) => return,
        };
        let outs = self.translator.lock().expect("translator lock").finish_turn(outcome);
        self.dispatch(outs);
    }

    /// 进程退出：收尾，通知句柄。`expected`：我们自己在关（`shutdown`）。
    fn on_process_exit(&self, label: &str, exit: &ExitInfo, stderr: &str, expected: bool, fatal: Option<&str>) {
        if self.exited.swap(true, Ordering::SeqCst) {
            return;
        }
        let fin = self.translator.lock().expect("translator lock").finish_process();
        self.dispatch(fin.out);
        self.busy.send_replace(false);
        self.pending.lock().expect("pending lock").clear();
        let detail = {
            let tail = crate::common::truncate_bytes(stderr.trim(), 400);
            if tail.is_empty() {
                exit.describe()
            } else {
                format!("{}: {tail}", exit.describe())
            }
        };
        let expected = expected || fin.cancelled;
        let error = if let Some(f) = fatal {
            // 协议错误：不管是不是正在关闭都要报
            Some(format!("{label} protocol error: {f}, the process was stopped ({detail})"))
        } else if expected || (exit.code == Some(0) && !fin.died_mid_turn) {
            None
        } else {
            Some(format!("{label} exited unexpectedly ({detail})"))
        };
        if fin.died_mid_turn {
            let message = match fatal {
                Some(f) => format!("{label} protocol error: {f}, so it was stopped ({detail})"),
                None => format!("{label} exited in the middle of a turn ({detail})"),
            };
            self.emit_event(Event::Error { code: "agent_exited".to_string(), message, recoverable: true });
        }
        self.emit(AgentEvent::Exited { code: exit.code, error });
        self.detach();
    }

    pub fn resume_handle(&self, launch_args: Value) -> ResumeHandle {
        ResumeHandle { agent_session_id: self.id.clone(), launch_args }
    }
}

// ---------- 读任务 ----------

fn session_of(sessions: &Mutex<HashMap<String, Arc<Session>>>, params: &Value) -> Option<Arc<Session>> {
    let sid = params.get("sessionId").and_then(Value::as_str)?;
    sessions.lock().expect("sessions lock").get(sid).cloned()
}

fn on_notification(sessions: &Mutex<HashMap<String, Arc<Session>>>, method: &str, params: &Value) {
    match method {
        "session/update" => match (session_of(sessions, params), params.get("update")) {
            (Some(s), Some(update)) => s.on_update(update),
            _ => tracing::debug!("acp: a session update for a session this connection does not know"),
        },
        "$/cancel_request" => {
            if let Some(id) = params.get("requestId") {
                let all: Vec<Arc<Session>> = sessions.lock().expect("sessions lock").values().cloned().collect();
                for s in all {
                    s.on_agent_cancelled(id);
                }
            }
        }
        // URL 模式 elicitation 的完成通知：用户已经在权限卡片上确认过了，没有别的要做
        "elicitation/complete" => {}
        other => tracing::debug!(method = other, "acp: ignoring a notification"),
    }
}

fn on_request(rpc: &Rpc, sessions: &Mutex<HashMap<String, Arc<Session>>>, id: Value, method: &str, params: &Value) {
    match method {
        permissions::M_REQUEST_PERMISSION | permissions::M_ELICITATION => match session_of(sessions, params) {
            Some(s) => s.on_request(id, method, params),
            None => {
                // 不认识的会话 / 请求级的 elicitation（不属于任何会话）：没人能答复
                let result = if method == permissions::M_ELICITATION {
                    json!({ "action": "decline" })
                } else {
                    json!({ "outcome": { "outcome": "cancelled" } })
                };
                rpc.reply(&id, result);
            }
        },
        // 我们没有声明 fs / terminal，agent 用它自己的工具；自定义（`_` 开头）的方法同理
        other => {
            tracing::debug!(method = other, "acp: refusing an agent request the client does not support");
            rpc.reply_error(&id, &RpcError::new(METHOD_NOT_FOUND, format!("method not supported: {other}")));
        }
    }
}

async fn read_loop(
    label: String,
    child: Arc<JsonlChild>,
    rpc: Arc<Rpc>,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    state: Arc<ConnState>,
    mut lines: mpsc::Receiver<LineEvent>,
) {
    while let Some(ev) = lines.recv().await {
        let line = match ev {
            LineEvent::Line(line) => line,
            LineEvent::Oversize { len, prefix } => {
                // 超长的通知（一个很大的工具输出）丢掉；待答复的请求、响应或认不出的才是协议错误：
                // 丢了它们 agent / 我们会一直等
                let limit = DEFAULT_MAX_LINE_BYTES;
                let fatal = match classify_oversize(&prefix) {
                    OversizeKind::Notification { method } => {
                        tracing::warn!(method, len, limit, "acp: dropping a notification over the line limit");
                        continue;
                    }
                    OversizeKind::Request { method } => {
                        format!(
                            "a request ({method}) was {len} bytes, over the {limit}-byte limit and cannot be answered"
                        )
                    }
                    OversizeKind::Response => format!("a response was {len} bytes, over the {limit}-byte limit"),
                    OversizeKind::Unknown => {
                        format!("a line of {len} bytes, over the {limit}-byte limit, could not be identified")
                    }
                };
                tracing::error!(%fatal, "acp: protocol error, stopping the process");
                state.fatal.lock().expect("fatal lock").get_or_insert(fatal);
                child.kill();
                continue;
            }
        };
        match classify(&line) {
            Some(Incoming::Response { id, result }) => rpc.resolve(id, result),
            Some(Incoming::Notification { method, params }) => on_notification(&sessions, &method, &params),
            Some(Incoming::Request { id, method, params }) => on_request(&rpc, &sessions, id, &method, &params),
            None => {
                if !line.trim().is_empty() {
                    tracing::debug!(len = line.len(), "acp: ignoring a non-JSON line on stdout");
                }
            }
        }
    }

    // stdout 关闭：等进程真正退出；等待请求的人立刻得到 Closed。
    state.dead.store(true, Ordering::SeqCst);
    rpc.close();
    let exit = match child.wait_exit(Duration::from_secs(5)).await {
        Some(i) => i,
        None => {
            child.kill();
            child.wait_exit_forever().await
        }
    };
    // stderr 读任务可能还差最后几行。
    tokio::time::sleep(Duration::from_millis(50)).await;
    let stderr = child.stderr_tail();
    let expected = state.shutting_down.load(Ordering::SeqCst);
    let fatal = state.fatal.lock().expect("fatal lock").clone();
    let all: Vec<Arc<Session>> = sessions.lock().expect("sessions lock").values().cloned().collect();
    for s in all {
        s.on_process_exit(&label, &exit, &stderr, expected, fatal.as_deref());
    }
}
