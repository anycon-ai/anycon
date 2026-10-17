//! 浏览器登录会话：cookie 读写、会话查找、Axum 提取器。
//!
//! cookie 名 `anycon_session`，值是 32 字节随机数的 hex（库里只存哈希）。
//! 属性：`HttpOnly; SameSite=Lax; Path=/`，**不设 Domain**；`Secure` 当 `ANYCON_PUBLIC_URL` 是 https（始终设），
//! 或者请求经由 https（可信代理转发的 `X-Forwarded-Proto: https`）。会话滑动续期。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue};

use crate::app::AppState;
use crate::clientip::{self, ClientCtx};
use crate::error::{ApiError, ApiResult};
use crate::store::{Account, WebSession};
use crate::util::{clip, hash_token, ip_prefix, new_secret, now_ms};

pub const COOKIE_NAME: &str = "anycon_session";

/// 从 `Cookie` 头里取指定名字的值。
pub fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.trim().to_string())
}

pub fn set_cookie_header(value: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("cookie header is ascii")
}

pub fn clear_cookie_header(secure: bool) -> HeaderValue {
    set_cookie_header("", 0, secure)
}

/// 查 cookie 对应的会话（未过期），并按需滑动续期。
pub async fn lookup_session(state: &Arc<AppState>, headers: &HeaderMap) -> ApiResult<Option<(WebSession, Account)>> {
    let Some(raw) = cookie_value(headers, COOKIE_NAME) else { return Ok(None) };
    if raw.is_empty() || raw.len() > 256 {
        return Ok(None);
    }
    let now = now_ms();
    let Some((session, account)) = state.store.find_web_session(&hash_token(&raw), now).await? else {
        return Ok(None);
    };
    // 每分钟至多写一次库。
    if now.saturating_sub(session.last_seen_ms) >= 60_000 {
        let expires = now + state.config.timing.session_ttl.as_millis() as u64;
        if let Err(e) = state.store.touch_web_session(&session.id, now, expires).await {
            tracing::warn!(error = %e, "failed to refresh web session");
        }
    }
    Ok(Some((session, account)))
}

/// WebSocket 升级用：请求带的登录 cookie 对应的账号（无效 / 没带都返回 `None`）。
pub async fn account_from_cookie(state: &Arc<AppState>, headers: &HeaderMap) -> Option<String> {
    match lookup_session(state, headers).await {
        Ok(found) => found.map(|(_, a)| a.id),
        Err(_) => None,
    }
}

/// 为账号创建浏览器会话，返回 `Set-Cookie` 头的值。
pub async fn start_session(state: &Arc<AppState>, account_id: &str, client: &Client) -> ApiResult<HeaderValue> {
    let raw = new_secret();
    let now = now_ms();
    let ttl = state.config.timing.session_ttl;
    state
        .store
        .create_web_session(
            account_id,
            &hash_token(&raw),
            &clip(&client.user_agent, 300),
            &ip_prefix(client.ctx.ip),
            now,
            now + ttl.as_millis() as u64,
        )
        .await?;
    Ok(set_cookie_header(&raw, ttl.as_secs(), cookie_secure(state, client)))
}

/// cookie 要不要带 `Secure`：`ANYCON_PUBLIC_URL` 是 https 就一定要（反代不在信任列表里、`X-Forwarded-Proto`
/// 不被采信时也不例外）；否则看请求是否经由 https。
pub fn cookie_secure(state: &AppState, client: &Client) -> bool {
    state.config.public_url.scheme() == "https" || client.ctx.secure
}

/// 真实客户端 IP / 协议 / UA。
pub struct Client {
    pub ctx: ClientCtx,
    pub user_agent: String,
}

impl FromRequestParts<Arc<AppState>> for Client {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0)
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
        let ctx = clientip::resolve(peer, &parts.headers, &state.config.trusted_proxies);
        let user_agent = parts.headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        Ok(Client { ctx, user_agent })
    }
}

/// 已登录账号（`401 unauthorized` 当 cookie 缺失 / 过期）。
pub struct Authed {
    pub account: Account,
    pub session: WebSession,
}

impl Authed {
    pub fn require_admin(&self) -> ApiResult<()> {
        if self.account.is_admin {
            Ok(())
        } else {
            Err(ApiError::forbidden())
        }
    }
}

impl FromRequestParts<Arc<AppState>> for Authed {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        match lookup_session(state, &parts.headers).await? {
            Some((session, account)) => Ok(Authed { account, session }),
            None => Err(ApiError::unauthorized()),
        }
    }
}

/// 只读接口用的登录态：登录 cookie，或设备 token（`Authorization: Bearer <device token>`）。
///
/// 带了 `Authorization` 头就只看它（不回退到 cookie）：不是 `Bearer`、token 不存在、设备已吊销都是
/// `401 unauthorized`。设备 token 只能读（目前只有 `GET /api/devices`），写操作仍然要登录 cookie。
/// CLI 已经是这个 hub 上的设备时用它列出账号里的节点，不必再登录一次（spec/cli.md § 3.3）。
pub struct AccountReader {
    pub account_id: String,
}

impl FromRequestParts<Arc<AppState>> for AccountReader {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        if let Some(raw) = parts.headers.get(header::AUTHORIZATION) {
            let token = raw
                .to_str()
                .ok()
                .and_then(|v| v.split_once(' '))
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map(|(_, t)| t.trim())
                .filter(|t| !t.is_empty() && t.len() <= 256)
                .ok_or_else(ApiError::unauthorized)?;
            return match state.store.find_device_by_token_hash(&hash_token(token)).await? {
                Some(d) if d.revoked_at_ms.is_none() => Ok(AccountReader { account_id: d.account_id }),
                _ => Err(ApiError::unauthorized()),
            };
        }
        match lookup_session(state, &parts.headers).await? {
            Some((_, account)) => Ok(AccountReader { account_id: account.id }),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_header() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; anycon_session=abc123; b=2"));
        assert_eq!(cookie_value(&h, COOKIE_NAME).as_deref(), Some("abc123"));
        assert_eq!(cookie_value(&h, "a").as_deref(), Some("1"));
        assert!(cookie_value(&h, "zzz").is_none());
    }

    #[test]
    fn cookie_attributes() {
        let plain = set_cookie_header("v", 100, false);
        let s = plain.to_str().unwrap();
        assert!(s.starts_with("anycon_session=v;"));
        assert!(s.contains("HttpOnly") && s.contains("SameSite=Lax") && s.contains("Path=/"));
        assert!(!s.contains("Secure") && !s.contains("Domain"));
        let secure = set_cookie_header("v", 100, true);
        assert!(secure.to_str().unwrap().contains("; Secure"));
        assert!(clear_cookie_header(false).to_str().unwrap().contains("Max-Age=0"));
    }
}
