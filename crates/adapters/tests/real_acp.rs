//! 真实 ACP agent 的冒烟测试（OpenCode）。默认 `#[ignore]`，且需要 `ANYCON_REAL_AGENT_TESTS=1`：
//!
//! ```text
//! ANYCON_REAL_AGENT_TESTS=1 cargo test -p anycon-adapters --test real_acp -- --ignored --nocapture
//! ```
//!
//! - `real_opencode_probe`：对真 `opencode acp` 跑探测（`initialize` + `session/new`，在临时目录里，不碰任何项目）。不发消息，
//!   不花 token；没装 `opencode` 就跳过。`opencode` 没登录 / 没配提供方时 `session/new` 可能报认证错误——那也是一种合格的结果
//!   （`not_authenticated`），测试只要求探测能给出明确的结论。
//! - `real_opencode_turn`：真的发一条很短的消息，**会通过用户配置的提供方花一个真实请求**；需要时用 `--skip real_opencode_turn` 跳过。
//!   没装 / 探测结果不是 ready 就跳过。

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anycon_adapters::AcpAdapter;
use anycon_agent_api::{Adapter, AdapterConfig, AgentEvent, ProbeContext, SpawnRequest, ENV_STRIP};
use anycon_protocol::{AgentStatus, Event, SessionSettings, TurnDoneReason};
use common::*;

fn enabled() -> bool {
    std::env::var("ANYCON_REAL_AGENT_TESTS").ok().as_deref() == Some("1")
}

fn real_env() -> HashMap<String, String> {
    std::env::vars().filter(|(k, _)| !ENV_STRIP.contains(&k.as_str())).collect()
}

fn real_opencode() -> Option<PathBuf> {
    anycon_adapters::common::bin::collect_candidates("opencode", &real_env(), &std::env::temp_dir())
        .into_iter()
        .next()
        .map(|(p, _)| p)
}

fn probe_ctx(scratch: &std::path::Path) -> ProbeContext {
    ProbeContext {
        env: Arc::new(real_env()),
        config: AdapterConfig { enabled: true, ..Default::default() },
        deadline: Duration::from_secs(90),
        cheap: false,
        previous: None,
        scratch_dir: scratch.to_path_buf(),
    }
}

#[tokio::test]
#[ignore = "needs a real opencode; set ANYCON_REAL_AGENT_TESTS=1"]
async fn real_opencode_probe() {
    if !enabled() {
        return;
    }
    let Some(path) = real_opencode() else {
        eprintln!("opencode is not installed, skipping");
        return;
    };
    eprintln!("using {}", path.display());
    let scratch = tempfile::tempdir().unwrap();
    let adapter = AcpAdapter::new("opencode");
    let info = adapter.probe(&probe_ctx(scratch.path())).await;
    eprintln!("{info:#?}");
    assert_eq!(info.id, "opencode");
    assert_eq!(info.label, "OpenCode");
    match info.status {
        AgentStatus::Ready => {
            assert!(info.version.is_some());
            assert!(
                !info.models.is_empty() || !info.modes.is_empty(),
                "a ready agent should offer a model or a mode choice"
            );
        }
        AgentStatus::NotAuthenticated => assert!(info.issue.is_some()),
        other => panic!("unexpected status {other:?}: {:?}", info.issue),
    }
}

#[tokio::test]
#[ignore = "needs a real opencode and spends a real request; set ANYCON_REAL_AGENT_TESTS=1"]
async fn real_opencode_turn() {
    if !enabled() {
        return;
    }
    if real_opencode().is_none() {
        eprintln!("opencode is not installed, skipping");
        return;
    }
    let scratch = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let adapter = AcpAdapter::new("opencode");
    let ctx = probe_ctx(scratch.path());
    let info = adapter.probe(&ctx).await;
    if info.status != AgentStatus::Ready {
        eprintln!("opencode is not ready ({:?}), skipping", info.status);
        return;
    }
    let req = SpawnRequest {
        session_id: "sess_real".into(),
        cwd: project.path().to_path_buf(),
        settings: SessionSettings::default(),
        env: ctx.env.clone(),
        config: ctx.config.clone(),
        resume: None,
        agent_info: Arc::new(info),
    };
    let mut h = adapter.spawn(req).await.expect("spawn");
    let started = match next_event(&mut h).await {
        AgentEvent::Started { resume } => resume,
        other => panic!("{other:?}"),
    };
    assert!(!started.agent_session_id.is_empty());
    h.send(input("Reply with exactly the word: pong")).await.unwrap();
    let evs =
        collect_until(&mut h, |e| matches!(e, AgentEvent::Event(Event::TurnDone { .. }) | AgentEvent::Exited { .. }))
            .await;
    eprintln!("{}", names(&evs).join(" "));
    match evs.last().unwrap() {
        AgentEvent::Event(Event::TurnDone { reason, .. }) => assert_eq!(*reason, TurnDoneReason::Completed, "{evs:?}"),
        other => panic!("{other:?}"),
    }
    assert!(text_of(&evs).to_lowercase().contains("pong"), "{}", text_of(&evs));
    h.shutdown().await.unwrap();
}
