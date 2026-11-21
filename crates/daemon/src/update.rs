//! 更新检查（ADR-57，docs/spec/daemon.md § 23）：大约每天一次 `GET https://anycon.ai/releases.json`，有更新的正式版本时
//! 在 `node.info.update_available`、`anycon doctor` 和一行 info 日志里提示。不下载、不安装。
//!
//! - **请求**：只有 `Host` 和 `User-Agent: anycon/<版本> (<os>; <arch>)`，没有查询串、cookie 或任何账号 / 设备 / 节点 / 安装的标识。
//!   anycon.ai 的访问日志按这个 User-Agent 数活跃的 daemon（国家、版本、平台），所以它的格式是对外的约定，见 [`user_agent`]。
//! - **开关**：`[update] check`（缺省 `true`，热加载）和环境变量 `ANYCON_UPDATE_CHECK`（`0` / `false` / `off` / `no` 关，
//!   `1` / `true` / `on` / `yes` 开，盖过文件）。开发构建（`debug_assertions`）缺省不联系 anycon.ai，要 `ANYCON_UPDATE_CHECK=1` 才检查：
//!   `just dev`、e2e 和 `cargo test` 起的 daemon 都是开发构建。不进远程修改的白名单（ADR-43）。
//! - **时间**：daemon 起来之后随机 2–10 分钟做第一次；成功之后 24 小时 + 随机 0–60 分钟再做；失败按 2 小时起翻倍退避，最多 24 小时。
//!   时间按墙上时钟算（每次最多睡 30 分钟再看），机器睡眠醒来后不会推迟一整轮。上次成功 / 尝试的时间和结果存在数据目录的
//!   `update-check.json`，重启不会多发请求。
//! - **网络**：只用 HTTPS（webpki 根证书）；整次请求 10 秒超时；回应体最多 16 KiB；不跟随任何重定向（不是 200 就算失败）。
//!   测试可以经 [`Endpoint::Custom`] 指向本机回环地址上的 `http://`，这只能由嵌入 daemon 的代码设置，没有配置键或环境变量。
//! - **回应不可信**：`latest` 必须是严格的 `X.Y.Z` 并且比自己新才算有更新；给用户看的链接由版本号拼出来
//!   （[`release_url`]），回应里的 `url` 和别的字段都不用。错误信息不带回应的内容，所以日志、终端和 Web UI 里能出现的
//!   只有校验过的版本号。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_protocol::{DoctorCheck, NodeUpdateAvailable};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::watch;

use crate::config::{Config, ConfigStore, Paths};
use crate::util::now_ms;

/// 生产环境的检查地址。
pub const RELEASES_URL: &str = "https://anycon.ai/releases.json";
/// 给用户看的发布页链接的前缀，后面接 `v<版本>`。
pub const RELEASE_PAGE_PREFIX: &str = "https://github.com/anycon-ai/anycon/releases/tag/v";
/// 状态文件名（数据目录下）。
pub const STATE_FILE: &str = "update-check.json";
/// 环境变量开关。
pub const ENV_SWITCH: &str = "ANYCON_UPDATE_CHECK";

/// 整次请求（DNS、连接、TLS、请求、读完回应体）的超时。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 回应体上限。
pub const MAX_BODY_BYTES: usize = 16 * 1024;
/// 两次成功检查之间至少隔多久。
pub const INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// 在 [`INTERVAL`] 之上再加的随机量上限。
const INTERVAL_JITTER_MS: u64 = 60 * 60 * 1000;
/// daemon 起来之后第一次检查的随机延迟范围（毫秒）。
const FIRST_DELAY_MS: std::ops::RangeInclusive<u64> = (2 * 60 * 1000)..=(10 * 60 * 1000);
/// 失败退避：第一次失败之后 2 小时，之后翻倍，最多 24 小时。
const BACKOFF_MIN_MS: u64 = 2 * 3600 * 1000;
const BACKOFF_MAX_MS: u64 = 24 * 3600 * 1000;
/// 等待时每次最多睡多久再看墙上时钟。
const MAX_SLEEP: Duration = Duration::from_secs(30 * 60);
/// 连续失败到这个次数时打一行 warn（之后不再重复）。
const WARN_AFTER_FAILURES: u32 = 3;

/// daemon 自己的版本。
pub const RUNNING_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `anycon/<版本> (<os>; <arch>)`，如 `anycon/0.3.1 (linux; x86_64)`。`os` / `arch` 是 Rust 的 `std::env::consts`
/// （`linux` / `macos` / `windows`，`x86_64` / `aarch64`）。anycon.ai 的统计脚本按这个格式解析，改它要同时改那边。
pub fn user_agent() -> String {
    format!("anycon/{RUNNING_VERSION} ({}; {})", std::env::consts::OS, std::env::consts::ARCH)
}

/// 发布页链接：只由校验过的版本号拼出来。
pub fn release_url(v: &Version) -> String {
    format!("{RELEASE_PAGE_PREFIX}{v}")
}

// ---------------------------------------------------------------- 版本

/// 严格的 `X.Y.Z`：三段十进制数字，不带 `v`、前导零（`0` 本身除外）、预发布或构建后缀，每段最多 9 位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    pub fn parse_strict(s: &str) -> Option<Version> {
        let mut it = s.split('.');
        let mut part = || -> Option<u32> {
            let p = it.next()?;
            let ok = !p.is_empty()
                && p.len() <= 9
                && p.bytes().all(|b| b.is_ascii_digit())
                && !(p.len() > 1 && p.starts_with('0'));
            if ok {
                p.parse().ok()
            } else {
                None
            }
        };
        let v = Version { major: part()?, minor: part()?, patch: part()? };
        if it.next().is_some() {
            return None;
        }
        Some(v)
    }
}

/// `latest` 是否比正在运行的版本新。运行的版本可能带预发布后缀（`0.4.0-rc.1`）：同一个 `X.Y.Z` 的正式版比它新。
/// 运行的版本解析不了时不提示（宁可漏报）。
pub fn is_newer(latest: &Version, running: &str) -> bool {
    let (core, pre) = match running.split_once(['-', '+']) {
        Some((c, rest)) => (c, running.as_bytes()[c.len()] == b'-' && !rest.is_empty()),
        None => (running, false),
    };
    let Some(cur) = Version::parse_strict(core) else { return false };
    *latest > cur || (*latest == cur && pre)
}

// ---------------------------------------------------------------- 开关

/// 为什么没在检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// `ANYCON_UPDATE_CHECK=0`。
    Env,
    /// `[update] check = false`（或 `ANYCON_UPDATE__CHECK=false`）。
    Config,
    /// 开发构建，没有 `ANYCON_UPDATE_CHECK=1`。
    DevBuild,
    /// 嵌入 daemon 的代码关掉了（测试）。
    Embedder,
}

impl Off {
    pub fn describe(self) -> &'static str {
        match self {
            Off::Env => "off (ANYCON_UPDATE_CHECK=0)",
            Off::Config => "off ([update] check = false in config.toml)",
            Off::DevBuild => "off in development builds (set ANYCON_UPDATE_CHECK=1 to check)",
            Off::Embedder => "off (disabled by the program that runs the daemon)",
        }
    }
}

/// `ANYCON_UPDATE_CHECK` 的取值：`Some(false)` 关、`Some(true)` 开、`None` 没设或认不出来（认不出来时打一行 warn）。
pub fn parse_env_switch(v: Option<&str>) -> Option<bool> {
    let v = v?.trim();
    match v.to_ascii_lowercase().as_str() {
        "" => None,
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => {
            tracing::warn!("{ENV_SWITCH} has an unrecognized value; expected 0 or 1, ignoring it");
            None
        }
    }
}

fn env_switch() -> Option<bool> {
    parse_env_switch(std::env::var(ENV_SWITCH).ok().as_deref())
}

/// 检查去哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// [`RELEASES_URL`]。开发构建缺省不联系它（[`Off::DevBuild`]）。
    Releases,
    /// 测试用：别的地址（`https://`，或本机回环地址上的 `http://`）。不受开发构建的规则限制。
    Custom(String),
    /// 不检查。
    Off,
}

/// 判断现在该不该检查。环境变量盖过文件，和别的配置键一样。
pub fn decide(cfg_check: bool, env: Option<bool>, endpoint: &Endpoint, dev_build: bool) -> Result<(), Off> {
    if *endpoint == Endpoint::Off {
        return Err(Off::Embedder);
    }
    match env {
        Some(false) => return Err(Off::Env),
        Some(true) => return Ok(()),
        None => {}
    }
    if !cfg_check {
        return Err(Off::Config);
    }
    if dev_build && *endpoint == Endpoint::Releases {
        return Err(Off::DevBuild);
    }
    Ok(())
}

fn decide_now(cfg: &Config, endpoint: &Endpoint) -> Result<(), Off> {
    decide(cfg.update.check, env_switch(), endpoint, cfg!(debug_assertions))
}

// ---------------------------------------------------------------- 状态

/// `update-check.json`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// 最近一次成功检查的时间。
    pub last_success_ms: Option<u64>,
    /// 那次检查得到的最新版本（校验过的 `X.Y.Z`）。
    pub latest: Option<String>,
    /// 最近一次尝试（成功或失败）的时间。
    pub last_attempt_ms: Option<u64>,
    /// 连续失败的次数。
    pub failures: u32,
}

impl State {
    fn latest_version(&self) -> Option<Version> {
        self.latest.as_deref().and_then(Version::parse_strict)
    }
}

pub fn state_path(paths: &Paths) -> PathBuf {
    paths.home.join(STATE_FILE)
}

/// 读状态文件；不存在、坏了或者内容不合格都当作没有记录（`latest` 不合格时去掉）。
pub fn load_state(path: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(path) else { return State::default() };
    let mut s: State = serde_json::from_str(&text).unwrap_or_default();
    if s.latest.as_deref().is_some_and(|l| Version::parse_strict(l).is_none()) {
        s.latest = None;
        s.last_success_ms = None;
    }
    s
}

fn save_state(path: &Path, s: &State) {
    let text = serde_json::to_string(s).unwrap_or_default();
    crate::util::warn_err("write the update check state", crate::identity::write_private(path, &text));
}

/// 第 `failures` 次连续失败之后等多久（毫秒）。
fn backoff_ms(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(8);
    BACKOFF_MIN_MS.saturating_mul(1 << shift).min(BACKOFF_MAX_MS)
}

/// 下一次检查的时间（墙上时钟，毫秒）：不早于 `earliest`，离上次成功至少 24 小时 + `jitter_ms`，失败时离上次尝试至少退避的时长。
/// 记录里的时间在 `now` 之后（时钟被往回调过）时按 `now` 算，不会因此停很久。
pub fn next_due(s: &State, now: u64, earliest: u64, jitter_ms: u64) -> u64 {
    let mut due = earliest;
    if let Some(t) = s.last_success_ms {
        due = due.max(t.min(now) + INTERVAL.as_millis() as u64 + jitter_ms);
    }
    if s.failures > 0 {
        if let Some(t) = s.last_attempt_ms {
            due = due.max(t.min(now) + backoff_ms(s.failures));
        }
    }
    due
}

// ---------------------------------------------------------------- 请求

/// 一次检查失败的原因。只有固定的文字和数字，不带回应的内容。
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("unsupported update check url")]
    BadUrl,
    #[error("timed out")]
    Timeout,
    #[error("connect: {0}")]
    Connect(std::io::ErrorKind),
    #[error("tls handshake failed")]
    Tls,
    #[error("http exchange failed")]
    Http,
    #[error("unexpected HTTP status {0}")]
    Status(u16),
    #[error("response body unreadable or larger than {MAX_BODY_BYTES} bytes")]
    Body,
    #[error("response is not the expected JSON")]
    Json,
    #[error("`latest` is not a plain X.Y.Z version")]
    BadVersion,
}

/// 解析好的检查地址。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    tls: bool,
    host: String,
    port: u16,
    /// `Host` 头。
    authority: String,
    path: String,
}

/// `https://host[:port]/path`；`http://` 只接受本机回环地址（测试用）。不接受查询串、片段和用户信息。
fn parse_target(url: &str) -> Result<Target, CheckError> {
    let (scheme, rest) = url.split_once("://").ok_or(CheckError::BadUrl)?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        _ => return Err(CheckError::BadUrl),
    };
    if rest.contains(['?', '#', '@']) || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(CheckError::BadUrl);
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let (h, p) = stripped.split_once(']').ok_or(CheckError::BadUrl)?;
        let port = match p {
            "" => default_port,
            p => p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or(CheckError::BadUrl)?,
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| CheckError::BadUrl)?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(CheckError::BadUrl);
    }
    if !tls {
        let loopback = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(CheckError::BadUrl);
        }
    }
    Ok(Target { tls, host, port, authority: authority.to_string(), path: path.to_string() })
}

/// `releases.json` 的回应。只取 `latest`；`url`、`published_at` 和别的字段都不用（链接由版本号拼出来）。
#[derive(Deserialize)]
struct ReleasesJson {
    latest: String,
}

/// 解析回应体，校验 `latest`。
pub fn parse_response(body: &[u8]) -> Result<Version, CheckError> {
    let r: ReleasesJson = serde_json::from_slice(body).map_err(|_| CheckError::Json)?;
    Version::parse_strict(&r.latest).ok_or(CheckError::BadVersion)
}

/// 被丢弃时结束连接任务（超时取消外层 future 时不留下后台任务）。
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn exchange<S>(io: S, t: &Target) -> Result<Bytes, CheckError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) =
        http1::handshake::<_, Empty<Bytes>>(TokioIo::new(io)).await.map_err(|_| CheckError::Http)?;
    let _conn = AbortOnDrop(tokio::spawn(async move {
        let _ = conn.await;
    }));
    // 只有这两个头：没有 cookie、Accept-Language 之类会区分用户的东西
    let req = http::Request::get(t.path.as_str())
        .header(http::header::HOST, t.authority.as_str())
        .header(http::header::USER_AGENT, user_agent())
        .body(Empty::<Bytes>::new())
        .map_err(|_| CheckError::Http)?;
    let resp = sender.send_request(req).await.map_err(|_| CheckError::Http)?;
    // 不跟随重定向：3xx 和别的非 200 一样算失败
    if resp.status() != http::StatusCode::OK {
        return Err(CheckError::Status(resp.status().as_u16()));
    }
    let body = Limited::new(resp.into_body(), MAX_BODY_BYTES).collect().await.map_err(|_| CheckError::Body)?;
    Ok(body.to_bytes())
}

/// 请求一次，返回校验过的最新版本。
pub async fn fetch_latest(url: &str, timeout: Duration) -> Result<Version, CheckError> {
    let t = parse_target(url)?;
    let work = async {
        let tcp = TcpStream::connect((t.host.as_str(), t.port)).await.map_err(|e| CheckError::Connect(e.kind()))?;
        let body = if t.tls {
            let name = rustls::pki_types::ServerName::try_from(t.host.clone()).map_err(|_| CheckError::BadUrl)?;
            let tls = tokio_rustls::TlsConnector::from(crate::hub::tls_config())
                .connect(name, tcp)
                .await
                .map_err(|_| CheckError::Tls)?;
            exchange(tls, &t).await?
        } else {
            exchange(tcp, &t).await?
        };
        parse_response(&body)
    };
    tokio::time::timeout(timeout, work).await.map_err(|_| CheckError::Timeout)?
}

// ---------------------------------------------------------------- 检查器

/// 嵌入 daemon 时的选项（[`crate::DaemonOptions::update_check`]）。
#[derive(Debug, Clone)]
pub struct UpdateCheckOptions {
    pub endpoint: Endpoint,
    /// 覆盖第一次检查的随机延迟（测试用）。
    pub first_delay: Option<Duration>,
    /// 覆盖请求超时（测试用）。
    pub timeout: Duration,
}

impl Default for UpdateCheckOptions {
    fn default() -> Self {
        Self { endpoint: Endpoint::Releases, first_delay: None, timeout: REQUEST_TIMEOUT }
    }
}

impl UpdateCheckOptions {
    /// 不检查（测试夹具用）。
    pub fn off() -> Self {
        Self { endpoint: Endpoint::Off, ..Default::default() }
    }
}

pub struct UpdateChecker {
    config: Arc<ConfigStore>,
    path: PathBuf,
    opts: UpdateCheckOptions,
    state: Mutex<State>,
}

impl UpdateChecker {
    pub fn new(paths: &Paths, config: Arc<ConfigStore>, opts: UpdateCheckOptions) -> Arc<Self> {
        let path = state_path(paths);
        let state = load_state(&path);
        Arc::new(Self { config, path, opts, state: Mutex::new(state) })
    }

    fn url(&self) -> Option<&str> {
        match &self.opts.endpoint {
            Endpoint::Releases => Some(RELEASES_URL),
            Endpoint::Custom(u) => Some(u),
            Endpoint::Off => None,
        }
    }

    /// 现在为什么没在检查；`None` = 在检查。
    pub fn off_reason(&self) -> Option<Off> {
        decide_now(&self.config.get(), &self.opts.endpoint).err()
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `node.info.update_available`：检查开着、最近一次成功的检查得到的版本比自己新时给出。
    pub fn available(&self) -> Option<NodeUpdateAvailable> {
        if self.off_reason().is_some() {
            return None;
        }
        available_from(&self.state(), RUNNING_VERSION)
    }

    /// 做一次检查并更新状态。返回这次得到的版本。
    pub async fn check_now(&self) -> Result<Version, CheckError> {
        let url = self.url().ok_or(CheckError::BadUrl)?.to_string();
        let r = fetch_latest(&url, self.opts.timeout).await;
        let now = now_ms();
        let (prev, snapshot) = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let prev = s.latest_version();
            s.last_attempt_ms = Some(now);
            match &r {
                Ok(v) => {
                    s.last_success_ms = Some(now);
                    s.latest = Some(v.to_string());
                    s.failures = 0;
                }
                Err(_) => s.failures = s.failures.saturating_add(1),
            }
            (prev, s.clone())
        };
        save_state(&self.path, &snapshot);
        match &r {
            Ok(v) => {
                if is_newer(v, RUNNING_VERSION) && prev != Some(*v) {
                    tracing::info!(
                        latest = %v,
                        running = RUNNING_VERSION,
                        url = %release_url(v),
                        "a newer anycon release is available"
                    );
                } else {
                    tracing::debug!(latest = %v, "update check done");
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, failures = snapshot.failures, "update check failed");
                if snapshot.failures == WARN_AFTER_FAILURES {
                    tracing::warn!(
                        error = %e,
                        "the update check has failed {WARN_AFTER_FAILURES} times in a row; it keeps retrying at most once a day. \
                         Set [update] check = false in config.toml to stop it"
                    );
                }
            }
        }
        r
    }

    /// 后台任务：按 [`next_due`] 的节奏检查，直到 `shutdown` 变成 `true`。
    pub fn spawn(self: &Arc<Self>, mut shutdown: watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let first = me
                .opts
                .first_delay
                .map(|d| d.as_millis() as u64)
                .unwrap_or_else(|| rand::thread_rng().gen_range(FIRST_DELAY_MS));
            let mut due = next_due(&me.state(), now_ms(), now_ms() + first, jitter());
            loop {
                let now = now_ms();
                if now < due {
                    let wait = Duration::from_millis(due - now).min(MAX_SLEEP);
                    tokio::select! {
                        _ = tokio::time::sleep(wait) => {}
                        _ = shutdown.changed() => {}
                    }
                    if *shutdown.borrow() {
                        return;
                    }
                    continue;
                }
                // 关着就等下一轮再看（热加载：重新打开之后最多一轮的延迟）
                if me.off_reason().is_some() {
                    due = now_ms() + FIRST_DELAY_MS.end();
                    continue;
                }
                let _ = me.check_now().await;
                due = next_due(&me.state(), now_ms(), now_ms(), jitter());
            }
        })
    }
}

fn jitter() -> u64 {
    rand::thread_rng().gen_range(0..=INTERVAL_JITTER_MS)
}

fn available_from(s: &State, running: &str) -> Option<NodeUpdateAvailable> {
    let v = s.latest_version()?;
    let at = s.last_success_ms?;
    is_newer(&v, running).then(|| NodeUpdateAvailable {
        latest: v.to_string(),
        url: release_url(&v),
        checked_at_ms: at,
    })
}

/// `anycon doctor` 的 `update` 一项。总是 `ok`（它是提示，不是故障）。daemon 没在跑时（`checker = None`）读状态文件，
/// 按 `anycon daemon` 的设置（[`Endpoint::Releases`]）判断开关。
pub fn doctor_check(paths: &Paths, cfg: &Config, checker: Option<&UpdateChecker>) -> DoctorCheck {
    let (off, state) = match checker {
        Some(c) => (c.off_reason(), c.state()),
        None => (decide_now(cfg, &Endpoint::Releases).err(), load_state(&state_path(paths))),
    };
    let detail = doctor_detail(off, &state, RUNNING_VERSION, now_ms());
    DoctorCheck { id: "update".into(), ok: true, detail, fix: None }
}

fn doctor_detail(off: Option<Off>, s: &State, running: &str, now: u64) -> String {
    if let Some(off) = off {
        return format!("update check {}", off.describe());
    }
    let ago = |t: u64| {
        let h = now.saturating_sub(t) / 3_600_000;
        if h == 0 {
            "less than an hour ago".to_string()
        } else {
            format!("{h}h ago")
        }
    };
    let failing = if s.failures > 0 { format!("; the last {} attempt(s) failed", s.failures) } else { String::new() };
    match (available_from(s, running), s.last_success_ms) {
        (Some(u), _) => format!(
            "anycon {} is available (this is {running}): {} (checked {}{failing})",
            u.latest,
            u.url,
            ago(u.checked_at_ms)
        ),
        (None, Some(t)) => format!("up to date (checked anycon.ai {}{failing})", ago(t)),
        (None, None) => format!("on; no successful check yet (anycon.ai is asked about once a day){failing}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn v(s: &str) -> Version {
        Version::parse_strict(s).unwrap()
    }

    #[test]
    fn strict_versions() {
        assert_eq!(v("0.3.1"), Version { major: 0, minor: 3, patch: 1 });
        assert_eq!(v("10.0.20").to_string(), "10.0.20");
        for bad in [
            "",
            "v0.3.1",
            "0.3",
            "0.3.1.2",
            "0.3.1-rc.1",
            "0.3.1+b",
            "01.2.3",
            "1.02.3",
            " 1.2.3",
            "1.2.3\n",
            "1.2.x",
            "1..3",
            "1234567890.0.0",
            "1.2.3\u{1b}[31m",
            "１.2.3",
        ] {
            assert!(Version::parse_strict(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn newer_than_running() {
        assert!(is_newer(&v("0.4.0"), "0.3.1"));
        assert!(is_newer(&v("0.3.2"), "0.3.1"));
        assert!(is_newer(&v("1.0.0"), "0.99.99"));
        assert!(!is_newer(&v("0.3.1"), "0.3.1"));
        assert!(!is_newer(&v("0.3.0"), "0.3.1"));
        // 预发布：同一个 X.Y.Z 的正式版更新；更旧的正式版不是
        assert!(is_newer(&v("0.4.0"), "0.4.0-rc.1"));
        assert!(!is_newer(&v("0.3.9"), "0.4.0-rc.1"));
        // 构建元数据不算预发布
        assert!(!is_newer(&v("0.4.0"), "0.4.0+abc"));
        assert!(!is_newer(&v("9.9.9"), "garbage"));
    }

    #[test]
    fn user_agent_matches_the_stats_parser() {
        // 和 anycon.ai 统计脚本里的正则一致：
        // ^anycon/[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]*)? \([A-Za-z0-9_.-]+; [A-Za-z0-9_.-]+\)$
        let ua = user_agent();
        let rest = ua.strip_prefix("anycon/").unwrap();
        let (ver, plat) = rest.split_once(" (").unwrap();
        let core = ver.split(['-', '+']).next().unwrap();
        let nums: Vec<&str> = core.split('.').collect();
        assert_eq!(nums.len(), 3, "{ua}");
        assert!(nums.iter().all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())), "{ua}");
        let suffix = &ver[core.len()..];
        assert!(suffix.chars().all(|c| c.is_ascii_alphanumeric() || ".+-".contains(c)), "{ua}");
        let plat = plat.strip_suffix(')').unwrap();
        let (os, arch) = plat.split_once("; ").unwrap();
        let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
        assert!(word(os) && word(arch), "{ua}");
        assert!(os.len() <= 20 && arch.len() <= 20 && ver.len() <= 40, "{ua}");
        assert_eq!(
            ua,
            format!("anycon/{} ({}; {})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH)
        );
    }

    #[test]
    fn switches() {
        assert_eq!(parse_env_switch(None), None);
        assert_eq!(parse_env_switch(Some("")), None);
        assert_eq!(parse_env_switch(Some("0")), Some(false));
        assert_eq!(parse_env_switch(Some("FALSE")), Some(false));
        assert_eq!(parse_env_switch(Some("off")), Some(false));
        assert_eq!(parse_env_switch(Some("1")), Some(true));
        assert_eq!(parse_env_switch(Some("yes")), Some(true));
        assert_eq!(parse_env_switch(Some("maybe")), None);

        let rel = Endpoint::Releases;
        let custom = Endpoint::Custom("http://127.0.0.1:1/x".into());
        // 正式构建：缺省开
        assert_eq!(decide(true, None, &rel, false), Ok(()));
        assert_eq!(decide(false, None, &rel, false), Err(Off::Config));
        assert_eq!(decide(true, Some(false), &rel, false), Err(Off::Env));
        // 环境变量盖过文件
        assert_eq!(decide(false, Some(true), &rel, false), Ok(()));
        // 开发构建：缺省不联系 anycon.ai，ANYCON_UPDATE_CHECK=1 才检查；测试指定的地址不受这条限制
        assert_eq!(decide(true, None, &rel, true), Err(Off::DevBuild));
        assert_eq!(decide(true, Some(true), &rel, true), Ok(()));
        assert_eq!(decide(true, None, &custom, true), Ok(()));
        assert_eq!(decide(false, None, &custom, true), Err(Off::Config));
        // 嵌入方关掉的，什么都打不开
        assert_eq!(decide(true, Some(true), &Endpoint::Off, false), Err(Off::Embedder));
    }

    #[test]
    fn targets() {
        let t = parse_target(RELEASES_URL).unwrap();
        assert_eq!(
            t,
            Target {
                tls: true,
                host: "anycon.ai".into(),
                port: 443,
                authority: "anycon.ai".into(),
                path: "/releases.json".into()
            }
        );
        assert_eq!(parse_target("http://127.0.0.1:8080/r.json").unwrap().port, 8080);
        assert_eq!(parse_target("http://[::1]:9/r.json").unwrap().host, "::1");
        assert!(parse_target("http://localhost:9/r.json").is_ok());
        for bad in [
            "http://anycon.ai/releases.json",
            "http://10.0.0.1/releases.json",
            "ftp://anycon.ai/releases.json",
            "https://anycon.ai/releases.json?id=1",
            "https://anycon.ai/releases.json#x",
            "https://user@anycon.ai/releases.json",
            "https:///releases.json",
            "anycon.ai/releases.json",
        ] {
            assert!(parse_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn responses() {
        let ok = br#"{"latest":"0.4.0","url":"https://github.com/anycon-ai/anycon/releases/tag/v0.4.0","published_at":"2026-11-20T10:00:00Z","extra":{"x":1}}"#;
        assert_eq!(parse_response(ok).unwrap(), v("0.4.0"));
        // 回应里的链接不用：给用户看的链接只由版本号拼出来
        let evil = br#"{"latest":"0.4.0","url":"https://evil.example/\u001b]8;;x"}"#;
        assert_eq!(
            release_url(&parse_response(evil).unwrap()),
            "https://github.com/anycon-ai/anycon/releases/tag/v0.4.0"
        );
        assert!(matches!(parse_response(br#"{"latest":"v0.4.0"}"#), Err(CheckError::BadVersion)));
        assert!(matches!(parse_response(br#"{"latest":"0.4.0\u001b[2J"}"#), Err(CheckError::BadVersion)));
        assert!(matches!(parse_response(br#"{"latest":4}"#), Err(CheckError::Json)));
        assert!(matches!(parse_response(b"<html>"), Err(CheckError::Json)));
        // 错误信息里没有回应的内容
        let e = parse_response(br#"{"latest":"SECRET-TEXT"}"#).unwrap_err().to_string();
        assert!(!e.contains("SECRET"), "{e}");
        let e = parse_response(br#"{"nope":"SECRET-TEXT"}"#).unwrap_err().to_string();
        assert!(!e.contains("SECRET"), "{e}");
    }

    #[test]
    fn schedule() {
        let day = INTERVAL.as_millis() as u64;
        let now = 1_000 * day;
        let fresh = State::default();
        // 没有记录：daemon 起来几分钟之后
        assert_eq!(next_due(&fresh, now, now + 300_000, 0), now + 300_000);
        // 上次成功是 1 小时前：重启不提前，24 小时加抖动之后
        let s = State {
            last_success_ms: Some(now - 3_600_000),
            latest: Some("0.3.1".into()),
            last_attempt_ms: Some(now - 3_600_000),
            failures: 0,
        };
        assert_eq!(next_due(&s, now, now + 300_000, 42), now - 3_600_000 + day + 42);
        // 上次成功是两天前：照常在启动几分钟之后
        let old = State { last_success_ms: Some(now - 2 * day), ..s.clone() };
        assert_eq!(next_due(&old, now, now + 300_000, 0), now + 300_000);
        // 失败退避：2h、4h、8h……最多 24h
        assert_eq!(backoff_ms(1), 2 * 3_600_000);
        assert_eq!(backoff_ms(2), 4 * 3_600_000);
        assert_eq!(backoff_ms(4), 16 * 3_600_000);
        assert_eq!(backoff_ms(5), day);
        assert_eq!(backoff_ms(500), day);
        let failing = State { last_success_ms: None, latest: None, last_attempt_ms: Some(now), failures: 1 };
        assert_eq!(next_due(&failing, now, now, 0), now + 2 * 3_600_000);
        // 时钟被往回调：记录在未来，按现在算
        let future = State { last_success_ms: Some(now + 365 * day), ..s };
        assert_eq!(next_due(&future, now, now, 0), now + day);
    }

    #[test]
    fn availability_and_doctor_text() {
        let now = 10 * INTERVAL.as_millis() as u64;
        let s = State {
            last_success_ms: Some(now - 2 * 3_600_000),
            latest: Some("0.4.0".into()),
            last_attempt_ms: Some(now - 2 * 3_600_000),
            failures: 0,
        };
        let a = available_from(&s, "0.3.1").unwrap();
        assert_eq!(a.latest, "0.4.0");
        assert_eq!(a.url, "https://github.com/anycon-ai/anycon/releases/tag/v0.4.0");
        assert_eq!(a.checked_at_ms, now - 2 * 3_600_000);
        assert!(available_from(&s, "0.4.0").is_none());
        assert!(available_from(&s, "0.5.0").is_none());
        let d = doctor_detail(None, &s, "0.3.1", now);
        assert!(
            d.contains("anycon 0.4.0 is available") && d.contains("releases/tag/v0.4.0") && d.contains("2h ago"),
            "{d}"
        );
        assert!(doctor_detail(None, &s, "0.4.0", now).starts_with("up to date"));
        assert!(doctor_detail(None, &State::default(), "0.4.0", now).contains("no successful check yet"));
        assert!(doctor_detail(Some(Off::Config), &s, "0.3.1", now).contains("check = false"));
        assert!(doctor_detail(Some(Off::DevBuild), &s, "0.3.1", now).contains("ANYCON_UPDATE_CHECK=1"));
    }

    #[test]
    fn state_file_is_validated_on_load() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(STATE_FILE);
        assert_eq!(load_state(&p), State::default());
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(load_state(&p), State::default());
        std::fs::write(&p, r#"{"last_success_ms":5,"latest":"0.4.0\u001b[2J","failures":2,"future_field":1}"#).unwrap();
        let s = load_state(&p);
        assert_eq!((s.latest, s.last_success_ms, s.failures), (None, None, 2));
        let good =
            State { last_success_ms: Some(7), latest: Some("1.2.3".into()), last_attempt_ms: Some(7), failures: 0 };
        save_state(&p, &good);
        assert_eq!(load_state(&p), good);
    }

    /// 本机回环地址上的一次性 HTTP 服务器：记下收到的请求头，回 `response`。
    async fn serve_once(response: Vec<u8>, delay: Duration) -> (String, tokio::task::JoinHandle<String>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases.json", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            tokio::time::sleep(delay).await;
            let _ = s.write_all(&response).await;
            let _ = s.shutdown().await;
            String::from_utf8_lossy(&buf).into_owned()
        });
        (url, h)
    }

    fn http_response(status: &str, extra_headers: &str, body: &[u8]) -> Vec<u8> {
        let mut r = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra_headers}\r\n",
            body.len()
        )
        .into_bytes();
        r.extend_from_slice(body);
        r
    }

    #[tokio::test]
    async fn request_carries_only_host_and_user_agent() {
        let body = br#"{"latest":"9.0.0","url":"https://github.com/anycon-ai/anycon/releases/tag/v9.0.0","published_at":"2026-11-20T10:00:00Z"}"#;
        let (url, server) = serve_once(http_response("200 OK", "", body), Duration::ZERO).await;
        assert_eq!(fetch_latest(&url, Duration::from_secs(5)).await.unwrap(), v("9.0.0"));
        let req = server.await.unwrap();
        let mut lines = req.split("\r\n").filter(|l| !l.is_empty());
        assert_eq!(lines.next(), Some("GET /releases.json HTTP/1.1"));
        let mut names: Vec<String> = Vec::new();
        for l in lines {
            let (k, val) = l.split_once(": ").unwrap();
            names.push(k.to_ascii_lowercase());
            if k.eq_ignore_ascii_case("user-agent") {
                assert_eq!(val, user_agent());
            }
        }
        names.sort();
        assert_eq!(names, vec!["host", "user-agent"], "{req}");
    }

    #[tokio::test]
    async fn redirects_errors_big_bodies_and_timeouts_fail() {
        let (url, _s) = serve_once(
            http_response("302 Found", "location: https://evil.example/releases.json\r\n", b""),
            Duration::ZERO,
        )
        .await;
        assert!(matches!(fetch_latest(&url, Duration::from_secs(5)).await, Err(CheckError::Status(302))));
        let (url, _s) = serve_once(http_response("500 Internal Server Error", "", b"oops"), Duration::ZERO).await;
        assert!(matches!(fetch_latest(&url, Duration::from_secs(5)).await, Err(CheckError::Status(500))));
        let big = format!(r#"{{"latest":"9.0.0","pad":"{}"}}"#, "x".repeat(MAX_BODY_BYTES));
        let (url, _s) = serve_once(http_response("200 OK", "", big.as_bytes()), Duration::ZERO).await;
        assert!(matches!(fetch_latest(&url, Duration::from_secs(5)).await, Err(CheckError::Body)));
        let (url, _s) = serve_once(http_response("200 OK", "", b"{}"), Duration::from_secs(5)).await;
        assert!(matches!(fetch_latest(&url, Duration::from_millis(200)).await, Err(CheckError::Timeout)));
        // 本机回环之外的 http:// 不发请求
        assert!(matches!(
            fetch_latest("http://anycon.ai/releases.json", Duration::from_secs(5)).await,
            Err(CheckError::BadUrl)
        ));
    }

    fn store_with(home: &Path, check: bool) -> Arc<ConfigStore> {
        let mut cfg = Config::default();
        cfg.update.check = check;
        ConfigStore::with_config(Paths::new(home), cfg)
    }

    #[tokio::test]
    async fn checker_persists_and_does_not_recheck_after_restart() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        let body = br#"{"latest":"99.0.0"}"#;
        let (url, server) = serve_once(http_response("200 OK", "", body), Duration::ZERO).await;
        let opts = UpdateCheckOptions {
            endpoint: Endpoint::Custom(url),
            first_delay: Some(Duration::ZERO),
            timeout: Duration::from_secs(5),
        };
        let c = UpdateChecker::new(&paths, store_with(d.path(), true), opts.clone());
        assert!(c.available().is_none());
        let (tx, rx) = watch::channel(false);
        let task = c.spawn(rx);
        server.await.unwrap();
        let t = std::time::Instant::now();
        while c.available().is_none() {
            assert!(t.elapsed() < Duration::from_secs(5), "no result");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let a = c.available().unwrap();
        assert_eq!(a.latest, "99.0.0");
        assert_eq!(a.url, "https://github.com/anycon-ai/anycon/releases/tag/v99.0.0");
        let _ = tx.send(true);
        let _ = task.await;
        // 重启：从状态文件读回结果；下一次检查在 24 小时之后
        let c2 = UpdateChecker::new(&paths, store_with(d.path(), true), opts.clone());
        assert_eq!(c2.available(), Some(a));
        let s = c2.state();
        assert!(next_due(&s, now_ms(), now_ms(), 0) >= s.last_success_ms.unwrap() + INTERVAL.as_millis() as u64);
        // 关掉之后不再给出结果
        let c3 = UpdateChecker::new(&paths, store_with(d.path(), false), opts);
        assert!(c3.available().is_none());
        assert_eq!(c3.off_reason(), Some(Off::Config));
    }

    #[tokio::test]
    async fn disabled_checker_sends_nothing() {
        let d = tempfile::tempdir().unwrap();
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases.json", l.local_addr().unwrap());
        let opts = UpdateCheckOptions {
            endpoint: Endpoint::Custom(url),
            first_delay: Some(Duration::ZERO),
            ..Default::default()
        };
        let c = UpdateChecker::new(&Paths::new(d.path()), store_with(d.path(), false), opts);
        let (tx, rx) = watch::channel(false);
        let task = c.spawn(rx);
        let accepted = tokio::time::timeout(Duration::from_millis(500), l.accept()).await;
        assert!(accepted.is_err(), "a disabled checker must not connect");
        let _ = tx.send(true);
        let _ = task.await;
        assert!(!d.path().join(STATE_FILE).exists());
    }

    #[tokio::test]
    async fn failures_are_counted_and_backed_off() {
        let d = tempfile::tempdir().unwrap();
        let paths = Paths::new(d.path());
        let (url, _s) = serve_once(http_response("503 Service Unavailable", "", b""), Duration::ZERO).await;
        let opts =
            UpdateCheckOptions { endpoint: Endpoint::Custom(url), first_delay: None, timeout: Duration::from_secs(5) };
        let c = UpdateChecker::new(&paths, store_with(d.path(), true), opts);
        assert!(matches!(c.check_now().await, Err(CheckError::Status(503))));
        let s = c.state();
        assert_eq!((s.failures, s.last_success_ms.is_none()), (1, true));
        assert!(next_due(&s, now_ms(), now_ms(), 0) >= s.last_attempt_ms.unwrap() + BACKOFF_MIN_MS);
        assert_eq!(load_state(&state_path(&paths)), s);
    }
}
