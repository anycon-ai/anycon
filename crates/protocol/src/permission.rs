//! 权限请求。`tool` / `plan` / `question` / `mode_change` 都走这一条通道（docs/spec/daemon.md § 9）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{DeviceId, SessionId, TimestampMs, ToolCallDetail, ToolCategory};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Tool,
    Plan,
    Question,
    ModeChange,
    Other,
}

/// 借用 ACP 的枚举，客户端据此选图标和颜色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionOption {
    /// adapter 定义；Claude Code 固定 `allow` / `allow_session` / `deny`，ACP 透传 optionId。
    pub id: String,
    pub label: String,
    pub kind: PermissionOptionKind,
    /// 可附一句话给 agent（通常 reject 类）。
    #[serde(default)]
    pub needs_message: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct QuestionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Question {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<QuestionChoice>,
    #[serde(default)]
    pub multi: bool,
    #[serde(default)]
    pub free_text: bool,
    /// 可以不答：客户端允许留空提交，答复里就没有这一题的答案。默认 `false`（必答），
    /// 所以 AskUserQuestion 这类每题都要答的行为不变；MCP elicitation 表单里 schema 没列进 `required` 的字段是 `true`。
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// `kind = tool` 的详情：和 `tool_call` 同一套字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolPermissionDetail {
    pub tool: String,
    pub category: ToolCategory,
    #[serde(default)]
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ToolCallDetail>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionDetail {
    /// kind = tool
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolPermissionDetail>,
    /// kind = plan：计划全文（Markdown）。kind = other：说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// kind = other：要用户在浏览器里打开的链接（MCP server 的 URL 模式 elicitation）。只会是 `http` / `https`；
    /// 客户端只对这两种协议提供「打开」入口，并且显示完整 URL。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// kind = question
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<Question>,
    /// kind = mode_change：目标模式 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// `permission.request` 通知 / `permission.list` 的元素。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionRequest {
    pub session_id: SessionId,
    pub request_id: String,
    pub kind: PermissionKind,
    pub title: String,
    pub detail: PermissionDetail,
    pub options: Vec<PermissionOption>,
    pub created_at_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionRespondParams {
    pub request_id: String,
    /// kind = question 时可空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// kind = question：`{ [question_id]: value }`，value 是字符串或字符串数组。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionResolved {
    pub session_id: SessionId,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
    pub by_device: DeviceId,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PermissionListResult {
    pub requests: Vec<PermissionRequest>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_optional_defaults_to_required_and_stays_off_the_wire_when_false() {
        let q: Question = serde_json::from_value(serde_json::json!({"id": "q", "text": "t"})).unwrap();
        assert!(!q.optional);
        assert!(serde_json::to_value(&q).unwrap().get("optional").is_none());

        let q = Question {
            id: "q".into(),
            text: "t".into(),
            choices: vec![],
            multi: false,
            free_text: true,
            optional: true,
        };
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["optional"], true);
        assert_eq!(serde_json::from_value::<Question>(v).unwrap(), q);
    }

    #[test]
    fn permission_detail_url_is_optional() {
        // 旧数据没有 url 照常解析；没有值时不上线路。
        let d: PermissionDetail = serde_json::from_value(serde_json::json!({"text": "t"})).unwrap();
        assert_eq!(d.url, None);
        assert!(serde_json::to_value(&d).unwrap().get("url").is_none());

        let d = PermissionDetail {
            text: Some("Sign in".into()),
            url: Some("https://example.com/auth".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["url"], "https://example.com/auth");
        let back: PermissionDetail = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }
}
