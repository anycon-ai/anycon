//! 预览经真实 hub 的端到端（ADR-39）：进程内的真 hub（配了 `ANYCON_PREVIEW_URL`）、真 daemon、`FakeWeb`（配对 + IK +
//! `preview.open`）、令牌接口（`POST /api/preview/tokens`）、`FakePreview` 经 `HubWire`（预览域名上的真 WebSocket）、
//! 本机 HTTP / WebSocket 测试服务器。
//!
//! 和 `preview.rs` 的区别：那边信封直接交给 daemon 的 `HubLink`，这里每一封都经过 hub 的路由、粘住规则、预览预算和令牌校验。
//! 端口全部由系统分配（hub 的预览域名带端口，所以先找一个空闲端口再让 hub 监听它）；不依赖网络和外部程序。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_crypto::Keypair;
use anycon_daemon::testutil::preview::*;
use anycon_daemon::testutil::{eventually, FakeWeb, TestClient, TestDaemon};
use anycon_hub::Config as HubConfig;
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(20);
const MIB: usize = 1024 * 1024;

struct Hub {
    handle: anycon_hub::Handle,
    _dir: tempfile::TempDir,
    http: reqwest::Client,
    port: u16,
}

impl Hub {
    async fn start(tweak: impl FnOnce(&mut HubConfig)) -> Hub {
        // 预览 origin 带端口（`http://<label>.preview.localhost:<端口>`，和 `just dev` 一样），所以先找一个空闲端口
        let port = free_port();
        let dir = tempfile::tempdir().unwrap();
        let mut c = HubConfig::new("http://127.0.0.1", dir.path()).unwrap();
        c.listen = format!("127.0.0.1:{port}").parse().unwrap();
        c.preview = Some(
            anycon_hub::config::parse_preview_url(&format!("http://*.preview.localhost:{port}"), &c.public_url)
                .unwrap(),
        );
        tweak(&mut c);
        let handle = anycon_hub::run(c).await.expect("hub starts");
        assert_eq!(handle.addr.port(), port);
        Hub { handle, _dir: dir, http: reqwest::Client::new(), port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn ws_url(&self) -> String {
        format!("ws://127.0.0.1:{}/v1/ws", self.port)
    }

    /// 管理员登录，返回 cookie（web 客户端配对需要账号归属）。
    async fn admin_cookie(&self) -> String {
        let token = self.handle.generated_admin_token.clone().unwrap();
        let res = self
            .http
            .post(format!("{}/auth/token", self.url()))
            .header("x-anycon", "1")
            .json(&json!({ "token": token }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 204);
        res.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string()
    }

    /// `POST /api/preview/tokens`（设备 token，`X-Anycon: 1`）。
    async fn request_token(&self, device_token: &str, node: &str, label: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/api/preview/tokens", self.url()))
            .header("authorization", format!("Bearer {device_token}"))
            .header("x-anycon", "1")
            .json(&json!({ "node": node, "label": label }))
            .send()
            .await
            .unwrap()
    }
}

async fn wait_daemon_connected(td: &TestDaemon) {
    assert!(
        eventually(T, || async { td.ctx.hub_state.get().connected }).await,
        "daemon should connect to the hub: {:?}",
        td.ctx.hub_state.get()
    );
}

/// 完整配对 + IK：返回已建立会话的假 web。
async fn pair_web(td: &TestDaemon, hub: &Hub, name: &str) -> FakeWeb {
    let cookie = hub.admin_cookie().await;
    let mut local = td.client("cli");
    let begin: NodePairBeginResult = local.call(td, methods::NODE_PAIR_BEGIN, json!({})).await.unwrap();
    let mut web = FakeWeb::connect(&hub.url(), None, Some(&cookie), name, Keypair::generate()).await;
    web.pair(&begin.code).await;
    assert!(eventually(T, || async { td.ctx.identity.device_token().is_some() }).await);
    web.ik().await.expect("IK handshake");
    web
}

/// 一个 hub、一台连着它的 daemon、一个已配对的假 web、一个本机上游（端口在 `preview.allow_ports` 里）。
struct Fx {
    hub: Hub,
    td: TestDaemon,
    up: TestUpstream,
    web: FakeWeb,
    local: TestClient,
}

impl Fx {
    async fn new() -> Fx {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut HubConfig)) -> Fx {
        let hub = Hub::start(tweak).await;
        let up = TestUpstream::start().await;
        let allow = format!("[{}]", up.port);
        let td = TestDaemon::start_full(&[("preview.allow_ports", &allow)], true, Some(&hub.url())).await;
        wait_daemon_connected(&td).await;
        let web = pair_web(&td, &hub, "Phone").await;
        let local = td.client("cli");
        Fx { hub, td, up, web, local }
    }

    fn node_id(&self) -> String {
        self.web.node_id.clone().expect("paired")
    }

    /// `preview.open`（经 hub 的加密通道，由假 web 调）。
    async fn open(&mut self) -> PreviewOpenResult {
        self.web.call(methods::PREVIEW_OPEN, json!({ "port": self.up.port })).await.unwrap()
    }

    /// 为 label 申请预览令牌：`(token, origin, host)`。
    async fn token(&self, label: &str) -> (String, String, String) {
        let res = self.hub.request_token(self.web.token.as_deref().unwrap(), &self.node_id(), label).await;
        assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());
        let body: Value = res.json().await.unwrap();
        let origin = body["origin"].as_str().unwrap().to_string();
        assert_eq!(origin, format!("http://{label}.preview.localhost:{}", self.hub.port));
        let host = origin.strip_prefix("http://").unwrap().to_string();
        (body["token"].as_str().unwrap().to_string(), origin, host)
    }

    /// 预览页的 WebSocket 连接（预览域名上，令牌认证）。
    async fn wire(&self, t: &PreviewOpenResult) -> HubWire {
        let (token, origin, host) = self.token(&t.label).await;
        let wire = HubWire::connect(&self.hub.ws_url(), &host, &origin, &token, &self.node_id())
            .await
            .expect("the preview connection is accepted");
        assert!(wire.node_online, "the daemon is online for the preview connection");
        // hub 给 daemon 发 `preview_peer`；daemon 登记之后才会处理这条连接的握手
        assert!(
            eventually(T, || async { self.td.ctx.preview.is_peer(&wire.id) }).await,
            "preview_peer reached the daemon"
        );
        wire
    }

    /// 开票据、拿令牌、连上 hub、握手。
    async fn preview(&mut self) -> (PreviewOpenResult, FakePreview) {
        let t = self.open().await;
        let wire = self.wire(&t).await;
        let pv =
            FakePreview::connect(wire, self.td.ctx.identity.public(), &t).await.expect("the tunnel handshake succeeds");
        (t, pv)
    }
}

// ===================================================================== 基本转发

#[tokio::test]
async fn http_and_websocket_cross_a_real_hub() {
    let mut fx = Fx::new().await;
    let (t, mut pv) = fx.preview().await;
    assert_eq!(pv.hello.as_ref().unwrap().port, fx.up.port);
    assert_eq!(fx.td.ctx.preview.tunnel_count(), 1);

    // GET：头部改写、响应原样
    let origin = format!("http://{}.preview.localhost:{}", t.label, fx.hub.port);
    let res = pv.fetch("GET", "/hello?x=1", &[("origin", &origin), ("accept", "text/html")], None).await;
    assert_eq!((res.status, res.text().as_str()), (200, "hello"));
    let seen = fx.up.last_request("/hello").unwrap();
    assert_eq!(seen.header("host"), Some(format!("localhost:{}", fx.up.port).as_str()));
    assert_eq!(seen.header("origin"), Some(format!("http://localhost:{}", fx.up.port).as_str()));

    // POST：请求体比一个 stream 窗口大
    let body = big_body(700_000);
    let res = pv.fetch("POST", "/echo", &[("content-type", "application/octet-stream")], Some(&body)).await;
    assert_eq!((res.status, res.decoded() == body), (200, true));

    // 文本被 daemon gzip，上游自己压缩的原样转发，重定向改成路径
    let res = pv.get("/text?n=300000").await;
    assert_eq!(res.encoding, Some(PreviewBodyEncoding::Gzip));
    assert_eq!(res.decoded(), compressible_text(300_000));
    assert!(res.body.len() < 30_000);
    assert_eq!(pv.get("/gz").await.text(), "gzipped by the upstream".repeat(100));
    assert_eq!(pv.get("/redirect").await.header("location"), Some("/landing?x=1"));

    // WebSocket 双向
    let ws = pv.ws_open("/chat", &[("origin", &origin), ("sec-websocket-protocol", "vite-hmr")]).await.unwrap();
    assert_eq!(pv.response(ws).header("sec-websocket-protocol"), Some("vite-hmr"));
    pv.ws_send_text(ws, "hi").await;
    assert_eq!(pv.ws_recv(ws, T).await, Some(WsEvent::Message { binary: false, data: b"echo:hi".to_vec() }));
    let blob = big_body(150_000);
    pv.ws_send(ws, true, &blob).await;
    assert_eq!(pv.ws_recv(ws, T).await, Some(WsEvent::Message { binary: true, data: blob }));
    pv.ws_send_text(ws, "close").await;
    assert_eq!(pv.ws_recv(ws, T).await, Some(WsEvent::Close { code: 4000, reason: "bye".into() }));
    pv.ws_close(ws, 4000, "bye").await;

    // 会话 RPC 同时不受影响
    let info: NodeInfo = fx.web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.preview);
    // 隧道的建立 / 断开不进审计，票据 id 不出现在日志之外的地方——这里只确认 hub 没有看到内容：hub 的库里只有令牌哈希
    let store = fx.hub.handle.store();
    assert_eq!(store.list_preview_labels(&fx.node_id()).await.unwrap(), vec![t.label.clone()]);
}

/// 这台机器上（debug 构建）Noise 加密 + 解密 1 MiB 要多久。debug 里 ChaCha20-Poly1305 不优化，只有约 2.5 MiB/s，
/// 这是端到端传输速度的硬上限（加密在 daemon、解密在预览页，哪怕分在两个线程上也一样），和协议无关。
fn noise_seconds_per_mib() -> f64 {
    let kp = Keypair::generate();
    let psk = [3u8; 32];
    let mut i = anycon_crypto::Handshake::preview_initiator(&kp.public, &psk, b"p").unwrap();
    let mut r = anycon_crypto::Handshake::preview_responder(&kp, &psk, b"p").unwrap();
    let m1 = i.write(&[]).unwrap();
    r.read(&m1).unwrap();
    let m2 = r.write(b"x").unwrap();
    i.read(&m2).unwrap();
    let (mut tx, mut rx) = (i.into_transport().unwrap(), r.into_transport().unwrap());
    let chunk = vec![7u8; 128 * 1024];
    let started = Instant::now();
    for _ in 0..16 {
        let ct = tx.encrypt(&chunk).unwrap();
        rx.decrypt(&ct).unwrap();
    }
    started.elapsed().as_secs_f64() / 2.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_large_response_keeps_flowing_on_end_to_end_acks_instead_of_the_session_retention() {
    let mut fx = Fx::new().await;
    let (_t, mut pv) = fx.preview().await;
    // 会话传输靠「保留 10 秒」限流，窗口填满之后持续吞吐约 16 MiB / 10 s = 1.6 MiB/s；预览按回执周转，不受它约束。
    // debug 构建里加解密本身就比这慢（见 `noise_seconds_per_mib`），所以绝对速度说明不了什么：
    // 看两件事——(1) 总时间不超过「纯加解密」需要的时间的 2 倍多一点（hub、回执、窗口来回的开销有限）；
    // (2) 传输期间相邻两封数据信封之间从没有过长时间的空档（卡在保留期上会有好几秒的空档）
    let per_mib = noise_seconds_per_mib();
    let n = 32 * MIB;
    pv.reset_gap();
    let started = Instant::now();
    let id = pv.start_request("GET", &format!("/big?n={n}"), &[], None).await;
    let res = pv.finish(id, Duration::from_secs(240)).await;
    let secs = started.elapsed().as_secs_f64();
    assert_eq!(res.body.len(), n);
    assert!(res.body == big_body(n), "all the bytes, in order");
    let mib = n as f64 / MIB as f64;
    let floor = per_mib * mib;
    eprintln!(
        "preview throughput through the real hub: {:.2} MiB/s ({mib:.0} MiB in {secs:.1} s); \
         encrypt+decrypt alone would take {floor:.1} s ({:.2} MiB/s) in this debug build; longest gap between data envelopes {:?}",
        mib / secs,
        mib / floor,
        pv.max_recv_gap()
    );
    assert!(secs < floor * 2.0 + 5.0, "{secs:.1} s for {mib:.0} MiB; crypto alone is {floor:.1} s");
    assert!(pv.max_recv_gap() < Duration::from_secs(3), "stalled for {:?}", pv.max_recv_gap());
    assert_eq!(pv.close_code(), None, "the preview connection stayed open");
    // 传完之后保留的原文已经按回执释放（按时间 / 字节逐出的话会留着 16 MiB 上下）
    let kept = fx.td.ctx.outbox.retained_bytes(&pv.peer_id());
    assert!(kept <= 8 * MIB, "retained {kept} bytes");
    // 会话 RPC 在传输之后照常
    let _: NodeInfo = fx.web.call(methods::NODE_INFO, json!({})).await.unwrap();
}

// ===================================================================== 重连

#[tokio::test]
async fn after_the_daemon_reconnects_the_hub_announces_the_preview_peer_again_and_the_page_can_resume() {
    let mut fx = Fx::new().await;
    let (_t, mut pv) = fx.preview().await;
    assert_eq!(pv.get("/hello").await.status, 200);
    let (peer, epoch) = (pv.peer_id(), fx.td.ctx.outbox.epoch());

    // daemon 到 hub 的连接断了又连上（睡眠唤醒、网络抖动…）：出站队列没了，旧隧道作废；预览页的连接还在 hub 上
    fx.td.ctx.peers.reconnect.notify_one();
    assert!(
        eventually(T, || async { fx.td.ctx.outbox.epoch() > epoch && fx.td.ctx.preview.is_peer(&peer) }).await,
        "the hub re-sent preview_peer after the daemon came back"
    );
    // 重新握手之后继续可用（旧票据还有效）
    assert!(pv.handshake().await.is_ok(), "the page re-handshakes");
    assert!(eventually(T, || async { fx.td.ctx.preview.tunnel_count() == 1 }).await);
    let res = pv.get("/hello").await;
    assert_eq!((res.status, res.text().as_str()), (200, "hello"));
    let ws = pv.ws_open("/again", &[]).await.unwrap();
    pv.ws_send_text(ws, "x").await;
    assert_eq!(pv.ws_recv(ws, T).await, Some(WsEvent::Message { binary: false, data: b"echo:x".to_vec() }));
    // 会话一侧也重新连上了：daemon 重连之后旧的 Noise 会话作废，假 web 重新做 IK
    fx.web.ik().await.expect("IK after the daemon reconnected");
    let _: NodeInfo = fx.web.call(methods::NODE_INFO, json!({})).await.unwrap();
}

// ===================================================================== 预算

#[tokio::test]
async fn a_tiny_account_preview_budget_slows_only_the_preview_peer() {
    // 账号的预览预算很小：下载很快就会被 hub 退回（`backpressure`）并收到 `rate_limited { to }`
    let mut fx = Fx::with(|c| c.limits.preview_bytes_per_min = 256 * 1024).await;
    let (_t, mut pv) = fx.preview().await;
    let peer = pv.peer_id();

    // 采样 daemon 对这个预览对端的暂停（`rate_limited { to }` 只暂停发往它的信封）
    let longest = Arc::new(AtomicU64::new(0));
    let (outbox, p, l) = (fx.td.ctx.outbox.clone(), peer.clone(), longest.clone());
    let sampler = tokio::spawn(async move {
        loop {
            l.fetch_max(outbox.peer_pause_left(&p).as_millis() as u64, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let n = 4 * MIB;
    let id = pv.start_request("GET", &format!("/big?n={n}"), &[], None).await;
    pv.settle(Duration::from_secs(3)).await;
    assert!(!pv.response(id).ended, "the download is being throttled");
    assert!(pv.body_len(id) < n / 2, "only a part arrived: {}", pv.body_len(id));
    assert!(longest.load(Ordering::SeqCst) > 0, "the daemon was told to pause this peer (rate_limited {{ to }})");
    assert_eq!(fx.td.ctx.outbox.rate_pause_left(), Duration::ZERO, "the connection as a whole is not paused");
    // 会话 RPC 不受影响：照常很快
    let started = Instant::now();
    let info: NodeInfo = fx.web.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(info.capabilities.preview);
    let devices: NodeDevicesResult = fx.web.call(methods::NODE_DEVICES, json!({})).await.unwrap();
    assert_eq!(devices.devices.len(), 1);
    sampler.abort();
}

// ===================================================================== 作废与断开

#[tokio::test]
async fn preview_close_sends_goaway_closed_through_the_hub() {
    let mut fx = Fx::new().await;
    let (t, mut pv) = fx.preview().await;
    assert_eq!(pv.get("/hello").await.status, 200);
    let _: Empty = fx.web.call(methods::PREVIEW_CLOSE, json!({ "label": t.label })).await.unwrap();
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await, "no GOAWAY");
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Closed);
    assert_eq!(pv.handshake().await, Err(HandshakeError::Rejected("unknown_ticket".into())));
}

#[tokio::test]
async fn revoking_the_device_closes_the_preview_connection_with_4004_and_sends_goaway_revoked() {
    let mut fx = Fx::new().await;
    let (_t, mut pv) = fx.preview().await;
    assert_eq!(pv.get("/hello").await.status, 200);
    let web_id = fx.web.device_id.clone();
    // 在节点上吊销假 web（和本机 `anycon devices revoke` 一样）：票据作废、隧道 GOAWAY { revoked }，daemon 通知 hub 删配对，
    // hub 用 4004 关掉这个设备签发的令牌对应的预览连接
    let _: Empty = fx.local.call(&fx.td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": web_id })).await.unwrap();
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await, "no GOAWAY");
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Revoked);
    assert!(pv.wait_until(T, |p| p.close_code().is_some()).await, "the hub closed the preview connection");
    assert_eq!(pv.close_code(), Some(4004));
    assert_eq!(fx.td.ctx.preview.tunnel_count(), 0);
    // 令牌也用不了了：设备被吊销
    let res = fx.hub.request_token(fx.web.token.as_deref().unwrap(), &fx.node_id(), "aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert_eq!(res.status(), 401);
}

// ===================================================================== 令牌和主机名

#[tokio::test]
async fn a_token_works_only_for_its_own_label_and_origin() {
    let mut fx = Fx::new().await;
    let t = fx.open().await;
    let (token, origin, host) = fx.token(&t.label).await;
    let other_label = anycon_protocol::tunnel::label_from_bytes(&[5u8; 16]);
    let other_host = format!("{other_label}.preview.localhost:{}", fx.hub.port);
    let other_origin = format!("http://{other_host}");
    let (ws, node) = (fx.hub.ws_url(), fx.node_id());

    // label 和主机名不符：auth_fail { invalid }
    match HubWire::connect(&ws, &other_host, &other_origin, &token, &node).await {
        Err(HubWireError::AuthFail(AuthFailReason::Invalid)) => {}
        other => panic!("{other:?}"),
    }
    // 没有 / 乱写的令牌
    for bad in ["", "nope", &"0".repeat(64)] {
        match HubWire::connect(&ws, &host, &origin, bad, &node).await {
            Err(HubWireError::AuthFail(AuthFailReason::Invalid)) => {}
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    // Origin 不是这个预览页的：升级就被拒绝
    match HubWire::connect(&ws, &host, "http://evil.example", &token, &node).await {
        Err(HubWireError::Upgrade(_)) => {}
        other => panic!("{other:?}"),
    }
    match HubWire::connect(&ws, &host, &other_origin, &token, &node).await {
        Err(HubWireError::Upgrade(_)) => {}
        other => panic!("{other:?}"),
    }
    // 主站域名上不接受预览令牌
    match HubWire::connect(&ws, &format!("127.0.0.1:{}", fx.hub.port), &fx.hub.url(), &token, &node).await {
        Err(HubWireError::AuthFail(_)) | Err(HubWireError::Closed(_)) => {}
        other => panic!("{other:?}"),
    }
    // 对的那一个仍然可用，用完一个令牌可以连多次
    let a = HubWire::connect(&ws, &host, &origin, &token, &node).await.expect("the right host");
    let b =
        HubWire::connect(&ws, &host, &origin, &token, &node).await.expect("a second connection with the same token");
    assert_ne!(a.id, b.id);
}

#[tokio::test]
async fn two_preview_connections_share_a_ticket_and_close_independently() {
    let mut fx = Fx::new().await;
    let t = fx.open().await;
    let (wa, wb) = (fx.wire(&t).await, fx.wire(&t).await);
    let public = fx.td.ctx.identity.public();
    let mut a = FakePreview::connect(wa, public, &t).await.unwrap();
    let mut b = FakePreview::connect(wb, public, &t).await.unwrap();
    assert_eq!(fx.td.ctx.preview.tunnel_count(), 2);
    assert_eq!(a.get("/hello").await.status, 200);
    assert_eq!(b.get("/hello").await.status, 200);
    a.disconnect().await;
    assert!(eventually(T, || async { fx.td.ctx.preview.tunnel_count() == 1 }).await);
    assert_eq!(b.get("/hello").await.status, 200);
}
