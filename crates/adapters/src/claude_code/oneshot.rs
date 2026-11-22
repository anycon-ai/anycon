//! Claude Code 的无工具一次性调用，解析最终 JSON 与包含缓存的输入用量。

use anycon_agent_api::{OneshotError, OneshotReply, OneshotRequest};
use serde::Deserialize;

use crate::common::oneshot::run;

/// 已核对完整参数集的保守版本下界，不声称是参数首次引入的版本。
pub(super) const MIN_VERSION: &str = "2.1.325";

#[derive(Deserialize)]
struct Output {
    result: String,
    #[serde(default)]
    is_error: bool,
    usage: Option<Usage>,
    total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

pub(super) async fn call(req: OneshotRequest) -> Result<OneshotReply, OneshotError> {
    tokio::time::timeout(req.timeout, async {
        if !req.config.enabled || !req.agent_info.capabilities.oneshot {
            return Err(OneshotError::Unsupported);
        }
        let (program, mut args) =
            super::process::resolve_program(&req.config, req.agent_info.path.as_deref(), &req.env)
                .map_err(|_| OneshotError::Spawn)?;
        args.extend(
            [
                "-p",
                "--output-format",
                "json",
                "--tools",
                "",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--system-prompt",
            ]
            .map(String::from),
        );
        args.push(req.system.clone());
        if let Some(model) = &req.model {
            args.extend(["--model".into(), model.clone()]);
        }
        let bytes = run(&program, &args, &req, req.prompt.as_bytes()).await?;
        let out: Output = serde_json::from_slice(&bytes).map_err(|_| OneshotError::Failed)?;
        if out.is_error {
            return Err(OneshotError::Failed);
        }
        let mut reply = OneshotReply { text: out.result, cost_usd: out.total_cost_usd, ..Default::default() };
        if let Some(usage) = out.usage {
            // Anthropic 的 input_tokens 不含缓存读写；嵌套的 cache_creation 明细已计入总数，不重复加。
            reply.input_tokens = usage
                .input_tokens
                .map(|n| {
                    n.checked_add(usage.cache_creation_input_tokens)
                        .and_then(|n| n.checked_add(usage.cache_read_input_tokens))
                        .ok_or(OneshotError::Failed)
                })
                .transpose()?;
            reply.output_tokens = usage.output_tokens;
        }
        Ok(reply)
    })
    .await
    .map_err(|_| OneshotError::Timeout)?
}
