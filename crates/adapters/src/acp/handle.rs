//! `AgentHandle` 的 ACP 实现：一个 daemon 会话在共享进程里的视图（[`Session`]）。
//!
//! - `send`：`session/prompt`。请求写出去就返回（一个 turn 可以跑很久，`AgentHandle` 的 `send` 不能等它），等响应的任务在
//!   响应到达时收尾 turn（`turn_done`）。agent 声明 `promptCapabilities.image` 时图片附件作 `image` 内容块（图在前、文字在后）。
//!   `interject` 恒为假：ACP v1 没有往进行中的 turn 里插话。
//! - `cancel`：先把未答复的权限请求 / elicitation 按取消答复（规格要求取消时必须这样），再发 `session/cancel` 通知，
//!   agent 以 `stopReason: cancelled` 结束 prompt。**不发 SIGINT 兜底**：进程是这个 agent 所有会话共用的，杀了会连累别的会话；
//!   agent 迟迟不收尾由 daemon 的取消宽限处理（它会让这个句柄 `shutdown`）。
//! - `set_settings`：模型 / 思考 / 模式 / 其余选项都是 `session/set_config_option`（旧式的模式用 `session/set_mode`），随时生效
//!   （`applied_now`），不需要重启。只设和上一次**请求值**不一样的：agent 自己切了模式（比如批准计划之后）不会被 daemon 记着的旧值冲掉。
//! - `shutdown`：会话脱离（见 `conn.rs`）；最后一个句柄走的时候关进程。幂等（daemon 会调两次）。
//!
//! 设置里 ACP 不支持的字段（`env`、`system_prompt_append`、`max_turns`、`max_budget_usd`）在 `validate_settings_values` 里拒绝。

use std::sync::Arc;

use anycon_agent_api::{
    AdapterError, AgentEvent, AgentHandle, PermissionDecision, Result, ResumeHandle, SettingsApply, UserInput,
};
use anycon_protocol::{Event, SessionSettings};
use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::common::IMAGE_MIMES;

use super::conn::{map_failure, Connection, Pool, Session, Timings};
use super::options::ModeTarget;
use super::permissions;
use super::rpc::{Rpc, RpcFailure};

/// ACP 不支持的会话设置。
pub fn validate_settings_values(s: &SessionSettings) -> Result<()> {
    let unsupported = |field: &str, reason: &str| {
        Err(AdapterError::InvalidSettings { field: field.to_string(), reason: reason.to_string() })
    };
    if !s.env.is_empty() {
        return unsupported(
            "env",
            "an ACP agent runs as one process shared by all its sessions, so a per-session environment is not supported; set it with agents.acp.<id>.env",
        );
    }
    if s.system_prompt_append.is_some() {
        return unsupported("system_prompt_append", "not supported by ACP agents");
    }
    if s.max_turns.is_some() {
        return unsupported("max_turns", "not supported by ACP agents");
    }
    if s.max_budget_usd.is_some() {
        return unsupported("max_budget_usd", "not supported by ACP agents");
    }
    Ok(())
}

/// 一个没能应用的设置。
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub field: String,
    pub reason: String,
}

fn problem(field: &str, reason: impl Into<String>) -> Problem {
    Problem { field: field.to_string(), reason: reason.into() }
}

/// `session/set_config_option`；成功后用响应里**完整**的选项列表换掉会话的选项状态（改模型可能让思考等级变了）。
async fn set_config(
    rpc: &Rpc,
    session: &Session,
    config_id: &str,
    value: &Value,
    timeout: std::time::Duration,
) -> std::result::Result<(), String> {
    let params = match value {
        Value::Bool(b) => json!({ "sessionId": session.id, "configId": config_id, "type": "boolean", "value": b }),
        v => json!({ "sessionId": session.id, "configId": config_id, "value": v }),
    };
    match rpc.request("session/set_config_option", params, timeout).await {
        Ok(v) => {
            let model = {
                let mut st = session.state.lock().expect("state lock");
                match v.get("configOptions") {
                    Some(opts) if opts.is_array() => st.replace_options(opts),
                    // 响应不带完整列表（不合规的 agent）：就地改这一项
                    _ => {
                        if let Some(o) = st.options.iter_mut().find(|o| o.id == config_id) {
                            o.current = value.clone();
                        }
                    }
                }
                st.current_model()
            };
            if let Some(m) = model {
                session.translator.lock().expect("translator lock").set_model(Some(m));
            }
            Ok(())
        }
        Err(RpcFailure::Rpc(e)) => Err(e.message),
        Err(other) => Err(other.to_string()),
    }
}

/// 把 `desired` 里和 `last`（上一次请求的设置；`None` = 新进程，和 agent 现在的值比）不一样的设置发给 agent，
/// 返回没能应用的。顺序：模型 → 思考等级 → 模式 → 其余选项（换模型会改变思考等级的候选，所以按这个顺序，每一步看最新的选项状态）。
pub async fn apply_settings(
    rpc: &Rpc,
    session: &Session,
    desired: &SessionSettings,
    last: Option<&SessionSettings>,
    timeout: std::time::Duration,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    let unchanged = |f: &dyn Fn(&SessionSettings) -> bool| last.is_some_and(f);

    if let Some(v) = &desired.model {
        if !unchanged(&|l| l.model.as_ref() == Some(v)) {
            let opt = session.state.lock().expect("state lock").model_option().cloned();
            match opt {
                None => problems.push(problem("model", "this agent does not offer a model choice")),
                Some(o) => match o.coerce(&json!(v)) {
                    Err(r) => problems.push(problem("model", r)),
                    Ok(val) if o.current != val => {
                        if let Err(r) = set_config(rpc, session, &o.id, &val, timeout).await {
                            problems.push(problem("model", r));
                        }
                    }
                    Ok(_) => {}
                },
            }
        }
    }
    if let Some(v) = &desired.thinking {
        if !unchanged(&|l| l.thinking.as_ref() == Some(v)) {
            let opt = session.state.lock().expect("state lock").thought_option().cloned();
            match opt {
                None => problems.push(problem("thinking", "this agent does not offer a thinking level choice")),
                Some(o) => match o.coerce(&json!(v)) {
                    Err(r) => problems.push(problem("thinking", r)),
                    Ok(val) if o.current != val => {
                        if let Err(r) = set_config(rpc, session, &o.id, &val, timeout).await {
                            problems.push(problem("thinking", r));
                        }
                    }
                    Ok(_) => {}
                },
            }
        }
    }
    if let Some(v) = &desired.mode {
        if !unchanged(&|l| l.mode.as_ref() == Some(v)) {
            let (target, current, choices) = {
                let st = session.state.lock().expect("state lock");
                (st.mode_target(), st.current_mode_id(), st.mode_choices())
            };
            if current.as_deref() != Some(v.as_str()) {
                match target {
                    None => problems.push(problem("mode", "this agent does not offer modes")),
                    Some(_) if !choices.iter().any(|m| &m.id == v) => problems.push(problem(
                        "mode",
                        format!(
                            "{v} is not one of: {}",
                            choices.iter().map(|m| m.id.as_str()).collect::<Vec<_>>().join(", ")
                        ),
                    )),
                    Some(ModeTarget::ConfigOption(id)) => {
                        if let Err(r) = set_config(rpc, session, &id, &json!(v), timeout).await {
                            problems.push(problem("mode", r));
                        }
                    }
                    Some(ModeTarget::Legacy) => {
                        match rpc
                            .request("session/set_mode", json!({ "sessionId": session.id, "modeId": v }), timeout)
                            .await
                        {
                            Ok(_) => session.state.lock().expect("state lock").set_current_mode(v),
                            Err(RpcFailure::Rpc(e)) => problems.push(problem("mode", e.message)),
                            Err(other) => problems.push(problem("mode", other.to_string())),
                        }
                    }
                }
            }
        }
    }
    for (k, v) in &desired.options {
        if last.is_some_and(|l| l.options.get(k) == Some(v)) {
            continue;
        }
        let field = format!("options.{k}");
        let opt = session.state.lock().expect("state lock").option(k).cloned();
        match opt {
            None => problems.push(problem(&field, "unknown option")),
            Some(o) => match o.coerce(v) {
                Err(r) => problems.push(problem(&field, r)),
                Ok(val) if o.current != val => {
                    if let Err(r) = set_config(rpc, session, &o.id, &val, timeout).await {
                        problems.push(problem(&field, r));
                    }
                }
                Ok(_) => {}
            },
        }
    }
    problems
}

/// 一条用户消息 → `session/prompt` 的内容块。图片在前、文字在后；只有图片时不发空文本。
pub async fn prompt_blocks(input: &UserInput, image_supported: bool) -> Result<Vec<Value>> {
    if input.attachments.len() != input.attachment_paths.len() {
        return Err(AdapterError::Protocol("attachments and attachment_paths differ in length".into()));
    }
    let mut blocks = Vec::new();
    for (a, path) in input.attachments.iter().zip(&input.attachment_paths) {
        if !IMAGE_MIMES.contains(&a.mime.as_str()) {
            return Err(AdapterError::Unsupported(format!(
                "ACP agents here only take image attachments ({} is {})",
                a.name, a.mime
            )));
        }
        if !image_supported {
            return Err(AdapterError::Unsupported(format!(
                "this agent does not accept images (promptCapabilities.image is false), so {} cannot be sent",
                a.name
            )));
        }
        let bytes = tokio::fs::read(path).await?;
        blocks.push(json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(bytes),
            "mimeType": a.mime,
        }));
    }
    if !input.content.is_empty() || input.attachments.is_empty() {
        blocks.push(json!({ "type": "text", "text": input.content }));
    }
    Ok(blocks)
}

pub struct AcpHandle {
    pool: Arc<Pool>,
    conn: Arc<Connection>,
    session: Arc<Session>,
    rx: mpsc::Receiver<AgentEvent>,
    /// 上一次请求的设置（`set_settings` 只发和它不一样的）。
    requested: SessionSettings,
    timings: Timings,
    /// `resume_handle().launch_args` 里记着的当前设置。
    launch_args: Value,
    closed: bool,
}

fn launch_args_of(s: &SessionSettings) -> Value {
    let mut m = serde_json::Map::new();
    if let Some(v) = &s.model {
        m.insert("model".into(), json!(v));
    }
    if let Some(v) = &s.thinking {
        m.insert("thinking".into(), json!(v));
    }
    if let Some(v) = &s.mode {
        m.insert("mode".into(), json!(v));
    }
    if !s.options.is_empty() {
        m.insert("options".into(), Value::Object(s.options.clone()));
    }
    Value::Object(m)
}

impl AcpHandle {
    pub(crate) fn new(
        pool: Arc<Pool>,
        conn: Arc<Connection>,
        session: Arc<Session>,
        requested: SessionSettings,
        timings: Timings,
    ) -> Self {
        let rx = session.attach();
        let launch_args = launch_args_of(&requested);
        Self { pool, conn, session, rx, requested, timings, launch_args, closed: false }
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// 拉起之后发 `Started`（agent 会话 id + 当前设置的快照）。
    pub(crate) fn announce_started(&self) {
        self.session.emit(AgentEvent::Started { resume: self.session.resume_handle(self.launch_args.clone()) });
    }

    /// 设置变了：补发一个 `Started`，让 daemon 把新的 `launch_args` 落库（尽力而为；daemon 在 `shutdown` 时还会用 `resume_handle()` 再取一次）。
    fn commit_launch_args(&mut self) {
        let args = launch_args_of(&self.requested);
        if args != self.launch_args {
            self.launch_args = args;
            self.announce_started();
        }
    }

    fn cancel_pending_requests(&self) {
        let pending: Vec<_> = self.session.pending.lock().expect("pending lock").drain().collect();
        for (_, native) in pending {
            if let Ok(v) = permissions::build_response(&native, &PermissionDecision::default(), true) {
                self.conn.rpc.reply(&native.rpc_id, v);
            }
        }
    }
}

#[async_trait::async_trait]
impl AgentHandle for AcpHandle {
    fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.rx
    }

    fn pid(&self) -> Option<u32> {
        self.conn.pid()
    }

    async fn send(&mut self, input: UserInput) -> Result<()> {
        if self.conn.is_dead() {
            return Err(AdapterError::Exited("the ACP agent is not running".into()));
        }
        if input.interject {
            // `capabilities.interject` 是 false，daemon 不会这样发
            return Err(AdapterError::Unsupported("ACP agents cannot take a message in the middle of a turn".into()));
        }
        if self.session.is_busy() {
            return Err(AdapterError::Unsupported("the agent is already running a turn".into()));
        }
        let blocks = prompt_blocks(&input, self.conn.caps().prompt_capabilities.image).await?;
        self.session.translator.lock().expect("translator lock").begin_turn();
        self.session.busy.send_replace(true);
        let pending =
            match self.conn.rpc.start("session/prompt", json!({ "sessionId": self.session.id, "prompt": blocks })) {
                Ok(p) => p,
                Err(e) => {
                    self.session.translator.lock().expect("translator lock").abort_turn();
                    self.session.busy.send_replace(false);
                    return Err(map_failure("session/prompt", e));
                }
            };
        let session = self.session.clone();
        tokio::spawn(async move {
            let result = pending.wait().await;
            session.finish_prompt(result);
        });
        Ok(())
    }

    async fn respond_permission(&mut self, request_id: &str, decision: PermissionDecision) -> Result<()> {
        let native = self.session.pending.lock().expect("pending lock").remove(request_id);
        let Some(native) = native else {
            // 取消 / 重复答复 / agent 自己了结的竞态：幂等。
            tracing::debug!(request_id, "acp: no pending permission with this id");
            return Ok(());
        };
        let cancelling = self.session.translator.lock().expect("translator lock").is_cancelling();
        let result = match permissions::build_response(&native, &decision, cancelling) {
            Ok(v) => v,
            Err(e) => {
                // 答复不合法：请求仍然未决，放回去。
                self.session.pending.lock().expect("pending lock").insert(request_id.to_string(), native);
                return Err(e);
            }
        };
        // 用户接受了表单，但回答转不成它要的类型：上面已经按拒绝答复了，把原因告诉用户
        if let Some(problem) = permissions::answer_problem(&native, &decision, cancelling) {
            self.session.emit_event(Event::Error {
                code: "elicitation_invalid_answer".to_string(),
                message: format!("The agent's form was declined because {problem}."),
                recoverable: true,
            });
        }
        if !self.conn.rpc.reply(&native.rpc_id, result) {
            return Err(AdapterError::Exited("the ACP agent is not running".into()));
        }
        Ok(())
    }

    async fn set_settings(&mut self, settings: &SessionSettings) -> Result<SettingsApply> {
        // 校验在做任何事之前，非法设置不会留下半截状态
        validate_settings_values(settings)?;
        if self.conn.is_dead() {
            return Err(AdapterError::Exited("the ACP agent is not running".into()));
        }
        let problems =
            apply_settings(&self.conn.rpc, &self.session, settings, Some(&self.requested), self.timings.request).await;
        if let Some(p) = problems.into_iter().next() {
            return Err(AdapterError::InvalidSettings { field: p.field, reason: p.reason });
        }
        self.requested = settings.clone();
        self.commit_launch_args();
        Ok(SettingsApply { applied_now: true, restart_required: false })
    }

    async fn cancel(&mut self) -> Result<()> {
        if !self.session.is_busy() {
            return Ok(());
        }
        self.session.translator.lock().expect("translator lock").mark_cancel_requested();
        // 未答复的请求按取消答复（规格：取消时必须），再通知 agent
        self.cancel_pending_requests();
        self.conn.rpc.notify("session/cancel", json!({ "sessionId": self.session.id }));
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let busy = self.session.is_busy();
        if busy {
            // 关闭期间 / 之后到达的东西都是预期内的
            self.session.translator.lock().expect("translator lock").mark_cancel_requested();
            self.cancel_pending_requests();
            if !self.conn.is_dead() {
                self.conn.rpc.notify("session/cancel", json!({ "sessionId": self.session.id }));
            }
        }
        // 进程还活着时通知读任务这是正常收尾：先发 `Exited`（daemon 把它当作句柄的终结），再脱离
        self.session.emit(AgentEvent::Exited { code: Some(0), error: None });
        self.conn.detach_session(&self.session, busy, &self.timings).await;
        self.pool.release(self.conn.clone(), &self.timings).await;
        Ok(())
    }

    fn resume_handle(&self) -> Option<ResumeHandle> {
        Some(self.session.resume_handle(self.launch_args.clone()))
    }

    fn is_busy(&self) -> bool {
        self.session.is_busy()
    }
}

impl Drop for AcpHandle {
    fn drop(&mut self) {
        // 没走 shutdown 就被丢弃：别留孤儿进程，也别让会话一直显示「挂着」。
        if !self.closed {
            self.session.detach();
            self.pool.release_nowait(&self.conn);
        }
    }
}
