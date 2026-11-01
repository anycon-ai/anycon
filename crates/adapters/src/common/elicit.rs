//! 表单式 elicitation（MCP 的 `elicitation/create` 一族）的转换，Codex 的 MCP elicitation 和 ACP 的 `elicitation/create` 共用：
//!
//! - [`field_from_schema`]：schema 里的一个属性 → 一道题（[`Question`]）+ 答复时用的类型信息（[`ElicitField`]）；
//! - [`convert_answers`]：用户的回答 → 按各属性类型转成的 JSON 值（必填缺失、类型不对、不在候选里、超出范围都是 `Err(说明)`）；
//! - [`is_safe_http_url`]：URL 模式的链接只接受 http / https。
//!
//! MCP server（或者经 agent 转来的）给的文字 / 候选不可信，所以有长度和个数上限。

use anycon_protocol::{Question, QuestionChoice};
use serde_json::{Map, Value};

/// 表单里的一个属性：答复时按它的类型把用户的回答转成 JSON 值。
#[derive(Debug, Clone, PartialEq)]
pub struct ElicitField {
    pub key: String,
    /// 给用户看的名字（`title`，缺省用属性名），出错说明里用。
    pub label: String,
    pub required: bool,
    pub ty: FieldType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Text,
    Number {
        integer: bool,
        min: Option<f64>,
        max: Option<f64>,
    },
    Boolean,
    /// 单选，值是 `enum` / `oneOf[].const`。
    Choice(Vec<String>),
    /// 多选（`type: array`），值是 `items.enum` / `items.anyOf[].const`。
    Choices(Vec<String>),
}

pub fn answer_strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
        Value::String(s) => vec![s.clone()],
        Value::Null => vec![],
        other => vec![other.to_string()],
    }
}

/// 一个表单最多几个属性 / 一个枚举最多几个候选 / 说明文字的上限：MCP server 给的东西不可信，不能让它撑爆请求。
pub const MAX_ELICIT_FIELDS: usize = 50;
pub const MAX_ELICIT_CHOICES: usize = 100;
pub const MAX_ELICIT_TEXT_BYTES: usize = 2000;

pub fn clip(s: &str) -> String {
    crate::common::truncate_bytes(s, MAX_ELICIT_TEXT_BYTES)
}

/// 只接受 http / https，且不含空白和控制字符：这个 URL 会原样显示给用户并提供「打开」入口。
pub fn is_safe_http_url(u: &str) -> bool {
    let lower = u.to_ascii_lowercase();
    let rest = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty()) && u.len() <= 8192 && !u.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn number_hint(integer: bool, min: Option<f64>, max: Option<f64>) -> String {
    let kind = if integer { "an integer" } else { "a number" };
    match (min, max) {
        (Some(a), Some(b)) => format!("Enter {kind} between {} and {}.", fmt_num(a), fmt_num(b)),
        (Some(a), None) => format!("Enter {kind}, at least {}.", fmt_num(a)),
        (None, Some(b)) => format!("Enter {kind}, at most {}.", fmt_num(b)),
        (None, None) => format!("Enter {kind}."),
    }
}

/// 单选 / 多选的候选：`oneOf: [{ const, title }]`、`anyOf: [{ const, title }]`、`enum: [..]`（+ `enumNames`）。
fn schema_choices(v: &Value) -> Option<Vec<QuestionChoice>> {
    let titled = |arr: &Vec<Value>| -> Vec<QuestionChoice> {
        arr.iter()
            .filter_map(|o| {
                let id = o.get("const").and_then(Value::as_str)?;
                Some(QuestionChoice { id: id.to_string(), label: str_of(o, "title").unwrap_or(id).to_string() })
            })
            .collect()
    };
    for key in ["oneOf", "anyOf"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            let mut c = titled(arr);
            if !c.is_empty() {
                c.truncate(MAX_ELICIT_CHOICES);
                return Some(c);
            }
        }
    }
    let values = v.get("enum").and_then(Value::as_array)?;
    let names = v.get("enumNames").and_then(Value::as_array);
    let c: Vec<QuestionChoice> = values
        .iter()
        .enumerate()
        .filter_map(|(i, x)| {
            let id = x.as_str()?;
            let label = names.and_then(|n| n.get(i)).and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or(id);
            Some(QuestionChoice { id: id.to_string(), label: label.to_string() })
        })
        .take(MAX_ELICIT_CHOICES)
        .collect();
    (!c.is_empty()).then_some(c)
}

/// 表单的一个属性 → 一道题 + 答复时用的类型信息。
pub fn field_from_schema(key: &str, prop: &Value, required: bool) -> (ElicitField, Question) {
    let label = crate::common::truncate_bytes(str_of(prop, "title").unwrap_or(key), 200);
    let ty_name = str_of(prop, "type").unwrap_or("string");
    let mut hint: Option<String> = None;
    let (ty, choices, multi) = if ty_name == "array" {
        match prop.get("items").and_then(schema_choices) {
            Some(c) => (FieldType::Choices(c.iter().map(|c| c.id.clone()).collect()), c, true),
            None => (FieldType::Text, Vec::new(), false),
        }
    } else if let Some(c) = schema_choices(prop) {
        (FieldType::Choice(c.iter().map(|c| c.id.clone()).collect()), c, false)
    } else if ty_name == "boolean" {
        let c = vec![
            QuestionChoice { id: "true".into(), label: "Yes".into() },
            QuestionChoice { id: "false".into(), label: "No".into() },
        ];
        (FieldType::Boolean, c, false)
    } else if ty_name == "number" || ty_name == "integer" {
        let integer = ty_name == "integer";
        let (min, max) = (prop.get("minimum").and_then(Value::as_f64), prop.get("maximum").and_then(Value::as_f64));
        hint = Some(number_hint(integer, min, max));
        (FieldType::Number { integer, min, max }, Vec::new(), false)
    } else {
        (FieldType::Text, Vec::new(), false)
    };
    let mut text = label.clone();
    for extra in [str_of(prop, "description").map(clip), hint].into_iter().flatten() {
        text.push('\n');
        text.push_str(&extra);
    }
    let free_text = choices.is_empty();
    (
        ElicitField { key: key.to_string(), label, required, ty },
        // schema 没列进 `required` 的属性可以留空：答复时直接省略（见 `convert_answers`）
        Question { id: key.to_string(), text, choices, multi, free_text, optional: !required },
    )
}

/// 把用户对表单的回答按各属性的类型转成 `content`。缺失的非必填项直接省略；必填项缺失、类型不对、不在候选里、
/// 超出范围都是 `Err(说明)`。
pub fn convert_answers(
    fields: &[ElicitField],
    answers: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>, String> {
    fn scalar(v: &Value) -> Option<String> {
        match v {
            Value::String(s) => Some(s.trim().to_string()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            // 只选了一项的数组按那一项算
            Value::Array(a) if a.len() == 1 => scalar(&a[0]),
            _ => None,
        }
    }
    let mut out = Map::new();
    for f in fields {
        let raw = answers.and_then(|a| a.get(&f.key)).filter(|v| match v {
            Value::Null => false,
            Value::String(s) => !s.trim().is_empty(),
            Value::Array(a) => !a.is_empty(),
            _ => true,
        });
        let Some(raw) = raw else {
            if f.required {
                return Err(format!("\"{}\" is required but was not answered", f.label));
            }
            continue;
        };
        let bad = |why: &str| format!("the answer for \"{}\" {why}", f.label);
        let value = match &f.ty {
            FieldType::Text => match raw {
                Value::String(s) => Value::String(s.clone()),
                other => Value::String(scalar(other).ok_or_else(|| bad("is not text"))?),
            },
            FieldType::Number { integer, min, max } => {
                let s = scalar(raw).ok_or_else(|| bad("is not a number"))?;
                let n: f64 = if *integer {
                    s.parse::<i64>().map_err(|_| bad("is not an integer"))? as f64
                } else {
                    s.parse::<f64>().ok().filter(|n| n.is_finite()).ok_or_else(|| bad("is not a number"))?
                };
                if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                    return Err(bad("is out of range"));
                }
                if *integer {
                    Value::from(n as i64)
                } else {
                    serde_json::Number::from_f64(n).map(Value::Number).ok_or_else(|| bad("is not a number"))?
                }
            }
            FieldType::Boolean => match scalar(raw).map(|s| s.to_ascii_lowercase()).as_deref() {
                Some("true") => Value::Bool(true),
                Some("false") => Value::Bool(false),
                _ => return Err(bad("is not true or false")),
            },
            FieldType::Choice(allowed) => {
                let s = scalar(raw).ok_or_else(|| bad("is not one of the choices"))?;
                if !allowed.contains(&s) {
                    return Err(bad("is not one of the choices"));
                }
                Value::String(s)
            }
            FieldType::Choices(allowed) => {
                let picked = answer_strings(raw);
                if picked.iter().any(|s| !allowed.contains(s)) {
                    return Err(bad("has a value that is not one of the choices"));
                }
                Value::Array(picked.into_iter().map(Value::String).collect())
            }
        };
        out.insert(f.key.clone(), value);
    }
    Ok(out)
}
