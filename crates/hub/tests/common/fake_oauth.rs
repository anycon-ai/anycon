//! 假的 OAuth 提供方：一个监听 `127.0.0.1` 随机端口的 axum 服务，同时扮演 GitHub 和 Google
//! （路径前缀 `/github/…`、`/google/…`），通过 `Config.oauth_endpoint_overrides` 接到 hub 上。测试从不联网。
//!
//! 它像真的提供方一样检查 hub 发来的东西：授权请求带 PKCE（S256）和 state、换 token 时 `code_verifier` 必须和授权时的
//! `code_challenge` 对得上、`client_secret` 和 `redirect_uri` 要对、授权码只能用一次、用户信息接口要带 `Bearer` token
//! （GitHub 的还要有 `User-Agent`）。可以让它：返回已验证 / 未验证 / 没有的邮箱、让用户点「拒绝」、回错误、回得很慢。
//!
//! 用法：`let fake = FakeOAuth::start().await;` → 配进 hub（[`FakeOAuth::configure`]）→ `fake.set_user("github", user)`
//! 设定「下一个在授权页登录的人」→ 测试自己扮演浏览器：GET hub 的 `/auth/oauth/<p>/start`，
//! 把 `Location` 交给 [`FakeOAuth::authorize`]，再把它回的回调地址 GET 回 hub。
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_hub::config::{OAuthClient, OAuthEndpoints, OAuthProvider};
use anycon_hub::Config;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const CLIENT_ID: &str = "fake-client-id";
pub const CLIENT_SECRET: &str = "fake-client-secret-0f9e8d7c6b5a";

/// 提供方那边的一个用户。
#[derive(Clone, Debug)]
pub struct FakeUser {
    /// GitHub 是数字 id（字符串形式），Google 是 `sub`。
    pub subject: String,
    /// GitHub 的 `login`。
    pub login: String,
    pub name: Option<String>,
    /// 主邮箱（GitHub 的 `primary` 那条 / Google 的 `email`）。`None` = 提供方没有给邮箱。
    pub email: Option<String>,
    /// 主邮箱是否已验证（GitHub 的 `verified` / Google 的 `email_verified`）。
    pub verified: bool,
    /// GitHub 的非主邮箱 `(邮箱, 是否已验证)`。
    pub extra_emails: Vec<(String, bool)>,
}

impl FakeUser {
    pub fn new(subject: &str) -> Self {
        Self {
            subject: subject.into(),
            login: format!("login-{subject}"),
            name: None,
            email: None,
            verified: false,
            extra_emails: Vec::new(),
        }
    }

    pub fn name(mut self, name: &str) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn login(mut self, login: &str) -> Self {
        self.login = login.into();
        self
    }

    /// 主邮箱，`verified` 是提供方是否声明它已验证。
    pub fn email(mut self, email: &str, verified: bool) -> Self {
        self.email = Some(email.into());
        self.verified = verified;
        self
    }

    pub fn extra_email(mut self, email: &str, verified: bool) -> Self {
        self.extra_emails.push((email.into(), verified));
        self
    }
}

/// 假提供方收到的一个请求（给测试断言用）。
#[derive(Clone, Debug, Default)]
pub struct Recorded {
    /// `authorize` / `token` / `user` / `emails` / `userinfo`
    pub endpoint: String,
    pub provider: String,
    /// 请求头（名字小写）。
    pub headers: HashMap<String, String>,
    /// 授权请求的 query，或换 token 请求的表单。
    pub params: HashMap<String, String>,
}

/// 假提供方的行为开关。
#[derive(Clone, Debug, Default)]
pub struct Behavior {
    pub token_delay: Duration,
    pub userinfo_delay: Duration,
    pub emails_delay: Duration,
    /// 换 token 直接回这个状态码（`{"error":"server_error"}`）。
    pub token_status: Option<u16>,
    /// 换 token 回 `200` + `{"error":"bad_verification_code"}`（GitHub 的做法）。
    pub token_error_in_200: bool,
    pub userinfo_status: Option<u16>,
    pub emails_status: Option<u16>,
    /// 下一次授权页让用户点「拒绝」（一次性）。
    pub deny_next_authorize: bool,
    /// 下一次授权页回这个 `error`（一次性）。
    pub fail_next_authorize_with: Option<String>,
}

struct Grant {
    provider: String,
    user: FakeUser,
    challenge: String,
    redirect_uri: String,
}

#[derive(Default)]
struct Inner {
    /// 每个提供方「下一个在授权页登录的人」。
    next_user: Mutex<HashMap<String, FakeUser>>,
    codes: Mutex<HashMap<String, Grant>>,
    tokens: Mutex<HashMap<String, (String, FakeUser)>>,
    behavior: Mutex<Behavior>,
    log: Mutex<Vec<Recorded>>,
}

pub struct FakeOAuth {
    pub addr: SocketAddr,
    inner: Arc<Inner>,
}

fn json_error(status: u16, error: &str) -> Response {
    (StatusCode::from_u16(status).unwrap(), Json(json!({ "error": error }))).into_response()
}

fn lower_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_ascii_lowercase(), v.to_str().unwrap_or("<binary>").to_string()))
        .collect()
}

impl FakeOAuth {
    pub async fn start() -> Self {
        let inner = Arc::new(Inner::default());
        let app = Router::new()
            .route("/{provider}/authorize", get(authorize))
            .route("/{provider}/token", post(token))
            .route("/{provider}/user", get(github_user))
            .route("/{provider}/user/emails", get(github_emails))
            .route("/{provider}/userinfo", get(google_userinfo))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { addr, inner }
    }

    pub fn endpoints(&self, provider: &str) -> OAuthEndpoints {
        let base = format!("http://{}/{provider}", self.addr);
        OAuthEndpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            userinfo: if provider == "github" { format!("{base}/user") } else { format!("{base}/userinfo") },
            emails: (provider == "github").then(|| format!("{base}/user/emails")),
        }
    }

    /// 把这些提供方配进 hub：凭据是假的 client id / secret，端点指向这个假提供方。
    pub fn configure(&self, config: &mut Config, providers: &[OAuthProvider]) {
        for provider in providers {
            let client = Some(OAuthClient { client_id: CLIENT_ID.into(), client_secret: CLIENT_SECRET.into() });
            let endpoints = Some(self.endpoints(provider.as_str()));
            match provider {
                OAuthProvider::Github => {
                    config.oauth_github = client;
                    config.oauth_endpoint_overrides.github = endpoints;
                }
                OAuthProvider::Google => {
                    config.oauth_google = client;
                    config.oauth_endpoint_overrides.google = endpoints;
                }
            }
        }
    }

    /// 设定下一个在 `provider` 授权页登录的人。
    pub fn set_user(&self, provider: &str, user: FakeUser) {
        self.inner.next_user.lock().unwrap().insert(provider.into(), user);
    }

    pub fn set_behavior(&self, f: impl FnOnce(&mut Behavior)) {
        f(&mut self.inner.behavior.lock().unwrap());
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.log.lock().unwrap().clone()
    }

    pub fn requests_to(&self, endpoint: &str) -> Vec<Recorded> {
        self.requests().into_iter().filter(|r| r.endpoint == endpoint).collect()
    }

    /// 浏览器带着 hub 给的授权地址去提供方：返回提供方对它的回答（302，`Location` 是 hub 的回调地址）。
    pub async fn authorize(&self, authorize_url: &str) -> reqwest::Response {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        client.get(authorize_url).send().await.unwrap()
    }
}

async fn authorize(
    State(inner): State<Arc<Inner>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    inner.log.lock().unwrap().push(Recorded {
        endpoint: "authorize".into(),
        provider: provider.clone(),
        headers: lower_headers(&headers),
        params: q.clone(),
    });
    // 和真的提供方一样先检查请求本身
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    if get("response_type") != "code" || get("client_id") != CLIENT_ID {
        return (StatusCode::BAD_REQUEST, "bad authorize request").into_response();
    }
    if get("code_challenge_method") != "S256" || get("code_challenge").len() != 43 || get("state").is_empty() {
        return (StatusCode::BAD_REQUEST, "PKCE (S256) and state are required").into_response();
    }
    let redirect_uri = get("redirect_uri");
    let state = get("state");
    let back = |extra: &str| {
        let sep = if redirect_uri.contains('?') { '&' } else { '?' };
        let location = format!("{redirect_uri}{sep}{extra}&state={}", urlencoding_encode(&state));
        (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
    };
    {
        let mut behavior = inner.behavior.lock().unwrap();
        if std::mem::take(&mut behavior.deny_next_authorize) {
            return back("error=access_denied");
        }
        if let Some(error) = behavior.fail_next_authorize_with.take() {
            return back(&format!("error={error}"));
        }
    }
    let Some(user) = inner.next_user.lock().unwrap().get(&provider).cloned() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "the test did not set a user for this provider").into_response();
    };
    let code = format!("code-{}", anycon_hub::util::random_hex(12));
    inner.codes.lock().unwrap().insert(
        code.clone(),
        Grant { provider, user, challenge: get("code_challenge"), redirect_uri: redirect_uri.clone() },
    );
    back(&format!("code={code}"))
}

/// 够用的百分号编码（state 是 hex，用不到，留着防止测试里手写别的值）。
fn urlencoding_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

async fn token(
    State(inner): State<Arc<Inner>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    inner.log.lock().unwrap().push(Recorded {
        endpoint: "token".into(),
        provider: provider.clone(),
        headers: lower_headers(&headers),
        params: form.clone(),
    });
    let (delay, status, error_in_200) = {
        let b = inner.behavior.lock().unwrap();
        (b.token_delay, b.token_status, b.token_error_in_200)
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return json_error(status, "server_error");
    }
    if error_in_200 {
        return Json(json!({ "error": "bad_verification_code", "error_description": "The code passed is incorrect or expired." }))
            .into_response();
    }
    let get = |k: &str| form.get(k).cloned().unwrap_or_default();
    // 授权码只能用一次
    let Some(grant) = inner.codes.lock().unwrap().remove(&get("code")) else {
        return json_error(400, "invalid_grant");
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(get("code_verifier").as_bytes()));
    if grant.provider != provider
        || get("client_id") != CLIENT_ID
        || get("client_secret") != CLIENT_SECRET
        || get("redirect_uri") != grant.redirect_uri
        || challenge != grant.challenge
    {
        return json_error(400, "invalid_grant");
    }
    let access_token = format!("tok-{}", anycon_hub::util::random_hex(16));
    inner.tokens.lock().unwrap().insert(access_token.clone(), (provider, grant.user));
    Json(json!({ "access_token": access_token, "token_type": "bearer", "scope": "read:user,user:email" }))
        .into_response()
}

/// `Authorization: Bearer <token>` → 这个 token 对应的用户。
fn bearer_user(inner: &Inner, provider: &str, headers: &HeaderMap) -> Option<FakeUser> {
    let token = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?.to_string();
    let (p, user) = inner.tokens.lock().unwrap().get(&token)?.clone();
    (p == provider).then_some(user)
}

async fn github_user(State(inner): State<Arc<Inner>>, Path(provider): Path<String>, headers: HeaderMap) -> Response {
    inner.log.lock().unwrap().push(Recorded {
        endpoint: "user".into(),
        provider: provider.clone(),
        headers: lower_headers(&headers),
        params: HashMap::new(),
    });
    let (delay, status) = {
        let b = inner.behavior.lock().unwrap();
        (b.userinfo_delay, b.userinfo_status)
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return json_error(status, "server_error");
    }
    // GitHub 的 API 没有 User-Agent 就拒绝
    if !headers.contains_key(header::USER_AGENT) {
        return (
            StatusCode::FORBIDDEN,
            "Request forbidden by administrative rules. Please make sure your request has a User-Agent header",
        )
            .into_response();
    }
    let Some(user) = bearer_user(&inner, &provider, &headers) else { return json_error(401, "bad_credentials") };
    Json(json!({
        "login": user.login,
        "id": user.subject.parse::<u64>().expect("GitHub ids are numbers in tests"),
        "name": user.name,
        "email": null,
    }))
    .into_response()
}

async fn github_emails(State(inner): State<Arc<Inner>>, Path(provider): Path<String>, headers: HeaderMap) -> Response {
    inner.log.lock().unwrap().push(Recorded {
        endpoint: "emails".into(),
        provider: provider.clone(),
        headers: lower_headers(&headers),
        params: HashMap::new(),
    });
    let (delay, status) = {
        let b = inner.behavior.lock().unwrap();
        (b.emails_delay, b.emails_status)
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return json_error(status, "server_error");
    }
    if !headers.contains_key(header::USER_AGENT) {
        return (StatusCode::FORBIDDEN, "User-Agent required").into_response();
    }
    let Some(user) = bearer_user(&inner, &provider, &headers) else { return json_error(401, "bad_credentials") };
    let mut list: Vec<Value> = user
        .email
        .iter()
        .map(|e| json!({ "email": e, "primary": true, "verified": user.verified, "visibility": "private" }))
        .collect();
    list.extend(
        user.extra_emails
            .iter()
            .map(|(e, v)| json!({ "email": e, "primary": false, "verified": v, "visibility": null })),
    );
    Json(Value::Array(list)).into_response()
}

async fn google_userinfo(
    State(inner): State<Arc<Inner>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> Response {
    inner.log.lock().unwrap().push(Recorded {
        endpoint: "userinfo".into(),
        provider: provider.clone(),
        headers: lower_headers(&headers),
        params: HashMap::new(),
    });
    let (delay, status) = {
        let b = inner.behavior.lock().unwrap();
        (b.userinfo_delay, b.userinfo_status)
    };
    tokio::time::sleep(delay).await;
    if let Some(status) = status {
        return json_error(status, "server_error");
    }
    let Some(user) = bearer_user(&inner, &provider, &headers) else { return json_error(401, "invalid_token") };
    let mut body = json!({ "sub": user.subject, "name": user.name, "picture": "https://example.invalid/p.png" });
    if let Some(email) = &user.email {
        body["email"] = json!(email);
        body["email_verified"] = json!(user.verified);
    }
    Json(body).into_response()
}
