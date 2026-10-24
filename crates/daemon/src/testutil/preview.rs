//! 预览（ADR-39）的测试支撑：进程内的假 hub、Rust 写的预览页客户端、本机上游服务器。
//!
//! - [`LocalPreviewHub`]：把 daemon 的出站通道接到一个 demux 任务上，按 `to` 分发给各个预览连接；用 daemon 自己的
//!   `HubLink::on_frame` / `on_control` 喂入站（走和真 hub 完全相同的路由），可以模拟 hub 的退回（粘住规则）、
//!   `rate_limited { to }`、`undeliverable`；
//! - [`FakePreview`]：预览页一侧——Noise NKpsk0 握手（`Handshake::preview_initiator`）、隧道消息编解码、端到端回执、
//!   stream 窗口、发请求 / 收响应、WebSocket 收发。线路抽象成 [`PreviewWire`]：这里是进程内的 [`LocalWire`]，
//!   端到端测试可以换成连真 hub 的 WebSocket；
//! - [`TestUpstream`]：本机 HTTP + WebSocket 服务器（手写的最小 HTTP/1.1），记录收到的请求。
//!
//! 只在 `cfg(test)` 或 `testing` feature 下编译。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::Read as _;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Transport};
use anycon_protocol::envelope::frame;
use anycon_protocol::tunnel::{self, Frame, FrameType, HandshakeReply};
use anycon_protocol::{
    Control, Envelope, Kind, PreviewBodyEncoding, PreviewGoaway, PreviewHello, PreviewOpenResult, PreviewRequestHead,
    PreviewReset, PreviewResetCode, PreviewResponseHead,
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use super::TestDaemon;
use crate::hub::{HubLink, HubState};
use crate::rpc::Rpc;

// ====================================================================== 线路

/// hub 转发给预览页的一封信封。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    Handshake(Vec<u8>),
    Data(Vec<u8>),
}

/// 预览页和 daemon 之间的线路（hub 信封的收发）。
#[async_trait::async_trait]
pub trait PreviewWire: Send {
    /// 预览页 → daemon：发一封 `handshake` / `data` 信封。
    async fn send(&mut self, kind: Kind, body: Vec<u8>);
    /// daemon → 预览页：等下一封信封。
    async fn recv(&mut self, timeout: Duration) -> Option<Delivery>;
    /// hub 给这条预览连接的临时 id（线路不区分时为空）。
    fn peer_id(&self) -> String {
        String::new()
    }
    /// 预览连接断开（hub 给 daemon 发 `peer_offline`）。
    async fn disconnect(&mut self) {}
    /// 连接被对端（hub）关掉时的 WebSocket close code（还开着或线路没有这个概念时是 `None`）。
    fn close_code(&self) -> Option<u16> {
        None
    }
}

struct Lane {
    tx: mpsc::UnboundedSender<Delivery>,
    /// 接下来多少封 `data` 被退回（`backpressure`）。
    bounce_next: usize,
    /// 被退回、还没重发成功的 seq（粘住：只放行最小的）。
    pending: BTreeSet<u64>,
}

/// 假 hub 的统计。
#[derive(Debug, Default)]
pub struct HubStats {
    /// daemon 发给预览页的 `data` 信封：转发的 / 退回的（含重发）。
    pub data_forwarded: AtomicUsize,
    pub data_bounced: AtomicUsize,
    pub data_bytes: AtomicU64,
}

/// 进程内的假 hub（预览一侧）。
pub struct LocalPreviewHub {
    pub link: Arc<HubLink>,
    rpc: Arc<Rpc>,
    pub node_id: String,
    lanes: Arc<Mutex<HashMap<String, Lane>>>,
    controls: Arc<Mutex<Vec<Control>>>,
    next_peer: AtomicU64,
    pub stats: Arc<HubStats>,
}

impl LocalPreviewHub {
    /// 把 daemon（`TestDaemon::start*`，不连真 hub）的出站通道接到这个假 hub 上。
    pub fn attach(td: &TestDaemon) -> Arc<LocalPreviewHub> {
        let node_id = "node".to_string() + &"0".repeat(28);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        td.ctx.outbox.attach(&node_id, tx);
        td.ctx.identity.set_device(Some(&node_id), Some("test-device-token")).unwrap();
        let link = Arc::new(HubLink {
            config: td.ctx.config.clone(),
            identity: td.ctx.identity.clone(),
            outbox: td.ctx.outbox.clone(),
            state: HubState::new(),
            peers: td.ctx.peers.clone(),
            preview: td.ctx.preview.clone(),
            agents: td.ctx.agents.clone(),
            wake: Arc::new(tokio::sync::Notify::new()),
        });
        let hub = Arc::new(LocalPreviewHub {
            link: link.clone(),
            rpc: td.rpc(),
            node_id,
            lanes: Arc::new(Mutex::new(HashMap::new())),
            controls: Arc::new(Mutex::new(Vec::new())),
            next_peer: AtomicU64::new(1),
            stats: Arc::new(HubStats::default()),
        });
        let (lanes, controls, stats, outbox) =
            (hub.lanes.clone(), hub.controls.clone(), hub.stats.clone(), td.ctx.outbox.clone());
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                outbox.wire_done(bytes.len());
                let Ok(env) = Envelope::decode(&bytes) else { continue };
                match env.kind {
                    Kind::Control => {
                        if let Ok(c) = env.control_body() {
                            controls.lock().unwrap().push(c);
                        }
                    }
                    Kind::Handshake => {
                        if let Some(l) = lanes.lock().unwrap().get(&env.to) {
                            let _ = l.tx.send(Delivery::Handshake(env.body));
                        }
                    }
                    Kind::Notify => {}
                    Kind::Data => {
                        enum Out {
                            Offline,
                            Bounce,
                            Forwarded,
                        }
                        let out = {
                            let mut lanes = lanes.lock().unwrap();
                            match lanes.get_mut(&env.to) {
                                None => Out::Offline,
                                Some(lane) => {
                                    let sticky = lane.pending.iter().next().copied();
                                    let bounce = match sticky {
                                        Some(min) => min != env.seq,
                                        None if lane.bounce_next > 0 => {
                                            lane.bounce_next -= 1;
                                            true
                                        }
                                        None => false,
                                    };
                                    if bounce {
                                        lane.pending.insert(env.seq);
                                        Out::Bounce
                                    } else {
                                        lane.pending.remove(&env.seq);
                                        stats.data_bytes.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                                        let _ = lane.tx.send(Delivery::Data(env.body));
                                        Out::Forwarded
                                    }
                                }
                            }
                        };
                        match out {
                            Out::Offline => {
                                link.on_control(Control::Undeliverable { to: env.to.clone(), seq: env.seq }).await
                            }
                            Out::Bounce => {
                                stats.data_bounced.fetch_add(1, Ordering::SeqCst);
                                link.on_control(Control::Backpressure { to: env.to.clone(), seq: env.seq }).await;
                            }
                            Out::Forwarded => {
                                stats.data_forwarded.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
            }
        });
        hub
    }

    /// 一条新的预览连接接上来：hub 给 daemon 发 `preview_peer { peer, label }`，返回这条连接的线路。
    pub async fn connect(self: &Arc<Self>, label: &str) -> LocalWire {
        let n = self.next_peer.fetch_add(1, Ordering::SeqCst);
        let peer = format!("pv{n:030}");
        let (tx, rx) = mpsc::unbounded_channel();
        self.lanes.lock().unwrap().insert(peer.clone(), Lane { tx, bounce_next: 0, pending: BTreeSet::new() });
        self.link.on_control(Control::PreviewPeer { peer: peer.clone(), label: label.to_string() }).await;
        LocalWire { hub: self.clone(), peer, rx, seq: 0 }
    }

    /// 一条没有被 hub 告知给 daemon 的连接（没有 `preview_peer`）：daemon 必须忽略它发来的握手。
    pub fn connect_unregistered(self: &Arc<Self>) -> LocalWire {
        let n = self.next_peer.fetch_add(1, Ordering::SeqCst);
        let peer = format!("pv{n:030}");
        let (tx, rx) = mpsc::unbounded_channel();
        self.lanes.lock().unwrap().insert(peer.clone(), Lane { tx, bounce_next: 0, pending: BTreeSet::new() });
        LocalWire { hub: self.clone(), peer, rx, seq: 0 }
    }

    /// 让 hub 向 daemon 发一条控制消息（`rate_limited { to }`、`backpressure`…），走真正的 `HubLink::on_control`。
    pub async fn control(&self, c: Control) {
        self.link.on_control(c).await;
    }

    /// 接下来 `n` 封发给 `peer` 的 `data` 被 hub 退回（`backpressure`，粘住规则：被退回的重发之前后面的也退回）。
    pub fn bounce_next(&self, peer: &str, n: usize) {
        if let Some(l) = self.lanes.lock().unwrap().get_mut(peer) {
            l.bounce_next = n;
        }
    }

    /// daemon 发给 hub 的控制消息（取走）。
    pub fn take_controls(&self) -> Vec<Control> {
        std::mem::take(&mut *self.controls.lock().unwrap())
    }
}

/// 进程内线路：预览页发的信封直接交给 `HubLink::on_frame`，daemon 发的从 demux 任务收。
pub struct LocalWire {
    hub: Arc<LocalPreviewHub>,
    /// hub 给这条预览连接的临时 id。
    pub peer: String,
    rx: mpsc::UnboundedReceiver<Delivery>,
    seq: u64,
}

impl LocalWire {
    /// 预览连接断开：hub 给 daemon 发 `peer_offline`。
    pub async fn disconnect(&self) {
        self.hub.lanes.lock().unwrap().remove(&self.peer);
        self.hub.link.on_control(Control::PeerOffline { device: self.peer.clone() }).await;
    }
}

#[async_trait::async_trait]
impl PreviewWire for LocalWire {
    async fn send(&mut self, kind: Kind, body: Vec<u8>) {
        self.seq += 1;
        let bytes = Envelope::new(self.peer.clone(), self.hub.node_id.clone(), kind, self.seq, body).encode().unwrap();
        self.hub.link.on_frame(&bytes, &self.hub.rpc).await;
    }

    async fn recv(&mut self, timeout: Duration) -> Option<Delivery> {
        tokio::time::timeout(timeout, self.rx.recv()).await.ok().flatten()
    }

    fn peer_id(&self) -> String {
        self.peer.clone()
    }

    async fn disconnect(&mut self) {
        LocalWire::disconnect(self).await
    }
}

// ====================================================================== 连真 hub 的线路

/// 经真 hub 的预览线路（docs/protocol.md § 11.1）：在预览域名上开 WebSocket（`Host` / `Origin` 是预览页自己的），
/// 第一帧 `auth { preview_token }`，之后只和令牌绑定的那台 daemon 互发 `handshake` / `data`。
///
/// 作为发送方守 § 3.1 的规则：hub 退回（`backpressure`）的 `seq` 暂存的原文按升序重发；`rate_limited` 期间暂停发送；
/// `peer_online` / `peer_offline` 记下 daemon 在不在线；`dropped` 记下来供断言。
pub struct HubWire {
    ws: super::Ws,
    /// hub 给这条预览连接的临时 id。
    pub id: String,
    /// 令牌绑定的 daemon。
    pub node_id: String,
    seq: u64,
    /// daemon 现在在线吗（认证时的 `peers_online`，之后跟着 `peer_online` / `peer_offline`）。
    pub node_online: bool,
    /// `node_online` 变成 true 的次数（含认证时那次）：断线重连之后会再加一。
    pub node_online_count: u32,
    retained: std::collections::BTreeMap<u64, Vec<u8>>,
    bounced: BTreeSet<u64>,
    resend_at: Option<Instant>,
    paused_until: Option<Instant>,
    close: Option<Option<u16>>,
    queue: VecDeque<Delivery>,
    /// 收到过的 `rate_limited` 和 `dropped`（断言用）。
    pub rate_limited: Vec<(u64, Option<String>)>,
    pub dropped: Vec<(u64, anycon_protocol::DropReason)>,
    /// 收到过多少次 `backpressure`。
    pub bounces: usize,
}

impl std::fmt::Debug for HubWire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubWire").field("id", &self.id).field("node_online", &self.node_online).finish_non_exhaustive()
    }
}

/// 连接预览线路失败。
#[derive(Debug)]
pub enum HubWireError {
    /// WebSocket 升级就被拒绝了（`Origin` / `Host` 不对…）。
    Upgrade(String),
    /// hub 回了 `auth_fail`。
    AuthFail(anycon_protocol::AuthFailReason),
    /// 认证期间连接被关了。
    Closed(Option<u16>),
}

impl HubWire {
    /// `ws_url`：`ws://127.0.0.1:PORT/v1/ws`；`host` / `origin`：预览页的（如 `<label>.preview.localhost:PORT` 和
    /// `http://<label>.preview.localhost:PORT`）；`node_id`：令牌绑定的 daemon 的设备 id。
    pub async fn connect(
        ws_url: &str,
        host: &str,
        origin: &str,
        token: &str,
        node_id: &str,
    ) -> Result<HubWire, HubWireError> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = ws_url.into_client_request().map_err(|e| HubWireError::Upgrade(e.to_string()))?;
        req.headers_mut().insert("host", host.parse().map_err(|_| HubWireError::Upgrade("bad host".into()))?);
        req.headers_mut().insert("origin", origin.parse().map_err(|_| HubWireError::Upgrade("bad origin".into()))?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| HubWireError::Upgrade(e.to_string()))?;
        let mut w = HubWire {
            ws,
            id: String::new(),
            node_id: node_id.to_string(),
            seq: 0,
            node_online: false,
            node_online_count: 0,
            retained: Default::default(),
            bounced: BTreeSet::new(),
            resend_at: None,
            paused_until: None,
            close: None,
            queue: VecDeque::new(),
            rate_limited: Vec::new(),
            dropped: Vec::new(),
            bounces: 0,
        };
        let auth = Control::Auth {
            device_token: None,
            protocol_version: anycon_protocol::PROTOCOL_VERSION,
            client: anycon_protocol::ClientInfo {
                name: "preview".into(),
                kind: anycon_protocol::DeviceKind::Web,
                platform: "test".into(),
                version: "test".into(),
                origin: None,
            },
            preview_token: Some(token.to_string()),
        };
        let bytes = Envelope::control("", 0, &auth).and_then(|e| e.encode()).expect("auth encodes");
        w.ws.send(tokio_tungstenite::tungstenite::Message::binary(bytes))
            .await
            .map_err(|_| HubWireError::Closed(None))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match w.read_one(left).await {
                None => return Err(HubWireError::Closed(None)),
                Some(Control::AuthOk { device_id, peers_online, .. }) => {
                    w.id = device_id;
                    w.node_online = peers_online.contains(&w.node_id);
                    w.node_online_count = u32::from(w.node_online);
                    return Ok(w);
                }
                Some(Control::AuthFail { reason }) => return Err(HubWireError::AuthFail(reason)),
                Some(_) => {}
            }
            if let Some(code) = w.close {
                return Err(HubWireError::Closed(code));
            }
        }
    }

    /// 读一条 WebSocket 消息（最多等 `wait`）并处理；信封里的 `handshake` / `data` 进队列。返回读到的控制消息（如果是）。
    /// 连接关了或没读到东西返回 `None`。
    async fn read_one(&mut self, wait: Duration) -> Option<Control> {
        use tokio_tungstenite::tungstenite::Message;
        if self.close.is_some() {
            return None;
        }
        match tokio::time::timeout(wait, self.ws.next()).await {
            Err(_) => None,
            Ok(None) | Ok(Some(Err(_))) => {
                self.close = Some(None);
                None
            }
            Ok(Some(Ok(Message::Close(f)))) => {
                self.close = Some(f.map(|f| u16::from(f.code)));
                None
            }
            Ok(Some(Ok(Message::Binary(b)))) => {
                let env = Envelope::decode(&b).expect("a well-formed envelope from the hub");
                match env.kind {
                    Kind::Control => {
                        let c = env.control_body().expect("a well-formed control message");
                        self.on_control(&c);
                        Some(c)
                    }
                    Kind::Handshake => {
                        self.queue.push_back(Delivery::Handshake(env.body));
                        None
                    }
                    Kind::Data => {
                        self.queue.push_back(Delivery::Data(env.body));
                        None
                    }
                    Kind::Notify => None,
                }
            }
            Ok(Some(Ok(_))) => None,
        }
    }

    fn on_control(&mut self, c: &Control) {
        let now = Instant::now();
        match c {
            Control::PeerOnline { device } if *device == self.node_id => {
                self.node_online = true;
                self.node_online_count += 1;
            }
            Control::PeerOffline { device } if *device == self.node_id => self.node_online = false,
            Control::Backpressure { seq, .. } => {
                self.bounces += 1;
                if self.retained.contains_key(seq) {
                    self.bounced.insert(*seq);
                    self.resend_at.get_or_insert(now + Duration::from_millis(100));
                }
            }
            Control::RateLimited { retry_after_ms, to } => {
                self.rate_limited.push((*retry_after_ms, to.clone()));
                let until = now + Duration::from_millis((*retry_after_ms).min(30_000));
                self.paused_until = Some(self.paused_until.map_or(until, |t| t.max(until)));
                if !self.bounced.is_empty() {
                    self.resend_at = Some(self.resend_at.map_or(until, |t| t.max(until)));
                }
            }
            Control::Dropped { seq, reason } => self.dropped.push((*seq, *reason)),
            _ => {}
        }
    }

    /// 被退回的信封按升序原样重发（到点了才发，`rate_limited` 期间不发）。
    async fn resend_due(&mut self) {
        let now = Instant::now();
        if self.bounced.is_empty()
            || self.resend_at.is_some_and(|t| t > now)
            || self.paused_until.is_some_and(|t| t > now)
        {
            return;
        }
        self.resend_at = None;
        for seq in std::mem::take(&mut self.bounced) {
            if let Some(bytes) = self.retained.get(&seq).cloned() {
                if self.ws.send(tokio_tungstenite::tungstenite::Message::binary(bytes)).await.is_err() {
                    self.close.get_or_insert(None);
                    return;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl PreviewWire for HubWire {
    async fn send(&mut self, kind: Kind, body: Vec<u8>) {
        // 限速期间等一等（`rate_limited` 暂停的是整个发送方向）
        while let Some(t) = self.paused_until.filter(|t| *t > Instant::now()) {
            let left = t.saturating_duration_since(Instant::now());
            self.read_one(left.min(Duration::from_millis(50))).await;
            self.resend_due().await;
        }
        self.seq += 1;
        let bytes =
            Envelope::new(self.id.clone(), self.node_id.clone(), kind, self.seq, body).encode().expect("encodes");
        // 保留最近发出的原文，hub 退回时重发
        self.retained.insert(self.seq, bytes.clone());
        while self.retained.len() > 8192 {
            self.retained.pop_first();
        }
        if self.ws.send(tokio_tungstenite::tungstenite::Message::binary(bytes)).await.is_err() {
            self.close.get_or_insert(None);
        }
    }

    async fn recv(&mut self, timeout: Duration) -> Option<Delivery> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(d) = self.queue.pop_front() {
                return Some(d);
            }
            self.resend_due().await;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.close.is_some() {
                return None;
            }
            self.read_one(left.min(Duration::from_millis(50))).await;
        }
    }

    fn peer_id(&self) -> String {
        self.id.clone()
    }

    async fn disconnect(&mut self) {
        let _ = self.ws.close(None).await;
        self.close.get_or_insert(None);
    }

    fn close_code(&self) -> Option<u16> {
        self.close.flatten()
    }
}

// ====================================================================== 预览页客户端

/// 握手失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// daemon 回了拒绝（原因字符串，如 `expired`）。
    Rejected(String),
    /// Noise 层面失败（回复读不出来、payload 不对）。
    Noise(String),
    /// 没等到回复。
    Timeout,
}

/// 一个 WebSocket 事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    Message { binary: bool, data: Vec<u8> },
    Close { code: u16, reason: String },
}

/// 一次请求的结果。
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub stream: u32,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub encoding: Option<PreviewBodyEncoding>,
    /// `DATA` 拼起来的原始字节（`encoding = gzip` 时还是压缩的）。
    pub body: Vec<u8>,
    pub ended: bool,
    pub reset: Option<PreviewReset>,
}

impl Fetched {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()).collect()
    }

    /// 解压之后的响应体（`encoding = gzip`）或原样。
    pub fn decoded(&self) -> Vec<u8> {
        if self.encoding == Some(PreviewBodyEncoding::Gzip) {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(&self.body[..]).read_to_end(&mut out).expect("a valid gzip stream");
            out
        } else {
            self.body.clone()
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.decoded()).into_owned()
    }
}

#[derive(Default)]
struct ClientStream {
    resp: Fetched,
    /// 请求方向还能发多少（daemon 给的窗口）。
    send_window: i64,
    /// 收到、还没还给 daemon 的数据字节。
    pending_window: u32,
    ws: VecDeque<WsEvent>,
    ws_buf: Vec<u8>,
    ws_binary: bool,
    ws_open: bool,
}

/// 预览页一侧。
pub struct FakePreview {
    wire: Box<dyn PreviewWire>,
    pub label: String,
    ticket_id: [u8; 16],
    psk: [u8; 32],
    daemon_public: [u8; 32],
    transport: Option<Transport>,
    pub hello: Option<PreviewHello>,
    /// 已发出 / 已收到并解密的隧道消息条数（`ack` 数的就是这个）。
    sent: u64,
    recv: u64,
    unacked_msgs: u64,
    unacked_bytes: usize,
    /// 按 § 11.5 自动回执（关掉之后只能手动 [`FakePreview::ack_now`]，用来测 daemon 的在途窗口）。关掉时发出的消息
    /// 里的 `ack` 停在上一次手动回执的值。
    pub auto_ack: bool,
    manual_ack: u64,
    /// 收到的数据字节消费后自动用 `WINDOW` 归还（关掉用来测 daemon 的发送窗口）。
    pub auto_window: bool,
    next_stream: u32,
    streams: HashMap<u32, ClientStream>,
    /// daemon 发的 `GOAWAY`。
    pub goaway: Option<PreviewGoaway>,
    /// 收到了 daemon 的 `RESET` 数据帧体（传输失步）。
    pub transport_reset: bool,
    /// 收到的密文总字节数（hub 信封体）。
    pub cipher_bytes: usize,
    /// 收到的隧道消息条数 / 其中不带帧的纯回执条数。
    pub messages: u64,
    pub pure_acks: u64,
    /// daemon 回执过的最大值（它收到了我们多少条消息）。
    pub peer_ack: u64,
    /// 收到的所有帧的类型序列（调试 / 断言用）。
    pub frame_log: Vec<(FrameType, u32)>,
    /// 握手等回复的时限（默认 10 秒）。
    pub handshake_timeout: Duration,
    last_data_at: Option<Instant>,
    max_gap: Duration,
}

const STEP: Duration = Duration::from_millis(50);

impl FakePreview {
    pub fn new(
        wire: impl PreviewWire + 'static,
        daemon_public: [u8; 32],
        label: &str,
        ticket_id_hex: &str,
        psk_hex: &str,
    ) -> FakePreview {
        FakePreview {
            wire: Box::new(wire),
            label: label.to_string(),
            ticket_id: hex::decode(ticket_id_hex).unwrap().try_into().unwrap(),
            psk: hex::decode(psk_hex).unwrap().try_into().unwrap(),
            daemon_public,
            transport: None,
            hello: None,
            sent: 0,
            recv: 0,
            unacked_msgs: 0,
            unacked_bytes: 0,
            auto_ack: true,
            manual_ack: 0,
            auto_window: true,
            next_stream: 1,
            streams: HashMap::new(),
            goaway: None,
            transport_reset: false,
            cipher_bytes: 0,
            messages: 0,
            pure_acks: 0,
            peer_ack: 0,
            frame_log: Vec::new(),
            handshake_timeout: Duration::from_secs(10),
            last_data_at: None,
            max_gap: Duration::ZERO,
        }
    }

    /// 用 `preview.open` 的结果建客户端并握手。
    pub async fn connect(
        wire: impl PreviewWire + 'static,
        daemon_public: [u8; 32],
        ticket: &PreviewOpenResult,
    ) -> Result<FakePreview, HandshakeError> {
        let mut p = FakePreview::new(wire, daemon_public, &ticket.label, &ticket.ticket_id, &ticket.psk);
        p.handshake().await?;
        Ok(p)
    }

    /// 发握手第一条、等回复。可以随时重新调用（对应「预览页任何时候都可以重新发第一条消息」）。
    pub async fn handshake(&mut self) -> Result<PreviewHello, HandshakeError> {
        let prologue = tunnel::prologue(&self.label, &self.ticket_id);
        let mut hs = Handshake::preview_initiator(&self.daemon_public, &self.psk, &prologue)
            .map_err(|e| HandshakeError::Noise(e.to_string()))?;
        let first = hs.write(&[]).map_err(|e| HandshakeError::Noise(e.to_string()))?;
        self.transport = None;
        self.streams.clear();
        self.sent = 0;
        self.recv = 0;
        self.unacked_msgs = 0;
        self.unacked_bytes = 0;
        self.next_stream = 1;
        self.goaway = None;
        self.transport_reset = false;
        self.wire.send(Kind::Handshake, tunnel::handshake_init(&self.ticket_id, &first)).await;
        let deadline = Instant::now() + self.handshake_timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.wire.recv(left).await {
                None => return Err(HandshakeError::Timeout),
                Some(Delivery::Data(_)) => continue, // 上一条传输的残余
                Some(Delivery::Handshake(body)) => {
                    return match tunnel::parse_handshake_reply(&body)
                        .map_err(|e| HandshakeError::Noise(e.to_string()))?
                    {
                        HandshakeReply::Reject(r) => Err(HandshakeError::Rejected(r)),
                        HandshakeReply::Accept(noise) => {
                            let payload = hs.read(noise).map_err(|e| HandshakeError::Noise(e.to_string()))?;
                            let hello: PreviewHello =
                                serde_json::from_slice(&payload).map_err(|e| HandshakeError::Noise(e.to_string()))?;
                            self.transport =
                                Some(hs.into_transport().map_err(|e| HandshakeError::Noise(e.to_string()))?);
                            self.hello = Some(hello.clone());
                            Ok(hello)
                        }
                    };
                }
            }
        }
    }

    // ------------------------------------------------------------ 收发

    /// 发一条隧道消息（带当前的 `ack`）。
    pub async fn send_frames(&mut self, frames: &[Frame]) {
        let ack = if self.auto_ack { self.recv } else { self.manual_ack };
        self.send_frames_with_ack(frames, ack).await;
    }

    async fn send_frames_with_ack(&mut self, frames: &[Frame], ack: u64) {
        let plain = tunnel::encode_message(ack, frames).expect("message fits");
        let ct = self.transport.as_mut().expect("handshake first").encrypt(&plain).unwrap();
        let mut bodies = frame::encode(&ct, 0);
        assert_eq!(bodies.len(), 1, "a tunnel message is never fragmented");
        self.sent += 1;
        // 这条消息带着 ack，欠的回执清零
        if ack == self.recv {
            self.unacked_msgs = 0;
            self.unacked_bytes = 0;
        }
        self.wire.send(Kind::Data, bodies.remove(0)).await;
    }

    /// 发一条只有回执的消息（回执到目前为止收到的全部）。
    pub async fn ack_now(&mut self) {
        self.manual_ack = self.recv;
        let ack = self.recv;
        self.send_frames_with_ack(&[], ack).await;
    }

    /// 预览连接的临时 id。
    pub fn peer_id(&self) -> String {
        self.wire.peer_id()
    }

    /// 预览连接断开。
    pub async fn disconnect(&mut self) {
        self.wire.disconnect().await;
    }

    /// 线路被 hub 关掉时的 close code（经真 hub 的线路才有，如令牌被撤销时的 `4004`）。
    pub fn close_code(&self) -> Option<u16> {
        self.wire.close_code()
    }

    /// 直接发一个 `data` 信封体（协议违规测试：`FRAGMENT`、乱码…）。
    pub async fn send_raw_data(&mut self, body: Vec<u8>) {
        self.wire.send(Kind::Data, body).await;
    }

    /// 等并处理一封 daemon 的信封，最多等 `wait`。返回是否收到了东西。
    pub async fn pump(&mut self, wait: Duration) -> bool {
        let step = wait.min(STEP);
        match self.wire.recv(step).await {
            Some(Delivery::Data(body)) => {
                self.on_data(&body).await;
                self.flush_windows(false).await;
                true
            }
            Some(Delivery::Handshake(_)) => true,
            None => {
                // 空闲：欠着的回执 50 ms 内要发
                if self.auto_ack && self.unacked_msgs > 0 {
                    self.ack_now().await;
                }
                self.flush_windows(true).await;
                false
            }
        }
    }

    /// 从上次 [`FakePreview::reset_gap`] 以来，两封相邻的 `data` 信封到达的最大间隔。大响应传输期间它应该很小：
    /// 间隔很大说明发送方在等什么（回执、窗口、时间）。
    pub fn max_recv_gap(&self) -> Duration {
        self.max_gap
    }

    /// 重新开始量 [`FakePreview::max_recv_gap`]（从下一封 `data` 信封算起）。
    pub fn reset_gap(&mut self) {
        self.last_data_at = None;
        self.max_gap = Duration::ZERO;
    }

    async fn on_data(&mut self, body: &[u8]) {
        let now = Instant::now();
        if let Some(last) = self.last_data_at {
            self.max_gap = self.max_gap.max(now.duration_since(last));
        }
        self.last_data_at = Some(now);
        self.cipher_bytes += body.len();
        let ct = match frame::decode(body).expect("a well-formed data envelope") {
            frame::DataFrame::Whole(ct) => ct,
            frame::DataFrame::Reset => {
                self.transport_reset = true;
                return;
            }
            frame::DataFrame::Fragment { .. } => panic!("the daemon fragmented a preview message"),
        };
        let plain = self.transport.as_mut().expect("handshake first").decrypt(ct).expect("decrypts");
        let msg = tunnel::decode_message(&plain).expect("a well-formed tunnel message");
        self.recv += 1;
        self.messages += 1;
        self.peer_ack = self.peer_ack.max(msg.ack);
        if msg.frames.is_empty() {
            self.pure_acks += 1;
        } else {
            self.unacked_msgs += 1;
            self.unacked_bytes += plain.len();
        }
        for f in msg.frames {
            self.frame_log.push((f.ty, f.stream));
            self.on_frame(f);
        }
        if self.auto_ack
            && (self.unacked_msgs >= tunnel::ACK_EVERY_MESSAGES || self.unacked_bytes >= tunnel::ACK_EVERY_BYTES)
        {
            self.ack_now().await;
        }
    }

    fn on_frame(&mut self, f: Frame) {
        match f.ty {
            FrameType::Goaway => self.goaway = tunnel::decode_head(&f.payload).ok(),
            FrameType::Response => {
                let head: PreviewResponseHead = tunnel::decode_head(&f.payload).expect("response head");
                let s = self.streams.entry(f.stream).or_default();
                s.resp.status = head.status;
                s.resp.headers = head.headers;
                s.resp.encoding = Some(head.encoding);
                s.ws_open = head.status == 101;
            }
            FrameType::Data => {
                let s = self.streams.entry(f.stream).or_default();
                s.resp.body.extend_from_slice(&f.payload);
                s.pending_window += f.payload.len() as u32;
            }
            FrameType::End => {
                self.streams.entry(f.stream).or_default().resp.ended = true;
            }
            FrameType::Reset => {
                let r: PreviewReset = tunnel::decode_head(&f.payload).expect("reset payload");
                self.streams.entry(f.stream).or_default().resp.reset = Some(r);
            }
            FrameType::Window => {
                let n = tunnel::parse_window(&f.payload).expect("window payload");
                self.streams.entry(f.stream).or_default().send_window += i64::from(n);
            }
            FrameType::WsMessage => {
                let (flags, data) = tunnel::parse_ws_message(&f.payload).expect("ws payload");
                let s = self.streams.entry(f.stream).or_default();
                s.pending_window += data.len() as u32;
                if s.ws_buf.is_empty() {
                    s.ws_binary = flags & tunnel::WS_BINARY != 0;
                }
                s.ws_buf.extend_from_slice(data);
                if flags & tunnel::WS_FIN != 0 {
                    let data = std::mem::take(&mut s.ws_buf);
                    let binary = s.ws_binary;
                    s.ws.push_back(WsEvent::Message { binary, data });
                }
            }
            FrameType::WsClose => {
                let (code, reason) = tunnel::parse_ws_close(&f.payload).expect("ws close payload");
                self.streams.entry(f.stream).or_default().ws.push_back(WsEvent::Close { code, reason });
            }
            FrameType::Request => {}
        }
    }

    /// 把攒着的窗口用 `WINDOW` 帧还给 daemon。`all`：不论多少都还。
    async fn flush_windows(&mut self, all: bool) {
        if !self.auto_window {
            return;
        }
        let mut frames = Vec::new();
        for (id, s) in self.streams.iter_mut() {
            if s.pending_window >= 32 * 1024 || (all && s.pending_window > 0) {
                frames.push(Frame::new(FrameType::Window, *id, tunnel::window(s.pending_window)));
                s.pending_window = 0;
            }
        }
        if !frames.is_empty() {
            self.send_frames(&frames).await;
        }
    }

    /// 手动归还窗口（`auto_window = false` 时）：把某个 stream 收到的数据字节全还回去。
    pub async fn return_window(&mut self, stream: u32) {
        let n = self.streams.get_mut(&stream).map(|s| std::mem::take(&mut s.pending_window)).unwrap_or(0);
        if n > 0 {
            self.send_frames(&[Frame::new(FrameType::Window, stream, tunnel::window(n))]).await;
        }
    }

    /// 一直处理到 `pred` 为真（或超时）。
    pub async fn wait_until(&mut self, timeout: Duration, mut pred: impl FnMut(&mut FakePreview) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(self) {
                return true;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            self.pump(left).await;
        }
    }

    /// 处理 `d` 这么久（什么都等不到也照样等满）。
    pub async fn settle(&mut self, d: Duration) {
        let deadline = Instant::now() + d;
        while Instant::now() < deadline {
            self.pump(deadline.saturating_duration_since(Instant::now())).await;
        }
    }

    // ------------------------------------------------------------ HTTP

    fn alloc_stream(&mut self) -> u32 {
        let id = self.next_stream;
        self.next_stream += 1;
        self.streams.insert(id, ClientStream { send_window: i64::from(tunnel::STREAM_WINDOW), ..Default::default() });
        id
    }

    /// 只发 `REQUEST`（不发请求体和 `END`），返回 stream id。
    pub async fn open_request(&mut self, method: &str, path: &str, headers: &[(&str, &str)]) -> u32 {
        let id = self.alloc_stream();
        let head = PreviewRequestHead {
            method: method.to_string(),
            path: path.to_string(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            websocket: false,
        };
        self.send_frames(&[Frame::new(FrameType::Request, id, tunnel::encode_head(&head).unwrap())]).await;
        id
    }

    /// 发 `REQUEST`（和请求体、`END`），返回 stream id。请求体按 daemon 给的窗口发，窗口不够就等 `WINDOW`。
    pub async fn start_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> u32 {
        let id = self.open_request(method, path, headers).await;
        match body {
            None => self.send_frames(&[Frame::new(FrameType::End, id, vec![])]).await,
            Some(b) => self.send_body(id, b).await,
        }
        id
    }

    /// 请求体分段发，窗口用完就等 `WINDOW`。最后发 `END`。
    pub async fn send_body(&mut self, id: u32, body: &[u8]) {
        let mut sent = 0;
        while sent < body.len() {
            let avail = self.streams.get(&id).map_or(0, |s| s.send_window);
            if avail <= 0 {
                let ok = self
                    .wait_until(Duration::from_secs(10), |p| {
                        p.streams.get(&id).is_some_and(|s| s.send_window > 0 || s.resp.reset.is_some())
                    })
                    .await;
                assert!(ok, "the daemon did not return request window in time");
                if self.streams.get(&id).is_some_and(|s| s.resp.reset.is_some()) {
                    return;
                }
                continue;
            }
            let n = (avail as usize).min(body.len() - sent).min(32 * 1024);
            self.streams.get_mut(&id).unwrap().send_window -= n as i64;
            self.send_frames(&[Frame::new(FrameType::Data, id, body[sent..sent + n].to_vec())]).await;
            sent += n;
        }
        self.send_frames(&[Frame::new(FrameType::End, id, vec![])]).await;
    }

    /// 等一个 stream 的响应结束（`END` 或 `RESET`）。
    pub async fn finish(&mut self, id: u32, timeout: Duration) -> Fetched {
        let ok = self
            .wait_until(timeout, |p| p.streams.get(&id).is_some_and(|s| s.resp.ended || s.resp.reset.is_some()))
            .await;
        let mut r = self.streams.get(&id).map(|s| s.resp.clone()).unwrap_or_default();
        r.stream = id;
        assert!(ok || r.ended || r.reset.is_some(), "stream {id} did not finish in {timeout:?}: status {}", r.status);
        r
    }

    /// 发请求并等响应结束。
    pub async fn fetch(&mut self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Fetched {
        let id = self.start_request(method, path, headers, body).await;
        self.finish(id, Duration::from_secs(30)).await
    }

    pub async fn get(&mut self, path: &str) -> Fetched {
        self.fetch("GET", path, &[], None).await
    }

    /// 某个 stream 到目前为止收到的响应（不等）。
    pub fn response(&self, id: u32) -> Fetched {
        let mut r = self.streams.get(&id).map(|s| s.resp.clone()).unwrap_or_default();
        r.stream = id;
        r
    }

    // ------------------------------------------------------------ WebSocket

    /// 发 WebSocket 升级请求；成功（`101`）返回 stream id，被拒绝返回那条普通响应。
    pub async fn ws_open(&mut self, path: &str, headers: &[(&str, &str)]) -> Result<u32, Fetched> {
        let id = self.alloc_stream();
        let head = PreviewRequestHead {
            method: "GET".into(),
            path: path.to_string(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            websocket: true,
        };
        self.send_frames(&[Frame::new(FrameType::Request, id, tunnel::encode_head(&head).unwrap())]).await;
        self.wait_until(Duration::from_secs(10), |p| {
            p.streams.get(&id).is_some_and(|s| s.resp.status != 0 || s.resp.reset.is_some())
        })
        .await;
        let s = self.streams.get(&id).unwrap();
        if s.ws_open {
            return Ok(id);
        }
        if s.resp.reset.is_some() {
            return Err(self.response(id));
        }
        // 被拒绝：等响应体收完
        Err(self.finish(id, Duration::from_secs(10)).await)
    }

    pub async fn ws_send(&mut self, id: u32, binary: bool, data: &[u8]) {
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[]] } else { data.chunks(16 * 1024).collect() };
        let last = chunks.len() - 1;
        for (i, c) in chunks.into_iter().enumerate() {
            let flags = (if binary { tunnel::WS_BINARY } else { 0 }) | (if i == last { tunnel::WS_FIN } else { 0 });
            self.send_frames(&[Frame::new(FrameType::WsMessage, id, tunnel::ws_message(flags, c))]).await;
        }
    }

    pub async fn ws_send_text(&mut self, id: u32, text: &str) {
        self.ws_send(id, false, text.as_bytes()).await;
    }

    pub async fn ws_close(&mut self, id: u32, code: u16, reason: &str) {
        self.send_frames(&[Frame::new(FrameType::WsClose, id, tunnel::ws_close(code, reason))]).await;
    }

    /// 下一个 WebSocket 事件（消息或关闭）。
    pub async fn ws_recv(&mut self, id: u32, timeout: Duration) -> Option<WsEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(e) = self.streams.get_mut(&id).and_then(|s| s.ws.pop_front()) {
                return Some(e);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.pump(left).await;
        }
    }

    /// 某个 stream 收到、还没还给 daemon 的数据字节数（`auto_window = false` 时观察 daemon 停在窗口上）。
    pub fn pending_window(&self, id: u32) -> u32 {
        self.streams.get(&id).map_or(0, |s| s.pending_window)
    }

    /// 某个 stream 到目前为止收到的响应体字节数。
    pub fn body_len(&self, id: u32) -> usize {
        self.streams.get(&id).map_or(0, |s| s.resp.body.len())
    }

    pub fn reset_of(&self, id: u32) -> Option<PreviewResetCode> {
        self.streams.get(&id).and_then(|s| s.resp.reset.as_ref()).map(|r| r.code)
    }
}

// ====================================================================== 本机上游服务器

/// 上游收到的一个请求。
#[derive(Debug, Clone, Default)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    /// 请求头，名字小写。
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// 升级握手收到的请求头（每次握手一份，名字小写）。
pub type WsHandshakes = Arc<Mutex<Vec<Vec<(String, String)>>>>;

/// 本机 HTTP + WebSocket 服务器，绑 `127.0.0.1:0`。路径：
///
/// | 路径 | 行为 |
/// |---|---|
/// | `/hello` | `200 text/plain` `hello` |
/// | `/echo` | 把请求体原样回（`content-type: application/octet-stream`），`x-method` / `x-host` 带方法和收到的 `Host` |
/// | `/redirect` | `302`，`Location: http://localhost:<端口>/landing?x=1` |
/// | `/cookies` | `200`，两个 `Set-Cookie` |
/// | `/big?n=N` | `200 application/octet-stream`，N 字节的伪随机内容（`content-length`） |
/// | `/text?n=N` | `200 text/plain`，N 字节的可压缩文本（`content-length`） |
/// | `/chunked` | `200 text/plain`，分块传输，三块 |
/// | `/gz` | `200 text/plain`，上游自己 gzip（`content-encoding: gzip`），内容是 `gzipped by the upstream` 重复 100 次 |
/// | `/slow` | 收到请求之后一直不回 |
/// | `/status?code=N` | 回 N 状态码，空体 |
/// | 带 `Upgrade: websocket` 的请求 | WebSocket：回显（文本加前缀 `echo:`，二进制原样）；文本 `close` 让服务器以 `4000 bye` 关闭；路径 `/reject` 回 `403` |
pub struct TestUpstream {
    pub port: u16,
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// 升级握手收到的请求头（小写名）。
    pub ws_handshakes: WsHandshakes,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn pseudo_random(n: usize) -> Vec<u8> {
    let mut x: u32 = 0x9e37_79b9;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 8) as u8
        })
        .collect()
}

/// `/text?n=N` 的内容。
pub fn compressible_text(n: usize) -> Vec<u8> {
    let unit = b"The quick brown fox jumps over the lazy dog. ";
    unit.iter().copied().cycle().take(n).collect()
}

/// `/big?n=N` 的内容。
pub fn big_body(n: usize) -> Vec<u8> {
    pseudo_random(n)
}

fn query_n(path: &str, key: &str) -> usize {
    path.split_once('?')
        .map(|(_, q)| q)
        .into_iter()
        .flat_map(|q| q.split('&'))
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

impl TestUpstream {
    /// 绑 `127.0.0.1:0` 启动。
    pub async fn start() -> TestUpstream {
        Self::start_on("127.0.0.1:0").await
    }

    /// 绑指定地址启动（例如 `[::1]:0` 测 IPv6 回退）。
    pub async fn start_on(addr: &str) -> TestUpstream {
        let listener = TcpListener::bind(addr).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let ws_handshakes = Arc::new(Mutex::new(Vec::new()));
        let (reqs, wsh) = (requests.clone(), ws_handshakes.clone());
        let task = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let (reqs, wsh) = (reqs.clone(), wsh.clone());
                tokio::spawn(async move { serve_conn(sock, port, reqs, wsh).await });
            }
        });
        TestUpstream { port, requests, ws_handshakes, task }
    }

    pub fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 最近一个路径以 `prefix` 开头的请求。
    pub fn last_request(&self, prefix: &str) -> Option<RecordedRequest> {
        self.requests.lock().unwrap().iter().rev().find(|r| r.path.starts_with(prefix)).cloned()
    }
}

/// 读到 `\r\n\r\n` 为止（不消费 `\r\n\r\n` 之后的字节：`peek` 够用的话用 `peek`，这里直接读，多读的留给 body）。
async fn read_head(sock: &mut TcpStream) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(i + 4);
            buf.truncate(i);
            return Some((buf, rest));
        }
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 256 * 1024 {
            return None;
        }
    }
}

fn parse_head(head: &[u8]) -> Option<RecordedRequest> {
    let text = String::from_utf8_lossy(head).into_owned();
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(RecordedRequest { method, path, headers, body: Vec::new() })
}

async fn read_body(sock: &mut TcpStream, req: &RecordedRequest, mut rest: Vec<u8>) -> Option<Vec<u8>> {
    let chunked = req.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    if chunked {
        let mut body = Vec::new();
        loop {
            // 读一行块大小
            let line = loop {
                if let Some(i) = rest.windows(2).position(|w| w == b"\r\n") {
                    let l = String::from_utf8_lossy(&rest[..i]).into_owned();
                    rest.drain(..i + 2);
                    break l;
                }
                let mut tmp = [0u8; 8192];
                let n = sock.read(&mut tmp).await.ok()?;
                if n == 0 {
                    return None;
                }
                rest.extend_from_slice(&tmp[..n]);
            };
            let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
            while rest.len() < size + 2 {
                let mut tmp = [0u8; 65536];
                let n = sock.read(&mut tmp).await.ok()?;
                if n == 0 {
                    return None;
                }
                rest.extend_from_slice(&tmp[..n]);
            }
            if size == 0 {
                return Some(body);
            }
            body.extend_from_slice(&rest[..size]);
            rest.drain(..size + 2);
        }
    }
    let len: usize = req.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    while rest.len() < len {
        let mut tmp = [0u8; 65536];
        let n = sock.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        rest.extend_from_slice(&tmp[..n]);
    }
    rest.truncate(len);
    Some(rest)
}

fn response(status: u16, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status} X\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

async fn serve_conn(
    mut sock: TcpStream,
    port: u16,
    reqs: Arc<Mutex<Vec<RecordedRequest>>>,
    ws_handshakes: WsHandshakes,
) {
    // WebSocket 升级：先 peek 请求头，判断之后把整个连接交给 tungstenite（它自己读握手）
    let mut peeked = vec![0u8; 8192];
    let mut tries = 0;
    let is_ws = loop {
        let Ok(n) = sock.peek(&mut peeked).await else { return };
        let text = String::from_utf8_lossy(&peeked[..n]).to_ascii_lowercase();
        if text.contains("\r\n\r\n") {
            break text.contains("upgrade: websocket");
        }
        tries += 1;
        if n == 0 || tries > 200 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    if is_ws {
        serve_ws(sock, reqs, ws_handshakes).await;
        return;
    }
    let Some((head, rest)) = read_head(&mut sock).await else { return };
    let Some(mut req) = parse_head(&head) else { return };
    let Some(body) = read_body(&mut sock, &req, rest).await else { return };
    req.body = body;
    reqs.lock().unwrap().push(req.clone());
    let route = req.path.split('?').next().unwrap_or("").to_string();
    let host = req.header("host").unwrap_or("").to_string();
    let out: Vec<u8> = match route.as_str() {
        "/hello" => response(200, &[("content-type", "text/plain".into())], b"hello"),
        "/echo" => response(
            200,
            &[("content-type", "application/octet-stream".into()), ("x-method", req.method.clone()), ("x-host", host)],
            &req.body,
        ),
        "/redirect" => response(302, &[("location", format!("http://localhost:{port}/landing?x=1"))], b""),
        "/cookies" => response(
            200,
            &[
                ("content-type", "text/plain".into()),
                ("set-cookie", "a=1; Path=/; HttpOnly".into()),
                ("set-cookie", "b=2; Path=/".into()),
            ],
            b"ok",
        ),
        "/big" => {
            response(200, &[("content-type", "application/octet-stream".into())], &big_body(query_n(&req.path, "n")))
        }
        "/text" => response(
            200,
            &[("content-type", "text/plain; charset=utf-8".into())],
            &compressible_text(query_n(&req.path, "n")),
        ),
        "/chunked" => {
            let mut out = b"HTTP/1.1 200 X\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
            for part in ["alpha", "beta", "gamma"] {
                out.extend_from_slice(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes());
            }
            out.extend_from_slice(b"0\r\n\r\n");
            out
        }
        "/gz" => {
            use std::io::Write as _;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&b"gzipped by the upstream".repeat(100)).unwrap();
            let gz = enc.finish().unwrap();
            response(200, &[("content-type", "text/plain".into()), ("content-encoding", "gzip".into())], &gz)
        }
        "/slow" => {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            return;
        }
        "/status" => response(query_n(&req.path, "code") as u16, &[], b""),
        _ => response(404, &[("content-type", "text/plain".into())], b"not found"),
    };
    let _ = sock.write_all(&out).await;
    let _ = sock.shutdown().await;
}

async fn serve_ws(sock: TcpStream, reqs: Arc<Mutex<Vec<RecordedRequest>>>, ws_handshakes: WsHandshakes) {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::Message;
    #[allow(clippy::result_large_err)] // 签名由 tungstenite 的 `Callback` 决定
    let cb = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let headers: Vec<(String, String)> = req
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        reqs.lock().unwrap().push(RecordedRequest {
            method: req.method().to_string(),
            path: req.uri().to_string(),
            headers: headers.clone(),
            body: vec![],
        });
        ws_handshakes.lock().unwrap().push(headers);
        if req.uri().path() == "/reject" {
            let mut r = ErrorResponse::new(Some("no websockets here".to_string()));
            *r.status_mut() = http::StatusCode::FORBIDDEN;
            return Err(r);
        }
        // 子协议：客户端要的第一个
        if let Some(p) = req.headers().get("sec-websocket-protocol").and_then(|v| v.to_str().ok()) {
            if let Some(first) = p.split(',').next() {
                resp.headers_mut().insert("sec-websocket-protocol", first.trim().parse().unwrap());
            }
        }
        Ok(resp)
    };
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(sock, cb).await else { return };
    while let Some(Ok(msg)) = ws.next().await {
        match msg {
            Message::Text(t) => {
                if t.as_str() == "close" {
                    let _ = ws.send(Message::Close(Some(CloseFrame { code: 4000.into(), reason: "bye".into() }))).await;
                } else if t.as_str() == "big" {
                    let _ = ws.send(Message::text("x".repeat(300_000))).await;
                } else {
                    let _ = ws.send(Message::text(format!("echo:{}", t.as_str()))).await;
                }
            }
            Message::Binary(b) => {
                let _ = ws.send(Message::Binary(b)).await;
            }
            // tungstenite 读到 Close 会自动排一个回复；要冲出去再关，不然客户端只看到连接断了
            Message::Close(_) => {
                let _ = ws.flush().await;
                break;
            }
            _ => {}
        }
    }
}

/// 找一个现在没人监听的本机端口。
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// `lsof` 能用吗（端口探测相关的测试在没有它的机器上跳过）。
pub fn have_lsof() -> bool {
    std::process::Command::new("lsof")
        .arg("-v")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}
