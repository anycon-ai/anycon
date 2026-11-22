//! 清单（`todo.*`，docs/spec/daemon.md § 21）：节点上的个人待办。
//!
//! `Todo` 的 `source` 和 `unreviewed` 由 daemon 填，调用方不能指定；`todo.add` 的 `agent` 只是调用方自报「这是 agent 加的」。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, SessionId, SessionSettings, SessionSummary, SessionWorkspace, TimestampMs};

/// 清单项的状态。这一步 daemon 不限制状态之间怎么转。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    /// 还没人处理（新建时的初值）。
    Open,
    /// 正在处理。
    Working,
    /// 等用户回答或决定。
    Waiting,
    /// 做完了，等用户验收。
    Review,
    /// 完成（关闭）。
    Done,
    /// 不做了（关闭）。
    Dropped,
}

impl TodoState {
    /// `done` / `dropped`：已关闭，有 `closed_ms`，按 `history_retention` 清理。
    pub fn is_closed(self) -> bool {
        matches!(self, TodoState::Done | TodoState::Dropped)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TodoState::Open => "open",
            TodoState::Working => "working",
            TodoState::Waiting => "waiting",
            TodoState::Review => "review",
            TodoState::Done => "done",
            TodoState::Dropped => "dropped",
        }
    }
}

/// 清单项的处理人；旧节点没有这个字段时由人处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoHandler {
    #[default]
    Me,
    Twin,
    #[serde(other)]
    Unknown,
}

impl TodoHandler {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Me => "me",
            Self::Twin => "twin",
            Self::Unknown => "unknown",
        }
    }
}

/// 一项是谁记的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoSourceKind {
    /// 经 hub 来的已配对设备。
    Device,
    /// 本机 socket 上的 CLI。
    Local,
    /// agent 加的（`todo.add` 带了 `agent: true`，或来自自报了 `mcp` 的本机连接）。
    Agent,
    /// 分身做的动作。
    Twin,
    /// 这个版本不认识的来源（更新的 daemon 加了新的种类）。**daemon 自己从不产生它**，只是让旧客户端解析更新的 daemon 发来的数据时
    /// 不报错：客户端把它当作「不是某个人记的」（docs/spec/daemon.md § 21.2）。daemon 内部对已知来源逐个处理。
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoSource {
    pub kind: TodoSourceKind,
    /// 经 hub 来的调用带上发起设备的 id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    /// 这一项是从哪个会话里来的（只有 `kind = agent` 才可能有；会话之后被删，这个字段原样留着）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Todo {
    /// 客户端生成的 UUID；`todo.add` 按它幂等。
    pub id: String,
    /// 单行，1–200 字符（去掉首尾空白之后）。
    pub title: String,
    /// Markdown，≤ 16 KiB，可以是空串。
    pub body: String,
    /// 项目名；不校验项目存在，项目改名 / 删除后原样保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub state: TodoState,
    #[serde(default)]
    pub handler: TodoHandler,
    /// 想在什么时候之前处理；只用于显示和排序。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<TimestampMs>,
    pub source: TodoSource,
    /// agent 记的、还没有人收下的项。
    pub unreviewed: bool,
    pub created_ms: TimestampMs,
    pub updated_ms: TimestampMs,
    /// 进入 `done` / `dropped` 的时间；重新打开时清掉。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_ms: Option<TimestampMs>,
    /// 这一项派出的会话（`todo.dispatch`，docs/spec/daemon.md § 22.3），按派发的先后，最多 20 个。会话被删除后关联留着。
    #[serde(default)]
    pub sessions: Vec<SessionId>,
    /// 派出的会话现在的情况，daemon 每次返回这一项时按会话此刻的状态算出来（docs/spec/daemon.md § 22.3）：不存储、不动
    /// `updated_ms`、不进审计。已关闭（`done` / `dropped`）或者没有派发过的项没有这个字段。`state` 只由人设置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<TodoActivity>,
}

/// 清单项派出的会话现在的情况（算出来的，不存；`Todo.activity`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoActivity {
    /// 有会话在跑、排队、还有后台任务，或者刚结束的一轮的结果还没记下。
    Working,
    /// 在等审批、出错、被打断、被取消，或者会话都没有了：要人来决定。
    Waiting,
    /// 最近的会话最近一轮正常结束、结果也已经记下，等人验收。
    Review,
    /// 更新版本的 daemon 给了一个这里不认识的值：客户端按「没有 activity」处理，不能因此解析失败。daemon 自己不产生它。
    #[serde(other)]
    Unknown,
}

/// `todo.list` 的翻页游标：上一页最后一项的这两个值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoCursor {
    pub updated_ms: TimestampMs,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoListParams {
    /// 缺省只给没关闭的；`true` 时也给 `done` / `dropped`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_closed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 缺省 200，上限 1000。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 返回排在这个游标后面的项（按 `(updated_ms, id)` 从新到旧）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<TodoCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoListResult {
    pub todos: Vec<Todo>,
    /// 后面还有没给出的项（一页另有 1 MiB 的字节预算，可能少于 `limit`）。
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoAddParams {
    /// UUID。已经存在时原样返回已有的那一项（幂等）。
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<TodoHandler>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<TimestampMs>,
    /// 调用方自报「这是 agent 加的」。只会让这一项更不被信任，所以不需要校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    /// 这一项从哪个会话里来；daemon 只在这一项记成 `kind = agent`、且这个会话存在时才记。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session: Option<SessionId>,
}

/// `todo.update` 的 `clear` 可以清空的可选字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClearableTodoField {
    Project,
    DueMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoUpdateParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler: Option<TodoHandler>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<TodoState>,
    /// `true`：把 `unreviewed` 清掉（`false` 或对已经收下的项会被忽略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed: Option<bool>,
    /// 调用方自报是 agent；不能把处理人设成分身，也不算人收下了这一项。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<bool>,
    /// 要清空的可选字段，在合并之后生效。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear: Vec<ClearableTodoField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoIdParams {
    pub id: String,
}

/// `todo.delete`：`if_updated_ms` 给了、而这一项现在的 `updated_ms` 不是它，就拒绝（`invalid`，`data.reason = "changed"`，
/// `data.updated_ms` 是现在的值），什么都不删。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoDeleteParams {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_updated_ms: Option<TimestampMs>,
}

/// `todo.changed` 通知：新建、修改、删除后发给所有在线客户端（包括发起的那一台）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoChanged {
    pub id: String,
    /// `deleted = true` 时没有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo: Option<Todo>,
    #[serde(default)]
    pub deleted: bool,
}

// ---------------- 派发（docs/spec/daemon.md § 22） ----------------

/// `todo.dispatch`：从一项开一个会话，把会话和这一项关联。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoDispatchParams {
    pub id: String,
    /// 同 `session.create`。
    pub agent: String,
    /// 缺省用这一项的 `project`；两者都没有 → `invalid`（`data.field = "project"`）。给了而且和这一项的不同，派发成功时写回这一项。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// 同 `session.create`。缺省：项目能开 worktree 就用新的 worktree，否则在项目目录里（不看 `worktree.default`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<SessionWorkspace>,
    /// 同 `session.create`。
    #[serde(default)]
    pub settings: SessionSettings,
    /// 「不 push、不开 PR」（agent 的 `no_push` 选项）。缺省 `true`；所选 agent 没有声明这个选项而它为 `true` → `unsupported`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_push: Option<bool>,
    /// 幂等：和 `session.create` 的 `message_id` 一样。同一个值重试（比如回执丢了）返回第一次的结果，不再开第二个会话、
    /// 不多记关联和「经过」；它同时是会话第一条消息的 id。缺省由 daemon 生成（那样就没有幂等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoDispatchResult {
    pub todo: Todo,
    pub session: SessionSummary,
}

/// `todo.dispatch_preview`：这一项现在派发的话，第一条消息会是什么。不改这一项、不开会话；唯一的副作用是先把排着的结果记进「经过」（§ 22.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoDispatchPreviewParams {
    pub id: String,
    /// 同 `todo.dispatch` 的 `no_push`，缺省 `true`（决定第一条消息里有没有「不 push、不开 PR」那一行）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_push: Option<bool>,
    /// 同 `todo.dispatch` 的 `project`：只影响 `rules_text`（派发会选另一个项目时，那个项目的规则）；缺省用这一项自己的项目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoDispatchPreviewResult {
    /// 此刻派发会发出的第一条消息，一字不差（同一个组装函数、同一套「之前的结果」的选法）。
    pub first_message: String,
    /// 此刻开一个这一项的项目的会话会注入的规则文字（docs/spec/daemon.md § 23.5，和 `rule.preview` 同一个函数）；这一项没有项目时
    /// 只有全节点规则；没有生效的规则时为空串。不看所选的 agent：它收不收得到由 `AgentInfo.capabilities.system_prompt` 说了算。
    #[serde(default)]
    pub rules_text: String,
}

/// 派发时生效的规则没有注入的原因（`TodoEventData::Dispatched.rules_skipped`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RulesSkipped {
    /// 所选 agent 收不到 `system_prompt_append`（`AgentInfo.capabilities.system_prompt = false`）。
    Unsupported,
    /// 这个版本不认识的原因；daemon 自己不产生它。
    #[serde(other)]
    Unknown,
}

/// `todo.history` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoHistoryParams {
    pub id: String,
    /// 只给 `seq` 小于它的事件（翻页用：上一页最后一条的 `seq`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_seq: Option<u64>,
    /// 缺省 50，上限 200。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoHistoryResult {
    /// 按 `seq` 从新到旧。
    pub events: Vec<TodoEvent>,
    pub has_more: bool,
}

/// 谁做了这件事（审计里的「发起者」，不含会话内容）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoActor {
    pub kind: TodoSourceKind,
    /// 分身动作的请求方（local 或设备 id）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
}

/// 「经过」里的一条的内容；`kind` 是线路上的标签。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TodoEventData {
    /// 派发了一次。
    Dispatched {
        session_id: SessionId,
        agent: String,
        workspace_mode: crate::SessionWorkspaceMode,
        no_push: bool,
        by: TodoActor,
        /// 注入这个会话的系统提示的规则（`rule.*`，docs/spec/daemon.md § 23.5），按在文字里的顺序。没有注入时缺省（旧 daemon 的记录也没有）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        rules: Vec<String>,
        /// 有生效的规则、但没有注入的原因。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules_skipped: Option<RulesSkipped>,
    },
    /// 关联会话的一轮结束了：agent 这一轮的最后一段话（≤ 4 KiB，在码点边界截断）。这一轮没有文字时 `text` 为空。
    /// `text` 是 agent 写的，按会话内容对待：不进日志、不进审计；客户端按不可信内容渲染。
    Result {
        session_id: SessionId,
        turn: u64,
        text: String,
        truncated: bool,
    },
    /// 人改了状态（这一项有过派发之后才记）。会话引起的变化不在这里：它是算出来的 `activity`，不存、不进「经过」。
    State {
        from: TodoState,
        to: TodoState,
        by: TodoActor,
    },
    HandedBack {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<String>,
    },
    TakenOver {
        by: TodoActor,
    },
    TwinDecision {
        run_id: String,
        action: crate::TwinAction,
        reason: String,
    },
    TwinQuestion {
        question_id: String,
    },
    Handler {
        from: TodoHandler,
        to: TodoHandler,
        by: TodoActor,
    },
    #[serde(other)]
    Unknown,
}

/// 「经过」里的一条：只追加，没有修改、删除一条的方法；删除这一项时一起删。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoEvent {
    /// 这一项内从 1 递增。
    pub seq: u64,
    pub ts_ms: TimestampMs,
    #[serde(flatten)]
    pub data: TodoEventData,
}

/// `todo.event` 通知：这一项的「经过」多了一条，发给所有在线客户端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoEventNotice {
    pub todo_id: String,
    pub event: TodoEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_roundtrips_and_omits_empty_optionals() {
        let t = Todo {
            id: "a".into(),
            title: "t".into(),
            body: String::new(),
            project: None,
            state: TodoState::Waiting,
            handler: Default::default(),
            due_ms: None,
            source: TodoSource { kind: TodoSourceKind::Agent, device_id: None, session_id: Some("sess_1".into()) },
            unreviewed: true,
            created_ms: 1,
            updated_ms: 2,
            closed_ms: None,
            sessions: vec![],
            activity: None,
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["state"], "waiting");
        assert_eq!(v["source"]["kind"], "agent");
        assert!(v.get("project").is_none() && v.get("closed_ms").is_none() && v["source"].get("device_id").is_none());
        assert_eq!(serde_json::from_value::<Todo>(v).unwrap(), t);
    }

    #[test]
    fn update_params_accept_a_bare_id_and_snake_case_clear() {
        let p: TodoUpdateParams = serde_json::from_value(serde_json::json!({ "id": "x" })).unwrap();
        assert!(p.clear.is_empty() && p.state.is_none());
        let p: TodoUpdateParams =
            serde_json::from_value(serde_json::json!({ "id": "x", "clear": ["project", "due_ms"] })).unwrap();
        assert_eq!(p.clear, vec![ClearableTodoField::Project, ClearableTodoField::DueMs]);
    }

    #[test]
    fn an_unknown_source_kind_still_parses_and_is_not_human() {
        let v = serde_json::json!({
            "id": "a", "title": "t", "body": "", "state": "open", "unreviewed": false, "created_ms": 1, "updated_ms": 2,
            "source": { "kind": "future", "device_id": "d" }
        });
        let t: Todo = serde_json::from_value(v).unwrap();
        assert_eq!(t.source.kind, TodoSourceKind::Unknown);
        assert_eq!(t.source.device_id.as_deref(), Some("d"));
    }

    #[test]
    fn delete_params_accept_a_bare_id_and_an_optional_condition() {
        let p: TodoDeleteParams = serde_json::from_value(serde_json::json!({ "id": "x" })).unwrap();
        assert_eq!(p.if_updated_ms, None);
        let p: TodoDeleteParams = serde_json::from_value(serde_json::json!({ "id": "x", "if_updated_ms": 5 })).unwrap();
        assert_eq!(p.if_updated_ms, Some(5));
        assert!(serde_json::to_value(TodoDeleteParams { id: "x".into(), if_updated_ms: None })
            .unwrap()
            .get("if_updated_ms")
            .is_none());
    }

    #[test]
    fn only_done_and_dropped_are_closed() {
        let closed: Vec<_> = [
            TodoState::Open,
            TodoState::Working,
            TodoState::Waiting,
            TodoState::Review,
            TodoState::Done,
            TodoState::Dropped,
        ]
        .into_iter()
        .filter(|s| s.is_closed())
        .collect();
        assert_eq!(closed, vec![TodoState::Done, TodoState::Dropped]);
    }

    #[test]
    fn todo_event_wire_shape() {
        let ev = TodoEvent {
            seq: 3,
            ts_ms: 9,
            data: TodoEventData::State {
                from: TodoState::Open,
                to: TodoState::Waiting,
                by: TodoActor { id: None, kind: TodoSourceKind::Device, device_id: Some("d1".into()) },
            },
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "seq": 3, "ts_ms": 9, "kind": "state", "from": "open", "to": "waiting",
                "by": { "kind": "device", "device_id": "d1" } })
        );
        assert_eq!(serde_json::from_value::<TodoEvent>(v).unwrap(), ev);
        let r = TodoEvent {
            seq: 1,
            ts_ms: 1,
            data: TodoEventData::Result { session_id: "s".into(), turn: 2, text: String::new(), truncated: false },
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "result");
        assert_eq!(serde_json::from_value::<TodoEvent>(v).unwrap(), r);
    }

    #[test]
    fn an_unknown_activity_from_a_newer_daemon_still_parses() {
        let t: Todo = serde_json::from_value(serde_json::json!({
            "id": "a", "title": "t", "body": "", "state": "open", "source": { "kind": "local" },
            "unreviewed": false, "created_ms": 1, "updated_ms": 1, "activity": "paused"
        }))
        .unwrap();
        assert_eq!(t.activity, Some(TodoActivity::Unknown));
    }

    #[test]
    fn activity_is_left_out_when_absent_and_snake_case_when_present() {
        let mut t: Todo = serde_json::from_value(serde_json::json!({
            "id": "a", "title": "t", "body": "", "state": "open", "source": { "kind": "local" },
            "unreviewed": false, "created_ms": 1, "updated_ms": 1
        }))
        .unwrap();
        assert!(t.activity.is_none() && serde_json::to_value(&t).unwrap().get("activity").is_none());
        t.activity = Some(TodoActivity::Review);
        assert_eq!(serde_json::to_value(&t).unwrap()["activity"], "review");
    }

    #[test]
    fn todo_without_sessions_field_reads_as_empty() {
        let v = serde_json::json!({
            "id": "a", "title": "t", "body": "", "state": "open", "source": { "kind": "local" },
            "unreviewed": false, "created_ms": 1, "updated_ms": 1
        });
        assert!(serde_json::from_value::<Todo>(v).unwrap().sessions.is_empty());
    }
}

#[cfg(test)]
mod twin_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_todos_default_to_me_and_future_handlers_are_safe() {
        let value = json!({"id":"t", "title":"t", "body":"", "state":"open", "source":{"kind":"local"},
            "unreviewed":false, "created_ms":1, "updated_ms":1});
        let todo: Todo = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(todo.handler, TodoHandler::Me);
        let mut future = value;
        future["handler"] = json!("future");
        let todo: Todo = serde_json::from_value(future).unwrap();
        assert_eq!(todo.handler, TodoHandler::Unknown);
        assert_eq!(serde_json::from_value::<TodoSourceKind>(json!("twin")).unwrap(), TodoSourceKind::Twin);
    }

    #[test]
    fn history_accepts_future_kinds_and_roundtrips_twin_events() {
        let unknown: TodoEvent =
            serde_json::from_value(json!({"seq":1,"ts_ms":2,"kind":"future","extra":{"a":3}})).unwrap();
        assert_eq!(unknown.data, TodoEventData::Unknown);
        for value in [
            json!({"kind":"handed_back", "reason":"Review needed"}),
            json!({"kind":"handed_back", "reason":"Review needed", "run_id":"r"}),
            json!({"kind":"taken_over", "by":{"kind":"device", "device_id":"d"}}),
            json!({"kind":"twin_decision", "run_id":"r", "action":"wait", "reason":"Running"}),
            json!({"kind":"twin_question", "question_id":"q"}),
            json!({"kind":"handler", "from":"me", "to":"twin", "by":{"kind":"twin", "id":"local"}}),
        ] {
            let event: TodoEventData = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(event).unwrap(), value);
        }
    }

    #[test]
    fn old_mode_and_judge_info_default_new_fields() {
        let m: crate::ModeInfo = serde_json::from_value(json!({"id":"ask", "label":"Ask", "risk":"normal"})).unwrap();
        assert!(!m.unattended);
        let j: crate::JudgeInfo = serde_json::from_value(json!({"backends":[],"daily_calls":20})).unwrap();
        assert!(j.advisor.is_none());
    }
}
