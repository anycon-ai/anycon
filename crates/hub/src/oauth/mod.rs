//! OAuth 登录（GitHub、Google）：授权码流程 + PKCE（S256）+ `state`，两种模式都可用，配了凭据的提供方才启用
//! （docs/architecture.md § 3.1 / § 3.2，docs/spec/hub.md § 2.2）。
//!
//! 分工：
//! - [`client`]：出站 HTTPS 客户端（hyper + hyper-rustls，和 Web Push 同一套），每次调用 10 秒超时，不跟随重定向；
//! - [`provider`]：每个提供方的授权地址、换 token、取用户信息，以及对响应的解析（纯函数，单元测试覆盖）；
//! - [`state`]：PKCE、`next` 校验、`anycon_oauth` cookie；
//! - [`login`]：拿到提供方给的身份之后，怎么匹配 / 关联 / 注册账号（contract 里的四步规则）。
//!
//! HTTP 路由在 [`crate::http::oauth`]。
//!
//! hub 只记 `(提供方, 提供方的稳定用户 id)` 属于哪个账号。access token 只在内存里用来调一次用户信息接口，
//! 不存、不记日志；提供方给的邮箱只有在**提供方声明已验证**时才用来匹配账号 / 作为新账号的邮箱。

pub mod client;
pub mod login;
pub mod provider;
pub mod state;

use std::time::Duration;

use crate::ratelimit::RateLimiter;

/// `GET /auth/oauth/:provider/start`：每个真实 IP 每分钟的次数（docs/spec/hub.md § 7.2）。
pub const START_PER_MIN: u32 = 20;

/// 回调页面重定向时用的稳定错误码（`/login?oauth_error=<code>&provider=<p>`），前端据此做 i18n。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthErrorCode {
    /// 用户在提供方那里点了拒绝（`error=access_denied`）。
    Cancelled,
    /// 提供方出错、换 token / 取用户信息失败或超时。
    ProviderError,
    /// state 缺失、与 cookie 不一致、不存在、过期、已被用过，或不属于这个提供方。
    InvalidState,
    /// `single` 且没有邀请码，不能用这个身份注册。
    RegistrationClosed,
    InviteInvalid,
    InviteExpired,
    InviteUsed,
    /// 这个身份已经绑在别的账号上。
    IdentityTaken,
    /// 这个账号在这个提供方已经绑了别的身份。
    AlreadyLinked,
    /// 关联身份需要登录态，而 cookie 没有登录 / 不是发起关联的那个账号。
    NotSignedIn,
    /// hub 这边出错（库不可用之类）：不是用户或提供方的问题，稍后重试。
    ServerError,
}

impl OAuthErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            OAuthErrorCode::Cancelled => "cancelled",
            OAuthErrorCode::ProviderError => "provider_error",
            OAuthErrorCode::InvalidState => "invalid_state",
            OAuthErrorCode::RegistrationClosed => "registration_closed",
            OAuthErrorCode::InviteInvalid => "invite_invalid",
            OAuthErrorCode::InviteExpired => "invite_expired",
            OAuthErrorCode::InviteUsed => "invite_used",
            OAuthErrorCode::IdentityTaken => "identity_taken",
            OAuthErrorCode::AlreadyLinked => "already_linked",
            OAuthErrorCode::NotSignedIn => "not_signed_in",
            OAuthErrorCode::ServerError => "server_error",
        }
    }
}

/// [`crate::AppState`] 里 OAuth 相关的运行时状态：出站 HTTP 客户端、`start` 的限速。
pub struct OAuthRuntime {
    /// 出站客户端。TLS 配置建不起来（实际上不会发生）时是 `None`，所有提供方调用都当 `provider_error`。
    http: Option<client::HttpClient>,
    start: RateLimiter,
}

impl OAuthRuntime {
    /// `timeout`：每次出站调用（连接 + 请求 + 读完响应）的上限（[`crate::config::Timing::oauth_timeout`]）。
    pub fn new(timeout: Duration) -> Self {
        let http = match client::HttpClient::new(timeout) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(error = %e, "cannot build the HTTP client for OAuth sign-in; OAuth logins will fail");
                None
            }
        };
        Self { http, start: RateLimiter::per_minute(START_PER_MIN) }
    }

    pub fn http(&self) -> Option<&client::HttpClient> {
        self.http.as_ref()
    }

    /// 消耗 `start` 的一个令牌；超限返回需要等待的时间。
    pub fn check_start(&self, ip: std::net::IpAddr) -> Result<(), Duration> {
        self.start.check(ip)
    }

    /// 清掉回满的限速桶（和别的限速器一起，每分钟一次）。
    pub fn sweep(&self) {
        self.start.sweep();
    }
}
