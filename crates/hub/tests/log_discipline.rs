//! 日志纪律（docs/spec/hub.md § 10，code review 硬项）：把 hub 自己的 trace 级日志收集起来，
//! 跑一遍登录 / 邀请 / 配对 / 转发，断言里面没有信封 body、device token、登录 token、cookie、邀请码。
//!
//! 配对相关：短码本来就到不了 hub；`match_id` 是短码的函数（拿到就能离线穷举），同样按秘密对待。
//! 所以日志、数据目录里的所有文件（SQLite 主库 / WAL / admin-token……）、`Broker` 的 `Debug` 输出里，
//! 都不能出现短码和 `match_id`（含大小写变体）——包括撮合成功的、登记着还没人加入的、猜错的、格式非法的。
//!
//! ADR-37（第二个测试，同一份日志捕获）：邮箱验证码、邮箱原文（用哈希前 8 位代替）、推送 endpoint 的路径、p256dh / auth、
//! VAPID 私钥、VAPID JWT、SMTP 的用户名密码，都不能出现在日志里；验证码也不能出现在数据目录里（库里只存哈希）。

mod common;

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anycon_hub::config::{parse_email_from, SmtpConfig, SmtpSecurity};
use anycon_hub::mail::{MailError, Mailer, OutgoingMail};
use anycon_protocol::{Control, DeviceKind, Kind, NotifyCategory};
use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const MARKER: &str = "TOP-SECRET-PAYLOAD-MARKER-0123456789";

/// 进程里只能装一次全局 tracing subscriber；两个测试共用同一份日志捕获。
fn capture() -> Capture {
    static CAPTURE: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let c = Capture::default();
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::new("anycon_hub=trace"))
                .with_writer(c.clone())
                .with_ansi(false)
                .init();
            c
        })
        .clone()
}

#[tokio::test]
async fn logs_never_contain_secrets_or_envelope_bodies() {
    let capture = capture();

    let hub = TestHub::start().await;
    let mut secrets: Vec<(String, String)> = vec![("admin token".into(), hub.admin_token.clone())];
    // 配对用的短码 / match_id：除了日志，数据目录和 broker 状态里也不能有（admin-token 文件本来就存明文 admin token，所以不对所有秘密做这项检查）
    let mut pairing_secrets: Vec<(String, String)> = Vec::new();

    // 登录、邀请、账号 token
    let cookie = hub.login_admin().await;
    secrets.push(("session cookie".into(), cookie.split_once('=').unwrap().1.to_string()));
    let inv: Value = hub.send(reqwest::Method::POST, "/api/invites", &cookie, json!({})).await.json().await.unwrap();
    secrets.push(("invite code".into(), inv["code"].as_str().unwrap().to_string()));
    let tok: Value =
        hub.send(reqwest::Method::POST, "/api/me/tokens", &cookie, json!({ "label": "x" })).await.json().await.unwrap();
    secrets.push(("account token".into(), tok["token"].as_str().unwrap().to_string()));
    hub.http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": "a-wrong-token-value-that-must-not-be-logged" }))
        .send()
        .await
        .unwrap();
    secrets.push(("rejected login token".into(), "a-wrong-token-value-that-must-not-be-logged".into()));

    // 完整配对 + 双向转发（含离线 / 非配对 / 超大的丢弃路径）
    let seeded = hub.seed_device(DeviceKind::Daemon, "d").await;
    secrets.push(("daemon device token".into(), seeded.token.clone()));
    let mut daemon = FakeDevice::connect(&hub, Some(&seeded.token), DeviceKind::Daemon, "d").await;
    let (code, match_id) = new_code();
    let (code_pending, match_id_pending) = new_code();
    let (code_wrong, match_id_wrong) = new_code();
    let match_id_malformed = "not-a-match-id-!!".to_string();
    for (what, code, match_id) in [
        ("matched pairing", &code, &match_id),
        ("pending registration", &code_pending, &match_id_pending),
        ("wrong guess", &code_wrong, &match_id_wrong),
    ] {
        for (kind, s) in [
            ("code", code.clone()),
            ("code (no hyphen)", code.replace('-', "")),
            ("code (lowercase)", code.to_lowercase()),
            ("match id", match_id.clone()),
            ("match id (uppercase)", match_id.to_uppercase()),
        ] {
            secrets.push((format!("{what}: {kind}"), s.clone()));
            pairing_secrets.push((format!("{what}: {kind}"), s));
        }
    }
    secrets.push(("malformed match id".into(), match_id_malformed.clone()));
    pairing_secrets.push(("malformed match id".into(), match_id_malformed.clone()));

    daemon.pair_begin(&match_id).await;
    // 一个登记着、没人加入的 host；一个猜错的 joiner；一个格式非法的 PairBegin
    let pending = hub.seed_device(DeviceKind::Daemon, "pending").await;
    let mut pending_host = FakeDevice::connect(&hub, Some(&pending.token), DeviceKind::Daemon, "pending").await;
    pending_host.pair_begin(&match_id_pending).await;
    pending_host.send_control(&Control::PairBegin { match_id: match_id_malformed.clone() }).await;
    assert!(matches!(pending_host.recv_control().await, Control::PairFailed { .. }));
    pending_host.pair_begin(&match_id_pending).await; // 再登记回来，扫描时仍有一个活着的登记
    let mut guesser = FakeDevice::connect(&hub, None, DeviceKind::Cli, "g").await;
    guesser.send_control(&Control::PairJoin { match_id: match_id_wrong.clone() }).await;
    assert!(matches!(guesser.recv_control().await, Control::PairFailed { .. }));
    guesser.send_control(&Control::PairJoin { match_id: match_id_malformed.clone() }).await;
    assert!(matches!(guesser.recv_control().await, Control::PairFailed { .. }));

    let mut joiner = FakeDevice::connect(&hub, None, DeviceKind::Web, "w").await;
    joiner.send_control(&Control::PairJoin { match_id: match_id.clone() }).await;
    let _ = joiner.recv_control().await;
    let temp = joiner.id.clone();
    let _ = daemon.recv_control().await;
    joiner.send_env(&seeded.id, Kind::Handshake, MARKER.as_bytes().to_vec()).await;
    assert_eq!(daemon.recv_env().await.body, MARKER.as_bytes());
    joiner.send_control(&Control::PairConfirm { peer: seeded.id.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: temp }).await;
    let new_token = match joiner.recv_control().await {
        Control::PairDone { device_token: Some(t), .. } => t,
        other => panic!("{other:?}"),
    };
    secrets.push(("joiner device token".into(), new_token));
    let _ = daemon.recv_control().await;
    let _ = daemon.recv_control().await; // PeerOnline
    let new_id = joiner.id.clone();
    let _ = new_id;
    joiner
        .send_env_as(&joiner_perm_id(&hub, &seeded.id).await, &seeded.id, Kind::Data, MARKER.as_bytes().to_vec())
        .await;
    assert_eq!(daemon.recv_env().await.body, MARKER.as_bytes());
    joiner.send_env(&seeded.id, Kind::Data, MARKER.as_bytes().to_vec()).await; // from 不匹配 => 丢弃
    joiner.send_env_as(&"f".repeat(32), "e".repeat(32).as_str(), Kind::Notify, MARKER.as_bytes().to_vec()).await;
    daemon.send_env(&"0".repeat(32), Kind::Data, MARKER.as_bytes().to_vec()).await; // 未配对 => 丢弃
    daemon.send_raw(MARKER.as_bytes().to_vec()).await; // 畸形帧 => 丢弃

    // 账号内接入（ADR-36）：握手体（含新设备自报的提示信息）和配对一样，hub 不解析也不记录；`enroll_id` 不是秘密，但状态的
    // `Debug` 输出里也只有数量。一个走完（拿到新 token），一个停在半路（会话还活着）。
    let enroll_node = hub.seed_device(DeviceKind::Daemon, "enroll-node").await;
    secrets.push(("enroll node device token".into(), enroll_node.token.clone()));
    let mut enroll_daemon =
        FakeDevice::connect(&hub, Some(&enroll_node.token), DeviceKind::Daemon, "enroll-node").await;
    let mut enrollee = FakeDevice::connect_with(&hub, None, DeviceKind::Web, "e", &[("cookie", cookie.as_str())]).await;
    let enrollee_temp = enrollee.id.clone();
    enrollee.send_control(&Control::EnrollBegin { node: enroll_node.id.clone() }).await;
    let enroll_id = match enrollee.recv_control().await {
        Control::EnrollStarted { enroll_id, .. } => enroll_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(enroll_daemon.recv_control().await, Control::EnrollRequest { .. }));
    enrollee.send_env(&enroll_node.id, Kind::Handshake, MARKER.as_bytes().to_vec()).await;
    assert_eq!(enroll_daemon.recv_env().await.body, MARKER.as_bytes());
    enroll_daemon.send_env(&enrollee_temp, Kind::Handshake, MARKER.as_bytes().to_vec()).await;
    assert_eq!(enrollee.recv_env().await.body, MARKER.as_bytes());
    enrollee.send_control(&Control::PairConfirm { peer: enroll_node.id.clone() }).await;
    enroll_daemon.send_control(&Control::PairConfirm { peer: enrollee_temp.clone() }).await;
    match enrollee.recv_control().await {
        Control::PairDone { device_token: Some(t), .. } => secrets.push(("enrollee device token".into(), t)),
        other => panic!("{other:?}"),
    }
    let mut unfinished =
        FakeDevice::connect_with(&hub, None, DeviceKind::Cli, "u", &[("cookie", cookie.as_str())]).await;
    let unfinished_temp = unfinished.id.clone();
    let enroll_id_pending = {
        unfinished.send_control(&Control::EnrollBegin { node: enroll_node.id.clone() }).await;
        match unfinished.recv_control().await {
            Control::EnrollStarted { enroll_id, .. } => enroll_id,
            other => panic!("{other:?}"),
        }
    };
    let _ = enroll_daemon.drain_controls(std::time::Duration::from_millis(200)).await;
    unfinished.send_env(&enroll_node.id, Kind::Handshake, MARKER.as_bytes().to_vec()).await;
    let enroll_dump = format!("{:?}", hub.handle.state.enrolls);
    assert!(enroll_dump.contains("sessions: 1"), "the unfinished session is still alive: {enroll_dump}");
    for id in [&enroll_id, &enroll_id_pending, &enroll_node.id, &unfinished_temp, &enrollee_temp] {
        assert!(!enroll_dump.contains(id.as_str()), "enrollment state dump leaks {id}: {enroll_dump}");
    }

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // hub 的状态里没有短码 / match_id：数据目录里的每个文件（含 SQLite 的 WAL）、Broker 的调试输出
    let broker_dump = format!("{:?}", hub.handle.state.broker);
    assert!(broker_dump.contains("registrations: 1"), "the pending registration is still alive: {broker_dump}");
    let mut files = Vec::new();
    collect_files(hub.dir.path(), &mut files);
    assert!(files.iter().any(|f| f.ends_with("hub.db")), "sanity: the database is in the data dir: {files:?}");
    let state_blobs: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|f| (f.display().to_string(), std::fs::read(f).unwrap_or_default()))
        .chain(std::iter::once(("Broker debug output".to_string(), broker_dump.into_bytes())))
        .collect();

    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    assert!(log.contains("device connected"), "sanity: the capture works\n{log}");
    assert!(log.contains("drop:"), "sanity: drops are logged at debug/trace\n{log}");
    for (what, secret) in secrets {
        assert!(!log.contains(&secret), "{what} leaked into the logs");
    }
    for (what, secret) in pairing_secrets {
        let needle = secret.as_bytes();
        for (name, blob) in &state_blobs {
            assert!(!blob.windows(needle.len()).any(|w| w == needle), "{what} found in {name}");
        }
    }
    assert!(!log.contains(MARKER), "envelope body leaked into the logs");
    assert!(!log.contains(&hex::encode(MARKER)), "envelope body (hex) leaked into the logs");
    assert!(!log.to_lowercase().contains("set-cookie"), "cookie header leaked into the logs");
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// joiner 的正式设备 id（配对时由 hub 生成，只有库里能查到）。
async fn joiner_perm_id(hub: &TestHub, host: &str) -> String {
    hub.handle.store().peers_of(host).await.unwrap().into_iter().next().expect("paired")
}

// ---------------------------------------------------------------------------
// ADR-37：邮箱验证码、Web Push、邮件通知
// ---------------------------------------------------------------------------

/// 记下发出的邮件；打开 `leak` 后发信失败，而且失败信息里带着收件人地址（真实的 SMTP 服务器常这么回）。
#[derive(Default)]
struct LeakyMailer {
    sent: Mutex<Vec<OutgoingMail>>,
    leak: AtomicBool,
}

#[async_trait]
impl Mailer for LeakyMailer {
    async fn send(&self, mail: OutgoingMail) -> Result<(), MailError> {
        if self.leak.load(Ordering::SeqCst) {
            return Err(MailError::Smtp {
                status: Some("550".into()),
                message: format!("<{}>: Recipient address rejected: user unknown", mail.to),
            });
        }
        self.sent.lock().unwrap().push(mail);
        Ok(())
    }
}

const SMTP_USER: &str = "smtp-user-SECRET";
const SMTP_PASSWORD: &str = "smtp-PASSWORD-SECRET-9f3a";
const ENDPOINT_PATH: &str = "PUSH-ENDPOINT-PATH-SECRET-abc123";
const OTHER_ENDPOINT_PATH: &str = "DEAD-ENDPOINT-PATH-SECRET-def456";
const GONE_ENDPOINT_PATH: &str = "GONE-ENDPOINT-PATH-SECRET-789xyz";

#[tokio::test]
async fn email_and_push_secrets_never_reach_the_logs() {
    let capture = capture();
    let mailer = Arc::new(LeakyMailer::default());
    let push = FakePush::start().await;
    let hub = TestHub::start_with({
        let mailer = mailer.clone();
        move |c| {
            c.mailer_override = Some(mailer);
            c.push_allow_hosts = Some(vec!["127.0.0.1".into()]);
            // 配置里有 SMTP 凭据：启动日志和 Debug 输出里不能出现
            c.smtp = Some(SmtpConfig {
                security: SmtpSecurity::Plain,
                host: "127.0.0.1".into(),
                port: 2525,
                username: Some(SMTP_USER.into()),
                password: Some(SMTP_PASSWORD.into()),
            });
            c.email_from = Some(parse_email_from("Anycon <noreply@example.test>").unwrap());
        }
    })
    .await;
    let mut secrets: Vec<(String, String)> = vec![
        ("smtp password".into(), SMTP_PASSWORD.into()),
        ("smtp username".into(), SMTP_USER.into()),
        ("push endpoint path".into(), ENDPOINT_PATH.into()),
        ("dead push endpoint path".into(), OTHER_ENDPOINT_PATH.into()),
        ("gone push endpoint path".into(), GONE_ENDPOINT_PATH.into()),
    ];
    // 邮箱原文（含大小写变体）：日志里只能有哈希前 8 位
    let typed = "Secret.Person@Example.COM";
    for (what, addr) in [
        ("email as typed", typed),
        ("email normalized", "secret.person@example.com"),
        ("unbound email", "nobody.here@example.com"),
        ("second account email", "guest.person@example.com"),
    ] {
        secrets.push((what.into(), addr.into()));
        secrets.push((format!("{what} (lower)"), addr.to_lowercase()));
        secrets.push((format!("{what} (upper)"), addr.to_uppercase()));
    }
    let admin = hub.login_admin().await;
    let post = |path: &'static str, body: Value| {
        let req = hub.http.post(hub.url(path)).header("x-anycon", "1").json(&body);
        async move { req.send().await.unwrap() }
    };
    let mut codes: Vec<String> = Vec::new();
    let last_code = |mailer: &LeakyMailer| code_in(mailer.sent.lock().unwrap().last().unwrap());

    // 绑定：发码 → 错码 → 对码
    assert_eq!(hub.send(reqwest::Method::POST, "/api/me/email", &admin, json!({ "email": typed })).await.status(), 200);
    hub.settle().await;
    let bind_code = last_code(&mailer);
    codes.push(bind_code.clone());
    let wrong = if bind_code == "000000" { "000001" } else { "000000" };
    let res =
        hub.send(reqwest::Method::POST, "/api/me/email/verify", &admin, json!({ "email": typed, "code": wrong })).await;
    assert_eq!(res.status(), 400);
    let res = hub
        .send(reqwest::Method::POST, "/api/me/email/verify", &admin, json!({ "email": typed, "code": bind_code }))
        .await;
    assert_eq!(res.status(), 200);

    // 登录：发码、立刻再要一次（限速）、错码、对码；没绑定的邮箱也要一次
    assert_eq!(post("/auth/email/send", json!({ "email": typed })).await.status(), 200);
    hub.settle().await;
    let login_code = last_code(&mailer);
    codes.push(login_code.clone());
    assert_eq!(post("/auth/email/send", json!({ "email": typed })).await.status(), 429);
    assert_eq!(post("/auth/email/send", json!({ "email": "nobody.here@example.com" })).await.status(), 200);
    let wrong = if login_code == "000000" { "000001" } else { "000000" };
    assert_eq!(post("/auth/email/verify", json!({ "email": typed, "code": wrong })).await.status(), 401);
    assert_eq!(post("/auth/email/verify", json!({ "email": typed, "code": login_code })).await.status(), 204);
    hub.settle().await;

    // 发信失败，失败信息里带着收件人地址：日志里必须是打码之后的
    let (gid, _guest) = hub.invite_account(&admin).await;
    hub.handle.store().set_account_email(&gid, Some("guest.person@example.com")).await.unwrap();
    mailer.leak.store(true, Ordering::SeqCst);
    assert_eq!(post("/auth/email/send", json!({ "email": "guest.person@example.com" })).await.status(), 200);
    hub.settle().await;
    mailer.leak.store(false, Ordering::SeqCst);

    // 推送：成功、订阅过期（410）、连不上的 endpoint
    let daemon = hub.seed_device(DeviceKind::Daemon, "node-for-push").await;
    let web = hub.seed_device(DeviceKind::Web, "browser-ok").await;
    let web_gone = hub.seed_device(DeviceKind::Web, "browser-gone").await;
    let web_dead = hub.seed_device(DeviceKind::Web, "browser-dead").await;
    for w in [&web, &web_gone, &web_dead] {
        hub.seed_paired(&daemon, w).await;
    }
    let mut daemon_conn = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "node-for-push").await;
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut browsers = Vec::new();
    for (w, endpoint) in [
        (&web, push.endpoint(ENDPOINT_PATH)),
        (&web_gone, push.endpoint(GONE_ENDPOINT_PATH)),
        (&web_dead, format!("http://127.0.0.1:{dead_port}/{OTHER_ENDPOINT_PATH}")),
    ] {
        let b = Browser::new();
        secrets.push(("p256dh".into(), b.p256dh()));
        secrets.push(("push auth secret".into(), b.auth_b64()));
        secrets.push(("push endpoint".into(), endpoint.clone()));
        let res =
            hub.send(reqwest::Method::POST, "/api/push/subscriptions", &admin, b.subscription(&w.id, &endpoint)).await;
        assert_eq!(res.status(), 200);
        browsers.push(b);
    }
    // 被拒绝的 endpoint（白名单）：路径里的秘密同样不能进日志
    let rejected = "https://evil.example/REJECTED-ENDPOINT-PATH-SECRET".to_string();
    secrets.push(("rejected push endpoint".into(), "REJECTED-ENDPOINT-PATH-SECRET".into()));
    let b = Browser::new();
    assert_eq!(
        hub.send(reqwest::Method::POST, "/api/push/subscriptions", &admin, b.subscription(&web.id, &rejected))
            .await
            .status(),
        400
    );

    let notify = |to: &str| {
        let to = to.to_string();
        let body = notify_body(NotifyCategory::PermissionRequested, Some("sess-1"));
        (to, body)
    };
    let (to, body) = notify(&web.id);
    daemon_conn.send_env(&to, Kind::Notify, body).await;
    assert!(matches!(daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    hub.settle().await;
    push.set_status(410);
    let (to, body) = notify(&web_gone.id);
    daemon_conn.send_env(&to, Kind::Notify, body).await;
    assert!(matches!(daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    let (to, body) = notify(&web_dead.id);
    daemon_conn.send_env(&to, Kind::Notify, body).await;
    assert!(matches!(daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    hub.settle().await;
    push.set_status(500);
    let res = hub.send(reqwest::Method::POST, "/api/push/test", &admin, json!({ "device_id": web.id })).await;
    assert_eq!(res.status(), 200);

    // 通知邮件：账号绑了邮箱、没有客户端在线（这个 hub 里没连任何 web / cli / sdk）
    let (to, body) = notify(&web.id);
    daemon_conn.send_env(&to, Kind::Notify, body).await;
    assert!(matches!(daemon_conn.recv_control().await, Control::Undeliverable { .. }));
    hub.settle().await;
    let notification_mail = mailer.sent.lock().unwrap().last().unwrap().clone();
    assert!(notification_mail.subject.starts_with("[Anycon]"), "{}", notification_mail.subject);

    // 推送服务收到的 Authorization（VAPID JWT）和载荷里的东西也不能出现在日志里
    for req in push.requests() {
        let auth = req.header("authorization").to_string();
        let jwt = auth.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap().0.to_string();
        secrets.push(("vapid jwt".into(), jwt));
    }
    // VAPID 私钥
    let vapid: Value = serde_json::from_slice(&std::fs::read(hub.dir.path().join("vapid.json")).unwrap()).unwrap();
    secrets.push(("vapid private key".into(), vapid["private_key"].as_str().unwrap().to_string()));
    for code in &codes {
        secrets.push(("email code".into(), code.clone()));
        secrets.push(("email code (grouped)".into(), format!("{} {}", &code[..3], &code[3..])));
    }

    // Debug 输出：配置里的 SMTP 凭据、推送服务里的私钥
    let config_dump = format!("{:?}", hub.handle.state.config);
    let push_dump = format!("{:?}", hub.handle.state.push);
    assert!(config_dump.contains("127.0.0.1"), "sanity: {config_dump}");
    for (what, dump) in [("Config", &config_dump), ("PushService", &push_dump)] {
        for (name, secret) in &secrets {
            assert!(!dump.contains(secret.as_str()), "{what} Debug output leaks {name}");
        }
    }

    // 数据目录里：库里只存验证码的哈希，邮件正文里「123 456」那种写法不会落盘；SMTP 密码不落盘。
    // （6 位数字本身在二进制里很容易碰巧出现，所以不扫它。）
    let mut files = Vec::new();
    collect_files(hub.dir.path(), &mut files);
    assert!(files.iter().any(|f| f.ends_with("vapid.json")), "sanity: {files:?}");
    for f in &files {
        let blob = std::fs::read(f).unwrap_or_default();
        for code in &codes {
            let grouped = format!("{} {}", &code[..3], &code[3..]);
            assert!(!blob.windows(grouped.len()).any(|w| w == grouped.as_bytes()), "grouped code in {}", f.display());
        }
        assert!(
            !blob.windows(SMTP_PASSWORD.len()).any(|w| w == SMTP_PASSWORD.as_bytes()),
            "SMTP password in {}",
            f.display()
        );
    }
    let rows = hub
        .handle
        .store()
        .list_email_codes("secret.person@example.com", anycon_hub::store::EmailPurpose::Login)
        .await
        .unwrap();
    assert!(rows.is_empty(), "the used login code is gone from the table");

    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    // 自检：这些路径真的被走到了、日志里有该有的东西（主机名 / 状态码 / 哈希标记），而不是什么都没记
    assert!(log.contains("email code sent"), "sanity\n{log}");
    assert!(log.contains("email code could not be sent"), "sanity\n{log}");
    assert!(
        log.contains("web push rejected by the push service") || log.contains("web push request failed"),
        "sanity\n{log}"
    );
    assert!(log.contains("the subscription is gone"), "sanity\n{log}");
    assert!(log.contains("web push request failed"), "sanity: the dead endpoint\n{log}");
    assert!(log.contains("notification email sent"), "sanity\n{log}");
    assert!(
        log.contains(&anycon_hub::util::email_tag("secret.person@example.com")),
        "the email shows up only as its hash prefix\n{log}"
    );
    assert!(log.contains("127.0.0.1"), "push results name the push service host\n{log}");
    assert!(log.contains("status=410") || log.contains("status=500"), "and the status code\n{log}");
    assert!(log.contains("<redacted>"), "the recipient was scrubbed out of the SMTP error\n{log}");
    for (what, secret) in &secrets {
        assert!(!log.contains(secret.as_str()), "{what} leaked into the logs");
    }
    // 请求日志只记路由模板，不记 URI
    assert!(!log.contains("evil.example"), "rejected endpoint hosts are not logged either\n");
}

// ---------------------------------------------------------------------------
// ADR-39：预览令牌、预览域名上的请求路径、隧道里的信封
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preview_tokens_paths_and_tunnel_traffic_never_reach_the_logs_or_the_data_dir() {
    let capture = capture();
    let hub = TestHub::start_preview().await;
    let node = hub.seed_device(DeviceKind::Daemon, "node").await;
    let web = hub.seed_device(DeviceKind::Web, "web").await;
    hub.seed_paired(&node, &web).await;
    let label = new_label();
    let wrong_device_token = "a-wrong-device-token-for-previews-that-must-not-be-logged";
    let wrong_preview_token = "a-wrong-preview-token-that-must-not-be-logged";
    let path_marker = "PREVIEW-PATH-SECRET-0123456789";
    let query_marker = "PREVIEW-QUERY-SECRET-0123456789";

    // 令牌申请：成功的（两次，同一个 label）、设备 token 不对的、节点没配对的
    let token = hub.preview_token(&web, &node, &label).await;
    let second = hub.preview_token(&web, &node, &label).await;
    assert_eq!(hub.request_preview_token(wrong_device_token, &node.id, &label).await.status(), 401);
    let stranger = hub.seed_device(DeviceKind::Daemon, "stranger").await;
    assert_eq!(hub.request_preview_token(&web.token, &stranger.id, &label).await.status(), 404);

    // 预览连接：成功的、令牌错误的；双向转发带标记的信封，也走一遍丢弃的路径
    let mut daemon = FakeDevice::connect(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let mut p = FakeDevice::connect_preview(&hub, &label, &token).await;
    assert!(matches!(daemon.recv_control().await, Control::PreviewPeer { .. }));
    let mut bad = FakeDevice::open_preview(&hub, &label).await.unwrap();
    bad.send_preview_auth(Some(wrong_preview_token)).await;
    assert!(matches!(bad.recv_control().await, Control::AuthFail { .. }));
    p.send_env(&node.id, Kind::Handshake, MARKER.as_bytes().to_vec()).await;
    assert_eq!(daemon.recv_env().await.body, MARKER.as_bytes());
    daemon.send_env(&p.id, Kind::Data, MARKER.as_bytes().to_vec()).await;
    assert_eq!(p.recv_env().await.body, MARKER.as_bytes());
    p.send_env(&web.id, Kind::Data, MARKER.as_bytes().to_vec()).await; // 发给别人 => 丢弃
    p.send_env(&node.id, Kind::Notify, MARKER.as_bytes().to_vec()).await; // notify => 丢弃
    p.send_raw(MARKER.as_bytes().to_vec()).await; // 畸形帧 => 丢弃

    // 预览域名上的 HTTP：路径和查询串里带标记
    hub.preview_get(&label, &format!("/{path_marker}?q={query_marker}")).await;
    hub.preview_get(&label, &format!("/__anycon/{path_marker}.js?q={query_marker}")).await;
    hub.preview_get(&label, "/__anycon/config.json").await;
    hub.preview_get(&label, "/__anycon/sw.js").await;

    // 吊销签发令牌的设备：连接以 4004 断开，令牌作废
    let admin = hub.login_admin().await;
    assert_eq!(
        hub.send(reqwest::Method::DELETE, &format!("/api/devices/{}", web.id), &admin, json!(null)).await.status(),
        204
    );
    // （之前被丢弃的帧的 dropped 回执可能排在前面）
    loop {
        match p.recv().await {
            Recv::Closed(code) => {
                assert_eq!(code, Some(4004));
                break;
            }
            Recv::Env(_) => {}
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // 数据目录里只有令牌的哈希，没有明文
    let mut files = Vec::new();
    collect_files(hub.dir.path(), &mut files);
    assert!(files.iter().any(|f| f.ends_with("hub.db")), "sanity: the database is in the data dir: {files:?}");
    for f in &files {
        let blob = std::fs::read(f).unwrap_or_default();
        for (what, secret) in [("preview token", &token), ("second preview token", &second)] {
            assert!(!blob.windows(secret.len()).any(|w| w == secret.as_bytes()), "{what} in {}", f.display());
        }
    }

    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    // 自检：这些路径真的走到了
    assert!(log.contains("preview token issued"), "sanity\n{log}");
    assert!(log.contains("preview connected"), "sanity\n{log}");
    assert!(
        log.contains("route=\"(preview)\"") || log.contains("route=(preview)"),
        "preview requests are logged as (preview)\n{log}"
    );
    assert!(log.contains("drop:"), "sanity: drops are logged at debug/trace\n{log}");
    for (what, secret) in [
        ("preview token", token.clone()),
        ("second preview token", second.clone()),
        ("preview token hash", anycon_hub::util::hash_token(&token)),
        ("second preview token hash", anycon_hub::util::hash_token(&second)),
        ("rejected preview token", wrong_preview_token.to_string()),
        ("rejected device token", wrong_device_token.to_string()),
        ("web device token", web.token.clone()),
        ("request path on the preview domain", path_marker.to_string()),
        ("request query on the preview domain", query_marker.to_string()),
        ("preview label", label.clone()),
        ("envelope body", MARKER.to_string()),
        ("envelope body (hex)", hex::encode(MARKER)),
    ] {
        assert!(!log.contains(&secret), "{what} leaked into the logs");
    }
}
