//! anycon-hub：账号、设备与路由、配对与账号内接入撮合、通知转发、静态文件。
//!
//! 一条判据（docs/spec/hub.md § 1）：这个功能需要看消息内容吗？需要，就不属于 hub。
//! 所以这里没有任何会话 / 项目 / 消息的概念，转发路径上只看信封头，不记录、不存储 `body`。
//! 唯一的例外是 `notify` 信封：目标设备离线时，后台任务（[`notify`]，不阻塞转发）会读 `body` 的**明文外层**——
//! 通知类别和不透明的会话 id，用来决定要不要推送 / 发邮件。外层之内的 `detail`（Noise 密文）一个字节都不碰，
//! 其余种类的信封根本不看 `body`。
//!
//! 入口：[`run`] 起一个完整的 hub（测试用它起真实服务），二进制 `anycon-hub` 只是对它的薄封装。

pub mod app;
pub mod bootstrap;
pub mod clientip;
pub mod config;
pub mod enroll;
pub mod error;
pub mod http;
pub mod mail;
pub mod notify;
pub mod pairing;
pub mod push;
pub mod ratelimit;
pub mod store;
pub mod util;
pub mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing_subscriber::EnvFilter;

pub use app::AppState;
pub use config::Config;
pub use store::{SqliteStore, Store};

use crate::util::now_ms;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(transparent)]
    Bootstrap(#[from] bootstrap::BootstrapError),
    #[error(transparent)]
    Store(#[from] store::StoreError),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
    #[error(transparent)]
    Vapid(#[from] push::VapidError),
    #[error(transparent)]
    Push(#[from] push::PushSetupError),
    #[error("cannot set up email delivery (ANYCON_SMTP_URL): {0}")]
    Mail(#[from] mail::MailError),
}

/// 运行中的 hub。
pub struct Handle {
    /// 实际监听地址（配置端口为 0 时是系统分配的端口）。
    pub addr: SocketAddr,
    /// 共享状态，测试可以直接访问 `store` 等。
    pub state: Arc<AppState>,
    /// 首次启动自动生成的 admin token 明文（只有这一次拿得到；之后只在 `<data>/admin-token` 里）。
    pub generated_admin_token: Option<String>,
    /// `<data>/admin-token`。
    pub admin_token_file: PathBuf,
    server: JoinHandle<()>,
    background: JoinHandle<()>,
}

impl Handle {
    pub fn store(&self) -> Arc<dyn Store> {
        self.state.store.clone()
    }

    /// `http://127.0.0.1:<port>`，给本机客户端 / 测试用。
    pub fn base_url(&self) -> String {
        let ip = if self.addr.ip().is_unspecified() { "127.0.0.1".to_string() } else { self.addr.ip().to_string() };
        format!("http://{}:{}", ip, self.addr.port())
    }

    /// 优雅关停：停止接受新连接 → 向所有 WS 发 `4002 restarting` → 最多等 `shutdown_grace` → 返回。
    pub async fn shutdown(self) {
        let grace = self.state.config.timing.shutdown_grace;
        self.state.shutdown.cancel();
        self.state.close_all_connections(grace).await;
        self.background.abort();
        let mut server = self.server;
        if tokio::time::timeout(Duration::from_secs(1), &mut server).await.is_err() {
            server.abort();
        }
    }

    /// 等服务任务结束（正常只会在 shutdown 后发生）。
    pub async fn wait(&mut self) {
        let _ = (&mut self.server).await;
    }
}

/// 初始化 tracing。过滤表达式优先级：`ANYCON_LOG` > `RUST_LOG` > `info`。
pub fn init_tracing(filter: &str) {
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    // 输出不是终端（Docker / systemd 日志）时不带 ANSI 转义。
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_ansi(ansi).try_init();
}

/// 起一个完整的 hub：校验数据目录 → 占端口 → 打开库并迁移 → admin 账号 / token → 开始服务。
pub async fn run(config: Config) -> Result<Handle, RunError> {
    config.validate()?;
    bootstrap::prepare_data_dir(&config.data_dir)?;
    // 先占端口再动状态：端口被占时启动失败，不应该已经消耗掉「首次启动」（生成 admin token）。
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .map_err(|source| RunError::Bind { addr: config.listen, source })?;
    let addr = listener.local_addr().map_err(|source| RunError::Bind { addr: config.listen, source })?;

    // Web Push 的 VAPID 密钥：首次生成，坏了报错（不重建，否则所有订阅失效）。放在动数据库和 admin token 之前：
    // 启动失败不该已经消耗掉「首次启动」。
    let vapid = push::VapidKeys::load_or_create(&config.data_dir)?;
    let push = push::PushService::new(
        vapid,
        config.vapid_subject(),
        config.push_allow_hosts.clone(),
        config.timing.push_timeout,
    )?;
    let mailer = build_mailer(&config)?;
    let store: Arc<dyn Store> = Arc::new(SqliteStore::open(config.database_url.as_deref(), &config.data_dir).await?);
    let boot = bootstrap::bootstrap_admin(store.as_ref(), &config.data_dir, config.admin_token.as_deref()).await?;

    if config.public_url.scheme() == "http" {
        tracing::warn!(
            public_url = %config.public_url_str(),
            "ANYCON_PUBLIC_URL is plain http: logins and pairing links are not encrypted in transit. Fine for local development, put TLS in front for anything else"
        );
    }
    // 限速按真实 IP 计，cookie 的 Secure 也参考 X-Forwarded-Proto：只有落在受信代理网段的直连对端发来的
    // X-Forwarded-* 才会被采信。把现状打出来，部署出错时一眼能看到。
    let proxies = config.trusted_proxies.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
    tracing::info!(trusted_proxies = %proxies, "trusting X-Forwarded-For / X-Forwarded-Proto only from these networks (ANYCON_TRUSTED_PROXIES)");
    if config.public_url.scheme() == "https" && config.trusted_proxies.is_empty() {
        tracing::warn!(
            "ANYCON_PUBLIC_URL is https but ANYCON_TRUSTED_PROXIES is empty: behind a reverse proxy every client shares the proxy's IP and its rate-limit buckets"
        );
    }
    if config.max_envelope_bytes < config::MIN_ENVELOPE_BYTES {
        // 环境变量路径在 `Config::from_vars` 里已经拒绝；程序化构造的配置（测试）走到这里只警告。
        tracing::warn!(
            max_envelope_bytes = config.max_envelope_bytes,
            min = config::MIN_ENVELOPE_BYTES,
            "max_envelope_bytes is below one full fragment envelope: every full-size fragment will be dropped as too_large"
        );
    }
    for name in &config.unimplemented_vars {
        tracing::warn!(var = %name, "{name} is set but not implemented in v0.1, ignored");
    }
    for name in &config.unknown_vars {
        tracing::debug!(var = %name, "unrecognised ANYCON_* variable ignored by the hub");
    }
    if config.admin_token.as_deref().is_some_and(|t| t.len() < 16) {
        tracing::warn!("ANYCON_ADMIN_TOKEN is shorter than 16 characters; use a long random value");
    }

    match &config.preview {
        Some(preview) => tracing::info!(
            origin_template = %preview.origin_template(),
            bytes_per_min = config.limits.preview_bytes_per_min,
            max_connections = config.limits.preview_max_connections,
            "dev server preview is enabled (ANYCON_PREVIEW_URL)"
        ),
        None => tracing::info!("preview disabled (ANYCON_PREVIEW_URL is not set)"),
    }

    match &config.smtp {
        Some(smtp) => tracing::info!(
            host = %smtp.host,
            port = smtp.port,
            security = ?smtp.security,
            "email is enabled: login codes and notification emails go through ANYCON_SMTP_URL"
        ),
        None if mailer.is_some() => tracing::info!("email is enabled (custom mailer)"),
        None => tracing::info!("ANYCON_SMTP_URL is not set: no email login, no email notifications"),
    }

    let state = AppState::new(config, store, boot.admin, push, mailer);
    let app = http::router(state.clone());

    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move {
        let serve = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move { shutdown.cancelled().await });
        if let Err(e) = serve.await {
            tracing::error!(error = %e, "http server stopped with error");
        }
    });
    let background = tokio::spawn(background_tasks(state.clone()));

    Ok(Handle {
        addr,
        state,
        generated_admin_token: boot.generated_token,
        admin_token_file: boot.token_file,
        server,
        background,
    })
}

/// 发信实现：测试注入的优先，其次按 `ANYCON_SMTP_URL` 建 SMTP；都没有就没有邮件功能。
fn build_mailer(config: &Config) -> Result<Option<Arc<dyn mail::Mailer>>, mail::MailError> {
    if let Some(m) = &config.mailer_override {
        return Ok(Some(m.clone()));
    }
    let (Some(smtp), Some(from)) = (&config.smtp, &config.email_from) else { return Ok(None) };
    Ok(Some(Arc::new(mail::SmtpMailer::new(smtp, from.mailbox.clone())?)))
}

/// 定时清理：过期的配对登记 / 撮合和接入会话（每秒）、限速桶、过期会话和过期的预览令牌（每分钟）、90 天前的审计（每小时）。
async fn background_tasks(state: Arc<AppState>) {
    const AUDIT_RETENTION_MS: u64 = 90 * 24 * 3600 * 1000;
    let skip = tokio::time::MissedTickBehavior::Skip;
    let mut every_sec = tokio::time::interval(Duration::from_secs(1));
    let mut every_min = tokio::time::interval(Duration::from_secs(60));
    let mut every_hour = tokio::time::interval(Duration::from_secs(3600));
    every_sec.set_missed_tick_behavior(skip);
    every_min.set_missed_tick_behavior(skip);
    every_hour.set_missed_tick_behavior(skip);
    loop {
        tokio::select! {
            _ = every_sec.tick() => {
                state.broker.sweep();
                state.enrolls.sweep();
            }
            _ = every_min.tick() => {
                state.limits.auth_token.sweep();
                state.limits.ws_upgrade.sweep();
                state.limits.pair_join.sweep();
                state.limits.invite.sweep();
                state.limits.enroll_begin.sweep();
                state.limits.email_send.sweep();
                state.limits.bind_send.sweep();
                state.limits.email_verify.sweep();
                state.limits.bind_verify.sweep();
                state.limits.preview_ws_upgrade.sweep();
                state.account_limits.enroll_begin.sweep();
                state.account_limits.push_api.sweep();
                state.account_limits.email_resend.sweep();
                state.account_limits.email_daily_codes.sweep();
                state.account_limits.email_daily_failures.sweep();
                state.account_limits.notify_device.sweep();
                state.account_limits.notify_email.sweep();
                state.account_limits.preview_tokens.sweep();
                state.preview_budget.sweep();
                if let Some(q) = &state.account_limits.daily_pairings {
                    q.sweep();
                }
                if let Err(e) = state.store.purge_expired_web_sessions(now_ms()).await {
                    tracing::warn!(error = %e, "failed to purge expired sessions");
                }
                if let Err(e) = state.store.purge_expired_email_codes(now_ms()).await {
                    tracing::warn!(error = %e, "failed to purge expired email codes");
                }
                if let Err(e) = state.store.purge_expired_preview_tokens(now_ms()).await {
                    tracing::warn!(error = %e, "failed to purge expired preview tokens");
                }
            }
            _ = every_hour.tick() => {
                if let Err(e) = state.store.purge_audit_before(now_ms().saturating_sub(AUDIT_RETENTION_MS)).await {
                    tracing::warn!(error = %e, "failed to purge old audit entries");
                }
            }
        }
    }
}
