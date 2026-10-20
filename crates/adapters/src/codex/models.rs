//! 模型表（docs/spec/daemon.md § 5.4）：**不内置任何模型 id**。
//!
//! 来源：`model/list`（跳过 `hidden`；`supportedReasoningEfforts` 就是该模型的思考等级，`defaultReasoningEffort` 为默认，
//! `isDefault` 为默认模型）→ `config/read` 里的 `model` / `model_reasoning_effort` 作为默认（列表里没有就追加一项）→
//! `config.agents.codex.additional_models`。Codex 的模型淘汰很快，官方文档也说以返回值为准。
//!
//! 追加的那项（常见于 `model_provider` 指向自定义提供方，`model/list` 只列官方模型）没有自己的等级信息：
//! 取列表里所有模型都支持的等级（交集），再并上 `model_reasoning_effort`，否则这个默认模型在 UI 上调不了思考等级。

use anycon_protocol::{Choice, ModelInfo, ThinkingOptions};

use super::process::check_effort;
use super::wire::{ConfigSummary, ModelEntry};

/// 思考等级的显示名。认识的给固定写法，不认识的首字母大写。
pub fn effort_label(id: &str) -> String {
    match id {
        "none" => "None".to_string(),
        "minimal" => "Minimal".to_string(),
        "low" => "Low".to_string(),
        "medium" => "Medium".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "Extra high".to_string(),
        "max" => "Max".to_string(),
        "ultra" => "Ultra".to_string(),
        other => {
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}

/// `model/list` 的一项 → [`ModelInfo`]。`hidden` 的由调用方先过滤。
pub fn model_info(e: &ModelEntry) -> ModelInfo {
    let id = e.model.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| e.id.clone());
    let options: Vec<Choice> = e
        .supported_reasoning_efforts
        .iter()
        .map(|o| Choice {
            id: o.reasoning_effort.clone(),
            label: effort_label(&o.reasoning_effort),
            description: o.description.clone().filter(|d| !d.is_empty()),
        })
        .collect();
    let thinking = if options.is_empty() {
        None
    } else {
        // 默认等级必须在选项里，否则取第一项
        let default = e
            .default_reasoning_effort
            .clone()
            .filter(|d| options.iter().any(|o| &o.id == d))
            .or_else(|| options.first().map(|o| o.id.clone()));
        Some(ThinkingOptions { options, default })
    };
    ModelInfo {
        label: e.display_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone()),
        id,
        description: e.description.clone().filter(|d| !d.is_empty()),
        default: e.is_default,
        thinking,
        tags: vec![],
    }
}

/// 过滤掉 hidden、按 id 去重（先到先得），保证至多一个默认。
pub fn from_list(entries: &[ModelEntry]) -> Vec<ModelInfo> {
    let mut out: Vec<ModelInfo> = Vec::new();
    for e in entries.iter().filter(|e| !e.hidden) {
        let m = model_info(e);
        if !out.iter().any(|x| x.id == m.id) {
            out.push(m);
        }
    }
    let mut seen_default = false;
    for m in &mut out {
        if m.default {
            if seen_default {
                m.default = false;
            }
            seen_default = true;
        }
    }
    out
}

/// 用户 `config.toml` 的 `model` / `model_reasoning_effort` 作为默认。列表里没有的模型追加一项，思考等级见 [`fallback_thinking`]。
pub fn apply_config(models: &mut Vec<ModelInfo>, cfg: &ConfigSummary) {
    let Some(model) = cfg.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else { return };
    let effort = cfg.model_reasoning_effort.as_deref().map(str::trim).filter(|e| !e.is_empty());
    if !models.iter().any(|m| m.id == model) {
        let thinking = fallback_thinking(models, effort);
        models.push(ModelInfo {
            id: model.to_string(),
            label: model.to_string(),
            description: Some("From the Codex config.toml (model)".to_string()),
            default: false,
            thinking,
            tags: vec![],
        });
    }
    for m in models.iter_mut() {
        m.default = m.id == model;
    }
    if let Some(effort) = effort {
        if let Some(t) = models.iter_mut().find(|m| m.id == model).and_then(|m| m.thinking.as_mut()) {
            if t.options.iter().any(|o| o.id == effort) {
                t.default = Some(effort.to_string());
            }
        }
    }
}

/// 不在 `model/list` 里的模型的思考等级：列表里**每个**带等级的模型都支持的那些（按第一个模型的顺序，沿用它的说明），
/// 再并上用户配置的 `model_reasoning_effort`（用户给这个模型配了它，应当可用）。都没有就 `None`。
/// 默认：配置的等级 → 原默认模型的默认等级（在选项里时）→ 第一项。
fn fallback_thinking(models: &[ModelInfo], effort: Option<&str>) -> Option<ThinkingOptions> {
    let listed: Vec<&ThinkingOptions> = models.iter().filter_map(|m| m.thinking.as_ref()).collect();
    let mut options: Vec<Choice> = match listed.split_first() {
        Some((first, rest)) => first
            .options
            .iter()
            .filter(|o| rest.iter().all(|t| t.options.iter().any(|x| x.id == o.id)))
            .cloned()
            .collect(),
        None => vec![],
    };
    if let Some(e) = effort {
        if check_effort(e).is_ok() && !options.iter().any(|o| o.id == e) {
            options.push(Choice { id: e.to_string(), label: effort_label(e), description: None });
        }
    }
    if options.is_empty() {
        return None;
    }
    let listed_default =
        models.iter().find(|m| m.default).and_then(|m| m.thinking.as_ref()).and_then(|t| t.default.clone());
    let default = effort
        .map(str::to_string)
        .or(listed_default)
        .filter(|d| options.iter().any(|o| &o.id == d))
        .or_else(|| options.first().map(|o| o.id.clone()));
    Some(ThinkingOptions { options, default })
}

/// 加上 `additional_models`（按 id 去重，不许抢默认）。
pub fn add_additional(models: &mut Vec<ModelInfo>, additional: &[ModelInfo]) {
    for m in additional {
        if !models.iter().any(|x| x.id == m.id) {
            let mut m = m.clone();
            m.default = false;
            models.push(m);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries() -> Vec<ModelEntry> {
        serde_json::from_value(json!([
            {"id": "m-big", "model": "m-big", "displayName": "Big", "description": "The big one", "hidden": false,
             "isDefault": true, "defaultReasoningEffort": "low",
             "supportedReasoningEfforts": [
                 {"reasoningEffort": "low", "description": "Fast"},
                 {"reasoningEffort": "xhigh", "description": ""},
                 {"reasoningEffort": "ultra", "description": "Delegates"}]},
            {"id": "m-small", "model": "m-small", "displayName": "Small", "description": "", "hidden": false,
             "isDefault": false, "defaultReasoningEffort": "medium",
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "medium"}]},
            {"id": "m-old", "model": "m-old", "displayName": "Old", "description": "", "hidden": true,
             "isDefault": false, "defaultReasoningEffort": "medium", "supportedReasoningEfforts": []},
            {"id": "m-none", "model": "m-none", "displayName": "NoEffort", "description": "", "hidden": false,
             "isDefault": false, "defaultReasoningEffort": "medium", "supportedReasoningEfforts": []}
        ]))
        .unwrap()
    }

    #[test]
    fn list_becomes_models_with_per_model_thinking() {
        let models = from_list(&entries());
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["m-big", "m-small", "m-none"]);
        let big = &models[0];
        assert!(big.default);
        assert_eq!(big.label, "Big");
        assert_eq!(big.description.as_deref(), Some("The big one"));
        let t = big.thinking.as_ref().unwrap();
        assert_eq!(t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "xhigh", "ultra"]);
        assert_eq!(t.options[1].label, "Extra high");
        assert_eq!(t.options[1].description, None);
        assert_eq!(t.options[2].description.as_deref(), Some("Delegates"));
        assert_eq!(t.default.as_deref(), Some("low"));
        // 小模型没有 ultra
        let small = models[1].thinking.as_ref().unwrap();
        assert_eq!(small.options.len(), 2);
        assert_eq!(small.default.as_deref(), Some("medium"));
        // 没有等级信息就没有 thinking
        assert!(models[2].thinking.is_none());
    }

    #[test]
    fn default_effort_outside_the_options_falls_back_to_the_first() {
        let mut e = entries();
        e[0].default_reasoning_effort = Some("max".into());
        assert_eq!(from_list(&e)[0].thinking.as_ref().unwrap().default.as_deref(), Some("low"));
    }

    #[test]
    fn at_most_one_default() {
        let mut e = entries();
        e[1].is_default = true;
        let models = from_list(&e);
        assert_eq!(models.iter().filter(|m| m.default).count(), 1);
        assert!(models[0].default);
    }

    #[test]
    fn config_model_becomes_the_default_even_when_unlisted() {
        let mut models = from_list(&entries());
        apply_config(
            &mut models,
            &ConfigSummary {
                model: Some("m-small".into()),
                model_reasoning_effort: Some("low".into()),
                model_provider: None,
            },
        );
        assert_eq!(models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["m-small"]);
        assert_eq!(models[1].thinking.as_ref().unwrap().default.as_deref(), Some("low"));

        let mut models = from_list(&entries());
        let n = models.len();
        apply_config(
            &mut models,
            &ConfigSummary {
                model: Some("my-private-model".into()),
                model_reasoning_effort: Some("high".into()),
                model_provider: None,
            },
        );
        assert_eq!(models.len(), n + 1);
        let added = models.last().unwrap();
        assert_eq!((added.id.as_str(), added.default), ("my-private-model", true));
        // 列表模型共有的 low，加上配置里的 high，默认取配置
        let t = added.thinking.as_ref().unwrap();
        assert_eq!(t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "high"]);
        assert_eq!(t.options[0].description.as_deref(), Some("Fast"));
        assert_eq!(t.options[1].label, "High");
        assert_eq!(t.default.as_deref(), Some("high"));
        assert_eq!(models.iter().filter(|m| m.default).count(), 1);

        // 配置里没有 model：什么都不动
        let mut models = from_list(&entries());
        apply_config(&mut models, &ConfigSummary::default());
        assert!(models[0].default);
    }

    #[test]
    fn unlisted_config_model_gets_the_common_efforts() {
        // 自定义提供方：没有配置等级时取各模型共有的等级，默认沿用原默认模型的默认（不在选项里就取第一项）
        let mut e = entries();
        e[1].supported_reasoning_efforts = e[0].supported_reasoning_efforts.clone();
        let mut models = from_list(&e);
        apply_config(&mut models, &ConfigSummary { model: Some("proxy/sol".into()), ..Default::default() });
        let t = models.last().unwrap().thinking.as_ref().unwrap();
        assert_eq!(t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "xhigh", "ultra"]);
        assert_eq!(t.default.as_deref(), Some("low"));

        // model/list 拿不到：只有配置的等级
        let mut models = vec![];
        apply_config(
            &mut models,
            &ConfigSummary {
                model: Some("proxy/sol".into()),
                model_reasoning_effort: Some("medium".into()),
                model_provider: None,
            },
        );
        let t = models[0].thinking.as_ref().unwrap();
        assert_eq!((t.options.len(), t.default.as_deref()), (1, Some("medium")));

        // 什么等级信息都没有，或者配置的等级不像个等级：不给思考
        let mut models = vec![];
        apply_config(
            &mut models,
            &ConfigSummary {
                model: Some("proxy/sol".into()),
                model_reasoning_effort: Some("--x".into()),
                model_provider: None,
            },
        );
        assert!(models[0].thinking.is_none());
    }

    #[test]
    fn effort_outside_the_models_options_is_ignored() {
        let mut models = from_list(&entries());
        apply_config(
            &mut models,
            &ConfigSummary {
                model: Some("m-small".into()),
                model_reasoning_effort: Some("ultra".into()),
                model_provider: None,
            },
        );
        assert_eq!(models[1].thinking.as_ref().unwrap().default.as_deref(), Some("medium"));
    }

    #[test]
    fn additional_models_cannot_steal_the_default() {
        let mut models = from_list(&entries());
        let extra = vec![
            ModelInfo {
                id: "m-big".into(),
                label: "dup".into(),
                description: None,
                default: true,
                thinking: None,
                tags: vec![],
            },
            ModelInfo {
                id: "corp".into(),
                label: "Corp".into(),
                description: None,
                default: true,
                thinking: None,
                tags: vec![],
            },
        ];
        let n = models.len();
        add_additional(&mut models, &extra);
        assert_eq!(models.len(), n + 1);
        assert_eq!(models.iter().filter(|m| m.default).count(), 1);
        assert_eq!(models.iter().find(|m| m.id == "m-big").unwrap().label, "Big");
    }

    #[test]
    fn unknown_effort_names_get_a_label() {
        assert_eq!(effort_label("ultra"), "Ultra");
        assert_eq!(effort_label("turbo"), "Turbo");
        assert_eq!(effort_label(""), "");
    }
}
