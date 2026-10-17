//! 集成测试共用的小工具：假 `claude`、临时目录、事件收集。
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_adapters::claude_code::Timings;
use anycon_adapters::common::ShutdownTimings;
use anycon_adapters::{ClaudeCodeAdapter, CodexAdapter};
use anycon_agent_api::{
    Adapter, AdapterConfig, AgentEvent, AgentHandle, ProbeContext, ResumeHandle, SpawnRequest, UserInput,
};
use anycon_protocol::{AgentInfo, Event, SessionSettings};
use serde_json::Value;

pub fn fake_claude() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/bin/fake-claude")
}

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name)
}

/// 测试里把等待时间都缩短。
pub fn fast_timings() -> Timings {
    Timings {
        interrupt_fallback: Duration::from_millis(400),
        control_timeout: Duration::from_secs(10),
        shutdown: ShutdownTimings { after_stdin_close: Duration::from_millis(150), grace: Duration::from_millis(600) },
    }
}

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub project: PathBuf,
    pub log: PathBuf,
    pub env: HashMap<String, String>,
    pub config: AdapterConfig,
}

impl Harness {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let log = dir.path().join("fake.log");
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
        env.insert("HOME".to_string(), dir.path().to_string_lossy().into_owned());
        env.insert("FAKE_CLAUDE_LOG".to_string(), log.to_string_lossy().into_owned());
        let config = AdapterConfig {
            enabled: true,
            command: Some(fake_claude().to_string_lossy().into_owned()),
            ..Default::default()
        };
        Self { dir, project, log, env, config }
    }

    pub fn var(mut self, k: &str, v: &str) -> Self {
        self.env.insert(k.to_string(), v.to_string());
        self
    }

    pub fn script(self, fixture: &str) -> Self {
        let p = fixture_path(fixture);
        self.var("FAKE_CLAUDE_SCRIPT", &p.to_string_lossy())
    }

    pub fn probe_ctx(&self) -> ProbeContext {
        ProbeContext {
            env: Arc::new(self.env.clone()),
            config: self.config.clone(),
            deadline: Duration::from_secs(20),
            cheap: false,
            previous: None,
            scratch_dir: self.dir.path().to_path_buf(),
        }
    }

    pub async fn info(&self) -> Arc<AgentInfo> {
        Arc::new(ClaudeCodeAdapter::new().probe(&self.probe_ctx()).await)
    }

    pub async fn request(&self, settings: SessionSettings, resume: Option<ResumeHandle>) -> SpawnRequest {
        SpawnRequest {
            session_id: "sess_test".into(),
            cwd: self.project.clone(),
            settings,
            env: Arc::new(self.env.clone()),
            config: self.config.clone(),
            resume,
            agent_info: self.info().await,
        }
    }

    pub async fn spawn(&self, settings: SessionSettings, resume: Option<ResumeHandle>) -> Box<dyn AgentHandle> {
        let req = self.request(settings, resume).await;
        ClaudeCodeAdapter::with_timings(fast_timings()).spawn(req).await.expect("spawn")
    }

    /// fake-claude 写下的日志（每行一个 JSON）。
    pub fn log_entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn argv(&self) -> Vec<String> {
        self.log_entries()
            .into_iter()
            .find(|e| e["kind"] == "argv")
            .map(|e| e["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    /// 子进程收到的所有 stdin 消息。
    pub fn stdin_messages(&self) -> Vec<Value> {
        self.log_entries().into_iter().filter(|e| e["kind"] == "stdin").map(|e| e["msg"].clone()).collect()
    }

    /// 等日志里出现满足条件的 stdin 消息。
    pub async fn wait_stdin(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(m) = self.stdin_messages().into_iter().find(|m| pred(m)) {
                return m;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for stdin message: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

pub fn input(text: &str) -> UserInput {
    UserInput {
        message_id: "m".into(),
        interject: false,
        content: text.into(),
        attachments: vec![],
        attachment_paths: vec![],
    }
}

/// 插话：送进正在进行的 turn。
pub fn interject(text: &str) -> UserInput {
    UserInput { interject: true, ..input(text) }
}

pub async fn next_event(h: &mut Box<dyn AgentHandle>) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(15), h.events().recv())
        .await
        .expect("timed out waiting for an agent event")
        .expect("event channel closed")
}

/// 收事件直到 `pred` 命中（含）。
pub async fn collect_until(h: &mut Box<dyn AgentHandle>, pred: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next_event(h).await;
        let hit = pred(&ev);
        out.push(ev);
        if hit {
            return out;
        }
    }
}

pub fn is_turn_done(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::Event(Event::TurnDone { .. }))
}

pub fn is_permission(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::Permission(_))
}

pub fn is_exited(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::Exited { .. })
}

/// 把事件压成可读的类型名，便于断言顺序。
pub fn names(evs: &[AgentEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| match e {
            AgentEvent::Event(ev) => serde_json::to_value(ev).unwrap()["type"].as_str().unwrap().to_string(),
            AgentEvent::Permission(_) => "PERMISSION".into(),
            AgentEvent::Started { .. } => "STARTED".into(),
            AgentEvent::Title(_) => "TITLE".into(),
            AgentEvent::Exited { .. } => "EXITED".into(),
        })
        .collect()
}

pub fn text_of(evs: &[AgentEvent]) -> String {
    evs.iter()
        .filter_map(|e| if let AgentEvent::Event(Event::TextDelta { text }) = e { Some(text.as_str()) } else { None })
        .collect()
}

/// 进程是否还活着（`kill -0`）。
pub fn pid_alive(pid: u64) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

// ---------------------------------------------------------------- Codex

pub fn fake_codex() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/bin/fake-codex")
}

pub fn codex_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex").join(name)
}

/// 测试里把等待时间都缩短。
pub fn codex_fast_timings() -> anycon_adapters::codex::Timings {
    anycon_adapters::codex::Timings {
        interrupt_fallback: Duration::from_millis(500),
        handshake: Duration::from_secs(10),
        request: Duration::from_secs(10),
        shutdown: ShutdownTimings { after_stdin_close: Duration::from_millis(150), grace: Duration::from_millis(600) },
    }
}

/// 用假 `codex`（`tests/bin/fake-codex`）的一套环境。
pub struct CodexHarness {
    pub dir: tempfile::TempDir,
    pub project: PathBuf,
    pub log: PathBuf,
    pub env: HashMap<String, String>,
    pub config: AdapterConfig,
    pub timings: anycon_adapters::codex::Timings,
}

impl CodexHarness {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let log = dir.path().join("fake-codex.log");
        let mut env = HashMap::new();
        env.insert("PATH".to_string(), std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()));
        env.insert("HOME".to_string(), dir.path().to_string_lossy().into_owned());
        env.insert("FAKE_CODEX_LOG".to_string(), log.to_string_lossy().into_owned());
        // 开发机忙（别的 cargo 在编译）时 python 冷启动可能超过 2 秒
        env.insert("ANYCON_VERSION_TIMEOUT_MS".to_string(), "20000".to_string());
        let config = AdapterConfig {
            enabled: true,
            command: Some(fake_codex().to_string_lossy().into_owned()),
            ..Default::default()
        };
        Self { dir, project, log, env, config, timings: codex_fast_timings() }
    }

    pub fn var(mut self, k: &str, v: &str) -> Self {
        self.env.insert(k.to_string(), v.to_string());
        self
    }

    pub fn script(self, fixture: &str) -> Self {
        let p = codex_fixture_path(fixture);
        self.var("FAKE_CODEX_SCRIPT", &p.to_string_lossy())
    }

    pub fn probe_ctx(&self) -> ProbeContext {
        ProbeContext {
            env: Arc::new(self.env.clone()),
            config: self.config.clone(),
            deadline: Duration::from_secs(40),
            cheap: false,
            previous: None,
            scratch_dir: self.dir.path().to_path_buf(),
        }
    }

    pub async fn info(&self) -> Arc<AgentInfo> {
        Arc::new(CodexAdapter::new().probe(&self.probe_ctx()).await)
    }

    pub async fn request(&self, settings: SessionSettings, resume: Option<ResumeHandle>) -> SpawnRequest {
        let info = self.info().await;
        // 探测会写一条 argv 日志；清掉，让后面的断言只看会话进程
        let _ = std::fs::remove_file(&self.log);
        SpawnRequest {
            session_id: "sess_test".into(),
            cwd: self.project.clone(),
            settings,
            env: Arc::new(self.env.clone()),
            config: self.config.clone(),
            resume,
            agent_info: info,
        }
    }

    pub async fn spawn(&self, settings: SessionSettings, resume: Option<ResumeHandle>) -> Box<dyn AgentHandle> {
        self.try_spawn(settings, resume).await.expect("spawn")
    }

    pub async fn try_spawn(
        &self,
        settings: SessionSettings,
        resume: Option<ResumeHandle>,
    ) -> anycon_agent_api::Result<Box<dyn AgentHandle>> {
        let req = self.request(settings, resume).await;
        CodexAdapter::with_timings(self.timings).spawn(req).await
    }

    /// fake-codex 写下的日志（每行一个 JSON）。
    pub fn log_entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn argv_entry(&self) -> Value {
        self.log_entries().into_iter().find(|e| e["kind"] == "argv").unwrap_or(Value::Null)
    }

    /// 客户端发给 app-server 的全部消息。
    pub fn stdin_messages(&self) -> Vec<Value> {
        self.log_entries().into_iter().filter(|e| e["kind"] == "stdin").map(|e| e["msg"].clone()).collect()
    }

    /// 某个 method 的全部请求 / 通知。
    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.stdin_messages().into_iter().filter(|m| m["method"] == method).collect()
    }

    /// 客户端对服务端请求的答复（有 `result` / `error`、没有 `method`）。
    pub fn answers(&self) -> Vec<Value> {
        self.stdin_messages().into_iter().filter(|m| m.get("method").is_none() && m.get("id").is_some()).collect()
    }

    /// 等日志里出现满足条件的消息。
    pub async fn wait_stdin(&self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            if let Some(m) = self.stdin_messages().into_iter().find(|m| pred(m)) {
                return m;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for stdin message: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// 最常用的一套设置：只指定模式。
pub fn mode(m: &str) -> SessionSettings {
    SessionSettings { mode: Some(m.into()), ..Default::default() }
}
