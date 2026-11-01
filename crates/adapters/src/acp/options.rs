//! 会话选项：agent 在 `session/new`（`session/load`、`session/resume`）响应里给的 `configOptions` / `modes`，
//! 和 `AgentInfo` 的 models / modes / options 之间的转换（docs/spec/daemon.md § 5.4、§ 6.3）。
//!
//! - `category = model` 的 select → 模型列表（分组的取组名作 tag）；**没有就不提供模型选择**（旧的 `models` / `session/set_model`
//!   不在 v1 稳定 schema 里，不支持）
//! - `category = thought_level` 的 select → 每个模型的思考等级（ACP 的等级不按模型区分，探测时看到的是默认模型的那一组，
//!   所有模型共用；换模型后 agent 可能给出别的等级，那时再设会被 agent 拒绝）
//! - `category = mode` 的 select → 权限模式；没有就用旧的 `modes.availableModes`（`session/set_mode`）。ACP 不给风险等级，
//!   全部 `normal`，用户用 `dangerous_modes` 标记
//! - 其余的 select / boolean → 通用 `ConfigOption`（`model_config` 和自定义类别都在这里）；别的 `type` 忽略
//!
//! 同一个类别有多个选项时取排在前面的（规格要求按数组顺序决定）。每个会话有自己的 [`ConfigState`]（agent 随时会用
//! `config_option_update` / `current_mode_update` 改它）；设置值时按它里面当前的选项 id 找。

use anycon_protocol::{Choice, ConfigOption, ConfigOptionKind, ModeInfo, ModeRisk, ModelInfo, ThinkingOptions};
use serde_json::Value;

pub const CATEGORY_MODE: &str = "mode";
pub const CATEGORY_MODEL: &str = "model";
pub const CATEGORY_THOUGHT: &str = "thought_level";

#[derive(Debug, Clone, PartialEq)]
pub struct LiveChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
    /// 分组选项所在的组名。
    pub group: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveKind {
    Select(Vec<LiveChoice>),
    Boolean,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub kind: LiveKind,
    /// select 是字符串，boolean 是布尔。
    pub current: Value,
}

impl LiveOption {
    pub fn choices(&self) -> &[LiveChoice] {
        match &self.kind {
            LiveKind::Select(c) => c,
            LiveKind::Boolean => &[],
        }
    }

    pub fn is_select(&self) -> bool {
        matches!(self.kind, LiveKind::Select(_))
    }

    /// 把设置里的值转成这个选项要的类型并校验：select 要是字符串且在候选里，boolean 要是布尔。
    pub fn coerce(&self, v: &Value) -> Result<Value, String> {
        match &self.kind {
            LiveKind::Boolean => v.as_bool().map(Value::Bool).ok_or_else(|| "expected true or false".to_string()),
            LiveKind::Select(choices) => {
                let s = v.as_str().ok_or_else(|| "expected a string".to_string())?;
                if choices.is_empty() || choices.iter().any(|c| c.value == s) {
                    Ok(Value::String(s.to_string()))
                } else {
                    Err(format!(
                        "{s} is not one of: {}",
                        choices.iter().map(|c| c.value.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LiveMode {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// 设模式走哪条路。
#[derive(Debug, Clone, PartialEq)]
pub enum ModeTarget {
    /// `session/set_config_option`，值是模式 id（`category = mode` 的那个选项的 id）。
    ConfigOption(String),
    /// 旧的 `session/set_mode`。
    Legacy,
}

/// 一个会话里 agent 当前声明的选项。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigState {
    pub options: Vec<LiveOption>,
    /// 旧的 `modes.availableModes`。
    pub modes: Vec<LiveMode>,
    pub current_mode: Option<String>,
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn parse_choice(v: &Value, group: Option<&str>) -> Option<LiveChoice> {
    let value = v.get("value")?.as_str()?.to_string();
    let name = non_empty(v.get("name")).unwrap_or_else(|| value.clone());
    Some(LiveChoice { value, name, description: non_empty(v.get("description")), group: group.map(str::to_string) })
}

/// `options`：平铺的 `[{value, name}]`，或者分组的 `[{group, name, options: [...]}]`（两种不会混在一个数组里）。
fn parse_choices(options: &Value) -> Vec<LiveChoice> {
    let mut out = Vec::new();
    for item in options.as_array().map(Vec::as_slice).unwrap_or_default() {
        match item.get("options").and_then(Value::as_array) {
            Some(inner) => {
                let group = non_empty(item.get("name")).or_else(|| non_empty(item.get("group")));
                out.extend(inner.iter().filter_map(|c| parse_choice(c, group.as_deref())));
            }
            None => out.extend(parse_choice(item, None)),
        }
    }
    out
}

fn parse_option(v: &Value) -> Option<LiveOption> {
    let id = non_empty(v.get("id"))?;
    let name = non_empty(v.get("name")).unwrap_or_else(|| id.clone());
    let current = v.get("currentValue")?.clone();
    // `type` 缺省按 select；不认识的类型忽略（规格：收到不认识的 type 就当没有这个选项）
    let kind = match v.get("type").and_then(Value::as_str) {
        Some("boolean") if current.is_boolean() => LiveKind::Boolean,
        Some("select") | None if current.is_string() => LiveKind::Select(parse_choices(v.get("options")?)),
        _ => return None,
    };
    Some(LiveOption {
        id,
        name,
        description: non_empty(v.get("description")),
        category: non_empty(v.get("category")),
        kind,
        current,
    })
}

impl ConfigState {
    /// 从 `session/new` / `session/load` / `session/resume` 的响应取。
    pub fn from_response(result: &Value) -> Self {
        let mut s = ConfigState::default();
        if let Some(arr) = result.get("configOptions") {
            s.replace_options(arr);
        }
        if let Some(m) = result.get("modes").filter(|m| m.is_object()) {
            s.current_mode = non_empty(m.get("currentModeId"));
            s.modes = m
                .get("availableModes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            let id = non_empty(x.get("id"))?;
                            Some(LiveMode {
                                name: non_empty(x.get("name")).unwrap_or_else(|| id.clone()),
                                description: non_empty(x.get("description")),
                                id,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        s
    }

    /// 整份换掉选项（`set_config_option` 的响应和 `config_option_update` 都给**完整**的列表）。不是数组 = 不动。
    pub fn replace_options(&mut self, v: &Value) {
        if let Some(arr) = v.as_array() {
            self.options = arr.iter().filter_map(parse_option).collect();
            // 模式选项和旧的 modes 保持一致
            if let Some(cur) = self.mode_option().and_then(|o| o.current.as_str().map(str::to_string)) {
                self.current_mode = Some(cur);
            }
        }
    }

    /// agent 自己切了模式（`current_mode_update`）。
    pub fn set_current_mode(&mut self, id: &str) {
        self.current_mode = Some(id.to_string());
        if let Some(opt) =
            self.options.iter_mut().find(|o| o.category.as_deref() == Some(CATEGORY_MODE) && o.is_select())
        {
            if opt.choices().iter().any(|c| c.value == id) {
                opt.current = Value::String(id.to_string());
            }
        }
    }

    pub fn option(&self, id: &str) -> Option<&LiveOption> {
        self.options.iter().find(|o| o.id == id)
    }

    /// 某个类别里排在最前面的 select。
    pub fn category(&self, category: &str) -> Option<&LiveOption> {
        self.options.iter().find(|o| o.category.as_deref() == Some(category) && o.is_select())
    }

    pub fn model_option(&self) -> Option<&LiveOption> {
        self.category(CATEGORY_MODEL)
    }

    pub fn thought_option(&self) -> Option<&LiveOption> {
        self.category(CATEGORY_THOUGHT)
    }

    pub fn mode_option(&self) -> Option<&LiveOption> {
        self.category(CATEGORY_MODE)
    }

    pub fn current_model(&self) -> Option<String> {
        self.model_option().and_then(|o| o.current.as_str().map(str::to_string))
    }

    /// 设模式走哪条路：有 `category = mode` 的选项用它，否则用旧的 `modes`；两者都没有 = 不能设。
    pub fn mode_target(&self) -> Option<ModeTarget> {
        if let Some(o) = self.mode_option() {
            Some(ModeTarget::ConfigOption(o.id.clone()))
        } else if !self.modes.is_empty() {
            Some(ModeTarget::Legacy)
        } else {
            None
        }
    }

    /// 当前模式 id。
    pub fn current_mode_id(&self) -> Option<String> {
        match self.mode_option() {
            Some(o) => o.current.as_str().map(str::to_string),
            None => self.current_mode.clone(),
        }
    }

    /// 可选的模式（模式选项优先，其次旧的 `modes`）。
    pub fn mode_choices(&self) -> Vec<LiveMode> {
        match self.mode_option() {
            Some(o) => o
                .choices()
                .iter()
                .map(|c| LiveMode { id: c.value.clone(), name: c.name.clone(), description: c.description.clone() })
                .collect(),
            None => self.modes.clone(),
        }
    }

    /// 不是模型 / 思考等级 / 模式选择器的其余选项（取 [`ConfigOption`] 的通用控件渲染）。
    fn extra(&self) -> impl Iterator<Item = &LiveOption> {
        let special: Vec<&str> = [self.model_option(), self.thought_option(), self.mode_option()]
            .into_iter()
            .flatten()
            .map(|o| o.id.as_str())
            .collect();
        self.options.iter().filter(move |o| !special.contains(&o.id.as_str()))
    }
}

fn choice(c: &LiveChoice) -> Choice {
    Choice { id: c.value.clone(), label: c.name.clone(), description: c.description.clone() }
}

/// 模型列表。思考等级来自 `thought_level` 选项，所有模型共用（见模块说明）。
pub fn models(state: &ConfigState) -> Vec<ModelInfo> {
    let Some(opt) = state.model_option() else { return Vec::new() };
    let thinking = state.thought_option().filter(|t| !t.choices().is_empty()).map(|t| ThinkingOptions {
        options: t.choices().iter().map(choice).collect(),
        default: t.current.as_str().map(str::to_string),
    });
    let current = opt.current.as_str().unwrap_or_default();
    opt.choices()
        .iter()
        .map(|c| ModelInfo {
            id: c.value.clone(),
            label: c.name.clone(),
            description: c.description.clone(),
            default: c.value == current,
            thinking: thinking.clone(),
            tags: c.group.clone().into_iter().collect(),
        })
        .collect()
}

/// 权限模式。ACP 不给风险等级：全部 `normal`，`dangerous_modes` 由 daemon 叠加。
pub fn modes(state: &ConfigState) -> Vec<ModeInfo> {
    let current = state.current_mode_id();
    state
        .mode_choices()
        .into_iter()
        .map(|m| ModeInfo {
            default: current.as_deref() == Some(m.id.as_str()),
            id: m.id,
            label: m.name,
            description: m.description,
            risk: ModeRisk::Normal,
        })
        .collect()
}

/// 其余的选项。
pub fn options(state: &ConfigState) -> Vec<ConfigOption> {
    state
        .extra()
        .map(|o| ConfigOption {
            id: o.id.clone(),
            label: o.name.clone(),
            description: o.description.clone(),
            kind: if o.is_select() { ConfigOptionKind::Select } else { ConfigOptionKind::Bool },
            choices: o.choices().iter().map(choice).collect(),
            default: o.current.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "sessionId": "s1",
            "configOptions": [
                { "id": "mode", "name": "Session Mode", "category": "mode", "type": "select", "currentValue": "ask",
                  "options": [ { "value": "ask", "name": "Ask", "description": "Ask first" }, { "value": "code", "name": "Code" } ] },
                { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "m2",
                  "options": [
                    { "group": "fast", "name": "Fast", "options": [ { "value": "m1", "name": "Model 1" } ] },
                    { "group": "big", "name": "Big", "options": [ { "value": "m2", "name": "Model 2", "description": "The big one" } ] }
                  ] },
                { "id": "effort", "name": "Reasoning", "category": "thought_level", "type": "select", "currentValue": "medium",
                  "options": [ { "value": "low", "name": "Low" }, { "value": "medium", "name": "Medium" }, { "value": "high", "name": "High" } ] },
                { "id": "ctx", "name": "Context", "category": "model_config", "type": "select", "currentValue": "200k",
                  "options": [ { "value": "200k", "name": "200K" }, { "value": "1m", "name": "1M" } ] },
                { "id": "brave", "name": "Brave", "description": "Skip prompts", "type": "boolean", "currentValue": false },
                { "id": "weird", "name": "Weird", "type": "slider", "currentValue": 3 },
                { "id": "custom", "name": "Custom", "category": "_x", "currentValue": "a", "options": [ { "value": "a", "name": "A" } ] }
            ]
        })
    }

    #[test]
    fn config_options_become_models_modes_and_options() {
        let st = ConfigState::from_response(&sample());
        assert_eq!(st.options.len(), 6, "the slider is ignored");
        let models = models(&st);
        assert_eq!(
            models.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
            vec![("m1", false), ("m2", true)]
        );
        assert_eq!(models[1].label, "Model 2");
        assert_eq!(models[1].description.as_deref(), Some("The big one"));
        assert_eq!(models[0].tags, vec!["Fast"]);
        let th = models[0].thinking.as_ref().unwrap();
        assert_eq!(th.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "medium", "high"]);
        assert_eq!(th.default.as_deref(), Some("medium"));
        assert_eq!(models[0].thinking, models[1].thinking, "the level list is shared by every model");

        let modes = modes(&st);
        assert_eq!(
            modes.iter().map(|m| (m.id.as_str(), m.default, m.risk)).collect::<Vec<_>>(),
            vec![("ask", true, ModeRisk::Normal), ("code", false, ModeRisk::Normal)]
        );
        assert_eq!(modes[0].description.as_deref(), Some("Ask first"));
        assert_eq!(st.mode_target(), Some(ModeTarget::ConfigOption("mode".into())));

        let opts = options(&st);
        assert_eq!(opts.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["ctx", "brave", "custom"]);
        assert_eq!(opts[0].kind, ConfigOptionKind::Select);
        assert_eq!(opts[0].default, json!("200k"));
        assert_eq!(opts[0].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["200k", "1m"]);
        assert_eq!((opts[1].kind, opts[1].default.clone()), (ConfigOptionKind::Bool, json!(false)));
        assert_eq!(opts[1].description.as_deref(), Some("Skip prompts"));
        // `type` 缺省按 select
        assert_eq!(opts[2].kind, ConfigOptionKind::Select);
    }

    #[test]
    fn without_a_model_option_there_is_no_model_choice() {
        let st = ConfigState::from_response(&json!({
            "sessionId": "s",
            "models": { "currentModelId": "x", "availableModels": [ { "modelId": "x", "name": "X" } ] }
        }));
        assert!(models(&st).is_empty());
        assert!(modes(&st).is_empty() && options(&st).is_empty());
        assert_eq!(st.mode_target(), None);
    }

    #[test]
    fn legacy_modes_are_used_when_there_is_no_mode_option() {
        let st = ConfigState::from_response(&json!({
            "sessionId": "s",
            "modes": { "currentModeId": "architect", "availableModes": [
                { "id": "ask", "name": "Ask" }, { "id": "architect", "name": "Architect", "description": "Plan" } ] }
        }));
        let m = modes(&st);
        assert_eq!(
            m.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
            vec![("ask", false), ("architect", true)]
        );
        assert_eq!(m[1].description.as_deref(), Some("Plan"));
        assert_eq!(st.mode_target(), Some(ModeTarget::Legacy));
        assert_eq!(st.current_mode_id().as_deref(), Some("architect"));
    }

    #[test]
    fn the_mode_option_wins_over_legacy_modes_and_the_first_of_a_category_wins() {
        let st = ConfigState::from_response(&json!({
            "modes": { "currentModeId": "old", "availableModes": [ { "id": "old", "name": "Old" } ] },
            "configOptions": [
                { "id": "m_a", "name": "A", "category": "mode", "type": "select", "currentValue": "x", "options": [ { "value": "x", "name": "X" } ] },
                { "id": "m_b", "name": "B", "category": "mode", "type": "select", "currentValue": "y", "options": [ { "value": "y", "name": "Y" } ] }
            ]
        }));
        assert_eq!(st.mode_target(), Some(ModeTarget::ConfigOption("m_a".into())));
        assert_eq!(modes(&st).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["x"]);
        // 第二个 mode 选项不是选择器，退回通用选项
        assert_eq!(options(&st).iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["m_b"]);
    }

    #[test]
    fn updates_replace_the_options_and_keep_modes_in_sync() {
        let mut st = ConfigState::from_response(&sample());
        st.set_current_mode("code");
        assert_eq!(st.current_mode_id().as_deref(), Some("code"));
        assert_eq!(st.mode_option().unwrap().current, json!("code"));
        // 未知的模式 id 只记在旧字段里，不破坏选项
        st.set_current_mode("nope");
        assert_eq!(st.mode_option().unwrap().current, json!("code"));
        let mut update = sample()["configOptions"].clone();
        update[1]["currentValue"] = json!("m1");
        st.replace_options(&update);
        assert_eq!(st.current_model().as_deref(), Some("m1"));
        // 不是数组：不动
        st.replace_options(&json!(null));
        assert_eq!(st.current_model().as_deref(), Some("m1"));
    }

    #[test]
    fn values_are_coerced_to_the_option_type() {
        let st = ConfigState::from_response(&sample());
        let ctx = st.option("ctx").unwrap();
        assert_eq!(ctx.coerce(&json!("1m")), Ok(json!("1m")));
        assert!(ctx.coerce(&json!("2m")).unwrap_err().contains("200k"));
        assert!(ctx.coerce(&json!(true)).is_err());
        let brave = st.option("brave").unwrap();
        assert_eq!(brave.coerce(&json!(true)), Ok(json!(true)));
        assert!(brave.coerce(&json!("true")).is_err());
    }

    #[test]
    fn malformed_options_are_skipped_not_fatal() {
        let st = ConfigState::from_response(&json!({
            "configOptions": [
                "junk", { "name": "no id" }, { "id": "a", "name": "A", "type": "select", "currentValue": 5, "options": [] },
                { "id": "b", "name": "B", "type": "select", "currentValue": "v", "options": [ { "value": "v" }, { "name": "no value" } ] }
            ]
        }));
        assert_eq!(st.options.len(), 1);
        assert_eq!(st.options[0].choices().len(), 1);
        assert_eq!(st.options[0].choices()[0].name, "v", "a missing name falls back to the value");
    }
}
