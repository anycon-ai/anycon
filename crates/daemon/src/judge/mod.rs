//! 判断后端（docs/spec/daemon.md § 25，ADR-54 第 4 条）：Bot 要做的「判断」交给一个由用户在 `config.toml` 里配置的后端，
//! 提炼规则（§ 23.9）、分拣和顾问（§ 26）共用它。`kind = "api"` 是 daemon 直接调用模型 API（[`api`]），`kind = "agent"` 是一次性
//! 调用本机的 agent CLI（[`agent`]）。
//!
//! 这里只放各后端共用的类型和 [`Backend`] 接口。提示和回应都是用户内容：不进日志、不存库，错误里也不带。

pub mod agent;
pub mod api;
pub mod service;

use std::collections::HashMap;
use std::time::Duration;

/// 一次调用的输入：一段系统提示加一段用户消息。
#[derive(Clone)]
pub struct JudgeRequest {
    pub system: String,
    pub prompt: String,
}

/// 只输出长度：提示是用户内容。
impl std::fmt::Debug for JudgeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeRequest")
            .field("system_bytes", &self.system.len())
            .field("prompt_bytes", &self.prompt.len())
            .finish()
    }
}

/// 后端的回答：回应里的第一段文字，加上服务报告的用量（报不出的留空）。
#[derive(Clone, Default, PartialEq)]
pub struct JudgeReply {
    pub text: String,
    pub usage: Usage,
}

/// 只输出长度和用量：回答是用户内容。
impl std::fmt::Debug for JudgeReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JudgeReply").field("text_bytes", &self.text.len()).field("usage", &self.usage).finish()
    }
}

/// 服务报告的用量。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// 只有 agent 报了钱数时才有（Claude Code 的 `total_cost_usd`）。
    pub cost_usd: Option<f64>,
}

/// 一次调用失败的原因（§ 25.1 的错误映射）。不带回应的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("the call timed out")]
    Timeout,
    /// 连不上、TLS 失败、连接中途断了。
    #[error("network error")]
    Network,
    /// 401 / 403 / 429 以外的非 2xx。
    #[error("HTTP status {0}")]
    Http(u16),
    /// 401 / 403。
    #[error("authentication failed")]
    Auth,
    /// 429。
    #[error("rate limited")]
    RateLimited,
    /// 回应解析不了，或者里面没有文字。
    #[error("the reply could not be used")]
    BadOutput,
    /// agent 退出码非 0，或者报了错。
    #[error("the agent failed")]
    AgentFailed,
    /// daemon 停下时被取消。
    #[error("cancelled")]
    Cancelled,
    #[error("internal error")]
    Internal,
}

/// 一个判断后端。实现必须遵守：调用整体受配置的 `timeout` 约束；不把提示、回应或密钥写进日志和错误。
#[async_trait::async_trait]
pub trait Backend: Send + Sync {
    async fn call(&self, req: &JudgeRequest) -> Result<JudgeReply, CallError>;
}

/// `kind = "api"` 的回应格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    /// Anthropic Messages API：`POST {base_url}/v1/messages`。
    Anthropic,
    /// OpenAI Chat Completions（含兼容它的网关）：`POST {base_url}/chat/completions`。
    Openai,
}

impl ApiFormat {
    /// 没写 `base_url` 时用的地址。
    pub fn default_base_url(self) -> &'static str {
        match self {
            ApiFormat::Anthropic => "https://api.anthropic.com",
            ApiFormat::Openai => "https://api.openai.com/v1",
        }
    }
}

/// 密钥从哪里来（§ 25.1）。
#[derive(Clone, PartialEq, Eq)]
pub enum ApiKeySource {
    /// `api_key_env`：环境变量名。
    Env(String),
    /// `api_key`：直接写在 `config.toml` 里。
    Literal(String),
    /// 都没写（只有回环地址上的 `http://` 后端可以这样）。
    None,
}

/// 不输出密钥本身。
impl std::fmt::Debug for ApiKeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKeySource::Env(name) => f.debug_tuple("Env").field(name).finish(),
            ApiKeySource::Literal(_) => f.write_str("Literal(<redacted>)"),
            ApiKeySource::None => f.write_str("None"),
        }
    }
}

/// 取出密钥：`Env` 从 `env`（daemon 给 agent 用的那份环境，§ 5.5）里读，空串算没有。
pub fn resolve_api_key(src: &ApiKeySource, env: &HashMap<String, String>) -> Option<String> {
    match src {
        ApiKeySource::Env(name) => env.get(name).filter(|v| !v.is_empty()).cloned(),
        ApiKeySource::Literal(v) => Some(v.clone()).filter(|v| !v.is_empty()),
        ApiKeySource::None => None,
    }
}

/// 一个 `kind = "api"` 后端的配置，已经校验过、补上了缺省值（§ 25.1）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiBackendConfig {
    pub format: ApiFormat,
    /// 不带末尾的 `/`。`https://`，或者回环地址上的 `http://`。
    pub base_url: String,
    pub model: String,
    pub api_key: ApiKeySource,
    /// 1–16 384，缺省 2 048。
    pub max_output_tokens: u32,
    /// 整次请求的时限，10s–10m，缺省 2m。
    pub timeout: Duration,
}

/// 一个 `kind = "agent"` 后端的配置，已经校验过（§ 25.1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBackendConfig {
    /// agent id（`agents.<id>`）。
    pub agent: String,
    /// 缺省用 agent 的 `default_model`。
    pub model: Option<String>,
    /// 10s–15m，缺省 5m。
    pub timeout: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_from_the_environment_or_the_file_and_empty_counts_as_missing() {
        let env = HashMap::from([("K".to_string(), "secret".to_string()), ("EMPTY".to_string(), String::new())]);
        assert_eq!(resolve_api_key(&ApiKeySource::Env("K".into()), &env).as_deref(), Some("secret"));
        assert_eq!(resolve_api_key(&ApiKeySource::Env("EMPTY".into()), &env), None);
        assert_eq!(resolve_api_key(&ApiKeySource::Env("MISSING".into()), &env), None);
        assert_eq!(resolve_api_key(&ApiKeySource::Literal("x".into()), &env).as_deref(), Some("x"));
        assert_eq!(resolve_api_key(&ApiKeySource::None, &env), None);
    }

    #[test]
    fn debug_output_never_contains_the_key_or_the_prompt() {
        assert_eq!(format!("{:?}", ApiKeySource::Literal("sk-secret".into())), "Literal(<redacted>)");
        let req = JudgeRequest { system: "sys".into(), prompt: "private words".into() };
        assert!(!format!("{req:?}").contains("private"));
        let reply = JudgeReply { text: "private words".into(), usage: Usage::default() };
        assert!(!format!("{reply:?}").contains("private"));
    }
}
