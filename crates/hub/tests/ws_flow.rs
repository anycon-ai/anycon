//! WebSocket 表面的集成测试：认证、配对全流程、路由与转发规则、单连接、吊销、Origin、限速、关停。

mod common;

use std::time::Duration;

use anycon_protocol::{AuthFailReason, Control, DeviceKind, DropReason, Envelope, Kind, PairFailReason, PairRole};
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

const SHORT: Duration = Duration::from_millis(300);

fn body_of(n: usize, fill: u8) -> Vec<u8> {
    vec![fill; n]
}

// ---------------------------------------------------------------------------
// 认证
// ---------------------------------------------------------------------------

#[tokio::test]
async fn auth_rules() {
    let hub = TestHub::start_with(|c| c.timing.auth_timeout = Duration::from_millis(600)).await;
    let daemon = hub.seed_device(DeviceKind::Daemon, "d").await;

    // 好 token
    let ok = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "d").await;
    assert_eq!(ok.id, daemon.id);
    assert!(ok.peers_online.is_empty());
    drop(ok);

    // 坏 token
    let mut bad = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    bad.send_auth(Some("nope"), DeviceKind::Cli, "x").await;
    assert!(matches!(bad.recv_control().await, Control::AuthFail { reason: AuthFailReason::Invalid }));
    assert!(bad.expect_close().await.is_some());

    // 协议版本差太多
    let mut v = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    v.send_control(&Control::Auth {
        device_token: None,
        protocol_version: 99,
        client: client_info(DeviceKind::Cli, "x"),
    })
    .await;
    assert!(matches!(v.recv_control().await, Control::AuthFail { reason: AuthFailReason::Version }));

    // 第一帧不是 auth => AuthFail(protocol) + 4005
    let mut wrong = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    wrong.send_control(&Control::PairBegin { match_id: "0123456789abcdef".into() }).await;
    assert!(matches!(wrong.recv_control().await, Control::AuthFail { reason: AuthFailReason::Protocol }));
    assert_eq!(wrong.expect_close().await, Some(4005));

    // 垃圾字节
    let mut junk = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    junk.send_raw(vec![0xff, 0x00, 0x13]).await;
    assert!(matches!(junk.recv_control().await, Control::AuthFail { reason: AuthFailReason::Protocol }));

    // 超时没发 auth => 4005
    let mut silent = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    assert_eq!(silent.expect_close().await, Some(4005));
}

#[tokio::test]
async fn auth_ok_lists_online_peers_and_peers_get_online_offline_events() {
    let hub = TestHub::start().await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    let c = hub.seed_device(DeviceKind::Web, "c").await;
    hub.seed_paired(&a, &b).await;

    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    assert!(da.peers_online.is_empty());
    let mut dc = FakeDevice::connect(&hub, Some(&c.token), DeviceKind::Web, "c").await; // 与 a 未配对
    let db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert_eq!(db.peers_online, vec![a.id.clone()]);

    match da.recv_control().await {
        Control::PeerOnline { device } => assert_eq!(device, b.id),
        other => panic!("{other:?}"),
    }
    drop(db);
    match da.recv_control().await {
        Control::PeerOffline { device } => assert_eq!(device, b.id),
        other => panic!("{other:?}"),
    }
    dc.expect_silence(SHORT).await; // 无关设备什么都收不到
}

// ---------------------------------------------------------------------------
// 配对
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_pairing_flow() {
    let hub = TestHub::start().await;
    let seeded = hub.seed_device(DeviceKind::Daemon, "mac-mini").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&seeded.token), DeviceKind::Daemon, "mac-mini").await;

    // host: 短码在 daemon 本地生成，hub 只收到 match_id；PairBegin => PairRegistered（5 分钟有效）
    let (_code, match_id) = new_code();
    let before = anycon_hub::util::now_ms();
    let expires_at_ms = daemon.pair_begin(&match_id).await;
    assert!((before + 299_000..=anycon_hub::util::now_ms() + 300_000).contains(&expires_at_ms), "{expires_at_ms}");

    // joiner: 临时连接（无 token）。match_id 大小写不敏感
    let mut joiner = FakeDevice::connect(&hub, None, DeviceKind::Web, "Chrome · macOS").await;
    assert_ne!(joiner.id, seeded.id);
    assert_eq!(joiner.id.len(), 32);
    joiner.send_control(&Control::PairJoin { match_id: match_id.to_uppercase() }).await;
    match joiner.recv_control().await {
        Control::PairMatched { peer, role } => {
            assert_eq!(peer, seeded.id);
            assert_eq!(role, PairRole::Joiner);
        }
        other => panic!("{other:?}"),
    }
    let temp_id = joiner.id.clone();
    match daemon.recv_control().await {
        Control::PairMatched { peer, role } => {
            assert_eq!(peer, temp_id);
            assert_eq!(role, PairRole::Host);
        }
        other => panic!("{other:?}"),
    }

    // 登记是一次性的：第三方再 join => not_found
    let mut third = FakeDevice::connect(&hub, None, DeviceKind::Cli, "late").await;
    third.send_control(&Control::PairJoin { match_id: match_id.clone() }).await;
    assert!(matches!(third.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));

    // 三条握手帧：joiner → host → joiner，hub 只转发，字节不变
    let m1 = body_of(48, 0x11);
    let m2 = body_of(96, 0x22);
    let m3 = body_of(80, 0x33);
    let seq = joiner.send_env(&seeded.id, Kind::Handshake, m1.clone()).await;
    let got = daemon.recv_env().await;
    assert_eq!(
        (got.kind, got.from.as_str(), got.to.as_str(), got.seq, &got.body),
        (Kind::Handshake, temp_id.as_str(), seeded.id.as_str(), seq, &m1)
    );
    daemon.send_env(&temp_id, Kind::Handshake, m2.clone()).await;
    assert_eq!(joiner.recv_env().await.body, m2);
    joiner.send_env(&seeded.id, Kind::Handshake, m3.clone()).await;
    assert_eq!(daemon.recv_env().await.body, m3);

    // 配对确认前 data 不放行；发送方收到 Dropped { not_paired }
    let data_seq = joiner.send_env(&seeded.id, Kind::Data, body_of(10, 9)).await;
    daemon.expect_silence(SHORT).await;
    assert!(matches!(
        joiner.recv_control().await,
        Control::Dropped { seq, reason: DropReason::NotPaired } if seq == data_seq
    ));

    // 只有一边确认：没有 PairDone
    joiner.send_control(&Control::PairConfirm { peer: seeded.id.clone() }).await;
    daemon.expect_silence(SHORT).await;
    // 另一边也确认：双方收到 PairDone
    daemon.send_control(&Control::PairConfirm { peer: temp_id.clone() }).await;

    let (new_id, new_token) = match joiner.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, seeded.id);
            (device_id.expect("joiner gets its permanent id"), device_token.expect("joiner gets a device token"))
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(new_id.len(), 32);
    assert_eq!(new_token.len(), 64);
    match daemon.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, new_id, "host records the joiner by its permanent id");
            assert!(device_id.is_none() && device_token.is_none());
        }
        other => panic!("{other:?}"),
    }
    // PairDone 之后双方各收到对方的 peer_online（两边都在升级后的连接上在线）
    assert!(matches!(daemon.recv_control().await, Control::PeerOnline { device } if device == new_id));
    assert!(matches!(joiner.recv_control().await, Control::PeerOnline { device } if device == seeded.id));

    // 库里：设备记录（来自 ClientInfo，归入 host 的账号）+ 配对 + 审计；token 只存哈希
    let store = hub.handle.store();
    let dev = store.get_device(&new_id).await.unwrap().unwrap();
    assert_eq!(dev.name, "Chrome · macOS");
    assert_eq!(dev.kind, DeviceKind::Web);
    assert_eq!(dev.account_id, hub.admin_account_id());
    assert!(store.is_paired(&seeded.id, &new_id).await.unwrap());
    assert!(store.find_device_by_token_hash(&new_token).await.unwrap().is_none(), "plaintext token is not stored");
    assert!(store.find_device_by_token_hash(&anycon_hub::util::hash_token(&new_token)).await.unwrap().is_some());
    let audit = store.list_audit(50, None).await.unwrap();
    assert!(audit.iter().any(|e| e.event == "device_paired" && e.device_id.as_deref() == Some(new_id.as_str())));

    // 临时连接已升级为正式身份：不用重连就能发 data
    let payload = body_of(200, 0x5a);
    let seq = joiner.send_env_as(&new_id, &seeded.id, Kind::Data, payload.clone()).await;
    let got = daemon.recv_env().await;
    assert_eq!((got.kind, got.from.as_str(), got.seq, got.body), (Kind::Data, new_id.as_str(), seq, payload));

    // joiner 断开 => host 收到 peer_offline(正式 id)；之后带 token 重连，peers_online 里有 daemon
    drop(joiner);
    match daemon.recv_control().await {
        Control::PeerOffline { device } => assert_eq!(device, new_id),
        other => panic!("{other:?}"),
    }
    let again = FakeDevice::connect(&hub, Some(&new_token), DeviceKind::Web, "Chrome · macOS").await;
    assert_eq!(again.id, new_id);
    assert_eq!(again.peers_online, vec![seeded.id.clone()]);
    match daemon.recv_control().await {
        Control::PeerOnline { device } => assert_eq!(device, new_id),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn pair_join_unknown_match_id_and_rate_limit() {
    let hub = TestHub::start().await;
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Cli, "j").await;
    for _ in 0..10 {
        j.send_control(&Control::PairJoin { match_id: "0000000000000000".into() }).await;
        assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));
    }
    j.send_control(&Control::PairJoin { match_id: "0000000000000000".into() }).await;
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::RateLimited }));
    // 超限的回执每秒至多一条：接着猛发，一秒内不会再有任何回复
    for _ in 0..20 {
        j.send_control(&Control::PairJoin { match_id: "0000000000000000".into() }).await;
    }
    j.expect_silence(SHORT).await;
}

#[tokio::test]
async fn new_pair_begin_replaces_previous_registration() {
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let (_, first) = new_code();
    let (_, second) = new_code();
    assert_ne!(first, second);
    daemon.pair_begin(&first).await;
    daemon.pair_begin(&second).await;
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Cli, "j").await;
    j.send_control(&Control::PairJoin { match_id: first }).await;
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));
    j.send_control(&Control::PairJoin { match_id: second }).await;
    assert!(matches!(j.recv_control().await, Control::PairMatched { .. }));
}

#[tokio::test]
async fn malformed_match_ids_are_refused_and_never_registered() {
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let (code, _) = new_code();
    // 短码本身（hub 不该收到它）、太短、太长、非 hex、空串：都不是合法的 match_id
    for bad in
        [code.as_str(), "", "abc", "0123456789abcde", "0123456789abcdef0", "0123456789abcdeg", "0123-4567-89ab-cd"]
    {
        daemon.send_control(&Control::PairBegin { match_id: bad.to_string() }).await;
        assert!(
            matches!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }),
            "{bad:?}"
        );
    }
    // 没有登记任何东西：非法的 id 同样 join 不上
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Cli, "j").await;
    j.send_control(&Control::PairJoin { match_id: String::new() }).await;
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));
}

/// 加入时 match_id 差一个字符、或者直接发短码，都撮合不上；host 什么都收不到。对的才行。
#[tokio::test]
async fn joiner_with_a_wrong_match_id_is_not_matched() {
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let (code, match_id) = new_code();
    daemon.pair_begin(&match_id).await;

    let flip_last = |m: &str| {
        let mut chars: Vec<char> = m.chars().collect();
        let last = chars.last_mut().unwrap();
        *last = if *last == '0' { '1' } else { '0' };
        chars.into_iter().collect::<String>()
    };
    let mut attacker = FakeDevice::connect(&hub, None, DeviceKind::Cli, "attacker").await;
    for guess in [flip_last(&match_id), code.clone(), code.replace('-', ""), "0".repeat(16), match_id[..15].to_string()]
    {
        attacker.send_control(&Control::PairJoin { match_id: guess.clone() }).await;
        assert!(
            matches!(attacker.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }),
            "{guess:?} must not match"
        );
    }
    daemon.expect_silence(SHORT).await;
    // 猜错没有让登记作废：真正的 joiner 仍然可以匹配
    let mut joiner = FakeDevice::connect(&hub, None, DeviceKind::Web, "joiner").await;
    joiner.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(joiner.recv_control().await, Control::PairMatched { role: PairRole::Joiner, .. }));
    assert!(matches!(daemon.recv_control().await, Control::PairMatched { role: PairRole::Host, .. }));
    // 攻击者不在撮合里：握手帧不会被转发给 host
    attacker.send_env(&d.id, Kind::Handshake, vec![1, 2, 3]).await;
    daemon.expect_silence(SHORT).await;
}

/// hub 对握手帧只转发：字节原样（整帧，含信封头），没有改动、没有重组。
#[tokio::test]
async fn handshake_frames_are_forwarded_byte_for_byte() {
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut joiner = FakeDevice::connect(&hub, None, DeviceKind::Web, "joiner").await;
    let temp = joiner.id.clone();
    joiner.send_control(&Control::PairJoin { match_id }).await;
    expect_matched(&mut daemon, &mut joiner).await;

    // 三条「握手消息」：随机字节（包括 0x00 / 0xff 开头、长度各异），像 Noise 密文一样不可解析
    let msgs: [Vec<u8>; 3] = [
        (0..32u8).map(|i| i.wrapping_mul(37)).collect(),
        std::iter::once(0xff).chain((0..95u8).map(|i| i ^ 0xa5)).collect(),
        std::iter::once(0x00).chain((0..200u8).rev()).collect(),
    ];
    for (i, msg) in msgs.iter().enumerate() {
        // joiner → host → joiner（XX 的三条消息）
        let from_joiner = i != 1;
        let (from, to, sender, receiver) = if from_joiner {
            (temp.as_str(), d.id.as_str(), &mut joiner, &mut daemon)
        } else {
            (d.id.as_str(), temp.as_str(), &mut daemon, &mut joiner)
        };
        sender.seq += 1;
        let sent = Envelope::new(from, to, Kind::Handshake, sender.seq, msg.clone()).encode().unwrap();
        sender.send_raw(sent.clone()).await;
        let received = receiver.recv_raw().await;
        assert_eq!(received, sent, "hub must not touch handshake frames (message {})", i + 1);
    }
}

#[tokio::test]
async fn pairing_times_out_and_peer_gone() {
    let hub = TestHub::start_with(|c| c.timing.pair_confirm_timeout = Duration::from_millis(700)).await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;

    // 超时：matched 后没人确认
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Cli, "j").await;
    j.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(j.recv_control().await, Control::PairMatched { .. }));
    assert!(matches!(daemon.recv_control().await, Control::PairMatched { .. }));
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::Timeout }));
    assert!(matches!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::Timeout }));

    // 对端在握手期间断开
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut j2 = FakeDevice::connect(&hub, None, DeviceKind::Cli, "j2").await;
    j2.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(j2.recv_control().await, Control::PairMatched { .. }));
    assert!(matches!(daemon.recv_control().await, Control::PairMatched { .. }));
    drop(j2);
    assert!(matches!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::PeerGone }));
}

#[tokio::test]
async fn temporary_connections_cannot_begin_pairing_or_send_data() {
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    // 只有 kind = daemon 的临时连接可以发 PairBegin；其余 kind 一律无声丢弃
    let (_, match_id) = new_code();
    for kind in [DeviceKind::Cli, DeviceKind::Web, DeviceKind::Sdk] {
        let mut other = FakeDevice::connect(&hub, None, kind, "other").await;
        other.send_control(&Control::PairBegin { match_id: match_id.clone() }).await;
        other.expect_silence(SHORT).await; // 没有 PairRegistered
    }
    // 它们没有登记任何东西
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Web, "j").await;
    j.send_control(&Control::PairJoin { match_id: match_id.clone() }).await;
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));

    let mut tmp = FakeDevice::connect(&hub, None, DeviceKind::Cli, "tmp").await;
    tmp.send_control(&Control::PairBegin { match_id }).await;
    tmp.expect_silence(SHORT).await; // 没有 PairRegistered
    tmp.send_env(&d.id, Kind::Data, vec![1, 2, 3]).await;
    // 没配对、没匹配 => 不放行；发送方（临时连接）只会收到一条 Dropped 回执
    assert!(matches!(tmp.recv_control().await, Control::Dropped { reason: DropReason::NotPaired, .. }));
    tmp.send_env(&d.id, Kind::Handshake, vec![1, 2, 3]).await; // 回执限频：同一秒内没有第二条
    daemon.expect_silence(SHORT).await;
    tmp.expect_silence(SHORT).await;
}

#[tokio::test]
async fn temporary_daemon_connection_may_begin_pairing_only_a_few_times() {
    let hub = TestHub::start().await;
    let mut tmp = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "fresh").await;
    // 前 3 次登记成功（新的顶掉旧的）
    for _ in 0..3 {
        let (_, match_id) = new_code();
        tmp.pair_begin(&match_id).await;
    }
    // 第 4 次起被拒。match id 先算好：每个都要一次 Argon2id（64 MiB × 3 轮），夹在发送之间会把这一串拉长到跨过回执的 1 秒预算。
    let rejected: Vec<String> = (0..10).map(|_| new_code().1).collect();
    let started = std::time::Instant::now();
    for match_id in &rejected {
        tmp.send_control(&Control::PairBegin { match_id: match_id.clone() }).await;
    }
    // 栅栏：hub 按顺序处理同一条连接上的帧，PairJoin 的 not_found 之前收到的就是这 10 次 PairBegin 引起的全部回应。
    // 不管 last 有没有被（错误地）登记，自己 join 自己都是 not_found，所以栅栏的回应是确定的。
    let last = rejected.last().unwrap().clone();
    tmp.send_control(&Control::PairJoin { match_id: last.clone() }).await;
    let mut receipts = 0u64;
    loop {
        match tmp.recv_control().await {
            Control::PairFailed { reason: PairFailReason::RateLimited } => receipts += 1,
            Control::PairFailed { reason: PairFailReason::NotFound } => break,
            other => panic!("a rejected pair_begin answers at most rate_limited, got {other:?}"),
        }
    }
    // 回执每秒至多一条：第一次被拒必有一条，之后这一串花了多少整秒，最多再多几条（机器正常时就是 1 条）
    let elapsed = started.elapsed();
    assert!((1..=1 + elapsed.as_secs()).contains(&receipts), "{receipts} rate_limited receipts in {elapsed:?}");
    // 被拒的确实没登记：要是登记了，新的顶掉旧的，最后一个会是有效的，别人 join 它会撮合成功
    let mut j = FakeDevice::connect(&hub, None, DeviceKind::Web, "j").await;
    j.send_control(&Control::PairJoin { match_id: last }).await;
    assert!(matches!(j.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));
    // 正式设备没有这个限制
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    for _ in 0..10 {
        let (_, match_id) = new_code();
        daemon.pair_begin(&match_id).await;
    }
}

#[tokio::test]
async fn temporary_connections_expire_but_permanent_ones_do_not() {
    let hub = TestHub::start_with(|c| c.timing.temp_conn_ttl = Duration::from_millis(1500)).await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut permanent = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let mut tmp = FakeDevice::connect(&hub, None, DeviceKind::Daemon, "fresh").await;
    // 临时连接到点被关（4005），hub 的路由表里也清掉了
    assert_eq!(tmp.expect_close().await, Some(4005));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(hub.handle.state.registry.get(&tmp.id).is_none());
    // 正式设备不受影响
    permanent.expect_silence(Duration::from_millis(500)).await;
    let (_, match_id) = new_code();
    permanent.pair_begin(&match_id).await;
}

#[tokio::test]
async fn pairing_requires_same_account() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;

    // 另一个账号的浏览器 cookie
    let inv: Value = hub.send(Method::POST, "/api/invites", &admin, json!({})).await.json().await.unwrap();
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": inv["code"] }))
        .send()
        .await
        .unwrap();
    let other_cookie = cookie_pair(&res);

    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut joiner =
        FakeDevice::connect_with(&hub, None, DeviceKind::Web, "other", &[("cookie", other_cookie.as_str())]).await;
    joiner.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(joiner.recv_control().await, Control::PairMatched { .. }));
    let temp = joiner.id.clone();
    assert!(matches!(daemon.recv_control().await, Control::PairMatched { .. }));
    joiner.send_control(&Control::PairConfirm { peer: d.id.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: temp }).await;
    assert!(matches!(joiner.recv_control().await, Control::PairFailed { reason: PairFailReason::Forbidden }));
    assert!(matches!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::Forbidden }));
    assert!(hub.handle.store().peers_of(&d.id).await.unwrap().is_empty());

    // 同账号的 cookie 可以
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut joiner = FakeDevice::connect_with(&hub, None, DeviceKind::Web, "mine", &[("cookie", admin.as_str())]).await;
    joiner.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(joiner.recv_control().await, Control::PairMatched { .. }));
    let temp = joiner.id.clone();
    assert!(matches!(daemon.recv_control().await, Control::PairMatched { .. }));
    joiner.send_control(&Control::PairConfirm { peer: d.id.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: temp }).await;
    assert!(matches!(joiner.recv_control().await, Control::PairDone { device_token: Some(_), .. }));
}

#[tokio::test]
async fn authenticated_device_can_join_a_second_daemon_and_keeps_its_id() {
    let hub = TestHub::start().await;
    let host = hub.seed_device(DeviceKind::Daemon, "second-daemon").await;
    let web = hub.seed_device(DeviceKind::Web, "browser").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&host.token), DeviceKind::Daemon, "second-daemon").await;
    let mut browser = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "browser").await;

    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    browser.send_control(&Control::PairJoin { match_id }).await;
    assert!(matches!(browser.recv_control().await, Control::PairMatched { role: PairRole::Joiner, .. }));
    assert!(
        matches!(daemon.recv_control().await, Control::PairMatched { role: PairRole::Host, peer } if peer == web.id)
    );
    browser.send_env(&host.id, Kind::Handshake, vec![7; 32]).await;
    assert_eq!(daemon.recv_env().await.body, vec![7; 32]);
    browser.send_control(&Control::PairConfirm { peer: host.id.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: web.id.clone() }).await;

    match browser.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, host.id);
            assert!(device_id.is_none() && device_token.is_none(), "already-authenticated joiners get no new identity");
        }
        other => panic!("{other:?}"),
    }
    match daemon.recv_control().await {
        Control::PairDone { peer, device_token, .. } => {
            assert_eq!(peer, web.id);
            assert!(device_token.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(daemon.recv_control().await, Control::PeerOnline { device } if device == web.id));
    assert!(matches!(browser.recv_control().await, Control::PeerOnline { device } if device == host.id));
    assert!(hub.handle.store().is_paired(&host.id, &web.id).await.unwrap());
    // 立刻能互发 data
    browser.send_env(&host.id, Kind::Data, vec![1]).await;
    assert_eq!(daemon.recv_env().await.from, web.id);
}

// ---------------------------------------------------------------------------
// 路由
// ---------------------------------------------------------------------------

#[tokio::test]
async fn routing_rules() {
    let hub = TestHub::start_with(|c| c.max_envelope_bytes = 4096).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    let c = hub.seed_device(DeviceKind::Web, "c").await;
    hub.seed_paired(&a, &b).await;

    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    let mut dc = FakeDevice::connect(&hub, Some(&c.token), DeviceKind::Web, "c").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. })); // b 上线

    // 已配对 => 原样转发（字节一致）
    let payload: Vec<u8> = (0..=255u8).cycle().take(1500).collect();
    let seq = da.send_env(&b.id, Kind::Data, payload.clone()).await;
    let got = db.recv_env().await;
    assert_eq!(got, Envelope::new(a.id.clone(), b.id.clone(), Kind::Data, seq, payload));
    // 反向 + notify
    db.send_env(&a.id, Kind::Notify, vec![0xAB; 20]).await;
    let got = da.recv_env().await;
    assert_eq!((got.kind, got.from.as_str()), (Kind::Notify, b.id.as_str()));
    // 已配对设备之间的 handshake（重连握手）也放行
    da.send_env(&b.id, Kind::Handshake, vec![1; 64]).await;
    assert_eq!(db.recv_env().await.kind, Kind::Handshake);

    // 未配对 => 丢弃：目标什么都收不到，发送方收到 Dropped { not_paired }（回执每秒至多一条，所以 dc 的第二封没有）
    let seq = da.send_env(&c.id, Kind::Data, vec![1; 10]).await;
    assert!(matches!(
        da.recv_control().await,
        Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq
    ));
    let seq = dc.send_env(&a.id, Kind::Data, vec![1; 10]).await;
    dc.send_env(&b.id, Kind::Handshake, vec![1; 10]).await;
    let receipts = dc.drain_controls(SHORT).await;
    assert!(
        matches!(receipts.as_slice(), [Control::Dropped { seq: s, reason: DropReason::NotPaired }] if *s == seq),
        "{receipts:?}"
    );
    da.expect_silence(SHORT).await;
    db.expect_silence(SHORT).await;

    // 伪造 from => 丢弃（发送方至多收到回执，目标什么都收不到）
    da.send_env_as(&c.id, &b.id, Kind::Data, vec![1; 10]).await;
    db.expect_silence(SHORT).await;
    assert!(da.drain_controls(SHORT).await.iter().all(|c| matches!(c, Control::Dropped { .. })));

    // 超大 => 丢弃（不断连接），之后正常
    da.send_env(&b.id, Kind::Data, body_of(5000, 1)).await;
    db.expect_silence(SHORT).await;
    assert!(da.drain_controls(SHORT).await.iter().all(|c| matches!(c, Control::Dropped { .. })));
    da.send_env(&b.id, Kind::Data, body_of(100, 2)).await;
    assert_eq!(db.recv_env().await.body, body_of(100, 2));

    // 目标离线 => undeliverable { to, seq }
    drop(db);
    assert!(matches!(da.recv_control().await, Control::PeerOffline { .. }));
    let seq = da.send_env(&b.id, Kind::Data, vec![1; 10]).await;
    match da.recv_control().await {
        Control::Undeliverable { to, seq: s } => assert_eq!((to, s), (b.id.clone(), seq)),
        other => panic!("{other:?}"),
    }
    let seq = da.send_env(&b.id, Kind::Notify, vec![1; 10]).await;
    assert!(matches!(da.recv_control().await, Control::Undeliverable { seq: s, .. } if s == seq));

    // 垃圾帧、版本不对的信封 => 静默丢弃，连接还在
    da.send_raw(vec![0xc1, 0x00]).await;
    let mut bad_v = Envelope::new(a.id.clone(), b.id.clone(), Kind::Data, 99, vec![1]);
    bad_v.v = 9;
    da.send_raw(bad_v.encode().unwrap()).await;
    da.expect_silence(SHORT).await;
    let (_, match_id) = new_code();
    da.pair_begin(&match_id).await;
}

/// 被 hub 丢弃的信封，发送方（已认证的连接）能知道：`Dropped { seq, reason }`。
/// 回执（`Dropped`、`RateLimited`、限速的 `PairFailed`）共用一个预算，每连接每秒至多一条。
#[tokio::test]
async fn dropped_receipts_name_the_seq_and_are_rate_limited() {
    let hub = TestHub::start_with(|c| {
        c.max_envelope_bytes = 4096;
        c.limits.conn_envelopes_per_sec = 30;
    })
    .await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    let c = hub.seed_device(DeviceKind::Web, "c").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));

    // 超大（> ANYCON_MAX_ENVELOPE_BYTES，但没到 2 倍）：Dropped { too_large }，连接不断，目标收不到
    let seq = da.send_env(&b.id, Kind::Data, body_of(5000, 1)).await;
    assert!(matches!(
        da.recv_control().await,
        Control::Dropped { seq: s, reason: DropReason::TooLarge } if s == seq
    ));
    db.expect_silence(SHORT).await;

    // 同一秒里再丢十个（超大 + 未配对 + 超过每连接限速）：一条回执都不会有，哪种都一样
    for i in 0..10 {
        if i % 2 == 0 {
            da.send_env(&b.id, Kind::Data, body_of(5000, 1)).await;
        } else {
            da.send_env(&c.id, Kind::Data, body_of(10, 1)).await;
        }
    }
    let mut notifies = Vec::new();
    for _ in 0..60 {
        // 其中超过每连接 30 个/秒的会被限速：不丢，退回 backpressure（它不占回执预算）
        notifies.push(da.send_env(&b.id, Kind::Notify, body_of(10, 2)).await);
    }
    let late = da.drain_controls(Duration::from_millis(500)).await;
    let (backpressure, late): (Vec<_>, Vec<_>) =
        late.into_iter().partition(|c| matches!(c, Control::Backpressure { .. }));
    assert!(late.is_empty(), "receipts must share one budget of 1/s, got {late:?}");
    // 没有静默丢弃：每封 notify 要么到了 b，要么收到了 backpressure
    let mut delivered = 0;
    while let Some(Recv::Env(_)) = db.recv_within(Duration::from_millis(200)).await {
        delivered += 1;
    }
    assert!(!backpressure.is_empty(), "the notifies over the per-second limit are handed back");
    assert_eq!(delivered + backpressure.len(), notifies.len(), "forwarded or handed back, never dropped");

    // 过了这一秒，下一条丢弃又有回执
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let seq = da.send_env(&c.id, Kind::Data, body_of(10, 1)).await;
    let receipts: Vec<_> = da
        .drain_controls(SHORT)
        .await
        .into_iter()
        .filter(|c| !matches!(c, Control::Backpressure { .. } | Control::Undeliverable { .. }))
        .collect();
    assert!(
        matches!(receipts.as_slice(), [Control::Dropped { seq: s, reason: DropReason::NotPaired }] if *s == seq),
        "{receipts:?}"
    );
}

/// 超过每秒信封数的 `data` 不再被丢：退回 `backpressure`（每封一条）并粘住，另外每秒至多一条 `rate_limited`。
#[tokio::test]
async fn over_the_envelope_rate_is_handed_back_with_one_rate_limited_notice_per_second() {
    let hub = TestHub::start_with(|c| c.limits.conn_envelopes_per_sec = 5).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));
    let mut sent = Vec::new();
    for _ in 0..50 {
        sent.push(da.send_env(&b.id, Kind::Data, body_of(10, 1)).await);
    }
    let notices = da.drain_controls(Duration::from_millis(600)).await;
    let rate_limited: Vec<_> = notices.iter().filter(|c| matches!(c, Control::RateLimited { .. })).collect();
    assert_eq!(rate_limited.len(), 1, "{notices:?}");
    assert!(
        matches!(rate_limited[0], Control::RateLimited { retry_after_ms } if (100..=1000).contains(retry_after_ms))
    );
    let returned: Vec<u64> = notices
        .iter()
        .filter_map(|c| match c {
            Control::Backpressure { to, seq } => {
                assert_eq!(to, &b.id);
                Some(*seq)
            }
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1 + returned.len(), "nothing but rate_limited and backpressure: {notices:?}");

    let mut delivered = Vec::new();
    while let Some(Recv::Env(e)) = db.recv_within(Duration::from_millis(200)).await {
        delivered.push(e.seq);
    }
    assert!((1..=10).contains(&delivered.len()), "{delivered:?}");
    // 粘住：过了限额的第一封被退回之后，后面的（即使跨进了下一秒）也一律退回；到的是一个前缀，其余全被退回
    let k = delivered.len();
    assert_eq!(delivered, sent[..k].to_vec(), "what arrived is exactly the first {k}, in order");
    assert_eq!(returned, sent[k..].to_vec(), "everything else was handed back, in order");
}

/// 超过 `max(2 × ANYCON_MAX_ENVELOPE_BYTES, 64 KiB)` 的帧在 WebSocket 层就被拒绝（`max_message_size`），连接被关闭——
/// 在上限和这个硬上限之间的帧才会收到 `Dropped { too_large }`。这是有意的策略：正常客户端按 ≤ 200 KiB 分片，永远到不了这里。
#[tokio::test]
async fn frames_over_twice_the_limit_close_the_connection() {
    let hub = TestHub::start_with(|c| c.max_envelope_bytes = 4096).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));
    // 上限（4096）和硬上限（这里是 64 KiB 的下限）之间：丢弃 + 回执，连接保持
    da.send_env(&b.id, Kind::Data, body_of(60_000, 1)).await;
    assert!(matches!(da.recv_control().await, Control::Dropped { reason: DropReason::TooLarge, .. }));
    // 超过硬上限：连接被关
    da.send_env(&b.id, Kind::Data, body_of(70_000, 1)).await;
    assert!(matches!(da.recv().await, Recv::Closed(_)));
    // 目标没收到任何数据，只看到发送方下线
    assert!(matches!(db.recv_control().await, Control::PeerOffline { device } if device == a.id));
}

#[tokio::test]
async fn second_connection_for_same_device_supersedes_the_first() {
    let hub = TestHub::start().await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut peer = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;

    let mut first = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    assert!(matches!(peer.recv_control().await, Control::PeerOnline { device } if device == a.id));

    let mut second = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    assert_eq!(first.expect_close().await, Some(4001));
    assert_eq!(second.id, a.id);
    assert_eq!(second.peers_online, vec![b.id.clone()]);

    // 对端看到的是「又上线了一次」，旧连接的收尾不能制造 peer_offline
    assert!(matches!(peer.recv_control().await, Control::PeerOnline { device } if device == a.id));
    peer.expect_silence(SHORT).await;

    // 路由已指向新连接
    peer.send_env(&a.id, Kind::Data, vec![5; 5]).await;
    assert_eq!(second.recv_env().await.body, vec![5; 5]);
}

#[tokio::test]
async fn revoke_control_message_removes_pairing_and_disconnects_target() {
    let hub = TestHub::start().await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    let c = hub.seed_device(DeviceKind::Web, "c").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));

    // 与目标没有配对关系 => 无效
    da.send_control(&Control::Revoke { device: c.id.clone() }).await;
    da.expect_silence(SHORT).await;
    assert!(hub.handle.store().get_device(&c.id).await.unwrap().unwrap().revoked_at_ms.is_none());

    // 已配对 => 吊销：b 以 4004 断开，a 收到 peer_offline，配对删除，设备置 revoked
    da.send_control(&Control::Revoke { device: b.id.clone() }).await;
    assert_eq!(db.expect_close().await, Some(4004));
    assert!(matches!(da.recv_control().await, Control::PeerOffline { device } if device == b.id));
    let store = hub.handle.store();
    assert!(!store.is_paired(&a.id, &b.id).await.unwrap());
    assert!(store.get_device(&b.id).await.unwrap().unwrap().revoked_at_ms.is_some());
    // a 再给 b 发 data：不再是对端 => 丢弃（Dropped { not_paired }，不是 undeliverable）
    let seq = da.send_env(&b.id, Kind::Data, vec![1]).await;
    assert!(matches!(
        da.recv_control().await,
        Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq
    ));
    da.expect_silence(SHORT).await;
}

// ---------------------------------------------------------------------------
// Origin / 限速 / 关停 / 心跳
// ---------------------------------------------------------------------------

async fn upgrade_status(url: &str, origin: Option<&str>, host: Option<&str>) -> u16 {
    let mut headers = Vec::new();
    if let Some(o) = origin {
        headers.push(("origin", o));
    }
    if let Some(h) = host {
        headers.push(("host", h));
    }
    match FakeDevice::open_with(url, &headers).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status().as_u16(),
        Err(e) => panic!("unexpected error {e:?}"),
    }
}

#[tokio::test]
async fn websocket_origin_check() {
    let hub = TestHub::start().await;
    let url = hub.ws_url();
    // 没有 Origin（daemon / CLI / SDK）
    assert_eq!(upgrade_status(&url, None, None).await, 101);
    // ANYCON_PUBLIC_URL 的 origin
    assert_eq!(upgrade_status(&url, Some("https://hub.test"), None).await, 101);
    // 私网 / 回环的 http
    for o in [
        "http://127.0.0.1:5173",
        "http://localhost:5173",
        "http://192.168.1.20:8080",
        "http://10.0.0.5",
        "http://[::1]:3000",
    ] {
        assert_eq!(upgrade_status(&url, Some(o), None).await, 101, "{o}");
    }
    // 其他一律 403
    for o in ["https://evil.example", "http://evil.example", "http://8.8.8.8", "null", "https://hub.test:8443"] {
        assert_eq!(upgrade_status(&url, Some(o), None).await, 403, "{o}");
    }
    // 请求自身的 Host
    assert_eq!(upgrade_status(&url, Some("https://my-hub.example.net"), Some("my-hub.example.net")).await, 101);
    assert_eq!(upgrade_status(&url, Some("https://my-hub.example.net"), Some("other.example.net")).await, 403);
    // 被拒绝的响应是标准错误格式
    let res = hub
        .http
        .get(hub.url("/v1/ws"))
        .header("origin", "https://evil.example")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn websocket_upgrade_is_rate_limited() {
    let hub = TestHub::start().await;
    let url = hub.ws_url();
    for i in 0..30 {
        assert_eq!(upgrade_status(&url, None, None).await, 101, "upgrade {}", i + 1);
    }
    assert_eq!(upgrade_status(&url, None, None).await, 429);
}

#[tokio::test]
async fn graceful_shutdown_closes_connections_with_4002() {
    let hub = TestHub::start().await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut tmp = FakeDevice::connect(&hub, None, DeviceKind::Cli, "tmp").await;
    let TestHub { handle, dir: _dir, .. } = hub;
    let started = std::time::Instant::now();
    handle.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(5), "shutdown took {:?}", started.elapsed());
    assert_eq!(da.expect_close().await, Some(4002));
    assert_eq!(tmp.expect_close().await, Some(4002));
}

#[tokio::test]
async fn heartbeat_drops_silent_connections_but_keeps_responsive_ones() {
    let hub = TestHub::start_with(|c| {
        c.timing.ping_interval = Duration::from_millis(200);
        c.timing.pong_timeout = Duration::from_millis(1500);
    })
    .await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Daemon, "b").await;
    let mut alive = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let silent = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Daemon, "b").await;

    // alive 一直在读（tungstenite 会自动回 pong），silent 完全不读
    let mut saw_ping = false;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(3200);
    while tokio::time::Instant::now() < deadline {
        use futures::StreamExt;
        if let Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(_)))) =
            tokio::time::timeout(Duration::from_millis(100), alive.ws.next()).await
        {
            saw_ping = true;
        }
    }
    assert!(saw_ping, "hub sends pings");
    // hub 已把沉默的那条连接从路由表里清掉
    assert!(hub.handle.state.registry.get(&b.id).is_none(), "silent connection must be dropped");
    assert!(hub.handle.state.registry.get(&a.id).is_some(), "responsive connection stays");
    drop(silent);
    let (_, match_id) = new_code();
    alive.pair_begin(&match_id).await;
}

#[tokio::test]
async fn backpressure_when_target_buffer_is_full() {
    // 目标连接的发送缓冲很小：一次猛灌，超出的信封被丢弃并回 backpressure。
    let hub = TestHub::start_with(|c| c.outbound_buffer_bytes = 4096).await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let mut db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await;
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));

    const N: usize = 300;
    let mut sent = Vec::new();
    for _ in 0..N {
        sent.push(da.send_env(&b.id, Kind::Data, body_of(1024, 7)).await);
    }
    let mut backpressured = Vec::new();
    while let Some(r) = da.recv_within(Duration::from_millis(500)).await {
        match r {
            Recv::Env(e) if e.kind == Kind::Control => {
                if let Control::Backpressure { to, seq } = e.control_body().unwrap() {
                    assert_eq!(to, b.id);
                    backpressured.push(seq);
                }
            }
            other => panic!("{other:?}"),
        }
    }
    let mut delivered = Vec::new();
    while let Some(Recv::Env(e)) = db.recv_within(Duration::from_millis(300)).await {
        delivered.push(e.seq);
    }
    assert!(!backpressured.is_empty(), "expected at least one backpressure notice");
    assert!(backpressured.iter().all(|s| sent.contains(s)));
    assert_eq!(
        delivered.len() + backpressured.len(),
        N,
        "every envelope is either delivered or reported as backpressured"
    );
    // 粘住：第一封被退回之后，后面的一律被退回——到达的是发出顺序的一个前缀，被退回的是剩下的，一封不漏、不乱序
    let k = delivered.len();
    assert_eq!(delivered, sent[..k].to_vec());
    assert_eq!(backpressured, sent[k..].to_vec());
}

#[tokio::test]
async fn slow_consumer_is_disconnected_after_the_buffer_stays_full() {
    // B 不读 socket：hub 的写端卡在 TCP 背压上，4 MiB（这里 1 MiB）缓冲一直满，超过阈值后踢掉 B（4003）。
    let hub = TestHub::start_with(|c| {
        c.outbound_buffer_bytes = 1024 * 1024;
        c.timing.slow_consumer_timeout = Duration::from_millis(1200);
        c.limits.client_bytes_per_min = u64::MAX;
        c.limits.daemon_bytes_per_min = u64::MAX;
        c.limits.conn_envelopes_per_sec = u32::MAX;
        c.max_envelope_bytes = 262_144;
    })
    .await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let b = hub.seed_device(DeviceKind::Web, "b").await;
    hub.seed_paired(&a, &b).await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    let _db = FakeDevice::connect(&hub, Some(&b.token), DeviceKind::Web, "b").await; // 连上但从不读
    assert!(matches!(da.recv_control().await, Control::PeerOnline { .. }));

    // 发送方照协议办事：灌一批，被退回之后不断重发被退回的第一封（粘住规则下后面的都在等它）。
    // 每次重发都再次被拒（B 一直没读），「满」的计时就一直不被清掉，到点踢掉 B。
    let mut backpressure_seen = false;
    let mut first_returned: Option<(u64, Vec<u8>)> = None;
    let mut sent = std::collections::BTreeMap::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    for _ in 0..40 {
        let seq = da.send_env(&b.id, Kind::Data, body_of(250 * 1024, 3)).await;
        sent.insert(seq, Envelope::new(da.id.clone(), b.id.clone(), Kind::Data, seq, body_of(250 * 1024, 3)));
    }
    while hub.handle.state.registry.get(&b.id).is_some() {
        assert!(std::time::Instant::now() < deadline, "slow consumer was never disconnected");
        while let Some(Recv::Env(e)) = da.recv_within(Duration::from_millis(50)).await {
            if let Ok(Control::Backpressure { seq, .. }) = e.control_body() {
                backpressure_seen = true;
                if first_returned.as_ref().is_none_or(|(s, _)| seq < *s) {
                    first_returned = Some((seq, sent[&seq].encode().unwrap()));
                }
            }
        }
        if let Some((_, bytes)) = &first_returned {
            da.send_raw(bytes.clone()).await;
        }
    }
    assert!(backpressure_seen, "sender is told to back off before the slow consumer is dropped");
    // 发送方不受影响
    let (_, match_id) = new_code();
    da.send_control(&Control::PairBegin { match_id }).await;
    loop {
        match da.recv_control().await {
            Control::PairRegistered { .. } => break,
            Control::Backpressure { .. } | Control::PeerOffline { .. } | Control::Undeliverable { .. } => continue,
            other => panic!("{other:?}"),
        }
    }
}

/// 不读 socket 的连接不能用控制消息把 hub 的内存灌爆。控制消息计入同一个发送缓冲、有自己的上限；
/// 缓冲一直满，30 秒（这里缩短）后以 4003 断开，和消息种类无关。
#[tokio::test]
async fn control_message_flood_to_a_connection_that_never_reads_is_bounded_and_disconnected() {
    let hub = TestHub::start_with(|c| {
        c.outbound_buffer_bytes = 64 * 1024; // 控制消息的上限随之是 64 KiB（见 outq::control_cap_for）
        c.timing.slow_consumer_timeout = Duration::from_millis(1000);
        c.limits.client_bytes_per_min = u64::MAX;
        c.limits.daemon_bytes_per_min = u64::MAX;
        c.limits.conn_envelopes_per_sec = u32::MAX;
    })
    .await;
    let d = hub.seed_device(DeviceKind::Daemon, "flooder").await;
    let mut flooder = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "flooder").await; // 之后从不读
    let conn = hub.handle.state.registry.get(&d.id).expect("registered");
    let bound = conn.out.max_pending();
    assert_eq!(bound, 128 * 1024, "64 KiB of data + 64 KiB of control messages");

    // 每个 PairBegin 让 hub 回一条 PairRegistered。一直灌到 hub 把这条连接踢掉。
    let (_, match_id) = new_code();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut sent = 0u64;
    let mut max_seen = 0usize;
    while hub.handle.state.registry.get(&d.id).is_some() {
        assert!(std::time::Instant::now() < deadline, "flooder was never disconnected (sent {sent})");
        for _ in 0..500 {
            // hub 一旦踢掉连接，写就会失败——那正是我们要等的
            if !flooder.try_send_control(&Control::PairBegin { match_id: match_id.clone() }).await {
                break;
            }
            sent += 1;
        }
        // 每一轮都核对内存上界：数据 + 控制消息排队的字节数永远不超过 cap + control_cap
        max_seen = max_seen.max(conn.out.pending());
        assert!(conn.out.pending() <= bound, "queued {} > bound {bound}", conn.out.pending());
        assert!(conn.out.control_pending() <= conn.out.pending());
        tokio::task::yield_now().await;
    }
    assert!(max_seen > 0 && sent as usize * 40 > bound, "the flood must actually exceed the bound: sent {sent}");
    assert_eq!(conn.kill_code(), 4003, "an unread connection is closed as a slow consumer, whatever it is sent");
    assert!(conn.kill.is_cancelled());
    // hub 照常工作
    let ok = hub.seed_device(DeviceKind::Daemon, "ok").await;
    let mut other = FakeDevice::connect(&hub, Some(&ok.token), DeviceKind::Daemon, "ok").await;
    let (_, m2) = new_code();
    other.pair_begin(&m2).await;
}

#[tokio::test]
async fn client_initiated_close_is_answered_with_a_close_frame() {
    use futures::SinkExt;
    let hub = TestHub::start().await;
    let a = hub.seed_device(DeviceKind::Daemon, "a").await;
    let mut da = FakeDevice::connect(&hub, Some(&a.token), DeviceKind::Daemon, "a").await;
    da.ws.send(tokio_tungstenite::tungstenite::Message::Close(None)).await.unwrap();
    // hub 回 Close（不是直接断开 TCP），并把设备从路由表里清掉
    assert!(matches!(da.recv().await, Recv::Closed(_)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(hub.handle.state.registry.get(&a.id).is_none());
}

// ---------------------------------------------------------------------------
// token-less daemon：第一次连 hub 时靠配对拿到身份
// ---------------------------------------------------------------------------

/// 一个还没有 token 的 daemon（临时连接）发起配对，返回 (daemon 连接, match_id)。
async fn tokenless_daemon(hub: &TestHub) -> (FakeDevice, String) {
    let mut daemon = FakeDevice::connect(hub, None, DeviceKind::Daemon, "fresh-mac").await;
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    (daemon, match_id)
}

async fn expect_matched(host: &mut FakeDevice, joiner: &mut FakeDevice) {
    assert!(matches!(joiner.recv_control().await, Control::PairMatched { role: PairRole::Joiner, .. }));
    assert!(matches!(host.recv_control().await, Control::PairMatched { role: PairRole::Host, .. }));
}

#[tokio::test]
async fn tokenless_daemon_host_gets_its_identity_through_the_first_pairing() {
    let hub = TestHub::start().await;
    let cookie = hub.login_admin().await;
    let (mut daemon, match_id) = tokenless_daemon(&hub).await;
    let daemon_temp = daemon.id.clone();

    // joiner：没有 device token，但升级请求带了 web 登录 cookie
    let mut web = FakeDevice::connect_with(&hub, None, DeviceKind::Web, "Chrome", &[("cookie", cookie.as_str())]).await;
    let web_temp = web.id.clone();
    web.send_control(&Control::PairJoin { match_id }).await;
    expect_matched(&mut daemon, &mut web).await;

    // 握手帧走临时 id
    web.send_env(&daemon_temp, Kind::Handshake, body_of(40, 1)).await;
    assert_eq!(daemon.recv_env().await.from, web_temp);
    daemon.send_env(&web_temp, Kind::Handshake, body_of(80, 2)).await;
    assert_eq!(web.recv_env().await.body, body_of(80, 2));

    web.send_control(&Control::PairConfirm { peer: daemon_temp.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: web_temp.clone() }).await;

    let (web_id, web_token) = match web.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_ne!(peer, daemon_temp, "joiner learns the host's permanent id");
            (device_id.expect("joiner device id"), device_token.expect("joiner token"))
        }
        other => panic!("{other:?}"),
    };
    let (daemon_id, daemon_token) = match daemon.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, web_id, "host records the joiner by its permanent id");
            (device_id.expect("host device id"), device_token.expect("host gets a device token"))
        }
        other => panic!("{other:?}"),
    };
    assert_ne!(daemon_id, daemon_temp);
    assert_ne!(web_id, web_temp);
    assert_ne!(daemon_token, web_token);

    // 双方都收到对方的 peer_online（正式 id）
    assert!(matches!(daemon.recv_control().await, Control::PeerOnline { device } if device == web_id));
    assert!(matches!(web.recv_control().await, Control::PeerOnline { device } if device == daemon_id));

    // 库：两条设备记录都归 web cookie 的账号，配对行用正式 id，没有临时 id 的痕迹
    let store = hub.handle.store();
    let d = store.get_device(&daemon_id).await.unwrap().unwrap();
    assert_eq!((d.kind, d.name.as_str(), d.platform.as_str()), (DeviceKind::Daemon, "fresh-mac", "test"));
    assert_eq!(d.account_id, hub.admin_account_id());
    let w = store.get_device(&web_id).await.unwrap().unwrap();
    assert_eq!((w.kind, w.account_id.as_str()), (DeviceKind::Web, hub.admin_account_id().as_str()));
    assert!(store.is_paired(&daemon_id, &web_id).await.unwrap());
    assert!(store.get_device(&daemon_temp).await.unwrap().is_none());
    assert!(store.get_device(&web_temp).await.unwrap().is_none());
    assert_eq!(store.list_pairings_of_account(&hub.admin_account_id()).await.unwrap().len(), 1);
    let audit = store.list_audit(50, None).await.unwrap();
    assert_eq!(audit.iter().filter(|e| e.event == "device_paired").count(), 2);

    // 同一条连接立刻可以用新 id 互发 data
    let seq = daemon.send_env_as(&daemon_id, &web_id, Kind::Data, body_of(300, 7)).await;
    let got = web.recv_env().await;
    assert_eq!(
        (got.kind, got.from.as_str(), got.seq, got.body),
        (Kind::Data, daemon_id.as_str(), seq, body_of(300, 7))
    );
    web.send_env_as(&web_id, &daemon_id, Kind::Data, body_of(10, 8)).await;
    assert_eq!(daemon.recv_env().await.body, body_of(10, 8));
    // 旧的临时 id 不再有效
    daemon.send_env_as(&daemon_temp, &web_id, Kind::Data, body_of(10, 9)).await;
    web.expect_silence(SHORT).await;

    // 设备列表里两台都在线、互相配对
    let list: Value = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|d| d["online"] == true));
    assert_eq!(list.iter().find(|d| d["id"] == daemon_id).unwrap()["paired_with"], json!([web_id]));

    // daemon 之后用拿到的 token 重连：身份不变，peers_online 里有 web
    drop(daemon);
    assert!(matches!(web.recv_control().await, Control::PeerOffline { device } if device == daemon_id));
    let again = FakeDevice::connect(&hub, Some(&daemon_token), DeviceKind::Daemon, "fresh-mac").await;
    assert_eq!(again.id, daemon_id);
    assert_eq!(again.peers_online, vec![web_id.clone()]);
}

#[tokio::test]
async fn tokenless_daemon_host_with_an_authenticated_device_joiner() {
    let hub = TestHub::start().await;
    let web = hub.seed_device(DeviceKind::Web, "browser").await;
    let mut browser = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "browser").await;
    let (mut daemon, match_id) = tokenless_daemon(&hub).await;
    let daemon_temp = daemon.id.clone();

    browser.send_control(&Control::PairJoin { match_id }).await;
    expect_matched(&mut daemon, &mut browser).await;
    browser.send_control(&Control::PairConfirm { peer: daemon_temp.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: web.id.clone() }).await;

    // joiner 已是正式设备：不发新身份；host 拿到新身份，账号取 joiner 的
    let daemon_id = match daemon.recv_control().await {
        Control::PairDone { peer, device_id: Some(id), device_token: Some(_) } => {
            assert_eq!(peer, web.id);
            id
        }
        other => panic!("{other:?}"),
    };
    match browser.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, daemon_id);
            assert!(device_id.is_none() && device_token.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(daemon.recv_control().await, Control::PeerOnline { device } if device == web.id));
    assert!(matches!(browser.recv_control().await, Control::PeerOnline { device } if device == daemon_id));
    assert!(hub.handle.store().is_paired(&daemon_id, &web.id).await.unwrap());
    browser.send_env(&daemon_id, Kind::Data, vec![1, 2, 3]).await;
    assert_eq!(daemon.recv_env().await.body, vec![1, 2, 3]);
}

#[tokio::test]
async fn tokenless_host_and_tokenless_joiner_without_account_is_forbidden() {
    let hub = TestHub::start().await;
    let (mut daemon, match_id) = tokenless_daemon(&hub).await;
    let daemon_temp = daemon.id.clone();
    // joiner 既没有 device token 也没有 web cookie
    let mut cli = FakeDevice::connect(&hub, None, DeviceKind::Cli, "cli").await;
    let cli_temp = cli.id.clone();
    cli.send_control(&Control::PairJoin { match_id }).await;
    expect_matched(&mut daemon, &mut cli).await;
    cli.send_control(&Control::PairConfirm { peer: daemon_temp }).await;
    daemon.send_control(&Control::PairConfirm { peer: cli_temp }).await;
    assert!(matches!(cli.recv_control().await, Control::PairFailed { reason: PairFailReason::Forbidden }));
    assert!(matches!(daemon.recv_control().await, Control::PairFailed { reason: PairFailReason::Forbidden }));

    // 什么都没落库
    let store = hub.handle.store();
    assert_eq!(store.stats().await.unwrap().devices, 0);
    assert!(store.list_pairings_of_account(&hub.admin_account_id()).await.unwrap().is_empty());
    // 连接仍是临时身份：不能发 data
    daemon.send_env(&cli.id, Kind::Data, vec![1]).await;
    cli.expect_silence(SHORT).await;
}

#[tokio::test]
async fn permanent_host_with_tokenless_joiner_and_no_cookie_still_joins_the_hosts_account() {
    // 回归：原有行为（host 是正式设备，joiner 无账号 => 归入 host 账号）不受影响
    let hub = TestHub::start().await;
    let d = hub.seed_device(DeviceKind::Daemon, "d").await;
    let mut daemon = FakeDevice::connect(&hub, Some(&d.token), DeviceKind::Daemon, "d").await;
    let (_, match_id) = new_code();
    daemon.pair_begin(&match_id).await;
    let mut cli = FakeDevice::connect(&hub, None, DeviceKind::Cli, "cli").await;
    cli.send_control(&Control::PairJoin { match_id }).await;
    expect_matched(&mut daemon, &mut cli).await;
    let temp = cli.id.clone();
    cli.send_control(&Control::PairConfirm { peer: d.id.clone() }).await;
    daemon.send_control(&Control::PairConfirm { peer: temp }).await;
    let new_id = match cli.recv_control().await {
        Control::PairDone { device_id: Some(id), device_token: Some(_), .. } => id,
        other => panic!("{other:?}"),
    };
    let store = hub.handle.store();
    assert_eq!(store.get_device(&new_id).await.unwrap().unwrap().account_id, hub.admin_account_id());
}
