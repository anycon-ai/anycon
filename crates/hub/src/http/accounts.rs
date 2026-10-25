//! 账号生命周期与管理：删除自己的账号、管理员删账号、授予 / 取消管理员、`GET /api/quota`。
//!
//! - `DELETE /api/me`：要求最近认证过（当前浏览器会话创建于 10 分钟内），否则 `403 reauth_required`；`single` 的
//!   bootstrap 管理员不能删（`409 bootstrap_admin`）。成功 `204` 并清登录 cookie。
//! - `DELETE /api/admin/accounts/:id`：管理员删别人的账号，规则同上（目标是自己 `400 cannot_modify_self`）。
//! - `POST` / `DELETE /api/admin/accounts/:id/admin`：授予 / 取消库里的管理员标记，回更新后的账号行。
//!   `ANYCON_ADMIN_EMAILS` 派生的管理员取消不了（`409 admin_by_config`，要改配置）。
//!
//! 真正的删除过程在 [`crate::account_delete`]，配额的算法在 [`crate::quota`]。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::account_delete::{self, DeletedBy};
use crate::accounts;
use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::quota::{self, QuotaView};
use crate::store::{Account, NewAudit};
use crate::util::{ip_prefix, now_ms};

use super::api::{admin_account_view, AdminAccountView};
use super::session::{clear_cookie_header, cookie_secure, Authed, Client};

/// 「最近认证过」的窗口：删账号要求当前浏览器会话是这么久之内创建的。
pub const REAUTH_WINDOW_MS: u64 = 10 * 60 * 1000;

fn bootstrap_admin() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "bootstrap_admin", "the bootstrap admin account cannot be modified this way")
}

fn cannot_modify_self() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "cannot_modify_self", "you cannot do this to your own account")
}

fn admin_by_config() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "admin_by_config",
        "this account is an admin through ANYCON_ADMIN_EMAILS; change the configuration instead",
    )
}

// ---------------------------------------------------------------------------
// 配额
// ---------------------------------------------------------------------------

/// `GET /api/quota`：本账号的用量和上限。上限为 `null` = 不限（`single` 总是）。
pub async fn get_quota(State(state): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<QuotaView>> {
    Ok(Json(quota::usage(&state, &auth.account.id).await?))
}

// ---------------------------------------------------------------------------
// 删账号
// ---------------------------------------------------------------------------

/// `DELETE /api/me`。
pub async fn delete_me(State(state): State<Arc<AppState>>, auth: Authed, client: Client) -> ApiResult<Response> {
    // bootstrap 管理员是永久的限制，不必先让人重新登录一遍才告诉他。
    if accounts::is_bootstrap_admin(&state, &auth.account.id) {
        return Err(bootstrap_admin());
    }
    if now_ms().saturating_sub(auth.session.created_at_ms) > REAUTH_WINDOW_MS {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "reauth_required",
            "sign in again to delete your account (the current session is older than 10 minutes)",
        ));
    }
    if !account_delete::delete_account(&state, &auth.account.id, DeletedBy::Owner).await? {
        return Err(ApiError::not_found());
    }
    // 会话已随账号删掉，cookie 也清掉。
    Ok((StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear_cookie_header(cookie_secure(&state, &client)))])
        .into_response())
}

/// `DELETE /api/admin/accounts/:id`。
pub async fn admin_delete_account(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth.require_admin()?;
    if id == auth.account.id {
        return Err(cannot_modify_self());
    }
    if accounts::is_bootstrap_admin(&state, &id) {
        return Err(bootstrap_admin());
    }
    if !account_delete::delete_account(&state, &id, DeletedBy::Admin).await? {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// 管理员标记
// ---------------------------------------------------------------------------

/// 授予 / 取消管理员共用：校验、改库里的标记、状态真的变了才写审计，回更新后的账号行。
async fn set_admin(
    state: &AppState,
    auth: &Authed,
    client: &Client,
    id: &str,
    grant: bool,
) -> ApiResult<Json<AdminAccountView>> {
    auth.require_admin()?;
    if id == auth.account.id {
        return Err(cannot_modify_self());
    }
    let target: Account = state.store.get_account(id).await?.ok_or_else(ApiError::not_found)?;
    if !grant {
        if accounts::is_bootstrap_admin(state, id) {
            return Err(bootstrap_admin());
        }
        if accounts::admin_by_config(&state.config, &target) {
            return Err(admin_by_config());
        }
    }
    if target.is_admin != grant {
        // 同时被删掉的账号：`false` = 没改成，当作不存在。
        if !state.store.set_account_admin(id, grant).await? {
            return Err(ApiError::not_found());
        }
        state
            .audit(NewAudit {
                event: if grant { "admin_granted" } else { "admin_revoked" },
                account_id: Some(id.to_string()),
                ip_prefix: Some(ip_prefix(client.ctx.ip)),
                detail: Some(serde_json::json!({ "by": auth.account.id })),
                ..Default::default()
            })
            .await;
    }
    let updated = state.store.get_account(id).await?.ok_or_else(ApiError::not_found)?;
    Ok(Json(admin_account_view(state, updated)))
}

/// `POST /api/admin/accounts/:id/admin`。
pub async fn admin_grant(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    Path(id): Path<String>,
) -> ApiResult<Json<AdminAccountView>> {
    set_admin(&state, &auth, &client, &id, true).await
}

/// `DELETE /api/admin/accounts/:id/admin`。
pub async fn admin_revoke(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    Path(id): Path<String>,
) -> ApiResult<Json<AdminAccountView>> {
    set_admin(&state, &auth, &client, &id, false).await
}
