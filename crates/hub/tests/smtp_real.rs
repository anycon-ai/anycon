//! 真实的 lettre 发信路径对一个测试里起的最小 SMTP 服务（`smtp+plain://127.0.0.1:<port>`）：
//! 邮箱验证码、通知邮件真的能发出去，凭据按 URL 里写的发，`smtp://` 的 STARTTLS 是必须的（绝不退回明文）。

mod common;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anycon_hub::config::{parse_email_from, parse_smtp_url, SmtpConfig, SmtpSecurity};
use anycon_hub::mail::{HostResolver, MailError, Mailer, OutgoingMail, SmtpMailer};
use anycon_protocol::{Control, DeviceKind, Kind, NotifyCategory};
use async_trait::async_trait;
use common::smtp::FakeSmtp;
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

fn plain(port: u16) -> SmtpConfig {
    parse_smtp_url(&format!("smtp+plain://127.0.0.1:{port}")).unwrap()
}

async fn hub_with_smtp(smtp: SmtpConfig) -> TestHub {
    TestHub::start_with(move |c| {
        c.smtp = Some(smtp);
        c.email_from = Some(parse_email_from("Anycon <noreply@example.test>").unwrap());
    })
    .await
}

#[tokio::test]
async fn a_login_code_really_travels_over_smtp() {
    let smtp = FakeSmtp::start(false).await;
    let hub = hub_with_smtp(plain(smtp.port())).await;
    let admin = hub.login_admin().await;

    // 绑定：发码走真的 SMTP
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": " Me@Example.COM" })).await;
    assert_eq!(res.status(), 200);
    let mails = smtp.wait_for(1).await;
    let mail = &mails[0];
    assert_eq!(mail.mail_from, "noreply@example.test");
    assert_eq!(mail.rcpt_to, vec!["me@example.com"]);
    assert_eq!(mail.header("from").unwrap(), "Anycon <noreply@example.test>");
    assert_eq!(mail.header("to").unwrap(), "me@example.com");
    assert!(mail.header("subject").unwrap().contains("Confirm your email address"), "{:?}", mail.header("subject"));
    assert!(mail.header("content-type").unwrap().starts_with("text/plain"), "{:?}", mail.header("content-type"));
    assert!(mail.header("message-id").unwrap().ends_with("@example.test>"), "{:?}", mail.header("message-id"));
    assert!(mail.header("date").is_some());
    let text = mail.text();
    assert!(text.contains("valid for 10 minutes") && text.contains("ignore this email"), "{text}");
    // 英文验证码邮件原样发出（7bit），不会被 quoted-printable 的软换行从码中间切开
    assert_eq!(mail.header("content-transfer-encoding").unwrap().to_ascii_lowercase(), "7bit", "{:?}", mail.headers());
    assert!(mail.data.contains(&format!("is {} {}.", &mail.code()[..3], &mail.code()[3..])), "{}", mail.data);
    let res = hub
        .send(Method::POST, "/api/me/email/verify", &admin, json!({ "email": "me@example.com", "code": mail.code() }))
        .await;
    assert_eq!(res.status(), 200);

    // 登录：从另一个浏览器用邮箱 + 码
    let res = hub
        .http
        .post(hub.url("/auth/email/send"))
        .header("x-anycon", "1")
        .json(&json!({ "email": "me@example.com" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mails = smtp.wait_for(2).await;
    assert!(mails[1].header("subject").unwrap().contains("sign-in code"));
    let res = hub
        .http
        .post(hub.url("/auth/email/verify"))
        .header("x-anycon", "1")
        .json(&json!({ "email": "me@example.com", "code": mails[1].code() }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let me: Value = hub.get("/api/me", &cookie_pair(&res)).await.json().await.unwrap();
    assert_eq!(me["email"], "me@example.com");
    // 一个连接上的命令序列是正常的 SMTP 对话
    let cmds = smtp.commands();
    assert!(cmds.iter().any(|c| c == "EHLO") && cmds.iter().any(|c| c == "DATA"), "{cmds:?}");
    assert!(smtp.auths().is_empty(), "no credentials in the URL, so no AUTH");
    assert_eq!(
        smtp.ehlo_names(),
        vec!["example.test"; smtp.ehlo_names().len()],
        "EHLO announces the sender domain, not localhost"
    );
    assert!(!smtp.ehlo_names().is_empty());
}

#[tokio::test]
async fn chinese_mail_is_encoded_properly_and_decodes_back() {
    let smtp = FakeSmtp::start(false).await;
    let hub = hub_with_smtp(plain(smtp.port())).await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("me@example.com")).await.unwrap();
    let res = hub
        .http
        .post(hub.url("/auth/email/send"))
        .header("x-anycon", "1")
        .header("accept-language", "zh-CN,zh;q=0.9")
        .json(&json!({ "email": "me@example.com" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let mail = smtp.wait_for(1).await.remove(0);
    let text = mail.text();
    assert!(
        text.contains("你的 Anycon 登录码是")
            && text.contains("10 分钟内有效")
            && text.contains("不是你本人操作就忽略这封邮件"),
        "{text}"
    );
    assert_eq!(mail.code().len(), 6);
    assert!(
        mail.header("content-type").unwrap().contains("utf-8")
            || mail.header("content-type").unwrap().contains("UTF-8"),
        "{:?}",
        mail.header("content-type")
    );
    assert!(
        mail.header("subject").unwrap().is_ascii(),
        "non-ASCII subjects are RFC 2047 encoded: {:?}",
        mail.header("subject")
    );
}

#[tokio::test]
async fn credentials_from_the_url_are_used_percent_decoded() {
    let smtp = FakeSmtp::start(true).await;
    let cfg =
        parse_smtp_url(&format!("smtp+plain://mailer%40example.test:p%40ss%3Aword@127.0.0.1:{}", smtp.port())).unwrap();
    assert_eq!(cfg.username.as_deref(), Some("mailer@example.test"));
    let mailer = SmtpMailer::new(&cfg, parse_email_from("noreply@example.test").unwrap().mailbox).unwrap();
    mailer
        .send(OutgoingMail {
            to: "you@example.com".into(),
            subject: "hello".into(),
            body: "Your code is 123 456.\n".into(),
        })
        .await
        .unwrap();
    let mails = smtp.wait_for(1).await;
    assert_eq!(mails[0].rcpt_to, vec!["you@example.com"]);
    assert_eq!(mails[0].code(), "123456");
    assert_eq!(smtp.auths(), vec!["mailer@example.test:p@ss:word"], "user and password reach the server decoded");
}

#[tokio::test]
async fn starttls_is_required_and_never_falls_back_to_plain_text() {
    // 服务器不宣告 STARTTLS：smtp:// 必须发不出去，而且不能把邮件明文送出去
    let smtp = FakeSmtp::start(true).await;
    let cfg = SmtpConfig {
        security: SmtpSecurity::StartTls,
        host: "127.0.0.1".into(),
        port: smtp.port(),
        username: Some("u".into()),
        password: Some("hunter2".into()),
    };
    let mailer = SmtpMailer::new(&cfg, parse_email_from("noreply@example.test").unwrap().mailbox).unwrap();
    let err = mailer
        .send(OutgoingMail { to: "you@example.com".into(), subject: "s".into(), body: "123 456".into() })
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("hunter2"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(smtp.mails().is_empty(), "nothing was delivered in clear text");
    assert!(smtp.auths().is_empty(), "and no credentials were sent in clear text");
    assert!(!smtp.commands().iter().any(|c| c == "DATA" || c == "AUTH"), "{:?}", smtp.commands());

    // 宣告了 STARTTLS 但握手起不来（这个假服务回 454）：同样失败，不降级
    let smtp2 = FakeSmtp::start_with(true, true).await;
    let cfg2 = SmtpConfig { port: smtp2.port(), ..cfg };
    let mailer2 = SmtpMailer::new(&cfg2, parse_email_from("noreply@example.test").unwrap().mailbox).unwrap();
    assert!(mailer2
        .send(OutgoingMail { to: "you@example.com".into(), subject: "s".into(), body: "b".into() })
        .await
        .is_err());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(smtp2.mails().is_empty() && smtp2.auths().is_empty());
}

#[tokio::test]
async fn implicit_tls_to_a_plain_server_fails_cleanly() {
    let smtp = FakeSmtp::start(false).await;
    let cfg = SmtpConfig {
        security: SmtpSecurity::Tls,
        host: "127.0.0.1".into(),
        port: smtp.port(),
        username: None,
        password: None,
    };
    let mailer = SmtpMailer::new(&cfg, parse_email_from("noreply@example.test").unwrap().mailbox).unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(40),
        mailer.send(OutgoingMail { to: "you@example.com".into(), subject: "s".into(), body: "b".into() }),
    )
    .await;
    assert!(res.expect("fails instead of hanging").is_err());
    assert!(smtp.mails().is_empty());
}

#[tokio::test]
async fn an_smtp_error_or_a_dead_server_does_not_break_the_api() {
    let smtp = FakeSmtp::start(false).await;
    let hub = hub_with_smtp(plain(smtp.port())).await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("me@example.com")).await.unwrap();
    smtp.set_rcpt_reply(Some("550 5.1.1 <me@example.com> user unknown"));
    let post = |email: &str| {
        hub.http.post(hub.url("/auth/email/send")).header("x-anycon", "1").json(&json!({ "email": email })).send()
    };
    assert_eq!(
        post("me@example.com").await.unwrap().status(),
        200,
        "a rejected recipient is the server's problem, not the client's"
    );
    hub.settle().await;
    assert!(smtp.mails().is_empty());

    // 服务器根本连不上
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let hub2 = hub_with_smtp(plain(dead)).await;
    hub2.handle.store().set_account_email(&hub2.admin_account_id(), Some("me@example.com")).await.unwrap();
    let res = hub2
        .http
        .post(hub2.url("/auth/email/send"))
        .header("x-anycon", "1")
        .json(&json!({ "email": "me@example.com" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    hub2.settle().await;
}

#[tokio::test]
async fn notification_mail_travels_over_smtp_too() {
    let smtp = FakeSmtp::start(false).await;
    let hub = hub_with_smtp(plain(smtp.port())).await;
    hub.handle.store().set_account_email(&hub.admin_account_id(), Some("me@example.com")).await.unwrap();
    let daemon = hub.seed_device(DeviceKind::Daemon, "build-box").await;
    let web = hub.seed_device(DeviceKind::Web, "Chrome").await;
    hub.seed_paired(&daemon, &web).await;
    let mut conn = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "build-box").await;
    conn.send_env(&web.id, Kind::Notify, notify_body(NotifyCategory::EnrollRequested, None)).await;
    assert!(matches!(conn.recv_control().await, Control::Undeliverable { .. }));
    let mail = smtp.wait_for(1).await.remove(0);
    assert_eq!(mail.rcpt_to, vec!["me@example.com"]);
    let subject = mail.header("subject").unwrap();
    assert!(subject.is_ascii(), "the Chinese half of the subject is encoded: {subject}");
    let text = mail.text();
    assert!(text.contains("build-box") && text.contains("asking to join") && text.contains("请求接入"), "{text}");
    assert!(text.contains(&format!("https://hub.test/n/{}/settings?tab=enroll", daemon.id)), "{text}");
}

#[tokio::test]
async fn the_hub_refuses_to_start_when_smtp_plain_points_at_a_public_ip_address() {
    // 程序化构造的配置也要过启动检查（环境变量路径在 Config::from_vars 里已经拒绝）
    let dir = tempfile::tempdir().unwrap();
    let mut config = anycon_hub::Config::new("https://hub.test", dir.path()).unwrap();
    config.listen = "127.0.0.1:0".parse().unwrap();
    config.smtp = Some(SmtpConfig {
        security: SmtpSecurity::Plain,
        host: "8.8.8.8".into(),
        port: 25,
        username: Some("u".into()),
        password: Some("hunter2".into()),
    });
    config.email_from = Some(parse_email_from("noreply@example.com").unwrap());
    let err = anycon_hub::run(config.clone()).await.err().expect("must not start");
    let msg = err.to_string();
    assert!(msg.contains("smtp+plain") && msg.contains("8.8.8.8"), "{msg}");
    assert!(!msg.contains("hunter2"));
    // 没有发件人也不让启动
    config.smtp = Some(plain(25));
    config.email_from = None;
    let err = anycon_hub::run(config).await.err().expect("must not start");
    assert!(err.to_string().contains("ANYCON_EMAIL_FROM"), "{err}");
}

// ---------------------------------------------------------------------------
// smtp+plain:// 的主机名每次发信前自己解析，所有地址都必须是本机 / 内网，然后把 IP 交给 lettre
// ---------------------------------------------------------------------------

/// 假解析器：名字 → 地址表，记下被问过的名字。
#[derive(Default)]
struct FakeResolver {
    table: Mutex<HashMap<String, Vec<IpAddr>>>,
    asked: Mutex<Vec<String>>,
}

impl FakeResolver {
    fn with(name: &str, addrs: &[&str]) -> Arc<Self> {
        let r = Self::default();
        r.table.lock().unwrap().insert(name.into(), addrs.iter().map(|a| a.parse().unwrap()).collect());
        Arc::new(r)
    }
}

#[async_trait]
impl HostResolver for FakeResolver {
    async fn resolve(&self, host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
        self.asked.lock().unwrap().push(host.to_string());
        self.table
            .lock()
            .unwrap()
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
    }
}

fn plain_named(host: &str, port: u16) -> SmtpConfig {
    SmtpConfig {
        security: SmtpSecurity::Plain,
        host: host.into(),
        port,
        username: Some("mailer".into()),
        password: Some("hunter2-relay-password".into()),
    }
}

fn mail() -> OutgoingMail {
    OutgoingMail {
        to: "you@example.com".into(),
        subject: "hello".into(),
        body: "Your code is 123 456.
"
        .into(),
    }
}

fn sender() -> lettre::message::Mailbox {
    parse_email_from("noreply@example.test").unwrap().mailbox
}

#[tokio::test]
async fn localhost_works_through_the_real_resolver() {
    // `localhost` 可能解析出 ::1 和 127.0.0.1，假 SMTP 只听 127.0.0.1：连不上的地址要跳过去试下一个。
    let smtp = FakeSmtp::start(false).await;
    let hub = hub_with_smtp(parse_smtp_url(&format!("smtp+plain://localhost:{}", smtp.port())).unwrap()).await;
    let admin = hub.login_admin().await;
    let res = hub.send(Method::POST, "/api/me/email", &admin, json!({ "email": "me@example.com" })).await;
    assert_eq!(res.status(), 200);
    let mails = smtp.wait_for(1).await;
    assert_eq!(mails[0].rcpt_to, vec!["me@example.com"]);
}

#[tokio::test]
async fn the_resolved_address_is_what_gets_connected_to_not_the_name() {
    // 这个名字根本不存在于真 DNS：能发出去，说明交给 lettre 的是解析出的 IP，lettre 没有再查一次名字。
    let smtp = FakeSmtp::start(true).await;
    let resolver = FakeResolver::with("relay.internal.test", &["127.0.0.1"]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("relay.internal.test", smtp.port()), sender(), resolver.clone())
            .unwrap();
    mailer.send(mail()).await.unwrap();
    assert_eq!(smtp.wait_for(1).await[0].rcpt_to, vec!["you@example.com"]);
    assert_eq!(smtp.auths(), vec!["mailer:hunter2-relay-password"], "credentials still follow the URL");
    assert_eq!(*resolver.asked.lock().unwrap(), vec!["relay.internal.test"]);
    // 每次发信都重新解析（名字的指向可能变）
    mailer.send(mail()).await.unwrap();
    assert_eq!(resolver.asked.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_name_that_resolves_to_a_public_address_is_refused_and_nothing_is_sent() {
    let smtp = FakeSmtp::start(true).await;
    // 单独一个公网地址
    let resolver = FakeResolver::with("mail.internal.test", &["8.8.8.8"]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("mail.internal.test", smtp.port()), sender(), resolver).unwrap();
    let err = mailer.send(mail()).await.unwrap_err();
    assert!(matches!(err, MailError::NotLocal { addr } if addr == "8.8.8.8".parse::<IpAddr>().unwrap()), "{err}");
    // 错误信息（会写进 warn 日志）不带 URL、用户名和密码
    let text = err.to_string();
    assert!(!text.contains("hunter2") && !text.contains("mailer") && !text.contains("smtp+plain://"), "{text}");

    // 一个名字解析出多个地址，**每一个**都得是本机 / 内网：混进一个公网地址，哪怕还有一个能用的本机地址，也整封拒绝
    let resolver = FakeResolver::with("mixed.internal.test", &["127.0.0.1", "93.184.216.34"]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("mixed.internal.test", smtp.port()), sender(), resolver).unwrap();
    assert!(matches!(mailer.send(mail()).await.unwrap_err(), MailError::NotLocal { .. }));
    // IPv4 映射的 IPv6 公网地址同样是公网
    let resolver = FakeResolver::with("mapped.internal.test", &["::ffff:8.8.8.8"]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("mapped.internal.test", smtp.port()), sender(), resolver).unwrap();
    assert!(matches!(mailer.send(mail()).await.unwrap_err(), MailError::NotLocal { .. }));

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        smtp.mails().is_empty() && smtp.commands().is_empty(),
        "no connection was even attempted: {:?}",
        smtp.commands()
    );
    assert!(smtp.auths().is_empty(), "and no credentials were sent");
}

#[tokio::test]
async fn an_ip_literal_is_checked_at_every_send_too() {
    // 启动时挡住了公网 IP 字面量；直接构造 SmtpMailer（绕过配置检查）也不会把明文发出去。
    // 真实解析器对 IP 字面量直接返回它自己，不查 DNS。
    let mailer = SmtpMailer::new(&plain_named("8.8.8.8", 25), sender()).unwrap();
    let err = tokio::time::timeout(Duration::from_secs(5), mailer.send(mail()))
        .await
        .expect("no network involved")
        .unwrap_err();
    assert!(matches!(err, MailError::NotLocal { addr } if addr == "8.8.8.8".parse::<IpAddr>().unwrap()), "{err}");
}

#[tokio::test]
async fn resolution_failures_and_empty_answers_fail_the_mail_without_connecting() {
    let smtp = FakeSmtp::start(false).await;
    let mailer = SmtpMailer::with_resolver(
        &plain_named("unknown.internal.test", smtp.port()),
        sender(),
        Arc::new(FakeResolver::default()),
    )
    .unwrap();
    assert!(matches!(mailer.send(mail()).await.unwrap_err(), MailError::Resolve(_)));
    let resolver = FakeResolver::with("empty.internal.test", &[]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("empty.internal.test", smtp.port()), sender(), resolver).unwrap();
    assert!(matches!(mailer.send(mail()).await.unwrap_err(), MailError::Resolve(_)));
    assert!(smtp.commands().is_empty());
}

#[tokio::test]
async fn a_dead_first_address_falls_through_to_the_next_local_one() {
    let smtp = FakeSmtp::start(true).await;
    // ::1 上没人听（连接被拒），127.0.0.1 是假 SMTP
    let resolver = FakeResolver::with("dual.internal.test", &["::1", "127.0.0.1"]);
    let mailer =
        SmtpMailer::with_resolver(&plain_named("dual.internal.test", smtp.port()), sender(), resolver).unwrap();
    mailer.send(mail()).await.unwrap();
    assert_eq!(smtp.wait_for(1).await.len(), 1);
    // 服务器答话了但拒收：这是服务器的回答，原样报出来
    smtp.set_rcpt_reply(Some("550 5.1.1 user unknown"));
    let err = mailer.send(mail()).await.unwrap_err();
    assert_eq!(err.status(), Some("550"), "{err}");
    assert_eq!(smtp.mails().len(), 1);
}
