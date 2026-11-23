//! 分身的范围、状态、运行记录和问题协议（docs/spec/daemon.md § 26）；判断与执行由 daemon 负责。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, JudgeError, TimestampMs, TodoActor};

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
    /// 分身派出的会话的 PR 有了变化（检查失败、合并、关闭、新的评审意见；daemon spec § 26.9）。
    Pr,
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
    /// 派发到别的节点：那个节点不在线、调不通或超时（docs/spec/daemon.md § 27.4）。
    RemoteUnavailable,
    /// 派发到别的节点：那个节点拒绝了（范围、名额、预算、暂停）。
    RemoteRefused,
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

/// `twin.get` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinGetParams {
    /// 读谁的范围：缺省 `local`（本机分身）；也可以是一个已配对的分身设备（`DeviceKind::Twin`）的设备 id，读 B 给这条分身连接
    /// 的设置（docs/spec/daemon.md § 27.3）。不是已存在的分身设备 → `not_found`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinSetParams {
    /// 改谁的范围，同 [`TwinGetParams::requester`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
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
    /// 派发到哪个节点（节点表里的 id）；缺省 = 本机。只用于显示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
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

// ---------------- 分身连接（docs/spec/daemon.md § 27） ----------------

/// `node.twin_link_code` 的结果：B 开了一次只接受分身设备的短码配对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeTwinLinkCodeResult {
    /// 16 位短码（`XXXX-XXXX-XXXX-XXXX`）。
    pub code: String,
    /// 过期时间（10 分钟后）。
    pub expires_ms: TimestampMs,
}

/// `twin.link` 的参数：A 的 daemon 用 B 发的短码以分身设备的身份和 B 配对。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinLinkParams {
    /// B 的 `node.twin_link_code` 发出的短码。
    pub code: String,
    /// B 的节点 id：配对完成后核对，对不上就失败并丢掉这次配对。
    pub node_id: DeviceId,
}

/// B 对这条分身连接开放了什么、还剩多少（`twin.peer_status` 的结果，只给这条连接自己的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinPeerStatus {
    /// B 的节点名。
    pub name: String,
    /// B 的分组名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 对这条连接开放的项目；空 = 什么都没开放。
    pub projects: Vec<String>,
    pub agents: Vec<TwinPeerAgent>,
    /// 这条连接还能同时派出几个会话：它的上限和给人留的名额里更紧的那个，减去已在处理中的。
    pub remaining_sessions: u32,
    /// 这个本地日还能派发几次。
    pub remaining_dispatches: u32,
    /// 这条连接被 B 暂停了。
    pub paused: bool,
    /// B 现在接受分身连接（`[twin] accept_links`）。
    pub accept_links: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinPeerAgent {
    pub id: String,
    pub label: String,
    /// agent 现在可用（已安装、已登录）。
    pub ready: bool,
}

/// 一条分身连接（A 记下的，表 `twin_links`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinLink {
    /// B 的节点 id。
    pub node_id: DeviceId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// B 的静态公钥指纹。
    pub fingerprint: String,
    pub paired_ms: TimestampMs,
    /// 这条连接现在在线。
    #[serde(default)]
    pub online: bool,
    /// 上一次的 `twin.peer_status`；还没取到过为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TwinPeerStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinLinksResult {
    pub links: Vec<TwinLink>,
}

/// `twin.unlink` 的参数：A 忘掉这一条连接（关掉传输、删记录）。B 上的设备记录要在 B 上吊销。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinUnlinkParams {
    pub node_id: DeviceId,
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
        assert_eq!(
            serde_json::from_value::<TwinSkipReason>(json!("remote_unavailable")).unwrap(),
            TwinSkipReason::RemoteUnavailable
        );
        assert_eq!(
            serde_json::from_value::<TwinSkipReason>(json!("remote_refused")).unwrap(),
            TwinSkipReason::RemoteRefused
        );
    }

    #[test]
    fn requester_is_optional_and_left_out_when_absent() {
        let p: TwinGetParams = serde_json::from_value(json!({})).unwrap();
        assert!(p.requester.is_none() && serde_json::to_value(p).unwrap() == json!({}));
        let p: TwinSetParams = serde_json::from_value(json!({"requester":"dev1","paused":true})).unwrap();
        assert_eq!(p.requester.as_deref(), Some("dev1"));
    }

    #[test]
    fn peer_status_and_link_roundtrip() {
        let status = TwinPeerStatus {
            name: "b".into(),
            group: None,
            projects: vec!["p".into()],
            agents: vec![TwinPeerAgent { id: "claude-code".into(), label: "Claude Code".into(), ready: true }],
            remaining_sessions: 2,
            remaining_dispatches: 10,
            paused: false,
            accept_links: true,
        };
        let v = serde_json::to_value(&status).unwrap();
        assert!(v.get("group").is_none());
        assert_eq!(serde_json::from_value::<TwinPeerStatus>(v).unwrap(), status);
        let link: TwinLink =
            serde_json::from_value(json!({"node_id":"n","name":"b","fingerprint":"ab:cd","paired_ms":1})).unwrap();
        assert!(!link.online && link.status.is_none());
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
