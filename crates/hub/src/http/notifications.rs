//! Web Push 订阅和通知偏好（[ADR-37](../../../../docs/decisions.md#adr-37-web-push邮箱验证码登录和邮件通知)，
//! docs/spec/hub.md § 2.3 / § 6）。全部要求登录 cookie。
//!
//! - `GET` / `POST` / `DELETE /api/push/subscriptions`、`POST /api/push/test`
//! - `GET` / `PUT /api/me/notifications`
//!
//! 订阅里的 endpoint、`p256dh`、`auth` 都是凭据：只存库、只用来加密和投递，响应里只给 endpoint 的主机名，日志里一律不记。

use std::sync::Arc;

use anycon_protocol::{DeviceKind, NotifyCategory};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::notify::{effective_prefs, ALL_CATEGORIES};
use crate::push::webpush::parse_ua_public;
use crate::push::{decode_b64, endpoint_host, EndpointError, PushOptions, PushOutcome, Urgency};
use crate::store::{category_from_str, category_to_str, Device, NotificationPref, PushSubscriptionRow};
use crate::util::now_ms;

use super::json::ApiJson;
use super::session::Authed;

fn device_not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "device_not_found", "no such web device in this account")
}

/// 推送 API 的账号级限速（30 次 / 分钟）。
fn rate_limit(state: &AppState, auth: &Authed) -> ApiResult<()> {
    state.account_limits.push_api.check(&auth.account.id).map_err(|wait| {
        tracing::info!(limit = "push_api", "rate limited");
        ApiError::rate_limited(wait)
    })
}

/// 取账号名下未吊销的 **web** 设备；不存在、不是本账号的、已吊销、不是 web 都是 `404 device_not_found`。
async fn own_web_device(state: &AppState, auth: &Authed, id: &str) -> ApiResult<Device> {
    match state.store.get_device(id).await? {
        Some(d) if d.account_id == auth.account.id && d.revoked_at_ms.is_none() && d.kind == DeviceKind::Web => Ok(d),
        _ => Err(device_not_found()),
    }
}

// ---------------------------------------------------------------------------
// 订阅
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SubscriptionView {
    id: String,
    device_id: String,
    device_name: String,
    /// 只给推送服务的主机名；完整 endpoint 的路径是个凭据，不回显。
    endpoint_host: String,
    created_at_ms: u64,
}

fn view_of(r: PushSubscriptionRow) -> SubscriptionView {
    SubscriptionView {
        endpoint_host: endpoint_host(&r.endpoint),
        id: r.id,
        device_id: r.device_id,
        device_name: r.device_name,
        created_at_ms: r.created_at_ms,
    }
}

/// `GET /api/push/subscriptions`：本账号全部订阅（id、设备、endpoint 主机名、创建时间）。
pub async fn list_subscriptions(
    State(state): State<Arc<AppState>>,
    auth: Authed,
) -> ApiResult<Json<Vec<SubscriptionView>>> {
    let rows = state.store.list_push_subscriptions(&auth.account.id).await?;
    Ok(Json(rows.into_iter().map(view_of).collect()))
}

#[derive(Deserialize)]
pub struct Keys {
    p256dh: String,
    auth: String,
}

#[derive(Deserialize)]
pub struct NewSubscription {
    device_id: String,
    endpoint: String,
    keys: Keys,
}

/// `POST /api/push/subscriptions { device_id, endpoint, keys: { p256dh, auth } }`。同一设备只留一条（新的顶掉旧的）。
pub async fn create_subscription(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    ApiJson(req): ApiJson<NewSubscription>,
) -> ApiResult<Json<SubscriptionView>> {
    rate_limit(&state, &auth)?;
    let device = own_web_device(&state, &auth, &req.device_id).await?;

    state.push.policy().check(&req.endpoint).map_err(|e| match e {
        EndpointError::Invalid => ApiError::bad_request("endpoint is not a valid URL"),
        EndpointError::NotAllowed => ApiError::new(
            StatusCode::BAD_REQUEST,
            "push_endpoint_not_allowed",
            "the push endpoint must be an https URL on a known push service",
        ),
    })?;
    // 浏览器的公钥必须是 65 字节未压缩点且在曲线上，auth 是 16 字节；归一成不带填充的 base64url 存。
    let p256dh = decode_b64(&req.keys.p256dh)
        .filter(|b| parse_ua_public(b).is_ok())
        .ok_or_else(|| ApiError::bad_request("keys.p256dh is not a valid P-256 public key"))?;
    let auth_secret = decode_b64(&req.keys.auth)
        .filter(|b| b.len() == 16)
        .ok_or_else(|| ApiError::bad_request("keys.auth must be 16 bytes (base64url)"))?;

    let sub = state
        .store
        .upsert_push_subscription(
            &device.id,
            &req.endpoint,
            &URL_SAFE_NO_PAD.encode(&p256dh),
            &URL_SAFE_NO_PAD.encode(&auth_secret),
            now_ms(),
        )
        .await?;
    tracing::info!(host = %endpoint_host(&sub.endpoint), "push subscription saved");
    Ok(Json(SubscriptionView {
        endpoint_host: endpoint_host(&sub.endpoint),
        id: sub.id,
        device_id: device.id,
        device_name: device.name,
        created_at_ms: sub.created_at_ms,
    }))
}

/// `DELETE /api/push/subscriptions/:id`。
pub async fn delete_subscription(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    if state.store.delete_push_subscription(&auth.account.id, &id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Err(ApiError::not_found())
    }
}

#[derive(Deserialize)]
pub struct TestReq {
    device_id: String,
}

#[derive(Serialize)]
pub struct TestResult {
    /// `ok`：推送服务收下了；`gone`：订阅已失效（已删除，浏览器需要重新开启）；`error`：推送服务拒绝了或连不上。
    result: &'static str,
    /// 推送服务回的 HTTP 状态码（连不上时没有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
}

/// `POST /api/push/test { device_id }`：给这台设备的订阅发一条测试推送（载荷 `{"category":"test"}`），同步返回结果。
pub async fn test_push(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    ApiJson(req): ApiJson<TestReq>,
) -> ApiResult<Json<TestResult>> {
    rate_limit(&state, &auth)?;
    let device = own_web_device(&state, &auth, &req.device_id).await?;
    let Some(sub) = state.store.get_push_subscription_of_device(&device.id).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "subscription_not_found",
            "this device has no push subscription",
        ));
    };
    // 测试推送只放 60 秒：错过了就没必要再响。
    let opts = PushOptions { ttl: 60, urgency: Urgency::Normal };
    let outcome = state.push.send(&sub, br#"{"category":"test"}"#, opts).await;
    let result = match outcome {
        PushOutcome::Delivered => TestResult { result: "ok", status: None },
        PushOutcome::Gone => {
            state.store.delete_push_subscription_by_id(&sub.id).await?;
            TestResult { result: "gone", status: None }
        }
        PushOutcome::Failed { status } => TestResult { result: "error", status },
    };
    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// 通知偏好
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct PrefView {
    category: &'static str,
    push: bool,
    email: bool,
}

#[derive(Serialize)]
pub struct NotificationsView {
    /// 六个类别各一行，顺序固定。
    categories: Vec<PrefView>,
    /// 邮件通知可用：hub 配了 SMTP，而且账号绑定了邮箱。
    email_available: bool,
    /// hub 能发 Web Push。
    push_available: bool,
}

fn view(state: &AppState, auth: &Authed, prefs: &[NotificationPref]) -> NotificationsView {
    NotificationsView {
        categories: prefs
            .iter()
            .map(|p| PrefView { category: category_to_str(p.category), push: p.push, email: p.email })
            .collect(),
        email_available: state.email_enabled() && auth.account.email.is_some(),
        push_available: true,
    }
}

/// `GET /api/me/notifications`。
pub async fn get_prefs(State(state): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<NotificationsView>> {
    let stored = state.store.get_notification_prefs(&auth.account.id).await?;
    Ok(Json(view(&state, &auth, &effective_prefs(&stored))))
}

#[derive(Deserialize)]
pub struct PrefInput {
    category: String,
    push: bool,
    email: bool,
}

#[derive(Deserialize)]
pub struct PutPrefs {
    categories: Vec<PrefInput>,
}

/// `PUT /api/me/notifications { categories: [{ category, push, email }] }`：只认六个已知类别（未知类别 `400`），
/// 数组里没出现的类别保持原样。邮件只对 `permission_requested` / `error` / `enroll_requested` / `security` 有意义，
/// 其余两个类别的 `email` 一律存成 `false`。返回完整的结果（和 `GET` 一样）。
pub async fn put_prefs(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    ApiJson(req): ApiJson<PutPrefs>,
) -> ApiResult<Json<NotificationsView>> {
    if req.categories.len() > ALL_CATEGORIES.len() * 4 {
        return Err(ApiError::bad_request("too many categories"));
    }
    let mut changes: Vec<NotificationPref> = Vec::new();
    for p in req.categories {
        let category: NotifyCategory = category_from_str(&p.category).ok_or_else(|| {
            ApiError::bad_request(format!("unknown category {:?}", crate::util::clip(&p.category, 40)))
        })?;
        let email = p.email && crate::mail::is_email_category(category);
        // 同一类别出现多次：后面的覆盖前面的。
        changes.retain(|c| c.category != category);
        changes.push(NotificationPref { category, push: p.push, email });
    }
    state.store.set_notification_prefs(&auth.account.id, &changes).await?;
    let stored = state.store.get_notification_prefs(&auth.account.id).await?;
    Ok(Json(view(&state, &auth, &effective_prefs(&stored))))
}
