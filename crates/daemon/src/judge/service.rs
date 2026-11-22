//! 判断调用的节点级调度：位置选择、热加载、当地日预算、取消及只含元数据的调用记录。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anycon_protocol::{
    AgentInfo, AgentStatus, JudgeApiFormat, JudgeBackendInfo, JudgeBackendKind, JudgeError, JudgeErrorCode, JudgeInfo,
    JudgeProblem, JudgeUsage,
};
use tokio::sync::{watch, Mutex, OwnedMutexGuard};

use super::{agent, api, resolve_api_key, ApiFormat, Backend, CallError, JudgeReply, JudgeRequest};
use crate::agents::Agents;
use crate::config::{judge::is_loopback_http, Config, ConfigStore, JudgeBackendConfig, JudgeConfig};
use crate::env::DaemonEnv;
use crate::store::{JudgeCallFinish, JudgeCallStart, Store, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Extract,
    Triage,
}

impl Position {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extract => "extract",
            Self::Triage => "triage",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServiceError {
    #[error("judge is not configured or unavailable")]
    NotConfigured,
    #[error("judge daily budget exhausted ({used}/{limit})")]
    Budget { used: u32, limit: u32 },
    #[error("judge is busy")]
    Busy,
    #[error(transparent)]
    Call(#[from] CallError),
}

#[derive(Debug)]
pub struct CallOutcome {
    pub reply: JudgeReply,
    pub call_id: String,
    pub backend: String,
}

/// 工厂接收已校验的配置；测试可以返回共享的 FakeJudgeBackend。
pub type BackendFactory = dyn Fn(&JudgeBackendConfig) -> Arc<dyn Backend> + Send + Sync;

/// 可替换当前时刻；当天边界始终使用机器的时区，不改进程的 TZ。
pub struct Clock {
    fixed_ms: std::sync::atomic::AtomicU64,
}

impl Default for Clock {
    fn default() -> Self {
        Self { fixed_ms: std::sync::atomic::AtomicU64::new(u64::MAX) }
    }
}

impl Clock {
    pub fn now(&self) -> u64 {
        {
            let fixed = self.fixed_ms.load(std::sync::atomic::Ordering::Relaxed);
            if fixed != u64::MAX {
                return fixed;
            }
        }
        crate::util::now_ms()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn set(&self, ms: u64) {
        self.fixed_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 用当地午夜重新解析 DST，不能从现在减去小时数（夏令时切换日可能不是 24 小时）。
#[cfg(unix)]
pub fn local_day_start(now_ms: u64) -> Option<u64> {
    let seconds: libc::time_t = (now_ms / 1000).try_into().ok()?;
    // 指针指向有效的栈内存；localtime_r 不共享可变的 tm 缓冲区。
    unsafe {
        let mut local: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&seconds, &mut local).is_null() {
            return None;
        }
        local.tm_hour = 0;
        local.tm_min = 0;
        local.tm_sec = 0;
        local.tm_isdst = -1;
        let midnight = libc::mktime(&mut local);
        u64::try_from(midnight).ok()?.checked_mul(1000)
    }
}

#[cfg(windows)]
pub fn local_day_start(now_ms: u64) -> Option<u64> {
    // Windows CRT 的 tm 是九个 int；使用 64 位 time_t，避免 2038 年溢出。
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
    }
    unsafe extern "C" {
        fn _localtime64_s(result: *mut Tm, time: *const i64) -> i32;
        fn _mktime64(time: *mut Tm) -> i64;
    }
    let seconds = i64::try_from(now_ms / 1000).ok()?;
    // CRT 写入已分配的结构；随后按当地午夜和自动 DST 重新换算时间戳。
    unsafe {
        let mut local: Tm = std::mem::zeroed();
        if _localtime64_s(&mut local, &seconds) != 0 {
            return None;
        }
        local.hour = 0;
        local.min = 0;
        local.sec = 0;
        local.isdst = -1;
        u64::try_from(_mktime64(&mut local)).ok()?.checked_mul(1000)
    }
}

fn selected(cfg: &JudgeConfig, position: Position) -> Option<&str> {
    let name = match position {
        Position::Extract if !cfg.extract.is_empty() => &cfg.extract,
        _ => &cfg.triage,
    };
    cfg.backends.contains_key(name).then_some(name.as_str())
}

/// 在线 node.info 和离线 doctor 使用同一套脱敏及可用性判断。
pub fn describe(cfg: &Config, env: &HashMap<String, String>, agents: &[AgentInfo]) -> JudgeInfo {
    let backends = cfg
        .judge
        .backends
        .iter()
        .map(|(name, backend)| match backend {
            JudgeBackendConfig::Api(c) => JudgeBackendInfo {
                name: name.clone(),
                kind: JudgeBackendKind::Api,
                format: Some(match c.format {
                    ApiFormat::Anthropic => JudgeApiFormat::Anthropic,
                    ApiFormat::Openai => JudgeApiFormat::Openai,
                }),
                model: Some(c.model.clone()),
                agent: None,
                problem: (resolve_api_key(&c.api_key, env).is_none() && !is_loopback_http(&c.base_url))
                    .then_some(JudgeProblem::NoKey),
            },
            JudgeBackendConfig::Agent(c) => {
                let info = agents.iter().find(|i| i.id == c.agent);
                let enabled = cfg.adapter_config(&c.agent).is_none_or(|a| a.enabled);
                let problem = match info {
                    Some(i) if enabled && i.enabled && i.status == AgentStatus::Ready => {
                        (!i.capabilities.oneshot).then_some(JudgeProblem::AgentUnsupported)
                    }
                    _ => Some(JudgeProblem::AgentMissing),
                };
                JudgeBackendInfo {
                    name: name.clone(),
                    kind: JudgeBackendKind::Agent,
                    format: None,
                    model: c
                        .model
                        .clone()
                        .or_else(|| cfg.adapter_config(&c.agent).and_then(|a| a.default_model.clone())),
                    agent: Some(c.agent.clone()),
                    problem,
                }
            }
        })
        .collect();
    JudgeInfo {
        backends,
        extract: selected(&cfg.judge, Position::Extract).map(str::to_string),
        triage: selected(&cfg.judge, Position::Triage).map(str::to_string),
        daily_calls: cfg.judge.daily_calls,
    }
}

pub fn protocol_error(error: CallError) -> JudgeError {
    let code = match error {
        CallError::Timeout => JudgeErrorCode::Timeout,
        CallError::Network => JudgeErrorCode::Network,
        CallError::Http(_) => JudgeErrorCode::Http,
        CallError::Auth => JudgeErrorCode::Auth,
        CallError::RateLimited => JudgeErrorCode::RateLimited,
        CallError::BadOutput => JudgeErrorCode::BadOutput,
        CallError::AgentFailed => JudgeErrorCode::AgentFailed,
        CallError::Cancelled => JudgeErrorCode::Cancelled,
        CallError::Internal => JudgeErrorCode::Internal,
    };
    JudgeError { code, status: if let CallError::Http(s) = error { Some(s) } else { None } }
}

fn error_name(error: CallError) -> &'static str {
    match error {
        CallError::Timeout => "timeout",
        CallError::Network => "network",
        CallError::Http(_) => "http",
        CallError::Auth => "auth",
        CallError::RateLimited => "rate_limited",
        CallError::BadOutput => "bad_output",
        CallError::AgentFailed => "agent_failed",
        CallError::Cancelled => "cancelled",
        CallError::Internal => "internal",
    }
}

pub struct Judge {
    config: Arc<ConfigStore>,
    store: Store,
    env: Arc<DaemonEnv>,
    agents: Arc<Agents>,
    factory: RwLock<Arc<BackendFactory>>,
    gate: Arc<Mutex<()>>,
    shutdown: watch::Receiver<bool>,
    clock: Arc<Clock>,
}

impl Judge {
    pub async fn new(
        config: Arc<ConfigStore>,
        store: Store,
        env: Arc<DaemonEnv>,
        agents: Arc<Agents>,
        shutdown: watch::Receiver<bool>,
    ) -> Result<Arc<Self>, StoreError> {
        let api_env = env.clone();
        let backend_agents = agents.clone();
        let factory: Arc<BackendFactory> = Arc::new(move |cfg| match cfg {
            JudgeBackendConfig::Api(c) => Arc::new(api::ApiBackend::new(c.clone(), api_env.clone())),
            JudgeBackendConfig::Agent(c) => Arc::new(agent::AgentBackend::new(c.clone(), backend_agents.clone())),
        });
        let clock = Arc::new(Clock::default());
        store.cancel_unfinished_judge_calls(clock.now()).await?;
        store.cancel_running_extract_runs(clock.now()).await?;
        Ok(Arc::new(Self {
            config,
            store,
            env,
            agents,
            factory: RwLock::new(factory),
            gate: Arc::new(Mutex::new(())),
            shutdown,
            clock,
        }))
    }

    pub fn info(&self) -> JudgeInfo {
        describe(&self.config.get(), &self.env.base, &self.agents.list().agents)
    }
    pub fn daily_calls(&self) -> u32 {
        self.config.get().judge.daily_calls
    }
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// 替换工厂只影响之后的调用，不改变已经进行中的后端。
    #[cfg(any(test, feature = "testing"))]
    pub fn set_backend_factory(&self, factory: Arc<BackendFactory>) {
        *self.factory.write().unwrap() = factory;
    }

    pub async fn used_today(&self) -> Result<u32, StoreError> {
        let start = local_day_start(self.clock.now())
            .ok_or_else(|| std::io::Error::other("cannot determine local day start"))?;
        self.store.count_judge_calls_since(start).await
    }

    /// 依次检查配置、占用和预算；持锁后计数，预留本身不写任何记录。
    pub async fn begin(&self, position: Position) -> Result<Permit, ServiceError> {
        let cfg = self.config.get();
        let info = describe(&cfg, &self.env.base, &self.agents.list().agents);
        let name = selected(&cfg.judge, position).ok_or(ServiceError::NotConfigured)?;
        let backend_info = info.backends.into_iter().find(|b| b.name == name).ok_or(ServiceError::NotConfigured)?;
        if backend_info.problem.is_some() {
            return Err(ServiceError::NotConfigured);
        }
        let guard = self.gate.clone().try_lock_owned().map_err(|_| ServiceError::Busy)?;
        let used = self.used_today().await.map_err(|_| CallError::Internal)?;
        let limit = cfg.judge.daily_calls;
        if used >= limit {
            return Err(ServiceError::Budget { used, limit });
        }
        let backend = self.factory.read().unwrap()(&cfg.judge.backends[name]);
        Ok(Permit {
            guard,
            position,
            backend,
            backend_info,
            store: self.store.clone(),
            clock: self.clock.clone(),
            shutdown: self.shutdown.clone(),
        })
    }

    pub async fn call(&self, position: Position, req: JudgeRequest) -> Result<CallOutcome, ServiceError> {
        self.begin(position).await?.call(req).await
    }

    /// 关库前等待正在收尾的调用；daemon 已先广播关停。
    pub async fn wait_idle(&self) {
        let _guard = self.gate.lock().await;
    }

    pub async fn cleanup(&self) -> Result<(), StoreError> {
        let before = self.clock.now().saturating_sub(90 * 24 * 3600 * 1000);
        self.store.prune_extract_runs(before).await?;
        self.store.prune_judge_calls(before).await?;
        Ok(())
    }

    pub fn spawn_cleanup(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut shutdown = this.shutdown.clone();
            loop {
                tokio::select! {
                    _ = async {
                        let _ = shutdown.wait_for(|stopped| *stopped).await;
                    } => break,
                    _ = tick.tick() => {
                        if this.cleanup().await.is_err() {
                            tracing::warn!("judge retention cleanup failed");
                        }
                    }
                }
            }
        })
    }
}

/// 持有节点调用锁及按配置快照构造的后端；直接丢弃只释放锁，不产生记录。
pub struct Permit {
    guard: OwnedMutexGuard<()>,
    position: Position,
    backend: Arc<dyn Backend>,
    backend_info: JudgeBackendInfo,
    store: Store,
    clock: Arc<Clock>,
    shutdown: watch::Receiver<bool>,
}

impl Permit {
    /// 记账和调用在独立任务中完成，调用方丢弃 future 也不会中断收尾。
    pub async fn call(self, req: JudgeRequest) -> Result<CallOutcome, ServiceError> {
        tokio::spawn(async move {
            let Self { guard, position, backend, backend_info, store, clock, mut shutdown } = self;
            let _guard = guard;
            if *shutdown.borrow() {
                return Err(ServiceError::Call(CallError::Cancelled));
            }
            let name = backend_info.name;
            let kind = match backend_info.kind {
                JudgeBackendKind::Api => "api",
                _ => "agent",
            };
            let input_bytes = (req.system.len() + req.prompt.len()) as u64;
            let call_id = store
                .start_judge_call(&JudgeCallStart {
                    position: position.as_str(),
                    backend: &name,
                    kind,
                    model: backend_info.model.as_deref(),
                    started_ms: clock.now(),
                    input_bytes,
                })
                .await
                .map_err(|_| CallError::Internal)?;
            let timer = Instant::now();
            let result = tokio::select! {
                biased;
                _ = async {
                    let _ = shutdown.wait_for(|stopped| *stopped).await;
                } => Err(CallError::Cancelled),
                result = backend.call(&req) => result,
            };
            let (status, output_bytes, usage) = match &result {
                Ok(reply) => (
                    "ok",
                    reply.text.len() as u64,
                    JudgeUsage {
                        input_tokens: reply.usage.input_tokens,
                        output_tokens: reply.usage.output_tokens,
                        cost_usd: reply.usage.cost_usd,
                    },
                ),
                Err(error) => (error_name(*error), 0, JudgeUsage::default()),
            };
            let saved = store
                .finish_judge_call(&call_id, &JudgeCallFinish { finished_ms: clock.now(), status, output_bytes, usage })
                .await;
            tracing::info!(
                position = position.as_str(),
                backend = %name,
                result = status,
                elapsed_ms = timer.elapsed().as_millis() as u64,
                input_bytes,
                output_bytes,
                "judge call finished"
            );
            saved.map_err(|_| CallError::Internal)?;
            Ok(CallOutcome { reply: result?, call_id, backend: name })
        })
        .await
        .map_err(|_| ServiceError::Call(CallError::Internal))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;
    use crate::testutil::FakeJudgeBackend;
    use serde_json::json;
    use sqlx::Row;

    fn config(limit: u32) -> Config {
        serde_json::from_value(json!({"judge":{"triage":"local", "daily_calls":limit, "backends":{
            "local":{"kind":"api","format":"openai","base_url":"http://localhost:8888/v1","model":"m"}
        }}}))
        .unwrap()
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        judge: Arc<Judge>,
        store: Store,
        shutdown: watch::Sender<bool>,
        fake: Arc<FakeJudgeBackend>,
    }

    impl Fixture {
        async fn new(cfg: Config, replies: Vec<Result<JudgeReply, CallError>>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path());
            let store = Store::open(&paths.db()).await.unwrap();
            let config = ConfigStore::with_config(paths.clone(), cfg);
            let env = Arc::new(DaemonEnv {
                base: Arc::new(HashMap::new()),
                source: crate::env::EnvSource::Process,
                shell: None,
                warning: None,
            });
            let agents =
                Agents::new(vec![], config.clone(), env.clone(), store.clone(), crate::clients::Clients::new(), paths)
                    .await;
            let (shutdown, rx) = watch::channel(false);
            let judge = Judge::new(config, store.clone(), env, agents, rx).await.unwrap();
            let fake = FakeJudgeBackend::new(replies);
            let b = fake.clone();
            judge.set_backend_factory(Arc::new(move |_| b.clone()));
            Self { _dir: dir, judge, store, shutdown, fake }
        }

        async fn close(self) {
            self.shutdown.send(true).unwrap();
            self.judge.wait_idle().await;
            self.store.close().await;
        }
    }

    fn request() -> JudgeRequest {
        JudgeRequest { system: "系统".into(), prompt: "private prompt".into() }
    }
    fn reply() -> JudgeReply {
        JudgeReply {
            text: "private reply".into(),
            usage: super::super::Usage { input_tokens: Some(12), output_tokens: Some(3), cost_usd: Some(0.1) },
        }
    }

    async fn wait_called(fake: &FakeJudgeBackend) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while fake.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn unconfigured_broken_and_zero_budget_do_not_create_calls() {
        for (cfg, expected) in [
            (Config::default(), ServiceError::NotConfigured),
            (
                {
                    let mut c = config(20);
                    c.judge.extract = "missing".into();
                    c
                },
                ServiceError::NotConfigured,
            ),
            (
                {
                    let mut c = config(20);
                    if let JudgeBackendConfig::Api(a) = c.judge.backends.get_mut("local").unwrap() {
                        a.base_url = "https://private.example".into();
                    }
                    c
                },
                ServiceError::NotConfigured,
            ),
            (config(0), ServiceError::Budget { used: 0, limit: 0 }),
        ] {
            let f = Fixture::new(cfg, vec![]).await;
            assert_eq!(f.judge.call(Position::Extract, request()).await.unwrap_err(), expected);
            assert_eq!(f.judge.used_today().await.unwrap(), 0);
            assert!(f.fake.requests.lock().unwrap().is_empty());
            f.close().await;
        }
    }

    #[tokio::test]
    async fn unused_permit_releases_lock_without_records() {
        let f = Fixture::new(config(1), vec![]).await;
        let permit = f.judge.begin(Position::Extract).await.unwrap();
        assert_eq!(f.judge.used_today().await.unwrap(), 0);
        assert_eq!(f.judge.begin(Position::Triage).await.err(), Some(ServiceError::Busy));
        assert!(f.fake.requests.lock().unwrap().is_empty());
        drop(permit);
        let permit = f.judge.begin(Position::Triage).await.unwrap();
        drop(permit);
        assert_eq!(f.store.count_judge_calls_since(0).await.unwrap(), 0);
        f.close().await;
    }

    #[tokio::test]
    async fn begin_checks_configuration_then_busy_then_budget() {
        let f = Fixture::new(config(1), vec![]).await;
        let permit = f.judge.begin(Position::Extract).await.unwrap();
        let text = "[judge]\n\
                    triage='local'\n\
                    extract='missing'\n\
                    daily_calls=0\n\
                    [judge.backends.local]\n\
                    kind='api'\n\
                    format='openai'\n\
                    model='m'\n\
                    base_url='http://localhost'";
        let path = f.judge.config.paths().config_toml();
        std::fs::write(&path, text).unwrap();
        f.judge.config.reload().unwrap();
        assert_eq!(f.judge.begin(Position::Extract).await.err(), Some(ServiceError::NotConfigured));
        assert_eq!(f.judge.begin(Position::Triage).await.err(), Some(ServiceError::Busy));
        std::fs::write(&path, text.replace("http://localhost", "https://example.com")).unwrap();
        f.judge.config.reload().unwrap();
        assert_eq!(f.judge.begin(Position::Triage).await.err(), Some(ServiceError::NotConfigured));
        std::fs::write(&path, text).unwrap();
        f.judge.config.reload().unwrap();
        drop(permit);
        assert_eq!(f.judge.begin(Position::Triage).await.err(), Some(ServiceError::Budget { used: 0, limit: 0 }));
        assert_eq!(f.judge.used_today().await.unwrap(), 0);
        f.close().await;
    }

    #[tokio::test]
    async fn concurrent_begins_cannot_both_spend_the_last_call() {
        let f = Fixture::new(config(1), vec![Ok(reply())]).await;
        let (first, second) = tokio::join!(f.judge.begin(Position::Extract), f.judge.begin(Position::Triage));
        let permit = match (first, second) {
            (Ok(permit), Err(ServiceError::Busy)) | (Err(ServiceError::Busy), Ok(permit)) => permit,
            _ => panic!("only one request may reserve the node"),
        };
        assert_eq!(f.judge.used_today().await.unwrap(), 0);
        permit.call(request()).await.unwrap();
        assert_eq!(f.judge.begin(Position::Extract).await.err(), Some(ServiceError::Budget { used: 1, limit: 1 }));
        assert_eq!(f.judge.used_today().await.unwrap(), 1);
        assert_eq!(f.fake.requests.lock().unwrap().len(), 1);
        f.close().await;
    }

    #[tokio::test]
    async fn permit_keeps_backend_and_configuration_snapshot() {
        let f = Fixture::new(config(1), vec![Ok(reply())]).await;
        let permit = f.judge.begin(Position::Extract).await.unwrap();
        let text = "[judge]\n\
                    triage='changed'\n\
                    daily_calls=0\n\
                    [judge.backends.changed]\n\
                    kind='api'\n\
                    format='openai'\n\
                    model='changed-model'\n\
                    base_url='http://localhost'";
        let path = f.judge.config.paths().config_toml();
        std::fs::write(&path, text).unwrap();
        f.judge.config.reload().unwrap();
        let next = FakeJudgeBackend::new(vec![Err(CallError::Timeout)]);
        let next_backend = next.clone();
        f.judge.set_backend_factory(Arc::new(move |cfg| {
            let JudgeBackendConfig::Api(api) = cfg else { panic!() };
            assert_eq!(api.model, "changed-model");
            next_backend.clone()
        }));
        let outcome = permit.call(request()).await.unwrap();
        assert_eq!(outcome.backend, "local");
        let model: String = sqlx::query_scalar("SELECT model FROM judge_calls WHERE id = ?")
            .bind(outcome.call_id)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
        assert_eq!(model, "m");
        assert_eq!(f.fake.requests.lock().unwrap().len(), 1);
        assert!(next.requests.lock().unwrap().is_empty());
        assert_eq!(f.judge.begin(Position::Extract).await.err(), Some(ServiceError::Budget { used: 1, limit: 0 }));
        std::fs::write(&path, text.replace("daily_calls=0", "daily_calls=2")).unwrap();
        f.judge.config.reload().unwrap();
        assert_eq!(
            f.judge.call(Position::Extract, request()).await.unwrap_err(),
            ServiceError::Call(CallError::Timeout)
        );
        assert_eq!(next.requests.lock().unwrap().len(), 1);
        f.close().await;
    }

    #[tokio::test]
    async fn usage_query_propagates_store_errors_and_failed_begin_releases_lock() {
        let f = Fixture::new(config(1), vec![]).await;
        f.store.close().await;
        assert!(matches!(f.judge.used_today().await, Err(StoreError::Db(sqlx::Error::PoolClosed))));
        assert_eq!(f.judge.begin(Position::Extract).await.err(), Some(ServiceError::Call(CallError::Internal)));
        assert!(f.judge.gate.try_lock().is_ok());
        f.close().await;
    }

    #[tokio::test]
    async fn shutdown_before_permit_call_does_not_start_a_record() {
        let f = Fixture::new(config(1), vec![]).await;
        let permit = f.judge.begin(Position::Extract).await.unwrap();
        f.shutdown.send(true).unwrap();
        assert_eq!(permit.call(request()).await.unwrap_err(), ServiceError::Call(CallError::Cancelled));
        assert_eq!(f.judge.used_today().await.unwrap(), 0);
        assert!(f.fake.requests.lock().unwrap().is_empty());
        f.close().await;
    }

    #[tokio::test]
    async fn successes_and_errors_count_and_record_only_metadata() {
        let f = Fixture::new(config(2), vec![Ok(reply()), Err(CallError::Http(503))]).await;
        let result = f.judge.call(Position::Extract, request()).await.unwrap();
        assert_eq!(result.backend, "local");
        assert_eq!(result.reply, reply());
        assert_eq!(
            f.judge.call(Position::Triage, request()).await.unwrap_err(),
            ServiceError::Call(CallError::Http(503))
        );
        assert_eq!(f.judge.used_today().await.unwrap(), 2);
        assert_eq!(f.judge.daily_calls(), 2);
        assert_eq!(
            f.judge.call(Position::Extract, request()).await.unwrap_err(),
            ServiceError::Budget { used: 2, limit: 2 }
        );
        let rows = sqlx::query("SELECT * FROM judge_calls ORDER BY rowid").fetch_all(f.store.pool()).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<String, _>("id"), result.call_id);
        assert_eq!(rows[0].get::<String, _>("position"), "extract");
        assert_eq!(rows[0].get::<String, _>("kind"), "api");
        assert_eq!(rows[0].get::<String, _>("model"), "m");
        assert_eq!(rows[0].get::<i64, _>("input_bytes"), (request().system.len() + request().prompt.len()) as i64);
        assert_eq!(rows[0].get::<i64, _>("output_bytes"), reply().text.len() as i64);
        assert_eq!(rows[0].get::<i64, _>("input_tokens"), 12);
        assert_eq!(rows[0].get::<i64, _>("output_tokens"), 3);
        assert_eq!(rows[0].get::<f64, _>("cost_usd"), 0.1);
        assert_eq!(rows[0].get::<String, _>("status"), "ok");
        assert_eq!(rows[1].get::<String, _>("status"), "http");
        assert_eq!(rows[1].get::<i64, _>("output_bytes"), 0);
        assert!(rows[1].get::<Option<i64>, _>("input_tokens").is_none());
        assert_eq!(f.fake.requests.lock().unwrap()[0].prompt, request().prompt);
        let columns: Vec<String> = sqlx::query("PRAGMA table_info(judge_calls)")
            .fetch_all(f.store.pool())
            .await
            .unwrap()
            .iter()
            .map(|r| r.get("name"))
            .collect();
        for content in ["prompt", "system", "text", "api_key"] {
            assert!(!columns.contains(&content.to_string()));
        }
        f.close().await;
    }

    #[tokio::test]
    async fn busy_shutdown_and_dropped_callers_still_finalize_records() {
        let f = Fixture::new(config(20), vec![Ok(reply())]).await;
        *f.fake.delay.lock().unwrap() = Duration::from_secs(60);
        let judge = f.judge.clone();
        let call = tokio::spawn(async move { judge.call(Position::Extract, request()).await });
        wait_called(&f.fake).await;
        assert_eq!(f.judge.call(Position::Triage, request()).await.unwrap_err(), ServiceError::Busy);
        f.shutdown.send(true).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), call).await.unwrap().unwrap().unwrap_err(),
            ServiceError::Call(CallError::Cancelled)
        );
        assert_eq!(f.judge.used_today().await.unwrap(), 1);
        let status: String =
            sqlx::query_scalar("SELECT status FROM judge_calls").fetch_one(f.store.pool()).await.unwrap();
        assert_eq!(status, "cancelled");
        f.close().await;

        let f = Fixture::new(config(20), vec![Ok(reply())]).await;
        *f.fake.delay.lock().unwrap() = Duration::from_millis(100);
        let permit = f.judge.begin(Position::Extract).await.unwrap();
        let call = tokio::spawn(async move { permit.call(request()).await });
        wait_called(&f.fake).await;
        call.abort();
        f.judge.wait_idle().await;
        let status: String =
            sqlx::query_scalar("SELECT status FROM judge_calls").fetch_one(f.store.pool()).await.unwrap();
        assert_eq!(status, "ok");
        f.close().await;
    }

    #[tokio::test]
    async fn budget_resets_at_local_midnight_and_retention_runs() {
        let f = Fixture::new(config(1), vec![Ok(reply()), Ok(reply())]).await;
        let midnight = local_day_start(1_800_000_000_000).unwrap();
        f.judge.clock().set(midnight - 1);
        f.judge.call(Position::Extract, request()).await.unwrap();
        assert_eq!(f.judge.used_today().await.unwrap(), 1);
        f.judge.clock().set(midnight);
        assert_eq!(f.judge.used_today().await.unwrap(), 0);
        f.judge.call(Position::Extract, request()).await.unwrap();
        assert_eq!(f.store.count_judge_calls_since(0).await.unwrap(), 2);
        f.judge.clock().set(midnight + 90 * 24 * 3600 * 1000);
        f.judge.cleanup().await.unwrap();
        assert_eq!(f.store.count_judge_calls_since(0).await.unwrap(), 1);
        f.close().await;
    }

    #[tokio::test]
    async fn startup_recovers_interrupted_records_and_configuration_changes_take_effect() {
        let f = Fixture::new(config(20), vec![Ok(reply())]).await;
        let id = f
            .store
            .start_judge_call(&JudgeCallStart {
                position: "extract",
                backend: "local",
                kind: "api",
                model: None,
                started_ms: f.judge.clock().now(),
                input_bytes: 10,
            })
            .await
            .unwrap();
        let run: anycon_protocol::ExtractRun = serde_json::from_value(json!({
            "id":"interrupted", "session_id":"s", "trigger":"auto", "status":"running",
            "started_ms":f.judge.clock().now(), "points":1, "from_seq":1, "to_seq":1,
            "added":[], "dropped":{"invalid":0,"duplicate":0,"full":0}, "backend":"local"
        }))
        .unwrap();
        f.store.insert_extract_run(&run).await.unwrap();
        let recovered = Judge::new(
            f.judge.config.clone(),
            f.store.clone(),
            f.judge.env.clone(),
            f.judge.agents.clone(),
            f.shutdown.subscribe(),
        )
        .await
        .unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM judge_calls WHERE id = ?")
            .bind(&id)
            .fetch_one(f.store.pool())
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(
            f.store.get_extract_run("interrupted").await.unwrap().unwrap().error.unwrap().code,
            JudgeErrorCode::Cancelled
        );
        assert_eq!(recovered.used_today().await.unwrap(), 1);
        let path = f.judge.config.paths().config_toml();
        std::fs::write(
            &path,
            "[judge]\n\
             triage='changed'\n\
             daily_calls=0\n\
             [judge.backends.changed]\n\
             kind='api'\n\
             format='openai'\n\
             model='new'\n\
             base_url='http://localhost'",
        )
        .unwrap();
        f.judge.config.reload().unwrap();
        assert_eq!(f.judge.daily_calls(), 0);
        assert_eq!(f.judge.info().extract.as_deref(), Some("changed"));
        assert_eq!(
            f.judge.call(Position::Extract, request()).await.unwrap_err(),
            ServiceError::Budget { used: 1, limit: 0 }
        );
        std::fs::write(
            &path,
            "[judge]\n\
             triage='changed'\n\
             daily_calls=3\n\
             [judge.backends.changed]\n\
             kind='api'\n\
             format='openai'\n\
             model='new'\n\
             base_url='http://localhost'",
        )
        .unwrap();
        f.judge.config.reload().unwrap();
        let b = f.fake.clone();
        f.judge.set_backend_factory(Arc::new(move |cfg| {
            let JudgeBackendConfig::Api(c) = cfg else { panic!() };
            assert_eq!(c.model, "new");
            b.clone()
        }));
        assert_eq!(f.judge.call(Position::Extract, request()).await.unwrap().backend, "changed");
        f.close().await;
    }

    #[test]
    fn info_reports_missing_keys_agents_and_unsupported_agents_without_secrets() {
        let mut cfg: Config = serde_json::from_value(json!({"judge":{"triage":"a", "backends":{
            "a":{
                "kind":"api","format":"anthropic","model":"m",
                "api_key_env":"PRIVATE_ENV","base_url":"https://private.example"
            },
            "b":{"kind":"api","format":"openai","model":"m","api_key":"PRIVATE_KEY"},
            "c":{"kind":"agent","agent":"fake"}
        }}}))
        .unwrap();
        let info = describe(&cfg, &HashMap::new(), &[]);
        assert_eq!(info.extract.as_deref(), Some("a"));
        assert_eq!(info.triage, info.extract);
        assert_eq!(info.backends[0].problem, Some(JudgeProblem::NoKey));
        assert_eq!(info.backends[1].problem, None);
        assert_eq!(info.backends[2].problem, Some(JudgeProblem::AgentMissing));
        let text = format!("{info:?} {}", serde_json::to_string(&info).unwrap());
        for secret in ["PRIVATE_ENV", "PRIVATE_KEY", "private.example", "base_url"] {
            assert!(!text.contains(secret));
        }
        for value in ["", "key"] {
            let env = HashMap::from([("PRIVATE_ENV".into(), value.into())]);
            assert_eq!(describe(&cfg, &env, &[]).backends[0].problem.is_none(), !value.is_empty());
        }
        let mut agent: AgentInfo = serde_json::from_value(
            json!({"id":"fake","label":"fake","driver":"mock","enabled":true,"status":"ready","probed_at_ms":0}),
        )
        .unwrap();
        assert_eq!(
            describe(&cfg, &HashMap::new(), &[agent.clone()]).backends[2].problem,
            Some(JudgeProblem::AgentUnsupported)
        );
        agent.capabilities.oneshot = true;
        assert_eq!(describe(&cfg, &HashMap::new(), &[agent.clone()]).backends[2].problem, None);
        for status in [AgentStatus::NotInstalled, AgentStatus::Disabled, AgentStatus::Loading] {
            agent.status = status;
            assert_eq!(
                describe(&cfg, &HashMap::new(), &[agent.clone()]).backends[2].problem,
                Some(JudgeProblem::AgentMissing)
            );
        }
        agent.status = AgentStatus::Ready;
        agent.enabled = false;
        assert_eq!(describe(&cfg, &HashMap::new(), &[agent]).backends[2].problem, Some(JudgeProblem::AgentMissing));
        cfg.judge.extract = "absent".into();
        assert!(describe(&cfg, &HashMap::new(), &[]).extract.is_none());
        assert_eq!(protocol_error(CallError::Http(502)), JudgeError { code: JudgeErrorCode::Http, status: Some(502) });
    }
}
