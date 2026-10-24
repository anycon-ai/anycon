//! 日志纪律（docs/spec/hub.md § 10）在配额和删账号上的体现：配额日志只说哪一项超了，删账号只记谁发起的和规模；
//! 日志里不出现邮箱、账号 id（自己的和别人的）、设备 token、登录 cookie。
//!
//! 单独一个测试文件（= 单独的进程）：全局 tracing subscriber 只能装一次。

mod common;
mod multi_support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use anycon_protocol::{Control, DeviceKind};
use common::*;
use multi_support::*;
use reqwest::Method;
use serde_json::Value;
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

#[tokio::test]
async fn quota_and_account_deletion_logs_carry_no_emails_account_ids_tokens_or_cookies() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("anycon_hub=trace"))
        .with_writer(capture.clone())
        .with_ansi(false)
        .init();

    let q = limits(3, 1, 2, 5);
    let hub = multi_hub_with(q, |c| c.admin_emails = vec!["boss@example.com".into()]).await;
    let (boss, boss_cookie) = account(&hub, "Boss", Some("boss@example.com")).await;
    let (alice, alice_cookie) = account(&hub, "Alice", Some("alice@example.com")).await;
    let (bob, bob_cookie) = account(&hub, "Bob", Some("bob@example.com")).await;
    let mut secrets: Vec<(&str, String)> = Vec::new();
    for (what, value) in [("boss id", &boss), ("alice id", &alice), ("bob id", &bob)] {
        secrets.push((what, value.clone()));
    }
    for (what, value) in [("alice@example.com", "alice@example.com"), ("bob@example.com", "bob@example.com")] {
        secrets.push((what, value.to_string()));
    }
    for (what, cookie) in [("boss cookie", &boss_cookie), ("alice cookie", &alice_cookie), ("bob cookie", &bob_cookie)]
    {
        secrets.push((what, cookie.split_once('=').unwrap().1.to_string()));
    }

    // 连接数超限
    let devices: Vec<Seeded> = {
        let mut v = Vec::new();
        for i in 0..3 {
            v.push(hub.seed_device_in(&alice, DeviceKind::Web, &format!("d{i}")).await);
        }
        v
    };
    for d in &devices {
        secrets.push(("device token", d.token.clone()));
    }
    let (mut c0, _) = connect_with_quota(&hub, Some(&devices[0].token), DeviceKind::Web, "d0").await;
    let (_c1, _) = connect_with_quota(&hub, Some(&devices[1].token), DeviceKind::Web, "d1").await;
    connect_refused_by_quota(&hub, &devices[2].token, DeviceKind::Web, "d2").await;
    c0.drain_controls(SHORT).await;

    // 设备数超限（已经 3 台，再配对一台新的）
    let mut joiner = temp_with_cookie(&hub, &alice_cookie, DeviceKind::Cli, "fourth").await;
    pair_and_confirm(&mut c0, &mut joiner).await;
    assert_eq!(c0.recv_control().await, Control::PairFailed { reason: anycon_protocol::PairFailReason::Quota });
    assert_eq!(joiner.recv_control().await, Control::PairFailed { reason: anycon_protocol::PairFailReason::Quota });

    // 每日配对次数超限（上面已经用了 2 次：pair_begin + pair_join）
    for _ in 0..3 {
        joiner.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
        assert_eq!(
            joiner.recv_control().await,
            Control::PairFailed { reason: anycon_protocol::PairFailReason::NotFound }
        );
    }
    joiner.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
    assert_eq!(joiner.recv_control().await, Control::PairFailed { reason: anycon_protocol::PairFailReason::Quota });

    // 删账号：自己删、管理员删
    assert_eq!(hub.send(Method::DELETE, "/api/me", &bob_cookie, Value::Null).await.status(), 204);
    let (carol, _) = account(&hub, "Carol", Some("carol@example.com")).await;
    secrets.push(("carol id", carol.clone()));
    secrets.push(("carol@example.com", "carol@example.com".to_string()));
    let res = hub.send(Method::DELETE, &format!("/api/admin/accounts/{carol}"), &boss_cookie, Value::Null).await;
    assert_eq!(res.status(), 204);
    hub.settle().await;

    let log = String::from_utf8_lossy(&capture.0.lock().unwrap()).to_string();
    for needle in ["limit=\"connections\"", "limit=\"devices\"", "limit=\"daily_pairings\"", "account deleted"] {
        assert!(log.contains(needle), "expected {needle:?} in the log:\n{log}");
    }
    assert!(log.contains("by=\"self\"") && log.contains("by=\"admin\""), "who deleted is logged:\n{log}");
    for (what, secret) in &secrets {
        assert!(!log.contains(secret.as_str()), "the log contains the {what}:\n{log}");
    }
}
