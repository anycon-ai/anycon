//! 配置：`config.toml`（figment：默认值 → 文件 → `ANYCON_*` 环境变量）与目录布局，以及热加载。
//!
//! 键见 docs/spec/daemon.md § 3。环境变量用双下划线表示层级：`ANYCON_SESSIONS__MAX_CONCURRENT=2`。
//! `ANYCON_HOME` / `ANYCON_LOG` / `ANYCON_MOCK_AGENT` 不是配置键（`ANYCON_MOCK_AGENT=1` 等价于 `agents.mock.enabled = true`）。

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_agent_api::AdapterConfig;
use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::util::{expand_tilde, home_dir, parse_duration, parse_size};

/// 目录布局（docs/spec/cli.md § 2）。
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `ANYCON_HOME`，否则 `~/.anycon`。
    pub fn from_env() -> Self {
        if let Some(h) = std::env::var_os("ANYCON_HOME").filter(|s| !s.is_empty()) {
            return Self::new(expand_tilde(&h.to_string_lossy()));
        }
        Self::new(home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".anycon"))
    }

    pub fn config_toml(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn db(&self) -> PathBuf {
        self.home.join("daemon.db")
    }
    pub fn keys_dir(&self) -> PathBuf {
        self.home.join("keys")
    }
    pub fn node_key(&self) -> PathBuf {
        self.keys_dir().join("node.key")
    }
    pub fn device_token(&self) -> PathBuf {
        self.keys_dir().join("device-token")
    }
    pub fn device_id(&self) -> PathBuf {
        self.keys_dir().join("device-id")
    }
    pub fn peers_json(&self) -> PathBuf {
        self.home.join("peers.json")
    }
    pub fn sock(&self) -> PathBuf {
        self.home.join("daemon.sock")
    }
    pub fn lock(&self) -> PathBuf {
        self.home.join("daemon.lock")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
    /// 附件（blob，ADR-35）：正式文件 `<blob_id>.<ext>`。
    pub fn blobs_dir(&self) -> PathBuf {
        self.home.join("blobs")
    }
    /// 上传中的 `<blob_id>.part`；启动时清空。
    pub fn blobs_tmp(&self) -> PathBuf {
        self.blobs_dir().join("tmp")
    }
    pub fn probe_scratch(&self) -> PathBuf {
        self.home.join("tmp").join("probe")
    }
    /// CLI 作为客户端设备时的目录：`client/<hub-host>/`。
    pub fn client_dir(&self, hub_host: &str) -> PathBuf {
        let safe: String =
            hub_host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect();
        self.home.join("client").join(safe)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    pub url: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub root: String,
    /// 可以关联为项目 / 可以被 `workspace.browse` 浏览的本机目录范围（ADR-33）。每项是绝对路径或 `~` / `~/…`
    /// （按 daemon 环境的 `HOME` 展开）；`[]` = 关闭「关联已有目录」。热加载：每次调用读最新值。
    ///
    /// 范围之内还有三道规则（[`crate::linkrules`]）：隐藏目录（路径里从所在 root 往下有以 `.` 开头的段）、内置拒绝表
    /// （`~/.ssh`、`~/Library`、`/etc`…）、绝对拒绝（workspace 根、数据目录、文件系统根）。前两道可以靠把目录（或它所在的隐藏 /
    /// 敏感目录）显式写进这个列表放行；第三道不能。
    pub link_roots: Vec<String>,
    pub trash_retention_days: u32,
    pub scratch_retention_days: u32,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: "~/anycon".into(),
            link_roots: vec!["~".into()],
            trash_retention_days: 30,
            scratch_retention_days: 7,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionsConfig {
    pub max_concurrent: usize,
    pub idle_timeout: String,
    pub max_attachment_bytes: String,
    pub history_retention: String,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            idle_timeout: "40m".into(),
            max_attachment_bytes: "10MiB".into(),
            history_retention: "forever".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub allow_dangerous_modes: bool,
    pub request_timeout: String,
    /// 允许客户端在 `session.create` / `set_settings` 里带 `settings.env`（ADR-31，默认关）。
    /// 开启后每个键还要同时满足：在 `session_env_allow` 里、过 `anycon_agent_api::env_key_allowed`（黑名单兜底）、
    /// 不覆盖运维在 `agents.env` 里配置的键。
    pub allow_session_env: bool,
    /// 客户端可以按会话设置的环境变量名（ADR-31 的允许列表）：精确键名，或以 `*` 结尾的前缀（`FEATURE_*`）；
    /// 大小写敏感；**默认空 = 一个都不允许**（裸 `*` 不是通配）。只有 `allow_session_env = true` 时才有意义。
    pub session_env_allow: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            allow_dangerous_modes: true,
            request_timeout: "0".into(),
            allow_session_env: false,
            session_env_allow: Vec::new(),
        }
    }
}

/// `[worktree]`：每会话 worktree（ADR-34，docs/spec/daemon.md § 3 / § 10.1）。整段热加载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    /// git 项目里新会话的初始选择：`worktree` | `local`。只影响客户端的初始选择；线路上不带 `workspace` 时总是 local。
    pub default: String,
    /// 新分支名 `<prefix>/<slug>`；空串 = 不加前缀。允许 `[A-Za-z0-9._-]`，1–40 个字符，不能是 `main` / `master` / `HEAD`。
    pub branch_prefix: String,
    /// 分支的 PR 合并 / 关闭后自动归档会话（要装好并登录 `gh`）。
    pub auto_archive_on_pr_close: bool,
    /// `.worktreeinclude` 复制的文件数上限。
    pub include_max_files: u64,
    /// `.worktreeinclude` 复制的总字节数上限。
    pub include_max_bytes: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            default: "worktree".into(),
            branch_prefix: "anycon".into(),
            auto_archive_on_pr_close: false,
            include_max_files: 10_000,
            include_max_bytes: "1GiB".into(),
        }
    }
}

/// `worktree.branch_prefix` 的校验：空串，或 `[A-Za-z0-9._-]{1,40}` 且拼出来是合法的分支名开头
/// （不能是 `main` / `master` / `HEAD`，不以 `.` / `-` 开头，不以 `.` 或 `.lock` 结尾，不含 `..`）。
pub fn validate_branch_prefix(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Ok(());
    }
    if p.len() > 40 {
        return Err("at most 40 characters".into());
    }
    if !p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err("only letters, digits, '.', '_' and '-' are allowed".into());
    }
    if matches!(p, "main" | "master" | "HEAD") {
        return Err("must not be main, master or HEAD".into());
    }
    if p.starts_with(['.', '-']) || p.ends_with('.') || p.ends_with(".lock") || p.contains("..") {
        return Err("is not usable as the start of a git branch name".into());
    }
    Ok(())
}

/// `[enroll]`：账号内接入（ADR-36，docs/spec/daemon.md § 3 / § 15.1）。整段热加载。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrollConfig {
    /// 接受扫码批准和接入码；`false` = 这两种都 `method_disabled`，只能短码配对或节点口令。
    pub approval: bool,
    /// 设了节点口令时接受口令接入；`false` = 口令接入 `method_disabled`。口令本身用 `anycon passwd` 设，不写在配置里。
    pub password: bool,
}

impl Default for EnrollConfig {
    fn default() -> Self {
        Self { approval: true, password: true }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// always | when_unwatched | never
    pub turn_finished: String,
    pub session_idle_after: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { turn_finished: "when_unwatched".into(), session_idle_after: "30m".into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub retention_days: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "info".into(), retention_days: 7 }
    }
}

/// `[agents]`：标量键（`env_from_login_shell` 等）和 `[agents.<id>]` 子表混在同一张表里，
/// 所以反序列化走中间形态再分拣。
#[derive(Debug, Clone, Serialize)]
pub struct AgentsConfig {
    pub env_from_login_shell: bool,
    pub probe_interval: String,
    pub probe_timeout: String,
    /// 追加到所有 agent 子进程的环境。
    pub env: HashMap<String, String>,
    #[serde(flatten)]
    pub agent: BTreeMap<String, AdapterConfig>,
}

/// 内置的真实 agent：没有 `[agents.<id>]` 段也参与探测（没装就是 `not_installed`），写 `enabled = false` 才关。
/// `mock` 不在其中：它只在显式开启时出现。
pub const BUILTIN_AGENTS: &[&str] = &["claude-code", "codex"];

/// 缺省的 `agents` 子表：每个内置 agent 一项，`enabled = true`，其余缺省。文件里的段按键覆盖它。
fn builtin_agent_defaults() -> BTreeMap<String, AdapterConfig> {
    BUILTIN_AGENTS.iter().map(|id| ((*id).to_string(), AdapterConfig { enabled: true, ..Default::default() })).collect()
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            env_from_login_shell: true,
            probe_interval: "10m".into(),
            probe_timeout: "60s".into(),
            env: HashMap::new(),
            agent: builtin_agent_defaults(),
        }
    }
}

impl PartialEq for AgentsConfig {
    fn eq(&self, other: &Self) -> bool {
        let canon = |c: &Self| serde_json::to_value(c).map(|v| crate::util::canonical_json(&v)).unwrap_or_default();
        canon(self) == canon(other)
    }
}

#[derive(Deserialize)]
struct RawAgents {
    #[serde(default = "yes")]
    env_from_login_shell: bool,
    #[serde(default = "ten_min")]
    probe_interval: String,
    #[serde(default = "sixty_s")]
    probe_timeout: String,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}
fn ten_min() -> String {
    "10m".into()
}
fn sixty_s() -> String {
    "60s".into()
}

impl<'de> Deserialize<'de> for AgentsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = RawAgents::deserialize(d)?;
        // 内置 agent 先放缺省项（不经过 figment 的缺省层、直接反序列化时也一样），文件里的段覆盖它
        let mut agent = builtin_agent_defaults();
        for (k, v) in raw.rest {
            if !v.is_object() {
                // 未知的标量键：忽略（向前兼容）
                continue;
            }
            if k == "acp" {
                // ACP agent 注册表（`[agents.acp.<name>]`）v0.1 没有实现：以前会被解析然后没人读，现在明说。
                tracing::warn!("[agents.acp.*] is not supported yet and is ignored");
            } else {
                let c: AdapterConfig =
                    serde_json::from_value(v).map_err(|e| D::Error::custom(format!("agents.{k}: {e}")))?;
                agent.insert(k, c);
            }
        }
        Ok(AgentsConfig {
            env_from_login_shell: raw.env_from_login_shell,
            probe_interval: raw.probe_interval,
            probe_timeout: raw.probe_timeout,
            env: raw.env,
            agent,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hub: HubConfig,
    pub workspace: WorkspaceConfig,
    pub sessions: SessionsConfig,
    pub permissions: PermissionsConfig,
    pub agents: AgentsConfig,
    pub worktree: WorktreeConfig,
    pub enroll: EnrollConfig,
    pub notify: NotifyConfig,
    pub log: LogConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config: {0}")]
    Parse(String),
    #[error("config invalid: {0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Config {
    /// 默认值 → `config.toml`（不存在则跳过）→ `ANYCON_*` 环境变量。
    pub fn load(paths: &Paths) -> Result<Config, ConfigError> {
        Self::load_from(&paths.config_toml())
    }

    pub fn load_from(file: &Path) -> Result<Config, ConfigError> {
        let mut fig = Figment::from(Serialized::defaults(Config::default()));
        if file.exists() {
            fig = fig.merge(Toml::file(file));
        }
        fig = fig.merge(Env::prefixed("ANYCON_").ignore(&["HOME", "LOG", "MOCK_AGENT"]).split("__"));
        let mut cfg: Config = fig.extract().map_err(|e| ConfigError::Parse(e.to_string()))?;
        if matches!(std::env::var("ANYCON_MOCK_AGENT").as_deref(), Ok("1") | Ok("true")) {
            let e = cfg
                .agents
                .agent
                .entry("mock".into())
                .or_insert_with(|| AdapterConfig { enabled: true, ..Default::default() });
            e.enabled = true;
        }
        cfg.validate()?;
        // 运维把黑名单里的键写进了允许列表：黑名单永远先拦，所以这些项不会生效。静默无效最容易被误以为配置成功了
        for (pat, why) in shadowed_env_allow(&cfg.permissions.session_env_allow) {
            tracing::warn!(
                pattern = %pat,
                "permissions.session_env_allow: {why}; the per-session environment denylist is checked first and cannot be overridden"
            );
        }
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |k: &str, e: String| ConfigError::Invalid(format!("{k}: {e}"));
        parse_duration(&self.sessions.idle_timeout).map_err(|e| bad("sessions.idle_timeout", e))?;
        parse_size(&self.sessions.max_attachment_bytes).map_err(|e| bad("sessions.max_attachment_bytes", e))?;
        if self.sessions.history_retention != "forever" {
            parse_duration(&self.sessions.history_retention).map_err(|e| bad("sessions.history_retention", e))?;
        }
        parse_duration(&self.permissions.request_timeout).map_err(|e| bad("permissions.request_timeout", e))?;
        parse_duration(&self.agents.probe_interval).map_err(|e| bad("agents.probe_interval", e))?;
        parse_duration(&self.agents.probe_timeout).map_err(|e| bad("agents.probe_timeout", e))?;
        parse_duration(&self.notify.session_idle_after).map_err(|e| bad("notify.session_idle_after", e))?;
        for r in &self.workspace.link_roots {
            validate_link_root(r).map_err(|e| bad("workspace.link_roots", e))?;
        }
        if !matches!(self.worktree.default.as_str(), "worktree" | "local") {
            return Err(bad("worktree.default", "expected worktree | local".into()));
        }
        validate_branch_prefix(&self.worktree.branch_prefix).map_err(|e| bad("worktree.branch_prefix", e))?;
        parse_size(&self.worktree.include_max_bytes).map_err(|e| bad("worktree.include_max_bytes", e))?;
        if !matches!(self.notify.turn_finished.as_str(), "always" | "when_unwatched" | "never") {
            return Err(bad("notify.turn_finished", "expected always | when_unwatched | never".into()));
        }
        if self.sessions.max_concurrent == 0 {
            return Err(bad("sessions.max_concurrent", "must be >= 1".into()));
        }
        for pat in &self.permissions.session_env_allow {
            let name = pat.strip_suffix('*').unwrap_or(pat);
            let ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(bad(
                    "permissions.session_env_allow",
                    format!("{pat:?} is not a variable name or a PREFIX_* pattern (a bare * is not a wildcard)"),
                ));
            }
        }
        Ok(())
    }

    pub fn workspace_root(&self) -> PathBuf {
        expand_tilde(&self.workspace.root)
    }
    pub fn idle_timeout(&self) -> Duration {
        parse_duration(&self.sessions.idle_timeout).unwrap_or(Duration::from_secs(2400))
    }
    /// `None` = 不超时。
    pub fn request_timeout(&self) -> Option<Duration> {
        parse_duration(&self.permissions.request_timeout).ok().filter(|d| !d.is_zero())
    }
    pub fn probe_interval(&self) -> Duration {
        parse_duration(&self.agents.probe_interval).unwrap_or(Duration::from_secs(600)).max(Duration::from_secs(1))
    }
    pub fn probe_timeout(&self) -> Duration {
        parse_duration(&self.agents.probe_timeout).unwrap_or(Duration::from_secs(60))
    }
    pub fn session_idle_after(&self) -> Duration {
        parse_duration(&self.notify.session_idle_after).unwrap_or(Duration::from_secs(1800))
    }
    /// `.worktreeinclude` 复制的字节数上限。
    pub fn worktree_include_max_bytes(&self) -> u64 {
        parse_size(&self.worktree.include_max_bytes).unwrap_or(1024 * 1024 * 1024)
    }
    pub fn max_attachment_bytes(&self) -> u64 {
        parse_size(&self.sessions.max_attachment_bytes).unwrap_or(10 * 1024 * 1024)
    }
    /// `None` = forever。
    pub fn history_retention(&self) -> Option<Duration> {
        if self.sessions.history_retention == "forever" {
            None
        } else {
            parse_duration(&self.sessions.history_retention).ok()
        }
    }
    /// 取某个 adapter 的配置。没有 `[agents.<id>]` 段 = 未配置。
    pub fn adapter_config(&self, id: &str) -> Option<&AdapterConfig> {
        self.agents.agent.get(id)
    }
    pub fn node_name(&self) -> String {
        if let Some(n) = self.hub.name.as_ref().filter(|n| !n.is_empty()) {
            return n.clone();
        }
        hostname()
    }
}

/// `workspace.link_roots` 的一项：绝对路径，或 `~` / `~/…`（不支持 `~user`），不含 NUL / 控制字符。
fn validate_link_root(r: &str) -> Result<(), String> {
    if r.is_empty() {
        return Err("an entry is empty".into());
    }
    if r.len() > 4096 || r.chars().any(|c| c.is_control()) {
        return Err(format!("{r:?} is too long or contains control characters"));
    }
    if r == "~" || r.starts_with("~/") || r.starts_with('/') {
        return Ok(());
    }
    Err(format!("{r:?} must be an absolute path, `~` or `~/…`"))
}

/// 主机名（`hostname` 命令；失败退回 `anycon-node`）。
pub fn hostname() -> String {
    // libc::gethostname：避免起子进程
    let mut buf = [0u8; 256];
    // SAFETY: buf 足够大，gethostname 只写入 buf。
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
        if !name.is_empty() {
            return name.trim_end_matches(".local").to_string();
        }
    }
    "anycon-node".into()
}

/// 配置的共享句柄：读最新值、订阅变化、热加载。
pub struct ConfigStore {
    paths: Paths,
    tx: watch::Sender<Arc<Config>>,
    last_stamp: Mutex<Option<(std::time::SystemTime, u64)>>,
}

impl ConfigStore {
    pub fn load(paths: &Paths) -> Result<Arc<Self>, ConfigError> {
        let cfg = Config::load(paths)?;
        Ok(Self::with_config(paths.clone(), cfg))
    }

    pub fn with_config(paths: Paths, cfg: Config) -> Arc<Self> {
        let (tx, _rx) = watch::channel(Arc::new(cfg));
        let stamp = file_stamp(&paths.config_toml());
        Arc::new(Self { paths, tx, last_stamp: Mutex::new(stamp) })
    }

    pub fn get(&self) -> Arc<Config> {
        self.tx.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Config>> {
        self.tx.subscribe()
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// 重读文件。内容有变化返回 `Ok(true)`；解析失败保留旧配置并返回错误。
    pub fn reload(&self) -> Result<bool, ConfigError> {
        let new = Config::load(&self.paths)?;
        let changed = self.tx.send_if_modified(|cur| {
            if **cur == new {
                false
            } else {
                *cur = Arc::new(new);
                true
            }
        });
        *self.last_stamp.lock().unwrap() = file_stamp(&self.paths.config_toml());
        Ok(changed)
    }

    /// 轮询 `config.toml` 的 mtime / 长度，变了就热加载（`config set` 直接写文件）。
    pub fn spawn_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(500));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let stamp = file_stamp(&this.paths.config_toml());
                let changed = { *this.last_stamp.lock().unwrap() != stamp };
                if changed {
                    match this.reload() {
                        Ok(true) => tracing::info!("config reloaded"),
                        Ok(false) => {}
                        Err(e) => {
                            tracing::error!(error = %e, "config reload failed; keeping previous config");
                            *this.last_stamp.lock().unwrap() = stamp;
                        }
                    }
                }
            }
        })
    }
}

fn file_stamp(p: &Path) -> Option<(std::time::SystemTime, u64)> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok().map(|t| (t, m.len())))
}

/// 读取一个点分隔的键（`sessions.max_concurrent`、`agents.claude-code.default_model`），来源是「默认值 + 文件 + 环境」合并后的配置。
pub fn get_value(cfg: &Config, key: &str) -> Option<serde_json::Value> {
    let mut cur = serde_json::to_value(cfg).ok()?;
    for seg in key.split('.') {
        cur = cur.get(seg)?.clone();
    }
    Some(cur)
}

/// 进程内串行化 `set_value`（读-改-写）。跨进程（`anycon config set` 与 daemon 的 `set_agent_defaults`）靠 [`ConfigFileLock`]。
static WRITE_LOCK: Mutex<()> = Mutex::new(());
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `<home>/config.toml.lock` 上的 `flock`，drop 时释放。最多等 10 秒。
struct ConfigFileLock {
    _file: std::fs::File,
}

impl ConfigFileLock {
    fn acquire(home: &Path) -> Result<Self, ConfigError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(home)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(home.join("config.toml.lock"))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: flock 只操作我们刚打开的 fd。
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                return Ok(ConfigFileLock { _file: file });
            }
            if std::time::Instant::now() >= deadline {
                return Err(ConfigError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "config.toml is locked by another process",
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// 写一个唯一文件名的 0600 临时文件（同目录，便于 rename 原子替换）。
fn write_unique_private(dir: &Path, stem: &str, content: &str) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let n = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let p = dir.join(format!("{stem}.{}.{n}.tmp", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    Ok(p)
}

/// 改 `config.toml` 里的一个键（保留注释和格式）。`raw` 先按 TOML 值解析（`true`、`3`、`"x"`、`["a"]`），
/// 解析不了就当字符串。写之前先校验整份配置，非法则不写。
///
/// 读-改-写在进程内锁和 `config.toml.lock`（flock）里完成，临时文件名唯一，结果权限 0600
/// （`agents.*.env` 里可能放着 API key）。
pub fn set_value(paths: &Paths, key: &str, raw: &str) -> Result<(), ConfigError> {
    set_values(paths, &[(key, raw)])
}

/// [`set_value`] 的异步版：读-改-写里有 `flock` 等待和同步文件 IO（争用时最多等 10 秒），放进 blocking 线程，
/// 别堵住 tokio 的工作线程。
pub async fn set_value_blocking(paths: &Paths, key: &str, raw: &str) -> Result<(), ConfigError> {
    set_values_blocking(paths, vec![(key.to_string(), raw.to_string())]).await
}

/// [`set_values`] 的异步版（见 [`set_value_blocking`]）。
pub async fn set_values_blocking(paths: &Paths, edits: Vec<(String, String)>) -> Result<(), ConfigError> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        let refs: Vec<(&str, &str)> = edits.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        set_values(&paths, &refs)
    })
    .await
    .map_err(|e| ConfigError::Io(std::io::Error::other(format!("config write task failed: {e}"))))?
}

/// 一次读-改-写里改多个键：要么全部写入要么都不写（校验失败整体放弃）。
pub fn set_values(paths: &Paths, edits: &[(&str, &str)]) -> Result<(), ConfigError> {
    use toml_edit::{DocumentMut, Item, Table, Value};
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _flock = ConfigFileLock::acquire(&paths.home)?;
    let file = paths.config_toml();
    let text = if file.exists() { std::fs::read_to_string(&file)? } else { String::new() };
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| ConfigError::Parse(e.to_string()))?;
    for (key, raw) in edits {
        let segs: Vec<&str> = key.split('.').collect();
        if segs.is_empty() || segs.iter().any(|s| s.is_empty()) {
            return Err(ConfigError::Invalid(format!("bad key: {key}")));
        }
        let mut value: Value = raw.parse::<Value>().unwrap_or_else(|_| Value::from(*raw));
        let mut tbl: &mut Table = doc.as_table_mut();
        for seg in &segs[..segs.len() - 1] {
            let entry = tbl.entry(seg).or_insert(Item::Table(Table::new()));
            if entry.is_inline_table() {
                let t = entry.as_inline_table().cloned().unwrap().into_table();
                *entry = Item::Table(t);
            }
            tbl = entry.as_table_mut().ok_or_else(|| ConfigError::Invalid(format!("{seg} is not a table")))?;
            tbl.set_implicit(true);
        }
        let last = segs[segs.len() - 1];
        // 覆盖已有值时保留它的行内注释
        if let Some(old) = tbl.get(last).and_then(|i| i.as_value()) {
            *value.decor_mut() = old.decor().clone();
        }
        tbl.insert(last, Item::Value(value));
    }
    let new_text = doc.to_string();
    // 先校验再落盘：不影响环境变量覆盖，只校验文件本身
    let check_path = write_unique_private(&paths.home, ".config-check", &new_text)?;
    let check = Config::load_from(&check_path);
    let _ = std::fs::remove_file(&check_path);
    check?;
    let staged = write_unique_private(&paths.home, "config.toml", &new_text)?;
    if let Err(e) = std::fs::rename(&staged, &file) {
        let _ = std::fs::remove_file(&staged);
        return Err(e.into());
    }
    Ok(())
}

/// 默认的 `config.toml` 文本（`anycon init` 用）。
pub fn default_config_toml(hub_url: Option<&str>, node_name: Option<&str>, workspace_root: &str) -> String {
    let mut s = String::new();
    s.push_str("# anycon daemon 配置。热加载的键：workspace.link_roots sessions.* permissions.* notify.* worktree.* enroll.* log.level agents.<id>.default_* agents.<id>.enabled\n\n");
    s.push_str("[hub]\n");
    match hub_url {
        Some(u) => s.push_str(&format!("url = {}\n", toml_str(u))),
        None => s.push_str("# url = \"https://hub.example.com\"\n"),
    }
    match node_name {
        Some(n) => s.push_str(&format!("name = {}\n", toml_str(n))),
        None => s.push_str("# name = \"my-machine\"\n"),
    }
    s.push_str("\n[workspace]\n");
    s.push_str(&format!("root = {}\n", toml_str(workspace_root)));
    s.push_str("# 可以关联为项目（workspace.link）/ 浏览的本机目录范围；绝对路径或 ~/…；[] = 关闭。关联的目录不会被移动或删除。\n");
    s.push_str(
        "# 范围之内，隐藏目录（~/.x、~/work/.y）和凭证 / 系统 / 自启动目录（~/.ssh、~/Library、/etc…）默认不能关联；\n",
    );
    s.push_str("# 要关联它们，把那个目录显式写进这个列表（例如 \"~/.config/nvim\"）。关联后 agent 按所选权限模式在里面工作，文件读取不经审批\n");
    s.push_str("link_roots = [\"~\"]\n");
    s.push_str("trash_retention_days = 30\nscratch_retention_days = 7\n");
    s.push_str("\n[sessions]\nmax_concurrent = 3\nidle_timeout = \"40m\"\n");
    s.push_str("# 单个图片附件的上限；一条消息的附件合计另有 20 MiB 的上限（固定，不可配置）\n");
    s.push_str("max_attachment_bytes = \"10MiB\"\nhistory_retention = \"forever\"\n");
    s.push_str("\n[permissions]\nallow_dangerous_modes = true\nrequest_timeout = \"0\"\nallow_session_env = false\n");
    s.push_str("# 客户端可以按会话设置哪些环境变量（精确名或 PREFIX_*；默认空 = 都不允许；还要 allow_session_env = true，且黑名单里的键永远不行）\n");
    s.push_str("# session_env_allow = [\"MY_FLAG\", \"FEATURE_*\"]\n");
    s.push_str("\n[agents]\nenv_from_login_shell = true\nprobe_interval = \"10m\"\n");
    s.push_str("\n[agents.claude-code]\nenabled = true\n");
    s.push_str("\n[worktree]\n");
    s.push_str("# 每会话 git worktree（热加载）。default 只影响客户端的初始选择：worktree | local\n");
    s.push_str("default = \"worktree\"\n");
    s.push_str("# 新分支名 <prefix>/<slug>；空串 = 不加前缀。只允许字母 / 数字 / . _ -，最多 40 个字符，不能是 main / master / HEAD\n");
    s.push_str("branch_prefix = \"anycon\"\n");
    s.push_str("# 分支的 PR 合并 / 关闭后自动归档会话（要装好并登录 gh）\n");
    s.push_str("auto_archive_on_pr_close = false\n");
    s.push_str("# 仓库根的 .worktreeinclude 列出的被忽略文件（.env、node_modules…）复制到新 worktree 的上限\n");
    s.push_str("include_max_files = 10000\ninclude_max_bytes = \"1GiB\"\n");
    s.push_str("\n[enroll]\n");
    s.push_str("# 账号内接入（热加载）。approval：接受扫码批准和接入码；false = 只能短码配对或节点口令\n");
    s.push_str("approval = true\n");
    s.push_str("# password：设了节点口令时接受口令接入（口令本身用 `anycon passwd` 设，不写在这里）\n");
    s.push_str("password = true\n");
    s.push_str("\n[notify]\nturn_finished = \"when_unwatched\"\nsession_idle_after = \"30m\"\n");
    s.push_str("\n[log]\nlevel = \"info\"\nretention_days = 7\n");
    s
}

fn toml_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 某个配置键是否支持热加载（`config set` 提示用）。
pub fn is_hot_key(key: &str) -> bool {
    if key == "workspace.link_roots"
        || key.starts_with("sessions.")
        || key.starts_with("permissions.")
        || key.starts_with("notify.")
        || key.starts_with("worktree.")
        || key.starts_with("enroll.")
        || key == "log.level"
    {
        return true;
    }
    if let Some(rest) = key.strip_prefix("agents.") {
        let tail = rest.rsplit('.').next().unwrap_or("");
        return tail == "enabled" || tail.starts_with("default_");
    }
    false
}

/// `permissions.session_env_allow` 里被黑名单（`anycon_agent_api::env_key_allowed`）盖住的项，及原因。
///
/// - 精确键名命中黑名单：这一项完全无效；
/// - `PREFIX_*`：前缀本身命中黑名单，整项无效；黑名单里有键落在这个前缀下，则那几个键无效，前缀下其余的键照常放行。
///
/// 黑名单永远先于允许列表（`env::validate_session_env` 先查黑名单），所以这些项不会生效。
pub fn shadowed_env_allow(allow: &[String]) -> Vec<(String, String)> {
    use anycon_agent_api::{env_key_allowed, ENV_DENY_EXACT, ENV_DENY_PREFIXES, ENV_PROTECTED};
    let mut out = Vec::new();
    for pat in allow {
        match pat.strip_suffix('*') {
            None => {
                if let Err(reason) = env_key_allowed(pat) {
                    out.push((pat.clone(), format!("{pat} has no effect: {reason}")));
                }
            }
            Some("") => {}
            Some(prefix) => {
                if let Err(reason) = env_key_allowed(prefix) {
                    out.push((pat.clone(), format!("{pat} has no effect: {reason}")));
                    continue;
                }
                let mut denied: Vec<&str> = ENV_PROTECTED
                    .iter()
                    .chain(ENV_DENY_EXACT.iter())
                    .copied()
                    .filter(|k| k.starts_with(prefix))
                    .collect();
                denied.extend(
                    ENV_DENY_PREFIXES.iter().copied().filter(|p| p.starts_with(prefix) && p.len() > prefix.len()),
                );
                denied.sort_unstable();
                denied.dedup();
                if !denied.is_empty() {
                    out.push((
                        pat.clone(),
                        format!(
                            "{pat} does not cover the denylisted {} (the rest of the family is allowed)",
                            denied.join(", ")
                        ),
                    ));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) {
        std::fs::write(dir.join("config.toml"), body).unwrap();
    }

    #[test]
    fn loads_defaults_without_file() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::load(&Paths::new(d.path())).unwrap();
        assert_eq!(cfg.sessions.max_concurrent, 3);
        assert_eq!(cfg.idle_timeout(), Duration::from_secs(2400));
        assert!(cfg.request_timeout().is_none());
    }

    #[test]
    fn parses_mixed_agents_table() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            r#"
[hub]
url = "https://hub.example.com"
[agents]
env_from_login_shell = false
probe_interval = "5m"
env = { FOO = "bar" }

[agents.claude-code]
enabled = true
command = "claude --foo"
default_model = "opus"
env = { X = "1" }

[agents.codex]
enabled = false

[agents.acp.opencode]
command = "opencode acp"
dangerous_modes = ["yolo"]
"#,
        );
        let cfg = Config::load(&Paths::new(d.path())).unwrap();
        assert!(!cfg.agents.env_from_login_shell);
        assert_eq!(cfg.agents.probe_interval, "5m");
        assert_eq!(cfg.agents.env.get("FOO").map(String::as_str), Some("bar"));
        let cc = cfg.adapter_config("claude-code").unwrap();
        assert!(cc.enabled);
        assert_eq!(cc.command.as_deref(), Some("claude --foo"));
        assert_eq!(cc.default_model.as_deref(), Some("opus"));
        assert!(!cfg.adapter_config("codex").unwrap().enabled);
        // `[agents.acp.*]` v0.1 不支持：忽略，不当作 adapter
        assert!(cfg.adapter_config("acp").is_none());
    }

    #[test]
    fn validation_rejects_bad_values() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "[sessions]\nidle_timeout = \"soon\"\n");
        assert!(matches!(Config::load(&Paths::new(d.path())), Err(ConfigError::Invalid(_))));
        write(d.path(), "[sessions]\nmax_concurrent = 0\n");
        assert!(Config::load(&Paths::new(d.path())).is_err());
    }

    #[test]
    fn session_env_allow_defaults_to_empty_and_rejects_wildcards_that_are_not_prefixes() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        assert!(Config::load(&paths).unwrap().permissions.session_env_allow.is_empty());
        set_value(&paths, "permissions.session_env_allow", "[\"MY_FLAG\", \"FEATURE_*\"]").unwrap();
        assert_eq!(
            Config::load(&paths).unwrap().permissions.session_env_allow,
            vec!["MY_FLAG".to_string(), "FEATURE_*".to_string()]
        );
        // 裸 `*`、空串、带奇怪字符的都是配置错误（不会落盘）
        for bad in ["[\"*\"]", "[\"\"]", "[\"A-B\"]", "[\"A B\"]"] {
            assert!(set_value(&paths, "permissions.session_env_allow", bad).is_err(), "{bad}");
        }
        assert_eq!(Config::load(&paths).unwrap().permissions.session_env_allow.len(), 2);
        // 示例配置里是注释掉的示例，默认仍为空
        let example = default_config_toml(None, None, "~/anycon");
        assert!(example.contains("# session_env_allow = ["));
        write(d.path(), &example);
        assert!(Config::load(&paths).unwrap().permissions.session_env_allow.is_empty());
    }

    #[test]
    fn a_single_attachment_may_be_at_most_ten_mib_by_default() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        // 缺省值（一条消息的图片要装进 Claude 一次请求的上限里，50 MiB 一张太大）
        assert_eq!(Config::load(&paths).unwrap().max_attachment_bytes(), 10 * 1024 * 1024);
        // 示例配置里写的就是这个值，并且解释了合计上限
        let example = default_config_toml(None, None, "/tmp/ws");
        assert!(example.contains("max_attachment_bytes = \"10MiB\""));
        assert!(example.contains("20 MiB"));
        write(d.path(), &example);
        assert_eq!(Config::load(&paths).unwrap().max_attachment_bytes(), 10 * 1024 * 1024);
        // 仍然可以调
        set_value(&paths, "sessions.max_attachment_bytes", "\"4MiB\"").unwrap();
        assert_eq!(Config::load(&paths).unwrap().max_attachment_bytes(), 4 * 1024 * 1024);
        assert!(set_value(&paths, "sessions.max_attachment_bytes", "\"lots\"").is_err());
    }

    #[test]
    fn the_worktree_section_has_documented_defaults_and_validates_its_values() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.worktree.default, "worktree");
        assert_eq!(cfg.worktree.branch_prefix, "anycon");
        assert!(!cfg.worktree.auto_archive_on_pr_close);
        assert_eq!((cfg.worktree.include_max_files, cfg.worktree_include_max_bytes()), (10_000, 1024 * 1024 * 1024));
        // 示例配置里写的就是这些默认值，解析得出来
        write(d.path(), &default_config_toml(None, None, "/tmp/ws"));
        assert_eq!(Config::load(&paths).unwrap().worktree, WorktreeConfig::default());
        for (k, v) in [
            ("worktree.default", "\"local\""),
            ("worktree.branch_prefix", "\"\""),
            ("worktree.branch_prefix", "\"team-1.x_y\""),
            ("worktree.include_max_files", "5"),
            ("worktree.include_max_bytes", "\"10MiB\""),
            ("worktree.auto_archive_on_pr_close", "true"),
        ] {
            set_value(&paths, k, v).unwrap_or_else(|e| panic!("{k} = {v}: {e}"));
        }
        let cfg = Config::load(&paths).unwrap();
        assert_eq!((cfg.worktree.default.as_str(), cfg.worktree.branch_prefix.as_str()), ("local", "team-1.x_y"));
        assert_eq!((cfg.worktree.include_max_files, cfg.worktree_include_max_bytes()), (5, 10 * 1024 * 1024));
        for (k, v) in [
            ("worktree.default", "\"elsewhere\""),
            ("worktree.branch_prefix", "\"main\""),
            ("worktree.branch_prefix", "\"master\""),
            ("worktree.branch_prefix", "\"HEAD\""),
            ("worktree.branch_prefix", "\"a/b\""),
            ("worktree.branch_prefix", "\"with space\""),
            ("worktree.branch_prefix", "\".hidden\""),
            ("worktree.branch_prefix", "\"-dash\""),
            ("worktree.branch_prefix", "\"ends.lock\""),
            ("worktree.branch_prefix", "\"a..b\""),
            ("worktree.branch_prefix", "\"ünï\""),
            ("worktree.include_max_bytes", "\"lots\""),
        ] {
            assert!(set_value(&paths, k, v).is_err(), "{k} = {v} must be rejected");
        }
        // 40 个字符可以，41 个不行
        assert!(validate_branch_prefix(&"a".repeat(40)).is_ok());
        assert!(validate_branch_prefix(&"a".repeat(41)).is_err());
        // 被拒绝的值没有写进文件
        assert_eq!(Config::load(&paths).unwrap().worktree.branch_prefix, "team-1.x_y");
    }

    #[test]
    fn the_enroll_section_defaults_on_loads_from_the_file_and_is_hot() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        assert_eq!(Config::load(&paths).unwrap().enroll, EnrollConfig { approval: true, password: true });
        // `anycon init` 写出的默认配置里有 [enroll]，而且读得回来
        std::fs::write(paths.config_toml(), default_config_toml(None, None, "~/anycon")).unwrap();
        let text = std::fs::read_to_string(paths.config_toml()).unwrap();
        assert!(text.contains("[enroll]") && text.contains("approval = true") && text.contains("password = true"));
        assert_eq!(Config::load(&paths).unwrap().enroll, EnrollConfig::default());
        set_value(&paths, "enroll.approval", "false").unwrap();
        set_value(&paths, "enroll.password", "false").unwrap();
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.enroll, EnrollConfig { approval: false, password: false });
        assert_eq!(get_value(&cfg, "enroll.approval"), Some(serde_json::json!(false)));
        // 类型不对的值不写盘
        assert!(set_value(&paths, "enroll.approval", "\"sometimes\"").is_err());
        assert!(!Config::load(&paths).unwrap().enroll.approval);
        // 整段热加载
        assert!(is_hot_key("enroll.approval") && is_hot_key("enroll.password"));
    }

    #[test]
    fn hot_keys() {
        assert!(is_hot_key("sessions.max_concurrent"));
        assert!(is_hot_key("agents.claude-code.default_model"));
        assert!(is_hot_key("agents.claude-code.enabled"));
        assert!(is_hot_key("log.level"));
        assert!(is_hot_key("workspace.link_roots"));
        // `[worktree]` 整段热加载（ADR-34）
        for k in [
            "worktree.default",
            "worktree.branch_prefix",
            "worktree.auto_archive_on_pr_close",
            "worktree.include_max_files",
            "worktree.include_max_bytes",
        ] {
            assert!(is_hot_key(k), "{k}");
        }
        assert!(!is_hot_key("hub.url"));
        assert!(!is_hot_key("workspace.root"));
    }

    #[test]
    fn set_and_get_values_preserving_comments() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        write(d.path(), "# keep me\n[sessions]\nmax_concurrent = 2  # inline\n");
        set_value(&paths, "sessions.max_concurrent", "4").unwrap();
        set_value(&paths, "agents.claude-code.default_model", "opus").unwrap();
        set_value(&paths, "agents.claude-code.enabled", "true").unwrap();
        set_value(&paths, "hub.url", "http://127.0.0.1:8080").unwrap();
        let text = std::fs::read_to_string(paths.config_toml()).unwrap();
        assert!(text.contains("# keep me") && text.contains("# inline"), "{text}");
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(cfg.sessions.max_concurrent, 4);
        assert_eq!(cfg.adapter_config("claude-code").unwrap().default_model.as_deref(), Some("opus"));
        assert!(cfg.adapter_config("claude-code").unwrap().enabled);
        assert_eq!(get_value(&cfg, "sessions.max_concurrent"), Some(serde_json::json!(4)));
        assert_eq!(get_value(&cfg, "agents.claude-code.default_model"), Some(serde_json::json!("opus")));
        assert_eq!(get_value(&cfg, "nope.nothing"), None);
        // 非法值不落盘
        assert!(set_value(&paths, "sessions.idle_timeout", "soon").is_err());
        assert_eq!(std::fs::read_to_string(paths.config_toml()).unwrap(), text);
        // 数组与字符串回退
        set_value(&paths, "agents.claude-code.dangerous_modes", "[\"yolo\"]").unwrap();
        assert_eq!(
            Config::load(&paths).unwrap().adapter_config("claude-code").unwrap().dangerous_modes,
            vec!["yolo".to_string()]
        );
        set_value(&paths, "hub.name", "my box").unwrap();
        assert_eq!(Config::load(&paths).unwrap().hub.name.as_deref(), Some("my box"));
    }

    #[test]
    fn concurrent_set_value_calls_lose_no_updates_and_the_file_stays_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        write(d.path(), "[agents.claude-code]\nenabled = true\n");
        let handles: Vec<_> = (0..12)
            .map(|i| {
                let paths = paths.clone();
                std::thread::spawn(move || {
                    set_value(&paths, &format!("agents.claude-code.env.K{i}"), &format!("\"v{i}\"")).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let cfg = Config::load(&paths).unwrap();
        let env = &cfg.adapter_config("claude-code").unwrap().env;
        for i in 0..12 {
            assert_eq!(
                env.get(&format!("K{i}")).map(String::as_str),
                Some(format!("v{i}").as_str()),
                "update {i} was lost"
            );
        }
        // 重写之后仍然是 0600（里面可能有 API key），也没有遗留临时文件
        let mode = std::fs::metadata(paths.config_toml()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let leftovers: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp") || n.starts_with(".config-check"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        // 批量写入：一个非法值整批放弃
        let before = std::fs::read_to_string(paths.config_toml()).unwrap();
        assert!(set_values(&paths, &[("sessions.max_concurrent", "9"), ("sessions.idle_timeout", "soon")]).is_err());
        assert_eq!(std::fs::read_to_string(paths.config_toml()).unwrap(), before);
    }

    #[test]
    fn default_toml_parses() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), &default_config_toml(Some("http://127.0.0.1:8080"), Some("test \"node\""), "/tmp/ws"));
        let cfg = Config::load(&Paths::new(d.path())).unwrap();
        assert_eq!(cfg.hub.url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(cfg.hub.name.as_deref(), Some("test \"node\""));
        assert!(cfg.adapter_config("claude-code").unwrap().enabled);
    }

    #[tokio::test]
    async fn hot_reload_picks_up_file_changes() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "[sessions]\nmax_concurrent = 2\n");
        let store = ConfigStore::load(&Paths::new(d.path())).unwrap();
        assert_eq!(store.get().sessions.max_concurrent, 2);
        let mut rx = store.subscribe();
        let _w = store.spawn_watcher();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        write(d.path(), "[sessions]\nmax_concurrent = 5\nidle_timeout = \"3s\"\n");
        tokio::time::timeout(Duration::from_secs(5), rx.changed()).await.unwrap().unwrap();
        assert_eq!(store.get().sessions.max_concurrent, 5);
        assert_eq!(store.get().idle_timeout(), Duration::from_secs(3));
        // 非法内容：保留旧值
        write(d.path(), "[sessions]\nmax_concurrent = \"x\"\n");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(store.get().sessions.max_concurrent, 5);
    }

    #[test]
    fn env_allow_entries_that_the_denylist_always_overrides_are_reported() {
        let a = |v: &[&str]| shadowed_env_allow(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // 命中黑名单的精确键、前缀、受保护的键、npm_config_（不分大小写）：整项无效
        let r = a(&["LD_PRELOAD", "NODE_OPTIONS", "PATH", "npm_config_registry", "LD_*", "FEATURE_X", "MY_*"]);
        let pats: Vec<&str> = r.iter().map(|(p, _)| p.as_str()).collect();
        assert!(pats.contains(&"LD_PRELOAD") && pats.contains(&"PATH") && pats.contains(&"LD_*"), "{r:?}");
        assert!(pats.contains(&"npm_config_registry"), "{r:?}");
        assert!(!pats.contains(&"FEATURE_X") && !pats.contains(&"MY_*"), "ordinary entries are fine: {r:?}");
        // 前缀下有黑名单里的键：那几个键无效，其余照常放行
        let r = a(&["SSL_*"]);
        assert_eq!(r.len(), 1);
        assert!(r[0].1.contains("SSL_CERT_FILE") && r[0].1.contains("rest of the family"), "{r:?}");
        // 前缀本身被黑名单盖住（XDG_ 整个家族）：整项无效，不是「部分」
        let r = a(&["XDG_*"]);
        assert!(r.len() == 1 && r[0].1.contains("has no effect"), "{r:?}");
        assert!(a(&[]).is_empty() && a(&["*"]).is_empty());
    }

    #[test]
    fn builtin_agents_participate_without_a_section_and_the_file_overrides_them() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        // 没有配置文件：内置的真实 agent 默认启用，mock 不在
        let cfg = Config::load(&paths).unwrap();
        for id in BUILTIN_AGENTS {
            let ac = cfg.adapter_config(id).unwrap_or_else(|| panic!("{id} has no default entry"));
            assert!(ac.enabled && ac.command.is_none() && ac.default_model.is_none(), "{id}: {ac:?}");
        }
        assert_eq!(BUILTIN_AGENTS, &["claude-code", "codex"]);
        assert!(cfg.adapter_config("mock").is_none());
        assert!(Config::default().adapter_config("codex").unwrap().enabled);
        // 文件里写别的段不影响内置项；写 `enabled = false` 才关；其余键按键覆盖
        write(d.path(), "[agents.codex]\nenabled = false\n\n[agents.claude-code]\ndefault_model = \"opus\"\n");
        let cfg = Config::load(&paths).unwrap();
        assert!(!cfg.adapter_config("codex").unwrap().enabled);
        let cc = cfg.adapter_config("claude-code").unwrap();
        assert!(cc.enabled, "a section that does not mention `enabled` keeps the default");
        assert_eq!(cc.default_model.as_deref(), Some("opus"));
        write(d.path(), "[agents]\nprobe_interval = \"5m\"\n\n[agents.mock]\nenabled = true\n");
        let cfg = Config::load(&paths).unwrap();
        assert!(cfg.adapter_config("claude-code").unwrap().enabled && cfg.adapter_config("codex").unwrap().enabled);
        assert!(cfg.adapter_config("mock").unwrap().enabled);
        // `config set` 之后仍然成立，读出来的值也包含内置项
        set_value(&paths, "agents.codex.enabled", "false").unwrap();
        let cfg = Config::load(&paths).unwrap();
        assert_eq!(get_value(&cfg, "agents.codex.enabled"), Some(serde_json::json!(false)));
        assert_eq!(get_value(&cfg, "agents.claude-code.enabled"), Some(serde_json::json!(true)));
        // 解析前后一致（热加载的相等比较不能被内置项搅乱）
        assert_eq!(Config::load(&paths).unwrap(), cfg);
    }

    #[test]
    fn link_roots_default_validation_and_array_values_via_config_set() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        assert_eq!(Config::load(&paths).unwrap().workspace.link_roots, vec!["~".to_string()]);
        // `anycon config set workspace.link_roots '["/abs/path"]'`：值按 TOML 数组解析
        set_value(&paths, "workspace.link_roots", "[\"/abs/path\", \"~/work\", \"~\"]").unwrap();
        assert_eq!(
            Config::load(&paths).unwrap().workspace.link_roots,
            vec!["/abs/path".to_string(), "~/work".to_string(), "~".to_string()]
        );
        assert_eq!(
            get_value(&Config::load(&paths).unwrap(), "workspace.link_roots"),
            Some(serde_json::json!(["/abs/path", "~/work", "~"]))
        );
        set_value(&paths, "workspace.link_roots", "[]").unwrap();
        assert!(Config::load(&paths).unwrap().workspace.link_roots.is_empty());
        // 每项必须是绝对路径或 ~ / ~/…；非法值不落盘
        let before = std::fs::read_to_string(paths.config_toml()).unwrap();
        for bad in
            ["[\"relative/path\"]", "[\"\"]", "[\"~user/x\"]", "[\"./x\"]", "[\"/a\\nb\"]", "[\"/ok\", \"nope\"]"]
        {
            assert!(matches!(set_value(&paths, "workspace.link_roots", bad), Err(ConfigError::Invalid(_))), "{bad}");
        }
        assert_eq!(std::fs::read_to_string(paths.config_toml()).unwrap(), before);
        // 示例配置带着缺省值，能解析
        let example = default_config_toml(None, None, "~/anycon");
        assert!(example.contains("link_roots = [\"~\"]"));
        write(d.path(), &example);
        assert_eq!(Config::load(&paths).unwrap().workspace.link_roots, vec!["~".to_string()]);
    }
}
