//! `AgentHandle` 的 Codex 实现：一个 `codex app-server` 进程（每个 anycon 会话一个，ADR-26 修订）+ 一个读 stdout 的任务
//! + 一个把事件转进 `events()` 通道的任务。
//!
//! 数据流：stdout 行 →（`rpc::classify`）→ 响应交给 [`RpcClient`]，通知 / 服务端请求交给 `Translator` → `AgentEvent`；
//! `send` / `respond_permission` / `cancel` → JSON-RPC 请求或答复写进 stdin。
//! **读任务只管读、分类、处理响应和服务端请求**；事件进一个不限长的内部队列（句柄只留弱引用），由 `forward_events` 往有界的
//! `events()` 通道（1024）里送。daemon 在等 `send` / `respond_permission` / `set_settings` / `cancel` 返回时不读通道（`AgentHandle`
//! 的不阻塞约定），所以通道满了也不能拖住读任务，否则 `turn/interrupt` 的响应读不到、`cancel` 会卡死。`TurnDone` 的 busy 标志在
//! 事件入队的那一刻就清了，而不是等消费者读到。
//!
//! **拉起 = 握手做完才返回**：`spawn` 里 `initialize`（带 `optOutNotificationMethods`，让 app-server 不发 `turn/diff/updated` 这类
//! 不用又可能很大的通知）→ `initialized` → `thread/start`（新会话）或 `thread/resume`（续接），然后发
//! `Started { agent_session_id = thread id }`。**续接不上就改开新 thread，并明说**：`thread/resume` 报 `no rollout found`（这个 thread
//! 从来没有记录过用户消息，比如第一个 turn 在写下用户消息之前就失败了）或 `thread not found`（rollout 被删 / 被 Codex 归档了）就
//! 回退到 `thread/start`，`Started` 带新 id，紧跟一条可恢复的 `error { code: "agent_context_reset" }`（不让 turn 失败；之前的上下文
//! agent 看不到了，不说的话用户还以为它记得）。其它 resume 错误照常报错。
//!
//! **turn**：`send` 在没有 turn 时发 `turn/start`（等响应，错误当场返回），turn 进行中且 `interject = true` 时发 `turn/steer`。
//! **插话绝不自己开 turn**：没有进行中的 turn（包括 turn 刚结束、`TurnDone` 还在队列里没被 daemon 读到）、`turn/steer` 报
//! `no active turn` / `expected active turn id`、**以及 `cancel` 已经发出而 turn 还没收尾**，一律返回 [`AdapterError::TurnEnded`]，
//! daemon 把消息退回队列、作为下一个 turn 发。最后一种不发 `turn/steer` 是因为真实 codex 会接受它，随后的 interrupt 却把还没被模型读到的
//! steer 消息连同 turn 一起丢掉（见 README）。每个 `turn/start` 都带上当前的 model / effort / 审批 / 摘要，
//! 沙箱 / 服务档位只在变了才带（见 `process.rs`），所以改这些设置**立刻生效**（下一个 turn 起），不需要重启。
//! `cancel`：先把未答复的审批回 `cancel`，再 `turn/interrupt`，3 秒内没有 `turn/completed` 就补一个 SIGINT。
//!
//! **`set_settings`**：传入的是完整的目标设置（daemon 一直传合并后的整份）。`system_prompt_append` 只能在 thread 创建时给；
//! 之后的改动用 `thread/inject_items` 追加一条 developer 消息（`thread/resume` 会忽略新的 `developerInstructions`）。
//! 只有 `env` 变了才要求重启。
//!
//! **超长的行**：stdout 单行上限 8 MiB，用 `JsonlChild::spawn_events` 读，超长的行整行丢弃、以 `LineEvent::Oversize`（带前 4 KiB 前缀）
//! 交给读任务分类（`rpc::classify_oversize`）。**通知**（大补丁的 `item/started` / `item/completed` 等）丢掉，用前缀里认得出的信息补一个
//! "details too large to show" 的 `tool_call` / `tool_result`（超长的文本 / 思考补 `text_done` / `thinking_done`），turn 照常跑完；
//! 只有**待答复的服务端请求**、**对我们某个请求的响应**或**前缀里认不出是什么**的超长行才是协议错误：杀进程，
//! `Exited` 带 `codex protocol error: …`。
//!
//! 关闭顺序：关 stdin → 短暂等待 → SIGINT → 5 秒 → SIGKILL（整个进程组），见 `JsonlChild::shutdown`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::{
    AdapterConfig, AdapterError, AgentEvent, AgentHandle, PermissionDecision, Result, ResumeHandle, SettingsApply,
    SpawnRequest, UserInput,
};
use anycon_protocol::{AgentInfo, Event, SessionSettings};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::common::{JsonlChild, LineEvent, ShutdownTimings, SpawnSpec, DEFAULT_MAX_LINE_BYTES};

use super::permissions::{self, NativePending};
use super::probe::initialize_params;
use super::process::{self, LaunchOptions, Sent};
use super::rpc::{classify, classify_oversize, Incoming, OversizeKind, RpcClient, RpcFailure};
use super::translate::{Out, Translator};
use super::wire::{ThreadResponse, TurnStartResponse};

/// 各种等待时间。生产用默认值；测试里缩短。
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// `turn/interrupt` 之后多久没等到 `turn/completed` 就补 SIGINT。
    pub interrupt_fallback: Duration,
    /// 握手（`initialize`、`thread/start`、`thread/resume`）每一步的上限。
    pub handshake: Duration,
    /// 普通请求（`turn/start`、`turn/steer`、`turn/interrupt`、`thread/inject_items`）的上限。
    pub request: Duration,
    pub shutdown: ShutdownTimings,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            interrupt_fallback: Duration::from_secs(3),
            handshake: Duration::from_secs(30),
            request: Duration::from_secs(30),
            shutdown: ShutdownTimings::default(),
        }
    }
}

struct Shared {
    translator: Mutex<Translator>,
    rpc: RpcClient,
    busy: watch::Sender<bool>,
    /// anycon 的 request_id → 原生上下文。
    pending: Mutex<HashMap<String, NativePending>>,
    shutting_down: AtomicBool,
    resume: Mutex<ResumeHandle>,
    /// 读任务发现的、必须结束这个进程的协议错误（比如待答复的请求超长）。
    fatal: Mutex<Option<String>>,
}

pub struct CodexHandle {
    child: Arc<JsonlChild>,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<AgentEvent>,
    /// 内部事件队列（不限长）的弱引用：`set_settings` 改了 `launch_args` 之后用它补发一个 `Started`，让 daemon 落库。
    /// 不能持有强引用，否则读任务结束后通道关不上。
    events_tx: mpsc::WeakUnboundedSender<AgentEvent>,
    reader: Option<JoinHandle<()>>,
    thread_id: String,
    /// 当前状态：启动选项 + 之后 `set_settings` 记下的变化。
    current: LaunchOptions,
    /// 线程里当前带着的沙箱 / 服务档位（见 `process::Sent`）。
    sent: Sent,
    /// thread 已经知道的 `system_prompt_append`。
    instructions_applied: Option<String>,
    config: AdapterConfig,
    info: Arc<AgentInfo>,
    timings: Timings,
}

fn map_spawn_error(program: &std::path::Path, e: std::io::Error) -> AdapterError {
    match e.kind() {
        std::io::ErrorKind::NotFound => AdapterError::NotInstalled(format!("{}: {e}", program.display())),
        _ => AdapterError::Spawn(format!("{}: {e}", program.display())),
    }
}

fn map_failure(what: &str, e: RpcFailure) -> AdapterError {
    match e {
        RpcFailure::Rpc(err) => AdapterError::Protocol(format!("{what}: {}", err.message)),
        RpcFailure::Timeout => AdapterError::Protocol(format!("{what}: timed out waiting for codex")),
        RpcFailure::Closed => AdapterError::Exited(format!("{what}: codex is not running")),
    }
}

/// 握手阶段的失败：进程自己退了就带上它 stderr 的尾巴（给用户看得出为什么）。
async fn startup_failure(shared: &Shared, child: &JsonlChild, e: RpcFailure, what: &str) -> AdapterError {
    match e {
        RpcFailure::Closed => {
            // 让 stderr 读任务把最后几行读完
            child.wait_exit(Duration::from_millis(500)).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            if let Some(f) = shared.fatal.lock().expect("fatal lock").clone() {
                return AdapterError::Spawn(format!("codex protocol error: {f}, the process was stopped"));
            }
            let tail = crate::common::truncate_bytes(child.stderr_tail().trim(), 400);
            AdapterError::Spawn(format!(
                "codex app-server exited during startup{}",
                if tail.is_empty() { String::new() } else { format!(": {tail}") }
            ))
        }
        other => map_failure(what, other),
    }
}

fn is_no_rollout(e: &RpcFailure) -> bool {
    matches!(e, RpcFailure::Rpc(err) if err.message.contains("no rollout found") || err.message.contains("thread not found"))
}

impl CodexHandle {
    pub(crate) async fn start(
        req: &SpawnRequest,
        opts: LaunchOptions,
        program: std::path::PathBuf,
        prefix_args: Vec<String>,
        timings: Timings,
    ) -> Result<Self> {
        if !req.cwd.is_dir() {
            return Err(AdapterError::Spawn(format!("working directory does not exist: {}", req.cwd.display())));
        }
        opts.validate()?;
        let mut args = prefix_args;
        args.extend(process::app_server_args());
        let spec = SpawnSpec {
            program: program.clone(),
            args,
            cwd: req.cwd.clone(),
            env: (*req.env).clone(),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        };
        let (child, lines) = JsonlChild::spawn_events(&spec).map_err(|e| map_spawn_error(&program, e))?;
        let child = Arc::new(child);

        let (busy, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            translator: Mutex::new(Translator::new(req.cwd.to_string_lossy().into_owned(), opts.model.clone())),
            rpc: RpcClient::new(child.clone()),
            busy,
            pending: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            resume: Mutex::new(ResumeHandle { agent_session_id: String::new(), launch_args: Value::Null }),
            fatal: Mutex::new(None),
        });
        // 读任务（读行 + 分类 + 处理响应 / 请求）不能被事件转发挡住：事件先进一个不限长的队列，由另一个任务往有界通道里送。
        // daemon 在等 send / cancel / set_settings 的返回时不读 `events()`，通道满了也不能卡住响应（不阻塞约定）。
        let (tx, rx) = mpsc::channel(1024);
        let (queue, queued) = mpsc::unbounded_channel();
        let events_tx = queue.downgrade();
        tokio::spawn(forward_events(queued, tx));
        let reader = tokio::spawn(read_loop(shared.clone(), child.clone(), lines, queue.clone()));

        let mut handle = Self {
            child,
            shared,
            rx,
            events_tx,
            reader: Some(reader),
            thread_id: String::new(),
            sent: opts.initial_sent(),
            current: opts,
            instructions_applied: None,
            config: req.config.clone(),
            info: req.agent_info.clone(),
            timings,
        };
        let context_reset = match handle.handshake(&req.cwd).await {
            Ok(why) => why,
            Err(e) => {
                // 起不来：别留进程
                handle.child.close_stdin();
                handle.child.kill();
                return Err(e);
            }
        };
        let resume = handle.shared.resume.lock().expect("resume lock").clone();
        let _ = queue.send(AgentEvent::Started { resume });
        if let Some(why) = context_reset {
            // 续接不上原来的 thread，开了新的：可恢复的提示，不让 turn 失败
            let _ = queue.send(AgentEvent::Event(Event::Error {
                code: "agent_context_reset".to_string(),
                message: format!(
                    "Codex could not restore this conversation ({why}), so it started a new thread: the agent can no longer see what was said before."
                ),
                recoverable: true,
            }));
        }
        Ok(handle)
    }

    /// `initialize` → `initialized` → `thread/start` | `thread/resume`。
    /// 返回 `Some(原因)` 表示续接不上原来的 thread、开了新的（`agent_context_reset`）。
    async fn handshake(&mut self, cwd: &std::path::Path) -> Result<Option<String>> {
        let rpc = &self.shared.rpc;
        let child = &self.child;
        let t = self.timings.handshake;
        if let Err(e) = rpc.request("initialize", initialize_params(), t).await {
            return Err(startup_failure(&self.shared, child, e, "initialize").await);
        }
        rpc.notify("initialized");

        let mut resumed: Option<String> = self.current.resume_id.clone();
        let mut reset_reason: Option<String> = None;
        let mut response: Option<ThreadResponse> = None;
        if let Some(id) = &resumed {
            let params = self.current.thread_resume_params(id, cwd);
            match rpc.request("thread/resume", params, t).await {
                Ok(v) => response = Some(parse_thread(&v, "thread/resume")?),
                Err(e) if is_no_rollout(&e) => {
                    let why = match &e {
                        RpcFailure::Rpc(err) => err.message.clone(),
                        other => other.to_string(),
                    };
                    tracing::warn!(thread = %id, reason = %why, "codex: the thread cannot be resumed; starting a new thread");
                    reset_reason = Some(why);
                    resumed = None;
                }
                Err(e) => return Err(startup_failure(&self.shared, child, e, "thread/resume").await),
            }
        }
        if response.is_none() {
            let params = self.current.thread_start_params(cwd);
            match rpc.request("thread/start", params, t).await {
                Ok(v) => response = Some(parse_thread(&v, "thread/start")?),
                Err(e) => return Err(startup_failure(&self.shared, child, e, "thread/start").await),
            }
        }
        let response = response.expect("thread response");
        self.thread_id = response.thread.id.clone();
        if let Some(m) = response.model.clone().filter(|_| self.current.model.is_none()) {
            self.shared.translator.lock().expect("translator lock").set_model(Some(m));
        }

        // 指令：新 thread 已经带着；续接的 thread 持久化的是旧指令，变了就追加一条 developer 消息
        let desired = self.current.system_prompt_append.clone();
        self.instructions_applied = desired.clone();
        if resumed.is_some() {
            if let Some(known) = self.current.resume_instructions.clone() {
                if known != desired {
                    if let Err(e) = self.inject_instructions(&desired).await {
                        tracing::warn!(error = %e, "codex: could not tell the resumed thread about the new instructions");
                        self.instructions_applied = known;
                    }
                }
            }
        }
        let launch_args = self.current.to_json(&self.instructions_applied);
        *self.shared.resume.lock().expect("resume lock") =
            ResumeHandle { agent_session_id: self.thread_id.clone(), launch_args };
        Ok(reset_reason)
    }

    async fn inject_instructions(&self, new: &Option<String>) -> Result<()> {
        let params = json!({ "threadId": self.thread_id, "items": [process::instructions_notice(new)] });
        self.shared
            .rpc
            .request("thread/inject_items", params, self.timings.request)
            .await
            .map(|_| ())
            .map_err(|e| map_failure("thread/inject_items", e))
    }

    fn busy(&self) -> bool {
        *self.shared.busy.borrow()
    }

    /// 把新的当前状态记下来，并让 `resume_handle()` 和 daemon 库里的 `launch_args` 都跟上
    /// （补发一个 `Started`，daemon 收到会更新并落库；尽力而为，daemon 在 shutdown 时还会用 `resume_handle()` 再取一次）。
    fn commit_launch_args(&self) {
        let launch_args = self.current.to_json(&self.instructions_applied);
        let resume = {
            let mut r = self.shared.resume.lock().expect("resume lock");
            if r.launch_args == launch_args {
                return;
            }
            r.launch_args = launch_args;
            r.clone()
        };
        if let Some(tx) = self.events_tx.upgrade() {
            if tx.send(AgentEvent::Started { resume }).is_err() {
                tracing::debug!("codex: could not announce the updated launch_args");
            }
        }
    }

    /// 等 codex 的 turn id 出现（`turn/start` 的响应或 `turn/started`），最多 `max`。
    async fn wait_for_turn_id(&self, max: Duration) -> Option<String> {
        let start = std::time::Instant::now();
        loop {
            if let Some(id) = self.shared.translator.lock().expect("translator lock").codex_turn_id() {
                return Some(id.to_string());
            }
            if !self.busy() || start.elapsed() >= max {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn start_turn(&mut self, items: Vec<Value>) -> Result<()> {
        self.shared.translator.lock().expect("translator lock").begin_turn();
        self.shared.busy.send_replace(true);
        let (mut params, next_sent) = self.current.turn_overrides(&self.sent);
        params.insert("threadId".into(), json!(self.thread_id));
        params.insert("input".into(), Value::Array(items));
        match self.shared.rpc.request("turn/start", Value::Object(params), self.timings.request).await {
            Ok(v) => {
                if let Ok(r) = serde_json::from_value::<TurnStartResponse>(v) {
                    self.shared.translator.lock().expect("translator lock").set_codex_turn_id(&r.turn.id);
                }
                self.sent = next_sent;
                Ok(())
            }
            Err(e) => {
                self.shared.translator.lock().expect("translator lock").abort_turn();
                self.shared.busy.send_replace(false);
                Err(map_failure("turn/start", e))
            }
        }
    }

    /// 插话：只送进**正在进行的** turn。没有进行中的 turn（包括 `turn/steer` 报 `no active turn`）一律是
    /// [`AdapterError::TurnEnded`]，**绝不自己开新 turn**：daemon 会把消息退回队列，按正常流程作为下一个 turn 发出。
    ///
    /// **已发出 `cancel` 但 turn 还没结束时也是 `TurnEnded`，不发 `turn/steer`**：真实 codex 会接受这条 steer
    /// （响应 `{turnId}`），可是随后的 interrupt 会把还没被模型读到的 steer 消息连同 turn 一起丢掉（既不进下一次请求的
    /// 历史，也没有 `userMessage` 条目）。返回 `TurnEnded` 让 daemon 把消息退回队列，作为下一个 turn 发出，才不会丢。
    /// 反过来，steer 已经被接受之后才到的 cancel 无法追回那条消息（见 README「已知限制」）。
    async fn steer(&mut self, items: Vec<Value>) -> Result<()> {
        if !self.busy() || self.shared.translator.lock().expect("translator lock").is_cancelling() {
            return Err(AdapterError::TurnEnded);
        }
        let Some(turn_id) = self.wait_for_turn_id(Duration::from_secs(2)).await else {
            return Err(if self.busy() {
                AdapterError::Protocol("timed out waiting for Codex to report the current turn".into())
            } else {
                AdapterError::TurnEnded
            });
        };
        let params = json!({ "threadId": self.thread_id, "expectedTurnId": turn_id, "input": items });
        match self.shared.rpc.request("turn/steer", params, self.timings.request).await {
            Ok(_) => Ok(()),
            // turn 在我们发 steer 的同时结束了
            Err(RpcFailure::Rpc(e))
                if e.message.contains("no active turn") || e.message.contains("expected active turn id") =>
            {
                Err(AdapterError::TurnEnded)
            }
            Err(e) => Err(map_failure("turn/steer", e)),
        }
    }
}

fn parse_thread(v: &Value, what: &str) -> Result<ThreadResponse> {
    serde_json::from_value::<ThreadResponse>(v.clone())
        .map_err(|e| AdapterError::Protocol(format!("{what}: unexpected response ({e})")))
}

/// 内部事件队列 → 有界通道。这是唯一会因为消费者不读而阻塞的地方，读任务不会被它拖住。
async fn forward_events(mut queued: mpsc::UnboundedReceiver<AgentEvent>, tx: mpsc::Sender<AgentEvent>) {
    while let Some(ev) = queued.recv().await {
        if tx.send(ev).await.is_err() {
            break;
        }
    }
}

fn dispatch(shared: &Shared, events: &mpsc::UnboundedSender<AgentEvent>, out: Out) {
    match out {
        Out::Event(ev) => {
            if let AgentEvent::Event(Event::TurnDone { .. }) = &ev {
                // 先清 busy 再转发 TurnDone：消费者收到后可以立刻 send 下一条；
                // 反过来，TurnDone 还在队列里时到达的插话会得到 `TurnEnded`，而不是被塞进一个已经结束的 turn。
                shared.pending.lock().expect("pending lock").clear();
                shared.busy.send_replace(false);
            }
            let _ = events.send(ev);
        }
        Out::Permission { pending, native } => {
            shared.pending.lock().expect("pending lock").insert(pending.request_id.clone(), native);
            let _ = events.send(AgentEvent::Permission(pending));
        }
        Out::Reply { id, result } => {
            let ok = match result {
                Ok(v) => shared.rpc.reply(&id, v),
                Err(e) => shared.rpc.reply_error(&id, &e),
            };
            if !ok {
                tracing::debug!("codex: could not answer a server request (the process is gone)");
            }
        }
        Out::Resolved { rpc_id } => {
            shared.pending.lock().expect("pending lock").retain(|_, n| n.rpc_id != rpc_id);
        }
        Out::TurnStarted { .. } => {}
    }
}

/// 超长的行：通知丢弃（补一个「内容没显示」的收尾），待答复的请求 / 请求的响应 / 认不出来的才是协议错误（杀进程）。
fn on_oversize(
    shared: &Shared,
    child: &JsonlChild,
    events: &mpsc::UnboundedSender<AgentEvent>,
    len: usize,
    prefix: &str,
) {
    let limit = DEFAULT_MAX_LINE_BYTES;
    let fatal = match classify_oversize(prefix) {
        OversizeKind::Notification { method } => {
            let outs = shared.translator.lock().expect("translator lock").feed_oversize(&method, prefix, len, limit);
            for out in outs {
                dispatch(shared, events, out);
            }
            return;
        }
        OversizeKind::Request { method } => {
            format!("a server request ({method}) was {len} bytes, over the {limit}-byte limit and cannot be answered")
        }
        OversizeKind::Response => format!("a response was {len} bytes, over the {limit}-byte limit"),
        OversizeKind::Unknown => format!("a line of {len} bytes, over the {limit}-byte limit, could not be identified"),
    };
    tracing::error!(%fatal, "codex: protocol error, stopping the process");
    shared.fatal.lock().expect("fatal lock").get_or_insert(fatal);
    child.kill();
}

async fn read_loop(
    shared: Arc<Shared>,
    child: Arc<JsonlChild>,
    mut lines: mpsc::Receiver<LineEvent>,
    events: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(ev) = lines.recv().await {
        let line = match ev {
            LineEvent::Line(line) => line,
            LineEvent::Oversize { len, prefix } => {
                on_oversize(&shared, &child, &events, len, &prefix);
                continue;
            }
        };
        match classify(&line) {
            Some(Incoming::Response { id, result }) => shared.rpc.resolve(id, result),
            Some(other) => {
                let outs = shared.translator.lock().expect("translator lock").feed_incoming(other);
                for out in outs {
                    dispatch(&shared, &events, out);
                }
            }
            None => {
                if !line.trim().is_empty() {
                    tracing::debug!(len = line.len(), "codex: ignoring a non-JSON line on stdout");
                }
            }
        }
    }

    // stdout 关闭：等进程真正退出；等待请求的人立刻得到 Closed。
    shared.rpc.close();
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

    let fin = shared.translator.lock().expect("translator lock").finish();
    for out in fin.out {
        dispatch(&shared, &events, out);
    }
    shared.busy.send_replace(false);
    shared.pending.lock().expect("pending lock").clear();

    let expected = shared.shutting_down.load(Ordering::SeqCst) || fin.cancelled;
    let fatal = shared.fatal.lock().expect("fatal lock").clone();
    let detail = {
        let tail = crate::common::truncate_bytes(stderr.trim(), 400);
        if tail.is_empty() {
            exit.describe()
        } else {
            format!("{}: {tail}", exit.describe())
        }
    };
    let error = if let Some(f) = &fatal {
        // 协议错误：不管是不是正在关闭都要报
        Some(format!("codex protocol error: {f}, the process was stopped ({detail})"))
    } else if expected || (exit.code == Some(0) && !fin.died_mid_turn) {
        None
    } else {
        Some(format!("codex exited unexpectedly ({detail})"))
    };
    if fin.died_mid_turn {
        let message = match &fatal {
            Some(f) => format!("Codex protocol error: {f}, so it was stopped ({detail})"),
            None => format!("Codex exited in the middle of a turn ({detail})"),
        };
        let _ = events.send(AgentEvent::Event(Event::Error {
            code: "agent_exited".to_string(),
            message,
            recoverable: true,
        }));
    }
    let _ = events.send(AgentEvent::Exited { code: exit.code, error });
}

#[async_trait::async_trait]
impl AgentHandle for CodexHandle {
    fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.rx
    }

    async fn send(&mut self, input: UserInput) -> Result<()> {
        let items = process::input_items(&input)?;
        if self.child.has_exited() {
            return Err(AdapterError::Exited("codex is not running".into()));
        }
        if input.interject {
            return self.steer(items).await;
        }
        if self.busy() {
            // daemon 只在没有 turn 时才不带 `interject` 发消息
            return Err(AdapterError::Unsupported(
                "Codex is already running a turn; a message for it must be sent with interject = true".into(),
            ));
        }
        self.start_turn(items).await
    }

    async fn respond_permission(&mut self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let native = self.shared.pending.lock().expect("pending lock").remove(request_id);
        let Some(native) = native else {
            // 取消 / 重复答复 / codex 自己了结的竞态：幂等。
            tracing::debug!(request_id, "codex: no pending permission with this id");
            return Ok(());
        };
        let cancelling = self.shared.translator.lock().expect("translator lock").is_cancelling();
        let result = match permissions::build_response(&native, &decision, cancelling) {
            Ok(v) => v,
            Err(e) => {
                // 答复不合法：请求仍然未决，放回去。
                self.shared.pending.lock().expect("pending lock").insert(request_id.to_string(), native);
                return Err(e);
            }
        };
        if !self.shared.rpc.reply(&native.rpc_id, result) {
            return Err(AdapterError::Exited("codex is not running".into()));
        }
        Ok(())
    }

    async fn set_settings(&mut self, settings: &SessionSettings) -> Result<SettingsApply> {
        // 所有校验都在做任何事之前，非法设置不会留下半截状态。
        let next = LaunchOptions::resolve_desired(settings, &self.config, &self.info, &self.current)?;
        let restart = next.env != self.current.env;

        // 指令只能在 thread 创建时给：变了就追加一条 developer 消息。失败 = 整个调用失败（daemon 会标记重启，
        // 重启时 `launch_args.instructions_applied` 还是旧值，续接时会再试一次）。
        if next.system_prompt_append != self.instructions_applied {
            self.inject_instructions(&next.system_prompt_append).await?;
            self.instructions_applied = next.system_prompt_append.clone();
        }
        if next.model.is_some() {
            self.shared.translator.lock().expect("translator lock").set_model(next.model.clone());
        }
        self.current = next;
        self.commit_launch_args();
        Ok(SettingsApply { applied_now: !restart, restart_required: restart })
    }

    async fn cancel(&mut self) -> Result<()> {
        if !self.busy() {
            return Ok(());
        }
        self.shared.translator.lock().expect("translator lock").mark_cancel_requested();
        // 未答复的审批回 `cancel`（`decline` 会让 agent 接着干）
        let pending: Vec<(String, NativePending)> = self.shared.pending.lock().expect("pending lock").drain().collect();
        for (_, native) in pending {
            if let Ok(v) = permissions::build_response(&native, &PermissionDecision::default(), true) {
                self.shared.rpc.reply(&native.rpc_id, v);
            }
        }
        if let Some(turn_id) = self.wait_for_turn_id(Duration::from_secs(2)).await {
            let params = json!({ "threadId": self.thread_id, "turnId": turn_id });
            match self.shared.rpc.request("turn/interrupt", params, self.timings.request).await {
                Ok(_) => {}
                // turn 刚好结束了：正好
                Err(RpcFailure::Rpc(e)) if e.message.contains("no active turn") => {}
                Err(e) => tracing::warn!(error = %e, "codex: turn/interrupt failed"),
            }
        }

        // 兜底：一段时间内没有 `turn/completed` 就 SIGINT。只认发起取消的这个 turn：`busy` 是 watch，
        // 收尾后紧接着开始的下一个 turn 会把那一下 false 盖掉，单看 `busy` 会把下一个 turn 一起打断。
        let turn = self.shared.translator.lock().expect("translator lock").turn_id();
        let mut busy_rx = self.shared.busy.subscribe();
        let shared = self.shared.clone();
        let child = self.child.clone();
        let fallback = self.timings.interrupt_fallback;
        tokio::spawn(async move {
            let _ = tokio::time::timeout(fallback, busy_rx.wait_for(|b| !*b)).await;
            let still_running = {
                let t = shared.translator.lock().expect("translator lock");
                t.is_busy() && t.turn_id() == turn
            };
            if still_running && !child.has_exited() {
                tracing::warn!("codex: no turn/completed after interrupt, sending SIGINT");
                child.interrupt();
            }
        });
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if !self.shared.shutting_down.swap(true, Ordering::SeqCst) {
            self.shared.translator.lock().expect("translator lock").mark_cancel_requested();
            self.child.shutdown(self.timings.shutdown).await;
        } else {
            self.child.wait_exit(Duration::from_secs(10)).await;
        }
        // 让读任务把收尾事件（TurnDone / Exited）送进通道再返回。
        if let Some(r) = self.reader.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
        Ok(())
    }

    fn resume_handle(&self) -> Option<ResumeHandle> {
        let r = self.shared.resume.lock().expect("resume lock").clone();
        (!r.agent_session_id.is_empty()).then_some(r)
    }

    fn is_busy(&self) -> bool {
        self.busy()
    }
}

impl Drop for CodexHandle {
    fn drop(&mut self) {
        // 没走 shutdown 就被丢弃：别留孤儿进程。
        self.child.close_stdin();
        self.child.kill();
    }
}
