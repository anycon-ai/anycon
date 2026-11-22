//! Codex exec 的临时只读调用；最终回答来自文件，用量来自 JSON 事件流。

use anycon_agent_api::{OneshotError, OneshotReply, OneshotRequest};
use serde::Deserialize;

use crate::common::oneshot::{read_limited, run};

/// 已核对完整参数集的保守版本下界，不声称是参数首次引入的版本。
pub(super) const MIN_VERSION: &str = "0.169.0";

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

pub(super) async fn call(req: OneshotRequest) -> Result<OneshotReply, OneshotError> {
    tokio::time::timeout(req.timeout, async {
        if !req.config.enabled || !req.agent_info.capabilities.oneshot {
            return Err(OneshotError::Unsupported);
        }
        let (program, mut args) =
            super::process::resolve_program(&req.config, req.agent_info.path.as_deref(), &req.env)
                .map_err(|_| OneshotError::Spawn)?;
        let output = req.cwd.join("oneshot-reply.txt");
        args.extend(
            [
                "exec",
                "--skip-git-repo-check",
                "--ephemeral",
                "--sandbox",
                "read-only",
                "--color",
                "never",
                "--json",
                "-o",
            ]
            .map(String::from),
        );
        args.push(output.to_string_lossy().into_owned());
        if let Some(model) = &req.model {
            args.extend(["-m".into(), model.clone()]);
        }
        args.push("-".into());
        let input = format!(
            concat!(
                "=== SYSTEM INSTRUCTIONS ===\n{}\n=== END SYSTEM INSTRUCTIONS ===\n\n",
                "=== USER PROMPT ===\n{}\n=== END USER PROMPT ===\n",
            ),
            req.system, req.prompt,
        );
        let bytes = run(&program, &args, &req, input.as_bytes()).await?;
        let mut reply = OneshotReply::default();
        let mut completed = false;
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.iter().all(u8::is_ascii_whitespace)) {
            let event: Event = serde_json::from_slice(line).map_err(|_| OneshotError::Failed)?;
            match event.kind.as_str() {
                "turn.failed" => return Err(OneshotError::Failed),
                // Codex 的可重试错误也用 error；成功必须等到 turn.completed。
                "error" => {}
                "turn.completed" => {
                    completed = true;
                    if let Some(usage) = event.usage {
                        // cached_input_tokens 是 input_tokens 的子集，不再相加。
                        reply.input_tokens = usage.input_tokens;
                        reply.output_tokens = usage.output_tokens;
                    }
                }
                _ => {}
            }
        }
        if !completed {
            return Err(OneshotError::Failed);
        }
        // 不读取链接、管道或设备文件；文件本身的读取也有上限。
        if !tokio::fs::symlink_metadata(&output).await.map_err(|_| OneshotError::Failed)?.is_file() {
            return Err(OneshotError::Failed);
        }
        let file = tokio::fs::File::open(&output).await.map_err(|_| OneshotError::Failed)?;
        reply.text = String::from_utf8(read_limited(file).await?).map_err(|_| OneshotError::Failed)?;
        Ok(reply)
    })
    .await
    .map_err(|_| OneshotError::Timeout)?
}
