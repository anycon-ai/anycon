//! 长期在线的多节点客户端 [`Client`]：一个设备身份、**一条** hub 连接，同时连多个节点——每个节点各自一个 Noise 传输
//! （IK，本端是 initiator），各自握手、各自重连。
//!
//! - **请求**：[`Client::request`] 发 JSON-RPC 请求并等回应；节点的通知经 [`Client::subscribe`] 以 [`ClientEvent`] 带节点 id 推出来。
//! - **节点上下线**：hub 的 `peer_online` / `peer_offline` / `undeliverable` 驱动。节点上线（或重新上线）就重新握手，下线就把该节点
//!   的传输丢掉、未完成的请求以离线收场；在线状态由 [`Client::node_status`] 暴露。一个节点的通道出了问题（解不开密、被重置、
//!   被退回的信封已不在保留缓冲里）只重来这一个节点的握手，不影响别的节点。
//! - **hub 重连**：连接断了按退避重连（和 daemon 自己连 hub 的一样：1 秒起、翻倍、上限 60 秒、加抖动；被顶掉、被重启、被吊销各有
//!   自己的等待），重连后对每个仍在线的已配对节点重新握手。
//! - **配对**：[`Client::pair`] 在这条已有的连接上走短码配对（同一设备在 hub 上只留一条连接，另开一条会把长连接顶掉）。还没有
//!   设备 token 的新身份没有长连接，配对用一条临时连接，成功后才开长连接。
//! - **token 轮换**：照 [`super::hubws`] 的规则（30 天一次，第一帧发 `rotate_token`，回执先落盘再换内存里的 token）；长连接每 10 分钟
//!   再看一次是否到期，所以不必等重连。
//! - **关停**：[`Client::shutdown`] 断开 hub 连接并结束后台任务，未完成的请求以离线收场。
//!
//! 一个节点的收发复用 [`super::channel::NodeChannel`]（分片、重组、发送方流控），和单节点的 [`super::link`] 是同一套。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Role};
use anycon_protocol::{AuthFailReason, ClientInfo, Control, Envelope, Kind, RpcId, RpcMessage, RpcNotification};
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::channel::NodeChannel;
use super::error::{ClientError, ErrorClass, Result};
use super::hubws::{close_reason, ConnectFail, HubWs};
use super::pairing::{
    derive_secrets, finish_pairing, join_handshake, pair_state, parse_code_arg, rate_limit_wait, ACK_TIMEOUT,
};
use super::state::{write_token_files, HubState, NodeEntry};
use crate::flow::{RatePause, RETAIN_MAX_AGE};

/// 事件广播的容量；订阅者落后太多会收到 `Lagged`（丢了一些事件，状态以 [`Client::node_status`] 为准）。
const EVENT_CAPACITY: usize = 1024;
/// 后台的例行检查（释放旧的保留信封、清掉没人等的请求）的间隔。
const TICK: Duration = Duration::from_secs(30);
/// 长连接上多久看一次 token 是否该轮换了。
const ROTATION_CHECK: Duration = Duration::from_secs(600);
/// 发了 `rotate_token` 之后，这么久没有回执才会再发（hub 对同一设备 1 分钟内多余的请求不回复）。
const ROTATION_RETRY: Duration = Duration::from_secs(90);
/// 对一个在线节点握手，最多尝试几次（每次等 [`ClientOptions::handshake_timeout`]）。
const HANDSHAKE_ATTEMPTS: u32 = 3;

/// 配置。
#[derive(Debug, Clone)]
pub struct ClientOptions {
    /// 认证和配对时自报的设备信息（名字、种类）。
    pub client: ClientInfo,
    /// hub 重连退避的起点（之后每次翻倍）。
    pub reconnect_initial: Duration,
    /// hub 重连退避的上限。
    pub reconnect_max: Duration,
    /// 对一个节点的 IK 握手等多久。
    pub handshake_timeout: Duration,
    /// 配对时等对端 `pair.ack` 多久。
    pub ack_timeout: Duration,
}

impl ClientOptions {
    pub fn new(client: ClientInfo) -> ClientOptions {
        ClientOptions {
            client,
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(6),
            ack_timeout: ACK_TIMEOUT,
        }
    }
}

/// 一个已配对节点现在的连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeStatus {
    /// 不在线、没连上 hub、或者握手没成功（节点不认这个设备）：请求立刻失败。
    Offline,
    /// 在握手：请求先排着，握手完成后发出。
    Connecting,
    /// 加密通道已建立。
    Ready,
}

/// [`Client::subscribe`] 推出来的事件。
#[derive(Debug, Clone)]
pub enum ClientEvent {
    /// hub 连接认证成功。
    HubConnected,
    /// hub 连接断了（所有节点都不可用，退避之后自动重连）。
    HubDisconnected,
    /// 和这个节点的加密通道建立好了（每次重新握手成功都发一次）。
    NodeReady { node_id: String },
    /// 这个节点的通道没了（下线、被重置、握手失败）。
    NodeOffline { node_id: String },
    /// 节点发来的通知。
    Notification { node_id: String, notification: RpcNotification },
}

type Reply = oneshot::Sender<Result<Value>>;

enum Cmd {
    Request { node_id: String, method: String, params: Value, reply: Reply },
    Pair { code: String, expect_node: Option<String>, reply: oneshot::Sender<Result<NodeEntry>> },
    RemoveNode { node_id: String, reply: oneshot::Sender<Result<bool>> },
    Shutdown { done: oneshot::Sender<()> },
}

#[derive(Default)]
struct SharedInner {
    hub_connected: bool,
    statuses: HashMap<String, NodeStatus>,
    nodes: Vec<NodeEntry>,
    last_error: Option<String>,
}

struct Inner {
    cmd: mpsc::UnboundedSender<Cmd>,
    events: broadcast::Sender<ClientEvent>,
    shared: Arc<Mutex<SharedInner>>,
}

/// 长期在线的多节点客户端。`Clone` 出来的句柄共用同一个后台任务；最后一个句柄丢掉、或者调 [`Client::shutdown`]，
/// 后台任务就结束并断开 hub 连接。
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl Client {
    /// 以 `state`（设备身份和已配对节点）启动。立刻返回：连 hub、握手都在后台做，没有 token 的新身份不连 hub（等 [`Client::pair`]）。
    pub fn start(state: HubState, opts: ClientOptions) -> Result<Client> {
        state.check()?;
        let (cmd, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let shared = Arc::new(Mutex::new(SharedInner {
            nodes: state.nodes.clone(),
            statuses: state.nodes.iter().map(|n| (n.id.clone(), NodeStatus::Offline)).collect(),
            ..Default::default()
        }));
        let actor =
            Actor { state, opts, rx, events: events.clone(), shared: shared.clone(), shutdown_done: Vec::new() };
        tokio::spawn(actor.run());
        Ok(Client { inner: Arc::new(Inner { cmd, events, shared }) })
    }

    /// 订阅事件（每个订阅者各收一份，只收订阅之后的）。
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.inner.events.subscribe()
    }

    /// 对节点发一个请求并等回应，最多等 `timeout`。节点不在线立刻返回离线错误；正在握手就排队等握手完成。
    /// 节点回了错误响应时返回的错误带着 [`ClientError::rpc`]。
    pub async fn request(
        &self,
        node_id: &str,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<Value> {
        let params = serde_json::to_value(params)?;
        let (reply, rx) = oneshot::channel();
        let cmd = Cmd::Request { node_id: node_id.to_string(), method: method.to_string(), params, reply };
        self.inner.cmd.send(cmd).map_err(|_| stopped())?;
        match tokio::time::timeout(timeout, rx).await {
            Err(_) => Err(ClientError::new(
                ErrorClass::Offline,
                "timeout",
                format!("no answer to {method} within {} s; is the node busy or unreachable?", timeout.as_secs()),
            )),
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(stopped()),
        }
    }

    /// 作为加入方用短码配对一个新节点，成功后立刻开始和它握手。`expect_node` 给了就要求配对到的节点是它，对不上就把这次
    /// 配对从本地丢掉并报 `node_mismatch`（对端那边的设备记录要在对端吊销）。
    pub async fn pair(&self, code: &str, expect_node: Option<&str>) -> Result<NodeEntry> {
        let (reply, rx) = oneshot::channel();
        let cmd = Cmd::Pair { code: code.to_string(), expect_node: expect_node.map(str::to_string), reply };
        self.inner.cmd.send(cmd).map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// 忘掉一个已配对的节点（关掉传输、从本地状态删掉）；本来就没有返回 `false`。对端的设备记录不受影响。
    pub async fn remove_node(&self, node_id: &str) -> Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.inner.cmd.send(Cmd::RemoveNode { node_id: node_id.to_string(), reply }).map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// 已配对的节点（本地状态）。
    pub fn nodes(&self) -> Vec<NodeEntry> {
        self.inner.shared.lock().unwrap().nodes.clone()
    }

    /// 一个节点现在的状态；没配对过的当作 [`NodeStatus::Offline`]。
    pub fn node_status(&self, node_id: &str) -> NodeStatus {
        self.inner.shared.lock().unwrap().statuses.get(node_id).copied().unwrap_or(NodeStatus::Offline)
    }

    /// hub 连接现在是不是认证好了的。
    pub fn hub_connected(&self) -> bool {
        self.inner.shared.lock().unwrap().hub_connected
    }

    /// 最近一次连不上 / 被拒绝 / 被断开的原因（连上之后清掉）。
    pub fn last_error(&self) -> Option<String> {
        self.inner.shared.lock().unwrap().last_error.clone()
    }

    /// 断开 hub 连接、结束后台任务；返回时已经结束。之后所有调用都报离线错误。
    pub async fn shutdown(&self) {
        let (done, rx) = oneshot::channel();
        if self.inner.cmd.send(Cmd::Shutdown { done }).is_ok() {
            let _ = rx.await;
        }
    }
}

fn stopped() -> ClientError {
    ClientError::offline("the client was shut down")
}

// ---------------------------------------------------------------- 后台任务

struct Queued {
    method: String,
    params: Value,
    reply: Reply,
}

struct Handshaking {
    /// 握手进行中；`None` = 在等下一次重试（`deadline` 是重试的时刻）。
    hs: Option<Handshake>,
    deadline: Instant,
    attempts: u32,
    waiting: Vec<Queued>,
}

struct Ready {
    chan: NodeChannel,
    pending: HashMap<u64, Reply>,
    next: u64,
}

enum Session {
    Handshaking(Box<Handshaking>),
    Ready(Box<Ready>),
}

/// 一条认证好的 hub 连接和它上面各节点的会话。
struct Live {
    h: HubWs,
    sessions: HashMap<String, Session>,
    rate: RatePause,
    /// 发出 `rotate_token`、还没收到回执的时刻。
    rotation_asked: Option<Instant>,
    next_rotation_check: Instant,
    next_tick: Instant,
    connected_at: Instant,
}

enum Outcome {
    Shutdown,
    Lost { reason: String, code: Option<u16> },
}

struct Actor {
    state: HubState,
    opts: ClientOptions,
    rx: mpsc::UnboundedReceiver<Cmd>,
    events: broadcast::Sender<ClientEvent>,
    shared: Arc<Mutex<SharedInner>>,
    /// 等关停完成的调用方：收尾（断开 hub 连接、清理状态）做完才通知它们。
    shutdown_done: Vec<oneshot::Sender<()>>,
}

fn jitter(d: Duration) -> Duration {
    d.mul_f64(rand::thread_rng().gen_range(0.75..1.25))
}

fn offline_hub() -> ClientError {
    ClientError::offline("not connected to the hub right now; it is reconnecting")
}

impl Actor {
    fn emit(&self, e: ClientEvent) {
        let _ = self.events.send(e);
    }

    fn set_status(&self, node_id: &str, st: NodeStatus) {
        self.shared.lock().unwrap().statuses.insert(node_id.to_string(), st);
    }

    fn set_error(&self, e: Option<String>) {
        self.shared.lock().unwrap().last_error = e;
    }

    fn sync_nodes(&self) {
        let mut g = self.shared.lock().unwrap();
        g.nodes = self.state.nodes.clone();
        let ids: Vec<String> = self.state.nodes.iter().map(|n| n.id.clone()).collect();
        g.statuses.retain(|k, _| ids.contains(k));
        for id in ids {
            g.statuses.entry(id).or_insert(NodeStatus::Offline);
        }
    }

    fn can_connect(&self) -> bool {
        self.state.token.is_some() && self.state.keypair().is_some()
    }

    async fn run(mut self) {
        self.run_loop().await;
        for done in self.shutdown_done.drain(..) {
            let _ = done.send(());
        }
    }

    async fn run_loop(&mut self) {
        let mut backoff = self.opts.reconnect_initial;
        loop {
            // 没有 token 的新身份：没有长连接，只处理命令（配对用临时连接）
            if !self.can_connect() {
                let Some(cmd) = self.rx.recv().await else { return };
                if self.offline_cmd(cmd).await {
                    return;
                }
                continue;
            }
            let (live, deferred) = match self.connect().await {
                Connected::Shutdown => return,
                Connected::Up(live, deferred) => (*live, deferred),
                Connected::Failed(delay) => {
                    let delay = delay.unwrap_or_else(|| {
                        let d = jitter(backoff);
                        backoff = (backoff * 2).min(self.opts.reconnect_max);
                        d
                    });
                    if self.wait(delay).await {
                        return;
                    }
                    continue;
                }
            };
            let started = live.connected_at;
            match self.serve(live, deferred).await {
                Outcome::Shutdown => return,
                Outcome::Lost { reason, code } => {
                    tracing::info!(%reason, ?code, "the hub connection of the client ended");
                    self.set_error(Some(reason));
                    if started.elapsed() > Duration::from_secs(30) {
                        backoff = self.opts.reconnect_initial;
                    }
                    let delay = match code {
                        Some(anycon_protocol::close_code::RESTARTING) => {
                            Duration::from_secs_f64(rand::thread_rng().gen_range(1.0..3.0))
                        }
                        // 同一设备在别处连上了：别互相踢，退避久一点
                        Some(anycon_protocol::close_code::SUPERSEDED) => {
                            let d = jitter(backoff.max(Duration::from_secs(30)));
                            backoff = (backoff * 2).min(self.opts.reconnect_max);
                            d
                        }
                        Some(anycon_protocol::close_code::REVOKED) => Duration::from_secs(3600),
                        _ => {
                            let d = jitter(backoff);
                            backoff = (backoff * 2).min(self.opts.reconnect_max);
                            d
                        }
                    };
                    if self.wait(delay).await {
                        return;
                    }
                }
            }
        }
    }

    /// 没有 hub 连接时等 `delay`，期间照常处理命令。返回 `true` = 该退出了。
    async fn wait(&mut self, delay: Duration) -> bool {
        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else { return true };
                    let paired = matches!(cmd, Cmd::Pair { .. });
                    if self.offline_cmd(cmd).await {
                        return true;
                    }
                    // 配对成功换了身份：不用再等，马上重连
                    if paired && self.can_connect() && !self.shared.lock().unwrap().hub_connected {
                        return false;
                    }
                }
            }
        }
    }

    /// 没有长连接时处理一条命令。返回 `true` = 收到了关停。
    async fn offline_cmd(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Request { node_id, reply, .. } => {
                let e = if self.state.nodes.iter().any(|n| n.id == node_id) {
                    offline_hub()
                } else {
                    not_paired_with(&node_id)
                };
                let _ = reply.send(Err(e));
            }
            Cmd::Pair { code, expect_node, reply } => {
                let r = self.pair_standalone(&code, expect_node.as_deref()).await;
                let _ = reply.send(r);
                // 等限速的时候收到了关停
                if !self.shutdown_done.is_empty() {
                    return true;
                }
            }
            Cmd::RemoveNode { node_id, reply } => {
                let _ = reply.send(self.remove_node(&node_id));
            }
            Cmd::Shutdown { done } => {
                self.shutdown_done.push(done);
                return true;
            }
        }
        false
    }

    fn remove_node(&mut self, node_id: &str) -> Result<bool> {
        let before = self.state.nodes.len();
        self.state.nodes.retain(|n| n.id != node_id);
        let removed = self.state.nodes.len() != before;
        if removed {
            self.state.save()?;
            self.sync_nodes();
        }
        Ok(removed)
    }

    /// 没有长连接：用一条临时连接配对（带着已有的 token，所以只在没连着 hub 的时候用）。
    /// hub 限速（429）时按 `Retry-After` 等一等再试，总共最多 [`super::pairing::RATE_LIMIT_PATIENCE`]；
    /// 等待期间照常处理别的命令，收到关停就放弃。
    async fn pair_standalone(&mut self, code: &str, expect: Option<&str>) -> Result<NodeEntry> {
        let (mut waited, mut attempt) = (Duration::ZERO, 0u32);
        loop {
            match pair_state(&mut self.state, self.opts.client.clone(), code, self.opts.ack_timeout).await {
                Ok(entry) => return self.after_pair(entry, expect),
                Err(e) => {
                    let Some(d) = rate_limit_wait(&e, waited, attempt) else { return Err(e) };
                    tracing::info!(wait_ms = d.as_millis() as u64, "the hub limits new connections; pairing waits");
                    if self.pause(d).await {
                        return Err(stopped());
                    }
                    waited += d;
                    attempt += 1;
                }
            }
        }
    }

    /// 没有长连接时睡 `d`，期间回应别的命令（和 [`Self::wait`] 一样，但配对不能嵌套进来）。返回 `true` = 收到了关停。
    async fn pause(&mut self, d: Duration) -> bool {
        let sleep = tokio::time::sleep(d);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return false,
                cmd = self.rx.recv() => match cmd {
                    None => return true,
                    Some(Cmd::Shutdown { done }) => {
                        self.shutdown_done.push(done);
                        return true;
                    }
                    Some(Cmd::Pair { reply, .. }) => {
                        let _ = reply.send(Err(ClientError::general("another pairing is already in progress")));
                    }
                    Some(Cmd::Request { reply, .. }) => {
                        let _ = reply.send(Err(offline_hub()));
                    }
                    Some(Cmd::RemoveNode { node_id, reply }) => {
                        let _ = reply.send(self.remove_node(&node_id));
                    }
                },
            }
        }
    }

    /// 配对完成后的核对和登记。
    fn after_pair(&mut self, entry: NodeEntry, expect: Option<&str>) -> Result<NodeEntry> {
        if let Some(want) = expect.filter(|w| *w != entry.id) {
            // 丢掉这次配对：本地不留这个节点
            self.state.nodes.retain(|n| n.id != entry.id);
            self.state.save()?;
            self.sync_nodes();
            return Err(ClientError::new(
                ErrorClass::NotPaired,
                "node_mismatch",
                format!(
                    "the code paired with node {} but node {want} was expected; the pairing was discarded here",
                    entry.id
                ),
            ));
        }
        self.sync_nodes();
        Ok(entry)
    }

    // ------------------------------------------------------------ 连接

    async fn connect(&mut self) -> Connected {
        let url = self.state.hub_url.clone();
        let token = self.state.token.clone();
        let info = self.opts.client.clone();
        let mut deferred: Vec<Cmd> = Vec::new();
        let connecting = HubWs::connect_raw(&url, token, info, None);
        tokio::pin!(connecting);
        let result = loop {
            tokio::select! {
                r = &mut connecting => break r,
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else { return Connected::Shutdown };
                    match cmd {
                        // 配对要用这条连接：等它连好再做
                        Cmd::Pair { .. } => deferred.push(cmd),
                        other => {
                            if self.offline_cmd(other).await {
                                return Connected::Shutdown;
                            }
                        }
                    }
                }
            }
        };
        match result {
            Ok(h) => {
                let now = Instant::now();
                self.set_error(None);
                self.shared.lock().unwrap().hub_connected = true;
                self.emit(ClientEvent::HubConnected);
                let live = Live {
                    h,
                    sessions: HashMap::new(),
                    rate: RatePause::default(),
                    rotation_asked: None,
                    next_rotation_check: now + ROTATION_CHECK,
                    next_tick: now + TICK,
                    connected_at: now,
                };
                Connected::Up(Box::new(live), deferred)
            }
            Err(fail) => {
                let delay = match &fail {
                    ConnectFail::Auth(AuthFailReason::Quota) => Some(Duration::from_secs(60)),
                    ConnectFail::Auth(AuthFailReason::Revoked) => Some(Duration::from_secs(3600)),
                    ConnectFail::Auth(_) => Some(Duration::from_secs(600)),
                    // hub 限速：按它给的 `Retry-After` 再连，不走指数退避（没给就退避）
                    ConnectFail::Other(e) if e.code == super::error::RATE_LIMITED_CODE => {
                        e.retry_after().map(|d| d.clamp(Duration::from_secs(1), Duration::from_secs(120)))
                    }
                    ConnectFail::Other(_) => None,
                };
                let e = fail.into_error();
                tracing::warn!(error = %e.message, "the client cannot connect to the hub");
                self.set_error(Some(e.message));
                // 连不上期间排着的配对命令：没有长连接，走临时连接
                for cmd in deferred {
                    if self.offline_cmd(cmd).await {
                        return Connected::Shutdown;
                    }
                }
                Connected::Failed(delay)
            }
        }
    }

    /// 在一条认证好的连接上服务到它结束。
    async fn serve(&mut self, mut live: Live, deferred: Vec<Cmd>) -> Outcome {
        let outcome = self.serve_inner(&mut live, deferred).await;
        // 连接结束：所有节点不可用，未完成的请求以离线收场
        let lost = match &outcome {
            Outcome::Shutdown => "the client was shut down".to_string(),
            Outcome::Lost { reason, .. } => format!("the connection to the hub was lost ({reason})"),
        };
        let ids: Vec<String> = live.sessions.keys().cloned().collect();
        for id in ids {
            self.drop_session(&mut live, &id, &lost);
        }
        self.shared.lock().unwrap().hub_connected = false;
        for n in self.state.nodes.iter() {
            self.set_status(&n.id, NodeStatus::Offline);
        }
        if matches!(outcome, Outcome::Shutdown) {
            let _ = live.h.ws.close(None).await;
        } else {
            self.emit(ClientEvent::HubDisconnected);
        }
        outcome
    }

    async fn serve_inner(&mut self, live: &mut Live, deferred: Vec<Cmd>) -> Outcome {
        // 认证回复里已经在线的节点：握手
        let online = std::mem::take(&mut live.h.peers_online);
        if let Err(r) = self.maybe_rotate(live).await {
            return Outcome::Lost { reason: r, code: live.h.close_code };
        }
        let paired: Vec<NodeEntry> = self.state.nodes.clone();
        for n in paired.iter().filter(|n| online.contains(&n.id)) {
            if let Err(r) = self.begin_handshake(live, n, 1, Vec::new()).await {
                return Outcome::Lost { reason: r, code: None };
            }
        }
        for cmd in deferred {
            match self.cmd_live(live, cmd).await {
                Ok(true) => return Outcome::Shutdown,
                Ok(false) => {}
                Err(r) => return Outcome::Lost { reason: r, code: live.h.close_code },
            }
        }
        loop {
            let wake = self.next_wake(live);
            tokio::select! {
                frame = live.h.ws.next() => {
                    let Some(frame) = frame else {
                        return Outcome::Lost { reason: "the hub closed the connection".into(), code: None };
                    };
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => {
                            return Outcome::Lost { reason: format!("the hub connection failed: {e}"), code: None }
                        }
                    };
                    match frame {
                        Message::Binary(b) => {
                            let Ok(env) = Envelope::decode(&b) else { continue };
                            if let Err(r) = self.on_envelope(live, env).await {
                                return Outcome::Lost { reason: r, code: None };
                            }
                        }
                        Message::Ping(_) => {
                            let _ = live.h.ws.flush().await;
                        }
                        Message::Close(f) => {
                            let code = f.map(|f| u16::from(f.code));
                            return Outcome::Lost { reason: close_reason(code), code };
                        }
                        _ => {}
                    }
                }
                cmd = self.rx.recv() => {
                    // 所有句柄都丢了：和关停一样收场
                    let Some(cmd) = cmd else { return Outcome::Shutdown };
                    match self.cmd_live(live, cmd).await {
                        Ok(true) => return Outcome::Shutdown,
                        Ok(false) => {}
                        Err(r) => return Outcome::Lost { reason: r, code: live.h.close_code },
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                    if let Err(r) = self.on_timer(live).await {
                        return Outcome::Lost { reason: r, code: None };
                    }
                }
            }
        }
    }

    /// 下一次该醒的时刻：握手超时、被退回信封的重发、例行检查、token 轮换检查，取最早的。
    fn next_wake(&self, live: &Live) -> Instant {
        let now = Instant::now();
        let mut at = live.next_tick.min(live.next_rotation_check);
        for s in live.sessions.values() {
            let t = match s {
                Session::Handshaking(h) => Some(h.deadline),
                Session::Ready(r) => r.chan.wake_at(&live.rate, now),
            };
            if let Some(t) = t {
                at = at.min(t);
            }
        }
        at
    }

    async fn on_timer(&mut self, live: &mut Live) -> std::result::Result<(), String> {
        let now = Instant::now();
        // 握手超时
        let expired: Vec<String> = live
            .sessions
            .iter()
            .filter_map(|(id, s)| match s {
                Session::Handshaking(h) if h.deadline <= now => Some(id.clone()),
                _ => None,
            })
            .collect();
        for id in expired {
            let Some(Session::Handshaking(h)) = live.sessions.remove(&id) else { continue };
            let Some(node) = self.state.nodes.iter().find(|n| n.id == id).cloned() else { continue };
            if h.hs.is_none() {
                // 退避结束：重试
                self.begin_handshake(live, &node, h.attempts, h.waiting).await?;
            } else {
                tracing::debug!(node = %id, attempts = h.attempts, "client: handshake timed out");
                self.handshake_failed(live, &node, h).await?;
            }
        }
        // 被退回的信封到点重发
        let ids: Vec<String> = live.sessions.keys().cloned().collect();
        for id in ids {
            let Live { h, sessions, rate, .. } = live;
            let Some(Session::Ready(r)) = sessions.get_mut(&id) else { continue };
            match r.chan.take_resend(&h.device_id, rate) {
                Ok(frames) => send_frames(h, frames).await?,
                Err(e) => {
                    self.restart_session(live, &id, &e).await?;
                }
            }
        }
        if now >= live.next_tick {
            live.next_tick = now + TICK;
            for s in live.sessions.values_mut() {
                if let Session::Ready(r) = s {
                    r.chan.flow.expire(now, RETAIN_MAX_AGE);
                    r.pending.retain(|_, reply| !reply.is_closed());
                }
            }
        }
        if now >= live.next_rotation_check {
            live.next_rotation_check = now + ROTATION_CHECK;
            self.maybe_rotate(live).await?;
        }
        Ok(())
    }

    // ------------------------------------------------------------ token 轮换

    async fn maybe_rotate(&mut self, live: &mut Live) -> std::result::Result<(), String> {
        let due = crate::identity::token_due(self.state.token_issued_at_ms, crate::util::now_ms());
        let asked_recently = live.rotation_asked.is_some_and(|t| t.elapsed() < ROTATION_RETRY);
        if !due || asked_recently {
            return Ok(());
        }
        live.h.send_control(&Control::RotateToken {}).await.map_err(|e| e.message)?;
        live.rotation_asked = Some(Instant::now());
        tracing::debug!("the device token is due for rotation; asked the hub for a new one");
        Ok(())
    }

    /// `token_rotated` 回执：先落盘（token、签发时间），再换内存里的。只认我们要过的。
    fn on_token_rotated(&mut self, live: &mut Live, token: &str) {
        if live.rotation_asked.take().is_none() {
            return;
        }
        match write_token_files(&self.state.dir, token, crate::util::now_ms()) {
            Ok(()) => tracing::debug!("device token rotated"),
            Err(e) => super::notice(&format!(
                "warning: the hub issued a new device token but it could not be saved ({e}); the previous token \
                 stops working within a minute, so this device will have to pair again"
            )),
        }
        self.state.adopt_rotated_token(token);
    }

    // ------------------------------------------------------------ 命令

    /// 在长连接上处理一条命令。`Ok(true)` = 关停；`Err` = hub 连接坏了。
    async fn cmd_live(&mut self, live: &mut Live, cmd: Cmd) -> std::result::Result<bool, String> {
        match cmd {
            Cmd::Request { node_id, method, params, reply } => {
                if !self.state.nodes.iter().any(|n| n.id == node_id) {
                    let _ = reply.send(Err(not_paired_with(&node_id)));
                    return Ok(false);
                }
                match live.sessions.get_mut(&node_id) {
                    None => {
                        let _ = reply.send(Err(ClientError::offline("the node is offline")));
                    }
                    // 已经放弃过前几次、在退避重试：不让请求干等
                    Some(Session::Handshaking(h)) if h.attempts > HANDSHAKE_ATTEMPTS => {
                        let _ = reply.send(Err(ClientError::offline(
                            "the node is online but is not answering the handshake; still retrying",
                        )));
                    }
                    Some(Session::Handshaking(h)) => h.waiting.push(Queued { method, params, reply }),
                    Some(Session::Ready(_)) => {
                        self.send_request(live, &node_id, Queued { method, params, reply }).await?
                    }
                }
            }
            Cmd::Pair { code, expect_node, reply } => {
                let r = self.pair_live(live, &code, expect_node.as_deref()).await;
                let _ = reply.send(r);
                // 配对期间别的信封先收进了暂存区
                self.drain_stash(live).await?;
            }
            Cmd::RemoveNode { node_id, reply } => {
                self.drop_session(live, &node_id, "this node was removed from the client");
                let _ = reply.send(self.remove_node(&node_id));
            }
            Cmd::Shutdown { done } => {
                self.shutdown_done.push(done);
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn pair_live(&mut self, live: &mut Live, code: &str, expect: Option<&str>) -> Result<NodeEntry> {
        let code = parse_code_arg(code)?;
        let (psk, match_id) = derive_secrets(&code).await?;
        let kp = self.state.keypair().ok_or_else(|| ClientError::not_paired("this device has no key for the hub"))?;
        let used_token = self.state.token.clone();
        let (mut transport, node_pub) = join_handshake(&mut live.h, &kp, &psk, match_id, &self.opts.client).await?;
        let entry =
            finish_pairing(&mut self.state, &mut live.h, &mut transport, node_pub, self.opts.ack_timeout).await?;
        let entry = self.after_pair(entry, expect)?;
        if self.state.token != used_token {
            // 不该发生（长连接上的设备已经有身份）：身份变了，让连接重来
            live.h.close_code = None;
            let _ = live.h.ws.close(None).await;
            return Ok(entry);
        }
        // 刚配好的节点一定在线：直接握手（hub 的 `peer_online` 之后再来也无妨，握手中的会被忽略）
        if let Err(r) = self.begin_handshake(live, &entry, 1, Vec::new()).await {
            tracing::debug!(reason = %r, "could not start the handshake after pairing");
        }
        Ok(entry)
    }

    /// 配对 / 等待期间被暂存的 control 和信封，按到达顺序补处理。
    async fn drain_stash(&mut self, live: &mut Live) -> std::result::Result<(), String> {
        loop {
            let controls = std::mem::take(&mut live.h.controls);
            let mut envs: VecDeque<Envelope> = std::mem::take(&mut live.h.bystanders);
            let mut data = std::mem::take(&mut live.h.data);
            envs.append(&mut data);
            if controls.is_empty() && envs.is_empty() {
                return Ok(());
            }
            for c in controls {
                self.on_control(live, c).await?;
            }
            for env in envs {
                self.on_envelope(live, env).await?;
            }
        }
    }

    // ------------------------------------------------------------ 信封

    async fn on_envelope(&mut self, live: &mut Live, env: Envelope) -> std::result::Result<(), String> {
        match env.kind {
            Kind::Control => {
                if let Ok(c) = env.control_body() {
                    self.on_control(live, c).await?;
                }
            }
            Kind::Handshake => self.on_handshake(live, env).await?,
            Kind::Data => self.on_data(live, env).await?,
            _ => {}
        }
        Ok(())
    }

    async fn on_control(&mut self, live: &mut Live, c: Control) -> std::result::Result<(), String> {
        match c {
            Control::PeerOnline { device } => {
                tracing::debug!(node = %device, "client: peer_online");
                let Some(node) = self.state.nodes.iter().find(|n| n.id == device).cloned() else { return Ok(()) };
                match live.sessions.get(&device) {
                    // 已经在握手：不重复
                    Some(Session::Handshaking(_)) => {}
                    // 通道还在、节点却重新上线了：它那边的传输已经没了，换新的
                    Some(Session::Ready(_)) => {
                        self.drop_session(live, &device, "the node reconnected; the channel is being re-established");
                        self.begin_handshake(live, &node, 1, Vec::new()).await?;
                    }
                    None => self.begin_handshake(live, &node, 1, Vec::new()).await?,
                }
            }
            Control::PeerOffline { device } => {
                tracing::debug!(node = %device, "client: peer_offline");
                self.drop_session(live, &device, "the node went offline");
            }
            Control::Undeliverable { to, .. } => {
                self.drop_session(live, &to, "the node is offline");
            }
            Control::Backpressure { to, seq } => {
                let Live { sessions, rate, .. } = live;
                let failed = match sessions.get_mut(&to) {
                    Some(Session::Ready(r)) => r.chan.on_backpressure(seq, rate).err(),
                    _ => None,
                };
                if let Some(e) = failed {
                    self.restart_session(live, &to, &e).await?;
                }
            }
            Control::RateLimited { retry_after_ms, to } => {
                let d = Duration::from_millis(retry_after_ms);
                match to {
                    None => live.rate.limited(Instant::now(), d),
                    Some(to) => {
                        if let Some(Session::Ready(r)) = live.sessions.get_mut(&to) {
                            r.chan.flow.pause_for(Instant::now(), d);
                        }
                    }
                }
            }
            // hub 丢了我们的某一封（不是退回）：那个节点的加密通道少了一条，只能重来
            Control::Dropped { seq, reason } => {
                let owner = live.sessions.iter().find_map(|(id, s)| match s {
                    Session::Ready(r) if r.chan.flow.contains(seq) => Some(id.clone()),
                    _ => None,
                });
                if let Some(id) = owner {
                    self.restart_session(live, &id, &format!("the hub dropped one of our messages ({reason:?})"))
                        .await?;
                }
            }
            Control::TokenRotated { device_token } => self.on_token_rotated(live, &device_token),
            _ => {}
        }
        Ok(())
    }

    async fn on_handshake(&mut self, live: &mut Live, env: Envelope) -> std::result::Result<(), String> {
        let id = env.from.clone();
        // 只有真在握手的会话才收这条回复。迟到的、对不上的回复（同一设备身份上一条连接的握手，hub 按设备 id
        // 转给了现在这条）不能把已经建好的通道拆掉。
        if !matches!(live.sessions.get(&id), Some(Session::Handshaking(h)) if h.hs.is_some()) {
            return Ok(());
        }
        let Some(Session::Handshaking(mut h)) = live.sessions.remove(&id) else { return Ok(()) };
        tracing::debug!(node = %id, "client: handshake reply");
        let mut hs = h.hs.take().expect("checked above");
        if let Err(e) = hs.read(&env.body) {
            // 对不上的回复多半是上面说的迟到回复，不是节点拒绝：重新握手（真被拒绝的节点根本不回，走超时）
            tracing::debug!(node = %id, error = %e, "client: a handshake reply did not match; retrying");
            if let Some(node) = self.state.nodes.iter().find(|n| n.id == id).cloned() {
                self.handshake_failed(live, &node, h).await?;
            }
            return Ok(());
        }
        let transport = match hs.into_transport() {
            Ok(t) => t,
            Err(e) => {
                let err = ClientError::general(e.to_string());
                for q in h.waiting {
                    let _ = q.reply.send(Err(err.clone()));
                }
                self.set_status(&id, NodeStatus::Offline);
                return Ok(());
            }
        };
        live.sessions.insert(
            id.clone(),
            Session::Ready(Box::new(Ready {
                chan: NodeChannel::new(id.clone(), transport),
                pending: HashMap::new(),
                next: 0,
            })),
        );
        self.set_status(&id, NodeStatus::Ready);
        self.emit(ClientEvent::NodeReady { node_id: id.clone() });
        for q in h.waiting {
            self.send_request(live, &id, q).await?;
        }
        Ok(())
    }

    async fn on_data(&mut self, live: &mut Live, env: Envelope) -> std::result::Result<(), String> {
        let id = env.from.clone();
        let Some(Session::Ready(r)) = live.sessions.get_mut(&id) else { return Ok(()) };
        match r.chan.on_data(&env) {
            Err(e) => self.restart_session(live, &id, &e).await?,
            Ok(Some(RpcMessage::Response(resp))) => {
                if let RpcId::Num(n) = resp.id {
                    if let Some(tx) = r.pending.remove(&n) {
                        let _ = tx.send(match resp.error {
                            Some(e) => Err(ClientError::from_rpc(e)),
                            None => Ok(resp.result.unwrap_or(Value::Null)),
                        });
                    }
                }
            }
            Ok(Some(RpcMessage::Notification(n))) => {
                self.emit(ClientEvent::Notification { node_id: id, notification: n });
            }
            Ok(_) => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ 节点会话

    /// 开始（或重新开始）对一个节点的 IK 握手。`waiting`：握手完成后要发的请求。
    async fn begin_handshake(
        &mut self,
        live: &mut Live,
        node: &NodeEntry,
        attempts: u32,
        waiting: Vec<Queued>,
    ) -> std::result::Result<(), String> {
        let fail = |this: &Self, waiting: Vec<Queued>, msg: &str| {
            let e = ClientError::general(msg.to_string());
            for q in waiting {
                let _ = q.reply.send(Err(e.clone()));
            }
            this.set_status(&node.id, NodeStatus::Offline);
        };
        let node_pub: Option<[u8; 32]> =
            hex::decode(&node.pubkey).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok());
        let (Some(node_pub), Some(kp)) = (node_pub, self.state.keypair()) else {
            fail(self, waiting, "the saved key of this node is unusable");
            return Ok(());
        };
        let started = Handshake::reconnect(Role::Initiator, &kp, Some(&node_pub)).and_then(|mut hs| {
            let m1 = hs.write(&[])?;
            Ok((hs, m1))
        });
        let (hs, m1) = match started {
            Ok(x) => x,
            Err(e) => {
                fail(self, waiting, &e.to_string());
                return Ok(());
            }
        };
        tracing::debug!(node = %node.id, attempts, "client: sending the IK handshake");
        live.h.send_env(&node.id, Kind::Handshake, m1).await.map_err(|e| e.message)?;
        live.sessions.insert(
            node.id.clone(),
            Session::Handshaking(Box::new(Handshaking {
                hs: Some(hs),
                deadline: Instant::now() + self.opts.handshake_timeout,
                attempts,
                waiting,
            })),
        );
        self.set_status(&node.id, NodeStatus::Connecting);
        Ok(())
    }

    /// 一次握手没成（超时，或回复对不上）。前 [`HANDSHAKE_ATTEMPTS`] 次立刻重来；用完之后排着的请求以错误收场，
    /// 但只要节点还在线就一直重试（间隔 1 秒起翻倍、上限 30 秒），不等下一次 `peer_online`——节点可能只是
    /// 暂时没处理握手（忙、还在清理上一条连接的会话），而它不会再发 `peer_online`。
    async fn handshake_failed(
        &mut self,
        live: &mut Live,
        node: &NodeEntry,
        h: Box<Handshaking>,
    ) -> std::result::Result<(), String> {
        let done = h.attempts;
        let waiting = if done >= HANDSHAKE_ATTEMPTS {
            let e = ClientError::not_paired(
                "the node is online but did not accept this device; it may have been revoked or may not be \
                 paired with it any more",
            );
            for q in h.waiting {
                let _ = q.reply.send(Err(e.clone()));
            }
            Vec::new()
        } else {
            h.waiting
        };
        if done < HANDSHAKE_ATTEMPTS {
            return self.begin_handshake(live, node, done + 1, waiting).await;
        }
        let delay = Duration::from_secs(1 << (done - HANDSHAKE_ATTEMPTS).min(5)).min(Duration::from_secs(30));
        tracing::debug!(node = %node.id, attempts = done, ?delay, "client: the node did not answer; retrying later");
        live.sessions.insert(
            node.id.clone(),
            Session::Handshaking(Box::new(Handshaking {
                hs: None,
                deadline: Instant::now() + delay,
                attempts: done + 1,
                waiting,
            })),
        );
        Ok(())
    }

    /// 丢掉一个节点的会话：排着的和未完成的请求以 `why` 的离线错误收场，状态标成不在线。
    fn drop_session(&mut self, live: &mut Live, node_id: &str, why: &str) {
        let Some(s) = live.sessions.remove(node_id) else { return };
        let err = ClientError::offline(why.to_string());
        let was_ready = match s {
            Session::Handshaking(h) => {
                for q in h.waiting {
                    let _ = q.reply.send(Err(err.clone()));
                }
                false
            }
            Session::Ready(r) => {
                for (_, reply) in r.pending {
                    let _ = reply.send(Err(err.clone()));
                }
                true
            }
        };
        self.set_status(node_id, NodeStatus::Offline);
        if was_ready {
            self.emit(ClientEvent::NodeOffline { node_id: node_id.to_string() });
        }
    }

    /// 一个节点的通道坏了（失去同步）：丢掉它，节点还在线就重新握手。
    async fn restart_session(&mut self, live: &mut Live, node_id: &str, why: &str) -> std::result::Result<(), String> {
        tracing::debug!(node = %node_id, reason = %why, "re-establishing the channel to a node");
        self.drop_session(live, node_id, why);
        if let Some(node) = self.state.nodes.iter().find(|n| n.id == node_id).cloned() {
            self.begin_handshake(live, &node, 1, Vec::new()).await?;
        }
        Ok(())
    }

    async fn send_request(&mut self, live: &mut Live, node_id: &str, q: Queued) -> std::result::Result<(), String> {
        let Live { h, sessions, rate, .. } = live;
        let Some(Session::Ready(r)) = sessions.get_mut(node_id) else {
            let _ = q.reply.send(Err(ClientError::offline("the node is offline")));
            return Ok(());
        };
        let size = q.params.to_string().len();
        if !r.chan.flow.has_room(Instant::now(), size) {
            let _ = q.reply.send(Err(ClientError::new(
                ErrorClass::General,
                "congested",
                "the channel to the node is congested; try again in a moment",
            )));
            return Ok(());
        }
        r.next += 1;
        let id = r.next;
        let msg = RpcMessage::request(id, q.method, q.params);
        match r.chan.encode_request(&msg, &h.device_id, &mut h.seq, rate) {
            Ok(frames) => {
                r.pending.insert(id, q.reply);
                send_frames(h, frames).await
            }
            Err(e) => {
                let _ = q.reply.send(Err(ClientError::general(e.clone())));
                self.restart_session(live, node_id, &e).await
            }
        }
    }
}

enum Connected {
    Up(Box<Live>, Vec<Cmd>),
    Failed(Option<Duration>),
    Shutdown,
}

fn not_paired_with(node_id: &str) -> ClientError {
    ClientError::not_paired(format!("this device is not paired with node {node_id}"))
}

async fn send_frames(h: &mut HubWs, frames: Vec<Vec<u8>>) -> std::result::Result<(), String> {
    for b in frames {
        h.ws.send(Message::Binary(b.into())).await.map_err(|e| format!("sending to the hub failed: {e}"))?;
    }
    Ok(())
}
