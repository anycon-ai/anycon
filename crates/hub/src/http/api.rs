//! `/api/*`：账号、登录 token、浏览器会话、设备、邀请、管理。全部（除 `/api/config`）要求登录态；
//! 写操作的 `X-Anycon: 1` 校验在路由层的中间件里（见 `http::csrf`）。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anycon_protocol::{DeviceKind, PROTOCOL_VERSION};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::accounts;
use crate::app::AppState;
use crate::error::{ApiError, ApiResult};
use crate::store::{Account, Device, NewAudit};
use crate::util::{hash_token, ip_prefix, new_secret, now_ms, random_hex};

use super::json::ApiJson;
use super::session::{AccountReader, Authed, Client};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

// ---------------------------------------------------------------------------
// 列表分页（docs/spec/hub.md § 2.5：`?limit=&cursor=`，不做 offset 分页）
// ---------------------------------------------------------------------------

/// 默认每页条数 / 单页上限。
const DEFAULT_PAGE: u32 = 100;
const MAX_PAGE: u32 = 500;
/// 还有下一页时，响应头里带下一页的游标；最后一页没有这个头。响应体保持裸数组（兼容已有客户端）。
pub const NEXT_CURSOR_HEADER: &str = "x-next-cursor";

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

impl PageQuery {
    /// 每页条数：缺省 100，夹在 `1..=500`；不是数字 → 400。
    fn limit(&self) -> ApiResult<u32> {
        match self.limit.as_deref() {
            None | Some("") => Ok(DEFAULT_PAGE),
            Some(s) => s
                .parse::<u32>()
                .map(|n| n.clamp(1, MAX_PAGE))
                .map_err(|_| ApiError::bad_request("limit must be a positive integer")),
        }
    }

    fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref().filter(|c| !c.is_empty())
    }
}

/// 时间 + id 的 keyset 游标：`<created_at_ms>.<id>`。
fn encode_cursor(ms: u64, id: &str) -> String {
    format!("{ms}.{id}")
}

fn decode_cursor(raw: &str) -> ApiResult<(u64, String)> {
    let bad = || ApiError::bad_request("invalid cursor");
    let (ms, id) = raw.split_once('.').ok_or_else(bad)?;
    let ms: u64 = ms.parse().map_err(|_| bad())?;
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(bad());
    }
    Ok((ms, id.to_string()))
}

/// 多取一条判断有没有下一页：取 `limit + 1` 条，多出来的那条丢掉，并据最后保留的一项算下一页游标。
fn split_page<T>(mut rows: Vec<T>, limit: u32, cursor_of: impl Fn(&T) -> String) -> (Vec<T>, Option<String>) {
    let limit = limit as usize;
    if rows.len() <= limit {
        return (rows, None);
    }
    rows.truncate(limit);
    let next = rows.last().map(cursor_of);
    (rows, next)
}

fn page_response<T: Serialize>(items: Vec<T>, next: Option<String>) -> Response {
    let mut res = Json(items).into_response();
    if let Some(c) = next.and_then(|c| HeaderValue::from_str(&c).ok()) {
        res.headers_mut().insert(HeaderName::from_static(NEXT_CURSOR_HEADER), c);
    }
    res
}

/// 校验并规整一个显示用的名字：去首尾空白，1..=`max` 个字符。
fn clean_name(raw: &str, max: usize, what: &str) -> ApiResult<String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(ApiError::bad_request(format!("{what} must not be empty")));
    }
    if s.chars().count() > max {
        return Err(ApiError::bad_request(format!("{what} must be at most {max} characters")));
    }
    Ok(s.to_string())
}

// ---------------------------------------------------------------------------
// 公开配置
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ConfigView {
    mode: &'static str,
    public_url: String,
    /// 启用的登录方式，顺序固定：`github`、`google`（配了凭据）、`email`（配了 SMTP）、`token`（只有 `single`）。
    login_methods: Vec<&'static str>,
    protocol_version: u32,
    version: &'static str,
    queue_enabled: bool,
    /// Web Push 的 VAPID 公钥（base64url，65 字节未压缩点），浏览器订阅时用作 `applicationServerKey`。
    vapid_public_key: Option<String>,
    /// hub 能发 Web Push（VAPID 密钥已就绪）。
    push_available: bool,
    /// dev server 预览的 origin 模板（如 `"https://{label}.aiplace.app"`），没配预览时是 `null`（docs/spec/hub.md § 13）。
    preview_origin_template: Option<String>,
}

pub async fn config(State(state): State<Arc<AppState>>) -> Json<ConfigView> {
    Json(ConfigView {
        mode: state.config.mode.as_str(),
        public_url: state.config.public_url_str(),
        login_methods: accounts::config_login_methods(&state),
        protocol_version: PROTOCOL_VERSION,
        version: VERSION,
        queue_enabled: false,
        vapid_public_key: Some(state.push.public_key().to_string()),
        push_available: true,
        preview_origin_template: state.config.preview.as_ref().map(|p| p.origin_template()),
    })
}

// ---------------------------------------------------------------------------
// 账号
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct MeView {
    id: String,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    is_admin: bool,
    /// 这个账号现在能用来登录的方式（[`accounts::account_login_methods`]）：`github` / `google`（绑了身份）、
    /// `email`（绑定了邮箱，而且 hub 配了 SMTP）、`token`（只有 `single`：管理员的 bootstrap token，或自己建过登录 token）。
    /// 可能是空的（邀请进来、还没建 token、没绑邮箱）。
    login_methods: Vec<&'static str>,
}

pub async fn me_view(state: &AppState, a: Account) -> ApiResult<MeView> {
    let login_methods = accounts::account_login_methods(state, &a).await?;
    let is_admin = accounts::is_effective_admin(&state.config, &a);
    Ok(MeView { id: a.id, display_name: a.display_name, email: a.email, is_admin, login_methods })
}

pub async fn me(State(state): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<MeView>> {
    Ok(Json(me_view(&state, auth.account).await?))
}

#[derive(Deserialize)]
pub struct PatchMe {
    display_name: String,
}

pub async fn patch_me(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    ApiJson(req): ApiJson<PatchMe>,
) -> ApiResult<Json<MeView>> {
    let name = clean_name(&req.display_name, 64, "display_name")?;
    state.store.update_display_name(&auth.account.id, &name).await?;
    let mut account = auth.account;
    account.display_name = name;
    Ok(Json(me_view(&state, account).await?))
}

// ---------------------------------------------------------------------------
// 浏览器会话
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct SessionView {
    id: String,
    user_agent: String,
    ip_prefix: String,
    created_at_ms: u64,
    last_seen_ms: u64,
    current: bool,
}

pub async fn list_sessions(State(state): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<Vec<SessionView>>> {
    let sessions = state.store.list_web_sessions(&auth.account.id, now_ms()).await?;
    Ok(Json(
        sessions
            .into_iter()
            .map(|s| SessionView {
                current: s.id == auth.session.id,
                id: s.id,
                user_agent: s.user_agent,
                ip_prefix: s.ip_prefix,
                created_at_ms: s.created_at_ms,
                last_seen_ms: s.last_seen_ms,
            })
            .collect(),
    ))
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    if state.store.delete_web_session(&auth.account.id, &id).await? {
        Ok(no_content())
    } else {
        Err(ApiError::not_found())
    }
}

// ---------------------------------------------------------------------------
// 登录 token
// ---------------------------------------------------------------------------

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    auth: Authed,
) -> ApiResult<Json<Vec<crate::store::AccountToken>>> {
    Ok(Json(state.store.list_account_tokens(&auth.account.id).await?))
}

#[derive(Deserialize)]
pub struct CreateToken {
    label: String,
}

#[derive(Serialize)]
pub struct CreatedToken {
    id: String,
    /// 明文只在创建时返回这一次。
    token: String,
}

pub async fn create_token(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    ApiJson(req): ApiJson<CreateToken>,
) -> ApiResult<Json<CreatedToken>> {
    let label = clean_name(&req.label, 64, "label")?;
    let token = new_secret();
    let created = state.store.create_account_token(&auth.account.id, &label, &hash_token(&token), now_ms()).await?;
    state
        .audit(NewAudit {
            event: "token_created",
            account_id: Some(auth.account.id),
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            detail: Some(serde_json::json!({ "token_id": created.id })),
            ..Default::default()
        })
        .await;
    Ok(Json(CreatedToken { id: created.id, token }))
}

/// `DELETE /api/me/tokens/{id}`（`single`）。删的是账号最后一个登录 token、而账号没有别的登录方式（邮箱、OAuth 身份，
/// 也不是 bootstrap 管理员）→ `409 last_login_method`。和解绑邮箱、解除 OAuth 绑定一起按账号串行
/// （[`accounts::begin_login_method_removal`]）。
pub async fn delete_token(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let removal = accounts::begin_login_method_removal(&state, &auth.account.id).await?;
    let tokens = state.store.list_account_tokens(&auth.account.id).await?;
    if !tokens.iter().any(|t| t.id == id) {
        return Err(ApiError::not_found());
    }
    // bootstrap 管理员的 token 不是库里的行，删它名下的 token 行不会少一种方式（`token` 一直在清单里）。
    let is_last_token = tokens.len() == 1 && !accounts::is_bootstrap_admin(&state, &auth.account.id);
    if is_last_token && removal.would_lock_out("token") {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "last_login_method",
            "this is the only way this account can sign in; add another sign-in method first",
        ));
    }
    if !state.store.delete_account_token(&auth.account.id, &id).await? {
        return Err(ApiError::not_found());
    }
    state
        .audit(NewAudit {
            event: "token_deleted",
            account_id: Some(auth.account.id),
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            detail: Some(serde_json::json!({ "token_id": id })),
            ..Default::default()
        })
        .await;
    Ok(no_content())
}

// ---------------------------------------------------------------------------
// 设备
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct DeviceView {
    id: String,
    name: String,
    kind: DeviceKind,
    platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    created_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen_ms: Option<u64>,
    online: bool,
    paired_with: Vec<String>,
}

fn device_view(d: Device, online: bool, mut paired_with: Vec<String>) -> DeviceView {
    paired_with.sort();
    DeviceView {
        id: d.id,
        name: d.name,
        kind: d.kind,
        platform: d.platform,
        origin: d.origin,
        created_at_ms: d.created_at_ms,
        last_seen_ms: d.last_seen_ms,
        online,
        paired_with,
    }
}

/// `GET /api/devices`：登录 cookie 或设备 token（`Authorization: Bearer`，只读）都行。
pub async fn list_devices(State(state): State<Arc<AppState>>, auth: AccountReader) -> ApiResult<Json<Vec<DeviceView>>> {
    let devices = state.store.list_devices(&auth.account_id).await?;
    let live: HashSet<&str> = devices.iter().map(|d| d.id.as_str()).collect();
    let mut peers: HashMap<String, Vec<String>> = HashMap::new();
    for (a, b) in state.store.list_pairings_of_account(&auth.account_id).await? {
        if live.contains(a.as_str()) && live.contains(b.as_str()) {
            peers.entry(a.clone()).or_default().push(b.clone());
            peers.entry(b).or_default().push(a);
        }
    }
    let views = devices
        .into_iter()
        .map(|d| {
            let online = state.registry.is_online(&d.id);
            let paired = peers.remove(&d.id).unwrap_or_default();
            device_view(d, online, paired)
        })
        .collect();
    Ok(Json(views))
}

/// 取账号名下、未吊销的设备；不属于该账号一律当作不存在。
async fn own_device(state: &AppState, auth: &Authed, id: &str) -> ApiResult<Device> {
    match state.store.get_device(id).await? {
        Some(d) if d.account_id == auth.account.id && d.revoked_at_ms.is_none() => Ok(d),
        _ => Err(ApiError::not_found()),
    }
}

#[derive(Deserialize)]
pub struct PatchDevice {
    name: String,
}

pub async fn patch_device(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
    ApiJson(req): ApiJson<PatchDevice>,
) -> ApiResult<Json<DeviceView>> {
    let mut device = own_device(&state, &auth, &id).await?;
    let name = clean_name(&req.name, 100, "name")?;
    state.store.rename_device(&id, &name).await?;
    device.name = name;
    let paired = state.store.peers_of(&id).await?;
    let online = state.registry.is_online(&id);
    Ok(Json(device_view(device, online, paired)))
}

/// `DELETE /api/devices/:id`：吊销（hub 侧）。和 `Control::Revoke` 共用 [`AppState::revoke_device`]。
pub async fn delete_device(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    own_device(&state, &auth, &id).await?;
    if state.revoke_device(&id, None).await? {
        Ok(no_content())
    } else {
        Err(ApiError::not_found())
    }
}

// ---------------------------------------------------------------------------
// 邀请（仅管理员）
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct InviteView {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    created_at_ms: u64,
    expires_at_ms: u64,
    used: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    used_at_ms: Option<u64>,
}

/// `GET /api/invites?limit=&cursor=`：最新的在前；还有下一页时响应头 `X-Next-Cursor` 给出游标。
pub async fn list_invites(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Query(q): Query<PageQuery>,
) -> ApiResult<Response> {
    auth.require_admin()?;
    let limit = q.limit()?;
    let after = q.cursor().map(decode_cursor).transpose()?;
    let rows = state.store.list_invites(limit + 1, after).await?;
    let (invites, next) = split_page(rows, limit, |i| encode_cursor(i.created_at_ms, &i.id));
    let views: Vec<InviteView> = invites
        .into_iter()
        .map(|i| InviteView {
            used: i.used_by.is_some(),
            id: i.id,
            note: i.note,
            created_at_ms: i.created_at_ms,
            expires_at_ms: i.expires_at_ms,
            used_by: i.used_by,
            used_at_ms: i.used_at_ms,
        })
        .collect();
    Ok(page_response(views, next))
}

#[derive(Deserialize)]
pub struct CreateInvite {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    ttl_hours: Option<u64>,
}

#[derive(Serialize)]
pub struct CreatedInvite {
    id: String,
    /// 明文邀请码只在创建时返回这一次。
    code: String,
    url: String,
    expires_at_ms: u64,
}

pub async fn create_invite(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    ApiJson(req): ApiJson<CreateInvite>,
) -> ApiResult<Json<CreatedInvite>> {
    auth.require_admin()?;
    let ttl_hours = req.ttl_hours.unwrap_or(7 * 24);
    if !(1..=24 * 365).contains(&ttl_hours) {
        return Err(ApiError::bad_request("ttl_hours must be between 1 and 8760"));
    }
    let note = match req.note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if n.chars().count() > 200 => return Err(ApiError::bad_request("note must be at most 200 characters")),
        other => other.map(str::to_string),
    };
    let code = random_hex(16);
    let now = now_ms();
    let expires_at_ms = now + ttl_hours * 3_600_000;
    let invite =
        state.store.create_invite(&auth.account.id, &hash_token(&code), note.as_deref(), now, expires_at_ms).await?;
    state
        .audit(NewAudit {
            event: "invite_created",
            account_id: Some(auth.account.id),
            ip_prefix: Some(ip_prefix(client.ctx.ip)),
            detail: Some(serde_json::json!({ "invite_id": invite.id })),
            ..Default::default()
        })
        .await;
    Ok(Json(CreatedInvite {
        url: format!("{}/invite/{}", state.config.public_url_str(), code),
        id: invite.id,
        code,
        expires_at_ms,
    }))
}

pub async fn delete_invite(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth.require_admin()?;
    if state.store.delete_invite(&id).await? {
        Ok(no_content())
    } else {
        Err(ApiError::not_found())
    }
}

// ---------------------------------------------------------------------------
// 管理
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct AdminStatus {
    mode: &'static str,
    version: &'static str,
    started_at_ms: u64,
    uptime_ms: u64,
    online_connections: usize,
    online_devices: usize,
    devices: u64,
    accounts: u64,
    queue_enabled: bool,
    db_size_bytes: u64,
}

pub async fn admin_status(State(state): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<AdminStatus>> {
    auth.require_admin()?;
    let stats = state.store.stats().await?;
    Ok(Json(AdminStatus {
        mode: state.config.mode.as_str(),
        version: VERSION,
        started_at_ms: state.started_at_ms,
        uptime_ms: now_ms().saturating_sub(state.started_at_ms),
        online_connections: state.registry.len(),
        online_devices: state.registry.permanent_count(),
        devices: stats.devices,
        accounts: stats.accounts,
        queue_enabled: false,
        db_size_bytes: stats.db_size_bytes,
    }))
}

/// `GET /api/admin/audit?limit=&cursor=`：最新的在前，游标是上一页最后一条的 id。
pub async fn admin_audit(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Query(q): Query<PageQuery>,
) -> ApiResult<Response> {
    auth.require_admin()?;
    let limit = q.limit()?;
    let before =
        q.cursor().map(|c| c.parse::<i64>().map_err(|_| ApiError::bad_request("invalid cursor"))).transpose()?;
    let rows = state.store.list_audit(limit + 1, before).await?;
    let (entries, next) = split_page(rows, limit, |e| e.id.to_string());
    Ok(page_response(entries, next))
}

/// 管理页的账号行：账号元数据，`is_admin` 是**有效**值，`admin_by_config` 标出它是靠 `ANYCON_ADMIN_EMAILS` 当上的
/// （这种管理员改不了库里的标记，要改配置）。
#[derive(Serialize)]
pub struct AdminAccountView {
    id: String,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    is_admin: bool,
    admin_by_config: bool,
    created_at_ms: u64,
}

/// 管理页账号列表的搜索参数。
#[derive(Deserialize)]
pub struct AccountSearch {
    /// 显示名或邮箱的子串（不区分大小写），或者正好等于账号 id。空白等于没给。
    #[serde(default)]
    q: Option<String>,
}

/// 搜索串最长多少个字符。
const MAX_SEARCH_CHARS: usize = 200;

/// 一个账号在管理页里的样子（`is_admin` 是有效值，`admin_by_config` 见 [`AdminAccountView`]）。
pub fn admin_account_view(state: &AppState, a: Account) -> AdminAccountView {
    AdminAccountView {
        is_admin: accounts::is_effective_admin(&state.config, &a),
        admin_by_config: accounts::admin_by_config(&state.config, &a),
        id: a.id,
        display_name: a.display_name,
        email: a.email,
        created_at_ms: a.created_at_ms,
    }
}

/// `GET /api/admin/accounts?q=&limit=&cursor=`：账号元数据，最早创建的在前（spec § 2.4）。
/// `q` 搜显示名和邮箱（子串，不区分大小写）或账号 id（精确）；分页同没有 `q` 时。
pub async fn admin_accounts(
    State(state): State<Arc<AppState>>,
    auth: Authed,
    Query(q): Query<PageQuery>,
    Query(search): Query<AccountSearch>,
) -> ApiResult<Response> {
    auth.require_admin()?;
    let limit = q.limit()?;
    let after = q.cursor().map(decode_cursor).transpose()?;
    let needle = search.q.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if needle.is_some_and(|n| n.chars().count() > MAX_SEARCH_CHARS) {
        return Err(ApiError::bad_request(format!("q must be at most {MAX_SEARCH_CHARS} characters")));
    }
    let rows = match needle {
        Some(needle) => state.store.search_accounts(needle, limit + 1, after).await?,
        None => state.store.list_accounts(limit + 1, after).await?,
    };
    let (rows, next) = split_page(rows, limit, |a| encode_cursor(a.created_at_ms, &a.id));
    let views: Vec<AdminAccountView> = rows.into_iter().map(|a| admin_account_view(&state, a)).collect();
    Ok(page_response(views, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip_and_validation() {
        let c = encode_cursor(1_700_000_000_123, "abc123");
        assert_eq!(decode_cursor(&c).unwrap(), (1_700_000_000_123, "abc123".to_string()));
        for bad in ["", "123", ".abc", "x.abc", "12.", "12.a b", "12.a'b", &format!("1.{}", "a".repeat(65))] {
            assert!(decode_cursor(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn page_split_reports_a_cursor_only_when_there_is_more() {
        let (items, next) = split_page(vec![1, 2, 3], 3, |n| n.to_string());
        assert_eq!((items, next), (vec![1, 2, 3], None));
        let (items, next) = split_page(vec![1, 2, 3, 4], 3, |n| n.to_string());
        assert_eq!((items, next.as_deref()), (vec![1, 2, 3], Some("3")));
    }

    #[test]
    fn limit_defaults_and_clamps() {
        let q = |l: Option<&str>| PageQuery { limit: l.map(str::to_string), cursor: None };
        assert_eq!(q(None).limit().unwrap(), 100);
        assert_eq!(q(Some("0")).limit().unwrap(), 1);
        assert_eq!(q(Some("7")).limit().unwrap(), 7);
        assert_eq!(q(Some("99999")).limit().unwrap(), 500);
        assert!(q(Some("abc")).limit().is_err());
        assert!(q(Some("-1")).limit().is_err());
    }
}
