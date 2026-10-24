//! `/v1/ws`：设备 ↔ hub 的 WebSocket。
//!
//! - [`origin`]：升级时的 Origin 校验；
//! - [`outq`]：每连接的有界发送队列（背压）；
//! - [`registry`]：路由表与连接对象；
//! - [`sticky`]：被退回的 `seq` 的「粘住」状态，保证转发永远有序；
//! - [`conn`]：连接生命周期（认证、读循环、转发、控制消息、心跳、收尾）。
//!
//! 升级入口有两个：主域名的 [`ws_upgrade`]，和预览域名上的 [`preview_ws_upgrade`]（ADR-39，由 `http::preview` 调用）。
//!
//! hub 在这里**不解析任何 `body`**：转发的 data / handshake / notify 只看信封头（from / to / kind / seq / 长度），
//! 原样转发收到的字节；只有 `kind = control` 的 body 才解码，那是 hub 自己的协议。

pub mod conn;
pub mod origin;
pub mod outq;
pub mod registry;
pub mod sticky;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::clientip;
use crate::error::ApiError;
use crate::http::session::account_from_cookie;

/// 升级时确定的、与连接绑定的上下文。
pub struct UpgradeCtx {
    pub ip: std::net::IpAddr,
    /// 升级请求带了有效的 web 登录 cookie 时的账号（临时连接配对时用来确定归属）。
    pub cookie_account: Option<String>,
    /// `Some(label)` = 预览域名上的升级（label 取自主机名）：这条连接只接受预览令牌认证（docs/spec/hub.md § 13.4）。
    pub preview_label: Option<String>,
}

pub async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let client = clientip::resolve(peer, &headers, &state.config.trusted_proxies);

    if state.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "restarting").into_response();
    }
    if let Err(wait) = state.limits.ws_upgrade.check(client.ip) {
        tracing::info!(limit = "ws_upgrade", "rate limited");
        return ApiError::rate_limited(wait).into_response();
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin_header_present = headers.contains_key(header::ORIGIN);
    // Origin 头存在但不是合法 UTF-8 也按「不允许」处理。
    if (origin_header_present && origin.is_none()) || !origin::origin_allowed(origin, host, &state.config.public_url) {
        tracing::info!("websocket upgrade rejected: origin not allowed");
        return ApiError::new(StatusCode::FORBIDDEN, "origin_not_allowed", "origin not allowed").into_response();
    }
    let cookie_account = account_from_cookie(&state, &headers).await;

    let max = state.config.max_envelope_bytes;
    let ctx = UpgradeCtx { ip: client.ip, cookie_account, preview_label: None };
    upgrade(state, ws, ctx, max)
}

/// 预览域名上的 `GET /v1/ws`（`label` 是主机名里的 label）。
///
/// `Origin` 必须**正好**是这个请求的预览 origin（`expected_origin`：scheme + 主机 + 端口），没带或别的都是
/// `403 origin_not_allowed`；另有单独的 IP 限速桶（预览的 service worker 被浏览器回收后会重连，比主域名宽）。
/// 不看登录 cookie：预览 origin 不用 cookie。
pub async fn preview_ws_upgrade(
    state: Arc<AppState>,
    peer: SocketAddr,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    label: String,
    expected_origin: String,
) -> Response {
    let client = clientip::resolve(peer, &headers, &state.config.trusted_proxies);
    if state.shutdown.is_cancelled() {
        return (StatusCode::SERVICE_UNAVAILABLE, "restarting").into_response();
    }
    if let Err(wait) = state.limits.preview_ws_upgrade.check(client.ip) {
        tracing::info!(limit = "preview_ws_upgrade", "rate limited");
        return ApiError::rate_limited(wait).into_response();
    }
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin.is_some_and(|o| o == expected_origin) {
        tracing::info!("preview websocket upgrade rejected: origin not allowed");
        return ApiError::new(StatusCode::FORBIDDEN, "origin_not_allowed", "origin not allowed").into_response();
    }
    let max = state.config.max_envelope_bytes;
    let ctx = UpgradeCtx { ip: client.ip, cookie_account: None, preview_label: Some(label) };
    upgrade(state, ws, ctx, max)
}

fn upgrade(state: Arc<AppState>, ws: WebSocketUpgrade, ctx: UpgradeCtx, max: usize) -> Response {
    ws
        // 允许略大于上限的帧进来再丢弃（而不是由底层直接断连接），但要有个硬上限防止被灌。
        .max_message_size(max.saturating_mul(2).max(64 * 1024))
        .max_frame_size(max.saturating_mul(2).max(64 * 1024))
        .on_upgrade(move |socket| conn::run_socket(state, socket, ctx))
}
