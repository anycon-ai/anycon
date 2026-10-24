//! `POST /api/preview/tokens`：已配对设备用设备 token 换一个预览令牌（ADR-39，docs/spec/hub.md § 13.3）。
//!
//! 只认 `Authorization: Bearer <device token>`，不看 cookie；照常要 `X-Anycon: 1`（路由层的 CSRF 中间件）。
//! 令牌是 32 字节随机数的 hex，库里只存哈希；第一次见到的 label 在这里绑定到节点。令牌不进日志。

use std::sync::Arc;

use anycon_protocol::preview::tunnel::is_valid_label;
use anycon_protocol::DeviceKind;
use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::store::{IssueOutcome, NewPreviewToken, PreviewIssueLimits};
use crate::util::{hash_token, new_secret, now_ms};

use super::json::ApiJson;
use super::session::DeviceBearer;

#[derive(Deserialize)]
pub struct TokenReq {
    node: String,
    label: String,
}

#[derive(Serialize)]
pub struct TokenView {
    token: String,
    /// `https://<label>.<预览域名>[:端口]`
    origin: String,
    expires_at_ms: u64,
}

fn conflict(code: &'static str, message: &'static str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, code, message)
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    caller: DeviceBearer,
    ApiJson(req): ApiJson<TokenReq>,
) -> ApiResult<Json<TokenView>> {
    let Some(preview) = &state.config.preview else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "preview_disabled", "dev server preview is not enabled"));
    };
    let device = &caller.device;
    if let Err(wait) = state.account_limits.preview_tokens.check(&device.id) {
        tracing::info!(limit = "preview_tokens", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }
    if !is_valid_label(&req.label) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_label",
            "label must be 26 lowercase base32 characters",
        ));
    }
    // 节点：同账号未吊销的 daemon，而且调用的设备和它配过对（不区分「不存在」和「不是你的」）。
    let node_ok = match state.store.get_device(&req.node).await? {
        Some(n) if n.account_id == device.account_id && n.kind == DeviceKind::Daemon && n.revoked_at_ms.is_none() => {
            state.store.is_paired(&device.id, &n.id).await?
        }
        _ => false,
    };
    if !node_ok {
        tracing::debug!(device = %device.id, "preview token refused: not a paired node of this account");
        return Err(ApiError::not_found());
    }

    let token = new_secret();
    let now = now_ms();
    let expires_at_ms = now + state.config.timing.preview_token_ttl.as_millis() as u64;
    let limits = PreviewIssueLimits {
        labels_per_node: state.config.limits.preview_labels_per_node,
        tokens_per_device: state.config.limits.preview_tokens_per_device,
    };
    let issued = state
        .store
        .issue_preview_token(
            NewPreviewToken {
                token_hash: hash_token(&token),
                label: req.label.clone(),
                node_id: req.node.clone(),
                account_id: device.account_id.clone(),
                device_id: device.id.clone(),
                expires_at_ms,
            },
            now,
            limits,
        )
        .await?;
    match issued {
        IssueOutcome::Issued => {}
        IssueOutcome::LabelTaken => return Err(conflict("label_taken", "label is bound to another node")),
        IssueOutcome::TooManyLabels => {
            return Err(conflict("too_many_labels", "this node has too many preview labels"))
        }
    }
    tracing::info!(device = %device.id, node = %req.node, "preview token issued");
    Ok(Json(TokenView { token, origin: preview.origin_for(&req.label), expires_at_ms }))
}
