//! `multi` 的账号配额（docs/spec/hub.md § 7.1）：设备 / daemon 数、在线连接数、每日配对与接入次数，
//! `auth_ok.quota`、`quota_update` 和 `GET /api/quota`；以及 `single` 里这些都不存在。

mod common;
mod multi_support;

use anycon_protocol::{Control, DeviceKind, EnrollFailReason, PairFailReason};
use common::*;
use multi_support::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// auth_ok.quota
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_ok_carries_the_quota_for_permanent_connections_in_multi() {
    let q = limits(5, 2, 3, 20);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;

    let (mut n, got) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    assert_eq!(got, Some(quota(2, 1, 1, &q)), "devices, daemons and the connection that is just coming up");
    let (mut w, got) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    assert_eq!(got, Some(quota(2, 1, 2, &q)));
    // 已经在线的那条在 `quota_update` 里看到第二条连接
    assert_eq!(expect_quota_update(&mut n).await, quota(2, 1, 2, &q));
    w.expect_silence(SHORT).await; // 刚认证的这条已经在 auth_ok 里拿到了，不再收一遍

    // 临时连接（没有设备身份，哪怕带着登录 cookie）：没有 quota，也不算连接，不触发 quota_update
    let (_t, got) = connect_with_quota(&hub, None, DeviceKind::Web, "pairing").await;
    assert_eq!(got, None);
    let mut t_cookie = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "pairing with cookie").await;
    t_cookie.expect_silence(SHORT / 3).await;
    n.expect_silence(SHORT).await;
    w.expect_silence(SHORT / 3).await;
}

#[tokio::test]
async fn single_mode_has_no_quota_no_limits_and_no_quota_updates() {
    let hub = TestHub::start().await;
    // 比 multi 的默认值（10 台设备 / 10 条连接）还多：single 不设限
    let mut devices = Vec::new();
    for i in 0..12 {
        devices.push(hub.seed_device(DeviceKind::Web, &format!("web-{i}")).await);
    }
    let (mut first, got) = connect_with_quota(&hub, Some(&devices[0].token), DeviceKind::Web, "web-0").await;
    assert_eq!(got, None, "auth_ok.quota is multi-only");
    let mut rest = Vec::new();
    for (i, d) in devices.iter().enumerate().skip(1) {
        let (conn, got) = connect_with_quota(&hub, Some(&d.token), DeviceKind::Web, &format!("web-{i}")).await;
        assert_eq!(got, None);
        rest.push(conn);
    }
    first.expect_silence(SHORT).await; // 连接上线没有 quota_update
    drop(rest);
    first.expect_silence(SHORT).await; // 下线也没有
}

// ---------------------------------------------------------------------------
// quota_update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn quota_update_follows_connections_going_up_and_down() {
    let q = limits(10, 3, 10, 20);
    let hub = multi_hub(q).await;
    let (acc, _) = account(&hub, "alice", None).await;
    let (d1, d2, d3) = (
        hub.seed_device_in(&acc, DeviceKind::Web, "d1").await,
        hub.seed_device_in(&acc, DeviceKind::Web, "d2").await,
        hub.seed_device_in(&acc, DeviceKind::Cli, "d3").await,
    );
    let (mut c1, _) = connect_with_quota(&hub, Some(&d1.token), DeviceKind::Web, "d1").await;
    let (c2, _) = connect_with_quota(&hub, Some(&d2.token), DeviceKind::Web, "d2").await;
    assert_eq!(expect_quota_update(&mut c1).await, quota(3, 0, 2, &q));
    let (c3, _) = connect_with_quota(&hub, Some(&d3.token), DeviceKind::Cli, "d3").await;
    assert_eq!(expect_quota_update(&mut c1).await, quota(3, 0, 3, &q));

    drop(c2);
    assert_eq!(expect_quota_update(&mut c1).await, quota(3, 0, 2, &q));
    drop(c3);
    assert_eq!(expect_quota_update(&mut c1).await, quota(3, 0, 1, &q));
    c1.expect_silence(SHORT).await;

    // 同一设备的新连接顶掉旧的：连接数没变，不发 quota_update（旧连接收到 4001）
    let (mut again, got) = connect_with_quota(&hub, Some(&d1.token), DeviceKind::Web, "d1 again").await;
    assert_eq!(got, Some(quota(3, 0, 1, &q)));
    assert_eq!(c1.expect_close().await, Some(4001));
    again.expect_silence(SHORT).await;

    // 别的账号的连接变化，和这个账号无关
    let (other, _) = account(&hub, "bob", None).await;
    let theirs = hub.seed_device_in(&other, DeviceKind::Web, "theirs").await;
    let (_theirs, _) = connect_with_quota(&hub, Some(&theirs.token), DeviceKind::Web, "theirs").await;
    again.expect_silence(SHORT).await;
}

#[tokio::test]
async fn quota_update_follows_revocation_through_the_api_and_through_control_revoke() {
    let q = limits(10, 3, 10, 20);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let (a, b, c, d) = (
        hub.seed_device_in(&acc, DeviceKind::Web, "a").await,
        hub.seed_device_in(&acc, DeviceKind::Web, "b").await,
        hub.seed_device_in(&acc, DeviceKind::Daemon, "c").await,
        hub.seed_device_in(&acc, DeviceKind::Web, "d").await,
    );
    hub.seed_paired(&a, &c).await;
    let (mut ca, _) = connect_with_quota(&hub, Some(&a.token), DeviceKind::Web, "a").await;
    let (mut cb, _) = connect_with_quota(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    let (mut cc, _) = connect_with_quota(&hub, Some(&c.token), DeviceKind::Daemon, "c").await;
    let (cd, _) = connect_with_quota(&hub, Some(&d.token), DeviceKind::Web, "d").await;
    for dev in [&mut ca, &mut cb, &mut cc] {
        dev.drain_controls(SHORT).await;
    }
    assert_eq!(hub.handle.store().count_devices(&acc).await.unwrap(), (4, 1));

    // API 吊销在线的 b：其余三条各收到一条 quota_update（4 → 3 台设备，3 条连接），b 自己 4004
    let res = hub.send(reqwest::Method::DELETE, &format!("/api/devices/{}", b.id), &cookie, json!(null)).await;
    assert_eq!(res.status(), 204);
    assert_eq!(cb.expect_close().await, Some(4004));
    for dev in [&mut ca, &mut cc] {
        assert_eq!(dev.drain_controls(SHORT).await, [Control::QuotaUpdate { quota: quota(3, 1, 3, &q) }]);
    }

    // 设备通过 control.revoke 吊销配对的对端 c（daemon，在线）：a 先收到 peer_offline，再是 quota_update
    ca.send_control(&Control::Revoke { device: c.id.clone() }).await;
    assert_eq!(cc.expect_close().await, Some(4004));
    let got = ca.drain_controls(SHORT).await;
    assert_eq!(
        got,
        [Control::PeerOffline { device: c.id.clone() }, Control::QuotaUpdate { quota: quota(2, 0, 2, &q) }]
    );
    drop(cd);
}

#[tokio::test]
async fn pairing_completion_pushes_a_quota_update_with_the_new_device() {
    let q = limits(5, 2, 10, 20);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (mut w, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    n.drain_controls(SHORT).await;

    // 新的浏览器（带登录 cookie 的临时连接）和 daemon 配对：创建第 3 台设备
    let mut joiner = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "new browser").await;
    pair_and_confirm(&mut n, &mut joiner).await;
    let host_msgs = n.drain_controls(SHORT).await;
    let joiner_msgs = joiner.drain_controls(SHORT).await;
    assert!(matches!(&host_msgs[0], Control::PairDone { device_id: None, .. }));
    assert!(matches!(&joiner_msgs[0], Control::PairDone { device_id: Some(_), .. }));
    // 升级后的连接也是永久连接：3 台设备、3 条连接；已经在线的、新升级的都收到
    let want = quota(3, 1, 3, &q);
    assert_eq!(quota_updates(&host_msgs), vec![want.clone()]);
    assert_eq!(quota_updates(&joiner_msgs), vec![want.clone()]);
    assert_eq!(w.drain_controls(SHORT).await, [Control::QuotaUpdate { quota: want }]);
}

// ---------------------------------------------------------------------------
// 连接数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_connection_limit_refuses_with_auth_fail_quota_and_1008_and_nothing_is_registered() {
    let q = limits(10, 3, 2, 20);
    let hub = multi_hub(q).await;
    let (acc, _) = account(&hub, "alice", None).await;
    let d: Vec<Seeded> = seed_web_devices(&hub, &acc, 4).await;
    let (mut c0, _) = connect_with_quota(&hub, Some(&d[0].token), DeviceKind::Web, "d0").await;
    let (mut c1, _) = connect_with_quota(&hub, Some(&d[1].token), DeviceKind::Web, "d1").await;
    assert_eq!(expect_quota_update(&mut c0).await, quota(4, 0, 2, &q));

    // 第 3 条被拒绝；没登记（不在线），已经在线的也没有收到任何通知
    connect_refused_by_quota(&hub, &d[2].token, DeviceKind::Web, "d2").await;
    assert!(!hub.handle.state.registry.is_online(&d[2].id));
    assert_eq!(hub.handle.state.registry.permanent_count_of(&acc), 2);
    c0.expect_silence(SHORT).await;

    // 临时连接不受限：配对中的新设备还没有身份
    let (_temp, got) = connect_with_quota(&hub, None, DeviceKind::Web, "pairing").await;
    assert_eq!(got, None);

    // 同一设备的新连接顶掉自己的旧连接，不算新增：满了也进得来
    let (c1b, got) = connect_with_quota(&hub, Some(&d[1].token), DeviceKind::Web, "d1 again").await;
    assert_eq!(got, Some(quota(4, 0, 2, &q)));
    assert_eq!(c1.expect_close().await, Some(4001));
    assert_eq!(hub.handle.state.registry.permanent_count_of(&acc), 2);

    // 一条下线，空出位置，之前被拒绝的设备就能连上了
    drop(c1b);
    assert_eq!(expect_quota_update(&mut c0).await, quota(4, 0, 1, &q));
    let (_c2, got) = connect_with_quota(&hub, Some(&d[2].token), DeviceKind::Web, "d2 retry").await;
    assert_eq!(got, Some(quota(4, 0, 2, &q)));

    // 别的账号有自己的额度
    let (other, _) = account(&hub, "bob", None).await;
    let theirs = hub.seed_device_in(&other, DeviceKind::Web, "theirs").await;
    let (_t, got) = connect_with_quota(&hub, Some(&theirs.token), DeviceKind::Web, "theirs").await;
    assert_eq!(got, Some(quota(1, 0, 1, &q)));
}

async fn seed_web_devices(hub: &TestHub, account: &str, n: usize) -> Vec<Seeded> {
    let mut out = Vec::new();
    for i in 0..n {
        out.push(hub.seed_device_in(account, DeviceKind::Web, &format!("d{i}")).await);
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_authentications_never_exceed_the_connection_limit() {
    let q = limits(50, 50, 3, 20);
    let hub = multi_hub(q).await;
    let (acc, _) = account(&hub, "alice", None).await;
    let devices = seed_web_devices(&hub, &acc, 14).await;

    let attempts = devices.iter().map(|d| try_connect(&hub, Some(&d.token), DeviceKind::Web, &d.id));
    let results = futures::future::join_all(attempts).await;
    let accepted: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(accepted.len(), 3, "exactly the limit gets in, however the 14 authentications interleave");
    for refused in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(refused, &(anycon_protocol::AuthFailReason::Quota, Some(1008)));
    }
    assert_eq!(hub.handle.state.registry.permanent_count_of(&acc), 3);
}

// ---------------------------------------------------------------------------
// 设备 / daemon 数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pairing_that_would_exceed_the_device_limit_fails_for_both_sides_and_writes_nothing() {
    let q = limits(3, 3, 10, 50);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (mut w, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    n.drain_controls(SHORT).await; // w 上线的 quota_update
    let store = hub.handle.store();

    // 第 3 台设备：刚好到上限，成功
    let mut third = temp_with_cookie(&hub, &cookie, DeviceKind::Cli, "third").await;
    pair_and_confirm(&mut n, &mut third).await;
    assert!(matches!(third.recv_control().await, Control::PairDone { device_id: Some(_), .. }));
    assert_eq!(store.count_devices(&acc).await.unwrap(), (3, 1));
    n.drain_controls(SHORT).await;
    w.drain_controls(SHORT).await;
    let paired_before = store.peers_of(&node.id).await.unwrap().len();

    // 第 4 台：双方都收到 pair_failed { quota }，什么都没写，也没有 quota_update
    let mut fourth = temp_with_cookie(&hub, &cookie, DeviceKind::Cli, "fourth").await;
    pair_and_confirm(&mut n, &mut fourth).await;
    assert_eq!(n.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(fourth.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(store.count_devices(&acc).await.unwrap(), (3, 1));
    assert_eq!(store.peers_of(&node.id).await.unwrap().len(), paired_before);
    assert!(store.list_devices(&acc).await.unwrap().iter().all(|d| d.name != "fourth"));
    assert_eq!(hub.handle.state.registry.permanent_count_of(&acc), 3, "the failed joiner stays a temporary connection");
    w.expect_silence(SHORT).await;
    n.expect_silence(SHORT / 3).await;

    // 把已有的两台设备重新配对不新建设备，不检查配额
    pair_and_confirm(&mut n, &mut w).await;
    assert!(matches!(n.recv_control().await, Control::PairDone { device_id: None, device_token: None, .. }));
    assert!(matches!(w.recv_control().await, Control::PairDone { device_id: None, device_token: None, .. }));
    assert!(store.is_paired(&node.id, &web.id).await.unwrap());
    assert_eq!(store.count_devices(&acc).await.unwrap(), (3, 1));
}

#[tokio::test]
async fn the_daemon_limit_applies_to_a_new_daemon_and_not_to_other_kinds() {
    let q = limits(10, 1, 10, 50);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (mut w, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    n.drain_controls(SHORT).await; // w 上线的 quota_update
    let store = hub.handle.store();

    // 一台全新的 daemon（临时连接，没有账号）经已配对的设备配对进来：第二台 daemon 超了
    let mut new_daemon = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "second daemon").await;
    pair_and_confirm(&mut new_daemon, &mut w).await;
    assert_eq!(new_daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(w.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(store.count_devices(&acc).await.unwrap(), (2, 1));

    // 同样是新设备，不是 daemon 就不受 daemon 上限影响
    let mut cli = temp_with_cookie(&hub, &cookie, DeviceKind::Cli, "cli").await;
    pair_and_confirm(&mut n, &mut cli).await;
    assert!(matches!(cli.recv_control().await, Control::PairDone { device_id: Some(_), .. }));
    assert_eq!(store.count_devices(&acc).await.unwrap(), (3, 1));

    // 吊销那台 daemon 以后名额空出来，新的 daemon 就能配对进来
    let res = hub.send(reqwest::Method::DELETE, &format!("/api/devices/{}", node.id), &cookie, json!(null)).await;
    assert_eq!(res.status(), 204);
    let mut retry = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "second daemon").await;
    let mut browser = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "browser").await;
    pair_and_confirm(&mut retry, &mut browser).await;
    assert!(matches!(retry.recv_control().await, Control::PairDone { device_id: Some(_), .. }));
    assert_eq!(store.count_devices(&acc).await.unwrap(), (4, 1));
}

#[tokio::test]
async fn a_pairing_that_creates_two_devices_is_all_or_nothing() {
    let q = limits(3, 3, 10, 50);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let _ = hub.seed_device_in(&acc, DeviceKind::Web, "a").await;
    let _ = hub.seed_device_in(&acc, DeviceKind::Web, "b").await;
    // 一台新 daemon + 一个带 cookie 的新浏览器：要建两台，只剩一个名额，一台都不能建
    let mut daemon = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "new daemon").await;
    let mut browser = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "new browser").await;
    pair_and_confirm(&mut daemon, &mut browser).await;
    assert_eq!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(browser.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    assert_eq!(hub.handle.store().count_devices(&acc).await.unwrap(), (2, 0));
    assert_eq!(hub.handle.state.registry.permanent_count_of(&acc), 0);
}

#[tokio::test]
async fn an_enrollment_that_would_exceed_the_device_limit_fails_with_enroll_failed_quota() {
    let q = limits(2, 2, 10, 50);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let _web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let mut initiator = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "new browser").await;

    let id = enroll_and_confirm(&mut initiator, &mut n, &node.id).await;
    let want = Control::EnrollFailed { enroll_id: Some(id), reason: EnrollFailReason::Quota };
    assert_eq!(initiator.recv_control().await, want);
    assert_eq!(n.recv_control().await, want);
    assert_eq!(hub.handle.store().count_devices(&acc).await.unwrap(), (2, 1));
    assert!(hub.handle.store().peers_of(&node.id).await.unwrap().is_empty());

    // 发起方已经是本账号的设备时接入不新建设备，不检查配额
    let web = hub.seed_device_in(&acc, DeviceKind::Cli, "cli").await;
    let (mut existing, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Cli, "cli").await;
    n.drain_controls(SHORT).await;
    enroll_and_confirm(&mut existing, &mut n, &node.id).await;
    assert!(matches!(existing.recv_control().await, Control::PairDone { device_id: None, device_token: None, .. }));
    assert!(matches!(n.recv_control().await, Control::PairDone { device_id: None, device_token: None, .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_enrollments_never_exceed_the_device_limit() {
    // 3 台 daemon，每台 3 个并发的接入会话 = 9 个新设备争着收尾；设备上限 7 = 3 台 daemon + 4 个名额
    let q = limits(7, 10, 20, 100);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;

    // 先把 9 个会话都建好，daemon 一次性确认完；发起方这一边的确认留到最后同时发出，让收尾尽量同时发生
    let mut daemons = Vec::new();
    let mut sessions = Vec::new();
    for i in 0..3 {
        let node = hub.seed_device_in(&acc, DeviceKind::Daemon, &format!("node-{i}")).await;
        let (mut conn, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
        for k in 0..3 {
            let mut init = temp_with_cookie(&hub, &cookie, DeviceKind::Web, &format!("browser-{i}-{k}")).await;
            init.send_control(&Control::EnrollBegin { node: node.id.clone() }).await;
            let enroll_id = match init.recv_control().await {
                Control::EnrollStarted { enroll_id, .. } => enroll_id,
                other => panic!("expected enroll_started, got {other:?}"),
            };
            let initiator_id = loop {
                match conn.recv_control().await {
                    Control::EnrollRequest { peer, .. } => break peer,
                    Control::QuotaUpdate { .. } | Control::PeerOnline { .. } => continue,
                    other => panic!("expected enroll_request, got {other:?}"),
                }
            };
            conn.send_control(&Control::PairConfirm { peer: initiator_id }).await;
            sessions.push((init, node.id.clone(), enroll_id));
        }
        daemons.push(conn);
    }
    let tasks: Vec<_> = sessions
        .into_iter()
        .map(|(mut init, node_id, enroll_id)| {
            tokio::spawn(async move {
                init.send_control(&Control::PairConfirm { peer: node_id }).await;
                let result = init.recv_control().await;
                let succeeded = match result {
                    Control::PairDone { device_id: Some(_), .. } => true,
                    Control::EnrollFailed { enroll_id: Some(id), reason: EnrollFailReason::Quota } => {
                        assert_eq!(id, enroll_id);
                        false
                    }
                    other => panic!("unexpected enrollment result {other:?}"),
                };
                (succeeded, init)
            })
        })
        .collect();
    let mut succeeded = 0;
    let mut initiators = Vec::new();
    for t in tasks {
        let (ok, init) = t.await.unwrap();
        succeeded += usize::from(ok);
        initiators.push(init);
    }
    let (devices, daemons_count) = hub.handle.store().count_devices(&acc).await.unwrap();
    assert_eq!(succeeded, 4, "9 contenders for 4 free slots");
    assert_eq!((devices, daemons_count), (7, 3), "and the limit holds exactly");
    drop(daemons);
}

// ---------------------------------------------------------------------------
// 每日配对与接入次数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pair_begin_pair_join_and_enroll_begin_share_one_daily_budget_per_known_account() {
    let q = limits(10, 5, 10, 4);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    let (_, other_cookie) = account(&hub, "bob", None).await;
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let mut browser = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "browser").await;
    let ghost = anycon_hub::util::new_id();
    let never_joined = new_code().1;

    // 1) pair_begin（正式设备）2) pair_join（带 cookie 的临时连接）3) enroll_begin（同一账号）
    n.pair_begin(&new_code().1).await;
    browser.send_control(&Control::PairJoin { match_id: never_joined.clone() }).await;
    assert_eq!(browser.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });
    browser.send_control(&Control::EnrollBegin { node: ghost.clone() }).await;
    assert!(matches!(
        browser.recv_control().await,
        Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::NotFound }
    ));
    // 4) 第四次还在额度内
    n.pair_begin(&new_code().1).await;

    // 第五次起，不管是哪一种、哪条连接，都是 quota
    n.send_control(&Control::PairBegin { match_id: new_code().1 }).await;
    assert_eq!(n.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    browser.send_control(&Control::PairJoin { match_id: never_joined }).await;
    assert_eq!(browser.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    browser.send_control(&Control::EnrollBegin { node: ghost.clone() }).await;
    assert!(matches!(
        browser.recv_control().await,
        Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::Quota }
    ));
    // 被拒绝的 pair_begin 没有登记：它的 match_id 谁也 join 不到
    let (_code, refused_id) = new_code();
    n.send_control(&Control::PairBegin { match_id: refused_id.clone() }).await;
    assert_eq!(n.recv_control().await, Control::PairFailed { reason: PairFailReason::Quota });
    let mut anonymous = FakeDevice::connect(&hub, None, DeviceKind::Cli, "anonymous").await;
    anonymous.send_control(&Control::PairJoin { match_id: refused_id }).await;
    assert_eq!(anonymous.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });

    // 用量
    let view = json_of(hub.get("/api/quota", &cookie).await).await;
    assert_eq!((view["daily_pairings"].as_u64(), view["daily_pairings_max"].as_u64()), (Some(4), Some(4)));

    // 别的账号各算各的
    let mut theirs = temp_with_cookie(&hub, &other_cookie, DeviceKind::Web, "theirs").await;
    theirs.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
    assert_eq!(theirs.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });
    assert_eq!(json_of(hub.get("/api/quota", &other_cookie).await).await["daily_pairings"], 1);
}

#[tokio::test]
async fn connections_without_an_account_are_not_counted_toward_the_daily_budget() {
    let q = limits(10, 5, 10, 1);
    let hub = multi_hub(q).await;
    // 没有账号的临时连接：pair_join 不记（受每 IP 限速管），没有账号的临时 daemon 发 pair_begin 也不记
    let mut anonymous = FakeDevice::connect(&hub, None, DeviceKind::Cli, "anonymous").await;
    for _ in 0..4 {
        anonymous.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
        assert_eq!(anonymous.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });
    }
    let mut daemon = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "fresh daemon").await;
    daemon.pair_begin(&new_code().1).await;
    daemon.pair_begin(&new_code().1).await;
}

// ---------------------------------------------------------------------------
// GET /api/quota
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_api_quota_reports_usage_and_limits() {
    let q = limits(5, 2, 7, 9);
    let hub = multi_hub(q).await;
    let (acc, cookie) = account(&hub, "alice", None).await;
    assert_eq!(hub.http.get(hub.url("/api/quota")).send().await.unwrap().status(), 401);
    assert_eq!(
        json_of(hub.get("/api/quota", &cookie).await).await,
        json!({
            "devices": 0, "devices_max": 5, "daemons": 0, "daemons_max": 2,
            "connections": 0, "connections_max": 7, "daily_pairings": 0, "daily_pairings_max": 9,
        })
    );
    let node = hub.seed_device_in(&acc, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&acc, DeviceKind::Web, "web").await;
    let (_n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (_w, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    let mut joiner = temp_with_cookie(&hub, &cookie, DeviceKind::Web, "joiner").await;
    joiner.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
    assert_eq!(joiner.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });
    assert_eq!(
        json_of(hub.get("/api/quota", &cookie).await).await,
        json!({
            "devices": 2, "devices_max": 5, "daemons": 1, "daemons_max": 2,
            "connections": 2, "connections_max": 7, "daily_pairings": 1, "daily_pairings_max": 9,
        })
    );
    // 吊销之后用量下降
    let res = hub.send(reqwest::Method::DELETE, &format!("/api/devices/{}", web.id), &cookie, json!(null)).await;
    assert_eq!(res.status(), 204);
    let view = json_of(hub.get("/api/quota", &cookie).await).await;
    assert_eq!((view["devices"].as_u64(), view["connections"].as_u64()), (Some(1), Some(1)));
}

#[tokio::test]
async fn get_api_quota_in_single_mode_has_real_counts_and_no_limits() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    assert_eq!(hub.http.get(hub.url("/api/quota")).send().await.unwrap().status(), 401);
    let node = hub.seed_device(DeviceKind::Daemon, "node").await;
    let cli = hub.seed_device(DeviceKind::Cli, "cli").await;
    let (_n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (_c, _) = connect_with_quota(&hub, Some(&cli.token), DeviceKind::Cli, "cli").await;
    // 配对尝试在 single 里不记账
    let mut joiner = temp_with_cookie(&hub, &admin, DeviceKind::Web, "joiner").await;
    joiner.send_control(&Control::PairJoin { match_id: new_code().1 }).await;
    assert_eq!(joiner.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });
    assert_eq!(
        json_of(hub.get("/api/quota", &admin).await).await,
        json!({
            "devices": 2, "devices_max": null, "daemons": 1, "daemons_max": null,
            "connections": 2, "connections_max": null, "daily_pairings": 0, "daily_pairings_max": null,
        })
    );
    // 邀请来的账号：自己的用量
    let (_guest, guest_cookie) = hub.invite_account(&admin).await;
    let view = json_of(hub.get("/api/quota", &guest_cookie).await).await;
    assert_eq!((view["devices"].as_u64(), view["connections"].as_u64()), (Some(0), Some(0)));
}

#[tokio::test]
async fn single_mode_pairs_past_what_multi_would_allow() {
    // 默认 multi 配额是 10 台设备；single 没有配额：第 11 台照样配得进来
    let hub = TestHub::start().await;
    let node = hub.seed_device(DeviceKind::Daemon, "node").await;
    for i in 0..10 {
        hub.seed_device(DeviceKind::Web, &format!("web-{i}")).await;
    }
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let mut joiner = FakeDevice::connect(&hub, None, DeviceKind::Cli, "11th").await;
    pair_and_confirm(&mut n, &mut joiner).await;
    assert!(matches!(joiner.recv_control().await, Control::PairDone { device_id: Some(_), .. }));
    assert_eq!(hub.handle.store().count_devices(&hub.admin_account_id()).await.unwrap(), (12, 1));
    // 没有 quota_update
    let msgs = n.drain_controls(SHORT).await;
    assert!(quota_updates(&msgs).is_empty(), "{msgs:?}");
}
