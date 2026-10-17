//! `codex app-server` 线上格式的子集（只写用到的字段）。
//!
//! 对照 `codex app-server generate-json-schema` 的输出（0.157.1）：字段名是 camelCase（`config/read` 的 `config`
//! 是 snake_case，因为那是 `config.toml` 的键）。**未知字段一律忽略**，未知的 `type` / 枚举值落到 `Unknown` /
//! `Other` 变体，由上层 `debug!` 后丢弃——app-server 官方标注 experimental，字段会变，不能因为多一个字段就解析失败。
//! 本文件不含任何行为，只有反序列化类型和几个小的取值函数。

use serde::Deserialize;
use serde_json::Value;

// ---------- 通用 ----------

/// turn 的状态。`failed` 时 [`Turn::error`] 有内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    #[default]
    InProgress,
    #[serde(other)]
    Unknown,
}

/// `commandExecution` / `fileChange` / `mcpToolCall` / `dynamicToolCall` 等 item 的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    #[default]
    InProgress,
    Completed,
    Failed,
    Declined,
    Interrupted,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    #[serde(default)]
    pub message: String,
    /// 字符串（`unauthorized`、`usageLimitExceeded`…）或单键对象（`{ "httpConnectionFailed": { … } }`）。
    #[serde(default)]
    pub codex_error_info: Option<Value>,
    #[serde(default)]
    pub additional_details: Option<String>,
}

impl TurnError {
    /// `codexErrorInfo` 的名字（字符串本身，或对象的唯一键）。
    pub fn info_name(&self) -> Option<String> {
        match self.codex_error_info.as_ref()? {
            Value::String(s) => Some(s.clone()),
            Value::Object(m) => m.keys().next().cloned(),
            _ => None,
        }
    }

    /// 上游 HTTP 状态码（对象形的 `codexErrorInfo` 里 `httpStatusCode`）。
    pub fn http_status(&self) -> Option<u64> {
        let Value::Object(m) = self.codex_error_info.as_ref()? else { return None };
        m.values().find_map(|v| v.get("httpStatusCode").and_then(Value::as_u64))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    #[serde(default)]
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

// ---------- thread item ----------

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PatchChangeKind {
    Add,
    Delete,
    Update {
        #[serde(default)]
        move_path: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FileUpdateChange {
    pub path: String,
    pub kind: PatchChangeKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    UserMessage {},
    #[serde(rename_all = "camelCase")]
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    Plan {
        id: String,
        #[serde(default)]
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    CommandExecution {
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        status: ItemStatus,
        #[serde(default)]
        command_actions: Vec<Value>,
        #[serde(default)]
        aggregated_output: Option<String>,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        source: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    FileChange {
        id: String,
        #[serde(default)]
        changes: Vec<FileUpdateChange>,
        #[serde(default)]
        status: ItemStatus,
    },
    #[serde(rename_all = "camelCase")]
    McpToolCall {
        id: String,
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        status: ItemStatus,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    DynamicToolCall {
        id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        namespace: Option<String>,
        #[serde(default)]
        status: ItemStatus,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        content_items: Option<Vec<Value>>,
        #[serde(default)]
        success: Option<bool>,
    },
    #[serde(rename_all = "camelCase")]
    CollabAgentToolCall {
        id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        status: ItemStatus,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        receiver_thread_ids: Vec<String>,
        #[serde(default)]
        agents_states: Value,
    },
    #[serde(rename_all = "camelCase")]
    WebSearch {
        id: String,
        #[serde(default)]
        query: String,
        #[serde(default)]
        action: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    ImageView {
        id: String,
        #[serde(default)]
        path: String,
    },
    #[serde(rename_all = "camelCase")]
    ImageGeneration {
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        saved_path: Option<String>,
        #[serde(default)]
        revised_prompt: Option<String>,
    },
    #[serde(other)]
    Other,
}

impl ThreadItem {
    /// 无法识别的 item 在日志里显示的类型名。
    pub fn raw_type(raw: &Value) -> &str {
        raw.get("type").and_then(Value::as_str).unwrap_or("?")
    }
}

// ---------- 通知的 params ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    pub turn: Turn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub item: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaNotification {
    pub item_id: String,
    #[serde(default)]
    pub delta: String,
    #[serde(default)]
    pub summary_index: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryPartAdded {
    pub item_id: String,
    #[serde(default)]
    pub summary_index: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenBreakdown {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageNotification {
    #[serde(default)]
    pub turn_id: Option<String>,
    pub token_usage: TokenUsage,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default)]
    pub last: TokenBreakdown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub error: TurnError,
    #[serde(default)]
    pub will_retry: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanUpdated {
    #[serde(default)]
    pub plan: Vec<PlanStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolved {
    pub request_id: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRerouted {
    #[serde(default)]
    pub to_model: Option<String>,
}

// ---------- 服务端请求的 params ----------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApproval {
    pub item_id: String,
    /// zsh-exec-bridge 的子命令审批：同一个 `itemId` 会有多次回调，每次的 `command` 是那条子命令。
    #[serde(default)]
    pub approval_id: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub command_actions: Option<Vec<Value>>,
    #[serde(default)]
    pub network_approval_context: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileApproval {
    pub item_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub grant_root: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsApproval {
    pub item_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub permissions: Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub options: Option<Vec<UserInputOption>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputRequest {
    #[serde(default)]
    pub questions: Vec<UserInputQuestion>,
}

// ---------- 请求的响应 ----------

#[derive(Debug, Clone, Deserialize)]
pub struct IdRef {
    pub id: String,
}

/// `thread/start` / `thread/resume` 的响应。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResponse {
    pub thread: IdRef,
    #[serde(default)]
    pub model: Option<String>,
}

/// `turn/start` 的响应。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartResponse {
    pub turn: IdRef,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffortOption {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<EffortOption>,
}

/// `model/list` 的一页。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListPage {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// `config/read` 里我们关心的几个键（`config.toml` 的键，snake_case）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ConfigSummary {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
}

/// `account/read` 的响应。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountReadResponse {
    #[serde(default)]
    pub account: Option<AccountRef>,
    /// 缺省按需要登录算（最保守的是「别放行」之外的那一边：没有这个字段就当它需要）。
    #[serde(default = "default_true")]
    pub requires_openai_auth: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct AccountRef {
    /// `apiKey` / `chatgpt` / `amazonBedrock`
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigReadResponse {
    #[serde(default)]
    pub config: ConfigSummary,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_item_types_and_fields_are_tolerated() {
        let v = json!({"type": "brandNewThing", "id": "x", "extra": 1});
        assert!(matches!(serde_json::from_value::<ThreadItem>(v.clone()).unwrap(), ThreadItem::Other));
        assert_eq!(ThreadItem::raw_type(&v), "brandNewThing");
        let m = json!({"type": "agentMessage", "id": "m1", "text": "hi", "phase": null, "newField": [1, 2]});
        assert!(matches!(serde_json::from_value::<ThreadItem>(m).unwrap(), ThreadItem::AgentMessage { .. }));
    }

    #[test]
    fn unknown_status_values_do_not_fail() {
        let t: Turn = serde_json::from_value(json!({"id": "t", "status": "someFutureStatus", "items": []})).unwrap();
        assert_eq!(t.status, TurnStatus::Unknown);
        let c: ThreadItem = serde_json::from_value(
            json!({"type": "commandExecution", "id": "c", "command": "ls", "status": "weird", "commandActions": []}),
        )
        .unwrap();
        assert!(matches!(c, ThreadItem::CommandExecution { status: ItemStatus::Unknown, .. }));
    }

    #[test]
    fn error_info_shapes() {
        let e: TurnError = serde_json::from_value(json!({"message": "m", "codexErrorInfo": "unauthorized"})).unwrap();
        assert_eq!(e.info_name().as_deref(), Some("unauthorized"));
        assert_eq!(e.http_status(), None);
        let e: TurnError = serde_json::from_value(
            json!({"message": "m", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 502}}}),
        )
        .unwrap();
        assert_eq!(e.info_name().as_deref(), Some("httpConnectionFailed"));
        assert_eq!(e.http_status(), Some(502));
    }

    #[test]
    fn patch_change_kinds() {
        let add: FileUpdateChange =
            serde_json::from_value(json!({"path": "/a", "kind": {"type": "add"}, "diff": "x\n"})).unwrap();
        assert_eq!(add.kind, PatchChangeKind::Add);
        let mv: FileUpdateChange =
            serde_json::from_value(json!({"path": "/a", "kind": {"type": "update", "move_path": "/b"}, "diff": "@@"}))
                .unwrap();
        assert_eq!(mv.kind, PatchChangeKind::Update { move_path: Some("/b".into()) });
        let odd: FileUpdateChange =
            serde_json::from_value(json!({"path": "/a", "kind": {"type": "teleport"}, "diff": ""})).unwrap();
        assert_eq!(odd.kind, PatchChangeKind::Unknown);
    }
}
