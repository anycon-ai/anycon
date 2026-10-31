//! 探测（docs/spec/daemon.md § 5.3）与诊断（§ 5.5）。
//!
//! 步骤：解析命令 → `--version`（semver，最低 2.1.0）→ `claude auth status` → 内置模型表 + 用户配置的模型。
//! 没有昂贵的步骤（CLI 没有「列模型」命令），所以 `cheap` 与否结果一样。整个探测遵守 `ctx.deadline`，
//! 超时时错误里列出还没完成的步骤。

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anycon_agent_api::ProbeContext;
use anycon_protocol::{
    AgentCapabilities, AgentInfo, AgentIssue, AgentStatus, ConfigOption, ConfigOptionKind, DiagnosticField, ModeInfo,
    ModeRisk,
};
use semver::Version;
use serde_json::Value;

use crate::common::bin::which_all;
use crate::common::probe_util::candidate_lines;
use crate::common::{
    effective_env, field, now_ms, parse_semver, resolve_binary, run_capture, shell_type_a, version_at_least,
    version_timeout, Budget, Resolution, ResolveOptions,
};

use super::models;

pub const ID: &str = "claude-code";
pub const LABEL: &str = "Claude Code";
pub const DRIVER: &str = "claude-stream-json";
pub const DEFAULT_COMMAND: &str = "claude";
/// 低于它的 CLI 没有我们依赖的 stream-json 控制协议。
pub const MIN_CLI_VERSION: &str = "2.1.0";
pub const INSTALL_HINT: &str = "npm install -g @anthropic-ai/claude-code";
pub const UPGRADE_HINT: &str = "claude update";
pub const LOGIN_HINT: &str = "run `claude` on the machine to log in";

pub use crate::common::VERSION_TIMEOUT_ENV;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    LoggedIn {
        method: Option<String>,
        email: Option<String>,
    },
    LoggedOut,
    /// 没能确定（超时、CLI 不认识 `auth status`…）；不阻塞，真正失败时会话以 `auth_failed` 结束。
    Unknown(String),
}

impl AuthState {
    pub fn describe(&self) -> String {
        match self {
            AuthState::LoggedIn { method, email } => {
                let mut s = "logged in".to_string();
                if let Some(m) = method {
                    s.push_str(&format!(" via {m}"));
                }
                if let Some(e) = email {
                    s.push_str(&format!(" as {e}"));
                }
                s
            }
            AuthState::LoggedOut => "not logged in".to_string(),
            AuthState::Unknown(why) => format!("unknown ({why})"),
        }
    }
}

/// 一次探测的完整结果（`diagnose` 要用里面的细节）。
pub struct ProbeReport {
    pub info: AgentInfo,
    pub resolution: Option<Resolution>,
    pub version: Option<Version>,
    pub auth: Option<AuthState>,
}

pub fn modes() -> Vec<ModeInfo> {
    let m = |id: &str, label: &str, description: &str, default: bool, risk: ModeRisk| ModeInfo {
        id: id.to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        default,
        risk,
    };
    vec![
        m("default", "Always Ask", "Ask before every file edit and command", true, ModeRisk::Normal),
        m(
            "acceptEdits",
            "Accept File Edits",
            "Edit files without asking; still ask for commands",
            false,
            ModeRisk::Normal,
        ),
        m("plan", "Plan Mode", "Read-only planning; no edits or commands", false, ModeRisk::Safe),
        m("auto", "Auto", "A classifier approves routine actions automatically", false, ModeRisk::Normal),
        m("dontAsk", "Don't Ask", "Deny anything that is not pre-approved instead of asking", false, ModeRisk::Normal),
        m("bypassPermissions", "Bypass Permissions", "Run everything without asking", false, ModeRisk::Dangerous),
    ]
}

pub fn options() -> Vec<ConfigOption> {
    vec![ConfigOption {
        id: "fast_mode".to_string(),
        label: "Fast mode".to_string(),
        description: Some("Faster output at a higher price (supported models only)".to_string()),
        kind: ConfigOptionKind::Bool,
        choices: vec![],
        default: Value::Bool(false),
    }]
}

pub fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        resume: true,
        interrupt: true,
        // stream-json 里 turn 进行中收到的用户消息会排到下一个 turn。
        interject: false,
        // 图片：adapter 读 daemon 落盘的文件，转 base64 放进 stream-json 用户消息的 `image` block（ADR-35）
        attachments: crate::common::IMAGE_MIMES.iter().map(|m| m.to_string()).collect(),
        thinking_stream: true,
        usage: true,
        structured_output: true,
        env: true,
        rewind: false,
    }
}

fn base_info(ctx: &ProbeContext) -> AgentInfo {
    AgentInfo {
        id: ID.to_string(),
        label: LABEL.to_string(),
        driver: DRIVER.to_string(),
        enabled: ctx.config.enabled,
        status: AgentStatus::Ready,
        version: None,
        path: None,
        issue: None,
        models: vec![],
        allow_custom_model: true,
        modes: vec![],
        options: vec![],
        capabilities: AgentCapabilities::default(),
        probed_at_ms: now_ms(),
    }
}

fn fail(info: &mut AgentInfo, status: AgentStatus, code: &str, message: String, hint: Option<&str>) {
    info.status = status;
    info.issue = Some(AgentIssue { code: code.to_string(), message, hint: hint.map(str::to_string) });
}

fn truthy(env: &HashMap<String, String>, key: &str) -> bool {
    env.get(key).map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")).unwrap_or(false)
}

/// 环境里有不依赖 `claude login` 的凭证（API key、Bedrock / Vertex / Foundry）。
fn has_env_credentials(env: &HashMap<String, String>) -> bool {
    ["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"].iter().any(|k| truthy(env, k))
        || ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"]
            .iter()
            .any(|k| env.get(*k).is_some_and(|v| !v.is_empty()))
}

/// `claude auth status`：退出码 0 已登录 / 1 未登录，stdout 是 JSON（`loggedIn`、`authMethod`、`email`）。
pub async fn auth_status(res: &Resolution, env: &HashMap<String, String>, cwd: &Path, timeout: Duration) -> AuthState {
    let Some(path) = res.path() else { return AuthState::Unknown("no binary".to_string()) };
    let mut args = res.prefix_args.clone();
    args.push("auth".to_string());
    args.push("status".to_string());
    let out = run_capture(path, &args, env, cwd, timeout).await;
    let parsed: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
    let field = |k: &str| parsed.as_ref().and_then(|p| p.get(k)).and_then(Value::as_str).map(str::to_string);
    let logged_in_flag = parsed.as_ref().and_then(|p| p.get("loggedIn")).and_then(Value::as_bool);

    let state = match (out.code, logged_in_flag) {
        (Some(0), Some(false)) => AuthState::LoggedOut,
        (Some(0), _) => AuthState::LoggedIn { method: field("authMethod"), email: field("email") },
        (Some(1), _) => AuthState::LoggedOut,
        _ => AuthState::Unknown(out.failure_reason()),
    };
    // 用环境变量里的凭证（API key / Bedrock…）的人，`auth status` 可能报未登录；不挡他们。
    if state == AuthState::LoggedOut && has_env_credentials(env) {
        return AuthState::Unknown(
            "not logged in via `claude login`, but credentials are set in the environment".to_string(),
        );
    }
    state
}

/// 完整探测。`probe_all`：`--version` 把所有候选都跑一遍（`doctor` 用）。
pub async fn run_probe(ctx: &ProbeContext, probe_all: bool) -> ProbeReport {
    let budget = Budget::new(ctx.deadline);
    let mut info = base_info(ctx);
    let report = |info: AgentInfo, resolution, version, auth| ProbeReport { info, resolution, version, auth };

    if !ctx.config.enabled {
        fail(
            &mut info,
            AgentStatus::Disabled,
            "disabled",
            "Claude Code is disabled in the config".to_string(),
            Some("set agents.claude-code.enabled = true"),
        );
        return report(info, None, None, None);
    }

    let env = effective_env(ctx);

    // 1. 解析命令
    let resolved = tokio::time::timeout(
        budget.remaining(),
        resolve_binary(ResolveOptions {
            env: &env,
            config_command: ctx.config.command.as_deref(),
            default_name: DEFAULT_COMMAND,
            probe_args: &["--version"],
            per_candidate_timeout: version_timeout(&env),
            cwd: &ctx.scratch_dir,
            probe_all,
        }),
    )
    .await;
    let Ok(resolution) = resolved else {
        fail(
            &mut info,
            AgentStatus::Error,
            "probe_timeout",
            "probe timed out while locating the claude binary; unfinished steps: version, auth, models".to_string(),
            None,
        );
        return report(info, None, None, None);
    };
    if resolution.chosen().is_none() {
        if let Some(summary) = resolution.failure_summary() {
            let code = if resolution.config_error.is_some() { "invalid_command" } else { "binary_failed" };
            fail(
                &mut info,
                AgentStatus::Error,
                code,
                format!("found Claude Code but it does not run: {summary}"),
                Some("check that `claude --version` works in a terminal"),
            );
        } else {
            fail(
                &mut info,
                AgentStatus::NotInstalled,
                "binary_not_found",
                "the claude command was not found".to_string(),
                Some(INSTALL_HINT),
            );
        }
        return report(info, Some(resolution), None, None);
    }
    info.path = resolution.path().map(|p| p.to_string_lossy().into_owned());

    // 2. 版本
    let version = resolution.version_output().and_then(parse_semver);
    info.version = version
        .as_ref()
        .map(|v| v.to_string())
        .or_else(|| resolution.version_output().map(|s| s.lines().next().unwrap_or("").to_string()));
    if let Some(v) = &version {
        if !version_at_least(v, MIN_CLI_VERSION) {
            fail(
                &mut info,
                AgentStatus::UnsupportedVersion,
                "version_too_old",
                format!("Claude Code {v} is older than the minimum supported {MIN_CLI_VERSION}"),
                Some(UPGRADE_HINT),
            );
            return report(info, Some(resolution), version, None);
        }
    } else {
        tracing::warn!(output = ?resolution.version_output(), "claude: could not parse a version, skipping the minimum-version check");
    }

    // 3. 认证
    let auth_budget = budget.cap(AUTH_TIMEOUT);
    if auth_budget.is_zero() {
        fail(
            &mut info,
            AgentStatus::Error,
            "probe_timeout",
            "probe timed out; unfinished steps: auth, models".to_string(),
            None,
        );
        return report(info, Some(resolution), version, None);
    }
    let auth = auth_status(&resolution, &env, &ctx.scratch_dir, auth_budget).await;

    // 4. 模型与选项（本地计算，没有慢步骤）
    let settings_json = models::read_user_settings(&env);
    let user = models::user_configured_models(&env, settings_json.as_ref());
    info.models = models::assemble(
        models::builtin_models(models::BUILTIN, version.as_ref()),
        user,
        &ctx.config.additional_models,
    );
    info.modes = modes();
    info.options = options();
    info.capabilities = capabilities();

    if auth == AuthState::LoggedOut {
        fail(
            &mut info,
            AgentStatus::NotAuthenticated,
            "login_required",
            "Claude Code is not logged in".to_string(),
            Some(LOGIN_HINT),
        );
    }
    report(info, Some(resolution), version, Some(auth))
}

/// § 5.5 的诊断：命令来源、daemon 的 PATH / shell、`which -a`、`type -a`、各候选的探活结果、解析路径、版本、认证、模型数、状态。
pub async fn diagnose_fields(ctx: &ProbeContext) -> Vec<DiagnosticField> {
    let report = run_probe(ctx, true).await;
    let env = effective_env(ctx);
    let name = ctx
        .config
        .command
        .as_deref()
        .and_then(|c| crate::common::parse_command(c).ok())
        .filter(|c| !c.is_path())
        .map(|c| c.program)
        .unwrap_or_else(|| DEFAULT_COMMAND.to_string());

    let mut f = vec![field("adapter", ID), field("enabled", ctx.config.enabled.to_string())];
    f.push(field(
        "command",
        match ctx.config.command.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(c) => format!("{c} (from config)"),
            None => format!("{DEFAULT_COMMAND} (default)"),
        },
    ));
    f.push(field(
        "resolved from",
        report
            .resolution
            .as_ref()
            .and_then(|r| r.chosen())
            .map(|c| c.origin.as_str().to_string())
            .unwrap_or_else(|| "(not found)".to_string()),
    ));
    f.push(field("daemon PATH", env.get("PATH").cloned().unwrap_or_else(|| "(unset)".to_string())));
    f.push(field("daemon shell", env.get("SHELL").cloned().unwrap_or_else(|| "(unset)".to_string())));
    let which = env.get("PATH").map(|p| which_all(&name, p, &ctx.scratch_dir)).unwrap_or_default();
    f.push(field(
        &format!("which -a {name}"),
        if which.is_empty() {
            "(none)".to_string()
        } else {
            which.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("\n")
        },
    ));
    f.push(field(&format!("$SHELL -lc 'type -a {name}'"), shell_type_a(&env, &name, &ctx.scratch_dir).await));
    if let Some(res) = &report.resolution {
        f.push(field("candidates", candidate_lines(res)));
    }
    f.push(field("resolved path", report.info.path.clone().unwrap_or_else(|| "(none)".to_string())));
    f.push(field("version", report.info.version.clone().unwrap_or_else(|| "(unknown)".to_string())));
    f.push(field("auth", report.auth.as_ref().map(AuthState::describe).unwrap_or_else(|| "(not checked)".to_string())));
    f.push(field("models", report.info.models.len().to_string()));
    let status =
        serde_json::to_value(report.info.status).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
    f.push(field("status", status));
    if let Some(issue) = &report.info.issue {
        let mut s = format!("{}: {}", issue.code, issue.message);
        if let Some(h) = &issue.hint {
            s.push_str(&format!("\nhint: {h}"));
        }
        f.push(field("issue", s));
    }
    f
}
