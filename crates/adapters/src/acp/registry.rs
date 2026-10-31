//! 内置的 ACP agent 注册表（docs/spec/daemon.md § 5.7）和 agent 名字的规则。
//!
//! 注册表只收录**用户机器上已经有二进制**的启动方式：命令的第一个词在 daemon 环境的 `PATH` 里找得到才算安装，
//! 不跑 `npx` / `uvx` 这类会在用户机器上下载并执行东西的分发方式。用户在 `config.agents.acp.<name>` 里可以覆盖这里的
//! 命令，也可以加注册表里没有的 agent。Claude Code 和 Codex 也有 ACP 封装，anycon 不用它们：原生 adapter 的控制面更完整。
//!
//! 命令和包名取自 [ACP 官方注册表](https://github.com/agentclientprotocol/registry)；id 是 agent 在 `agent.list`、
//! `session.create { agent }` 和 `config.agents.acp.<id>` 里的名字，按命令名取，和注册表里的 id 不一定相同。

use anycon_agent_api::AdapterConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistryEntry {
    /// `agent.list` 里的 id，也是 `[agents.acp.<id>]` 的键。
    pub id: &'static str,
    pub label: &'static str,
    /// 带参数的启动命令，和 `config.agents.acp.<id>.command` 同一个写法。
    pub command: &'static str,
    /// 没装时给用户的提示。
    pub install_hint: &'static str,
}

pub const REGISTRY: &[RegistryEntry] = &[
    RegistryEntry {
        id: "opencode",
        label: "OpenCode",
        command: "opencode acp",
        install_hint: "install OpenCode (https://opencode.ai) and make sure `opencode` is on the daemon's PATH",
    },
    RegistryEntry {
        id: "gemini",
        label: "Gemini CLI",
        command: "gemini --acp",
        install_hint: "npm i -g @google/gemini-cli",
    },
    RegistryEntry {
        id: "goose",
        label: "Goose",
        command: "goose acp",
        install_hint: "install goose (https://block.github.io/goose/) and make sure `goose` is on the daemon's PATH",
    },
    RegistryEntry {
        id: "kimi",
        label: "Kimi Code",
        command: "kimi acp",
        install_hint: "install Kimi CLI (https://moonshotai.github.io/kimi-cli/) and make sure `kimi` is on the daemon's PATH",
    },
    RegistryEntry {
        id: "qwen",
        label: "Qwen Code",
        command: "qwen --acp",
        install_hint: "npm i -g @qwen-code/qwen-code",
    },
    RegistryEntry {
        id: "cursor",
        label: "Cursor",
        command: "cursor-agent acp",
        install_hint: "install the Cursor CLI (https://cursor.com/docs/cli/acp) and make sure `cursor-agent` is on the daemon's PATH",
    },
    RegistryEntry {
        id: "copilot",
        label: "GitHub Copilot",
        command: "copilot --acp",
        install_hint: "npm i -g @github/copilot",
    },
    RegistryEntry { id: "pi", label: "Pi", command: "pi-acp", install_hint: "npm i -g pi-acp" },
];

/// 注册表里的 agent。
pub fn registry_entry(id: &str) -> Option<&'static RegistryEntry> {
    REGISTRY.iter().find(|e| e.id == id)
}

/// 注册表里全部 agent 的 id。
pub fn registry_ids() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|e| e.id)
}

/// 不能用作 ACP agent 名字的词：别的 adapter 的 id，以及 `[agents]` 表里本来就有含义的键（`config.agents.<键>` 和
/// `config.agents.acp.<名字>` 在序列化的配置里共用一个命名空间）。
pub const RESERVED_NAMES: &[&str] =
    &["claude-code", "codex", "mock", "acp", "env", "env_from_login_shell", "probe_interval", "probe_timeout"];

/// agent 名字的规则：小写字母 / 数字开头，之后可以有小写字母、数字、`-`、`_`，最多 32 个字符。名字会出现在命令行
/// （`--agent <名字>`）、会话记录和配置键里，所以收紧到不需要转义的字符。
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok_chars = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    let ok_first = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if name.is_empty() || name.len() > 32 || !ok_chars || !ok_first {
        return Err(format!(
            "{name:?} is not a valid agent name: use 1-32 lowercase letters, digits, '-' or '_', starting with a letter or digit"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Err(format!("{name:?} is reserved and cannot be the name of an ACP agent"));
    }
    Ok(())
}

/// 生效的启动命令：`config.command`（非空）优先，其次注册表。两者都没有 = `None`（自己加的 agent 必须写 `command`）。
pub fn effective_command(id: &str, config: &AdapterConfig) -> Option<String> {
    config
        .command
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .or_else(|| registry_entry(id).map(|e| e.command.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_ids_are_valid_unique_names() {
        let mut seen = std::collections::HashSet::new();
        for e in REGISTRY {
            validate_name(e.id).unwrap_or_else(|m| panic!("{m}"));
            assert!(seen.insert(e.id), "duplicate id {}", e.id);
            assert!(!e.command.trim().is_empty() && !e.label.is_empty() && !e.install_hint.is_empty());
        }
        // 规格 § 5.7 的八项
        assert_eq!(
            registry_ids().collect::<Vec<_>>(),
            vec!["opencode", "gemini", "goose", "kimi", "qwen", "cursor", "copilot", "pi"]
        );
    }

    #[test]
    fn names_are_restricted() {
        for ok in ["opencode", "my-agent", "a1", "x_y", "9lives"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in
            ["", "Open", "-x", "_x", "a b", "a.b", "é", &"x".repeat(33), "codex", "claude-code", "mock", "acp", "env"]
        {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn the_config_command_overrides_the_registry() {
        let none = AdapterConfig::default();
        assert_eq!(effective_command("opencode", &none).as_deref(), Some("opencode acp"));
        assert_eq!(effective_command("mine", &none), None);
        let custom = AdapterConfig { command: Some("  /opt/oc/opencode acp --x ".into()), ..Default::default() };
        assert_eq!(effective_command("opencode", &custom).as_deref(), Some("/opt/oc/opencode acp --x"));
        assert_eq!(effective_command("mine", &custom).as_deref(), Some("/opt/oc/opencode acp --x"));
        let blank = AdapterConfig { command: Some("   ".into()), ..Default::default() };
        assert_eq!(effective_command("pi", &blank).as_deref(), Some("pi-acp"));
    }
}
