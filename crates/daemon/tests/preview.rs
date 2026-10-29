//! dev server 预览（ADR-39，docs/spec/daemon.md § 19）：票据、端口策略、隧道握手、HTTP / WebSocket 转发、
//! 流控与端到端回执、hub 的退回和限速、GOAWAY。
//!
//! 用 `TestDaemon` + 进程内的假 hub（`LocalPreviewHub`，信封走 daemon 自己的 `HubLink::on_frame` / `on_control`）+ Rust 写的预览页
//! （`FakePreview`）+ 本机 HTTP / WebSocket 测试服务器（`TestUpstream`）。不依赖网络；端口探测相关的用例没有 `lsof` 时跳过。
//! 运行时用默认的 current_thread，所以「一口气发出去、daemon 还没处理」这类时序是确定的。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_daemon::preview::upstream::Timeouts;
use anycon_daemon::store::{PeerRow, PreviewTicketRow};
use anycon_daemon::testutil::preview::*;
use anycon_daemon::testutil::{eventually, TestClient, TestDaemon};
use anycon_protocol::tunnel::{Frame, FrameType};
use anycon_protocol::*;
use serde_json::{json, Value};

const T: Duration = Duration::from_secs(15);
const MIB: usize = 1024 * 1024;

struct Rig {
    td: TestDaemon,
    hub: Arc<LocalPreviewHub>,
    up: TestUpstream,
    c: TestClient,
}

impl Rig {
    /// 上游端口写进 `preview.allow_ports`，其余配置用 `overrides`。
    async fn new() -> Rig {
        Self::with(&[]).await
    }

    async fn with(overrides: &[(&str, &str)]) -> Rig {
        let up = TestUpstream::start().await;
        let allow = format!("[{}]", up.port);
        let mut o: Vec<(&str, &str)> = vec![("preview.allow_ports", &allow)];
        o.extend_from_slice(overrides);
        let td = TestDaemon::start_with(&o, false).await;
        let hub = LocalPreviewHub::attach(&td);
        let c = td.client("a");
        Rig { td, hub, up, c }
    }

    async fn open(&mut self, port: u16) -> PreviewOpenResult {
        self.c.call(&self.td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap()
    }

    async fn open_err(&mut self, params: Value) -> RpcError {
        self.c.call::<PreviewOpenResult>(&self.td, methods::PREVIEW_OPEN, params).await.unwrap_err()
    }

    fn public(&self) -> [u8; 32] {
        self.td.ctx.identity.public()
    }

    /// 开票据并建好隧道。
    async fn tunnel(&mut self) -> (PreviewOpenResult, FakePreview) {
        let t = self.open(self.up.port).await;
        let p = self.connect(&t).await;
        (t, p)
    }

    async fn connect(&self, t: &PreviewOpenResult) -> FakePreview {
        let wire = self.hub.connect(&t.label).await;
        FakePreview::connect(wire, self.public(), t).await.expect("the handshake succeeds")
    }

    async fn call<R: serde::de::DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<R, RpcError> {
        self.c.call(&self.td, method, params).await
    }

    /// 改 `config.toml` 里的一个键并热加载。
    fn set(&self, key: &str, value: &str) {
        anycon_daemon::config::set_value(&self.td.paths, key, value).unwrap();
        self.td.ctx.config.reload().unwrap();
    }
}

fn code(e: &RpcError) -> ErrorCode {
    e.stable_code().expect("stable code")
}

fn reason(e: &RpcError) -> Option<String> {
    e.data.as_ref().and_then(|d| d.reason.clone())
}

/// macOS 默认每个进程只能开 256 个文件：测试进程里 daemon 和测试服务器两端都在，256 个并发 stream 要 500 多个。
fn raise_fd_limit() {
    // SAFETY: getrlimit / setrlimit 只读写我们传进去的结构体。
    #[cfg(unix)]
    unsafe {
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) == 0 {
            lim.rlim_cur = lim.rlim_max.min(8192);
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
        }
    }
}

fn hdr<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

// ===================================================================== 票据与 RPC

#[tokio::test]
async fn open_issues_a_ticket_and_the_tunnel_handshake_returns_the_hello() {
    let mut r = Rig::new().await;
    let info: NodeInfo = r.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.preview);

    let before = anycon_daemon::util::now_ms();
    let t = r.open(r.up.port).await;
    assert!(anycon_protocol::tunnel::is_valid_label(&t.label), "{}", t.label);
    assert_eq!((t.ticket_id.len(), t.psk.len(), t.port), (32, 64, r.up.port));
    assert!(t.ticket_id.bytes().chain(t.psk.bytes()).all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    // 默认有效期 24 小时
    let ttl = t.expires_at_ms - before;
    assert!((23 * 3600 * 1000..=24 * 3600 * 1000 + 5000).contains(&ttl), "{ttl}");

    let listed: PreviewListResult = r.call(methods::PREVIEW_LIST, json!({})).await.unwrap();
    assert_eq!(listed.previews.len(), 1);
    let p = &listed.previews[0];
    assert_eq!(
        (p.label.as_str(), p.port, p.ticket_id.as_str(), p.created_by.as_str(), p.tunnels),
        (t.label.as_str(), t.port, t.ticket_id.as_str(), "dev-a", 0)
    );
    assert!(!serde_json::to_string(&listed).unwrap().contains(&t.psk), "the list never carries the secret");

    let mut pv = r.connect(&t).await;
    let hello = pv.hello.clone().unwrap();
    assert_eq!((hello.port, hello.expires_at_ms, hello.session_id), (t.port, t.expires_at_ms, None));
    assert_eq!(r.td.ctx.preview.tunnel_count(), 1);
    let listed: PreviewListResult = r.call(methods::PREVIEW_LIST, json!({})).await.unwrap();
    assert_eq!(listed.previews[0].tunnels, 1);
    // 隧道能用
    assert_eq!(pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn ticket_ttl_and_session_id_come_from_the_config_and_the_request() {
    let mut r = Rig::with(&[("preview.ticket_ttl", "\"10m\"")]).await;
    let s: SessionSummary = r.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let now = anycon_daemon::util::now_ms();
    let t: PreviewOpenResult =
        r.call(methods::PREVIEW_OPEN, json!({ "port": r.up.port, "session_id": s.id })).await.unwrap();
    assert!(
        (t.expires_at_ms - now) <= 10 * 60 * 1000 + 5000 && (t.expires_at_ms - now) >= 9 * 60 * 1000,
        "{}",
        t.expires_at_ms - now
    );
    assert_eq!(t.session_id.as_deref(), Some(s.id.as_str()));
    let pv = r.connect(&t).await;
    assert_eq!(pv.hello.unwrap().session_id.as_deref(), Some(s.id.as_str()));
    let listed: PreviewListResult = r.call(methods::PREVIEW_LIST, json!({})).await.unwrap();
    assert_eq!(listed.previews[0].session_id.as_deref(), Some(s.id.as_str()));
}

#[tokio::test]
async fn labels_are_fixed_per_port_tickets_are_reused_and_survive_a_restart() {
    let mut r = Rig::new().await;
    let a = r.open(r.up.port).await;
    let again = r.open(r.up.port).await;
    assert_eq!(
        (again.ticket_id.clone(), again.psk.clone(), again.label.clone()),
        (a.ticket_id.clone(), a.psk.clone(), a.label.clone()),
        "reused"
    );
    // 别的设备：同一个端口同一个 label，另一张票据
    let mut other = r.td.client("b");
    let b: PreviewOpenResult = other.call(&r.td, methods::PREVIEW_OPEN, json!({ "port": r.up.port })).await.unwrap();
    assert_eq!(b.label, a.label);
    assert_ne!(b.ticket_id, a.ticket_id);
    assert_ne!(b.psk, a.psk);
    // 另一个端口：另一个 label
    let second = TestUpstream::start().await;
    r.set("preview.allow_ports", &format!("[{}, {}]", r.up.port, second.port));
    let c2 = r.open(second.port).await;
    assert_ne!(c2.label, a.label);

    // 剩余有效期不到一半的不复用：签一张新的
    let now = anycon_daemon::util::now_ms();
    r.td.ctx
        .store
        .preview_ticket_insert(&PreviewTicketRow {
            ticket_id: "ab".repeat(16),
            label: c2.label.clone(),
            port: second.port,
            psk: "cd".repeat(32),
            device_id: "dev-old".into(),
            session_id: None,
            created_at_ms: now - 20 * 3600 * 1000,
            expires_at_ms: now + 4 * 3600 * 1000,
        })
        .await
        .unwrap();
    let mut old = r.td.client("old");
    let fresh: PreviewOpenResult =
        old.call(&r.td, methods::PREVIEW_OPEN, json!({ "port": second.port })).await.unwrap();
    assert_ne!(fresh.ticket_id, "ab".repeat(16));
    assert_eq!(fresh.label, c2.label);

    // daemon 重启之后 label 不变，票据还能用
    r.td.restart().await;
    let hub = LocalPreviewHub::attach(&r.td);
    let mut c = r.td.client("a");
    let after: PreviewOpenResult = c.call(&r.td, methods::PREVIEW_OPEN, json!({ "port": r.up.port })).await.unwrap();
    assert_eq!((after.label.as_str(), after.ticket_id.as_str()), (a.label.as_str(), a.ticket_id.as_str()));
    let wire = hub.connect(&a.label).await;
    let mut pv = FakePreview::connect(wire, r.td.ctx.identity.public(), &a).await.expect("the old ticket still works");
    assert_eq!(pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn open_validates_its_parameters_and_the_port_policy() {
    let mut r = Rig::new().await;
    assert_eq!(code(&r.open_err(json!({ "port": 0 })).await), ErrorCode::Invalid);
    assert_eq!(code(&r.open_err(json!({ "port": r.up.port, "session_id": "sess_nope" })).await), ErrorCode::NotFound);
    // 不在 allow_ports 里、也没有会话起的服务：拒绝，带原因
    let other = TestUpstream::start().await;
    let e = r.open_err(json!({ "port": other.port })).await;
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("port_not_allowed")));
    let nobody = free_port();
    let e = r.open_err(json!({ "port": nobody })).await;
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("port_not_allowed")));
    // 参数类型不对
    assert_eq!(code(&r.open_err(json!({ "port": 70000 })).await), ErrorCode::Invalid);
    assert_eq!(code(&r.open_err(json!({})).await), ErrorCode::Invalid);
}

#[tokio::test]
async fn the_local_hubs_own_port_is_never_allowed_even_when_listed() {
    let up = TestUpstream::start().await;
    let allow = format!("[{}]", up.port);
    let hub_url = format!("\"http://127.0.0.1:{}\"", up.port);
    let td = TestDaemon::start_with(&[("preview.allow_ports", &allow), ("hub.url", &hub_url)], false).await;
    // 只看端口策略（`start_with` 不连 hub）
    let mut c = td.client("a");
    let e = c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": up.port })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("port_not_allowed")));
    let ports: PreviewPortsResult = c.call(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    assert!(ports.ports.iter().all(|p| p.port != up.port), "{ports:?}");
}

#[tokio::test]
async fn open_and_close_are_audited_without_the_secret() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let _ = r.open(r.up.port).await; // 复用
    let _: Empty = r.call(methods::PREVIEW_CLOSE, json!({ "label": t.label })).await.unwrap();
    let audit: NodeAuditResult = r.call(methods::NODE_AUDIT, json!({ "limit": 50 })).await.unwrap();
    let opens: Vec<&AuditEntry> = audit.entries.iter().filter(|e| e.action == "preview.open").collect();
    assert_eq!(opens.len(), 2);
    let (newest, oldest) = (&opens[0], &opens[1]);
    assert_eq!(oldest.target.as_deref(), Some(r.up.port.to_string().as_str()));
    assert_eq!(oldest.detail["reused"], json!(false));
    assert_eq!(newest.detail["reused"], json!(true));
    assert_eq!(oldest.detail["label"], json!(t.label));
    assert_eq!(oldest.detail["ticket_id"], json!(t.ticket_id));
    assert_eq!(oldest.detail["expires_at_ms"], json!(t.expires_at_ms));
    let close = audit.entries.iter().find(|e| e.action == "preview.close").expect("close is audited");
    assert_eq!(close.target.as_deref(), Some(t.label.as_str()));
    assert_eq!(close.detail, json!({ "tickets": 1, "tunnels": 0 }));
    assert!(!serde_json::to_string(&audit).unwrap().contains(&t.psk), "no secret in the audit");
    // 不存在的 label 也算成功
    let _: Empty = r.call(methods::PREVIEW_CLOSE, json!({ "label": "nonexistent" })).await.unwrap();
}

#[tokio::test]
async fn disabling_preview_closes_tunnels_and_refuses_new_ones_but_keeps_the_tickets() {
    let mut r = Rig::new().await;
    let (t, mut pv) = r.tunnel().await;
    assert_eq!(pv.get("/hello").await.status, 200);

    r.set("preview.enabled", "false");
    let info: NodeInfo = r.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(!info.capabilities.preview);
    // 已有的隧道收到 GOAWAY { disabled }
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await, "no GOAWAY");
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Disabled);
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
    // 新握手：disabled；open：forbidden / disabled
    assert_eq!(pv.handshake().await, Err(HandshakeError::Rejected("disabled".into())));
    let e = r.open_err(json!({ "port": r.up.port })).await;
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("disabled")));
    // 票据保留：重新打开之后还能用
    r.set("preview.enabled", "true");
    let info: NodeInfo = r.call(methods::NODE_INFO, json!({})).await.unwrap();
    assert!(info.capabilities.preview);
    assert!(pv.handshake().await.is_ok());
    assert_eq!(pv.get("/hello").await.status, 200);
    assert_eq!(r.open(r.up.port).await.ticket_id, t.ticket_id);
}

// ===================================================================== 握手

#[tokio::test]
async fn handshakes_with_a_wrong_secret_label_or_ticket_are_rejected() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let public = r.public();

    // 错的 PSK：Noise 读不了第一条
    let mut wrong = t.clone();
    wrong.psk = "11".repeat(32);
    let wire = r.hub.connect(&t.label).await;
    assert_eq!(
        FakePreview::connect(wire, public, &wrong).await.err(),
        Some(HandshakeError::Rejected("malformed".into()))
    );

    // hub 告知的 label 和票据的不一致
    let other_label = anycon_protocol::tunnel::label_from_bytes(&[7u8; 16]);
    let wire = r.hub.connect(&other_label).await;
    assert_eq!(
        FakePreview::connect(wire, public, &t).await.err(),
        Some(HandshakeError::Rejected("label_mismatch".into()))
    );

    // 没有这张票据
    let mut unknown = t.clone();
    unknown.ticket_id = "ee".repeat(16);
    let wire = r.hub.connect(&t.label).await;
    assert_eq!(
        FakePreview::connect(wire, public, &unknown).await.err(),
        Some(HandshakeError::Rejected("unknown_ticket".into()))
    );

    // 过期的票据
    let now = anycon_daemon::util::now_ms();
    r.td.ctx
        .store
        .preview_ticket_insert(&PreviewTicketRow {
            ticket_id: "aa".repeat(16),
            label: t.label.clone(),
            port: t.port,
            psk: "bb".repeat(32),
            device_id: "dev-a".into(),
            session_id: None,
            created_at_ms: now - 2000,
            expires_at_ms: now - 1,
        })
        .await
        .unwrap();
    let mut expired = t.clone();
    expired.ticket_id = "aa".repeat(16);
    expired.psk = "bb".repeat(32);
    let wire = r.hub.connect(&t.label).await;
    assert_eq!(
        FakePreview::connect(wire, public, &expired).await.err(),
        Some(HandshakeError::Rejected("expired".into()))
    );

    // 连接的是别的 daemon（静态公钥不是钉住的那把）：daemon 读得了第一条（它不验证对端），但预览页读不了回复
    // ——这里反过来：预览页钉了错的公钥，第一条就读不了，daemon 回 malformed
    let wire = r.hub.connect(&t.label).await;
    let err = FakePreview::connect(wire, [9u8; 32], &t).await.err();
    assert!(
        matches!(err, Some(HandshakeError::Rejected(ref m)) if m == "malformed")
            || matches!(err, Some(HandshakeError::Noise(_))),
        "{err:?}"
    );

    // 端口不再允许（从 allow_ports 里去掉；上游在监听但不是会话起的）
    r.set("preview.allow_ports", "[]");
    let wire = r.hub.connect(&t.label).await;
    assert_eq!(
        FakePreview::connect(wire, public, &t).await.err(),
        Some(HandshakeError::Rejected("port_not_allowed".into()))
    );

    // 没收到 preview_peer 的连接：忽略，不回复
    let wire = r.hub.connect_unregistered();
    let mut stray = FakePreview::new(wire, public, &t.label, &t.ticket_id, &t.psk);
    stray.handshake_timeout = Duration::from_millis(400);
    assert_eq!(stray.handshake().await, Err(HandshakeError::Timeout));
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
}

#[tokio::test]
async fn a_new_first_message_replaces_the_old_transport_and_its_streams() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let slow = pv.start_request("GET", "/slow", &[], None).await;
    assert_eq!(slow, 1);
    pv.settle(Duration::from_millis(100)).await;
    // 重新握手：旧传输和全部 stream 作废，stream id 从头开始
    assert!(pv.handshake().await.is_ok());
    assert_eq!(r.td.ctx.preview.tunnel_count(), 1, "one transport per peer");
    let res = pv.get("/hello").await;
    assert_eq!((res.stream, res.status, res.text().as_str()), (1, 200, "hello"));
}

#[tokio::test]
async fn at_most_64_tunnels_at_a_time() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let mut tunnels = Vec::new();
    for _ in 0..64 {
        tunnels.push(r.connect(&t).await);
    }
    assert_eq!(r.td.ctx.preview.tunnel_count(), 64);
    let wire = r.hub.connect(&t.label).await;
    assert_eq!(FakePreview::connect(wire, r.public(), &t).await.err(), Some(HandshakeError::Rejected("busy".into())));
    // 一条断开之后又有空位
    tunnels[0].disconnect().await;
    assert!(eventually(T, || async { r.td.ctx.preview.tunnel_count() == 63 }).await);
    let mut again = r.connect(&t).await;
    assert_eq!(again.get("/hello").await.status, 200);
    // 一台设备至多 64 张未过期的票据由 `trim` 管，见 `the_oldest_ticket_of_a_device_is_dropped_past_64`
}

// ===================================================================== HTTP

#[tokio::test]
async fn requests_are_rewritten_for_the_local_server_and_responses_come_back() {
    let mut r = Rig::new().await;
    let (t, mut pv) = r.tunnel().await;
    let origin = format!("https://{}.aiplace.app", t.label);
    let referer = format!("{origin}/page?y=2");
    let res = pv
        .fetch(
            "GET",
            "/hello?x=1",
            &[
                ("accept", "text/html"),
                ("origin", &origin),
                ("referer", &referer),
                ("cookie", "sid=abc"),
                ("x-forwarded-for", "1.2.3.4"),
                ("forwarded", "for=1.2.3.4"),
                ("te", "trailers"),
                ("upgrade", "h2c"),
                ("accept-encoding", "br, gzip"),
                ("x-custom", "kept"),
            ],
            None,
        )
        .await;
    assert_eq!((res.status, res.text().as_str()), (200, "hello"));
    assert_eq!(res.header("content-type"), Some("text/plain"));
    assert_eq!(res.header("content-length"), Some("5"), "identity keeps the length");
    assert_eq!(res.encoding, Some(PreviewBodyEncoding::Identity));
    assert!(res.ended && res.reset.is_none());
    let seen = r.up.last_request("/hello").expect("the upstream saw the request");
    assert_eq!((seen.method.as_str(), seen.path.as_str()), ("GET", "/hello?x=1"));
    let local = format!("localhost:{}", r.up.port);
    assert_eq!(seen.header("host"), Some(local.as_str()));
    assert_eq!(seen.header("origin"), Some(format!("http://{local}").as_str()));
    assert_eq!(seen.header("referer"), Some(format!("http://{local}/page?y=2").as_str()));
    assert_eq!(seen.header("accept-encoding"), Some("gzip"));
    assert_eq!(seen.header("cookie"), Some("sid=abc"));
    assert_eq!(seen.header("x-custom"), Some("kept"));
    assert_eq!(seen.header("accept"), Some("text/html"));
    for gone in ["x-forwarded-for", "forwarded", "te", "upgrade"] {
        assert_eq!(seen.header(gone), None, "{gone} is not forwarded");
    }
}

#[tokio::test]
async fn post_bodies_larger_than_the_stream_window_flow_through_and_get_echoed() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let body = big_body(600_000);
    let res = pv.fetch("POST", "/echo", &[("content-type", "application/octet-stream")], Some(&body)).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("x-method"), Some("POST"));
    assert_eq!(res.decoded(), body, "600 KB out and back");
    let seen = r.up.last_request("/echo").unwrap();
    assert_eq!(seen.body.len(), 600_000);
    // 没有 content-length 的请求体（预览页边读边发）：上游收到的是 chunked，内容一样
    let res = pv.fetch("PUT", "/echo", &[], Some(b"small body")).await;
    assert_eq!((res.status, res.decoded()), (200, b"small body".to_vec()));
    assert_eq!(r.up.last_request("/echo").unwrap().body, b"small body");
    // 空的 POST
    let res = pv.fetch("POST", "/echo", &[("content-length", "0")], None).await;
    assert_eq!((res.status, res.body.len()), (200, 0));
}

#[tokio::test]
async fn redirects_cookies_chunked_and_status_codes_pass_through() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let res = pv.get("/redirect").await;
    assert_eq!(
        (res.status, res.header("location")),
        (302, Some("/landing?x=1")),
        "absolute localhost URL becomes a path"
    );
    let res = pv.get("/cookies").await;
    assert_eq!(res.headers_named("set-cookie"), vec!["a=1; Path=/; HttpOnly", "b=2; Path=/"], "order kept, not merged");
    // 分块响应：transfer-encoding 不转；长度未知的文本会被压缩
    let res = pv.get("/chunked").await;
    assert_eq!((res.status, res.text().as_str()), (200, "alphabetagamma"));
    assert_eq!(res.header("transfer-encoding"), None);
    assert_eq!(res.header("connection"), None);
    assert_eq!(pv.get("/status?code=204").await.status, 204);
    assert_eq!(pv.get("/status?code=404").await.status, 404);
    assert_eq!(pv.get("/nope").await.status, 404);
    // HEAD：有头无体，content-length 保留
    let res = pv.fetch("HEAD", "/hello", &[], None).await;
    assert_eq!((res.status, res.header("content-length"), res.body.len()), (200, Some("5"), 0));
    assert!(res.ended);
}

#[tokio::test]
async fn text_is_gzipped_on_the_fly_and_upstream_gzip_is_passed_through() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    // 大于 1 KiB 的文本：daemon 压缩
    let res = pv.get("/text?n=200000").await;
    assert_eq!(res.encoding, Some(PreviewBodyEncoding::Gzip));
    assert_eq!(res.header("content-length"), None);
    assert_eq!(res.header("content-encoding"), None);
    assert!(res.body.len() < 20_000, "compressed: {}", res.body.len());
    assert_eq!(res.decoded(), compressible_text(200_000));
    assert_eq!(r.up.last_request("/text").unwrap().header("accept-encoding"), Some("gzip"));
    // 上游自己压缩的：原样转发，标 gzip，去掉 content-encoding / content-length
    let res = pv.get("/gz").await;
    assert_eq!(res.encoding, Some(PreviewBodyEncoding::Gzip));
    assert_eq!((res.header("content-length"), res.header("content-encoding")), (None, None));
    assert_eq!(res.text(), "gzipped by the upstream".repeat(100));
    // 小的文本和非文本不压
    let res = pv.get("/hello").await;
    assert_eq!((res.encoding, res.header("content-length")), (Some(PreviewBodyEncoding::Identity), Some("5")));
    let res = pv.get("/big?n=100000").await;
    assert_eq!((res.encoding, res.header("content-length")), (Some(PreviewBodyEncoding::Identity), Some("100000")));
    assert_eq!(res.body, big_body(100_000));
}

#[tokio::test]
async fn a_response_larger_than_the_in_flight_window_keeps_flowing_on_acks() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let n = 12 * MIB;
    let started = Instant::now();
    let res = pv.get(&format!("/big?n={n}")).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body.len(), n);
    assert!(res.body == big_body(n), "content intact");
    assert!(pv.cipher_bytes > n, "ciphertext counted");
    assert!(started.elapsed() < Duration::from_secs(60));
    // 回执让保留的信封原文被释放：12 MiB 传完之后留着的不超过在途窗口（按时间 / 字节逐出的话会留着全部 12 MiB）
    let kept = r.td.ctx.outbox.retained_bytes(&pv.peer_id());
    assert!(kept <= 8 * MIB, "retained {kept} bytes");
}

#[tokio::test]
async fn the_daemon_stops_at_8_mib_unacknowledged_and_resumes_when_acked() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    pv.auto_ack = false;
    let n = 20 * MIB;
    let id = pv.start_request("GET", &format!("/big?n={n}"), &[], None).await;
    // 等到它不再前进（debug 构建里加解密慢，8 MiB 要几秒）
    let mut last = 0;
    loop {
        pv.settle(Duration::from_millis(700)).await;
        if pv.cipher_bytes == last && last > 0 {
            break;
        }
        last = pv.cipher_bytes;
    }
    let stalled_at = pv.cipher_bytes;
    assert!(stalled_at <= 8 * MIB, "never more than 8 MiB in flight: {stalled_at}");
    assert!(stalled_at >= 7 * MIB, "but the window is used: {stalled_at}");
    pv.settle(Duration::from_millis(500)).await;
    assert_eq!(pv.cipher_bytes, stalled_at, "nothing more until acked");
    // 回执一下，继续
    pv.ack_now().await;
    assert!(pv.wait_until(T, |p| p.cipher_bytes > stalled_at + MIB).await, "resumed after the ack");
    // 手动回执模式：每轮回执一次，它就多走一段，直到完成
    let deadline = Instant::now() + Duration::from_secs(120);
    while !pv.response(id).ended {
        pv.ack_now().await;
        pv.settle(Duration::from_millis(50)).await;
        assert!(Instant::now() < deadline, "stuck at {} bytes", pv.body_len(id));
    }
    assert!(pv.response(id).body == big_body(n));
}

#[tokio::test]
async fn the_daemon_stops_at_the_stream_window_until_it_is_returned() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    pv.auto_window = false;
    let n = MIB;
    let id = pv.start_request("GET", &format!("/big?n={n}"), &[], None).await;
    // 等到它不再前进（机器忙的时候一个固定的等待时间不够）
    let mut last = 0;
    loop {
        pv.settle(Duration::from_millis(400)).await;
        let now = pv.body_len(id);
        if now == last && now > 0 {
            break;
        }
        last = now;
    }
    let first = pv.body_len(id);
    assert!((256 * 1024 - 62 * 1024..=256 * 1024).contains(&first), "the 256 KiB window is the limit: {first}");
    pv.settle(Duration::from_millis(400)).await;
    assert_eq!(pv.body_len(id), first, "nothing moves without a WINDOW");
    assert!(!pv.response(id).ended);
    // 归还窗口，它继续；每次都停在新的窗口上
    let mut rounds = 0;
    while !pv.response(id).ended {
        pv.return_window(id).await;
        pv.settle(Duration::from_millis(100)).await;
        rounds += 1;
        assert!(rounds < 100, "stuck at {} bytes", pv.body_len(id));
    }
    assert!(rounds >= 3, "{rounds}");
    assert!(pv.response(id).body == big_body(n));
}

#[tokio::test]
async fn data_beyond_the_window_resets_the_stream_with_flow_control() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let id = pv.open_request("POST", "/echo", &[("content-type", "application/octet-stream")]).await;
    // 窗口 256 KiB；一口气发 5 × 60000，daemon 还没来得及消费
    for _ in 0..5 {
        pv.send_frames(&[Frame::new(FrameType::Data, id, vec![1; 60_000])]).await;
    }
    assert!(pv.wait_until(T, |p| p.reset_of(id).is_some()).await);
    assert_eq!(pv.reset_of(id), Some(PreviewResetCode::FlowControl));
    // 隧道本身没事，别的 stream 照常
    assert_eq!(pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn stream_ids_must_increase_and_the_open_stream_limit_is_enforced() {
    raise_fd_limit();
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    // 重复使用的 id
    let head = PreviewRequestHead { method: "GET".into(), path: "/hello".into(), headers: vec![], websocket: false };
    let payload = anycon_protocol::tunnel::encode_head(&head).unwrap();
    pv.send_frames(&[Frame::new(FrameType::Request, 5, payload.clone()), Frame::new(FrameType::End, 5, vec![])]).await;
    assert!(pv.wait_until(T, |p| p.response(5).ended).await);
    pv.send_frames(&[Frame::new(FrameType::Request, 5, payload.clone()), Frame::new(FrameType::End, 5, vec![])]).await;
    assert!(pv.wait_until(T, |p| p.reset_of(5).is_some()).await);
    assert_eq!(pv.reset_of(5), Some(PreviewResetCode::Protocol));
    pv.send_frames(&[Frame::new(FrameType::Request, 3, payload.clone()), Frame::new(FrameType::End, 3, vec![])]).await;
    assert!(pv.wait_until(T, |p| p.reset_of(3).is_some()).await);
    // 256 个同时打开：第 257 个 RESET { limit }
    let slow = PreviewRequestHead { method: "GET".into(), path: "/slow".into(), headers: vec![], websocket: false };
    let slow = anycon_protocol::tunnel::encode_head(&slow).unwrap();
    for id in 10..10 + 256u32 {
        pv.send_frames(&[Frame::new(FrameType::Request, id, slow.clone()), Frame::new(FrameType::End, id, vec![])])
            .await;
    }
    pv.send_frames(&[Frame::new(FrameType::Request, 1000, slow.clone())]).await;
    assert!(pv.wait_until(T, |p| p.reset_of(1000).is_some()).await);
    assert_eq!(pv.reset_of(1000), Some(PreviewResetCode::Limit));
    // 取消一个：RESET { cancelled } 是预览页发的，daemon 丢掉上游连接
    pv.send_frames(&[Frame::new(
        FrameType::Reset,
        10,
        anycon_protocol::tunnel::encode_head(&PreviewReset { code: PreviewResetCode::Cancelled, message: None })
            .unwrap(),
    )])
    .await;
    pv.send_frames(&[Frame::new(FrameType::Request, 1001, slow)]).await;
    pv.settle(Duration::from_millis(300)).await;
    assert_eq!(pv.reset_of(1001), None, "a slot was freed");
}

#[tokio::test]
async fn an_unreachable_or_silent_upstream_resets_the_stream() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let mut pv = r.connect(&t).await;
    // 上游不响应：120 秒的超时在测试里缩短
    r.td.ctx.preview.set_timeouts(Timeouts { head: Duration::from_millis(300), ..Timeouts::default() });
    let started = Instant::now();
    let res = pv.get("/slow").await;
    assert_eq!(res.reset.as_ref().map(|r| r.code), Some(PreviewResetCode::Timeout));
    assert!(started.elapsed() >= Duration::from_millis(250));

    // 连不上：先 127.0.0.1 再 [::1] 都失败
    let dead = free_port();
    r.set("preview.allow_ports", &format!("[{}, {dead}]", r.up.port));
    let t2 = r.open(dead).await;
    let mut pv2 = r.connect(&t2).await;
    let res = pv2.get("/anything").await;
    assert_eq!(res.reset.as_ref().map(|r| r.code), Some(PreviewResetCode::Refused));
    assert_eq!(res.status, 0, "no response head");
    // 隧道还在，之后服务起来就能用（握手时没人监听也放行）
    assert_eq!(r.td.ctx.preview.tunnel_count(), 2);
}

#[tokio::test]
async fn a_server_that_only_listens_on_ipv6_is_reached() {
    let Ok(l) = tokio::net::TcpListener::bind("[::1]:0").await else { return }; // 没有 IPv6 的机器
    drop(l);
    let v6 = TestUpstream::start_on("[::1]:0").await;
    let mut r = Rig::new().await;
    r.set("preview.allow_ports", &format!("[{}]", v6.port));
    let t = r.open(v6.port).await;
    let mut pv = r.connect(&t).await;
    let res = pv.get("/hello").await;
    assert_eq!((res.status, res.text().as_str()), (200, "hello"));
    assert!(v6.last_request("/hello").is_some());
}

// ===================================================================== WebSocket

#[tokio::test]
async fn websocket_messages_flow_both_ways_with_the_upstream_handshake_rewritten() {
    let mut r = Rig::new().await;
    let (t, mut pv) = r.tunnel().await;
    let origin = format!("https://{}.aiplace.app", t.label);
    let id = pv
        .ws_open(
            "/chat?x=1",
            &[
                ("sec-websocket-protocol", "vite-hmr, other"),
                ("origin", &origin),
                ("sec-websocket-extensions", "permessage-deflate"),
                ("x-forwarded-for", "1.1.1.1"),
                ("cookie", "s=1"),
            ],
        )
        .await
        .expect("the upgrade succeeds");
    let head = pv.response(id);
    assert_eq!(head.status, 101);
    assert_eq!(head.header("sec-websocket-protocol"), Some("vite-hmr"), "the chosen subprotocol comes back");
    // 上游收到的握手
    let hs = r.up.ws_handshakes.lock().unwrap().last().cloned().unwrap();
    let local = format!("localhost:{}", r.up.port);
    assert_eq!(hdr(&hs, "host"), Some(local.as_str()));
    assert_eq!(hdr(&hs, "origin"), Some(format!("http://{local}").as_str()));
    assert_eq!(hdr(&hs, "sec-websocket-protocol"), Some("vite-hmr, other"));
    assert_eq!(hdr(&hs, "cookie"), Some("s=1"));
    assert_eq!(hdr(&hs, "x-forwarded-for"), None);
    assert_eq!(hdr(&hs, "sec-websocket-extensions"), None);
    assert!(hdr(&hs, "sec-websocket-key").is_some_and(|k| !k.is_empty()));

    pv.ws_send_text(id, "hi").await;
    assert_eq!(pv.ws_recv(id, T).await, Some(WsEvent::Message { binary: false, data: b"echo:hi".to_vec() }));
    // 二进制，且比一个窗口大（分段、窗口来回归还）
    let blob = big_body(200_000);
    pv.ws_send(id, true, &blob).await;
    assert_eq!(pv.ws_recv(id, T).await, Some(WsEvent::Message { binary: true, data: blob }));
    // 服务器发大消息（拆成多段）
    pv.ws_send_text(id, "big").await;
    assert_eq!(pv.ws_recv(id, T).await, Some(WsEvent::Message { binary: false, data: vec![b'x'; 300_000] }));
    // 空消息
    pv.ws_send(id, true, b"").await;
    assert_eq!(pv.ws_recv(id, T).await, Some(WsEvent::Message { binary: true, data: vec![] }));
}

#[tokio::test]
async fn websocket_close_works_from_either_side_and_ends_the_stream() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    // 上游先关
    let a = pv.ws_open("/a", &[]).await.unwrap();
    pv.ws_send_text(a, "close").await;
    assert_eq!(pv.ws_recv(a, T).await, Some(WsEvent::Close { code: 4000, reason: "bye".into() }));
    pv.ws_close(a, 4000, "bye").await;
    // 预览页先关
    let b = pv.ws_open("/b", &[]).await.unwrap();
    pv.ws_send_text(b, "ping").await;
    assert_eq!(pv.ws_recv(b, T).await, Some(WsEvent::Message { binary: false, data: b"echo:ping".to_vec() }));
    pv.ws_close(b, 1000, "done").await;
    assert_eq!(pv.ws_recv(b, T).await, Some(WsEvent::Close { code: 1000, reason: "done".into() }));
    // 隧道没事，stream 都结束了，可以继续用
    pv.settle(Duration::from_millis(200)).await;
    assert_eq!(pv.get("/hello").await.status, 200);
    let c = pv.ws_open("/c", &[]).await.unwrap();
    pv.ws_send_text(c, "again").await;
    assert_eq!(pv.ws_recv(c, T).await, Some(WsEvent::Message { binary: false, data: b"echo:again".to_vec() }));
}

#[tokio::test]
async fn a_rejected_websocket_upgrade_is_an_ordinary_response() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    let res = pv.ws_open("/reject", &[]).await.expect_err("the upstream refuses the upgrade");
    assert_eq!(res.status, 403);
    assert_eq!(res.text(), "no websockets here");
    assert!(res.ended);
    // 上游连不上：RESET { refused }
    r.set("preview.allow_ports", &format!("[{}]", r.up.port));
    let dead = free_port();
    r.set("preview.allow_ports", &format!("[{}, {dead}]", r.up.port));
    let t2 = r.open(dead).await;
    let mut pv2 = r.connect(&t2).await;
    let res = pv2.ws_open("/x", &[]).await.expect_err("nothing listens");
    assert_eq!(res.reset.map(|r| r.code), Some(PreviewResetCode::Refused));
}

// ===================================================================== hub 的退回和限速

#[tokio::test]
async fn bounced_envelopes_are_resent_in_order_and_the_transport_stays_in_sync() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    // 下载 2 MiB 的过程中 hub 连续退回 3 封（粘住：被退回的重发之前后面的也退回）
    r.hub.bounce_next(&pv.peer_id(), 3);
    let n = 2 * MIB;
    let res = pv.get(&format!("/big?n={n}")).await;
    assert!(res.body == big_body(n), "all the bytes, in order");
    assert!(r.hub.stats.data_bounced.load(std::sync::atomic::Ordering::SeqCst) >= 3);
    assert!(!pv.transport_reset, "a bounce is not a reset");
    assert_eq!(r.td.ctx.preview.tunnel_count(), 1);
    // 之后还能用（Noise 没有失步：解密在 FakePreview 里是 expect）
    assert_eq!(pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn rate_limited_to_a_peer_pauses_only_that_peer() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let mut a = r.connect(&t).await;
    let mut b = r.connect(&t).await;
    let outbox = r.td.ctx.outbox.clone();
    let started = Instant::now();
    r.hub.control(Control::RateLimited { retry_after_ms: 1500, to: Some(a.peer_id()) }).await;
    assert!(outbox.peer_pause_left(&a.peer_id()) > Duration::from_millis(1000));
    assert_eq!(outbox.peer_pause_left(&b.peer_id()), Duration::ZERO);
    assert_eq!(outbox.rate_pause_left(), Duration::ZERO, "the connection as a whole is not paused");

    let ida = a.start_request("GET", "/hello", &[], None).await;
    // B 照常
    let res = b.get("/hello").await;
    assert_eq!(res.status, 200);
    assert!(started.elapsed() < Duration::from_millis(1000));
    // A 的响应被压住
    a.settle(Duration::from_millis(400)).await;
    assert_eq!(a.response(ida).status, 0, "paused");
    let res = a.finish(ida, Duration::from_secs(10)).await;
    assert_eq!(res.status, 200);
    assert!(started.elapsed() >= Duration::from_millis(1300), "{:?}", started.elapsed());

    // 没有 `to`：整条连接暂停
    r.hub.control(Control::RateLimited { retry_after_ms: 700, to: None }).await;
    assert!(outbox.rate_pause_left() > Duration::from_millis(400));
    let t0 = Instant::now();
    let res = b.get("/hello").await;
    assert_eq!(res.status, 200);
    assert!(t0.elapsed() >= Duration::from_millis(500), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn a_bounce_for_a_lost_envelope_resets_the_transport() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    assert_eq!(pv.get("/hello").await.status, 200);
    // hub 说退回了一个我们根本没留着的 seq：传输少了一封，只能重置
    r.hub.control(Control::Backpressure { to: pv.peer_id(), seq: 999_999 }).await;
    assert!(pv.wait_until(T, |p| p.transport_reset).await, "the page is told to start over");
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
    // 预览页重新握手就好
    assert!(pv.handshake().await.is_ok());
    assert_eq!(pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn undeliverable_and_peer_offline_drop_the_tunnel() {
    let mut r = Rig::new().await;
    let (t, mut pv) = r.tunnel().await;
    assert_eq!(r.td.ctx.preview.tunnel_count(), 1);
    r.hub.control(Control::Undeliverable { to: pv.peer_id(), seq: 1 }).await;
    assert!(eventually(T, || async { r.td.ctx.preview.tunnel_count() == 0 }).await);
    // 它又上来重新握手
    assert!(pv.handshake().await.is_ok());
    assert_eq!(r.td.ctx.preview.tunnel_count(), 1);
    pv.disconnect().await;
    assert!(
        eventually(T, || async { r.td.ctx.preview.tunnel_count() == 0 && !r.td.ctx.preview.is_peer("nope") }).await
    );
    // 断开的对端不再被认：它的 id 发来的握手没有人理
    let mut again = r.connect(&t).await;
    assert_eq!(again.get("/hello").await.status, 200);
}

#[tokio::test]
async fn a_broken_transport_is_reset_on_decrypt_failure_or_a_fragment() {
    let mut r = Rig::new().await;
    let (_t, mut pv) = r.tunnel().await;
    // 乱码密文
    pv.send_raw_data({
        let mut b = vec![0u8]; // WHOLE
        b.extend_from_slice(&[7u8; 64]);
        b
    })
    .await;
    assert!(pv.wait_until(T, |p| p.transport_reset).await, "decrypt failure -> RESET");
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
    assert!(pv.handshake().await.is_ok());
    // FRAGMENT 帧体
    let mut frag = vec![1u8];
    frag.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 2, 9, 9, 9]);
    pv.send_raw_data(frag).await;
    assert!(pv.wait_until(T, |p| p.transport_reset).await, "FRAGMENT -> RESET");
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
    assert!(pv.handshake().await.is_ok());
    assert_eq!(pv.get("/hello").await.status, 200);
}

// ===================================================================== 作废与断开

#[tokio::test]
async fn preview_close_sends_goaway_and_voids_the_tickets() {
    let mut r = Rig::new().await;
    let (t, mut pv) = r.tunnel().await;
    let _: Empty = r.call(methods::PREVIEW_CLOSE, json!({ "label": t.label })).await.unwrap();
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await);
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Closed);
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
    let listed: PreviewListResult = r.call(methods::PREVIEW_LIST, json!({})).await.unwrap();
    assert!(listed.previews.is_empty());
    assert_eq!(pv.handshake().await, Err(HandshakeError::Rejected("unknown_ticket".into())));
    // 之后 open 重新签发一张，label 不变
    let again = r.open(r.up.port).await;
    assert_eq!(again.label, t.label);
    assert_ne!(again.ticket_id, t.ticket_id);
    let audit: NodeAuditResult = r.call(methods::NODE_AUDIT, json!({})).await.unwrap();
    let close = audit.entries.iter().find(|e| e.action == "preview.close").unwrap();
    assert_eq!(close.detail, json!({ "tickets": 1, "tunnels": 1 }));
}

#[tokio::test]
async fn revoking_the_issuing_device_voids_its_tickets_and_closes_its_tunnels() {
    let mut r = Rig::new().await;
    r.td.ctx
        .peers
        .add_test_peer(PeerRow {
            device_id: "dev-a".into(),
            pubkey: [1u8; 32],
            name: "Alice phone".into(),
            kind: DeviceKind::Web,
            paired_at_ms: 1,
            last_seen_ms: None,
        })
        .await;
    let (t, mut pv) = r.tunnel().await; // 签发者 dev-a
    let mut other = r.td.client("b");
    let tb: PreviewOpenResult = other.call(&r.td, methods::PREVIEW_OPEN, json!({ "port": r.up.port })).await.unwrap();
    let mut pvb = r.connect(&tb).await;
    let listed: PreviewListResult = r.call(methods::PREVIEW_LIST, json!({})).await.unwrap();
    assert!(listed.previews.iter().any(|p| p.created_by_name == "Alice phone"));

    let _: Empty = other.call(&r.td, methods::NODE_REVOKE_DEVICE, json!({ "device_id": "dev-a" })).await.unwrap();
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await);
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Revoked);
    assert_eq!(pv.handshake().await, Err(HandshakeError::Rejected("unknown_ticket".into())));
    assert!(r.td.ctx.store.preview_ticket_get(&t.ticket_id).await.unwrap().is_none());
    // 别的设备签发的不受影响
    assert!(pvb.goaway.is_none());
    assert_eq!(pvb.get("/hello").await.status, 200);
    assert!(r.td.ctx.store.preview_ticket_get(&tb.ticket_id).await.unwrap().is_some());
}

#[tokio::test]
async fn expired_tickets_are_swept_and_their_tunnels_get_goaway_expired() {
    let mut r = Rig::new().await;
    let t = r.open(r.up.port).await;
    let now = anycon_daemon::util::now_ms();
    r.td.ctx
        .store
        .preview_ticket_insert(&PreviewTicketRow {
            ticket_id: "5a".repeat(16),
            label: t.label.clone(),
            port: t.port,
            psk: "6b".repeat(32),
            device_id: "dev-a".into(),
            session_id: None,
            created_at_ms: now,
            expires_at_ms: now + 700,
        })
        .await
        .unwrap();
    let short =
        PreviewOpenResult { ticket_id: "5a".repeat(16), psk: "6b".repeat(32), expires_at_ms: now + 700, ..t.clone() };
    let mut short_pv = r.connect(&short).await;
    let mut long_pv = r.connect(&t).await;
    assert_eq!(short_pv.get("/hello").await.status, 200);
    tokio::time::sleep(Duration::from_millis(800)).await;
    r.td.ctx.preview.sweep().await;
    assert!(short_pv.wait_until(T, |p| p.goaway.is_some()).await);
    assert_eq!(short_pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Expired);
    assert!(r.td.ctx.store.preview_ticket_get(&short.ticket_id).await.unwrap().is_none(), "removed from the database");
    assert!(long_pv.goaway.is_none());
    assert_eq!(long_pv.get("/hello").await.status, 200);
}

#[tokio::test]
async fn the_oldest_ticket_of_a_device_is_dropped_past_64_and_its_tunnel_closed() {
    let mut r = Rig::new().await;
    let second = TestUpstream::start().await;
    r.set("preview.allow_ports", &format!("[{}, {}]", r.up.port, second.port));
    let label = anycon_protocol::tunnel::label_from_bytes(&[3u8; 16]);
    let now = anycon_daemon::util::now_ms();
    // 64 张：最旧的一张用 up.port（并建隧道）
    r.td.ctx.store.preview_label_ensure(r.up.port, &label, now).await.unwrap();
    for i in 0..64u64 {
        r.td.ctx
            .store
            .preview_ticket_insert(&PreviewTicketRow {
                ticket_id: format!("{i:032x}"),
                label: if i == 0 { label.clone() } else { anycon_protocol::tunnel::label_from_bytes(&[i as u8; 16]) },
                port: if i == 0 { r.up.port } else { 40_000 + i as u16 },
                psk: format!("{:064x}", i + 1),
                device_id: "dev-a".into(),
                session_id: None,
                created_at_ms: now - 10_000 + i,
                expires_at_ms: now + 3_600_000,
            })
            .await
            .unwrap();
    }
    let oldest = PreviewOpenResult {
        label: label.clone(),
        port: r.up.port,
        ticket_id: format!("{:032x}", 0),
        psk: format!("{:064x}", 1),
        expires_at_ms: now + 3_600_000,
        session_id: None,
    };
    let mut pv = r.connect(&oldest).await;
    // 第 65 张（另一个端口，不复用）：最旧的被删，它的隧道收到 GOAWAY
    let _t = r.open(second.port).await;
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await);
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::Closed);
    assert!(r.td.ctx.store.preview_ticket_get(&oldest.ticket_id).await.unwrap().is_none());
    assert_eq!(r.td.ctx.store.preview_tickets_of_device("dev-a", 0).await.unwrap().len(), 64);
}

#[tokio::test]
async fn the_port_policy_is_rechecked_while_streams_are_active() {
    let mut r = Rig::new().await;
    r.td.ctx.preview.set_recheck_every(Duration::from_millis(200));
    let (_t, mut pv) = r.tunnel().await;
    // 一个一直挂着的 stream = 有活动
    let _hang = pv.start_request("GET", "/slow", &[], None).await;
    pv.settle(Duration::from_millis(300)).await;
    assert!(pv.goaway.is_none(), "still allowed");
    // 端口不再允许（上游在监听，但不是会话起的，也不在 allow_ports 里）
    r.set("preview.allow_ports", "[]");
    assert!(pv.wait_until(T, |p| p.goaway.is_some()).await, "no GOAWAY after the policy changed");
    assert_eq!(pv.goaway.as_ref().unwrap().code, PreviewGoawayCode::PortNotAllowed);
    assert_eq!(r.td.ctx.preview.tunnel_count(), 0);
}

// ===================================================================== preview.ports 与会话归属

#[tokio::test]
async fn ports_lists_the_configured_ports_with_whatever_listens_on_them() {
    let mut r = Rig::new().await;
    let ports: PreviewPortsResult = r.call(methods::PREVIEW_PORTS, json!({})).await.unwrap();
    let mine = ports.ports.iter().find(|p| p.port == r.up.port).expect("the allowed port is listed");
    assert_eq!((mine.source, mine.session_id.clone()), (PreviewPortSource::Config, None));
    // Windows 上不用 lsof，直接从系统拿（`preview::ports` 的 Windows 部分）
    if have_lsof() || cfg!(windows) {
        assert_eq!(ports.detection, PreviewDetection::Ok);
        assert_eq!(mine.pid, Some(std::process::id()), "who listens on it");
        assert!(mine.process.as_deref().is_some_and(|p| !p.is_empty()));
        // 没有会话起的服务，也没有别的配置端口：只有这一个
        assert_eq!(ports.ports.iter().filter(|p| p.source != PreviewPortSource::Config).count(), 0);
    }
    // 带 session_id：外加 config 的
    let s: SessionSummary = r.call(methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let scoped: PreviewPortsResult = r.call(methods::PREVIEW_PORTS, json!({ "session_id": s.id })).await.unwrap();
    assert!(scoped.ports.iter().any(|p| p.port == r.up.port && p.source == PreviewPortSource::Config));
}

#[tokio::test]
async fn without_a_working_lsof_only_the_configured_ports_are_available() {
    let up = TestUpstream::start().await;
    let td = TestDaemon::start_with(&[("preview.allow_ports", &format!("[{}]", up.port))], false).await;
    // 命令都跑不起来（或者不存在）：detection = unavailable
    td.ctx.preview.detector().set_runner(std::sync::Arc::new(NoCommands));
    let mut c = td.client("a");
    let ports: PreviewPortsResult = c.call(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    assert_eq!(ports.detection, PreviewDetection::Unavailable);
    assert_eq!(ports.ports.iter().map(|p| p.port).collect::<Vec<_>>(), vec![up.port]);
    assert_eq!(ports.ports[0].source, PreviewPortSource::Config);
    // 配置里的端口照常能开；别的不行
    let t: PreviewOpenResult = c.call(&td, methods::PREVIEW_OPEN, json!({ "port": up.port })).await.unwrap();
    assert_eq!(t.port, up.port);
    let other = TestUpstream::start().await;
    let e = c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": other.port })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("port_not_allowed")));
    // 隧道握手同理：不在配置里的端口不放行
    let hub = LocalPreviewHub::attach(&td);
    let wire = hub.connect(&t.label).await;
    let mut pv = FakePreview::connect(wire, td.ctx.identity.public(), &t).await.expect("allowed by config");
    assert_eq!(pv.get("/hello").await.status, 200);
}

struct NoCommands;

#[async_trait::async_trait]
impl anycon_daemon::preview::ports::CommandRunner for NoCommands {
    async fn run(&self, program: &str, _args: &[String]) -> Result<String, String> {
        Err(format!("{program} is not installed"))
    }
}

/// 起一个监听本机端口的 perl 进程（作为 `sh -c '… & wait'` 的子进程），返回它监听的端口。测试结束时杀掉。
#[cfg(unix)]
struct Listener {
    child: tokio::process::Child,
    port: u16,
}

#[cfg(unix)]
impl Listener {
    /// `cwd`：监听进程的工作目录。没有 perl 返回 `None`（测试跳过）。
    async fn start(cwd: &std::path::Path) -> Option<Listener> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let script = r#"use IO::Socket::INET; $|=1; my $s = IO::Socket::INET->new(Listen=>5, LocalAddr=>"127.0.0.1", LocalPort=>0, Proto=>"tcp", ReuseAddr=>1) or die; print $s->sockport, "\n"; sleep 120;"#;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("perl -e '{script}' & wait"))
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            // 自己一个进程组：结束时整组杀掉（perl 是 sh 的子进程，只杀 sh 它会留着）
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut line = String::new();
        let mut out = BufReader::new(child.stdout.take()?);
        tokio::time::timeout(Duration::from_secs(10), out.read_line(&mut line)).await.ok()?.ok()?;
        let port = line.trim().parse().ok()?;
        Some(Listener { child, port })
    }

    fn agent_pid(&self) -> u32 {
        self.child.id().expect("running")
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            // SAFETY: killpg 只发信号。
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
fn perl_available() -> bool {
    std::process::Command::new("perl").arg("-e").arg("1").status().is_ok_and(|s| s.success())
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_started_under_the_agent_process_is_attributed_to_the_session() {
    if !have_lsof() || !perl_available() {
        eprintln!("skipped: needs lsof and perl");
        return;
    }
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let dir = tempfile::tempdir().unwrap();
    let Some(listener) = Listener::start(dir.path()).await else { return };
    // 这个会话的 agent 进程就是那个 sh；perl 是它的子进程，在监听
    td.fake.set_pid(Some(listener.agent_pid()));
    let s: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let _: SessionSendResult = c
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m1", "content": "hello" }))
        .await
        .unwrap();
    assert!(eventually(T, || async { td.fake.spawn_count() == 1 }).await);
    td.ctx.preview.detector().invalidate().await;

    let ports: PreviewPortsResult = c.call(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    assert_eq!(ports.detection, PreviewDetection::Ok);
    let p = ports.ports.iter().find(|p| p.port == listener.port).unwrap_or_else(|| panic!("{ports:?}"));
    assert_eq!((p.source, p.session_id.as_deref()), (PreviewPortSource::Session, Some(s.id.as_str())));
    assert!(p.pid.is_some() && p.process.as_deref() == Some("perl"), "{p:?}");
    // 别的会话看不到它
    let other: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let scoped: PreviewPortsResult =
        c.call(&td, methods::PREVIEW_PORTS, json!({ "session_id": other.id })).await.unwrap();
    assert!(scoped.ports.iter().all(|p| p.port != listener.port));
    // 不用写 allow_ports 就能开
    td.ctx.preview.detector().invalidate().await;
    let t: PreviewOpenResult =
        c.call(&td, methods::PREVIEW_OPEN, json!({ "port": listener.port, "session_id": s.id })).await.unwrap();
    assert_eq!(t.session_id.as_deref(), Some(s.id.as_str()));

    // agent 进程退出之后，没有工作目录可以认领它（perl 的 cwd 是别处）：不再允许
    let _: SessionSendResult = c
        .call(&td, methods::SESSION_SEND, json!({ "session_id": s.id, "message_id": "m2", "content": "exit now" }))
        .await
        .unwrap();
    let gone = eventually(T, || async {
        td.ctx.preview.detector().invalidate().await;
        td.ctx.sessions.preview_targets().await.iter().all(|t| t.pid.is_none())
    })
    .await;
    assert!(gone, "the pid is cleared when the agent exits");
    let e =
        c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": listener.port })).await.unwrap_err();
    assert_eq!((code(&e), reason(&e).as_deref()), (ErrorCode::Forbidden, Some("port_not_allowed")));
}

#[cfg(unix)]
#[tokio::test]
async fn a_server_running_in_the_sessions_directory_is_attributed_by_working_directory() {
    if !have_lsof() || !perl_available() {
        eprintln!("skipped: needs lsof and perl");
        return;
    }
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let _: Value = c.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "web" })).await.unwrap();
    let project_dir = td.workspace.join("web");
    let Some(listener) = Listener::start(&project_dir).await else { return };
    // 会话在 web 项目里；它的 agent 进程（没有 pid）早就退出了
    let s: SessionSummary =
        c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "web" })).await.unwrap();
    td.ctx.preview.detector().invalidate().await;
    let ports: PreviewPortsResult = c.call(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    let p = ports.ports.iter().find(|p| p.port == listener.port).unwrap_or_else(|| panic!("{ports:?}"));
    assert_eq!((p.source, p.session_id.as_deref()), (PreviewPortSource::Workspace, Some(s.id.as_str())));
    let t: PreviewOpenResult = c.call(&td, methods::PREVIEW_OPEN, json!({ "port": listener.port })).await.unwrap();
    assert_eq!(t.port, listener.port);
    // 没有这个目录里的会话（scratch 会话）看不到它
    let scratch: SessionSummary = c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake" })).await.unwrap();
    let scoped: PreviewPortsResult =
        c.call(&td, methods::PREVIEW_PORTS, json!({ "session_id": scratch.id })).await.unwrap();
    assert!(scoped.ports.iter().all(|p| p.port != listener.port));
    // 删掉会话之后不再允许
    let _: Empty = c.call(&td, methods::SESSION_DELETE, json!({ "session_id": s.id })).await.unwrap();
    td.ctx.preview.detector().invalidate().await;
    let e =
        c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": listener.port })).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("port_not_allowed"));
}

/// 脚本化的命令执行器：`listening` 为真时 `lsof` 报告 pid 4242 在 `port` 上监听、工作目录是 `workdir`；
/// `intruder` 为真时另有 pid 4343 也在同一个端口上监听（IPv6 地址），工作目录在别处。
struct ScriptedRunner {
    listening: std::sync::atomic::AtomicBool,
    intruder: std::sync::atomic::AtomicBool,
    runs: std::sync::atomic::AtomicUsize,
    port: u16,
    workdir: std::path::PathBuf,
}

impl ScriptedRunner {
    fn new(port: u16, workdir: std::path::PathBuf) -> Arc<ScriptedRunner> {
        Arc::new(ScriptedRunner {
            listening: std::sync::atomic::AtomicBool::new(false),
            intruder: std::sync::atomic::AtomicBool::new(false),
            runs: std::sync::atomic::AtomicUsize::new(0),
            port,
            workdir,
        })
    }
}

#[async_trait::async_trait]
impl anycon_daemon::preview::ports::CommandRunner for ScriptedRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<String, String> {
        use std::sync::atomic::Ordering::SeqCst;
        let (on, intruder) = (self.listening.load(SeqCst), self.intruder.load(SeqCst));
        match (program, args.iter().any(|a| a == "cwd")) {
            ("lsof", false) => {
                self.runs.fetch_add(1, SeqCst);
                let mut out = String::new();
                if on {
                    out += &format!("p4242\ncnode\nn127.0.0.1:{}\n", self.port);
                }
                if intruder {
                    out += &format!("p4343\ncother\nn[::1]:{}\n", self.port);
                }
                Ok(out)
            }
            ("lsof", true) => {
                let mut out = String::new();
                if on {
                    out += &format!("p4242\nfcwd\nn{}\n", self.workdir.display());
                }
                if intruder {
                    out += "p4343\nfcwd\nn/somewhere/else\n";
                }
                Ok(out)
            }
            ("ps", _) => Ok("4242 1 4242 /usr/local/bin/node\n4343 1 4343 /usr/bin/other\n".into()),
            _ => Err(format!("{program} is not installed")),
        }
    }
}

#[tokio::test]
async fn a_port_shared_with_a_process_that_is_not_the_sessions_is_neither_allowed_nor_listed() {
    use std::sync::atomic::Ordering::SeqCst;
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let _: Value = c.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "web" })).await.unwrap();
    let s: SessionSummary =
        c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "web" })).await.unwrap();
    let port = free_port();
    let runner = ScriptedRunner::new(port, anycon_daemon::sys::canonicalize(td.workspace.join("web")).unwrap());
    td.ctx.preview.detector().set_runner(runner.clone());
    // 只有会话的服务（127.0.0.1）在监听：允许、列出
    runner.listening.store(true, SeqCst);
    let t: PreviewOpenResult = c.call(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap();
    assert_eq!(t.port, port);
    td.ctx.preview.detector().invalidate().await;
    let listed = c.call::<PreviewPortsResult>(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    let p = listed.ports.iter().find(|p| p.port == port).expect("listed");
    assert_eq!((p.source, p.session_id.as_deref()), (PreviewPortSource::Workspace, Some(s.id.as_str())));
    // 别的进程在 [::1] 上也监听了同一个端口：daemon 先连 127.0.0.1，另一个进程也可能被连到——不允许、不列出
    runner.intruder.store(true, SeqCst);
    td.ctx.preview.detector().invalidate().await;
    let e = c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("port_not_allowed"));
    let listed = c.call::<PreviewPortsResult>(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    assert!(listed.ports.iter().all(|p| p.port != port), "{listed:?}");
    // 隧道握手同样不放行
    let hub = LocalPreviewHub::attach(&td);
    let wire = hub.connect(&t.label).await;
    assert_eq!(
        FakePreview::connect(wire, td.ctx.identity.public(), &t).await.err(),
        Some(HandshakeError::Rejected("port_not_allowed".into()))
    );
    // 写进 allow_ports 就照旧放行
    anycon_daemon::config::set_value(&td.paths, "preview.allow_ports", &format!("[{port}]")).unwrap();
    td.ctx.config.reload().unwrap();
    let again: PreviewOpenResult = c.call(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap();
    assert_eq!(again.port, port);
    let listed = c.call::<PreviewPortsResult>(&td, methods::PREVIEW_PORTS, json!({})).await.unwrap();
    let p = listed.ports.iter().find(|p| p.port == port).unwrap();
    assert_eq!(p.source, PreviewPortSource::Config);
}

#[tokio::test]
async fn a_denial_based_on_a_stale_snapshot_is_checked_again_once() {
    use std::sync::atomic::Ordering::SeqCst;
    let td = TestDaemon::start().await;
    let mut c = td.client("a");
    let _: Value = c.call(&td, methods::WORKSPACE_CREATE, json!({ "name": "web" })).await.unwrap();
    let _s: SessionSummary =
        c.call(&td, methods::SESSION_CREATE, json!({ "agent": "fake", "project": "web" })).await.unwrap();
    let port = free_port();
    let workdir = anycon_daemon::sys::canonicalize(td.workspace.join("web")).unwrap();
    let runner = ScriptedRunner::new(port, workdir);
    td.ctx.preview.detector().set_runner(runner.clone());
    // 还没有人监听：拒绝
    let e = c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("port_not_allowed"));
    // agent 把 dev server 拉起来了。探测结果还很新（不到 0.5 秒）：这次不重新探测，仍然拒绝，不会为每个被拒绝的请求起进程
    runner.listening.store(true, SeqCst);
    let runs_before = runner.runs.load(SeqCst);
    let e = c.call::<PreviewOpenResult>(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap_err();
    assert_eq!(reason(&e).as_deref(), Some("port_not_allowed"));
    assert_eq!(runner.runs.load(SeqCst), runs_before, "a fresh snapshot is trusted");
    // 缓存有一会儿了（但还没过 2 秒的 TTL）：拒绝之前重新探测，这回认出来了
    tokio::time::sleep(Duration::from_millis(700)).await;
    let t: PreviewOpenResult = c.call(&td, methods::PREVIEW_OPEN, json!({ "port": port })).await.unwrap();
    assert_eq!(t.port, port);
    assert_eq!(runner.runs.load(SeqCst), runs_before + 1);
}
