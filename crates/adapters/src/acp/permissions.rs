//! agent 发给我们的请求 ↔ [`PendingPermission`] / [`PermissionDecision`]（docs/spec/daemon.md § 9）。
//!
//! | 请求 | kind | 答复 |
//! |---|---|---|
//! | `session/request_permission`（`toolCall.kind = switch_mode` 以外） | `tool` | `{ outcome: { outcome: "selected", optionId } }` |
//! | `session/request_permission`（`toolCall.kind = switch_mode`） | `mode_change` | 同上 |
//! | `elicitation/create`，`mode = form` | `question`（schema 每个属性一题） | `{ action: "accept", content }` / `{ action: "decline" }` |
//! | `elicitation/create`，`mode = url` | `other`（`detail.url`） | `{ action: "accept" }` / `{ action: "decline" }` |
//!
//! `session/request_permission` 的 options 由 agent 定义（可能两项也可能四项），**原样透传**：`optionId` 当 option id，
//! `name` 当 label，`kind` 对应 `allow_once` / `allow_always` / `reject_once` / `reject_always`（不认识的当 `allow_once`：
//! 只影响颜色，daemon 自动拒绝时只挑 `reject_*`，所以不会误挑它）。答复里的 `optionId` 原样回。会话取消时回
//! `{ outcome: "cancelled" }`（elicitation 是 `{ action: "cancel" }`）——规格要求取消时必须这样答复所有未决请求。
//! `session/request_permission` 没法带「理由」，所以 `reject_*` 的 `needs_message` 恒为假。
//!
//! 表单 elicitation 的转换和 Codex 共用（`common::elicit`）。agent 给的文字不可信，标题只取第一行，说明有长度上限。

use anycon_agent_api::{AdapterError, PendingPermission, PermissionDecision};
use anycon_protocol::{
    PermissionDetail, PermissionKind, PermissionOption, PermissionOptionKind, Question, ToolPermissionDetail,
};
use serde_json::{json, Value};

use crate::common::elicit::{
    clip, convert_answers, field_from_schema, is_safe_http_url, ElicitField, MAX_ELICIT_FIELDS,
};

use super::tools::{first_line_clipped, ToolView};

pub const OPT_ACCEPT: &str = "accept";
pub const OPT_DECLINE: &str = "decline";

pub const M_REQUEST_PERMISSION: &str = "session/request_permission";
pub const M_ELICITATION: &str = "elicitation/create";

/// 适配器自己记着的、答复时要用的原生上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct NativePending {
    /// agent 请求的 JSON-RPC id，原样回传。
    pub rpc_id: Value,
    pub kind: NativeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NativeKind {
    /// `session/request_permission`：agent 给的选项（id 和 kind）。
    Tool { options: Vec<(String, PermissionOptionKind)> },
    /// `elicitation/create`。`fields` 是表单的各个属性；`None` = 没有表单（URL 模式、空表单），只有接受 / 拒绝。
    Elicitation { fields: Option<Vec<ElicitField>> },
}

pub fn option_kind(kind: &str) -> PermissionOptionKind {
    match kind {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => PermissionOptionKind::AllowOnce,
    }
}

fn option(id: &str, label: &str, kind: PermissionOptionKind) -> PermissionOption {
    PermissionOption { id: id.to_string(), label: label.to_string(), kind, needs_message: false }
}

/// `session/request_permission`。`view` 是 `toolCall` 合进已知的那次调用之后的样子。agent 的选项一个都读不出来 = `None`
/// （调用方回 `cancelled`，免得 agent 一直等）。
pub fn from_request_permission(
    request_id: &str,
    rpc_id: Value,
    params: &Value,
    view: &ToolView,
    cwd: &str,
) -> Option<(PendingPermission, NativePending)> {
    let mut options = Vec::new();
    let mut native_options = Vec::new();
    for o in params.get("options").and_then(Value::as_array)? {
        let Some(id) = o.get("optionId").and_then(Value::as_str).filter(|i| !i.is_empty()) else { continue };
        if native_options.iter().any(|(seen, _): &(String, PermissionOptionKind)| seen == id) {
            continue;
        }
        let kind = option_kind(o.get("kind").and_then(Value::as_str).unwrap_or(""));
        let label = o.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(id);
        options.push(option(id, label, kind));
        native_options.push((id.to_string(), kind));
    }
    if options.is_empty() {
        return None;
    }
    let title = first_line_clipped(&view.display_title(), 200);
    let (kind, detail) = if view.kind == "switch_mode" {
        let text = view.text();
        let text = if text.trim().is_empty() { view.display_title() } else { text };
        (PermissionKind::ModeChange, PermissionDetail { text: Some(clip(&text)), ..Default::default() })
    } else {
        (
            PermissionKind::Tool,
            PermissionDetail {
                tool: Some(ToolPermissionDetail {
                    tool: view.display_name(),
                    category: view.category(),
                    input: view.input(),
                    cwd: Some(cwd.to_string()),
                    detail: view.detail(cwd),
                }),
                ..Default::default()
            },
        )
    };
    let pending =
        PendingPermission { request_id: request_id.to_string(), kind, title, detail, options, expires_in: None };
    Some((pending, NativePending { rpc_id, kind: NativeKind::Tool { options: native_options } }))
}

fn elicitation_options() -> Vec<PermissionOption> {
    vec![
        option(OPT_ACCEPT, "Submit", PermissionOptionKind::AllowOnce),
        option(OPT_DECLINE, "Decline", PermissionOptionKind::RejectOnce),
    ]
}

/// `elicitation/create`（带 `sessionId`、调用方已经找到会话）→ 权限请求。
///
/// - `mode = form` → `kind = question`，schema 的每个属性一题；没有属性的表单 → `kind = other`，只有接受 / 拒绝；
/// - `mode = url` → `kind = other`，`detail.url`（只接受 http / https）。
///
/// `Err(说明)` = 这个请求不能交给用户（不认识的模式、读不懂的 schema、链接不是 http(s)）：调用方按 `decline` 答复，
/// 并把说明作为一条可恢复的 `error` 发出。
pub fn from_elicitation(
    request_id: &str,
    rpc_id: Value,
    params: &Value,
) -> Result<(PendingPermission, NativePending), String> {
    let message = params.get("message").and_then(Value::as_str).map(str::trim).unwrap_or("");
    let mode = params.get("mode").and_then(Value::as_str).unwrap_or("");

    if mode == "url" {
        let url = params.get("url").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if !is_safe_http_url(url) {
            return Err(
                "The agent asked you to open a link that is not a plain http(s) URL, so the request was declined."
                    .to_string(),
            );
        }
        let pending = PendingPermission {
            request_id: request_id.to_string(),
            kind: PermissionKind::Other,
            title: if message.is_empty() {
                "The agent needs you to open a link".to_string()
            } else {
                first_line_clipped(message, 120)
            },
            detail: PermissionDetail {
                text: Some(clip(message)).filter(|m| !m.is_empty()),
                url: Some(url.to_string()),
                ..Default::default()
            },
            options: vec![
                option(OPT_ACCEPT, "I've completed it", PermissionOptionKind::AllowOnce),
                option(OPT_DECLINE, "Decline", PermissionOptionKind::RejectOnce),
            ],
            expires_in: None,
        };
        return Ok((pending, NativePending { rpc_id, kind: NativeKind::Elicitation { fields: None } }));
    }
    if mode != "form" {
        return Err(format!(
            "The agent asked for input in a form anycon does not support ({}), so the request was declined.",
            first_line_clipped(mode, 40)
        ));
    }

    let schema = params.get("requestedSchema").filter(|s| s.is_object());
    let Some(props) = schema.and_then(|s| s.get("properties")).and_then(Value::as_object) else {
        return Err(
            "The agent asked for input with a form anycon cannot read, so the request was declined.".to_string()
        );
    };
    let title =
        if message.is_empty() { "The agent asks for input".to_string() } else { first_line_clipped(message, 120) };
    if props.is_empty() {
        let pending = PendingPermission {
            request_id: request_id.to_string(),
            kind: PermissionKind::Other,
            title,
            detail: PermissionDetail { text: Some(clip(message)).filter(|m| !m.is_empty()), ..Default::default() },
            options: elicitation_options(),
            expires_in: None,
        };
        return Ok((pending, NativePending { rpc_id, kind: NativeKind::Elicitation { fields: None } }));
    }
    if props.len() > MAX_ELICIT_FIELDS {
        return Err(format!(
            "The agent asked for input with a form of {} fields, which is too long to show, so the request was declined.",
            props.len()
        ));
    }
    let required: Vec<&str> = schema
        .and_then(|s| s.get("required"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let (mut fields, mut questions): (Vec<ElicitField>, Vec<Question>) = (Vec::new(), Vec::new());
    for (key, prop) in props {
        let (f, q) = field_from_schema(key, prop, required.contains(&key.as_str()));
        fields.push(f);
        questions.push(q);
    }
    // 标题放不下的整段说明（多行 / 很长）并进第一题，不丢
    if message.contains('\n') || message.chars().count() > 120 {
        questions[0].text = format!("{}\n\n{}", clip(message), questions[0].text);
    }
    let pending = PendingPermission {
        request_id: request_id.to_string(),
        kind: PermissionKind::Question,
        title,
        detail: PermissionDetail { questions, ..Default::default() },
        options: elicitation_options(),
        expires_in: None,
    };
    Ok((pending, NativePending { rpc_id, kind: NativeKind::Elicitation { fields: Some(fields) } }))
}

/// 用户接受了吗：选了 `accept`，或者没选项但带了回答（问题卡片的提交）。
fn elicitation_accepted(decision: &PermissionDecision) -> bool {
    match decision.option_id.as_deref() {
        Some(OPT_ACCEPT) => true,
        None => decision.answers.as_ref().is_some_and(|a| !a.is_empty()),
        Some(_) => false,
    }
}

/// 用户接受了表单但回答转不成 `content`：返回说明（调用方按 `decline` 答复，并发一条可恢复的 `error`）。
pub fn answer_problem(native: &NativePending, decision: &PermissionDecision, cancelling: bool) -> Option<String> {
    let NativeKind::Elicitation { fields: Some(fields) } = &native.kind else { return None };
    if cancelling || !elicitation_accepted(decision) {
        return None;
    }
    convert_answers(fields, decision.answers.as_ref()).err()
}

/// 客户端的答复 → 写回 agent 的 `result`。`cancelling`：会话正在取消，未答复的一律按取消答复。
pub fn build_response(
    native: &NativePending,
    decision: &PermissionDecision,
    cancelling: bool,
) -> Result<Value, AdapterError> {
    let option_id = decision.option_id.as_deref();
    match &native.kind {
        NativeKind::Tool { options } => {
            if cancelling {
                return Ok(json!({ "outcome": { "outcome": "cancelled" } }));
            }
            let chosen = match option_id {
                Some(id) => {
                    if !options.iter().any(|(o, _)| o == id) {
                        return Err(AdapterError::Protocol(format!("unknown permission option {id}")));
                    }
                    Some(id)
                }
                // 没选项（超时 / 取消之外的自动拒绝）：挑 agent 给的拒绝选项，一个都没有就按取消答复
                None => [PermissionOptionKind::RejectOnce, PermissionOptionKind::RejectAlways]
                    .into_iter()
                    .find_map(|k| options.iter().find(|(_, ok)| *ok == k))
                    .map(|(id, _)| id.as_str()),
            };
            Ok(match chosen {
                Some(id) => json!({ "outcome": { "outcome": "selected", "optionId": id } }),
                None => json!({ "outcome": { "outcome": "cancelled" } }),
            })
        }
        NativeKind::Elicitation { fields } => {
            if let Some(other) = option_id.filter(|o| *o != OPT_ACCEPT && *o != OPT_DECLINE) {
                return Err(AdapterError::Protocol(format!("unknown permission option {other}")));
            }
            if cancelling {
                return Ok(json!({ "action": "cancel" }));
            }
            if !elicitation_accepted(decision) {
                return Ok(json!({ "action": "decline" }));
            }
            match fields {
                None => Ok(json!({ "action": "accept" })),
                Some(fields) => match convert_answers(fields, decision.answers.as_ref()) {
                    Ok(content) => Ok(json!({ "action": "accept", "content": Value::Object(content) })),
                    // 回答转不成表单要的类型：不把半截内容交给 agent，按拒绝答复（调用方另发一条 error 说明）
                    Err(_) => Ok(json!({ "action": "decline" })),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn view(kind: &str, title: &str, raw: Value) -> ToolView {
        let mut v = ToolView::new("c1");
        v.merge(&json!({"kind": kind, "title": title, "rawInput": raw}));
        v
    }

    fn params() -> Value {
        json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "c1"},
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow-always", "name": "Always allow `ls`", "kind": "allow_always"},
                {"optionId": "reject-once", "name": "Reject", "kind": "reject_once"},
                {"optionId": "reject-always", "name": "Never", "kind": "reject_always"}
            ]
        })
    }

    fn decide(option: Option<&str>) -> PermissionDecision {
        PermissionDecision { option_id: option.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn the_agents_options_are_passed_through_unchanged() {
        let v = view("execute", "Run `ls -la`", json!({"command": "ls -la"}));
        let (p, native) = from_request_permission("r1", json!(7), &params(), &v, "/w").unwrap();
        assert_eq!(p.kind, PermissionKind::Tool);
        assert_eq!(p.title, "Run `ls -la`");
        assert_eq!(
            p.options.iter().map(|o| (o.id.as_str(), o.label.as_str(), o.kind)).collect::<Vec<_>>(),
            vec![
                ("allow-once", "Allow once", PermissionOptionKind::AllowOnce),
                ("allow-always", "Always allow `ls`", PermissionOptionKind::AllowAlways),
                ("reject-once", "Reject", PermissionOptionKind::RejectOnce),
                ("reject-always", "Never", PermissionOptionKind::RejectAlways),
            ]
        );
        assert!(p.options.iter().all(|o| !o.needs_message));
        let t = p.detail.tool.as_ref().unwrap();
        assert_eq!((t.category, t.cwd.as_deref()), (anycon_protocol::ToolCategory::Shell, Some("/w")));
        assert_eq!(t.input, json!({"command": "ls -la"}));
        assert_eq!(t.detail.as_ref().unwrap().command.as_deref(), Some("ls -la"));
        assert_eq!(native.rpc_id, json!(7));
        // optionId 原样回
        assert_eq!(
            build_response(&native, &decide(Some("allow-always")), false).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "allow-always"}})
        );
        assert_eq!(
            build_response(&native, &decide(Some("reject-once")), false).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}})
        );
    }

    #[test]
    fn two_option_requests_and_unknown_kinds_work() {
        let p = json!({"options": [
            {"optionId": "yes", "name": "Yes", "kind": "allow_once"},
            {"optionId": "no", "name": "No", "kind": "whatever"},
            {"optionId": "yes", "name": "dup", "kind": "allow_once"},
            {"name": "no id"},
            {"optionId": "", "name": "empty id"}
        ]});
        let (pending, _) = from_request_permission("r", json!("x"), &p, &view("other", "", Value::Null), "/w").unwrap();
        assert_eq!(pending.options.len(), 2, "duplicates and id-less options are dropped");
        assert_eq!(
            pending.options[1].kind,
            PermissionOptionKind::AllowOnce,
            "an unknown kind never counts as a rejection"
        );
        // 标题缺省时用名字 / kind
        assert_eq!(pending.title, "other");
        // 一个选项都读不出来：不能交给用户
        assert!(from_request_permission(
            "r",
            json!(1),
            &json!({"options": []}),
            &view("other", "t", Value::Null),
            "/w"
        )
        .is_none());
        assert!(from_request_permission("r", json!(1), &json!({}), &view("other", "t", Value::Null), "/w").is_none());
    }

    #[test]
    fn decisions_without_an_option_pick_a_reject_option_or_cancel() {
        let v = view("edit", "Edit a.txt", Value::Null);
        let (_, native) = from_request_permission("r", json!(1), &params(), &v, "/w").unwrap();
        assert_eq!(
            build_response(&native, &decide(None), false).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}})
        );
        // 会话取消：不管选了什么都是 cancelled
        assert_eq!(
            build_response(&native, &decide(Some("allow-once")), true).unwrap(),
            json!({"outcome": {"outcome": "cancelled"}})
        );
        // 只有放行类的选项、又没有答复：取消，而不是放行
        let allow_only = json!({"options": [{"optionId": "go", "name": "Go", "kind": "allow_once"}]});
        let (_, native) = from_request_permission("r", json!(1), &allow_only, &v, "/w").unwrap();
        assert_eq!(
            build_response(&native, &decide(None), false).unwrap(),
            json!({"outcome": {"outcome": "cancelled"}})
        );
        // 不在选项里的答复是错误（daemon 本来就校验过，这里是兜底）
        assert!(build_response(&native, &decide(Some("nope")), false).is_err());
    }

    #[test]
    fn a_switch_mode_tool_is_a_mode_change_with_the_plan_text() {
        let mut v = ToolView::new("c9");
        v.merge(&json!({
            "kind": "switch_mode", "title": "Ready for implementation",
            "content": [{"type": "content", "content": {"type": "text", "text": "## Plan\n1. do it"}}]
        }));
        let p = json!({"options": [
            {"optionId": "code", "name": "Yes, and auto-accept", "kind": "allow_always"},
            {"optionId": "ask", "name": "Yes, ask each time", "kind": "allow_once"},
            {"optionId": "reject", "name": "No", "kind": "reject_once"}
        ]});
        let (pending, native) = from_request_permission("r", json!(3), &p, &v, "/w").unwrap();
        assert_eq!(pending.kind, PermissionKind::ModeChange);
        assert_eq!(pending.detail.text.as_deref(), Some("## Plan\n1. do it"));
        assert!(pending.detail.tool.is_none() && pending.detail.mode.is_none());
        assert_eq!(
            build_response(&native, &decide(Some("code")), false).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "code"}})
        );
    }

    fn form(props: Value, required: Value) -> Value {
        json!({"sessionId": "s", "mode": "form", "message": "How should I proceed?",
               "requestedSchema": {"type": "object", "properties": props, "required": required}})
    }

    #[test]
    fn a_form_becomes_questions_and_the_answers_are_typed() {
        let p = form(
            json!({
                "strategy": {"type": "string", "title": "Strategy", "enum": ["safe", "fast"]},
                "count": {"type": "integer", "minimum": 1, "maximum": 5},
                "notify": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}},
                "note": {"type": "string"}
            }),
            json!(["strategy", "count"]),
        );
        let (pending, native) = from_elicitation("r", json!(11), &p).unwrap();
        assert_eq!(pending.kind, PermissionKind::Question);
        assert_eq!(pending.title, "How should I proceed?");
        assert_eq!(pending.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["accept", "decline"]);
        let q = &pending.detail.questions;
        assert_eq!(q.len(), 5);
        let strategy = q.iter().find(|x| x.id == "strategy").unwrap();
        assert_eq!(strategy.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["safe", "fast"]);
        assert!(!strategy.optional && q.iter().find(|x| x.id == "note").unwrap().optional);

        let ok = PermissionDecision {
            option_id: Some("accept".into()),
            answers: Some(answers(json!({"strategy": "fast", "count": "3", "notify": "true", "tags": ["a", "b"]}))),
            ..Default::default()
        };
        assert_eq!(answer_problem(&native, &ok, false), None);
        assert_eq!(
            build_response(&native, &ok, false).unwrap(),
            json!({"action": "accept", "content": {"strategy": "fast", "count": 3, "notify": true, "tags": ["a", "b"]}})
        );
        // 必填项缺失：按拒绝答复，并给出说明
        let bad = PermissionDecision {
            option_id: Some("accept".into()),
            answers: Some(answers(json!({"strategy": "fast"}))),
            ..Default::default()
        };
        assert!(answer_problem(&native, &bad, false).unwrap().contains("count"));
        assert_eq!(build_response(&native, &bad, false).unwrap(), json!({"action": "decline"}));
        // 超出范围
        let range = PermissionDecision {
            answers: Some(answers(json!({"strategy": "safe", "count": 9}))),
            ..Default::default()
        };
        assert!(answer_problem(&native, &range, false).unwrap().contains("range"));
        // decline / 取消
        assert_eq!(build_response(&native, &decide(Some("decline")), false).unwrap(), json!({"action": "decline"}));
        assert_eq!(build_response(&native, &ok, true).unwrap(), json!({"action": "cancel"}));
        assert_eq!(answer_problem(&native, &ok, true), None);
        assert!(build_response(&native, &decide(Some("maybe")), false).is_err());
    }

    #[test]
    fn url_forms_and_empty_forms_only_get_accept_or_decline() {
        let url = json!({"sessionId": "s", "mode": "url", "elicitationId": "e1", "url": "https://agent.example.com/connect?x=1",
                         "message": "Please authorize access to your repositories."});
        let (p, native) = from_elicitation("r", json!(5), &url).unwrap();
        assert_eq!(p.kind, PermissionKind::Other);
        assert_eq!(p.detail.url.as_deref(), Some("https://agent.example.com/connect?x=1"));
        assert_eq!(p.detail.text.as_deref(), Some("Please authorize access to your repositories."));
        assert_eq!(build_response(&native, &decide(Some("accept")), false).unwrap(), json!({"action": "accept"}));
        assert_eq!(build_response(&native, &decide(Some("decline")), false).unwrap(), json!({"action": "decline"}));
        assert_eq!(build_response(&native, &decide(Some("accept")), true).unwrap(), json!({"action": "cancel"}));
        // 不是 http(s) 的链接不给用户看
        for bad in ["javascript:alert(1)", "file:///etc/passwd", "https://a b", "ftp://x", ""] {
            let p = json!({"mode": "url", "url": bad, "elicitationId": "e", "message": "m"});
            assert!(from_elicitation("r", json!(1), &p).is_err(), "{bad:?}");
        }
        // 没有属性的表单：只有接受 / 拒绝
        let (p, native) = from_elicitation("r", json!(6), &form(json!({}), json!([]))).unwrap();
        assert_eq!(p.kind, PermissionKind::Other);
        assert_eq!(build_response(&native, &decide(Some("accept")), false).unwrap(), json!({"action": "accept"}));
    }

    #[test]
    fn unreadable_or_unknown_requests_are_refused_not_shown() {
        for p in [
            json!({"mode": "_custom", "message": "x"}),
            json!({"mode": "", "message": "x"}),
            json!({"mode": "form", "message": "x"}),
            json!({"mode": "form", "message": "x", "requestedSchema": "nope"}),
        ] {
            assert!(from_elicitation("r", json!(1), &p).is_err(), "{p}");
        }
        let many: serde_json::Map<String, Value> =
            (0..MAX_ELICIT_FIELDS + 1).map(|i| (format!("f{i}"), json!({"type": "string"}))).collect();
        assert!(from_elicitation("r", json!(1), &form(Value::Object(many), json!([]))).is_err());
        // 没有 message 也有可用的标题；很长的说明并进第一题
        let p = json!({"mode": "form", "requestedSchema": {"properties": {"a": {"type": "string"}}}});
        assert_eq!(from_elicitation("r", json!(1), &p).unwrap().0.title, "The agent asks for input");
        let long = format!("first line\n{}", "x".repeat(300));
        let p = json!({"mode": "form", "message": long, "requestedSchema": {"properties": {"a": {"type": "string"}}}});
        let (pending, _) = from_elicitation("r", json!(1), &p).unwrap();
        assert_eq!(pending.title, "first line");
        assert!(pending.detail.questions[0].text.contains(&"x".repeat(300)));
    }
}
