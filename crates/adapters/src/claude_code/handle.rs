//! `AgentHandle` 的 Claude Code 实现：一个 `claude -p` 进程 + 一个读 stdout 的任务。
//!
//! 数据流：stdout 行 →（`Translator`）→ 不限长的内部队列 →（转发任务）→ 有界的 `AgentEvent` 通道；`send` / `respond_permission` / 控制请求 → stdin。
//! **读任务不能被事件转发挡住**（不阻塞约定，见 `AgentHandle` 的文档）：daemon 在等 `set_model` 之类控制请求的回执期间不读 `events()`，
//! 有界通道满了的话，回执（和它前面的事件在同一条 stdout 上）会读不到，一直等到超时。所以读任务只管读、分类、处理回执，
//! 事件进内部队列，由另一个任务往有界通道里送。
//! 共享状态（是否忙、未决权限、等待中的控制请求）放在内部的 `Shared`，读任务和 handle 都能访问。
//!
//! 关闭顺序：关 stdin → 短暂等待 → SIGINT → 5 秒 → SIGKILL（整个进程组），见 `JsonlChild::shutdown`。
//! 不管怎么退出，leader 一走整个进程组都会被清掉（工具派生的后台进程不留）。
//! 取消：先发 `interrupt` 控制请求，3 秒内没有 `result` 就补一个 SIGINT。
//!
//! **stdout 单行上限**（8 MiB，见 `JsonlChild`）：超长 = 协议错误，进程被杀，事件流里是 `Error { agent_exited }`
//! （turn 进行中时）+ `Exited { error: Some("… protocol error …") }`。
//!
//! **`set_settings` 的语义**：传入的是**完整的目标设置**（daemon 一直传合并后的整份设置），没给的字段 = 清除，
//! 回到配置 / `AgentInfo` 的默认值（或「没有」）。可热生效的（模型、权限模式）按固定顺序逐个应用，
//! 每成功一个就记进当前状态；中途失败时已应用的保留、后面的不动，错误信息里列出哪些已应用。

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
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::common::{JsonlChild, ShutdownTimings, SpawnSpec, DEFAULT_MAX_LINE_BYTES};

use super::input;
use super::permissions::{self, NativePending};
use super::process::{self, LaunchOptions};
use super::translate::{ControlAck, Out, Translator, UserTurn};

/// 各种等待时间。生产用默认值；测试里缩短。
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// `interrupt` 之后多久没等到 `result` 就补 SIGINT。
    pub interrupt_fallback: Duration,
    /// 控制请求（`set_model` 等）等 CLI 回执的上限。CLI 冷启动要几秒，所以给得宽。
    pub control_timeout: Duration,
    pub shutdown: ShutdownTimings,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            interrupt_fallback: Duration::from_secs(3),
            control_timeout: Duration::from_secs(15),
            shutdown: ShutdownTimings::default(),
        }
    }
}

struct Shared {
    translator: Mutex<Translator>,
    busy: watch::Sender<bool>,
    pending: Mutex<HashMap<String, NativePending>>,
    acks: Mutex<HashMap<String, oneshot::Sender<ControlAck>>>,
    shutting_down: AtomicBool,
    resume: Mutex<ResumeHandle>,
}

pub struct ClaudeHandle {
    child: Arc<JsonlChild>,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<AgentEvent>,
    /// 内部事件队列的弱引用：`set_settings` 改了 `launch_args` 之后用它补发一个 `Started`，让 daemon 落库。
    /// 不能持有强引用，否则读任务结束后通道关不上。
    events_tx: mpsc::WeakUnboundedSender<AgentEvent>,
    reader: Option<JoinHandle<()>>,
    /// 进程启动时用的选项（决定能不能热切进 `bypassPermissions`）。
    launch: LaunchOptions,
    /// 当前状态：启动选项 + 之后 `set_settings` 记下的变化（包括要等重启才生效的）。
    current: LaunchOptions,
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

impl ClaudeHandle {
    pub(crate) fn start(
        req: &SpawnRequest,
        opts: LaunchOptions,
        program: std::path::PathBuf,
        prefix_args: Vec<String>,
        timings: Timings,
    ) -> Result<Self> {
        if !req.cwd.is_dir() {
            return Err(AdapterError::Spawn(format!("working directory does not exist: {}", req.cwd.display())));
        }
        // 拼 argv 之前再验一次：不管 `opts` 怎么来的，进 argv 的值都是干净的
        opts.validate()?;
        let mut args = prefix_args;
        args.extend(process::base_args());
        args.extend(opts.to_args());
        let spec = SpawnSpec {
            program: program.clone(),
            args,
            cwd: req.cwd.clone(),
            env: opts.child_env(&req.env),
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        };
        let (child, lines) = JsonlChild::spawn(&spec).map_err(|e| map_spawn_error(&program, e))?;
        let child = Arc::new(child);

        let session_id = opts.resume_id.clone().or_else(|| opts.session_id.clone()).unwrap_or_default();
        let resume = ResumeHandle { agent_session_id: session_id.clone(), launch_args: opts.to_json() };
        let translator =
            Translator::new(req.cwd.to_string_lossy().into_owned(), resume.launch_args.clone(), Some(session_id));
        let (busy, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            translator: Mutex::new(translator),
            busy,
            pending: Mutex::new(HashMap::new()),
            acks: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            resume: Mutex::new(resume.clone()),
        });

        let (tx, rx) = mpsc::channel(256);
        let (queue, queued) = mpsc::unbounded_channel();
        // 我们自己生成 / 指定了会话 id，所以不用等 `system/init`（CLI 要等到第一条用户消息才发它）。
        let _ = queue.send(AgentEvent::Started { resume });
        let events_tx = queue.downgrade();
        tokio::spawn(forward_events(queued, tx));
        let reader = tokio::spawn(read_loop(shared.clone(), child.clone(), lines, queue));
        Ok(Self {
            child,
            shared,
            rx,
            events_tx,
            reader: Some(reader),
            launch: opts.clone(),
            current: opts,
            config: req.config.clone(),
            info: req.agent_info.clone(),
            timings,
        })
    }

    fn busy(&self) -> bool {
        *self.shared.busy.borrow()
    }

    /// 发一个控制请求并等回执。
    async fn control(&self, request: Value) -> Result<ControlAck> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.shared.acks.lock().expect("acks lock").insert(id.clone(), tx);
        let line = json!({ "type": "control_request", "request_id": id, "request": request });
        if !self.child.send_json(&line) {
            self.shared.acks.lock().expect("acks lock").remove(&id);
            return Err(AdapterError::Exited("claude is not running".into()));
        }
        match tokio::time::timeout(self.timings.control_timeout, rx).await {
            Ok(Ok(ack)) => Ok(ack),
            Ok(Err(_)) => Err(AdapterError::Exited("claude exited before answering a control request".into())),
            Err(_) => {
                self.shared.acks.lock().expect("acks lock").remove(&id);
                Err(AdapterError::Protocol("timed out waiting for claude to answer a control request".into()))
            }
        }
    }

    /// `set_model`（`None` = 回 CLI 的默认模型）。
    async fn apply_model(&self, model: Option<&str>) -> Result<()> {
        let ack = self.control(json!({ "subtype": "set_model", "model": model })).await?;
        if ack.ok {
            Ok(())
        } else {
            Err(AdapterError::InvalidSettings {
                field: "model".into(),
                reason: ack.error.unwrap_or_else(|| "claude rejected the model".into()),
            })
        }
    }

    /// `set_permission_mode`。`Ok(true)` = 已热生效；`Ok(false)` = CLI 说启动时没带危险标志，要重启才行。
    async fn apply_mode(&self, mode: &str) -> Result<bool> {
        let ack = self.control(json!({ "subtype": "set_permission_mode", "mode": mode })).await?;
        if ack.ok {
            return Ok(true);
        }
        if ack.error_code.as_deref() == Some("bypass_not_launched") {
            return Ok(false);
        }
        Err(AdapterError::InvalidSettings {
            field: "mode".into(),
            reason: ack.error.unwrap_or_else(|| "claude rejected the permission mode".into()),
        })
    }

    /// 把新的当前状态记下来，并让 `resume_handle()`、翻译器、daemon 的库里的 `launch_args` 都跟上
    /// （daemon 存的快照不过期。`spawn` 的 resume 路径本身已经不读 `launch_args`，不会靠它让旧值复活，但别的消费者——
    /// 诊断、旧版 daemon、以后的 adapter 代码——不该看到过期的值）。
    fn commit(&mut self, cur: LaunchOptions) {
        if cur == self.current {
            return;
        }
        self.current = cur;
        let launch_args = self.current.to_json();
        self.shared.translator.lock().expect("translator lock").set_launch_args(launch_args.clone());
        let resume = {
            let mut r = self.shared.resume.lock().expect("resume lock");
            if r.launch_args == launch_args {
                return;
            }
            r.launch_args = launch_args;
            r.clone()
        };
        // 补发一个 `Started`（daemon 收到会更新 launch_args 并落库）。尽力而为：通道满了就算了，
        // daemon 在 shutdown 时还会用 `resume_handle()` 再取一次。
        if let Some(tx) = self.events_tx.upgrade() {
            if tx.send(AgentEvent::Started { resume }).is_err() {
                tracing::debug!("claude: could not announce the updated launch_args");
            }
        }
    }
}

/// 把「哪些已应用、哪些没应用」写进错误信息。
fn annotate_partial(e: AdapterError, applied: &[&str], not_applied: &[&str]) -> AdapterError {
    let note = format!(
        "applied: {}; not applied: {}",
        if applied.is_empty() { "nothing".to_string() } else { applied.join(", ") },
        if not_applied.is_empty() { "nothing".to_string() } else { not_applied.join(", ") },
    );
    match e {
        AdapterError::InvalidSettings { field, reason } => {
            AdapterError::InvalidSettings { field, reason: format!("{reason} ({note})") }
        }
        AdapterError::Protocol(m) => AdapterError::Protocol(format!("{m} ({note})")),
        AdapterError::Exited(m) => AdapterError::Exited(format!("{m} ({note})")),
        other => other,
    }
}

/// 内部事件队列 → 有界通道。这是唯一会因为消费者不读而阻塞的地方，读任务不会被它拖住。
async fn forward_events(mut queued: mpsc::UnboundedReceiver<AgentEvent>, tx: mpsc::Sender<AgentEvent>) {
    while let Some(ev) = queued.recv().await {
        if tx.send(ev).await.is_err() {
            break;
        }
    }
}

fn dispatch(shared: &Shared, child: &JsonlChild, tx: &mpsc::UnboundedSender<AgentEvent>, out: Out) {
    match out {
        Out::Event(ev) => {
            match &ev {
                // 先清 busy 再转发 TurnDone：消费者收到后可以立刻 send 下一条。
                AgentEvent::Event(Event::TurnDone { .. }) => {
                    shared.pending.lock().expect("pending lock").clear();
                    shared.busy.send_replace(false);
                }
                // CLI 自己开始了一轮：和 `send` 开的 turn 一样算忙（`cancel` 要能打断它）
                AgentEvent::TurnStarted => {
                    shared.busy.send_replace(true);
                }
                AgentEvent::Started { resume } => *shared.resume.lock().expect("resume lock") = resume.clone(),
                _ => {}
            }
            let _ = tx.send(ev);
        }
        Out::Permission { pending, native } => {
            shared.pending.lock().expect("pending lock").insert(pending.request_id.clone(), native);
            let _ = tx.send(AgentEvent::Permission(pending));
        }
        Out::ControlAck(ack) => {
            let waiter = shared.acks.lock().expect("acks lock").remove(&ack.request_id);
            match waiter {
                Some(w) => {
                    let _ = w.send(ack);
                }
                None => {
                    tracing::debug!(request_id = %ack.request_id, ok = ack.ok, "claude: control_response with no waiter")
                }
            }
        }
        Out::ControlCancel { request_id } => {
            shared.pending.lock().expect("pending lock").remove(&request_id);
        }
        // CLI 自己的一轮结束了，暂存的用户消息现在写进去（这一轮的未决权限请求随它一起作废）
        Out::Write(msg) => {
            shared.pending.lock().expect("pending lock").clear();
            child.send_json(&msg);
        }
        Out::Unsupported { request_id, subtype } => {
            tracing::debug!(%subtype, "claude: replying with an error to an unsupported control_request");
            child.send_json(&permissions::control_error(
                &request_id,
                &format!("unsupported control request: {subtype}"),
            ));
        }
    }
}

async fn read_loop(
    shared: Arc<Shared>,
    child: Arc<JsonlChild>,
    mut lines: mpsc::Receiver<String>,
    tx: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(line) = lines.recv().await {
        let outs = shared.translator.lock().expect("translator lock").feed_line(&line);
        for out in outs {
            dispatch(&shared, &child, &tx, out);
        }
    }

    // stdout 关闭：等进程真正退出。
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
        dispatch(&shared, &child, &tx, out);
    }
    shared.busy.send_replace(false);
    shared.pending.lock().expect("pending lock").clear();
    shared.acks.lock().expect("acks lock").clear();

    let expected = shared.shutting_down.load(Ordering::SeqCst) || fin.cancelled;
    let overflow = child.line_overflowed();
    let detail = {
        let tail = crate::common::truncate_bytes(stderr.trim(), 400);
        if tail.is_empty() {
            exit.describe()
        } else {
            format!("{}: {tail}", exit.describe())
        }
    };
    let error = if overflow {
        // 协议错误：不管是不是正在关闭都要报
        Some(format!(
            "claude protocol error: a stdout line was longer than {DEFAULT_MAX_LINE_BYTES} bytes, the process was stopped ({detail})"
        ))
    } else if expected || (exit.code == Some(0) && !fin.died_mid_turn) {
        None
    } else {
        Some(format!("claude exited unexpectedly ({detail})"))
    };
    if fin.died_mid_turn {
        let message = if overflow {
            format!("Claude Code sent a line over the {DEFAULT_MAX_LINE_BYTES}-byte limit and was stopped ({detail})")
        } else {
            format!("Claude Code exited in the middle of a turn ({detail})")
        };
        let _ =
            tx.send(AgentEvent::Event(Event::Error { code: "agent_exited".to_string(), message, recoverable: true }));
    }
    let _ = tx.send(AgentEvent::Exited { code: exit.code, error });
}

#[async_trait::async_trait]
impl AgentHandle for ClaudeHandle {
    fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.rx
    }

    fn pid(&self) -> Option<u32> {
        self.child.pid()
    }

    async fn send(&mut self, input: UserInput) -> Result<()> {
        if input.interject {
            // stream-json 里 turn 进行中收到的用户消息会排到下一个 turn，不是真正的插话（`capabilities.interject = false`）
            return Err(AdapterError::Unsupported("Claude Code cannot take input during a turn".into()));
        }
        if self.child.has_exited() {
            return Err(AdapterError::Exited("claude is not running".into()));
        }
        // 用户的 turn 在跑：不读附件就拒绝。CLI 自己开的一轮在跑不算——消息暂存到这一轮结束（`submit_user_message`）
        if self.shared.translator.lock().expect("translator lock").would_refuse_user_message() {
            return Err(AdapterError::Unsupported("Claude Code cannot take input during a turn".into()));
        }
        // 附件的校验和读文件都在改任何状态之前：失败时没有半截的 turn
        let images = input::load_images(&input).await?;
        let msg = input::user_message(&input.content, images);
        let turn = self.shared.translator.lock().expect("translator lock").submit_user_message(msg);
        match turn {
            UserTurn::Busy => Err(AdapterError::Unsupported("Claude Code cannot take input during a turn".into())),
            UserTurn::Deferred => {
                self.shared.busy.send_replace(true);
                Ok(())
            }
            UserTurn::Write(msg) => {
                self.shared.busy.send_replace(true);
                if !self.child.send_json(&msg) {
                    self.shared.busy.send_replace(false);
                    return Err(AdapterError::Exited("claude is not running".into()));
                }
                Ok(())
            }
        }
    }

    async fn respond_permission(&mut self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let native = self.shared.pending.lock().expect("pending lock").remove(request_id);
        let Some(native) = native else {
            // 取消 / 重复答复的竞态：幂等。
            tracing::debug!(request_id, "claude: no pending permission with this id");
            return Ok(());
        };
        let inner = match permissions::build_decision(&native, &decision) {
            Ok(v) => v,
            Err(e) => {
                // 答复不合法：请求仍然未决，放回去。
                self.shared.pending.lock().expect("pending lock").insert(request_id.to_string(), native);
                return Err(e);
            }
        };
        if !self.child.send_json(&permissions::control_response(request_id, inner)) {
            return Err(AdapterError::Exited("claude is not running".into()));
        }
        Ok(())
    }

    async fn set_settings(&mut self, settings: &SessionSettings) -> Result<SettingsApply> {
        // 1. 目标 = 完整的目标设置（没给的字段 = 清除 / 回默认）。所有校验都在发任何控制请求之前，
        //    非法设置不会留下半截状态。
        let next = LaunchOptions::resolve_desired(settings, &self.config, &self.info, &self.current)?;
        let mut cur = self.current.clone();
        let mut restart = false;

        // 2. 只能在启动时给的字段：记下新值，等 daemon 重启（resume）时生效，不发控制请求。
        if next.thinking != cur.thinking
            || next.system_prompt_append != cur.system_prompt_append
            || next.max_turns != cur.max_turns
            || next.max_budget_usd != cur.max_budget_usd
            || next.fast_mode != cur.fast_mode
            || next.env != cur.env
        {
            restart = true;
        }
        cur.thinking = next.thinking.clone();
        cur.system_prompt_append = next.system_prompt_append.clone();
        cur.max_turns = next.max_turns;
        cur.max_budget_usd = next.max_budget_usd;
        cur.fast_mode = next.fast_mode;
        cur.env = next.env.clone();

        // 3. 能热生效的字段，固定顺序 model → mode，逐个应用。每成功一个就记进 `cur`；
        //    出错就停，已应用的保留、后面的不动。
        let mut failure: Option<AdapterError> = None;
        if next.model != cur.model {
            match self.apply_model(next.model.as_deref()).await {
                Ok(()) => cur.model = next.model.clone(),
                Err(e) => failure = Some(e),
            }
        }
        if failure.is_none() && next.mode != cur.mode {
            // 启动时没带 --dangerously-skip-permissions 就切不进 bypassPermissions：记下来，重启时生效。
            if next.mode == process::MODE_BYPASS && !self.launch.dangerous() {
                restart = true;
                cur.mode = next.mode.clone();
            } else {
                match self.apply_mode(&next.mode).await {
                    Ok(true) => cur.mode = next.mode.clone(),
                    Ok(false) => {
                        restart = true;
                        cur.mode = next.mode.clone();
                    }
                    Err(e) => failure = Some(e),
                }
            }
        }

        let applied: Vec<&str> = [
            ("model", next.model != self.current.model && next.model == cur.model),
            ("mode", next.mode != self.current.mode && next.mode == cur.mode),
        ]
        .into_iter()
        .filter_map(|(f, ok)| ok.then_some(f))
        .collect();
        let not_applied: Vec<&str> = [("model", next.model != cur.model), ("mode", next.mode != cur.mode)]
            .into_iter()
            .filter_map(|(f, pending)| pending.then_some(f))
            .collect();
        self.commit(cur);

        match failure {
            None => Ok(SettingsApply { applied_now: !restart, restart_required: restart }),
            Some(e) => {
                tracing::warn!(error = %e, ?applied, ?not_applied, "claude: set_settings only partly applied");
                Err(annotate_partial(e, &applied, &not_applied))
            }
        }
    }

    async fn cancel(&mut self) -> Result<()> {
        if !self.busy() {
            return Ok(());
        }
        self.shared.translator.lock().expect("translator lock").mark_cancel_requested();
        let id = uuid::Uuid::new_v4().to_string();
        self.child
            .send_json(&json!({ "type": "control_request", "request_id": id, "request": { "subtype": "interrupt" } }));

        // 兜底：一段时间内没有 `result` 就 SIGINT。只认发起取消的这个 turn：`busy` 是 watch，
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
                tracing::warn!("claude: no result after interrupt, sending SIGINT");
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
        Some(self.shared.resume.lock().expect("resume lock").clone())
    }

    fn is_busy(&self) -> bool {
        self.busy()
    }
}

impl Drop for ClaudeHandle {
    fn drop(&mut self) {
        // 没走 shutdown 就被丢弃：别留孤儿进程。
        self.child.close_stdin();
        self.child.kill();
    }
}
