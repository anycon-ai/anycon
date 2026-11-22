//! 分身的范围、状态、运行记录和问题协议（docs/spec/daemon.md § 26）；判断与执行由 daemon 负责。

use serde::{Deserialize, Serialize};

use crate::{JudgeError, TimestampMs, TodoActor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinState {
    Running,
    Idle,
    Paused,
    Disabled,
    NotConfigured,
    Budget,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinTrigger {
    Todo,
    Session,
    Answer,
    Tick,
    Manual,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinRunResult {
    Done,
    Idle,
    Budget,
    Failed,
    Cancelled,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinAction {
    Dispatch,
    AskUser,
    AskAdvisor,
    Wait,
    HandBack,
    Done,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinDecisionOutcome {
    Executed,
    Skipped,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinSkipReason {
    Invalid,
    OutOfScope,
    NoCapacity,
    Budget,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TwinQuestionStatus {
    Open,
    Answered,
    Void,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearableTwinField {
    Agent,
    Model,
    Mode,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinScope {
    pub paused: bool,
    pub projects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub max_sessions: u32,
    pub daily_dispatches: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinToday {
    pub dispatches: u32,
    pub judge_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinGetResult {
    pub scope: TwinScope,
    pub state: TwinState,
    pub enabled: bool,
    pub configured: bool,
    pub today: TwinToday,
    pub working: Vec<String>,
    pub waiting: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinSetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_dispatches: Option<u32>,
    /// 合并字段之后清空；恢复 agent / model / mode 的缺省选择。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<ClearableTwinField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRun {
    pub id: String,
    /// 本机为 local；分身连接以设备 id 作请求方。
    pub requester: String,
    pub started_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<TimestampMs>,
    pub triggers: Vec<TwinTrigger>,
    pub dry_run: bool,
    pub result: TwinRunResult,
    pub items: Vec<String>,
    pub judge_calls: Vec<String>,
    pub decisions: Vec<TwinDecision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JudgeError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinDecision {
    pub todo_id: String,
    pub action: TwinAction,
    pub reason: String,
    pub rules: Vec<String>,
    pub outcome: TwinDecisionOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<TwinSkipReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_id: Option<String>,
    /// 解析后的动作参数；试跑据此展示计划，不保存原始模型输出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinQuestion {
    pub id: String,
    pub todo_id: String,
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    pub created_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_ms: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<TodoActor>,
    pub status: TwinQuestionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinDryRunResult {
    pub run: TwinRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRunsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_ms: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRunsResult {
    pub runs: Vec<TwinRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinQuestionsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_only: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinQuestionsResult {
    pub questions: Vec<TwinQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinAnswerParams {
    pub id: String,
    pub answer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinChanged {
    pub state: TwinState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<TwinRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinQuestionNotice {
    pub question: TwinQuestion,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn future_enum_values_are_accepted() {
        macro_rules! unknown {
            ($($ty:ty),+ $(,)?) => { $(
                assert_eq!(serde_json::from_value::<$ty>(json!("future")).unwrap(), <$ty>::Unknown);
            )+ };
        }
        unknown!(
            TwinState,
            TwinTrigger,
            TwinRunResult,
            TwinAction,
            TwinDecisionOutcome,
            TwinSkipReason,
            TwinQuestionStatus,
            ClearableTwinField
        );
    }

    #[test]
    fn set_supports_partial_updates_and_clearing_defaults() {
        let empty: TwinSetParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, TwinSetParams::default());
        assert_eq!(serde_json::to_value(empty).unwrap(), json!({}));
        let p: TwinSetParams =
            serde_json::from_value(json!({"agent":"mock", "clear":["agent","model","mode"]})).unwrap();
        assert_eq!(p.clear, vec![ClearableTwinField::Agent, ClearableTwinField::Model, ClearableTwinField::Mode]);
        assert_eq!(p.agent.as_deref(), Some("mock"));
        let p: TwinRunsParams = serde_json::from_value(json!({})).unwrap();
        assert!(p.limit.is_none() && p.before_ms.is_none());
        let p: TwinQuestionsParams = serde_json::from_value(json!({})).unwrap();
        assert!(p.open_only.is_none());
    }

    #[test]
    fn questions_and_runs_roundtrip_with_optional_metadata() {
        let q: TwinQuestion = serde_json::from_value(json!({
            "id":"q", "todo_id":"t", "question":"Which project?", "created_ms":1, "status":"open"
        }))
        .unwrap();
        assert!(q.options.is_empty() && q.answered_by.is_none());
        assert!(serde_json::to_value(q).unwrap().get("answered_ms").is_none());
        let value = json!({
            "id":"r", "requester":"local", "started_ms":1, "finished_ms":2,
            "triggers":["manual","todo"], "dry_run":true, "result":"failed", "items":["t"], "judge_calls":["j"],
            "decisions":[{"todo_id":"t", "action":"dispatch", "reason":"Ready", "rules":["rule"],
                "outcome":"skipped", "skip_reason":"no_capacity", "project":"p"}],
            "error":{"code":"bad_output"}
        });
        let run: TwinRun = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(run).unwrap(), value);
    }
}
