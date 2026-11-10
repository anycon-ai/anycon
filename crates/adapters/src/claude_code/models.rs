//! 模型表（docs/spec/daemon.md § 5.4）：CLI 自己的模型清单 + 用户 `~/.claude/settings.json` / 环境里的模型 + 配置里的
//! `additional_models`。
//!
//! CLI 没有「列模型」的子命令，但 stream-json 的 `initialize` 控制请求回的 `models[]` 就是它的模型清单（和终端里 `/model`
//! 看到的一样，随账号、CLI 版本变）。完整探测发一次 `initialize`，[`from_cli`] 把它转成模型表；问不到（老 CLI、超时、
//! 起不来）才用内置表 [`BUILTIN`]，内置表随 adapter 版本维护，只是兜底。
//! 思考等级**挂在模型上**：5.x 系列不能关（没有 `off`），4.6 系列没有 `xhigh` 但可以关。

use std::collections::HashMap;
use std::path::PathBuf;

use anycon_protocol::{Choice, ModelInfo, ThinkingOptions};
use semver::Version;
use serde::Deserialize;
use serde_json::Value;

use crate::common::version_at_least;

pub const LEVELS_5X: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub const LEVELS_46: &[&str] = &["off", "low", "medium", "high", "max"];

/// 内置表的一项（`initialize` 问不到模型时的兜底）。`min_cli_version` 低于它的 CLI 看不到这个模型。
pub struct BuiltinModel {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub default: bool,
    pub levels: &'static [&'static str],
    pub default_level: &'static str,
    pub tags: &'static [&'static str],
    pub min_cli_version: &'static str,
}

/// 随 adapter 版本维护。别名（`opus` 等）指向各系列最新版，所以思考等级跟最新版一致。
pub const BUILTIN: &[BuiltinModel] = &[
    BuiltinModel {
        id: "claude-opus-5-5",
        label: "Opus 5.5",
        description: "Best for everyday, complex tasks",
        default: true,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-fable-5-1",
        label: "Fable 5.1",
        description: "Most capable, for the hardest and longest-running tasks",
        default: false,
        levels: LEVELS_5X,
        default_level: "high",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-sonnet-5-5",
        label: "Sonnet 5.5",
        description: "Efficient for routine tasks",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-haiku-5-5",
        label: "Haiku 5.5",
        description: "Fastest for quick answers",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-opus-4-6",
        label: "Opus 4.6",
        description: "Previous generation Opus",
        default: false,
        levels: LEVELS_46,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-opus-4-6[1m]",
        label: "Opus 4.6 (1M context)",
        description: "Previous generation Opus with 1M context",
        default: false,
        levels: LEVELS_46,
        default_level: "medium",
        tags: &["1m"],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-sonnet-4-6",
        label: "Sonnet 4.6",
        description: "Previous generation Sonnet",
        default: false,
        levels: LEVELS_46,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "claude-sonnet-4-6[1m]",
        label: "Sonnet 4.6 (1M context)",
        description: "Previous generation Sonnet with 1M context",
        default: false,
        levels: LEVELS_46,
        default_level: "medium",
        tags: &["1m"],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "opus",
        label: "Opus (latest)",
        description: "Alias for the latest Opus",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "sonnet",
        label: "Sonnet (latest)",
        description: "Alias for the latest Sonnet",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "haiku",
        label: "Haiku (latest)",
        description: "Alias for the latest Haiku",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &[],
        min_cli_version: "2.1.0",
    },
    BuiltinModel {
        id: "fable",
        label: "Fable (latest)",
        description: "Alias for the latest Fable",
        default: false,
        levels: LEVELS_5X,
        default_level: "high",
        tags: &[],
        min_cli_version: "2.1.0",
    },
];

/// 思考等级的显示名。认识的给固定写法，不认识的（CLI 以后加的等级）首字母大写。
fn level_label(id: &str) -> String {
    match id {
        "off" => "Off".to_string(),
        "low" => "Low".to_string(),
        "medium" => "Medium".to_string(),
        "high" => "High".to_string(),
        "xhigh" => "Extra high".to_string(),
        "max" => "Max".to_string(),
        other => capitalize(other),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// 思考等级选项。`levels` 是 CLI `--effort` 的取值（外加 `off`）。
pub fn thinking_options(levels: &[&str], default: &str) -> ThinkingOptions {
    ThinkingOptions {
        options: levels
            .iter()
            .map(|l| Choice { id: (*l).to_string(), label: level_label(l), description: None })
            .collect(),
        default: Some(default.to_string()),
    }
}

/// 按 CLI 版本过滤后的内置表。`cli` 为 `None`（版本解析不出来）时不过滤。
pub fn builtin_models(table: &[BuiltinModel], cli: Option<&Version>) -> Vec<ModelInfo> {
    table
        .iter()
        .filter(|m| cli.map(|v| version_at_least(v, m.min_cli_version)).unwrap_or(true))
        .map(|m| ModelInfo {
            id: m.id.to_string(),
            label: m.label.to_string(),
            description: Some(m.description.to_string()),
            default: m.default,
            thinking: Some(thinking_options(m.levels, m.default_level)),
            tags: m.tags.iter().map(|t| (*t).to_string()).collect(),
        })
        .collect()
}

/// `initialize` 响应里 `models[]` 的一项。只取用得到的字段，其余（`supportsFastMode`、`promoListPrice`…）忽略。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliModel {
    /// 传给 `--model` 的值：完整 id、别名（`opus`）或 `default`。
    pub value: String,
    /// 这一行实际指向的完整 id（`opus` → `claude-opus-5-5`）。CLI 的说明：让调用方把存下来的完整 id 对到覆盖它的别名行。
    #[serde(default)]
    pub resolved_model: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub supports_effort: Option<bool>,
    #[serde(default)]
    pub supported_effort_levels: Option<Vec<String>>,
    #[serde(default)]
    pub supports_thinking_off: Option<bool>,
    /// 看得见但不能选（比如组织的零数据保留设置排除了它）。
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// CLI 清单里表示「默认模型」的那一行。
pub const DEFAULT_ROW: &str = "default";

fn non_empty(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

/// CLI 不给默认思考等级：沿用内置表里同一个模型的，没有就同系列别名的（`claude-fable-5-2` → `fable`），
/// 再没有就 `medium`，最后取第一个不是 `off` 的。
fn default_level(id: &str, levels: &[&str]) -> String {
    let family = id.strip_prefix("claude-").and_then(|r| r.split('-').next());
    BUILTIN
        .iter()
        .find(|b| b.id == id)
        .or_else(|| family.and_then(|f| BUILTIN.iter().find(|b| b.id == f)))
        .map(|b| b.default_level)
        .filter(|l| levels.contains(l))
        .or_else(|| levels.iter().copied().find(|l| *l == "medium"))
        .or_else(|| levels.iter().copied().find(|l| *l != "off"))
        .or_else(|| levels.first().copied())
        .unwrap_or_default()
        .to_string()
}

/// 一行的思考等级：`supportedEffortLevels`，能关（`supportsThinkingOff`）就在前面加 `off`。没有等级的模型不给思考选项。
fn cli_thinking(row: &CliModel, id: &str) -> Option<ThinkingOptions> {
    if row.supports_effort == Some(false) {
        return None;
    }
    let mut levels: Vec<&str> = Vec::new();
    if row.supports_thinking_off == Some(true) {
        levels.push("off");
    }
    for l in row.supported_effort_levels.iter().flatten().map(|l| l.trim()) {
        if !l.is_empty() && !levels.contains(&l) {
            levels.push(l);
        }
    }
    if levels.iter().all(|l| *l == "off") {
        return None;
    }
    Some(thinking_options(&levels, &default_level(id, &levels)))
}

/// `initialize` 的模型清单 → 模型表。
///
/// - 每个模型以**完整 id** 出现一次（别名行用它的 `resolvedModel`），所以以前存下来的 `claude-opus-5-5` 之类照样对得上；
/// - 别名（`opus`、`fable`…）另列在完整 id 后面，跟着 CLI 解析（`fable` 不一定是最新的 Fable，描述里写当前指向谁）；
/// - `default` 行不单列：它指向的模型标为默认、排第一；CLI 没给默认就标第一个；
/// - `disabled` 的不列。
pub fn from_cli(rows: &[CliModel]) -> Vec<ModelInfo> {
    let mut full: Vec<ModelInfo> = Vec::new();
    let mut aliases: Vec<ModelInfo> = Vec::new();
    let mut default_row: Option<&CliModel> = None;
    for row in rows {
        if row.disabled == Some(true) {
            continue;
        }
        let Some(value) = non_empty(Some(&row.value)) else { continue };
        if value == DEFAULT_ROW {
            default_row.get_or_insert(row);
            continue;
        }
        let id = non_empty(row.resolved_model.as_deref()).unwrap_or(value);
        let label = non_empty(row.display_name.as_deref()).unwrap_or(id).to_string();
        let thinking = cli_thinking(row, id);
        if !full.iter().any(|m| m.id == id) {
            full.push(ModelInfo {
                id: id.to_string(),
                label: label.clone(),
                description: non_empty(row.description.as_deref()).map(str::to_string),
                default: false,
                thinking: thinking.clone(),
                tags: vec![],
            });
        }
        if id != value && !aliases.iter().any(|m| m.id == value) {
            aliases.push(ModelInfo {
                id: value.to_string(),
                label: format!("{} (alias)", capitalize(value)),
                description: Some(format!("Claude Code's `{value}` alias, currently {label}")),
                default: false,
                thinking,
                tags: vec![],
            });
        }
    }
    let default_id = default_row.and_then(|r| non_empty(r.resolved_model.as_deref()));
    match default_id.and_then(|id| full.iter().position(|m| m.id == id)) {
        Some(i) => {
            let m = full.remove(i);
            full.insert(0, m);
        }
        // 默认行指向一个清单里别处没列的模型：单独加一项，名字只能用 id（`displayName` 是「Default (recommended)」）
        None => {
            if let (Some(id), Some(row)) = (default_id, default_row) {
                full.insert(
                    0,
                    ModelInfo {
                        id: id.to_string(),
                        label: id.to_string(),
                        description: non_empty(row.description.as_deref()).map(str::to_string),
                        default: false,
                        thinking: cli_thinking(row, id),
                        tags: vec![],
                    },
                );
            }
        }
    }
    if let Some(first) = full.first_mut() {
        first.default = true;
    }
    full.extend(aliases);
    full
}

/// `~/.claude/settings.json`（或 `$CLAUDE_CONFIG_DIR/settings.json`）。读不到 / 解析不了就是 `None`。
pub fn read_user_settings(env: &HashMap<String, String>) -> Option<Value> {
    let dir = match env.get("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(anycon_agent_api::env_home(env)?).join(".claude"),
    };
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// 用户自己配的模型：`(id, 来源说明)`。去重保序；环境（daemon 环境 + config env）优先于 settings.json。
pub fn user_configured_models(env: &HashMap<String, String>, settings: Option<&Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |id: &str, source: String| {
        let id = id.trim();
        if !id.is_empty() && !out.iter().any(|(x, _)| x == id) {
            out.push((id.to_string(), source));
        }
    };
    let is_model_var = |k: &str| {
        k == "ANTHROPIC_MODEL"
            || k == "ANTHROPIC_SMALL_FAST_MODEL"
            || (k.starts_with("ANTHROPIC_DEFAULT_") && k.ends_with("_MODEL"))
    };
    // `ANTHROPIC_MODEL` 是主模型，排最前；其余按名字排序，结果稳定。
    let order = |a: &&String, b: &&String| {
        (a.as_str() != "ANTHROPIC_MODEL", a.as_str()).cmp(&(b.as_str() != "ANTHROPIC_MODEL", b.as_str()))
    };
    let mut env_keys: Vec<&String> = env.keys().filter(|k| is_model_var(k)).collect();
    env_keys.sort_by(order);
    for k in env_keys {
        add(&env[k], format!("From environment variable {k}"));
    }
    if let Some(s) = settings {
        if let Some(m) = s.get("model").and_then(Value::as_str) {
            add(m, "From ~/.claude/settings.json (model)".to_string());
        }
        if let Some(e) = s.get("env").and_then(Value::as_object) {
            let mut keys: Vec<&String> = e.keys().filter(|k| is_model_var(k)).collect();
            keys.sort_by(order);
            for k in keys {
                if let Some(v) = e[k].as_str() {
                    add(v, format!("From ~/.claude/settings.json (env.{k})"));
                }
            }
        }
    }
    out
}

/// CLI 清单（或内置表）+ 用户配置的 + `additional_models`，按 id 去重（先到先得）。
pub fn assemble(base: Vec<ModelInfo>, user: Vec<(String, String)>, additional: &[ModelInfo]) -> Vec<ModelInfo> {
    let mut models = base;
    let push = |m: ModelInfo, models: &mut Vec<ModelInfo>| {
        if !models.iter().any(|x| x.id == m.id) {
            models.push(m);
        }
    };
    for (id, source) in user {
        push(
            ModelInfo {
                id: id.clone(),
                label: id,
                description: Some(source),
                default: false,
                thinking: None,
                tags: vec![],
            },
            &mut models,
        );
    }
    for m in additional {
        let mut m = m.clone();
        m.default = false;
        push(m, &mut models);
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_table_shape() {
        let models = builtin_models(BUILTIN, None);
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("missing {id}"));
        // 恰好一个默认：opus 5.5
        assert_eq!(
            models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["claude-opus-5-5"]
        );
        let opus = by_id("claude-opus-5-5").thinking.as_ref().unwrap();
        assert_eq!(
            opus.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(opus.default.as_deref(), Some("medium"));
        assert_eq!(by_id("claude-fable-5-1").thinking.as_ref().unwrap().default.as_deref(), Some("high"));
        // 5.x 不能关、4.6 能关且没有 xhigh
        for id in ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"] {
            assert!(!by_id(id).thinking.as_ref().unwrap().options.iter().any(|o| o.id == "off"), "{id}");
        }
        for id in ["claude-opus-4-6", "claude-sonnet-4-6"] {
            let t = by_id(id).thinking.as_ref().unwrap();
            assert_eq!(
                t.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
                vec!["off", "low", "medium", "high", "max"]
            );
            assert!(!t.options.iter().any(|o| o.id == "xhigh"));
        }
        for alias in ["opus", "sonnet", "haiku", "fable"] {
            by_id(alias);
        }
        assert_eq!(by_id("claude-sonnet-4-6[1m]").tags, vec!["1m"]);
        // 5.x 默认就是 1M 上下文，`[1m]` 不起作用（2.1.316 实测 `opus[1m]` 的 contextWindow 与 `opus` 相同），不列
        assert!(!models.iter().any(|m| m.id == "opus[1m]" || m.id == "sonnet[1m]"));
        // 每个模型的默认等级都在它的选项里
        for m in &models {
            let t = m.thinking.as_ref().unwrap();
            assert!(t.options.iter().any(|o| Some(&o.id) == t.default.as_ref()), "{}", m.id);
        }
    }

    #[test]
    fn min_cli_version_filters_models() {
        const TABLE: &[BuiltinModel] = &[
            BuiltinModel {
                id: "old",
                label: "Old",
                description: "",
                default: true,
                levels: LEVELS_5X,
                default_level: "medium",
                tags: &[],
                min_cli_version: "2.1.0",
            },
            BuiltinModel {
                id: "new",
                label: "New",
                description: "",
                default: false,
                levels: LEVELS_5X,
                default_level: "medium",
                tags: &[],
                min_cli_version: "2.5.0",
            },
        ];
        let v = Version::parse("2.2.0").unwrap();
        assert_eq!(builtin_models(TABLE, Some(&v)).iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["old"]);
        let v = Version::parse("2.5.0").unwrap();
        assert_eq!(builtin_models(TABLE, Some(&v)).len(), 2);
        assert_eq!(builtin_models(TABLE, None).len(), 2);
    }

    #[test]
    fn user_models_from_env_and_settings() {
        let env = HashMap::from([
            ("ANTHROPIC_MODEL".to_string(), "us.anthropic.claude-sonnet-5-5-v1:0".to_string()),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), "my-proxy-opus".to_string()),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(), "  ".to_string()),
            ("UNRELATED".to_string(), "x".to_string()),
        ]);
        let settings = json!({
            "model": "opusplan",
            "env": { "ANTHROPIC_DEFAULT_SONNET_MODEL": "my-proxy-sonnet", "ANTHROPIC_MODEL": "us.anthropic.claude-sonnet-5-5-v1:0", "OTHER": "y" }
        });
        let got = user_configured_models(&env, Some(&settings));
        let ids: Vec<&str> = got.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, vec!["us.anthropic.claude-sonnet-5-5-v1:0", "my-proxy-opus", "opusplan", "my-proxy-sonnet"]);
        assert!(got[0].1.contains("ANTHROPIC_MODEL"));
        assert!(got[2].1.contains("settings.json"));
    }

    #[test]
    fn assemble_dedups_and_appends() {
        let builtin = builtin_models(BUILTIN, None);
        let n = builtin.len();
        let additional = vec![
            ModelInfo {
                id: "opus".into(),
                label: "dup".into(),
                description: None,
                default: true,
                thinking: None,
                tags: vec![],
            },
            ModelInfo {
                id: "corp-model".into(),
                label: "Corp".into(),
                description: None,
                default: true,
                thinking: None,
                tags: vec![],
            },
        ];
        let models = assemble(builtin, vec![("my-model".into(), "From env".into())], &additional);
        assert_eq!(models.len(), n + 2);
        assert_eq!(models[n].id, "my-model");
        assert_eq!(models[n + 1].id, "corp-model");
        // additional 不能抢默认
        assert_eq!(models.iter().filter(|m| m.default).count(), 1);
        assert!(models.iter().find(|m| m.id == "opus").unwrap().label.contains("latest"));
    }

    /// 真实 CLI（2.1.311）`initialize` 回的 `models[]`（录制见 `tests/fixtures/claude/real_initialize.json`）。
    fn recorded_rows() -> Vec<CliModel> {
        let line: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/claude/real_initialize.json")).unwrap();
        serde_json::from_value(line["response"]["response"]["models"].clone()).unwrap()
    }

    fn row(value: &str, resolved: Option<&str>, levels: &[&str]) -> CliModel {
        CliModel {
            value: value.into(),
            resolved_model: resolved.map(Into::into),
            display_name: Some(format!("Name of {value}")),
            description: None,
            supports_effort: Some(!levels.is_empty()),
            supported_effort_levels: Some(levels.iter().map(|l| l.to_string()).collect()),
            supports_thinking_off: None,
            disabled: None,
        }
    }

    #[test]
    fn recorded_initialize_models_become_the_model_table() {
        let models = from_cli(&recorded_rows());
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap_or_else(|| panic!("missing {id}"));
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        // 完整 id 在前（默认行指向的排第一），别名在后；`default` 行本身不列
        assert_eq!(
            ids,
            vec![
                "claude-sonnet-5-5",
                "claude-opus-5-5",
                "claude-fable-5-2",
                "claude-haiku-5-5",
                "claude-fable-5-1",
                "claude-haiku-4-5-20251001",
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
                "opus",
                "sonnet",
                "haiku",
                "fable",
            ]
        );
        assert_eq!(
            models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["claude-sonnet-5-5"]
        );

        // 别名行的名字给它指向的完整 id；别名自己写清楚当前指向谁
        assert_eq!(by_id("claude-opus-5-5").label, "Opus 5.5");
        assert_eq!(by_id("fable").label, "Fable (alias)");
        assert!(by_id("fable").description.as_deref().unwrap().contains("Fable 5.1"));

        let levels =
            |id: &str| by_id(id).thinking.as_ref().map(|t| t.options.iter().map(|o| o.id.clone()).collect::<Vec<_>>());
        let default_level = |id: &str| by_id(id).thinking.as_ref().and_then(|t| t.default.clone());
        assert_eq!(levels("claude-opus-5-5").unwrap(), vec!["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(default_level("claude-opus-5-5").as_deref(), Some("medium"));
        // 新模型沿用同系列的默认等级
        assert_eq!(default_level("claude-fable-5-2").as_deref(), Some("high"));
        assert_eq!(default_level("fable").as_deref(), Some("high"));
        // 能关的在前面加 off；4.6 没有 xhigh
        assert_eq!(levels("claude-opus-4-6").unwrap(), vec!["off", "low", "medium", "high", "max"]);
        assert_eq!(levels("claude-opus-4-8").unwrap(), vec!["off", "low", "medium", "high", "xhigh", "max"]);
        // 不支持 effort 的（Haiku 4.5）没有思考选项
        assert!(by_id("claude-haiku-4-5-20251001").thinking.is_none());
        for m in &models {
            if let Some(t) = &m.thinking {
                assert!(t.options.iter().any(|o| Some(&o.id) == t.default.as_ref()), "{}", m.id);
            }
        }
    }

    #[test]
    fn from_cli_edge_cases() {
        // 没有默认行：第一个当默认；没有 resolvedModel：value 就是 id，不当别名
        let models = from_cli(&[row("model-a", None, &["low", "high"]), row("model-b", None, &[])]);
        assert_eq!(
            models.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
            vec![("model-a", true), ("model-b", false)]
        );
        assert_eq!(models[0].thinking.as_ref().unwrap().default.as_deref(), Some("low"));
        assert!(models[1].thinking.is_none());

        // 默认行指向别处没列的模型：单独加一项
        let mut d = row("default", Some("claude-new-9"), &["medium"]);
        d.description = Some("New 9 · Something".into());
        let models = from_cli(&[d, row("claude-opus-5-5", Some("claude-opus-5-5"), &["medium"])]);
        assert_eq!(
            models.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
            vec![("claude-new-9", true), ("claude-opus-5-5", false)]
        );
        assert_eq!(models[0].label, "claude-new-9");

        // disabled 的不列；同一个完整 id 只列一次；不认识的等级首字母大写
        let mut off = row("claude-zdr", None, &["low"]);
        off.disabled = Some(true);
        let models = from_cli(&[
            off,
            row("opus", Some("claude-opus-5-5"), &["low", "ultra"]),
            row("claude-opus-5-5", Some("claude-opus-5-5"), &["low"]),
            row("  ", None, &[]),
        ]);
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["claude-opus-5-5", "opus"]);
        let opts = &models[0].thinking.as_ref().unwrap().options;
        assert_eq!(opts.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), vec!["Low", "Ultra"]);
        assert!(from_cli(&[]).is_empty());
    }

    #[test]
    fn reads_settings_json_from_home_or_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::write(tmp.path().join(".claude/settings.json"), r#"{"model":"opus"}"#).unwrap();
        let env = HashMap::from([("HOME".to_string(), tmp.path().to_string_lossy().into_owned())]);
        assert_eq!(read_user_settings(&env).unwrap()["model"], "opus");
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("settings.json"), r#"{"model":"haiku"}"#).unwrap();
        let env = HashMap::from([("CLAUDE_CONFIG_DIR".to_string(), other.path().to_string_lossy().into_owned())]);
        assert_eq!(read_user_settings(&env).unwrap()["model"], "haiku");
        assert!(read_user_settings(&HashMap::new()).is_none());
        std::fs::write(other.path().join("settings.json"), "not json").unwrap();
        assert!(read_user_settings(&env).is_none());
    }
}
