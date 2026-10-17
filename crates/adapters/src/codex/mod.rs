//! Codex adapter：驱动 `codex app-server`（stdio，换行分隔的 JSON-RPC，省略 `jsonrpc` 头）。**不用 `codex exec`**：
//! 它在无头模式下写死 `approval_policy = never`，无法把审批交给人（ADR-26）。细节与对真实 CLI 的观察见 README.md。
//!
//! v0.1 每个 anycon 会话一个 `codex app-server` 子进程、一个 thread（ADR-26 修订）。
//!
//! 模块：`probe`（找二进制、版本、认证、取模型、诊断）、`models`（`model/list` → 模型表）、`process`（会话设置 → 请求参数）、
//! `rpc`（JSON-RPC 行分类与请求 / 响应对应）、`wire`（线上格式的 serde 子集）、`translate`（通知 → 事件，纯同步）、
//! `permissions`（审批 / 提问 ↔ 权限请求）、`tools`（item → category / 标题 / 渲染字段）、`handle`（`AgentHandle`）。

pub mod handle;
pub mod models;
pub mod permissions;
pub mod probe;
pub mod process;
pub mod rpc;
pub mod tools;
pub mod translate;
pub mod wire;

use anycon_agent_api::{default_validate, Adapter, AdapterError, AgentHandle, ProbeContext, Result, SpawnRequest};
use anycon_protocol::{AgentInfo, DiagnosticField, SessionSettings};

use crate::diagnose::{fields_to_text, Diagnosable};

pub use handle::{CodexHandle, Timings};
pub use probe::{DRIVER, ID, LABEL};

#[derive(Debug, Clone, Default)]
pub struct CodexAdapter {
    timings: Timings,
    /// 上一次认证判定（cheap 探测沿用）。`Clone` 出来的实例共享同一份。
    auth_memo: std::sync::Arc<probe::AuthMemoCell>,
}

impl CodexAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 自定义等待时间（测试里把 3 秒 / 5 秒缩短）。
    pub fn with_timings(timings: Timings) -> Self {
        Self { timings, ..Self::default() }
    }
}

#[async_trait::async_trait]
impl Adapter for CodexAdapter {
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
        probe::run_probe(ctx, false, &self.auth_memo).await.info
    }

    /// 通用校验（模型 / 模式 / 选项存在）+ Codex 自己的规则（值的字符集和长度、选项取值、不支持 `max_turns` / `max_budget_usd`），
    /// 这样 daemon 在 `session.create` / `session.set_settings` 时就能以 `invalid_settings` 拒绝。
    fn validate_settings(&self, info: &AgentInfo, settings: &SessionSettings) -> Result<()> {
        default_validate(info, settings)?;
        process::validate_settings_values(settings)
    }

    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn AgentHandle>> {
        if !req.config.enabled {
            return Err(AdapterError::Spawn("codex is disabled in the config".into()));
        }
        self.validate_settings(&req.agent_info, &req.settings)?;
        // `req.settings` 是完整目标设置；resume 只从 `req.resume` 取 thread id 和 `instructions_applied`
        let opts = process::LaunchOptions::resolve(&req.settings, &req.config, &req.agent_info, req.resume.as_ref())?;
        let (program, prefix) = process::resolve_program(&req.config, req.agent_info.path.as_deref(), &req.env)?;
        Ok(Box::new(CodexHandle::start(&req, opts, program, prefix, self.timings).await?))
    }
}

#[async_trait::async_trait]
impl Diagnosable for CodexAdapter {
    async fn diagnose(&self, ctx: &ProbeContext) -> (String, Vec<DiagnosticField>) {
        let fields: Vec<DiagnosticField> = probe::diagnose_fields(ctx).await;
        (fields_to_text("Codex (codex)", &fields), fields)
    }
}
