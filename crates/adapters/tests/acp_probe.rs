//! ACP 探测测试：对着假 ACP agent（`tests/bin/fake-acp-agent`）跑 `AcpAdapter::probe`：找命令、`--version`、
//! `initialize` + `session/new` 取模型 / 模式 / 选项、认证、cheap 探测、诊断。
//!
//! 假 CLI 是带 shebang 的 python 脚本，Windows 上不能直接执行：这个文件只在 unix 上跑。
#![cfg(unix)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use anycon_agent_api::{Adapter, AdapterConfig, ProbeContext};
use anycon_protocol::{AgentInfo, AgentStatus, ConfigOptionKind, ModeRisk};
use common::*;

fn ctx_with_command(h: &AcpHarness, command: Option<&str>) -> ProbeContext {
    let mut ctx = h.probe_ctx();
    ctx.config = AdapterConfig { enabled: true, command: command.map(str::to_string), ..Default::default() };
    ctx
}

fn issue_code(info: &AgentInfo) -> String {
    info.issue.as_ref().map(|i| i.code.clone()).unwrap_or_default()
}

#[tokio::test]
async fn a_ready_agent_declares_models_modes_options_and_capabilities() {
    let h = AcpHarness::new();
    let info = h.info().await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!((info.id.as_str(), info.driver.as_str()), ("fakeacp", "acp"));
    // 自己加的 agent 的名字来自它的 `agentInfo`；版本也是（它优先于 `--version` 的 1.0.0）
    assert_eq!((info.label.as_str(), info.version.as_deref()), ("Fake ACP", Some("1.2.3")));
    assert!(same_path(std::path::Path::new(info.path.as_deref().unwrap()), &fake_acp()));
    assert!(info.enabled && !info.allow_custom_model);

    assert_eq!(
        info.models.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
        vec![("fast", true), ("smart", false)]
    );
    assert_eq!(info.models[0].label, "Fast model");
    assert_eq!(info.models[0].description.as_deref(), Some("quick"));
    assert_eq!(info.models[0].tags, vec!["Group"]);
    let th = info.models[0].thinking.as_ref().unwrap();
    assert_eq!(th.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["low", "medium", "high"]);
    assert_eq!(th.default.as_deref(), Some("medium"));

    assert_eq!(
        info.modes.iter().map(|m| (m.id.as_str(), m.default, m.risk)).collect::<Vec<_>>(),
        vec![("ask", true, ModeRisk::Normal), ("code", false, ModeRisk::Normal), ("plan", false, ModeRisk::Normal)]
    );
    assert_eq!(info.modes[0].description.as_deref(), Some("Ask before changes"));

    assert_eq!(info.options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["brave", "ctx"]);
    assert_eq!(
        (info.options[0].kind, info.options[0].default.clone()),
        (ConfigOptionKind::Bool, serde_json::json!(false))
    );
    assert_eq!(
        (info.options[1].kind, info.options[1].default.clone()),
        (ConfigOptionKind::Select, serde_json::json!("200k"))
    );
    assert_eq!(info.options[1].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec!["200k", "1m"]);

    let c = &info.capabilities;
    assert!(c.resume && c.interrupt && !c.interject && c.thinking_stream && c.usage);
    assert!(!c.structured_output && !c.env && !c.rewind);
    assert_eq!(c.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp"]);
}

#[tokio::test]
async fn the_probe_closes_its_session_and_leaves_no_process() {
    let h = AcpHarness::new();
    let info = h.adapter.probe(&h.probe_ctx()).await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert_eq!(h.methods(), vec!["initialize", "session/new", "session/close"]);
    // 探测用的会话在 scratch 目录里，不在任何项目里
    let cwd = h.requests("session/new")[0]["params"]["cwd"].as_str().unwrap().to_string();
    assert!(same_path(std::path::Path::new(&cwd), h.dir.path()), "{cwd}");
    let pid = h.launches()[0]["pid"].as_u64().unwrap();
    assert!(wait_gone(pid).await);
    // 没有 session/close 的 agent：不发，进程照样退出
    let h = AcpHarness::new().var("FAKE_ACP_CAPS", "load,image");
    h.adapter.probe(&h.probe_ctx()).await;
    assert_eq!(h.methods(), vec!["initialize", "session/new"]);
    let pid = h.launches()[0]["pid"].as_u64().unwrap();
    assert!(wait_gone(pid).await);
}

#[tokio::test]
async fn a_registry_agent_keeps_its_registry_name_and_a_config_command_overrides_the_registry_command() {
    let h = AcpHarness::with_id("opencode");
    let info = h.info().await;
    assert_eq!((info.id.as_str(), info.label.as_str()), ("opencode", "OpenCode"), "registry label wins over agentInfo");
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
}

#[tokio::test]
async fn a_missing_command_is_not_installed_with_a_hint() {
    // 注册表里的 agent：hint 是它的安装方式
    let h = AcpHarness::with_id("opencode");
    let info = h.adapter.probe(&ctx_with_command(&h, Some("definitely-not-an-installed-acp-agent acp"))).await;
    assert_eq!(info.status, AgentStatus::NotInstalled);
    assert_eq!(issue_code(&info), "binary_not_found");
    assert!(info.issue.as_ref().unwrap().hint.as_deref().unwrap().contains("opencode.ai"));
    assert!(info.issue.as_ref().unwrap().message.contains("definitely-not-an-installed-acp-agent"));
    assert_eq!(h.launches().len(), 0, "nothing was started");
    // 自己加的：让用户检查命令
    let h = AcpHarness::with_id("mine");
    let info = h.adapter.probe(&ctx_with_command(&h, Some("no-such-binary-xyz"))).await;
    assert_eq!(info.status, AgentStatus::NotInstalled);
    assert!(info.issue.as_ref().unwrap().hint.as_deref().unwrap().contains("agents.acp.mine.command"));
    // 绝对路径不存在
    let info = h.adapter.probe(&ctx_with_command(&h, Some("/definitely/not/here/agent acp"))).await;
    assert_eq!(info.status, AgentStatus::NotInstalled);
}

#[tokio::test]
async fn bad_commands_are_errors_not_installs() {
    let h = AcpHarness::with_id("mine");
    // 相对路径会按子进程的 cwd 解析，不接受
    let info = h.adapter.probe(&ctx_with_command(&h, Some("./agent acp"))).await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "invalid_command"));
    let info = h.adapter.probe(&ctx_with_command(&h, Some("agent 'unclosed"))).await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "invalid_command"));
    // 自己加的 agent 没写命令
    let info = h.adapter.probe(&ctx_with_command(&h, None)).await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "no_command"));
    assert!(info.issue.unwrap().hint.unwrap().contains("agents.acp.mine.command"));
    // 文件存在但不能执行
    let not_exec = h.dir.path().join("plain.txt");
    std::fs::write(&not_exec, "hi").unwrap();
    let info = h.adapter.probe(&ctx_with_command(&h, Some(&format!("{} acp", not_exec.display())))).await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "binary_failed"));
}

#[tokio::test]
async fn a_disabled_agent_is_not_started() {
    let h = AcpHarness::new();
    let mut ctx = h.probe_ctx();
    ctx.config.enabled = false;
    let info = h.adapter.probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::Disabled);
    assert!(!info.enabled);
    assert!(info.issue.unwrap().hint.unwrap().contains("agents.acp.fakeacp.enabled"));
    assert_eq!(h.launches().len(), 0);
}

#[tokio::test]
async fn an_agent_that_does_not_know_version_is_still_installed() {
    let h = AcpHarness::new().var("FAKE_ACP_VERSION_EXIT", "2").var("FAKE_ACP_VERSION_TEXT", "usage: unknown flag");
    let info = h.info().await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.version.as_deref(), Some("1.2.3"), "the version comes from initialize");
}

#[tokio::test]
async fn login_required_is_not_authenticated_with_the_login_methods_in_the_message() {
    let h = AcpHarness::new().var("FAKE_ACP_AUTH", "required");
    let info = h.info().await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    let issue = info.issue.clone().unwrap();
    assert_eq!(issue.code, "login_required");
    assert!(issue.message.contains("Log in with Fake"), "{}", issue.message);
    assert!(issue.hint.unwrap().contains("agent's own CLI"));
    // 登录之前看不到模型，但能力是知道的
    assert!(info.models.is_empty());
    assert!(info.capabilities.resume);
    // terminal 类型的登录方式：给出要在机器上跑的命令
    let h = AcpHarness::new().var("FAKE_ACP_AUTH", "terminal");
    let info = h.info().await;
    let hint = info.issue.clone().unwrap().hint.unwrap();
    assert!(hint.contains("fake-acp-agent acp --login"), "{hint}");
}

#[tokio::test]
async fn startup_failures_report_the_agents_stderr_and_protocol_mismatches_are_unsupported_versions() {
    let h = AcpHarness::new().var("FAKE_ACP_INIT", "fail");
    let info = h.info().await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "handshake_failed"));
    assert!(info.issue.clone().unwrap().message.contains("missing config"));
    let h = AcpHarness::new().var("FAKE_ACP_INIT", "v2");
    let info = h.info().await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::UnsupportedVersion, "protocol_version"));
    // 起得来但永远不回 initialize：总预算到了就报超时，不无限等
    let h = AcpHarness::new().var("FAKE_ACP_INIT", "hang");
    let mut ctx = h.probe_ctx();
    ctx.deadline = Duration::from_secs(4);
    let t = std::time::Instant::now();
    let info = h.adapter.probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::Error);
    assert!(t.elapsed() < Duration::from_secs(30), "{:?}", t.elapsed());
}

#[tokio::test]
async fn an_exhausted_budget_is_a_probe_timeout() {
    let h = AcpHarness::new();
    let mut ctx = h.probe_ctx();
    ctx.deadline = Duration::ZERO;
    let info = h.adapter.probe(&ctx).await;
    assert_eq!((info.status, issue_code(&info).as_str()), (AgentStatus::Error, "probe_timeout"));
    assert_eq!(h.launches().len(), 0);
}

#[tokio::test]
async fn agents_without_config_options_or_with_legacy_modes_declare_what_they_have() {
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "legacy");
    let info = h.info().await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert!(info.models.is_empty() && info.options.is_empty());
    assert_eq!(
        info.modes.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(),
        vec![("ask", true), ("code", false)]
    );
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "none");
    let info = h.info().await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert!(info.models.is_empty() && info.modes.is_empty() && info.options.is_empty());
    // 有选项但没有模型选择器：不提供模型选择
    let h = AcpHarness::new().var("FAKE_ACP_CONFIG", "nomodel");
    let info = h.info().await;
    assert!(info.models.is_empty());
    assert_eq!(info.modes.len(), 3);
}

#[tokio::test]
async fn capabilities_follow_what_the_agent_declares() {
    for (caps, resume, images) in
        [("load", true, false), ("resume", true, false), ("image", false, true), ("", false, false)]
    {
        let h = AcpHarness::new().var("FAKE_ACP_CAPS", caps);
        let info = h.info().await;
        assert_eq!(info.capabilities.resume, resume, "caps {caps:?}");
        assert_eq!(!info.capabilities.attachments.is_empty(), images, "caps {caps:?}");
    }
}

#[tokio::test]
async fn a_cheap_probe_reuses_the_last_result_until_something_changes() {
    let h = AcpHarness::new();
    let full = Arc::new(h.adapter.probe(&h.probe_ctx()).await);
    assert_eq!(full.status, AgentStatus::Ready);
    let launches = h.launches().len();
    assert_eq!(launches, 1);

    // 路径和 `--version` 都没变：不拉起 agent，结果一样
    let mut cheap = h.probe_ctx();
    cheap.cheap = true;
    cheap.previous = Some((*full).clone());
    let again = h.adapter.probe(&cheap).await;
    assert_eq!(h.launches().len(), launches, "a cheap probe starts no agent");
    assert_eq!(
        (again.models.clone(), again.modes.clone(), again.version.clone()),
        (full.models.clone(), full.modes.clone(), full.version.clone())
    );
    assert_eq!(again.status, AgentStatus::Ready);

    // 上一次不是 ready（登录状态 cheap 探测看不出来）：做完整探测
    let mut was_broken = h.probe_ctx();
    was_broken.cheap = true;
    let mut prev = (*full).clone();
    prev.status = AgentStatus::NotAuthenticated;
    was_broken.previous = Some(prev);
    let info = h.adapter.probe(&was_broken).await;
    assert_eq!(h.launches().len(), launches + 1);
    assert_eq!(info.status, AgentStatus::Ready);

    // `--version` 的输出变了（升级）：也做完整探测
    let mut upgraded = cheap.clone();
    let mut env = (*upgraded.env).clone();
    env.insert("FAKE_ACP_VERSION_TEXT".into(), "fake-acp 2.0.0".into());
    upgraded.env = Arc::new(env);
    let info = h.adapter.probe(&upgraded).await;
    assert_eq!(h.launches().len(), launches + 2);
    assert_eq!(info.status, AgentStatus::Ready);
}

#[tokio::test]
async fn diagnose_lists_the_command_the_login_methods_and_the_capabilities() {
    let h = AcpHarness::new().var("FAKE_ACP_AUTH", "terminal");
    let (text, fields) = h.adapter.diagnose(&h.probe_ctx()).await.expect("ACP agents have their own diagnosis");
    let value = |label: &str| {
        fields.iter().find(|f| f.label == label).unwrap_or_else(|| panic!("no field {label}: {text}")).value.clone()
    };
    assert!(value("command").ends_with("fake-acp-agent acp"));
    assert_eq!(value("command source"), "config");
    assert_eq!(value("ACP protocol version"), "1");
    assert!(value("agent (initialize)").contains("Fake ACP"));
    assert!(value("login methods").contains("Terminal login (terminal)"));
    assert!(value("agent capabilities").contains("resume=true"));
    assert_eq!(value("status"), "notauthenticated");
    assert!(value("issue").contains("login_required"));
    assert!(text.contains("fakeacp") && text.contains("login methods"));
    // 每个探测过的候选都列出来
    assert!(value("which -a / candidates").contains("fake-acp-agent"));
}
