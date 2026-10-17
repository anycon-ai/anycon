//! Claude Code adapter：`claude -p --output-format stream-json --input-format stream-json` 双向模式，
//! 权限走 `control_request { subtype: can_use_tool }`。细节与对真实 CLI 的观察见 README.md。
//!
//! 模块：`probe`（找二进制、版本、认证、诊断）、`models`（内置模型表）、`process`（启动参数）、
//! `translate`（stream-json → 事件，纯同步）、`permissions`（权限请求 ↔ 答复）、`tools`（工具分类与渲染字段）、
//! `input`（用户输入 → stream-json 用户消息，含图片附件）、`handle`（`AgentHandle`）。

pub mod handle;
pub mod input;
pub mod models;
pub mod permissions;
pub mod probe;
pub mod process;
pub mod tools;
pub mod translate;

use anycon_agent_api::{default_validate, Adapter, AdapterError, AgentHandle, ProbeContext, Result, SpawnRequest};
use anycon_protocol::{AgentInfo, DiagnosticField, SessionSettings};

use crate::diagnose::{fields_to_text, Diagnosable};

pub use handle::{ClaudeHandle, Timings};
pub use probe::{DRIVER, ID, LABEL};

#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeAdapter {
    timings: Timings,
}

impl ClaudeCodeAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 自定义等待时间（测试里把 3 秒 / 5 秒缩短）。
    pub fn with_timings(timings: Timings) -> Self {
        Self { timings }
    }
}

#[async_trait::async_trait]
impl Adapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        LABEL
    }

    fn driver(&self) -> &'static str {
        DRIVER
    }

    async fn probe(&self, ctx: &ProbeContext) -> AgentInfo {
        probe::run_probe(ctx, false).await.info
    }

    /// 通用校验（模型 / 模式 / 选项存在）+ 会进 argv 的值的检查（字符集、长度、预算有限性），
    /// 这样 daemon 在 `session.create` / `session.set_settings` 时就能以 `invalid_settings` 拒绝。
    fn validate_settings(&self, info: &AgentInfo, settings: &SessionSettings) -> Result<()> {
        default_validate(info, settings)?;
        process::validate_settings_values(settings)
    }

    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn AgentHandle>> {
        if !req.config.enabled {
            return Err(AdapterError::Spawn("claude-code is disabled in the config".into()));
        }
        self.validate_settings(&req.agent_info, &req.settings)?;
        // `req.settings` 是完整目标设置；resume 只从 `req.resume` 取 agent 会话 id，`launch_args` 不参与
        let opts = process::LaunchOptions::resolve(&req.settings, &req.config, &req.agent_info, req.resume.as_ref())?;
        let (program, prefix) = process::resolve_program(&req.config, req.agent_info.path.as_deref(), &req.env)?;
        Ok(Box::new(ClaudeHandle::start(&req, opts, program, prefix, self.timings)?))
    }
}

#[async_trait::async_trait]
impl Diagnosable for ClaudeCodeAdapter {
    async fn diagnose(&self, ctx: &ProbeContext) -> (String, Vec<DiagnosticField>) {
        let fields: Vec<DiagnosticField> = probe::diagnose_fields(ctx).await;
        (fields_to_text("Claude Code (claude-code)", &fields), fields)
    }
}
