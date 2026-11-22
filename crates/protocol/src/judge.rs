//! 判断后端的公开状态和错误：只含元数据，不含地址、密钥、提示或回应。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeBackendKind {
    Api,
    Agent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeApiFormat {
    Anthropic,
    Openai,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeProblem {
    NoKey,
    AgentMissing,
    AgentUnsupported,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JudgeErrorCode {
    Timeout,
    Network,
    Http,
    Auth,
    RateLimited,
    BadOutput,
    AgentFailed,
    Cancelled,
    Internal,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JudgeInfo {
    pub backends: Vec<JudgeBackendInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisor: Option<String>,
    pub daily_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JudgeBackendInfo {
    pub name: String,
    pub kind: JudgeBackendKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<JudgeApiFormat>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<JudgeProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JudgeError {
    pub code: JudgeErrorCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct JudgeUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_enum_values_are_accepted() {
        let value = serde_json::json!("future");
        assert_eq!(serde_json::from_value::<JudgeBackendKind>(value.clone()).unwrap(), JudgeBackendKind::Unknown);
        assert_eq!(serde_json::from_value::<JudgeApiFormat>(value.clone()).unwrap(), JudgeApiFormat::Unknown);
        assert_eq!(serde_json::from_value::<JudgeProblem>(value.clone()).unwrap(), JudgeProblem::Unknown);
        assert_eq!(serde_json::from_value::<JudgeErrorCode>(value.clone()).unwrap(), JudgeErrorCode::Unknown);
        assert_eq!(
            serde_json::from_value::<crate::ExtractTrigger>(value.clone()).unwrap(),
            crate::ExtractTrigger::Unknown
        );
        assert_eq!(serde_json::from_value::<crate::ExtractRunStatus>(value).unwrap(), crate::ExtractRunStatus::Unknown);
    }

    #[test]
    fn old_nodes_and_optional_usage_remain_compatible() {
        let node: crate::NodeInfo = serde_json::from_value(serde_json::json!({
            "node_id":"n", "name":"n", "version":"1", "protocol_version":1, "platform":"test",
            "started_at_ms":1, "capabilities":{"workspace":true,"fs":true,"search":true,"blobs":true,"audit":true}
        }))
        .unwrap();
        assert!(node.judge.is_none());
        assert!(!node.capabilities.rule_extract);
        assert_eq!(serde_json::to_value(JudgeUsage::default()).unwrap(), serde_json::json!({}));
        let result: crate::RuleExtractResult = serde_json::from_value(serde_json::json!({"points":0})).unwrap();
        assert!(result.run.is_none());
        let params: crate::RuleExtractRunsParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(params.session_id.is_none() && params.limit.is_none());
    }
}
