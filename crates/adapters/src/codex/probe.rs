//! 探测（docs/spec/daemon.md § 5.3）与诊断（§ 5.5）。
//!
//! 步骤：解析命令 → `codex --version`（semver，最低 0.157.0）→ `codex login status` → 模型与默认值
//! （在 `ctx.scratch_dir` 下拉起 `codex app-server`：`initialize` → `model/list`（翻页）→ `config/read` → `account/read`）。
//! 第 4 步是整个探测里最慢的。整个探测遵守 `ctx.deadline`。
//!
//! **认证判定**（本机实测，codex-cli 0.157.1）：
//! - `codex login status` 把文字写在 **stderr**，stdout 为空——已登录退出码 0（`Logged in using an API key - sk-…` /
//!   `Logged in using ChatGPT`），未登录退出码 1 且 stderr 是 `Not logged in`；auth.json 损坏时同样退出码 1 但 stderr 是
//!   `Error checking login status: …`（这种情况不能断定未登录，标 unknown）。
//! - **权威判定是 app-server 的 `account/read`**（`{ account?, requiresOpenaiAuth }`，稳定版 schema，不联网）：
//!   `requiresOpenaiAuth = false`（自定义提供方等）→ 不挡；`account` 为空且 `requiresOpenaiAuth = true` → `not_authenticated`；
//!   其余按已登录。它会解析 profile、`-c model_provider=…` 这些我们自己猜不到的配置。实测：空 `CODEX_HOME` →
//!   `{ account: null, requiresOpenaiAuth: true }`；配了自定义提供方 → `requiresOpenaiAuth: false`；API key →
//!   `{ account: { type: "apiKey" }, requiresOpenaiAuth: true }`。
//! - **完整探测**在 `config/read` 之后调 `account/read`。**cheap 探测**只跑 `login status`：结果（已登录 / 未登录 / unknown）和
//!   上一次一样就沿用上一次的判定（不拉起 app-server）；不一样（登录了 / 退出了）才拉起 app-server 补跑一次 `account/read`
//!   （只做这一步，不拉模型列表）。「上一次」存在 adapter 实例里的 [`AuthMemo`]（`all()` 里的实例在 daemon 的整个生命周期里是同一个）；
//!   没有记录、`previous` 为空、二进制路径变了、或上一次的判定不是来自 `account/read`（它不可用时退回 `login status`），
//!   都按「不一样」处理。
//! - **`account/read` 用不了时的兜底顺序**（完整探测）：同一个二进制上一次**权威**（来自 `account/read`）的判定，**且当时记下的
//!   `login status` 和现在的相同** > `login status`（只有「已确认未登录」才挡）。`login status` 不认自定义提供方，一次
//!   `account/read` 超时就退回它会把原来能用的会话挡掉；沿用上一次判定时不改记录。`login status` 变了（用户刚登录 / 退出）旧判定多半就是错的，
//!   所以 cheap 探测因此才补跑 `account/read`、问不到时完整探测也一样，直接退回 `login status`。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::ProbeContext;
use anycon_protocol::{
    AgentCapabilities, AgentInfo, AgentIssue, AgentStatus, Choice, ConfigOption, ConfigOptionKind, DiagnosticField,
    ModeInfo, ModeRisk,
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

use super::models;
use super::process::{self, OPT_FAST_MODE, OPT_REASONING_SUMMARY, SUMMARY_CHOICES};
use super::rpc::{classify, Incoming, RpcClient, RpcError, METHOD_NOT_FOUND};
use super::wire::{AccountReadResponse, ConfigReadResponse, ConfigSummary, ModelEntry, ModelListPage};

pub const ID: &str = "codex";
pub const LABEL: &str = "Codex";
pub const DRIVER: &str = "codex-app-server";
pub const DEFAULT_COMMAND: &str = "codex";
/// 本机验证过的版本。更老的 app-server 协议不保证有我们依赖的方法 / 字段。
pub const MIN_CLI_VERSION: &str = "0.157.0";
pub const INSTALL_HINT: &str = "npm i -g @openai/codex";
pub const UPGRADE_HINT: &str = "npm i -g @openai/codex@latest";
pub const LOGIN_HINT: &str = "codex login";

const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// 拉起 app-server 取模型列表的单步上限（再受总预算约束）。
const MODELS_TIMEOUT: Duration = Duration::from_secs(25);
/// 一次请求的上限。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// `model/list` 最多翻这么多页（防止服务端一直给 cursor）。
const MAX_MODEL_PAGES: usize = 20;

// ---------- 静态声明 ----------

pub fn modes() -> Vec<ModeInfo> {
    let m = |id: &str, label: &str, description: &str, default: bool, risk: ModeRisk| ModeInfo {
        id: id.to_string(),
        label: label.to_string(),
        description: Some(description.to_string()),
        default,
        risk,
    };
    vec![
        m(
            process::MODE_READ_ONLY,
            "Read Only",
            "Read-only sandbox: Codex can look around but asks before changing anything",
            false,
            ModeRisk::Safe,
        ),
        m(
            process::MODE_WORKSPACE,
            "Workspace",
            "Edits and commands run in a sandbox around the project; Codex asks before going beyond it",
            true,
            ModeRisk::Normal,
        ),
        m(
            process::MODE_AUTO_REVIEW,
            "Auto Review",
            "Like Workspace, but Codex's own reviewer agent decides routine approval requests instead of asking you",
            false,
            ModeRisk::Normal,
        ),
        m(process::MODE_FULL_ACCESS, "Full Access", "No sandbox and no approval prompts", false, ModeRisk::Dangerous),
    ]
}

pub fn options() -> Vec<ConfigOption> {
    let choice = |id: &str, label: &str| Choice { id: id.to_string(), label: label.to_string(), description: None };
    debug_assert_eq!(SUMMARY_CHOICES, ["auto", "concise", "detailed", "none"]);
    vec![
        ConfigOption {
            id: OPT_FAST_MODE.to_string(),
            label: "Fast mode".to_string(),
            description: Some("Faster output at a higher usage rate (supported models only)".to_string()),
            kind: ConfigOptionKind::Bool,
            choices: vec![],
            default: Value::Bool(false),
        },
        ConfigOption {
            id: OPT_REASONING_SUMMARY.to_string(),
            label: "Reasoning summary".to_string(),
            description: Some("How much of the model's reasoning Codex summarizes".to_string()),
            kind: ConfigOptionKind::Select,
            choices: vec![
                choice("auto", "Auto"),
                choice("concise", "Concise"),
                choice("detailed", "Detailed"),
                choice("none", "None"),
            ],
            default: Value::String("auto".to_string()),
        },
    ]
}

pub fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        resume: true,
        interrupt: true,
        // `turn/steer`
        interject: true,
        // 图片走 `localImage`，其它文件（`*/*`）把路径告诉 Codex（ADR-35）
        attachments: crate::common::accepted_attachments(),
        thinking_stream: true,
        usage: true,
        // `turn/start.outputSchema` 存在，但 `SessionSettings` 里没有东西可以喂给它
        structured_output: false,
        env: true,
        rewind: false,
    }
}

// ---------- 认证 ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// `detail` 是 `login status` 打印的那一行（API key 会被 codex 自己打码）。
    LoggedIn {
        detail: Option<String>,
    },
    LoggedOut,
    /// 没能确定（超时、auth.json 损坏、CLI 不认识 `login status`…）；不阻塞，真正失败时会话以 `auth_failed` 结束。
    Unknown(String),
}

/// `login status` 的三种结果（cheap 探测拿它判断「和上一次一样不一样」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginKind {
    LoggedIn,
    LoggedOut,
    Unknown,
}

impl AuthState {
    pub fn kind(&self) -> LoginKind {
        match self {
            AuthState::LoggedIn { .. } => LoginKind::LoggedIn,
            AuthState::LoggedOut => LoginKind::LoggedOut,
            AuthState::Unknown(_) => LoginKind::Unknown,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            AuthState::LoggedIn { detail: Some(d) } => format!("logged in ({d})"),
            AuthState::LoggedIn { detail: None } => "logged in".to_string(),
            AuthState::LoggedOut => "not logged in".to_string(),
            AuthState::Unknown(why) => format!("unknown ({why})"),
        }
    }
}

/// 解读 `codex login status` 的结果：退出码 0 已登录；退出码 1 且文字是 `Not logged in` 未登录；其余 unknown。
pub fn interpret_login_status(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
    failure: impl FnOnce() -> String,
) -> AuthState {
    let text = format!("{}\n{}", stderr.trim(), stdout.trim());
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string);
    match code {
        Some(0) => AuthState::LoggedIn { detail: first },
        Some(1) if text.to_ascii_lowercase().contains("not logged in") => AuthState::LoggedOut,
        _ => AuthState::Unknown(failure()),
    }
}

/// `codex login status`（文字在 stderr，见模块说明）。
pub async fn auth_status(res: &Resolution, env: &HashMap<String, String>, cwd: &Path, timeout: Duration) -> AuthState {
    let Some(path) = res.path() else { return AuthState::Unknown("no binary".to_string()) };
    let mut args = res.prefix_args.clone();
    args.push("login".to_string());
    args.push("status".to_string());
    let out = run_capture(path, &args, env, cwd, timeout).await;
    interpret_login_status(out.code, &out.stdout, &out.stderr, || out.failure_reason())
}

/// `account/read` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    /// `apiKey` / `chatgpt` / `amazonBedrock`；没有账户 = `None`。
    pub account_type: Option<String>,
    pub requires_openai_auth: bool,
}

impl AccountInfo {
    /// 会话是否被挡住：需要 OpenAI 登录、而且没有账户。
    pub fn blocks(&self) -> bool {
        self.requires_openai_auth && self.account_type.is_none()
    }

    pub fn describe(&self) -> String {
        match (&self.account_type, self.requires_openai_auth) {
            (Some(t), false) => format!("account {t}; the model provider does not need an OpenAI login"),
            (None, false) => "no account; the model provider does not need an OpenAI login".to_string(),
            (Some(t), true) => format!("logged in ({t})"),
            (None, true) => "no account; an OpenAI login is required".to_string(),
        }
    }
}

impl From<AccountReadResponse> for AccountInfo {
    fn from(r: AccountReadResponse) -> Self {
        AccountInfo {
            account_type: r.account.and_then(|a| a.kind).filter(|t| !t.is_empty()),
            requires_openai_auth: r.requires_openai_auth,
        }
    }
}

/// cheap 探测沿用的判定：当时 `login status` 的结果、二进制路径，以及当时的判定（是否挡住会话）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthMemo {
    pub path: Option<String>,
    pub login: LoginKind,
    pub blocked: bool,
    /// 判定来自 `account/read`（权威）。退回 `login status` 得出的判定不沿用。
    pub authoritative: bool,
}

/// adapter 实例里存 [`AuthMemo`] 的地方。
pub type AuthMemoCell = Mutex<Option<AuthMemo>>;

// ---------- 取模型（拉起 app-server） ----------

/// 不需要的、而且可能很大的通知，让 app-server 别发（`initialize.capabilities.optOutNotificationMethods`）：
/// `turn/diff/updated` 是整个 turn 的累计 diff，每改一次文件就重发一次（实测 11 MB 的新文件补丁产生 3 条 11 MB 的行）；
/// 其余是频繁但我们不用的。
pub const OPT_OUT_NOTIFICATIONS: &[&str] = &[
    "turn/diff/updated",
    "item/fileChange/patchUpdated",
    "account/rateLimits/updated",
    "remoteControl/status/changed",
];

/// 一次 `initialize` 的参数。
pub fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "anycon", "title": "anycon", "version": env!("CARGO_PKG_VERSION") },
        // 只用稳定版 schema 里的方法和字段
        "capabilities": { "experimentalApi": false, "optOutNotificationMethods": OPT_OUT_NOTIFICATIONS },
    })
}

/// 拉起 app-server 要问哪些。
#[derive(Debug, Clone, Copy, Default)]
pub struct Wants {
    pub models: bool,
    pub config: bool,
    pub account: bool,
}

/// 拉起 app-server 取回的东西。各项各自可能失败。
#[derive(Debug, Default)]
pub struct AppServerData {
    pub models: Option<Vec<ModelEntry>>,
    pub models_error: Option<String>,
    pub config: Option<ConfigSummary>,
    pub account: Option<AccountInfo>,
    pub account_error: Option<String>,
}

/// 在 `cwd` 下拉起 `codex app-server`，`initialize` →（按 `wants`）`model/list`（翻页）→ `config/read` → `account/read`，然后关掉。
/// 整个过程不超过 `budget`。拉不起来 / `initialize` 失败 = `Err`。
pub async fn query_app_server(
    program: &Path,
    prefix_args: &[String],
    env: &HashMap<String, String>,
    cwd: &Path,
    budget: Duration,
    wants: Wants,
) -> Result<AppServerData, String> {
    let started = std::time::Instant::now();
    let remaining = || budget.saturating_sub(started.elapsed());
    let mut args: Vec<String> = prefix_args.to_vec();
    args.extend(process::app_server_args());
    let cwd = if cwd.is_dir() { cwd.to_path_buf() } else { neutral_cwd() };
    let spec = SpawnSpec {
        program: program.to_path_buf(),
        args,
        cwd,
        env: env.clone(),
        max_line_bytes: DEFAULT_MAX_LINE_BYTES,
    };
    let (child, mut lines) =
        JsonlChild::spawn(&spec).map_err(|e| format!("failed to start {}: {e}", program.display()))?;
    let child = Arc::new(child);
    let rpc = Arc::new(RpcClient::new(child.clone()));
    let reader = {
        let rpc = rpc.clone();
        tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                match classify(&line) {
                    Some(Incoming::Response { id, result }) => rpc.resolve(id, result),
                    // 探测用的会话不处理任何服务端请求：回「没有这个方法」，免得对面干等
                    Some(Incoming::Request { id, method, .. }) => {
                        rpc.reply_error(&id, &RpcError::new(METHOD_NOT_FOUND, format!("unsupported: {method}")));
                    }
                    _ => {}
                }
            }
            rpc.close();
        })
    };

    let result = async {
        let step = |r: Duration| r.min(REQUEST_TIMEOUT);
        rpc.request("initialize", initialize_params(), step(remaining()))
            .await
            .map_err(|e| format!("initialize failed: {e}"))?;
        rpc.notify("initialized");

        let mut data = AppServerData::default();
        if wants.models {
            let mut entries: Vec<ModelEntry> = Vec::new();
            let mut cursor: Option<String> = None;
            let mut failure: Option<String> = None;
            for _ in 0..MAX_MODEL_PAGES {
                let mut params = json!({ "includeHidden": false });
                if let Some(c) = &cursor {
                    params["cursor"] = json!(c);
                }
                match rpc.request("model/list", params, step(remaining())).await {
                    Ok(v) => match serde_json::from_value::<ModelListPage>(v) {
                        Ok(page) => {
                            entries.extend(page.data);
                            match page.next_cursor.filter(|c| !c.is_empty()) {
                                Some(c) => cursor = Some(c),
                                None => break,
                            }
                        }
                        Err(e) => {
                            failure = Some(format!("model/list returned an unexpected shape: {e}"));
                            break;
                        }
                    },
                    Err(e) => {
                        failure = Some(format!("model/list failed: {e}"));
                        break;
                    }
                }
            }
            match failure {
                None => data.models = Some(entries),
                Some(f) => data.models_error = Some(f),
            }
        }
        // config/read、account/read 失败不影响别的
        if wants.config {
            match rpc.request("config/read", json!({}), step(remaining())).await {
                Ok(v) => match serde_json::from_value::<ConfigReadResponse>(v) {
                    Ok(c) => data.config = Some(c.config),
                    Err(e) => tracing::debug!(error = %e, "codex: config/read had an unexpected shape"),
                },
                Err(e) => tracing::debug!(error = %e, "codex: config/read failed"),
            }
        }
        if wants.account {
            // 不带 `refreshToken`：只读本地状态，不联网
            match rpc.request("account/read", json!({}), step(remaining())).await {
                Ok(v) => match serde_json::from_value::<AccountReadResponse>(v) {
                    Ok(a) => data.account = Some(a.into()),
                    Err(e) => data.account_error = Some(format!("account/read returned an unexpected shape: {e}")),
                },
                Err(e) => data.account_error = Some(format!("account/read failed: {e}")),
            }
        }
        Ok::<_, String>(data)
    }
    .await;

    child
        .shutdown(ShutdownTimings { after_stdin_close: Duration::from_millis(300), grace: Duration::from_secs(2) })
        .await;
    reader.abort();
    result
}

// ---------- 完整探测 ----------

/// 一次探测的完整结果（`diagnose` 要用里面的细节）。
pub struct ProbeReport {
    pub info: AgentInfo,
    pub resolution: Option<Resolution>,
    pub version: Option<Version>,
    /// `codex login status` 的结果。
    pub auth: Option<AuthState>,
    /// `account/read` 的结果。
    pub account: Option<AccountInfo>,
    /// 认证这一步最终依据什么（给 `diagnose` 看）。
    pub auth_note: Option<String>,
    /// 第 4 步的说明（成功 / 失败原因 / 跳过的原因）。
    pub models_note: Option<String>,
    /// `config/read` 里的 `model_provider`（只给诊断看，不参与判定）。
    pub provider: Option<String>,
}

impl ProbeReport {
    fn new(info: AgentInfo) -> Self {
        Self {
            info,
            resolution: None,
            version: None,
            auth: None,
            account: None,
            auth_note: None,
            models_note: None,
            provider: None,
        }
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

/// 完整探测。`probe_all`：`--version` 把所有候选都跑一遍（`doctor` 用）。`memo`：cheap 探测沿用上一次认证判定的地方。
pub async fn run_probe(ctx: &ProbeContext, probe_all: bool, memo: &AuthMemoCell) -> ProbeReport {
    let budget = Budget::new(ctx.deadline);
    let mut info = base_info(ctx);

    if !ctx.config.enabled {
        fail(
            &mut info,
            AgentStatus::Disabled,
            "disabled",
            "Codex is disabled in the config".to_string(),
            Some("set agents.codex.enabled = true"),
        );
        return ProbeReport::new(info);
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
            "probe timed out while locating the codex binary; unfinished steps: version, auth, models".to_string(),
            None,
        );
        return ProbeReport::new(info);
    };
    let mut report = ProbeReport::new(info);
    if resolution.chosen().is_none() {
        if let Some(summary) = resolution.failure_summary() {
            let code = if resolution.config_error.is_some() { "invalid_command" } else { "binary_failed" };
            fail(
                &mut report.info,
                AgentStatus::Error,
                code,
                format!("found Codex but it does not run: {summary}"),
                Some("check that `codex --version` works in a terminal"),
            );
        } else {
            fail(
                &mut report.info,
                AgentStatus::NotInstalled,
                "binary_not_found",
                "the codex command was not found".to_string(),
                Some(INSTALL_HINT),
            );
        }
        report.resolution = Some(resolution);
        return report;
    }
    report.info.path = resolution.path().map(|p| p.to_string_lossy().into_owned());

    // 2. 版本
    let version = resolution.version_output().and_then(parse_semver);
    report.info.version = version
        .as_ref()
        .map(|v| v.to_string())
        .or_else(|| resolution.version_output().map(|s| s.lines().next().unwrap_or("").to_string()));
    report.version = version.clone();
    if let Some(v) = &version {
        if !version_at_least(v, MIN_CLI_VERSION) {
            fail(
                &mut report.info,
                AgentStatus::UnsupportedVersion,
                "version_too_old",
                format!("Codex {v} is older than the minimum supported {MIN_CLI_VERSION}"),
                Some(UPGRADE_HINT),
            );
            report.resolution = Some(resolution);
            return report;
        }
    } else {
        tracing::warn!(output = ?resolution.version_output(), "codex: could not parse a version, skipping the minimum-version check");
    }

    // 3. 认证（`login status`）
    let auth_budget = budget.cap(AUTH_TIMEOUT);
    if auth_budget.is_zero() {
        fail(
            &mut report.info,
            AgentStatus::Error,
            "probe_timeout",
            "probe timed out; unfinished steps: auth, models".to_string(),
            None,
        );
        report.resolution = Some(resolution);
        return report;
    }
    let auth = auth_status(&resolution, &env, &ctx.scratch_dir, auth_budget).await;
    let login = auth.kind();
    report.auth = Some(auth);

    // 4. 模型与选项，以及（权威的）认证判定 `account/read`
    report.info.modes = modes();
    report.info.options = options();
    report.info.capabilities = capabilities();
    let program = resolution.path().expect("chosen candidate").to_path_buf();
    let mut account: Option<AccountInfo> = None;
    let mut account_error: Option<String> = None;
    // cheap 探测：`login status` 的结果和上一次一样、而且上一次的判定来自 `account/read`，就沿用
    let reusable = memo.lock().expect("auth memo lock").clone().filter(|m| {
        ctx.cheap && ctx.previous.is_some() && m.authoritative && m.login == login && m.path == report.info.path
    });

    let (models_note, models_list) = if ctx.cheap {
        if reusable.is_none() && !budget.remaining().is_zero() {
            // 登录状态变了（或没有记录）：只补跑 `account/read`，不拉模型列表
            let step = budget.cap(MODELS_TIMEOUT);
            let wants = Wants { account: true, ..Wants::default() };
            match query_app_server(&program, &resolution.prefix_args, &env, &ctx.scratch_dir, step, wants).await {
                Ok(data) => {
                    account = data.account;
                    account_error = data.account_error;
                }
                Err(e) => account_error = Some(e),
            }
        }
        (
            Some("skipped (cheap probe); models come from the previous probe".to_string()),
            ctx.previous.as_ref().map(|p| p.models.clone()),
        )
    } else if budget.remaining().is_zero() {
        (Some("skipped: the probe ran out of time".to_string()), ctx.previous.as_ref().map(|p| p.models.clone()))
    } else {
        let step = budget.cap(MODELS_TIMEOUT);
        let wants = Wants { models: true, config: true, account: true };
        match query_app_server(&program, &resolution.prefix_args, &env, &ctx.scratch_dir, step, wants).await {
            Ok(data) => {
                report.provider = data.config.as_ref().and_then(|c| c.model_provider.clone());
                account = data.account;
                account_error = data.account_error;
                match data.models {
                    Some(entries) => {
                        let mut list = models::from_list(&entries);
                        if let Some(c) = &data.config {
                            models::apply_config(&mut list, c);
                        }
                        (Some(format!("{} models from model/list", list.len())), Some(list))
                    }
                    None => {
                        // 沿用上一次的；都没有就只有 config.toml 里的那个模型（如果有）
                        let mut list = ctx.previous.as_ref().map(|p| p.models.clone()).unwrap_or_default();
                        if list.is_empty() {
                            if let Some(c) = &data.config {
                                models::apply_config(&mut list, c);
                            }
                        }
                        (Some(data.models_error.unwrap_or_else(|| "model/list gave nothing".to_string())), Some(list))
                    }
                }
            }
            Err(e) => {
                account_error = Some(e.clone());
                (Some(e), ctx.previous.as_ref().map(|p| p.models.clone()))
            }
        }
    };
    let mut list = models_list.unwrap_or_default();
    models::add_additional(&mut list, &ctx.config.additional_models);
    report.info.models = list;
    report.models_note = models_note;

    // 认证判定，优先级：
    // 1. `account/read`（权威）；
    // 2. 沿用上一次（cheap 探测且 `login status` 没变）；
    // 3. 完整探测里 `account/read` 用不了：同一个二进制上一次**权威**的判定，**且当时记下的 `login status` 和现在的相同**
    //    （`login status` 不认自定义提供方，一次 `account/read` 超时就退回它会把原来能用的会话挡掉）；
    //    这时不改记录，下一次 cheap 探测照旧比较 `login status`；
    // 4. 退回 `login status`（只有确认未登录才挡）。
    //    `login status` 变了（用户刚登录 / 退出），旧判定很可能就是错的：cheap 探测因此才来问 `account/read`，
    //    完整探测同理，都不用第 3 条。
    let last_authoritative = memo
        .lock()
        .expect("auth memo lock")
        .clone()
        .filter(|m| !ctx.cheap && m.authoritative && m.path == report.info.path && m.login == login);
    let mut keep_memo = false;
    let (blocked, authoritative, note) = match (&account, &reusable, &last_authoritative) {
        (Some(a), _, _) => (a.blocks(), true, format!("account/read: {}", a.describe())),
        (None, Some(m), _) => (m.blocked, true, "reused from the last check (login status unchanged)".to_string()),
        (None, None, Some(m)) => {
            keep_memo = true;
            (
                m.blocked,
                true,
                format!(
                    "account/read unavailable ({}); kept the last account/read verdict for this binary",
                    account_error.as_deref().unwrap_or("not asked")
                ),
            )
        }
        (None, None, None) => (
            login == LoginKind::LoggedOut,
            false,
            format!(
                "account/read unavailable ({}); fell back to codex login status",
                account_error.as_deref().unwrap_or("not asked")
            ),
        ),
    };
    report.account = account;
    report.auth_note = Some(note);
    if !keep_memo {
        *memo.lock().expect("auth memo lock") =
            Some(AuthMemo { path: report.info.path.clone(), login, blocked, authoritative });
    }
    if blocked {
        fail(
            &mut report.info,
            AgentStatus::NotAuthenticated,
            "login_required",
            "Codex is not logged in".to_string(),
            Some(LOGIN_HINT),
        );
    }
    report.resolution = Some(resolution);
    report
}

/// § 5.5 的诊断：命令来源、daemon 的 PATH / shell、`which -a`、`type -a`、各候选的探活结果、解析路径、版本、认证、模型数、状态。
pub async fn diagnose_fields(ctx: &ProbeContext) -> Vec<DiagnosticField> {
    let report = run_probe(ctx, true, &AuthMemoCell::default()).await;
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
    f.push(field(
        "account",
        report.account.as_ref().map(AccountInfo::describe).unwrap_or_else(|| "(account/read gave nothing)".to_string()),
    ));
    if let Some(n) = &report.auth_note {
        f.push(field("auth verdict", n.clone()));
    }
    f.push(field("model provider", report.provider.clone().unwrap_or_else(|| "(default: openai)".to_string())));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_status_interpretation() {
        let unk = || "boom".to_string();
        assert_eq!(
            interpret_login_status(Some(0), "", "Logged in using ChatGPT\n", unk),
            AuthState::LoggedIn { detail: Some("Logged in using ChatGPT".into()) }
        );
        assert_eq!(
            interpret_login_status(Some(0), "", "Logged in using an API key - sk-test-***l-key", unk),
            AuthState::LoggedIn { detail: Some("Logged in using an API key - sk-test-***l-key".into()) }
        );
        assert_eq!(interpret_login_status(Some(1), "", "Not logged in\n", unk), AuthState::LoggedOut);
        // 文字也可能在 stdout
        assert_eq!(interpret_login_status(Some(1), "Not logged in", "", unk), AuthState::LoggedOut);
        // 退出码 1 但不是「未登录」：auth.json 损坏之类，不能断定
        assert_eq!(
            interpret_login_status(Some(1), "", "Error checking login status: expected ident at line 1 column 2", unk),
            AuthState::Unknown("boom".into())
        );
        assert_eq!(interpret_login_status(Some(2), "", "", unk), AuthState::Unknown("boom".into()));
        assert_eq!(interpret_login_status(None, "", "", unk), AuthState::Unknown("boom".into()));
    }

    #[test]
    fn initialize_declares_stable_api_only_and_opts_out_of_the_big_notifications() {
        let p = initialize_params();
        assert_eq!(p["clientInfo"]["name"], "anycon");
        assert_eq!(p["capabilities"]["experimentalApi"], false);
        let opt_out: Vec<&str> = p["capabilities"]["optOutNotificationMethods"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(opt_out.contains(&"turn/diff/updated"), "{opt_out:?}");
        // 我们依赖的通知一个都不能关
        for needed in [
            "turn/started",
            "turn/completed",
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/commandExecution/outputDelta",
            "thread/tokenUsage/updated",
            "error",
            "serverRequest/resolved",
            "turn/plan/updated",
        ] {
            assert!(!opt_out.contains(&needed), "{needed} must not be opted out");
        }
    }

    #[test]
    fn account_info_decides_whether_the_session_is_blocked() {
        let info = |t: Option<&str>, requires| AccountInfo {
            account_type: t.map(str::to_string),
            requires_openai_auth: requires,
        };
        assert!(info(None, true).blocks());
        assert!(!info(None, false).blocks());
        assert!(!info(Some("chatgpt"), true).blocks());
        assert!(!info(Some("apiKey"), true).blocks());
        assert!(!info(Some("amazonBedrock"), false).blocks());
        // 响应的解析：缺 requiresOpenaiAuth 按需要登录算
        let r: AccountReadResponse =
            serde_json::from_value(serde_json::json!({"account": {"type": "apiKey"}})).unwrap();
        assert_eq!(AccountInfo::from(r), info(Some("apiKey"), true));
        let r: AccountReadResponse = serde_json::from_value(
            serde_json::json!({"account": null, "requiresOpenaiAuth": false, "workspaceRouting": null}),
        )
        .unwrap();
        assert_eq!(AccountInfo::from(r), info(None, false));
        assert!(info(None, true).describe().contains("required"));
    }

    #[test]
    fn static_declarations() {
        let m = modes();
        assert_eq!(
            m.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["read_only", "workspace", "auto_review", "full_access"]
        );
        assert_eq!(m.iter().filter(|m| m.default).map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["workspace"]);
        assert_eq!(
            m.iter().map(|m| m.risk).collect::<Vec<_>>(),
            vec![ModeRisk::Safe, ModeRisk::Normal, ModeRisk::Normal, ModeRisk::Dangerous]
        );
        let o = options();
        assert_eq!(o.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(), vec!["fast_mode", "model_reasoning_summary"]);
        assert_eq!(o[1].choices.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), SUMMARY_CHOICES);
        let c = capabilities();
        assert!(c.resume && c.interrupt && c.interject && c.thinking_stream && c.usage && c.env);
        assert!(c.attachments.contains(&"image/png".to_string()));
        assert!(c.attachments.contains(&"*/*".to_string()));
    }
}
