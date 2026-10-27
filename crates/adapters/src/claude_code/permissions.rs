//! `control_request { subtype: can_use_tool }` ↔ [`PendingPermission`] / [`PermissionDecision`]
//! （docs/spec/daemon.md § 9）。
//!
//! 三种来源：
//! - 普通工具 → `kind = tool`，options `allow` / `allow_session` / `deny`
//! - `ExitPlanMode` → `kind = plan`，计划全文来自 `input.plan`，options `allow` / `deny`
//! - `AskUserQuestion` → `kind = question`，题目来自 `input.questions`
//!
//! 另有 `control_request { subtype: elicitation }`（MCP server 向用户要输入，docs/spec/daemon.md § 9.1）：
//! 表单模式 → `kind = question`（schema 的每个属性一道题），URL 模式 → `kind = other`（`detail.url`），
//! options `accept` / `decline`；答复形状见 [`elicitation_reply`]。
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
pub const OPT_ACCEPT: &str = "accept";
pub const OPT_DECLINE: &str = "decline";

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
    /// MCP elicitation 请求：答复时按它把答案转换成 schema 要的类型（`from_elicitation`）。其它请求为 `None`。
    pub elicitation: Option<ElicitationNative>,
}

/// elicitation 的表单模式 / URL 模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationMode {
    Form,
    Url,
}

/// 表单里一个字段的类型（来自 `requested_schema.properties.<key>`）。
#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    Number,
    Integer,
    Boolean,
    /// enum / oneOf：允许的取值，答复时按取值的字符串形式匹配，回给 server 的是原值（可能是数字）。
    Choice(Vec<Value>),
    /// 数组 + enum 的 items：多选。
    MultiChoice(Vec<Value>),
}

/// 表单里的一个字段。
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationField {
    pub key: String,
    /// 给用户看的名字（`title`，没有就是 `key`），错误信息里用。
    pub label: String,
    pub required: bool,
    pub ty: FieldType,
}

/// elicitation 请求的原生上下文。
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationNative {
    pub mode: ElicitationMode,
    pub fields: Vec<ElicitationField>,
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
        elicitation: None,
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
                        optional: false,
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
            // Workflow 的 input 只有脚本；一句话说明在请求的 `description` 里（脚本 `meta.description`）
            let title = match (name, req.get("description").and_then(Value::as_str).filter(|d| !d.is_empty())) {
                ("Workflow", Some(d)) => format!("Run workflow: {d}"),
                _ => tool_title(name, &input),
            };
            PendingPermission {
                request_id: request_id.to_string(),
                kind: PermissionKind::Tool,
                title,
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

/// 取值的字符串形式：字符串原样，数字 / 布尔取 JSON 写法。选项 id 和答复里的取值都用它比较。
fn value_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `enum` / `oneOf` / `anyOf`（`{const, title}`）→ `(原值, 显示名)`。取不到返回空。
fn choices_of(schema: &Value) -> Vec<(Value, String)> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        // 旧写法的 `enumNames` 与 `enum` 按位置对应
        let names = schema.get("enumNames").and_then(Value::as_array);
        return values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let label = names
                    .and_then(|n| n.get(i))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| value_key(v));
                (v.clone(), label)
            })
            .collect();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array) {
            let out: Vec<(Value, String)> = options
                .iter()
                .filter_map(|o| {
                    let v = o.get("const")?;
                    let label = o.get("title").and_then(Value::as_str).map(str::to_string);
                    Some((v.clone(), label.unwrap_or_else(|| value_key(v))))
                })
                .collect();
            if !out.is_empty() {
                return out;
            }
        }
    }
    Vec::new()
}

/// 一个属性的 schema → 字段类型和（有的话）选项。
fn field_of(prop: &Value) -> (FieldType, Vec<(Value, String)>) {
    let single = choices_of(prop);
    if !single.is_empty() {
        let values = single.iter().map(|(v, _)| v.clone()).collect();
        return (FieldType::Choice(values), single);
    }
    match prop.get("type").and_then(Value::as_str) {
        Some("boolean") => {
            (FieldType::Boolean, vec![(Value::Bool(true), "Yes".to_string()), (Value::Bool(false), "No".to_string())])
        }
        Some("integer") => (FieldType::Integer, Vec::new()),
        Some("number") => (FieldType::Number, Vec::new()),
        Some("array") => {
            let multi = prop.get("items").map(choices_of).unwrap_or_default();
            if multi.is_empty() {
                (FieldType::Text, Vec::new())
            } else {
                let values = multi.iter().map(|(v, _)| v.clone()).collect();
                (FieldType::MultiChoice(values), multi)
            }
        }
        _ => (FieldType::Text, Vec::new()),
    }
}

/// 数字类字段在题干里提示取值范围：自由输入的题目没有别的约束提示。
fn range_hint(prop: &Value, ty: &FieldType) -> Option<String> {
    let kind = match ty {
        FieldType::Integer => "integer",
        FieldType::Number => "number",
        _ => return None,
    };
    let bound = |k: &str| prop.get(k).filter(|v| v.is_number()).map(Value::to_string);
    Some(match (bound("minimum"), bound("maximum")) {
        (Some(lo), Some(hi)) => format!("{kind}, {lo}–{hi}"),
        (Some(lo), None) => format!("{kind}, at least {lo}"),
        (None, Some(hi)) => format!("{kind}, at most {hi}"),
        (None, None) => kind.to_string(),
    })
}

/// 只有 `http` / `https` 的链接能进 `detail.url`（客户端据此提供「打开」入口）。
fn is_web_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && !url.chars().any(char::is_control)
}

/// 解析 `control_request { subtype: elicitation }`。`req` 是 `control_request.request` 对象。
///
/// - 表单模式（`mode` 为 `form` 或缺省）→ `kind = question`：`requested_schema.properties` 的每个属性一道题
///   （题目 id = 属性名；顺序按 `property_order`，没有就按 schema 里的顺序）。enum / oneOf → 选项；
///   boolean → `true` / `false` 两个选项；string / number / integer → 自由输入；必填项在题干里标出。
///   选项 `accept`（Submit）/ `decline`。
/// - URL 模式（`mode = url`，或没写 `mode` 但带了 `url`）→ `kind = other`，`detail.url` 是链接，
///   选项 `accept`（「我已经完成了」）/ `decline`。
///
/// `Err(原因)`：请求本身不能交给用户（URL 不是 http / https）。调用方应当直接回 decline 并报一个可恢复的 `error`。
pub fn from_elicitation(request_id: &str, req: &Value) -> Result<(PendingPermission, NativePending), String> {
    let server = req.get("mcp_server_name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("MCP server");
    let message = req.get("message").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let url = req.get("url").and_then(Value::as_str).filter(|u| !u.is_empty());
    let mode = match req.get("mode").and_then(Value::as_str) {
        Some("url") => ElicitationMode::Url,
        Some(_) => ElicitationMode::Form,
        None if url.is_some() => ElicitationMode::Url,
        None => ElicitationMode::Form,
    };
    let mut native = NativePending {
        request_id: request_id.to_string(),
        kind: PermissionKind::Question,
        tool_name: "elicitation".to_string(),
        tool_use_id: None,
        input: Value::Null,
        questions: vec![],
        elicitation: None,
    };

    if mode == ElicitationMode::Url {
        let Some(url) = url.filter(|u| is_web_url(u)) else {
            return Err(format!("{server} asked you to open a link that is not an http(s) URL, so it was declined"));
        };
        native.kind = PermissionKind::Other;
        native.elicitation = Some(ElicitationNative { mode, fields: vec![] });
        let pending = PendingPermission {
            request_id: request_id.to_string(),
            kind: PermissionKind::Other,
            title: format!("{server} needs you to open a link"),
            detail: PermissionDetail {
                text: Some(if message.is_empty() {
                    format!("{server} asks you to complete a step in your browser.")
                } else {
                    message
                }),
                url: Some(url.trim().to_string()),
                ..Default::default()
            },
            options: vec![
                option(OPT_ACCEPT, "I've completed it", PermissionOptionKind::AllowOnce, false),
                option(OPT_DECLINE, "Decline", PermissionOptionKind::RejectOnce, false),
            ],
            expires_in: None,
        };
        return Ok((pending, native));
    }

    let schema = req.get("requested_schema").unwrap_or(&Value::Null);
    let props = schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    // 属性顺序：CLI 在 `property_order` 里给出 server 列出的顺序（JSON 对象本身在这里不保序）；没列到的按对象里的顺序补在后面
    let mut ordered: Vec<(&String, &Value)> = Vec::new();
    if let Some(props) = props {
        let listed = req.get("property_order").and_then(Value::as_array);
        for k in listed.into_iter().flatten().filter_map(Value::as_str) {
            if let Some((key, prop)) = props.get_key_value(k) {
                if !ordered.iter().any(|(x, _)| *x == key) {
                    ordered.push((key, prop));
                }
            }
        }
        for (key, prop) in props {
            if !ordered.iter().any(|(x, _)| *x == key) {
                ordered.push((key, prop));
            }
        }
    }

    let mut questions = Vec::new();
    let mut fields = Vec::new();
    for (key, prop) in ordered {
        let (ty, choices) = field_of(prop);
        let label = prop.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(key).to_string();
        let is_required = required.contains(&key.as_str());
        let mut text = label.clone();
        if let Some(hint) = range_hint(prop, &ty) {
            text.push_str(&format!(" ({hint})"));
        }
        if let Some(d) = prop.get("description").and_then(Value::as_str).filter(|d| !d.is_empty()) {
            text.push_str(&format!(" — {d}"));
        }
        let multi = matches!(ty, FieldType::MultiChoice(_));
        let free_text = matches!(ty, FieldType::Text | FieldType::Number | FieldType::Integer);
        native.questions.push((key.clone(), text.clone()));
        questions.push(Question {
            id: key.clone(),
            text,
            choices: choices.iter().map(|(v, l)| QuestionChoice { id: value_key(v), label: l.clone() }).collect(),
            multi,
            free_text,
            // schema 没列进 `required` 的字段可以留空：答复时直接省略（见 `convert_answers`）
            optional: !is_required,
        });
        fields.push(ElicitationField { key: key.clone(), label, required: is_required, ty });
    }
    native.elicitation = Some(ElicitationNative { mode, fields });
    let pending = PendingPermission {
        request_id: request_id.to_string(),
        kind: PermissionKind::Question,
        title: if message.is_empty() { format!("{server} needs your input") } else { format!("{server}: {message}") },
        detail: PermissionDetail { questions, ..Default::default() },
        options: vec![
            option(OPT_ACCEPT, "Submit", PermissionOptionKind::AllowOnce, false),
            option(OPT_DECLINE, "Decline", PermissionOptionKind::RejectOnce, false),
        ],
        expires_in: None,
    };
    Ok((pending, native))
}

/// 答复 elicitation 的结果：写给 CLI 的 `response`（`{action, content?}`），以及答案不合法时要报给用户的原因。
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationReply {
    pub response: Value,
    /// 用户选了 accept，但答案和 schema 对不上，已经改按 decline 答复；调用方发一个可恢复的 `error` 说明。
    pub problem: Option<String>,
}

fn decline() -> Value {
    json!({ "action": "decline" })
}

/// 一个答案的字符串形式（多选取第一项以外的由调用方处理）。
fn answer_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        other => other.to_string(),
    }
}

/// 把客户端的答案按 schema 转换成 `content`。答案缺失 / 为空：可选字段跳过，必填字段报错。
fn convert_answers(fields: &[ElicitationField], answers: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let mut content = Map::new();
    for f in fields {
        let raw = answers.get(&f.key);
        let items: Vec<String> = match raw {
            None | Some(Value::Null) => vec![],
            Some(Value::Array(a)) => a.iter().map(answer_text).filter(|s| !s.is_empty()).collect(),
            Some(v) => Some(answer_text(v)).filter(|s| !s.is_empty()).into_iter().collect(),
        };
        if items.is_empty() {
            if f.required {
                return Err(format!("\"{}\" is required", f.label));
            }
            continue;
        }
        let first = &items[0];
        let value = match &f.ty {
            FieldType::Text => Value::String(items.join(", ")),
            FieldType::Integer => first
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| format!("\"{}\" must be an integer, got {first:?}", f.label))?,
            FieldType::Number => first
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| format!("\"{}\" must be a number, got {first:?}", f.label))?,
            FieldType::Boolean => match first.to_ascii_lowercase().as_str() {
                "true" | "yes" | "y" | "1" => Value::Bool(true),
                "false" | "no" | "n" | "0" => Value::Bool(false),
                _ => return Err(format!("\"{}\" must be true or false, got {first:?}", f.label)),
            },
            FieldType::Choice(allowed) => allowed
                .iter()
                .find(|v| value_key(v) == *first)
                .cloned()
                .ok_or_else(|| format!("\"{}\": {first:?} is not one of the allowed values", f.label))?,
            FieldType::MultiChoice(allowed) => {
                let mut picked = Vec::new();
                for item in &items {
                    let v = allowed
                        .iter()
                        .find(|v| value_key(v) == *item)
                        .ok_or_else(|| format!("\"{}\": {item:?} is not one of the allowed values", f.label))?;
                    picked.push(v.clone());
                }
                Value::Array(picked)
            }
        };
        content.insert(f.key.clone(), value);
    }
    Ok(content)
}

/// 把客户端对 elicitation 的答复变成 `control_response.response.response`。
///
/// 已用真实 CLI 2.1.308 验证（录制见 `tests/fixtures/claude/real_elicitation_*.jsonl`）：形状是
/// `{ action: "accept" | "decline" | "cancel", content?: {…} }`，MCP server 收到的就是这个对象（合法的表单 `content` 原样透传；不合法的没有试过，所以先按 schema 转类型）。
/// - 选 `decline`、没选也没给答案 → `{action: decline}`（会话取消 / 超时 daemon 自动答复的也是 reject 选项，同样是 decline）
/// - 选 `accept`（或只带了 `answers`）：URL 模式 → `{action: accept}`；表单模式 → `{action: accept, content}`，
///   答案按 schema 转换类型。转换失败 → 改按 decline 答复，`problem` 说明原因（请求在 daemon 里已经算答复过，
///   不能再留着未决，否则 CLI 会一直等下去）
/// - 选项不认识 → 错误
pub fn elicitation_reply(
    spec: &ElicitationNative,
    decision: &PermissionDecision,
) -> Result<ElicitationReply, AdapterError> {
    let answers = decision.answers.as_ref();
    let accepting = match decision.option_id.as_deref() {
        Some(OPT_DECLINE) => false,
        Some(OPT_ACCEPT) => true,
        None => answers.is_some() && spec.mode == ElicitationMode::Form,
        Some(other) => return Err(AdapterError::Protocol(format!("unknown permission option {other}"))),
    };
    if !accepting {
        return Ok(ElicitationReply { response: decline(), problem: None });
    }
    if spec.mode == ElicitationMode::Url {
        return Ok(ElicitationReply { response: json!({ "action": "accept" }), problem: None });
    }
    let empty = Map::new();
    match convert_answers(&spec.fields, answers.unwrap_or(&empty)) {
        Ok(content) => {
            Ok(ElicitationReply { response: json!({ "action": "accept", "content": content }), problem: None })
        }
        Err(why) => Ok(ElicitationReply {
            response: decline(),
            problem: Some(format!("The answer to the MCP server's question was not valid ({why}); it was declined.")),
        }),
    }
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
    fn workflow_permission_is_titled_with_its_description() {
        // 形状取自 CLI 2.1.308：`description` 是脚本 meta 里的一句话说明，input 里只有脚本
        let r = json!({ "subtype": "can_use_tool", "tool_name": "Workflow", "display_name": "Workflow",
            "input": {"script": "export const meta = {}"}, "description": "Review the diff", "tool_use_id": "toolu_w" });
        let (p, _) = from_can_use_tool("r1", &r, "/p");
        assert_eq!(p.kind, PermissionKind::Tool);
        assert_eq!(p.title, "Run workflow: Review the diff");
        let (p, _) = from_can_use_tool("r1", &req("Workflow", json!({"script": "x"})), "/p");
        assert_eq!(p.title, "Use tool Workflow");
    }

    // ---------- MCP elicitation ----------

    /// 实录（CLI 2.1.308，`tests/fixtures/claude/real_elicitation_form.jsonl`）里的表单请求。
    fn form_request() -> Value {
        json!({"subtype": "elicitation", "mcp_server_name": "example-elicit", "message": "Please enter your details", "mode": "form",
            "requested_schema": {"type": "object", "properties": {
                "name": {"type": "string", "title": "Name", "description": "Your first name"},
                "age": {"type": "integer", "title": "Age", "minimum": 0, "maximum": 150},
                "subscribe": {"type": "boolean", "title": "Subscribe to updates", "default": false},
                "color": {"type": "string", "title": "Favorite color", "enum": ["red", "green", "blue"]}},
                "required": ["name"]},
            "property_order": ["name", "age", "subscribe", "color"]})
    }

    fn answers(pairs: &[(&str, Value)]) -> PermissionDecision {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        PermissionDecision { option_id: Some(OPT_ACCEPT.into()), answers: Some(m), ..Default::default() }
    }

    #[test]
    fn elicitation_form_becomes_one_question_per_property() {
        let (p, n) = from_elicitation("e1", &form_request()).unwrap();
        assert_eq!(p.kind, PermissionKind::Question);
        assert_eq!(p.title, "example-elicit: Please enter your details");
        assert_eq!(
            p.options.iter().map(|o| (o.id.as_str(), o.label.as_str(), o.kind)).collect::<Vec<_>>(),
            vec![
                ("accept", "Submit", PermissionOptionKind::AllowOnce),
                ("decline", "Decline", PermissionOptionKind::RejectOnce),
            ]
        );
        let q = &p.detail.questions;
        // 顺序按 `property_order`（JSON 对象本身按字母序），题目 id 是属性名
        assert_eq!(q.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec!["name", "age", "subscribe", "color"]);
        // 必答是默认，不在题干里标；schema 没列进 `required` 的字段是 `optional`
        assert_eq!(q[0].text, "Name — Your first name");
        assert_eq!(q.iter().map(|q| q.optional).collect::<Vec<_>>(), vec![false, true, true, true]);
        assert!(q[0].free_text && q[0].choices.is_empty() && !q[0].multi);
        assert_eq!(q[1].text, "Age (integer, 0–150)");
        assert!(q[1].free_text);
        assert_eq!(q[2].text, "Subscribe to updates");
        assert!(!q[2].free_text);
        assert_eq!(
            q[2].choices.iter().map(|c| (c.id.as_str(), c.label.as_str())).collect::<Vec<_>>(),
            vec![("true", "Yes"), ("false", "No")]
        );
        assert_eq!(q[3].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["red", "green", "blue"]);
        assert!(!q[3].free_text);
        assert_eq!(n.kind, PermissionKind::Question);
        assert_eq!(n.elicitation.as_ref().unwrap().fields.len(), 4);
        // 没有 `mode` 的表单请求（老 server）同样是表单
        let mut r = form_request();
        r.as_object_mut().unwrap().remove("mode");
        assert_eq!(from_elicitation("e1", &r).unwrap().0.kind, PermissionKind::Question);
    }

    #[test]
    fn elicitation_choices_come_from_enum_names_one_of_and_array_items() {
        let r = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m", "requested_schema": {"type": "object", "properties": {
            "legacy": {"type": "string", "enum": ["a", "b"], "enumNames": ["Option A", "Option B"]},
            "titled": {"type": "string", "oneOf": [{"const": "x", "title": "Ex"}, {"const": "y", "title": "Why"}]},
            "many": {"type": "array", "title": "Pick some", "items": {"type": "string", "enum": ["p", "q", "r"]}},
            "level": {"type": "number", "enum": [1, 2.5]},
            "ratio": {"type": "number", "minimum": 0.5},
        }, "required": ["legacy", "many"]},
            "property_order": ["legacy", "titled", "many", "level", "ratio"]});
        let (p, n) = from_elicitation("e", &r).unwrap();
        let q = &p.detail.questions;
        assert_eq!(
            q.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
            vec!["legacy", "titled", "many", "level", "ratio"]
        );
        let labels = |i: usize| q[i].choices.iter().map(|c| (c.id.as_str(), c.label.as_str())).collect::<Vec<_>>();
        assert_eq!(labels(0), vec![("a", "Option A"), ("b", "Option B")]);
        assert_eq!(labels(1), vec![("x", "Ex"), ("y", "Why")]);
        assert_eq!(labels(2), vec![("p", "p"), ("q", "q"), ("r", "r")]);
        assert!(q[2].multi && !q[2].free_text);
        assert_eq!(q[2].text, "Pick some");
        assert_eq!(q.iter().map(|q| q.optional).collect::<Vec<_>>(), vec![false, true, false, true, true]);
        assert_eq!(labels(3), vec![("1", "1"), ("2.5", "2.5")]);
        assert_eq!(q[4].text, "ratio (number, at least 0.5)");

        // 没有 `property_order` 时每个属性仍然都在（顺序取决于 JSON 对象，不保证）
        let mut no_order = r.clone();
        no_order.as_object_mut().unwrap().remove("property_order");
        let (p2, _) = from_elicitation("e", &no_order).unwrap();
        let mut ids: Vec<_> = p2.detail.questions.iter().map(|q| q.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["legacy", "level", "many", "ratio", "titled"]);

        // 答案回到 server 要的原值：数字枚举回数字，多选回数组
        let d = answers(&[
            ("legacy", json!("b")),
            ("level", json!("2.5")),
            ("many", json!(["p", "r"])),
            ("titled", json!("y")),
        ]);
        let reply = elicitation_reply(n.elicitation.as_ref().unwrap(), &d).unwrap();
        assert_eq!(reply.problem, None);
        assert_eq!(
            reply.response,
            json!({"action": "accept", "content": {"legacy": "b", "level": 2.5, "many": ["p", "r"], "titled": "y"}})
        );
    }

    #[test]
    fn elicitation_answers_are_converted_to_the_schema_types() {
        let (_, n) = from_elicitation("e1", &form_request()).unwrap();
        let spec = n.elicitation.as_ref().unwrap();
        // 和录制时 CLI 收下并原样转给 MCP server 的那条回复一致
        let reply = elicitation_reply(
            spec,
            &answers(&[
                ("name", json!("Ada")),
                ("age", json!("36")),
                ("subscribe", json!("true")),
                ("color", json!("green")),
            ]),
        )
        .unwrap();
        assert_eq!(
            reply.response,
            json!({"action": "accept", "content": {"name": "Ada", "age": 36, "subscribe": true, "color": "green"}})
        );
        assert_eq!(reply.problem, None);
        // 可选字段留空就不出现在 content 里
        let reply = elicitation_reply(spec, &answers(&[("name", json!(" Ada ")), ("age", json!(""))])).unwrap();
        assert_eq!(reply.response, json!({"action": "accept", "content": {"name": "Ada"}}));
        // 没给 option_id 但带了 answers：当作提交
        let d = PermissionDecision { answers: answers(&[("name", json!("Ada"))]).answers, ..Default::default() };
        assert_eq!(elicitation_reply(spec, &d).unwrap().response["action"], "accept");
    }

    #[test]
    fn elicitation_answers_that_do_not_fit_are_declined_with_a_reason() {
        let (_, n) = from_elicitation("e1", &form_request()).unwrap();
        let spec = n.elicitation.as_ref().unwrap();
        for (a, why) in [
            (answers(&[("name", json!("Ada")), ("age", json!("thirty"))]), "\"Age\" must be an integer"),
            (
                answers(&[("name", json!("Ada")), ("subscribe", json!("maybe"))]),
                "\"Subscribe to updates\" must be true or false",
            ),
            (
                answers(&[("name", json!("Ada")), ("color", json!("purple"))]),
                "\"Favorite color\": \"purple\" is not one of the allowed values",
            ),
            (answers(&[("age", json!("3"))]), "\"Name\" is required"),
            (answers(&[]), "\"Name\" is required"),
        ] {
            // 请求在 daemon 那边已经算答复过：回 decline，把原因交给调用方报给用户，而不是留着未决
            let reply = elicitation_reply(spec, &a).unwrap();
            assert_eq!(reply.response, json!({"action": "decline"}), "{why}");
            let problem = reply.problem.unwrap_or_default();
            assert!(problem.contains(why), "{problem}");
        }
    }

    #[test]
    fn unanswered_optional_fields_are_left_out_of_the_content_and_required_ones_still_have_to_be_answered() {
        let r = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m", "requested_schema": {"type": "object", "properties": {
            "must": {"type": "string"},
            "nick": {"type": "string"},
            "age": {"type": "integer"},
            "agree": {"type": "boolean"},
            "color": {"type": "string", "enum": ["red", "blue"]},
            "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}},
        }, "required": ["must"]}, "property_order": ["must", "nick", "age", "agree", "color", "tags"]});
        let (p, n) = from_elicitation("e", &r).unwrap();
        assert_eq!(
            p.detail.questions.iter().map(|q| (q.id.as_str(), q.optional)).collect::<Vec<_>>(),
            vec![("must", false), ("nick", true), ("age", true), ("agree", true), ("color", true), ("tags", true)]
        );
        let spec = n.elicitation.as_ref().unwrap();
        // 只答了必填的；客户端留空的可选项（空串、空数组）也省略，而不是当成非法答案
        let reply = elicitation_reply(
            spec,
            &answers(&[("must", json!("x")), ("nick", json!("")), ("age", json!("  ")), ("tags", json!([]))]),
        )
        .unwrap();
        assert_eq!((reply.response, reply.problem), (json!({"action": "accept", "content": {"must": "x"}}), None));
        // 回答了的可选项照常转换类型
        let reply =
            elicitation_reply(spec, &answers(&[("must", json!("x")), ("agree", json!("false")), ("age", json!("7"))]))
                .unwrap();
        assert_eq!(reply.response, json!({"action": "accept", "content": {"must": "x", "age": 7, "agree": false}}));
        // 必填的没答仍然不行
        let reply = elicitation_reply(spec, &answers(&[("nick", json!("n"))])).unwrap();
        assert_eq!(reply.response, json!({"action": "decline"}));
        assert!(reply.problem.unwrap().contains("\"must\" is required"));

        // 一个字段都没有必填：什么都不答也能提交
        let all_optional = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m",
            "requested_schema": {"type": "object", "properties": {"nick": {"type": "string"}}}});
        let (p, n) = from_elicitation("e", &all_optional).unwrap();
        assert!(p.detail.questions.iter().all(|q| q.optional));
        let accept = PermissionDecision { option_id: Some(OPT_ACCEPT.into()), ..Default::default() };
        let reply = elicitation_reply(n.elicitation.as_ref().unwrap(), &accept).unwrap();
        assert_eq!((reply.response, reply.problem), (json!({"action": "accept", "content": {}}), None));
    }

    #[test]
    fn elicitation_decline_and_unknown_options() {
        let (_, n) = from_elicitation("e1", &form_request()).unwrap();
        let spec = n.elicitation.as_ref().unwrap();
        for d in [
            // 用户拒绝；即使带着答案也是拒绝（选项优先）
            PermissionDecision { option_id: Some(OPT_DECLINE.into()), ..Default::default() },
            PermissionDecision {
                option_id: Some(OPT_DECLINE.into()),
                answers: answers(&[("name", json!("Ada"))]).answers,
                ..Default::default()
            },
            // 什么都没选：不会误放行
            PermissionDecision::default(),
        ] {
            let reply = elicitation_reply(spec, &d).unwrap();
            assert_eq!((reply.response, reply.problem), (json!({"action": "decline"}), None));
        }
        let bogus = PermissionDecision { option_id: Some("zzz".into()), ..Default::default() };
        assert!(elicitation_reply(spec, &bogus).is_err());
    }

    #[test]
    fn elicitation_url_mode_is_an_other_permission_with_the_link() {
        // 实录（`real_elicitation_url.jsonl`）里的请求
        let r = json!({"subtype": "elicitation", "mcp_server_name": "example-elicit", "message": "Sign in to Example to continue",
            "mode": "url", "url": "https://example.com/signin?state=abc123", "elicitation_id": "elic-1"});
        let (p, n) = from_elicitation("e2", &r).unwrap();
        assert_eq!(p.kind, PermissionKind::Other);
        assert_eq!(p.title, "example-elicit needs you to open a link");
        assert_eq!(p.detail.text.as_deref(), Some("Sign in to Example to continue"));
        assert_eq!(p.detail.url.as_deref(), Some("https://example.com/signin?state=abc123"));
        assert_eq!(
            p.options.iter().map(|o| (o.id.as_str(), o.label.as_str(), o.kind)).collect::<Vec<_>>(),
            vec![
                ("accept", "I've completed it", PermissionOptionKind::AllowOnce),
                ("decline", "Decline", PermissionOptionKind::RejectOnce),
            ]
        );
        let spec = n.elicitation.as_ref().unwrap();
        // 录制时 CLI 收下的就是 `{action: accept}`（不带 content）
        let accept = PermissionDecision { option_id: Some(OPT_ACCEPT.into()), ..Default::default() };
        assert_eq!(elicitation_reply(spec, &accept).unwrap().response, json!({"action": "accept"}));
        let no = PermissionDecision { option_id: Some(OPT_DECLINE.into()), ..Default::default() };
        assert_eq!(elicitation_reply(spec, &no).unwrap().response, json!({"action": "decline"}));
        // 没写 mode 但带了 url：URL 模式；大小写不敏感的 scheme 也认
        let r = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "", "url": "HTTP://example.com/x"});
        let (p, _) = from_elicitation("e3", &r).unwrap();
        assert_eq!((p.kind, p.detail.url.as_deref()), (PermissionKind::Other, Some("HTTP://example.com/x")));
        assert!(p.detail.text.as_deref().unwrap().contains("browser"));
    }

    #[test]
    fn elicitation_url_must_be_http_or_https() {
        for url in [
            "javascript:alert(1)",
            "file:///etc/passwd",
            "data:text/html,x",
            "ftp://example.com",
            "//example.com",
            "example.com",
            "",
            "https://a\u{7}b",
        ] {
            let r =
                json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m", "mode": "url", "url": url});
            let err = from_elicitation("e", &r).unwrap_err();
            assert!(err.contains("not an http(s) URL"), "{url:?}: {err}");
        }
        // 没有 url 的 URL 模式请求同样不能交给用户
        let r = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m", "mode": "url"});
        assert!(from_elicitation("e", &r).is_err());
    }

    #[test]
    fn elicitation_without_a_schema_is_an_empty_form() {
        let r = json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "Continue?"});
        let (p, n) = from_elicitation("e", &r).unwrap();
        assert_eq!((p.kind, p.detail.questions.len()), (PermissionKind::Question, 0));
        assert_eq!(p.title, "s: Continue?");
        let reply = elicitation_reply(n.elicitation.as_ref().unwrap(), &answers(&[])).unwrap();
        assert_eq!(reply.response, json!({"action": "accept", "content": {}}));
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
