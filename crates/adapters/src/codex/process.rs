//! 会话设置 → app-server 的 `thread/start` / `thread/resume` / `turn/start` 参数（docs/spec/daemon.md § 6.3 / § 7.4）。
//!
//! - **模式**映射（四档，`mode_spec`）：`approvalPolicy` × 沙箱 × `approvalsReviewer`。线上格式不一样：`thread/start` 的
//!   `sandbox` 是 `read-only` / `workspace-write` / `danger-full-access`（短横线），`turn/start` 的 `sandboxPolicy` 是
//!   `{ "type": "readOnly" | "workspaceWrite" | "dangerFullAccess" }`（驼峰）。
//! - **thread 开始时给一次**：`cwd`、`developerInstructions`（= `system_prompt_append`）。**resume 不会改写已经持久化的开发者指令**
//!   （实测：`thread/resume { developerInstructions }` 被忽略，rollout 里的赢），所以后来的改动走 `thread/inject_items`
//!   追加一条 developer 消息（见 [`instructions_notice`]），`ResumeHandle.launch_args.instructions_applied` 记着 thread 已经知道的文本。
//! - **按 turn 给**（下一个 turn 起生效，`turn/start` 的覆盖对「本 turn 及之后」有效）：`model`、`effort`、`summary`、
//!   `approvalPolicy`、`approvalsReviewer`；`sandboxPolicy` 与 `serviceTier` 只在与线程当前值不同时才传（[`Sent`]），
//!   这样没动过它们的会话保留用户 `config.toml` 里 `[sandbox_workspace_write]` / `service_tier` 的细节。
//! - `fast_mode` → `serviceTier`：开 = `priority`（`model/list` 里叫 "Fast" 的那档），明确关 = `default`（标准速度）。
//! - 不支持：`max_turns`、`max_budget_usd`（Codex 没有对应项）→ `invalid_settings`；`plan_mode`（`collaborationMode` 是
//!   experimental，稳定版 schema 的 `turn/start` 里没有）不声明。

use std::collections::BTreeMap;
use std::path::PathBuf;

use anycon_agent_api::{AdapterConfig, AdapterError, Result, ResumeHandle, UserInput};
use anycon_protocol::{AgentInfo, SessionSettings};
use serde_json::{json, Map, Value};

use super::probe::{self, DEFAULT_COMMAND};

pub const MODE_READ_ONLY: &str = "read_only";
pub const MODE_WORKSPACE: &str = "workspace";
pub const MODE_AUTO_REVIEW: &str = "auto_review";
pub const MODE_FULL_ACCESS: &str = "full_access";

pub const OPT_FAST_MODE: &str = "fast_mode";
pub const OPT_REASONING_SUMMARY: &str = "model_reasoning_summary";
pub const SUMMARY_CHOICES: &[&str] = &["auto", "concise", "detailed", "none"];

/// `model/list` 里叫 "Fast" 的服务档位 id。
pub const TIER_FAST: &str = "priority";
/// 标准速度。
pub const TIER_STANDARD: &str = "default";

/// 模型 id 最长。
pub const MAX_MODEL_LEN: usize = 200;
/// 思考等级最长。
pub const MAX_EFFORT_LEN: usize = 32;
/// `system_prompt_append` 最大字节数。
pub const MAX_SYSTEM_PROMPT_APPEND_BYTES: usize = 32 * 1024;
/// 作为图片交给模型的附件类型（和 claude-code / mock 同一份，见 [`crate::common::IMAGE_MIMES`]）。
pub use crate::common::IMAGE_MIMES;
use crate::common::{file_reference_text, is_inline_image};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl Sandbox {
    /// `thread/start.sandbox` 的取值。
    pub fn thread_value(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
            Sandbox::DangerFullAccess => "danger-full-access",
        }
    }

    /// `turn/start.sandboxPolicy` 的取值（不带任何细节：网络 / 可写根都用默认）。
    pub fn policy(self) -> Value {
        match self {
            Sandbox::ReadOnly => json!({ "type": "readOnly" }),
            Sandbox::WorkspaceWrite => json!({ "type": "workspaceWrite" }),
            Sandbox::DangerFullAccess => json!({ "type": "dangerFullAccess" }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSpec {
    pub approval_policy: &'static str,
    pub sandbox: Sandbox,
    pub reviewer: &'static str,
}

/// 四档权限模式 → 线上取值。
pub fn mode_spec(mode: &str) -> Option<ModeSpec> {
    Some(match mode {
        MODE_READ_ONLY => ModeSpec { approval_policy: "on-request", sandbox: Sandbox::ReadOnly, reviewer: "user" },
        MODE_WORKSPACE => {
            ModeSpec { approval_policy: "on-request", sandbox: Sandbox::WorkspaceWrite, reviewer: "user" }
        }
        MODE_AUTO_REVIEW => {
            ModeSpec { approval_policy: "on-request", sandbox: Sandbox::WorkspaceWrite, reviewer: "auto_review" }
        }
        MODE_FULL_ACCESS => ModeSpec { approval_policy: "never", sandbox: Sandbox::DangerFullAccess, reviewer: "user" },
        _ => return None,
    })
}

fn invalid(field: &str, reason: impl Into<String>) -> AdapterError {
    AdapterError::InvalidSettings { field: field.to_string(), reason: reason.into() }
}

/// 模型 id：非空、≤ [`MAX_MODEL_LEN`]，字母数字开头，之后 `[A-Za-z0-9._:/@\[\]+-]`。
pub fn check_model(model: &str) -> Result<()> {
    let ok = !model.is_empty()
        && model.len() <= MAX_MODEL_LEN
        && model.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && model.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@[]+-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(invalid(
            "model",
            format!(
                "model ids must start with a letter or digit, use only letters, digits and . _ : / @ [ ] + -, and be at most {MAX_MODEL_LEN} characters"
            ),
        ))
    }
}

/// 思考等级：字母数字开头，之后 `[A-Za-z0-9_-]`，≤ [`MAX_EFFORT_LEN`]。
pub fn check_effort(thinking: &str) -> Result<()> {
    let ok = !thinking.is_empty()
        && thinking.len() <= MAX_EFFORT_LEN
        && thinking.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && thinking.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(invalid("thinking", "invalid thinking level"))
    }
}

/// `system_prompt_append`：≤ [`MAX_SYSTEM_PROMPT_APPEND_BYTES`]，不含 NUL 等控制字符（换行 / 制表符可以）。
pub fn check_system_prompt_append(text: &str) -> Result<()> {
    if text.len() > MAX_SYSTEM_PROMPT_APPEND_BYTES {
        return Err(invalid(
            "system_prompt_append",
            format!("too long ({} bytes; the limit is {MAX_SYSTEM_PROMPT_APPEND_BYTES})", text.len()),
        ));
    }
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(invalid("system_prompt_append", "control characters are not allowed"));
    }
    Ok(())
}

/// 只看设置本身的值检查（不需要 `AgentInfo`）。`Adapter::validate_settings` 和 [`LaunchOptions::resolve`] 都会调。
pub fn validate_settings_values(settings: &SessionSettings) -> Result<()> {
    if let Some(m) = &settings.model {
        check_model(m)?;
    }
    if let Some(t) = &settings.thinking {
        check_effort(t)?;
    }
    if let Some(s) = &settings.system_prompt_append {
        check_system_prompt_append(s)?;
    }
    if settings.max_turns.is_some() {
        return Err(invalid("max_turns", "not supported by Codex"));
    }
    if settings.max_budget_usd.is_some() {
        return Err(invalid("max_budget_usd", "not supported by Codex"));
    }
    if let Some(m) = &settings.mode {
        if mode_spec(m).is_none() {
            return Err(invalid("mode", format!("unknown mode {m}")));
        }
    }
    if let Some(v) = settings.options.get(OPT_FAST_MODE) {
        if !v.is_boolean() {
            return Err(invalid("options.fast_mode", "expected a boolean"));
        }
    }
    if let Some(v) = settings.options.get(OPT_REASONING_SUMMARY) {
        if !v.as_str().is_some_and(|s| SUMMARY_CHOICES.contains(&s)) {
            return Err(invalid(
                "options.model_reasoning_summary",
                format!("expected one of {}", SUMMARY_CHOICES.join(", ")),
            ));
        }
    }
    Ok(())
}

/// 线程当前已经带着的、「只在与期望不同时才传」的那两项（见模块说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub sandbox: Sandbox,
    pub service_tier: Option<String>,
}

/// 一个 `codex app-server` 会话启动时用的设置（也是 `ResumeHandle.launch_args` 的主要内容）。
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchOptions {
    pub model: Option<String>,
    /// `effort` 的取值（`low` … `ultra`，以所选模型为准）。
    pub thinking: Option<String>,
    pub mode: String,
    pub system_prompt_append: Option<String>,
    /// `None` = 没指定，沿用 Codex 自己的配置；`Some(true)` = `priority`；`Some(false)` = `default`。
    pub fast_mode: Option<bool>,
    pub reasoning_summary: Option<String>,
    /// 会话设置里的 `env` 覆盖（进程环境本身由 daemon 合并好放在 `SpawnRequest.env`），只用来判断改了它要不要重启。
    pub env: BTreeMap<String, String>,
    /// 续接：`thread/resume`。
    pub resume_id: Option<String>,
    /// resume 时 thread 已经知道的 `system_prompt_append`（来自 `launch_args.instructions_applied`）；`None` = 不知道。
    pub resume_instructions: Option<Option<String>>,
}

impl LaunchOptions {
    /// 拉起（含 resume）时用：`settings` 是 daemon 传来的**完整目标设置**，没给的字段 = 清除，回到配置 / `AgentInfo`
    /// 的默认值（或「没有」），和 [`Self::resolve_desired`] 同一套语义。`ResumeHandle.launch_args` 只用来取
    /// `instructions_applied`（thread 已知的指令文本），其余一律不读（它可能是过期快照）。
    pub fn resolve(
        settings: &SessionSettings,
        config: &AdapterConfig,
        info: &AgentInfo,
        resume: Option<&ResumeHandle>,
    ) -> Result<Self> {
        let mut o = Self::build(settings, config, info)?;
        if let Some(r) = resume {
            o.resume_id = Some(r.agent_session_id.clone());
            o.resume_instructions = r.launch_args.get("instructions_applied").map(|v| v.as_str().map(str::to_string));
        }
        o.validate()?;
        Ok(o)
    }

    /// `set_settings` 用：完整的目标设置，会话 id 沿用 `current` 的。
    pub fn resolve_desired(
        settings: &SessionSettings,
        config: &AdapterConfig,
        info: &AgentInfo,
        current: &LaunchOptions,
    ) -> Result<Self> {
        let mut o = Self::build(settings, config, info)?;
        o.resume_id = current.resume_id.clone();
        o.resume_instructions = current.resume_instructions.clone();
        o.validate()?;
        Ok(o)
    }

    fn build(settings: &SessionSettings, config: &AdapterConfig, info: &AgentInfo) -> Result<Self> {
        validate_settings_values(settings)?;

        let model = settings
            .model
            .clone()
            .or_else(|| config.default_model.clone())
            .or_else(|| info.models.iter().find(|m| m.default).map(|m| m.id.clone()));

        // 思考：显式设置必须是所选模型支持的（模型不在列表里 / 没有等级信息就只做字符检查）；推导出来的值不合法就回落。
        let model_thinking =
            model.as_ref().and_then(|id| info.models.iter().find(|m| &m.id == id)).and_then(|m| m.thinking.as_ref());
        let allowed = |t: &str| match model_thinking {
            Some(opts) => opts.options.iter().any(|o| o.id == t),
            None => check_effort(t).is_ok(),
        };
        let thinking = match &settings.thinking {
            Some(t) => {
                if !allowed(t) {
                    return Err(invalid(
                        "thinking",
                        format!("{t} is not supported by {}", model.as_deref().unwrap_or("this model")),
                    ));
                }
                Some(t.clone())
            }
            None => config
                .default_thinking
                .clone()
                .filter(|t| allowed(t))
                .or_else(|| model_thinking.and_then(|o| o.default.clone())),
        };

        let mode = settings
            .mode
            .clone()
            .or_else(|| config.default_mode.clone())
            .or_else(|| info.modes.iter().find(|m| m.default).map(|m| m.id.clone()))
            .unwrap_or_else(|| MODE_WORKSPACE.to_string());
        if mode_spec(&mode).is_none() {
            return Err(invalid("mode", format!("unknown mode {mode}")));
        }

        Ok(Self {
            model,
            thinking,
            mode,
            system_prompt_append: settings.system_prompt_append.clone(),
            fast_mode: settings.options.get(OPT_FAST_MODE).and_then(Value::as_bool),
            reasoning_summary: settings.options.get(OPT_REASONING_SUMMARY).and_then(Value::as_str).map(str::to_string),
            env: settings.env.clone(),
            resume_id: None,
            resume_instructions: None,
        })
    }

    /// 检查最终要发出去的值。
    pub fn validate(&self) -> Result<()> {
        if let Some(m) = &self.model {
            check_model(m)?;
        }
        if let Some(t) = &self.thinking {
            check_effort(t)?;
        }
        if mode_spec(&self.mode).is_none() {
            return Err(invalid("mode", format!("unknown mode {}", self.mode)));
        }
        if let Some(s) = &self.system_prompt_append {
            check_system_prompt_append(s)?;
        }
        if let Some(id) = &self.resume_id {
            let ok = !id.is_empty()
                && id.len() <= 128
                && id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
                && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !ok {
                return Err(invalid("resume", "invalid agent session id"));
            }
        }
        Ok(())
    }

    pub fn spec(&self) -> ModeSpec {
        // `validate` 保证了模式合法
        mode_spec(&self.mode).unwrap_or_else(|| mode_spec(MODE_WORKSPACE).expect("workspace mode"))
    }

    /// `fast_mode` 对应的 `serviceTier`（`None` = 不指定）。
    pub fn service_tier(&self) -> Option<&'static str> {
        self.fast_mode.map(|f| if f { TIER_FAST } else { TIER_STANDARD })
    }

    /// 线程创建 / 恢复时带着的 `sandbox` 与 `serviceTier`，之后 `turn/start` 只在与它不同时才传。
    pub fn initial_sent(&self) -> Sent {
        Sent { sandbox: self.spec().sandbox, service_tier: self.service_tier().map(str::to_string) }
    }

    fn base_params(&self, cwd: &std::path::Path) -> Map<String, Value> {
        let spec = self.spec();
        let mut p = Map::new();
        p.insert("cwd".into(), json!(cwd.to_string_lossy()));
        if let Some(m) = &self.model {
            p.insert("model".into(), json!(m));
        }
        p.insert("approvalPolicy".into(), json!(spec.approval_policy));
        p.insert("approvalsReviewer".into(), json!(spec.reviewer));
        p.insert("sandbox".into(), json!(spec.sandbox.thread_value()));
        if let Some(t) = self.service_tier() {
            p.insert("serviceTier".into(), json!(t));
        }
        p
    }

    /// `thread/start` 的参数。
    pub fn thread_start_params(&self, cwd: &std::path::Path) -> Value {
        let mut p = self.base_params(cwd);
        if let Some(s) = &self.system_prompt_append {
            p.insert("developerInstructions".into(), json!(s));
        }
        Value::Object(p)
    }

    /// `thread/resume` 的参数。`excludeTurns`：不要把整段历史回给我们（daemon 自己有一份）。
    /// 不传 `developerInstructions`——resume 会忽略它（见模块说明）。
    pub fn thread_resume_params(&self, thread_id: &str, cwd: &std::path::Path) -> Value {
        let mut p = self.base_params(cwd);
        p.insert("threadId".into(), json!(thread_id));
        p.insert("excludeTurns".into(), json!(true));
        Value::Object(p)
    }

    /// `turn/start` 里的设置覆盖，以及发完之后线程的新状态（调用方在请求成功后才提交）。
    pub fn turn_overrides(&self, sent: &Sent) -> (Map<String, Value>, Sent) {
        let spec = self.spec();
        let mut p = Map::new();
        p.insert("approvalPolicy".into(), json!(spec.approval_policy));
        p.insert("approvalsReviewer".into(), json!(spec.reviewer));
        if let Some(m) = &self.model {
            p.insert("model".into(), json!(m));
        }
        if let Some(t) = &self.thinking {
            p.insert("effort".into(), json!(t));
        }
        if let Some(s) = &self.reasoning_summary {
            p.insert("summary".into(), json!(s));
        }
        if spec.sandbox != sent.sandbox {
            p.insert("sandboxPolicy".into(), spec.sandbox.policy());
        }
        let mut next = Sent { sandbox: spec.sandbox, service_tier: sent.service_tier.clone() };
        let want = match (self.service_tier(), sent.service_tier.as_deref()) {
            (Some(t), s) if s != Some(t) => Some(t),
            // 以前明确开过 fast、现在没指定了：退回标准速度，免得上一次的设置悄悄留着
            (None, Some(s)) if s != TIER_STANDARD => Some(TIER_STANDARD),
            _ => None,
        };
        if let Some(t) = want {
            p.insert("serviceTier".into(), json!(t));
            next.service_tier = Some(t.to_string());
        }
        (p, next)
    }

    /// 写进 `ResumeHandle.launch_args`。键名与 `SessionSettings` 同名（daemon 在没有存活进程时会按它们同步），
    /// 外加私有的 `instructions_applied`（thread 已经知道的 `system_prompt_append`）。
    pub fn to_json(&self, instructions_applied: &Option<String>) -> Value {
        json!({
            "model": self.model,
            "thinking": self.thinking,
            "mode": self.mode,
            "system_prompt_append": self.system_prompt_append,
            "fast_mode": self.fast_mode,
            "model_reasoning_summary": self.reasoning_summary,
            "instructions_applied": instructions_applied,
        })
    }
}

/// 把 `system_prompt_append` 的变化告诉已经存在的 thread：`thread/inject_items` 的一条 developer 消息。
pub fn instructions_notice(new: &Option<String>) -> Value {
    let text = match new.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(t) => format!(
            "The user changed this conversation's additional instructions. From now on follow the instructions below instead of any earlier additional instructions:\n\n{t}"
        ),
        None => "The user removed this conversation's additional instructions. Earlier additional instructions no longer apply."
            .to_string(),
    };
    json!({ "type": "message", "role": "developer", "content": [{ "type": "input_text", "text": text }] })
}

/// 用户输入 → `turn/start.input` / `turn/steer.input`（daemon 已落盘，路径与 `attachments` 一一对应）。
/// 其它文件的路径清单（[`file_reference_text`]）是第一项文字，然后是用户的文字（为空就不放），图片走 `localImage` 跟在后面。
/// Codex 的沙箱只限制写，读 daemon 数据目录里的文件不需要审批。
pub fn input_items(input: &UserInput) -> Result<Vec<Value>> {
    if input.attachments.len() != input.attachment_paths.len() {
        return Err(AdapterError::Protocol("attachments and attachment_paths differ in length".into()));
    }
    let mut items = Vec::new();
    if let Some(files) =
        file_reference_text(input.attachments.iter().zip(input.attachment_paths.iter().map(|p| p.as_path())))
    {
        items.push(json!({ "type": "text", "text": files }));
    }
    if !input.content.is_empty() || input.attachments.is_empty() {
        items.push(json!({ "type": "text", "text": input.content }));
    }
    for (a, path) in input.attachments.iter().zip(&input.attachment_paths) {
        if is_inline_image(&a.mime) {
            items.push(json!({ "type": "localImage", "path": path.to_string_lossy() }));
        }
    }
    Ok(items)
}

/// 找到要执行的程序 + 固定前置参数（`codex` 本身不需要子命令之外的前置参数；配置里 `command` 的多余词原样放前面）。
pub fn resolve_program(
    config: &AdapterConfig,
    probed_path: Option<&str>,
    env: &std::collections::HashMap<String, String>,
) -> Result<(PathBuf, Vec<String>)> {
    crate::common::bin::resolve_program(config, probed_path, env, DEFAULT_COMMAND, probe::INSTALL_HINT)
}

/// `codex app-server` 的参数（前置参数之后）。
pub fn app_server_args() -> Vec<String> {
    vec!["app-server".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::{AgentCapabilities, AgentStatus, AttachmentRef, Choice, ModelInfo, ThinkingOptions};

    fn model(id: &str, default: bool, levels: &[&str], default_level: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            label: id.into(),
            description: None,
            default,
            thinking: Some(ThinkingOptions {
                options: levels
                    .iter()
                    .map(|l| Choice { id: (*l).into(), label: (*l).into(), description: None })
                    .collect(),
                default: Some(default_level.into()),
            }),
            tags: vec![],
        }
    }

    fn info() -> AgentInfo {
        AgentInfo {
            id: "codex".into(),
            label: "Codex".into(),
            driver: "codex-app-server".into(),
            enabled: true,
            status: AgentStatus::Ready,
            version: Some("0.157.1".into()),
            path: None,
            issue: None,
            models: vec![
                model("m-big", true, &["low", "medium", "high", "ultra"], "medium"),
                model("m-small", false, &["low", "medium"], "low"),
            ],
            allow_custom_model: true,
            modes: probe::modes(),
            options: probe::options(),
            capabilities: AgentCapabilities::default(),
            probed_at_ms: 0,
        }
    }

    fn resolve(settings: SessionSettings, resume: Option<ResumeHandle>) -> Result<LaunchOptions> {
        LaunchOptions::resolve(&settings, &AdapterConfig::default(), &info(), resume.as_ref())
    }

    #[test]
    fn the_four_modes_map_to_policy_sandbox_and_reviewer() {
        let spec = |m| mode_spec(m).unwrap();
        assert_eq!(
            spec(MODE_READ_ONLY),
            ModeSpec { approval_policy: "on-request", sandbox: Sandbox::ReadOnly, reviewer: "user" }
        );
        assert_eq!(
            spec(MODE_WORKSPACE),
            ModeSpec { approval_policy: "on-request", sandbox: Sandbox::WorkspaceWrite, reviewer: "user" }
        );
        assert_eq!(
            spec(MODE_AUTO_REVIEW),
            ModeSpec { approval_policy: "on-request", sandbox: Sandbox::WorkspaceWrite, reviewer: "auto_review" }
        );
        assert_eq!(
            spec(MODE_FULL_ACCESS),
            ModeSpec { approval_policy: "never", sandbox: Sandbox::DangerFullAccess, reviewer: "user" }
        );
        assert!(mode_spec("plan").is_none());
        // 线上取值的写法
        assert_eq!(Sandbox::WorkspaceWrite.thread_value(), "workspace-write");
        assert_eq!(Sandbox::DangerFullAccess.policy(), json!({"type": "dangerFullAccess"}));
        assert_eq!(Sandbox::ReadOnly.policy(), json!({"type": "readOnly"}));
    }

    #[test]
    fn defaults_come_from_agent_info() {
        let o = resolve(SessionSettings::default(), None).unwrap();
        assert_eq!(o.model.as_deref(), Some("m-big"));
        assert_eq!(o.thinking.as_deref(), Some("medium"));
        assert_eq!(o.mode, "workspace");
        assert_eq!(o.fast_mode, None);
        assert_eq!(o.reasoning_summary, None);
        assert!(o.resume_id.is_none());
        // config 的默认值优先于 AgentInfo 的
        let cfg = AdapterConfig {
            default_model: Some("m-small".into()),
            default_thinking: Some("low".into()),
            default_mode: Some("read_only".into()),
            ..Default::default()
        };
        let o = LaunchOptions::resolve(&SessionSettings::default(), &cfg, &info(), None).unwrap();
        assert_eq!(
            (o.model.as_deref(), o.thinking.as_deref(), o.mode.as_str()),
            (Some("m-small"), Some("low"), "read_only")
        );
        // config 的默认思考等级不适用于所选模型时回落到模型默认
        let cfg = AdapterConfig { default_thinking: Some("ultra".into()), ..Default::default() };
        let o = LaunchOptions::resolve(
            &SessionSettings { model: Some("m-small".into()), ..Default::default() },
            &cfg,
            &info(),
            None,
        )
        .unwrap();
        assert_eq!(o.thinking.as_deref(), Some("low"));
    }

    #[test]
    fn thread_start_params_have_the_schema_shapes() {
        let o = resolve(
            SessionSettings {
                model: Some("m-small".into()),
                mode: Some("auto_review".into()),
                system_prompt_append: Some("Be terse.".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let p = o.thread_start_params(std::path::Path::new("/work/proj"));
        assert_eq!(
            p,
            json!({
                "cwd": "/work/proj", "model": "m-small",
                "approvalPolicy": "on-request", "approvalsReviewer": "auto_review", "sandbox": "workspace-write",
                "developerInstructions": "Be terse."
            })
        );
        let full = resolve(SessionSettings { mode: Some("full_access".into()), ..Default::default() }, None).unwrap();
        let p = full.thread_start_params(std::path::Path::new("/p"));
        assert_eq!((p["approvalPolicy"].clone(), p["sandbox"].clone()), (json!("never"), json!("danger-full-access")));
        assert!(p.get("developerInstructions").is_none());
    }

    /// ADR-34 第 9 条：cwd 是 worktree 时和普通目录一样对待——参数里除 `cwd` 之外一模一样，
    /// 不加 `writableRoots`（不把主仓库 `.git` 放进可写目录），`sandboxPolicy` 也不带任何细节。
    #[test]
    fn worktree_cwd_adds_no_writable_roots() {
        let plain = std::path::Path::new("/work/proj");
        let worktree = std::path::Path::new("/work/proj/.anycon/worktrees/fix-login");
        for mode in ["read_only", "workspace", "auto_review", "full_access"] {
            let o = resolve(SessionSettings { mode: Some(mode.into()), ..Default::default() }, None).unwrap();
            for (a, b) in [
                (o.thread_start_params(plain), o.thread_start_params(worktree)),
                (o.thread_resume_params("t1", plain), o.thread_resume_params("t1", worktree)),
            ] {
                assert_eq!(b["cwd"], "/work/proj/.anycon/worktrees/fix-login");
                let (mut a, mut b) = (a, b);
                a["cwd"] = json!("");
                b["cwd"] = json!("");
                assert_eq!(a, b, "{mode}");
                assert!(!b.to_string().contains("writableRoots"), "{mode}");
            }
        }
        for sandbox in [Sandbox::ReadOnly, Sandbox::WorkspaceWrite, Sandbox::DangerFullAccess] {
            let policy = sandbox.policy();
            assert_eq!(policy.as_object().unwrap().len(), 1, "{policy}");
        }
    }

    #[test]
    fn resume_params_skip_instructions_and_history() {
        let o = resolve(
            SessionSettings { system_prompt_append: Some("x".into()), ..Default::default() },
            Some(ResumeHandle {
                agent_session_id: "019abc-1".into(),
                launch_args: json!({"instructions_applied": "old", "model": "stale-model", "mode": "full_access"}),
            }),
        )
        .unwrap();
        // 只取 agent 会话 id 和 instructions_applied，launch_args 里的其它旧值不复活
        assert_eq!(o.resume_id.as_deref(), Some("019abc-1"));
        assert_eq!(o.resume_instructions, Some(Some("old".to_string())));
        assert_eq!(o.model.as_deref(), Some("m-big"));
        assert_eq!(o.mode, "workspace");
        let p = o.thread_resume_params("019abc-1", std::path::Path::new("/p"));
        assert_eq!(p["threadId"], "019abc-1");
        assert_eq!(p["excludeTurns"], true);
        assert!(p.get("developerInstructions").is_none());
        // launch_args 里没有 instructions_applied = 不知道
        let o = resolve(
            SessionSettings::default(),
            Some(ResumeHandle { agent_session_id: "t".into(), launch_args: json!({"model": "x"}) }),
        )
        .unwrap();
        assert_eq!(o.resume_instructions, None);
        let o = resolve(
            SessionSettings::default(),
            Some(ResumeHandle { agent_session_id: "t".into(), launch_args: json!({"instructions_applied": null}) }),
        )
        .unwrap();
        assert_eq!(o.resume_instructions, Some(None));
    }

    #[test]
    fn turn_overrides_always_carry_the_per_turn_settings() {
        let o = resolve(
            SessionSettings {
                model: Some("m-big".into()),
                thinking: Some("ultra".into()),
                mode: Some("read_only".into()),
                options: serde_json::Map::from_iter([(OPT_REASONING_SUMMARY.to_string(), json!("concise"))]),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let (p, next) = o.turn_overrides(&o.initial_sent());
        assert_eq!(
            Value::Object(p),
            json!({
                "approvalPolicy": "on-request", "approvalsReviewer": "user",
                "model": "m-big", "effort": "ultra", "summary": "concise"
            })
        );
        assert_eq!(next, o.initial_sent());
    }

    #[test]
    fn sandbox_policy_is_only_sent_when_it_changes() {
        let ws = resolve(SessionSettings::default(), None).unwrap();
        let ro = resolve(SessionSettings { mode: Some("read_only".into()), ..Default::default() }, None).unwrap();
        // 线程以 workspace-write 开始，现在要 read-only：传 sandboxPolicy
        let (p, next) = ro.turn_overrides(&ws.initial_sent());
        assert_eq!(p["sandboxPolicy"], json!({"type": "readOnly"}));
        assert_eq!(next.sandbox, Sandbox::ReadOnly);
        // 再来一个 turn，没有变化：不传
        let (p, _) = ro.turn_overrides(&next);
        assert!(p.get("sandboxPolicy").is_none());
        // auto_review 与 workspace 的沙箱相同：只换 reviewer，不传 sandboxPolicy
        let ar = resolve(SessionSettings { mode: Some("auto_review".into()), ..Default::default() }, None).unwrap();
        let (p, _) = ar.turn_overrides(&ws.initial_sent());
        assert!(p.get("sandboxPolicy").is_none());
        assert_eq!(p["approvalsReviewer"], "auto_review");
    }

    #[test]
    fn fast_mode_maps_to_service_tier_and_is_sticky_aware() {
        let opt = |v: Option<bool>| SessionSettings {
            options: v.map(|b| serde_json::Map::from_iter([(OPT_FAST_MODE.to_string(), json!(b))])).unwrap_or_default(),
            ..Default::default()
        };
        let none = resolve(opt(None), None).unwrap();
        let on = resolve(opt(Some(true)), None).unwrap();
        let off = resolve(opt(Some(false)), None).unwrap();
        assert_eq!(on.thread_start_params(std::path::Path::new("/p"))["serviceTier"], "priority");
        assert_eq!(off.thread_start_params(std::path::Path::new("/p"))["serviceTier"], "default");
        assert!(none.thread_start_params(std::path::Path::new("/p")).get("serviceTier").is_none());
        // 没指定：turn 里不传
        let (p, _) = none.turn_overrides(&none.initial_sent());
        assert!(p.get("serviceTier").is_none());
        // 线程没带 fast，现在开了：传 priority，之后不重复传
        let (p, sent) = on.turn_overrides(&none.initial_sent());
        assert_eq!(p["serviceTier"], "priority");
        let (p, sent2) = on.turn_overrides(&sent);
        assert!(p.get("serviceTier").is_none());
        // 开过 fast 之后把选项去掉：退回标准速度，且只传一次
        let (p, sent3) = none.turn_overrides(&sent2);
        assert_eq!(p["serviceTier"], "default");
        let (p, _) = none.turn_overrides(&sent3);
        assert!(p.get("serviceTier").is_none());
    }

    #[test]
    fn invalid_settings_are_rejected_with_the_field_named() {
        let field = |r: Result<LaunchOptions>| match r {
            Err(AdapterError::InvalidSettings { field, .. }) => field,
            other => panic!("expected invalid settings, got {other:?}"),
        };
        assert_eq!(field(resolve(SessionSettings { mode: Some("plan".into()), ..Default::default() }, None)), "mode");
        assert_eq!(
            field(resolve(SessionSettings { model: Some("-rm -rf".into()), ..Default::default() }, None)),
            "model"
        );
        assert_eq!(
            field(resolve(
                SessionSettings { model: Some("m-small".into()), thinking: Some("ultra".into()), ..Default::default() },
                None
            )),
            "thinking"
        );
        assert_eq!(field(resolve(SessionSettings { max_turns: Some(3), ..Default::default() }, None)), "max_turns");
        assert_eq!(
            field(resolve(SessionSettings { max_budget_usd: Some(1.0), ..Default::default() }, None)),
            "max_budget_usd"
        );
        let opts = |k: &str, v: Value| serde_json::Map::from_iter([(k.to_string(), v)]);
        assert_eq!(
            field(resolve(SessionSettings { options: opts(OPT_FAST_MODE, json!("yes")), ..Default::default() }, None)),
            "options.fast_mode"
        );
        assert_eq!(
            field(resolve(
                SessionSettings { options: opts(OPT_REASONING_SUMMARY, json!("verbose")), ..Default::default() },
                None
            )),
            "options.model_reasoning_summary"
        );
        assert_eq!(
            field(resolve(
                SessionSettings { system_prompt_append: Some("a\u{0}b".into()), ..Default::default() },
                None
            )),
            "system_prompt_append"
        );
        assert_eq!(
            field(resolve(
                SessionSettings { system_prompt_append: Some("x".repeat(40_000)), ..Default::default() },
                None
            )),
            "system_prompt_append"
        );
        // 不在列表里的模型（自定义 / 本机 config 里的）只做字符检查，思考等级随意（也只做字符检查）
        let o = resolve(
            SessionSettings {
                model: Some("my-private/model:v2".into()),
                thinking: Some("turbo".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!((o.model.as_deref(), o.thinking.as_deref()), (Some("my-private/model:v2"), Some("turbo")));
        // resume id 不能以 - 开头
        assert!(resolve(
            SessionSettings::default(),
            Some(ResumeHandle { agent_session_id: "-x".into(), launch_args: Value::Null })
        )
        .is_err());
    }

    #[test]
    fn launch_args_use_settings_field_names_plus_the_private_marker() {
        let o = resolve(
            SessionSettings {
                system_prompt_append: Some("Be terse.".into()),
                options: serde_json::Map::from_iter([(OPT_FAST_MODE.to_string(), json!(true))]),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let v = o.to_json(&Some("Be terse.".to_string()));
        assert_eq!(v["model"], "m-big");
        assert_eq!(v["mode"], "workspace");
        assert_eq!(v["fast_mode"], true);
        assert_eq!(v["system_prompt_append"], "Be terse.");
        assert_eq!(v["instructions_applied"], "Be terse.");
        assert_eq!(o.to_json(&None)["instructions_applied"], Value::Null);
    }

    #[test]
    fn instructions_notice_is_a_developer_message() {
        let n = instructions_notice(&Some("Answer in French.".into()));
        assert_eq!(n["type"], "message");
        assert_eq!(n["role"], "developer");
        assert!(n["content"][0]["text"].as_str().unwrap().contains("Answer in French."));
        assert_eq!(n["content"][0]["type"], "input_text");
        let cleared = instructions_notice(&None);
        assert!(cleared["content"][0]["text"].as_str().unwrap().contains("no longer apply"));
    }

    #[test]
    fn input_items_text_and_images() {
        let input = UserInput {
            message_id: "m".into(),
            interject: false,
            content: "look".into(),
            attachments: vec![AttachmentRef {
                blob_id: "b".into(),
                name: "a.png".into(),
                mime: "image/png".into(),
                bytes: 3,
                width: None,
                height: None,
            }],
            attachment_paths: vec!["/tmp/a.png".into()],
        };
        assert_eq!(
            input_items(&input).unwrap(),
            vec![json!({"type": "text", "text": "look"}), json!({"type": "localImage", "path": "/tmp/a.png"})]
        );
        // 只有图片：不发空文本
        let only_image = UserInput { content: String::new(), ..input.clone() };
        assert_eq!(input_items(&only_image).unwrap(), vec![json!({"type": "localImage", "path": "/tmp/a.png"})]);
        // 空消息仍然是一条空文本（Codex 至少要一项）
        let empty =
            UserInput { content: String::new(), attachments: vec![], attachment_paths: vec![], ..input.clone() };
        assert_eq!(input_items(&empty).unwrap(), vec![json!({"type": "text", "text": ""})]);
        // 非图片：路径清单在前，然后是文字和图片
        let pdf_ref = AttachmentRef {
            blob_id: "c".into(),
            name: "a.pdf".into(),
            mime: "application/pdf".into(),
            bytes: 3,
            width: None,
            height: None,
        };
        let mixed = UserInput {
            attachments: vec![input.attachments[0].clone(), pdf_ref.clone()],
            attachment_paths: vec!["/tmp/a.png".into(), "/tmp/c.pdf".into()],
            ..input.clone()
        };
        let items = input_items(&mixed).unwrap();
        assert_eq!(items.len(), 3);
        let listed = items[0]["text"].as_str().unwrap();
        assert!(listed.contains("- a.pdf (application/pdf, 3 bytes): /tmp/c.pdf"), "{listed}");
        assert!(!listed.contains("a.png"), "{listed}");
        assert_eq!(
            items[1..],
            [json!({"type": "text", "text": "look"}), json!({"type": "localImage", "path": "/tmp/a.png"})]
        );
        // 只有文件：只有路径清单
        let only_pdf = UserInput {
            content: String::new(),
            attachments: vec![pdf_ref],
            attachment_paths: vec!["/tmp/c.pdf".into()],
            ..input.clone()
        };
        let items = input_items(&only_pdf).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0]["text"].as_str().unwrap().starts_with("The user attached a file."));
        // 路径数量对不上
        let bad = UserInput { attachment_paths: vec![], ..input };
        assert!(matches!(input_items(&bad), Err(AdapterError::Protocol(_))));
    }

    #[test]
    fn app_server_args_are_just_the_subcommand() {
        assert_eq!(app_server_args(), vec!["app-server"]);
    }
}
