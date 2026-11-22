//! 判断后端的 agent 调用：状态防御、环境复用、错误映射和临时目录生命周期。

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anycon_agent_api::{OneshotError, OneshotReply};
use anycon_daemon::agents::Agents;
use anycon_daemon::clients::Clients;
use anycon_daemon::config::{ConfigStore, Paths};
use anycon_daemon::env::{DaemonEnv, EnvSource};
use anycon_daemon::judge::{agent::AgentBackend, AgentBackendConfig, Backend, CallError, JudgeRequest};
use anycon_daemon::store::Store;
use anycon_daemon::testutil::FakeAdapter;
use anycon_protocol::AgentStatus;

struct Fixture {
    _dir: tempfile::TempDir,
    paths: Paths,
    config: Arc<ConfigStore>,
    agents: Arc<Agents>,
    fake: Arc<FakeAdapter>,
}

impl Fixture {
    async fn new(overrides: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::write(paths.config_toml(), "[agents.fake]\nenabled = true\n").unwrap();
        for (key, value) in overrides {
            anycon_daemon::config::set_value(&paths, key, value).unwrap();
        }
        let config = ConfigStore::load(&paths).unwrap();
        let env = Arc::new(DaemonEnv {
            base: Arc::new(Default::default()),
            source: EnvSource::Process,
            shell: None,
            warning: None,
        });
        let store = Store::open(&paths.db()).await.unwrap();
        let fake = FakeAdapter::new("fake");
        let agents = Agents::new(vec![fake.clone()], config.clone(), env, store, Clients::new(), paths.clone()).await;
        agents.refresh_all().await;
        Self { _dir: dir, paths, config, agents, fake }
    }
}

fn backend(td: &Fixture, agent: &str, model: Option<&str>, timeout: Duration) -> AgentBackend {
    AgentBackend::new(
        AgentBackendConfig { agent: agent.into(), model: model.map(String::from), timeout },
        td.agents.clone(),
    )
}

fn request() -> JudgeRequest {
    JudgeRequest { system: "private system".into(), prompt: "private prompt".into() }
}

async fn enable(td: &Fixture) {
    td.fake.oneshot.store(true, Ordering::SeqCst);
    td.agents.probe("fake", false).await.unwrap();
}

#[tokio::test]
async fn success_environment_models_and_directory_cleanup() {
    let td = Fixture::new(&[
        ("agents.env.GLOBAL_ENV", "global"),
        ("agents.fake.env.AGENT_ENV", "private key"),
        ("agents.fake.default_model", "model-b"),
    ])
    .await;
    enable(&td).await;
    *td.fake.oneshot_result.lock().unwrap() = Ok(OneshotReply {
        text: "private answer".into(),
        input_tokens: Some(11),
        output_tokens: Some(3),
        cost_usd: Some(0.02),
    });
    for model in [None, Some("explicit-model")] {
        let reply = backend(&td, "fake", model, Duration::from_secs(2)).call(&request()).await.unwrap();
        assert_eq!(reply.text, "private answer");
        assert_eq!(reply.usage.input_tokens, Some(11));
        assert_eq!(reply.usage.output_tokens, Some(3));
        assert_eq!(reply.usage.cost_usd, Some(0.02));
    }
    let calls = td.fake.oneshot_requests.lock().unwrap();
    assert_eq!(calls[0].model.as_deref(), Some("model-b"));
    assert_eq!(calls[1].model.as_deref(), Some("explicit-model"));
    assert_ne!(calls[0].cwd, calls[1].cwd);
    for call in calls.iter() {
        assert_eq!(call.env["GLOBAL_ENV"], "global");
        assert_eq!(call.env["AGENT_ENV"], "private key");
        assert_eq!(call.env["ANYCON_AGENT"], "1");
        assert!(!call.env.contains_key("ANYCON_SESSION_ID"));
        assert_eq!(call.system, "private system");
        assert_eq!(call.prompt, "private prompt");
        assert!(!call.cwd.exists());
    }
}

#[tokio::test]
async fn rejects_missing_unsupported_disabled_and_unready_agents() {
    let td = Fixture::new(&[]).await;
    for agent in ["missing", "fake"] {
        assert_eq!(
            backend(&td, agent, None, Duration::from_secs(2)).call(&request()).await.unwrap_err(),
            CallError::AgentFailed
        );
    }
    enable(&td).await;
    for status in [AgentStatus::Disabled, AgentStatus::NotInstalled, AgentStatus::NotAuthenticated] {
        *td.fake.status.lock().unwrap() = status;
        td.agents.probe("fake", false).await.unwrap();
        assert_eq!(
            backend(&td, "fake", None, Duration::from_secs(2)).call(&request()).await.unwrap_err(),
            CallError::AgentFailed
        );
    }
    *td.fake.status.lock().unwrap() = AgentStatus::Ready;
    td.agents.probe("fake", false).await.unwrap();
    anycon_daemon::config::set_value(&td.paths, "agents.fake.enabled", "false").unwrap();
    td.config.reload().unwrap();
    // 缓存的探测仍是 ready，也必须服从最新的禁用配置。
    assert_eq!(
        backend(&td, "fake", None, Duration::from_secs(2)).call(&request()).await.unwrap_err(),
        CallError::AgentFailed
    );
    assert!(td.fake.oneshot_requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn maps_adapter_errors_and_removes_directories() {
    let td = Fixture::new(&[]).await;
    enable(&td).await;
    for error in [OneshotError::Timeout, OneshotError::Unsupported, OneshotError::Spawn, OneshotError::Failed] {
        *td.fake.oneshot_result.lock().unwrap() = Err(error);
        let got = backend(&td, "fake", None, Duration::from_secs(2)).call(&request()).await.unwrap_err();
        assert_eq!(got, if error == OneshotError::Timeout { CallError::Timeout } else { CallError::AgentFailed });
        assert!(!td.fake.oneshot_requests.lock().unwrap().last().unwrap().cwd.exists());
    }
}

#[tokio::test]
async fn outer_timeout_and_cancellation_remove_empty_temporary_directory() {
    let td = Fixture::new(&[]).await;
    enable(&td).await;
    *td.fake.oneshot_delay.lock().unwrap() = Duration::from_secs(30);
    let call = backend(&td, "fake", None, Duration::from_millis(100));
    assert_eq!(call.call(&request()).await.unwrap_err(), CallError::Timeout);
    assert!(!td.fake.oneshot_requests.lock().unwrap()[0].cwd.exists());
    let call = backend(&td, "fake", None, Duration::from_secs(30));
    let task = tokio::spawn(async move { call.call(&request()).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while td.fake.oneshot_requests.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let path = td.fake.oneshot_requests.lock().unwrap()[1].cwd.clone();
    assert!(path.is_dir());
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    std::fs::write(path.join("reply"), "private response").unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(!path.exists());
}
