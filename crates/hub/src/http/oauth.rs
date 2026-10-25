//! OAuth 登录与身份管理（GitHub、Google；contract 见 [`crate::oauth`]）。两种模式都有，配了凭据的提供方才存在，
//! 没配 / 不认识的提供方一律 `404 not_found`。
//!
//! - `GET /auth/oauth/{provider}/start`：登记一次授权流程（state 存哈希、PKCE、用途、`next`），种 `anycon_oauth` cookie，
//!   302 去提供方。请求的 `Host` 不是 `ANYCON_PUBLIC_URL` 的主机时先 302 到公网地址上的同一路径（回调只回得到公网域名；
//!   这里的目标只用配置的公网地址，不用 `Host` 拼链接）。每个 IP 每分钟 20 次。
//!
//!   这个跳转**至多一次**：目标 URL 的 query 末尾带标记 `hop=1`（[`HOP_PARAM`]），带着它的请求不再跳转，直接按
//!   公网地址上的请求处理。否则改写 `Host` 的反向代理（到了公网地址，hub 看到的 `Host` 仍不是公网主机）会让浏览器
//!   在 start 和公网地址之间无限重定向。标记只管跳转，处理请求时先把它从参数里去掉，不会进 state，也不会带给提供方。
//! - `GET /auth/callback/{provider}`：校验 state（cookie 必须一致、单次使用、未过期、属于这个提供方），用授权码换用户信息，
//!   按 [`crate::oauth::login`] 的规则登录 / 关联 / 注册，然后 302 回 Web UI。失败也是 302
//!   （`/login?oauth_error=<code>&provider=<p>`，关联用途回 `/settings/login?...`），码见 [`OAuthErrorCode`]。
//!   库 / 内部出错也是同样的 302（码 `server_error`）：回调是浏览器的顶层导航，回 JSON 它只会显示一屏原始文本。
//!   **不论成败都清掉 `anycon_oauth` cookie。**
//! - `GET /api/me/identities` · `DELETE /api/me/identities/{provider}`：账号已绑定的身份；解绑最后一种登录方式 → `409 last_login_method`。
//!
//! 日志里没有 `code`、`state`、PKCE verifier、access token、client secret、提供方的 `subject` 和邮箱。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use url::Url;

use crate::accounts;
use crate::app::AppState;
use crate::config::{Mode, OAuthProvider};
use crate::error::{ApiError, ApiResult};
use crate::oauth::login::{self, SignIn};
use crate::oauth::provider;
use crate::oauth::state::{self, COOKIE_NAME, STATE_TTL};
use crate::oauth::OAuthErrorCode;
use crate::store::{NewOAuthState, OAuthPurpose, OAuthState};
use crate::util::{ct_eq, hash_token, new_secret, now_ms};

use super::session::{cookie_secure, cookie_value, lookup_session, start_session, Authed, Client};

/// 授权码的长度上限（提供方的码都远短于此）。
const CODE_MAX_LEN: usize = 2048;
/// `state` 参数的长度上限（我们发的是 64 个 hex）。
const STATE_MAX_LEN: usize = 256;
/// start 跳到公网地址时加在 query 末尾的标记（`hop=1`）：带着它的请求已经跳过一次了，不再跳。
const HOP_PARAM: &str = "hop";

/// 提供方名字（路径参数）→ 已配置凭据的提供方；不认识 / 没配都是 `404 not_found`。
fn configured_provider(state: &AppState, name: &str) -> ApiResult<OAuthProvider> {
    OAuthProvider::parse(name).filter(|p| state.config.oauth_client(*p).is_some()).ok_or_else(ApiError::not_found)
}

/// query 字符串里的参数（百分号解码后）。同名参数取第一个。
struct Params(Vec<(String, String)>);

impl Params {
    fn parse(raw: Option<&str>) -> Self {
        Self(url::form_urlencoded::parse(raw.unwrap_or("").as_bytes()).into_owned().collect())
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// 去掉所有叫 `key` 的参数，返回其中有没有值为 `1` 的。
    fn take_flag(&mut self, key: &str) -> bool {
        let set = self.0.iter().any(|(k, v)| k == key && v == "1");
        self.0.retain(|(k, _)| k != key);
        set
    }
}

/// 302，`Cache-Control: no-store`（回调地址的 query 里有授权码）。`cookies` 按顺序各一个 `Set-Cookie`。
fn redirect(location: &str, cookies: impl IntoIterator<Item = HeaderValue>) -> ApiResult<Response> {
    let location =
        HeaderValue::from_str(location).map_err(|_| ApiError::internal("redirect target is not a valid header"))?;
    let mut res = Response::new(Body::empty());
    *res.status_mut() = StatusCode::FOUND;
    res.headers_mut().insert(header::LOCATION, location);
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for cookie in cookies {
        res.headers_mut().append(header::SET_COOKIE, cookie);
    }
    Ok(res)
}

/// 失败后去哪：关联用途回设置页，其余回登录页。
fn error_location(purpose: Option<OAuthPurpose>, code: OAuthErrorCode, provider: OAuthProvider) -> String {
    let page = if purpose == Some(OAuthPurpose::Link) { "/settings/login" } else { "/login" };
    format!("{page}?oauth_error={}&provider={}", code.as_str(), provider.as_str())
}

// ---------------------------------------------------------------------------
// start
// ---------------------------------------------------------------------------

/// 请求的 `Host`（`host[:port]`）：HTTP/1.1 的头，没有就退回 URI 里的 authority（HTTP/2）。
fn request_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
}

/// `Host` 是不是 `ANYCON_PUBLIC_URL` 的主机和端口（缺省端口按协议补）。
fn host_is_public(public_url: &Url, host: &str) -> bool {
    if host.contains(['@', '/', '\\', '?', '#', ' ']) {
        return false;
    }
    let Ok(h) = Url::parse(&format!("{}://{host}/", public_url.scheme())) else { return false };
    match (h.host_str(), public_url.host_str()) {
        (Some(a), Some(b)) => {
            a.eq_ignore_ascii_case(b) && h.port_or_known_default() == public_url.port_or_known_default()
        }
        _ => false,
    }
}

/// `GET /auth/oauth/{provider}/start`。
pub async fn start(
    State(app): State<Arc<AppState>>,
    Path(provider): Path<String>,
    client: Client,
    headers: HeaderMap,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let provider = configured_provider(&app, &provider)?;
    let p = provider.as_str();

    // 标记先取下来：它只管跳转，后面的处理看不到它。
    let mut params = Params::parse(query.as_deref());
    let hopped = params.take_flag(HOP_PARAM);

    // 回调只回得到公网地址：从别的地址（局域网 IP、旧域名）来的，先去公网地址上重走一遍。
    // 目标由配置的公网地址 + 固定的路径 + 原样的 query + 跳转标记组成，没有用到请求的 `Host`。
    // 只跳一次：带着标记的请求不管 `Host` 是什么都往下走（改写 `Host` 的反向代理后面，否则会一直跳）。
    if !hopped {
        if let Some(host) = request_host(&headers, &uri) {
            if !host_is_public(&app.config.public_url, &host) {
                let query = query.as_deref().filter(|q| !q.is_empty()).map(|q| format!("{q}&")).unwrap_or_default();
                return redirect(
                    &format!("{}/auth/oauth/{p}/start?{query}{HOP_PARAM}=1", app.config.public_url_str()),
                    [],
                );
            }
        }
    }

    if let Err(wait) = app.oauth.check_start(client.ctx.ip) {
        tracing::info!(limit = "oauth_start", "rate limited");
        return Err(ApiError::rate_limited(wait));
    }

    let next_path = params.get("next").and_then(state::sanitize_next);
    let (purpose, account_id, invite_hash) = if params.get("link") == Some("1") {
        // 关联身份必须已经登录：把账号记在 state 里，回调时 cookie 还得是这个账号。
        match lookup_session(&app, &headers).await? {
            Some((_, account)) => (OAuthPurpose::Link, Some(account.id), None),
            None => {
                let location = error_location(None, OAuthErrorCode::NotSignedIn, provider);
                return redirect(&location, []);
            }
        }
    } else {
        // 邀请码只有 `single` 有（`multi` 开放注册，带了也忽略）。
        match params.get("invite").map(str::trim).filter(|c| !c.is_empty()) {
            Some(code) if app.config.mode == Mode::Single => (OAuthPurpose::Invite, None, Some(hash_token(code))),
            _ => (OAuthPurpose::Login, None, None),
        }
    };

    let state_token = new_secret();
    let verifier = state::new_verifier();
    let Some(authorize) = provider::authorize_url(&app.config, provider, &state_token, &state::challenge(&verifier))
    else {
        return Err(ApiError::internal("the OAuth authorize endpoint is not a valid URL"));
    };
    let now = now_ms();
    app.store
        .put_oauth_state(
            NewOAuthState {
                state_hash: hash_token(&state_token),
                provider: p.to_string(),
                purpose,
                account_id,
                invite_hash,
                next_path,
                pkce_verifier: verifier,
                expires_at_ms: now + STATE_TTL.as_millis() as u64,
            },
            now,
        )
        .await?;
    tracing::info!(provider = p, purpose = purpose.as_str(), "oauth sign-in started");
    redirect(&authorize, [state::set_cookie_header(&state_token, cookie_secure(&app, &client))])
}

// ---------------------------------------------------------------------------
// callback
// ---------------------------------------------------------------------------

/// 一次回调的结果（还没变成响应）。
enum Outcome {
    /// 登录成功：新会话的 `Set-Cookie` 和登录后去哪。
    SignedIn { session_cookie: HeaderValue, next: String },
    /// 关联成功。
    Linked,
    /// 失败。`purpose` 是 state 里记的用途；state 本身就不对时不知道用途，是 `None`。
    Failed { purpose: Option<OAuthPurpose>, code: OAuthErrorCode },
}

fn failed(purpose: OAuthPurpose, code: OAuthErrorCode) -> ApiResult<Outcome> {
    Ok(Outcome::Failed { purpose: Some(purpose), code })
}

/// `GET /auth/callback/{provider}?code=&state=`（或提供方带来的 `?error=`）。
///
/// 不返回 `ApiError`：这是浏览器的顶层导航，任何失败（含库 / 内部错误）都要 302 回 Web UI，让它显示一句话。
pub async fn callback(
    State(app): State<Arc<AppState>>,
    Path(provider): Path<String>,
    client: Client,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let provider = configured_provider(&app, &provider)?;
    let secure = cookie_secure(&app, &client);
    let params = Params::parse(query.as_deref());
    let outcome = run_callback(&app, &client, &headers, provider, &params).await;

    // 会话 cookie 在前、清 cookie 在后：只看第一个 `Set-Cookie` 的客户端（测试、简单的脚本）拿到的是会话。
    let mut response = match outcome {
        Outcome::SignedIn { session_cookie, next } => redirect(&state::location_value(&next), [session_cookie])?,
        Outcome::Linked => redirect(&format!("/settings/login?linked={}", provider.as_str()), [])?,
        Outcome::Failed { purpose, code } => {
            tracing::info!(
                provider = provider.as_str(),
                purpose = purpose.map_or("unknown", OAuthPurpose::as_str),
                code = code.as_str(),
                "oauth sign-in did not complete"
            );
            // 登录用途的失败写审计（state 都不对的请求不知道用途，也不是一次登录尝试，不写）。
            if purpose == Some(OAuthPurpose::Login) {
                login::audit(&app, &client, "login_fail", None, json!({ "method": provider.as_str() })).await;
            }
            redirect(&error_location(purpose, code, provider), [])?
        }
    };
    response.headers_mut().append(header::SET_COOKIE, state::clear_cookie_header(secure));
    Ok(response)
}

/// 库 / 内部出错的失败：细节已经在 [`ApiError::internal`] 里记过日志，这里只补上下文（提供方、用途），
/// 日志里没有 `code`、`state`、token、`subject`、邮箱。
fn server_error(provider: OAuthProvider, purpose: Option<OAuthPurpose>, e: &ApiError) -> Outcome {
    tracing::warn!(
        provider = provider.as_str(),
        purpose = purpose.map_or("unknown", OAuthPurpose::as_str),
        status = e.status.as_u16(),
        error_code = e.code,
        "oauth callback hit an internal error"
    );
    Outcome::Failed { purpose, code: OAuthErrorCode::ServerError }
}

async fn run_callback(
    app: &Arc<AppState>,
    client: &Client,
    headers: &HeaderMap,
    provider: OAuthProvider,
    params: &Params,
) -> Outcome {
    let saved = match claim_state(app, headers, provider, params).await {
        Ok(Some(saved)) => saved,
        Ok(None) => return Outcome::Failed { purpose: None, code: OAuthErrorCode::InvalidState },
        Err(e) => return server_error(provider, None, &e),
    };
    let purpose = saved.purpose;
    match complete(app, client, headers, provider, params, &saved).await {
        Ok(outcome) => outcome,
        Err(e) => server_error(provider, Some(purpose), &e),
    }
}

/// 校验并取走 state：URL 里有，cookie 里有且一致（常数时间），库里有、没过期、属于这个提供方；取走即作废（单次使用）。
/// 任何一项不满足是 `Ok(None)`（`invalid_state`）。
async fn claim_state(
    app: &Arc<AppState>,
    headers: &HeaderMap,
    provider: OAuthProvider,
    params: &Params,
) -> ApiResult<Option<OAuthState>> {
    let (Some(state_param), Some(cookie)) = (params.get("state"), cookie_value(headers, COOKIE_NAME)) else {
        return Ok(None);
    };
    if state_param.is_empty() || state_param.len() > STATE_MAX_LEN || !ct_eq(&cookie, state_param) {
        return Ok(None);
    }
    let Some(saved) = app.store.take_oauth_state(&hash_token(state_param), now_ms()).await? else {
        return Ok(None);
    };
    Ok((saved.provider == provider.as_str()).then_some(saved))
}

/// state 通过之后的部分：提供方的错误、换 token、取用户信息、对应到账号。
async fn complete(
    app: &Arc<AppState>,
    client: &Client,
    headers: &HeaderMap,
    provider: OAuthProvider,
    params: &Params,
    saved: &OAuthState,
) -> ApiResult<Outcome> {
    use OAuthErrorCode as E;
    let p = provider.as_str();
    let purpose = saved.purpose;

    // 提供方带回了错误：用户点了拒绝是 `cancelled`，其余都是 `provider_error`。
    if let Some(error) = params.get("error") {
        let code = if error == "access_denied" { E::Cancelled } else { E::ProviderError };
        return failed(purpose, code);
    }
    let Some(code) = params.get("code").filter(|c| !c.is_empty() && c.len() <= CODE_MAX_LEN) else {
        return failed(purpose, E::ProviderError);
    };

    // 关联：发起时的账号必须还登录着（先查，不必为一个注定失败的请求去调提供方）。
    if purpose == OAuthPurpose::Link {
        let current = lookup_session(app, headers).await?;
        match (&current, &saved.account_id) {
            (Some((_, account)), Some(expected)) if account.id == *expected => {}
            _ => return failed(purpose, E::NotSignedIn),
        }
    }

    // 换 token、取用户信息（各 10 秒超时）。失败原因只记类别和状态码。
    let Some(http) = app.oauth.http() else { return failed(purpose, E::ProviderError) };
    let profile = match provider::fetch_profile(http, &app.config, provider, code, &saved.pkce_verifier).await {
        Ok(profile) => profile,
        Err(e) => {
            tracing::warn!(provider = p, error = %e, "oauth: the provider call failed");
            return failed(purpose, E::ProviderError);
        }
    };

    // 对应到账号。
    if purpose == OAuthPurpose::Link {
        let Some(account_id) = saved.account_id.as_deref() else { return failed(purpose, E::NotSignedIn) };
        return match login::link(app, client, account_id, provider, &profile).await? {
            None => Ok(Outcome::Linked),
            Some(code) => failed(purpose, code),
        };
    }
    let account = match login::sign_in(app, client, provider, &profile, saved.invite_hash.as_deref()).await? {
        SignIn::Account(account) => account,
        SignIn::Rejected(code) => return failed(purpose, code),
    };
    let session_cookie = start_session(app, &account.id, client).await?;
    login::audit(app, client, "login_ok", Some(&account.id), json!({ "method": p })).await;
    let next = saved.next_path.as_deref().and_then(state::sanitize_next).unwrap_or_else(|| "/".to_string());
    Ok(Outcome::SignedIn { session_cookie, next })
}

// ---------------------------------------------------------------------------
// 身份管理
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct IdentityView {
    provider: String,
    created_at_ms: u64,
}

/// `GET /api/me/identities`：`[{ provider, created_at_ms }]`。不返回也不存用户名、邮箱和提供方的用户 id。
pub async fn list_identities(State(app): State<Arc<AppState>>, auth: Authed) -> ApiResult<Json<Vec<IdentityView>>> {
    let identities = app.store.list_identities(&auth.account.id).await?;
    Ok(Json(
        identities.into_iter().map(|i| IdentityView { provider: i.provider, created_at_ms: i.created_at_ms }).collect(),
    ))
}

/// `DELETE /api/me/identities/{provider}`：解绑。没绑过也是 `204`；这是账号唯一的登录方式 → `409 last_login_method`
/// （和解绑邮箱、删登录 token 一起按账号串行，见 [`accounts::begin_login_method_removal`]）。
pub async fn delete_identity(
    State(app): State<Arc<AppState>>,
    auth: Authed,
    client: Client,
    Path(provider): Path<String>,
) -> ApiResult<Response> {
    let provider = OAuthProvider::parse(&provider).ok_or_else(ApiError::not_found)?;
    let p = provider.as_str();
    // 「检查还剩别的登录方式」和「删除」之间不让别的移除（解绑邮箱、另一次解绑、删 token）插进来，
    // 判断用锁里重读的账号，不用请求开始时的快照。
    let removal = accounts::begin_login_method_removal(&app, &auth.account.id).await?;
    let linked = app.store.list_identities(&auth.account.id).await?;
    if !linked.iter().any(|i| i.provider == p) {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    // 只有「现在能用来登录」的才算数：提供方已经不在配置里的身份不是登录方式，删它不会让账号少一种。
    if removal.would_lock_out(p) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "last_login_method",
            "this is the only way this account can sign in; add another sign-in method first",
        ));
    }
    app.store.unlink_identity(&auth.account.id, p).await?;
    login::audit(&app, &client, "identity_unlinked", Some(&auth.account.id), json!({ "provider": p })).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(url: &str) -> Url {
        Url::parse(url).unwrap()
    }

    #[test]
    fn only_the_public_host_counts_as_the_public_origin() {
        let https = public("https://hub.example.com");
        for host in ["hub.example.com", "HUB.example.com", "hub.example.com:443"] {
            assert!(host_is_public(&https, host), "{host}");
        }
        for host in [
            "192.168.1.10:8080",
            "hub.example.com:8443",
            "hub.example.com:80",
            "evil.example",
            "evil.example/hub.example.com",
            "user@hub.example.com",
            "hub.example.com@evil.example",
            "hub.example.com.evil.example",
            "",
            "hub example.com",
        ] {
            assert!(!host_is_public(&https, host), "{host:?}");
        }
        let custom = public("http://hub.lan:8080");
        assert!(host_is_public(&custom, "hub.lan:8080"));
        assert!(!host_is_public(&custom, "hub.lan"), "the default port 80 is not 8080");
        assert!(!host_is_public(&custom, "hub.lan:9090"));
        let v6 = public("https://[::1]:8443");
        assert!(host_is_public(&v6, "[::1]:8443"));
        assert!(!host_is_public(&v6, "[::2]:8443"));
    }

    #[test]
    fn failures_go_to_the_login_page_except_link_attempts() {
        let g = OAuthProvider::Github;
        assert_eq!(
            error_location(Some(OAuthPurpose::Login), OAuthErrorCode::Cancelled, g),
            "/login?oauth_error=cancelled&provider=github"
        );
        assert_eq!(
            error_location(None, OAuthErrorCode::InvalidState, OAuthProvider::Google),
            "/login?oauth_error=invalid_state&provider=google"
        );
        assert_eq!(
            error_location(Some(OAuthPurpose::Invite), OAuthErrorCode::InviteUsed, g),
            "/login?oauth_error=invite_used&provider=github"
        );
        assert_eq!(
            error_location(Some(OAuthPurpose::Link), OAuthErrorCode::IdentityTaken, g),
            "/settings/login?oauth_error=identity_taken&provider=github"
        );
    }

    #[test]
    fn every_error_code_has_its_stable_name() {
        use OAuthErrorCode::*;
        let all = [
            (Cancelled, "cancelled"),
            (ProviderError, "provider_error"),
            (InvalidState, "invalid_state"),
            (RegistrationClosed, "registration_closed"),
            (InviteInvalid, "invite_invalid"),
            (InviteExpired, "invite_expired"),
            (InviteUsed, "invite_used"),
            (IdentityTaken, "identity_taken"),
            (AlreadyLinked, "already_linked"),
            (NotSignedIn, "not_signed_in"),
            (ServerError, "server_error"),
        ];
        for (code, name) in all {
            assert_eq!(code.as_str(), name);
        }
    }

    #[test]
    fn query_parameters_are_decoded_and_the_first_one_wins() {
        let p = Params::parse(Some("next=%2Fchat%3Fa%3D1&link=1&next=%2Fother&invite=a+b"));
        assert_eq!(p.get("next"), Some("/chat?a=1"));
        assert_eq!(p.get("link"), Some("1"));
        assert_eq!(p.get("invite"), Some("a b"));
        assert_eq!(p.get("missing"), None);
        assert_eq!(Params::parse(None).get("next"), None);
    }

    #[test]
    fn the_hop_marker_is_taken_out_of_the_parameters() {
        let mut p = Params::parse(Some("next=%2Fa&hop=1&link=1"));
        assert!(p.take_flag(HOP_PARAM));
        assert_eq!((p.get("hop"), p.get("next"), p.get("link")), (None, Some("/a"), Some("1")));
        // 不是 1 的不算标记，但同样去掉；有一个 1 就算
        let mut p = Params::parse(Some("hop=0"));
        assert!(!p.take_flag(HOP_PARAM) && p.get("hop").is_none());
        let mut p = Params::parse(Some("hop=0&hop=1"));
        assert!(p.take_flag(HOP_PARAM));
        assert!(!Params::parse(None).take_flag(HOP_PARAM));
    }
}
