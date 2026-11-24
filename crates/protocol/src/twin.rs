//! 分身的范围、状态、运行记录和问题协议（docs/spec/daemon.md § 26）；判断与执行由 daemon 负责。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, JudgeError, JudgeUsage, TimestampMs, TodoActor};

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

/// 一次运行的摘要（`TwinGetResult.last_run`）：不含每项的决定，要看细节用 `twin.runs`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRunBrief {
    pub id: String,
    pub started_ms: TimestampMs,
    /// 缺省 = 还在运行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<TimestampMs>,
    pub triggers: Vec<TwinTrigger>,
    pub result: TwinRunResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JudgeError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinGetResult {
    pub scope: TwinScope,
    pub state: TwinState,
    pub enabled: bool,
    pub configured: bool,
    pub today: TwinToday,
    pub working: Vec<String>,
    pub waiting: Vec<String>,
    /// 最近一次真实唤醒（不含试跑），按 `started_ms`；可能还在运行（没有 `finished_ms`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<TwinRunBrief>,
    /// 已配置、本机启用且未暂停时：待处理触发的合并截止时间（运行中没有）和下一次兜底 tick 里较早的那个；都没有则缺省。
    /// 读取时的快照，变化时不发通知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake_ms: Option<TimestampMs>,
    /// 在分身范围内（§ 26.1），但既不在 `working` 里也没有未回答的问题的待办 id。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<String>,
    /// 仍在范围内、但上一次判断放不进输入（§ 26.4 的大小上限）的待办 id。只在内存里，重启后为空。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// 这次运行所有判断调用的用量之和；调用都没报告的成员缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<JudgeUsage>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRunsResult {
    pub runs: Vec<TwinRun>,
}

/// `twin.stats` 的参数。只统计本机分身。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinStatsParams {
    /// 统计最近几个本地日（含今天），1..=90，缺省 7；越界 → `invalid`，`data.field = "days"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
}

/// `twin.stats` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinStatsResult {
    /// 节点本地时区的每一天，按日期升序，最后一项是今天；没有活动的日子也在，数字为 0。
    pub days: Vec<TwinDayStats>,
}

/// 非试跑运行按 `result` 的计数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinRunCounts {
    #[serde(default)]
    pub done: u32,
    #[serde(default)]
    pub idle: u32,
    #[serde(default)]
    pub budget: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default)]
    pub cancelled: u32,
}

/// 非试跑运行里 `outcome = executed` 的决定按动作的计数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinDecisionCounts {
    #[serde(default)]
    pub dispatch: u32,
    #[serde(default)]
    pub ask_user: u32,
    #[serde(default)]
    pub ask_advisor: u32,
    #[serde(default)]
    pub wait: u32,
    #[serde(default)]
    pub hand_back: u32,
    #[serde(default)]
    pub done: u32,
}

/// 一个本地日的分身统计。数据来自节点保留的运行记录（90 天）和待办的经过（已删除待办的经过没了），所以旧日子的数字可能变小。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TwinDayStats {
    /// 节点本地时间的 `YYYY-MM-DD`。
    pub date: String,
    /// 这一天开始的本地零点。
    pub start_ms: TimestampMs,
    /// 这天开始的非试跑运行，按 `result`。
    #[serde(default)]
    pub runs: TwinRunCounts,
    #[serde(default)]
    pub dry_runs: u32,
    /// 这天开始的所有运行（含试跑，它们也占预算）发起的判断调用次数。
    #[serde(default)]
    pub judge_calls: u32,
    /// 这些调用的用量之和。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<JudgeUsage>,
    #[serde(default)]
    pub decisions: TwinDecisionCounts,
    /// 非试跑运行里 `outcome = skipped` 的决定数。
    #[serde(default)]
    pub skipped: u32,
    /// 这天待办经过里的 `handed_back` 事件数：分身没做完就交回给人的次数（`hand_back` 判断、连续 3 次失败、2 小时卡住）。
    /// 不含 `done` 判断（分身认为做完了，请人确认）写的那一条。
    #[serde(default)]
    pub handed_back: u32,
    /// 这天的 `taken_over` 事件数，只算本机分身派发过（不晚于事件）的项；分身设备派来的会话里的接手不算。
    #[serde(default)]
    pub taken_over: u32,
    /// 这天由未关闭（非 `done` / `dropped`）变为 `done` 的 `state` 事件数，且本机分身在事件时间或之前派发过这一项。
    /// 已关闭项在 `done` 和 `dropped` 之间改来改去不算；重新打开后再关闭算新的一次。
    #[serde(default)]
    pub accepted: u32,
    /// 同 `accepted`，但是变为 `dropped`。
    #[serde(default)]
    pub dropped: u32,
    /// 这天每个 `accepted` 事件的（事件时间 - 该项在事件时间或之前最近一次被本机分身派发的时间）之和；平均 = `turnaround_ms / accepted`。
    #[serde(default)]
    pub turnaround_ms: u64,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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

    #[test]
    fn get_result_new_fields_are_optional_and_skipped_when_empty() {
        let old = json!({
            "scope":{"paused":false,"projects":[],"max_sessions":1,"daily_dispatches":1},
            "state":"idle", "enabled":true, "configured":true,
            "today":{"dispatches":0,"judge_calls":0}, "working":[], "waiting":[]
        });
        let r: TwinGetResult = serde_json::from_value(old.clone()).unwrap();
        assert!(r.last_run.is_none() && r.next_wake_ms.is_none() && r.queued.is_empty() && r.deferred.is_empty());
        assert_eq!(serde_json::to_value(&r).unwrap(), old);
        let mut full = old;
        full["last_run"] = json!({"id":"r","started_ms":1,"triggers":["tick"],"result":"done"});
        full["next_wake_ms"] = json!(9);
        full["queued"] = json!(["a"]);
        full["deferred"] = json!(["b"]);
        let r: TwinGetResult = serde_json::from_value(full.clone()).unwrap();
        assert!(r.last_run.as_ref().unwrap().finished_ms.is_none());
        assert_eq!(serde_json::to_value(&r).unwrap(), full);
    }

    #[test]
    fn run_usage_is_optional() {
        let value = json!({
            "id":"r", "requester":"local", "started_ms":1, "triggers":[], "dry_run":false, "result":"done",
            "items":[], "judge_calls":[], "decisions":[], "usage":{"input_tokens":5,"cost_usd":0.5}
        });
        let run: TwinRun = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(run.usage.as_ref().unwrap().input_tokens, Some(5));
        assert!(run.usage.as_ref().unwrap().output_tokens.is_none());
        assert_eq!(serde_json::to_value(run).unwrap(), value);
        let mut no_usage = value;
        no_usage.as_object_mut().unwrap().remove("usage");
        let run: TwinRun = serde_json::from_value(no_usage).unwrap();
        assert!(run.usage.is_none());
        assert!(serde_json::to_value(run).unwrap().get("usage").is_none());
    }

    #[test]
    fn stats_params_and_day_stats_tolerate_missing_counters() {
        let p: TwinStatsParams = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p, TwinStatsParams::default());
        assert_eq!(serde_json::to_value(p).unwrap(), json!({}));
        let r: TwinStatsResult = serde_json::from_value(json!({
            "days":[{"date":"2026-01-02","start_ms":5,"runs":{"done":2},"decisions":{"ask_user":1},"accepted":1,"turnaround_ms":7}]
        }))
        .unwrap();
        let d = &r.days[0];
        assert_eq!((d.runs.done, d.runs.failed, d.decisions.ask_user, d.decisions.dispatch), (2, 0, 1, 0));
        assert!(d.usage.is_none() && d.dry_runs == 0 && d.judge_calls == 0 && d.taken_over == 0);
        assert_eq!((d.accepted, d.turnaround_ms), (1, 7));
        let v = serde_json::to_value(&r).unwrap();
        assert!(v["days"][0].get("usage").is_none());
        assert_eq!(serde_json::from_value::<TwinStatsResult>(v).unwrap(), r);
    }
}
