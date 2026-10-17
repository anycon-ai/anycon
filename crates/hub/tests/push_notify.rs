//! Web Push 订阅 / 发送、邮件通知和通知偏好（ADR-37，docs/spec/hub.md § 2.3、§ 6）。
//!
//! 推送服务用测试里起的本机假服务（`Config::push_allow_hosts` 放行 `127.0.0.1`），发信用记录型的假 `Mailer`。
//! daemon 用真实的 WebSocket 连上 hub，发 `kind = notify` 的信封，和线上一样走转发路径。

mod common;

use std::sync::Arc;
use std::time::Duration;

use anycon_hub::mail::RecordingMailer;
use anycon_protocol::{Control, DeviceKind, Kind, NotifyCategory};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use common::*;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use reqwest::Method;
use serde_json::{json, Value};

struct Rig {
    hub: TestHub,
    mailer: Arc<RecordingMailer>,
    push: FakePush,
    cookie: String,
    daemon: Seeded,
    web: Seeded,
    daemon_conn: FakeDevice,
    browser: Browser,
}

impl Rig {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    /// 起 hub、假推送服务，配好一台 daemon（已连上）和一台离线的 web 设备（已配对），并为这台 web 设备订阅了推送。
    async fn with(tweak: impl FnOnce(&mut anycon_hub::Config)) -> Self {
        let (hub, mailer) = TestHub::start_mail_with(|c| {
            c.push_allow_hosts = Some(vec!["127.0.0.1".into()]);
            tweak(c);
        })
        .await;
        let push = FakePush::start().await;
        let cookie = hub.login_admin().await;
        let daemon = hub.seed_device(DeviceKind::Daemon, "build-box").await;
        let web = hub.seed_device(DeviceKind::Web, "Chrome · macOS").await;
        hub.seed_paired(&daemon, &web).await;
        let daemon_conn = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "build-box").await;
        let browser = Browser::new();
        let rig = Self { hub, mailer, push, cookie, daemon, web, daemon_conn, browser };
        let res = rig.subscribe(&rig.web.id, &rig.browser, "sub-secret-path-1").await;
        assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
        rig
    }

    async fn subscribe(&self, device_id: &str, browser: &Browser, path: &str) -> reqwest::Response {
        self.hub
            .send(
                Method::POST,
                "/api/push/subscriptions",
                &self.cookie,
                browser.subscription(device_id, &self.push.endpoint(path)),
            )
            .await
    }

    /// daemon 给 `to` 发一封 notify，等 hub 回 `undeliverable`（目标离线）并等后台任务做完。
    async fn notify_offline(&mut self, to: &str, category: NotifyCategory, session: Option<&str>) {
        self.daemon_conn.send_env(to, Kind::Notify, notify_body(category, session)).await;
        self.expect_undeliverable(to).await;
        self.hub.settle().await;
    }

    /// 读到 `undeliverable { to }` 为止（沿途的 `peer_online` / `peer_offline` 跳过）。
    async fn expect_undeliverable(&mut self, to: &str) {
        loop {
            match self.daemon_conn.recv_control().await {
                Control::Undeliverable { to: t, .. } => return assert_eq!(t, to),
                Control::PeerOnline { .. } | Control::PeerOffline { .. } => continue,
                other => panic!("expected undeliverable, got {other:?}"),
            }
        }
    }

    async fn prefs(&self) -> Value {
        self.hub.get("/api/me/notifications", &self.cookie).await.json().await.unwrap()
    }

    async fn put_prefs(&self, categories: Value) -> reqwest::Response {
        self.hub.send(Method::PUT, "/api/me/notifications", &self.cookie, json!({ "categories": categories })).await
    }

    async fn subscriptions(&self) -> Vec<Value> {
        self.hub
            .get("/api/push/subscriptions", &self.cookie)
            .await
            .json::<Value>()
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .clone()
    }

    async fn assert_no_push(&self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(self.push.requests().is_empty(), "no push expected, got {:?}", self.push.requests().len());
    }
}

fn b64(s: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(s).unwrap()
}

// ---------------------------------------------------------------------------
// 发送：加密、请求头、VAPID
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_offline_device_gets_an_encrypted_push_with_the_right_headers() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("sess-1")).await;

    let reqs = rig.push.wait_for(1).await;
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert_eq!((req.method.as_str(), req.path.as_str()), ("POST", "/sub-secret-path-1"));
    assert_eq!(req.header("content-encoding"), "aes128gcm");
    assert_eq!(req.header("content-type"), "application/octet-stream");
    assert_eq!(req.header("ttl"), "3600");
    assert_eq!(req.header("urgency"), "high");
    assert!(req.body.len() <= 4096 && req.body.len() > 86, "{}", req.body.len());

    // 载荷：只有 category / node / session，浏览器的私钥能解开
    let payload = rig.browser.decrypt(req);
    assert_eq!(payload, json!({ "category": "permission_requested", "node": rig.daemon.id, "session": "sess-1" }));

    // VAPID：Authorization: vapid t=<JWT>, k=<公钥>，公钥就是 /api/config 里下发的那个
    let cfg: Value = rig.hub.http.get(rig.hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    let public_key = cfg["vapid_public_key"].as_str().unwrap();
    assert_eq!(cfg["push_available"], true);
    assert_eq!(b64(public_key).len(), 65);
    let auth = req.header("authorization");
    let rest = auth.strip_prefix("vapid t=").expect("vapid scheme");
    let (jwt, k) = rest.split_once(", k=").expect("t=…, k=…");
    assert_eq!(k, public_key);
    let parts: Vec<&str> = jwt.split('.').collect();
    assert_eq!(parts.len(), 3);
    let header: Value = serde_json::from_slice(&b64(parts[0])).unwrap();
    assert_eq!(header, json!({ "typ": "JWT", "alg": "ES256" }));
    let claims: Value = serde_json::from_slice(&b64(parts[1])).unwrap();
    assert_eq!(
        claims["aud"],
        format!("http://127.0.0.1:{}", rig.push.addr.port()),
        "aud is the origin of the endpoint"
    );
    assert_eq!(claims["sub"], "https://hub.test", "sub defaults to the public URL when it is https");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let exp = claims["exp"].as_u64().unwrap();
    assert!(
        (now + 12 * 3600 - 120..=now + 12 * 3600 + 120).contains(&exp),
        "exp is about 12 hours away: {exp} vs {now}"
    );
    let verifier = VerifyingKey::from_sec1_bytes(&b64(public_key)).unwrap();
    let sig = Signature::from_slice(&b64(parts[2])).unwrap();
    verifier
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .expect("the JWT verifies with the advertised key");
}

#[tokio::test]
async fn ttl_and_urgency_follow_the_category() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    for (category, ttl, urgency, session) in [
        (NotifyCategory::EnrollRequested, "300", "high", None),
        (NotifyCategory::TurnFinished, "3600", "normal", Some("s2")),
        (NotifyCategory::Error, "3600", "normal", Some("s3")),
        (NotifyCategory::Security, "3600", "normal", None),
    ] {
        rig.notify_offline(&web, category, session).await;
        let reqs = rig.push.requests();
        let last = reqs.last().unwrap();
        assert_eq!((last.header("ttl"), last.header("urgency")), (ttl, urgency), "{category:?}");
        let payload = rig.browser.decrypt(last);
        assert_eq!(payload["session"], session.map_or(Value::Null, |s| json!(s)), "{category:?}");
        assert_eq!(payload["node"], rig.daemon.id);
    }
}

#[tokio::test]
async fn the_vapid_subject_comes_from_the_config() {
    let mut rig = Rig::with(|c| c.vapid_subject = Some("mailto:ops@example.org".into())).await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, None).await;
    let req = rig.push.wait_for(1).await.remove(0);
    let jwt = req.header("authorization").strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap().0.to_string();
    let claims: Value = serde_json::from_slice(&b64(jwt.split('.').nth(1).unwrap())).unwrap();
    assert_eq!(claims["sub"], "mailto:ops@example.org");
}

#[tokio::test]
async fn each_message_uses_a_fresh_ephemeral_key_and_salt() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    let reqs = rig.push.wait_for(2).await;
    assert_ne!(reqs[0].body[..16], reqs[1].body[..16], "different salts");
    assert_ne!(reqs[0].body[21..86], reqs[1].body[21..86], "different ephemeral public keys");
    assert_eq!(rig.browser.decrypt(&reqs[0]), rig.browser.decrypt(&reqs[1]));
}

// ---------------------------------------------------------------------------
// 谁收推送
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_online_device_gets_the_envelope_and_no_push() {
    let mut rig = Rig::new().await;
    let mut web = FakeDevice::connect(&rig.hub, Some(&rig.web.token), DeviceKind::Web, "Chrome").await;
    rig.daemon_conn
        .send_env(&rig.web.id, Kind::Notify, notify_body(NotifyCategory::PermissionRequested, Some("s1")))
        .await;
    let env = web.recv_env().await;
    assert_eq!(env.kind, Kind::Notify, "online devices still get the notify envelope over the websocket");
    rig.hub.settle().await;
    rig.assert_no_push().await;

    // 下线之后再来一封，就推了
    drop(web);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let web_id = rig.web.id.clone();
    rig.notify_offline(&web_id, NotifyCategory::PermissionRequested, Some("s2")).await;
    assert_eq!(rig.push.wait_for(1).await.len(), 1);
}

#[tokio::test]
async fn only_the_recipient_device_is_pushed_not_every_web_device_of_the_account() {
    let mut rig = Rig::new().await;
    // 同账号的第二台 web 设备：也订阅了，但这封信不是发给它的
    let other = rig.hub.seed_device(DeviceKind::Web, "Safari").await;
    rig.hub.pair_live(&rig.daemon, &other).await;
    let other_browser = Browser::new();
    let res = rig.subscribe(&other.id, &other_browser, "other-secret-path").await;
    assert_eq!(res.status(), 200);

    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    let reqs = rig.push.wait_for(1).await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/sub-secret-path-1");

    rig.notify_offline(&other.id, NotifyCategory::Error, Some("s1")).await;
    let reqs = rig.push.wait_for(2).await;
    assert_eq!(reqs[1].path, "/other-secret-path");
    assert_eq!(other_browser.decrypt(&reqs[1])["category"], "error");
}

#[tokio::test]
async fn a_device_without_a_subscription_gets_nothing() {
    let mut rig = Rig::new().await;
    let bare = rig.hub.seed_device(DeviceKind::Web, "no push here").await;
    rig.hub.pair_live(&rig.daemon, &bare).await;
    rig.notify_offline(&bare.id, NotifyCategory::PermissionRequested, Some("s1")).await;
    rig.assert_no_push().await;
}

#[tokio::test]
async fn only_a_daemon_of_the_same_account_can_trigger_a_push() {
    let rig = Rig::new().await;
    // 另一台 web 设备（在线）给离线的 web 设备发 notify：不是 daemon，不推
    let sender = rig.hub.seed_device(DeviceKind::Web, "other web").await;
    rig.hub.pair_live(&sender, &rig.web).await;
    let mut sender_conn = FakeDevice::connect(&rig.hub, Some(&sender.token), DeviceKind::Web, "other web").await;
    sender_conn.send_env(&rig.web.id, Kind::Notify, notify_body(NotifyCategory::PermissionRequested, Some("s1"))).await;
    assert!(matches!(sender_conn.recv_control().await, Control::Undeliverable { .. }));
    rig.hub.settle().await;
    rig.assert_no_push().await;

    // 没配对的 daemon：信封直接被丢弃（not_paired），更不会推
    let stranger = rig.hub.seed_device(DeviceKind::Daemon, "stranger").await;
    let mut stranger_conn = FakeDevice::connect(&rig.hub, Some(&stranger.token), DeviceKind::Daemon, "stranger").await;
    stranger_conn
        .send_env(&rig.web.id, Kind::Notify, notify_body(NotifyCategory::PermissionRequested, Some("s1")))
        .await;
    assert!(matches!(stranger_conn.recv_control().await, Control::Dropped { .. }));
    rig.hub.settle().await;
    rig.assert_no_push().await;
}

#[tokio::test]
async fn a_pairing_across_accounts_never_triggers_a_push_or_an_email() {
    // 配对要求同一个账号，所以正常流程里不会有跨账号的配对；这是纵深防御：万一库里有了一条（手改库、以后的 bug），
    // daemon 也不能让别的账号的设备收推送、让别的账号收邮件。
    let mut rig = Rig::new().await;
    let (guest_id, guest_cookie) = rig.hub.invite_account(&rig.cookie).await;
    let foreign_web = rig.hub.seed_device_in(&guest_id, DeviceKind::Web, "guest browser").await;
    rig.hub.handle.store().set_account_email(&guest_id, Some("guest@example.com")).await.unwrap();
    rig.bind_email("me@example.com").await;
    // 外人的浏览器订阅了推送
    let guest_browser = Browser::new();
    let res = rig
        .hub
        .send(
            Method::POST,
            "/api/push/subscriptions",
            &guest_cookie,
            guest_browser.subscription(&foreign_web.id, &rig.push.endpoint("guest-sub")),
        )
        .await;
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
    // 库里硬造一个跨账号的配对
    rig.hub.pair_live(&rig.daemon, &foreign_web).await;

    rig.notify_offline(&foreign_web.id, NotifyCategory::PermissionRequested, Some("s1")).await;
    rig.assert_no_push().await;
    assert!(rig.mailer.sent().is_empty(), "no mail to either account: {:?}", rig.mailer.sent().len());
}

#[tokio::test]
async fn notify_bodies_the_hub_cannot_read_are_forwarded_normally_but_never_pushed() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    // 不是 msgpack / 类别不认识 / 缺类别 / 超大
    let unknown_category = rmp_serde::to_vec_named(&json!({ "category": "bogus", "session_id": null })).unwrap();
    let missing_category = rmp_serde::to_vec_named(&json!({ "session_id": "s" })).unwrap();
    for body in [b"definitely not msgpack".to_vec(), unknown_category, missing_category, vec![0u8; 70_000], Vec::new()]
    {
        rig.daemon_conn.send_env(&web, Kind::Notify, body).await;
        assert!(
            matches!(rig.daemon_conn.recv_control().await, Control::Undeliverable { .. }),
            "the envelope is handled as usual"
        );
    }
    rig.hub.settle().await;
    rig.assert_no_push().await;
    // 之后正常的 notify 照常工作
    rig.notify_offline(&web, NotifyCategory::Error, None).await;
    assert_eq!(rig.push.wait_for(1).await.len(), 1);
}

#[tokio::test]
async fn a_revoked_device_is_not_pushed_and_loses_its_subscription() {
    let mut rig = Rig::new().await;
    assert_eq!(rig.subscriptions().await.len(), 1);
    let res = rig.hub.send(Method::DELETE, &format!("/api/devices/{}", rig.web.id), &rig.cookie, Value::Null).await;
    assert_eq!(res.status(), 204);
    assert!(rig.subscriptions().await.is_empty(), "revoking the device removes its subscription");
    assert!(rig.hub.handle.store().get_push_subscription_of_device(&rig.web.id).await.unwrap().is_none());
    // 吊销时配对也没了，notify 会被当作 not_paired 丢弃；总之不推
    rig.daemon_conn.send_env(&rig.web.id, Kind::Notify, notify_body(NotifyCategory::Error, None)).await;
    let _ = rig.daemon_conn.drain_controls(Duration::from_millis(300)).await;
    rig.hub.settle().await;
    rig.assert_no_push().await;
    // 已吊销的设备不能再订阅
    let res = rig.subscribe(&rig.web.id, &Browser::new(), "again").await;
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn at_most_thirty_notifications_a_minute_are_considered_per_device() {
    let mut rig = Rig::with(|c| c.limits.notify_per_min_per_device = 3).await;
    let web = rig.web.id.clone();
    for i in 0..6 {
        rig.notify_offline(&web, NotifyCategory::Error, Some(&format!("s{i}"))).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.push.requests().len(), 3, "the flood is cut off");
}

// ---------------------------------------------------------------------------
// 推送服务的回应
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_404_or_410_from_the_push_service_deletes_the_subscription() {
    for status in [404u16, 410] {
        let mut rig = Rig::new().await;
        rig.push.set_status(status);
        assert_eq!(rig.subscriptions().await.len(), 1);
        let web = rig.web.id.clone();
        rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
        assert_eq!(rig.push.requests().len(), 1, "{status}");
        assert!(rig.subscriptions().await.is_empty(), "{status}: the dead subscription is removed");
        // 之后不再尝试
        rig.notify_offline(&web, NotifyCategory::Error, Some("s2")).await;
        assert_eq!(rig.push.requests().len(), 1, "{status}: no second attempt");
    }
}

#[tokio::test]
async fn other_failures_keep_the_subscription() {
    for status in [400u16, 401, 403, 413, 429, 500, 503] {
        let mut rig = Rig::new().await;
        rig.push.set_status(status);
        let web = rig.web.id.clone();
        rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
        assert_eq!(rig.push.requests().len(), 1);
        assert_eq!(rig.subscriptions().await.len(), 1, "{status}: transient problems do not delete the subscription");
    }
}

#[tokio::test]
async fn a_replaced_subscription_is_not_deleted_by_the_old_ones_410() {
    let mut rig = Rig::new().await;
    rig.push.set_status(410);
    rig.push.set_delay(300);
    let web = rig.web.id.clone();
    rig.daemon_conn.send_env(&web, Kind::Notify, notify_body(NotifyCategory::Error, None)).await;
    assert!(matches!(rig.daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    // 推送请求还在路上（假服务延迟 300 ms 才回 410），浏览器换了一个新订阅
    rig.push.wait_for(1).await;
    let new_browser = Browser::new();
    assert_eq!(rig.subscribe(&web, &new_browser, "fresh-secret-path").await.status(), 200);
    rig.hub.settle().await;
    let subs = rig.subscriptions().await;
    assert_eq!(subs.len(), 1, "the new subscription survives the late 410 for the old one");
}

#[tokio::test]
async fn a_slow_push_service_times_out_without_losing_the_subscription() {
    let mut rig = Rig::with(|c| c.timing.push_timeout = Duration::from_millis(300)).await;
    rig.push.set_delay(2_000);
    let web = rig.web.id.clone();
    let started = std::time::Instant::now();
    rig.notify_offline(&web, NotifyCategory::Error, None).await;
    assert!(started.elapsed() < Duration::from_millis(1_500), "gave up after the timeout: {:?}", started.elapsed());
    assert_eq!(rig.subscriptions().await.len(), 1);
    // 转发不受影响：timeout 期间信封转发是立刻回的（notify_offline 等的是后台任务，这里只证明后台任务会结束）
}

#[tokio::test]
async fn forwarding_does_not_wait_for_a_slow_push_service() {
    let mut rig = Rig::new().await;
    rig.push.set_delay(1_500);
    let web = rig.web.id.clone();
    let started = std::time::Instant::now();
    rig.daemon_conn.send_env(&web, Kind::Notify, notify_body(NotifyCategory::Error, None)).await;
    assert!(matches!(rig.daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "undeliverable came back immediately: {:?}",
        started.elapsed()
    );
    rig.hub.settle().await;
}

#[tokio::test]
async fn at_most_sixteen_pushes_are_in_flight_at_once() {
    let mut rig = Rig::new().await;
    rig.push.set_delay(300);
    // 24 台各自订阅了推送的 web 设备
    let mut targets = Vec::new();
    for i in 0..24 {
        let d = rig.hub.seed_device(DeviceKind::Web, &format!("web {i}")).await;
        rig.hub.pair_live(&rig.daemon, &d).await;
        let b = Browser::new();
        assert_eq!(rig.subscribe(&d.id, &b, &format!("many-{i}")).await.status(), 200);
        targets.push(d);
    }
    for d in &targets {
        rig.daemon_conn.send_env(&d.id, Kind::Notify, notify_body(NotifyCategory::Error, None)).await;
    }
    for d in &targets {
        rig.expect_undeliverable(&d.id).await;
    }
    rig.hub.settle().await;
    let n = rig.push.requests().len();
    assert_eq!(n, 24, "every device got its push");
    assert!(rig.push.max_in_flight() <= 16, "max in flight: {}", rig.push.max_in_flight());
    assert!(rig.push.max_in_flight() >= 8, "and they really ran concurrently: {}", rig.push.max_in_flight());
}

#[tokio::test]
async fn nothing_is_pushed_to_an_endpoint_that_is_no_longer_allowed() {
    // 订阅是在放行 127.0.0.1 的配置下存的；换一个不放行它的 hub（同一个库）后不会发出去
    let mut rig = Rig::new().await;
    let sub = rig.hub.handle.store().get_push_subscription_of_device(&rig.web.id).await.unwrap().unwrap();
    assert!(sub.endpoint.starts_with("http://127.0.0.1:"));
    let strict = anycon_hub::push::PushService::new(
        anycon_hub::push::VapidKeys::generate(),
        "mailto:a@example.com".into(),
        None,
        Duration::from_secs(1),
    )
    .unwrap();
    let outcome = strict
        .send(
            &sub,
            br#"{"category":"error"}"#,
            anycon_hub::push::PushOptions { ttl: 60, urgency: anycon_hub::push::Urgency::Normal },
        )
        .await;
    assert_eq!(outcome, anycon_hub::push::PushOutcome::Failed { status: None });
    rig.push.set_status(201);
    assert!(rig.push.requests().is_empty(), "the strict service never contacted the fake push server");
    let _ = &mut rig;
}

// ---------------------------------------------------------------------------
// 订阅 API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subscriptions_are_listed_without_their_secrets_and_deleted_by_id() {
    let rig = Rig::new().await;
    let subs = rig.subscriptions().await;
    assert_eq!(subs.len(), 1);
    let s = &subs[0];
    assert_eq!(s["device_id"], rig.web.id);
    assert_eq!(s["device_name"], "Chrome · macOS");
    assert_eq!(s["endpoint_host"], "127.0.0.1");
    assert!(s["created_at_ms"].as_u64().unwrap() > 0);
    let dump = serde_json::to_string(&subs).unwrap();
    assert!(
        !dump.contains("sub-secret-path-1")
            && !dump.contains(&rig.browser.p256dh())
            && !dump.contains(&rig.browser.auth_b64()),
        "{dump}"
    );
    assert!(s.get("endpoint").is_none() && s.get("keys").is_none() && s.get("p256dh").is_none());

    // 删除：别的账号删不了
    let id = s["id"].as_str().unwrap();
    let (_gid, guest) = rig.hub.invite_account(&rig.cookie).await;
    assert_eq!(
        rig.hub.send(Method::DELETE, &format!("/api/push/subscriptions/{id}"), &guest, Value::Null).await.status(),
        404
    );
    assert_eq!(rig.subscriptions().await.len(), 1);
    assert_eq!(
        rig.hub.send(Method::DELETE, &format!("/api/push/subscriptions/{id}"), &rig.cookie, Value::Null).await.status(),
        204
    );
    assert!(rig.subscriptions().await.is_empty());
    assert_eq!(
        rig.hub.send(Method::DELETE, &format!("/api/push/subscriptions/{id}"), &rig.cookie, Value::Null).await.status(),
        404
    );
    // 也看不到别的账号的订阅
    assert!(rig
        .hub
        .get("/api/push/subscriptions", &guest)
        .await
        .json::<Value>()
        .await
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn one_subscription_per_device_the_new_one_replaces_the_old() {
    let mut rig = Rig::new().await;
    let first_id = rig.subscriptions().await[0]["id"].as_str().unwrap().to_string();
    let new_browser = Browser::new();
    let res = rig.subscribe(&rig.web.id, &new_browser, "second-path").await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let subs = rig.subscriptions().await;
    assert_eq!(subs.len(), 1);
    assert_ne!(subs[0]["id"], json!(first_id), "a new id");
    assert_eq!(subs[0]["id"], body["id"]);
    assert_eq!(body["device_id"], rig.web.id);
    // 推送只发到新的 endpoint，用新的密钥加密
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, None).await;
    let reqs = rig.push.wait_for(1).await;
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/second-path");
    assert_eq!(new_browser.decrypt(&reqs[0])["category"], "error");
    assert!(
        anycon_hub::push::webpush::decrypt(&reqs[0].body, &rig.browser.secret, &rig.browser.auth).is_err(),
        "the old keys cannot read it"
    );
}

#[tokio::test]
async fn only_known_push_services_over_https_are_accepted() {
    let rig = Rig::new().await;
    let device = rig.hub.seed_device(DeviceKind::Web, "another browser").await;
    let browser = Browser::new();
    let body = |endpoint: &str| browser.subscription(&device.id, endpoint);
    for bad in [
        "https://evil.example/push/abc",
        "http://fcm.googleapis.com/fcm/send/abc",
        "https://fcm.googleapis.com.evil.example/fcm/send/abc",
        "https://fcm.googleapis.com@evil.example/x",
        "https://127.0.0.2/x",
        "https://localhost/x",
        "https://169.254.169.254/latest/meta-data/",
        "https://10.0.0.1/x",
        "ftp://fcm.googleapis.com/x",
    ] {
        let res = rig.hub.send(Method::POST, "/api/push/subscriptions", &rig.cookie, body(bad)).await;
        assert_eq!(res.status(), 400, "{bad}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "push_endpoint_not_allowed", "{bad}");
    }
    let res = rig.hub.send(Method::POST, "/api/push/subscriptions", &rig.cookie, body("not a url")).await;
    assert_eq!(res.status(), 400);
    // 真的推送服务的 endpoint 被接受（hub 此刻不会去连它）
    for ok in [
        "https://fcm.googleapis.com/fcm/send/dXNlcg:APA91b",
        "https://updates.push.services.mozilla.com/wpush/v2/gAAAA",
        "https://web.push.apple.com/QWERTY",
        "https://wns2-par02p.notify.windows.com/w/?token=abc",
    ] {
        let res = rig.hub.send(Method::POST, "/api/push/subscriptions", &rig.cookie, body(ok)).await;
        assert_eq!(res.status(), 200, "{ok}: {}", res.text().await.unwrap());
        let sub: Value = rig.hub.get("/api/push/subscriptions", &rig.cookie).await.json().await.unwrap();
        let hosts: Vec<&str> = sub.as_array().unwrap().iter().map(|s| s["endpoint_host"].as_str().unwrap()).collect();
        assert!(hosts.contains(&anycon_hub::push::endpoint_host(ok).as_str()), "{hosts:?}");
    }
}

#[tokio::test]
async fn the_keys_must_be_a_real_p256_key_and_a_16_byte_auth_secret() {
    let rig = Rig::new().await;
    let device = rig.hub.seed_device(DeviceKind::Web, "another browser").await;
    let good = Browser::new();
    let endpoint = rig.push.endpoint("k");
    let post = |p256dh: String, auth: String| {
        let body = json!({ "device_id": device.id, "endpoint": endpoint, "keys": { "p256dh": p256dh, "auth": auth } });
        let hub = &rig.hub;
        let cookie = rig.cookie.clone();
        async move { hub.send(Method::POST, "/api/push/subscriptions", &cookie, body).await.status() }
    };
    assert_eq!(post("AAAA".into(), good.auth_b64()).await, 400, "too short");
    assert_eq!(post(URL_SAFE_NO_PAD.encode([4u8; 65]), good.auth_b64()).await, 400, "not on the curve");
    assert_eq!(post("!!!".into(), good.auth_b64()).await, 400, "not base64");
    assert_eq!(post(good.p256dh(), "AAAA".into()).await, 400, "auth too short");
    assert_eq!(post(good.p256dh(), URL_SAFE_NO_PAD.encode([0u8; 17])).await, 400, "auth too long");
    assert_eq!(post(good.p256dh(), good.auth_b64()).await, 200);
    // 标准字母表、带填充的写法也认
    let std_b64 = base64::engine::general_purpose::STANDARD.encode(b64(&good.p256dh()));
    let device2 = rig.hub.seed_device(DeviceKind::Web, "third browser").await;
    let res = rig
        .hub
        .send(Method::POST, "/api/push/subscriptions", &rig.cookie, json!({ "device_id": device2.id, "endpoint": rig.push.endpoint("k2"), "keys": { "p256dh": std_b64, "auth": good.auth_b64() } }))
        .await;
    assert_eq!(res.status(), 200);
    // 库里存的是归一化的 base64url
    let stored = rig.hub.handle.store().get_push_subscription_of_device(&device2.id).await.unwrap().unwrap();
    assert_eq!(stored.p256dh, good.p256dh());
}

#[tokio::test]
async fn the_device_must_be_a_live_web_device_of_this_account() {
    let rig = Rig::new().await;
    let browser = Browser::new();
    let (guest_id, guest) = rig.hub.invite_account(&rig.cookie).await;
    let guest_web = rig.hub.seed_device_in(&guest_id, DeviceKind::Web, "guest browser").await;

    let attempt = |cookie: &str, device_id: &str| {
        let body = browser.subscription(device_id, &rig.push.endpoint("x"));
        let cookie = cookie.to_string();
        let hub = &rig.hub;
        async move { hub.send(Method::POST, "/api/push/subscriptions", &cookie, body).await }
    };
    // 别的账号的设备
    let res = attempt(&rig.cookie, &guest_web.id).await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "device_not_found");
    assert_eq!(attempt(&guest, &rig.web.id).await.status(), 404, "and the other way round");
    // 不存在的设备、daemon（不是 web）
    assert_eq!(attempt(&rig.cookie, &"0".repeat(32)).await.status(), 404);
    assert_eq!(attempt(&rig.cookie, &rig.daemon.id).await.status(), 404);
    // 自己账号的 web 设备可以
    assert_eq!(attempt(&guest, &guest_web.id).await.status(), 200);
    // 没登录
    let res = rig
        .hub
        .http
        .post(rig.hub.url("/api/push/subscriptions"))
        .header("x-anycon", "1")
        .json(&browser.subscription(&rig.web.id, &rig.push.endpoint("x")))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    let res = rig.hub.http.get(rig.hub.url("/api/push/subscriptions")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    // 没有 CSRF 头
    let res = rig
        .hub
        .http
        .post(rig.hub.url("/api/push/subscriptions"))
        .header("cookie", &rig.cookie)
        .json(&browser.subscription(&rig.web.id, &rig.push.endpoint("x")))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn subscribing_and_testing_are_limited_to_thirty_requests_a_minute_per_account() {
    let rig = Rig::new().await;
    let browser = Browser::new();
    // 已经订阅过 1 次；再来 29 次用满配额
    for i in 0..29 {
        let res = rig
            .hub
            .send(
                Method::POST,
                "/api/push/subscriptions",
                &rig.cookie,
                browser.subscription(&rig.web.id, &rig.push.endpoint(&format!("p{i}"))),
            )
            .await;
        assert_eq!(res.status(), 200, "{i}");
    }
    let res = rig
        .hub
        .send(
            Method::POST,
            "/api/push/subscriptions",
            &rig.cookie,
            browser.subscription(&rig.web.id, &rig.push.endpoint("over")),
        )
        .await;
    assert_eq!(res.status(), 429);
    assert!(res.headers().contains_key("retry-after"));
    // test 接口共用这个配额
    let res = rig.hub.send(Method::POST, "/api/push/test", &rig.cookie, json!({ "device_id": rig.web.id })).await;
    assert_eq!(res.status(), 429);
    // 别的账号不受影响
    let (gid, guest) = rig.hub.invite_account(&rig.cookie).await;
    let gweb = rig.hub.seed_device_in(&gid, DeviceKind::Web, "g").await;
    let res = rig
        .hub
        .send(Method::POST, "/api/push/subscriptions", &guest, browser.subscription(&gweb.id, &rig.push.endpoint("g")))
        .await;
    assert_eq!(res.status(), 200);
}

// ---------------------------------------------------------------------------
// 测试推送
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_test_push_reaches_the_browser_and_reports_the_result() {
    let rig = Rig::new().await;
    let res = rig.hub.send(Method::POST, "/api/push/test", &rig.cookie, json!({ "device_id": rig.web.id })).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "result": "ok" }));
    let reqs = rig.push.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(rig.browser.decrypt(&reqs[0]), json!({ "category": "test" }));
    assert_eq!(reqs[0].header("ttl"), "60");
    assert!(reqs[0].header("authorization").starts_with("vapid t="));
}

#[tokio::test]
async fn the_test_push_reports_gone_and_error() {
    let rig = Rig::new().await;
    rig.push.set_status(500);
    let res = rig.hub.send(Method::POST, "/api/push/test", &rig.cookie, json!({ "device_id": rig.web.id })).await;
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "result": "error", "status": 500 }));
    assert_eq!(rig.subscriptions().await.len(), 1, "an error keeps the subscription");

    rig.push.set_status(410);
    let res = rig.hub.send(Method::POST, "/api/push/test", &rig.cookie, json!({ "device_id": rig.web.id })).await;
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "result": "gone" }));
    assert!(rig.subscriptions().await.is_empty(), "gone removes it");
    // 订阅没了：404 subscription_not_found
    let res = rig.hub.send(Method::POST, "/api/push/test", &rig.cookie, json!({ "device_id": rig.web.id })).await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "subscription_not_found");
}

#[tokio::test]
async fn the_test_push_checks_the_device_like_subscribing_does() {
    let rig = Rig::new().await;
    let (gid, guest) = rig.hub.invite_account(&rig.cookie).await;
    let gweb = rig.hub.seed_device_in(&gid, DeviceKind::Web, "g").await;
    for (cookie, device) in [
        (&guest, rig.web.id.as_str()),
        (&rig.cookie, gweb.id.as_str()),
        (&rig.cookie, rig.daemon.id.as_str()),
        (&rig.cookie, "nope"),
    ] {
        let res = rig.hub.send(Method::POST, "/api/push/test", cookie, json!({ "device_id": device })).await;
        assert_eq!(res.status(), 404, "{device}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "device_not_found");
    }
    assert!(rig.push.requests().is_empty());
}

// ---------------------------------------------------------------------------
// 通知偏好
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preferences_default_to_the_spec_and_list_all_six_categories() {
    let rig = Rig::new().await;
    let prefs = rig.prefs().await;
    assert_eq!(
        prefs["categories"],
        json!([
            { "category": "permission_requested", "push": true, "email": true },
            { "category": "turn_finished", "push": true, "email": false },
            { "category": "error", "push": true, "email": true },
            { "category": "session_idle", "push": false, "email": false },
            { "category": "enroll_requested", "push": true, "email": true },
            { "category": "security", "push": true, "email": true },
        ])
    );
    assert_eq!(prefs["push_available"], true);
    assert_eq!(prefs["email_available"], false, "SMTP is configured but the account has no email yet");
    bind_email(&rig.hub, &rig.mailer, &rig.cookie, "me@example.com").await;
    assert_eq!(rig.prefs().await["email_available"], true);
    // 没登录
    let res = rig.hub.http.get(rig.hub.url("/api/me/notifications")).send().await.unwrap();
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn email_is_not_available_without_smtp() {
    let hub = TestHub::start().await;
    let cookie = hub.login_admin().await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("me@example.com")).await.unwrap();
    let prefs: Value = hub.get("/api/me/notifications", &cookie).await.json().await.unwrap();
    assert_eq!(prefs["email_available"], false, "an address alone is not enough without SMTP");
    assert_eq!(prefs["push_available"], true);
}

#[tokio::test]
async fn put_updates_only_the_listed_categories_and_returns_the_full_result() {
    let rig = Rig::new().await;
    let res = rig
        .put_prefs(json!([
            { "category": "turn_finished", "push": false, "email": false },
            { "category": "session_idle", "push": true, "email": false },
            { "category": "security", "push": false, "email": true },
        ]))
        .await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let cats = body["categories"].as_array().unwrap();
    assert_eq!(cats.len(), 6);
    let pref = |c: &str| cats.iter().find(|p| p["category"] == c).unwrap().clone();
    assert_eq!(pref("turn_finished"), json!({ "category": "turn_finished", "push": false, "email": false }));
    assert_eq!(pref("session_idle"), json!({ "category": "session_idle", "push": true, "email": false }));
    assert_eq!(pref("security"), json!({ "category": "security", "push": false, "email": true }));
    assert_eq!(
        pref("error"),
        json!({ "category": "error", "push": true, "email": true }),
        "untouched categories keep their value"
    );
    // 持久化：GET 得到同样的结果
    assert_eq!(rig.prefs().await["categories"], body["categories"]);
    // 重复出现的类别：最后一个生效
    let res = rig.put_prefs(json!([{ "category": "error", "push": false, "email": false }, { "category": "error", "push": true, "email": false }])).await;
    assert_eq!(
        pref_of(&res.json::<Value>().await.unwrap(), "error"),
        json!({ "category": "error", "push": true, "email": false })
    );
    // 空数组：什么都不改
    let before = rig.prefs().await;
    assert_eq!(rig.put_prefs(json!([])).await.json::<Value>().await.unwrap()["categories"], before["categories"]);
}

fn pref_of(body: &Value, category: &str) -> Value {
    body["categories"].as_array().unwrap().iter().find(|p| p["category"] == category).unwrap().clone()
}

#[tokio::test]
async fn unknown_categories_and_malformed_bodies_are_rejected_without_changing_anything() {
    let rig = Rig::new().await;
    let before = rig.prefs().await;
    for bad in [
        json!([{ "category": "bogus", "push": true, "email": true }]),
        json!([{ "category": "error", "push": false, "email": false }, { "category": "test", "push": true, "email": true }]),
        json!([{ "category": "", "push": true, "email": true }]),
        json!([{ "category": "ERROR", "push": true, "email": true }]),
    ] {
        let res = rig.put_prefs(bad.clone()).await;
        assert_eq!(res.status(), 400, "{bad}");
    }
    for bad in
        [json!({ "categories": "error" }), json!({ "categories": [{ "category": "error" }] }), json!({}), json!([])]
    {
        let res = rig.hub.send(Method::PUT, "/api/me/notifications", &rig.cookie, bad.clone()).await;
        assert_eq!(res.status(), 400, "{bad}");
    }
    assert_eq!(
        rig.prefs().await,
        before,
        "a rejected PUT changes nothing (not even the valid entries before the bad one)"
    );
    // 没有 CSRF 头
    let res = rig
        .hub
        .http
        .put(rig.hub.url("/api/me/notifications"))
        .header("cookie", &rig.cookie)
        .json(&json!({ "categories": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn email_is_stored_as_off_for_categories_that_never_send_mail() {
    let rig = Rig::new().await;
    let res = rig
        .put_prefs(json!([
            { "category": "turn_finished", "push": true, "email": true },
            { "category": "session_idle", "push": true, "email": true },
            { "category": "error", "push": true, "email": true },
        ]))
        .await;
    let body: Value = res.json().await.unwrap();
    assert_eq!(pref_of(&body, "turn_finished")["email"], false);
    assert_eq!(pref_of(&body, "session_idle")["email"], false);
    assert_eq!(pref_of(&body, "error")["email"], true);
}

#[tokio::test]
async fn preferences_belong_to_the_account() {
    let rig = Rig::new().await;
    rig.put_prefs(json!([{ "category": "error", "push": false, "email": false }])).await;
    let (_gid, guest) = rig.hub.invite_account(&rig.cookie).await;
    let prefs: Value = rig.hub.get("/api/me/notifications", &guest).await.json().await.unwrap();
    assert_eq!(
        pref_of(&prefs, "error"),
        json!({ "category": "error", "push": true, "email": true }),
        "the guest still has the defaults"
    );
}

#[tokio::test]
async fn push_off_for_a_category_means_no_push_for_it() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    rig.put_prefs(json!([{ "category": "permission_requested", "push": false, "email": true }])).await;
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    rig.assert_no_push().await;
    // 别的类别不受影响
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert_eq!(rig.push.wait_for(1).await.len(), 1);
    // session_idle 默认关；打开之后才推
    rig.notify_offline(&web, NotifyCategory::SessionIdle, Some("s1")).await;
    assert_eq!(rig.push.requests().len(), 1, "session_idle is off by default");
    rig.put_prefs(json!([{ "category": "session_idle", "push": true, "email": false }])).await;
    rig.notify_offline(&web, NotifyCategory::SessionIdle, Some("s1")).await;
    assert_eq!(rig.push.wait_for(2).await.len(), 2);
    // 再把 permission_requested 打开
    rig.put_prefs(json!([{ "category": "permission_requested", "push": true, "email": true }])).await;
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s2")).await;
    assert_eq!(rig.push.wait_for(3).await.len(), 3);
}

// ---------------------------------------------------------------------------
// 邮件通知
// ---------------------------------------------------------------------------

impl Rig {
    async fn bind_email(&self, email: &str) {
        self.hub.handle.store().set_account_email(&self.hub.admin_account_id(), Some(email)).await.unwrap();
    }
}

#[tokio::test]
async fn an_account_with_no_client_online_gets_a_bilingual_email() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("sess-1")).await;

    let sent = rig.mailer.sent();
    assert_eq!(sent.len(), 1);
    let mail = &sent[0];
    assert_eq!(mail.to, "me@example.com");
    assert!(
        mail.subject.starts_with("[Anycon]") && mail.subject.contains("approval") && mail.subject.contains("审批"),
        "{}",
        mail.subject
    );
    assert!(mail.body.contains("build-box"), "the node name comes from the device record: {}", mail.body);
    assert!(mail.body.contains("waiting for your approval") && mail.body.contains("在等你审批"), "{}", mail.body);
    assert!(mail.body.contains(&format!("https://hub.test/n/{}/s/sess-1", rig.daemon.id)), "{}", mail.body);
    // 同时推送照常
    assert_eq!(rig.push.wait_for(1).await.len(), 1);
}

#[tokio::test]
async fn each_email_category_has_its_own_template_and_link() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s9")).await;
    rig.notify_offline(&web, NotifyCategory::EnrollRequested, None).await;
    rig.notify_offline(&web, NotifyCategory::Security, None).await;
    let sent = rig.mailer.sent();
    assert_eq!(sent.len(), 3);
    assert!(sent[0].body.contains("hit an error") && sent[0].body.contains("出错了"), "{}", sent[0].body);
    assert!(sent[0].body.contains(&format!("/n/{}/s/s9", rig.daemon.id)));
    assert!(sent[1].body.contains("asking to join") && sent[1].body.contains("请求接入"), "{}", sent[1].body);
    assert!(
        sent[1].body.contains(&format!("https://hub.test/n/{}/settings?tab=enroll", rig.daemon.id)),
        "{}",
        sent[1].body
    );
    assert!(sent[2].body.contains("security event") && sent[2].body.contains("安全事件"), "{}", sent[2].body);
    assert!(sent[2].body.contains("settings?tab=enroll"));
}

#[tokio::test]
async fn mail_carries_no_session_content_only_the_node_name_category_and_link() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    // detail 里放个「会话内容」的标记：hub 不读它，邮件和推送里都不能出现
    let body = rmp_serde::to_vec_named(&anycon_protocol::NotifyBody {
        category: NotifyCategory::PermissionRequested,
        session_id: Some("s1".into()),
        detail: Some("TOP-SECRET-DETAIL-MARKER".into()),
    })
    .unwrap();
    rig.daemon_conn.send_env(&web, Kind::Notify, body).await;
    assert!(matches!(rig.daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    rig.hub.settle().await;
    let mail = &rig.mailer.sent()[0];
    assert!(!mail.body.contains("TOP-SECRET") && !mail.subject.contains("TOP-SECRET"));
    let push = rig.push.wait_for(1).await.remove(0);
    assert!(!rig.browser.decrypt(&push).to_string().contains("TOP-SECRET"));
    assert!(!push.body.windows(10).any(|w| w == b"TOP-SECRET"));
}

#[tokio::test]
async fn no_email_while_a_web_device_of_the_account_is_online() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    // 一台在线的 web 设备（不是通知的收件人）：有人正在看
    let online = rig.hub.seed_device(DeviceKind::Web, "in use").await;
    rig.hub.pair_live(&rig.daemon, &online).await;
    let _online_conn = FakeDevice::connect(&rig.hub, Some(&online.token), DeviceKind::Web, "in use").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty(), "the user is already looking at Anycon in a browser");
    // 推送不受影响：收件设备离线、有订阅
    assert_eq!(rig.push.wait_for(1).await.len(), 1);
}

#[tokio::test]
async fn an_always_on_cli_or_sdk_device_does_not_stop_the_email() {
    // 常驻的 SDK / CLI 设备（脚本、机器人）不代表人在看，不能让邮件通知永远不发。
    for kind in [DeviceKind::Cli, DeviceKind::Sdk] {
        let mut rig = Rig::new().await;
        rig.bind_email("me@example.com").await;
        let online = rig.hub.seed_device(kind, "bot").await;
        rig.hub.pair_live(&rig.daemon, &online).await;
        let _online_conn = FakeDevice::connect(&rig.hub, Some(&online.token), kind, "bot").await;
        let web = rig.web.id.clone();
        rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
        assert_eq!(rig.mailer.sent().len(), 1, "{kind:?}: an online {kind:?} device is not a person looking");
        assert_eq!(rig.push.wait_for(1).await.len(), 1, "{kind:?}: and the push is unaffected");
    }
}

#[tokio::test]
async fn once_the_last_web_device_goes_offline_email_resumes() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let online = rig.hub.seed_device(DeviceKind::Web, "browser").await;
    rig.hub.pair_live(&rig.daemon, &online).await;
    let online_conn = FakeDevice::connect(&rig.hub, Some(&online.token), DeviceKind::Web, "browser").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty());
    drop(online_conn);
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.notify_offline(&web, NotifyCategory::Error, Some("s2")).await;
    assert_eq!(rig.mailer.sent().len(), 1, "the earlier suppressed attempt did not use up the dedupe window");
}

#[tokio::test]
async fn a_connected_daemon_does_not_count_as_the_user_being_there() {
    // Rig 里 daemon 一直在线；上面的邮件测试已经证明它不挡邮件。这里再加一台 daemon 确认。
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let second = rig.hub.seed_device(DeviceKind::Daemon, "second daemon").await;
    let _second_conn = FakeDevice::connect(&rig.hub, Some(&second.token), DeviceKind::Daemon, "second daemon").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert_eq!(rig.mailer.sent().len(), 1);
}

#[tokio::test]
async fn at_most_one_email_per_account_and_category_in_ten_minutes() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    // daemon 给每台设备各发一封 notify：这里模拟成给两台离线设备各发一封
    let second_web = rig.hub.seed_device(DeviceKind::Web, "Safari").await;
    rig.hub.pair_live(&rig.daemon, &second_web).await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    rig.notify_offline(&second_web.id, NotifyCategory::PermissionRequested, Some("s1")).await;
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s2")).await;
    assert_eq!(rig.mailer.sent().len(), 1, "merged into one mail");
    // 别的类别各算各的
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert_eq!(rig.mailer.sent().len(), 2);
    rig.notify_offline(&web, NotifyCategory::Error, Some("s2")).await;
    assert_eq!(rig.mailer.sent().len(), 2);
}

#[tokio::test]
async fn the_dedupe_window_expires() {
    let mut rig = Rig::with(|c| c.timing.notify_email_dedupe = Duration::from_secs(1)).await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    rig.notify_offline(&web, NotifyCategory::Error, Some("s2")).await;
    assert_eq!(rig.mailer.sent().len(), 1);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    rig.notify_offline(&web, NotifyCategory::Error, Some("s3")).await;
    assert_eq!(rig.mailer.sent().len(), 2);
}

#[tokio::test]
async fn the_email_preference_switches_mail_off_per_category() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    rig.put_prefs(json!([{ "category": "permission_requested", "push": true, "email": false }])).await;
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty(), "email is off for this category");
    assert_eq!(rig.push.wait_for(1).await.len(), 1, "but push is still on");
    // 关掉这个账号的 push、保留 email：邮件仍然发
    rig.put_prefs(json!([{ "category": "error", "push": false, "email": true }])).await;
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert_eq!(rig.mailer.sent().len(), 1);
    assert_eq!(rig.push.requests().len(), 1, "no push for error any more");
}

#[tokio::test]
async fn no_email_without_a_bound_address_and_no_email_for_chatty_categories() {
    let mut rig = Rig::new().await;
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty(), "no address bound");

    rig.bind_email("me@example.com").await;
    // 就算把偏好直接写成 email = true，turn_finished / session_idle 也不发邮件
    rig.hub
        .handle
        .store()
        .set_notification_prefs(
            &rig.hub.admin_account_id(),
            &[
                anycon_hub::store::NotificationPref { category: NotifyCategory::TurnFinished, push: true, email: true },
                anycon_hub::store::NotificationPref { category: NotifyCategory::SessionIdle, push: true, email: true },
            ],
        )
        .await
        .unwrap();
    rig.notify_offline(&web, NotifyCategory::TurnFinished, Some("s1")).await;
    rig.notify_offline(&web, NotifyCategory::SessionIdle, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty());
}

#[tokio::test]
async fn no_email_when_smtp_is_not_configured() {
    let push = FakePush::start().await;
    let hub = TestHub::start_with(|c| c.push_allow_hosts = Some(vec!["127.0.0.1".into()])).await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("me@example.com")).await.unwrap();
    let cookie = hub.login_admin().await;
    let daemon = hub.seed_device(DeviceKind::Daemon, "d").await;
    let web = hub.seed_device(DeviceKind::Web, "w").await;
    hub.seed_paired(&daemon, &web).await;
    let mut daemon_conn = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "d").await;
    let browser = Browser::new();
    let res = hub
        .send(Method::POST, "/api/push/subscriptions", &cookie, browser.subscription(&web.id, &push.endpoint("p")))
        .await;
    assert_eq!(res.status(), 200);
    daemon_conn.send_env(&web.id, Kind::Notify, notify_body(NotifyCategory::PermissionRequested, Some("s1"))).await;
    assert!(matches!(daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    hub.settle().await;
    // 推送不依赖 SMTP
    assert_eq!(push.wait_for(1).await.len(), 1);
}

#[tokio::test]
async fn a_failing_mail_server_does_not_stop_push_or_later_mail() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    rig.mailer.fail_next(1);
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::Error, Some("s1")).await;
    assert!(rig.mailer.sent().is_empty(), "the first mail failed");
    assert_eq!(rig.push.wait_for(1).await.len(), 1, "push is independent");
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    assert_eq!(rig.mailer.sent().len(), 1, "the next category still goes out");
}

#[tokio::test]
async fn the_notification_mail_goes_to_the_account_that_owns_the_devices() {
    let mut rig = Rig::new().await;
    rig.bind_email("admin@example.com").await;
    // 另一个账号：有自己的 daemon / web 设备和邮箱，不应该收到管理员那边的通知
    let (gid, _guest) = rig.hub.invite_account(&rig.cookie).await;
    rig.hub.handle.store().set_account_email(&gid, Some("guest@example.com")).await.unwrap();
    let web = rig.web.id.clone();
    rig.notify_offline(&web, NotifyCategory::PermissionRequested, Some("s1")).await;
    let sent = rig.mailer.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "admin@example.com");
}

#[tokio::test]
async fn forwarding_still_reports_undeliverable_for_an_offline_target_even_with_everything_enabled() {
    let mut rig = Rig::new().await;
    rig.bind_email("me@example.com").await;
    let web = rig.web.id.clone();
    let seq = rig.daemon_conn.send_env(&web, Kind::Notify, notify_body(NotifyCategory::Error, None)).await;
    match rig.daemon_conn.recv_control().await {
        Control::Undeliverable { to, seq: s } => assert_eq!((to, s), (web, seq)),
        other => panic!("{other:?}"),
    }
    rig.hub.settle().await;
}

// ---------------------------------------------------------------------------
// VAPID 密钥文件
// ---------------------------------------------------------------------------

fn bare_config(dir: &std::path::Path) -> anycon_hub::Config {
    let mut config = anycon_hub::Config::new("https://hub.test", dir).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config
}

#[tokio::test]
async fn the_vapid_key_pair_survives_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let key_of = |h: &anycon_hub::Handle| {
        let url = format!("{}/api/config", h.base_url());
        async move {
            reqwest::get(url).await.unwrap().json::<Value>().await.unwrap()["vapid_public_key"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let first = anycon_hub::run(bare_config(dir.path())).await.unwrap();
    let key = key_of(&first).await;
    assert_eq!(URL_SAFE_NO_PAD.decode(&key).unwrap().len(), 65);
    first.shutdown().await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("vapid.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the private key is readable by the hub user only");
    }
    let second = anycon_hub::run(bare_config(dir.path())).await.unwrap();
    assert_eq!(key_of(&second).await, key, "the same key after a restart, so existing subscriptions keep working");
    second.shutdown().await;
}

#[tokio::test]
async fn a_corrupt_vapid_file_stops_the_hub_instead_of_silently_replacing_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vapid.json");
    std::fs::write(&path, "{ this is not json").unwrap();
    let err = anycon_hub::run(bare_config(dir.path())).await.err().expect("must not start");
    let msg = err.to_string();
    assert!(msg.contains("vapid.json") && msg.contains("Refusing to generate"), "{msg}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ this is not json", "the file is left alone");
    // 启动失败没有消耗「首次启动」：没有生成 admin token，也没有建库
    assert!(!dir.path().join("admin-token").exists());
    assert!(!dir.path().join("hub.db").exists());
}
