//! daemon ↔ adapter 的接口。`crates/adapters` 实现 [`Adapter`]，`crates/daemon` 调用。两边只依赖这里和 protocol。
//!
//! 一个 adapter 对应一种 agent（claude-code、codex、mock…）。daemon 对每个会话调用 [`Adapter::spawn`]
//! 得到一个 [`AgentHandle`]，之后通过它收事件、发用户输入、答复权限、改设置、取消、关闭。
//!
//! 语义见 docs/spec/daemon.md § 5–9。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anycon_protocol::{
    AgentInfo, AttachmentRef, Event, ModelInfo, PermissionDetail, PermissionKind, PermissionOption, SessionSettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `config.agents.<id>` 的内容，daemon 读 toml 后交给 adapter。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdapterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 命令；可以是绝对路径或带参数的字符串（shell-words 切分）。空 = adapter 默认。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub default_thinking: Option<String>,
    #[serde(default)]
    pub default_mode: Option<String>,
    #[serde(default)]
    pub default_options: serde_json::Map<String, Value>,
    #[serde(default)]
    pub additional_models: Vec<ModelInfo>,
    /// ACP 等不提供风险等级的 agent，由用户标记哪些模式是 dangerous。
    #[serde(default)]
    pub dangerous_modes: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// 探测上下文。
#[derive(Debug, Clone)]
pub struct ProbeContext {
    /// daemon 环境（登录 shell 解析后 + config 追加）。找二进制、起子进程都用它。
    pub env: Arc<HashMap<String, String>>,
    pub config: AdapterConfig,
    /// 整个 probe 的总预算，adapter 自己在内部各步分配。
    pub deadline: Duration,
    /// 为 true 时跳过慢的第 4 步（模型列表），沿用 `previous` 里的模型 / 选项。
    pub cheap: bool,
    /// 上一次的结果，便于 cheap 探测复用。
    pub previous: Option<AgentInfo>,
    /// 探测子进程用的临时 cwd（workspace 之外）。
    pub scratch_dir: PathBuf,
}

/// 续接所需的信息，daemon 持久化在 sessions 表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumeHandle {
    /// agent 自己的会话 id（Claude Code session_id、Codex thread id、ACP session id）。
    pub agent_session_id: String,
    /// 启动时用过的、resume 时必须原样再传的参数（adapter 私有格式）。
    #[serde(default)]
    pub launch_args: Value,
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub session_id: String,
    /// 项目目录或 scratch 目录。
    pub cwd: PathBuf,
    pub settings: SessionSettings,
    /// daemon 环境 + config env + settings.env（已按白名单过滤）。
    pub env: Arc<HashMap<String, String>>,
    pub config: AdapterConfig,
    /// Some = 续接已有 agent 会话。
    pub resume: Option<ResumeHandle>,
    /// 探测结果（校验设置、取模型默认值用）。
    pub agent_info: Arc<AgentInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UserInput {
    pub message_id: String,
    pub content: String,
    pub attachments: Vec<AttachmentRef>,
    /// 附件的本地路径（daemon 已落盘），与 `attachments` 一一对应。
    pub attachment_paths: Vec<PathBuf>,
    /// daemon 要把这条消息**插进正在进行的 turn**（turn 进行中、`capabilities.interject` 为 true）。
    ///
    /// - `true`：adapter 只能把它送进当前这个 turn。adapter 那边已经没有进行中的 turn（刚结束、`TurnDone`
    ///   还在通道里没被 daemon 读到）时返回 [`AdapterError::TurnEnded`]，**不要自己开新 turn**——
    ///   daemon 会把消息放回队列，按正常流程作为下一个 turn 发出。当前 turn 已经在取消中（`cancel` 发出、
    ///   还没收尾）也算「没有进行中的 turn」，同样返回 `TurnEnded`。
    /// - `false`：开一个新 turn。
    pub interject: bool,
}

/// 客户端对权限请求的答复，已由 daemon 校验过 option_id 在该请求的 options 里。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PermissionDecision {
    pub option_id: Option<String>,
    pub message: Option<String>,
    pub answers: Option<serde_json::Map<String, Value>>,
}

/// adapter 发起的权限请求（不含 session_id，daemon 补）。
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPermission {
    pub request_id: String,
    pub kind: PermissionKind,
    pub title: String,
    pub detail: PermissionDetail,
    pub options: Vec<PermissionOption>,
    pub expires_in: Option<Duration>,
}

/// adapter → daemon 的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    /// 进程起来并拿到 agent 会话 id。resume 时也发（id 可能不变）。
    Started { resume: ResumeHandle },
    /// 直接进会话事件流的事件。adapter **不要**发 `UserMessage`、`TurnStart`、`Status`、
    /// `Permission`、`PermissionResolved`、`SettingsChanged`、`TitleChanged`——这些由 daemon 生成。
    Event(Event),
    /// 权限 / 计划 / 提问。daemon 转成 `Permission` 事件 + `permission.request` 通知，答复通过
    /// [`AgentHandle::respond_permission`] 回来。
    Permission(PendingPermission),
    /// agent 自己给会话起的标题（ACP `session_info_update`）。
    Title(String),
    /// agent **自己**开始了一个 turn，不是 [`AgentHandle::send`] 触发的：Claude Code 的后台任务（后台 subagent、
    /// 后台命令）结束后，CLI 会自己再跑一轮处理结果。daemon 据此开一个没有用户消息的 turn，结束照常是
    /// `Event::TurnDone`。adapter 只在**没有**进行中的 turn 时发；`send` 恰好撞上这样一轮时，adapter 负责把
    /// 两者理顺（Claude Code：等这一轮结束再把消息写进去，见 `crates/adapters/src/claude_code/README.md`），
    /// daemon 已经有 turn 时收到它直接忽略。
    TurnStarted,
    /// 进程里还在跑的后台任务数（变化时发，含降到 0）。大于 0 时 daemon 不按 `idle_timeout` 回收进程——
    /// 回收会连同这些任务一起杀掉（docs/spec/daemon.md § 7.3）。进程退出后 daemon 自己清零，adapter 不用补发。
    Background { tasks: u32 },
    /// 进程退出。`error` 为空表示正常退出（如 idle 超时后 daemon 主动关的）。
    Exited { code: Option<i32>, error: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsApply {
    /// 已对当前进程生效。
    pub applied_now: bool,
    /// 需要 daemon 在下一 turn 前重启进程（resume）才能生效。
    pub restart_required: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("agent not installed: {0}")]
    NotInstalled(String),
    #[error("agent not authenticated: {0}")]
    NotAuthenticated(String),
    #[error("unsupported agent version: {0}")]
    UnsupportedVersion(String),
    #[error("invalid setting {field}: {reason}")]
    InvalidSettings { field: String, reason: String },
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("agent protocol error: {0}")]
    Protocol(String),
    #[error("agent exited: {0}")]
    Exited(String),
    /// 插话（`UserInput.interject = true`）时目标 turn 已经结束。daemon 把消息退回队列，不算 turn 失败。
    #[error("the turn already ended")]
    TurnEnded,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, AdapterError>;

#[async_trait::async_trait]
pub trait Adapter: Send + Sync + 'static {
    /// 稳定 id，也是 `config.agents.<id>` 的键。
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// `claude-stream-json` | `codex-app-server` | `acp` | `mock`
    fn driver(&self) -> &'static str;

    /// 探测。永远返回一个 `AgentInfo`（失败也要返回 status + issue），不要 panic、不要无限等。
    async fn probe(&self, ctx: &ProbeContext) -> AgentInfo;

    /// 校验设置是否被该 agent 接受（daemon 在 `session.create` / `set_settings` 时调）。
    /// 默认实现按 `agent_info` 的 models / modes / options 检查；adapter 可覆盖加自己的规则。
    fn validate_settings(&self, info: &AgentInfo, settings: &SessionSettings) -> Result<()> {
        default_validate(info, settings)
    }

    /// 拉起或续接一个 agent 会话。
    async fn spawn(&self, req: SpawnRequest) -> Result<Box<dyn AgentHandle>>;
}

#[async_trait::async_trait]
///
/// **不阻塞约定**：daemon 在等 `send` / `respond_permission` / `set_settings` / `cancel` 返回的期间**不读** `events()`。
/// 所以这些调用不能因为事件通道满了而卡住：adapter 读 agent 输出的循环必须先处理 RPC 响应 / 控制响应，
/// 转发事件不能挡住它（例如读行与转发分成两个任务、中间用不限长的队列）。
pub trait AgentHandle: Send {
    /// 事件通道。adapter 内部的读取任务往里写；daemon 从这里收。进程结束后发 `Exited` 并关闭通道。
    fn events(&mut self) -> &mut tokio::sync::mpsc::Receiver<AgentEvent>;

    /// 发送用户输入。`input.interject = false`：开始一个新 turn。`input.interject = true`（只在 turn 进行中、
    /// `capabilities.interject` 为 true 时由 daemon 发出）：送进当前 turn；当前 turn 已经结束返回
    /// [`AdapterError::TurnEnded`]，不要自己开 turn。不支持插话的 adapter 收到 `interject = true` 返回
    /// `AdapterError::Unsupported`。插话失败（任何错误）daemon 都把消息退回队列，不会因此让当前 turn 失败。
    async fn send(&mut self, input: UserInput) -> Result<()>;

    async fn respond_permission(&mut self, request_id: &str, decision: PermissionDecision) -> Result<()>;

    /// 会话中改设置。只在 turn 不进行时被调用。
    async fn set_settings(&mut self, settings: &SessionSettings) -> Result<SettingsApply>;

    /// 中断当前 turn，不杀进程。幂等。
    async fn cancel(&mut self) -> Result<()>;

    /// 关闭进程（idle 超时、删除、daemon 退出）。先礼后兵：SIGINT → 5 秒 → SIGKILL。
    async fn shutdown(&mut self) -> Result<()>;

    /// 当前可用于续接的信息。
    fn resume_handle(&self) -> Option<ResumeHandle>;

    /// 当前是否有 turn 在跑（daemon 以自己的状态机为准，这个只用于一致性检查）。
    fn is_busy(&self) -> bool;

    /// agent 主进程的 pid（它也是进程组 id：adapter 用 `process_group(0)` 拉起）。daemon 据此判断某个监听端口
    /// 是不是这个会话起的（dev server 预览，ADR-39）。没有本地进程的 adapter（远程、mock）返回 `None`。
    fn pid(&self) -> Option<u32> {
        None
    }
}

/// 按 `AgentInfo` 做通用校验。
pub fn default_validate(info: &AgentInfo, settings: &SessionSettings) -> Result<()> {
    if let Some(model) = &settings.model {
        let known = info.models.iter().any(|m| &m.id == model);
        if !known && !info.allow_custom_model {
            return Err(AdapterError::InvalidSettings {
                field: "model".into(),
                reason: format!("unknown model {model}"),
            });
        }
        if let Some(th) = &settings.thinking {
            if let Some(m) = info.models.iter().find(|m| &m.id == model) {
                match &m.thinking {
                    Some(t) if t.options.iter().any(|o| &o.id == th) => {}
                    Some(_) => {
                        return Err(AdapterError::InvalidSettings {
                            field: "thinking".into(),
                            reason: format!("{th} not supported by {model}"),
                        })
                    }
                    None => {
                        return Err(AdapterError::InvalidSettings {
                            field: "thinking".into(),
                            reason: format!("{model} has no thinking options"),
                        })
                    }
                }
            }
        }
    }
    if let Some(mode) = &settings.mode {
        if !info.modes.iter().any(|m| &m.id == mode) {
            return Err(AdapterError::InvalidSettings { field: "mode".into(), reason: format!("unknown mode {mode}") });
        }
    }
    for (k, _) in settings.options.iter() {
        if !info.options.iter().any(|o| &o.id == k) {
            return Err(AdapterError::InvalidSettings {
                field: format!("options.{k}"),
                reason: "unknown option".into(),
            });
        }
    }
    Ok(())
}

/// 把「只改 model、thinking 不在新模型里」回落到新模型默认值。返回是否改动。
pub fn reconcile_thinking(info: &AgentInfo, settings: &mut SessionSettings) -> bool {
    let Some(model) = settings.model.as_ref() else { return false };
    let Some(m) = info.models.iter().find(|m| &m.id == model) else { return false };
    match (&m.thinking, &settings.thinking) {
        (Some(t), Some(th)) if !t.options.iter().any(|o| &o.id == th) => {
            settings.thinking = t.default.clone();
            true
        }
        (None, Some(_)) => {
            settings.thinking = None;
            true
        }
        _ => false,
    }
}

/// 不进会话 env 白名单：这些键不允许被 `settings.env` 覆盖。后半是 Windows 上同样地位的变量（家目录、系统目录、临时目录）。
pub const ENV_PROTECTED: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "SYSTEMROOT",
    "WINDIR",
    "TEMP",
    "TMP",
];

/// 环境变量名在本平台上的规范写法。Windows 的环境变量不区分大小写（`Path` 和 `PATH` 是同一个变量），统一成大写；
/// 其它平台原样。daemon 和 adapter 往环境表里放变量、按名字查找或比较之前都先过它，免得同一个变量以两种写法同时存在
/// （交给子进程时哪个生效取决于顺序），也免得 `path` 这样的写法绕过按名字的检查。
pub fn env_key_canonical(key: &str) -> std::borrow::Cow<'_, str> {
    if cfg!(windows) && key.bytes().any(|b| b.is_ascii_lowercase()) {
        std::borrow::Cow::Owned(key.to_ascii_uppercase())
    } else {
        std::borrow::Cow::Borrowed(key)
    }
}

/// 环境表里的家目录（必须是绝对路径）：unix 看 `HOME`；Windows 上原生程序用 `USERPROFILE`，先看它再看 `HOME`
/// （Git Bash 之类会设一个 `/c/Users/…` 形式的 `HOME`，那不是绝对路径，会被跳过）。键按 [`env_key_canonical`] 查。
pub fn env_home(env: &HashMap<String, String>) -> Option<&str> {
    let keys: &[&str] = if cfg!(windows) { &["USERPROFILE", "HOME"] } else { &["HOME"] };
    keys.iter().filter_map(|k| env.get(*k)).map(String::as_str).find(|h| std::path::Path::new(h).is_absolute())
}

/// 会话 env 的黑名单前缀：能让子进程在审批之外执行代码、或把请求 / 依赖改发到别处的变量。
/// 前缀匹配（`LD_` 匹配 `LD_PRELOAD`），大小写敏感（Windows 上按 [`env_key_canonical`] 不区分大小写）。
///
/// **这份名单不可能枚举完**（实测 `SHELLOPTS` + `PS4`、`LESSOPEN` 都能执行代码），所以它只是
/// 第二道防线；第一道是 daemon 配置里的允许列表 `permissions.session_env_allow`（ADR-31），
/// 键要先在允许列表里、再通过这里的检查。
pub const ENV_DENY_PREFIXES: &[&str] = &[
    // 动态链接 / 运行时注入
    "LD_",
    "DYLD_",
    "GCONV_",
    "CORECLR_",
    // 本项目和各 agent / 模型厂商的变量（凭证、base URL）
    "ANYCON_",
    "CLAUDE",
    "ANTHROPIC_",
    "OPENAI_",
    "CODEX_",
    "GEMINI_",
    "GOOGLE_",
    "AWS_",
    "AZURE_",
    "GH_",
    "GITHUB_",
    "SSH_",
    "GIT_",
    // 语言运行时 / 工具链（启动钩子、编译器、包源）
    "PYTHON",
    "PIP_",
    "UV_",
    "NODE_",
    "NPM_CONFIG_",
    "npm_config_",
    "YARN_",
    "PNPM_",
    "BUN_",
    "DENO_",
    "RUBY",
    "GEM_",
    "BUNDLE_",
    "PERL",
    "LUA_",
    "JAVA_",
    "JDK_",
    "_JAVA_",
    "MAVEN_",
    "GRADLE_",
    "DOTNET_",
    "NUGET_",
    "CARGO_",
    "RUSTC",
    "RUSTDOC",
    "RUSTFLAGS",
    "RUSTUP_",
    "GOFLAGS",
    "GOPROXY",
    "GOPRIVATE",
    "GONOSUM",
    "GONOPROXY",
    "GOINSECURE",
    "GOENV",
    "GOPATH",
    "GOROOT",
    "GOTOOLCHAIN",
    "PHP",
    "R_",
    "DOCKER_",
    "KUBE",
    "HELM_",
    "ELECTRON_",
    "OPENSSL_",
    "XDG_",
    // 代理
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];
/// 会话 env 的黑名单键名。
pub const ENV_DENY_EXACT: &[&str] = &[
    // shell 启动 / 调试钩子
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS0",
    "PS1",
    "PS2",
    "PS4",
    "PROMPT_COMMAND",
    "ZDOTDIR",
    "FPATH",
    "IFS",
    "CDPATH",
    "GLOBIGNORE",
    "HISTFILE",
    // 会被各种工具拉起的外部程序
    "EDITOR",
    "VISUAL",
    "PAGER",
    "GIT_PAGER",
    "MANPAGER",
    "LESSOPEN",
    "LESSCLOSE",
    "LESSSECURE",
    "VIMINIT",
    "EXINIT",
    "MYVIMRC",
    "BROWSER",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
    "SUDO_ASKPASS",
    "SSH_ASKPASS",
    "DISPLAY_ASKPASS",
    // Windows：`cmd /c` 用哪个解释器、哪些扩展名算可执行、PowerShell 从哪里加载模块
    "COMSPEC",
    "PATHEXT",
    "PSMODULEPATH",
    // 编译器 / 构建器
    "CC",
    "CXX",
    "CPP",
    "LD",
    "AR",
    "AS",
    "NM",
    "RANLIB",
    "STRIP",
    "MAKEFLAGS",
    "MFLAGS",
    "MAKE",
    "CLASSPATH",
    "GCC_EXEC_PREFIX",
    "COMPILER_PATH",
    "LIBRARY_PATH",
    "CMAKE_PREFIX_PATH",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_SYSROOT_DIR",
    // 证书 / 名字解析 / 本地化（都能改请求去向或加载文件）
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "CURL_HOME",
    "REQUESTS_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "HOSTALIASES",
    "RESOLV_HOST_CONF",
    "LOCPATH",
    "NLSPATH",
    "TZDIR",
    "TERMINFO",
    "TERMINFO_DIRS",
    "KUBECONFIG",
    "PHPRC",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
];

/// 运维配置的会话 env 允许列表（`permissions.session_env_allow`）是否放行 `key`。
/// 每一项是精确键名，或以 `*` 结尾的前缀（`FEATURE_*`）；大小写敏感（Windows 上不区分）；列表为空就什么都不放行。
pub fn env_key_in_allowlist(key: &str, allow: &[String]) -> bool {
    let key = env_key_canonical(key);
    allow.iter().any(|pat| {
        let pat = env_key_canonical(pat);
        match pat.strip_suffix('*') {
            Some(prefix) => !prefix.is_empty() && key.starts_with(prefix),
            None => *pat == *key,
        }
    })
}

/// 会话 env 的键是否允许。规则：`^[A-Za-z_][A-Za-z0-9_]*$`、不在 `ENV_PROTECTED`、不命中黑名单。
/// 这是第二道防线：daemon 还要求 `permissions.allow_session_env = true`（默认 false）、键在
/// `permissions.session_env_allow` 里（`env_key_in_allowlist`）、不覆盖运维配置的键，并审计键名。
pub fn env_key_allowed(key: &str) -> std::result::Result<(), String> {
    let valid_chars = !key.is_empty()
        && key.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_chars {
        return Err("invalid variable name".into());
    }
    // Windows 上 `path` 就是 `PATH`：按规范写法比较
    let canonical = env_key_canonical(key);
    if ENV_PROTECTED.contains(&&*canonical) || ENV_DENY_EXACT.contains(&&*canonical) {
        return Err(format!("{key} may not be set per session"));
    }
    if let Some(p) = ENV_DENY_PREFIXES.iter().find(|p| canonical.starts_with(*p)) {
        return Err(format!("variables starting with {p} may not be set per session"));
    }
    // npm 读 `npm_config_*` 不分大小写（`NPM_CONFIG_REGISTRY`、`Npm_Config_Registry` 都算）
    if key.to_ascii_lowercase().starts_with("npm_config_") {
        return Err("variables starting with npm_config_ (any case) may not be set per session".into());
    }
    Ok(())
}

/// 从 daemon 环境里剔除「我正在某个 agent 会话里」的标记，避免拉起的 agent 拒绝嵌套启动。
pub const ENV_STRIP: &[&str] =
    &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "CLAUDE_CODE_SSE_PORT", "CLAUDE_AGENT_SDK_VERSION"];

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn denylist_covers_known_code_execution_vectors() {
        // 实测过能执行代码的，以及同类的
        for key in [
            "SHELLOPTS",
            "PS4",
            "LESSOPEN",
            "BASH_ENV",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "LUA_INIT",
            "LUA_INIT_5_4",
            "VIMINIT",
            "MAKEFLAGS",
            "CC",
            "RUSTC_WRAPPER",
            "_JAVA_OPTIONS",
            "JDK_JAVA_OPTIONS",
            "npm_config_registry",
            "NPM_CONFIG_SCRIPT_SHELL",
            "Npm_Config_Registry",
            "OPENSSL_CONF",
            "CLASSPATH",
            "GCC_EXEC_PREFIX",
            "XDG_DATA_DIRS",
            "ELECTRON_RUN_AS_NODE",
            "PIP_INDEX_URL",
            "UV_INDEX_URL",
            "GOPROXY",
            "DOCKER_HOST",
            "KUBECONFIG",
            "GCONV_PATH",
            "LOCPATH",
            "HOSTALIASES",
            "ANTHROPIC_BASE_URL",
            "HTTPS_PROXY",
            "PATH",
            "HOME",
        ] {
            assert!(env_key_allowed(key).is_err(), "{key} must be denied");
        }
        for key in ["MY_FLAG", "FEATURE_X", "DEBUG", "RUST_LOG", "TERM"] {
            assert!(env_key_allowed(key).is_ok(), "{key} should pass the denylist");
        }
        // Windows 的变量；换个大小写在 Windows 上是同一个变量，同样拒绝
        for key in ["COMSPEC", "PATHEXT", "USERPROFILE", "SYSTEMROOT"] {
            assert!(env_key_allowed(key).is_err(), "{key} must be denied");
        }
        for key in ["path", "Path", "ComSpec", "Ld_Preload"] {
            assert_eq!(env_key_allowed(key).is_err(), cfg!(windows), "{key}");
        }
        for key in ["", "1ABC", "A-B", "A B", "é"] {
            assert!(env_key_allowed(key).is_err(), "{key:?} is not a valid name");
        }
    }

    #[test]
    fn allowlist_matches_exact_and_prefix_only() {
        let allow = vec!["MY_FLAG".to_string(), "FEATURE_*".to_string(), "*".to_string()];
        assert!(env_key_in_allowlist("MY_FLAG", &allow));
        assert!(env_key_in_allowlist("FEATURE_X", &allow));
        assert!(!env_key_in_allowlist("MY_FLAG2", &allow));
        // 大小写敏感；Windows 的环境变量不区分大小写，`feature_x` 就是 `FEATURE_X`
        assert_eq!(env_key_in_allowlist("feature_x", &allow), cfg!(windows));
        // 裸 `*` 不是通配，防止手滑放行一切
        assert!(!env_key_in_allowlist("ANYTHING", &allow));
        assert!(!env_key_in_allowlist("MY_FLAG", &[]));
    }
}
