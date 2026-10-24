//! `ANYCON_MODE=multi` 的地基：没有 bootstrap 管理员、token / 邀请相关路由不存在、邮箱验证码登录即注册、
//! 管理员来自 `ANYCON_ADMIN_EMAILS`。`single` 的行为由其余测试文件守着，这里只放对照。

mod common;

use std::sync::Arc;
use std::time::Duration;

use anycon_hub::config::{Mode, OAuthClient, QuotaLimits, DEFAULT_DAILY_PAIRINGS};
use anycon_hub::mail::RecordingMailer;
use anycon_hub::{Config, Handle};
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

async fn post_json(hub: &TestHub, path: &str, body: Value) -> reqwest::Response {
    hub.http.post(hub.url(path)).header("x-anycon", "1").json(&body).send().await.unwrap()
}

async fn error_code(res: reqwest::Response) -> String {
    res.json::<Value>().await.unwrap()["error"]["code"].as_str().unwrap_or("").to_string()
}

fn quick_resend(c: &mut Config) {
    c.limits.email_resend_interval = Duration::from_millis(1);
}

/// 要一个登录码，返回邮件里的码。
async fn request_code(hub: &TestHub, mailer: &RecordingMailer, email: &str) -> String {
    let before = mailer.sent().len();
    let res = post_json(hub, "/auth/email/send", json!({ "email": email })).await;
    assert_eq!(res.status(), 200);
    hub.settle().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), before + 1, "a code mail was sent to {email}");
    assert_eq!(sent.last().unwrap().to, email.trim().to_lowercase());
    code_in(sent.last().unwrap())
}

/// `/auth/email/send` → `/auth/email/verify`，返回会话 cookie。
async fn email_sign_in(hub: &TestHub, mailer: &RecordingMailer, email: &str) -> String {
    let code = request_code(hub, mailer, email).await;
    let res = post_json(hub, "/auth/email/verify", json!({ "email": email, "code": code })).await;
    assert_eq!(res.status(), 204, "{}", res.text().await.unwrap());
    cookie_pair(&res)
}

async fn me(hub: &TestHub, cookie: &str) -> Value {
    let res = hub.get("/api/me", cookie).await;
    assert_eq!(res.status(), 200);
    res.json().await.unwrap()
}

async fn config_of(hub: &TestHub) -> Value {
    hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap()
}

/// 管理员读审计：事件名 + 账号 id + 方式。
async fn audit(hub: &TestHub, admin: &str) -> Vec<Value> {
    let res = hub.get("/api/admin/audit", admin).await;
    assert_eq!(res.status(), 200);
    res.json::<Value>().await.unwrap().as_array().unwrap().clone()
}

fn count(events: &[Value], event: &str, method: &str) -> usize {
    events.iter().filter(|e| e["event"] == event && e["detail"]["method"] == method).count()
}

// ---------------------------------------------------------------------------
// 启动：没有 bootstrap 管理员
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_multi_hub_boots_without_a_bootstrap_admin_account_token_or_file() {
    let (hub, _mailer) = TestHub::start_multi_with(|c| {
        // 设了也被忽略（启动警告）：不会变成 bootstrap token
        c.admin_token = Some("a-token-that-multi-must-ignore".into());
    })
    .await;
    assert!(hub.handle.state.admin.is_none(), "no bootstrap admin");
    assert!(hub.handle.generated_admin_token.is_none() && hub.handle.admin_token_file.is_none());
    assert!(!hub.dir.path().join("admin-token").exists(), "the admin-token file is never written");
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 0, "no account exists before the first sign-up");
    assert_eq!(hub.handle.state.config.mode, Mode::Multi);

    // 那个被忽略的 token 登不进去（连路由都没有）
    let res = post_json(&hub, "/auth/token", json!({ "token": "a-token-that-multi-must-ignore" })).await;
    assert_eq!(res.status(), 404);
    // 健康检查等不受影响
    assert_eq!(hub.http.get(hub.url("/healthz")).send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn a_multi_hub_does_not_read_an_existing_admin_token_file() {
    let (hub, _mailer) = TestHub::start_multi().await;
    // 之前 single 留下的 admin-token 文件：既不读也不改
    let path = hub.dir.path().join("admin-token");
    assert!(!path.exists());
    hub.handle.shutdown().await;
    std::fs::write(&path, "leftover-from-single-mode\n").unwrap();
    let dir = hub.dir;

    let mut config = Config::new("https://hub.test", dir.path()).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
    config.mailer_override = Some(Arc::new(RecordingMailer::default()));
    let handle = anycon_hub::run(config).await.expect("multi hub starts next to a stale admin-token file");
    assert!(handle.state.admin.is_none() && handle.generated_admin_token.is_none());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "leftover-from-single-mode\n", "untouched");
    handle.shutdown().await;
}

#[tokio::test]
async fn a_multi_hub_without_any_way_to_sign_in_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::new("https://hub.test", dir.path()).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
    let err = anycon_hub::run(config.clone()).await.err().expect("nobody could sign in");
    assert!(err.to_string().contains("no way to sign in"), "{err}");
    // 只配 OAuth 也行（不需要 SMTP）
    config.oauth_github = Some(OAuthClient { client_id: "id".into(), client_secret: "secret".into() });
    anycon_hub::run(config).await.expect("OAuth alone is a way to sign in").shutdown().await;
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

#[tokio::test]
async fn token_invite_and_login_token_routes_do_not_exist_in_multi() {
    let (hub, mailer) = TestHub::start_multi_with(|c| c.admin_emails = vec!["boss@example.com".into()]).await;
    let boss = email_sign_in(&hub, &mailer, "boss@example.com").await;
    assert_eq!(me(&hub, &boss).await["is_admin"], true, "even an admin does not get them");

    let routes: [(Method, &str); 8] = [
        (Method::POST, "/auth/token"),
        (Method::POST, "/auth/invite"),
        (Method::GET, "/api/invites"),
        (Method::POST, "/api/invites"),
        (Method::DELETE, "/api/invites/abc"),
        (Method::GET, "/api/me/tokens"),
        (Method::POST, "/api/me/tokens"),
        (Method::DELETE, "/api/me/tokens/abc"),
    ];
    for (method, path) in routes {
        // 登录着、没登录：一样的 404 not_found（没有这个功能，也不是「要登录」）
        for cookie in [boss.as_str(), ""] {
            let res = hub
                .http
                .request(method.clone(), hub.url(path))
                .header("x-anycon", "1")
                .header("cookie", cookie)
                .json(&json!({ "token": "x", "code": "x", "label": "x" }))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{method} {path} (cookie: {})", !cookie.is_empty());
            assert_eq!(error_code(res).await, "not_found", "{method} {path}");
        }
    }

    // 其余的照常：会话、设备、管理、邮箱登录
    assert_eq!(hub.get("/api/me/sessions", &boss).await.status(), 200);
    assert_eq!(hub.get("/api/devices", &boss).await.status(), 200);
    assert_eq!(hub.get("/api/admin/status", &boss).await.status(), 200);
    assert_eq!(post_json(&hub, "/auth/logout", json!({})).await.status(), 204);
}

#[tokio::test]
async fn single_mode_keeps_the_token_and_invite_routes() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    assert_eq!(hub.get("/api/invites", &admin).await.status(), 200);
    assert_eq!(hub.get("/api/me/tokens", &admin).await.status(), 200);
    // 没登录是 401（路由在），不是 404
    assert_eq!(hub.http.get(hub.url("/api/invites")).send().await.unwrap().status(), 401);
    let (_, guest) = hub.invite_account(&admin).await;
    assert_eq!(me(&hub, &guest).await["is_admin"], false);
    let config = config_of(&hub).await;
    assert_eq!(config["mode"], "single");
    assert_eq!(config["login_methods"], json!(["token"]));
}

#[tokio::test]
async fn config_in_multi_never_offers_token_login() {
    let (hub, _mailer) = TestHub::start_multi().await;
    let config = config_of(&hub).await;
    assert_eq!(config["mode"], "multi");
    assert_eq!(config["login_methods"], json!(["email"]));
    assert_eq!(config["public_url"], "https://hub.test");

    // OAuth 配了就出现，顺序固定：github、google、email
    let (hub, _mailer) = TestHub::start_multi_with(|c| {
        c.oauth_google = Some(OAuthClient { client_id: "g".into(), client_secret: "s".into() });
        c.oauth_github = Some(OAuthClient { client_id: "h".into(), client_secret: "s".into() });
    })
    .await;
    assert_eq!(config_of(&hub).await["login_methods"], json!(["github", "google", "email"]));

    // 没有 SMTP、只有 OAuth
    let dir = tempfile::tempdir().unwrap();
    let mut c = Config::new("https://hub.test", dir.path()).unwrap();
    c.listen = "127.0.0.1:0".parse().unwrap();
    c.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
    c.oauth_google = Some(OAuthClient { client_id: "g".into(), client_secret: "s".into() });
    let handle: Handle = anycon_hub::run(c).await.unwrap();
    let url = format!("{}/api/config", handle.base_url());
    let config: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
    assert_eq!(config["login_methods"], json!(["google"]));
    handle.shutdown().await;

    // single 对照：邮箱在前，token 在最后
    let (hub, _mailer) = TestHub::start_mail().await;
    let config = config_of(&hub).await;
    assert_eq!((config["mode"].as_str(), &config["login_methods"]), (Some("single"), &json!(["email", "token"])));
}

// ---------------------------------------------------------------------------
// 邮箱验证码注册
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_email_login_creates_the_account_and_the_second_reuses_it() {
    let (hub, mailer) = TestHub::start_multi_with(|c| {
        quick_resend(c);
        c.admin_emails = vec!["boss@example.com".into()];
    })
    .await;
    let boss = email_sign_in(&hub, &mailer, "boss@example.com").await;
    let boss_id = me(&hub, &boss).await["id"].as_str().unwrap().to_string();
    let accounts_before = hub.handle.store().stats().await.unwrap().accounts;
    assert_eq!(accounts_before, 1);

    // 没注册过的邮箱：发码、验证 → 建账号并登录；邮箱写成规范化后的形式，显示名是 @ 之前的部分
    let cookie = email_sign_in(&hub, &mailer, "  Grace.Hopper@Example.COM ").await;
    let profile = me(&hub, &cookie).await;
    assert_eq!(profile["email"], "grace.hopper@example.com");
    assert_eq!(profile["display_name"], "grace.hopper");
    assert_eq!(profile["is_admin"], false);
    assert_eq!(profile["login_methods"], json!(["email"]), "no token in multi");
    let id = profile["id"].as_str().unwrap().to_string();
    assert_ne!(id, boss_id);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, accounts_before + 1);
    let account = hub.handle.store().find_account_by_email("grace.hopper@example.com").await.unwrap().unwrap();
    assert_eq!((account.id.as_str(), account.is_admin), (id.as_str(), false));

    // 会话 cookie 的属性和别的登录一样
    let code = request_code(&hub, &mailer, "grace.hopper@example.com").await;
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "grace.hopper@example.com", "code": code })).await;
    assert_eq!(res.status(), 204);
    let raw = set_cookie_raw(&res);
    assert!(raw.starts_with("anycon_session=") && raw.contains("HttpOnly") && raw.contains("SameSite=Lax"), "{raw}");
    assert!(raw.contains("; Secure"), "{raw}");

    // 第二次登录（另一个浏览器）：同一个账号，没有新账号
    let again = cookie_pair(&res);
    assert_ne!(again, cookie, "a new session");
    assert_eq!(me(&hub, &again).await["id"], id.as_str());
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, accounts_before + 1, "no second account");

    // 审计：建账号一次（方式 email），登录两次
    let events = audit(&hub, &boss).await;
    let created: Vec<&Value> =
        events.iter().filter(|e| e["event"] == "account_created" && e["account_id"] == id.as_str()).collect();
    assert_eq!(created.len(), 1, "{events:?}");
    assert_eq!(created[0]["detail"]["method"], "email");
    assert_eq!(count(&events, "account_created", "email"), 2, "boss and grace each signed up once");
    let logins = events.iter().filter(|e| e["event"] == "login_ok" && e["account_id"] == id.as_str()).count();
    assert_eq!(logins, 2);
}

#[tokio::test]
async fn a_login_code_goes_to_any_valid_address_in_multi_with_the_same_answer_and_limits() {
    let (hub, mailer) = TestHub::start_multi().await;
    let known = hub.handle.store().create_account_with_email("known", Some("known@example.com"), 1).await.unwrap();
    assert!(matches!(known, anycon_hub::store::CreateAccountOutcome::Created(_)));

    // 已注册 / 没注册：同样的 200、同样的响应体、都发了邮件（防枚举靠的是「不区分」，不是「不发」）
    let mut answers = Vec::new();
    for email in ["known@example.com", "stranger@example.com"] {
        let res = post_json(&hub, "/auth/email/send", json!({ "email": email })).await;
        answers.push((res.status().as_u16(), res.json::<Value>().await.unwrap()));
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[1], (200, json!({ "ok": true, "resend_after_secs": 60 })));
    hub.settle().await;
    let mut recipients: Vec<String> = mailer.sent().into_iter().map(|m| m.to).collect();
    recipients.sort();
    assert_eq!(recipients, ["known@example.com", "stranger@example.com"]);

    // 马上再要：两种地址一样 429 + Retry-After（60 秒的重发间隔对所有地址一视同仁）
    for email in ["known@example.com", "stranger@example.com"] {
        let res = post_json(&hub, "/auth/email/send", json!({ "email": email })).await;
        assert_eq!(res.status(), 429, "{email}");
        assert!(res.headers().contains_key("retry-after"));
        assert_eq!(error_code(res).await, "rate_limited");
    }

    // 地址不合法照旧 400
    let bad = post_json(&hub, "/auth/email/send", json!({ "email": "not an email" })).await;
    assert_eq!(bad.status(), 400);
    assert_eq!(error_code(bad).await, "invalid_email");
}

#[tokio::test]
async fn a_wrong_code_for_a_new_address_creates_nothing() {
    let (hub, mailer) =
        TestHub::start_multi_with(|c| c.timing.email_verify_failure_delay = Duration::from_millis(150)).await;
    let code = request_code(&hub, &mailer, "new@example.com").await;
    let wrong = if code == "000000" { "000001" } else { "000000" };

    let started = std::time::Instant::now();
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "new@example.com", "code": wrong })).await;
    assert_eq!(res.status(), 401);
    assert_eq!(error_code(res).await, "invalid_code");
    assert!(started.elapsed() >= Duration::from_millis(150), "failures keep their padded timing");
    // 从来没发过码的地址：同样的 401
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "never@example.com", "code": "123456" })).await;
    assert_eq!(res.status(), 401);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 0, "no account without a correct code");

    // 对的码照样能用（上面的错误尝试没有用掉它）
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "new@example.com", "code": code })).await;
    assert_eq!(res.status(), 204);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_verifies_racing_to_sign_up_the_same_address_end_up_in_one_account() {
    let (hub, mailer) = TestHub::start_multi_with(|c| {
        quick_resend(c);
        c.admin_emails = vec!["boss@example.com".into()];
    })
    .await;
    let boss = email_sign_in(&hub, &mailer, "boss@example.com").await;
    // 同一个地址手里有两个有效的码，两个浏览器同时提交：两边都没找到账号，都去建
    let first = request_code(&hub, &mailer, "race@example.com").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = request_code(&hub, &mailer, "race@example.com").await;
    let (a, b) = tokio::join!(
        post_json(&hub, "/auth/email/verify", json!({ "email": "race@example.com", "code": first })),
        post_json(&hub, "/auth/email/verify", json!({ "email": "race@example.com", "code": second })),
    );
    assert_eq!((a.status().as_u16(), b.status().as_u16()), (204, 204), "the loser logs into the winner's account");
    let (cookie_a, cookie_b) = (cookie_pair(&a), cookie_pair(&b));
    let (id_a, id_b) = (me(&hub, &cookie_a).await["id"].clone(), me(&hub, &cookie_b).await["id"].clone());
    assert_eq!(id_a, id_b);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 2, "boss + the racing address");
    let events = audit(&hub, &boss).await;
    let created = events.iter().filter(|e| e["event"] == "account_created" && e["account_id"] == id_a).count();
    assert_eq!(created, 1, "{events:?}");
}

#[tokio::test]
async fn the_only_login_method_of_an_email_account_cannot_be_removed() {
    let (hub, mailer) = TestHub::start_multi().await;
    let cookie = email_sign_in(&hub, &mailer, "solo@example.com").await;
    let res = hub.send(Method::DELETE, "/api/me/email", &cookie, Value::Null).await;
    assert_eq!(res.status(), 409);
    assert_eq!(error_code(res).await, "last_login_method");
    assert_eq!(me(&hub, &cookie).await["email"], "solo@example.com");
}

// ---------------------------------------------------------------------------
// 管理员
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admin_emails_make_exactly_those_accounts_admins() {
    let (hub, mailer) = TestHub::start_multi_with(|c| {
        // 规范化后的邮箱；配置里写大小写不同的写法，由 `Config::from_vars` 负责规范化
        c.admin_emails = vec!["boss@example.com".into()];
    })
    .await;
    let boss = email_sign_in(&hub, &mailer, "Boss@Example.com").await;
    let guest = email_sign_in(&hub, &mailer, "guest@example.com").await;

    let boss_me = me(&hub, &boss).await;
    assert_eq!(boss_me["is_admin"], true);
    assert_eq!(me(&hub, &guest).await["is_admin"], false);

    // 管理员路由：配置里的管理员可以，别人 403
    for path in ["/api/admin/status", "/api/admin/accounts", "/api/admin/audit"] {
        assert_eq!(hub.get(path, &boss).await.status(), 200, "{path} for the admin");
        let res = hub.get(path, &guest).await;
        assert_eq!(res.status(), 403, "{path} for a regular account");
        assert_eq!(error_code(res).await, "forbidden");
    }

    // 管理页的账号行：is_admin 是有效值，admin_by_config 标明来源；库里的标记没被改
    let rows: Value = hub.get("/api/admin/accounts", &boss).await.json().await.unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let flags = |email: &str| {
        let r = rows.iter().find(|r| r["email"] == email).unwrap();
        (r["is_admin"].as_bool().unwrap(), r["admin_by_config"].as_bool().unwrap())
    };
    assert_eq!(flags("boss@example.com"), (true, true));
    assert_eq!(flags("guest@example.com"), (false, false));
    let stored = hub.handle.store().find_account_by_email("boss@example.com").await.unwrap().unwrap();
    assert!(!stored.is_admin, "the config-derived admin is never written into the database flag");
    assert_eq!(hub.get("/api/admin/status", &boss).await.json::<Value>().await.unwrap()["mode"], "multi");
}

#[tokio::test]
async fn without_admin_emails_nobody_is_an_admin_and_a_database_flag_still_counts() {
    let (hub, mailer) = TestHub::start_multi().await;
    let first = email_sign_in(&hub, &mailer, "first@example.com").await;
    assert_eq!(me(&hub, &first).await["is_admin"], false, "being first earns nothing in multi");
    assert_eq!(hub.get("/api/admin/accounts", &first).await.status(), 403);

    // 库里的标记（比如这个库以前跑过 single）：在 multi 里照样是管理员，但不是「由配置授予」
    let store = hub.handle.store();
    let legacy = store.create_account("legacy admin", true, 1).await.unwrap();
    store.set_account_email(&legacy.id, Some("legacy@example.com")).await.unwrap();
    let cookie = email_sign_in(&hub, &mailer, "legacy@example.com").await;
    assert_eq!(me(&hub, &cookie).await["is_admin"], true);
    let rows: Value = hub.get("/api/admin/accounts", &cookie).await.json().await.unwrap();
    let legacy_row = rows.as_array().unwrap().iter().find(|r| r["email"] == "legacy@example.com").unwrap();
    assert_eq!((&legacy_row["is_admin"], &legacy_row["admin_by_config"]), (&json!(true), &json!(false)));
}

#[tokio::test]
async fn admin_emails_do_nothing_in_single_mode() {
    let (hub, mailer) = TestHub::start_mail_with(|c| c.admin_emails = vec!["guest@example.com".into()]).await;
    let admin = hub.login_admin().await;
    // single 里邮箱只发给已绑定的账号，建一个已绑定的普通账号
    let (guest_id, guest) = hub.invite_account(&admin).await;
    hub.handle.store().set_account_email(&guest_id, Some("guest@example.com")).await.unwrap();
    let guest_again = {
        let code = request_code(&hub, &mailer, "guest@example.com").await;
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "guest@example.com", "code": code })).await;
        assert_eq!(res.status(), 204);
        cookie_pair(&res)
    };
    for cookie in [&guest, &guest_again] {
        assert_eq!(me(&hub, cookie).await["is_admin"], false, "ANYCON_ADMIN_EMAILS is ignored in single mode");
    }
    assert_eq!(hub.get("/api/admin/status", &guest).await.status(), 403);
    // 管理页的账号行：bootstrap 管理员是管理员，来源不是配置
    let rows: Value = hub.get("/api/admin/accounts", &admin).await.json().await.unwrap();
    assert!(rows.as_array().unwrap().iter().all(|r| r["admin_by_config"] == false), "{rows}");
}

#[tokio::test]
async fn single_mode_still_only_mails_codes_to_bound_addresses_and_never_signs_anyone_up() {
    let (hub, mailer) = TestHub::start_mail().await;
    let before = mailer.sent().len();
    let res = post_json(&hub, "/auth/email/send", json!({ "email": "stranger@example.com" })).await;
    assert_eq!(res.status(), 200);
    hub.settle().await;
    assert_eq!(mailer.sent().len(), before, "no mail for an address without an account in single mode");
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "stranger@example.com", "code": "123456" })).await;
    assert_eq!(res.status(), 401);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 1, "only the bootstrap admin");
}

// ---------------------------------------------------------------------------
// 从 single 切到 multi
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switching_an_existing_single_database_to_multi_keeps_accounts_and_drops_token_logins() {
    let dir = tempfile::tempdir().unwrap();
    let mailer = Arc::new(RecordingMailer::default());
    let new_config = |multi: bool| {
        let mut c = Config::new("https://hub.test", dir.path()).unwrap();
        c.listen = "127.0.0.1:0".parse().unwrap();
        c.timing.email_verify_failure_delay = Duration::ZERO;
        c.mailer_override = Some(mailer.clone());
        if multi {
            c.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
        }
        c
    };

    // single：bootstrap 管理员绑个邮箱，建一个登录 token
    let single = anycon_hub::run(new_config(false)).await.unwrap();
    let admin_token = single.generated_admin_token.clone().unwrap();
    let admin_id = single.state.admin.as_ref().unwrap().account_id.clone();
    single.store().set_account_email(&admin_id, Some("admin@example.com")).await.unwrap();
    single.store().create_account_token(&admin_id, "laptop", &anycon_hub::util::hash_token("tok"), 1).await.unwrap();
    single.shutdown().await;

    // multi：同一个库
    let multi = anycon_hub::run(new_config(true)).await.unwrap();
    assert!(multi.state.admin.is_none());
    let hub = TestHub {
        handle: multi,
        dir,
        http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
        admin_token: String::new(),
    };
    // token 登录不存在了（bootstrap token 和账号 token 都一样）
    for token in [admin_token.as_str(), "tok"] {
        assert_eq!(post_json(&hub, "/auth/token", json!({ "token": token })).await.status(), 404);
    }
    // 账号还在：用绑定的邮箱登录，库里的管理员标记照旧有效；登录方式里没有 token，也没有 token 管理
    let cookie = email_sign_in(&hub, &mailer, "admin@example.com").await;
    let profile = me(&hub, &cookie).await;
    assert_eq!(profile["id"], admin_id.as_str());
    assert_eq!(profile["is_admin"], true);
    assert_eq!(profile["login_methods"], json!(["email"]));
    assert_eq!(hub.get("/api/me/tokens", &cookie).await.status(), 404);
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 1);
}

// ---------------------------------------------------------------------------
// 从 multi 切到 single
// ---------------------------------------------------------------------------

#[tokio::test]
async fn switching_a_multi_database_with_a_granted_admin_to_single_gets_a_separate_bootstrap_account() {
    let dir = tempfile::tempdir().unwrap();
    let mailer = Arc::new(RecordingMailer::default());

    // multi：没有 bootstrap 账号；管理员标记授予了一个普通用户（现在两种模式都可以授予）
    let multi = anycon_hub::run(config_in(dir.path(), &mailer, true)).await.unwrap();
    let anycon_hub::store::CreateAccountOutcome::Created(user) =
        multi.store().create_account_with_email("Ulysses", Some("u@example.com"), 1).await.unwrap()
    else {
        panic!("created")
    };
    multi.store().set_account_admin(&user.id, true).await.unwrap();
    assert_eq!(multi.store().first_admin().await.unwrap().unwrap().id, user.id, "the setup: U is the first admin");
    multi.shutdown().await;

    // single：bootstrap token 登录的是它自己的账号，不是 U
    let single = anycon_hub::run(config_in(dir.path(), &mailer, false)).await.unwrap();
    let bootstrap_id = single.state.admin.as_ref().unwrap().account_id.clone();
    assert_ne!(bootstrap_id, user.id, "the bootstrap token must not be bound to a user who was granted admin");
    let token = single.generated_admin_token.clone().unwrap();
    let hub = TestHub {
        handle: single,
        dir,
        http: reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap(),
        admin_token: token.clone(),
    };
    let cookie = hub.login(&token).await;
    let profile = me(&hub, &cookie).await;
    assert_eq!(profile["id"], bootstrap_id.as_str());
    assert_eq!(profile["is_admin"], true);
    assert_eq!(profile["login_methods"], json!(["token"]));
    // U 原样还在，仍是管理员，也没有因此多出 token 登录
    let u = hub.handle.store().get_account(&user.id).await.unwrap().unwrap();
    assert!(u.is_admin && u.email.as_deref() == Some("u@example.com"));
    assert!(hub.handle.store().list_account_tokens(&user.id).await.unwrap().is_empty());

    // 再启动一次还是同一个账号（记在库里）
    let dir = hub.dir;
    hub.handle.shutdown().await;
    let again = anycon_hub::run(config_in(dir.path(), &mailer, false)).await.unwrap();
    assert_eq!(again.state.admin.as_ref().unwrap().account_id, bootstrap_id);
    again.shutdown().await;
}

/// 在 `dir` 这份数据上起 hub 的配置（`multi` 或 `single`）。
fn config_in(dir: &std::path::Path, mailer: &Arc<RecordingMailer>, multi: bool) -> Config {
    let mut c = Config::new("https://hub.test", dir).unwrap();
    c.listen = "127.0.0.1:0".parse().unwrap();
    c.mailer_override = Some(mailer.clone());
    if multi {
        c.enable_multi(QuotaLimits::default(), DEFAULT_DAILY_PAIRINGS);
    }
    c
}
