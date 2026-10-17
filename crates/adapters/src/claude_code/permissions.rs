//! `control_request { subtype: can_use_tool }` ↔ [`PendingPermission`] / [`PermissionDecision`]
//! （docs/spec/daemon.md § 9）。
//!
//! 三种来源：
//! - 普通工具 → `kind = tool`，options `allow` / `allow_session` / `deny`
//! - `ExitPlanMode` → `kind = plan`，计划全文来自 `input.plan`，options `allow` / `deny`
//! - `AskUserQuestion` → `kind = question`，题目来自 `input.questions`
//!
//! **question 的答复形状**（已用真实 CLI 2.1.293 验证，录制见 `tests/fixtures/claude/real_question_plan.jsonl`）：
//! `{ behavior: allow, updatedInput: { questions: <原样>, answers: { "<题目全文>": "<所选 label>" } } }`，
//! CLI 的 tool_result 回显 `Your questions have been answered: "<题目>"="<label>"`。多选按 Agent SDK 文档用 `", "`
//! 连接（多选这一条没单独验证）。读到的 `input` 原样回传，再填上 `answers`。
//! 题目 id 是 `q1..qn`，选项 id 就是 label 本身，所以客户端回的 `answers[q1] = "label"` 可以直接用。
//! **选了 `deny`（跳过）时一律回 `{ behavior: deny, message }`，即使请求里带着 `answers`**（选项优先于内容）；
//! 没带 `option_id` 但带了 `answers` 视为回答；`option_id` 不认识就报错。
//!
//! `ExitPlanMode` 允许时把 `input`（含 `plan`、`planFilePath`）原样回传；CLI 随后自己退出 plan 模式
//! （stdout 出现 `system/status { permissionMode }`），这个模式变化 adapter 没有事件可报，由 daemon 按规格自己切。

use anycon_agent_api::{AdapterError, PendingPermission, PermissionDecision};
use anycon_protocol::{
    PermissionDetail, PermissionKind, PermissionOption, PermissionOptionKind, Question, QuestionChoice,
    ToolPermissionDetail,
};
use serde_json::{json, Map, Value};

use super::tools;

pub const OPT_ALLOW: &str = "allow";
pub const OPT_ALLOW_SESSION: &str = "allow_session";
pub const OPT_DENY: &str = "deny";
pub const OPT_ANSWER: &str = "answer";

const TOOL_EXIT_PLAN_MODE: &str = "ExitPlanMode";
const TOOL_ASK_USER_QUESTION: &str = "AskUserQuestion";

/// 适配器自己记着的、回包时要用的原生上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct NativePending {
    pub request_id: String,
    pub kind: PermissionKind,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    pub input: Value,
    /// kind = question：`(question_id, 题目全文)`。
    pub questions: Vec<(String, String)>,
}

fn option(id: &str, label: &str, kind: PermissionOptionKind, needs_message: bool) -> PermissionOption {
    PermissionOption { id: id.to_string(), label: label.to_string(), kind, needs_message }
}

fn tool_title(name: &str, input: &Value) -> String {
    match name {
        "Bash" => "Run command".to_string(),
        "Edit" | "MultiEdit" | "NotebookEdit" => match tools::file_path_of(name, input) {
            Some(p) => format!("Edit {p}"),
            None => "Edit file".to_string(),
        },
        "Write" => match tools::file_path_of(name, input) {
            Some(p) => format!("Write {p}"),
            None => "Write file".to_string(),
        },
        "Read" => match tools::file_path_of(name, input) {
            Some(p) => format!("Read {p}"),
            None => "Read file".to_string(),
        },
        "WebFetch" => match input.get("url").and_then(Value::as_str) {
            Some(u) => format!("Fetch {u}"),
            None => "Fetch URL".to_string(),
        },
        "WebSearch" => "Search the web".to_string(),
        "Grep" | "Glob" | "LS" => "Search files".to_string(),
        "Task" | "Agent" => "Run subagent".to_string(),
        n => match tools::split_mcp(n) {
            Some((s, t)) => format!("Call {s}/{t}"),
            None => format!("Use tool {n}"),
        },
    }
}

/// 解析 CLI 发来的 `can_use_tool`。`req` 是 `control_request.request` 对象。
pub fn from_can_use_tool(request_id: &str, req: &Value, cwd: &str) -> (PendingPermission, NativePending) {
    let tool_name = req.get("tool_name").and_then(Value::as_str).unwrap_or("").to_string();
    let input = req.get("input").cloned().unwrap_or(Value::Null);
    let tool_use_id = req.get("tool_use_id").and_then(Value::as_str).map(str::to_string);

    let mut native = NativePending {
        request_id: request_id.to_string(),
        kind: PermissionKind::Tool,
        tool_name: tool_name.clone(),
        tool_use_id,
        input: input.clone(),
        questions: vec![],
    };

    let pending = match tool_name.as_str() {
        TOOL_EXIT_PLAN_MODE => {
            native.kind = PermissionKind::Plan;
            let plan = input.get("plan").and_then(Value::as_str).unwrap_or("").to_string();
            PendingPermission {
                request_id: request_id.to_string(),
                kind: PermissionKind::Plan,
                title: "Plan ready".to_string(),
                detail: PermissionDetail { text: Some(plan), ..Default::default() },
                options: vec![
                    option(OPT_ALLOW, "Start implementing", PermissionOptionKind::AllowOnce, false),
                    option(OPT_DENY, "Keep planning", PermissionOptionKind::RejectOnce, true),
                ],
                expires_in: None,
            }
        }
        TOOL_ASK_USER_QUESTION => {
            native.kind = PermissionKind::Question;
            let mut questions = Vec::new();
            if let Some(arr) = input.get("questions").and_then(Value::as_array) {
                for (i, q) in arr.iter().enumerate() {
                    let id = format!("q{}", i + 1);
                    let text = q.get("question").and_then(Value::as_str).unwrap_or("").to_string();
                    let choices = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|opts| {
                            opts.iter()
                                .filter_map(|o| o.get("label").and_then(Value::as_str))
                                .map(|l| QuestionChoice { id: l.to_string(), label: l.to_string() })
                                .collect()
                        })
                        .unwrap_or_default();
                    native.questions.push((id.clone(), text.clone()));
                    questions.push(Question {
                        id,
                        text,
                        choices,
                        multi: q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        // Claude Code 的提问界面总带一个「其他」自由输入。
                        free_text: true,
                    });
                }
            }
            PendingPermission {
                request_id: request_id.to_string(),
                kind: PermissionKind::Question,
                title: "Agent has a question".to_string(),
                detail: PermissionDetail { questions, ..Default::default() },
                options: vec![
                    option(OPT_ANSWER, "Submit answers", PermissionOptionKind::AllowOnce, false),
                    option(OPT_DENY, "Skip", PermissionOptionKind::RejectOnce, true),
                ],
                expires_in: None,
            }
        }
        name => {
            let category = tools::category_of(name);
            let allow_session_label = format!("Allow {name} for this session");
            PendingPermission {
                request_id: request_id.to_string(),
                kind: PermissionKind::Tool,
                title: tool_title(name, &input),
                detail: PermissionDetail {
                    tool: Some(ToolPermissionDetail {
                        tool: name.to_string(),
                        category,
                        input: input.clone(),
                        cwd: Some(cwd.to_string()),
                        detail: tools::detail_of(name, &input, cwd),
                    }),
                    ..Default::default()
                },
                options: vec![
                    option(OPT_ALLOW, "Allow", PermissionOptionKind::AllowOnce, false),
                    option(OPT_ALLOW_SESSION, &allow_session_label, PermissionOptionKind::AllowAlways, false),
                    option(OPT_DENY, "Deny", PermissionOptionKind::RejectOnce, true),
                ],
                expires_in: None,
            }
        }
    };
    (pending, native)
}

fn answer_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

fn deny(message: Option<&str>, default: &str) -> Value {
    let message = message.map(str::trim).filter(|m| !m.is_empty()).unwrap_or(default);
    json!({ "behavior": "deny", "message": message })
}

/// 把客户端的答复变成 `control_response.response.response`（`{ behavior, … }`）。
pub fn build_decision(native: &NativePending, decision: &PermissionDecision) -> Result<Value, AdapterError> {
    // question：先看选了哪个选项。选了 deny（跳过）就一定是拒绝——不管请求里有没有带 answers——
    // 否则审计记录是「拒绝」而实际效果是「放行并回答」。
    if native.kind == PermissionKind::Question {
        let answers = decision.answers.as_ref().filter(|a| !a.is_empty());
        return match (decision.option_id.as_deref(), answers) {
            (Some(OPT_DENY), _) | (None, None) => {
                Ok(deny(decision.message.as_deref(), "The user skipped this question."))
            }
            (Some(OPT_ANSWER) | None, Some(answers)) => {
                let mut mapped = Map::new();
                for (qid, v) in answers {
                    let key = native
                        .questions
                        .iter()
                        .find(|(id, _)| id == qid)
                        .map(|(_, text)| text.clone())
                        .unwrap_or_else(|| qid.clone());
                    mapped.insert(key, Value::String(answer_string(v)));
                }
                let mut input = match native.input.clone() {
                    Value::Object(m) => m,
                    _ => Map::new(),
                };
                input.insert("answers".to_string(), Value::Object(mapped));
                Ok(json!({ "behavior": "allow", "updatedInput": Value::Object(input) }))
            }
            (Some(OPT_ANSWER), None) => {
                Err(AdapterError::Protocol("question answered with option answer but no answers".to_string()))
            }
            (Some(other), _) => Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
        };
    }

    match decision.option_id.as_deref() {
        Some(OPT_ALLOW) => Ok(json!({ "behavior": "allow", "updatedInput": native.input })),
        Some(OPT_ALLOW_SESSION) => Ok(json!({
            "behavior": "allow",
            "updatedInput": native.input,
            "updatedPermissions": [{
                "type": "addRules",
                "rules": [{ "toolName": native.tool_name }],
                "behavior": "allow",
                "destination": "session",
            }],
        })),
        Some(OPT_DENY) | None => {
            let default = if native.kind == PermissionKind::Plan {
                "The user wants to keep planning."
            } else {
                "The user denied this tool call."
            };
            Ok(deny(decision.message.as_deref(), default))
        }
        Some(other) => Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
    }
}

/// 外层信封：写到 CLI stdin 的一行。
pub fn control_response(request_id: &str, inner: Value) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": inner },
    })
}

/// 对不认识的 `control_request` 回错误，免得 CLI 干等。
pub fn control_error(request_id: &str, message: &str) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": request_id, "error": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: &str, input: Value) -> Value {
        json!({ "subtype": "can_use_tool", "tool_name": tool, "input": input, "tool_use_id": "toolu_1",
                "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}] })
    }

    #[test]
    fn tool_request_maps_to_three_options() {
        let (p, n) = from_can_use_tool("r1", &req("Bash", json!({"command": "rm -rf build"})), "/proj");
        assert_eq!(p.kind, PermissionKind::Tool);
        assert_eq!(p.request_id, "r1");
        assert_eq!(p.title, "Run command");
        let t = p.detail.tool.unwrap();
        assert_eq!(t.tool, "Bash");
        assert_eq!(t.cwd.as_deref(), Some("/proj"));
        assert_eq!(t.detail.unwrap().command.as_deref(), Some("rm -rf build"));
        let ids: Vec<_> = p.options.iter().map(|o| (o.id.as_str(), o.kind, o.needs_message)).collect();
        assert_eq!(
            ids,
            vec![
                ("allow", PermissionOptionKind::AllowOnce, false),
                ("allow_session", PermissionOptionKind::AllowAlways, false),
                ("deny", PermissionOptionKind::RejectOnce, true),
            ]
        );
        assert_eq!(n.tool_use_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn allow_echoes_input() {
        let (_, n) = from_can_use_tool("r1", &req("Write", json!({"file_path": "/a", "content": "x"})), "/p");
        let d =
            build_decision(&n, &PermissionDecision { option_id: Some("allow".into()), ..Default::default() }).unwrap();
        assert_eq!(d, json!({"behavior": "allow", "updatedInput": {"file_path": "/a", "content": "x"}}));
    }

    #[test]
    fn allow_session_adds_session_scoped_rule() {
        let (_, n) = from_can_use_tool("r1", &req("Bash", json!({"command": "ls"})), "/p");
        let d =
            build_decision(&n, &PermissionDecision { option_id: Some("allow_session".into()), ..Default::default() })
                .unwrap();
        assert_eq!(d["behavior"], "allow");
        assert_eq!(
            d["updatedPermissions"],
            json!([{"type": "addRules", "rules": [{"toolName": "Bash"}], "behavior": "allow", "destination": "session"}])
        );
        assert_eq!(d["updatedInput"], json!({"command": "ls"}));
    }

    #[test]
    fn deny_carries_message_or_default() {
        let (_, n) = from_can_use_tool("r1", &req("Bash", json!({"command": "ls"})), "/p");
        let d = build_decision(
            &n,
            &PermissionDecision {
                option_id: Some("deny".into()),
                message: Some("use cargo instead".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(d, json!({"behavior": "deny", "message": "use cargo instead"}));
        let d =
            build_decision(&n, &PermissionDecision { option_id: Some("deny".into()), ..Default::default() }).unwrap();
        assert_eq!(d["behavior"], "deny");
        assert!(!d["message"].as_str().unwrap().is_empty());
        // 没有 option_id 也按拒绝处理（不会误放行）
        let d = build_decision(&n, &PermissionDecision::default()).unwrap();
        assert_eq!(d["behavior"], "deny");
    }

    #[test]
    fn unknown_option_is_an_error() {
        let (_, n) = from_can_use_tool("r1", &req("Bash", json!({})), "/p");
        assert!(
            build_decision(&n, &PermissionDecision { option_id: Some("zzz".into()), ..Default::default() }).is_err()
        );
    }

    #[test]
    fn exit_plan_mode_is_a_plan_permission() {
        let (p, n) = from_can_use_tool("r2", &req("ExitPlanMode", json!({"plan": "## Steps\n1. a"})), "/p");
        assert_eq!(p.kind, PermissionKind::Plan);
        assert_eq!(p.detail.text.as_deref(), Some("## Steps\n1. a"));
        assert!(p.detail.tool.is_none());
        assert_eq!(p.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["allow", "deny"]);
        let d = build_decision(
            &n,
            &PermissionDecision {
                option_id: Some("deny".into()),
                message: Some("also add tests".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(d, json!({"behavior": "deny", "message": "also add tests"}));
    }

    fn ask_input() -> Value {
        json!({"questions": [
            {"question": "Which database?", "header": "DB", "multiSelect": false,
             "options": [{"label": "Postgres", "description": "x"}, {"label": "SQLite", "description": "y"}]},
            {"question": "Which features?", "header": "Feat", "multiSelect": true,
             "options": [{"label": "Auth"}, {"label": "Billing"}]}
        ]})
    }

    #[test]
    fn ask_user_question_maps_questions_and_choices() {
        let (p, n) = from_can_use_tool("r3", &req("AskUserQuestion", ask_input()), "/p");
        assert_eq!(p.kind, PermissionKind::Question);
        assert_eq!(p.detail.questions.len(), 2);
        let q1 = &p.detail.questions[0];
        assert_eq!((q1.id.as_str(), q1.text.as_str(), q1.multi, q1.free_text), ("q1", "Which database?", false, true));
        assert_eq!(q1.choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["Postgres", "SQLite"]);
        assert!(p.detail.questions[1].multi);
        assert_eq!(n.questions[0], ("q1".to_string(), "Which database?".to_string()));
    }

    #[test]
    fn ask_user_question_answers_keyed_by_question_text() {
        let (_, n) = from_can_use_tool("r3", &req("AskUserQuestion", ask_input()), "/p");
        let mut answers = Map::new();
        answers.insert("q1".into(), json!("SQLite"));
        answers.insert("q2".into(), json!(["Auth", "Billing"]));
        let d = build_decision(&n, &PermissionDecision { answers: Some(answers), ..Default::default() }).unwrap();
        assert_eq!(d["behavior"], "allow");
        assert_eq!(
            d["updatedInput"]["answers"],
            json!({"Which database?": "SQLite", "Which features?": "Auth, Billing"})
        );
        // 原 questions 原样带回
        assert_eq!(d["updatedInput"]["questions"], ask_input()["questions"]);
    }

    #[test]
    fn ask_user_question_skip_denies() {
        let (_, n) = from_can_use_tool("r3", &req("AskUserQuestion", ask_input()), "/p");
        let d =
            build_decision(&n, &PermissionDecision { option_id: Some("deny".into()), ..Default::default() }).unwrap();
        assert_eq!(d["behavior"], "deny");
        // free text 里没匹配到题目 id 的 key 原样透传
        let mut answers = Map::new();
        answers.insert("custom".into(), json!("free"));
        let d = build_decision(&n, &PermissionDecision { answers: Some(answers), ..Default::default() }).unwrap();
        assert_eq!(d["updatedInput"]["answers"], json!({"custom": "free"}));
    }

    #[test]
    fn question_deny_option_wins_over_answers() {
        let (_, n) = from_can_use_tool("r3", &req("AskUserQuestion", ask_input()), "/p");
        let mut answers = Map::new();
        answers.insert("q1".into(), json!("SQLite"));
        // 选了 deny 又带了 answers：必须是拒绝，且带上用户的话
        let d = build_decision(
            &n,
            &PermissionDecision {
                option_id: Some("deny".into()),
                message: Some("not now".into()),
                answers: Some(answers.clone()),
            },
        )
        .unwrap();
        assert_eq!(d, json!({"behavior": "deny", "message": "not now"}));
        assert!(d.get("updatedInput").is_none());
        // 没给 message 就用默认话术
        let d = build_decision(
            &n,
            &PermissionDecision {
                option_id: Some("deny".into()),
                answers: Some(answers.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(d["behavior"], "deny");
        assert!(!d["message"].as_str().unwrap().is_empty());
        // 选了 answer + answers：放行并回答
        let d = build_decision(
            &n,
            &PermissionDecision {
                option_id: Some("answer".into()),
                answers: Some(answers.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"]["answers"], json!({"Which database?": "SQLite"}));
        // 选了 answer 却没有 answers / 选项不认识：报错，请求保持未决
        assert!(
            build_decision(&n, &PermissionDecision { option_id: Some("answer".into()), ..Default::default() }).is_err()
        );
        assert!(build_decision(
            &n,
            &PermissionDecision { option_id: Some("zzz".into()), answers: Some(answers), ..Default::default() }
        )
        .is_err());
        // 空 answers 等于没有
        let d = build_decision(&n, &PermissionDecision { answers: Some(Map::new()), ..Default::default() }).unwrap();
        assert_eq!(d["behavior"], "deny");
    }

    #[test]
    fn envelopes() {
        let v = control_response("r9", json!({"behavior": "allow"}));
        assert_eq!(v["type"], "control_response");
        assert_eq!(v["response"]["subtype"], "success");
        assert_eq!(v["response"]["request_id"], "r9");
        assert_eq!(v["response"]["response"]["behavior"], "allow");
        let e = control_error("r9", "nope");
        assert_eq!(e["response"]["subtype"], "error");
        assert_eq!(e["response"]["error"], "nope");
    }
}
