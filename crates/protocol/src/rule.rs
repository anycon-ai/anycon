//! 决策档案里的规则（`rule.*`，docs/spec/daemon.md § 23）：用户自己的做法，生效的规则注入派发出去的会话的系统提示。
//!
//! 规则只影响 agent 怎么判断，不影响任何权限。`Rule` 的 `source.device_id` 由 daemon 填，调用方不能指定；`rule.add` 的 `agent`
//! 只是调用方自报「这是 agent 写的」。规则的文字是用户内容：不进日志，不进审计。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{DeviceId, SessionId, TimestampMs};

/// 规则的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    /// 待确认：不是人直接写下的（agent 提的、之后提炼出来的）。不注入。
    Pending,
    /// 生效。注入。
    Active,
    /// 人停用了，留着，可以再打开。不注入。
    Disabled,
    /// 人否决了。留着是为了之后提炼时不再提同样的。不注入。
    Rejected,
    /// 这个版本不认识的状态（更新的 daemon 加了新的值）。**daemon 自己从不产生它，也不接受它作为写入的值**；客户端按「不生效」
    /// 显示，解析不能因此失败（docs/spec/daemon.md § 23.2）。
    #[serde(other)]
    Unknown,
}

impl RuleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleStatus::Pending => "pending",
            RuleStatus::Active => "active",
            RuleStatus::Disabled => "disabled",
            RuleStatus::Rejected => "rejected",
            RuleStatus::Unknown => "unknown",
        }
    }
}

/// 规则是怎么来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleSourceKind {
    /// 人手写。
    Manual,
    /// 人在答复权限请求或提问时点了「记成规则」（带 `session_id`、`request_id`）。
    Answer,
    /// 从会话里提炼出来的（带 `session_id`、`seq`；还没有这个来源，docs/spec/daemon.md § 23.9）。
    Extracted,
    /// 调用方自报是 agent。
    Agent,
    /// 这个版本不认识的来源。**daemon 自己从不产生它**；客户端按「不是人写的」显示，解析不能因此失败。
    #[serde(other)]
    Unknown,
}

impl RuleSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RuleSourceKind::Manual => "manual",
            RuleSourceKind::Answer => "answer",
            RuleSourceKind::Extracted => "extracted",
            RuleSourceKind::Agent => "agent",
            RuleSourceKind::Unknown => "unknown",
        }
    }
}

/// 规则在节点之间的同步范围，由客户端同步（docs/spec/daemon.md § 23.8）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleSync {
    /// 只在这个节点。
    Node,
    /// 同组节点（由客户端同步）。
    Group,
    /// 这个版本不认识的范围；daemon 自己不产生它。
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleSource {
    pub kind: RuleSourceKind,
    /// `answer` / `extracted`：规则出自哪个会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// `extracted`：出自会话的哪个事件。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// `answer`：出自哪个权限请求或提问。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// 经 hub 来的调用带上发起设备的 id（daemon 填）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rule {
    /// 客户端生成的小写带连字符的 UUID；`rule.add` 按它幂等。
    pub id: String,
    /// 1–600 个 Unicode 码点，可以多行；除换行外不含控制字符，不含 U+2028 / U+2029。
    pub text: String,
    /// 只进这个项目的会话；缺省 = 这个节点上所有项目的会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub status: RuleStatus,
    pub sync: RuleSync,
    pub source: RuleSource,
    pub created_ms: TimestampMs,
    /// 每次真正的修改严格递增，也是版本号（`if_updated_ms`）。
    pub updated_ms: TimestampMs,
    /// 最近一次被注入某个会话；改它不动 `updated_ms`，也不发 `rule.changed`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_injected_ms: Option<TimestampMs>,
    /// 最近一次被分拣的决定引用、并且这个决定被执行；改它不动 `updated_ms`，也不发 `rule.changed`（§ 26.10）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_referenced_ms: Option<TimestampMs>,
    /// 过期：生效的规则 60 天里没被注入也没被引用，daemon 标上它。过期的规则仍是 `active`，但不注入、不计字数预算；
    /// 只有人能用 `rule.update { stale: false }` 恢复（docs/spec/daemon.md § 23.2、§ 26.10）。缺省 `false`。
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleListParams {
    /// 只给这个状态的规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RuleStatus>,
    /// 只给 `project` 等于它的规则（去掉首尾空白后比较；不含全节点规则）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// 注入的字数预算（docs/spec/daemon.md § 23.4）：字节数按**生效**的规则算，不受 `rule.list` 的过滤条件影响。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleBudget {
    /// 一个会话拿到的规则文字（含模板）的字节上限。
    pub cap: u64,
    /// 模板的开头加上全节点生效规则的字节数；一条生效的规则都没有时是 0。一个会话拿到的文字是它加上这个会话的项目的 `projects[项目]`
    /// （没有项目规则的会话就是它自己）。只有项目规则生效时，它只是模板的开头。
    pub node_bytes: u64,
    /// 每个项目的生效规则的字节数（不含 `node_bytes`），键是项目名。一个这个项目的会话拿到的文字是 `node_bytes + projects[项目]`。
    pub projects: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleListResult {
    /// 按 `created_ms` 从早到晚（同一毫秒的按写入先后）。
    pub rules: Vec<Rule>,
    pub budget: RuleBudget,
}

/// `rule.add` 可以直接给的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleAddStatus {
    Active,
    Pending,
    Disabled,
    Rejected,
}

/// `rule.add` 的调用方可以声明的来源种类（`extracted` 只有 daemon 自己提炼时才有，`agent` 由 `agent: true` 决定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleAddSourceKind {
    Manual,
    Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleAddSource {
    pub kind: RuleAddSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleAddParams {
    /// UUID。已经存在时原样返回已有的那一条（幂等）。
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 缺省 `active`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RuleAddStatus>,
    /// 缺省 `node`；agent 写入强制为 `node`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<RuleSync>,
    /// 缺省 `{ kind: manual }`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RuleAddSource>,
    /// 调用方自报「这是 agent 写的」：状态强制为 `pending`，来源强制为 `agent`。只会让这条规则更不被信任，所以不需要校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    /// 这条规则是哪个会话里的 agent 提的；只在这条规则记成 agent 写的、并且这个会话存在时才记（同 `todo.add` 的 `origin_session`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<SessionId>,
}

/// `rule.update` 的 `clear` 可以清空的可选字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearableRuleField {
    Project,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleUpdateParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 要清空的可选字段，在合并之后生效。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<ClearableRuleField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<RuleStatus>,
    /// 同组范围仅接受全节点、非待确认的规则。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<RuleSync>,
    /// 只能给 `false`：恢复一条过期的规则（和「变成生效」一样过字数检查）。给 `true` → `invalid`（`data.field = "stale"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
    /// 给了而和这条规则现在的 `updated_ms` 不同：什么都不改，`invalid`（`data.reason = "changed"`，`data.updated_ms` 是现在的值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_updated_ms: Option<TimestampMs>,
}

/// `rule.delete`：`if_updated_ms` 的语义同 `rule.update`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleDeleteParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_updated_ms: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RulePreviewParams {
    /// 缺省：不属于任何项目的会话（只有全节点规则）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// `rule.preview`：现在开一个这个项目的会话会注入的那段文字，和它用到的规则。只读。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RulePreviewResult {
    /// 没有生效的规则时为空串。
    pub text: String,
    /// 写进 `text` 的规则，按它们在 `text` 里的顺序。
    pub rule_ids: Vec<String>,
    /// `text` 的 UTF-8 字节数。
    pub bytes: u64,
}

/// `rule.changed` 通知：新建、修改、删除后发给所有在线客户端（包括发起的那一台）。`last_injected_ms` 的变化不通知。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleChanged {
    pub id: String,
    /// `deleted = true` 时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    #[serde(default)]
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_roundtrips_and_omits_empty_optionals() {
        let r = Rule {
            id: "a".into(),
            text: "t".into(),
            project: None,
            status: RuleStatus::Pending,
            sync: RuleSync::Node,
            source: RuleSource {
                kind: RuleSourceKind::Agent,
                session_id: None,
                seq: None,
                request_id: None,
                device_id: None,
            },
            created_ms: 1,
            updated_ms: 2,
            last_injected_ms: None,
            last_referenced_ms: None,
            stale: false,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            (v["status"].as_str(), v["sync"].as_str(), v["source"]["kind"].as_str()),
            (Some("pending"), Some("node"), Some("agent"))
        );
        assert!(
            v.get("project").is_none()
                && v.get("last_injected_ms").is_none()
                && v.get("last_referenced_ms").is_none()
                && v.get("stale").is_none()
                && v["source"].get("device_id").is_none()
        );
        assert_eq!(serde_json::from_value::<Rule>(v).unwrap(), r);
    }

    #[test]
    fn unknown_status_source_and_sync_still_parse() {
        let v = serde_json::json!({
            "id": "a", "text": "t", "status": "expired", "sync": "galaxy", "created_ms": 1, "updated_ms": 2,
            "source": { "kind": "twin", "device_id": "d" }
        });
        let r: Rule = serde_json::from_value(v).unwrap();
        assert_eq!(
            (r.status, r.sync, r.source.kind),
            (RuleStatus::Unknown, RuleSync::Unknown, RuleSourceKind::Unknown)
        );
        assert_eq!(r.source.device_id.as_deref(), Some("d"));
    }

    #[test]
    fn add_only_names_the_statuses_and_sources_a_caller_may_give() {
        let p: RuleAddParams = serde_json::from_value(serde_json::json!({ "id": "x", "text": "t" })).unwrap();
        assert!(p.status.is_none() && p.source.is_none() && p.agent.is_none());
        for bad in [
            serde_json::json!({ "id": "x", "text": "t", "status": "future" }),
            serde_json::json!({ "id": "x", "text": "t", "source": { "kind": "extracted" } }),
            serde_json::json!({ "id": "x", "text": "t", "source": { "kind": "agent" } }),
        ] {
            assert!(serde_json::from_value::<RuleAddParams>(bad).is_err());
        }
    }

    #[test]
    fn update_and_delete_params_accept_a_bare_id() {
        let p: RuleUpdateParams = serde_json::from_value(serde_json::json!({ "id": "x" })).unwrap();
        assert!(p.clear.is_empty() && p.status.is_none() && p.if_updated_ms.is_none());
        let p: RuleUpdateParams =
            serde_json::from_value(serde_json::json!({ "id": "x", "clear": ["project"], "if_updated_ms": 5 })).unwrap();
        assert_eq!((p.clear, p.if_updated_ms), (vec![ClearableRuleField::Project], Some(5)));
        let p: RuleDeleteParams = serde_json::from_value(serde_json::json!({ "id": "x" })).unwrap();
        assert_eq!(p.if_updated_ms, None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractTrigger {
    Manual,
    Auto,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractRunStatus {
    Running,
    Done,
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtractDropped {
    pub invalid: u32,
    pub duplicate: u32,
    pub full: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ExtractRun {
    pub id: String,
    pub session_id: SessionId,
    pub trigger: ExtractTrigger,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub status: ExtractRunStatus,
    pub started_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_ms: Option<TimestampMs>,
    pub points: u32,
    pub from_seq: u64,
    pub to_seq: u64,
    pub added: Vec<String>,
    pub dropped: ExtractDropped,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::JudgeError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::JudgeUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleExtractParams {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleExtractResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ExtractRun>,
    pub points: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleExtractRunsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleExtractRunsResult {
    pub runs: Vec<ExtractRun>,
    pub used_today: u32,
    pub daily_calls: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleExtractDone {
    pub run: ExtractRun,
}
