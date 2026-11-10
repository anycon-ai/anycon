//! 探测（docs/spec/daemon.md § 5.3）与诊断（§ 5.5）。
//!
//! 步骤：解析命令 → `--version`（semver，最低 2.1.0）→ `claude auth status` → 模型（`initialize` 的 `models[]`，问不到用
//! 内置表）+ 用户配置的模型。整个探测遵守 `ctx.deadline`，超时时错误里列出还没完成的步骤。
//!
//! 昂贵的只有模型这一步：要起一个 `claude` 进程发 `initialize`（约 4 秒）。cheap 探测不起，沿用同一个二进制、同一个版本上一次
//! 问到的清单（记在 adapter 实例里，daemon 一直用同一个实例）；版本变了 daemon 会补一次完整探测。探测用的进程不拉起用户的
//! MCP server（`--strict-mcp-config`）、不跑 hooks（`disableAllHooks`）、不写会话记录（`--no-session-persistence`），
//! cwd 是 workspace 之外的临时目录。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anycon_agent_api::ProbeContext;
use anycon_protocol::{
    AgentCapabilities, AgentInfo, AgentIssue, AgentStatus, ConfigOption, ConfigOptionKind, DiagnosticField, ModeInfo,
    ModeRisk, ModelInfo,
};
use semver::Version;
use serde_json::{json, Value};

use crate::common::bin::which_all;
use crate::common::probe_util::candidate_lines;
use crate::common::{
    effective_env, field, neutral_cwd, now_ms, parse_semver, resolve_binary, run_capture, shell_type_a,
    version_at_least, version_timeout, Budget, JsonlChild, Resolution, ResolveOptions, ShutdownTimings, SpawnSpec,
    DEFAULT_MAX_LINE_BYTES,
};

use super::models::{self, CliModel};
use super::process;

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
/// `initialize` 这一步的上限（实测冷启动约 4 秒，留足机器忙时的余量）。
const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
/// 探测发的 `initialize` 控制请求的 id。
const MODELS_REQUEST_ID: &str = "anycon-probe-models";

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
    /// 模型清单的来源（`initialize` / 上一次的 / 内置表，以及为什么）。
    pub models_note: Option<String>,
}

/// 上一次 `initialize` 问到的模型表（[`models::from_cli`] 的结果），和当时的二进制路径、版本。cheap 探测只在二者都没变时沿用。
#[derive(Debug, Clone)]
pub struct CliModelsMemo {
    pub path: Option<String>,
    pub version: Option<String>,
    pub models: Vec<ModelInfo>,
}

pub type CliModelsMemoCell = Mutex<Option<CliModelsMemo>>;

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
    vec![
        ConfigOption {
            id: "fast_mode".to_string(),
            label: "Fast mode".to_string(),
            description: Some("Faster output at a higher price (supported models only)".to_string()),
            kind: ConfigOptionKind::Bool,
            choices: vec![],
            default: Value::Bool(false),
        },
        // `permissions.blockWritesOutsideWorkingDirectories`（2.1.314+；更老的 CLI 收下但不生效）。默认关：打开后项目外的写入
        // 直接被拒、连权限请求都没有（ADR-48）
        ConfigOption {
            id: OPTION_BLOCK_OUTSIDE_WRITES.to_string(),
            label: "Block file writes outside the project".to_string(),
            description: Some(
                "Write, Edit and NotebookEdit refuse paths outside the working directory in every permission mode, \
                 including Bypass Permissions. Shell commands are not covered. Needs Claude Code 2.1.314 or later."
                    .to_string(),
            ),
            kind: ConfigOptionKind::Bool,
            choices: vec![],
            default: Value::Bool(false),
        },
    ]
}

/// 选项 id：文件工具不许写工作目录以外的路径。
pub const OPTION_BLOCK_OUTSIDE_WRITES: &str = "block_outside_writes";

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

/// 完整探测。`probe_all`：`--version` 把所有候选都跑一遍（`doctor` 用）。`memo`：cheap 探测沿用的模型清单。
pub async fn run_probe(ctx: &ProbeContext, probe_all: bool, memo: &CliModelsMemoCell) -> ProbeReport {
    let budget = Budget::new(ctx.deadline);
    let mut info = base_info(ctx);
    let report =
        |info: AgentInfo, resolution, version, auth| ProbeReport { info, resolution, version, auth, models_note: None };

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

    // 4. 模型：完整探测问 CLI（`initialize`），cheap 探测沿用上一次问到的；都没有就用内置表
    let remembered = memo
        .lock()
        .expect("models memo lock")
        .clone()
        .filter(|m| m.path == info.path && m.version == info.version)
        .map(|m| m.models);
    let (cli_models, models_note) = if ctx.cheap {
        let note = if remembered.is_some() {
            "from the last initialize (cheap probe)"
        } else {
            "built-in table (cheap probe)"
        };
        (remembered, note.to_string())
    } else if budget.remaining().is_zero() {
        (remembered, "initialize skipped: the probe ran out of time".to_string())
    } else {
        let program = resolution.path().expect("chosen candidate");
        let step = budget.cap(MODELS_TIMEOUT);
        let listed = query_cli_models(program, &resolution.prefix_args, &env, &ctx.scratch_dir, step)
            .await
            .map(|rows| models::from_cli(&rows))
            .and_then(|m| if m.is_empty() { Err("initialize listed no usable models".to_string()) } else { Ok(m) });
        match listed {
            Ok(m) => {
                *memo.lock().expect("models memo lock") =
                    Some(CliModelsMemo { path: info.path.clone(), version: info.version.clone(), models: m.clone() });
                let note = format!("{} models from initialize", m.len());
                (Some(m), note)
            }
            Err(e) => {
                tracing::warn!(error = %e, "claude: could not list models via initialize");
                let fallback =
                    if remembered.is_some() { "kept the last initialize" } else { "using the built-in table" };
                (remembered, format!("{e}; {fallback}"))
            }
        }
    };
    let settings_json = models::read_user_settings(&env);
    let user = models::user_configured_models(&env, settings_json.as_ref());
    info.models = models::assemble(
        cli_models.unwrap_or_else(|| models::builtin_models(models::BUILTIN, version.as_ref())),
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
    let mut out = report(info, Some(resolution), version, Some(auth));
    out.models_note = Some(models_note);
    out
}

/// 探测用的 `claude` 进程的参数：会话的固定参数 + 不拉起 MCP server、不跑 hooks、不写会话记录。
pub fn models_query_args() -> Vec<String> {
    let mut a = process::base_args();
    a.push("--strict-mcp-config".to_string());
    a.push("--no-session-persistence".to_string());
    a.push(format!("--settings={}", json!({ "disableAllHooks": true })));
    a
}

/// `initialize` 的回执里取 `models[]`。不是我们那个请求的回执给 `None`；单项解析不了的跳过。
pub fn parse_initialize_response(line: &str) -> Option<Result<Vec<CliModel>, String>> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("control_response") {
        return None;
    }
    let r = v.get("response")?;
    if r.get("request_id").and_then(Value::as_str) != Some(MODELS_REQUEST_ID) {
        return None;
    }
    if r.get("subtype").and_then(Value::as_str) == Some("error") {
        let why = r.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        return Some(Err(format!("initialize failed: {why}")));
    }
    let Some(rows) = r.pointer("/response/models").and_then(Value::as_array) else {
        return Some(Err("the initialize response has no models list".to_string()));
    };
    Some(Ok(rows.iter().filter_map(|m| serde_json::from_value(m.clone()).ok()).collect()))
}

/// 起一个 `claude` 进程，发 `initialize`，取回模型清单，然后关掉进程。整个过程不超过 `timeout`（加上关进程的几秒）。
pub async fn query_cli_models(
    program: &Path,
    prefix_args: &[String],
    env: &HashMap<String, String>,
    cwd: &Path,
    timeout: Duration,
) -> Result<Vec<CliModel>, String> {
    let mut args = prefix_args.to_vec();
    args.extend(models_query_args());
    let spec = SpawnSpec {
        program: program.to_path_buf(),
        args,
        cwd: if cwd.is_dir() { cwd.to_path_buf() } else { neutral_cwd() },
        env: env.clone(),
        max_line_bytes: DEFAULT_MAX_LINE_BYTES,
    };
    let (child, mut lines) =
        JsonlChild::spawn(&spec).map_err(|e| format!("failed to start {}: {e}", program.display()))?;
    let request = json!({
        "type": "control_request",
        "request_id": MODELS_REQUEST_ID,
        "request": { "subtype": "initialize" },
    });
    let result = if child.send_json(&request) {
        let wait = async {
            while let Some(line) = lines.recv().await {
                if let Some(r) = parse_initialize_response(&line) {
                    return r;
                }
            }
            let tail = child.stderr_tail();
            Err(format!("claude exited before answering initialize: {}", tail.trim()))
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(r) => r,
            Err(_) => Err(format!("initialize did not answer within {}s", timeout.as_secs())),
        }
    } else {
        Err("could not write to the stdin of claude".to_string())
    };
    child
        .shutdown(ShutdownTimings { after_stdin_close: Duration::from_millis(300), grace: Duration::from_secs(2) })
        .await;
    result
}

/// § 5.5 的诊断：命令来源、daemon 的 PATH / shell、`which -a`、`type -a`、各候选的探活结果、解析路径、版本、认证、模型数、状态。
pub async fn diagnose_fields(ctx: &ProbeContext) -> Vec<DiagnosticField> {
    let report = run_probe(ctx, true, &CliModelsMemoCell::default()).await;
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
    if let Some(n) = &report.models_note {
        f.push(field("models source", n.clone()));
    }
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
