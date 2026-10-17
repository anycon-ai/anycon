//! agent 探测结果与能力声明。客户端不内置任何 agent 知识，全部来自这里（docs/spec/daemon.md § 5）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::TimestampMs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Ready,
    /// 首次探测还没跑完。
    Loading,
    Disabled,
    NotInstalled,
    NotAuthenticated,
    UnsupportedVersion,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentIssue {
    /// 稳定码，如 `binary_not_found`、`login_required`、`version_too_old`、`probe_timeout`。
    pub code: String,
    pub message: String,
    /// 修复建议，通常是一条可复制的命令。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Choice {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 某个模型支持的思考等级。**挂在模型上**，不同模型不同。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThinkingOptions {
    pub options: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelInfo {
    /// 原样传给 agent 的字符串。
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingOptions>,
    /// 如 `1m`。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModeRisk {
    /// 只读 / 只规划。
    Safe,
    Normal,
    /// 全部放行。需 `permissions.allow_dangerous_modes`，UI 要确认。
    Dangerous,
}

/// 权限模式。id 是 agent 自己的（Claude Code 的 `acceptEdits`、Codex 的 `workspace`…），不翻译不归一。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModeInfo {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub default: bool,
    pub risk: ModeRisk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionKind {
    Bool,
    Select,
    Text,
}

/// agent 特定的其他选项（Codex fast mode、ACP 自定义 configOptions…）。客户端用通用控件渲染。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ConfigOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: ConfigOptionKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub default: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentCapabilities {
    /// 进程退出后能否续接。
    pub resume: bool,
    pub interrupt: bool,
    /// turn 进行中能否插话。
    pub interject: bool,
    /// 可接受的附件 mime，如 `image/png`。
    #[serde(default)]
    pub attachments: Vec<String>,
    /// 会不会发 `thinking_delta`。
    pub thinking_stream: bool,
    /// `turn_done` 带不带 usage。
    pub usage: bool,
    pub structured_output: bool,
    /// 支持每会话 env。
    pub env: bool,
    pub rewind: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentInfo {
    /// adapter id：`claude-code`、`codex`、`mock`、ACP agent 用自己的名字。
    pub id: String,
    pub label: String,
    /// `claude-stream-json` | `codex-app-server` | `acp` | `sdk` | `mock`
    pub driver: String,
    pub enabled: bool,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<AgentIssue>,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub allow_custom_model: bool,
    #[serde(default)]
    pub modes: Vec<ModeInfo>,
    #[serde(default)]
    pub options: Vec<ConfigOption>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub probed_at_ms: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentListParams {
    /// 强制重探（含模型列表那一步）。
    #[serde(default)]
    pub refresh: bool,
    /// 等于当前 etag 时返回 `not_modified = true` 且 `agents` 为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_none_match: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentListResult {
    pub agents: Vec<AgentInfo>,
    pub etag: String,
    #[serde(default)]
    pub not_modified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentDiagnoseParams {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DiagnosticField {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentDiagnoseResult {
    pub id: String,
    /// 人类可读全文（`anycon doctor` 直接打印）。
    pub text: String,
    /// 结构化：命令来源、daemon PATH、shell、which -a、解析路径、版本、认证、模型数、状态。
    pub fields: Vec<DiagnosticField>,
}
