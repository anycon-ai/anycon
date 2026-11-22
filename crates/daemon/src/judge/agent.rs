//! `kind = "agent"` 的判断后端（docs/spec/daemon.md § 25.1）：一次性调用本机的 agent CLI，不开会话、没有工具。

use std::sync::Arc;

use super::{AgentBackendConfig, Backend, CallError, JudgeReply, JudgeRequest};
use crate::agents::Agents;

pub struct AgentBackend {
    #[allow(dead_code)]
    cfg: AgentBackendConfig,
    #[allow(dead_code)]
    agents: Arc<Agents>,
}

impl AgentBackend {
    pub fn new(cfg: AgentBackendConfig, agents: Arc<Agents>) -> Self {
        Self { cfg, agents }
    }
}

#[async_trait::async_trait]
impl Backend for AgentBackend {
    async fn call(&self, _req: &JudgeRequest) -> Result<JudgeReply, CallError> {
        Err(CallError::Internal)
    }
}
