//! `multi` 配额和删账号测试共用的小工具，建在 `tests/common` 之上：直接用库造账号和浏览器会话（不走邮箱验证码，
//! 免得撞上发码限速），带配额的认证，以及不看握手内容的配对 / 接入收尾。
#![allow(dead_code)]

use std::time::Duration;

use anycon_hub::config::QuotaLimits;
use anycon_hub::store::CreateAccountOutcome;
use anycon_hub::util::{hash_token, new_secret, now_ms};
use anycon_protocol::{AuthFailReason, Control, DeviceKind, Quota};

use crate::common::*;

pub const SHORT: Duration = Duration::from_millis(300);

pub fn limits(devices: u32, daemons: u32, connections: u32, daily_pairings: u32) -> QuotaLimits {
    QuotaLimits { devices, daemons, connections, daily_pairings }
}

/// 起一个 `multi` hub，配额取给定值。
pub async fn multi_hub(q: QuotaLimits) -> TestHub {
    multi_hub_with(q, |_| {}).await
}

pub async fn multi_hub_with(q: QuotaLimits, tweak: impl FnOnce(&mut anycon_hub::Config)) -> TestHub {
    let (hub, _mailer) = TestHub::start_multi_with(move |c| {
        c.enable_multi(q);
        tweak(c);
    })
    .await;
    hub
}

/// 直接在库里建一个账号（可带邮箱）并给它一个新登录会话。返回（账号 id，cookie）。
pub async fn account(hub: &TestHub, name: &str, email: Option<&str>) -> (String, String) {
    let outcome = hub.handle.store().create_account_with_email(name, email, now_ms()).await.unwrap();
    let CreateAccountOutcome::Created(acc) = outcome else { panic!("the email {email:?} is already taken") };
    let cookie = session_created_at(hub, &acc.id, now_ms()).await;
    // 账号按创建时间排序：隔开一毫秒以上，测试里的先后顺序才确定
    tokio::time::sleep(Duration::from_millis(2)).await;
    (acc.id, cookie)
}

/// 给账号造一个创建时间为 `created_at_ms` 的浏览器会话（测「最近认证」的窗口），返回 cookie。
pub async fn session_created_at(hub: &TestHub, account_id: &str, created_at_ms: u64) -> String {
    let raw = new_secret();
    hub.handle
        .store()
        .create_web_session(
            account_id,
            &hash_token(&raw),
            "test-agent",
            "127.0.0.0/24",
            created_at_ms,
            now_ms() + 3_600_000,
        )
        .await
        .unwrap();
    format!("anycon_session={raw}")
}

/// 带登录 cookie 的临时连接（浏览器，或 CLI 临时登录拿到的会话）。
pub async fn temp_with_cookie(hub: &TestHub, cookie: &str, kind: DeviceKind, name: &str) -> FakeDevice {
    FakeDevice::connect_with(hub, None, kind, name, &[("cookie", cookie)]).await
}

/// 认证的结果：成功（连接和 `auth_ok.quota`），或被拒绝（原因和关闭码）。
pub type Connected = Result<(FakeDevice, Option<Quota>), (AuthFailReason, Option<u16>)>;

pub async fn try_connect(hub: &TestHub, token: Option<&str>, kind: DeviceKind, name: &str) -> Connected {
    let mut raw = FakeDevice::open_with(&hub.ws_url(), &[]).await.expect("ws upgrade");
    raw.send_auth(token, kind, name).await;
    match raw.recv_control().await {
        Control::AuthOk { device_id, peers_online, quota, .. } => {
            raw.id = device_id;
            raw.peers_online = peers_online;
            Ok((raw, quota))
        }
        Control::AuthFail { reason } => {
            let code = raw.expect_close().await;
            Err((reason, code))
        }
        other => panic!("expected auth_ok or auth_fail, got {other:?}"),
    }
}

/// 认证并要求成功，连带 `auth_ok.quota`。
pub async fn connect_with_quota(
    hub: &TestHub,
    token: Option<&str>,
    kind: DeviceKind,
    name: &str,
) -> (FakeDevice, Option<Quota>) {
    match try_connect(hub, token, kind, name).await {
        Ok(connected) => connected,
        Err(refused) => panic!("expected auth_ok, got auth_fail {refused:?}"),
    }
}

/// 认证并要求被配额拒绝：`auth_fail { quota }`，然后以 1008 关闭。
pub async fn connect_refused_by_quota(hub: &TestHub, token: &str, kind: DeviceKind, name: &str) {
    match try_connect(hub, Some(token), kind, name).await {
        Err((AuthFailReason::Quota, Some(1008))) => {}
        Err(other) => panic!("expected auth_fail quota + 1008, got {other:?}"),
        Ok(_) => panic!("expected auth_fail quota, but the connection was accepted"),
    }
}

/// 一份用量。
pub fn quota(devices: u32, daemons: u32, connections: u32, q: &QuotaLimits) -> Quota {
    Quota {
        devices,
        devices_max: Some(q.devices),
        daemons,
        daemons_max: Some(q.daemons),
        connections,
        connections_max: Some(q.connections),
    }
}

/// 一批控制消息里的 `quota_update`。
pub fn quota_updates(msgs: &[Control]) -> Vec<Quota> {
    msgs.iter()
        .filter_map(|m| match m {
            Control::QuotaUpdate { quota } => Some(quota.clone()),
            _ => None,
        })
        .collect()
}

/// 下一条控制消息必须是 `quota_update`。
pub async fn expect_quota_update(d: &mut FakeDevice) -> Quota {
    match d.recv_control().await {
        Control::QuotaUpdate { quota } => quota,
        other => panic!("expected quota_update, got {other:?}"),
    }
}

/// 短码配对走到「已撮合」：host 登记、joiner 加入。返回 `(host 看到的 joiner id, joiner 看到的 host id)`。
pub async fn pair_matched(host: &mut FakeDevice, joiner: &mut FakeDevice) -> (String, String) {
    let (_code, match_id) = new_code();
    host.pair_begin(&match_id).await;
    joiner.send_control(&Control::PairJoin { match_id }).await;
    let host_sees = match host.recv_control().await {
        Control::PairMatched { peer, .. } => peer,
        other => panic!("host: expected pair_matched, got {other:?}"),
    };
    let joiner_sees = match joiner.recv_control().await {
        Control::PairMatched { peer, .. } => peer,
        other => panic!("joiner: expected pair_matched, got {other:?}"),
    };
    (host_sees, joiner_sees)
}

/// 配对撮合 + 两边确认（hub 不看握手内容，所以不用做 Noise）。收尾的结果由调用方接着读。
pub async fn pair_and_confirm(host: &mut FakeDevice, joiner: &mut FakeDevice) {
    let (host_sees, joiner_sees) = pair_matched(host, joiner).await;
    joiner.send_control(&Control::PairConfirm { peer: joiner_sees }).await;
    host.send_control(&Control::PairConfirm { peer: host_sees }).await;
}

/// 发起账号内接入并两边确认。返回 `enroll_id`；收尾的结果由调用方接着读。
pub async fn enroll_and_confirm(initiator: &mut FakeDevice, node: &mut FakeDevice, node_id: &str) -> String {
    initiator.send_control(&Control::EnrollBegin { node: node_id.to_string() }).await;
    let enroll_id = match initiator.recv_control().await {
        Control::EnrollStarted { enroll_id, .. } => enroll_id,
        other => panic!("expected enroll_started, got {other:?}"),
    };
    let initiator_sees = match node.recv_control().await {
        Control::EnrollRequest { peer, .. } => peer,
        other => panic!("expected enroll_request, got {other:?}"),
    };
    initiator.send_control(&Control::PairConfirm { peer: node_id.to_string() }).await;
    node.send_control(&Control::PairConfirm { peer: initiator_sees }).await;
    enroll_id
}

pub async fn json_of(res: reqwest::Response) -> serde_json::Value {
    res.json().await.expect("json body")
}

pub async fn error_code(res: reqwest::Response) -> String {
    json_of(res).await["error"]["code"].as_str().unwrap_or("").to_string()
}

/// 一直读到连接被关闭为止（中间的信封跳过），返回关闭码。
pub async fn closed_with(d: &mut FakeDevice) -> Option<u16> {
    loop {
        match d.recv().await {
            Recv::Closed(code) => return code,
            Recv::Env(_) => continue,
        }
    }
}

/// 库里的审计行（最新的在前，最多 500 条）。
pub async fn audit_rows(hub: &TestHub) -> Vec<anycon_hub::store::AuditEntry> {
    hub.handle.store().list_audit(500, None).await.unwrap()
}

/// 响应必须是这个状态码，JSON 错误体里的 `error.code` 必须是 `code`。
pub async fn assert_failure(res: reqwest::Response, status: u16, code: &str) {
    let got = res.status().as_u16();
    let body = res.text().await.unwrap_or_default();
    assert_eq!(got, status, "status; body: {body}");
    let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    assert_eq!(body["error"]["code"], code, "error code; body: {body}");
}
