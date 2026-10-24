//! 配置：`ANYCON_*` 环境变量（docs/deploy.md § 3），用 figment 装载。
//!
//! 环境变量的值一律按**原始字符串**取（自定义 provider），不让 figment 去猜类型——
//! 否则全数字的 admin token 会被当成整数、前导零丢失。类型转换和校验都在 [`Config::from_vars`]。
//!
//! 测试直接构造 [`Config`]（[`Config::new`] + 字段覆盖），不经过环境变量。

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anycon_protocol::envelope::frame;
use figment::value::{Dict, Map, Value};
use figment::{Figment, Metadata, Profile, Provider};
use ipnet::IpNet;
use serde::Deserialize;
use url::Url;

use crate::mail::Mailer;

/// 运行模式（`ANYCON_MODE`）。
///
/// - `Single`：一个管理员 + 邀请制，登录靠 bootstrap token / 登录 token / 已绑定邮箱的验证码 / 邀请；没有配额。
/// - `Multi`：开放注册（第一次邮箱验证码或 OAuth 登录就建账号），没有 bootstrap 账号、token 登录和邀请；
///   有配额，管理员由 `ANYCON_ADMIN_EMAILS` 和库里的标记决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Single,
    Multi,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Single => "single",
            Mode::Multi => "multi",
        }
    }

    pub fn is_multi(self) -> bool {
        self == Mode::Multi
    }
}

/// `multi` 模式下每个账号的数量类配额上限（`ANYCON_QUOTA_*`，每项 ≥ 1）。
///
/// 每日配对次数（`ANYCON_QUOTA_DAILY_PAIRINGS`）不在这里：它只有一份，是
/// [`Limits::daily_pairings_per_account`]（限速器、`GET /api/quota` 都读它；`single` 程序化设置它也照样生效）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimits {
    /// 未吊销的设备数（所有种类，含 daemon）。
    pub devices: u32,
    /// 未吊销的 daemon 数。
    pub daemons: u32,
    /// 同时在线的永久连接数。
    pub connections: u32,
}

impl QuotaLimits {
    pub const DEFAULT_DEVICES: u32 = 10;
    pub const DEFAULT_DAEMONS: u32 = 3;
    pub const DEFAULT_CONNECTIONS: u32 = 10;
}

/// `multi` 里每账号每日配对与接入次数的默认上限（`ANYCON_QUOTA_DAILY_PAIRINGS` 没设时）。
pub const DEFAULT_DAILY_PAIRINGS: u32 = 20;

impl Default for QuotaLimits {
    fn default() -> Self {
        Self { devices: Self::DEFAULT_DEVICES, daemons: Self::DEFAULT_DAEMONS, connections: Self::DEFAULT_CONNECTIONS }
    }
}

/// 支持的 OAuth 登录提供方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OAuthProvider {
    Github,
    Google,
}

impl OAuthProvider {
    /// 全部提供方，顺序也是 `login_methods` 里的顺序（github 在前）。
    pub const ALL: [OAuthProvider; 2] = [OAuthProvider::Github, OAuthProvider::Google];

    /// 路由、审计、`login_methods` 里用的名字。
    pub fn as_str(self) -> &'static str {
        match self {
            OAuthProvider::Github => "github",
            OAuthProvider::Google => "google",
        }
    }

    /// 精确匹配（小写）；未知返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }

    /// 生产环境用的端点（全部 https）。
    pub fn default_endpoints(self) -> OAuthEndpoints {
        match self {
            OAuthProvider::Github => OAuthEndpoints {
                authorize: "https://github.com/login/oauth/authorize".into(),
                token: "https://github.com/login/oauth/access_token".into(),
                userinfo: "https://api.github.com/user".into(),
                emails: Some("https://api.github.com/user/emails".into()),
            },
            OAuthProvider::Google => OAuthEndpoints {
                authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: "https://oauth2.googleapis.com/token".into(),
                userinfo: "https://openidconnect.googleapis.com/v1/userinfo".into(),
                emails: None,
            },
        }
    }
}

/// 一个 OAuth 提供方的应用凭据（`ANYCON_OAUTH_<P>_CLIENT_ID` / `_CLIENT_SECRET`，两个都有才算配了）。
/// `Debug` 不输出 secret。
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: String,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient").field("client_id", &self.client_id).field("client_secret", &"<redacted>").finish()
    }
}

/// 一个 OAuth 提供方的四个端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    pub authorize: String,
    pub token: String,
    pub userinfo: String,
    /// 单独的邮箱列表端点（GitHub 的 `/user/emails`）；Google 的 userinfo 里直接带邮箱，为 `None`。
    pub emails: Option<String>,
}

/// **只给测试用的**端点覆盖（没有环境变量）：让集成测试把 OAuth 流程指向 `http://127.0.0.1:<端口>` 上的假提供方。
/// 生产里两项都是 `None`，用 [`OAuthProvider::default_endpoints`]。
#[derive(Debug, Clone, Default)]
pub struct OAuthEndpointOverrides {
    pub github: Option<OAuthEndpoints>,
    pub google: Option<OAuthEndpoints>,
}

/// 默认的客户端连接入站预算：10 MiB / 分钟。
pub const DEFAULT_CLIENT_BYTES_PER_MIN: u64 = 10 * 1024 * 1024;
/// 默认的 daemon 连接入站预算：100 MiB / 分钟（它是所有客户端的汇聚点）。
pub const DEFAULT_DAEMON_BYTES_PER_MIN: u64 = 100 * 1024 * 1024;

/// 默认的账号预览流量预算：512 MiB / 分钟（`ANYCON_PREVIEW_BYTES_PER_MIN`）。
pub const DEFAULT_PREVIEW_BYTES_PER_MIN: u64 = 512 * 1024 * 1024;
/// 默认的每账号预览连接数上限（`ANYCON_PREVIEW_MAX_CONNECTIONS`）。
pub const DEFAULT_PREVIEW_MAX_CONNECTIONS: u32 = 16;

/// 按 IP / 按连接的限速参数。默认值见 docs/spec/hub.md § 3.4、§ 7.2。
#[derive(Debug, Clone)]
pub struct Limits {
    pub auth_token_per_min: u32,
    pub ws_upgrade_per_min: u32,
    pub pair_join_per_min: u32,
    pub invite_per_min: u32,
    /// 每条连接每秒最多处理的信封数。
    pub conn_envelopes_per_sec: u32,
    /// 客户端连接（web / cli / sdk，以及还没有 device token 的临时连接）每分钟的入站字节预算
    /// （`ANYCON_RATE_CLIENT_BYTES_PER_MIN`）。超过的 `data` / `notify` 不丢，退回 `backpressure`。
    pub client_bytes_per_min: u64,
    /// daemon 连接每分钟的入站字节预算（`ANYCON_RATE_DAEMON_BYTES_PER_MIN`）。
    pub daemon_bytes_per_min: u64,
    /// 没有 device token 的临时连接一生最多发几次 `PairBegin`（docs/spec/hub.md § 4）。
    pub temp_pair_begins_per_conn: u32,
    /// `enroll_begin`：每个真实 IP 每分钟（docs/spec/hub.md § 7.2）。
    pub enroll_begin_per_min_per_ip: u32,
    /// `enroll_begin`：每个账号每小时。
    pub enroll_begin_per_hour_per_account: u32,
    /// 每账号每日（滚动 24 小时）配对与接入次数：`pair_begin`、`enroll_begin` 各记一次，`pair_join` 只检查不记
    /// （超过 `pair_failed { quota }` / `enroll_failed { quota }`）。**这是唯一的一份**：限速器从它建，
    /// `GET /api/quota` 的 `daily_pairings_max` 读它。`None` = 不限（`single` 默认）；`multi` 由
    /// `ANYCON_QUOTA_DAILY_PAIRINGS` 经 [`Config::enable_multi`] 设置，`single` 程序化设置它也照样生效。
    pub daily_pairings_per_account: Option<u32>,
    /// 每台 daemon 同时进行的接入会话上限，多出来的 `busy`。
    pub enrolls_per_daemon: u32,
    /// `POST /auth/email/send`、`POST /api/me/email`：每个真实 IP 每小时（各自一个桶，docs/spec/hub.md § 7.2）。
    pub email_send_per_hour_per_ip: u32,
    /// 同一个邮箱（同一用途）两次发码之间至少隔多久。
    pub email_resend_interval: Duration,
    /// `POST /auth/email/verify`、`POST /api/me/email/verify`：每个真实 IP 每分钟（各自一个桶）。
    pub email_verify_per_min_per_ip: u32,
    /// 一个验证码最多被尝试几次（每次验证提交对每条有效的码各计一次），用完作废。
    pub email_code_max_attempts: u32,
    /// 同一组（邮箱，用途，账号）里最多同时有几条有效的码：新码不让旧码作废，超出时顶掉最旧的
    /// （别人请求新码顶不掉你手里那个，docs/spec/hub.md § 8）。
    pub email_codes_active: u32,
    /// 每个邮箱每天最多发几个码，登录和绑定分开计，不管请求来自哪个 IP（docs/spec/hub.md § 7.2）。
    pub email_codes_per_day: u32,
    /// 每个邮箱每天累计验证失败几次之后，当天不再接受这个邮箱的码（登录和绑定分开计，不管请求来自哪个 IP）。
    pub email_failures_per_day: u32,
    /// `POST /api/push/subscriptions`、`POST /api/push/test`：每个账号每分钟。
    pub push_api_per_min_per_account: u32,
    /// 每台收件设备每分钟最多触发几条推送 / 邮件通知判定（防止配对设备狂发 `notify` 把推送服务和邮件刷爆）。
    pub notify_per_min_per_device: u32,
    /// 每个账号预览流量（预览连接 ↔ 节点，两个方向合计）每分钟的字节预算（`ANYCON_PREVIEW_BYTES_PER_MIN`，
    /// docs/spec/hub.md § 13.5）。
    pub preview_bytes_per_min: u64,
    /// 每个账号同时在线的预览连接数上限（`ANYCON_PREVIEW_MAX_CONNECTIONS`）。
    pub preview_max_connections: u32,
    /// 预览域名上的 WebSocket 升级：每个真实 IP 每分钟（service worker 被浏览器回收后会重连，比主域名宽）。
    pub preview_ws_upgrade_per_min: u32,
    /// `POST /api/preview/tokens`：每台设备每分钟。
    pub preview_tokens_per_min_per_device: u32,
    /// 同一台设备未过期的预览令牌上限，超过时删掉最旧的。
    pub preview_tokens_per_device: u32,
    /// 一个节点最多绑定几个预览 label。
    pub preview_labels_per_node: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_token_per_min: 10,
            ws_upgrade_per_min: 30,
            pair_join_per_min: 10,
            invite_per_min: 10,
            conn_envelopes_per_sec: 500,
            client_bytes_per_min: DEFAULT_CLIENT_BYTES_PER_MIN,
            daemon_bytes_per_min: DEFAULT_DAEMON_BYTES_PER_MIN,
            temp_pair_begins_per_conn: 3,
            enroll_begin_per_min_per_ip: 10,
            enroll_begin_per_hour_per_account: 30,
            daily_pairings_per_account: None,
            enrolls_per_daemon: 3,
            email_send_per_hour_per_ip: 5,
            email_resend_interval: Duration::from_secs(60),
            email_verify_per_min_per_ip: 20,
            email_code_max_attempts: 5,
            email_codes_active: 3,
            email_codes_per_day: 10,
            email_failures_per_day: 30,
            push_api_per_min_per_account: 30,
            notify_per_min_per_device: 30,
            preview_bytes_per_min: DEFAULT_PREVIEW_BYTES_PER_MIN,
            preview_max_connections: DEFAULT_PREVIEW_MAX_CONNECTIONS,
            preview_ws_upgrade_per_min: 120,
            preview_tokens_per_min_per_device: 60,
            preview_tokens_per_device: 64,
            preview_labels_per_node: 256,
        }
    }
}

/// 各种超时。不对应环境变量，只给测试缩短用。
#[derive(Debug, Clone)]
pub struct Timing {
    /// WS 升级后第一帧（auth）的期限。
    pub auth_timeout: Duration,
    pub ping_interval: Duration,
    /// 超过这么久没收到任何入站帧（含 pong）就断开。
    pub pong_timeout: Duration,
    /// `PairBegin` 登记的有效期。
    pub pair_ttl: Duration,
    /// matched 之后，这么久没有任何动静（握手帧 / confirm）就作废。
    pub pair_confirm_timeout: Duration,
    /// 账号内接入会话的有效期：从 `enroll_begin` 起算，到点没走到 `pair_done` 就作废（`enroll_failed { timeout }`）。
    pub enroll_ttl: Duration,
    /// 发送缓冲持续满这么久就以 4003 断开。
    pub slow_consumer_timeout: Duration,
    /// 优雅关停时等待连接收尾的上限。
    pub shutdown_grace: Duration,
    /// 浏览器登录会话有效期（滑动续期）。
    pub session_ttl: Duration,
    /// 没有 device token 的临时连接最长存活时间，到点以 4005 关闭（配对用的连接不需要挂这么久）。
    pub temp_conn_ttl: Duration,
    /// 发送方在 hub 明确叫它暂停（`rate_limited`）之后仍然持续发送，超过这么久就以 1008 断开。
    pub rate_violation_close_after: Duration,
    /// 邮箱验证码的有效期。
    pub email_code_ttl: Duration,
    /// 邮箱验证码**验证失败**的响应，至少在请求开始之后这么久才发出（成功不等）：把「这个邮箱有没有待验证的码」
    /// 这点库操作的耗时差埋在固定的延迟里（docs/spec/hub.md § 2.2）。
    pub email_verify_failure_delay: Duration,
    /// 同一账号同一类别的邮件通知最短间隔（docs/spec/hub.md § 6.2）。
    pub notify_email_dedupe: Duration,
    /// 单个 Web Push 请求的超时（docs/spec/hub.md § 6.1：10 秒）。
    pub push_timeout: Duration,
    /// OAuth 登录时每次出站调用（换 token、取用户信息、取 GitHub 邮箱）的超时（10 秒）。
    pub oauth_timeout: Duration,
    /// 预览令牌的有效期（docs/spec/hub.md § 13.3：24 小时）。
    pub preview_token_ttl: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            auth_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(30),
            pong_timeout: Duration::from_secs(90),
            pair_ttl: Duration::from_secs(5 * 60),
            pair_confirm_timeout: Duration::from_secs(60),
            enroll_ttl: Duration::from_secs(5 * 60),
            slow_consumer_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            session_ttl: Duration::from_secs(30 * 24 * 3600),
            temp_conn_ttl: Duration::from_secs(10 * 60),
            rate_violation_close_after: Duration::from_secs(10),
            email_code_ttl: Duration::from_secs(10 * 60),
            email_verify_failure_delay: Duration::from_millis(250),
            notify_email_dedupe: Duration::from_secs(10 * 60),
            push_timeout: Duration::from_secs(10),
            oauth_timeout: Duration::from_secs(10),
            preview_token_ttl: Duration::from_secs(24 * 3600),
        }
    }
}

/// SMTP 连接方式（`ANYCON_SMTP_URL` 的 scheme）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// `smtp://`：明文连上之后升级到 STARTTLS，**必须成功**（服务器不支持就发不出去，不会退回明文）。
    StartTls,
    /// `smtps://`：隐式 TLS（连上就是 TLS，通常 465）。
    Tls,
    /// `smtp+plain://`：不加密。只发往本机 / 内网（本机中继、Mailpit、测试）：IP 字面量启动时检查，主机名每次发信前解析、
    /// 所有地址都必须是本机 / 内网（[`check_plain_host`]）。
    Plain,
}

/// 解析后的 `ANYCON_SMTP_URL`。`Debug` 不输出用户名和密码。
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub security: SmtpSecurity,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl std::fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpConfig")
            .field("security", &self.security)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// 解析后的 `ANYCON_EMAIL_FROM`。
#[derive(Debug, Clone)]
pub struct EmailFrom {
    pub mailbox: lettre::message::Mailbox,
    /// 只有地址部分（`noreply@example.com`），VAPID 的 `mailto:` 兜底用它。
    pub address: String,
}

/// hub 的完整运行配置。`Debug` 输出里 `admin_token`、SMTP 的用户名密码、OAuth 的 client secret 会被遮掉，
/// 管理员邮箱只输出个数，免得误打日志时泄露。
#[derive(Clone)]
pub struct Config {
    /// `ANYCON_PUBLIC_URL`，不带末尾 `/`。
    pub public_url: Url,
    pub listen: SocketAddr,
    pub mode: Mode,
    /// `ANYCON_ADMIN_TOKEN`。只在 `single` 用；`multi` 不生成也不读 bootstrap token，设了会被忽略（启动警告）。
    pub admin_token: Option<String>,
    /// `ANYCON_ADMIN_EMAILS`：规范化（[`crate::mail::normalize_email`]）、去重后的邮箱。只在 `multi` 起作用
    /// （见 [`crate::accounts::admin_by_config`]）。`Debug` 和日志里只给个数。
    pub admin_emails: Vec<String>,
    /// `multi` 的数量类配额上限；`single` 为 `None`（没有配额）。每日配对次数不在这里，见
    /// [`Limits::daily_pairings_per_account`]。程序化构造请用 [`Config::enable_multi`]。
    pub quota: Option<QuotaLimits>,
    /// `ANYCON_OAUTH_GITHUB_CLIENT_ID` / `_CLIENT_SECRET`。两种模式都可以配。
    pub oauth_github: Option<OAuthClient>,
    /// `ANYCON_OAUTH_GOOGLE_CLIENT_ID` / `_CLIENT_SECRET`。
    pub oauth_google: Option<OAuthClient>,
    /// **只给测试用，不对应任何环境变量**：把 OAuth 提供方的端点指到本机的假提供方。生产里全是 `None`。
    pub oauth_endpoint_overrides: OAuthEndpointOverrides,
    pub data_dir: PathBuf,
    /// `None` = `sqlite://<data_dir>/hub.db`。
    pub database_url: Option<String>,
    pub trusted_proxies: Vec<IpNet>,
    /// tracing 过滤表达式（`ANYCON_LOG`）。
    pub log: String,
    pub max_envelope_bytes: usize,
    /// 每条连接的发送缓冲上限（字节）。
    pub outbound_buffer_bytes: usize,
    pub limits: Limits,
    pub timing: Timing,
    /// 已识别但没实现的 `ANYCON_*` 变量名（启动时警告）。
    pub unimplemented_vars: Vec<String>,
    /// 设了、但在当前模式下被忽略的变量名（`multi` 里的 `ANYCON_ADMIN_TOKEN`，`single` 里的
    /// `ANYCON_ADMIN_EMAILS` 和 `ANYCON_QUOTA_*`）；只有 [`Config::from_vars`] 会填。启动警告见 [`Config::startup_warnings`]。
    pub ignored_vars: Vec<String>,
    /// 完全不认识的 `ANYCON_*` 变量名（debug 级提示；daemon 的变量也会落在这里）。
    pub unknown_vars: Vec<String>,
    /// `ANYCON_SMTP_URL`（ADR-37）。配了才有邮箱验证码登录和邮件通知。
    pub smtp: Option<SmtpConfig>,
    /// `ANYCON_EMAIL_FROM`。配了 SMTP 就必填（[`Config::from_vars`] 检查）。
    pub email_from: Option<EmailFrom>,
    /// `ANYCON_VAPID_SUBJECT`；没设时见 [`Config::vapid_subject`]。
    pub vapid_subject: Option<String>,
    /// **只给测试用，不对应任何环境变量**：额外放行的推送 endpoint 主机（精确匹配主机名，允许 http 和任意端口），
    /// 让测试能把推送发到本机的假推送服务。生产里为 `None`，只认内置的推送服务白名单（docs/spec/hub.md § 6.1）。
    pub push_allow_hosts: Option<Vec<String>>,
    /// **只给测试用**：用这个发信实现代替 SMTP（`Some` 时即使没配 `smtp` 也算「邮件可用」）。
    pub mailer_override: Option<Arc<dyn Mailer>>,
    /// `ANYCON_PREVIEW_URL`（ADR-39）。`None` = 没有 dev server 预览。
    pub preview: Option<PreviewUrl>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("public_url", &self.public_url.as_str())
            .field("listen", &self.listen)
            .field("mode", &self.mode)
            .field("admin_token", &self.admin_token.as_ref().map(|_| "<redacted>"))
            .field("admin_emails", &self.admin_emails.len())
            .field("quota", &self.quota)
            .field("oauth_github", &self.oauth_github)
            .field("oauth_google", &self.oauth_google)
            .field("oauth_endpoint_overrides", &self.oauth_endpoint_overrides)
            .field("data_dir", &self.data_dir)
            .field("database_url", &self.database_url.as_ref().map(|_| "<redacted>"))
            .field("trusted_proxies", &self.trusted_proxies)
            .field("log", &self.log)
            .field("max_envelope_bytes", &self.max_envelope_bytes)
            .field("outbound_buffer_bytes", &self.outbound_buffer_bytes)
            .field("limits", &self.limits)
            .field("timing", &self.timing)
            .field("smtp", &self.smtp)
            .field("email_from", &self.email_from.as_ref().map(|f| f.address.as_str()))
            .field("vapid_subject", &self.vapid_subject)
            .field("push_allow_hosts", &self.push_allow_hosts)
            .field("mailer_override", &self.mailer_override.is_some())
            .field("preview", &self.preview)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("ANYCON_PUBLIC_URL is required (e.g. https://hub.example.com)")]
    MissingPublicUrl,
    #[error("invalid {var}: {reason}")]
    Invalid { var: &'static str, reason: String },
    #[error(
        "ANYCON_MODE=multi has no way to sign in: set ANYCON_SMTP_URL (email codes) and/or an OAuth provider \
         (ANYCON_OAUTH_GITHUB_CLIENT_ID/_SECRET, ANYCON_OAUTH_GOOGLE_CLIENT_ID/_SECRET)"
    )]
    MultiNoSignIn,
    #[error("failed to read environment: {0}")]
    Env(String),
}

fn invalid(var: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid { var, reason: reason.into() }
}

/// 分片信封里除密文以外的 `data` body 框架头：flags(1) + msg_id(4) + index(2) + count(2)。
const FRAGMENT_FRAME_HEADER: usize = 1 + 4 + 2 + 2;

/// 外层 `Envelope` 的 msgpack 头开销的上界：`v` / `from` / `to` / `kind` / `seq` 五个字段名和值，加 `body` 的 bin32 头。
/// 实际约 120 字节（32 位 hex 的设备 id，`seq` 用满 u64）；这里留够，设备 id 长到 64 个字符也放得下。
const ENVELOPE_HEADER_ALLOWANCE: usize = 512;

/// `ANYCON_MAX_ENVELOPE_BYTES` 的下限：一个满片的分片信封（`MAX_FRAGMENT` 密文 + 分片框架头 + 外层信封头），取整到 KiB，
/// 即 201 KiB。更小的话，发送方按协议切出来的每个满片都会被 `too_large` 丢掉，大消息永远传不过去。
pub const MIN_ENVELOPE_BYTES: usize =
    (frame::MAX_FRAGMENT + FRAGMENT_FRAME_HEADER + ENVELOPE_HEADER_ALLOWANCE).next_multiple_of(1024);

/// 默认的受信代理：Docker 默认桥接网段（`172.16.0.0/12`）。
///
/// 来自这些网段的直连对端发来的 `X-Forwarded-For` / `X-Forwarded-Proto` 会被相信（限速按其中的真实 IP 计，
/// cookie 是否 `Secure` 也参考它）。**不在 Docker 里部署、而客户端本身就在这个网段时，客户端能伪造这两个头**——
/// 这时请显式设置 `ANYCON_TRUSTED_PROXIES`（空串 = 谁都不信）。
pub const DEFAULT_TRUSTED_PROXIES: &str = "172.16.0.0/12";

/// deploy.md § 3 里列出、但还没实现的变量（小写、去前缀）。`quota_` 开头的另算（见 [`Config::from_vars`]）：
/// 设备数、daemon 数、连接数和每日配对数已经实现，其余的（队列配额）仍是「已识别、未实现」。
const UNIMPLEMENTED: &[&str] = &["queue_enabled", "queue_max_count", "queue_max_bytes", "queue_ttl"];

impl Config {
    /// 带默认值的配置，测试用它再覆盖字段。
    pub fn new(public_url: &str, data_dir: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        let public_url = parse_public_url(public_url)?;
        Ok(Self {
            public_url,
            listen: "0.0.0.0:8080".parse().expect("static addr"),
            mode: Mode::Single,
            admin_token: None,
            admin_emails: Vec::new(),
            quota: None,
            oauth_github: None,
            oauth_google: None,
            oauth_endpoint_overrides: OAuthEndpointOverrides::default(),
            data_dir: data_dir.into(),
            database_url: None,
            trusted_proxies: parse_trusted_proxies(DEFAULT_TRUSTED_PROXIES)?,
            log: "info".into(),
            max_envelope_bytes: 262_144,
            outbound_buffer_bytes: 4 * 1024 * 1024,
            limits: Limits::default(),
            timing: Timing::default(),
            unimplemented_vars: Vec::new(),
            ignored_vars: Vec::new(),
            unknown_vars: Vec::new(),
            smtp: None,
            email_from: None,
            vapid_subject: None,
            push_allow_hosts: None,
            mailer_override: None,
            preview: None,
        })
    }

    /// 从进程环境变量读取。
    pub fn from_env() -> Result<Self, ConfigError> {
        let vars = std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect::<Vec<_>>();
        Self::from_vars(vars)
    }

    /// 从 `(名字, 值)` 列表读取（名字带 `ANYCON_` 前缀，其余忽略）。
    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Result<Self, ConfigError> {
        let provider = RawEnv { vars: vars.into_iter().collect() };
        let raw: RawConfig = Figment::from(provider).extract().map_err(|e| ConfigError::Env(e.to_string()))?;

        let public_url =
            raw.public_url.as_deref().filter(|s| !s.trim().is_empty()).ok_or(ConfigError::MissingPublicUrl)?;
        let mut cfg = Config::new(public_url, raw.data_dir.clone().unwrap_or_else(|| "/data".into()))?;

        if let Some(s) = raw.listen.as_deref() {
            cfg.listen = s.trim().parse().map_err(|e| invalid("ANYCON_LISTEN", format!("{s:?}: {e}")))?;
        }
        if let Some(s) = raw.mode.as_deref() {
            match s.trim().to_ascii_lowercase().as_str() {
                "single" | "" => {}
                "multi" => cfg.mode = Mode::Multi,
                other => return Err(invalid("ANYCON_MODE", format!("{other:?} (expected `single` or `multi`)"))),
            }
        }
        cfg.admin_token = raw.admin_token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        if cfg.mode.is_multi() && cfg.admin_token.is_some() {
            cfg.ignored_vars.push("ANYCON_ADMIN_TOKEN".into());
        }
        cfg.database_url = raw.database_url.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if let Some(s) = raw.trusted_proxies.as_deref() {
            cfg.trusted_proxies = parse_trusted_proxies(s)?;
        }
        if let Some(s) = raw.log.filter(|s| !s.trim().is_empty()) {
            cfg.log = s;
        }
        if let Some(s) = raw.max_envelope_bytes.as_deref() {
            let n: usize = s.trim().parse().map_err(|e| invalid("ANYCON_MAX_ENVELOPE_BYTES", format!("{s:?}: {e}")))?;
            if n < MIN_ENVELOPE_BYTES {
                return Err(invalid(
                    "ANYCON_MAX_ENVELOPE_BYTES",
                    format!(
                        "{n} is too small: it must be at least {MIN_ENVELOPE_BYTES} ({} KiB), the size of one full \
                         {}-KiB fragment envelope. Clients split every message into fragments of that size, so with a \
                         smaller limit every full fragment would be dropped as too_large and large messages could never \
                         get through",
                        MIN_ENVELOPE_BYTES / 1024,
                        frame::MAX_FRAGMENT / 1024
                    ),
                ));
            }
            cfg.max_envelope_bytes = n;
        }
        if let Some(s) = raw.rate_client_bytes_per_min.as_deref() {
            cfg.limits.client_bytes_per_min = parse_rate("ANYCON_RATE_CLIENT_BYTES_PER_MIN", s)?;
        }
        if let Some(s) = raw.rate_daemon_bytes_per_min.as_deref() {
            cfg.limits.daemon_bytes_per_min = parse_rate("ANYCON_RATE_DAEMON_BYTES_PER_MIN", s)?;
        }
        if let Some(s) = raw.preview_url.as_deref().filter(|s| !s.trim().is_empty()) {
            cfg.preview = Some(parse_preview_url(s, &cfg.public_url)?);
        }
        if let Some(s) = raw.preview_bytes_per_min.as_deref() {
            cfg.limits.preview_bytes_per_min = parse_rate("ANYCON_PREVIEW_BYTES_PER_MIN", s)?;
        }
        if let Some(s) = raw.preview_max_connections.as_deref() {
            let n: u32 =
                s.trim().parse().map_err(|e| invalid("ANYCON_PREVIEW_MAX_CONNECTIONS", format!("{s:?}: {e}")))?;
            if n == 0 {
                return Err(invalid("ANYCON_PREVIEW_MAX_CONNECTIONS", "must be at least 1"));
            }
            cfg.limits.preview_max_connections = n;
        }
        if let Some(s) = raw.smtp_url.as_deref().filter(|s| !s.trim().is_empty()) {
            cfg.smtp = Some(parse_smtp_url(s)?);
        }
        if let Some(s) = raw.email_from.as_deref().filter(|s| !s.trim().is_empty()) {
            cfg.email_from = Some(parse_email_from(s)?);
        }
        if cfg.smtp.is_some() && cfg.email_from.is_none() {
            return Err(invalid(
                "ANYCON_EMAIL_FROM",
                "required when ANYCON_SMTP_URL is set (e.g. `Anycon <noreply@example.com>`)",
            ));
        }
        if let Some(s) = raw.vapid_subject.as_deref().filter(|s| !s.trim().is_empty()) {
            cfg.vapid_subject = Some(parse_vapid_subject(s)?);
        }

        // 管理员邮箱、配额、OAuth：不管哪个模式都解析和校验（笔误早点发现，切换模式时不会才炸），
        // 但只有 `multi` 用管理员邮箱和配额；`single` 里设了就警告被忽略。
        if let Some(s) = raw.admin_emails.as_deref().filter(|s| !s.trim().is_empty()) {
            cfg.admin_emails = parse_admin_emails(s)?;
            if !cfg.mode.is_multi() {
                cfg.ignored_vars.push("ANYCON_ADMIN_EMAILS".into());
            }
        }
        let quota_value = |var: &'static str, raw: Option<&str>| {
            raw.filter(|v| !v.trim().is_empty()).map(|v| parse_quota(var, v)).transpose()
        };
        let devices = quota_value("ANYCON_QUOTA_DEVICES", raw.quota_devices.as_deref())?;
        let daemons = quota_value("ANYCON_QUOTA_DAEMONS", raw.quota_daemons.as_deref())?;
        let connections = quota_value("ANYCON_QUOTA_CONNECTIONS", raw.quota_connections.as_deref())?;
        let daily_pairings = quota_value("ANYCON_QUOTA_DAILY_PAIRINGS", raw.quota_daily_pairings.as_deref())?;
        if cfg.mode.is_multi() {
            let d = QuotaLimits::default();
            cfg.enable_multi(
                QuotaLimits {
                    devices: devices.unwrap_or(d.devices),
                    daemons: daemons.unwrap_or(d.daemons),
                    connections: connections.unwrap_or(d.connections),
                },
                daily_pairings.unwrap_or(DEFAULT_DAILY_PAIRINGS),
            );
        } else {
            for (var, set) in [
                ("ANYCON_QUOTA_DEVICES", devices.is_some()),
                ("ANYCON_QUOTA_DAEMONS", daemons.is_some()),
                ("ANYCON_QUOTA_CONNECTIONS", connections.is_some()),
                ("ANYCON_QUOTA_DAILY_PAIRINGS", daily_pairings.is_some()),
            ] {
                if set {
                    cfg.ignored_vars.push(var.into());
                }
            }
        }
        cfg.oauth_github = parse_oauth_client(
            ("ANYCON_OAUTH_GITHUB_CLIENT_ID", raw.oauth_github_client_id.as_deref()),
            ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", raw.oauth_github_client_secret.as_deref()),
        )?;
        cfg.oauth_google = parse_oauth_client(
            ("ANYCON_OAUTH_GOOGLE_CLIENT_ID", raw.oauth_google_client_id.as_deref()),
            ("ANYCON_OAUTH_GOOGLE_CLIENT_SECRET", raw.oauth_google_client_secret.as_deref()),
        )?;

        for name in raw.other.keys() {
            if UNIMPLEMENTED.contains(&name.as_str()) || name.starts_with("quota_") {
                cfg.unimplemented_vars.push(format!("ANYCON_{}", name.to_ascii_uppercase()));
            } else {
                cfg.unknown_vars.push(format!("ANYCON_{}", name.to_ascii_uppercase()));
            }
        }
        cfg.check_sign_in()?;
        Ok(cfg)
    }

    /// 切到 `multi`：设模式、数量类配额和每日配对上限（写进 `limits.daily_pairings_per_account`，那是它唯一的一份）。
    /// 环境变量路径和测试都走它。
    pub fn enable_multi(&mut self, quota: QuotaLimits, daily_pairings: u32) {
        self.mode = Mode::Multi;
        self.quota = Some(quota);
        self.limits.daily_pairings_per_account = Some(daily_pairings);
    }

    /// 这个 OAuth 提供方配了应用凭据没有。
    pub fn oauth_client(&self, provider: OAuthProvider) -> Option<&OAuthClient> {
        match provider {
            OAuthProvider::Github => self.oauth_github.as_ref(),
            OAuthProvider::Google => self.oauth_google.as_ref(),
        }
    }

    /// 配了凭据的 OAuth 提供方，按 `login_methods` 的顺序（github 在前）。
    pub fn oauth_providers(&self) -> impl Iterator<Item = OAuthProvider> + '_ {
        OAuthProvider::ALL.into_iter().filter(|p| self.oauth_client(*p).is_some())
    }

    /// 某个提供方实际使用的端点：测试覆盖优先，否则是写死的生产端点。
    pub fn oauth_endpoints(&self, provider: OAuthProvider) -> OAuthEndpoints {
        let over = match provider {
            OAuthProvider::Github => self.oauth_endpoint_overrides.github.as_ref(),
            OAuthProvider::Google => self.oauth_endpoint_overrides.google.as_ref(),
        };
        over.cloned().unwrap_or_else(|| provider.default_endpoints())
    }

    /// 邮箱（规范化后）在 `ANYCON_ADMIN_EMAILS` 里。不看模式——谁能当管理员见 [`crate::accounts::is_effective_admin`]。
    pub fn is_admin_email(&self, email: &str) -> bool {
        self.admin_emails.iter().any(|a| a == email)
    }

    /// 是否有至少一种登录途径：SMTP（或测试注入的发信实现）或任一 OAuth 提供方。
    fn has_sign_in(&self) -> bool {
        self.smtp.is_some()
            || self.mailer_override.is_some()
            || self.oauth_github.is_some()
            || self.oauth_google.is_some()
    }

    /// `multi` 是开放注册，没有 bootstrap token：没有邮箱也没有 OAuth 就没人能登录，启动就报错。
    fn check_sign_in(&self) -> Result<(), ConfigError> {
        if self.mode.is_multi() && !self.has_sign_in() {
            return Err(ConfigError::MultiNoSignIn);
        }
        Ok(())
    }

    /// 启动时要打的警告（由 [`crate::run`] 逐条输出）：被当前模式忽略的变量，以及 OAuth 配了但
    /// `ANYCON_PUBLIC_URL` 不是 https。文字里不含邮箱、凭据这类值。
    pub fn startup_warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .ignored_vars
            .iter()
            .map(|name| format!("{name} is set but ignored in {} mode", self.mode.as_str()))
            .collect();
        if self.oauth_providers().next().is_some() && self.public_url.scheme() != "https" {
            out.push(
                "OAuth sign-in is configured but ANYCON_PUBLIC_URL is not https: providers expect an https callback \
                 URL (plain http is only fine for local development)"
                    .into(),
            );
        }
        out
    }

    /// 启动前的最后检查（环境变量路径在 [`Config::from_vars`] 里已经检查过；程序化构造的配置也要过这一关）：
    /// `multi` 至少要有一种登录途径，配了 SMTP 必须有发件人，`smtp+plain` 的主机写法要过 [`check_plain_host`]（IP 字面量必须是本机 / 内网），
    /// 预览域名不能和 `ANYCON_PUBLIC_URL` 在同一个可注册域名下。
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.check_sign_in()?;
        if let Some(preview) = &self.preview {
            check_preview_site(preview, &self.public_url)?;
        }
        if let Some(smtp) = &self.smtp {
            if self.email_from.is_none() {
                return Err(invalid("ANYCON_EMAIL_FROM", "required when ANYCON_SMTP_URL is set"));
            }
            if smtp.security == SmtpSecurity::Plain {
                check_plain_host(&smtp.host)
                    .map_err(|why| invalid("ANYCON_SMTP_URL", plain_host_message(&smtp.host, why)))?;
            }
        }
        Ok(())
    }

    /// `ANYCON_PUBLIC_URL` 的字符串形式，不带末尾 `/`。
    pub fn public_url_str(&self) -> String {
        self.public_url.as_str().trim_end_matches('/').to_string()
    }

    /// Web Push 的 VAPID `sub`（RFC 8292）：`ANYCON_VAPID_SUBJECT`；没设时 `ANYCON_PUBLIC_URL` 是 https 就用它，
    /// 否则 `mailto:` + 发件人地址；连发件人都没有（没配邮件、公网地址也不是 https）就用 `mailto:noreply@<主机名>`。
    pub fn vapid_subject(&self) -> String {
        if let Some(s) = &self.vapid_subject {
            return s.clone();
        }
        if self.public_url.scheme() == "https" {
            return self.public_url_str();
        }
        if let Some(f) = &self.email_from {
            return format!("mailto:{}", f.address);
        }
        format!("mailto:noreply@{}", self.public_url.host_str().unwrap_or("localhost"))
    }
}

/// 解析 `ANYCON_SMTP_URL`：`smtp://`（STARTTLS）、`smtps://`（隐式 TLS）、`smtp+plain://`（不加密，只限本机 / 私有网段）。
/// 用户名密码写在 URL 里（百分号编码）。**错误信息里不带原始 URL**（它含密码）。
pub fn parse_smtp_url(s: &str) -> Result<SmtpConfig, ConfigError> {
    const VAR: &str = "ANYCON_SMTP_URL";
    let url = Url::parse(s.trim()).map_err(|e| invalid(VAR, format!("not a valid URL ({e})")))?;
    let (security, default_port) = match url.scheme() {
        "smtp" => (SmtpSecurity::StartTls, 587),
        "smtps" => (SmtpSecurity::Tls, 465),
        "smtp+plain" => (SmtpSecurity::Plain, 25),
        other => {
            return Err(invalid(
                VAR,
                format!("unsupported scheme `{other}` (expected smtp://, smtps:// or smtp+plain://)"),
            ))
        }
    };
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_ascii_lowercase(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(invalid(VAR, "missing host")),
    };
    if !matches!(url.path(), "" | "/") || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid(VAR, "must not contain a path, query or fragment"));
    }
    let decode = |raw: &str, what: &str| {
        percent_encoding::percent_decode_str(raw)
            .decode_utf8()
            .map(|c| c.into_owned())
            .map_err(|_| invalid(VAR, format!("the {what} is not valid UTF-8 after percent-decoding")))
    };
    let username = Some(url.username()).filter(|u| !u.is_empty()).map(|u| decode(u, "username")).transpose()?;
    let password = url.password().map(|p| decode(p, "password")).transpose()?;
    if password.is_some() && username.is_none() {
        return Err(invalid(VAR, "a password needs a username"));
    }
    if security == SmtpSecurity::Plain {
        check_plain_host(&host).map_err(|why| invalid(VAR, plain_host_message(&host, why)))?;
    }
    Ok(SmtpConfig { security, host, port: url.port().unwrap_or(default_port), username, password })
}

/// `smtp+plain://` 的主机为什么不能接受（见 [`check_plain_host`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlainHostError {
    /// IP 字面量不是本机 / 内网地址。
    PublicAddress,
    /// 不是合法的主机名（含 `%` 一类的字符），或者是 libc 会当 IP 解析的纯数字写法（`134744072`、`0x7f.1`、`127.1`）。
    BadName,
}

fn plain_host_message(host: &str, why: PlainHostError) -> String {
    match why {
        PlainHostError::PublicAddress => format!(
            "smtp+plain:// sends mail and credentials unencrypted, so it is only allowed for localhost and private \
             networks (address `{host}` is not); use smtp:// (STARTTLS) or smtps:// for a remote server"
        ),
        PlainHostError::BadName => format!(
            "smtp+plain:// host `{host}` is not a valid host name (write an IP address as a plain dotted or IPv6 literal)"
        ),
    }
}

/// `smtp+plain://`（不加密）的主机在**启动时**能不能接受。
///
/// - **IP 字面量**：必须是回环 / 私有 / 链路本地 / 100.64.0.0/10 / IPv6 `fc00::/7` `fe80::/10`，否则启动报错。
/// - **主机名**（`localhost`、`*.localhost`、compose 里的服务名 `mailpit`……）：启动时不判断它指向哪里——服务名启动时往往
///   还解析不了，DNS 也会变。判断挪到**每次发信前**：[`crate::mail::SmtpMailer`] 自己解析主机名，所有解析结果都必须是本机 / 内网，
///   然后把解析出的 IP 交给 lettre，不让它再解析一次（不会被 DNS 搜索域、改解析骗到公网）。所以以前那条「不带点的名字 =
///   内网服务名」的规则没有了（`ai.`、会被搜索域补全的 `mail` 都不成立）。这里只拒写法本身有问题的：非法字符（`%` ……），
///   以及最后一段是数字的名字——libc 会把 `134744072`（= 8.8.8.8）、`0x08080808`、`127.1` 当 IP 解析，别让它们混成「主机名」。
///
/// `host` 可以带 IPv6 的方括号和末尾的点。
pub fn check_plain_host(host: &str) -> Result<(), PlainHostError> {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    // `smtp+plain` 不是 special scheme，`url` 不会把 `127.0.0.1` 解析成 IPv4，所以这里统一自己认 IP。
    if let Ok(ip) = h.parse::<IpAddr>() {
        return if is_private_ip(ip) { Ok(()) } else { Err(PlainHostError::PublicAddress) };
    }
    let name = h.trim_end_matches('.');
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    let numeric = |l: &str| {
        l.bytes().all(|b| b.is_ascii_digit())
            || l.strip_prefix("0x").is_some_and(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let last = name.rsplit('.').next().unwrap_or("");
    if name.is_empty() || name.len() > 253 || !name.split('.').all(label_ok) || numeric(last) {
        return Err(PlainHostError::BadName);
    }
    Ok(())
}

/// 是不是本机 / 内网地址：回环、私有（RFC 1918）、链路本地、100.64.0.0/10（运营商级 NAT / Tailscale），
/// IPv6 的 `::1`、`fc00::/7`、`fe80::/10`；`::ffff:a.b.c.d` 按 IPv4 判断。
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // 100.64.0.0/10：运营商级 NAT / Tailscale 一类的共享地址段，不在公网上路由
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback() || (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xffc0) == 0xfe80
        }
    }
}

/// 解析 `ANYCON_EMAIL_FROM`：`Anycon <noreply@example.com>` 或裸地址。
pub fn parse_email_from(s: &str) -> Result<EmailFrom, ConfigError> {
    let mailbox: lettre::message::Mailbox = s.trim().parse().map_err(|e| {
        invalid("ANYCON_EMAIL_FROM", format!("{s:?}: {e} (expected `Name <addr@example.com>` or `addr@example.com`)"))
    })?;
    let address = mailbox.email.to_string();
    Ok(EmailFrom { mailbox, address })
}

/// 解析 `ANYCON_VAPID_SUBJECT`：`mailto:you@example.com` 或 https 地址（RFC 8292 § 2.1）。
pub fn parse_vapid_subject(s: &str) -> Result<String, ConfigError> {
    let s = s.trim();
    let ok = if let Some(rest) = s.strip_prefix("mailto:") {
        rest.contains('@') && !rest.starts_with('@') && !rest.ends_with('@') && !rest.chars().any(char::is_whitespace)
    } else if s.starts_with("https://") {
        Url::parse(s).is_ok_and(|u| u.host_str().is_some())
    } else {
        false
    };
    if !ok {
        return Err(invalid(
            "ANYCON_VAPID_SUBJECT",
            format!("{s:?}: expected `mailto:you@example.com` or an https:// URL"),
        ));
    }
    Ok(s.to_string())
}

/// 解析 `ANYCON_RATE_*_BYTES_PER_MIN`：正整数字节数，不小于 [`MIN_ENVELOPE_BYTES`]——
/// 预算小于一个满片信封的话，发送方永远凑不够额度把它送出去。
fn parse_rate(var: &'static str, s: &str) -> Result<u64, ConfigError> {
    let n: u64 = s.trim().parse().map_err(|e| invalid(var, format!("{s:?}: {e}")))?;
    if n < MIN_ENVELOPE_BYTES as u64 {
        return Err(invalid(
            var,
            format!("{n} is too small: it must be at least {MIN_ENVELOPE_BYTES} bytes (one full fragment envelope)"),
        ));
    }
    Ok(n)
}

/// 解析后的 `ANYCON_PREVIEW_URL`（ADR-39，docs/spec/hub.md § 13.1）：预览 origin 的模板
/// `<scheme>://*.<域名>[:端口]`，`*` 是最左边的一整段，代表每个预览的 label。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewUrl {
    /// `http` 或 `https`（http 只允许 `localhost` / `*.localhost`）。
    pub scheme: &'static str,
    /// 预览域名（小写、不带 `*.`），如 `aiplace.app`。
    pub domain: String,
    /// 显式端口；scheme 的默认端口（80 / 443）归一成 `None`。
    pub port: Option<u16>,
}

/// [`PreviewUrl::match_host`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMatch {
    /// 不在预览域名下（主域名、局域网 IP……）：照旧走主站。
    Other,
    /// 在预览域名下，但 `*` 那一段不是合法的 label。
    InvalidLabel,
    /// `<label>.<预览域名>[:端口]`。
    Label(String),
}

impl PreviewUrl {
    fn default_port(&self) -> u16 {
        if self.scheme == "https" {
            443
        } else {
            80
        }
    }

    fn authority_suffix(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{p}", self.domain),
            None => self.domain.clone(),
        }
    }

    /// 某个 label 的预览 origin：`https://<label>.<预览域名>[:端口]`。
    pub fn origin_for(&self, label: &str) -> String {
        format!("{}://{label}.{}", self.scheme, self.authority_suffix())
    }

    /// 给 `/api/config` 的模板：`https://{label}.<预览域名>[:端口]`。
    pub fn origin_template(&self) -> String {
        format!("{}://{{label}}.{}", self.scheme, self.authority_suffix())
    }

    /// 主站 CSP `frame-src` 用的来源表达式：`https://*.<预览域名>[:端口]`。
    pub fn frame_src(&self) -> String {
        format!("{}://*.{}", self.scheme, self.authority_suffix())
    }

    /// `Host` 头（`host[:port]`）是不是 `<label>.<预览域名>[:端口]`。端口按配置：配了显式端口就必须带它，
    /// 没配则不带或带 scheme 的默认端口都行。主机名不分大小写，末尾的点忽略。
    pub fn match_host(&self, host: &str) -> HostMatch {
        let host = host.trim().to_ascii_lowercase();
        let (name, port) = match host.rsplit_once(':') {
            Some((name, port)) => match port.parse::<u16>() {
                Ok(p) => (name, Some(p)),
                Err(_) => return HostMatch::Other,
            },
            None => (host.as_str(), None),
        };
        if port.unwrap_or(self.default_port()) != self.port.unwrap_or(self.default_port()) {
            return HostMatch::Other;
        }
        let name = name.strip_suffix('.').unwrap_or(name);
        let Some(label) = name.strip_suffix(self.domain.as_str()).and_then(|rest| rest.strip_suffix('.')) else {
            return HostMatch::Other;
        };
        if anycon_protocol::preview::tunnel::is_valid_label(label) {
            HostMatch::Label(label.to_string())
        } else {
            HostMatch::InvalidLabel
        }
    }
}

/// 主机是 `localhost` 或 `*.localhost`（RFC 6761）：本机开发用，允许 http，也不参与可注册域名的比较。
fn is_localhost_name(host: &str) -> bool {
    let h = host.trim_end_matches('.');
    h == "localhost" || h.ends_with(".localhost")
}

/// 解析 `ANYCON_PREVIEW_URL`：`https://*.<预览域名>[:端口]`。
///
/// - `*` 只能是最左边的一整段；不带路径 / query / fragment / 用户名；
/// - 必须是 https，唯一的例外是主机是 `localhost` 或 `*.localhost`（本机开发和测试）；
/// - 预览域名不能是一个公共后缀（`*.com`），也不能和 `ANYCON_PUBLIC_URL` 在同一个可注册域名下（[`check_preview_site`]）。
pub fn parse_preview_url(s: &str, public_url: &Url) -> Result<PreviewUrl, ConfigError> {
    const VAR: &str = "ANYCON_PREVIEW_URL";
    let s = s.trim();
    let bad = |why: &str| invalid(VAR, format!("{s:?}: {why}"));
    let (scheme, rest) = if let Some(rest) = s.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = s.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(bad("expected https://*.<domain> (e.g. https://*.preview-example.net)"));
    };
    let Some(domain_part) = rest.strip_prefix("*.") else {
        return Err(bad("the wildcard `*` must be the whole leftmost label, as in https://*.preview-example.net"));
    };
    if domain_part.contains('*') {
        return Err(bad("the wildcard `*` must be the whole leftmost label and may appear only once"));
    }
    let url = Url::parse(&format!("{scheme}://{domain_part}")).map_err(|e| bad(&format!("not a valid URL ({e})")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("must not contain credentials"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(bad("must not contain a path, query or fragment"));
    }
    if url.port() == Some(0) {
        return Err(bad("port 0 is not valid"));
    }
    let Some(url::Host::Domain(domain)) = url.host() else {
        return Err(bad("the preview domain must be a DNS name, not an IP address"));
    };
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let localhost = is_localhost_name(&domain);
    if scheme == "http" && !localhost {
        return Err(bad(
            "must be https (plain http is only allowed for localhost development, e.g. http://*.preview.localhost:8080)",
        ));
    }
    if !localhost && psl::domain_str(&domain).is_none() {
        return Err(bad(
            "the preview domain must be a registrable domain (like preview-example.net), not a public suffix",
        ));
    }
    let preview = PreviewUrl { scheme, domain, port: url.port() };
    check_preview_site(&preview, public_url)?;
    Ok(preview)
}

/// 预览域名不能和 `ANYCON_PUBLIC_URL` 的主机在同一个可注册域名（eTLD+1，按 Public Suffix List）下：
/// 同站的子域能往父域写 cookie（cookie tossing），覆盖 hub 的登录 cookie。
/// `ANYCON_PUBLIC_URL` 是 IP 字面量或 `localhost` 一类时不比（没有可注册域名这回事）。
pub fn check_preview_site(preview: &PreviewUrl, public_url: &Url) -> Result<(), ConfigError> {
    let public_host = match public_url.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        _ => return Ok(()),
    };
    if is_localhost_name(&public_host) || is_localhost_name(&preview.domain) {
        return Ok(());
    }
    match (psl::domain_str(&public_host), psl::domain_str(&preview.domain)) {
        (Some(a), Some(b)) if a == b => Err(invalid(
            "ANYCON_PREVIEW_URL",
            format!(
                "the preview domain `{}` is in the same registrable domain (`{a}`) as ANYCON_PUBLIC_URL's `{public_host}`. \
                 Pages under a sibling subdomain could set cookies for the hub's domain, so the preview needs its own \
                 domain (for example the hub at hub.example.com and previews at *.preview-example.net)",
                preview.domain
            ),
        )),
        _ => Ok(()),
    }
}

/// 解析 `ANYCON_ADMIN_EMAILS`：逗号分隔的邮箱，每个按 [`crate::mail::normalize_email`] 规范化，去重，空项忽略。
/// 有不合法的项就报错——错误里只给序号，不回显内容（它多半是个真实邮箱的笔误）。
pub fn parse_admin_emails(s: &str) -> Result<Vec<String>, ConfigError> {
    let mut out: Vec<String> = Vec::new();
    for (i, item) in s.split(',').enumerate() {
        if item.trim().is_empty() {
            continue;
        }
        let email = crate::mail::normalize_email(item)
            .ok_or_else(|| invalid("ANYCON_ADMIN_EMAILS", format!("entry #{} is not a valid email address", i + 1)))?;
        if !out.contains(&email) {
            out.push(email);
        }
    }
    Ok(out)
}

/// 解析 `ANYCON_QUOTA_*`：正整数（≥ 1）。
fn parse_quota(var: &'static str, s: &str) -> Result<u32, ConfigError> {
    let n: u32 = s.trim().parse().map_err(|e| invalid(var, format!("{s:?}: {e} (expected a positive integer)")))?;
    if n == 0 {
        return Err(invalid(var, "must be at least 1"));
    }
    Ok(n)
}

/// 解析一个 OAuth 提供方的 `CLIENT_ID` / `CLIENT_SECRET`：都空 = 没配；只有一个 = 报错；都有则不能含空白 / 控制字符
/// （多半是复制粘贴带进来的换行）。**错误里不回显 secret。**
fn parse_oauth_client(
    (id_var, id): (&'static str, Option<&str>),
    (secret_var, secret): (&'static str, Option<&str>),
) -> Result<Option<OAuthClient>, ConfigError> {
    let id = id.map(str::trim).filter(|s| !s.is_empty());
    let secret = secret.map(str::trim).filter(|s| !s.is_empty());
    let (client_id, client_secret) = match (id, secret) {
        (None, None) => return Ok(None),
        (Some(_), None) => return Err(invalid(secret_var, format!("required when {id_var} is set"))),
        (None, Some(_)) => return Err(invalid(id_var, format!("required when {secret_var} is set"))),
        (Some(i), Some(s)) => (i, s),
    };
    for (var, value) in [(id_var, client_id), (secret_var, client_secret)] {
        if value.len() > 512 || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(invalid(var, "must be at most 512 characters with no whitespace or control characters"));
        }
    }
    Ok(Some(OAuthClient { client_id: client_id.to_string(), client_secret: client_secret.to_string() }))
}

/// 解析并校验 `ANYCON_PUBLIC_URL`：必须是 http(s) URL，有主机名，不带路径 / query / fragment。
pub fn parse_public_url(s: &str) -> Result<Url, ConfigError> {
    let s = s.trim();
    let url = Url::parse(s).map_err(|e| invalid("ANYCON_PUBLIC_URL", format!("{s:?}: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("ANYCON_PUBLIC_URL", format!("{s:?}: scheme must be http or https")));
    }
    if url.host_str().is_none() {
        return Err(invalid("ANYCON_PUBLIC_URL", format!("{s:?}: missing host")));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("ANYCON_PUBLIC_URL", format!("{s:?}: must not contain a path, query or fragment")));
    }
    Ok(url)
}

/// 逗号分隔的 CIDR / IP 列表。空串 = 不信任任何代理。
pub fn parse_trusted_proxies(s: &str) -> Result<Vec<IpNet>, ConfigError> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<IpNet>()
                .or_else(|_| p.parse::<std::net::IpAddr>().map(IpNet::from))
                .map_err(|e| invalid("ANYCON_TRUSTED_PROXIES", format!("{p:?}: {e}")))
        })
        .collect()
}

/// 原始字符串的 `ANYCON_*` 环境变量，作为 figment provider。
struct RawEnv {
    vars: Vec<(String, String)>,
}

impl Provider for RawEnv {
    fn metadata(&self) -> Metadata {
        Metadata::named("ANYCON_* environment variables")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        let mut dict = Dict::new();
        for (k, v) in &self.vars {
            if let Some(rest) = k.strip_prefix("ANYCON_") {
                dict.insert(rest.to_ascii_lowercase(), Value::from(v.clone()));
            }
        }
        Ok(Profile::Default.collect(dict))
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    public_url: Option<String>,
    listen: Option<String>,
    mode: Option<String>,
    admin_token: Option<String>,
    data_dir: Option<String>,
    database_url: Option<String>,
    trusted_proxies: Option<String>,
    log: Option<String>,
    max_envelope_bytes: Option<String>,
    rate_client_bytes_per_min: Option<String>,
    rate_daemon_bytes_per_min: Option<String>,
    smtp_url: Option<String>,
    email_from: Option<String>,
    vapid_subject: Option<String>,
    preview_url: Option<String>,
    preview_bytes_per_min: Option<String>,
    preview_max_connections: Option<String>,
    admin_emails: Option<String>,
    quota_devices: Option<String>,
    quota_daemons: Option<String>,
    quota_connections: Option<String>,
    quota_daily_pairings: Option<String>,
    oauth_github_client_id: Option<String>,
    oauth_github_client_secret: Option<String>,
    oauth_google_client_id: Option<String>,
    oauth_google_client_secret: Option<String>,
    #[serde(flatten)]
    other: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults() {
        let c = Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "https://hub.example.com")])).unwrap();
        assert_eq!(c.public_url_str(), "https://hub.example.com");
        assert_eq!(c.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(c.data_dir, PathBuf::from("/data"));
        assert_eq!(c.max_envelope_bytes, 262_144);
        assert_eq!(c.trusted_proxies.len(), 1);
        assert!(c.admin_token.is_none());
    }

    #[test]
    fn public_url_required_and_validated() {
        assert!(matches!(Config::from_vars(vars(&[])), Err(ConfigError::MissingPublicUrl)));
        assert!(Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "ftp://x")])).is_err());
        assert!(Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "hub.example.com")])).is_err());
        assert!(Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "https://x.com/sub")])).is_err());
        assert!(Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://127.0.0.1:8080/")])).is_ok());
    }

    #[test]
    fn debug_output_redacts_the_admin_token() {
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_ADMIN_TOKEN", "super-secret-token-value"),
        ]))
        .unwrap();
        assert!(!format!("{c:?}").contains("super-secret"));
    }

    #[test]
    fn mode_is_single_or_multi_case_insensitively_and_nothing_else() {
        let base = ("ANYCON_PUBLIC_URL", "http://x.test");
        for (value, expected) in [("single", Mode::Single), ("", Mode::Single), (" Single ", Mode::Single)] {
            assert_eq!(Config::from_vars(vars(&[base, ("ANYCON_MODE", value)])).unwrap().mode, expected, "{value:?}");
        }
        assert_eq!(Config::from_vars(vars(&[base])).unwrap().mode, Mode::Single, "default");
        for value in ["multi", "MULTI", " Multi "] {
            let c = Config::from_vars(vars(&[
                base,
                ("ANYCON_MODE", value),
                ("ANYCON_SMTP_URL", "smtp+plain://127.0.0.1"),
                ("ANYCON_EMAIL_FROM", "n@example.com"),
            ]))
            .unwrap();
            assert_eq!(c.mode, Mode::Multi, "{value:?}");
            assert_eq!(c.mode.as_str(), "multi");
        }
        let err = Config::from_vars(vars(&[base, ("ANYCON_MODE", "cluster")])).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_MODE", .. }), "{err}");
        assert!(err.to_string().contains("single") && err.to_string().contains("multi"), "{err}");
    }

    #[test]
    fn numeric_looking_token_stays_a_string() {
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_ADMIN_TOKEN", "0012345678901234567890"),
        ]))
        .unwrap();
        assert_eq!(c.admin_token.as_deref(), Some("0012345678901234567890"));
    }

    #[test]
    fn proxies_and_limits() {
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_TRUSTED_PROXIES", "10.0.0.0/8, 127.0.0.1 ,::1"),
            ("ANYCON_MAX_ENVELOPE_BYTES", "300000"),
            ("ANYCON_LISTEN", "127.0.0.1:9000"),
        ]))
        .unwrap();
        assert_eq!(c.trusted_proxies.len(), 3);
        assert_eq!(c.max_envelope_bytes, 300_000);
        assert_eq!(c.listen.port(), 9000);
        assert!(Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://x.test"), ("ANYCON_TRUSTED_PROXIES", "nope")]))
            .is_err());
        let none =
            Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://x.test"), ("ANYCON_TRUSTED_PROXIES", "")])).unwrap();
        assert!(none.trusted_proxies.is_empty());
    }

    #[test]
    fn unimplemented_and_unknown_vars_are_collected() {
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_QUEUE_ENABLED", "true"),
            ("ANYCON_QUOTA_QUEUE_ITEMS", "3"),
            ("ANYCON_HOME", "/tmp/x"),
            ("OTHER", "1"),
        ]))
        .unwrap();
        assert_eq!(c.unimplemented_vars, vec!["ANYCON_QUEUE_ENABLED", "ANYCON_QUOTA_QUEUE_ITEMS"]);
        assert_eq!(c.unknown_vars, vec!["ANYCON_HOME"]);
        assert!(c.ignored_vars.is_empty());
    }

    #[test]
    fn min_envelope_is_201_kib_and_fits_a_worst_case_full_fragment() {
        use anycon_protocol::{Envelope, Kind};
        assert_eq!(MIN_ENVELOPE_BYTES, 201 * 1024);
        // 真的编码一个满片的 data 信封：最长的设备 id（64 个字符）、用满的 seq，也不超过下限。
        let id = "f".repeat(64);
        let ciphertext = vec![0xAB; frame::MAX_FRAGMENT + 1];
        let bodies = frame::encode(&ciphertext, u32::MAX);
        assert_eq!(bodies.len(), 2);
        let full = bodies.into_iter().next().unwrap();
        assert_eq!(full.len(), frame::MAX_FRAGMENT + FRAGMENT_FRAME_HEADER);
        let bytes = Envelope::new(id.clone(), id, Kind::Handshake, u64::MAX, full).encode().unwrap();
        assert!(bytes.len() <= MIN_ENVELOPE_BYTES, "{} > {MIN_ENVELOPE_BYTES}", bytes.len());
        // 且头开销确实在预留的范围内、没有被预留吞掉整个 KiB 的余量
        assert!(bytes.len() - frame::MAX_FRAGMENT - FRAGMENT_FRAME_HEADER <= ENVELOPE_HEADER_ALLOWANCE);
    }

    #[test]
    fn max_envelope_bytes_below_a_full_fragment_refuses_to_start() {
        let base = ("ANYCON_PUBLIC_URL", "http://x.test");
        // 正好等于下限：可以
        let ok = Config::from_vars(vars(&[base, ("ANYCON_MAX_ENVELOPE_BYTES", &MIN_ENVELOPE_BYTES.to_string())]));
        assert_eq!(ok.unwrap().max_envelope_bytes, MIN_ENVELOPE_BYTES);
        // 默认值本身必须高于下限
        assert!(Config::new("http://x.test", "/tmp").unwrap().max_envelope_bytes >= MIN_ENVELOPE_BYTES);
        // 少一个字节、以及旧的最小值 1024：拒绝，错误信息说清楚原因
        for bad in [MIN_ENVELOPE_BYTES - 1, 205_000, 4096, 1024] {
            let err = Config::from_vars(vars(&[base, ("ANYCON_MAX_ENVELOPE_BYTES", &bad.to_string())])).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_MAX_ENVELOPE_BYTES", .. }), "{msg}");
            assert!(msg.contains(&MIN_ENVELOPE_BYTES.to_string()), "{msg}");
            assert!(msg.contains("201 KiB") || msg.contains("201 "), "{msg}");
            assert!(msg.contains("fragment") && msg.contains("too_large"), "{msg}");
        }
        assert!(Config::from_vars(vars(&[base, ("ANYCON_MAX_ENVELOPE_BYTES", "lots")])).is_err());
    }

    #[test]
    fn rate_budgets_default_per_connection_kind_and_are_configurable() {
        let c = Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://x.test")])).unwrap();
        assert_eq!(c.limits.client_bytes_per_min, 10 * 1024 * 1024);
        assert_eq!(c.limits.daemon_bytes_per_min, 100 * 1024 * 1024);
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_RATE_CLIENT_BYTES_PER_MIN", "20971520"),
            ("ANYCON_RATE_DAEMON_BYTES_PER_MIN", "1073741824"),
        ]))
        .unwrap();
        assert_eq!(c.limits.client_bytes_per_min, 20 * 1024 * 1024);
        assert_eq!(c.limits.daemon_bytes_per_min, 1024 * 1024 * 1024);
        // 这两个变量是已识别的，不会出现在「未知变量」里
        assert!(c.unknown_vars.is_empty() && c.unimplemented_vars.is_empty());
        // 只设一个，另一个保持默认
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://x.test"),
            ("ANYCON_RATE_DAEMON_BYTES_PER_MIN", "52428800"),
        ]))
        .unwrap();
        assert_eq!(c.limits.client_bytes_per_min, DEFAULT_CLIENT_BYTES_PER_MIN);
        assert_eq!(c.limits.daemon_bytes_per_min, 50 * 1024 * 1024);
        // 不是数字 / 小到连一个满片都送不出去：拒绝
        for (var, bad) in [
            ("ANYCON_RATE_CLIENT_BYTES_PER_MIN", "10MiB"),
            ("ANYCON_RATE_CLIENT_BYTES_PER_MIN", "0"),
            ("ANYCON_RATE_DAEMON_BYTES_PER_MIN", "-1"),
            ("ANYCON_RATE_DAEMON_BYTES_PER_MIN", "1000"),
        ] {
            let err = Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://x.test"), (var, bad)])).unwrap_err();
            assert!(err.to_string().contains(var), "{err}");
        }
    }

    // ---- ADR-37：SMTP / 发件人 / VAPID subject ----

    fn with(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut v = vec![("ANYCON_PUBLIC_URL", "https://hub.example.com")];
        v.extend_from_slice(extra);
        Config::from_vars(vars(&v))
    }

    #[test]
    fn email_is_off_by_default() {
        let c = with(&[]).unwrap();
        assert!(c.smtp.is_none() && c.email_from.is_none() && c.vapid_subject.is_none());
        assert!(c.push_allow_hosts.is_none() && c.mailer_override.is_none());
        assert!(c.unimplemented_vars.is_empty() && c.unknown_vars.is_empty());
    }

    #[test]
    fn smtp_url_schemes_ports_and_credentials() {
        let c = with(&[
            ("ANYCON_SMTP_URL", "smtp://bot%40example.com:p%40ss%3Aword%2F@smtp.example.com"),
            ("ANYCON_EMAIL_FROM", "Anycon <noreply@example.com>"),
        ])
        .unwrap();
        let smtp = c.smtp.unwrap();
        assert_eq!((smtp.security, smtp.host.as_str(), smtp.port), (SmtpSecurity::StartTls, "smtp.example.com", 587));
        assert_eq!(smtp.username.as_deref(), Some("bot@example.com"), "username is percent-decoded");
        assert_eq!(smtp.password.as_deref(), Some("p@ss:word/"), "password is percent-decoded");
        assert_eq!(c.email_from.unwrap().address, "noreply@example.com");

        let c =
            with(&[("ANYCON_SMTP_URL", "smtps://u:p@mail.example.com:2465"), ("ANYCON_EMAIL_FROM", "n@example.com")])
                .unwrap();
        let smtp = c.smtp.unwrap();
        assert_eq!((smtp.security, smtp.port), (SmtpSecurity::Tls, 2465));
        let c =
            with(&[("ANYCON_SMTP_URL", "smtps://mail.example.com"), ("ANYCON_EMAIL_FROM", "n@example.com")]).unwrap();
        assert_eq!(c.smtp.unwrap().port, 465, "smtps defaults to 465");
        let c = with(&[("ANYCON_SMTP_URL", "smtp+plain://127.0.0.1"), ("ANYCON_EMAIL_FROM", "n@example.com")]).unwrap();
        let smtp = c.smtp.unwrap();
        assert_eq!((smtp.security, smtp.port, smtp.username), (SmtpSecurity::Plain, 25, None));
    }

    #[test]
    fn smtp_url_errors_never_echo_the_url_or_the_password() {
        for bad in [
            "http://u:hunter2@mail.example.com",
            "smtp://u:hunter2@",
            "smtp://:hunter2@mail.example.com",
            "smtp://u:hunter2@mail.example.com/path",
            "smtp://u:hunter2@mail.example.com?x=1",
            "smtp+plain://u:hunter2@8.8.8.8:25",
            "smtp+plain://u:hunter2@[2001:4860:4860::8888]",
            "smtp+plain://u:hunter2@127%2E0%2E0%2E1:25",
            "smtp://u:%ff@mail.example.com",
            "not a url hunter2",
        ] {
            let err = with(&[("ANYCON_SMTP_URL", bad), ("ANYCON_EMAIL_FROM", "n@example.com")]).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_SMTP_URL", .. }), "{bad}: {msg}");
            assert!(!msg.contains("hunter2") && !msg.contains(bad), "{bad}: error leaks the URL: {msg}");
        }
    }

    #[test]
    fn smtp_plain_accepts_local_ip_literals_and_host_names_but_never_a_public_ip_literal() {
        let ok = |host: &str| {
            with(&[("ANYCON_SMTP_URL", &format!("smtp+plain://{host}:25")), ("ANYCON_EMAIL_FROM", "n@example.com")])
                .is_ok()
        };
        // IP 字面量：本机 / 内网才行（启动校验）
        for host in [
            "127.0.0.1",
            "127.1.2.3",
            "[::1]",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.10",
            "169.254.1.1",
            "100.64.0.1",
            "[fd00::1]",
            "[fe80::1]",
            "[::ffff:127.0.0.1]",
        ] {
            assert!(ok(host), "{host} should be allowed");
        }
        for host in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.0.1",
            "192.169.0.1",
            "100.128.0.1",
            "1.1.1.1",
            "[2001:4860:4860::8888]",
            "[2606:4700::1111]",
            "[::ffff:8.8.8.8]",
        ] {
            assert!(!ok(host), "{host} must be rejected");
        }
        // 主机名：启动时不判断它指向哪里（服务名启动时往往解析不了，DNS 也会变）；每次发信前解析，见 mail.rs。
        // 以前「不带点 = 内网服务名」的规则没有了：带点的、不带点的一视同仁。
        for host in [
            "localhost",
            "LOCALHOST",
            "mail.localhost",
            "mailpit",
            "postfix",
            "smtp.example.com",
            "mail.example.com.",
            "evil.localhost.example.com",
            "ai.",
            "my_mail-relay.internal",
        ] {
            assert!(ok(host), "{host} is a plausible host name, checked when mail is sent");
        }
        // 写法本身有问题的：libc 会当 IP 解析的纯数字写法、非法字符
        for host in [
            "134744072",
            "0x08080808",
            "127.1",
            "0x7f.0.0.1",
            "1.2.3",
            "127%2E0%2E0%2E1",
            "mail%2Eexample.com",
            "ma il",
            "-",
            "a..b",
        ] {
            assert!(!ok(host), "{host} must be rejected");
        }
    }

    #[test]
    fn check_plain_host_says_why() {
        assert_eq!(check_plain_host("8.8.8.8"), Err(PlainHostError::PublicAddress));
        assert_eq!(check_plain_host("[2001:4860:4860::8888]"), Err(PlainHostError::PublicAddress));
        assert_eq!(check_plain_host("134744072"), Err(PlainHostError::BadName));
        assert_eq!(check_plain_host("127.1"), Err(PlainHostError::BadName));
        assert_eq!(check_plain_host(""), Err(PlainHostError::BadName));
        assert_eq!(check_plain_host("[::1]"), Ok(()));
        assert_eq!(check_plain_host("Mailpit"), Ok(()));
        assert_eq!(check_plain_host("smtp.example.com"), Ok(()));
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("100.64.0.1".parse().unwrap()) && !is_private_ip("100.128.0.1".parse().unwrap()));
    }

    #[test]
    fn programmatic_configs_get_the_same_startup_check() {
        let mut c = Config::new("https://hub.test", std::path::Path::new("/nonexistent")).unwrap();
        c.email_from = Some(parse_email_from("n@example.com").unwrap());
        let plain = |host: &str| SmtpConfig {
            security: SmtpSecurity::Plain,
            host: host.into(),
            port: 25,
            username: Some("u".into()),
            password: Some("hunter2".into()),
        };
        c.smtp = Some(plain("8.8.8.8"));
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("smtp+plain") && !err.contains("hunter2"), "{err}");
        c.smtp = Some(plain("smtp.example.com"));
        assert!(c.validate().is_ok(), "a host name is checked per mail, not at startup");
        c.smtp = Some(plain("134744072"));
        assert!(c.validate().is_err());
    }

    #[test]
    fn email_limits_and_delays_have_the_documented_defaults() {
        // docs/spec/hub.md § 7.2 / § 8、ADR-37
        let l = Limits::default();
        assert_eq!(
            (l.email_send_per_hour_per_ip, l.email_verify_per_min_per_ip, l.email_code_max_attempts),
            (5, 20, 5)
        );
        assert_eq!((l.email_codes_active, l.email_codes_per_day, l.email_failures_per_day), (3, 10, 30));
        assert_eq!(l.email_resend_interval, Duration::from_secs(60));
        let t = Timing::default();
        assert_eq!(t.email_verify_failure_delay, Duration::from_millis(250));
        assert_eq!(t.email_code_ttl, Duration::from_secs(600));
    }

    #[test]
    fn smtp_without_a_sender_refuses_to_start() {
        let err = with(&[("ANYCON_SMTP_URL", "smtps://mail.example.com")]).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_EMAIL_FROM", .. }), "{err}");
        // 没配 SMTP 时发件人可有可无；但写了就要合法
        assert!(with(&[("ANYCON_EMAIL_FROM", "noreply@example.com")]).is_ok());
        for bad in ["not an address", "Anycon <>", "a@@b"] {
            let err = with(&[("ANYCON_SMTP_URL", "smtps://mail.example.com"), ("ANYCON_EMAIL_FROM", bad)]).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_EMAIL_FROM", .. }), "{bad}: {err}");
        }
        let c = with(&[("ANYCON_SMTP_URL", "smtps://mail.example.com"), ("ANYCON_EMAIL_FROM", "bare@example.com")])
            .unwrap();
        assert_eq!(c.email_from.unwrap().address, "bare@example.com");
    }

    #[test]
    fn debug_output_redacts_smtp_credentials() {
        let c = with(&[
            ("ANYCON_SMTP_URL", "smtps://mailuser:hunter2-smtp-password@mail.example.com"),
            ("ANYCON_EMAIL_FROM", "n@example.com"),
        ])
        .unwrap();
        let dump = format!("{c:?}");
        assert!(!dump.contains("hunter2") && !dump.contains("mailuser"), "{dump}");
        assert!(dump.contains("mail.example.com") && dump.contains("<redacted>"), "{dump}");
        let smtp_dump = format!("{:?}", c.smtp.unwrap());
        assert!(!smtp_dump.contains("hunter2") && !smtp_dump.contains("mailuser"), "{smtp_dump}");
    }

    #[test]
    fn vapid_subject_is_validated_and_has_sensible_defaults() {
        // 显式设置优先
        let c = with(&[("ANYCON_VAPID_SUBJECT", "mailto:admin@example.com")]).unwrap();
        assert_eq!(c.vapid_subject(), "mailto:admin@example.com");
        let c = with(&[("ANYCON_VAPID_SUBJECT", " https://example.org/contact ")]).unwrap();
        assert_eq!(c.vapid_subject(), "https://example.org/contact");
        for bad in ["admin@example.com", "mailto:", "mailto:nobody", "http://example.com", "ftp://x", "mailto:a b@c"] {
            let err = with(&[("ANYCON_VAPID_SUBJECT", bad)]).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_VAPID_SUBJECT", .. }), "{bad}: {err}");
        }
        // 没设：public_url 是 https 就用它
        assert_eq!(with(&[]).unwrap().vapid_subject(), "https://hub.example.com");
        // 不是 https：mailto: + 发件人地址
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "http://192.168.1.5:8080"),
            ("ANYCON_SMTP_URL", "smtps://mail.example.com"),
            ("ANYCON_EMAIL_FROM", "Anycon <noreply@example.com>"),
        ]))
        .unwrap();
        assert_eq!(c.vapid_subject(), "mailto:noreply@example.com");
        // 什么都没有：用主机名兜底，仍然是合法的 mailto:
        let c = Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", "http://192.168.1.5:8080")])).unwrap();
        assert_eq!(c.vapid_subject(), "mailto:noreply@192.168.1.5");
    }

    // ---- multi 模式：管理员邮箱、配额、OAuth ----

    /// 邮箱登录（SMTP）配齐的最小环境变量集合。
    const SMTP: [(&str, &str); 2] =
        [("ANYCON_SMTP_URL", "smtps://mail.example.com"), ("ANYCON_EMAIL_FROM", "n@example.com")];

    fn multi(extra: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let mut v = vec![("ANYCON_MODE", "multi")];
        v.extend_from_slice(&SMTP);
        v.extend_from_slice(extra);
        with(&v)
    }

    #[test]
    fn multi_gets_the_default_quotas_and_single_gets_none() {
        let c = multi(&[]).unwrap();
        assert_eq!(c.mode, Mode::Multi);
        assert_eq!(c.quota, Some(QuotaLimits { devices: 10, daemons: 3, connections: 10 }));
        assert_eq!(c.quota, Some(QuotaLimits::default()));
        // 每日配对上限只存在于 limits 里
        assert_eq!(c.limits.daily_pairings_per_account, Some(20));
        assert!(c.admin_emails.is_empty() && c.ignored_vars.is_empty() && c.unimplemented_vars.is_empty());

        let s = with(&[]).unwrap();
        assert_eq!(s.mode, Mode::Single);
        assert_eq!((s.quota, s.limits.daily_pairings_per_account), (None, None));
        assert!(s.admin_emails.is_empty() && s.oauth_github.is_none() && s.oauth_google.is_none());
    }

    #[test]
    fn quota_variables_are_positive_integers_and_only_apply_in_multi() {
        let c = multi(&[
            ("ANYCON_QUOTA_DEVICES", "25"),
            ("ANYCON_QUOTA_DAEMONS", " 7 "),
            ("ANYCON_QUOTA_CONNECTIONS", "40"),
            ("ANYCON_QUOTA_DAILY_PAIRINGS", "1"),
        ])
        .unwrap();
        assert_eq!(c.quota, Some(QuotaLimits { devices: 25, daemons: 7, connections: 40 }));
        assert_eq!(c.limits.daily_pairings_per_account, Some(1));
        assert!(c.unimplemented_vars.is_empty() && c.unknown_vars.is_empty() && c.ignored_vars.is_empty());
        // 只设一个，别的保持默认；空白 = 没设
        let c = multi(&[("ANYCON_QUOTA_DAEMONS", "9"), ("ANYCON_QUOTA_DEVICES", "  ")]).unwrap();
        assert_eq!(c.quota, Some(QuotaLimits { daemons: 9, ..QuotaLimits::default() }));

        for var in
            ["ANYCON_QUOTA_DEVICES", "ANYCON_QUOTA_DAEMONS", "ANYCON_QUOTA_CONNECTIONS", "ANYCON_QUOTA_DAILY_PAIRINGS"]
        {
            for bad in ["0", "-1", "1.5", "ten", "4294967296"] {
                let err = multi(&[(var, bad)]).unwrap_err();
                assert!(matches!(&err, ConfigError::Invalid { var: v, .. } if *v == var), "{var}={bad}: {err}");
                // single 里它们会被忽略，但写错了照样报错（切换模式时才炸太晚）
                assert!(with(&[(var, bad)]).is_err(), "{var}={bad} in single");
            }
        }

        // single：合法的值被忽略，并记下来给启动警告用；其它 quota_* 变量仍是「已识别、未实现」
        let c = with(&[
            ("ANYCON_QUOTA_DEVICES", "3"),
            ("ANYCON_QUOTA_DAILY_PAIRINGS", "5"),
            ("ANYCON_QUOTA_QUEUE_BYTES", "9"),
        ])
        .unwrap();
        assert_eq!((c.quota, c.limits.daily_pairings_per_account), (None, None));
        assert_eq!(c.ignored_vars, vec!["ANYCON_QUOTA_DEVICES", "ANYCON_QUOTA_DAILY_PAIRINGS"]);
        assert_eq!(c.unimplemented_vars, vec!["ANYCON_QUOTA_QUEUE_BYTES"]);
        assert_eq!(
            c.startup_warnings(),
            vec![
                "ANYCON_QUOTA_DEVICES is set but ignored in single mode",
                "ANYCON_QUOTA_DAILY_PAIRINGS is set but ignored in single mode"
            ]
        );
    }

    #[test]
    fn admin_emails_are_normalized_deduplicated_and_validated_without_echoing_them() {
        let c = multi(&[("ANYCON_ADMIN_EMAILS", " Boss@Example.com, ops@example.com ,,boss@example.com,")]).unwrap();
        assert_eq!(c.admin_emails, vec!["boss@example.com", "ops@example.com"]);
        assert!(c.is_admin_email("ops@example.com") && !c.is_admin_email("OPS@example.com"));
        assert!(!c.is_admin_email("other@example.com"));
        assert!(c.unimplemented_vars.is_empty() && c.ignored_vars.is_empty());
        // 空 = 没有
        assert!(multi(&[("ANYCON_ADMIN_EMAILS", "  ")]).unwrap().admin_emails.is_empty());
        // 有一项不合法就整体报错；错误只给序号，不回显那一项（多半是真实邮箱的笔误）
        for bad in
            ["boss@example.com, not-an-email", "typo@@example.com", "a b@example.com", "x@example.com;y@example.com"]
        {
            let err = multi(&[("ANYCON_ADMIN_EMAILS", bad)]).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_ADMIN_EMAILS", .. }), "{bad}: {msg}");
            assert!(msg.contains("entry #") && !msg.contains("typo") && !msg.contains("not-an-email"), "{bad}: {msg}");
            assert!(with(&[("ANYCON_ADMIN_EMAILS", bad)]).is_err(), "single validates too: {bad}");
        }
        // Debug 和日志只给个数
        let dump = format!("{c:?}");
        assert!(!dump.contains("boss@") && !dump.contains("ops@") && dump.contains("admin_emails: 2"), "{dump}");
        // single：解析了但忽略，启动时警告
        let c = with(&[("ANYCON_ADMIN_EMAILS", "boss@example.com")]).unwrap();
        assert_eq!(c.ignored_vars, vec!["ANYCON_ADMIN_EMAILS"]);
        assert_eq!(c.startup_warnings(), vec!["ANYCON_ADMIN_EMAILS is set but ignored in single mode"]);
    }

    #[test]
    fn the_admin_token_is_ignored_in_multi_with_a_warning() {
        let c = multi(&[("ANYCON_ADMIN_TOKEN", "super-secret-token-value")]).unwrap();
        assert_eq!(c.ignored_vars, vec!["ANYCON_ADMIN_TOKEN"]);
        assert_eq!(c.startup_warnings(), vec!["ANYCON_ADMIN_TOKEN is set but ignored in multi mode"]);
        assert!(!format!("{c:?}").contains("super-secret"));
        // single 里它是正常的
        let c = with(&[("ANYCON_ADMIN_TOKEN", "super-secret-token-value")]).unwrap();
        assert!(c.ignored_vars.is_empty() && c.startup_warnings().is_empty());
    }

    #[test]
    fn oauth_clients_need_both_halves_and_hide_the_secret() {
        let c = with(&[
            ("ANYCON_OAUTH_GITHUB_CLIENT_ID", "gh-id"),
            ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", "gh-secret-value"),
            ("ANYCON_OAUTH_GOOGLE_CLIENT_ID", " g-id.apps "),
            ("ANYCON_OAUTH_GOOGLE_CLIENT_SECRET", "g-secret-value"),
        ])
        .unwrap();
        assert_eq!(c.oauth_github.as_ref().unwrap().client_id, "gh-id");
        assert_eq!(c.oauth_google.as_ref().unwrap().client_id, "g-id.apps", "trimmed");
        assert_eq!(c.oauth_client(OAuthProvider::Github).unwrap().client_secret, "gh-secret-value");
        assert_eq!(c.oauth_providers().map(OAuthProvider::as_str).collect::<Vec<_>>(), ["github", "google"]);
        assert!(c.unimplemented_vars.is_empty() && c.unknown_vars.is_empty() && c.ignored_vars.is_empty());
        let dump = format!("{c:?}");
        assert!(!dump.contains("secret-value"), "{dump}");
        assert!(dump.contains("gh-id") && dump.contains("<redacted>"), "{dump}");
        assert!(!format!("{:?}", c.oauth_google.unwrap()).contains("secret-value"));

        // 只配一个提供方；没配的是 None；空白 = 没配
        let c = with(&[
            ("ANYCON_OAUTH_GOOGLE_CLIENT_ID", "g"),
            ("ANYCON_OAUTH_GOOGLE_CLIENT_SECRET", "s"),
            ("ANYCON_OAUTH_GITHUB_CLIENT_ID", ""),
            ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", "  "),
        ])
        .unwrap();
        assert!(c.oauth_github.is_none() && c.oauth_client(OAuthProvider::Github).is_none());
        assert_eq!(c.oauth_providers().collect::<Vec<_>>(), [OAuthProvider::Google]);

        // 两种模式都能配
        assert!(multi(&[("ANYCON_OAUTH_GITHUB_CLIENT_ID", "i"), ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", "s")])
            .unwrap()
            .oauth_github
            .is_some());

        // 只有一半：报错，指出缺的那个，不回显另一半
        for (id, secret, missing) in [
            (Some("the-id"), None, "ANYCON_OAUTH_GITHUB_CLIENT_SECRET"),
            (None, Some("the-secret"), "ANYCON_OAUTH_GITHUB_CLIENT_ID"),
        ] {
            let mut v = Vec::new();
            v.extend(id.map(|i| ("ANYCON_OAUTH_GITHUB_CLIENT_ID", i)));
            v.extend(secret.map(|s| ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", s)));
            let err = with(&v).unwrap_err();
            assert!(matches!(&err, ConfigError::Invalid { var, .. } if *var == missing), "{err}");
            assert!(!err.to_string().contains("the-secret"), "{err}");
        }
        let err = with(&[("ANYCON_OAUTH_GOOGLE_CLIENT_ID", "only-id")]).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_OAUTH_GOOGLE_CLIENT_SECRET", .. }), "{err}");

        // 值里有空白 / 控制字符（复制粘贴带进来的）：报错，不回显 secret
        for bad in ["a b", "a\nb", "a\tb"] {
            let err = with(&[("ANYCON_OAUTH_GITHUB_CLIENT_ID", "id"), ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", bad)])
                .unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_OAUTH_GITHUB_CLIENT_SECRET", .. }), "{err}");
            assert!(!err.to_string().contains(bad), "{err}");
        }
    }

    #[test]
    fn multi_without_any_way_to_sign_in_refuses_to_start() {
        let none = with(&[("ANYCON_MODE", "multi")]).unwrap_err();
        assert!(matches!(none, ConfigError::MultiNoSignIn), "{none}");
        assert!(none.to_string().contains("ANYCON_SMTP_URL") && none.to_string().contains("OAuth"), "{none}");
        // 邮箱登录或任一 OAuth 提供方都够
        assert!(multi(&[]).is_ok());
        for (id, secret) in [
            ("ANYCON_OAUTH_GITHUB_CLIENT_ID", "ANYCON_OAUTH_GITHUB_CLIENT_SECRET"),
            ("ANYCON_OAUTH_GOOGLE_CLIENT_ID", "ANYCON_OAUTH_GOOGLE_CLIENT_SECRET"),
        ] {
            assert!(with(&[("ANYCON_MODE", "multi"), (id, "i"), (secret, "s")]).is_ok(), "{id}");
        }
        // single 不需要（有 bootstrap token）
        assert!(with(&[]).is_ok());

        // 程序化构造的配置在 validate() 里过同一关；测试注入的发信实现也算
        let mut c = Config::new("https://hub.test", std::path::Path::new("/nonexistent")).unwrap();
        c.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
        assert!(matches!(c.validate(), Err(ConfigError::MultiNoSignIn)));
        c.mailer_override = Some(Arc::new(crate::mail::RecordingMailer::default()));
        assert!(c.validate().is_ok());
        c.mailer_override = None;
        c.oauth_google = Some(OAuthClient { client_id: "i".into(), client_secret: "s".into() });
        assert!(c.validate().is_ok());
    }

    #[test]
    fn oauth_over_plain_http_is_a_startup_warning_not_an_error() {
        let oauth = [("ANYCON_OAUTH_GITHUB_CLIENT_ID", "i"), ("ANYCON_OAUTH_GITHUB_CLIENT_SECRET", "s")];
        let http = |extra: &[(&str, &str)]| {
            let mut v = vec![("ANYCON_PUBLIC_URL", "http://localhost:8080")];
            v.extend_from_slice(extra);
            Config::from_vars(vars(&v)).unwrap()
        };
        let warnings = http(&oauth).startup_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("OAuth") && warnings[0].contains("https"), "{warnings:?}");
        // https、或没配 OAuth：不警告
        assert!(with(&oauth).unwrap().startup_warnings().is_empty());
        assert!(http(&[]).startup_warnings().is_empty());
    }

    #[test]
    fn oauth_endpoints_default_to_the_providers_and_tests_can_override_them() {
        let github = OAuthProvider::Github.default_endpoints();
        assert_eq!(github.authorize, "https://github.com/login/oauth/authorize");
        assert_eq!(github.token, "https://github.com/login/oauth/access_token");
        assert_eq!(github.userinfo, "https://api.github.com/user");
        assert_eq!(github.emails.as_deref(), Some("https://api.github.com/user/emails"));
        let google = OAuthProvider::Google.default_endpoints();
        assert_eq!(google.authorize, "https://accounts.google.com/o/oauth2/v2/auth");
        assert_eq!(google.token, "https://oauth2.googleapis.com/token");
        assert_eq!(google.userinfo, "https://openidconnect.googleapis.com/v1/userinfo");
        assert!(google.emails.is_none());
        for e in [&github, &google] {
            assert!([&e.authorize, &e.token, &e.userinfo].iter().all(|u| u.starts_with("https://")));
        }

        let mut c = with(&[]).unwrap();
        assert_eq!(c.oauth_endpoints(OAuthProvider::Github), github, "no override: production endpoints");
        let fake = OAuthEndpoints {
            authorize: "http://127.0.0.1:1/authorize".into(),
            token: "http://127.0.0.1:1/token".into(),
            userinfo: "http://127.0.0.1:1/user".into(),
            emails: Some("http://127.0.0.1:1/emails".into()),
        };
        c.oauth_endpoint_overrides.github = Some(fake.clone());
        assert_eq!(c.oauth_endpoints(OAuthProvider::Github), fake);
        assert_eq!(c.oauth_endpoints(OAuthProvider::Google), google, "the other provider is untouched");

        assert_eq!(OAuthProvider::parse("github"), Some(OAuthProvider::Github));
        assert_eq!(OAuthProvider::parse("google"), Some(OAuthProvider::Google));
        for bad in ["GitHub", "gitlab", ""] {
            assert_eq!(OAuthProvider::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn enable_multi_sets_the_quotas_and_the_one_daily_pairing_limit() {
        let mut c = Config::new("https://hub.test", std::path::Path::new("/nonexistent")).unwrap();
        assert_eq!((c.mode, c.quota, c.limits.daily_pairings_per_account), (Mode::Single, None, None));
        c.enable_multi(QuotaLimits { devices: 2, daemons: 1, connections: 3 }, 4);
        assert_eq!(c.mode, Mode::Multi);
        assert_eq!(c.quota.unwrap().daemons, 1);
        assert_eq!(c.limits.daily_pairings_per_account, Some(4));
    }

    // ---- ADR-39：预览 ----

    fn preview(url: &str) -> Result<PreviewUrl, ConfigError> {
        Ok(with(&[("ANYCON_PREVIEW_URL", url)])?.preview.expect("preview configured"))
    }

    fn preview_err(url: &str) -> String {
        let err = with(&[("ANYCON_PREVIEW_URL", url)]).expect_err(url);
        assert!(matches!(err, ConfigError::Invalid { var: "ANYCON_PREVIEW_URL", .. }), "{url}: {err}");
        err.to_string()
    }

    #[test]
    fn preview_is_off_by_default_and_the_variables_are_recognised() {
        let c = with(&[]).unwrap();
        assert!(c.preview.is_none());
        assert_eq!(c.limits.preview_bytes_per_min, 512 * 1024 * 1024);
        assert_eq!(c.limits.preview_max_connections, 16);
        assert_eq!(c.timing.preview_token_ttl, Duration::from_secs(24 * 3600));
        let c = with(&[
            ("ANYCON_PREVIEW_URL", "https://*.preview-example.net"),
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "1073741824"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "4"),
        ])
        .unwrap();
        assert!(c.unknown_vars.is_empty() && c.unimplemented_vars.is_empty());
        assert_eq!(c.limits.preview_bytes_per_min, 1024 * 1024 * 1024);
        assert_eq!(c.limits.preview_max_connections, 4);
        // 空串 = 没配
        assert!(with(&[("ANYCON_PREVIEW_URL", "  ")]).unwrap().preview.is_none());
    }

    #[test]
    fn preview_url_accepts_the_documented_forms() {
        let p = preview("https://*.preview-example.net").unwrap();
        assert_eq!((p.scheme, p.domain.as_str(), p.port), ("https", "preview-example.net", None));
        assert_eq!(p.origin_for("abc"), "https://abc.preview-example.net");
        assert_eq!(p.origin_template(), "https://{label}.preview-example.net");
        assert_eq!(p.frame_src(), "https://*.preview-example.net");
        // 默认端口归一成「没写」
        assert_eq!(preview("https://*.preview-example.net:443").unwrap().port, None);
        // 非默认端口保留
        let p = preview("https://*.preview-example.net:8443").unwrap();
        assert_eq!(p.port, Some(8443));
        assert_eq!(p.origin_template(), "https://{label}.preview-example.net:8443");
        // 大小写、末尾的 /
        assert_eq!(preview(" https://*.Preview-Example.NET/ ").unwrap().domain, "preview-example.net");
        // 二级泛域名也行（只是公共 hub 不用，见 deploy.md 的 Cloudflare 说明）
        assert_eq!(preview("https://*.preview.preview-example.net").unwrap().domain, "preview.preview-example.net");
        // 公共 hub 自己的配置：hub 在 anycon.ai，预览在 aiplace.app
        let c = Config::from_vars(vars(&[
            ("ANYCON_PUBLIC_URL", "https://my-hub.anycon.ai"),
            ("ANYCON_PREVIEW_URL", "https://*.aiplace.app"),
        ]))
        .unwrap();
        assert_eq!(c.preview.unwrap().origin_template(), "https://{label}.aiplace.app");
    }

    #[test]
    fn preview_url_http_is_only_for_localhost() {
        let p = preview("http://*.preview.localhost:8080").unwrap();
        assert_eq!((p.scheme, p.domain.as_str(), p.port), ("http", "preview.localhost", Some(8080)));
        assert_eq!(p.origin_for("x"), "http://x.preview.localhost:8080");
        assert_eq!(preview("http://*.localhost").unwrap().domain, "localhost");
        assert_eq!(preview("https://*.localhost").unwrap().scheme, "https");
        for bad in ["http://*.preview-example.net", "http://*.preview.localhost.example.net", "http://*.notlocalhost"] {
            assert!(preview_err(bad).contains("must be https"), "{bad}");
        }
    }

    #[test]
    fn preview_url_rejects_malformed_templates() {
        for (bad, why) in [
            ("https://preview-example.net", "wildcard"),
            ("https://*preview-example.net", "wildcard"),
            ("https://a.*.preview-example.net", "wildcard"),
            ("https://*.*.preview-example.net", "wildcard"),
            ("https://*", "wildcard"),
            ("https://*.", "not a valid URL"),
            ("*.preview-example.net", "expected https"),
            ("ftp://*.preview-example.net", "expected https"),
            ("https://*.preview-example.net/path", "path"),
            ("https://*.preview-example.net/?x=1", "path"),
            ("https://*.preview-example.net/#f", "path"),
            ("https://u:p@*.preview-example.net", "wildcard"),
            ("https://*.u:p@preview-example.net", "credentials"),
            ("https://*.preview-example.net:0", "port"),
            ("https://*.preview-example.net:99999", "not a valid URL"),
            ("https://*.127.0.0.1", "DNS name"),
            ("https://*.[::1]", "DNS name"),
            ("https://*.com", "public suffix"),
            ("https://*.co.uk", "public suffix"),
            ("https://*.intranet", "public suffix"),
        ] {
            let msg = preview_err(bad);
            assert!(msg.contains(why), "{bad}: expected {why:?} in {msg}");
        }
    }

    #[test]
    fn preview_domain_must_not_share_a_registrable_domain_with_the_hub() {
        let hub_and_preview = |hub: &str, preview: &str| {
            Config::from_vars(vars(&[("ANYCON_PUBLIC_URL", hub), ("ANYCON_PREVIEW_URL", preview)]))
        };
        for (hub, preview) in [
            ("https://hub.example.com", "https://*.preview.example.com"),
            ("https://hub.example.com", "https://*.example.com"),
            ("https://example.com", "https://*.preview.example.com"),
            ("https://hub.example.co.uk", "https://*.preview.example.co.uk"),
            ("https://a.b.example.com:8443", "https://*.c.example.com"),
        ] {
            let err = hub_and_preview(hub, preview).expect_err(preview).to_string();
            assert!(err.contains("same registrable domain") && err.contains("cookies"), "{hub} vs {preview}: {err}");
        }
        // 不同的可注册域名：可以。PSL 的私有部分也算（github.io 下每个用户是一个独立的站点）
        for (hub, preview) in [
            ("https://hub.example.com", "https://*.example.net"),
            ("https://hub.example.co.uk", "https://*.example.org.uk"),
            ("https://one.github.io", "https://*.two.github.io"),
            ("https://hub.example.com", "https://*.preview.localhost"),
        ] {
            assert!(hub_and_preview(hub, preview).is_ok(), "{hub} vs {preview}");
        }
        // ANYCON_PUBLIC_URL 是 IP 字面量或 localhost 一类：不比
        for hub in ["http://192.168.1.10:8080", "http://[::1]:8080", "http://localhost:8080", "http://hub.localhost"] {
            assert!(hub_and_preview(hub, "http://*.preview.localhost:8080").is_ok(), "{hub}");
            assert!(hub_and_preview(hub, "https://*.preview-example.net").is_ok(), "{hub}");
        }
        // 程序化构造的配置在 validate() 里过同一关
        let mut c = Config::new("https://hub.example.com", std::path::Path::new("/nonexistent")).unwrap();
        c.preview = Some(PreviewUrl { scheme: "https", domain: "preview.example.com".into(), port: None });
        assert!(c.validate().unwrap_err().to_string().contains("same registrable domain"));
        c.preview = Some(PreviewUrl { scheme: "https", domain: "example.net".into(), port: None });
        assert!(c.validate().is_ok());
    }

    #[test]
    fn preview_numbers_are_validated() {
        for (var, ok) in [
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "205824"),
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "536870912"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "1"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "100"),
        ] {
            assert!(with(&[(var, ok)]).is_ok(), "{var}={ok}");
        }
        for (var, bad) in [
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "205823"),
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "0"),
            ("ANYCON_PREVIEW_BYTES_PER_MIN", "512MiB"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "0"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "-1"),
            ("ANYCON_PREVIEW_MAX_CONNECTIONS", "many"),
        ] {
            let err = with(&[(var, bad)]).unwrap_err();
            assert!(matches!(err, ConfigError::Invalid { var: v, .. } if v == var), "{var}={bad}: {err}");
        }
    }

    #[test]
    fn preview_host_matching_follows_the_configured_port() {
        let label = "a".repeat(26);
        let on = |p: &PreviewUrl, host: &str| p.match_host(host);

        // 没配端口：不带或带默认端口（https 是 443）都行
        let p = preview("https://*.preview-example.net").unwrap();
        for host in [
            format!("{label}.preview-example.net"),
            format!("{label}.preview-example.net:443"),
            format!("{label}.PREVIEW-example.net"),
            format!("{label}.preview-example.net."),
        ] {
            assert_eq!(on(&p, &host), HostMatch::Label(label.clone()), "{host}");
        }
        for host in [
            format!("{label}.preview-example.net:8443"),
            format!("{label}.preview-example.net:80"),
            format!("{label}.preview-example.net:"),
            format!("{label}.preview-example.net:abc"),
            "preview-example.net".to_string(),
            format!("{label}.preview-example.org"),
            format!("{label}.xpreview-example.net"),
            format!("{label}preview-example.net"),
            "hub.example.com".to_string(),
            "127.0.0.1:8080".to_string(),
            "[::1]:8080".to_string(),
            String::new(),
        ] {
            assert_eq!(on(&p, &host), HostMatch::Other, "{host:?}");
        }
        // 域名下但 label 不合法
        for host in [
            "www.preview-example.net".to_string(),
            format!("{}.preview-example.net", "A1".repeat(13)),
            format!("{}.preview-example.net", "a".repeat(25)),
            format!("{}.preview-example.net", "a".repeat(27)),
            format!("x.{label}.preview-example.net"),
            format!("{label}.{label}.preview-example.net"),
            ".preview-example.net".to_string(),
        ] {
            assert_eq!(on(&p, &host), HostMatch::InvalidLabel, "{host}");
        }

        // 配了端口：必须带这个端口
        let p = preview("http://*.preview.localhost:8080").unwrap();
        assert_eq!(on(&p, &format!("{label}.preview.localhost:8080")), HostMatch::Label(label.clone()));
        assert_eq!(on(&p, &format!("{label}.preview.localhost")), HostMatch::Other);
        assert_eq!(on(&p, &format!("{label}.preview.localhost:80")), HostMatch::Other);
        assert_eq!(on(&p, &format!("{label}.preview.localhost:9090")), HostMatch::Other);
        // http 的默认端口 80 可省
        let p = preview("http://*.preview.localhost").unwrap();
        assert_eq!(on(&p, &format!("{label}.preview.localhost")), HostMatch::Label(label.clone()));
        assert_eq!(on(&p, &format!("{label}.preview.localhost:80")), HostMatch::Label(label));
    }
}
