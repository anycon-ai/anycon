//! `kind = "agent"` 的判断后端（docs/spec/daemon.md § 25.1）：一次性调用本机的 agent CLI，不开会话；Codex 仍能使用只读工具。

use std::sync::Arc;

use super::{AgentBackendConfig, Backend, CallError, JudgeReply, JudgeRequest};
use crate::agents::Agents;

pub struct AgentBackend {
    cfg: AgentBackendConfig,
    agents: Arc<Agents>,
}

impl AgentBackend {
    pub fn new(cfg: AgentBackendConfig, agents: Arc<Agents>) -> Self {
        Self { cfg, agents }
    }
}

#[async_trait::async_trait]
impl Backend for AgentBackend {
    async fn call(&self, req: &JudgeRequest) -> Result<JudgeReply, CallError> {
        use anycon_agent_api::{OneshotError, OneshotRequest};
        use anycon_protocol::AgentStatus;

        let adapter = self.agents.adapter(&self.cfg.agent).ok_or(CallError::AgentFailed)?;
        let info = self.agents.info(&self.cfg.agent).ok_or(CallError::AgentFailed)?;
        let (config, env) = self.agents.oneshot_context(&self.cfg.agent).ok_or(CallError::AgentFailed)?;
        if !config.enabled || !info.enabled || info.status != AgentStatus::Ready || !info.capabilities.oneshot {
            return Err(CallError::AgentFailed);
        }
        let model = self.cfg.model.clone().or_else(|| config.default_model.clone());
        // RAII 在成功、失败、超时以及调用被取消时都会尝试删除目录。
        let dir = tempfile::Builder::new().prefix("anycon-judge-").tempdir().map_err(|_| CallError::Internal)?;
        let request = OneshotRequest {
            system: req.system.clone(),
            prompt: req.prompt.clone(),
            model,
            timeout: self.cfg.timeout,
            cwd: dir.path().to_path_buf(),
            env,
            config,
            agent_info: info,
        };
        // adapter 自身执行总超时；外层也约束不遵守约定的实现。丢弃调用时 adapter 必须清理进程树。
        let result = tokio::time::timeout(self.cfg.timeout, adapter.oneshot(request)).await;
        let result = match result {
            Err(_) | Ok(Err(OneshotError::Timeout)) => Err(CallError::Timeout),
            Ok(Err(OneshotError::Unsupported | OneshotError::Spawn | OneshotError::Failed)) => {
                Err(CallError::AgentFailed)
            }
            Ok(Ok(reply)) => Ok(JudgeReply {
                text: reply.text,
                usage: super::Usage {
                    input_tokens: reply.input_tokens,
                    output_tokens: reply.output_tokens,
                    cost_usd: reply.cost_usd,
                },
            }),
        };
        if let Err(error) = dir.close() {
            tracing::warn!(error_kind = ?error.kind(), "failed to remove judge temporary directory");
        }
        result
    }
}
