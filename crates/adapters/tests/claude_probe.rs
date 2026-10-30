//! Claude Code 探测：用 PATH 里的假 `claude`（`tests/bin/fake-claude`）回答 `--version` 与 `auth status`。

mod common;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_adapters::claude_code::probe::{self, AuthState};
use anycon_adapters::ClaudeCodeAdapter;
use anycon_agent_api::{Adapter, AdapterConfig, ProbeContext};
use anycon_protocol::{AgentStatus, ConfigOptionKind, ModeRisk, ModelInfo};
use common::fake_claude;

/// 只含 python3 所在目录（假 CLI 是 python3 脚本）和系统目录；不含开发机上真实的 claude。
fn minimal_path() -> String {
    let py = which::which("python3").ok().and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()));
    match py {
        Some(d) => format!("{d}:/usr/bin:/bin"),
        None => "/usr/bin:/bin".to_string(),
    }
}

struct Setup {
    dir: tempfile::TempDir,
    env: HashMap<String, String>,
    config: AdapterConfig,
}

/// 写一个叫 `claude` 的脚本，转调假 CLI。
fn install_claude(dir: &Path, body: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("claude");
    let script = match body {
        Some(b) => format!("#!/bin/sh\n{b}\n"),
        None => format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", fake_claude().display()),
    };
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

impl Setup {
    /// PATH 里只有一个目录，放着能用的假 claude。
    fn with_claude_on_path() -> Self {
        let dir = tempfile::tempdir().unwrap();
        install_claude(&dir.path().join("bin"), None);
        let mut s = Self::bare(dir);
        let path = format!("{}:{}", s.dir.path().join("bin").display(), s.base_path());
        s.env.insert("PATH".into(), path);
        s
    }

    /// PATH 只有系统目录，HOME 指向临时目录。
    fn bare(dir: tempfile::TempDir) -> Self {
        let mut env = HashMap::new();
        env.insert("HOME".to_string(), dir.path().to_string_lossy().into_owned());
        env.insert("PATH".to_string(), minimal_path());
        env.insert("SHELL".to_string(), "/bin/sh".to_string());
        // 开发机忙（别的 cargo 在编译）时 python 冷启动可能超过 2 秒，放宽探活超时避免偶发失败
        env.insert("ANYCON_VERSION_TIMEOUT_MS".to_string(), "20000".to_string());
        Self { dir, env, config: AdapterConfig { enabled: true, ..Default::default() } }
    }

    fn base_path(&self) -> String {
        minimal_path()
    }

    fn var(mut self, k: &str, v: &str) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    fn ctx(&self) -> ProbeContext {
        ProbeContext {
            env: Arc::new(self.env.clone()),
            config: self.config.clone(),
            deadline: Duration::from_secs(30),
            cheap: false,
            previous: None,
            scratch_dir: self.dir.path().to_path_buf(),
        }
    }

    async fn probe(&self) -> anycon_protocol::AgentInfo {
        ClaudeCodeAdapter::new().probe(&self.ctx()).await
    }
}

#[tokio::test]
async fn ready_when_installed_and_logged_in() {
    let s = Setup::with_claude_on_path();
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert!(info.enabled);
    assert_eq!(
        (info.id.as_str(), info.label.as_str(), info.driver.as_str()),
        ("claude-code", "Claude Code", "claude-stream-json")
    );
    assert_eq!(info.version.as_deref(), Some("2.1.293"));
    assert_eq!(info.path.as_deref(), Some(s.dir.path().join("bin/claude").to_str().unwrap()));
    assert!(info.issue.is_none());
    assert!(info.probed_at_ms > 0);
    assert!(info.allow_custom_model);

    // 模型：内置表，一个默认，思考等级挂在模型上
    assert_eq!(
        info.models.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["claude-opus-5-5"]
    );
    let opus = info.models.iter().find(|m| m.id == "claude-opus-5-5").unwrap();
    assert_eq!(opus.thinking.as_ref().unwrap().default.as_deref(), Some("medium"));
    let old = info.models.iter().find(|m| m.id == "claude-opus-4-6").unwrap();
    assert!(old.thinking.as_ref().unwrap().options.iter().any(|o| o.id == "off"));
    assert!(info.models.iter().any(|m| m.id == "sonnet" || m.id == "opus"));

    // 权限模式
    let modes: Vec<(&str, ModeRisk, bool)> = info.modes.iter().map(|m| (m.id.as_str(), m.risk, m.default)).collect();
    assert_eq!(
        modes,
        vec![
            ("default", ModeRisk::Normal, true),
            ("acceptEdits", ModeRisk::Normal, false),
            ("plan", ModeRisk::Safe, false),
            ("auto", ModeRisk::Normal, false),
            ("dontAsk", ModeRisk::Normal, false),
            ("bypassPermissions", ModeRisk::Dangerous, false),
        ]
    );
    assert_eq!(info.options.len(), 1);
    assert_eq!((info.options[0].id.as_str(), info.options[0].kind), ("fast_mode", ConfigOptionKind::Bool));

    let c = &info.capabilities;
    assert!(c.resume && c.interrupt && c.thinking_stream && c.usage && c.structured_output && c.env);
    assert!(!c.interject);
    // 图片附件：四种，不多不少（ADR-35）
    assert_eq!(c.attachments, vec!["image/png", "image/jpeg", "image/gif", "image/webp", "*/*"]);
}

#[tokio::test]
async fn cheap_probe_gives_the_same_answer() {
    let s = Setup::with_claude_on_path();
    let full = s.probe().await;
    let mut ctx = s.ctx();
    ctx.cheap = true;
    ctx.previous = Some(full.clone());
    let cheap = ClaudeCodeAdapter::new().probe(&ctx).await;
    assert_eq!(cheap.status, full.status);
    assert_eq!(cheap.models, full.models);
}

#[tokio::test]
async fn not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Setup::bare(dir);
    // 不能让真实机器上的 claude 被找到
    s.env.insert("PATH".into(), s.dir.path().to_string_lossy().into_owned());
    if Path::new("/opt/homebrew/bin/claude").exists() || Path::new("/usr/local/bin/claude").exists() {
        eprintln!("skipping: a real claude lives in a well-known dir on this machine");
        return;
    }
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::NotInstalled);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "binary_not_found");
    assert!(issue.hint.unwrap().contains("npm install -g @anthropic-ai/claude-code"));
    assert!(info.models.is_empty());
    assert!(info.path.is_none());
}

#[tokio::test]
async fn broken_shim_is_skipped_in_favor_of_a_working_one() {
    let dir = tempfile::tempdir().unwrap();
    install_claude(&dir.path().join("nvm-shim"), Some("echo 'node: command not found' >&2; exit 127"));
    install_claude(&dir.path().join("good"), None);
    let mut s = Setup::bare(dir);
    let path = format!(
        "{}:{}:{}",
        s.dir.path().join("nvm-shim").display(),
        s.dir.path().join("good").display(),
        s.base_path()
    );
    s.env.insert("PATH".into(), path);
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.path.as_deref(), Some(s.dir.path().join("good/claude").to_str().unwrap()));
}

#[tokio::test]
async fn found_in_a_well_known_directory_when_not_on_path() {
    let dir = tempfile::tempdir().unwrap();
    install_claude(&dir.path().join(".local/bin"), None);
    let mut s = Setup::bare(dir);
    // PATH 里只留 python3 所在的系统目录，不含 ~/.local/bin
    s.env.insert("PATH".into(), s.base_path());
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Ready, "{:?}", info.issue);
    assert_eq!(info.path.as_deref(), Some(s.dir.path().join(".local/bin/claude").to_str().unwrap()));
}

#[tokio::test]
async fn too_old_version() {
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_VERSION_TEXT", "2.0.5 (Claude Code)");
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::UnsupportedVersion);
    assert_eq!(info.version.as_deref(), Some("2.0.5"));
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "version_too_old");
    assert!(issue.message.contains("2.1.0"));
    assert_eq!(issue.hint.as_deref(), Some("claude update"));
}

#[tokio::test]
async fn unparsable_version_does_not_block() {
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_VERSION_TEXT", "claude dev build");
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Ready);
    assert_eq!(info.version.as_deref(), Some("claude dev build"));
}

#[tokio::test]
async fn command_that_does_not_run_is_an_error_with_stderr() {
    let dir = tempfile::tempdir().unwrap();
    install_claude(&dir.path().join("bin"), Some("echo 'boom: bad install' >&2; exit 2"));
    let mut s = Setup::bare(dir);
    s.env.insert("PATH".into(), format!("{}:{}", s.dir.path().join("bin").display(), s.base_path()));
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Error);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "binary_failed");
    assert!(issue.message.contains("boom: bad install"), "{}", issue.message);
}

#[tokio::test]
async fn not_logged_in() {
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "out");
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::NotAuthenticated);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "login_required");
    assert_eq!(issue.hint.as_deref(), Some("run `claude` on the machine to log in"));
    // 版本和路径照样给出，方便诊断
    assert_eq!(info.version.as_deref(), Some("2.1.293"));
    assert!(info.path.is_some());
}

#[tokio::test]
async fn environment_credentials_are_not_blocked_by_auth_status() {
    for (k, v) in [("ANTHROPIC_API_KEY", "sk-test"), ("CLAUDE_CODE_USE_BEDROCK", "1")] {
        let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "out").var(k, v);
        let report = probe::run_probe(&s.ctx(), false).await;
        assert_eq!(report.info.status, AgentStatus::Ready, "{k}: {:?} auth={:?}", report.info.issue, report.auth);
        assert!(matches!(report.auth, Some(AuthState::Unknown(_))), "{k}: {:?}", report.auth);
    }
    // 显式关掉的 Bedrock 标志不算
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "out").var("CLAUDE_CODE_USE_BEDROCK", "0");
    assert_eq!(s.probe().await.status, AgentStatus::NotAuthenticated);
}

#[tokio::test]
async fn auth_state_details() {
    let s = Setup::with_claude_on_path();
    let report = probe::run_probe(&s.ctx(), false).await;
    assert_eq!(
        report.auth,
        Some(AuthState::LoggedIn { method: Some("claude.ai".into()), email: Some("test@example.com".into()) })
    );
    assert_eq!(report.auth.unwrap().describe(), "logged in via claude.ai as test@example.com");

    // 老版本 CLI 打文本、退出码 0
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "nojson");
    let report = probe::run_probe(&s.ctx(), false).await;
    assert_eq!(report.auth, Some(AuthState::LoggedIn { method: None, email: None }));

    // 退出码既不是 0 也不是 1：不确定，不阻塞
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "error");
    let report = probe::run_probe(&s.ctx(), false).await;
    assert!(matches!(report.auth, Some(AuthState::Unknown(_))));
    assert_eq!(report.info.status, AgentStatus::Ready);
}

#[tokio::test]
async fn hanging_auth_check_respects_the_deadline() {
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "hang");
    let mut ctx = s.ctx();
    ctx.deadline = Duration::from_secs(4);
    let t0 = Instant::now();
    let report = probe::run_probe(&ctx, false).await;
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
    assert!(matches!(report.auth, Some(AuthState::Unknown(ref why)) if why == "timed out"), "{:?}", report.auth);
    assert_eq!(report.info.status, AgentStatus::Ready);
}

#[tokio::test]
async fn exhausted_budget_lists_unfinished_steps() {
    let s = Setup::with_claude_on_path();
    let mut ctx = s.ctx();
    ctx.deadline = Duration::ZERO;
    let info = ClaudeCodeAdapter::new().probe(&ctx).await;
    assert_eq!(info.status, AgentStatus::Error);
    let issue = info.issue.unwrap();
    assert_eq!(issue.code, "probe_timeout");
    assert!(
        issue.message.contains("version") && issue.message.contains("auth") && issue.message.contains("models"),
        "{}",
        issue.message
    );
}

#[tokio::test]
async fn disabled_by_config() {
    let mut s = Setup::with_claude_on_path();
    s.config.enabled = false;
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Disabled);
    assert!(!info.enabled);
    assert!(info.models.is_empty());
}

#[tokio::test]
async fn config_command_override_with_args() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Setup::bare(dir);
    s.config.command = Some(format!("{} --profile work", fake_claude().display()));
    let report = probe::run_probe(&s.ctx(), false).await;
    assert_eq!(report.info.status, AgentStatus::Ready, "{:?}", report.info.issue);
    assert_eq!(report.info.path.as_deref(), Some(fake_claude().to_str().unwrap()));
    assert_eq!(report.resolution.unwrap().prefix_args, vec!["--profile", "work"]);

    // 写坏了的配置
    s.config.command = Some("claude 'oops".into());
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Error);
    assert_eq!(info.issue.unwrap().code, "invalid_command");

    // 配置指向一个不存在的路径
    s.config.command = Some("/no/such/claude".into());
    let info = s.probe().await;
    assert_eq!(info.status, AgentStatus::Error);
}

#[tokio::test]
async fn user_models_from_settings_json_env_and_config_are_appended() {
    let mut s = Setup::with_claude_on_path()
        .var("ANTHROPIC_DEFAULT_OPUS_MODEL", "corp-proxy-opus")
        .var("ANTHROPIC_MODEL", "us.anthropic.claude-sonnet-5-5-v1:0");
    std::fs::create_dir_all(s.dir.path().join(".claude")).unwrap();
    std::fs::write(
        s.dir.path().join(".claude/settings.json"),
        r#"{"model": "my-settings-model", "env": {"ANTHROPIC_DEFAULT_HAIKU_MODEL": "corp-haiku"}}"#,
    )
    .unwrap();
    s.config.additional_models = vec![ModelInfo {
        id: "from-config".into(),
        label: "From config".into(),
        description: None,
        default: true,
        thinking: None,
        tags: vec![],
    }];
    let info = s.probe().await;
    let ids: Vec<&str> = info.models.iter().map(|m| m.id.as_str()).collect();
    for want in
        ["corp-proxy-opus", "us.anthropic.claude-sonnet-5-5-v1:0", "my-settings-model", "corp-haiku", "from-config"]
    {
        assert!(ids.contains(&want), "missing {want} in {ids:?}");
    }
    // 追加的都排在内置表后面，也不会抢默认
    assert_eq!(info.models.iter().filter(|m| m.default).count(), 1);
    assert_eq!(info.models[0].id, "claude-opus-5-5");
    let custom = info.models.iter().find(|m| m.id == "my-settings-model").unwrap();
    assert!(custom.description.as_deref().unwrap().contains("settings.json"));
}

#[tokio::test]
async fn config_env_is_used_for_the_child_processes() {
    // config.env 里的变量会传给 `claude --version` / `auth status`
    let mut s = Setup::with_claude_on_path();
    s.config.env.insert("FAKE_CLAUDE_AUTH".into(), "out".into());
    assert_eq!(s.probe().await.status, AgentStatus::NotAuthenticated);
}

#[tokio::test]
async fn diagnose_has_the_fields_from_the_spec() {
    let s = Setup::with_claude_on_path();
    let (text, fields) = anycon_adapters::diagnose("claude-code", &s.ctx()).await.expect("diagnosable");
    let get = |label: &str| {
        fields
            .iter()
            .find(|f| f.label == label)
            .unwrap_or_else(|| {
                panic!("missing field {label}: {:?}", fields.iter().map(|f| &f.label).collect::<Vec<_>>())
            })
            .value
            .clone()
    };
    assert_eq!(get("command"), "claude (default)");
    assert_eq!(get("resolved from"), "PATH");
    assert!(get("daemon PATH").contains(s.dir.path().join("bin").to_str().unwrap()));
    assert_eq!(get("daemon shell"), "/bin/sh");
    assert!(get("which -a claude").contains(s.dir.path().join("bin/claude").to_str().unwrap()));
    assert!(fields.iter().any(|f| f.label.starts_with("$SHELL -lc 'type -a claude'")));
    assert!(get("candidates").contains("ok: 2.1.293 (Claude Code)"));
    assert_eq!(get("resolved path"), s.dir.path().join("bin/claude").to_str().unwrap());
    assert_eq!(get("version"), "2.1.293");
    assert_eq!(get("auth"), "logged in via claude.ai as test@example.com");
    assert!(get("models").parse::<usize>().unwrap() >= 10);
    assert_eq!(get("status"), "ready");
    // 文本版
    assert!(text.starts_with("Claude Code (claude-code)"));
    assert!(text.contains("version: 2.1.293"));
    assert!(text.contains("status: ready"));
}

#[tokio::test]
async fn diagnose_explains_a_problem() {
    let s = Setup::with_claude_on_path().var("FAKE_CLAUDE_AUTH", "out");
    let (text, fields) = anycon_adapters::diagnose("claude-code", &s.ctx()).await.unwrap();
    assert!(fields.iter().any(|f| f.label == "status" && f.value == "not_authenticated"));
    let issue = fields.iter().find(|f| f.label == "issue").unwrap();
    assert!(issue.value.contains("login_required") && issue.value.contains("hint: run `claude`"));
    assert!(text.contains("hint: run `claude`"));
    assert!(fields.iter().any(|f| f.label == "auth" && f.value == "not logged in"));
}

#[tokio::test]
async fn diagnose_when_missing_shows_empty_lookups() {
    if Path::new("/opt/homebrew/bin/claude").exists() || Path::new("/usr/local/bin/claude").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut s = Setup::bare(dir);
    s.env.insert("PATH".into(), s.dir.path().to_string_lossy().into_owned());
    let (_, fields) = anycon_adapters::diagnose("claude-code", &s.ctx()).await.unwrap();
    let get = |label: &str| fields.iter().find(|f| f.label == label).unwrap().value.clone();
    assert_eq!(get("which -a claude"), "(none)");
    assert_eq!(get("resolved from"), "(not found)");
    assert_eq!(get("status"), "not_installed");
}
