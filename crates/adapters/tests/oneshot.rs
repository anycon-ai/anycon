//! 一次性 adapter 调用：参数、内容保密、有界输出、取消和超时清理，不连接模型服务。
#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::{ClaudeCodeAdapter, CodexAdapter, MockAdapter};
use anycon_agent_api::{Adapter, AdapterConfig, OneshotError, OneshotRequest, ProbeContext};
use serde_json::Value;

async fn request(dir: &tempfile::TempDir, mode: &str) -> OneshotRequest {
    let config = AdapterConfig {
        enabled: true,
        command: Some(format!(
            "{} {} 'prefix argument'",
            shell_words::quote(which::which("python3").unwrap().to_str().unwrap()),
            shell_words::quote(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/bin/fake-oneshot.py"))
        )),
        ..Default::default()
    };
    let env = Arc::new(HashMap::from([
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("ONESHOT_MODE".into(), mode.into()),
        ("ONESHOT_SECRET".into(), "private key".into()),
    ]));
    let info = MockAdapter::new()
        .probe(&ProbeContext {
            config: config.clone(),
            env: env.clone(),
            deadline: Duration::from_secs(5),
            cheap: false,
            previous: None,
            scratch_dir: dir.path().into(),
        })
        .await;
    OneshotRequest {
        system: "private system 中文".into(),
        prompt: "private prompt\nnext line".into(),
        model: Some("test-model".into()),
        timeout: Duration::from_secs(5),
        cwd: dir.path().into(),
        config,
        env,
        agent_info: Arc::new(info),
    }
}

fn adapters() -> [Box<dyn Adapter>; 2] {
    [Box::new(ClaudeCodeAdapter::new()), Box::new(CodexAdapter::new())]
}

#[tokio::test]
async fn arguments_stdin_environment_and_usage() {
    for adapter in adapters() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(&dir, "ok").await;
        let reply = adapter.oneshot(req.clone()).await.unwrap();
        assert_eq!(reply.text, "answer 中文\n");
        assert_eq!(reply.output_tokens, Some(2));
        let log: Value = serde_json::from_slice(&std::fs::read(dir.path().join("request.json")).unwrap()).unwrap();
        assert_eq!(std::fs::canonicalize(log["cwd"].as_str().unwrap()).unwrap(), dir.path().canonicalize().unwrap());
        assert_eq!(log["env"], "private key");
        let args: Vec<&str> = log["args"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        if adapter.id() == "claude-code" {
            assert_eq!(
                args,
                [
                    "-p",
                    "--output-format",
                    "json",
                    "--tools",
                    "",
                    "--no-session-persistence",
                    "--strict-mcp-config",
                    "--system-prompt",
                    &req.system,
                    "--model",
                    "test-model"
                ]
            );
            assert_eq!(log["prompt"], req.prompt);
            assert_eq!(reply.input_tokens, Some(17));
            assert_eq!(reply.cost_usd, Some(0.001));
        } else {
            assert_eq!(
                args,
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
                    dir.path().join("oneshot-reply.txt").to_str().unwrap(),
                    "-m",
                    "test-model",
                    "-"
                ]
            );
            assert_eq!(
                log["prompt"],
                format!(
                    concat!(
                        "=== SYSTEM INSTRUCTIONS ===\n{}\n=== END SYSTEM INSTRUCTIONS ===\n\n",
                        "=== USER PROMPT ===\n{}\n=== END USER PROMPT ===\n",
                    ),
                    req.system, req.prompt,
                )
            );
            assert_eq!(reply.input_tokens, Some(10));
            assert_eq!(reply.cost_usd, None);
        }
        assert!(!format!("{req:?}").contains("private"));
        assert!(!format!("{reply:?}").contains("answer"));
    }
}

#[tokio::test]
async fn missing_usage_and_model_are_optional() {
    for adapter in adapters() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(&dir, "no_usage").await;
        req.model = None;
        let reply = adapter.oneshot(req).await.unwrap();
        assert_eq!(reply.input_tokens, None);
        assert_eq!(reply.output_tokens, None);
        assert_eq!(reply.cost_usd, None);
        let log = std::fs::read_to_string(dir.path().join("request.json")).unwrap();
        assert!(!log.contains("test-model"));
    }
}

#[tokio::test]
async fn failures_and_limits_do_not_expose_output() {
    for adapter in adapters() {
        for mode in ["error", "exit", "malformed", "stdout_limit", "stderr_limit"] {
            let dir = tempfile::tempdir().unwrap();
            let error = adapter.oneshot(request(&dir, mode).await).await.unwrap_err();
            assert_eq!(error, OneshotError::Failed, "{} {mode}", adapter.id());
            assert!(!format!("{error} {error:?}").contains("PRIVATE"));
        }
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(&dir, "ok").await;
        req.config.command = Some("/no/such/private-command".into());
        assert_eq!(adapter.oneshot(req).await.unwrap_err(), OneshotError::Spawn);
    }
    for mode in ["file_limit", "missing"] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(CodexAdapter::new().oneshot(request(&dir, mode).await).await.unwrap_err(), OneshotError::Failed);
    }
}

#[tokio::test]
async fn timeout_covers_stdin_and_kills_descendants() {
    for adapter in adapters() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(&dir, "sleep").await;
        req.timeout = Duration::from_millis(700);
        assert_eq!(adapter.oneshot(req).await.unwrap_err(), OneshotError::Timeout);
        assert!(dir.path().join("started").exists(), "child really started");
        let pid: i32 = std::fs::read_to_string(dir.path().join("started")).unwrap().parse().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("leader must exit and be reaped");
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert!(!dir.path().join("child-survived").exists(), "grandchild must be killed");
        let mut req = request(&dir, "blocked_stdin").await;
        req.prompt = "x".repeat(2 * 1024 * 1024);
        req.timeout = Duration::from_millis(300);
        assert_eq!(adapter.oneshot(req).await.unwrap_err(), OneshotError::Timeout);
    }
}

#[tokio::test]
async fn dropping_call_kills_descendants() {
    for adapter in adapters() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(&dir, "sleep").await;
        let task = tokio::spawn(async move { adapter.oneshot(req).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(1400)).await;
        assert!(!dir.path().join("child-survived").exists());
    }
}

#[tokio::test]
async fn mock_script_and_debug_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut req = request(&dir, "ok").await;
    assert!(req.agent_info.capabilities.oneshot);
    assert_eq!(MockAdapter::new().oneshot(req.clone()).await.unwrap().text, r#"{"rules":[]}"#);
    req.prompt = "before MOCK_ONESHOT:aGVsbG8= after".into();
    assert_eq!(MockAdapter::new().oneshot(req.clone()).await.unwrap().text, "hello");
    req.prompt = "MOCK_ONESHOT:%%%".into();
    assert_eq!(MockAdapter::new().oneshot(req).await.unwrap_err(), OneshotError::Failed);
}

#[tokio::test]
async fn unsupported_adapter_uses_the_default_implementation() {
    let dir = tempfile::tempdir().unwrap();
    let adapter = anycon_adapters::acp::adapter_for("test-acp");
    assert_eq!(adapter.oneshot(request(&dir, "ok").await).await.unwrap_err(), OneshotError::Unsupported);
    assert!(!dir.path().join("request.json").exists());
}

#[tokio::test]
async fn codex_accepts_a_recovered_error_followed_by_completion() {
    let dir = tempfile::tempdir().unwrap();
    let reply = CodexAdapter::new().oneshot(request(&dir, "recovered_error").await).await.unwrap();
    assert_eq!(reply.text, "answer 中文\n");
    assert_eq!(reply.input_tokens, Some(10));
    assert_eq!(reply.output_tokens, Some(2));
}

#[tokio::test]
async fn codex_requires_completion_even_with_a_reply_file_and_zero_exit() {
    for mode in ["error_only", "no_completion"] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(CodexAdapter::new().oneshot(request(&dir, mode).await).await.unwrap_err(), OneshotError::Failed);
        assert!(dir.path().join("oneshot-reply.txt").exists());
    }
}

#[tokio::test]
async fn codex_completion_does_not_override_a_failed_turn_or_nonzero_exit() {
    for mode in ["completed_then_failed", "completed_nonzero"] {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(CodexAdapter::new().oneshot(request(&dir, mode).await).await.unwrap_err(), OneshotError::Failed);
    }
}
