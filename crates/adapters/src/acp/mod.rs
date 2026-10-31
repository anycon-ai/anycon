//! ACP client adapter：驱动任何说 [Agent Client Protocol](https://agentclientprotocol.com) v1 的 agent（stdio，换行分隔的
//! JSON-RPC 2.0）——OpenCode、Gemini CLI、Goose、Kimi Code、Qwen Code、Cursor、GitHub Copilot、Pi 等。协议版本、驱动方式、
//! 映射表和已知限制见 README.md。
//!
//! **每个 ACP agent 是一个独立的 adapter 实例**（id 就是它的名字，`config.agents.acp.<id>`）：内置注册表（`registry`）一张，
//! 用户可以覆盖命令、也可以加自己的。**一个 agent 一个进程，服务它的所有会话**（`conn::Pool`）。
//!
//! 模块：`registry`（内置注册表、名字规则）、`rpc`（JSON-RPC 行分类与请求 / 响应对应）、`wire`（`initialize` 响应的宽松解析）、
//! `options`（`configOptions` / `modes` ↔ `AgentInfo`）、`translate`（`session/update` → 事件，纯同步）、`tools`（工具调用归一化）、
//! `diff`（`diff` 内容块 → unified diff）、`permissions`（权限请求 / elicitation ↔ 权限请求）、`conn`（进程、进程池、会话、读任务）、
//! `probe`（探测与诊断）、`handle`（`AgentHandle`）。

pub mod conn;
pub mod diff;
pub mod handle;
pub mod options;
pub mod permissions;
pub mod probe;
pub mod registry;
pub mod rpc;
pub mod tools;
pub mod translate;
pub mod wire;

use std::sync::Arc;

use anycon_agent_api::{
    default_validate, Adapter, AdapterConfig, AdapterError, AgentHandle, ProbeContext, Result, SpawnRequest,
};
use anycon_protocol::{AgentInfo, DiagnosticField, SessionSettings};

use crate::common::bin::resolve_program;
use crate::diagnose::fields_to_text;

pub use conn::Timings;
pub use probe::DRIVER;
pub use registry::{registry_entry, registry_ids, validate_name, RegistryEntry, REGISTRY, RESERVED_NAMES};

/// 一个 ACP agent。
pub struct AcpAdapter {
    id: String,
    label: String,
    pool: Arc<conn::Pool>,
    timings: Timings,
    memo: probe::ProbeMemo,
}

impl AcpAdapter {
    /// `id`：agent 的名字（注册表里的，或用户在 `[agents.acp.<id>]` 里起的）。
    pub fn new(id: &str) -> Self {
        Self::with_timings(id, Timings::default())
    }

    /// 自定义等待时间（测试里把各种上限缩短）。
    pub fn with_timings(id: &str, timings: Timings) -> Self {
        let label = registry_entry(id).map(|e| e.label.to_string()).unwrap_or_else(|| id.to_string());
        Self {
            id: id.to_string(),
            label,
            pool: Arc::new(conn::Pool::new()),
            timings,
            memo: probe::ProbeMemo::default(),
        }
    }
}

#[async_trait::async_trait]
impl Adapter for AcpAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn label(&self) -> &str {
        &self.label
    }

    fn driver(&self) -> &'static str {
        DRIVER
    }

    async fn probe(&self, ctx: &ProbeContext) -> AgentInfo {
        probe::run_probe(ctx, &self.id, &self.label, false, &self.memo).await.info
    }

    /// 通用校验（模型 / 模式 / 选项存在）+ ACP 不支持的字段（`env`、`system_prompt_append`、`max_turns`、`max_budget_usd`），
    /// 这样 daemon 在 `session.create` / `session.set_settings` 时就能以 `invalid_settings` 拒绝。
    fn validate_settings(&self, info: &AgentInfo, settings: &SessionSettings) -> Result<()> {
        default_validate(info, settings)?;
        handle::validate_settings_values(settings)
    }

    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn AgentHandle>> {
        if !req.config.enabled {
            return Err(AdapterError::Spawn(format!("{} is disabled in the config", self.id)));
        }
        self.validate_settings(&req.agent_info, &req.settings)?;
        let command = registry::effective_command(&self.id, &req.config).ok_or_else(|| {
            AdapterError::Spawn(format!(
                "no command is configured for the ACP agent {}: set agents.acp.{}.command",
                self.id, self.id
            ))
        })?;
        // 命令（含固定参数）当作配置里的 `command` 解析；探测时选中的路径优先，没有就现找
        let with_command = AdapterConfig { command: Some(command), ..req.config.clone() };
        let hint = registry_entry(&self.id)
            .map(|e| e.install_hint.to_string())
            .unwrap_or_else(|| format!("fix agents.acp.{}.command", self.id));
        let (program, args) =
            resolve_program(&with_command, req.agent_info.path.as_deref(), &req.env, &self.id, &hint)?;
        let launch = conn::LaunchSpec { program, args, env: req.env.clone() };
        if !req.cwd.is_dir() {
            return Err(AdapterError::Spawn(format!("working directory does not exist: {}", req.cwd.display())));
        }

        // 错误消息里用探测到的名字（自己加的 agent 的名字来自它的 `agentInfo`）
        let label = if req.agent_info.label.is_empty() { self.label.clone() } else { req.agent_info.label.clone() };
        let connection = self.pool.acquire(&launch, &label, &self.timings).await?;
        let (session, opened) = match connection.open_session(&req, &self.timings).await {
            Ok(x) => x,
            Err(e) => {
                self.pool.release(connection, &self.timings).await;
                return Err(e);
            }
        };
        let handle = handle::AcpHandle::new(
            self.pool.clone(),
            connection.clone(),
            session.clone(),
            req.settings.clone(),
            self.timings,
        );
        // 新会话 / 续接的会话都先把当前设置应用上（和 agent 现在的值一样的不发请求）；应用不上的不让会话失败，
        // 作为可恢复的 `error` 告诉用户，会话照常用 agent 当前的值
        handle.announce_started();
        let problems =
            handle::apply_settings(&connection.rpc, &session, &req.settings, None, self.timings.request).await;
        for p in problems {
            session.emit_event(anycon_protocol::Event::Error {
                code: "agent_setting_failed".to_string(),
                message: format!("{} could not be applied ({}); the agent keeps its own value", p.field, p.reason),
                recoverable: true,
            });
        }
        if let conn::Opened::ContextReset(why) = opened {
            // 续接不上原来的会话，开了新的：可恢复的提示，不让 turn 失败
            session.emit_event(anycon_protocol::Event::Error {
                code: "agent_context_reset".to_string(),
                message: format!(
                    "{} could not restore this conversation ({why}), so it started a new session: the agent can no longer see what was said before.",
                    self.label
                ),
                recoverable: true,
            });
        }
        Ok(Box::new(handle))
    }

    async fn diagnose(&self, ctx: &ProbeContext) -> Option<(String, Vec<DiagnosticField>)> {
        let fields = probe::diagnose_fields(ctx, &self.id, &self.label, &self.memo).await;
        Some((fields_to_text(&format!("{} ({})", self.label, self.id), &fields), fields))
    }
}

/// 一个 ACP agent 的 adapter。
pub fn adapter_for(id: &str) -> Arc<dyn Adapter> {
    Arc::new(AcpAdapter::new(id))
}

/// 注册表里每个 agent 加上 `extra`（用户在 `[agents.acp.<name>]` 里加的）各一个 adapter，按名字去重；
/// 不合规的名字（见 [`validate_name`]）跳过并警告。
pub fn adapters<I, S>(extra: I) -> Vec<Arc<dyn Adapter>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let names = registry_ids().map(str::to_string).chain(extra.into_iter().map(|s| s.as_ref().to_string()));
    for name in names {
        if let Err(why) = validate_name(&name) {
            tracing::warn!(%name, %why, "skipping an ACP agent");
            continue;
        }
        if seen.insert(name.clone()) {
            out.push(adapter_for(&name));
        }
    }
    out
}
