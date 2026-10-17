//! 模型表（docs/spec/daemon.md § 5.4）：内置一张表 + 用户 `~/.claude/settings.json` / 环境里的模型 + 配置里的 `additional_models`。
//!
//! CLI 没有「列模型」的子命令，所以内置表是基线。（`initialize` 控制请求的响应里其实带一份 `models[]`，但要起整个 CLI
//! 并拉起用户的 MCP server / 插件，探测不该这么重，所以不用；见 README.md。）
//! 思考等级**挂在模型上**：5.x 系列不能关（没有 `off`），4.6 系列没有 `xhigh` 但可以关。

use std::collections::HashMap;
use std::path::PathBuf;

use anycon_protocol::{Choice, ModelInfo, ThinkingOptions};
use semver::Version;
use serde_json::Value;

use crate::common::version_at_least;

pub const LEVELS_5X: &[&str] = &["low", "medium", "high", "xhigh", "max"];
pub const LEVELS_46: &[&str] = &["off", "low", "medium", "high", "max"];

/// 内置表的一项。`min_cli_version` 低于它的 CLI 看不到这个模型。
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
        id: "opus[1m]",
        label: "Opus (latest, 1M context)",
        description: "Alias for the latest Opus with 1M context",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &["1m"],
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
        id: "sonnet[1m]",
        label: "Sonnet (latest, 1M context)",
        description: "Alias for the latest Sonnet with 1M context",
        default: false,
        levels: LEVELS_5X,
        default_level: "medium",
        tags: &["1m"],
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

fn level_label(id: &str) -> &'static str {
    match id {
        "off" => "Off",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra high",
        "max" => "Max",
        _ => "Level",
    }
}

/// 思考等级选项。`levels` 是 CLI `--effort` 的取值（外加 `off`）。
pub fn thinking_options(levels: &[&str], default: &str) -> ThinkingOptions {
    ThinkingOptions {
        options: levels
            .iter()
            .map(|l| Choice { id: (*l).to_string(), label: level_label(l).to_string(), description: None })
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

/// `~/.claude/settings.json`（或 `$CLAUDE_CONFIG_DIR/settings.json`）。读不到 / 解析不了就是 `None`。
pub fn read_user_settings(env: &HashMap<String, String>) -> Option<Value> {
    let dir = match env.get("CLAUDE_CONFIG_DIR").filter(|s| !s.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env.get("HOME")?).join(".claude"),
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

/// 内置表 + 用户配置的 + `additional_models`，按 id 去重（先到先得）。
pub fn assemble(builtin: Vec<ModelInfo>, user: Vec<(String, String)>, additional: &[ModelInfo]) -> Vec<ModelInfo> {
    let mut models = builtin;
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
        assert_eq!(by_id("opus[1m]").tags, vec!["1m"]);
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
