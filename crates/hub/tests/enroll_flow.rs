//! 账号内接入撮合（ADR-36，docs/spec/hub.md § 4.1，docs/protocol.md § 4.4）的集成测试。
//!
//! hub 只确认「账号 + 节点」并路由 `handshake`，这里用真实的 `EnrollFrame`、CPace、XXpsk3、IK 把整条链路走通，
//! 证明 hub 对握手体一无所知也不碍事；其余用例覆盖拒绝、并发上限、限速、超时、取消 / 断线通知和转发隔离。

mod common;

use std::time::Duration;

use anycon_crypto::{
    derive_enroll_scan_psk, enroll_channel_id, enroll_sid, generate_enroll_nonce, generate_enroll_secret,
    normalize_enroll_code, Cpace, Handshake, Keypair, Role,
};
use anycon_hub::pairing::Origin;
use anycon_hub::store::NewDevice;
use anycon_hub::util::{hash_token, new_id, new_secret, now_ms};
use anycon_protocol::{
    Control, DeviceKind, DropReason, EnrollFailReason, EnrollFrame, EnrollHint, EnrollMethod, EnrollReplyStatus, Kind,
    PairFailReason,
};
use common::*;

const SHORT: Duration = Duration::from_millis(300);

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 一个节点（daemon）：已入库、已连上 hub。
struct Node {
    seeded: Seeded,
    dev: FakeDevice,
}

async fn node(hub: &TestHub) -> Node {
    let seeded = hub.seed_device(DeviceKind::Daemon, "mac-mini").await;
    let dev = FakeDevice::connect(hub, Some(&seeded.token), DeviceKind::Daemon, "mac-mini").await;
    Node { seeded, dev }
}

/// 带登录 cookie 的临时连接（浏览器，或 CLI `anycon connect` 里临时登录拿到的会话）。
async fn temp_with_cookie(hub: &TestHub, cookie: &str, kind: DeviceKind, name: &str) -> FakeDevice {
    FakeDevice::connect_with(hub, None, kind, name, &[("cookie", cookie)]).await
}

/// 另一个账号：返回 (账号 id, 带 `anycon_session` 的 cookie)。
async fn other_account(hub: &TestHub) -> (String, String) {
    let store = hub.handle.store();
    let acc = store.create_account("someone else", false, now_ms()).await.unwrap();
    let raw = new_secret();
    store
        .create_web_session(&acc.id, &hash_token(&raw), "test", "127.0.0.0/24", now_ms(), now_ms() + 3_600_000)
        .await
        .unwrap();
    (acc.id, format!("anycon_session={raw}"))
}

async fn seed_in(hub: &TestHub, account: &str, kind: DeviceKind, name: &str) -> Seeded {
    let (id, token) = (new_id(), new_secret());
    hub.handle
        .store()
        .create_device(
            NewDevice {
                id: id.clone(),
                account_id: account.to_string(),
                kind,
                name: name.into(),
                platform: "test".into(),
                origin: None,
                token_hash: hash_token(&token),
            },
            now_ms(),
        )
        .await
        .unwrap();
    Seeded { id, token }
}

/// 发 `enroll_begin`，要求 `enroll_started`；daemon 那边的 `enroll_request` 由调用方自己取。
async fn begin_ok(n: &mut FakeDevice, node: &str) -> (String, u64) {
    n.send_control(&Control::EnrollBegin { node: node.to_string() }).await;
    match n.recv_control().await {
        Control::EnrollStarted { enroll_id, node: n_node, expires_at_ms } => {
            assert_eq!(n_node, node);
            assert_eq!(enroll_id.len(), 32, "128-bit hex id");
            (enroll_id, expires_at_ms)
        }
        other => panic!("expected enroll_started, got {other:?}"),
    }
}

async fn recv_request(d: &mut FakeDevice) -> (String, String, u64) {
    match d.recv_control().await {
        Control::EnrollRequest { enroll_id, peer, expires_at_ms } => (enroll_id, peer, expires_at_ms),
        other => panic!("expected enroll_request, got {other:?}"),
    }
}

/// 发 `enroll_begin`，要求 `enroll_failed { reason }`（没有 id）。
async fn begin_refused(n: &mut FakeDevice, node: &str, reason: EnrollFailReason) {
    n.send_control(&Control::EnrollBegin { node: node.to_string() }).await;
    match n.recv_control().await {
        Control::EnrollFailed { enroll_id: None, reason: got } => assert_eq!(got, reason),
        other => panic!("expected enroll_failed {{ {reason:?} }}, got {other:?}"),
    }
}

async fn expect_failed(d: &mut FakeDevice, id: &str, reason: EnrollFailReason) {
    match d.recv_control().await {
        Control::EnrollFailed { enroll_id: Some(got), reason: r } => assert_eq!((got.as_str(), r), (id, reason)),
        other => panic!("expected enroll_failed {{ {reason:?} }} for {id}, got {other:?}"),
    }
}

fn frame_bytes(f: &EnrollFrame) -> Vec<u8> {
    rmp_serde::to_vec_named(f).unwrap()
}

fn hello(method: EnrollMethod, approval: bool, nonce: &[u8]) -> EnrollFrame {
    EnrollFrame::Hello {
        v: 1,
        method,
        approval,
        nonce: hex::encode(nonce),
        hint: EnrollHint { name: "Chrome · Windows".into(), kind: DeviceKind::Web, platform: "windows".into() },
    }
}

async fn send_frame(from: &mut FakeDevice, to: &str, f: &EnrollFrame) {
    from.send_env(to, Kind::Handshake, frame_bytes(f)).await;
}

async fn recv_frame(at: &mut FakeDevice, from: &str) -> EnrollFrame {
    let env = at.recv_env().await;
    assert_eq!((env.kind, env.from.as_str()), (Kind::Handshake, from), "{env:?}");
    rmp_serde::from_slice(&env.body).expect("an EnrollFrame, forwarded byte for byte")
}

fn hex32(s: &str) -> Vec<u8> {
    let b = hex::decode(s).unwrap();
    assert!(!b.is_empty());
    b
}

/// 两端各自的 XXpsk3 握手状态走完三条 `noise` 帧（经 hub 转发）。N 是 initiator。
/// 返回 (N 的钥匙对, D 的钥匙对)。
async fn run_xxpsk3(n: &mut FakeDevice, node_id: &str, d: &mut FakeDevice, psk: &[u8; 32]) -> (Keypair, Keypair) {
    let (n_kp, d_kp) = (Keypair::generate(), Keypair::generate());
    let mut hn = Handshake::pairing(Role::Initiator, &n_kp, psk).unwrap();
    let mut hd = Handshake::pairing(Role::Responder, &d_kp, psk).unwrap();
    let n_id = n.id.clone();

    let m1 = hn.write(b"").unwrap();
    send_frame(n, node_id, &EnrollFrame::Noise { msg: hex::encode(&m1) }).await;
    let EnrollFrame::Noise { msg } = recv_frame(d, &n_id).await else { panic!("expected noise") };
    hd.read(&hex32(&msg)).unwrap();

    let m2 = hd.write(br#"{"name":"mac-mini","version":"0.1.0"}"#).unwrap();
    send_frame(d, &n_id, &EnrollFrame::Noise { msg: hex::encode(&m2) }).await;
    let EnrollFrame::Noise { msg } = recv_frame(n, node_id).await else { panic!("expected noise") };
    hn.read(&hex32(&msg)).unwrap();

    let m3 = hn.write(br#"{"name":"Chrome","kind":"web","platform":"test","version":"0"}"#).unwrap();
    send_frame(n, node_id, &EnrollFrame::Noise { msg: hex::encode(&m3) }).await;
    let EnrollFrame::Noise { msg } = recv_frame(d, &n_id).await else { panic!("expected noise") };
    hd.read(&hex32(&msg)).unwrap();
    assert!(hn.is_finished() && hd.is_finished(), "PSKs agree, so the hub in the middle changed nothing");
    (n_kp, d_kp)
}

/// 重连握手 IK（新设备发起）经 hub 转发，然后 Noise 传输上互发一条消息。双方都要已经是 hub 眼里的配对设备。
async fn ik_and_talk(n: &mut FakeDevice, n_id: &str, n_kp: &Keypair, d: &mut FakeDevice, d_id: &str, d_kp: &Keypair) {
    let mut hn = Handshake::reconnect(Role::Initiator, n_kp, Some(&d_kp.public)).unwrap();
    let mut hd = Handshake::reconnect(Role::Responder, d_kp, Some(&n_kp.public)).unwrap();
    n.send_env_as(n_id, d_id, Kind::Handshake, hn.write(b"").unwrap()).await;
    hd.read(&d.recv_env().await.body).unwrap();
    d.send_env(n_id, Kind::Handshake, hd.write(b"").unwrap()).await;
    hn.read(&n.recv_env().await.body).unwrap();
    let (mut tn, mut td) = (hn.into_transport().unwrap(), hd.into_transport().unwrap());
    n.send_env_as(n_id, d_id, Kind::Data, tn.encrypt(b"hello node").unwrap()).await;
    let got = d.recv_env().await;
    assert_eq!((got.kind, got.from.as_str()), (Kind::Data, n_id));
    assert_eq!(td.decrypt(&got.body).unwrap(), b"hello node");
    d.send_env(n_id, Kind::Data, td.encrypt(b"hello device").unwrap()).await;
    assert_eq!(tn.decrypt(&n.recv_env().await.body).unwrap(), b"hello device");
}

fn enrolls_dump(hub: &TestHub) -> String {
    format!("{:?}", hub.handle.state.enrolls)
}

// ---------------------------------------------------------------------------
// 成功路径
// ---------------------------------------------------------------------------

/// 临时连接（升级请求带登录 cookie）+ 接入码：CPace → XXpsk3 → `pair_confirm` → `pair_done` → IK 通信。
#[tokio::test]
async fn temporary_connection_with_cookie_enrolls_end_to_end() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let node_id = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "Chrome · macOS").await;
    let temp_id = n.id.clone();

    let before = now_ms();
    let (enroll_id, expires_at_ms) = begin_ok(&mut n, &node_id).await;
    assert!((before + 299_000..=now_ms() + 300_000).contains(&expires_at_ms), "5 minute TTL: {expires_at_ms}");
    let (d_enroll_id, peer, d_expires) = recv_request(&mut node.dev).await;
    assert_eq!((d_enroll_id.as_str(), peer.as_str(), d_expires), (enroll_id.as_str(), temp_id.as_str(), expires_at_ms));

    // N 的 hello（code，用已有的码）→ D 的 reply { ready, nonce, cpace }
    let code = normalize_enroll_code("123 456").unwrap();
    let (nonce_n, nonce_d) = (generate_enroll_nonce(), generate_enroll_nonce());
    send_frame(&mut n, &node_id, &hello(EnrollMethod::Code, false, &nonce_n)).await;
    let EnrollFrame::Hello { method, approval, nonce, .. } = recv_frame(&mut node.dev, &temp_id).await else {
        panic!("expected hello")
    };
    assert_eq!((method, approval, nonce), (EnrollMethod::Code, false, hex::encode(nonce_n)));

    let sid = enroll_sid(&nonce_n, &nonce_d);
    let ci = enroll_channel_id(&node_id, "code");
    let cpace_d = Cpace::new(code.as_bytes(), &ci, &sid).unwrap();
    let reply = EnrollFrame::Reply {
        status: EnrollReplyStatus::Ready,
        nonce: Some(hex::encode(nonce_d)),
        reason: None,
        retry_after_ms: None,
        attempts_left: None,
        salt: None,
        cpace: Some(hex::encode(cpace_d.share())),
    };
    send_frame(&mut node.dev, &temp_id, &reply).await;
    let EnrollFrame::Reply { cpace: Some(share_d), .. } = recv_frame(&mut n, &node_id).await else {
        panic!("expected reply")
    };
    let cpace_n = Cpace::new(code.as_bytes(), &ci, &sid).unwrap();
    send_frame(&mut n, &node_id, &EnrollFrame::Cpace { share: hex::encode(cpace_n.share()) }).await;
    let EnrollFrame::Cpace { share: share_n } = recv_frame(&mut node.dev, &temp_id).await else {
        panic!("expected cpace")
    };
    let psk_n = cpace_n.finish(Role::Initiator, &hex32(&share_d)).unwrap();
    let psk_d = cpace_d.finish(Role::Responder, &hex32(&share_n)).unwrap();
    assert_eq!(psk_n, psk_d);

    let (n_kp, d_kp) = run_xxpsk3(&mut n, &node_id, &mut node.dev, &psk_n).await;

    // 收尾：配对确认前 hub 什么都没写
    let store = hub.handle.store();
    assert!(store.peers_of(&node_id).await.unwrap().is_empty());
    n.send_control(&Control::PairConfirm { peer: node_id.clone() }).await;
    node.dev.expect_silence(SHORT).await;
    node.dev.send_control(&Control::PairConfirm { peer: temp_id.clone() }).await;

    let (new_id, new_token) = match n.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, node_id);
            (device_id.expect("a new device identity"), device_token.expect("and its token"))
        }
        other => panic!("{other:?}"),
    };
    assert_eq!((new_id.len(), new_token.len()), (32, 64));
    match node.dev.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, new_id, "the daemon learns the new device by its permanent id");
            assert!(device_id.is_none() && device_token.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(node.dev.recv_control().await, Control::PeerOnline { device } if device == new_id));
    assert!(matches!(n.recv_control().await, Control::PeerOnline { device } if device == node_id));

    // 库里：设备（账号 = 登录的账号，名字来自 ClientInfo）、配对、审计；会话已经结束
    let dev = store.get_device(&new_id).await.unwrap().unwrap();
    assert_eq!(
        (dev.name.as_str(), dev.kind, dev.account_id.as_str()),
        ("Chrome · macOS", DeviceKind::Web, hub.admin_account_id().as_str())
    );
    assert!(store.is_paired(&node_id, &new_id).await.unwrap());
    assert!(store.find_device_by_token_hash(&hash_token(&new_token)).await.unwrap().is_some());
    assert!(store.find_device_by_token_hash(&new_token).await.unwrap().is_none(), "only the hash is stored");
    assert!(enrolls_dump(&hub).contains("sessions: 0"), "{}", enrolls_dump(&hub));
    let audit = store.list_audit(50, None).await.unwrap();
    let started = audit.iter().find(|e| e.event == "enroll_started").expect("enroll_started is audited");
    assert_eq!(started.account_id.as_deref(), Some(hub.admin_account_id().as_str()));
    assert_eq!(started.ip_prefix.as_deref(), Some("127.0.0.0/24"));
    assert!(started.device_id.is_none(), "a temporary connection has no device yet");
    let d = started.detail.as_ref().unwrap();
    assert_eq!((d["node"].as_str(), d["enroll_id"].as_str()), (Some(node_id.as_str()), Some(enroll_id.as_str())));
    let enrolled = audit.iter().find(|e| e.event == "device_enrolled").expect("device_enrolled is audited");
    assert_eq!(enrolled.device_id.as_deref(), Some(new_id.as_str()));
    assert_eq!(enrolled.detail.as_ref().unwrap()["node"], serde_json::json!(node_id));
    assert_eq!(enrolled.detail.as_ref().unwrap()["new_device"], serde_json::json!(true));
    assert!(!audit.iter().any(|e| e.event == "device_paired"), "an enrollment is not also a short-code pairing");

    // 升级之后同一条连接直接是正式设备：IK 握手 + 加密传输都能走
    ik_and_talk(&mut n, &new_id, &n_kp, &mut node.dev, &node_id, &d_kp).await;

    // 带新 token 重连：peers_online 里有节点
    let again = FakeDevice::connect(&hub, Some(&new_token), DeviceKind::Web, "Chrome · macOS").await;
    assert_eq!((again.id.as_str(), again.peers_online.as_slice()), (new_id.as_str(), &[node_id][..]));
}

/// 正式设备发起（`scan`）：不需要 cookie；已经是设备，所以不发新身份。
#[tokio::test]
async fn paired_device_enrolls_with_a_qr_secret_and_keeps_its_identity() {
    let hub = TestHub::start().await;
    let mut node = node(&hub).await;
    let node_id = node.seeded.id.clone();
    // 这台设备是本账号的 web 设备，但还没和这台节点配过对
    let web = hub.seed_device(DeviceKind::Web, "my browser").await;
    let mut n = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "my browser").await;

    let (enroll_id, expires_at_ms) = begin_ok(&mut n, &node_id).await;
    assert_eq!(recv_request(&mut node.dev).await, (enroll_id, web.id.clone(), expires_at_ms));

    // scan：N 的 hello，D 回 wait_approval，批准后回 ready；双方用 r 派生 PSK，不跑 CPace
    let r = generate_enroll_secret();
    send_frame(&mut n, &node_id, &hello(EnrollMethod::Scan, false, &generate_enroll_nonce())).await;
    assert!(matches!(recv_frame(&mut node.dev, &web.id).await, EnrollFrame::Hello { method: EnrollMethod::Scan, .. }));
    for status in [EnrollReplyStatus::WaitApproval, EnrollReplyStatus::Ready] {
        let reply = EnrollFrame::Reply {
            status,
            nonce: None,
            reason: None,
            retry_after_ms: None,
            attempts_left: None,
            salt: None,
            cpace: None,
        };
        send_frame(&mut node.dev, &web.id, &reply).await;
        assert!(matches!(recv_frame(&mut n, &node_id).await, EnrollFrame::Reply { status: s, .. } if s == status));
    }
    let psk = derive_enroll_scan_psk(&r, &node_id);
    let (n_kp, d_kp) = run_xxpsk3(&mut n, &node_id, &mut node.dev, &psk).await;

    n.send_control(&Control::PairConfirm { peer: node_id.clone() }).await;
    node.dev.send_control(&Control::PairConfirm { peer: web.id.clone() }).await;
    match n.recv_control().await {
        Control::PairDone { peer, device_id, device_token } => {
            assert_eq!(peer, node_id);
            assert!(device_id.is_none() && device_token.is_none(), "an authenticated device keeps its identity");
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(node.dev.recv_control().await, Control::PairDone { peer, .. } if peer == web.id));
    assert!(matches!(node.dev.recv_control().await, Control::PeerOnline { device } if device == web.id));
    assert!(matches!(n.recv_control().await, Control::PeerOnline { device } if device == node_id));
    let store = hub.handle.store();
    assert!(store.is_paired(&node_id, &web.id).await.unwrap());
    let audit = store.list_audit(50, None).await.unwrap();
    let enrolled = audit.iter().find(|e| e.event == "device_enrolled").expect("device_enrolled is audited");
    assert_eq!(enrolled.device_id.as_deref(), Some(web.id.as_str()));
    assert_eq!(enrolled.detail.as_ref().unwrap()["new_device"], serde_json::json!(false));
    let started = audit.iter().find(|e| e.event == "enroll_started").unwrap();
    assert_eq!(started.device_id.as_deref(), Some(web.id.as_str()), "a formal initiator is named in the audit entry");

    ik_and_talk(&mut n, &web.id, &n_kp, &mut node.dev, &node_id, &d_kp).await;
}

/// 已经和这台节点配过对的设备（比如丢了本地的节点记录）也放行，收尾和重新配对一样，库里的配对不重复。
#[tokio::test]
async fn a_device_already_paired_with_the_node_may_enroll_again() {
    let hub = TestHub::start().await;
    let mut node = node(&hub).await;
    let node_id = node.seeded.id.clone();
    let web = hub.seed_device(DeviceKind::Web, "my browser").await;
    hub.seed_paired(&node.seeded, &web).await;
    let mut n = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "my browser").await;
    assert_eq!(n.peers_online, vec![node_id.clone()]);
    assert!(matches!(node.dev.recv_control().await, Control::PeerOnline { device } if device == web.id));

    let (enroll_id, _) = begin_ok(&mut n, &node_id).await;
    assert_eq!(recv_request(&mut node.dev).await.0, enroll_id);
    send_frame(&mut n, &node_id, &hello(EnrollMethod::Password, false, &generate_enroll_nonce())).await;
    assert!(matches!(recv_frame(&mut node.dev, &web.id).await, EnrollFrame::Hello { .. }));
    n.send_control(&Control::PairConfirm { peer: node_id.clone() }).await;
    node.dev.send_control(&Control::PairConfirm { peer: web.id.clone() }).await;
    assert!(
        matches!(n.recv_control().await, Control::PairDone { peer, device_id: None, device_token: None } if peer == node_id)
    );
    assert!(matches!(node.dev.recv_control().await, Control::PairDone { peer, .. } if peer == web.id));
    assert_eq!(hub.handle.store().peers_of(&node_id).await.unwrap(), vec![web.id.clone()]);
    assert!(enrolls_dump(&hub).contains("sessions: 0"));
}

// ---------------------------------------------------------------------------
// 拒绝
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enroll_begin_is_refused_without_an_account_and_for_daemons() {
    let hub = TestHub::start_with(|c| c.limits.enroll_begin_per_min_per_ip = 1000).await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    use EnrollFailReason::Forbidden;

    // 没有账号：临时连接没带 cookie / cookie 无效
    let mut anon = FakeDevice::connect(&hub, None, DeviceKind::Web, "anon").await;
    begin_refused(&mut anon, &nid, Forbidden).await;
    let mut bogus = temp_with_cookie(&hub, "anycon_session=not-a-real-session", DeviceKind::Cli, "bogus").await;
    begin_refused(&mut bogus, &nid, Forbidden).await;
    // 带了账号 cookie 的临时连接，但自称 daemon
    let mut tmp_daemon = temp_with_cookie(&hub, &admin, DeviceKind::Daemon, "d").await;
    begin_refused(&mut tmp_daemon, &nid, Forbidden).await;
    // 正式的 daemon 设备（同账号的另一台节点）
    let second = hub.seed_device(DeviceKind::Daemon, "second-node").await;
    let mut second_dev = FakeDevice::connect(&hub, Some(&second.token), DeviceKind::Daemon, "second-node").await;
    begin_refused(&mut second_dev, &nid, Forbidden).await;
    // 库里是 daemon、连接时自称 web：以库里的为准
    let liar = hub.seed_device(DeviceKind::Daemon, "liar").await;
    let mut liar_dev = FakeDevice::connect(&hub, Some(&liar.token), DeviceKind::Web, "liar").await;
    begin_refused(&mut liar_dev, &nid, Forbidden).await;

    assert!(enrolls_dump(&hub).contains("sessions: 0"));
    node.dev.expect_silence(SHORT).await;
}

#[tokio::test]
async fn enroll_begin_target_must_be_an_online_unrevoked_daemon_of_the_same_account() {
    let hub = TestHub::start_with(|c| c.limits.enroll_begin_per_min_per_ip = 1000).await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let (other_acc, other_cookie) = other_account(&hub).await;
    let mut me = temp_with_cookie(&hub, &admin, DeviceKind::Web, "me").await;
    use EnrollFailReason::{NotFound, Offline};

    // 别人账号下的 daemon，哪怕在线：not_found（不区分「不存在」和「不是你的」）
    let foreign = seed_in(&hub, &other_acc, DeviceKind::Daemon, "foreign").await;
    let mut foreign_dev = FakeDevice::connect(&hub, Some(&foreign.token), DeviceKind::Daemon, "foreign").await;
    begin_refused(&mut me, &foreign.id, NotFound).await;
    // 反过来：别人的登录态对我的节点
    let mut stranger = temp_with_cookie(&hub, &other_cookie, DeviceKind::Web, "stranger").await;
    begin_refused(&mut stranger, &nid, NotFound).await;
    // 本账号的、但不是 daemon
    let web = hub.seed_device(DeviceKind::Web, "a browser").await;
    let _web_dev = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "a browser").await;
    begin_refused(&mut me, &web.id, NotFound).await;
    // 不存在的 id
    begin_refused(&mut me, &new_id(), NotFound).await;
    begin_refused(&mut me, "not even an id", NotFound).await;
    // 吊销的 daemon
    let revoked = hub.seed_device(DeviceKind::Daemon, "revoked").await;
    assert!(hub.handle.state.revoke_device(&revoked.id, None).await.unwrap());
    begin_refused(&mut me, &revoked.id, NotFound).await;
    // 本账号的 daemon 但不在线
    let offline = hub.seed_device(DeviceKind::Daemon, "offline").await;
    begin_refused(&mut me, &offline.id, Offline).await;

    assert!(enrolls_dump(&hub).contains("sessions: 0"));
    node.dev.expect_silence(SHORT).await;
    foreign_dev.expect_silence(SHORT).await;
}

/// cookie 只在 WebSocket 升级时读一次：CLI 可以在连接建好之后马上 `/auth/logout`，已经建好的连接照常接入；
/// 登出之后再用这个 cookie 开的新连接没有账号。
#[tokio::test]
async fn the_session_cookie_is_only_read_at_upgrade_so_logging_out_afterwards_is_fine() {
    let hub = TestHub::start().await;
    let cookie = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &cookie, DeviceKind::Cli, "anycon connect").await;

    let res = hub.send(reqwest::Method::POST, "/auth/logout", &cookie, serde_json::Value::Null).await;
    assert!(res.status().is_success(), "{}", res.status());
    assert_eq!(hub.get("/api/me", &cookie).await.status(), 401, "the login really is gone");

    let (id, _) = begin_ok(&mut n, &nid).await;
    assert_eq!(recv_request(&mut node.dev).await.0, id);
    let mut late = temp_with_cookie(&hub, &cookie, DeviceKind::Cli, "late").await;
    begin_refused(&mut late, &nid, EnrollFailReason::Forbidden).await;
}

// ---------------------------------------------------------------------------
// 并发上限
// ---------------------------------------------------------------------------

#[tokio::test]
async fn at_most_three_sessions_per_daemon_and_one_per_connection() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut second = self::node(&hub).await;
    let sid = second.seeded.id.clone();

    let mut first_three = Vec::new();
    for i in 0..3 {
        let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, &format!("n{i}")).await;
        let (id, _) = begin_ok(&mut n, &nid).await;
        let (rid, peer, _) = recv_request(&mut node.dev).await;
        assert_eq!((rid.as_str(), peer.as_str()), (id.as_str(), n.id.as_str()));
        first_three.push((n, id));
    }
    // 第 4 个：busy
    let mut n4 = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n4").await;
    begin_refused(&mut n4, &nid, EnrollFailReason::Busy).await;
    node.dev.expect_silence(SHORT).await;
    // 同一条连接再来一个：busy，哪怕是另一台 daemon
    begin_refused(&mut first_three[0].0, &nid, EnrollFailReason::Busy).await;
    // 另一台 daemon 的名额不受影响
    let (id4, _) = begin_ok(&mut n4, &sid).await;
    assert_eq!(recv_request(&mut second.dev).await.0, id4);

    // 取消一个，名额就空出来了
    let id0 = first_three[0].1.clone();
    let n0 = &mut first_three[0].0;
    n0.send_control(&Control::EnrollCancel { enroll_id: id0.clone() }).await;
    expect_failed(&mut node.dev, &id0, EnrollFailReason::Cancelled).await;
    let mut n5 = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n5").await;
    let (id5, _) = begin_ok(&mut n5, &nid).await;
    assert_eq!(recv_request(&mut node.dev).await.0, id5);
    // 取消过的连接也可以再开一个（等 n5 占满后又是 busy）
    begin_refused(&mut n4, &nid, EnrollFailReason::Busy).await;
}

// ---------------------------------------------------------------------------
// 限速
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enroll_begin_is_limited_per_ip_and_the_receipt_shares_the_pair_failed_budget() {
    let hub = TestHub::start_with(|c| {
        c.limits.enroll_begin_per_min_per_ip = 3;
        c.limits.pair_join_per_min = 1;
    })
    .await;
    let admin = hub.login_admin().await;
    let mut me = temp_with_cookie(&hub, &admin, DeviceKind::Web, "me").await;
    let ghost = new_id();
    // 前 3 次放行（目标不存在，所以是 not_found），第 4 次超限
    for _ in 0..3 {
        begin_refused(&mut me, &ghost, EnrollFailReason::NotFound).await;
    }
    begin_refused(&mut me, &ghost, EnrollFailReason::RateLimited).await;
    // 同一秒内的回执被丢弃（每连接每秒一条）
    me.send_control(&Control::EnrollBegin { node: ghost.clone() }).await;
    me.expect_silence(SHORT).await;

    // 和 pair_failed { rate_limited } 共用同一份预算：先用掉它，再来一次超限的 enroll_begin 就没有回执
    let mut other = temp_with_cookie(&hub, &admin, DeviceKind::Web, "other").await;
    other.send_control(&Control::PairJoin { match_id: "0123456789abcdef".into() }).await;
    assert!(matches!(other.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound }));
    other.send_control(&Control::PairJoin { match_id: "0123456789abcdef".into() }).await;
    assert!(matches!(other.recv_control().await, Control::PairFailed { reason: PairFailReason::RateLimited }));
    other.send_control(&Control::EnrollBegin { node: ghost.clone() }).await;
    other.expect_silence(SHORT).await;
    // 预算一秒后恢复
    tokio::time::sleep(Duration::from_millis(1100)).await;
    begin_refused(&mut other, &ghost, EnrollFailReason::RateLimited).await;
}

#[tokio::test]
async fn enroll_begin_is_limited_per_account() {
    let hub = TestHub::start_with(|c| c.limits.enroll_begin_per_hour_per_account = 2).await;
    let admin = hub.login_admin().await;
    let (_, other_cookie) = other_account(&hub).await;
    let mut me = temp_with_cookie(&hub, &admin, DeviceKind::Web, "me").await;
    let ghost = new_id();
    begin_refused(&mut me, &ghost, EnrollFailReason::NotFound).await;
    begin_refused(&mut me, &ghost, EnrollFailReason::NotFound).await;
    begin_refused(&mut me, &ghost, EnrollFailReason::RateLimited).await;
    // 换一条连接、同一个账号也一样；别的账号不受影响
    let mut me2 = temp_with_cookie(&hub, &admin, DeviceKind::Cli, "me again").await;
    begin_refused(&mut me2, &ghost, EnrollFailReason::RateLimited).await;
    let mut stranger = temp_with_cookie(&hub, &other_cookie, DeviceKind::Web, "stranger").await;
    begin_refused(&mut stranger, &ghost, EnrollFailReason::NotFound).await;
}

#[tokio::test]
async fn enroll_begin_counts_toward_the_daily_pairing_quota_when_one_is_configured() {
    let hub = TestHub::start_with(|c| c.limits.daily_pairings_per_account = Some(2)).await;
    let admin = hub.login_admin().await;
    let mut me = temp_with_cookie(&hub, &admin, DeviceKind::Web, "me").await;
    let ghost = new_id();
    begin_refused(&mut me, &ghost, EnrollFailReason::NotFound).await;
    begin_refused(&mut me, &ghost, EnrollFailReason::NotFound).await;
    // 超了配额：`quota`，不是 `rate_limited`，也不占回执预算（连着两次都有回执）
    begin_refused(&mut me, &ghost, EnrollFailReason::Quota).await;
    begin_refused(&mut me, &ghost, EnrollFailReason::Quota).await;
}

// ---------------------------------------------------------------------------
// 超时、取消、断线、吊销
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sessions_time_out_for_both_sides_and_stop_forwarding() {
    let hub = TestHub::start_with(|c| c.timing.enroll_ttl = Duration::from_millis(700)).await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;
    send_frame(&mut n, &nid, &hello(EnrollMethod::Scan, false, &generate_enroll_nonce())).await;
    assert!(matches!(recv_frame(&mut node.dev, &n.id.clone()).await, EnrollFrame::Hello { .. }));

    expect_failed(&mut n, &id, EnrollFailReason::Timeout).await;
    expect_failed(&mut node.dev, &id, EnrollFailReason::Timeout).await;
    assert!(enrolls_dump(&hub).contains("sessions: 0"));
    // 之后握手不再放行，确认也不会写库
    let seq = n.send_env(&nid, Kind::Handshake, vec![1, 2, 3]).await;
    assert!(matches!(n.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq));
    node.dev.expect_silence(SHORT).await;
    n.send_control(&Control::PairConfirm { peer: nid.clone() }).await;
    node.dev.send_control(&Control::PairConfirm { peer: n.id.clone() }).await;
    n.expect_silence(SHORT).await;
    assert!(hub.handle.store().peers_of(&nid).await.unwrap().is_empty());
}

#[tokio::test]
async fn one_sided_confirm_writes_nothing_and_the_session_times_out() {
    let hub = TestHub::start_with(|c| c.timing.enroll_ttl = Duration::from_millis(1000)).await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;

    // 发起方确认了，daemon 没有；daemon 对错误的 peer 确认也不算
    n.send_control(&Control::PairConfirm { peer: nid.clone() }).await;
    node.dev.send_control(&Control::PairConfirm { peer: new_id() }).await;
    node.dev.expect_silence(SHORT).await;
    n.expect_silence(Duration::from_millis(100)).await;
    let store = hub.handle.store();
    assert!(store.peers_of(&nid).await.unwrap().is_empty());
    assert_eq!(store.list_devices(&hub.admin_account_id()).await.unwrap().len(), 1, "no device was created");

    expect_failed(&mut n, &id, EnrollFailReason::Timeout).await;
    expect_failed(&mut node.dev, &id, EnrollFailReason::Timeout).await;
    assert!(store.peers_of(&nid).await.unwrap().is_empty());
    assert_eq!(store.list_devices(&hub.admin_account_id()).await.unwrap().len(), 1);
    assert!(!store.list_audit(50, None).await.unwrap().iter().any(|e| e.event == "device_enrolled"));
}

#[tokio::test]
async fn cancel_disconnect_and_revocation_end_the_session_and_tell_the_other_side() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;

    // 发起方取消 → daemon 收到 cancelled；发起方自己什么都不会再收到，握手也不再放行
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;
    n.send_control(&Control::EnrollCancel { enroll_id: id.clone() }).await;
    expect_failed(&mut node.dev, &id, EnrollFailReason::Cancelled).await;
    n.expect_silence(SHORT).await;
    let seq = node.dev.send_env(&n.id.clone(), Kind::Handshake, vec![9]).await;
    assert!(matches!(node.dev.recv_control().await, Control::Dropped { seq: s, .. } if s == seq));

    // daemon 取消 → 发起方收到 cancelled，并且可以马上再开一个
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;
    node.dev.send_control(&Control::EnrollCancel { enroll_id: id.clone() }).await;
    expect_failed(&mut n, &id, EnrollFailReason::Cancelled).await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;

    // 发起方断线 → daemon 收到 peer_gone
    drop(n);
    expect_failed(&mut node.dev, &id, EnrollFailReason::PeerGone).await;
    assert!(enrolls_dump(&hub).contains("sessions: 0"));

    // daemon 断线 → 发起方收到 peer_gone
    let mut gone = self::node(&hub).await;
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n2").await;
    let (id, _) = begin_ok(&mut n, &gone.seeded.id).await;
    recv_request(&mut gone.dev).await;
    drop(gone);
    expect_failed(&mut n, &id, EnrollFailReason::PeerGone).await;

    // 发起方（正式设备）被吊销 → daemon 收到 peer_gone
    let web = hub.seed_device(DeviceKind::Web, "browser").await;
    let mut web_dev = FakeDevice::connect(&hub, Some(&web.token), DeviceKind::Web, "browser").await;
    let (id, _) = begin_ok(&mut web_dev, &nid).await;
    recv_request(&mut node.dev).await;
    assert!(hub.handle.state.revoke_device(&web.id, None).await.unwrap());
    expect_failed(&mut node.dev, &id, EnrollFailReason::PeerGone).await;
    assert_eq!(web_dev.expect_close().await, Some(4004));

    // daemon 被吊销 → 发起方收到 peer_gone
    let mut doomed = self::node(&hub).await;
    let (id, _) = begin_ok(&mut n, &doomed.seeded.id).await;
    recv_request(&mut doomed.dev).await;
    assert!(hub.handle.state.revoke_device(&doomed.seeded.id, None).await.unwrap());
    expect_failed(&mut n, &id, EnrollFailReason::PeerGone).await;
    assert_eq!(doomed.dev.expect_close().await, Some(4004));
    assert!(enrolls_dump(&hub).contains("sessions: 0"));
}

#[tokio::test]
async fn temporary_connection_limit_of_ten_minutes_still_applies_and_ends_its_session() {
    let hub = TestHub::start_with(|c| c.timing.temp_conn_ttl = Duration::from_millis(1500)).await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;
    assert_eq!(n.expect_close().await, Some(4005));
    expect_failed(&mut node.dev, &id, EnrollFailReason::PeerGone).await;
}

#[tokio::test]
async fn cancelling_an_unknown_or_foreign_session_is_not_found_and_changes_nothing() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;

    // 同账号的第三方猜到了 id 也取消不了：回 not_found（和不存在一样），会话和两端都没动
    let mut third = temp_with_cookie(&hub, &admin, DeviceKind::Web, "third").await;
    third.send_control(&Control::EnrollCancel { enroll_id: id.clone() }).await;
    expect_failed(&mut third, &id, EnrollFailReason::NotFound).await;
    node.dev.expect_silence(SHORT).await;
    n.expect_silence(Duration::from_millis(100)).await;
    assert!(enrolls_dump(&hub).contains("sessions: 1"));
    let seq = n.send_env(&nid, Kind::Handshake, vec![1]).await;
    assert_eq!(node.dev.recv_env().await.seq, seq, "the session still forwards");

    // 不存在的 id：not_found，长得像 id 的才回显
    let mut probe = temp_with_cookie(&hub, &admin, DeviceKind::Web, "probe").await;
    let ghost = "f".repeat(32);
    probe.send_control(&Control::EnrollCancel { enroll_id: ghost.clone() }).await;
    expect_failed(&mut probe, &ghost, EnrollFailReason::NotFound).await;
    let mut probe2 = temp_with_cookie(&hub, &admin, DeviceKind::Web, "probe2").await;
    probe2.send_control(&Control::EnrollCancel { enroll_id: "x".repeat(5000) }).await;
    assert!(matches!(
        probe2.recv_control().await,
        Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::NotFound }
    ));
}

// ---------------------------------------------------------------------------
// 转发隔离
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handshake_is_forwarded_only_between_the_two_ends_of_a_session() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();

    // 会话之前：不放行
    let mut loner = temp_with_cookie(&hub, &admin, DeviceKind::Web, "loner").await;
    let seq = loner.send_env(&nid, Kind::Handshake, vec![1]).await;
    assert!(
        matches!(loner.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq)
    );
    node.dev.expect_silence(SHORT).await;

    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let n_id = n.id.clone();
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;

    // 两端之间：任意字节原样转发（hub 不解析 body），两个方向
    let junk: Vec<u8> = (0..=255u8).collect();
    let seq = n.send_env(&nid, Kind::Handshake, junk.clone()).await;
    let got = node.dev.recv_env().await;
    assert_eq!(
        (got.kind, got.from.as_str(), got.to.as_str(), got.seq, got.body),
        (Kind::Handshake, n_id.as_str(), nid.as_str(), seq, junk.clone())
    );
    let seq = node.dev.send_env(&n_id, Kind::Handshake, junk.clone()).await;
    let got = n.recv_env().await;
    assert_eq!((got.from.as_str(), got.seq, got.body), (nid.as_str(), seq, junk));

    // 第三方（同账号的另一条临时连接）发给 daemon：被丢弃；daemon 发给第三方：也被丢弃
    let mut third = temp_with_cookie(&hub, &admin, DeviceKind::Web, "third").await;
    let seq = third.send_env(&nid, Kind::Handshake, vec![7]).await;
    assert!(
        matches!(third.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq)
    );
    let seq = node.dev.send_env(&third.id.clone(), Kind::Handshake, vec![8]).await;
    assert!(
        matches!(node.dev.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq)
    );
    third.expect_silence(SHORT).await;
    // 发起方发给别的设备（哪怕是同账号的正式设备）也不行
    let other = hub.seed_device(DeviceKind::Web, "other").await;
    let mut other_dev = FakeDevice::connect(&hub, Some(&other.token), DeviceKind::Web, "other").await;
    let seq = n.send_env(&other.id, Kind::Handshake, vec![5]).await;
    assert!(matches!(n.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq));
    other_dev.expect_silence(SHORT).await;
    // 会话只放行 handshake：`data` 在配对完成之前照旧被丢弃（回执每秒一条，等一等）
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let seq = n.send_env(&nid, Kind::Data, vec![6]).await;
    assert!(matches!(n.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq));
    node.dev.expect_silence(SHORT).await;
    // 以上都没有影响会话本身
    let seq = n.send_env(&nid, Kind::Handshake, vec![4]).await;
    assert_eq!(node.dev.recv_env().await.seq, seq);

    // 会话结束之后两端也不能再互发
    n.send_control(&Control::EnrollCancel { enroll_id: id.clone() }).await;
    expect_failed(&mut node.dev, &id, EnrollFailReason::Cancelled).await;
    let seq = node.dev.send_env(&n_id, Kind::Handshake, vec![3]).await;
    assert!(
        matches!(node.dev.recv_control().await, Control::Dropped { seq: s, reason: DropReason::NotPaired } if s == seq)
    );
    n.expect_silence(SHORT).await;
}

// ---------------------------------------------------------------------------
// 收尾失败
// ---------------------------------------------------------------------------

/// 收尾共用配对的代码，但失败要用 `enroll_failed`（带 id）通知双方，而不是 `pair_failed`；什么都不写。
#[tokio::test]
async fn a_failing_finalize_is_reported_with_enroll_failed_and_writes_nothing() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut node = node(&hub).await;
    let nid = node.seeded.id.clone();
    let mut n = temp_with_cookie(&hub, &admin, DeviceKind::Web, "n").await;
    let (id, _) = begin_ok(&mut n, &nid).await;
    recv_request(&mut node.dev).await;

    let state = hub.handle.state.clone();
    let daemon = state.registry.get(&nid).expect("online");
    let initiator = state.registry.get(&n.id).expect("online");
    // 账号和 enroll_begin 时不一样：Forbidden
    let origin = Origin::Enroll { enroll_id: id.clone(), account: "someone-else".into() };
    anycon_hub::pairing::finalize(&state, daemon, initiator, origin).await;
    expect_failed(&mut n, &id, EnrollFailReason::Forbidden).await;
    expect_failed(&mut node.dev, &id, EnrollFailReason::Forbidden).await;
    assert!(n.drain_controls(SHORT).await.is_empty(), "no pair_failed on top of enroll_failed");
    assert!(node.dev.drain_controls(SHORT).await.is_empty());
    let store = hub.handle.store();
    assert!(store.peers_of(&nid).await.unwrap().is_empty());
    assert_eq!(store.list_devices(&hub.admin_account_id()).await.unwrap().len(), 1, "no device was created");
}
