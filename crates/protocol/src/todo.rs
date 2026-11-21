//! 清单（`todo.*`，docs/spec/daemon.md § 21）：节点上的个人待办。
//!
//! `Todo` 的 `source` 和 `unreviewed` 由 daemon 填，调用方不能指定；`todo.add` 的 `agent` 只是调用方自报「这是 agent 加的」。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, SessionId, TimestampMs};

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
    /// 这个版本不认识的来源（更新的 daemon 加了新的种类）。**daemon 自己从不产生它**，只是让旧客户端解析更新的 daemon 发来的数据时
    /// 不报错：客户端把它当作「不是某个人记的」（docs/spec/daemon.md § 21.2）。daemon 内部对真正的三种来源逐个处理。
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
            due_ms: None,
            source: TodoSource { kind: TodoSourceKind::Agent, device_id: None, session_id: Some("sess_1".into()) },
            unreviewed: true,
            created_ms: 1,
            updated_ms: 2,
            closed_ms: None,
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
    fn an_unknown_source_kind_still_parses_and_is_not_one_of_the_known_three() {
        let v = serde_json::json!({
            "id": "a", "title": "t", "body": "", "state": "open", "unreviewed": false, "created_ms": 1, "updated_ms": 2,
            "source": { "kind": "twin", "device_id": "d" }
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
}
