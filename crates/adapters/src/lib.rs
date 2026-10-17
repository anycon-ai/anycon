//! adapter 实现：`claude-code`（stream-json 双向）、`codex`（`codex app-server` JSON-RPC）与 `mock`（测试与 E2E）。
//!
//! daemon 通过 [`all`] 拿到全部 adapter，按配置决定启用哪些；`mock` 默认 `Disabled`，
//! 由 `agents.mock.enabled = true` 或 `ANYCON_MOCK_AGENT=1` 打开（daemon 把结果写进 `AdapterConfig.enabled`）。
//! 诊断文本走 [`diagnose()`]。共用工具在 [`common`]。

use std::sync::Arc;

use anycon_agent_api::Adapter;

pub mod claude_code;
pub mod codex;
pub mod common;
mod diagnose;
pub mod mock;

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use diagnose::{diagnose, Diagnosable};
pub use mock::MockAdapter;

/// 全部内置 adapter。是否启用由 daemon 按配置决定（mock 的 `probe` 在未启用时返回 `Disabled`）。
pub fn all() -> Vec<Arc<dyn Adapter>> {
    vec![Arc::new(ClaudeCodeAdapter::new()), Arc::new(CodexAdapter::new()), Arc::new(MockAdapter::new())]
}

#[cfg(test)]
mod tests {
    #[test]
    fn all_lists_every_builtin_adapter() {
        let ids: Vec<&str> = super::all().iter().map(|a| a.id()).collect();
        assert_eq!(ids, vec!["claude-code", "codex", "mock"]);
    }
}
