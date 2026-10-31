//! 设备 token 轮换（docs/spec/hub.md § 5）：`rotate_token` → `token_rotated`，旧 token 的 60 秒宽限，同一设备 1 分钟内最多一次，
//! 吊销之后新旧都失效，临时连接和预览连接不能轮换。
//!
//! 时间不靠真等：`Timing::token_grace` / `token_rotate_min_interval` 可以在测试里改（`Duration::ZERO` = 宽限立刻结束 /
//! 不限频）；按毫秒注入时间的细节（宽限的截止时刻、并发）在 `store::sqlite` 的单元测试里。

mod common;

use std::time::Duration;

use anycon_hub::util::{hash_token, now_ms};
use anycon_protocol::{AuthFailReason, Control, DeviceKind};
use common::*;
use serde_json::Value;

const SHORT: Duration = Duration::from_millis(300);

/// 发 `rotate_token`，要求收到 `token_rotated`，返回新 token。
async fn rotate(conn: &mut FakeDevice) -> String {
    conn.send_control(&Control::RotateToken {}).await;
    match conn.recv_control().await {
        Control::TokenRotated { device_token } => device_token,
        other => panic!("expected token_rotated, got {other:?}"),
    }
}

/// 发 `rotate_token`，要求什么都不回（请求被忽略，连接保持打开）。
async fn rotate_ignored(conn: &mut FakeDevice) {
    conn.send_control(&Control::RotateToken {}).await;
    conn.expect_silence(SHORT).await;
}

/// 用 `token` 认证，返回结果：成功时是设备 id，失败时是 `auth_fail` 的原因。
async fn try_auth(hub: &TestHub, token: &str) -> Result<String, AuthFailReason> {
    let mut d = FakeDevice::open_with(&hub.ws_url(), &[]).await.expect("ws upgrade");
    d.send_auth(Some(token), DeviceKind::Cli, "c").await;
    match d.recv_control().await {
        Control::AuthOk { device_id, .. } => Ok(device_id),
        Control::AuthFail { reason } => Err(reason),
        other => panic!("unexpected {other:?}"),
    }
}

async fn audit_events(hub: &TestHub, event: &str) -> Vec<anycon_hub::store::AuditEntry> {
    hub.handle.store().list_audit(100, None).await.unwrap().into_iter().filter(|e| e.event == event).collect()
}

fn is_token(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[tokio::test]
async fn rotation_issues_a_new_token_and_both_authenticate_during_the_grace_period() {
    let hub = TestHub::start().await;
    let dev = hub.seed_device(DeviceKind::Cli, "c").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Cli, "c").await;

    let new_token = rotate(&mut conn).await;
    assert!(is_token(&new_token), "a 32-byte random token in hex: {new_token}");
    assert_ne!(new_token, dev.token);

    // 库里只有哈希；新 token 认得出是这台设备
    let store = hub.handle.store();
    assert!(store.find_device_by_token_hash(&new_token, now_ms()).await.unwrap().is_none(), "plaintext is not stored");
    let found = store.find_device_by_token_hash(&hash_token(&new_token), now_ms()).await.unwrap().unwrap();
    assert_eq!(found.id, dev.id);

    // 新、旧 token 都能认证（每次认证顶掉上一条连接，所以逐个来）
    assert_eq!(try_auth(&hub, &new_token).await, Ok(dev.id.clone()));
    assert_eq!(try_auth(&hub, &dev.token).await, Ok(dev.id.clone()), "the previous token is valid during the grace");

    // 审计：只有设备 id 等元数据，没有 token
    let rows = audit_events(&hub, "token_rotated").await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].device_id.as_deref(), Some(dev.id.as_str()));
    assert_eq!(rows[0].account_id.as_deref(), Some(hub.admin_account_id().as_str()));
    let dump = format!("{rows:?}");
    for secret in [&dev.token, &new_token, &hash_token(&dev.token), &hash_token(&new_token)] {
        assert!(!dump.contains(secret.as_str()), "audit leaks a token or hash: {dump}");
    }
}

#[tokio::test]
async fn the_previous_token_is_rejected_once_the_grace_is_over_and_the_new_one_keeps_working() {
    // 宽限设为 0：旧 token 在轮换的同一刻就过期，不用真等 60 秒
    let hub = TestHub::start_with(|c| c.timing.token_grace = Duration::ZERO).await;
    let dev = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Daemon, "d").await;
    let new_token = rotate(&mut conn).await;

    assert_eq!(try_auth(&hub, &dev.token).await, Err(AuthFailReason::Invalid));
    assert_eq!(try_auth(&hub, &new_token).await, Ok(dev.id.clone()));
}

#[tokio::test]
async fn a_second_request_inside_the_interval_is_ignored_even_from_a_new_connection() {
    let hub = TestHub::start().await;
    let dev = hub.seed_device(DeviceKind::Web, "w").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Web, "w").await;
    let t2 = rotate(&mut conn).await;

    // 同一条连接上再来一次：没有回复，连接还开着
    rotate_ignored(&mut conn).await;
    // 限频记在设备上，不在连接上：用新 token 重连再请求，同样被忽略
    let mut again = FakeDevice::connect(&hub, Some(&t2), DeviceKind::Web, "w").await;
    rotate_ignored(&mut again).await;

    // 没有发生第二次轮换：旧 token 还在「上一个」的位置上，审计只有一条
    assert_eq!(try_auth(&hub, &dev.token).await, Ok(dev.id.clone()));
    assert_eq!(audit_events(&hub, "token_rotated").await.len(), 1);
}

#[tokio::test]
async fn rotation_is_possible_again_once_the_interval_has_passed() {
    let hub = TestHub::start_with(|c| c.timing.token_rotate_min_interval = Duration::ZERO).await;
    let dev = hub.seed_device(DeviceKind::Cli, "c").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Cli, "c").await;
    let t2 = rotate(&mut conn).await;
    let t3 = rotate(&mut conn).await;
    assert!(is_token(&t3) && t3 != t2 && t3 != dev.token);

    // 只保留「上一个」：t2 还在宽限里，最早的 token 已经彻底失效
    assert_eq!(try_auth(&hub, &t3).await, Ok(dev.id.clone()));
    assert_eq!(try_auth(&hub, &t2).await, Ok(dev.id.clone()));
    assert_eq!(try_auth(&hub, &dev.token).await, Err(AuthFailReason::Invalid));
    assert_eq!(audit_events(&hub, "token_rotated").await.len(), 2);
}

#[tokio::test]
async fn revoking_the_device_invalidates_the_old_and_the_new_token() {
    let hub = TestHub::start().await;
    let dev = hub.seed_device(DeviceKind::Cli, "c").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Cli, "c").await;
    let t2 = rotate(&mut conn).await;

    assert!(hub.handle.state.revoke_device(&dev.id, None).await.unwrap());
    assert_eq!(conn.expect_close().await, Some(4004));
    // 宽限内的旧 token 和新 token 都认得出这台设备，但它已经被吊销
    assert_eq!(try_auth(&hub, &dev.token).await, Err(AuthFailReason::Revoked));
    assert_eq!(try_auth(&hub, &t2).await, Err(AuthFailReason::Revoked));

    // 吊销之后库里也不再接受轮换
    let outcome =
        hub.handle.store().rotate_device_token(&dev.id, &hash_token("t3"), now_ms(), 60_000, 0).await.unwrap();
    assert_eq!(outcome, anycon_hub::store::RotateOutcome::Unavailable);
    assert_eq!(try_auth(&hub, "t3").await, Err(AuthFailReason::Invalid));
}

#[tokio::test]
async fn temporary_connections_cannot_rotate() {
    let hub = TestHub::start().await;
    // 没有身份的临时连接（配对用）：请求被忽略，连接不受影响；库里没有任何设备被动过
    let seeded = hub.seed_device(DeviceKind::Daemon, "d").await;
    for kind in [DeviceKind::Web, DeviceKind::Cli, DeviceKind::Daemon] {
        let mut temp = FakeDevice::connect(&hub, None, kind, "t").await;
        rotate_ignored(&mut temp).await;
    }
    assert!(audit_events(&hub, "token_rotated").await.is_empty());
    assert_eq!(try_auth(&hub, &seeded.token).await, Ok(seeded.id.clone()));
}

#[tokio::test]
async fn preview_connections_cannot_rotate() {
    let hub = TestHub::start_preview().await;
    let node = hub.seed_device(DeviceKind::Daemon, "node").await;
    let web = hub.seed_device(DeviceKind::Web, "web").await;
    hub.seed_paired(&node, &web).await;
    let label = new_label();
    let token = hub.preview_token(&web, &node, &label).await;
    let mut preview = FakeDevice::connect_preview(&hub, &label, &token).await;

    rotate_ignored(&mut preview).await;
    assert!(audit_events(&hub, "token_rotated").await.is_empty());
    for dev in [&node, &web] {
        let d = hub.handle.store().find_device_by_token_hash(&hash_token(&dev.token), now_ms()).await.unwrap();
        assert_eq!(d.map(|d| d.id), Some(dev.id.clone()), "the device tokens are untouched");
    }
}

#[tokio::test]
async fn the_http_endpoints_that_take_a_device_token_accept_the_new_and_the_previous_token() {
    let hub = TestHub::start().await;
    let dev = hub.seed_device(DeviceKind::Cli, "c").await;
    let mut conn = FakeDevice::connect(&hub, Some(&dev.token), DeviceKind::Cli, "c").await;
    let t2 = rotate(&mut conn).await;

    let list = |token: String| {
        let (http, url) = (hub.http.clone(), hub.url("/api/devices"));
        async move { http.get(url).header("authorization", format!("Bearer {token}")).send().await.unwrap() }
    };
    for token in [&t2, &dev.token] {
        let res = list(token.clone()).await;
        assert_eq!(res.status(), 200, "Bearer token is accepted");
        let body: Value = res.json().await.unwrap();
        assert!(body.to_string().contains(&dev.id));
    }

    // 宽限为 0 的 hub：旧 token 在 HTTP 上同样立刻失效
    let strict = TestHub::start_with(|c| c.timing.token_grace = Duration::ZERO).await;
    let dev = strict.seed_device(DeviceKind::Cli, "c").await;
    let mut conn = FakeDevice::connect(&strict, Some(&dev.token), DeviceKind::Cli, "c").await;
    let t2 = rotate(&mut conn).await;
    let get = |token: String| {
        let (http, url) = (strict.http.clone(), strict.url("/api/devices"));
        async move { http.get(url).header("authorization", format!("Bearer {token}")).send().await.unwrap().status() }
    };
    assert_eq!(get(t2).await, 200);
    assert_eq!(get(dev.token.clone()).await, 401);
}

#[tokio::test]
async fn a_peer_keeps_its_pairings_across_a_rotation() {
    // 轮换只换 token，不动设备 id 和配对：对端照常在线通知、转发
    let hub = TestHub::start().await;
    let daemon = hub.seed_device(DeviceKind::Daemon, "d").await;
    let web = hub.seed_device(DeviceKind::Web, "w").await;
    hub.seed_paired(&daemon, &web).await;
    let mut d = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "d").await;
    let mut w = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "w").await;
    assert!(matches!(d.recv_control().await, Control::PeerOnline { device } if device == web.id));

    let t2 = rotate(&mut w).await;
    drop(w);
    assert!(matches!(d.recv_control().await, Control::PeerOffline { device } if device == web.id));
    let w = FakeDevice::connect(&hub, Some(&t2), DeviceKind::Web, "w").await;
    assert_eq!(w.id, web.id);
    assert_eq!(w.peers_online, vec![daemon.id.clone()]);
    assert!(matches!(d.recv_control().await, Control::PeerOnline { device } if device == web.id));
}
