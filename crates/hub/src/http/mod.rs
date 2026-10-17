//! HTTP 表面：路由装配、健康检查、中间件（CSRF 头、安全头、请求日志）。
//!
//! 路由清单见 docs/spec/hub.md § 2。

pub mod api;
mod assets;
pub mod auth;
pub mod email;
pub mod json;
pub mod notifications;
pub mod session;

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde_json::json;

use crate::app::AppState;
use crate::error::ApiError;
use crate::ws;

/// JSON API 的请求体上限。信封走 WebSocket，不受它约束。
const API_BODY_LIMIT: usize = 64 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/config", get(api::config))
        .route("/me", get(api::me).patch(api::patch_me))
        .route("/me/email", post(email::bind_send).delete(email::unbind))
        .route("/me/email/verify", post(email::bind_verify))
        .route("/me/notifications", get(notifications::get_prefs).put(notifications::put_prefs))
        .route("/push/subscriptions", get(notifications::list_subscriptions).post(notifications::create_subscription))
        .route("/push/subscriptions/{id}", delete(notifications::delete_subscription))
        .route("/push/test", post(notifications::test_push))
        .route("/me/sessions", get(api::list_sessions))
        .route("/me/sessions/{id}", delete(api::delete_session))
        .route("/me/tokens", get(api::list_tokens).post(api::create_token))
        .route("/me/tokens/{id}", delete(api::delete_token))
        .route("/devices", get(api::list_devices))
        .route("/devices/{id}", patch(api::patch_device).delete(api::delete_device))
        .route("/invites", get(api::list_invites).post(api::create_invite))
        .route("/invites/{id}", delete(api::delete_invite))
        .route("/admin/status", get(api::admin_status))
        .route("/admin/accounts", get(api::admin_accounts))
        .route("/admin/audit", get(api::admin_audit))
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(csrf))
        .layer(DefaultBodyLimit::max(API_BODY_LIMIT));

    let auth = Router::new()
        .route("/token", post(auth::login_token))
        .route("/logout", post(auth::logout))
        .route("/invite", post(auth::login_invite))
        .route("/email/send", post(email::login_send))
        .route("/email/verify", post(email::login_verify))
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(csrf))
        .layer(DefaultBodyLimit::max(API_BODY_LIMIT));

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/v1/ws", get(ws::ws_upgrade))
        .nest("/api", api)
        .nest("/auth", auth)
        .fallback(assets::serve)
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn(log_requests))
        .with_state(state)
}

/// 写操作（POST / PUT / PATCH / DELETE）必须带 `X-Anycon: 1`。
/// 自定义头无法被跨站表单 / 简单请求带上，配合 `SameSite=Lax` cookie 挡 CSRF；hub 不开 CORS。
async fn csrf(req: Request, next: Next) -> Response {
    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && req.headers().get("x-anycon").and_then(|v| v.to_str().ok()) != Some("1") {
        return ApiError::csrf().into_response();
    }
    next.run(req).await
}

/// 嵌入的 Web UI（HTML 响应）的内容安全策略。
///
/// - `script-src 'self' 'wasm-unsafe-eval'`：只允许同源脚本，加上 Noise 的 wasm 需要的编译权限；
///   **没有** `unsafe-inline`——`index.html` 里不能有内联脚本（主题脚本已外置成文件）；
/// - `connect-src 'self'`：fetch / WebSocket 只能连 hub 自己，被 prompt 注入的页面没法把数据发去别处；
/// - `img-src 'self' data: blob:`：没有 `https:`，远程图片（Markdown 里的外泄通道）加载不出来；
/// - `style-src 'self' 'unsafe-inline'`：样式允许内联（组件库 / 动态样式需要），脚本不行；
/// - `frame-ancestors 'none'` 替代 `X-Frame-Options`，`base-uri` / `form-action` 收紧到同源。
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; \
connect-src 'self'; img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; font-src 'self' data:; \
worker-src 'self'; manifest-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

/// 所有响应加基础安全头；HTML 响应（嵌入的 Web UI，含 SPA 回退）另外加 CSP 和 COOP。
async fn security_headers(req: Request, next: Next) -> Response {
    // 304 没有 Content-Type：按路径判断是不是在重新验证一个 HTML 页面（`/`、SPA 路由、`*.html`）。
    let last_segment = req.uri().path().rsplit('/').next().unwrap_or("").to_string();
    let html_path = !last_segment.contains('.') || last_segment.ends_with(".html");
    let mut res = next.run(req).await;
    let is_html = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
        || (res.status() == StatusCode::NOT_MODIFIED && html_path);
    let h = res.headers_mut();
    for (name, value) in
        [("x-content-type-options", "nosniff"), ("referrer-policy", "no-referrer"), ("x-frame-options", "DENY")]
    {
        h.entry(HeaderName::from_static(name)).or_insert(HeaderValue::from_static(value));
    }
    if is_html {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CONTENT_SECURITY_POLICY));
        h.insert(HeaderName::from_static("cross-origin-opener-policy"), HeaderValue::from_static("same-origin"));
    }
    res
}

/// 请求日志：方法、**路由模板**、状态码、耗时。不记实际 URI（邀请码等在路径里）、不记头、不记 body。
async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route =
        req.extensions().get::<MatchedPath>().map(|m| m.as_str().to_string()).unwrap_or_else(|| "(static)".to_string());
    let start = Instant::now();
    let res = next.run(req).await;
    let status = res.status().as_u16();
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if route.starts_with("/api") || route.starts_with("/auth") {
        tracing::info!(%method, route = %route, status, elapsed_ms, "http");
    } else {
        tracing::debug!(%method, route = %route, status, elapsed_ms, "http");
    }
    res
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") }))
}

async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    match state.store.ping().await {
        Ok(()) => Json(json!({ "status": "ready" })).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "readyz: database not available");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "unavailable" }))).into_response()
        }
    }
}
