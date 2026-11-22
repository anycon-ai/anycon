//! `kind = "api"` 的判断后端（docs/spec/daemon.md § 25.1）：daemon 直接调用模型 API（Anthropic Messages 或 OpenAI Chat Completions）。

use std::sync::Arc;

use super::{ApiBackendConfig, Backend, CallError, JudgeReply, JudgeRequest};
use crate::env::DaemonEnv;

pub struct ApiBackend {
    #[allow(dead_code)]
    cfg: ApiBackendConfig,
    #[allow(dead_code)]
    env: Arc<DaemonEnv>,
}

impl ApiBackend {
    /// `env` 是 daemon 给 agent 用的那份环境（§ 5.5），`api_key_env` 每次调用时从它里面读。
    pub fn new(cfg: ApiBackendConfig, env: Arc<DaemonEnv>) -> Self {
        Self { cfg, env }
    }
}

#[async_trait::async_trait]
impl Backend for ApiBackend {
    async fn call(&self, _req: &JudgeRequest) -> Result<JudgeReply, CallError> {
        Err(CallError::Internal)
    }
}
