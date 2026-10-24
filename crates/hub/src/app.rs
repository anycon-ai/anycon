//! 全局共享状态 [`AppState`] 与跨 HTTP / WebSocket 的业务操作（吊销设备、审计）。

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anycon_protocol::{close_code, Control, NotifyCategory};
use tokio_util::sync::CancellationToken;

use crate::accounts::LoginMethodLocks;
use crate::config::Config;
use crate::enroll::Enrolls;
use crate::mail::Mailer;
use crate::oauth::OAuthRuntime;
use crate::pairing::Broker;
use crate::push::PushService;
use crate::ratelimit::{ByteBudgets, KeyedLimiter, RateLimiter, WindowCounter};
use crate::store::{NewAudit, Store};
use crate::util::now_ms;
use crate::ws::registry::Registry;

/// 邮箱验证码「每天」的窗口：滚动的 24 小时。
const EMAIL_DAILY_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// 每日配对与接入次数的窗口：滚动的 24 小时。
const PAIRING_DAILY_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// 按 IP 的限速器（见 docs/spec/hub.md § 7.2）。
pub struct IpLimiters {
    pub auth_token: RateLimiter,
    pub ws_upgrade: RateLimiter,
    pub pair_join: RateLimiter,
    pub invite: RateLimiter,
    /// `enroll_begin`：每 IP 每分钟（账号内接入，docs/spec/hub.md § 4.1）。
    pub enroll_begin: RateLimiter,
    /// `POST /auth/email/send`：每 IP 每小时。
    pub email_send: RateLimiter,
    /// `POST /api/me/email`：每 IP 每小时。
    pub bind_send: RateLimiter,
    /// `POST /auth/email/verify`：每 IP 每分钟。
    pub email_verify: RateLimiter,
    /// `POST /api/me/email/verify`：每 IP 每分钟。
    pub bind_verify: RateLimiter,
    /// 预览域名上的 WebSocket 升级：每 IP 每分钟（与主域名的 `ws_upgrade` 各算各的，docs/spec/hub.md § 13.4）。
    pub preview_ws_upgrade: RateLimiter,
}

/// 按账号 / 邮箱 / 设备的限速（键是账号 id、`<用途>:<邮箱>` 或设备 id）。
pub struct AccountLimiters {
    /// `enroll_begin`：每账号每小时。
    pub enroll_begin: KeyedLimiter<String>,
    /// 每日配对与接入次数（docs/spec/hub.md § 7.1）：每个成功的撮合恰好记一次。`pair_begin` / `enroll_begin` 在已知账号时检查并各记一次
    /// （[`crate::quota::record_pairing_attempt`]）；`pair_join` 先检查（[`crate::quota::pairing_quota_left`]），host 的登记没记过账
    /// （host 没有账号）时在撮合成立之前记在加入方的账号上（[`crate::pairing::Broker::join`]）。
    /// 严格的滚动 24 小时窗口，上限是 `limits.daily_pairings_per_account`。`None` = 不限（`single` 默认）。
    pub daily_pairings: Option<WindowCounter<String>>,
    /// `POST /api/push/subscriptions`、`POST /api/push/test`：每账号每分钟。
    pub push_api: KeyedLimiter<String>,
    /// 同一个邮箱两次发码之间的最短间隔。登录码的键是 `login:<邮箱>`（对所有请求者共用），
    /// 绑定码的键是 `bind:<账号>:<邮箱>`（别的账号对同一个邮箱要码，占不掉你的窗口）。
    pub email_resend: KeyedLimiter<String>,
    /// 每个邮箱（键同上）每天最多发几个码，不管请求来自哪个 IP。对所有邮箱一视同仁，不看有没有绑定账号。
    pub email_daily_codes: WindowCounter<String>,
    /// 每个邮箱（键同上）每天累计验证失败几次，满了当天不再接受这个邮箱的码。只记真的猜过某条码的失败。
    pub email_daily_failures: WindowCounter<String>,
    /// 每台收件设备触发通知判定的速率（键是设备 id）。
    pub notify_device: KeyedLimiter<String>,
    /// 同一账号同一类别的邮件通知去重（10 分钟一封）。
    pub notify_email: KeyedLimiter<(String, NotifyCategory)>,
    /// `POST /api/preview/tokens`：每台设备每分钟（键是设备 id）。
    pub preview_tokens: KeyedLimiter<String>,
}

/// 后台的「发了就不管」任务（发验证码邮件、发推送、发通知邮件）。HTTP 响应和信封转发都不等它们；
/// 计数只为了让测试能等到它们做完（[`Background::idle`]）。
#[derive(Default)]
pub struct Background {
    pending: Arc<AtomicUsize>,
}

impl Background {
    pub fn spawn(&self, fut: impl Future<Output = ()> + Send + 'static) {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.pending.fetch_add(1, Ordering::SeqCst);
        let guard = Guard(self.pending.clone());
        tokio::spawn(async move {
            let _guard = guard;
            fut.await;
        });
    }

    /// 没有在途的后台任务了（测试用；生产代码不需要等）。
    pub async fn idle(&self) {
        while self.pending.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// 管理员 bootstrap token（来自 `ANYCON_ADMIN_TOKEN` 或 `<data>/admin-token`，不在库里）的哈希与对应账号。
/// 只有 `single` 有（[`AppState::admin`]）。
pub struct AdminAuth {
    pub token_hash: String,
    pub account_id: String,
}

pub struct AppState {
    pub config: Config,
    pub store: Arc<dyn Store>,
    pub registry: Registry,
    pub broker: Broker,
    /// 账号内接入的会话（内存，hub 重启即作废）。
    pub enrolls: Enrolls,
    pub limits: IpLimiters,
    pub account_limits: AccountLimiters,
    /// bootstrap 管理员的 token 与账号。`single` 才有；`multi` 为 `None`（没有 bootstrap 账号，也没有 token 登录）。
    pub admin: Option<AdminAuth>,
    /// 账号级的预览流量预算（键是账号 id，docs/spec/hub.md § 13.5）：预览连接和节点之间的 `data` / `handshake`
    /// 从这里扣，不占发送连接自己的字节预算。
    pub preview_budget: ByteBudgets<String>,
    pub started_at_ms: u64,
    pub shutdown: CancellationToken,
    /// Web Push 发送（VAPID 密钥、endpoint 白名单、HTTP 客户端）。
    pub push: PushService,
    /// 发信。`None` = 没配 SMTP：没有邮箱登录、绑定和邮件通知。
    pub mailer: Option<Arc<dyn Mailer>>,
    /// OAuth 登录（GitHub、Google）：出站 HTTP 客户端、`start` 的限速。
    pub oauth: OAuthRuntime,
    /// 移除登录方式（解绑邮箱、解除 OAuth 绑定、删登录 token）的按账号串行化（[`crate::accounts::begin_login_method_removal`]）。
    pub login_method_locks: LoginMethodLocks,
    pub bg: Background,
}

impl AppState {
    pub fn new(
        config: Config,
        store: Arc<dyn Store>,
        admin: Option<AdminAuth>,
        push: PushService,
        mailer: Option<Arc<dyn Mailer>>,
    ) -> Arc<Self> {
        let l = &config.limits;
        let limits = IpLimiters {
            auth_token: RateLimiter::per_minute(l.auth_token_per_min),
            ws_upgrade: RateLimiter::per_minute(l.ws_upgrade_per_min),
            pair_join: RateLimiter::per_minute(l.pair_join_per_min),
            invite: RateLimiter::per_minute(l.invite_per_min),
            enroll_begin: RateLimiter::per_minute(l.enroll_begin_per_min_per_ip),
            email_send: RateLimiter::per_hour(l.email_send_per_hour_per_ip),
            bind_send: RateLimiter::per_hour(l.email_send_per_hour_per_ip),
            email_verify: RateLimiter::per_minute(l.email_verify_per_min_per_ip),
            bind_verify: RateLimiter::per_minute(l.email_verify_per_min_per_ip),
            preview_ws_upgrade: RateLimiter::per_minute(l.preview_ws_upgrade_per_min),
        };
        let minute = Duration::from_secs(60);
        let account_limits = AccountLimiters {
            enroll_begin: KeyedLimiter::per_hour(l.enroll_begin_per_hour_per_account),
            daily_pairings: l.daily_pairings_per_account.map(|n| WindowCounter::new(n, PAIRING_DAILY_WINDOW)),
            push_api: KeyedLimiter::new(l.push_api_per_min_per_account, minute),
            email_resend: KeyedLimiter::new(1, l.email_resend_interval),
            email_daily_codes: WindowCounter::new(l.email_codes_per_day, EMAIL_DAILY_WINDOW),
            email_daily_failures: WindowCounter::new(l.email_failures_per_day, EMAIL_DAILY_WINDOW),
            notify_device: KeyedLimiter::new(l.notify_per_min_per_device, minute),
            notify_email: KeyedLimiter::new(1, config.timing.notify_email_dedupe),
            preview_tokens: KeyedLimiter::new(l.preview_tokens_per_min_per_device, minute),
        };
        let preview_budget = ByteBudgets::per_minute(l.preview_bytes_per_min);
        let broker = Broker::new(config.timing.pair_ttl, config.timing.pair_confirm_timeout);
        let enrolls = Enrolls::new(config.timing.enroll_ttl, l.enrolls_per_daemon);
        let oauth = OAuthRuntime::new(config.timing.oauth_timeout);
        Arc::new(Self {
            config,
            store,
            registry: Registry::default(),
            broker,
            enrolls,
            limits,
            account_limits,
            admin,
            preview_budget,
            started_at_ms: now_ms(),
            shutdown: CancellationToken::new(),
            push,
            mailer,
            oauth,
            login_method_locks: LoginMethodLocks::default(),
            bg: Background::default(),
        })
    }

    /// 邮箱功能（验证码登录、绑定、邮件通知）是否可用：配了 SMTP（或测试注入了发信实现）。
    pub fn email_enabled(&self) -> bool {
        self.mailer.is_some()
    }

    /// 写一条 hub 审计。失败只记日志，不影响主流程。
    pub async fn audit(&self, entry: NewAudit) {
        let event = entry.event;
        if let Err(e) = self.store.insert_audit(entry, now_ms()).await {
            tracing::warn!(event, error = %e, "failed to write audit entry");
        }
    }

    /// 吊销设备：`DELETE /api/devices/:id` 和 `Control::Revoke` 共用这一条路径。
    ///
    /// 库里置 `revoked_at` 并删配对；在线则以 4004 断开；告诉原来的对端它下线了。
    /// 返回 `false` 表示设备不存在或早已吊销。
    pub async fn revoke_device(&self, device_id: &str, actor: Option<&str>) -> Result<bool, crate::store::StoreError> {
        let Some(device) = self.store.get_device(device_id).await? else { return Ok(false) };
        if device.revoked_at_ms.is_some() {
            return Ok(false);
        }
        let former_peers = self.store.revoke_device(device_id, now_ms()).await?;

        // 进行中的配对和账号内接入一并作废。
        self.broker.abort_device(device_id);
        self.enrolls.abort_device(device_id);

        // 原来的对端：移除缓存里的对端、通知它该设备已离线（配对已删，之后不会再有 peer_offline）。
        for peer in &former_peers {
            if let Some(c) = self.registry.get(peer) {
                c.remove_peer(device_id);
                c.send_control(&Control::PeerOffline { device: device_id.to_string() });
            }
        }
        // 被吊销设备自己的连接：清对端集合（这样它收尾时不会再扇出 peer_offline）、移出路由表、4004 断开。
        if let Some(conn) = self.registry.get(device_id) {
            conn.clear_peers();
            self.registry.unregister(device_id, conn.id);
            conn.close(close_code::REVOKED, "revoked");
        }
        // 预览（docs/spec/hub.md § 13.6）：库里已经删了它签发的令牌（它是节点的话还有全部 label 和令牌），
        // 这里关掉用这些令牌的预览连接。
        self.registry.close_previews_of(device_id, close_code::REVOKED, "revoked");
        tracing::info!(device = %device_id, "device revoked");
        self.audit(NewAudit {
            event: "device_revoked",
            account_id: Some(device.account_id.clone()),
            device_id: Some(device_id.to_string()),
            ip_prefix: None,
            user_agent: None,
            detail: Some(serde_json::json!({ "by_device": actor })),
        })
        .await;
        // 设备数（和被吊销设备自己的连接）变了：告诉账号其余在线的永久连接（`multi`）。
        crate::quota::broadcast(self, &device.account_id, None).await;
        Ok(true)
    }

    /// 优雅关停的网络部分：向所有连接发 4002，等它们收尾（最多 `grace`）。
    pub async fn close_all_connections(&self, grace: Duration) {
        self.registry.close_all(close_code::RESTARTING, "restarting");
        let deadline = tokio::time::Instant::now() + grace;
        while !self.registry.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
