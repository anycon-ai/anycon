//! 每个提供方的差异：授权地址、换 token、取用户信息，以及对响应的解析。
//!
//! | | GitHub | Google |
//! |---|---|---|
//! | scope | `read:user user:email` | `openid email profile` |
//! | 用户 id（`subject`） | `/user` 的数字 `id`（转成字符串） | userinfo 的 `sub` |
//! | 已验证邮箱 | `/user/emails` 里 `primary && verified` 的那一条 | userinfo 的 `email`，且 `email_verified == true` |
//! | 显示名候选 | `name`、`login` | `name` |
//!
//! Google 的 `id_token` 不解析：用 access token 调 userinfo 端点，信任它的回答。
//! access token 只在内存里用来调这一两次接口，不存、不记日志（[`AccessToken`] 的 `Debug` 不输出内容）。
//! 错误（[`ProviderError`]）只带类别、步骤和状态码——不带 URL，不带响应内容（里面可能有 token）。

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::client::{HttpClient, HttpError, Response};
use crate::config::{Config, OAuthEndpoints, OAuthProvider};
use crate::mail::normalize_email;

/// 提供方的稳定用户 id 的长度上限（字节）。
const SUBJECT_MAX_BYTES: usize = 255;

/// 出站请求的 `User-Agent`（GitHub API 要求有）。
const USER_AGENT: &str = concat!("anycon-hub/", env!("CARGO_PKG_VERSION"));

/// 授权码换来的 access token。`Debug` 不输出内容。
pub struct AccessToken(String);

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccessToken(<redacted>)")
    }
}

/// 提供方告诉我们的、这个用户的信息（只留登录需要的）。`Debug` 不输出内容。
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    /// 提供方的稳定用户 id。
    pub subject: String,
    /// 提供方**声明已验证**的邮箱（规范化后）。没有、没验证、格式不对都是 `None`——绝不用来匹配账号或当账号邮箱。
    pub email: Option<String>,
    /// 显示名候选，按优先级。
    pub names: Vec<String>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile").field("has_verified_email", &self.email.is_some()).finish_non_exhaustive()
    }
}

/// 调用的哪一步（日志用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Token,
    User,
    Emails,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Step::Token => "token exchange",
            Step::User => "user info",
            Step::Emails => "email list",
        })
    }
}

/// 和提供方交互失败。都当 `provider_error`；日志里只会有这里的文字。
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("the provider is not available")]
    NotAvailable,
    #[error("{step}: {source}")]
    Http { step: Step, source: HttpError },
    #[error("{step}: the provider answered HTTP {status}")]
    Status { step: Step, status: u16 },
    /// 提供方拒绝了授权码（`reason` 是它给的错误码，已过滤成短小的小写标识）。
    #[error("{step}: the provider rejected the request ({reason})")]
    Rejected { step: Step, reason: String },
    #[error("{step}: the response is not what was expected")]
    Malformed { step: Step },
}

// ---------------------------------------------------------------------------
// 授权地址
// ---------------------------------------------------------------------------

/// 授权回调地址：`${ANYCON_PUBLIC_URL}/auth/callback/<provider>`。只用配置里的公网地址，不用请求的 `Host`。
pub fn redirect_uri(config: &Config, provider: OAuthProvider) -> String {
    format!("{}/auth/callback/{}", config.public_url_str(), provider.as_str())
}

pub fn scope(provider: OAuthProvider) -> &'static str {
    match provider {
        OAuthProvider::Github => "read:user user:email",
        OAuthProvider::Google => "openid email profile",
    }
}

/// 带 PKCE（S256）和 `state` 的授权地址。提供方没配凭据，或端点配置不是合法 URL 时返回 `None`。
pub fn authorize_url(config: &Config, provider: OAuthProvider, state: &str, code_challenge: &str) -> Option<String> {
    let client = config.oauth_client(provider)?;
    let mut url = Url::parse(&config.oauth_endpoints(provider).authorize).ok()?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &redirect_uri(config, provider))
        .append_pair("scope", scope(provider))
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Some(url.into())
}

// ---------------------------------------------------------------------------
// 换 token、取用户信息
// ---------------------------------------------------------------------------

/// 用授权码换 access token，再取用户信息。每一步的超时在 [`HttpClient`] 里（默认 10 秒）。
pub async fn fetch_profile(
    http: &HttpClient,
    config: &Config,
    provider: OAuthProvider,
    code: &str,
    code_verifier: &str,
) -> Result<Profile, ProviderError> {
    let client = config.oauth_client(provider).ok_or(ProviderError::NotAvailable)?;
    let endpoints = config.oauth_endpoints(provider);

    let form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", &redirect_uri(config, provider))
        .append_pair("client_id", &client.client_id)
        .append_pair("client_secret", &client.client_secret)
        .append_pair("code_verifier", code_verifier)
        .finish();
    let response = http
        .post_form(&endpoints.token, &[("Accept", "application/json"), ("User-Agent", USER_AGENT)], form)
        .await
        .map_err(|source| ProviderError::Http { step: Step::Token, source })?;
    let token = token_from_response(&response)?;

    match provider {
        OAuthProvider::Github => github_profile(http, &endpoints, &token).await,
        OAuthProvider::Google => google_profile(http, &endpoints, &token).await,
    }
}

fn token_from_response(response: &Response) -> Result<AccessToken, ProviderError> {
    match parse_token_response(&response.body) {
        Ok(token) if (200..300).contains(&response.status) => Ok(token),
        // GitHub 授权码无效时是 `200` + `{"error":"bad_verification_code"}`，Google 是 `400` + `{"error":"invalid_grant"}`。
        Ok(_) => Err(ProviderError::Status { step: Step::Token, status: response.status }),
        Err(Some(reason)) => Err(ProviderError::Rejected { step: Step::Token, reason }),
        Err(None) if !(200..300).contains(&response.status) => {
            Err(ProviderError::Status { step: Step::Token, status: response.status })
        }
        Err(None) => Err(ProviderError::Malformed { step: Step::Token }),
    }
}

fn success(step: Step, response: &Response) -> Result<(), ProviderError> {
    if (200..300).contains(&response.status) {
        Ok(())
    } else {
        Err(ProviderError::Status { step, status: response.status })
    }
}

/// 带 access token 的 `GET`。`extra` 是这个提供方额外要的请求头。
async fn authorized_get(
    http: &HttpClient,
    step: Step,
    url: &str,
    token: &AccessToken,
    extra: &[(&str, &str)],
) -> Result<Response, ProviderError> {
    let authorization = format!("Bearer {}", token.0);
    let mut headers = vec![("Authorization", authorization.as_str()), ("User-Agent", USER_AGENT)];
    headers.extend_from_slice(extra);
    http.get(url, &headers).await.map_err(|source| ProviderError::Http { step, source })
}

async fn github_profile(
    http: &HttpClient,
    endpoints: &OAuthEndpoints,
    token: &AccessToken,
) -> Result<Profile, ProviderError> {
    // GitHub 的 API 要求有 `User-Agent`（上面统一带了），并建议指定 `Accept` 和 API 版本。
    const HEADERS: &[(&str, &str)] =
        &[("Accept", "application/vnd.github+json"), ("X-GitHub-Api-Version", "2022-11-28")];
    let user = authorized_get(http, Step::User, &endpoints.userinfo, token, HEADERS).await?;
    success(Step::User, &user)?;
    let (subject, names) = parse_github_user(&user.body).ok_or(ProviderError::Malformed { step: Step::User })?;

    let mut email = None;
    if let Some(url) = &endpoints.emails {
        let emails = authorized_get(http, Step::Emails, url, token, HEADERS).await?;
        match emails.status {
            // 授权里没有邮箱权限（GitHub App 没开 Email addresses、或 token 缺 `user:email`）：照样能登录，只是没有已验证邮箱。
            401 | 403 | 404 => {
                tracing::warn!(
                    status = emails.status,
                    "github: the email list is not available, continuing without a verified email"
                );
            }
            _ => {
                success(Step::Emails, &emails)?;
                email = pick_github_email(&emails.body).ok_or(ProviderError::Malformed { step: Step::Emails })?;
            }
        }
    }
    Ok(Profile { subject, email, names })
}

async fn google_profile(
    http: &HttpClient,
    endpoints: &OAuthEndpoints,
    token: &AccessToken,
) -> Result<Profile, ProviderError> {
    let info = authorized_get(http, Step::User, &endpoints.userinfo, token, &[("Accept", "application/json")]).await?;
    success(Step::User, &info)?;
    parse_google_userinfo(&info.body).ok_or(ProviderError::Malformed { step: Step::User })
}

// ---------------------------------------------------------------------------
// 响应解析（纯函数）
// ---------------------------------------------------------------------------

/// 提供方的用户 id 能不能用：非空、不太长、没有控制字符。
fn valid_subject(s: &str) -> bool {
    !s.is_empty() && s.len() <= SUBJECT_MAX_BYTES && !s.chars().any(char::is_control)
}

/// 提供方给的错误码（`error` 字段）只留短小的小写标识，其余当没有（日志里不放对端控制的任意文字）。
fn sanitize_reason(raw: &str) -> Option<String> {
    let ok = !raw.is_empty()
        && raw.len() <= 64
        && raw.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok.then(|| raw.to_string())
}

/// 解析换 token 的响应：`Ok` 是 access token；`Err(Some(reason))` 是提供方明确拒绝（带它的错误码）；`Err(None)` 看不懂。
fn parse_token_response(body: &[u8]) -> Result<AccessToken, Option<String>> {
    #[derive(Deserialize)]
    struct Body {
        access_token: Option<String>,
        error: Option<String>,
    }
    let parsed: Body = serde_json::from_slice(body).map_err(|_| None)?;
    match parsed.access_token.filter(|t| !t.is_empty()) {
        Some(token) => Ok(AccessToken(token)),
        None => Err(parsed.error.as_deref().and_then(sanitize_reason)),
    }
}

/// GitHub `/user`：（数字 `id` 的字符串形式，显示名候选 `[name, login]`）。
fn parse_github_user(body: &[u8]) -> Option<(String, Vec<String>)> {
    #[derive(Deserialize)]
    struct User {
        id: Option<Value>,
        login: Option<String>,
        name: Option<String>,
    }
    let user: User = serde_json::from_slice(body).ok()?;
    let subject = match user.id? {
        Value::Number(n) if n.is_u64() || n.is_i64() => n.to_string(),
        Value::String(s) => s,
        _ => return None,
    };
    valid_subject(&subject).then(|| (subject, user.name.into_iter().chain(user.login).collect()))
}

/// GitHub `/user/emails`：`primary && verified` 的那一条（规范化后）。
/// 外层 `None` = 响应看不懂；内层 `None` = 没有已验证的主邮箱。
fn pick_github_email(body: &[u8]) -> Option<Option<String>> {
    #[derive(Deserialize)]
    struct Entry {
        email: Option<String>,
        #[serde(default)]
        primary: bool,
        #[serde(default)]
        verified: bool,
    }
    let entries: Vec<Entry> = serde_json::from_slice(body).ok()?;
    Some(entries.into_iter().filter(|e| e.primary && e.verified).find_map(|e| normalize_email(&e.email?)))
}

/// Google userinfo：`sub` 是用户 id；`email` 只有在 `email_verified == true` 时才算已验证。
fn parse_google_userinfo(body: &[u8]) -> Option<Profile> {
    #[derive(Deserialize)]
    struct Info {
        sub: Option<String>,
        email: Option<String>,
        email_verified: Option<Value>,
        name: Option<String>,
    }
    let info: Info = serde_json::from_slice(body).ok()?;
    let subject = info.sub.filter(|s| valid_subject(s))?;
    // 标准里是布尔；个别实现返回字符串 "true"。其它一律当没验证。
    let verified = match info.email_verified {
        Some(Value::Bool(b)) => b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    };
    let email = info.email.filter(|_| verified).and_then(|e| normalize_email(&e));
    Some(Profile { subject, email, names: info.name.into_iter().collect() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OAuthClient;

    fn config() -> Config {
        let mut c = Config::new("https://hub.example.com/", "/nonexistent").unwrap();
        c.oauth_github = Some(OAuthClient { client_id: "gh id".into(), client_secret: "gh-secret".into() });
        c
    }

    #[test]
    fn authorize_urls_carry_pkce_state_and_the_public_callback() {
        let c = config();
        let url = Url::parse(&authorize_url(&c, OAuthProvider::Github, "st4te", "chall").unwrap()).unwrap();
        assert_eq!((url.scheme(), url.host_str(), url.path()), ("https", Some("github.com"), "/login/oauth/authorize"));
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "gh id");
        assert_eq!(q["redirect_uri"], "https://hub.example.com/auth/callback/github");
        assert_eq!(q["scope"], "read:user user:email");
        assert_eq!(
            (q["state"].as_str(), q["code_challenge"].as_str(), q["code_challenge_method"].as_str()),
            ("st4te", "chall", "S256")
        );
        assert!(!url.as_str().contains("gh-secret"), "the client secret never goes through the browser");
        // 没配的提供方没有授权地址
        assert!(authorize_url(&c, OAuthProvider::Google, "s", "c").is_none());
        assert_eq!(scope(OAuthProvider::Google), "openid email profile");
        assert_eq!(redirect_uri(&c, OAuthProvider::Google), "https://hub.example.com/auth/callback/google");
    }

    #[test]
    fn token_responses() {
        let t =
            parse_token_response(br#"{"access_token":"gho_abc","token_type":"bearer","scope":"read:user"}"#).unwrap();
        assert_eq!(t.0, "gho_abc");
        assert_eq!(format!("{t:?}"), "AccessToken(<redacted>)");
        // 提供方明确拒绝：带它的错误码
        let e = parse_token_response(
            br#"{"error":"bad_verification_code","error_description":"The code passed is incorrect"}"#,
        );
        assert_eq!(e.unwrap_err().as_deref(), Some("bad_verification_code"));
        assert_eq!(
            parse_token_response(br#"{"error":"invalid_grant"}"#).unwrap_err().as_deref(),
            Some("invalid_grant")
        );
        // 对端控制的任意文字不进日志
        assert_eq!(parse_token_response(br#"{"error":"Bad Thing: secret=abc"}"#).unwrap_err(), None);
        assert_eq!(parse_token_response(br#"{"error":""}"#).unwrap_err(), None);
        // 空 token、不是 JSON、没有 token
        for body in [&br#"{"access_token":""}"#[..], b"<html>", b"", b"[]", br#"{"token_type":"bearer"}"#] {
            assert!(parse_token_response(body).is_err(), "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn github_users() {
        let (subject, names) =
            parse_github_user(br#"{"login":"octocat","id":583231,"name":"The Octocat","email":null,"avatar_url":"x"}"#)
                .unwrap();
        assert_eq!(subject, "583231");
        assert_eq!(names, ["The Octocat", "octocat"]);
        // 没有 name：只剩 login；name 是 null 也一样
        assert_eq!(parse_github_user(br#"{"login":"octocat","id":1,"name":null}"#).unwrap().1, ["octocat"]);
        // 字符串形式的 id 也收
        assert_eq!(parse_github_user(br#"{"id":"42","login":"x"}"#).unwrap().0, "42");
        // 没有 id / id 不是数字或字符串 / 空 id / 浮点 / 控制字符
        for body in [
            &br#"{"login":"octocat"}"#[..],
            br#"{"id":null,"login":"x"}"#,
            br#"{"id":{"a":1},"login":"x"}"#,
            br#"{"id":1.5,"login":"x"}"#,
            br#"{"id":"","login":"x"}"#,
            br#"{"id":"a\nb","login":"x"}"#,
            b"nope",
        ] {
            assert!(parse_github_user(body).is_none(), "{}", String::from_utf8_lossy(body));
        }
        let long = format!(r#"{{"id":"{}","login":"x"}}"#, "9".repeat(300));
        assert!(parse_github_user(long.as_bytes()).is_none(), "ids are bounded");
    }

    #[test]
    fn github_picks_only_the_verified_primary_email() {
        let pick = |body: &str| pick_github_email(body.as_bytes()).expect("parsable");
        let list = r#"[
            {"email":"old@example.com","primary":false,"verified":true,"visibility":null},
            {"email":"Main@Example.com","primary":true,"verified":true,"visibility":"public"},
            {"email":"other@example.com","primary":false,"verified":false,"visibility":null}
        ]"#;
        assert_eq!(pick(list).as_deref(), Some("main@example.com"), "normalised (lower-cased)");
        // 主邮箱没验证：没有已验证邮箱（不退而求其次选别的已验证邮箱）
        assert_eq!(
            pick(
                r#"[{"email":"a@example.com","primary":true,"verified":false},{"email":"b@example.com","primary":false,"verified":true}]"#
            ),
            None
        );
        // 没有主邮箱 / 空列表 / 缺字段（默认 false）/ 邮箱格式不对
        assert_eq!(pick(r#"[{"email":"a@example.com","primary":false,"verified":true}]"#), None);
        assert_eq!(pick("[]"), None);
        assert_eq!(pick(r#"[{"email":"a@example.com"}]"#), None);
        assert_eq!(pick(r#"[{"email":"not an email","primary":true,"verified":true}]"#), None);
        assert_eq!(pick(r#"[{"email":null,"primary":true,"verified":true}]"#), None);
        // 看不懂的响应和「没有」区分开
        assert!(pick_github_email(b"{\"message\":\"Not Found\"}").is_none());
        assert!(pick_github_email(b"nope").is_none());
    }

    #[test]
    fn google_userinfo() {
        let p = parse_google_userinfo(
            br#"{"sub":"1234567890","name":"Ada Lovelace","email":"Ada@Example.com","email_verified":true,"picture":"x"}"#,
        )
        .unwrap();
        assert_eq!(p.subject, "1234567890");
        assert_eq!(p.email.as_deref(), Some("ada@example.com"));
        assert_eq!(p.names, ["Ada Lovelace"]);
        // 没验证 / 缺 email_verified / 不是布尔：没有已验证邮箱
        for verified in [
            r#","email_verified":false"#,
            "",
            r#","email_verified":null"#,
            r#","email_verified":1"#,
            r#","email_verified":"yes""#,
        ] {
            let body = format!(r#"{{"sub":"1","email":"ada@example.com"{verified}}}"#);
            let p = parse_google_userinfo(body.as_bytes()).unwrap();
            assert_eq!(p.email, None, "{body}");
            assert_eq!(p.subject, "1", "still a valid identity");
        }
        // 个别实现返回字符串 "true"
        let p = parse_google_userinfo(br#"{"sub":"1","email":"a@example.com","email_verified":"true"}"#).unwrap();
        assert_eq!(p.email.as_deref(), Some("a@example.com"));
        // 没有邮箱字段、没有名字
        let p = parse_google_userinfo(br#"{"sub":"1"}"#).unwrap();
        assert_eq!((p.email, p.names), (None, vec![]));
        // sub 缺失 / 空 / 不是字符串 / 看不懂
        for body in
            [&br#"{"email":"a@example.com","email_verified":true}"#[..], br#"{"sub":""}"#, br#"{"sub":12}"#, b"nope"]
        {
            assert!(parse_google_userinfo(body).is_none(), "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn profiles_and_errors_do_not_print_secrets() {
        let p =
            Profile { subject: "SUBJECT-123".into(), email: Some("who@example.com".into()), names: vec!["Who".into()] };
        let shown = format!("{p:?}");
        assert!(
            !shown.contains("SUBJECT-123") && !shown.contains("who@example.com") && !shown.contains("Who"),
            "{shown}"
        );
        for e in [
            ProviderError::Http { step: Step::Token, source: HttpError::Timeout },
            ProviderError::Status { step: Step::User, status: 502 },
            ProviderError::Rejected { step: Step::Token, reason: "invalid_grant".into() },
            ProviderError::Malformed { step: Step::Emails },
            ProviderError::NotAvailable,
        ] {
            let shown = e.to_string();
            assert!(!shown.contains("http") && !shown.contains('?'), "{shown}");
        }
    }
}
