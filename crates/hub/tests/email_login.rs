//! 邮箱验证码登录和账号里的邮箱绑定（ADR-37，docs/spec/hub.md § 2.2 / § 2.3 / § 7.2）。
//! 发信用记录型的假 `Mailer`，不碰真的 SMTP（真实 lettre 路径见 `smtp_real.rs`）。

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_hub::mail::{MailError, Mailer, OutgoingMail};
use async_trait::async_trait;
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

async fn post_json(hub: &TestHub, path: &str, body: Value, headers: &[(&str, &str)]) -> reqwest::Response {
    let mut req = hub.http.post(hub.url(path)).header("x-anycon", "1").json(&body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.send().await.unwrap()
}

async fn error_code(res: reqwest::Response) -> String {
    res.json::<Value>().await.unwrap()["error"]["code"].as_str().unwrap_or("").to_string()
}

/// 走完 `/auth/email/send` → 取码 → `/auth/email/verify`，返回 cookie。
async fn email_login(hub: &TestHub, mailer: &anycon_hub::mail::RecordingMailer, email: &str) -> String {
    let before = mailer.sent().len();
    let res = post_json(hub, "/auth/email/send", json!({ "email": email }), &[]).await;
    assert_eq!(res.status(), 200);
    hub.settle().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), before + 1, "a code mail was sent");
    let code = code_in(sent.last().unwrap());
    let res = post_json(hub, "/auth/email/verify", json!({ "email": email, "code": code }), &[]).await;
    assert_eq!(res.status(), 204, "{}", res.text().await.unwrap());
    cookie_pair(&res)
}

#[tokio::test]
async fn bind_an_email_then_log_in_with_it_from_a_fresh_browser() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;

    // /api/config 宣告了邮箱登录
    let cfg: Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["login_methods"], json!(["token", "email"]));

    // 绑定：发码 → 验证
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert!(me.get("email").is_none());
    assert_eq!(me["login_methods"], json!(["token"]));
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "  Admin@Example.COM " })).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body, json!({ "ok": true, "resend_after_secs": 60 }));
    hub.settle().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "admin@example.com", "the address is normalized: trimmed and lower-cased");
    assert!(sent[0].subject.contains("email address"), "{}", sent[0].subject);
    let code = code_in(&sent[0]);
    let res = hub
        .send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "ADMIN@example.com", "code": code }))
        .await;
    assert_eq!(res.status(), 200);
    let me: Value = res.json().await.unwrap();
    assert_eq!(me["email"], "admin@example.com");
    assert_eq!(me["login_methods"], json!(["token", "email"]));
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert_eq!(me["email"], "admin@example.com");

    // 另一个浏览器（没有任何 cookie）：用邮箱 + 码登录
    let cookie = email_login(&hub, &mailer, "ADMIN@Example.com").await;
    let me2: Value = hub.get("/api/me", &cookie).await.json().await.unwrap();
    assert_eq!(me2["id"], me["id"], "the same account");
    assert_eq!(me2["is_admin"], true);
    assert_ne!(cookie, admin, "a new session");

    // cookie 属性和 token 登录一致
    let res = post_json(&hub, "/auth/email/send", json!({ "email": "admin@example.com" }), &[]).await;
    assert_eq!(res.status(), 429, "(the resend limit applies, see the dedicated test)");

    // 审计：email 登录成功记 login_ok，方式 email；绑定记 email_changed
    let audit: Value = hub.get("/api/admin/audit", &admin).await.json().await.unwrap();
    let events = audit.as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["event"] == "login_ok" && e["detail"]["method"] == "email" && e["account_id"] == me["id"]),
        "{audit}"
    );
    assert!(events.iter().any(|e| e["event"] == "email_changed" && e["detail"]["change"] == "bound"), "{audit}");
}

#[tokio::test]
async fn the_session_cookie_has_the_same_attributes_as_a_token_login() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 204);
    let raw = set_cookie_raw(&res);
    assert!(raw.starts_with("anycon_session=") && raw.contains("HttpOnly") && raw.contains("SameSite=Lax"), "{raw}");
    assert!(raw.contains("; Secure") && !raw.contains("Domain"), "{raw}");
}

#[tokio::test]
async fn an_unbound_email_gets_no_mail_but_the_same_answer() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "bound@example.com").await;
    let before = mailer.sent().len();

    let bound = post_json(&hub, "/auth/email/send", json!({ "email": "bound@example.com" }), &[]).await;
    let unbound = post_json(&hub, "/auth/email/send", json!({ "email": "nobody@example.com" }), &[]).await;
    assert_eq!(bound.status(), 200);
    assert_eq!(unbound.status(), 200, "always 200");
    assert_eq!(bound.text().await.unwrap(), unbound.text().await.unwrap(), "identical bodies");
    hub.settle().await;
    let sent = mailer.sent();
    assert_eq!(sent.len(), before + 1, "only the bound address got a mail");
    assert_eq!(sent.last().unwrap().to, "bound@example.com");

    // 没绑定的邮箱，验证接口也只说「码不对」
    let res =
        post_json(&hub, "/auth/email/verify", json!({ "email": "nobody@example.com", "code": "123456" }), &[]).await;
    assert_eq!(res.status(), 401);
    assert_eq!(error_code(res).await, "invalid_code");
    // 邮箱格式不对：400
    let res = post_json(&hub, "/auth/email/send", json!({ "email": "not-an-email" }), &[]).await;
    assert_eq!(res.status(), 400);
    assert_eq!(error_code(res).await, "invalid_email");
}

/// 发信永远不返回的 mailer：用来证明 HTTP 响应不等发信。
struct HangingMailer;

#[async_trait]
impl Mailer for HangingMailer {
    async fn send(&self, _mail: OutgoingMail) -> Result<(), MailError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn the_response_never_waits_for_the_mail_so_timing_cannot_reveal_whether_an_email_is_bound() {
    let hub = TestHub::start_with(|c| c.mailer_override = Some(Arc::new(HangingMailer))).await;
    // 直接在库里绑一个邮箱（绕过需要收码的绑定流程）
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("bound@example.com")).await.unwrap();

    for email in ["bound@example.com", "nobody@example.com"] {
        let start = Instant::now();
        let res = tokio::time::timeout(
            Duration::from_secs(3),
            post_json(&hub, "/auth/email/send", json!({ "email": email }), &[]),
        )
        .await
        .expect("the response must not wait for the (never finishing) mail");
        assert_eq!(res.status(), 200, "{email}");
        assert!(start.elapsed() < Duration::from_secs(2), "{email}: {:?}", start.elapsed());
    }
    // 绑定接口同样不等发信
    let admin = hub.login_admin().await;
    let res = tokio::time::timeout(
        Duration::from_secs(3),
        hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "new@example.com" })),
    )
    .await
    .expect("binding does not wait for the mail either");
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn a_failing_smtp_server_does_not_change_the_answer() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    mailer.fail_next(1);
    let res = post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    assert_eq!(res.status(), 200, "the failure is logged, not reported");
    hub.settle().await;
    assert_eq!(mailer.sent().len(), 1, "only the binding mail; the login mail failed");
}

#[tokio::test]
async fn five_wrong_codes_void_the_code_even_for_the_right_one() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;

    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for i in 0..5 {
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": wrong }), &[]).await;
        assert_eq!(res.status(), 401, "attempt {i}");
        assert_eq!(error_code(res).await, "invalid_code");
    }
    // 第 6 次，码是对的，也已经作废
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 401);
    assert_eq!(error_code(res).await, "invalid_code");

    // 作废的码在库里已经没有了；要新码得重新发
    assert!(hub
        .handle
        .store()
        .list_email_codes("a@example.com", anycon_hub::store::EmailPurpose::Login)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn four_wrong_codes_then_the_right_one_still_works() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "111111" { "222222" } else { "111111" };
    for _ in 0..4 {
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": wrong }), &[]).await;
        assert_eq!(res.status(), 401);
    }
    // 用户可能抄成「123 456」这种带空格的样子
    let spaced = format!("{} {}", &code[..3], &code[3..]);
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": " A@example.com ", "code": spaced }), &[]).await;
    assert_eq!(res.status(), 204);
}

#[tokio::test]
async fn a_code_works_once() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 204);
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 401, "single use");
}

/// 一直要码直到手里有 `n` 个不同的登录码（返回它们，旧的在前）。重发间隔在这些用例里缩到很短。
async fn request_login_codes(
    hub: &TestHub,
    mailer: &anycon_hub::mail::RecordingMailer,
    email: &str,
    n: usize,
) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    while codes.len() < n {
        let before = mailer.sent().len();
        let res = post_json(hub, "/auth/email/send", json!({ "email": email }), &[]).await;
        assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
        hub.settle().await;
        assert_eq!(mailer.sent().len(), before + 1);
        let code = code_in(mailer.sent().last().unwrap());
        // 6 位数字，两个码撞上的概率是百万分之一：撞上了就再要一个，免得用例偶尔变红
        if !codes.contains(&code) {
            codes.push(code);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    codes
}

fn quick_resend(c: &mut anycon_hub::Config) {
    c.limits.email_resend_interval = Duration::from_millis(50);
}

#[tokio::test]
async fn requesting_another_code_does_not_void_the_one_in_your_inbox() {
    // 以前同一个邮箱只留最新一个码，任何人每 60 秒请求一次，就能把你刚收到的码作废。
    let (hub, mailer) = TestHub::start_mail_with(quick_resend).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;

    let codes = request_login_codes(&hub, &mailer, "a@example.com", 2).await;
    // 第一个码（你收到的）在「别人」又要了一个之后仍然能用
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": codes[0] }), &[]).await;
    assert_eq!(res.status(), 204, "the first code still works after a second one was requested");
    // 第二个码也还能用（各自一次性）
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": codes[1] }), &[]).await;
    assert_eq!(res.status(), 204);
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": codes[0] }), &[]).await;
    assert_eq!(res.status(), 401, "but each only once");
}

#[tokio::test]
async fn at_most_three_login_codes_are_valid_at_once_and_the_fourth_pushes_out_the_oldest() {
    let (hub, mailer) = TestHub::start_mail_with(quick_resend).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;

    let codes = request_login_codes(&hub, &mailer, "a@example.com", 4).await;
    let valid = |code: &String| {
        let hub = &hub;
        let code = code.clone();
        async move {
            post_json(hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await.status()
                == 204
        }
    };
    assert!(!valid(&codes[0]).await, "the oldest of four was pushed out");
    assert!(valid(&codes[1]).await);
    assert!(valid(&codes[2]).await);
    assert!(valid(&codes[3]).await);
}

#[tokio::test]
async fn five_submissions_use_up_every_live_code_so_more_codes_never_mean_more_guesses() {
    // 尝试次数每码各 5 次、一次提交对所有有效的码各计一次：3 个码同时有效时，攻击者一共也只有 5 次提交；
    // 5 次之后三个码都作废，连对的也不行。总的候选测试数是 5 × 发出的码数——和以前（每次只有一个码、每码 5 次）相比，
    // 每发一个码的猜测预算没变。
    let (hub, mailer) = TestHub::start_mail_with(quick_resend).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    let codes = request_login_codes(&hub, &mailer, "a@example.com", 3).await;
    let wrong: Vec<String> =
        (0u32..).map(|i| format!("{:06}", i * 7919 % 1_000_000)).filter(|w| !codes.contains(w)).take(5).collect();
    for w in &wrong {
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": w }), &[]).await;
        assert_eq!(res.status(), 401);
    }
    for code in &codes {
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
        assert_eq!(res.status(), 401, "all three codes were counted on every one of the five submissions");
    }
}

#[tokio::test]
async fn a_code_issued_later_starts_with_all_its_attempts() {
    let (hub, mailer) = TestHub::start_mail_with(quick_resend).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    let first = request_login_codes(&hub, &mailer, "a@example.com", 1).await.remove(0);
    let wrong = if first == "000000" { "000001" } else { "000000" };
    for _ in 0..4 {
        post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": wrong }), &[]).await;
    }
    // 第一个码被试了 4 次；再要一个新码：新码从 0 开始，有完整的 5 次
    let second = loop {
        let c = request_login_codes(&hub, &mailer, "a@example.com", 1).await.remove(0);
        if c != first {
            break c;
        }
    };
    for _ in 0..4 {
        let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": wrong }), &[]).await;
        assert_eq!(res.status(), 401);
    }
    // 新码只被试过 4 次，第 5 次还允许；第一个码被试了 4 + 1 = 5 次，已经用完
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": second }), &[]).await;
    assert_eq!(res.status(), 204, "the second code had all five of its own attempts");
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": first }), &[]).await;
    assert_eq!(res.status(), 401, "the first code ran out of attempts");
}

#[tokio::test]
async fn codes_expire() {
    let (hub, mailer) = TestHub::start_mail_with(|c| c.timing.email_code_ttl = Duration::from_millis(300)).await;
    // 先在库里直接绑一个邮箱，再测登录码的有效期
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("a@example.com")).await.unwrap();
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    tokio::time::sleep(Duration::from_millis(450)).await;
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 401, "expired");
    assert_eq!(error_code(res).await, "invalid_code");
    // 过期的码顺手被删掉
    assert!(hub
        .handle
        .store()
        .list_email_codes("a@example.com", anycon_hub::store::EmailPurpose::Login)
        .await
        .unwrap()
        .is_empty());

    // 默认有效期是 10 分钟，邮件里也这么写
    let (hub2, mailer2) = TestHub::start_mail().await;
    hub2.handle.store().set_account_email(&hub2.admin_account_id(), Some("a@example.com")).await.unwrap();
    post_json(&hub2, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub2.settle().await;
    assert!(mailer2.sent()[0].body.contains("10 minutes"), "{}", mailer2.sent()[0].body);
    let rows =
        hub2.handle.store().list_email_codes("a@example.com", anycon_hub::store::EmailPurpose::Login).await.unwrap();
    assert_eq!(rows.len(), 1);
    let ttl_ms = rows[0].expires_at_ms - rows[0].created_at_ms;
    assert_eq!(ttl_ms, 600_000);
}

#[tokio::test]
async fn the_same_email_can_only_be_asked_for_a_code_once_a_minute() {
    let (hub, mailer) = TestHub::start_mail().await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("a@example.com")).await.unwrap();

    let res = post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    assert_eq!(res.status(), 200);
    let res = post_json(&hub, "/auth/email/send", json!({ "email": " A@EXAMPLE.com" }), &[]).await;
    assert_eq!(res.status(), 429, "the key is the normalized email");
    let retry: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry), "{retry}");
    assert_eq!(error_code(res).await, "rate_limited");
    // 没绑定的邮箱同样受限（不然 429 就成了探测手段）
    assert_eq!(post_json(&hub, "/auth/email/send", json!({ "email": "x@example.com" }), &[]).await.status(), 200);
    assert_eq!(post_json(&hub, "/auth/email/send", json!({ "email": "x@example.com" }), &[]).await.status(), 429);
    hub.settle().await;
    assert_eq!(mailer.sent().len(), 1, "the limited request sent nothing");
}

#[tokio::test]
async fn sending_is_limited_to_five_requests_an_hour_per_ip() {
    let (hub, _mailer) = TestHub::start_mail().await;
    for i in 0..5 {
        let res = post_json(&hub, "/auth/email/send", json!({ "email": format!("user{i}@example.com") }), &[]).await;
        assert_eq!(res.status(), 200, "request {i}");
    }
    let res = post_json(&hub, "/auth/email/send", json!({ "email": "user9@example.com" }), &[]).await;
    assert_eq!(res.status(), 429, "the sixth request from the same IP");
    assert!(res.headers().contains_key("retry-after"));
    // 一小时 5 次：等待时间是分钟的量级
    let retry: u64 = res.headers()["retry-after"].to_str().unwrap().parse().unwrap();
    assert!(retry > 60, "{retry}");
}

#[tokio::test]
async fn verifying_is_limited_to_twenty_attempts_a_minute_per_ip() {
    let (hub, _mailer) = TestHub::start_mail().await;
    for i in 0..20 {
        let res =
            post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": "000000" }), &[]).await;
        assert_eq!(res.status(), 401, "attempt {i}");
    }
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": "000000" }), &[]).await;
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn the_ip_limit_follows_the_forwarded_address_from_a_trusted_proxy() {
    // 测试客户端是 127.0.0.1：把它设成受信代理，限速就按 X-Forwarded-For 里的真实地址算
    let (hub, _m) = TestHub::start_mail_with(|c| c.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()]).await;
    for i in 0..5 {
        let res = post_json(
            &hub,
            "/auth/email/send",
            json!({ "email": format!("u{i}@example.com") }),
            &[("x-forwarded-for", "203.0.113.7")],
        )
        .await;
        assert_eq!(res.status(), 200);
    }
    let res = post_json(
        &hub,
        "/auth/email/send",
        json!({ "email": "u9@example.com" }),
        &[("x-forwarded-for", "203.0.113.7")],
    )
    .await;
    assert_eq!(res.status(), 429);
    let res = post_json(
        &hub,
        "/auth/email/send",
        json!({ "email": "u9@example.com" }),
        &[("x-forwarded-for", "203.0.113.8")],
    )
    .await;
    assert_eq!(res.status(), 200, "another client address has its own bucket");
}

#[tokio::test]
async fn failed_and_successful_email_logins_are_audited_with_the_method() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(
        &hub,
        "/auth/email/verify",
        json!({ "email": "a@example.com", "code": "999999" }),
        &[("user-agent", "TestAgent/1.0")],
    )
    .await;
    email_login(&hub, &mailer, "a@example.com").await;
    let audit: Value = hub.get("/api/admin/audit", &admin).await.json().await.unwrap();
    let events = audit.as_array().unwrap();
    let fail =
        events.iter().find(|e| e["event"] == "login_fail" && e["detail"]["method"] == "email").expect("login_fail");
    assert_eq!(fail["user_agent"], "TestAgent/1.0");
    assert!(fail["ip_prefix"].as_str().unwrap().ends_with("/24"));
    assert!(events.iter().any(|e| e["event"] == "login_ok" && e["detail"]["method"] == "email"));
    // 审计里没有邮箱、没有码
    let dump = audit.to_string();
    assert!(!dump.contains("a@example.com") && !dump.contains("999999"), "{dump}");
}

#[tokio::test]
async fn email_login_needs_the_json_content_type_and_the_csrf_header() {
    let (hub, _m) = TestHub::start_mail().await;
    let res =
        hub.http.post(hub.url("/auth/email/send")).json(&json!({ "email": "a@example.com" })).send().await.unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(error_code(res).await, "csrf");
    let res = hub
        .http
        .post(hub.url("/auth/email/send"))
        .header("x-anycon", "1")
        .header("content-type", "text/plain")
        .body(r#"{"email":"a@example.com"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 415);
    let res = hub
        .http
        .post(hub.url("/auth/email/verify"))
        .json(&json!({ "email": "a@example.com", "code": "1" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn without_smtp_there_is_no_email_login() {
    let hub = TestHub::start().await;
    let cfg: Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["login_methods"], json!(["token"]));
    let admin = hub.login_admin().await;
    for (path, body) in [
        ("/auth/email/send", json!({ "email": "a@example.com" })),
        ("/auth/email/verify", json!({ "email": "a@example.com", "code": "123456" })),
        ("/api/me/email", json!({ "email": "a@example.com" })),
        ("/api/me/email/verify", json!({ "email": "a@example.com", "code": "123456" })),
    ] {
        let res = hub.send(Method::POST, path, &admin, body).await;
        assert_eq!(res.status(), 404, "{path}");
    }
    assert_eq!(hub.send(Method::DELETE, "/api/me/email", &admin, Value::Null).await.status(), 404);
    // 其余不受影响
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert_eq!(me["login_methods"], json!(["token"]));
}

#[tokio::test]
async fn codes_are_in_the_language_of_the_request() {
    let (hub, mailer) = TestHub::start_mail().await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("a@example.com")).await.unwrap();
    post_json(
        &hub,
        "/auth/email/send",
        json!({ "email": "a@example.com" }),
        &[("accept-language", "zh-CN,zh;q=0.9,en;q=0.5")],
    )
    .await;
    hub.settle().await;
    let zh = mailer.sent().last().unwrap().clone();
    assert!(zh.subject.contains("登录码"), "{}", zh.subject);
    assert!(zh.body.contains("分钟内有效") && zh.body.contains("不是你本人操作就忽略这封邮件"), "{}", zh.body);
    assert_eq!(code_in(&zh).len(), 6);

    let hub2_email = "b@example.com";
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some(hub2_email)).await.unwrap();
    post_json(&hub, "/auth/email/send", json!({ "email": hub2_email }), &[("accept-language", "en-US,en;q=0.9")]).await;
    hub.settle().await;
    let en = mailer.sent().last().unwrap().clone();
    assert!(
        en.body.contains("Your Anycon sign-in code is") && en.body.contains("If you did not request it"),
        "{}",
        en.body
    );

    let hub3_email = "c@example.com";
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some(hub3_email)).await.unwrap();
    post_json(&hub, "/auth/email/send", json!({ "email": hub3_email }), &[]).await;
    hub.settle().await;
    assert!(mailer.sent().last().unwrap().body.contains("Your Anycon sign-in code is"), "English by default");
}

// ---------------------------------------------------------------------------
// 绑定 / 换绑 / 解绑
// ---------------------------------------------------------------------------

#[tokio::test]
async fn binding_needs_a_login_and_the_right_code_for_that_account() {
    let (hub, mailer) = TestHub::start_mail().await;
    // 没登录
    let res = hub
        .http
        .post(hub.url("/api/me/email"))
        .header("x-anycon", "1")
        .json(&json!({ "email": "a@example.com" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = hub
        .http
        .post(hub.url("/api/me/email/verify"))
        .header("x-anycon", "1")
        .json(&json!({ "email": "a@example.com", "code": "123456" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = hub.http.delete(hub.url("/api/me/email")).header("x-anycon", "1").send().await.unwrap();
    assert_eq!(res.status(), 401);

    let admin = hub.login_admin().await;
    // 不合法的邮箱
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "nope" })).await;
    assert_eq!(res.status(), 400);
    assert_eq!(error_code(res).await, "invalid_email");

    // 没发过码就验证 / 错码：400 invalid_code（不是 401，免得前端当成登录失效）
    let res = hub
        .send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": "123456" }))
        .await;
    assert_eq!(res.status(), 400);
    assert_eq!(error_code(res).await, "invalid_code");

    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    // 一个码只对发起它的账号有效
    let (_guest_id, guest) = hub.invite_account(&admin).await;
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &guest, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 400, "another account cannot use the code");
    // 码对 login 用途无效
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 401, "a binding code cannot log anyone in");
    // 发起的账号可以
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn a_bind_code_for_one_email_does_not_verify_another() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let res = hub
        .send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "other@example.com", "code": code }))
        .await;
    assert_eq!(res.status(), 400);
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert!(me.get("email").is_none(), "nothing was bound");
}

#[tokio::test]
async fn wrong_bind_codes_are_limited_to_five() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for _ in 0..5 {
        let res = hub
            .send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": wrong }))
            .await;
        assert_eq!(res.status(), 400);
    }
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 400, "voided after five wrong tries");
}

#[tokio::test]
async fn an_email_can_belong_to_one_account_only() {
    let (hub, mailer) = TestHub::start_mail_with(|c| c.limits.email_resend_interval = Duration::from_secs(1)).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "shared@example.com").await;

    let (_id, guest) = hub.invite_account(&admin).await;
    // 发码阶段就拒绝，不发信
    let before = mailer.sent().len();
    let res = hub.send(Method::POST, "/api/me/email", &guest, json!({ "email": "SHARED@example.com" })).await;
    assert_eq!(res.status(), 409);
    assert_eq!(error_code(res).await, "email_taken");
    hub.settle().await;
    assert_eq!(mailer.sent().len(), before, "no code is sent for a taken address");
    // 自己的邮箱再绑一遍不算被占用
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "shared@example.com" })).await;
    assert_eq!(res.status(), 200);

    // 竞态：两个账号几乎同时拿到同一个邮箱的码，后验证的那个拿到 409
    hub.handle.store().set_account_email(&hub.admin_account_id(), None).await.unwrap();
    hub.send(Method::POST, "/api/me/email", &guest, json!({ "email": "race@example.com" })).await;
    hub.settle().await;
    let guest_code = code_in(mailer.sent().last().unwrap());
    // 管理员在 guest 验证之前先把这个邮箱占了
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("race@example.com")).await.unwrap();
    let res = hub
        .send(Method::POST, "/api/me/email/verify", &guest, json!({ "email": "race@example.com", "code": guest_code }))
        .await;
    assert_eq!(res.status(), 409);
    assert_eq!(error_code(res).await, "email_taken");
}

#[tokio::test]
async fn changing_the_email_moves_the_login_to_the_new_address() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "old@example.com").await;
    bind_email(&hub, &mailer, &admin, "new@example.com").await;

    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert_eq!(me["email"], "new@example.com");
    // 旧地址不能再登录（也不再发码）
    let before = mailer.sent().len();
    post_json(&hub, "/auth/email/send", json!({ "email": "old@example.com" }), &[]).await;
    hub.settle().await;
    assert_eq!(mailer.sent().len(), before, "the old address is no longer bound");
    // 新地址可以
    email_login(&hub, &mailer, "new@example.com").await;
    // 审计
    let audit: Value = hub.get("/api/admin/audit", &admin).await.json().await.unwrap();
    let changes: Vec<&str> = audit
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"] == "email_changed")
        .map(|e| e["detail"]["change"].as_str().unwrap())
        .collect();
    assert!(changes.contains(&"bound") && changes.contains(&"changed"), "{changes:?}");
    assert!(!audit.to_string().contains("old@example.com"), "no addresses in the audit log");
}

#[tokio::test]
async fn unbinding_keeps_at_least_one_way_to_log_in() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;

    // 管理员有 bootstrap token，随时可以解绑
    bind_email(&hub, &mailer, &admin, "admin@example.com").await;
    let res = hub.send(Method::DELETE, "/api/me/email", &admin, Value::Null).await;
    assert_eq!(res.status(), 204);
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert!(me.get("email").is_none());
    assert_eq!(me["login_methods"], json!(["token"]));
    // 解绑后再解绑：幂等
    assert_eq!(hub.send(Method::DELETE, "/api/me/email", &admin, Value::Null).await.status(), 204);
    // 解绑之后不能再用它登录
    let before = mailer.sent().len();
    post_json(&hub, "/auth/email/send", json!({ "email": "admin@example.com" }), &[]).await;
    hub.settle().await;
    assert_eq!(mailer.sent().len(), before);

    // 受邀用户一开始没有 token：邮箱是他唯一的登录方式
    let (_id, guest) = hub.invite_account(&admin).await;
    let me: Value = hub.get("/api/me", &guest).await.json().await.unwrap();
    assert_eq!(me["login_methods"], json!([]));
    bind_email(&hub, &mailer, &guest, "guest@example.com").await;
    let me: Value = hub.get("/api/me", &guest).await.json().await.unwrap();
    assert_eq!(me["login_methods"], json!(["email"]));
    let res = hub.send(Method::DELETE, "/api/me/email", &guest, Value::Null).await;
    assert_eq!(res.status(), 409);
    assert_eq!(error_code(res).await, "last_login_method");
    let me: Value = hub.get("/api/me", &guest).await.json().await.unwrap();
    assert_eq!(me["email"], "guest@example.com", "still bound");

    // 建一个登录 token 之后就可以解绑
    let res = hub.send(Method::POST, "/api/me/tokens", &guest, json!({ "label": "laptop" })).await;
    assert_eq!(res.status(), 200);
    let me: Value = hub.get("/api/me", &guest).await.json().await.unwrap();
    assert_eq!(me["login_methods"], json!(["token", "email"]));
    assert_eq!(hub.send(Method::DELETE, "/api/me/email", &guest, Value::Null).await.status(), 204);
    let audit: Value = hub.get("/api/admin/audit", &admin).await.json().await.unwrap();
    assert!(audit
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"] == "email_changed" && e["detail"]["change"] == "removed"));
}

#[tokio::test]
async fn the_resend_limit_is_per_purpose_so_binding_then_logging_in_works_back_to_back() {
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    // 绑定刚发过码，立刻用同一个邮箱登录不该被 60 秒限制挡住
    email_login(&hub, &mailer, "a@example.com").await;
    // 绑定接口自己也有 60 秒限制
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    assert_eq!(res.status(), 429);
}

#[tokio::test]
async fn binding_is_limited_to_five_requests_an_hour_per_ip() {
    let (hub, _m) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    for i in 0..5 {
        let res =
            hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": format!("u{i}@example.com") })).await;
        assert_eq!(res.status(), 200, "{i}");
    }
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "u9@example.com" })).await;
    assert_eq!(res.status(), 429);
}

#[tokio::test]
async fn racing_requests_with_the_right_code_log_in_exactly_once() {
    let (hub, mailer) = TestHub::start_mail().await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("a@example.com")).await.unwrap();
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let attempts =
        (0..10).map(|_| post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]));
    let statuses: Vec<u16> =
        futures::future::join_all(attempts).await.into_iter().map(|r| r.status().as_u16()).collect();
    assert_eq!(statuses.iter().filter(|s| **s == 204).count(), 1, "{statuses:?}");
    assert_eq!(statuses.iter().filter(|s| **s == 401).count(), 9, "{statuses:?}");
    // 只多了一个登录会话
    let admin = hub.login_admin().await;
    let sessions: Value = hub.get("/api/me/sessions", &admin).await.json().await.unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 2, "the admin's own session plus the one email login: {sessions}");
}

#[tokio::test]
async fn parallel_guessing_cannot_outlive_the_attempt_limit() {
    let (hub, mailer) = TestHub::start_mail().await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("a@example.com")).await.unwrap();
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    // 20 个并发请求，只有最后一个带着对的码；错的码互不相同
    let wrong: Vec<String> =
        (0..19).map(|i| format!("{:06}", (code.parse::<u32>().unwrap() + 1 + i) % 1_000_000)).collect();
    let mut attempts = Vec::new();
    for w in &wrong {
        attempts.push(post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": w }), &[]));
    }
    attempts.push(post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]));
    let results = futures::future::join_all(attempts).await;
    let ok = results.iter().filter(|r| r.status() == 204).count();
    // 计数先于比较：并发请求里排在第 6 个以后的，包括对的那个，一律作废
    let rows =
        hub.handle.store().list_email_codes("a@example.com", anycon_hub::store::EmailPurpose::Login).await.unwrap();
    assert!(rows.is_empty(), "the code is gone, either used or voided");
    assert!(ok <= 1);
}

// ---------------------------------------------------------------------------
// 邮箱验证码的滥用防护：每日上限、失败延迟、绑定码按（账号，邮箱）隔离、尝试计数
// ---------------------------------------------------------------------------

/// 受信的本机代理：测试里靠 `X-Forwarded-For` 假装来自不同的 IP。
fn behind_local_proxy(c: &mut anycon_hub::Config) {
    c.trusted_proxies = vec!["127.0.0.0/8".parse().unwrap()];
}

async fn post_from(hub: &TestHub, ip: &str, path: &str, body: Value) -> reqwest::Response {
    post_json(hub, path, body, &[("x-forwarded-for", ip)]).await
}

#[tokio::test]
async fn an_email_gets_at_most_a_fixed_number_of_codes_a_day_whatever_the_ip() {
    // 按 IP 的限速换个地址就绕开了；每个邮箱每天的发码上限与请求来自哪里无关。默认 10 个，这里调成 3 个。
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        behind_local_proxy(c);
        quick_resend(c);
        c.limits.email_codes_per_day = 3;
    })
    .await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await; // 绑定码另算，不占登录码的名额

    for i in 0..3 {
        let res =
            post_from(&hub, &format!("203.0.113.{i}"), "/auth/email/send", json!({ "email": "a@example.com" })).await;
        assert_eq!(res.status(), 200, "code {i}");
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let res = post_from(&hub, "203.0.113.9", "/auth/email/send", json!({ "email": "a@example.com" })).await;
    assert_eq!(res.status(), 429, "a fourth code in a day from a fresh IP");
    assert_eq!(error_code(res).await, "rate_limited");
    // 没绑定的邮箱同样受限：不然 429 就成了探测手段
    for i in 0..3 {
        let res =
            post_from(&hub, &format!("198.51.100.{i}"), "/auth/email/send", json!({ "email": "nobody@example.com" }))
                .await;
        assert_eq!(res.status(), 200);
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let res = post_from(&hub, "198.51.100.9", "/auth/email/send", json!({ "email": "nobody@example.com" })).await;
    assert_eq!(res.status(), 429, "an unbound address is limited exactly like a bound one");
    hub.settle().await;
    assert_eq!(mailer.sent().len(), 1 + 3, "one binding mail and three login codes; the refused request sent nothing");

    // 登录和绑定分开计：登录的名额用完了，同一个邮箱的绑定码照常发
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    assert_eq!(res.status(), 200, "binding has its own daily count");
}

#[tokio::test]
async fn too_many_failures_lock_the_email_for_the_day_whatever_the_ip() {
    // 每个邮箱每天累计验证失败 N 次（默认 30，这里 4）后，当天不再接受它的码，换 IP 也一样。
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        behind_local_proxy(c);
        c.limits.email_failures_per_day = 4;
    })
    .await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "000000" { "000001" } else { "000000" };

    // 4 次错码，每次来自不同的 IP（这个码一共有 5 次机会，还没用完）
    for i in 0..4 {
        let res = post_from(
            &hub,
            &format!("203.0.113.{i}"),
            "/auth/email/verify",
            json!({ "email": "a@example.com", "code": wrong }),
        )
        .await;
        assert_eq!(res.status(), 401, "wrong code {i}");
    }
    // 现在对的码也不收了（码本身还在，只是这个邮箱今天被锁了）
    let res =
        post_from(&hub, "203.0.113.200", "/auth/email/verify", json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 401, "locked for the day, from a fresh IP");
    assert_eq!(error_code(res).await, "invalid_code", "the same answer as any other failure");
    let rows =
        hub.handle.store().list_email_codes("a@example.com", anycon_hub::store::EmailPurpose::Login).await.unwrap();
    assert_eq!(rows.len(), 1, "the code itself was not even looked at");
    assert_eq!(rows[0].attempts, 4);
}

#[tokio::test]
async fn the_failure_lock_of_login_does_not_lock_binding_the_same_address() {
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        quick_resend(c);
        c.limits.email_failures_per_day = 2;
    })
    .await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    request_login_codes(&hub, &mailer, "a@example.com", 1).await;
    for _ in 0..2 {
        let res =
            post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": "000000" }), &[]).await;
        assert_eq!(res.status(), 401);
    }
    // 登录被锁；绑定同一个邮箱照常
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    assert_eq!(res.status(), 200);
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn bind_failures_lock_the_address_for_binding_too() {
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        behind_local_proxy(c);
        c.limits.email_failures_per_day = 2;
    })
    .await;
    let admin = hub.login_admin().await;
    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for i in 0..2 {
        let res = hub
            .http
            .post(hub.url("/api/me/email/verify"))
            .header("x-anycon", "1")
            .header("cookie", &admin)
            .header("x-forwarded-for", format!("203.0.113.{i}"))
            .json(&json!({ "email": "a@example.com", "code": wrong }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 400, "the right code is refused once the address collected its failures");
    assert_eq!(error_code(res).await, "invalid_code");
}

#[tokio::test]
async fn failures_without_any_code_to_guess_do_not_count() {
    // 没有可猜的码（没要过、过期了）不算「猜」：既不该把人锁住，也不该为随便一个邮箱字符串往内存里塞计数器。
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        quick_resend(c);
        c.limits.email_failures_per_day = 2;
        c.limits.email_verify_per_min_per_ip = 100;
    })
    .await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    for _ in 0..6 {
        let res =
            post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": "123456" }), &[]).await;
        assert_eq!(res.status(), 401);
    }
    assert_eq!(hub.handle.state.account_limits.email_daily_failures.len(), 0, "nothing was tracked");
    let code = request_login_codes(&hub, &mailer, "a@example.com", 1).await.remove(0);
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 204, "the earlier failures (no code existed) did not lock the address");
}

#[tokio::test]
async fn failed_verification_always_takes_at_least_the_minimum_time_success_does_not() {
    // 有待验证的码和没有的，库里做的事不同，耗时差几十微秒；失败的响应一律在请求开始之后至少等够固定的时间。
    let delay = Duration::from_millis(700);
    let (hub, mailer) = TestHub::start_mail_with(|c| {
        quick_resend(c);
        c.timing.email_verify_failure_delay = delay;
        c.limits.email_failures_per_day = 3;
    })
    .await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "bound@example.com").await;
    let code = request_login_codes(&hub, &mailer, "bound@example.com", 1).await.remove(0);
    let wrong = if code == "000000" { "000001" } else { "000000" };

    let timed = |email: &'static str, code: String| {
        let hub = &hub;
        async move {
            let start = Instant::now();
            let res = post_json(hub, "/auth/email/verify", json!({ "email": email, "code": code }), &[]).await;
            (res.status().as_u16(), start.elapsed())
        }
    };
    // 绑定了、有待验证的码，错码
    let (status, took) = timed("bound@example.com", wrong.into()).await;
    assert_eq!((status, took >= delay), (401, true), "wrong code with a pending code: {took:?}");
    // 没绑定的邮箱（根本没有码）
    let (status, took) = timed("nobody@example.com", "123456".into()).await;
    assert_eq!((status, took >= delay), (401, true), "no code at all: {took:?}");
    // 邮箱格式不对
    let (status, took) = timed("not-an-email", "123456".into()).await;
    assert_eq!((status, took >= delay), (401, true), "malformed address: {took:?}");
    // 被锁住的邮箱（再来一次错码凑够 3 次失败，然后用对的码）
    let (_, _) = timed("bound@example.com", wrong.into()).await;
    let (_, _) = timed("bound@example.com", wrong.into()).await;
    let (status, took) = timed("bound@example.com", code.clone()).await;
    assert_eq!((status, took >= delay), (401, true), "locked address: {took:?}");
}

#[tokio::test]
async fn a_successful_verification_does_not_wait() {
    let delay = Duration::from_millis(700);
    let (hub, mailer) = TestHub::start_mail_with(|c| c.timing.email_verify_failure_delay = delay).await;
    let admin = hub.login_admin().await;
    bind_email(&hub, &mailer, &admin, "a@example.com").await;
    post_json(&hub, "/auth/email/send", json!({ "email": "a@example.com" }), &[]).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let start = Instant::now();
    let res = post_json(&hub, "/auth/email/verify", json!({ "email": "a@example.com", "code": code }), &[]).await;
    assert_eq!(res.status(), 204);
    assert!(start.elapsed() < delay, "success is not padded: {:?}", start.elapsed());
}

#[tokio::test]
async fn failed_binding_verification_waits_too_and_success_does_not() {
    let delay = Duration::from_millis(700);
    let (hub, mailer) = TestHub::start_mail_with(|c| c.timing.email_verify_failure_delay = delay).await;
    let admin = hub.login_admin().await;
    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "a@example.com" })).await;
    hub.settle().await;
    let code = code_in(mailer.sent().last().unwrap());
    let wrong = if code == "000000" { "000001" } else { "000000" };
    for body in [
        json!({ "email": "a@example.com", "code": wrong }),
        json!({ "email": "other@example.com", "code": wrong }),
        json!({ "email": "not-an-email", "code": wrong }),
    ] {
        let start = Instant::now();
        let res = hub.send(Method::POST, "/api/me/email/verify", &admin, body.clone()).await;
        assert_eq!(res.status(), 400, "{body}");
        assert!(start.elapsed() >= delay, "{body}: {:?}", start.elapsed());
    }
    let start = Instant::now();
    let res =
        hub.send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "a@example.com", "code": code })).await;
    assert_eq!(res.status(), 200);
    assert!(start.elapsed() < delay, "{:?}", start.elapsed());
}

#[tokio::test]
async fn another_accounts_binding_request_for_the_same_address_does_not_void_yours() {
    // 绑定码按（账号，邮箱）存；以前 (邮箱, 用途) 只留一条，另一个账号对同一个邮箱要码就把你的顶掉了。
    let (hub, mailer) = TestHub::start_mail().await;
    let admin = hub.login_admin().await;
    let (_gid, guest) = hub.invite_account(&admin).await;

    hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "target@example.com" })).await;
    hub.settle().await;
    let admin_code = code_in(mailer.sent().last().unwrap());
    // 另一个账号马上对同一个邮箱也要一个绑定码（60 秒的窗口也是各算各的）
    let res = hub.send(Method::POST, "/api/me/email", &guest, json!({ "email": "target@example.com" })).await;
    assert_eq!(res.status(), 200, "the other account's own resend window is separate");
    hub.settle().await;
    let guest_code = code_in(mailer.sent().last().unwrap());
    assert_eq!(mailer.sent().len(), 2);

    // 各自的码只对各自的账号有效
    if admin_code != guest_code {
        let res = hub
            .send(
                Method::POST,
                "/api/me/email/verify",
                &guest,
                json!({ "email": "target@example.com", "code": admin_code }),
            )
            .await;
        assert_eq!(res.status(), 400, "an account cannot use the other account's code");
    }
    // 管理员的码没有被顶掉
    let res = hub
        .send(
            Method::POST,
            "/api/me/email/verify",
            &admin,
            json!({ "email": "target@example.com", "code": admin_code }),
        )
        .await;
    assert_eq!(res.status(), 200, "the admin's binding survived the other account's request");
    let me: Value = hub.get("/api/me", &admin).await.json().await.unwrap();
    assert_eq!(me["email"], "target@example.com");
}

#[tokio::test]
async fn binding_codes_have_their_own_daily_limit_per_address() {
    let (hub, _mailer) = TestHub::start_mail_with(|c| {
        behind_local_proxy(c);
        quick_resend(c);
        c.limits.email_codes_per_day = 3;
    })
    .await;
    let admin = hub.login_admin().await;
    let bind_from = |ip: String| {
        let hub = &hub;
        let admin = &admin;
        async move {
            hub.http
                .post(hub.url("/api/me/email"))
                .header("x-anycon", "1")
                .header("cookie", admin)
                .header("x-forwarded-for", ip)
                .json(&json!({ "email": "target@example.com" }))
                .send()
                .await
                .unwrap()
        }
    };
    for i in 0..3 {
        assert_eq!(bind_from(format!("203.0.113.{i}")).await.status(), 200, "code {i}");
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    let res = bind_from("203.0.113.99".into()).await;
    assert_eq!(res.status(), 429, "a fourth binding code for the same address in a day, from a fresh IP");
    // 登录码另算
    let res = post_from(&hub, "198.51.100.1", "/auth/email/send", json!({ "email": "target@example.com" })).await;
    assert_eq!(res.status(), 200);
}
