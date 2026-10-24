//! GitHub / Google 登录、账号关联和身份 API。提供方是 `tests/common/fake_oauth.rs` 里本机的假服务，测试自己扮演浏览器：
//! `GET /auth/oauth/<p>/start`（带 `Host: hub.test`，也就是配置的公网地址）→ 把 `Location` 交给假提供方的授权页 →
//! 把它回的回调地址连同 `anycon_oauth` cookie 送回 hub。

mod common;
#[path = "common/fake_oauth.rs"]
mod fake_oauth;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_hub::config::{Mode, OAuthProvider};
use anycon_hub::mail::RecordingMailer;
use anycon_hub::store::AuditEntry;
use anycon_hub::util::{hash_token, now_ms};
use anycon_hub::Config;
use common::*;
use fake_oauth::{FakeOAuth, FakeUser};
use reqwest::Method;
use serde_json::{json, Value};
use url::Url;

/// 测试 hub 配置的公网地址的主机（`Config::new("https://hub.test", …)`）。
const PUBLIC_HOST: &str = "hub.test";
const GITHUB: OAuthProvider = OAuthProvider::Github;
const GOOGLE: OAuthProvider = OAuthProvider::Google;
const BOTH: [OAuthProvider; 2] = [GITHUB, GOOGLE];

/// 对假提供方行为的一次调整。
type Tweak = Box<dyn Fn(&mut fake_oauth::Behavior)>;

// ---------------------------------------------------------------------------
// 测试台
// ---------------------------------------------------------------------------

/// 一个 hub + 一个假提供方 + 扮演浏览器的方法。
struct Env {
    hub: TestHub,
    fake: FakeOAuth,
    mailer: Option<Arc<RecordingMailer>>,
}

/// hub 回的一个重定向（或别的响应）。
struct Redirect {
    status: u16,
    location: Option<String>,
    /// 全部 `Set-Cookie` 头，按顺序。
    cookies: Vec<String>,
}

impl Redirect {
    async fn of(res: reqwest::Response) -> Self {
        Self {
            status: res.status().as_u16(),
            location: res.headers().get("location").map(|v| v.to_str().unwrap().to_string()),
            cookies: res.headers().get_all("set-cookie").iter().map(|v| v.to_str().unwrap().to_string()).collect(),
        }
    }

    fn location(&self) -> &str {
        self.location.as_deref().unwrap_or_else(|| panic!("no Location (status {})", self.status))
    }

    fn url(&self) -> Url {
        Url::parse("https://hub.test").unwrap().join(self.location()).unwrap()
    }

    fn param(&self, key: &str) -> Option<String> {
        self.url().query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
    }

    fn path(&self) -> String {
        self.url().path().to_string()
    }

    /// 失败重定向里的 `oauth_error`。
    fn error(&self) -> String {
        self.param("oauth_error").unwrap_or_else(|| panic!("not an oauth error redirect: {}", self.location()))
    }

    /// `name=value` 形式的 cookie（值非空）。
    fn cookie(&self, name: &str) -> Option<String> {
        self.cookies
            .iter()
            .filter_map(|c| c.split(';').next())
            .find(|pair| pair.strip_prefix(name).is_some_and(|rest| rest.starts_with('=') && rest.len() > 1))
            .map(str::to_string)
    }

    fn session(&self) -> String {
        self.cookie("anycon_session")
            .unwrap_or_else(|| panic!("no session cookie, status {} {:?}", self.status, self.location))
    }

    fn has_session(&self) -> bool {
        self.cookie("anycon_session").is_some()
    }

    fn oauth_cookie(&self) -> Option<String> {
        self.cookie("anycon_oauth")
    }

    /// 清掉 `anycon_oauth` 的那条 `Set-Cookie`。
    fn clears_oauth_cookie(&self) -> bool {
        self.cookies
            .iter()
            .any(|c| c.starts_with("anycon_oauth=;") && c.contains("Max-Age=0") && c.contains("Path=/auth/callback"))
    }
}

impl Env {
    async fn with(mode: Mode, providers: &[OAuthProvider], tweak: impl FnOnce(&mut Config)) -> Self {
        let fake = FakeOAuth::start().await;
        let providers = providers.to_vec();
        let f = &fake;
        let configure = move |c: &mut Config| {
            f.configure(c, &providers);
            tweak(c);
        };
        match mode {
            Mode::Multi => {
                // 借用 `fake` 的闭包不能跨 await 之外存活：先把配置算好再交给 TestHub
                let (hub, mailer) = TestHub::start_multi_with(configure).await;
                Self { hub, fake, mailer: Some(mailer) }
            }
            Mode::Single => {
                let (hub, mailer) = TestHub::start_mail_with(configure).await;
                Self { hub, fake, mailer: Some(mailer) }
            }
        }
    }

    async fn multi(providers: &[OAuthProvider]) -> Self {
        Self::with(Mode::Multi, providers, |_| {}).await
    }

    async fn single(providers: &[OAuthProvider]) -> Self {
        Self::with(Mode::Single, providers, |_| {}).await
    }

    fn mailer(&self) -> &RecordingMailer {
        self.mailer.as_ref().unwrap()
    }

    fn store(&self) -> Arc<dyn anycon_hub::Store> {
        self.hub.handle.store()
    }

    // ---- 扮演浏览器 ----

    async fn get_as(&self, path_and_query: &str, host: &str, cookie: Option<&str>) -> Redirect {
        let mut req = self.hub.http.get(self.hub.url(path_and_query)).header("host", host);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        Redirect::of(req.send().await.unwrap()).await
    }

    /// `GET /auth/oauth/<provider>/start<query>`（`query` 带前导 `?` 或为空），可选带登录会话。
    async fn start(&self, provider: &str, query: &str, session: Option<&str>) -> Redirect {
        self.get_as(&format!("/auth/oauth/{provider}/start{query}"), PUBLIC_HOST, session).await
    }

    /// `GET /auth/callback/<provider>?<query>`，`cookies` 是整个 `Cookie` 头。
    async fn callback(&self, provider: &str, query: &str, cookies: Option<&str>) -> Redirect {
        self.get_as(&format!("/auth/callback/{provider}?{query}"), PUBLIC_HOST, cookies).await
    }

    /// 带着 start 回的授权地址去假提供方的授权页，返回提供方让浏览器回 hub 的地址（路径 + query）。
    async fn authorize(&self, start: &Redirect) -> String {
        assert_eq!(start.status, 302, "start: {:?}", start.location);
        let res = self.fake.authorize(start.location()).await;
        assert_eq!(res.status(), 302, "the fake provider's authorize page: {}", res.text().await.unwrap());
        let back = Url::parse(res.headers()["location"].to_str().unwrap()).unwrap();
        assert_eq!(back.host_str(), Some(PUBLIC_HOST), "the provider sends the browser to the public origin");
        format!("{}?{}", back.path(), back.query().unwrap_or(""))
    }

    /// 一次完整的登录：设定提供方那边的用户 → start → 授权页 → 回调。返回回调的结果。
    async fn sign_in(&self, provider: &str, user: FakeUser, query: &str, session: Option<&str>) -> Redirect {
        self.fake.set_user(provider, user);
        let start = self.start(provider, query, session).await;
        let oauth = start.oauth_cookie().expect("start sets the anycon_oauth cookie");
        let back = self.authorize(&start).await;
        let cookies = match session {
            Some(s) => format!("{oauth}; {s}"),
            None => oauth,
        };
        self.get_as(&back, PUBLIC_HOST, Some(&cookies)).await
    }

    /// 同 [`sign_in`]，要求成功登录（302 去 `/` 且带会话），返回会话 cookie。
    async fn sign_in_ok(&self, provider: &str, user: FakeUser) -> String {
        let done = self.sign_in(provider, user, "", None).await;
        assert_eq!(done.status, 302);
        assert_eq!(done.location(), "/", "{:?}", done.location);
        done.session()
    }

    // ---- 读状态 ----

    async fn me(&self, cookie: &str) -> Value {
        let res = self.hub.get("/api/me", cookie).await;
        assert_eq!(res.status(), 200);
        res.json().await.unwrap()
    }

    async fn identities(&self, cookie: &str) -> Vec<Value> {
        let res = self.hub.get("/api/me/identities", cookie).await;
        assert_eq!(res.status(), 200);
        res.json::<Value>().await.unwrap().as_array().unwrap().clone()
    }

    async fn audit(&self) -> Vec<AuditEntry> {
        self.store().list_audit(500, None).await.unwrap()
    }

    async fn audit_count(&self, event: &str, detail_key: &str, detail_value: &str) -> usize {
        self.audit()
            .await
            .iter()
            .filter(|e| e.event == event && e.detail.as_ref().is_some_and(|d| d[detail_key] == detail_value))
            .count()
    }

    async fn accounts(&self) -> u64 {
        self.store().stats().await.unwrap().accounts
    }

    /// 邮箱验证码登录（`multi` 里没有账号就注册），返回会话 cookie。
    async fn email_sign_in(&self, email: &str) -> String {
        let hub = &self.hub;
        let before = self.mailer().sent().len();
        let res = hub
            .http
            .post(hub.url("/auth/email/send"))
            .header("x-anycon", "1")
            .json(&json!({ "email": email }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        hub.settle().await;
        let sent = self.mailer().sent();
        assert_eq!(sent.len(), before + 1);
        let code = code_in(sent.last().unwrap());
        let res = hub
            .http
            .post(hub.url("/auth/email/verify"))
            .header("x-anycon", "1")
            .json(&json!({ "email": email, "code": code }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        cookie_pair(&res)
    }

    async fn delete(&self, path: &str, cookie: &str) -> reqwest::Response {
        self.hub.send(Method::DELETE, path, cookie, Value::Null).await
    }
}

fn gh(subject: &str) -> FakeUser {
    FakeUser::new(subject)
}

// ---------------------------------------------------------------------------
// 登录与注册（multi）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_new_github_user_signs_up_in_multi_with_the_verified_email_and_lands_on_next() {
    let env = Env::multi(&[GITHUB]).await;
    env.fake.set_user("github", gh("583231").name("Ada Lovelace").login("ada").email("Ada@Example.com", true));

    let start = env.start("github", "?next=%2Fchat%3Fa%3D1", None).await;
    assert_eq!(start.status, 302);
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;

    assert_eq!((done.status, done.location()), (302, "/chat?a=1"));
    // 会话 cookie 在前（只看第一个 Set-Cookie 的客户端拿到的是会话），然后清掉 anycon_oauth
    assert!(done.cookies[0].starts_with("anycon_session="), "{:?}", done.cookies);
    assert!(done.cookies[0].contains("HttpOnly") && done.cookies[0].contains("SameSite=Lax"));
    assert!(done.clears_oauth_cookie(), "{:?}", done.cookies);

    let me = env.me(&done.session()).await;
    assert_eq!(me["display_name"], "Ada Lovelace");
    assert_eq!(me["email"], "ada@example.com", "the provider-verified email, normalised");
    assert_eq!(me["is_admin"], false);
    assert_eq!(
        me["login_methods"],
        json!(["github", "email"]),
        "linked GitHub, and the bound email works because SMTP is configured"
    );
    let identities = env.identities(&done.session()).await;
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0]["provider"], "github");
    assert!(identities[0]["created_at_ms"].as_u64().unwrap() > 0);
    assert!(identities[0].get("subject").is_none() && identities[0].get("email").is_none(), "{identities:?}");
    assert_eq!(env.accounts().await, 1);

    assert_eq!(env.audit_count("account_created", "method", "github").await, 1);
    assert_eq!(env.audit_count("login_ok", "method", "github").await, 1);
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 0);
}

#[tokio::test]
async fn logging_in_again_finds_the_account_by_identity_not_by_name_or_email() {
    let env = Env::multi(&[GITHUB]).await;
    let first = env.sign_in_ok("github", gh("100").name("Old Name").email("old@example.com", true)).await;
    let id = env.me(&first).await["id"].clone();

    // 提供方那边改了名字、邮箱、login：还是同一个账号，账号上的邮箱和名字也不被覆盖
    let second =
        env.sign_in_ok("github", gh("100").name("New Name").login("renamed").email("new@example.com", true)).await;
    let me = env.me(&second).await;
    assert_eq!(me["id"], id);
    assert_eq!(me["email"], "old@example.com");
    assert_eq!(me["display_name"], "Old Name");
    assert_eq!(env.accounts().await, 1);
    assert_eq!(env.identities(&second).await.len(), 1);
    assert_eq!(env.audit_count("login_ok", "method", "github").await, 2);
    assert_eq!(env.audit_count("account_created", "method", "github").await, 1);
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 0, "nothing new was linked");
    // 另一个 GitHub 用户是另一个账号
    let other = env.sign_in_ok("github", gh("200").name("Other")).await;
    assert_ne!(env.me(&other).await["id"], id);
    assert_eq!(env.accounts().await, 2);
}

#[tokio::test]
async fn a_verified_email_links_the_identity_to_the_account_that_owns_that_email() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    let by_email = env.email_sign_in("alice@example.com").await;
    let id = env.me(&by_email).await["id"].clone();
    assert_eq!(env.accounts().await, 1);

    let done = env.sign_in("github", gh("7").name("alice-on-github").email("Alice@Example.com", true), "", None).await;
    assert_eq!(done.location(), "/");
    let me = env.me(&done.session()).await;
    assert_eq!(me["id"], id, "logged into the existing account");
    assert_eq!(me["display_name"], "alice", "the existing display name is kept");
    assert_eq!(me["login_methods"], json!(["github", "email"]));
    assert_eq!(env.accounts().await, 1, "no second account");
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 1);
    assert_eq!(env.audit_count("login_ok", "method", "github").await, 1);
    assert_eq!(env.audit_count("account_created", "method", "github").await, 0);

    // 以后用身份直接登录；Google 同理，同一个账号可以有两个提供方的身份
    let again = env.sign_in_ok("github", gh("7").email("changed@example.com", true)).await;
    assert_eq!(env.me(&again).await["id"], id);
    let google = env.sign_in("google", gh("g-1").email("alice@example.com", true), "", None).await;
    let me = env.me(&google.session()).await;
    assert_eq!(me["id"], id);
    assert_eq!(me["login_methods"], json!(["github", "google", "email"]));
    assert_eq!(env.accounts().await, 1);
}

#[tokio::test]
async fn an_unverified_or_missing_email_never_links_and_never_becomes_the_account_email() {
    let env = Env::with(Mode::Multi, &BOTH, |c| c.limits.email_resend_interval = Duration::from_millis(1)).await;
    let owner = env.email_sign_in("victim@example.com").await;
    let victim_id = env.me(&owner).await["id"].clone();

    // GitHub：主邮箱没验证 → 一个全新的、没有邮箱的账号；受害者的账号没被碰
    let done = env.sign_in("github", gh("1").name("Mallory").email("victim@example.com", false), "", None).await;
    let me = env.me(&done.session()).await;
    assert_ne!(me["id"], victim_id, "did not take over the account");
    assert!(me.get("email").is_none(), "the unverified email is not the account's email: {me}");
    assert_eq!(me["login_methods"], json!(["github"]));
    assert!(env.identities(&owner).await.is_empty(), "the victim account gained no identity");

    // GitHub：已验证的是**非主**邮箱 → 也不算
    let done = env
        .sign_in(
            "github",
            gh("2").email("primary@example.com", false).extra_email("victim@example.com", true),
            "",
            None,
        )
        .await;
    let me = env.me(&done.session()).await;
    assert_ne!(me["id"], victim_id);
    assert!(me.get("email").is_none(), "{me}");

    // GitHub：根本没有邮箱
    let done = env.sign_in("github", gh("3"), "", None).await;
    assert!(env.me(&done.session()).await.get("email").is_none());

    // Google：email_verified = false
    let done = env.sign_in("google", gh("g-1").name("G").email("victim@example.com", false), "", None).await;
    let me = env.me(&done.session()).await;
    assert_ne!(me["id"], victim_id);
    assert!(me.get("email").is_none());

    // 受害者的账号从头到尾没变：邮箱还在、没有任何身份；一共 1 + 4 个账号
    assert_eq!(env.me(&owner).await["email"], "victim@example.com");
    assert_eq!(env.accounts().await, 5);
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 0);
    assert_eq!(env.audit_count("identity_linked", "provider", "google").await, 0);
    // 没验证的邮箱也没有变成别的账号可以占用的东西：之后受害者的邮箱登录照常
    assert_eq!(env.me(&env.email_sign_in("victim@example.com").await).await["id"], victim_id);
}

#[tokio::test]
async fn an_account_in_the_admin_emails_is_an_admin_when_it_signs_up_with_a_verified_email() {
    let env = Env::with(Mode::Multi, &[GITHUB], |c| c.admin_emails = vec!["boss@example.com".into()]).await;

    let boss = env.sign_in_ok("github", gh("1").email("Boss@Example.com", true)).await;
    let me = env.me(&boss).await;
    assert_eq!(me["is_admin"], true);
    assert_eq!(env.hub.get("/api/admin/accounts", &boss).await.status(), 200);

    // 同一个邮箱，但提供方没验证：不是管理员，也不会拿到这个邮箱
    let imposter = env.sign_in_ok("github", gh("2").email("boss@example.com", false)).await;
    assert_eq!(env.me(&imposter).await["is_admin"], false);
    assert_eq!(env.hub.get("/api/admin/accounts", &imposter).await.status(), 403);
    // 没有邮箱的人当然也不是
    let plain = env.sign_in_ok("github", gh("3").email("nobody@example.com", true)).await;
    assert_eq!(env.me(&plain).await["is_admin"], false);
}

#[tokio::test]
async fn display_names_come_from_the_provider_with_fallbacks() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    let name = |session: String| {
        let env = &env;
        async move { env.me(&session).await["display_name"].as_str().unwrap().to_string() }
    };
    // GitHub：name，没有就 login
    assert_eq!(name(env.sign_in_ok("github", gh("1").name("  Ada  ").login("ada")).await).await, "Ada");
    assert_eq!(name(env.sign_in_ok("github", gh("2").login("octocat")).await).await, "octocat");
    // Google：name；没有 name 就取已验证邮箱 @ 之前的部分
    assert_eq!(name(env.sign_in_ok("google", gh("g1").name("Grace Hopper")).await).await, "Grace Hopper");
    assert_eq!(
        name(env.sign_in_ok("google", gh("g2").email("grace.hopper@example.com", true)).await).await,
        "grace.hopper"
    );
    // 什么都没有，或者邮箱没验证（没验证的邮箱不拿来当名字）：user-<6 hex>
    for (i, user) in [gh("g3"), gh("g4").email("someone@example.com", false)].into_iter().enumerate() {
        let n = name(env.sign_in_ok("google", user).await).await;
        assert!(n.starts_with("user-") && n.len() == 11, "{i}: {n}");
    }
    // 过长的名字截到 64 个字符
    let long = "x".repeat(200);
    assert_eq!(name(env.sign_in_ok("github", gh("9").name(&long)).await).await.chars().count(), 64);
}

#[tokio::test]
async fn google_works_through_userinfo_and_never_through_an_id_token() {
    let env = Env::multi(&[GOOGLE]).await;
    let session = env.sign_in_ok("google", gh("1234567890").name("Ada").email("ada@example.com", true)).await;
    let me = env.me(&session).await;
    assert_eq!(me["email"], "ada@example.com");
    assert_eq!(me["login_methods"], json!(["google", "email"]));

    let authorize = &env.fake.requests_to("authorize")[0];
    assert_eq!(authorize.params["scope"], "openid email profile");
    let userinfo = &env.fake.requests_to("userinfo");
    assert_eq!(userinfo.len(), 1);
    assert!(userinfo[0].headers["authorization"].starts_with("Bearer tok-"), "{:?}", userinfo[0].headers);
    assert!(
        env.fake.requests_to("emails").is_empty() && env.fake.requests_to("user").is_empty(),
        "GitHub endpoints are not used"
    );
}

#[tokio::test]
async fn the_hub_speaks_oauth_correctly_to_the_provider() {
    let env = Env::multi(&[GITHUB]).await;
    let start = env.start("github", "", None).await;
    let authorize_url = Url::parse(start.location()).unwrap();
    // 浏览器看到的授权地址：PKCE + state + 公网回调；没有 client secret
    let q = |k: &str| authorize_url.query_pairs().find(|(a, _)| a == k).map(|(_, v)| v.into_owned()).unwrap();
    assert_eq!(q("response_type"), "code");
    assert_eq!(q("client_id"), fake_oauth::CLIENT_ID);
    assert_eq!(q("redirect_uri"), "https://hub.test/auth/callback/github");
    assert_eq!(q("scope"), "read:user user:email");
    assert_eq!(q("code_challenge_method"), "S256");
    assert_eq!(q("code_challenge").len(), 43);
    let state = q("state");
    assert_eq!(state.len(), 64);
    assert!(state.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(!start.location().contains(fake_oauth::CLIENT_SECRET));
    // cookie：绑定这个 state，只发给回调路径；公网地址是 https 所以带 Secure
    let cookie = start.cookies.iter().find(|c| c.starts_with("anycon_oauth=")).unwrap();
    assert_eq!(
        cookie,
        &format!("anycon_oauth={state}; Path=/auth/callback; Max-Age=600; HttpOnly; SameSite=Lax; Secure")
    );
    assert_eq!(start.cookies.len(), 1);
    // 库里只有 state 的哈希
    assert!(env.store().take_oauth_state(&state, now_ms()).await.unwrap().is_none(), "the raw state is not a key");
    let row = env.store().take_oauth_state(&hash_token(&state), now_ms()).await.unwrap().expect("stored by hash");
    assert_eq!(row.provider, "github");
    assert_eq!(row.pkce_verifier.len(), 43);
    assert!(row.expires_at_ms > now_ms() + 9 * 60_000 && row.expires_at_ms <= now_ms() + 10 * 60_000);

    // 走完一遍，看假提供方收到了什么
    env.fake.set_user("github", gh("55").name("N").email("n@example.com", true));
    let start = env.start("github", "", None).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    assert_eq!(env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await.status, 302);
    let token = &env.fake.requests_to("token")[0];
    assert_eq!(token.params["grant_type"], "authorization_code");
    assert_eq!(token.params["client_id"], fake_oauth::CLIENT_ID);
    assert_eq!(token.params["client_secret"], fake_oauth::CLIENT_SECRET);
    assert_eq!(token.params["redirect_uri"], "https://hub.test/auth/callback/github");
    assert_eq!(
        token.params["code_verifier"].len(),
        43,
        "PKCE verifier sent (the fake checked it matches the challenge)"
    );
    assert_eq!(token.headers["accept"], "application/json");
    for request in env.fake.requests_to("user").iter().chain(env.fake.requests_to("emails").iter()) {
        assert!(request.headers["user-agent"].starts_with("anycon-hub/"), "GitHub's API needs a User-Agent");
        assert!(request.headers["authorization"].starts_with("Bearer tok-"));
    }
    assert_eq!(env.fake.requests_to("user").len(), 1);
    assert_eq!(env.fake.requests_to("emails").len(), 1);
}

// ---------------------------------------------------------------------------
// single：注册要邀请
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_mode_without_an_invite_is_registration_closed() {
    let env = Env::single(&[GITHUB]).await;
    let done = env.sign_in("github", gh("1").name("Stranger").email("stranger@example.com", true), "", None).await;
    assert_eq!((done.status, done.path()), (302, "/login".to_string()));
    assert_eq!(done.error(), "registration_closed");
    assert_eq!(done.param("provider").as_deref(), Some("github"));
    assert!(!done.has_session());
    assert!(done.clears_oauth_cookie());
    assert_eq!(env.accounts().await, 1, "only the bootstrap admin exists");
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 1);
    assert_eq!(env.audit_count("login_ok", "method", "github").await, 0);
}

#[tokio::test]
async fn single_mode_still_matches_by_identity_and_by_verified_email_without_an_invite() {
    let env = Env::single(&[GITHUB]).await;
    let admin = env.hub.login_admin().await;
    bind_email(&env.hub, env.mailer(), &admin, "admin@example.com").await;
    let admin_id = env.me(&admin).await["id"].clone();

    // 已验证邮箱 = 管理员绑定的邮箱：关联并登录，不需要邀请
    let done = env.sign_in("github", gh("1").email("admin@example.com", true), "", None).await;
    assert_eq!(done.location(), "/");
    let me = env.me(&done.session()).await;
    assert_eq!(me["id"], admin_id);
    assert_eq!(me["is_admin"], true, "the bootstrap admin");
    assert_eq!(me["login_methods"], json!(["github", "email", "token"]));
    // 之后凭身份登录，哪怕邮箱变了
    let again = env.sign_in("github", gh("1").email("other@example.com", true), "", None).await;
    assert_eq!(env.me(&again.session()).await["id"], admin_id);
    // 没验证的邮箱不能借此进管理员的账号
    let imposter = env.sign_in("github", gh("2").email("admin@example.com", false), "", None).await;
    assert_eq!(imposter.error(), "registration_closed");
    assert_eq!(env.accounts().await, 1);
}

#[tokio::test]
async fn single_mode_registers_through_an_invite_and_consumes_it() {
    let env = Env::single(&[GITHUB, GOOGLE]).await;
    let admin = env.hub.login_admin().await;
    let invite: Value = env.hub.send(Method::POST, "/api/invites", &admin, json!({})).await.json().await.unwrap();
    let code = invite["code"].as_str().unwrap();

    let done = env
        .sign_in(
            "google",
            gh("g-1").name("Guest").email("guest@example.com", true),
            &format!("?invite={code}&next=%2Fdevices"),
            None,
        )
        .await;
    assert_eq!((done.status, done.location()), (302, "/devices"));
    let me = env.me(&done.session()).await;
    assert_eq!(me["display_name"], "Guest");
    assert_eq!(me["email"], "guest@example.com", "the verified email is set on the new account");
    assert_eq!(me["is_admin"], false);
    assert_eq!(me["login_methods"], json!(["google", "email"]));
    assert_eq!(env.accounts().await, 2);
    let invites = env.store().list_invites(10, None).await.unwrap();
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0].used_by.as_deref(), me["id"].as_str());
    assert_eq!(env.audit_count("account_created", "method", "google").await, 1);
    assert_eq!(env.audit_count("invite_used", "method", "google").await, 1);
    assert_eq!(env.audit_count("login_ok", "method", "google").await, 1);

    // 下次不用邀请，凭身份登录
    let again = env.sign_in("google", gh("g-1"), "", None).await;
    assert_eq!(env.me(&again.session()).await["id"], me["id"]);
    // 邀请已经用掉了：别人再用 → invite_used，没建账号
    let used = env.sign_in("github", gh("2"), &format!("?invite={code}"), None).await;
    assert_eq!((used.path(), used.error()), ("/login".to_string(), "invite_used".to_string()));
    assert_eq!(env.accounts().await, 2);
}

#[tokio::test]
async fn single_mode_invite_problems_are_reported_with_their_own_codes() {
    let env = Env::single(&[GITHUB]).await;
    let admin_id = env.hub.admin_account_id();
    let now = now_ms();
    env.store().create_invite(&admin_id, &hash_token("expired-code"), None, now - 10_000, now - 1).await.unwrap();
    env.store().create_invite(&admin_id, &hash_token("good-code"), None, now, now + 3_600_000).await.unwrap();

    for (code, expected) in [("no-such-code", "invite_invalid"), ("expired-code", "invite_expired")] {
        let done = env.sign_in("github", gh("1"), &format!("?invite={code}"), None).await;
        assert_eq!(done.error(), expected, "{code}");
        assert!(!done.has_session() && done.clears_oauth_cookie());
    }
    assert_eq!(env.accounts().await, 1, "nothing was created");
    // 失败没有烧掉别的邀请；一个空的 invite= 就是没带邀请
    let done = env.sign_in("github", gh("1"), "?invite=", None).await;
    assert_eq!(done.error(), "registration_closed");
    let done = env.sign_in("github", gh("1"), "?invite=good-code", None).await;
    assert_eq!(done.location(), "/");
    assert_eq!(env.accounts().await, 2);
}

#[tokio::test]
async fn the_invite_parameter_is_ignored_in_multi() {
    let env = Env::multi(&[GITHUB]).await;
    let done = env.sign_in("github", gh("1"), "?invite=whatever", None).await;
    assert_eq!(done.location(), "/", "registration is open, the invite code plays no role");
    assert_eq!(env.accounts().await, 1);
    assert_eq!(env.audit_count("invite_used", "method", "github").await, 0);
}

// ---------------------------------------------------------------------------
// 关联
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linking_adds_an_identity_to_the_signed_in_account_and_never_touches_its_email() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    let session = env.email_sign_in("me@example.com").await;
    let id = env.me(&session).await["id"].clone();

    // 提供方那边是另一个邮箱：关联不看邮箱，账号的邮箱不变
    let done = env.sign_in("github", gh("9").email("different@example.com", true), "?link=1", Some(&session)).await;
    assert_eq!((done.status, done.location()), (302, "/settings/login?linked=github"));
    assert!(!done.has_session(), "linking does not start a new session");
    assert!(done.clears_oauth_cookie());
    let me = env.me(&session).await;
    assert_eq!(me["id"], id);
    assert_eq!(me["email"], "me@example.com");
    assert_eq!(me["login_methods"], json!(["github", "email"]));
    assert_eq!(env.identities(&session).await.len(), 1);
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 1);
    assert_eq!(env.audit_count("login_ok", "method", "github").await, 0, "linking is not a login");
    assert_eq!(env.accounts().await, 1);

    // 再关联同一个身份：成功，什么都没变，不重复审计
    let again = env.sign_in("github", gh("9"), "?link=1", Some(&session)).await;
    assert_eq!(again.location(), "/settings/login?linked=github");
    assert_eq!(env.identities(&session).await.len(), 1);
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 1);

    // 同一个账号可以再关联另一个提供方；之后凭 GitHub 登录进的是这个账号
    let google = env.sign_in("google", gh("g-9"), "?link=1", Some(&session)).await;
    assert_eq!(google.location(), "/settings/login?linked=google");
    assert_eq!(env.me(&session).await["login_methods"], json!(["github", "google", "email"]));
    let via_github = env.sign_in_ok("github", gh("9")).await;
    assert_eq!(env.me(&via_github).await["id"], id);
}

#[tokio::test]
async fn linking_failures_go_back_to_the_settings_page_with_their_code() {
    let env = Env::multi(&[GITHUB]).await;
    let alice = env.email_sign_in("alice@example.com").await;
    let bob = env.email_sign_in("bob@example.com").await;
    env.sign_in("github", gh("1"), "?link=1", Some(&alice)).await;

    // 这个 GitHub 身份已经是 alice 的：bob 想关联 → identity_taken
    let taken = env.sign_in("github", gh("1"), "?link=1", Some(&bob)).await;
    assert_eq!(
        (taken.path(), taken.error(), taken.param("provider")),
        ("/settings/login".into(), "identity_taken".into(), Some("github".into()))
    );
    assert!(!taken.has_session() && taken.clears_oauth_cookie());
    assert!(env.identities(&bob).await.is_empty());

    // alice 已经有一个 GitHub 身份：再关联另一个 GitHub 账号 → already_linked
    let already = env.sign_in("github", gh("2"), "?link=1", Some(&alice)).await;
    assert_eq!((already.path(), already.error()), ("/settings/login".into(), "already_linked".into()));
    assert_eq!(env.identities(&alice).await.len(), 1);
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 0, "link failures are not login failures");
}

#[tokio::test]
async fn linking_needs_a_signed_in_session_at_start_and_at_the_callback() {
    let env = Env::multi(&[GITHUB]).await;
    let alice = env.email_sign_in("alice@example.com").await;
    let bob = env.email_sign_in("bob@example.com").await;

    // start：没登录 → /login?oauth_error=not_signed_in，不种 cookie、不去提供方
    let start = env.start("github", "?link=1", None).await;
    assert_eq!((start.status, start.location()), (302, "/login?oauth_error=not_signed_in&provider=github"));
    assert!(start.cookies.is_empty());
    // cookie 是个不存在的会话也一样
    let start = env.start("github", "?link=1", Some("anycon_session=nope")).await;
    assert_eq!(start.location(), "/login?oauth_error=not_signed_in&provider=github");
    assert!(env.fake.requests().is_empty());

    // callback：流程是 alice 发起的，但回调时没带会话 / 带的是 bob 的会话 / 会话已登出
    env.fake.set_user("github", gh("1"));
    for session in [None, Some(bob.clone()), Some("anycon_session=expired".to_string())] {
        let start = env.start("github", "?link=1", Some(&alice)).await;
        let oauth = start.oauth_cookie().unwrap();
        let back = env.authorize(&start).await;
        let cookies = match &session {
            Some(s) => format!("{oauth}; {s}"),
            None => oauth,
        };
        let done = env.get_as(&back, PUBLIC_HOST, Some(&cookies)).await;
        assert_eq!((done.path(), done.error()), ("/settings/login".into(), "not_signed_in".into()), "{session:?}");
        assert!(done.clears_oauth_cookie());
    }
    let start = env.start("github", "?link=1", Some(&alice)).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    let res = env.hub.send(Method::POST, "/auth/logout", &alice, Value::Null).await;
    assert_eq!(res.status(), 204);
    let done = env.get_as(&back, PUBLIC_HOST, Some(&format!("{oauth}; {alice}"))).await;
    assert_eq!(done.error(), "not_signed_in", "signed out in between");

    assert!(env.identities(&bob).await.is_empty());
    assert_eq!(env.audit_count("identity_linked", "provider", "github").await, 0);
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

/// 从 start 到授权页，停在回调之前：（state，回调地址，`anycon_oauth` cookie）。
async fn prepare(env: &Env, provider: &str, user: FakeUser) -> (String, String, String) {
    env.fake.set_user(provider, user);
    let start = env.start(provider, "", None).await;
    let state = start.param("state").unwrap();
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    (state, back, oauth)
}

#[tokio::test]
async fn a_state_works_once() {
    let env = Env::multi(&[GITHUB]).await;
    let (_, back, oauth) = prepare(&env, "github", gh("1")).await;
    let first = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(first.location(), "/");
    // 重放同一个回调（授权码和 state 都已经用掉）
    let replay = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!((replay.path(), replay.error()), ("/login".into(), "invalid_state".into()));
    assert!(!replay.has_session() && replay.clears_oauth_cookie());
    assert_eq!(env.accounts().await, 1);
}

#[tokio::test]
async fn the_cookie_must_match_the_state_in_the_url() {
    let env = Env::multi(&[GITHUB]).await;
    let (state, back, oauth) = prepare(&env, "github", gh("1")).await;
    let (_, _, other_oauth) = prepare(&env, "github", gh("2")).await;

    // 没带 cookie（比如攻击者把回调链接发给受害者）/ 别的流程的 cookie / 空 cookie / 别的 cookie
    for (what, cookies) in [
        ("no cookie", None),
        ("another flow's cookie", Some(other_oauth.clone())),
        ("empty cookie", Some("anycon_oauth=".to_string())),
        ("the state as another cookie name", Some(format!("anycon_other={state}"))),
        ("a truncated cookie", Some(oauth[..oauth.len() - 1].to_string())),
    ] {
        let done = env.get_as(&back, PUBLIC_HOST, cookies.as_deref()).await;
        assert_eq!((done.error(), done.has_session()), ("invalid_state".into(), false), "{what}");
        assert!(done.clears_oauth_cookie(), "{what}");
    }
    // 这些失败尝试没有烧掉 state：带对的 cookie 仍然能完成
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(done.location(), "/");
}

#[tokio::test]
async fn a_missing_garbage_or_expired_state_is_invalid() {
    let env = Env::multi(&[GITHUB]).await;
    let (_, back, oauth) = prepare(&env, "github", gh("1")).await;
    let code = Url::parse(&format!("https://hub.test{back}"))
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "code")
        .unwrap()
        .1
        .into_owned();

    // 没有 state 参数
    let done = env.callback("github", &format!("code={code}"), Some(&oauth)).await;
    assert_eq!(done.error(), "invalid_state");
    // 没有任何参数、没有 cookie
    assert_eq!(env.callback("github", "", None).await.error(), "invalid_state");
    // state 不存在（cookie 与之一致也没用）
    let garbage = "f".repeat(64);
    let done =
        env.callback("github", &format!("code={code}&state={garbage}"), Some(&format!("anycon_oauth={garbage}"))).await;
    assert_eq!(done.error(), "invalid_state");
    // 过期：直接在库里放一个已经过期的 state（cookie 一致）
    let expired = "e".repeat(64);
    env.store()
        .put_oauth_state(
            anycon_hub::store::NewOAuthState {
                state_hash: hash_token(&expired),
                provider: "github".into(),
                purpose: anycon_hub::store::OAuthPurpose::Login,
                account_id: None,
                invite_hash: None,
                next_path: None,
                pkce_verifier: "v".repeat(43),
                expires_at_ms: now_ms() - 1,
            },
            now_ms() - 600_000,
        )
        .await
        .unwrap();
    let done =
        env.callback("github", &format!("code={code}&state={expired}"), Some(&format!("anycon_oauth={expired}"))).await;
    assert_eq!(done.error(), "invalid_state");
    assert!(env.fake.requests_to("token").is_empty(), "the provider is never called for a bad state");
    assert_eq!(env.accounts().await, 0);
}

#[tokio::test]
async fn a_state_belongs_to_the_provider_it_was_issued_for() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    let (state, back, oauth) = prepare(&env, "github", gh("1")).await;
    // 把 GitHub 流程的回调送到 Google 的回调地址
    let query = back.split_once('?').unwrap().1;
    let done = env.callback("google", query, Some(&oauth)).await;
    assert_eq!((done.error(), done.param("provider")), ("invalid_state".into(), Some("google".into())));
    assert!(env.fake.requests_to("token").is_empty());
    // state 被这次尝试用掉了（单次使用）：回到 GitHub 的回调也不行
    let retry = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(retry.error(), "invalid_state");
    assert!(env.store().take_oauth_state(&hash_token(&state), now_ms()).await.unwrap().is_none());
    assert_eq!(env.accounts().await, 0);
}

#[tokio::test]
async fn concurrent_callbacks_of_one_state_have_one_winner() {
    let env = Env::multi(&[GITHUB]).await;
    let (_, back, oauth) = prepare(&env, "github", gh("1")).await;
    let results = futures::future::join_all((0..6).map(|_| env.get_as(&back, PUBLIC_HOST, Some(&oauth)))).await;
    let ok = results.iter().filter(|r| r.has_session()).count();
    assert_eq!(ok, 1, "{:?}", results.iter().map(|r| r.location.clone()).collect::<Vec<_>>());
    assert_eq!(env.accounts().await, 1);
}

#[tokio::test]
async fn two_simultaneous_first_logins_of_one_identity_create_one_account() {
    let env = Env::multi(&[GITHUB]).await;
    // 同一个人开了两个标签页，两个流程各自走到回调，同时到达
    env.fake.set_user("github", gh("1").name("Twin"));
    let mut flows = Vec::new();
    for _ in 0..2 {
        let start = env.start("github", "", None).await;
        let oauth = start.oauth_cookie().unwrap();
        flows.push((env.authorize(&start).await, oauth));
    }
    let (a, b) = tokio::join!(
        env.get_as(&flows[0].0, PUBLIC_HOST, Some(&flows[0].1)),
        env.get_as(&flows[1].0, PUBLIC_HOST, Some(&flows[1].1)),
    );
    assert!(a.has_session() && b.has_session(), "{:?} {:?}", a.location, b.location);
    assert_eq!(env.me(&a.session()).await["id"], env.me(&b.session()).await["id"], "same account");
    assert_eq!(env.accounts().await, 1, "no orphan account");
    assert_eq!(env.audit_count("account_created", "method", "github").await, 1);
}

// ---------------------------------------------------------------------------
// 提供方的错误
// ---------------------------------------------------------------------------

#[tokio::test]
async fn denying_access_at_the_provider_is_cancelled() {
    let env = Env::multi(&[GITHUB]).await;
    env.fake.set_behavior(|b| b.deny_next_authorize = true);
    let start = env.start("github", "", None).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    assert!(back.contains("error=access_denied") && !back.contains("code="), "{back}");
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!((done.status, done.path(), done.error()), (302, "/login".into(), "cancelled".into()));
    assert_eq!(done.param("provider").as_deref(), Some("github"));
    assert!(!done.has_session() && done.clears_oauth_cookie());
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 1);
    assert!(env.fake.requests_to("token").is_empty());
    // state 已经用掉了
    assert_eq!(env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await.error(), "invalid_state");
    assert_eq!(env.accounts().await, 0);
}

#[tokio::test]
async fn other_provider_errors_and_missing_codes_are_provider_errors() {
    let env = Env::multi(&[GITHUB]).await;
    env.fake.set_behavior(|b| b.fail_next_authorize_with = Some("server_error".into()));
    let start = env.start("github", "", None).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(done.error(), "provider_error");

    // 有 state 没有 code
    let start = env.start("github", "", None).await;
    let state = start.param("state").unwrap();
    let done = env.callback("github", &format!("state={state}"), start.oauth_cookie().as_deref()).await;
    assert_eq!(done.error(), "provider_error");
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 2);
    assert_eq!(env.accounts().await, 0);
}

#[tokio::test]
async fn token_exchange_and_userinfo_failures_are_provider_errors() {
    let env = Env::multi(&[GITHUB]).await;
    let cases: Vec<(&str, Tweak)> = vec![
        ("token endpoint 500", Box::new(|b| b.token_status = Some(500))),
        ("token endpoint 400", Box::new(|b| b.token_status = Some(400))),
        ("GitHub-style error inside a 200", Box::new(|b| b.token_error_in_200 = true)),
        ("user endpoint 500", Box::new(|b| b.userinfo_status = Some(500))),
        ("user endpoint 401", Box::new(|b| b.userinfo_status = Some(401))),
        ("email endpoint 500", Box::new(|b| b.emails_status = Some(500))),
    ];
    for (what, set) in &cases {
        env.fake.set_behavior(|b| *b = Default::default());
        env.fake.set_behavior(|b| set(b));
        let done = env.sign_in("github", gh("1").email("a@example.com", true), "", None).await;
        assert_eq!((done.path(), done.error()), ("/login".into(), "provider_error".into()), "{what}");
        assert!(!done.has_session() && done.clears_oauth_cookie(), "{what}");
    }
    assert_eq!(env.accounts().await, 0, "no account from a half-finished login");
    assert_eq!(env.audit_count("login_fail", "method", "github").await, cases.len());

    // 对照：恢复之后同样的流程成功
    env.fake.set_behavior(|b| *b = Default::default());
    assert_eq!(env.sign_in("github", gh("1"), "", None).await.location(), "/");
}

#[tokio::test]
async fn a_github_without_the_email_permission_still_logs_in_without_an_email() {
    let env = Env::multi(&[GITHUB]).await;
    // /user/emails 回 403 / 404（应用没有邮箱权限）：能登录，只是没有已验证邮箱
    for status in [403, 404] {
        env.fake.set_behavior(|b| b.emails_status = Some(status));
        let session = env.sign_in_ok("github", gh(&format!("{status}")).email("x@example.com", true)).await;
        assert!(env.me(&session).await.get("email").is_none(), "{status}");
    }
}

#[tokio::test]
async fn a_slow_provider_times_out_with_provider_error() {
    let env = Env::with(Mode::Multi, &BOTH, |c| c.timing.oauth_timeout = Duration::from_millis(300)).await;
    let slow = Duration::from_secs(5);
    let cases: Vec<(&str, &str, Tweak)> = vec![
        ("slow token endpoint", "github", Box::new(move |b| b.token_delay = slow)),
        ("slow user endpoint", "github", Box::new(move |b| b.userinfo_delay = slow)),
        ("slow email endpoint", "github", Box::new(move |b| b.emails_delay = slow)),
        ("slow userinfo endpoint", "google", Box::new(move |b| b.userinfo_delay = slow)),
    ];
    for (what, provider, set) in &cases {
        env.fake.set_behavior(|b| *b = Default::default());
        env.fake.set_behavior(|b| set(b));
        let started = Instant::now();
        let done = env.sign_in(provider, gh("1").email("a@example.com", true), "", None).await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{what}: gave up after the timeout, not after the provider answered"
        );
        assert_eq!(done.error(), "provider_error", "{what}");
        assert!(!done.has_session() && done.clears_oauth_cookie(), "{what}");
    }
    assert_eq!(env.accounts().await, 0);
}

#[tokio::test]
async fn an_unreachable_provider_is_a_provider_error() {
    // 授权页还是假提供方的，换 token 的端点指向一个没人监听的端口
    let env = Env::with(Mode::Multi, &[GITHUB], |c| {
        c.oauth_endpoint_overrides.github.as_mut().unwrap().token = "http://127.0.0.1:1/token".into();
    })
    .await;
    let done = env.sign_in("github", gh("1"), "", None).await;
    assert_eq!(done.error(), "provider_error");
    assert!(!done.has_session());
    assert_eq!(env.accounts().await, 0);
}

// ---------------------------------------------------------------------------
// start 本身
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unknown_and_unconfigured_providers_are_not_found() {
    let env = Env::multi(&[GITHUB]).await;
    for path in [
        "/auth/oauth/google/start",   // 认识但没配
        "/auth/oauth/facebook/start", // 不认识
        "/auth/oauth/GitHub/start",   // 大小写敏感
        "/auth/callback/google?code=x&state=y",
        "/auth/callback/nope",
    ] {
        let res = env.hub.http.get(env.hub.url(path)).header("host", PUBLIC_HOST).send().await.unwrap();
        assert_eq!(res.status(), 404, "{path}");
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["error"]["code"], "not_found", "{path}");
    }
    // 没配任何提供方的 hub 上 github 也不存在
    let (bare, _m) = TestHub::start_multi().await;
    let res = bare.http.get(bare.url("/auth/oauth/github/start")).send().await.unwrap();
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn start_is_rate_limited_per_ip() {
    let env = Env::multi(&[GITHUB]).await;
    for i in 0..20 {
        assert_eq!(env.start("github", "", None).await.status, 302, "request {i}");
    }
    let res =
        env.hub.http.get(env.hub.url("/auth/oauth/github/start")).header("host", PUBLIC_HOST).send().await.unwrap();
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after"));
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "rate_limited");
    // 限速只管 start：回调不受影响
    assert_eq!(env.callback("github", "", None).await.error(), "invalid_state");
}

#[tokio::test]
async fn start_from_another_host_redirects_to_the_public_origin_without_starting_anything() {
    let env = Env::multi(&[GITHUB]).await;
    for host in ["192.168.1.10:8080", "evil.example", "hub.test:8443", "hub.test.evil.example"] {
        let r = env.get_as("/auth/oauth/github/start?next=%2Fchat&invite=abc&link=1", host, None).await;
        assert_eq!(r.status, 302, "{host}");
        assert_eq!(
            r.location(),
            "https://hub.test/auth/oauth/github/start?next=%2Fchat&invite=abc&link=1&hop=1",
            "{host}: same query on the configured public URL plus the once-only marker, built from config rather than from Host"
        );
        assert!(r.cookies.is_empty(), "{host}: no state, no cookie yet");
    }
    let r = env.get_as("/auth/oauth/github/start", "192.168.1.10:8080", None).await;
    assert_eq!(r.location(), "https://hub.test/auth/oauth/github/start?hop=1");
    assert!(env.fake.requests().is_empty());
    // 公网主机（带不带默认端口、大小写）直接开始
    for host in ["hub.test", "HUB.test", "hub.test:443"] {
        let r = env.get_as("/auth/oauth/github/start", host, None).await;
        assert!(
            r.location().starts_with(&format!("http://{}/github/authorize?", env.fake.addr)),
            "{host}: {:?}",
            r.location
        );
    }
}

#[tokio::test]
async fn start_redirects_to_the_public_origin_at_most_once_even_when_host_is_rewritten() {
    let env = Env::multi(&[GITHUB]).await;
    // 反向代理改写了 Host：到了公网地址，hub 看到的仍然不是公网主机。跳转只发生一次，不循环。
    let rewritten = "internal-hub.svc:8080";
    let first = env.get_as("/auth/oauth/github/start?next=%2Fchat", rewritten, None).await;
    assert_eq!(first.status, 302);
    let target = first.url();
    assert_eq!((target.host_str(), target.path()), (Some("hub.test"), "/auth/oauth/github/start"));
    let followed = env.get_as(&format!("{}?{}", target.path(), target.query().unwrap()), rewritten, None).await;
    assert_eq!(followed.status, 302);
    assert!(
        followed.location().starts_with(&format!("http://{}/github/authorize?", env.fake.addr)),
        "the second request goes on to the provider instead of redirecting again: {:?}",
        followed.location
    );
    let oauth = followed.oauth_cookie().expect("the state cookie is set on this (public) origin");
    assert!(!followed.location().contains("hop"), "the marker never reaches the provider: {:?}", followed.location);

    // 标记不进 state：`next` 照常保存，往返之后落在 /chat
    env.fake.set_user("github", gh("1"));
    let back = env.authorize(&followed).await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!((done.status, done.location()), (302, "/chat"));
    assert!(done.has_session());

    // 带着标记的请求不管 Host 是什么都不再跳；没有标记的才跳
    for host in ["192.168.1.10:8080", "evil.example", "hub.test"] {
        let r = env.get_as("/auth/oauth/github/start?hop=1", host, None).await;
        assert!(r.location().starts_with(&format!("http://{}/github/authorize?", env.fake.addr)), "{host}");
        assert!(r.oauth_cookie().is_some(), "{host}");
    }
    // 只认 hop=1：别的值不算标记，会跳一次，且目标里的标记是 1，不会因为前面的 hop=0 又跳
    let r = env.get_as("/auth/oauth/github/start?hop=0", "evil.example", None).await;
    assert_eq!(r.location(), "https://hub.test/auth/oauth/github/start?hop=0&hop=1");
    let target = r.url();
    let again = env.get_as(&format!("{}?{}", target.path(), target.query().unwrap()), "evil.example", None).await;
    assert!(
        again.location().starts_with(&format!("http://{}/github/authorize?", env.fake.addr)),
        "{:?}",
        again.location
    );

    // 关联用途也一样：跳过去之后没有登录会话，是 not_signed_in，不是再跳一次
    let r = env.get_as("/auth/oauth/github/start?link=1&hop=1", rewritten, None).await;
    assert_eq!((r.status, r.path(), r.error()), (302, "/login".to_string(), "not_signed_in".to_string()));
}

#[tokio::test]
async fn next_is_validated_and_defaults_to_the_root() {
    let env = Env::multi(&[GITHUB]).await;
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    let ok =
        ["/", "/chat", "/chat?x=1&y=2#frag", "/%2F%2Fevil.example/after-decoding-is-still-a-path", "/settings/login"];
    let long_ok = format!("/{}", "a".repeat(511));
    let long_bad = format!("/{}", "a".repeat(512));
    let bad = [
        "//evil.example/x",
        "/\\evil.example",
        "https://evil.example/",
        "evil.example",
        "javascript:alert(1)",
        "/ok\r\nSet-Cookie: pwn=1",
        "/tab\there",
        "",
        long_bad.as_str(),
    ];
    for next in ok.iter().copied().chain(std::iter::once(long_ok.as_str())) {
        let done = env.sign_in("github", gh("1"), &format!("?next={}", enc(next)), None).await;
        assert_eq!(done.location(), next, "kept: {next:?}");
    }
    for next in bad {
        let done = env.sign_in("github", gh("1"), &format!("?next={}", enc(next)), None).await;
        assert_eq!(done.location(), "/", "dropped: {next:?}");
    }
    // 非 ASCII 的合法路径：Location 里百分号编码
    let done = env.sign_in("github", gh("1"), &format!("?next={}", enc("/chat?q=é ü")), None).await;
    assert_eq!(done.location(), "/chat?q=%C3%A9%20%C3%BC");
}

// ---------------------------------------------------------------------------
// hub 这边出错：也是 302，不是 JSON
// ---------------------------------------------------------------------------

/// 在 hub 的库文件上另开一个连接，把一张表改名（模拟库出了故障：之后碰这张表的查询都报错）。
async fn break_table(env: &Env, table: &str) {
    use sqlx::sqlite::SqliteConnectOptions;
    use sqlx::ConnectOptions;
    let mut conn = SqliteConnectOptions::new().filename(env.hub.dir.path().join("hub.db")).connect().await.unwrap();
    // 表名是测试里写死的字面量
    let sql = sqlx::AssertSqlSafe(format!("ALTER TABLE {table} RENAME TO {table}_broken"));
    sqlx::query(sql).execute(&mut conn).await.unwrap();
}

#[tokio::test]
async fn an_internal_error_in_the_callback_is_a_redirect_with_server_error_not_a_json_body() {
    // 登录用途：换完用户信息、建好账号之后，写会话失败
    let env = Env::multi(&[GITHUB]).await;
    env.fake.set_user("github", gh("1"));
    let start = env.start("github", "", None).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    break_table(&env, "web_sessions").await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(done.status, 302, "a top-level navigation must not end on a raw error body");
    assert_eq!(done.location(), "/login?oauth_error=server_error&provider=github");
    assert!(!done.has_session() && done.clears_oauth_cookie());
    assert_eq!(env.audit_count("login_fail", "method", "github").await, 1, "a failed sign-in is audited as before");

    // state 都读不出来（库坏了）：不知道用途，回登录页
    let env = Env::multi(&[GITHUB]).await;
    env.fake.set_user("github", gh("1"));
    let start = env.start("github", "", None).await;
    let oauth = start.oauth_cookie().unwrap();
    let back = env.authorize(&start).await;
    break_table(&env, "oauth_states").await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&oauth)).await;
    assert_eq!(done.status, 302);
    assert_eq!(done.location(), "/login?oauth_error=server_error&provider=github");
    assert!(done.clears_oauth_cookie());

    // 关联用途：回设置页
    let env = Env::multi(&[GITHUB]).await;
    let session = env.sign_in_ok("github", gh("1")).await;
    let start = env.start("github", "?link=1", Some(&session)).await;
    let oauth = start.oauth_cookie().unwrap();
    env.fake.set_user("github", gh("2"));
    let back = env.authorize(&start).await;
    break_table(&env, "identities").await;
    let done = env.get_as(&back, PUBLIC_HOST, Some(&format!("{oauth}; {session}"))).await;
    assert_eq!(done.status, 302);
    assert_eq!(done.location(), "/settings/login?oauth_error=server_error&provider=github");
    assert!(done.clears_oauth_cookie());
}

// ---------------------------------------------------------------------------
// 身份 API 与登录方式
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unlinking_removes_the_identity_unless_it_is_the_last_way_to_sign_in() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    // 没有邮箱的 GitHub 账号：GitHub 是它唯一的登录方式
    let session = env.sign_in_ok("github", gh("1")).await;
    assert_eq!(env.me(&session).await["login_methods"], json!(["github"]));
    let res = env.delete("/api/me/identities/github", &session).await;
    assert_eq!(res.status(), 409);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "last_login_method");
    assert_eq!(env.identities(&session).await.len(), 1, "untouched");
    assert_eq!(env.audit_count("identity_unlinked", "provider", "github").await, 0);

    // 再关联 Google：现在可以解绑 GitHub，但剩下的 Google 又是最后一种
    env.sign_in("google", gh("g-1"), "?link=1", Some(&session)).await;
    assert_eq!(env.me(&session).await["login_methods"], json!(["github", "google"]));
    assert_eq!(env.delete("/api/me/identities/github", &session).await.status(), 204);
    let me = env.me(&session).await;
    assert_eq!(me["login_methods"], json!(["google"]));
    let identities = env.identities(&session).await;
    assert_eq!(identities.iter().map(|i| i["provider"].as_str().unwrap()).collect::<Vec<_>>(), ["google"]);
    assert_eq!(env.audit_count("identity_unlinked", "provider", "github").await, 1);
    assert_eq!(env.delete("/api/me/identities/google", &session).await.status(), 409);
    // 没绑过：204，什么都不做，也不审计
    assert_eq!(env.delete("/api/me/identities/github", &session).await.status(), 204);
    assert_eq!(env.audit_count("identity_unlinked", "provider", "github").await, 1);
    // 解绑之后，这个 GitHub 身份又可以被别人用（登录进新账号）
    let again = env.sign_in_ok("github", gh("1")).await;
    assert_ne!(env.me(&again).await["id"], me["id"]);
    // 不认识的提供方
    assert_eq!(env.delete("/api/me/identities/facebook", &session).await.status(), 404);
}

#[tokio::test]
async fn the_last_method_rule_counts_the_email_and_the_identities_together() {
    let env = Env::multi(&[GITHUB]).await;
    // 邮箱注册的账号：邮箱是唯一方式，解绑邮箱 409
    let session = env.email_sign_in("me@example.com").await;
    let res = env.delete("/api/me/email", &session).await;
    assert_eq!(
        (res.status().as_u16(), res.json::<Value>().await.unwrap()["error"]["code"].clone()),
        (409, json!("last_login_method"))
    );
    // 关联 GitHub 之后，邮箱可以解绑；解绑后 GitHub 是唯一的方式
    env.sign_in("github", gh("1"), "?link=1", Some(&session)).await;
    assert_eq!(env.delete("/api/me/email", &session).await.status(), 204);
    let me = env.me(&session).await;
    assert!(me.get("email").is_none());
    assert_eq!(me["login_methods"], json!(["github"]));
    assert_eq!(env.delete("/api/me/identities/github", &session).await.status(), 409);

    // GitHub 注册并带了验证过的邮箱：两种方式，任何一种都能先解绑
    let session = env.sign_in_ok("github", gh("2").email("two@example.com", true)).await;
    assert_eq!(env.me(&session).await["login_methods"], json!(["github", "email"]));
    assert_eq!(env.delete("/api/me/identities/github", &session).await.status(), 204);
    assert_eq!(env.me(&session).await["login_methods"], json!(["email"]));
    assert_eq!(env.delete("/api/me/email", &session).await.status(), 409);
}

#[tokio::test]
async fn the_identities_api_needs_a_session_and_the_csrf_header() {
    let env = Env::multi(&[GITHUB]).await;
    let session = env.sign_in_ok("github", gh("1").email("a@example.com", true)).await;
    let res = env.hub.http.get(env.hub.url("/api/me/identities")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let res = env.hub.http.delete(env.hub.url("/api/me/identities/github")).send().await.unwrap();
    assert!(matches!(res.status().as_u16(), 401 | 403), "{}", res.status());
    // 带会话但没有 X-Anycon：被 CSRF 挡掉
    let res =
        env.hub.http.delete(env.hub.url("/api/me/identities/github")).header("cookie", &session).send().await.unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "csrf");
    assert_eq!(env.identities(&session).await.len(), 1);
}

#[tokio::test]
async fn login_methods_in_config_and_me_follow_the_configured_providers() {
    // multi：github、google、email（没有 token）
    let env = Env::multi(&BOTH).await;
    let config: Value = env.hub.http.get(env.hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(config["login_methods"], json!(["github", "google", "email"]));
    // 只配 Google
    let env = Env::multi(&[GOOGLE]).await;
    let config: Value = env.hub.http.get(env.hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(config["login_methods"], json!(["google", "email"]));

    // single：多一个 token，顺序固定
    let env = Env::single(&BOTH).await;
    let config: Value = env.hub.http.get(env.hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(config["login_methods"], json!(["github", "google", "email", "token"]));
    // 管理员 me.login_methods：token；绑了 GitHub 之后 github 在前
    let admin = env.hub.login_admin().await;
    assert_eq!(env.me(&admin).await["login_methods"], json!(["token"]));
    env.sign_in("github", gh("1"), "?link=1", Some(&admin)).await;
    assert_eq!(env.me(&admin).await["login_methods"], json!(["github", "token"]));
    // 管理员可以解绑 GitHub（还有 token），但解绑不影响 token
    assert_eq!(env.delete("/api/me/identities/github", &admin).await.status(), 204);
    assert_eq!(env.me(&admin).await["login_methods"], json!(["token"]));

    // 没有 SMTP 的 single：没有 email
    let fake = FakeOAuth::start().await;
    let hub = TestHub::start_with(|c| fake.configure(c, &[GITHUB])).await;
    let config: Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(config["login_methods"], json!(["github", "token"]));
}

#[tokio::test]
async fn a_provider_that_is_no_longer_configured_is_not_a_login_method() {
    let env = Env::multi(&[GITHUB, GOOGLE]).await;
    let session = env.sign_in_ok("github", gh("1").email("a@example.com", true)).await;
    // 重启成只配 Google 的 hub（同一份数据）：库里的 GitHub 身份还在，但不算登录方式
    let dir = env.hub.dir.path().to_path_buf();
    let fake = FakeOAuth::start().await;
    env.hub.handle.shutdown().await;
    let mut config = Config::new("https://hub.test", &dir).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.enable_multi(anycon_hub::config::QuotaLimits::default(), anycon_hub::config::DEFAULT_DAILY_PAIRINGS);
    config.mailer_override = Some(Arc::new(RecordingMailer::default()));
    config.timing.email_verify_failure_delay = Duration::ZERO;
    fake.configure(&mut config, &[GOOGLE]);
    let handle = anycon_hub::run(config).await.unwrap();
    let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let get = |path: &str| http.get(format!("{}{path}", handle.base_url())).header("cookie", &session).send();
    let me: Value = get("/api/me").await.unwrap().json().await.unwrap();
    assert_eq!(me["login_methods"], json!(["email"]), "GitHub is not configured any more");
    // 库里的身份仍然列得出来，也能解绑（它本来就不算登录方式，所以不受「最后一种」限制）
    let list: Value = get("/api/me/identities").await.unwrap().json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    let res = http
        .delete(format!("{}/api/me/identities/github", handle.base_url()))
        .header("x-anycon", "1")
        .header("cookie", &session)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    // 而 github 的登录入口已经不存在
    let res = http.get(format!("{}/auth/oauth/github/start", handle.base_url())).send().await.unwrap();
    assert_eq!(res.status(), 404);
    handle.shutdown().await;
}
