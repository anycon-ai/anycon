//! 测试支撑：一个可编排的假 adapter（不依赖 `crates/adapters` 的 mock）。
//!
//! 只在 `cfg(test)` 或 `testing` feature 下编译。行为由 prompt 里的关键词控制：
//!
//! | 关键词 | 行为 |
//! |---|---|
//! | （无） | `TextDelta("echo: <内容>")` → `TextDone` → `TurnDone(completed)` |
//! | `permission` | 发一个 `kind = tool` 的权限请求并等答复；允许 → `ToolCall`/`ToolResult`，拒绝 → `TextDelta("skipped")` |
//! | `question` / `plan` / `modechange` | 发对应 kind 的权限请求 |
//! | `permission` + `evilcommand` | 工具权限请求的命令里带 `\r`、ESC[2K，并且把 `TAILMARK-DO-NOT-HIDE` 放在 2000 多个字符之后 |
//! | `slow` | 5 段文本，每段 200 ms，可被 `cancel` 打断（`TurnDone(cancelled)`） |
//! | `hold` | 发一段文本后一直等，直到 `cancel` 或测试调用 `HandleLog::release()` |
//! | `crash` | 发 `Exited { code: 1, error }`（turn 没结束） |
//! | `exit` | 正常 `TurnDone` 之后发 `Exited { error: None }` |
//! | `closechannel` | 和 `crash` / `exit` 连用：发完 `Exited` 立刻关闭事件通道（真实 claude adapter 就是这样），用来测 `Exited` 只处理一次 |
//! | `error` | 发 `Error` 事件然后 `TurnDone(error)` |
//! | `title` | 发 `AgentEvent::Title("Agent title")` |
//! | `maxturns` | 回显后以 `TurnDone(max_turns)` 结束 |
//! | `bigresult` | 一个 `Bash` 工具调用，`tool_result.output` 是 300 KB（`BIGSTART…BIGEND`），无需审批 |
//! | `bigdelta` | 一个 `Bash` 工具调用，先流式发 200 KB 的 `tool_output_delta`，再发一个很小的结果 |
//! | `bigtext` | 回显之外再发一个 150 KB 的 `text_delta` |
//! | `modechange yolo` | `modechange` 的目标模式是 `yolo`（默认是 `plan`） |
//!
//! 关键词之外，测试可以用 [`HandleLog::inject`] 直接往事件通道里塞事件（比如后台任务数、agent 自己开的 turn）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::{
    Adapter, AdapterError, AgentEvent, AgentHandle, PendingPermission, PermissionDecision, ProbeContext, ResumeHandle,
    SettingsApply, SpawnRequest, UserInput,
};
use anycon_protocol::{
    AgentCapabilities, AgentInfo, AgentStatus, Choice, ConfigOption, ConfigOptionKind, Event, ModeInfo, ModeRisk,
    ModelInfo, PermissionDetail, PermissionKind, PermissionOption, PermissionOptionKind, Question, SessionSettings,
    ThinkingOptions, ToolCategory, ToolPermissionDetail, TurnDoneReason, Usage,
};
use tokio::sync::{mpsc, oneshot, Notify};

pub mod preview;

/// 一次 spawn 的记录。
#[derive(Debug, Clone)]
pub struct SpawnRecord {
    pub session_id: String,
    pub cwd: PathBuf,
    pub settings: SessionSettings,
    pub resume: Option<ResumeHandle>,
    pub env: HashMap<String, String>,
}

/// 每个 handle 的可观察状态。
#[derive(Default)]
pub struct HandleLog {
    pub inputs: Mutex<Vec<UserInput>>,
    pub shutdowns: AtomicUsize,
    pub cancels: AtomicUsize,
    pub decisions: Mutex<Vec<(String, PermissionDecision)>>,
    pub settings: Mutex<Vec<SessionSettings>>,
    /// 收到 cancel 之后多久才真正结束 `hold` 的 turn（测「取消已发出、turn 还没收尾」的窗口）。
    pub cancel_delay: Mutex<Duration>,
    release: Notify,
    cancel_gen: AtomicUsize,
    cancel_notify: Notify,
    /// 弱引用：不能因为测试能注入就让通道关不掉（`closechannel` 依赖 handle 放掉发送端后通道关闭）。
    injector: Mutex<Option<mpsc::WeakSender<AgentEvent>>>,
}

impl HandleLog {
    /// 让一个 `hold` 状态的 turn 正常结束。
    pub fn release(&self) {
        self.release.notify_one();
    }
    fn fire_cancel(&self) {
        self.cancel_gen.fetch_add(1, Ordering::SeqCst);
        self.cancel_notify.notify_waiters();
    }
    /// `start` 之后发生过 cancel 就返回。
    async fn wait_cancel(&self, start: usize) {
        loop {
            let n = self.cancel_notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.cancel_gen.load(Ordering::SeqCst) != start {
                return;
            }
            n.await;
        }
    }
    pub fn input_texts(&self) -> Vec<String> {
        self.inputs.lock().unwrap().iter().map(|i| i.content.clone()).collect()
    }
    /// 像 adapter 自己发出的一样，往这个 handle 的事件通道里塞一个事件（agent 不经 `send` 主动做的事：
    /// 后台任务数变化、agent 自己开的 turn 及其事件）。
    pub async fn inject(&self, ev: AgentEvent) {
        let tx = self.injector.lock().unwrap().as_ref().and_then(mpsc::WeakSender::upgrade);
        tx.expect("event channel is open").send(ev).await.expect("event channel is open");
    }
}

pub struct FakeAdapter {
    id: &'static str,
    pub probe_calls: AtomicUsize,
    pub full_probes: AtomicUsize,
    pub max_concurrent_probes: AtomicUsize,
    cur_probes: AtomicUsize,
    version: Mutex<String>,
    probe_delay: Mutex<Duration>,
    pub spawn_delay: Mutex<Duration>,
    pub fail_spawn: Mutex<Option<AdapterErrorKind>>,
    pub spawns: Mutex<Vec<SpawnRecord>>,
    pub handles: Mutex<Vec<Arc<HandleLog>>>,
    pub resume_supported: AtomicBool,
    pub interject: AtomicBool,
    /// `capabilities.attachments`（探测时读；默认四种图片，改了要 `agent.list { refresh: true }` 重新探测）。
    pub attachments: Mutex<Vec<String>>,
    /// 设了就让 `send` 像真实 adapter（Claude Code）一样在送出那一刻读 `attachment_paths`：读不了返回 `AdapterError::Io`。
    pub read_attachments: AtomicBool,
    /// `set_settings` 返回 `restart_required = true`
    pub restart_on_settings: AtomicBool,
    pub status: Mutex<AgentStatus>,
    counter: AtomicUsize,
    /// 设了就让之后所有插话按这种方式失败（`None` = 按真实 adapter 的约定：没有进行中的 turn 才 `TurnEnded`）。
    pub interject_failure: Arc<Mutex<Option<InterjectFailure>>>,
    /// 新 handle 的 `cancel_delay`。
    pub cancel_delay: Mutex<Duration>,
    /// 关闭一个 handle 要多久（测「resume 前先等旧进程关完」）。
    pub shutdown_delay: Arc<Mutex<Duration>>,
    /// `spawn:<n>` / `shutdown_done:<n>` 的发生顺序。
    pub order: Arc<Mutex<Vec<String>>>,
    /// 新 handle 的 `AgentHandle::pid()`（预览按它判断监听端口是不是会话起的）；默认 `None`。
    pub pid: Mutex<Option<u32>>,
}

/// 假 adapter 对**插话**（`UserInput.interject = true`）的失败方式。
#[derive(Debug, Clone)]
pub enum InterjectFailure {
    /// 返回 `AdapterError::TurnEnded`（turn 刚结束）。
    TurnEnded,
    /// 先把当前 turn 结束掉（`TurnDone` 进事件通道，但 daemon 还没读到），再返回 `TurnEnded`——真实的竞态。
    EndTurnThenTurnEnded,
    /// 返回别的错误（超时之类）；当前 turn 照常继续。
    Other(String),
}

#[derive(Debug, Clone)]
pub enum AdapterErrorKind {
    NotInstalled,
    NotAuthenticated,
    Other(String),
}

impl FakeAdapter {
    pub fn new(id: &'static str) -> Arc<Self> {
        Arc::new(FakeAdapter {
            id,
            probe_calls: AtomicUsize::new(0),
            full_probes: AtomicUsize::new(0),
            max_concurrent_probes: AtomicUsize::new(0),
            cur_probes: AtomicUsize::new(0),
            version: Mutex::new("1.0.0".into()),
            probe_delay: Mutex::new(Duration::ZERO),
            spawn_delay: Mutex::new(Duration::ZERO),
            fail_spawn: Mutex::new(None),
            spawns: Mutex::new(Vec::new()),
            handles: Mutex::new(Vec::new()),
            resume_supported: AtomicBool::new(true),
            interject: AtomicBool::new(false),
            attachments: Mutex::new(
                ["image/png", "image/jpeg", "image/gif", "image/webp"].iter().map(|m| m.to_string()).collect(),
            ),
            read_attachments: AtomicBool::new(false),
            restart_on_settings: AtomicBool::new(false),
            status: Mutex::new(AgentStatus::Ready),
            counter: AtomicUsize::new(0),
            interject_failure: Arc::new(Mutex::new(None)),
            cancel_delay: Mutex::new(Duration::ZERO),
            shutdown_delay: Arc::new(Mutex::new(Duration::ZERO)),
            order: Arc::new(Mutex::new(Vec::new())),
            pid: Mutex::new(None),
        })
    }

    /// 之后拉起的 handle 报告这个 pid（测试里常用 `std::process::id()`：测试进程自己开的监听 socket 就算「会话起的」）。
    pub fn set_pid(&self, pid: Option<u32>) {
        *self.pid.lock().unwrap() = pid;
    }

    pub fn set_version(&self, v: &str) {
        *self.version.lock().unwrap() = v.to_string();
    }
    pub fn set_probe_delay(&self, d: Duration) {
        *self.probe_delay.lock().unwrap() = d;
    }
    pub fn set_spawn_delay(&self, d: Duration) {
        *self.spawn_delay.lock().unwrap() = d;
    }
    pub fn spawn_count(&self) -> usize {
        self.spawns.lock().unwrap().len()
    }
    pub fn handle(&self, idx: usize) -> Arc<HandleLog> {
        self.handles.lock().unwrap()[idx].clone()
    }
    pub fn last_handle(&self) -> Arc<HandleLog> {
        self.handles.lock().unwrap().last().cloned().expect("no handle spawned yet")
    }

    fn info(&self, cheap_prev: Option<&AgentInfo>) -> AgentInfo {
        let version = self.version.lock().unwrap().clone();
        let status = *self.status.lock().unwrap();
        let resume = self.resume_supported.load(Ordering::SeqCst);
        let mut info = AgentInfo {
            id: self.id.to_string(),
            label: "Fake Agent".into(),
            driver: "mock".into(),
            enabled: true,
            status,
            version: Some(version),
            path: Some("/usr/local/bin/fake-agent".into()),
            issue: None,
            models: vec![],
            allow_custom_model: false,
            modes: vec![],
            options: vec![],
            capabilities: AgentCapabilities {
                resume,
                interrupt: true,
                interject: self.interject.load(Ordering::SeqCst),
                attachments: self.attachments.lock().unwrap().clone(),
                thinking_stream: true,
                usage: true,
                structured_output: false,
                env: true,
                rewind: false,
            },
            probed_at_ms: crate::util::now_ms(),
        };
        if status != AgentStatus::Ready {
            info.issue = Some(anycon_protocol::AgentIssue {
                code: "not_ready".into(),
                message: format!("fake status {status:?}"),
                hint: Some("fix it".into()),
            });
        }
        match cheap_prev {
            Some(prev) if !prev.models.is_empty() => {
                info.models = prev.models.clone();
                info.modes = prev.modes.clone();
                info.options = prev.options.clone();
            }
            _ => {
                self.full_probes.fetch_add(1, Ordering::SeqCst);
                let choice = |id: &str| Choice { id: id.into(), label: id.to_uppercase(), description: None };
                info.models = vec![
                    ModelInfo {
                        id: "fake-large".into(),
                        label: "Fake Large".into(),
                        description: None,
                        default: true,
                        thinking: Some(ThinkingOptions {
                            options: vec![choice("low"), choice("high")],
                            default: Some("high".into()),
                        }),
                        tags: vec![],
                    },
                    ModelInfo {
                        id: "fake-small".into(),
                        label: "Fake Small".into(),
                        description: None,
                        default: false,
                        thinking: None,
                        tags: vec![],
                    },
                ];
                info.modes = vec![
                    ModeInfo {
                        id: "ask".into(),
                        label: "Ask".into(),
                        description: None,
                        default: true,
                        risk: ModeRisk::Normal,
                    },
                    ModeInfo {
                        id: "plan".into(),
                        label: "Plan".into(),
                        description: None,
                        default: false,
                        risk: ModeRisk::Safe,
                    },
                    ModeInfo {
                        id: "yolo".into(),
                        label: "YOLO".into(),
                        description: None,
                        default: false,
                        risk: ModeRisk::Dangerous,
                    },
                ];
                info.options = vec![ConfigOption {
                    id: "verbose".into(),
                    label: "Verbose".into(),
                    description: None,
                    kind: ConfigOptionKind::Bool,
                    choices: vec![],
                    default: serde_json::json!(false),
                }];
            }
        }
        info
    }
}

#[async_trait::async_trait]
impl Adapter for FakeAdapter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn label(&self) -> &'static str {
        "Fake Agent"
    }
    fn driver(&self) -> &'static str {
        "mock"
    }

    async fn probe(&self, ctx: &ProbeContext) -> AgentInfo {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        let cur = self.cur_probes.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent_probes.fetch_max(cur, Ordering::SeqCst);
        let delay = *self.probe_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let info = self.info(if ctx.cheap { ctx.previous.as_ref() } else { None });
        self.cur_probes.fetch_sub(1, Ordering::SeqCst);
        info
    }

    async fn spawn(&self, req: SpawnRequest) -> anycon_agent_api::Result<Box<dyn AgentHandle>> {
        let delay = *self.spawn_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if let Some(kind) = self.fail_spawn.lock().unwrap().clone() {
            return Err(match kind {
                AdapterErrorKind::NotInstalled => AdapterError::NotInstalled("fake binary missing".into()),
                AdapterErrorKind::NotAuthenticated => AdapterError::NotAuthenticated("fake login required".into()),
                AdapterErrorKind::Other(m) => AdapterError::Spawn(m),
            });
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        self.spawns.lock().unwrap().push(SpawnRecord {
            session_id: req.session_id.clone(),
            cwd: req.cwd.clone(),
            settings: req.settings.clone(),
            resume: req.resume.clone(),
            env: (*req.env).clone(),
        });
        let log = Arc::new(HandleLog::default());
        *log.cancel_delay.lock().unwrap() = *self.cancel_delay.lock().unwrap();
        self.handles.lock().unwrap().push(log.clone());
        self.order.lock().unwrap().push(format!("spawn:{n}"));
        let (tx, rx) = mpsc::channel(512);
        let agent_session_id =
            req.resume.as_ref().map(|r| r.agent_session_id.clone()).unwrap_or_else(|| format!("fake-agent-{n}"));
        // 和真实 adapter 一样：launch_args 是「上次进程启动时有效设置」的快照（测试用它验证 clear 之后不会复活）
        let launch_args = serde_json::json!({
            "model": req.settings.model,
            "system_prompt_append": req.settings.system_prompt_append,
            "max_turns": req.settings.max_turns,
            "max_budget_usd": req.settings.max_budget_usd,
        });
        let resume = ResumeHandle { agent_session_id, launch_args };
        let _ = tx.try_send(AgentEvent::Started { resume: resume.clone() });
        *log.injector.lock().unwrap() = Some(tx.downgrade());
        Ok(Box::new(FakeHandle {
            rx,
            tx: Arc::new(Mutex::new(Some(tx))),
            log,
            resume,
            pending: Arc::new(Mutex::new(HashMap::new())),
            busy: Arc::new(AtomicBool::new(false)),
            restart_on_settings: self.restart_on_settings.load(Ordering::SeqCst),
            interject: self.interject.load(Ordering::SeqCst),
            perm_counter: Arc::new(AtomicUsize::new(0)),
            interject_failure: self.interject_failure.clone(),
            read_attachments: self.read_attachments.load(Ordering::SeqCst),
            n,
            order: self.order.clone(),
            shutdown_delay: self.shutdown_delay.clone(),
            pid: *self.pid.lock().unwrap(),
        }))
    }
}

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>;

pub struct FakeHandle {
    rx: mpsc::Receiver<AgentEvent>,
    tx: Arc<Mutex<Option<mpsc::Sender<AgentEvent>>>>,
    log: Arc<HandleLog>,
    resume: ResumeHandle,
    pending: PendingMap,
    busy: Arc<AtomicBool>,
    restart_on_settings: bool,
    interject: bool,
    perm_counter: Arc<AtomicUsize>,
    interject_failure: Arc<Mutex<Option<InterjectFailure>>>,
    read_attachments: bool,
    n: usize,
    order: Arc<Mutex<Vec<String>>>,
    shutdown_delay: Arc<Mutex<Duration>>,
    pid: Option<u32>,
}

fn usage() -> Usage {
    Usage {
        input_tokens: Some(10),
        output_tokens: Some(20),
        cache_read_tokens: Some(1),
        cache_write_tokens: Some(2),
        cost_usd: Some(0.01),
        duration_ms: 5,
        model: Some("fake-large".into()),
        ..Default::default()
    }
}

fn allow_deny_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption {
            id: "allow".into(),
            label: "Allow".into(),
            kind: PermissionOptionKind::AllowOnce,
            needs_message: false,
        },
        PermissionOption {
            id: "allow_session".into(),
            label: "Allow for session".into(),
            kind: PermissionOptionKind::AllowAlways,
            needs_message: false,
        },
        PermissionOption {
            id: "deny".into(),
            label: "Deny".into(),
            kind: PermissionOptionKind::RejectOnce,
            needs_message: true,
        },
    ]
}

#[async_trait::async_trait]
impl AgentHandle for FakeHandle {
    fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.rx
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    async fn send(&mut self, input: UserInput) -> anycon_agent_api::Result<()> {
        // 真实 adapter 在送出的那一刻才读附件文件（ADR-35）：读不了就是这条消息的失败，不是 daemon 要重试的事
        if self.read_attachments {
            for p in &input.attachment_paths {
                std::fs::read(p)?;
            }
        }
        if input.interject {
            // 契约（agent-api）：插话只送进进行中的 turn；没有进行中的 turn 返回 TurnEnded，绝不自己开 turn
            if !self.interject {
                return Err(AdapterError::Unsupported("interjection is not supported".into()));
            }
            let failure = self.interject_failure.lock().unwrap().clone();
            match failure {
                Some(InterjectFailure::TurnEnded) => return Err(AdapterError::TurnEnded),
                Some(InterjectFailure::Other(m)) => return Err(AdapterError::Protocol(m)),
                Some(InterjectFailure::EndTurnThenTurnEnded) => {
                    let tx = self.tx.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        self.busy.store(false, Ordering::SeqCst);
                        let _ = tx
                            .send(AgentEvent::Event(Event::TurnDone {
                                turn_id: 0,
                                reason: TurnDoneReason::Completed,
                                usage: Some(usage()),
                            }))
                            .await;
                    }
                    return Err(AdapterError::TurnEnded);
                }
                None => {}
            }
            if !self.busy.load(Ordering::SeqCst) {
                return Err(AdapterError::TurnEnded);
            }
        } else if self.busy.load(Ordering::SeqCst) && !self.interject {
            return Err(AdapterError::Unsupported("turn in progress".into()));
        }
        self.log.inputs.lock().unwrap().push(input.clone());
        let shared_tx = self.tx.clone();
        let tx = shared_tx.lock().unwrap().clone();
        let Some(tx) = tx else { return Err(AdapterError::Exited("closed".into())) };
        self.busy.store(true, Ordering::SeqCst);
        let (log, pending, busy, counter) =
            (self.log.clone(), self.pending.clone(), self.busy.clone(), self.perm_counter.clone());
        tokio::spawn(async move {
            let text = input.content.clone();
            let turn_end = |reason: TurnDoneReason| {
                AgentEvent::Event(Event::TurnDone { turn_id: 0, reason, usage: Some(usage()) })
            };
            let ev = |e: Event| AgentEvent::Event(e);
            let start = log.cancel_gen.load(Ordering::SeqCst);
            let _ = tx.send(ev(Event::ThinkingDelta { text: "hmm".into() })).await;
            if text.contains("crash") {
                busy.store(false, Ordering::SeqCst);
                if text.contains("crashevent") {
                    // 和 claude-code adapter 一致：先报 agent_exited，再 Exited，不补 TurnDone
                    let _ = tx
                        .send(ev(Event::Error {
                            code: "agent_exited".into(),
                            message: "claude exited unexpectedly".into(),
                            recoverable: true,
                        }))
                        .await;
                }
                let _ = tx.send(AgentEvent::Exited { code: Some(1), error: Some("boom".into()) }).await;
                if text.contains("closechannel") {
                    shared_tx.lock().unwrap().take();
                }
                return;
            }
            if text.contains("title") {
                let _ = tx.send(AgentEvent::Title("Agent title".into())).await;
            }
            let mut reason = TurnDoneReason::Completed;
            if text.contains("slow") {
                let mut cancelled = false;
                for i in 0..5 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                        _ = log.wait_cancel(start) => { cancelled = true; break; }
                    }
                    let _ = tx.send(ev(Event::TextDelta { text: format!("part{i} ") })).await;
                }
                if cancelled {
                    reason = TurnDoneReason::Cancelled;
                } else {
                    let _ = tx.send(ev(Event::TextDone {})).await;
                }
            } else if text.contains("hold") {
                let _ = tx.send(ev(Event::TextDelta { text: "holding".into() })).await;
                tokio::select! {
                    _ = log.release.notified() => {}
                    _ = log.wait_cancel(start) => {
                        reason = TurnDoneReason::Cancelled;
                        let delay = *log.cancel_delay.lock().unwrap();
                        tokio::time::sleep(delay).await;
                    }
                }
            } else if text.contains("permission")
                || text.contains("question")
                || text.contains("plan")
                || text.contains("modechange")
            {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                let request_id = format!("req-{n}");
                let (kind, title, detail, options) = if text.contains("question") {
                    (
                        PermissionKind::Question,
                        "Agent has a question".to_string(),
                        PermissionDetail {
                            questions: vec![Question {
                                id: "q1".into(),
                                text: "Which one?".into(),
                                choices: vec![],
                                multi: false,
                                free_text: true,
                                optional: false,
                            }],
                            ..Default::default()
                        },
                        vec![],
                    )
                } else if text.contains("plan") {
                    (
                        PermissionKind::Plan,
                        "Plan ready".to_string(),
                        PermissionDetail { text: Some("# plan".into()), ..Default::default() },
                        vec![
                            PermissionOption {
                                id: "implement".into(),
                                label: "Implement".into(),
                                kind: PermissionOptionKind::AllowOnce,
                                needs_message: false,
                            },
                            PermissionOption {
                                id: "keep_planning".into(),
                                label: "Keep planning".into(),
                                kind: PermissionOptionKind::RejectOnce,
                                needs_message: true,
                            },
                        ],
                    )
                } else if text.contains("modechange") {
                    (
                        PermissionKind::ModeChange,
                        "Switch mode".to_string(),
                        PermissionDetail {
                            mode: Some(if text.contains("yolo") { "yolo" } else { "plan" }.into()),
                            ..Default::default()
                        },
                        allow_deny_options(),
                    )
                } else {
                    // `evilcommand`：被 prompt 注入的 agent 会干的事——命令里带 `\r` 和 ESC[2K（改写终端显示），
                    // 危险的部分（TAILMARK）放在两千多个字符之后（CLI 审批提示测试用）
                    let command = if text.contains("evilcommand") {
                        format!("echo harmless\r{}\x1b[2K; echo TAILMARK-DO-NOT-HIDE", "x".repeat(2100))
                    } else {
                        "echo hi".to_string()
                    };
                    (
                        PermissionKind::Tool,
                        "Run command".to_string(),
                        PermissionDetail {
                            tool: Some(ToolPermissionDetail {
                                tool: "Bash".into(),
                                category: ToolCategory::Shell,
                                input: serde_json::json!({ "command": command }),
                                cwd: None,
                                detail: None,
                            }),
                            ..Default::default()
                        },
                        allow_deny_options(),
                    )
                };
                let (dtx, drx) = oneshot::channel();
                pending.lock().unwrap().insert(request_id.clone(), dtx);
                let _ = tx
                    .send(AgentEvent::Permission(PendingPermission {
                        request_id: request_id.clone(),
                        kind,
                        title,
                        detail,
                        options,
                        expires_in: None,
                    }))
                    .await;
                tokio::select! {
                    d = drx => {
                        let allowed = d.ok().map(|d| d.option_id.as_deref().is_some_and(|o| o.starts_with("allow") || o == "implement") || d.answers.is_some()).unwrap_or(false);
                        if allowed {
                            let _ = tx.send(ev(Event::ToolCall { id: "t1".into(), name: "Bash".into(), category: ToolCategory::Shell, title: "echo hi".into(), input: serde_json::json!({}), detail: None, parent_id: None })).await;
                            let _ = tx.send(ev(Event::ToolOutputDelta { id: "t1".into(), text: "hi\n".into() })).await;
                            let _ = tx.send(ev(Event::ToolResult { id: "t1".into(), output: serde_json::json!("hi"), is_error: false, detail: None })).await;
                        } else {
                            let _ = tx.send(ev(Event::TextDelta { text: "skipped".into() })).await;
                        }
                    }
                    _ = log.wait_cancel(start) => { reason = TurnDoneReason::Cancelled; }
                }
            } else if text.contains("bigresult") {
                let _ = tx
                    .send(ev(Event::ToolCall {
                        id: "t-big".into(),
                        name: "Bash".into(),
                        category: ToolCategory::Shell,
                        title: "cat big.log".into(),
                        input: serde_json::json!({ "command": "cat big.log" }),
                        detail: None,
                        parent_id: None,
                    }))
                    .await;
                let big = format!("BIGSTART{}BIGEND", "line of output\n".repeat(20_000));
                let _ = tx
                    .send(ev(Event::ToolResult {
                        id: "t-big".into(),
                        output: serde_json::json!(big),
                        is_error: false,
                        detail: None,
                    }))
                    .await;
            } else if text.contains("bigdelta") {
                let _ = tx
                    .send(ev(Event::ToolCall {
                        id: "t-delta".into(),
                        name: "Bash".into(),
                        category: ToolCategory::Shell,
                        title: "make".into(),
                        input: serde_json::json!({}),
                        detail: None,
                        parent_id: None,
                    }))
                    .await;
                for i in 0..20 {
                    let _ = tx
                        .send(ev(Event::ToolOutputDelta {
                            id: "t-delta".into(),
                            text: format!("{i:02}{}", "d".repeat(9_998)),
                        }))
                        .await;
                }
                let _ = tx
                    .send(ev(Event::ToolResult {
                        id: "t-delta".into(),
                        output: serde_json::json!("done"),
                        is_error: false,
                        detail: None,
                    }))
                    .await;
            } else if text.contains("error") {
                let _ = tx
                    .send(ev(Event::Error {
                        code: "fake_error".into(),
                        message: "something failed".into(),
                        recoverable: true,
                    }))
                    .await;
                reason = TurnDoneReason::Error;
            } else {
                let _ = tx.send(ev(Event::TextDelta { text: format!("echo: {text}") })).await;
                if text.contains("bigtext") {
                    let _ = tx.send(ev(Event::TextDelta { text: "T".repeat(150_000) })).await;
                }
                let _ = tx.send(ev(Event::TextDone {})).await;
                if text.contains("maxturns") {
                    reason = TurnDoneReason::MaxTurns;
                }
            }
            busy.store(false, Ordering::SeqCst);
            let _ = tx.send(turn_end(reason)).await;
            if text.contains("exit") {
                let _ = tx.send(AgentEvent::Exited { code: Some(0), error: None }).await;
                if text.contains("closechannel") {
                    shared_tx.lock().unwrap().take();
                }
            }
        });
        Ok(())
    }

    async fn respond_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> anycon_agent_api::Result<()> {
        self.log.decisions.lock().unwrap().push((request_id.to_string(), decision.clone()));
        if let Some(tx) = self.pending.lock().unwrap().remove(request_id) {
            let _ = tx.send(decision);
        }
        Ok(())
    }

    async fn set_settings(&mut self, settings: &SessionSettings) -> anycon_agent_api::Result<SettingsApply> {
        self.log.settings.lock().unwrap().push(settings.clone());
        Ok(SettingsApply { applied_now: !self.restart_on_settings, restart_required: self.restart_on_settings })
    }

    async fn cancel(&mut self) -> anycon_agent_api::Result<()> {
        self.log.cancels.fetch_add(1, Ordering::SeqCst);
        self.log.fire_cancel();
        Ok(())
    }

    async fn shutdown(&mut self) -> anycon_agent_api::Result<()> {
        let first = self.log.shutdowns.fetch_add(1, Ordering::SeqCst) == 0;
        let delay = *self.shutdown_delay.lock().unwrap();
        if first && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if first {
            self.order.lock().unwrap().push(format!("shutdown_done:{}", self.n));
        }
        let taken = self.tx.lock().unwrap().take();
        if let Some(tx) = taken {
            let _ = tx.send(AgentEvent::Exited { code: Some(0), error: None }).await;
        }
        Ok(())
    }

    fn resume_handle(&self) -> Option<ResumeHandle> {
        Some(self.resume.clone())
    }

    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

// ====================================================================== 整机测试夹具

use std::time::Instant;

use anycon_protocol::{RpcError, RpcId, RpcMessage, RpcNotification, RpcRequest};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::clients::{Caller, ClientHandle, ClientSink};
use crate::config::Paths;
use crate::daemon::{Ctx, Daemon, DaemonLock, DaemonOptions};

struct ChanSink(mpsc::UnboundedSender<RpcNotification>);

impl ClientSink for ChanSink {
    fn send(&self, n: RpcNotification) {
        let _ = self.0.send(n);
    }
}

/// 一台临时 daemon：临时 `ANYCON_HOME`、临时 workspace、假 adapter，不连 hub（可选开本机 socket）。
pub struct TestDaemon {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
    pub workspace: PathBuf,
    pub fake: Arc<FakeAdapter>,
    pub daemon: Option<Daemon>,
    pub ctx: Arc<Ctx>,
    /// home 上的单实例锁（`daemon.lock`），夹具活多久就持有多久；daemon 本身用 `lock: false` 启动。
    ///
    /// flock 跟着打开的文件描述走：同一个测试进程里别的测试并发拉起 git / agent 子进程时，子进程在 exec 完成前持有
    /// 父进程 fd 表的副本（`O_CLOEXEC` 到 exec 才关），刚关掉的锁会再多占几毫秒。要是让每次 [`TestDaemon::restart`]
    /// 都放锁再重取，新 daemon 偶尔会撞上 `AlreadyRunning`（本进程的 pid）。生产里一个进程只启动一次 daemon，没有这个问题。
    _lock: DaemonLock,
    socket: bool,
    hub: bool,
    /// 用 `anycon_adapters::all()`（真 mock / claude-code）而不是假 adapter。
    real_adapters: bool,
    ready_agent: &'static str,
    /// 叠加到 daemon 环境上的变量（git 夹具：`GIT_CONFIG_GLOBAL` 等）。
    env_extra: Vec<(String, String)>,
}

pub struct TestClient {
    pub handle: Arc<ClientHandle>,
    pub rx: mpsc::UnboundedReceiver<RpcNotification>,
    next_id: u64,
}

impl TestDaemon {
    pub async fn start() -> TestDaemon {
        Self::start_with(&[], false).await
    }

    /// `overrides` 是 `config set` 风格的键值（值按 TOML 解析）。
    pub async fn start_with(overrides: &[(&str, &str)], socket: bool) -> TestDaemon {
        Self::start_full(overrides, socket, None).await
    }

    /// 用 `anycon_adapters::all()` 里的真 mock adapter（`agents.mock.enabled = true`）。
    pub async fn start_mock(overrides: &[(&str, &str)], socket: bool) -> TestDaemon {
        Self::start_inner(overrides, socket, None, true, false, vec![]).await
    }

    /// 带 git 夹具的 daemon（假 adapter）：daemon 的环境里 `GIT_CONFIG_GLOBAL` 指向临时文件（`user.name` / `user.email` /
    /// `init.defaultBranch = main`），`GIT_CONFIG_NOSYSTEM=1`，`PATH` 前面放着一个假的 `gh`（见 [`TestDaemon::set_gh`]）——
    /// 不读开发机的 `~/.gitconfig`，不联网。
    pub async fn start_git(overrides: &[(&str, &str)]) -> TestDaemon {
        Self::start_inner(overrides, false, None, false, true, vec![]).await
    }

    /// daemon 的 `PATH` 里没有 `git`（`NodeCapabilities.worktree` 为 false）。
    pub async fn start_without_git() -> TestDaemon {
        Self::start_inner(&[], false, None, false, false, vec![("PATH".into(), "/nonexistent-anycon-test-dir".into())])
            .await
    }

    /// [`TestDaemon::start_git`]，用真的 mock agent（`anycon_adapters::all()`，支持 `write` 关键词）。
    pub async fn start_mock_git(overrides: &[(&str, &str)]) -> TestDaemon {
        Self::start_inner(overrides, false, None, true, true, vec![]).await
    }

    /// `hub_url` 为 Some 时写进 `hub.url` 并连接 hub。
    pub async fn start_full(overrides: &[(&str, &str)], socket: bool, hub_url: Option<&str>) -> TestDaemon {
        Self::start_inner(overrides, socket, hub_url, false, false, vec![]).await
    }

    async fn start_inner(
        overrides: &[(&str, &str)],
        socket: bool,
        hub_url: Option<&str>,
        real: bool,
        git: bool,
        extra: Vec<(String, String)>,
    ) -> TestDaemon {
        let dir = tempfile::tempdir().unwrap();
        let mut env_extra: Vec<(String, String)> = extra;
        if git {
            let cfg = dir.path().join("gitconfig");
            std::fs::write(
                &cfg,
                "[user]\n\tname = Anycon Test\n\temail = test@example.invalid\n[init]\n\tdefaultBranch = main\n[commit]\n\tgpgsign = false\n",
            )
            .unwrap();
            // PATH 里只有一个临时目录：`git`（真的，符号链接）和假的 `gh`。这样 `remove_gh` 之后 daemon 真的找不到 `gh`，
            // 开发机上装没装 `gh` 都不影响测试。Windows 上建符号链接要特权、假 `gh` 是 sh 脚本，所以 PATH 是这个空目录加上真 git
            // 所在的目录（Git for Windows 的 `cmd` 目录里只有 git 自己），没有 `gh`
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let git_name = format!("git{}", std::env::consts::EXE_SUFFIX);
            let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|d| d.join(&git_name))
                .find(|p| p.is_file())
                .expect("git is installed");
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(real_git, bin.join("git")).unwrap();
                write_fake_gh(&bin);
                env_extra.push(("PATH".into(), bin.to_string_lossy().into_owned()));
            }
            #[cfg(windows)]
            {
                let git_dir = real_git.parent().expect("git has a directory").to_path_buf();
                let path = std::env::join_paths([bin.clone(), git_dir]).unwrap();
                env_extra.push(("PATH".into(), path.to_string_lossy().into_owned()));
            }
            env_extra.push(("GIT_CONFIG_GLOBAL".into(), cfg.to_string_lossy().into_owned()));
            env_extra.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
        }
        let paths = Paths::new(dir.path().join("home"));
        let workspace = dir.path().join("ws");
        let opts = crate::setup::InitOptions {
            hub: hub_url.map(str::to_string),
            workspace: Some(workspace.to_string_lossy().into()),
            name: Some("test-node".into()),
            force: false,
        };
        crate::setup::init(&paths, &opts).await.unwrap();
        let ready_agent = if real { "mock" } else { "fake" };
        crate::config::set_value(&paths, &format!("agents.{ready_agent}.enabled"), "true").unwrap();
        // 内置的真实 agent 没有配置段也参与探测（daemon.md § 5.6）：测试不碰机器上真装着的 claude / codex
        for builtin in crate::config::BUILTIN_AGENTS {
            crate::config::set_value(&paths, &format!("agents.{builtin}.enabled"), "false").unwrap();
        }
        for (k, v) in overrides {
            crate::config::set_value(&paths, k, v).unwrap();
        }
        let fake = FakeAdapter::new("fake");
        let hub = hub_url.is_some();
        let lock = DaemonLock::acquire(&paths).unwrap();
        let (daemon, ctx) = Self::boot(&paths, &fake, socket, hub, real, &env_extra).await;
        // 等首次探测完成
        let t = Instant::now();
        while ctx.agents.info(ready_agent).is_none_or(|i| i.status != AgentStatus::Ready)
            && t.elapsed() < Duration::from_secs(5)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let workspace = ctx.workspace.root().to_path_buf();
        TestDaemon {
            dir,
            paths,
            workspace,
            fake,
            daemon: Some(daemon),
            ctx,
            _lock: lock,
            socket,
            hub,
            real_adapters: real,
            ready_agent,
            env_extra,
        }
    }

    async fn boot(
        paths: &Paths,
        fake: &Arc<FakeAdapter>,
        socket: bool,
        hub: bool,
        real: bool,
        env_extra: &[(String, String)],
    ) -> (Daemon, Arc<Ctx>) {
        let daemon = Daemon::start(
            paths.clone(),
            DaemonOptions {
                adapters: if real { None } else { Some(vec![fake.clone() as Arc<dyn anycon_agent_api::Adapter>]) },
                hub,
                socket,
                login_shell: Some(false),
                // 锁由夹具持有（见 `TestDaemon::_lock`）
                lock: false,
                env_extra: env_extra.to_vec(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let ctx = daemon.ctx.clone();
        (daemon, ctx)
    }

    /// 停掉再用同一个 home 重新启动（测试重启恢复）。
    pub async fn restart(&mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
        let (daemon, ctx) =
            Self::boot(&self.paths, &self.fake, self.socket, self.hub, self.real_adapters, &self.env_extra).await;
        let t = Instant::now();
        while ctx.agents.info(self.ready_agent).is_none_or(|i| i.status != AgentStatus::Ready)
            && t.elapsed() < Duration::from_secs(5)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.daemon = Some(daemon);
        self.ctx = ctx;
    }

    pub fn client(&self, name: &str) -> TestClient {
        let (tx, rx) = mpsc::unbounded_channel();
        let caller = Caller {
            device_id: format!("dev-{name}"),
            device_name: name.to_string(),
            kind: anycon_protocol::DeviceKind::Web,
            local: true,
        };
        let handle = self.ctx.clients.register(caller, Box::new(ChanSink(tx)));
        TestClient { handle, rx, next_id: 0 }
    }

    pub fn rpc(&self) -> Arc<crate::rpc::Rpc> {
        self.daemon.as_ref().unwrap().rpc.clone()
    }

    pub async fn shutdown(mut self) {
        if let Some(d) = self.daemon.take() {
            d.shutdown().await;
        }
    }

    // ---------------- git 夹具（`start_git` / `start_mock_git`）----------------

    /// 跑一条真的 `git`（和 daemon 同一份配置：不读开发机的 `~/.gitconfig`），返回 stdout；失败 panic。
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git_raw(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// 同 [`TestDaemon::git`]，但不检查退出码。
    pub fn git_raw(&self, dir: &Path, args: &[&str]) -> std::process::Output {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args).current_dir(dir).env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").env_remove("GIT_INDEX_FILE");
        for (k, v) in &self.env_extra {
            cmd.env(k, v);
        }
        cmd.output().expect("git is installed")
    }

    /// workspace 根下建一个有一个提交的 git 仓库项目（分支 `main`，文件 `README.md`），返回它的目录。
    pub fn new_repo(&self, name: &str) -> PathBuf {
        let dir = self.workspace.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        self.git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "# readme\n").unwrap();
        self.git(&dir, &["add", "-A"]);
        self.git(&dir, &["commit", "-q", "-m", "initial"]);
        crate::sys::canonicalize(&dir).unwrap()
    }

    /// 写文件（必要时建目录）、`git add`、`git commit`，返回新提交的 sha。
    pub fn commit_file(&self, dir: &Path, rel: &str, content: &str, msg: &str) -> String {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        self.git(dir, &["add", "-A"]);
        self.git(dir, &["commit", "-q", "-m", msg]);
        self.git(dir, &["rev-parse", "HEAD"])
    }

    #[cfg(unix)]
    /// 假 `gh` 的行为：下次被调用时 stdout 输出 `stdout`、stderr 输出 `stderr`、以 `code` 退出。
    pub fn set_gh(&self, stdout: &str, stderr: &str, code: i32) {
        let bin = self.dir.path().join("bin");
        std::fs::write(bin.join("gh.out"), stdout).unwrap();
        std::fs::write(bin.join("gh.err"), stderr).unwrap();
        std::fs::write(bin.join("gh.code"), code.to_string()).unwrap();
    }

    #[cfg(unix)]
    /// 假 `gh` 被调用过几次。
    pub fn gh_calls(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("bin/gh.calls")).map(|s| s.lines().count()).unwrap_or(0)
    }

    #[cfg(unix)]
    /// 把假 `gh` 删掉（模拟没装）。
    pub fn remove_gh(&self) {
        let _ = std::fs::remove_file(self.dir.path().join("bin/gh"));
    }

    #[cfg(unix)]
    /// 把 [`TestDaemon::remove_gh`] 删掉的假 `gh` 放回去。
    pub fn write_gh_back(&self) {
        write_fake_gh(&self.dir.path().join("bin"));
    }

    #[cfg(unix)]
    /// 假 `gh` 每次被调用时（记下调用之后）先睡这么多秒：测「`gh` 卡住」。0 = 不睡。
    pub fn set_gh_sleep(&self, secs: f64) {
        let f = self.dir.path().join("bin/gh.sleep");
        if secs > 0.0 {
            std::fs::write(f, secs.to_string()).unwrap();
        } else {
            let _ = std::fs::remove_file(f);
        }
    }

    /// 把 daemon `PATH` 里的 `git` 换成一个转发给真 git 的包装脚本（第一次调用时装上），返回 `bin` 目录。
    #[cfg(unix)]
    fn git_wrapper_dir(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.dir.path().join("bin");
        let git = bin.join("git");
        let real = bin.join("git.real");
        if !real.exists() {
            let target = std::fs::read_link(&git).expect("the fixture's git is a symlink to the real git");
            std::fs::write(&real, target.to_string_lossy().as_bytes()).unwrap();
            std::fs::remove_file(&git).unwrap();
            let script = "#!/bin/sh\norig_path=\"$PATH\"\nPATH=/usr/bin:/bin\nd=\"$(dirname \"$0\")\"\n[ -e \"$d/git.warm\" ] || : > \"$d/git.warm\"\nline=\"$PWD $*\"\nif [ -f \"$d/git.slow\" ]; then\n  while read -r secs pat; do\n    [ -z \"$pat\" ] && continue\n    case \"$line\" in\n      $pat) echo \"$line\" >> \"$d/git.hits\"; sleep \"$secs\" ;;\n    esac\n  done < \"$d/git.slow\"\nfi\nif [ -f \"$d/git.fail\" ]; then\n  while read -r pat; do\n    [ -z \"$pat\" ] && continue\n    case \"$line\" in\n      $pat) echo \"$line\" >> \"$d/git.hits\"; echo \"fatal: injected failure\" >&2; exit 1 ;;\n    esac\n  done < \"$d/git.fail\"\nfi\nreal=\"$(cat \"$d/git.real\")\"\nPATH=\"$orig_path\"\nexec \"$real\" \"$@\"\n";
            std::fs::write(&git, script).unwrap();
            std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
            run_once(&git);
        }
        bin
    }

    #[cfg(unix)]
    /// 把 daemon `PATH` 里的 `git` 换成一个转发给真 git 的包装脚本，让匹配 `rules` 的调用先睡一会儿——不依赖机器负载就能造出
    /// 慢 git。每条规则是 (shell 通配模式, 秒数)，模式匹配的是 `"<cwd> <git 的参数…>"` 这一整行（daemon 的每条 git 命令都带
    /// `-c core.fsmonitor=false -c core.hooksPath=/dev/null` 在前，所以写 `*slowrepo*status*`）。命中的调用记在
    /// [`TestDaemon::git_hits`]。可以反复调用来换规则；`&[]` 等于全部放行。只对 [`TestDaemon::start_git`] 系列夹具有效。
    pub fn slow_git(&self, rules: &[(&str, f64)]) {
        let bin = self.git_wrapper_dir();
        let text: String = rules.iter().map(|(pat, secs)| format!("{secs} {pat}\n")).collect();
        std::fs::write(bin.join("git.slow"), text).unwrap();
    }

    #[cfg(unix)]
    /// 同 [`TestDaemon::slow_git`]，但命中的调用直接以失败退出（stderr `fatal: injected failure`，退出码 1）。`&[]` = 全部放行。
    pub fn fail_git(&self, patterns: &[&str]) {
        let bin = self.git_wrapper_dir();
        let text: String = patterns.iter().map(|pat| format!("{pat}\n")).collect();
        std::fs::write(bin.join("git.fail"), text).unwrap();
    }

    #[cfg(unix)]
    /// [`TestDaemon::slow_git`] 的规则命中过的调用（`"<cwd> <参数…>"` 一行一条）。
    pub fn git_hits(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("bin/git.hits"))
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default()
    }
}

/// 把刚写出来的包装脚本同步执行一次（`<bin>/git --version`）。macOS 上「第一次执行一个刚写出的脚本」偶尔要停顿约 3 秒
/// （系统对新可执行文件的首次检查，和代码无关）；把这一次放在建夹具的时候，后面的测试里 git 调用才不会带着这个停顿去赶
/// 3 秒的冷缓存预算。给 30 秒，超时就杀掉并 panic。
///
/// 测试并行跑，别的线程这时可能正好 fork，子进程会短暂继承刚写脚本时还没关的写 fd，exec 这个脚本就得到 ETXTBSY
/// （「Text file busy」）；那个 fd 在子进程 exec 时就关了，所以重试几次即可。
#[cfg(unix)]
fn run_once(exe: &Path) {
    let started = std::time::Instant::now();
    let mut child = loop {
        let spawned = std::process::Command::new(exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => break child,
            // ETXTBSY
            Err(e) if e.raw_os_error() == Some(26) && started.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("cannot run {}: {e}", exe.display()),
        }
    };
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                assert!(status.success(), "{} --version failed: {status}", exe.display());
                return;
            }
            None if started.elapsed() > Duration::from_secs(30) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{} --version did not finish within 30 seconds", exe.display());
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// 断言 `p` 的权限位是 `mode`（如 0o600）。Windows 没有权限位，不检查（访问控制靠数据目录的 DACL，见 `sys::restrict_to_owner`）。
pub fn assert_mode(p: &Path, mode: u32) {
    if let Some(m) = crate::sys::mode_bits(&std::fs::metadata(p).unwrap()) {
        assert_eq!(m, mode, "mode of {}", p.display());
    }
}

/// 建一个指向文件的符号链接（Windows 上同 [`symlink_dir`]，要特权）。
pub fn symlink_file(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link)
    }
}

/// 建一个指向目录的符号链接。Windows 上是目录符号链接（要管理员权限或开发者模式；CI 的 runner 有）。
pub fn symlink_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link)
    }
}

/// 假 `gh`：输出 `gh.out`、`gh.err`，按 `gh.code` 退出，每次调用往 `gh.calls` 记一行。
#[cfg(unix)]
fn write_fake_gh(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let script = "#!/bin/sh\nPATH=/usr/bin:/bin\nd=\"$(dirname \"$0\")\"\necho \"$@\" >> \"$d/gh.calls\"\n[ -f \"$d/gh.sleep\" ] && sleep \"$(cat \"$d/gh.sleep\")\"\n[ -f \"$d/gh.err\" ] && cat \"$d/gh.err\" >&2\n[ -f \"$d/gh.out\" ] && cat \"$d/gh.out\"\ncode=0\n[ -f \"$d/gh.code\" ] && code=$(cat \"$d/gh.code\")\nexit \"$code\"\n";
    let p = bin.join("gh");
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl TestClient {
    /// 直接走 RPC 调度器。
    pub async fn call<T: DeserializeOwned>(
        &mut self,
        td: &TestDaemon,
        method: &str,
        params: impl Serialize,
    ) -> Result<T, RpcError> {
        self.next_id += 1;
        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: RpcId::Num(self.next_id),
            method: method.into(),
            params: serde_json::to_value(params).unwrap(),
        };
        match td.rpc().handle_request(&self.handle, req).await {
            RpcMessage::Response(r) => match (r.result, r.error) {
                (_, Some(e)) => Err(e),
                (Some(v), None) => Ok(serde_json::from_value(v).expect("result shape")),
                (None, None) => Ok(serde_json::from_value(serde_json::Value::Null).expect("null result")),
            },
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 等一条满足条件的通知（先消费之前的）。
    pub async fn wait_notification(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&RpcNotification) -> bool,
    ) -> Option<RpcNotification> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(n)) if pred(&n) => return Some(n),
                Ok(Some(_)) => continue,
                _ => return None,
            }
        }
    }

    /// 收集 `timeout` 内的全部通知。
    pub async fn drain_for(&mut self, timeout: Duration) -> Vec<RpcNotification> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        while let Ok(Some(n)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            out.push(n);
        }
        out
    }

    /// 非阻塞取走已有通知。
    pub fn drain_now(&mut self) -> Vec<RpcNotification> {
        let mut out = Vec::new();
        while let Ok(n) = self.rx.try_recv() {
            out.push(n);
        }
        out
    }
}

/// 文件头合法的 PNG 夹具（附件测试用）：IHDR 里是给定的宽高，后面补确定性的伪随机字节到 `total` 字节。
pub fn png_fixture(width: u32, height: u32, total: usize, seed: u8) -> Vec<u8> {
    let mut v = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&width.to_be_bytes());
    v.extend_from_slice(&height.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    let mut x = u32::from(seed) + 1;
    while v.len() < total {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((x >> 24) as u8);
    }
    v
}

/// 64 位小写 hex 的 SHA-256（`blob.put_begin` 的 `sha256`）。
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// 轮询直到 `f` 为真或超时。
pub async fn eventually<F, Fut>(timeout: Duration, mut f: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let t = Instant::now();
    loop {
        if f().await {
            return true;
        }
        if t.elapsed() > timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ====================================================================== 假 hub 的目标缓冲（粘住规则）

/// 一次 `offer` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offer {
    Forwarded,
    Bounced,
}

/// 模拟 hub 对**一个目标连接**的转发（docs/protocol.md § 3.1 的粘住规则）：目标发送缓冲有字节上限；放不下就退回
/// （`backpressure`），并且一旦某个 seq 被退回，之后同一发送方的信封一律退回，直到那个 seq 的重发被转发——然后轮到下一个
/// 被退回的 seq。用来在真实 hub 的粘住规则落地之前（以及不依赖它的单元测试里）验证发送方是否守规则。
pub struct StickyTarget {
    cap: usize,
    buffered: usize,
    queue: VecDeque<(u64, Vec<u8>)>,
    pending: std::collections::BTreeSet<u64>,
    /// 被转发的 seq，按转发顺序（断言「不乱序、不重复」用）。
    pub forwarded: Vec<u64>,
    pub bounces: usize,
}

impl StickyTarget {
    pub fn new(cap: usize) -> StickyTarget {
        StickyTarget {
            cap,
            buffered: 0,
            queue: VecDeque::new(),
            pending: Default::default(),
            forwarded: Vec::new(),
            bounces: 0,
        }
    }

    /// 发送方发来一封（`seq`、整封信封的字节）：转发进目标缓冲，或退回。
    pub fn offer(&mut self, seq: u64, frame: Vec<u8>) -> Offer {
        let sticky_expects = self.pending.iter().next().copied();
        let fits = self.buffered + frame.len() <= self.cap || self.queue.is_empty();
        let accept = match sticky_expects {
            Some(want) => want == seq && fits,
            None => fits,
        };
        if !accept {
            self.pending.insert(seq);
            self.bounces += 1;
            return Offer::Bounced;
        }
        self.pending.remove(&seq);
        self.buffered += frame.len();
        self.forwarded.push(seq);
        self.queue.push_back((seq, frame));
        Offer::Forwarded
    }

    /// 目标连接读走至多 `max_bytes`（至少一封）。
    pub fn read(&mut self, max_bytes: usize) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut taken = 0;
        while let Some((_, f)) = self.queue.front() {
            if !out.is_empty() && taken + f.len() > max_bytes {
                break;
            }
            let (seq, f) = self.queue.pop_front().expect("front exists");
            self.buffered -= f.len();
            taken += f.len();
            out.push((seq, f));
        }
        out
    }
}

/// 假 hub：从 daemon 出站通道里取信封，对发给 `to` 的 `data` / `notify` 走 [`StickyTarget`]，被退回的立刻调用
/// `outbox.on_backpressure`（和真的 hub 发 `backpressure` 回执等价）；被转发的按 `read_per_tick` / `tick` 的速度被「目标连接」读走，
/// 通过 `delivered` 交给测试。其它信封（握手、控制）原样记在 `others`。
pub struct SimHub {
    pub target: Arc<Mutex<StickyTarget>>,
    /// 被目标读走的信封（顺序 = 目标看到的顺序）。
    pub delivered: mpsc::UnboundedReceiver<anycon_protocol::Envelope>,
}

impl SimHub {
    pub fn spawn(
        outbox: Arc<crate::hub::HubOutbox>,
        mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
        to: &str,
        cap: usize,
        read_per_tick: usize,
        tick: Duration,
    ) -> SimHub {
        let target = Arc::new(Mutex::new(StickyTarget::new(cap)));
        let (dtx, delivered) = mpsc::unbounded_channel();
        let to = to.to_string();
        {
            let (target, to) = (target.clone(), to.clone());
            tokio::spawn(async move {
                while let Some(bytes) = rx.recv().await {
                    outbox.wire_done(bytes.len());
                    let Ok(env) = anycon_protocol::Envelope::decode(&bytes) else { continue };
                    if env.to != to || !matches!(env.kind, anycon_protocol::Kind::Data | anycon_protocol::Kind::Notify)
                    {
                        continue;
                    }
                    let seq = env.seq;
                    let offered = target.lock().unwrap().offer(seq, bytes);
                    if offered == Offer::Bounced {
                        outbox.on_backpressure(&to, seq);
                    }
                }
            });
        }
        {
            let target = target.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(tick).await;
                    let batch = target.lock().unwrap().read(read_per_tick);
                    for (_, f) in batch {
                        if let Ok(env) = anycon_protocol::Envelope::decode(&f) {
                            if dtx.send(env).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
        SimHub { target, delivered }
    }
}

// ====================================================================== 假 web 客户端（hub 联调测试用）

use std::collections::VecDeque;

use anycon_crypto::{derive_pair_secrets, fingerprint, Handshake, Keypair, Role, Transport};
use anycon_protocol::envelope::frame;
use anycon_protocol::{ClientInfo, Control, DeviceKind, Envelope, Kind, PairAck, PairRole, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const T: Duration = Duration::from_secs(10);

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct FakeWeb {
    pub ws: Ws,
    pub seq: u64,
    pub device_id: String,
    pub token: Option<String>,
    pub kp: Keypair,
    pub name: String,
    pub node_id: Option<String>,
    pub node_pub: Option<[u8; 32]>,
    pub transport: Option<Transport>,
    pub next_rpc: u64,
    pub notes: VecDeque<RpcNotification>,
    pub controls: VecDeque<Control>,
    pub online: Vec<String>,
    /// 出站消息的 `msg_id`（每条 RPC 一个）。
    pub msg_id: u32,
    /// 入站分片重组。
    pub reasm: frame::Reassembler,
    /// 最近一次配对收到的 `pair.ack`。
    pub last_ack: Option<PairAck>,
    /// 收到过多少个分片信封（测大消息确实分片了）。
    pub fragments_seen: usize,
}

impl FakeWeb {
    /// `hub_url` 形如 `http://127.0.0.1:PORT`。
    pub async fn connect(
        hub_url: &str,
        token: Option<String>,
        cookie: Option<&str>,
        name: &str,
        kp: Keypair,
    ) -> FakeWeb {
        Self::connect_with(hub_url, token, cookie, name, kp, None).await
    }

    /// 同 [`FakeWeb::connect`]；`recv_buffer` 给定时把 socket 的接收缓冲（`SO_RCVBUF`）定死在这个大小。
    /// 定死之后内核不再自动调大它，客户端不读时内核里能积压多少字节就有了上限——模拟读得慢的客户端时，
    /// hub 的发送缓冲才一定会满（否则 macOS / Linux 会把它自动调到几 MiB，大块数据全被内核吞下）。
    pub async fn connect_with(
        hub_url: &str,
        token: Option<String>,
        cookie: Option<&str>,
        name: &str,
        kp: Keypair,
        recv_buffer: Option<u32>,
    ) -> FakeWeb {
        let url = format!("{}/v1/ws", hub_url.replacen("http://", "ws://", 1));
        let mut req = url.into_client_request().unwrap();
        req.headers_mut().insert("Origin", hub_url.parse().unwrap());
        if let Some(c) = cookie {
            req.headers_mut().insert("Cookie", c.parse().unwrap());
        }
        let ws = match recv_buffer {
            None => tokio_tungstenite::connect_async(req).await.expect("ws connects").0,
            Some(n) => {
                let addr: std::net::SocketAddr = hub_url.trim_start_matches("http://").parse().expect("http://IP:PORT");
                let sock =
                    if addr.is_ipv4() { tokio::net::TcpSocket::new_v4() } else { tokio::net::TcpSocket::new_v6() };
                let sock = sock.expect("tcp socket");
                sock.set_recv_buffer_size(n).expect("SO_RCVBUF");
                let tcp = sock.connect(addr).await.expect("tcp connects");
                tokio_tungstenite::client_async(req, MaybeTlsStream::Plain(tcp)).await.expect("ws connects").0
            }
        };
        let mut w = FakeWeb {
            ws,
            seq: 0,
            device_id: String::new(),
            token: token.clone(),
            kp,
            name: name.to_string(),
            node_id: None,
            node_pub: None,
            transport: None,
            next_rpc: 0,
            notes: VecDeque::new(),
            controls: VecDeque::new(),
            online: vec![],
            msg_id: 0,
            reasm: frame::Reassembler::new(),
            last_ack: None,
            fragments_seen: 0,
        };
        w.send_control(Control::Auth {
            device_token: token,
            protocol_version: PROTOCOL_VERSION,
            client: ClientInfo {
                name: name.into(),
                kind: DeviceKind::Web,
                platform: "browser".into(),
                version: "test".into(),
                origin: None,
            },
            preview_token: None,
        })
        .await;
        match w.recv_control().await {
            Control::AuthOk { device_id, peers_online, .. } => {
                w.device_id = device_id;
                w.online = peers_online;
            }
            other => panic!("expected auth_ok, got {other:?}"),
        }
        w
    }

    pub async fn send_env(&mut self, to: &str, kind: Kind, body: Vec<u8>) {
        self.seq += 1;
        let bytes = Envelope::new(self.device_id.clone(), to, kind, self.seq, body).encode().unwrap();
        self.ws.send(Message::Binary(bytes.into())).await.unwrap();
    }

    pub async fn send_control(&mut self, c: Control) {
        self.seq += 1;
        let bytes = Envelope::control("", self.seq, &c).unwrap().encode().unwrap();
        self.ws.send(Message::Binary(bytes.into())).await.unwrap();
    }

    pub async fn recv_env(&mut self, timeout: Duration) -> Option<Envelope> {
        loop {
            match tokio::time::timeout(timeout, self.ws.next()).await {
                Ok(Some(Ok(Message::Binary(b)))) => return Some(Envelope::decode(&b).unwrap()),
                Ok(Some(Ok(Message::Ping(_)))) | Ok(Some(Ok(Message::Pong(_)))) => continue,
                _ => return None,
            }
        }
    }

    pub async fn recv_control(&mut self) -> Control {
        if let Some(c) = self.controls.pop_front() {
            return c;
        }
        loop {
            let env = self.recv_env(T).await.expect("timed out waiting for a control message");
            if env.kind == Kind::Control {
                return env.control_body().unwrap();
            }
        }
    }

    /// 等一条满足条件的 control；其它的暂存。
    pub async fn wait_control(&mut self, pred: impl Fn(&Control) -> bool) -> Control {
        if let Some(i) = self.controls.iter().position(&pred) {
            return self.controls.remove(i).unwrap();
        }
        loop {
            let env = self.recv_env(T).await.expect("timed out waiting for control");
            if env.kind == Kind::Control {
                let c = env.control_body().unwrap();
                if pred(&c) {
                    return c;
                }
                self.controls.push_back(c);
            }
        }
    }

    /// joiner 配对：返回 (host 正式 id, host 静态公钥)。
    ///
    /// 和真正的 joiner 一样：算 `match_id` 发给 hub（短码本身不出本机）、XXpsk3 握手、`PairDone` 之后等 daemon 经配对传输发来的
    /// `pair.ack` 并校验（`peer_id` 是自己的正式 id、`node_id` 是 `PairDone.peer`、指纹是握手得到的公钥的指纹），通过才算配对成功。
    pub async fn pair(&mut self, code: &str) -> (String, [u8; 32]) {
        let (host_temp, host_pub, mut pair_transport) = self.pair_handshake(code).await;
        self.pair_finish(host_temp, host_pub, &mut pair_transport).await
    }

    /// joiner 配对的前半段：`PairJoin`、XXpsk3 握手（含第 3 条里的 `ClientInfo`），**不发 `PairConfirm`**。
    /// 返回 (daemon 的临时 id, daemon 静态公钥, 配对用的传输)——测「确认阶段 hub 断线 / 看门狗」用。
    pub async fn pair_handshake(&mut self, code: &str) -> (String, [u8; 32], Transport) {
        // 一次 Argon2id 同时得到 match_id 和 PSK（和真正的 joiner 一样）
        let secrets = derive_pair_secrets(code).unwrap();
        self.send_control(Control::PairJoin { match_id: secrets.match_id.clone() }).await;
        let host_temp =
            match self.wait_control(|c| matches!(c, Control::PairMatched { .. } | Control::PairFailed { .. })).await {
                Control::PairMatched { peer, role } => {
                    assert_eq!(role, PairRole::Joiner);
                    peer
                }
                other => panic!("pairing failed: {other:?}"),
            };
        let mut hs = Handshake::pairing(Role::Initiator, &self.kp, &secrets.psk).unwrap();
        let m1 = hs.write(&[]).unwrap();
        self.send_env(&host_temp, Kind::Handshake, m1).await;
        let m2 = loop {
            let env = self.recv_env(T).await.expect("no handshake reply from the daemon");
            if env.kind == Kind::Handshake {
                break env.body;
            }
        };
        let node_info: Value =
            serde_json::from_slice(&hs.read(&m2).unwrap()).expect("message 2 carries the node info JSON");
        assert_eq!(node_info["name"], json!("test-node"));
        assert_eq!(node_info["version"], json!(env!("CARGO_PKG_VERSION")));
        // 第三条的 payload 带上本设备信息（daemon 据此记名字和类型）
        let info = ClientInfo {
            name: self.name.clone(),
            kind: DeviceKind::Web,
            platform: "browser".into(),
            version: "test".into(),
            origin: None,
        };
        let m3 = hs.write(&serde_json::to_vec(&info).unwrap()).unwrap();
        assert!(hs.is_finished());
        let host_pub = hs.remote_public().unwrap();
        let pair_transport = hs.into_transport().unwrap();
        self.send_env(&host_temp, Kind::Handshake, m3).await;
        (host_temp, host_pub, pair_transport)
    }

    /// joiner 配对的后半段：发 `PairConfirm`，等 `PairDone` 和 `pair.ack` 并校验。
    pub async fn pair_finish(
        &mut self,
        host_temp: String,
        host_pub: [u8; 32],
        pair_transport: &mut Transport,
    ) -> (String, [u8; 32]) {
        self.send_control(Control::PairConfirm { peer: host_temp.clone() }).await;
        let (peer, device_id, device_token) =
            match self.wait_control(|c| matches!(c, Control::PairDone { .. } | Control::PairFailed { .. })).await {
                Control::PairDone { peer, device_id, device_token } => (peer, device_id, device_token),
                other => panic!("pairing failed: {other:?}"),
            };
        if let Some(id) = device_id {
            self.device_id = id;
        }
        // daemon 经配对传输发来的 pair.ack：校验通过才落盘节点
        let ack = loop {
            let env = self.recv_env(T).await.expect("no pair.ack from the daemon after pair_done");
            match env.kind {
                Kind::Data => {
                    assert_eq!(env.from, peer, "pair.ack comes from the node's permanent id");
                    let mut r = frame::Reassembler::new();
                    let ct = match r.push(frame::decode(&env.body).unwrap()).unwrap() {
                        Some(ct) => ct,
                        None => continue,
                    };
                    let plain = pair_transport.decrypt(&ct).expect("pair.ack decrypts under the pairing transport");
                    match RpcMessage::decode(&plain).unwrap() {
                        RpcMessage::Notification(n) if n.method == anycon_protocol::methods::PAIR_ACK => {
                            break serde_json::from_value::<PairAck>(n.params).expect("PairAck shape")
                        }
                        other => panic!("first data message must be pair.ack, got {other:?}"),
                    }
                }
                Kind::Control => self.controls.push_back(env.control_body().unwrap()),
                _ => {}
            }
        };
        assert_eq!(ack.peer_id, self.device_id, "ack names this joiner's permanent id");
        assert_eq!(ack.node_id, peer, "ack names the node's permanent id");
        assert_eq!(ack.fingerprint, fingerprint(&host_pub), "ack carries the fingerprint of the handshake key");
        assert_eq!(ack.name, "test-node");
        assert_eq!(ack.version, env!("CARGO_PKG_VERSION"));
        if let Some(t) = device_token {
            self.token = Some(t);
        }
        self.node_id = Some(peer.clone());
        self.node_pub = Some(host_pub);
        self.last_ack = Some(ack);
        (peer, host_pub)
    }

    /// IK 握手，成功后建立 Transport。
    pub async fn ik(&mut self) -> Result<(), String> {
        let (node_id, node_pub) = (self.node_id.clone().unwrap(), self.node_pub.unwrap());
        let mut hs = Handshake::reconnect(Role::Initiator, &self.kp, Some(&node_pub)).map_err(|e| e.to_string())?;
        let m1 = hs.write(&[]).map_err(|e| e.to_string())?;
        self.send_env(&node_id, Kind::Handshake, m1).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(env) = self.recv_env(left).await else { return Err("no reply to the IK handshake".into()) };
            match env.kind {
                Kind::Handshake => {
                    hs.read(&env.body).map_err(|e| e.to_string())?;
                    assert!(hs.is_finished());
                    self.transport = Some(hs.into_transport().map_err(|e| e.to_string())?);
                    self.reasm = frame::Reassembler::new();
                    return Ok(());
                }
                Kind::Control => self.controls.push_back(env.control_body().unwrap()),
                _ => {}
            }
        }
    }

    pub async fn send_rpc(&mut self, method: &str, params: Value) -> u64 {
        self.next_rpc += 1;
        let id = self.next_rpc;
        let msg = RpcMessage::request(id, method, params);
        let ct = self.transport.as_mut().unwrap().encrypt(&msg.encode()).unwrap();
        self.msg_id += 1;
        let node = self.node_id.clone().unwrap();
        // 和真正的客户端一样：超过一片就分片（`envelope::frame`）
        for body in frame::encode(&ct, self.msg_id) {
            self.send_env(&node, Kind::Data, body).await;
        }
        id
    }

    /// 读一个 Data 信封并解出内层消息。None = 超时。
    pub async fn recv_msg(&mut self, timeout: Duration) -> Option<RpcMessage> {
        loop {
            let env = self.recv_env(timeout).await?;
            match env.kind {
                Kind::Data => {
                    let ct = match frame::decode(&env.body).expect("data frame") {
                        frame::DataFrame::Reset => panic!("the daemon asked for a transport reset"),
                        f => {
                            if matches!(f, frame::DataFrame::Fragment { .. }) {
                                self.fragments_seen += 1;
                            }
                            match self.reasm.push(f).expect("fragments arrive in order") {
                                Some(ct) => ct,
                                None => continue,
                            }
                        }
                    };
                    let plain = self.transport.as_mut().unwrap().decrypt(&ct).expect("decrypt");
                    return Some(RpcMessage::decode(&plain).unwrap());
                }
                Kind::Control => self.controls.push_back(env.control_body().unwrap()),
                _ => {}
            }
        }
    }

    pub async fn call_raw(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.send_rpc(method, params).await;
        loop {
            match self.recv_msg(T).await.expect("rpc response timed out") {
                RpcMessage::Response(r) if r.id == RpcId::Num(id) => {
                    return r.error.map(Err).unwrap_or_else(|| Ok(r.result.unwrap_or(Value::Null)))
                }
                RpcMessage::Notification(n) => self.notes.push_back(n),
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    pub async fn call<R: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<R, RpcError> {
        self.call_raw(method, params).await.map(|v| serde_json::from_value(v).unwrap())
    }

    pub async fn next_note(&mut self, pred: impl Fn(&RpcNotification) -> bool) -> RpcNotification {
        if let Some(i) = self.notes.iter().position(&pred) {
            return self.notes.remove(i).unwrap();
        }
        loop {
            match self.recv_msg(T).await.expect("notification timed out") {
                RpcMessage::Notification(n) if pred(&n) => return n,
                RpcMessage::Notification(n) => self.notes.push_back(n),
                _ => {}
            }
        }
    }
}

// ====================================================================== 账号内接入：进程内的假 hub 一侧

use anycon_protocol::EnrollFrame;

/// daemon 出站通道里读出来的一封信封，按用途拆好。
#[derive(Debug, Clone)]
pub enum LinkOut {
    /// 发给新设备的接入握手帧。
    Frame { to: String, frame: EnrollFrame },
    /// daemon 发给 hub 的控制消息（`pair_confirm`、`enroll_cancel`…）。
    Control(Control),
    /// `data` 信封（接入完成后的 `pair.ack` 等）。
    Data { to: String, body: Vec<u8> },
    /// `notify` 信封。
    Notify { to: String },
    /// 其它握手（IK 回复等）。
    Handshake { to: String, body: Vec<u8> },
}

/// 假 hub 的 daemon 一侧：把 daemon 的出站通道接到一个 channel 上，测试直接喂 `enroll_request` / `EnrollFrame`，
/// 再从 channel 里读 daemon 发出的信封。不经 WebSocket、不需要真 hub。
///
/// 用法：`FakeHubLink::attach(&td)`（daemon 要用 `TestDaemon::start*` 启动，不连 hub）→ `request` → `send_frame` → `pump` / `take_*`。
pub struct FakeHubLink {
    /// daemon 的正式 device id（`enroll_channel_id` 绑定它）。
    pub node_id: String,
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
    log: VecDeque<LinkOut>,
}

impl FakeHubLink {
    pub fn attach(td: &TestDaemon) -> FakeHubLink {
        let node_id = "node".to_string() + &"0".repeat(28);
        let (tx, rx) = mpsc::unbounded_channel();
        td.ctx.outbox.attach(&node_id, tx);
        td.ctx.identity.set_device(Some(&node_id), Some("test-device-token")).unwrap();
        FakeHubLink { node_id, rx, log: VecDeque::new() }
    }

    fn convert(bytes: &[u8]) -> LinkOut {
        let env = Envelope::decode(bytes).expect("daemon sends well-formed envelopes");
        match env.kind {
            Kind::Control => LinkOut::Control(env.control_body().expect("control body")),
            Kind::Handshake => match rmp_serde::from_slice::<EnrollFrame>(&env.body) {
                Ok(frame) => LinkOut::Frame { to: env.to, frame },
                Err(_) => LinkOut::Handshake { to: env.to, body: env.body },
            },
            Kind::Data => LinkOut::Data { to: env.to, body: env.body },
            Kind::Notify => LinkOut::Notify { to: env.to },
        }
    }

    /// 通道里已有的信封读进 `log`。
    pub fn pump(&mut self) {
        while let Ok(bytes) = self.rx.try_recv() {
            self.log.push_back(Self::convert(&bytes));
        }
    }

    /// 等 daemon 发出的下一封信封（先给 `log` 里还没被取走的）。假 hub 用它把 daemon 的出站一封封转给 WebSocket 另一端。
    pub async fn next(&mut self) -> Option<LinkOut> {
        if let Some(o) = self.log.pop_front() {
            return Some(o);
        }
        self.rx.recv().await.map(|b| Self::convert(&b))
    }

    /// hub 说：有设备请求接入这台 daemon。
    pub async fn request(&self, td: &TestDaemon, enroll_id: &str, peer: &str) {
        td.ctx.enroll.on_request(enroll_id.to_string(), peer.to_string()).await;
    }

    /// 新设备 `from` 发来一帧接入握手帧（hub 原样转发）。
    pub async fn send_frame(&self, td: &TestDaemon, from: &str, frame: &EnrollFrame) {
        let body = rmp_serde::to_vec_named(frame).unwrap();
        self.send_raw(td, from, &body).await;
    }

    pub async fn send_raw(&self, td: &TestDaemon, from: &str, body: &[u8]) {
        td.ctx.peers.on_handshake(from, body, &td.rpc()).await;
    }

    /// 取走所有发给 `to` 的接入握手帧（按顺序）。
    pub fn take_frames(&mut self, to: &str) -> Vec<EnrollFrame> {
        self.pump();
        let mut out = Vec::new();
        self.log.retain(|o| match o {
            LinkOut::Frame { to: t, frame } if t == to => {
                out.push(frame.clone());
                false
            }
            _ => true,
        });
        out
    }

    /// 取走所有控制消息。
    pub fn take_controls(&mut self) -> Vec<Control> {
        self.pump();
        let mut out = Vec::new();
        self.log.retain(|o| match o {
            LinkOut::Control(c) => {
                out.push(c.clone());
                false
            }
            _ => true,
        });
        out
    }

    /// 取走所有发给 `to` 的 `data` 信封体。
    pub fn take_data(&mut self, to: &str) -> Vec<Vec<u8>> {
        self.pump();
        let mut out = Vec::new();
        self.log.retain(|o| match o {
            LinkOut::Data { to: t, body } if t == to => {
                out.push(body.clone());
                false
            }
            _ => true,
        });
        out
    }

    /// 取走所有发给 `to` 的其它握手信封体（IK 回复）。
    pub fn take_handshakes(&mut self, to: &str) -> Vec<Vec<u8>> {
        self.pump();
        let mut out = Vec::new();
        self.log.retain(|o| match o {
            LinkOut::Handshake { to: t, body } if t == to => {
                out.push(body.clone());
                false
            }
            _ => true,
        });
        out
    }

    /// 取走并返回 `notify` 信封的个数。
    pub fn take_notifies(&mut self) -> usize {
        self.pump();
        let before = self.log.len();
        self.log.retain(|o| !matches!(o, LinkOut::Notify { .. }));
        before - self.log.len()
    }

    /// 还没被取走的东西（调试用）。
    pub fn pending(&mut self) -> Vec<LinkOut> {
        self.pump();
        self.log.iter().cloned().collect()
    }
}
