//! `/auth/*`：登录（token / 邀请）与登出。成功一律 `204` + `Set-Cookie`，之后用 `GET /api/me` 取账号信息。
//! token 登录和邀请只有 `single` 有；`multi` 里这两条路由不存在（`404 not_found`，见 [`super::router`]）。

use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::accounts;
use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::store::{NewAudit, RedeemOutcome};
use crate::util::{clip, ct_eq, hash_token, ip_prefix, new_id, now_ms};

use super::json::ApiJson;
use super::session::{clear_cookie_header, cookie_secure, cookie_value, start_session, Client, COOKIE_NAME};

#[derive(Deserialize)]
pub struct TokenReq {
    token: String,
}

fn audit_login(client: &Client, event: &'static str, account: Option<String>, method: &str) -> NewAudit {
    NewAudit {
        event,
        account_id: account,
        device_id: None,
        ip_prefix: Some(ip_prefix(client.ctx.ip)),
        user_agent: Some(clip(&client.user_agent, 300)),
        detail: Some(serde_json::json!({ "method": method })),
    }
}

/// `POST /auth/token { token }`：管理员 bootstrap token 或账号 token。按 IP 限速。
pub async fn login_token(
    State(state): State<Arc<AppState>>,
    client: Client,
    ApiJson(req): ApiJson<TokenReq>,
) -> ApiResult<Response> {
    if let Err(wait) = state.limits.auth_token.check(client.ctx.ip) {
        tracing::info!(limit = "auth_token", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let hash = hash_token(req.token.trim());
    // `multi` 没有 bootstrap 管理员（路由本身也不挂，这里是兜底）。
    let bootstrap = state.admin.as_ref().filter(|admin| ct_eq(&hash, &admin.token_hash));
    let account_id = if let Some(admin) = bootstrap {
        Some(admin.account_id.clone())
    } else {
        state.store.find_account_by_token_hash(&hash, now_ms()).await?.map(|a| a.id)
    };
    let Some(account_id) = account_id else {
        state.audit(audit_login(&client, "login_fail", None, "token")).await;
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "invalid_token", "invalid token"));
    };
    let cookie = start_session(&state, &account_id, &client).await?;
    state.audit(audit_login(&client, "login_ok", Some(account_id), "token")).await;
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}

/// `POST /auth/logout`：删当前浏览器会话并清 cookie。没登录也返回 204。
pub async fn logout(State(state): State<Arc<AppState>>, client: Client, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(raw) = cookie_value(&headers, COOKIE_NAME) {
        state.store.delete_web_session_by_hash(&hash_token(&raw)).await?;
    }
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear_cookie_header(cookie_secure(&state, &client)))])
        .into_response())
}

#[derive(Deserialize)]
pub struct InviteReq {
    code: String,
    #[serde(default)]
    display_name: Option<String>,
}

/// `POST /auth/invite { code, display_name? }`：兑换邀请，创建账号并登录。
pub async fn login_invite(
    State(state): State<Arc<AppState>>,
    client: Client,
    ApiJson(req): ApiJson<InviteReq>,
) -> ApiResult<Response> {
    if let Err(wait) = state.limits.invite.check(client.ctx.ip) {
        tracing::info!(limit = "invite", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    let name = req
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|n| clip(n, 64))
        .unwrap_or_else(|| format!("user-{}", &new_id()[..6]));
    let account = match state.store.redeem_invite(&hash_token(req.code.trim()), &name, now_ms()).await? {
        RedeemOutcome::Redeemed(a) => a,
        RedeemOutcome::NotFound => {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "invite_invalid", "invite not found"));
        }
        RedeemOutcome::Expired => return Err(ApiError::new(StatusCode::GONE, "invite_expired", "invite expired")),
        RedeemOutcome::Used => return Err(ApiError::new(StatusCode::GONE, "invite_used", "invite already used")),
    };
    let cookie = start_session(&state, &account.id, &client).await?;
    accounts::audit_account_created(&state, &client, &account.id, "invite").await;
    state.audit(audit_login(&client, "invite_used", Some(account.id.clone()), "invite")).await;
    state.audit(audit_login(&client, "login_ok", Some(account.id), "invite")).await;
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response())
}
