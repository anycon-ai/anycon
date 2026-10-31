//! 确定性的 mock agent：E2E 与各层测试用。行为表见同目录 `README.md`。
//!
//! 不碰网络。唯一会写文件的地方是 `write` 关键词：只写 `cwd` 里的 `anycon-mock.txt`，不跟随符号链接
//! （给 daemon / e2e 测试在 worktree 里制造改动用）；唯一会读文件的地方是 daemon 交给它的附件文件
//! （回显里报文件大小和 SHA-256 前缀，证明 daemon 给的路径对）；其它关键词（`echo hi` 之类）只发事件，
//! 不执行任何东西。

mod session;

/// `big` 关键词产生的确定性大输出文本（测试 / e2e 用来对照）。
pub use session::big_output_text;
/// `write` 关键词写的文件名，以及每个 turn 写进去的那一行（测试 / e2e 用来对照）。
pub use session::{write_line, LINK_URL, WRITE_FILE_NAME};

use anycon_agent_api::{Adapter, AdapterConfig, AdapterError, AgentHandle, ProbeContext, Result, SpawnRequest};
use anycon_protocol::{
    AgentCapabilities, AgentInfo, AgentIssue, AgentStatus, Choice, ConfigOption, ConfigOptionKind, DiagnosticField,
    ModeInfo, ModeRisk, ModelInfo, ThinkingOptions,
};

use crate::common::now_ms;
use crate::diagnose::{fields_to_text, Diagnosable};

pub const ID: &str = "mock";
pub const MODEL_LARGE: &str = "mock-large";
pub const MODEL_SMALL: &str = "mock-small";
pub const MODE_ASK: &str = "ask";
pub const MODE_PLAN: &str = "plan";
pub const MODE_YOLO: &str = "yolo";
pub const ENABLE_HINT: &str = "set agents.mock.enabled = true or ANYCON_MOCK_AGENT=1";

#[derive(Debug, Default, Clone, Copy)]
pub struct MockAdapter;

impl MockAdapter {
    pub fn new() -> Self {
        Self
    }
}

fn choice(id: &str, label: &str) -> Choice {
    Choice { id: id.to_string(), label: label.to_string(), description: None }
}

/// 内置模型表（`spawn` 解析默认值也用它，不依赖 daemon 传来的 `agent_info`）。
pub(crate) fn builtin_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: MODEL_LARGE.into(),
            label: "Mock Large".into(),
            description: Some("Deterministic mock model with thinking".into()),
            default: true,
            thinking: Some(ThinkingOptions {
                options: vec![choice("low", "Low"), choice("high", "High")],
                default: Some("high".into()),
            }),
            tags: vec![],
        },
        ModelInfo {
            id: MODEL_SMALL.into(),
            label: "Mock Small".into(),
            description: Some("Deterministic mock model without thinking".into()),
            default: false,
            thinking: None,
            tags: vec![],
        },
    ]
}

/// 内置模型 + `config.additional_models`（按 id 去重，内置的在前）。`probe` 和会话解析设置时都用它，
/// 所以 `additional_models` 里的模型既能被探测到，也能真的被选用。
pub(crate) fn models_with(config: &AdapterConfig) -> Vec<ModelInfo> {
    let mut models = builtin_models();
    for m in &config.additional_models {
        if !models.iter().any(|x| x.id == m.id) {
            models.push(m.clone());
        }
    }
    models
}

fn builtin_modes() -> Vec<ModeInfo> {
    vec![
        ModeInfo {
            id: MODE_ASK.into(),
            label: "Ask".into(),
            description: Some("Ask before running commands".into()),
            default: true,
            risk: ModeRisk::Normal,
        },
        ModeInfo {
            id: MODE_PLAN.into(),
            label: "Plan".into(),
            description: Some("Plan only, never run anything".into()),
            default: false,
            risk: ModeRisk::Safe,
        },
        ModeInfo {
            id: MODE_YOLO.into(),
            label: "YOLO".into(),
            description: Some("Run everything without asking".into()),
            default: false,
            risk: ModeRisk::Dangerous,
        },
    ]
}

fn builtin_options() -> Vec<ConfigOption> {
    vec![ConfigOption {
        id: "verbose".into(),
        label: "Verbose".into(),
        description: Some("Add an extra line to every reply".into()),
        kind: ConfigOptionKind::Bool,
        choices: vec![],
        default: serde_json::Value::Bool(false),
    }]
}

#[async_trait::async_trait]
impl Adapter for MockAdapter {
    fn id(&self) -> &'static str {
        ID
    }

    fn label(&self) -> &'static str {
        "Mock Agent"
    }

    fn driver(&self) -> &'static str {
        "mock"
    }

    async fn probe(&self, ctx: &ProbeContext) -> AgentInfo {
        let mut info = AgentInfo {
            id: ID.into(),
            label: self.label().into(),
            driver: self.driver().into(),
            enabled: ctx.config.enabled,
            status: AgentStatus::Ready,
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            path: None,
            issue: None,
            models: vec![],
            allow_custom_model: false,
            modes: vec![],
            options: vec![],
            capabilities: AgentCapabilities::default(),
            probed_at_ms: now_ms(),
        };
        if !ctx.config.enabled {
            info.status = AgentStatus::Disabled;
            info.issue = Some(AgentIssue {
                code: "disabled".into(),
                message: "the mock agent is disabled".into(),
                hint: Some(ENABLE_HINT.into()),
            });
            return info;
        }
        info.models = models_with(&ctx.config);
        info.modes = builtin_modes();
        info.options = builtin_options();
        info.capabilities = AgentCapabilities {
            resume: true,
            interrupt: true,
            interject: false,
            attachments: crate::common::accepted_attachments(),
            thinking_stream: true,
            usage: true,
            structured_output: false,
            env: false,
            rewind: false,
        };
        info
    }

    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn AgentHandle>> {
        if !req.config.enabled {
            return Err(AdapterError::Spawn(format!("mock agent is disabled: {ENABLE_HINT}")));
        }
        self.validate_settings(&req.agent_info, &req.settings)?;
        Ok(Box::new(session::MockHandle::start(req)?))
    }
}

#[async_trait::async_trait]
impl Diagnosable for MockAdapter {
    async fn diagnose(&self, ctx: &ProbeContext) -> (String, Vec<DiagnosticField>) {
        let info = self.probe(ctx).await;
        let fields = vec![
            DiagnosticField { label: "adapter".into(), value: ID.into() },
            DiagnosticField { label: "enabled".into(), value: ctx.config.enabled.to_string() },
            DiagnosticField { label: "status".into(), value: format!("{:?}", info.status).to_lowercase() },
            DiagnosticField { label: "models".into(), value: info.models.len().to_string() },
        ];
        (fields_to_text("Mock Agent (mock)", &fields), fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    pub(crate) fn ctx(enabled: bool) -> ProbeContext {
        ProbeContext {
            env: Arc::new(HashMap::new()),
            config: AdapterConfig { enabled, ..Default::default() },
            deadline: Duration::from_secs(5),
            cheap: false,
            previous: None,
            scratch_dir: std::env::temp_dir(),
        }
    }

    #[tokio::test]
    async fn disabled_by_default_with_hint() {
        let info = MockAdapter::new().probe(&ctx(false)).await;
        assert_eq!(info.status, AgentStatus::Disabled);
        assert!(!info.enabled);
        assert_eq!(info.issue.unwrap().hint.as_deref(), Some(ENABLE_HINT));
        assert!(info.models.is_empty());
    }

    #[tokio::test]
    async fn enabled_probe_shape() {
        let info = MockAdapter::new().probe(&ctx(true)).await;
        assert_eq!(info.status, AgentStatus::Ready);
        assert_eq!(info.models.len(), 2);
        let large = &info.models[0];
        assert_eq!(large.id, "mock-large");
        let th = large.thinking.as_ref().unwrap();
        assert_eq!(th.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "high"]);
        assert_eq!(th.default.as_deref(), Some("high"));
        assert!(info.models[1].thinking.is_none());
        assert_eq!(info.modes.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["ask", "plan", "yolo"]);
        assert_eq!(info.modes[0].risk, ModeRisk::Normal);
        assert!(info.modes[0].default);
        assert_eq!(info.modes[1].risk, ModeRisk::Safe);
        assert_eq!(info.modes[2].risk, ModeRisk::Dangerous);
        assert_eq!(info.options[0].id, "verbose");
        assert_eq!(info.options[0].kind, ConfigOptionKind::Bool);
        let c = &info.capabilities;
        assert!(c.resume && c.interrupt && c.thinking_stream && c.usage);
        assert!(!c.interject);
        assert_eq!(c.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp", "*/*"]);
    }
}
