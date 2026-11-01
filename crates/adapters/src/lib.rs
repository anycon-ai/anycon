//! adapter 实现：`claude-code`（stream-json 双向）、`codex`（`codex app-server` JSON-RPC）、`acp`（任何 ACP agent，
//! 每个 agent 一个 adapter 实例）与 `mock`（测试与 E2E）。
//!
//! daemon 通过 [`all_with_acp`] 拿到全部 adapter，按配置决定启用哪些；`mock` 默认 `Disabled`，
//! 由 `agents.mock.enabled = true` 或 `ANYCON_MOCK_AGENT=1` 打开（daemon 把结果写进 `AdapterConfig.enabled`）。
//! 诊断文本走 [`diagnose()`]（ACP 的在 `Adapter::diagnose`）。共用工具在 [`common`]。

use std::sync::Arc;

use anycon_agent_api::Adapter;

pub mod acp;
pub mod claude_code;
pub mod codex;
pub mod common;
mod diagnose;
pub mod mock;

pub use acp::AcpAdapter;
pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use diagnose::{diagnose, Diagnosable};
pub use mock::MockAdapter;

/// 三个内置 adapter（claude-code、codex、mock）。是否启用由 daemon 按配置决定（mock 的 `probe` 在未启用时返回 `Disabled`）。
pub fn all() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(ClaudeCodeAdapter::new()), Arc::new(CodexAdapter::new()), Arc::new(MockAdapter::new())]
}

/// [`all`] 加上 ACP agent：内置注册表里的，加上 `acp_names`（用户在 `[agents.acp.<name>]` 里加的）。
pub fn all_with_acp<I, S>(acp_names: I) -> Vec<Arc<dyn Adapter>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut v = all();
    v.extend(acp::adapters(acp_names));
    v
}

#[cfg(test)]
mod tests {
    #[test]
    fn acp_agents_are_added_per_name_without_duplicates() {
        let ids: Vec<String> =
            super::all_with_acp(["mine", "opencode", "Bad Name", "codex"]).iter().map(|a| a.id().to_string()).collect();
        assert_eq!(
            ids,
            [
                "claude-code",
                "codex",
                "mock",
                "opencode",
                "gemini",
                "goose",
                "kimi",
                "qwen",
                "cursor",
                "copilot",
                "pi",
                "mine"
            ]
        );
        let adapters = super::all_with_acp(Vec::<String>::new());
        assert_eq!(adapters.iter().filter(|a| a.driver() == "acp").count(), 8);
    }

    #[test]
    fn all_lists_every_builtin_adapter() {
        let ids: Vec<String> = super::all().iter().map(|a| a.id().to_string()).collect();
        assert_eq!(ids, vec!["claude-code", "codex", "mock"]);
    }
}
