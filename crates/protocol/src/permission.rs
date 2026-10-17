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
