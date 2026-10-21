//! mock 的会话：一个由 `AgentHandle` 方法驱动的小状态机。每个 turn 跑在自己的 tokio 任务里，
//! 按 prompt 关键词走固定脚本（见 README.md），取消 / 关闭通过 `CancellationToken` 立刻生效。
//!
//! 除 `write` 关键词写 cwd 里的 `anycon-mock.txt` 之外，mock 只读 daemon 交给它的附件文件（见 [`attachments_chunk`]），
//! 别的文件系统一概不碰（见 [`write_mock_file`]）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::{
    AdapterError, AgentEvent, AgentHandle, PendingPermission, PermissionDecision, Result, ResumeHandle, SettingsApply,
    SpawnRequest, UserInput,
};
use anycon_protocol::{
    AttachmentRef, EditKind, Event, FileChangeKind, ModelInfo, PermissionDetail, PermissionKind, PermissionOption,
    PermissionOptionKind, PlanEntry, PlanStatus, Question, QuestionChoice, SessionSettings, ToolCallDetail,
    ToolCategory, ToolPermissionDetail, ToolResultDetail, TurnDoneReason, Usage,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{models_with, MODEL_LARGE, MODE_ASK, MODE_PLAN, MODE_YOLO};

/// `slow` 关键词下每个事件之间的停顿。
const SLOW_STEP: Duration = Duration::from_millis(200);
/// `TurnDone.usage.duration_ms` 固定值（确定性）。
const FIXED_DURATION_MS: u64 = 100;
const ECHO_PROMPT_MAX_CHARS: usize = 200;

/// `prompt_append=` 回显最多留多少字符。
const ECHO_APPEND_MAX_CHARS: usize = 32;

/// `write` 关键词写的文件名：cwd 里的这一个文件，别的路径一概不碰。
pub const WRITE_FILE_NAME: &str = "anycon-mock.txt";
/// 追加时，已有的文件超过这个大小就拒绝（保证 diff 里的上下文行和事件都很小）。
const WRITE_APPEND_MAX_BASE_BYTES: u64 = 64 * 1024;
/// `unified_diff` 里追加行之前带的上下文行数（和 `git diff` 的默认值一样）。
const DIFF_CONTEXT_LINES: usize = 3;

/// 当前生效的设置。
#[derive(Debug, Clone, PartialEq)]
struct Effective {
    model: String,
    thinking: Option<String>,
    mode: String,
    verbose: bool,
    system_prompt_append: Option<String>,
    max_turns: Option<u32>,
    max_budget_usd: Option<f64>,
}

fn invalid(field: &str, reason: impl Into<String>) -> AdapterError {
    AdapterError::InvalidSettings { field: field.to_string(), reason: reason.into() }
}

/// 把 `SessionSettings` 解析成完整的生效设置。**`settings` 就是完整的目标设置**（daemon 传来的是合并好的整份），
/// 没给的字段 = 清除，回到默认（模型 → 默认模型，思考 → 该模型的默认，模式 → `ask`，`verbose` → 关，
/// `system_prompt_append` / `max_turns` / `max_budget_usd` → 没有），不读当前值——和 claude-code adapter 一致，
/// 这样 `clear` 真的生效、e2e 可以从回显里断言。`models` 是内置模型加上 `config.additional_models`。
fn resolve(settings: &SessionSettings, models: &[ModelInfo]) -> Result<Effective> {
    let model_id = settings
        .model
        .clone()
        .or_else(|| models.iter().find(|m| m.default).map(|m| m.id.clone()))
        .unwrap_or_else(|| MODEL_LARGE.to_string());
    let model = models
        .iter()
        .find(|m| m.id == model_id)
        .ok_or_else(|| invalid("model", format!("unknown model {model_id}")))?;
    let thinking = match &model.thinking {
        None => {
            if let Some(t) = &settings.thinking {
                return Err(invalid("thinking", format!("{model_id} has no thinking options (got {t})")));
            }
            None
        }
        Some(t) => {
            let valid = |id: &str| t.options.iter().any(|o| o.id == id);
            match &settings.thinking {
                Some(th) if valid(th) => Some(th.clone()),
                Some(th) => return Err(invalid("thinking", format!("{th} not supported by {model_id}"))),
                None => t.default.clone(),
            }
        }
    };
    let mode = settings.mode.clone().unwrap_or_else(|| MODE_ASK.to_string());
    if ![MODE_ASK, MODE_PLAN, MODE_YOLO].contains(&mode.as_str()) {
        return Err(invalid("mode", format!("unknown mode {mode}")));
    }
    for k in settings.options.keys() {
        if k != "verbose" {
            return Err(invalid(&format!("options.{k}"), "unknown option"));
        }
    }
    let verbose = match settings.options.get("verbose") {
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(invalid("options.verbose", "expected a boolean")),
        None => false,
    };
    Ok(Effective {
        model: model_id,
        thinking,
        mode,
        verbose,
        system_prompt_append: settings.system_prompt_append.clone(),
        max_turns: settings.max_turns,
        max_budget_usd: settings.max_budget_usd,
    })
}

impl Effective {
    /// `ResumeHandle.launch_args`：这次有效的设置（只作记录，续接时不读）。
    fn launch_args(&self) -> Value {
        json!({
            "model": self.model,
            "thinking": self.thinking,
            "mode": self.mode,
            "verbose": self.verbose,
            "system_prompt_append": self.system_prompt_append,
            "max_turns": self.max_turns,
            "max_budget_usd": self.max_budget_usd,
        })
    }

    /// 回显里 `[model=… thinking=… mode=…]` 和 ` verbose=on` 之后追加的部分：
    /// ` prompt_append=<前 32 字符> max_turns=<n> budget=<x>`，没设的项不显示；一项都没有时返回 `None`。
    fn echo_extras(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(a) = &self.system_prompt_append {
            // 回显保持单行：换行 / 制表符换成空格
            let head: String = a
                .chars()
                .take(ECHO_APPEND_MAX_CHARS)
                .map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c })
                .collect();
            parts.push(format!("prompt_append={head}"));
        }
        if let Some(n) = self.max_turns {
            parts.push(format!("max_turns={n}"));
        }
        if let Some(x) = self.max_budget_usd {
            parts.push(format!("budget={x}"));
        }
        (!parts.is_empty()).then(|| format!(" {}", parts.join(" ")))
    }
}

struct Shared {
    session_id: String,
    cwd: PathBuf,
    effective: Mutex<Effective>,
    busy: AtomicBool,
    closed: AtomicBool,
    /// 用户选过 `allow_session` 之后，同一会话里后续的工具权限请求自动放行。
    allow_session: AtomicBool,
    pending: Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>,
    perm_seq: AtomicU64,
    tool_seq: AtomicU64,
    turn_seq: AtomicU64,
}

pub(crate) struct MockHandle {
    shared: Arc<Shared>,
    /// 内置模型 + `config.additional_models`（`set_settings` 解析用）。
    models: Vec<ModelInfo>,
    rx: mpsc::Receiver<AgentEvent>,
    tx: Option<mpsc::Sender<AgentEvent>>,
    token: CancellationToken,
    turn: Option<JoinHandle<()>>,
    resume: ResumeHandle,
}

impl MockHandle {
    pub(crate) fn start(req: SpawnRequest) -> Result<Self> {
        let models = models_with(&req.config);
        let effective = resolve(&req.settings, &models)?;
        let agent_session_id = req
            .resume
            .as_ref()
            .map(|r| r.agent_session_id.clone())
            .unwrap_or_else(|| format!("mock-{}", req.session_id));
        let resume = ResumeHandle { agent_session_id, launch_args: effective.launch_args() };
        let (tx, rx) = mpsc::channel(256);
        // 通道刚建好，一定放得下。
        let _ = tx.try_send(AgentEvent::Started { resume: resume.clone() });
        let shared = Arc::new(Shared {
            session_id: req.session_id,
            cwd: req.cwd,
            effective: Mutex::new(effective),
            busy: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            allow_session: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            perm_seq: AtomicU64::new(0),
            tool_seq: AtomicU64::new(0),
            turn_seq: AtomicU64::new(0),
        });
        Ok(Self { shared, models, rx, tx: Some(tx), token: CancellationToken::new(), turn: None, resume })
    }
}

#[async_trait::async_trait]
impl AgentHandle for MockHandle {
    fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.rx
    }

    async fn send(&mut self, input: UserInput) -> Result<()> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(AdapterError::Exited("mock agent was shut down".into()));
        }
        let Some(tx) = self.tx.clone() else {
            return Err(AdapterError::Exited("mock agent was shut down".into()));
        };
        if input.attachments.len() != input.attachment_paths.len() {
            return Err(AdapterError::Protocol("attachments and attachment_paths differ in length".into()));
        }
        if input.interject || self.shared.busy.swap(true, Ordering::SeqCst) {
            return Err(AdapterError::Unsupported("the mock agent does not accept input during a turn".into()));
        }
        let token = CancellationToken::new();
        self.token = token.clone();
        let turn_id = self.shared.turn_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let shared = self.shared.clone();
        let files: Vec<(AttachmentRef, PathBuf)> = input.attachments.into_iter().zip(input.attachment_paths).collect();
        self.turn = Some(tokio::spawn(run_turn(shared, tx, token, turn_id, input.content, files)));
        Ok(())
    }

    async fn respond_permission(&mut self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let sender = self.shared.pending.lock().expect("pending lock").remove(request_id);
        match sender {
            Some(s) => {
                let _ = s.send(decision);
            }
            // 取消 / 重复答复的竞态：幂等，不报错。
            None => tracing::debug!(request_id, "mock: no pending permission with this id"),
        }
        Ok(())
    }

    async fn set_settings(&mut self, settings: &SessionSettings) -> Result<SettingsApply> {
        // `settings` 是完整的目标设置：缺省字段 = 清除。解析失败时不改变现有设置。
        let next = resolve(settings, &self.models)?;
        self.resume.launch_args = next.launch_args();
        *self.shared.effective.lock().expect("effective lock") = next;
        Ok(SettingsApply { applied_now: true, restart_required: false })
    }

    async fn cancel(&mut self) -> Result<()> {
        if self.shared.busy.load(Ordering::SeqCst) {
            self.token.cancel();
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.shared.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.token.cancel();
        if let Some(mut h) = self.turn.take() {
            // turn 任务可能正卡在往满了的事件通道里发（daemon 在等 shutdown 返回时不读事件）：等一会儿，不行就放弃它
            if tokio::time::timeout(Duration::from_secs(2), &mut h).await.is_err() {
                h.abort();
            }
        }
        self.shared.pending.lock().expect("pending lock").clear();
        if let Some(tx) = self.tx.take() {
            // 同样不能在这里等通道腾出位置：交给一个任务去送，shutdown 立刻返回
            tokio::spawn(async move {
                let _ = tx.send(AgentEvent::Exited { code: Some(0), error: None }).await;
            });
        }
        Ok(())
    }

    fn resume_handle(&self) -> Option<ResumeHandle> {
        Some(self.resume.clone())
    }

    fn is_busy(&self) -> bool {
        self.shared.busy.load(Ordering::SeqCst)
    }
}

// ---------- turn 脚本 ----------

enum Stop {
    Cancelled,
}

struct Turn {
    shared: Arc<Shared>,
    tx: mpsc::Sender<AgentEvent>,
    token: CancellationToken,
    turn_id: u64,
    slow: bool,
    out_chars: u64,
}

impl Turn {
    /// 每个事件之前的停顿 / 取消检查。
    async fn pause(&self) -> std::result::Result<(), Stop> {
        if self.token.is_cancelled() {
            return Err(Stop::Cancelled);
        }
        if self.slow {
            tokio::select! {
                _ = tokio::time::sleep(SLOW_STEP) => {}
                _ = self.token.cancelled() => return Err(Stop::Cancelled),
            }
        }
        Ok(())
    }

    async fn emit(&mut self, ev: Event) -> std::result::Result<(), Stop> {
        self.pause().await?;
        if let Event::TextDelta { text } = &ev {
            self.out_chars += text.chars().count() as u64;
        }
        self.tx.send(AgentEvent::Event(ev)).await.map_err(|_| Stop::Cancelled)
    }

    /// 一段文字：若干 delta + `TextDone`。
    async fn text(&mut self, chunks: Vec<String>) -> std::result::Result<(), Stop> {
        for c in chunks {
            self.emit(Event::TextDelta { text: c }).await?;
        }
        self.emit(Event::TextDone {}).await
    }

    /// 发权限请求并等答复；取消时返回 `Stop`。
    async fn ask(&mut self, p: PendingPermission) -> std::result::Result<PermissionDecision, Stop> {
        self.pause().await?;
        let (otx, orx) = oneshot::channel();
        let rid = p.request_id.clone();
        self.shared.pending.lock().expect("pending lock").insert(rid.clone(), otx);
        if self.tx.send(AgentEvent::Permission(p)).await.is_err() {
            self.shared.pending.lock().expect("pending lock").remove(&rid);
            return Err(Stop::Cancelled);
        }
        tokio::select! {
            d = orx => d.map_err(|_| Stop::Cancelled),
            _ = self.token.cancelled() => {
                self.shared.pending.lock().expect("pending lock").remove(&rid);
                Err(Stop::Cancelled)
            }
        }
    }

    fn next_request_id(&self) -> String {
        let n = self.shared.perm_seq.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{}-perm-{n}", self.shared.session_id)
    }
}

fn option(id: &str, label: &str, kind: PermissionOptionKind, needs_message: bool) -> PermissionOption {
    PermissionOption { id: id.to_string(), label: label.to_string(), kind, needs_message }
}

fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

async fn run_turn(
    shared: Arc<Shared>,
    tx: mpsc::Sender<AgentEvent>,
    token: CancellationToken,
    turn_id: u64,
    prompt: String,
    files: Vec<(AttachmentRef, PathBuf)>,
) {
    let eff = shared.effective.lock().expect("effective lock").clone();
    let lower = prompt.to_lowercase();
    let mut turn =
        Turn { shared: shared.clone(), tx: tx.clone(), token, turn_id, slow: lower.contains("slow"), out_chars: 0 };
    let reason = match script(&mut turn, &eff, &prompt, &lower, &files).await {
        Ok(r) => r,
        Err(Stop::Cancelled) => TurnDoneReason::Cancelled,
    };
    let in_chars = prompt.chars().count().max(1) as u64;
    let usage = Usage {
        input_tokens: Some(in_chars),
        output_tokens: Some(turn.out_chars),
        cache_read_tokens: Some(0),
        cache_write_tokens: Some(0),
        cost_usd: Some((in_chars + turn.out_chars) as f64 * 1e-6),
        duration_ms: FIXED_DURATION_MS,
        model: Some(eff.model.clone()),
    };
    // 先清 busy 再发 TurnDone：消费者收到 TurnDone 后可以立刻 send 下一条。
    shared.busy.store(false, Ordering::SeqCst);
    let _ = tx.send(AgentEvent::Event(Event::TurnDone { turn_id, reason, usage: Some(usage) })).await;
}

async fn script(
    t: &mut Turn,
    eff: &Effective,
    prompt: &str,
    lower: &str,
    files: &[(AttachmentRef, PathBuf)],
) -> std::result::Result<TurnDoneReason, Stop> {
    if eff.thinking.is_some() {
        t.emit(Event::ThinkingDelta { text: "Considering the request. ".into() }).await?;
        t.emit(Event::ThinkingDelta { text: "Planning the answer. ".into() }).await?;
        t.emit(Event::ThinkingDone { duration_ms: 20 }).await?;
    }

    let mut chunks = vec![
        "Mock reply: ".to_string(),
        format!("[model={} thinking={} mode={}]", eff.model, eff.thinking.as_deref().unwrap_or("none"), eff.mode),
    ];
    if eff.verbose {
        chunks.push(" verbose=on".to_string());
    }
    if let Some(extras) = eff.echo_extras() {
        chunks.push(extras);
    }
    chunks.push(format!(" You said: {}", clip(prompt, ECHO_PROMPT_MAX_CHARS)));
    if !files.is_empty() {
        chunks.push(attachments_chunk(files).await);
    }
    t.text(chunks).await?;

    if lower.contains("plan") {
        plan_branch(t).await?;
    }
    if lower.contains("question") {
        question_branch(t).await?;
    }

    let wants_permission = lower.contains("permission") || (eff.mode == MODE_ASK && lower.contains("run"));
    if wants_permission {
        tool_branch(t).await?;
    } else if eff.mode == MODE_YOLO && lower.contains("run") {
        run_tool(t).await?;
    }

    if lower.contains("big") {
        big_output(t).await?;
    }

    // 不受 mode 影响、自己不发权限请求（要先走审批就同时带 `permission`，那条请求只管 Bash，答复不影响这里）
    if lower.contains("write") {
        write_file(t).await?;
    }

    if lower.contains("error") {
        t.emit(Event::Error {
            code: "mock_error".into(),
            message: "Mock error requested by the prompt".into(),
            recoverable: true,
        })
        .await?;
        return Ok(TurnDoneReason::Error);
    }
    Ok(TurnDoneReason::Completed)
}

/// 带附件时回显串末尾多出的那一个 `text_delta`：` Attachments: <name> (<mime>, <bytes> bytes, sha256 <前 12 位>)`，
/// 多个用 `; ` 连接。`bytes` 和 sha256 是**读 daemon 给的文件**得出的（证明路径是对的，不是照抄 `AttachmentRef`
/// 里的 `bytes`）；读不了写 `<name> (unreadable)`。这是 mock 除 `write` 之外碰文件系统的唯一地方，而且只读。
async fn attachments_chunk(files: &[(AttachmentRef, PathBuf)]) -> String {
    let mut parts = Vec::with_capacity(files.len());
    for (a, path) in files {
        parts.push(match tokio::fs::read(path).await {
            Ok(bytes) => {
                let digest = Sha256::digest(&bytes);
                let short: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
                format!("{} ({}, {} bytes, sha256 {short})", a.name, a.mime, bytes.len())
            }
            Err(_) => format!("{} (unreadable)", a.name),
        });
    }
    format!(" Attachments: {}", parts.join("; "))
}

async fn plan_branch(t: &mut Turn) -> std::result::Result<(), Stop> {
    let entries = [
        ("Inspect the project", PlanStatus::Completed),
        ("Implement the change", PlanStatus::InProgress),
        ("Run the tests", PlanStatus::Pending),
    ];
    t.emit(Event::Plan {
        entries: entries.iter().map(|(c, s)| PlanEntry { content: (*c).to_string(), status: *s }).collect(),
    })
    .await?;
    let req = PendingPermission {
        request_id: t.next_request_id(),
        kind: PermissionKind::Plan,
        title: "Plan ready".into(),
        detail: PermissionDetail {
            text: Some("## Plan\n\n1. Inspect the project\n2. Implement the change\n3. Run the tests".into()),
            ..Default::default()
        },
        options: vec![
            option("allow", "Start implementing", PermissionOptionKind::AllowOnce, false),
            option("deny", "Keep planning", PermissionOptionKind::RejectOnce, true),
        ],
        expires_in: None,
    };
    let d = t.ask(req).await?;
    let text = match d.option_id.as_deref() {
        Some("allow") => "Plan approved.".to_string(),
        _ => match d.message {
            Some(m) if !m.is_empty() => format!("Plan rejected: {m}"),
            _ => "Plan rejected.".to_string(),
        },
    };
    t.text(vec![text]).await
}

async fn question_branch(t: &mut Turn) -> std::result::Result<(), Stop> {
    let req = PendingPermission {
        request_id: t.next_request_id(),
        kind: PermissionKind::Question,
        title: "Agent has a question".into(),
        detail: PermissionDetail {
            questions: vec![Question {
                id: "q1".into(),
                text: "Which option do you prefer?".into(),
                choices: vec![
                    QuestionChoice { id: "a".into(), label: "Option A".into() },
                    QuestionChoice { id: "b".into(), label: "Option B".into() },
                ],
                multi: false,
                free_text: true,
            }],
            ..Default::default()
        },
        options: vec![
            option("answer", "Submit answers", PermissionOptionKind::AllowOnce, false),
            option("deny", "Skip", PermissionOptionKind::RejectOnce, true),
        ],
        expires_in: None,
    };
    let d = t.ask(req).await?;
    let text = match d.answers {
        Some(answers) if !answers.is_empty() => {
            let mut keys: Vec<&String> = answers.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    let v = match &answers[k] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a
                            .iter()
                            .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
                            .collect::<Vec<_>>()
                            .join(", "),
                        other => other.to_string(),
                    };
                    format!("{k}={v}")
                })
                .collect();
            format!("You answered: {}", parts.join("; "))
        }
        _ => "Question skipped.".to_string(),
    };
    t.text(vec![text]).await
}

async fn tool_branch(t: &mut Turn) -> std::result::Result<(), Stop> {
    if !t.shared.allow_session.load(Ordering::SeqCst) {
        let cwd = t.shared.cwd.to_string_lossy().into_owned();
        let req = PendingPermission {
            request_id: t.next_request_id(),
            kind: PermissionKind::Tool,
            title: "Run command".into(),
            detail: PermissionDetail {
                tool: Some(ToolPermissionDetail {
                    tool: "Bash".into(),
                    category: ToolCategory::Shell,
                    input: json!({ "command": "echo hi" }),
                    cwd: Some(cwd.clone()),
                    detail: Some(ToolCallDetail {
                        command: Some("echo hi".into()),
                        cwd: Some(cwd),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            options: vec![
                option("allow", "Allow", PermissionOptionKind::AllowOnce, false),
                option("allow_session", "Allow for this session", PermissionOptionKind::AllowAlways, false),
                option("deny", "Deny", PermissionOptionKind::RejectOnce, true),
            ],
            expires_in: None,
        };
        let d = t.ask(req).await?;
        match d.option_id.as_deref() {
            Some("allow") => {}
            Some("allow_session") => t.shared.allow_session.store(true, Ordering::SeqCst),
            _ => return t.text(vec!["skipped".to_string()]).await,
        }
    }
    run_tool(t).await
}

/// `big` 关键词用的大输出：开头 `BIGSTART\n`，之后 6000 行定长（50 字节）的 `line NNNNN …`，结尾 `BIGEND`，约 300 000 字节、全 ASCII。
/// e2e 用它覆盖分片和 daemon 的 64 KiB 输出上限（保留前 48 KiB + 后 16 KiB）。
pub fn big_output_text() -> String {
    const LINES: usize = 6000;
    let mut s = String::with_capacity(LINES * 50 + 16);
    s.push_str("BIGSTART\n");
    for i in 0..LINES {
        s.push_str(&format!("line {i:05} abcdefghijklmnopqrstuvwxyz0123456789ab\n"));
    }
    s.push_str("BIGEND");
    s
}

/// `tool_call`（`Bash` / `cat big.log`，不要权限请求）→ 一条很大的 `tool_result`。不发 `tool_output_delta`。
async fn big_output(t: &mut Turn) -> std::result::Result<(), Stop> {
    let n = t.shared.tool_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("{}-tool-{n}", t.shared.session_id);
    let cwd = t.shared.cwd.to_string_lossy().into_owned();
    t.emit(Event::ToolCall {
        id: id.clone(),
        name: "Bash".into(),
        category: ToolCategory::Shell,
        title: "cat big.log".into(),
        input: json!({ "command": "cat big.log" }),
        detail: Some(ToolCallDetail { command: Some("cat big.log".into()), cwd: Some(cwd), ..Default::default() }),
        parent_id: None,
    })
    .await?;
    t.emit(Event::ToolResult {
        id,
        output: json!(big_output_text()),
        is_error: false,
        detail: Some(ToolResultDetail { exit_code: Some(0), ..Default::default() }),
    })
    .await
}

async fn run_tool(t: &mut Turn) -> std::result::Result<(), Stop> {
    let n = t.shared.tool_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("{}-tool-{n}", t.shared.session_id);
    let cwd = t.shared.cwd.to_string_lossy().into_owned();
    t.emit(Event::ToolCall {
        id: id.clone(),
        name: "Bash".into(),
        category: ToolCategory::Shell,
        title: "echo hi".into(),
        input: json!({ "command": "echo hi" }),
        detail: Some(ToolCallDetail { command: Some("echo hi".into()), cwd: Some(cwd), ..Default::default() }),
        parent_id: None,
    })
    .await?;
    t.emit(Event::ToolOutputDelta { id: id.clone(), text: "hi\n".into() }).await?;
    t.emit(Event::ToolResult {
        id,
        output: json!("hi\n"),
        is_error: false,
        detail: Some(ToolResultDetail { exit_code: Some(0), ..Default::default() }),
    })
    .await
}

// ---------- write：mock 唯一会写文件的地方 ----------

/// 每个 turn 写进文件的那一行（不含换行）。
pub fn write_line(turn_id: u64) -> String {
    format!("mock write turn {turn_id}")
}

/// `write` 的结果：文件是新建还是追加，以及这次改动的 unified diff。
#[derive(Debug)]
struct Written {
    kind: FileChangeKind,
    diff: String,
}

#[derive(Debug, thiserror::Error)]
enum WriteError {
    /// 目标不符合"只写 cwd 里的一个普通文件"：不写，也不去碰它。
    #[error("refusing to write {WRITE_FILE_NAME}: {0}")]
    Refused(&'static str),
    #[error("{WRITE_FILE_NAME}: {0}")]
    Io(#[from] std::io::Error),
}

/// 在 `cwd` 里写 `anycon-mock.txt`：不存在就创建（一行），存在就追加一行，内容是 [`write_line`]。
///
/// 这是 mock 唯一会写的文件，而且只写 cwd 这一层这一个名字：
/// - **不跟随符号链接**：目标是符号链接（含悬空链接）就拒绝；创建用 `O_EXCL`，追加用 `O_NOFOLLOW`，
///   检查之后被换成链接也写不进去；
/// - 目标不是普通文件、已有内容超过 64 KiB 或不是 UTF-8 文本，同样拒绝；
/// - cwd 不存在 / 不可写等 I/O 错误原样报出。
fn write_mock_file(cwd: &Path, turn_id: u64) -> std::result::Result<Written, WriteError> {
    use std::fs::OpenOptions;
    let path = cwd.join(WRITE_FILE_NAME);
    let line = write_line(turn_id);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // create_new = O_CREAT | O_EXCL：已经有任何东西（包括符号链接）就失败，不会写到别处
            let mut f = OpenOptions::new().write(true).create_new(true).open(&path)?;
            f.write_all(format!("{line}\n").as_bytes())?;
            Ok(Written { kind: FileChangeKind::Created, diff: created_diff(&line) })
        }
        Err(e) => Err(e.into()),
        Ok(m) if m.file_type().is_symlink() => Err(WriteError::Refused("it is a symbolic link")),
        Ok(m) if !m.is_file() => Err(WriteError::Refused("it is not a regular file")),
        Ok(_) => {
            let mut opts = OpenOptions::new();
            opts.read(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // O_NOFOLLOW：lstat 之后被换成符号链接也不跟；O_NONBLOCK：换成 FIFO 之类不会卡住
                opts.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
            }
            let mut f = opts.open(&path)?;
            let meta = f.metadata()?;
            if !meta.is_file() {
                return Err(WriteError::Refused("it is not a regular file"));
            }
            if meta.len() > WRITE_APPEND_MAX_BASE_BYTES {
                return Err(WriteError::Refused("it is larger than 64 KiB"));
            }
            let mut old = Vec::with_capacity(meta.len() as usize);
            f.read_to_end(&mut old)?;
            let old = String::from_utf8(old).map_err(|_| WriteError::Refused("it is not UTF-8 text"))?;
            // 原来没有结尾换行就先补一个，保证新内容独占一行
            let sep = if old.is_empty() || old.ends_with('\n') { "" } else { "\n" };
            // O_APPEND：不管读到哪，写入都落在末尾
            f.write_all(format!("{sep}{line}\n").as_bytes())?;
            Ok(Written { kind: FileChangeKind::Modified, diff: append_diff(&old, &line) })
        }
    }
}

/// 新建文件的 diff（和 claude-code adapter 的 `created_diff` 同一种写法）。
fn created_diff(line: &str) -> String {
    format!("--- /dev/null\n+++ b/{WRITE_FILE_NAME}\n@@ -0,0 +1,1 @@\n+{line}\n")
}

/// 往已有内容 `old` 后面追加一行 `line` 的 diff：最多 3 行上下文，hunk 头的行数是准确的
/// （web 的 `normalizeDiff` 靠它判断 hunk 在哪结束）。`old` 没有结尾换行时，最后一行按 git 的写法
/// 变成"删一行 + `\ No newline at end of file` + 加两行"。
fn append_diff(old: &str, line: &str) -> String {
    let mut s = format!("--- a/{WRITE_FILE_NAME}\n+++ b/{WRITE_FILE_NAME}\n");
    if old.is_empty() {
        s.push_str(&format!("@@ -0,0 +1,1 @@\n+{line}\n"));
        return s;
    }
    let (body, has_eol) = match old.strip_suffix('\n') {
        Some(b) => (b, true),
        None => (old, false),
    };
    let lines: Vec<&str> = body.split('\n').collect();
    // 没有结尾换行时，最后一行本身也在改动里，上下文只从它前面取
    let (ctx_pool, last) =
        if has_eol { (&lines[..], None) } else { (&lines[..lines.len() - 1], lines.last().copied()) };
    let ctx = ctx_pool.len().min(DIFF_CONTEXT_LINES);
    let start = ctx_pool.len() - ctx + 1;
    let (old_len, new_len) = if last.is_some() { (ctx + 1, ctx + 2) } else { (ctx, ctx + 1) };
    s.push_str(&format!("@@ -{start},{old_len} +{start},{new_len} @@\n"));
    for l in &ctx_pool[ctx_pool.len() - ctx..] {
        s.push_str(&format!(" {l}\n"));
    }
    if let Some(l) = last {
        s.push_str(&format!("-{l}\n\\ No newline at end of file\n+{l}\n"));
    }
    s.push_str(&format!("+{line}\n"));
    s
}

/// `write` 关键词：`tool_call(Write, file_edit)` → `tool_result(unified_diff)` → `file_changed`。
/// 写失败（符号链接、cwd 不可写等）：`tool_result { is_error: true }`，没有 `file_changed`，turn 照常结束。
async fn write_file(t: &mut Turn) -> std::result::Result<(), Stop> {
    let n = t.shared.tool_seq.fetch_add(1, Ordering::SeqCst) + 1;
    let id = format!("{}-tool-{n}", t.shared.session_id);
    let line = write_line(t.turn_id);
    t.emit(Event::ToolCall {
        id: id.clone(),
        name: "Write".into(),
        category: ToolCategory::FileEdit,
        title: format!("Write {WRITE_FILE_NAME}"),
        input: json!({ "path": WRITE_FILE_NAME, "content": format!("{line}\n") }),
        detail: Some(ToolCallDetail {
            path: Some(WRITE_FILE_NAME.into()),
            edit_kind: Some(EditKind::Write),
            ..Default::default()
        }),
        parent_id: None,
    })
    .await?;
    let cwd = t.shared.cwd.clone();
    let turn_id = t.turn_id;
    let outcome = match tokio::task::spawn_blocking(move || write_mock_file(&cwd, turn_id)).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(e) => Err(format!("{WRITE_FILE_NAME}: write task failed: {e}")),
    };
    match outcome {
        Ok(w) => {
            let verb = if w.kind == FileChangeKind::Created { "Created" } else { "Modified" };
            t.emit(Event::ToolResult {
                id,
                output: json!(format!("{verb} {WRITE_FILE_NAME}")),
                is_error: false,
                detail: Some(ToolResultDetail { unified_diff: Some(w.diff), ..Default::default() }),
            })
            .await?;
            t.emit(Event::FileChanged { path: WRITE_FILE_NAME.into(), kind: w.kind }).await
        }
        Err(message) => t.emit(Event::ToolResult { id, output: json!(message), is_error: true, detail: None }).await,
    }
}
