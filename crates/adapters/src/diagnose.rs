//! `agent.diagnose` / `anycon doctor` 用的诊断输出（docs/spec/daemon.md § 5.5）。
//!
//! daemon 不需要认识具体 adapter：调 [`diagnose`]`(id, ctx)` 即可，未知 id 返回 `None`。

use anycon_agent_api::ProbeContext;
use anycon_protocol::DiagnosticField;

use crate::{ClaudeCodeAdapter, CodexAdapter, MockAdapter};

/// 能给出诊断文本和结构化字段的 adapter。
#[async_trait::async_trait]
pub trait Diagnosable {
    /// 返回 `(人类可读全文, 结构化字段)`。内部会跑一次完整探测，所有子进程调用都带超时。
    async fn diagnose(&self, ctx: &ProbeContext) -> (String, Vec<DiagnosticField>);
}

/// 按 adapter id 诊断。未知 id = `None`。
pub async fn diagnose(id: &str, ctx: &ProbeContext) -> Option<(String, Vec<DiagnosticField>)> {
    match id {
        crate::claude_code::ID => Some(ClaudeCodeAdapter::new().diagnose(ctx).await),
        crate::codex::ID => Some(CodexAdapter::new().diagnose(ctx).await),
        crate::mock::ID => Some(MockAdapter::new().diagnose(ctx).await),
        _ => None,
    }
}

/// 把字段渲染成 `label: value` 的文本。多行值缩进到标签之后。
pub(crate) fn fields_to_text(title: &str, fields: &[DiagnosticField]) -> String {
    let mut out = String::new();
    out.push_str(title);
    out.push('\n');
    out.push_str(&"-".repeat(title.chars().count().max(8)));
    out.push('\n');
    for f in fields {
        let mut lines = f.value.lines();
        match lines.next() {
            Some(first) => out.push_str(&format!("{}: {}\n", f.label, first)),
            None => out.push_str(&format!("{}:\n", f.label)),
        }
        for l in lines {
            out.push_str(&format!("    {l}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_multiline_values_indented() {
        let fields = vec![
            DiagnosticField { label: "a".into(), value: "one".into() },
            DiagnosticField { label: "which -a".into(), value: "/x/claude\n/y/claude".into() },
            DiagnosticField { label: "empty".into(), value: String::new() },
        ];
        let t = fields_to_text("T", &fields);
        assert!(t.contains("a: one\n"));
        assert!(t.contains("which -a: /x/claude\n    /y/claude\n"));
        assert!(t.contains("empty:\n"));
    }
}
