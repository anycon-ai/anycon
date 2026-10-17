//! 已配对的客户端（peers）：公钥登记、配对（host / Noise XXpsk3 responder）、重连握手（IK responder）、
//! 每个 peer 一条 `Transport` 和一个有序发送队列（背压时合并 delta）。
//!
//! 约定（docs/protocol.md § 3–4、ADR-29 / ADR-30）：
//! - 配对由 joiner 发起（initiator 先写）：三条 handshake 信封 joiner→host→joiner；host 在第三条 payload 里读到对端的
//!   `ClientInfo`（JSON）作为设备名和类型（第 1、2 条的 payload 不受 PSK 保护，不信）；
//! - **短码由 daemon 本地生成，不发给 hub**：hub 只拿到 `match_id`（`derive_pair_secrets(code)`，一次 Argon2id 同时出 PSK）；`PairDone` 之后 daemon 经刚建立的
//!   Noise 传输发 `pair.ack`（带双方正式 id 和自己的公钥指纹），joiner 校验后才落盘——hub 伪造 `PairDone` 骗不到它；
//! - 重连由客户端发起 IK：daemon 创建 responder 时把登记过的公钥传进去，不符则 `read` 报错，直接忽略；
//! - 一条 JSON-RPC 消息 = 一次 `Transport::encrypt` 的完整输出 = 一个或多个 `data` 信封（`envelope::frame`：整条 / 分片 / 重置）；
//!   接收方拼完整条才解密。解密失败或分片乱序 → 丢掉这个 peer 的传输、回一个 `RESET`，等它重新做 IK 握手；
//! - 加密在真正发送前一刻、在每 peer 唯一的写任务里做，保证 nonce 顺序 = 发送顺序；
//! - hub 的 `Backpressure` 意味着那封信封被丢了：暂停该 peer，合并队列里的 delta，稍后**按原 seq 原字节**重发（窗口内保留最近发出的信封体）；
//! - 同一个客户端的请求按到达顺序串行处理（protocol § 7.3），见 `rpc::spawn_serial_worker`。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Role, Transport};
use anycon_protocol::envelope::frame;
use anycon_protocol::{
    methods, ClientInfo, Control, DeviceKind, DropReason, ErrorCode, Kind, NodePairBeginResult, NotifyBody, PairAck,
    PairFailReason, PairRole, PeerDevice, RpcError, RpcMessage, RpcNotification, RpcRequest,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Notify};
use zeroize::Zeroizing;

use crate::audit::Audit;
use crate::clients::{Caller, ClientSink, Clients};
use crate::config::{ConfigStore, Paths};
use crate::enroll::Enroll;
use crate::hub::{Bounce, HubOutbox};
use crate::identity::{write_private, Identity};
use crate::notify::NotifyOut;
use crate::rpc::Rpc;
use crate::store::{PeerRow, Store};
use crate::util::now_ms;

/// 一条消息（明文）最大多大：加密后要能被对端 `frame::Reassembler`（16 MiB）收下。
const MAX_PLAIN_MESSAGE: usize = frame::MAX_MESSAGE - 1024 * 1024;
const PAIR_CODE_TIMEOUT: Duration = Duration::from_secs(10);
/// 没有 device token 的临时 hub 连接：一生最多发这么多次 `PairBegin`（hub 的 `temp_pair_begins_per_conn`），
/// 连接最长 10 分钟（到点 4005）。用完 / 快到点了就换一条新连接再发（新码），不在同一条连接上反复发。
const TEMP_CONN_MAX_BEGINS: u32 = 3;
const TEMP_CONN_MAX_AGE: Duration = Duration::from_secs(9 * 60);
const FRESH_CONN_TIMEOUT: Duration = Duration::from_secs(15);
const PAIR_STEP_TIMEOUT: Duration = Duration::from_secs(70);
const STALL_LIMIT: Duration = Duration::from_secs(60);
/// 写任务每发出这么多字节就让出一小会儿（让 hub 的退回回执先到，大消息不会整条灌出去才发现被退回）。
const SEND_BATCH_BYTES: usize = 2 * 1024 * 1024;
const SEND_BATCH_GAP: Duration = Duration::from_millis(5);
/// 出站通道里还没写进 WebSocket 的字节超过这么多，写任务就先等。
const BACKLOG_LIMIT: usize = 2 * 1024 * 1024;
/// 暂停期间多久看一次是否可以继续。
const BLOCKED_POLL: Duration = Duration::from_millis(5);
/// 在途窗口满了（保留的都可能还在路上）时多久再问一次有没有空位：空位只会随时间（老过 `IN_FLIGHT_GUARD`）出现。
const WINDOW_POLL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------- 发送队列

#[derive(Default)]
struct QInner {
    items: VecDeque<RpcMessage>,
    coalesce: bool,
}

/// 每 peer 的有序发送队列。`coalesce` 为真（背压期间）时，相邻的同类 delta 事件合并成一条。
#[derive(Default)]
pub struct PeerQueue {
    inner: Mutex<QInner>,
    notify: Notify,
}

fn is_mergeable(ev: &Value) -> Option<&str> {
    let t = ev.get("type")?.as_str()?;
    matches!(t, "text_delta" | "thinking_delta" | "tool_output_delta").then_some(t)
}

/// 把 `new` 并进 `last`。条件：都是 `session.event` 通知，同会话、同类型（tool_output_delta 还要同 tool id）。
/// 合并后保留**最后**一个 seq / 时间戳——客户端按 seq 存事件，补历史时会用原始（未合并）的事件覆盖。
fn try_merge(last: &mut RpcMessage, new: &RpcMessage) -> bool {
    let (RpcMessage::Notification(a), RpcMessage::Notification(b)) = (last, new) else { return false };
    if a.method != methods::SESSION_EVENT || b.method != methods::SESSION_EVENT {
        return false;
    }
    let (Some(ea), Some(eb)) = (a.params.get("event"), b.params.get("event")) else { return false };
    let (Some(ta), Some(tb)) = (is_mergeable(ea), is_mergeable(eb)) else { return false };
    if ta != tb || a.params.get("session_id") != b.params.get("session_id") {
        return false;
    }
    if ta == "tool_output_delta" && ea.get("id") != eb.get("id") {
        return false;
    }
    let Some(add) = eb.get("text").and_then(Value::as_str).map(str::to_string) else { return false };
    let (seq, ts) = (b.params.get("seq").cloned(), b.params.get("ts_ms").cloned());
    if let Some(text) = a.params.get_mut("event").and_then(|e| e.get_mut("text")) {
        if let Some(s) = text.as_str() {
            *text = Value::String(format!("{s}{add}"));
        }
    }
    if let (Some(seq), Some(obj)) = (seq, a.params.as_object_mut()) {
        obj.insert("seq".into(), seq);
    }
    if let (Some(ts), Some(obj)) = (ts, a.params.as_object_mut()) {
        obj.insert("ts_ms".into(), ts);
    }
    true
}

impl PeerQueue {
    pub fn push(&self, msg: RpcMessage) {
        {
            let mut q = self.inner.lock().unwrap();
            let merged = q.coalesce && q.items.back_mut().is_some_and(|last| try_merge(last, &msg));
            if !merged {
                q.items.push_back(msg);
            }
        }
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<RpcMessage> {
        self.inner.lock().unwrap().items.pop_front()
    }

    fn set_coalesce(&self, on: bool) {
        self.inner.lock().unwrap().coalesce = on;
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 丢掉排队中的 `session.event` 通知（客户端回来后按 seq 补）。
    fn drop_events(&self) {
        self.inner
            .lock()
            .unwrap()
            .items
            .retain(|m| !matches!(m, RpcMessage::Notification(n) if n.method == methods::SESSION_EVENT));
    }
}

struct QueueSink(Arc<PeerSession>);

impl ClientSink for QueueSink {
    fn send(&self, n: RpcNotification) {
        self.0.queue.push(RpcMessage::Notification(n));
    }
}

// ---------------------------------------------------------------- 单个 peer 会话

pub struct PeerSession {
    pub device_id: String,
    transport: Mutex<Transport>,
    /// 入站分片重组（每个 peer 一个）。
    reassembler: Mutex<frame::Reassembler>,
    /// 出站消息的 `msg_id`（每条 RPC 消息一个，本 peer 内递增）。
    next_msg_id: AtomicU32,
    pub queue: PeerQueue,
    stop: Notify,
    stopped: std::sync::atomic::AtomicBool,
    /// 入站请求交给串行 worker；`stop()` 时取走，worker 随之退出（也断开 worker 对本会话的引用）。
    req_tx: Mutex<Option<mpsc::UnboundedSender<RpcRequest>>>,
    /// 本会话建立时（发出握手回复的那一封）的 `seq`；0 = 不知道。比它小的 `seq` 属于上一次传输。
    start_seq: std::sync::atomic::AtomicU64,
}

impl PeerSession {
    fn new(device_id: &str, transport: Transport) -> Arc<PeerSession> {
        Arc::new(PeerSession {
            device_id: device_id.to_string(),
            transport: Mutex::new(transport),
            reassembler: Mutex::new(frame::Reassembler::new()),
            next_msg_id: AtomicU32::new(1),
            queue: PeerQueue::default(),
            stop: Notify::new(),
            stopped: std::sync::atomic::AtomicBool::new(false),
            req_tx: Mutex::new(None),
            start_seq: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// `seq` 是不是本会话建立之前（上一次传输）发出的。
    fn predates(&self, seq: u64) -> bool {
        seq < self.start_seq.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        self.req_tx.lock().unwrap().take();
        self.stop.notify_waiters();
        self.stop.notify_one();
    }

    /// 取一条入站请求交给串行 worker；会话已停止返回 false。
    fn submit(&self, req: RpcRequest) -> bool {
        self.req_tx.lock().unwrap().as_ref().is_some_and(|tx| tx.send(req).is_ok())
    }

    /// 加密一条消息并切成若干 `data` 信封体（`frame::encode`）。超大的响应换成错误响应，超大的通知丢弃。
    fn seal(&self, msg: RpcMessage) -> Option<Vec<Vec<u8>>> {
        let mut plain = msg.encode();
        if plain.len() > MAX_PLAIN_MESSAGE {
            match &msg {
                RpcMessage::Response(r) => {
                    tracing::warn!(peer = %self.device_id, bytes = plain.len(), "response too large; replacing it with an error");
                    plain = RpcMessage::error(
                        r.id.clone(),
                        RpcError::new(
                            ErrorCode::Internal,
                            format!("the response is too large to send ({} bytes); ask for less data", plain.len()),
                        ),
                    )
                    .encode();
                }
                _ => {
                    tracing::warn!(peer = %self.device_id, bytes = plain.len(), "message too large; dropped");
                    return None;
                }
            }
        }
        let ct = match self.transport.lock().unwrap().encrypt(&plain) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(peer = %self.device_id, error = %e, "encrypt failed");
                return None;
            }
        };
        let msg_id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        Some(frame::encode(&ct, msg_id))
    }

    /// 等到向这个对端的发送不再被暂停（被退回 / 限速）。期间队列里的 delta 合并；持续 [`STALL_LIMIT`] 还不通就丢掉
    /// 排队中的事件（客户端回来后按 seq 补）。返回 `false` = 会话已停止。
    async fn wait_unblocked(&self, outbox: &HubOutbox, blocked_since: &mut Option<Instant>) -> bool {
        while outbox.is_blocked(&self.device_id) {
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.queue.set_coalesce(true);
            let since = *blocked_since.get_or_insert_with(Instant::now);
            if since.elapsed() > STALL_LIMIT {
                tracing::warn!(peer = %self.device_id, "peer stalled under backpressure for too long; dropping its events");
                self.queue.drop_events();
                *blocked_since = Some(Instant::now());
            }
            tokio::select! {
                _ = tokio::time::sleep(BLOCKED_POLL) => {}
                _ = self.stop.notified() => return false,
            }
        }
        *blocked_since = None;
        true
    }

    /// 等到在途窗口里有空位（见 [`HubOutbox::has_room`]）：保留缓冲里全是可能还在路上的信封时，**暂停写入新消息**，
    /// 而不是逐出其中的一部分。期间队列里的 delta 合并。返回 `false` = 会话已停止。
    async fn wait_room(&self, outbox: &HubOutbox, msg_bytes: usize) -> bool {
        let mut full_since: Option<Instant> = None;
        while !outbox.has_room(&self.device_id, msg_bytes) {
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.queue.set_coalesce(true);
            let since = *full_since.get_or_insert_with(Instant::now);
            if since.elapsed() > STALL_LIMIT {
                tracing::warn!(peer = %self.device_id, "the in-flight window stayed full for too long; dropping queued events");
                self.queue.drop_events();
                full_since = Some(Instant::now());
            }
            tokio::select! {
                _ = tokio::time::sleep(WINDOW_POLL) => {}
                _ = self.stop.notified() => return false,
            }
        }
        true
    }

    /// 写任务：取队列 → 加密 → 分片 → 发送。
    ///
    /// 发送方规则在 [`HubOutbox`]（保留原文、被退回后暂停并按升序原样重发、`rate_limited` 暂停整条连接）；这里只负责：
    /// - 目标对端被卡住（有被退回等着重发的 / 限速中）时**不再加密新消息**——暂停期间队列里的 delta 合并，
    ///   恢复后才一起发（加密必须发生在真正发送前一刻，nonce 顺序 = 发送顺序）；一条消息写到一半被卡住，剩下的分片留在
    ///   手里等，不往保留缓冲里堆；
    /// - 在途窗口满了（保留的都可能还在路上）时**暂停写入新消息**，不逐出；
    /// - 大消息每发 [`SEND_BATCH_BYTES`] 让出一下，让 hub 的退回回执先到。
    async fn writer(self: Arc<Self>, outbox: Arc<HubOutbox>) {
        let mut blocked_since: Option<Instant> = None;
        // 自上次让出以来发了多少字节（跨消息累计：很多条 1 MiB 的消息也要隔一阵让出一下）
        let mut batch = 0usize;
        loop {
            if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if !self.wait_unblocked(&outbox, &mut blocked_since).await {
                return;
            }
            // 没有被卡住：回到逐条发送（暂停期间合并过的 delta 已经是合并后的样子，照发）
            self.queue.set_coalesce(false);
            let Some(msg) = self.queue.pop() else {
                tokio::select! {
                    _ = self.queue.notify.notified() => {}
                    _ = self.stop.notified() => return,
                }
                continue;
            };
            let Some(bodies) = self.seal(msg) else { continue };
            let total: usize = bodies.iter().map(Vec::len).sum();
            if !self.wait_room(&outbox, total).await {
                return;
            }
            let group = outbox.new_group();
            for body in bodies {
                // 写到一半被退回了：等重发完成再写下一片（分片 seq 必须排在重发的后面）
                if !self.wait_unblocked(&outbox, &mut blocked_since).await {
                    return;
                }
                // 线路上的积压太多就先等（否则保留缓冲里「还没真正发出去」的帧会被当成已发出而逐出）
                tokio::select! {
                    _ = outbox.wait_backlog_below(BACKLOG_LIMIT) => {}
                    _ = self.stop.notified() => return,
                }
                batch += body.len();
                if outbox.send_in_group(&self.device_id, Kind::Data, body, Some(group)).is_none() {
                    // hub 没连上：这封丢了，Transport 计数已经前进 -> 对端必须重握手。丢掉整个会话。
                    tracing::debug!(peer = %self.device_id, "hub not connected; dropping peer session");
                    self.stop();
                    return;
                }
                if batch >= SEND_BATCH_BYTES {
                    batch = 0;
                    tokio::select! {
                        _ = tokio::time::sleep(SEND_BATCH_GAP) => {}
                        _ = self.stop.notified() => return,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------- 配对状态

enum Phase {
    /// 已发 `PairBegin`，等 hub 的 `PairRegistered`（`Err` = `PairFailed`）。
    AwaitRegistered {
        psk: Zeroizing<[u8; 32]>,
        tx: oneshot::Sender<Result<u64, PairFailReason>>,
    },
    AwaitMatch {
        psk: Zeroizing<[u8; 32]>,
    },
    Handshaking {
        peer: String,
        hs: Box<Handshake>,
    },
    /// 握手完成、已登记（临时 id），等 `PairDone`；`transport` 用来发 `pair.ack`。
    Confirming {
        peer: String,
        transport: Option<Transport>,
    },
}

struct PairState {
    phase: Phase,
    caller: Caller,
    updated: Instant,
    generation: u64,
}

/// 谁在等 `pair_done`：`pair_confirm` 发出去之后，hub 的 `pair_done` / `pair_failed` 里没有会话标识，
/// 所以同一时刻只允许一个（配对，或某个接入会话）占着「确认槽」，回执就算它的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmOwner {
    Pairing,
    Enroll(String),
}

pub struct Peers {
    store: Store,
    paths: Paths,
    identity: Arc<Identity>,
    outbox: Arc<HubOutbox>,
    clients: Arc<Clients>,
    audit: Audit,
    config: Arc<ConfigStore>,
    records: Mutex<HashMap<String, PeerRow>>,
    online: Mutex<HashSet<String>>,
    sessions: Mutex<HashMap<String, Arc<PeerSession>>>,
    pairing: Mutex<Option<PairState>>,
    /// 账号内接入引擎（ADR-36）：握手帧、`enroll_request` / `enroll_failed`、RPC 都归它；收尾借用这里的暂存 / 改键 / `pair.ack`。
    pub enroll: Arc<Enroll>,
    /// 确认槽：谁的 `pair_confirm` 在等 `pair_done`。
    confirm_owner: Mutex<Option<ConfirmOwner>>,
    /// 配对确认阶段暂存的 peer（以 hub 给的临时 id 登记，`PairDone` 之后才改键成正式 id）：不出现在 `node.devices`
    /// 里，也不收通知。看门狗、hub 断线、新的 `pair_begin`、`PairFailed` 都会把它连同登记一起删掉。
    staged: Mutex<HashSet<String>>,
    /// 配对每一步的看门狗时长（测试里可以缩短）。
    pair_step_timeout: Mutex<Duration>,
    pair_gen: std::sync::atomic::AtomicU64,
    /// 吊销时 hub 没连上：重连后补发 `Revoke`。
    pending_hub_revokes: Mutex<HashSet<String>>,
    /// 当前 hub 连接（epoch）上已经发过几次 `PairBegin`。
    begins_on_conn: Mutex<(u64, u32)>,
    /// 自己的弱引用（延迟断开等后台任务用）。
    me: std::sync::OnceLock<std::sync::Weak<Peers>>,
    /// 配对拿到新 token 后通知 hub 连接重连。
    pub reconnect: Arc<Notify>,
    /// 传输被我们重置过多少次（`reset_peer`；测试 / 诊断用）。
    resets: std::sync::atomic::AtomicU64,
    /// 测试用：卡住配对里「暂存记录写库」之前的那一步（见 [`Peers::hold_pair_saves`]）。
    #[cfg(any(test, feature = "testing"))]
    save_gate: Mutex<Option<PairSaveGate>>,
}

/// [`Peers::hold_pair_saves`] 的两端：daemon 登记暂存 peer（写库之前）时 `started.notify_one()`，然后等 `release`。
#[cfg(any(test, feature = "testing"))]
#[derive(Clone)]
pub struct PairSaveGate {
    pub started: Arc<Notify>,
    pub release: Arc<Notify>,
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(crate) fn parse_client_info(payload: &[u8], device_id: &str) -> (String, DeviceKind) {
    let fallback = || (format!("device-{}", short_id(device_id)), DeviceKind::Web);
    if payload.is_empty() {
        return fallback();
    }
    if let Ok(info) = serde_json::from_slice::<ClientInfo>(payload) {
        let name: String = info.name.chars().take(100).collect();
        return (if name.trim().is_empty() { fallback().0 } else { name }, info.kind);
    }
    if let Ok(v) = serde_json::from_slice::<Value>(payload) {
        let name = v
            .get("name")
            .and_then(Value::as_str)
            .map(|n| n.chars().take(100).collect::<String>())
            .filter(|n| !n.trim().is_empty());
        let kind =
            v.get("kind").and_then(|k| serde_json::from_value::<DeviceKind>(k.clone()).ok()).unwrap_or(DeviceKind::Web);
        return (name.unwrap_or_else(|| fallback().0), kind);
    }
    fallback()
}

impl Peers {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        store: Store,
        paths: Paths,
        identity: Arc<Identity>,
        outbox: Arc<HubOutbox>,
        clients: Arc<Clients>,
        audit: Audit,
        config: Arc<ConfigStore>,
        enroll: Arc<Enroll>,
    ) -> Arc<Self> {
        let mut records: HashMap<String, PeerRow> = match store.list_peers().await {
            Ok(rows) => rows.into_iter().map(|p| (p.device_id.clone(), p)).collect(),
            Err(e) => {
                tracing::error!(error = %e, "cannot read the paired devices from the database");
                HashMap::new()
            }
        };
        // 数据库为空但 peers.json 在（数据库被换过）：从 json 导入
        if records.is_empty() {
            for p in read_peers_json(&paths) {
                if let Err(e) = store.upsert_peer(&p).await {
                    tracing::warn!(error = %e, peer = %p.device_id, "cannot import a paired device from peers.json");
                }
                records.insert(p.device_id.clone(), p);
            }
        }
        let this = Arc::new(Peers {
            store,
            paths,
            identity,
            outbox,
            clients,
            audit,
            config,
            records: Mutex::new(records),
            online: Mutex::new(HashSet::new()),
            sessions: Mutex::new(HashMap::new()),
            pairing: Mutex::new(None),
            enroll,
            confirm_owner: Mutex::new(None),
            staged: Mutex::new(HashSet::new()),
            pair_step_timeout: Mutex::new(PAIR_STEP_TIMEOUT),
            pair_gen: std::sync::atomic::AtomicU64::new(0),
            pending_hub_revokes: Mutex::new(HashSet::new()),
            begins_on_conn: Mutex::new((0, 0)),
            me: std::sync::OnceLock::new(),
            reconnect: Arc::new(Notify::new()),
            resets: std::sync::atomic::AtomicU64::new(0),
            #[cfg(any(test, feature = "testing"))]
            save_gate: Mutex::new(None),
        });
        let _ = this.me.set(Arc::downgrade(&this));
        this.enroll.bind_peers(&this);
        this.write_json();
        this
    }

    // ------------------------------------------------------------ 登记

    fn json_doc(records: &HashMap<String, PeerRow>) -> String {
        let list: Vec<Value> = records
            .values()
            .map(|p| {
                serde_json::json!({
                    "device_id": p.device_id, "name": p.name, "kind": p.kind, "pubkey": hex::encode(p.pubkey),
                    "fingerprint": anycon_crypto::fingerprint(&p.pubkey), "paired_at_ms": p.paired_at_ms,
                })
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({ "peers": list })).unwrap_or_default()
    }

    fn write_json(&self) {
        let doc = Self::json_doc(&self.records.lock().unwrap());
        if let Err(e) = write_private(&self.paths.peers_json(), &doc) {
            tracing::warn!(error = %e, "failed to write peers.json");
        }
    }

    async fn save_record(&self, row: PeerRow) -> Result<(), RpcError> {
        self.store
            .upsert_peer(&row)
            .await
            .map_err(|e| RpcError::new(ErrorCode::Internal, format!("cannot save the paired device: {e}")))?;
        self.records.lock().unwrap().insert(row.device_id.clone(), row);
        self.write_json();
        Ok(())
    }

    pub fn list(&self) -> Vec<PeerDevice> {
        let online = self.online.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        let staged = self.staged.lock().unwrap().clone();
        let mut v: Vec<PeerDevice> = self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|p| !staged.contains(&p.device_id))
            .map(|p| PeerDevice {
                device_id: p.device_id.clone(),
                name: p.name.clone(),
                kind: p.kind,
                fingerprint: anycon_crypto::fingerprint(&p.pubkey),
                paired_at_ms: p.paired_at_ms,
                last_seen_ms: p.last_seen_ms,
                online: online.contains(&p.device_id) || sessions.contains_key(&p.device_id),
            })
            .collect();
        v.sort_by_key(|p| p.paired_at_ms);
        v
    }

    pub fn paired_ids(&self) -> Vec<String> {
        let staged = self.staged.lock().unwrap().clone();
        self.records.lock().unwrap().keys().filter(|k| !staged.contains(*k)).cloned().collect()
    }

    /// 缩短配对每一步的看门狗（测试用）。
    pub fn set_pair_step_timeout(&self, d: Duration) {
        *self.pair_step_timeout.lock().unwrap() = d;
    }

    /// 配对失败 / 作废：把暂存的 peer（确认阶段以临时 id 登记的那条）连同登记一起删掉。
    async fn discard_staged(&self, peer: &str) {
        self.staged.lock().unwrap().remove(peer);
        let removed = self.records.lock().unwrap().remove(peer).is_some();
        if let Err(e) = self.store.delete_peer(peer).await {
            tracing::warn!(error = %e, "cannot remove the tentative paired device");
        }
        if removed {
            self.write_json();
        }
    }

    /// 取走进行中的配对状态；如果它停在确认阶段，把暂存的 peer 也删掉。
    async fn abort_pairing(&self) {
        let state = self.pairing.lock().unwrap().take();
        if let Some(PairState { phase: Phase::Confirming { peer, .. }, .. }) = state {
            self.release_confirm(&ConfirmOwner::Pairing);
            if !peer.is_empty() {
                self.discard_staged(&peer).await;
            }
        }
    }

    pub fn record(&self, device_id: &str) -> Option<PeerRow> {
        self.records.lock().unwrap().get(device_id).cloned()
    }

    /// 按 id（或唯一的 id 前缀 / 名称）找设备。
    pub fn resolve(&self, key: &str) -> Option<String> {
        let recs = self.records.lock().unwrap();
        if recs.contains_key(key) {
            return Some(key.to_string());
        }
        let by_prefix: Vec<&String> = recs.keys().filter(|k| !key.is_empty() && k.starts_with(key)).collect();
        if by_prefix.len() == 1 {
            return Some(by_prefix[0].clone());
        }
        let by_name: Vec<&PeerRow> = recs.values().filter(|p| p.name == key).collect();
        if by_name.len() == 1 {
            return Some(by_name[0].device_id.clone());
        }
        None
    }

    /// 吊销：先持久化（`peers.json` 和数据库都要成功），再丢 Transport、通知 hub 删路由。
    ///
    /// 持久化失败返回错误、内存状态保持原样（可以重试）——不能出现「内存里删了、重启后又回来」。
    /// 返回 hub 是否已收到 `Revoke`；hub 没连上时记下，重连后补发（daemon 这边的公钥已经删了，握手本来就会被拒）。
    ///
    /// `caller` 就是被吊销的设备自己（web 的「退出并移除本设备」）时，**先回复再断开**：公钥马上删掉、不能再握手，
    /// 但它的会话和 hub 的 `Revoke`（会让 hub 立刻关它的连接）要等响应发出去之后再处理。
    pub async fn revoke(&self, device_id: &str, caller_device: Option<&str>) -> Result<bool, RpcError> {
        let before = self.records.lock().unwrap().clone();
        if !before.contains_key(device_id) {
            return Err(RpcError::new(ErrorCode::NotFound, format!("device {device_id} is not paired")));
        }
        let mut after = before.clone();
        after.remove(device_id);
        let internal = |what: &str, e: &dyn std::fmt::Display| {
            RpcError::new(ErrorCode::Internal, format!("revoke failed: {what}: {e}"))
        };
        write_private(&self.paths.peers_json(), &Self::json_doc(&after)).map_err(|e| internal("peers.json", &e))?;
        if let Err(e) = self.store.delete_peer(device_id).await {
            // 回滚 peers.json，让两处保持一致
            if let Err(e2) = write_private(&self.paths.peers_json(), &Self::json_doc(&before)) {
                tracing::error!(error = %e2, "could not roll peers.json back after a failed revoke");
            }
            return Err(internal("database", &e));
        }
        self.records.lock().unwrap().remove(device_id);
        if caller_device == Some(device_id) {
            let this = self.sessions.lock().unwrap().get(device_id).cloned();
            let (outbox, id) = (self.outbox.clone(), device_id.to_string());
            let me = self.self_ref();
            tokio::spawn(async move {
                // 等响应进了出站队列并发出去（队列空了再留一点余量），然后才断开
                if let Some(s) = &this {
                    let t = Instant::now();
                    while !s.queue.is_empty() && t.elapsed() < Duration::from_secs(3) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                if let Some(p) = me {
                    p.drop_session(&id);
                    if !outbox.send_control(&Control::Revoke { device: id.clone() }) {
                        p.pending_hub_revokes.lock().unwrap().insert(id);
                    }
                }
            });
            return Ok(true);
        }
        self.drop_session(device_id);
        let notified = self.outbox.send_control(&Control::Revoke { device: device_id.to_string() });
        if !notified {
            tracing::warn!(peer = %device_id, "hub is not connected; the revoke will be sent when it reconnects");
            self.pending_hub_revokes.lock().unwrap().insert(device_id.to_string());
        }
        Ok(notified)
    }

    pub async fn rename(&self, device_id: &str, name: &str) -> Result<(), RpcError> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return Err(RpcError::new(ErrorCode::Invalid, "name must be 1-100 bytes"));
        }
        let mut row = self.record(device_id).ok_or_else(|| RpcError::new(ErrorCode::NotFound, "device not paired"))?;
        row.name = name.to_string();
        self.save_record(row).await
    }

    fn self_ref(&self) -> Option<Arc<Peers>> {
        self.me.get().and_then(std::sync::Weak::upgrade)
    }

    pub fn online_count(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// 测试用：让下一次配对停在「已标成暂存、记录还没写库」这一步，等测试在这个窗口里作废这次配对（等价于写库的 `.await`
    /// 期间看门狗 / hub 断线 / 新的 `pair_begin` / `PairFailed` 抢先动手），再放行。
    #[cfg(any(test, feature = "testing"))]
    pub fn hold_pair_saves(&self) -> PairSaveGate {
        let gate = PairSaveGate { started: Arc::new(Notify::new()), release: Arc::new(Notify::new()) };
        *self.save_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(any(test, feature = "testing"))]
    async fn pair_save_gate(&self) {
        let gate = self.save_gate.lock().unwrap().take();
        if let Some(g) = gate {
            g.started.notify_one();
            g.release.notified().await;
        }
    }

    /// 到目前为止重置过多少次对端的传输（测试 / 诊断用）。
    pub fn reset_count(&self) -> u64 {
        self.resets.load(Ordering::Relaxed)
    }

    // ------------------------------------------------------------ 在线状态与会话

    pub fn set_online_list(&self, ids: &[String]) {
        *self.online.lock().unwrap() = ids.iter().cloned().collect();
        // 刚（重新）认证成功：补发上次没送出的 Revoke
        let pending: Vec<String> = self.pending_hub_revokes.lock().unwrap().drain().collect();
        for d in pending {
            if !self.outbox.send_control(&Control::Revoke { device: d.clone() }) {
                self.pending_hub_revokes.lock().unwrap().insert(d);
            }
        }
    }

    pub fn set_peer_online(&self, id: &str, online: bool) {
        if online {
            // 设备每次认证成功都会收到 peer_online（同设备新连接顶掉旧连接时不发 peer_offline）：
            // 旧会话作废，等它发来新的 IK 握手
            self.drop_session(id);
            self.online.lock().unwrap().insert(id.to_string());
        } else {
            self.online.lock().unwrap().remove(id);
            self.drop_session(id);
        }
    }

    /// hub 连接断开：所有 peer 会话作废，进行中的配对也作废——停在确认阶段的话，暂存的 peer 记录一并删掉。
    pub async fn on_hub_down(&self) {
        self.online.lock().unwrap().clear();
        let ids: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.drop_session(&id);
        }
        self.abort_pairing().await;
        // 接入会话随 hub 连接作废（hub 重启即作废）；确认槽也清空
        self.enroll.on_hub_down().await;
        *self.confirm_owner.lock().unwrap() = None;
    }

    fn drop_session(&self, id: &str) {
        let removed = self.sessions.lock().unwrap().remove(id);
        if let Some(s) = removed {
            s.stop();
            self.clients.unregister_device(id);
        }
        // 保留缓冲随会话作废；还在等重发的 seq 用 RESET 顶替，免得 hub 一直卡在它们上
        self.outbox.forget(id);
    }

    /// 传输失步（解密失败 / 分片乱序 / 对端发了 RESET）：丢掉这个 peer 的会话，告诉它重置，等它新的 IK 握手。
    fn reset_peer(&self, id: &str, why: &str) {
        self.resets.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(peer = %id, reason = why, "resetting the peer transport; waiting for a new handshake");
        self.drop_session(id);
        self.outbox.send_envelope(id, Kind::Data, frame::reset_body());
    }

    pub fn on_undeliverable(&self, to: &str) {
        // 对端离线：丢掉 Transport，等它重新上线后重握手
        self.drop_session(to);
    }

    /// hub 退回了发给 `to` 的 `seq`（目标缓冲满，或发送方超过速率预算）。发送方规则见 [`HubOutbox`]：暂停、等一会儿、
    /// 按升序原样重发；被退回的 seq 已经不在保留缓冲里就重置这个对端的传输。
    pub fn on_backpressure(&self, to: &str, seq: u64) {
        match self.outbox.on_backpressure(to, seq) {
            Bounce::Scheduled => {
                // 暂停期间队列里的 delta 合并，恢复后一起发
                if let Some(s) = self.sessions.lock().unwrap().get(to).cloned() {
                    s.queue.set_coalesce(true);
                }
            }
            Bounce::Lost => {
                // 出站通道已经用一封 RESET 占住了这个 seq（hub 的粘住状态因此能解开）。要不要再重置对端的传输，
                // 看这个 seq 属于哪一次会话：重新握手之前发出的，只是旧传输的残余，当前的传输没有少任何一封。
                let current = self.sessions.lock().unwrap().get(to).cloned();
                match current {
                    Some(s) if s.predates(seq) => tracing::info!(
                        peer = %to,
                        seq,
                        "the hub bounced an envelope of a previous transport; only unsticking it, the current transport is fine"
                    ),
                    Some(_) => self.reset_peer(to, "the hub bounced an envelope that is no longer retained"),
                    None => {}
                }
            }
        }
    }

    /// hub 丢了 `seq` 那封（不是因为对端离线）。`Dropped` 每连接每秒至多一条，不是每个被丢的 seq 都有，
    /// 所以不能据此逐条重发：它是「我在发错东西」的信号。分片之后不该再有 `too_large`；出现了（或者被限速 / 配对关系不对）
    /// 就说明这个 peer 的传输少了一封，只能重置、等它重新握手。
    pub fn on_dropped(&self, seq: u64, reason: DropReason) {
        let owner = self.outbox.peer_of_seq(seq);
        match reason {
            DropReason::TooLarge => {
                tracing::warn!(seq, "the hub dropped an envelope as too large; fragmentation should prevent this")
            }
            DropReason::NotPaired => tracing::warn!(seq, "the hub dropped an envelope: not paired"),
            DropReason::RateLimited => tracing::warn!(seq, "the hub dropped an envelope: rate limited"),
        }
        if let Some(id) = owner {
            self.reset_peer(&id, "the hub dropped one of our envelopes");
        }
    }

    // ------------------------------------------------------------ 重连握手（IK responder）

    pub async fn on_handshake(self: &Arc<Self>, from: &str, body: &[u8], rpc: &Arc<Rpc>) {
        // 账号内接入的握手帧（body 是 `EnrollFrame`；发起方是这个接入会话的对端）
        if self.enroll.on_handshake(from, body).await {
            return;
        }
        // 配对中的握手帧
        if self.try_pairing_handshake(from, body).await {
            return;
        }
        let Some(rec) = self.record(from) else {
            tracing::debug!(peer = %from, "handshake from an unpaired device; ignored");
            return;
        };
        let kp = self.identity.keypair();
        let mut hs = match Handshake::reconnect(Role::Responder, &kp, Some(&rec.pubkey)) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(peer = %from, error = %e, "cannot start responder handshake");
                return;
            }
        };
        if let Err(e) = hs.read(body) {
            tracing::warn!(peer = %from, error = %e, "rejected reconnect handshake (key mismatch or malformed)");
            return;
        }
        let reply = match hs.write(&[]) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(peer = %from, error = %e, "handshake write failed");
                return;
            }
        };
        if !hs.is_finished() {
            tracing::warn!(peer = %from, "IK handshake unexpectedly not finished");
            return;
        }
        // 防御：握手读出的静态公钥必须等于登记的
        if hs.remote_public() != Some(rec.pubkey) {
            tracing::warn!(peer = %from, "static key does not match the paired key; ignored");
            return;
        }
        let transport = match hs.into_transport() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(peer = %from, error = %e, "handshake finalize failed");
                return;
            }
        };
        // 先替换旧会话，再回握手
        self.drop_session(from);
        let sess = PeerSession::new(from, transport);
        let caller =
            Caller { device_id: from.to_string(), device_name: rec.name.clone(), kind: rec.kind, local: false };
        let client = self.clients.register(caller, Box::new(QueueSink(sess.clone())));
        // 请求按到达顺序串行处理
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        *sess.req_tx.lock().unwrap() = Some(req_tx);
        let out_sess = sess.clone();
        crate::rpc::spawn_serial_worker(rpc.clone(), client, req_rx, move |resp| out_sess.queue.push(resp));
        self.sessions.lock().unwrap().insert(from.to_string(), sess.clone());
        self.online.lock().unwrap().insert(from.to_string());
        let Some(start_seq) = self.outbox.send_envelope(from, Kind::Handshake, reply) else {
            self.drop_session(from);
            return;
        };
        // 记下会话从哪个 seq 开始：之后比它小的 seq 被退回，属于上一次传输
        sess.start_seq.store(start_seq, Ordering::SeqCst);
        tokio::spawn(sess.clone().writer(self.outbox.clone()));
        let mut row = rec;
        row.last_seen_ms = Some(now_ms());
        if let Err(e) = self.store.touch_peer(from, now_ms()).await {
            tracing::warn!(peer = %from, error = %e, "cannot record the last-seen time");
        }
        self.records.lock().unwrap().insert(from.to_string(), row);
        tracing::info!(peer = %from, "peer session established");
    }

    // ------------------------------------------------------------ 数据

    pub async fn on_data(self: &Arc<Self>, from: &str, body: &[u8]) {
        let Some(sess) = self.sessions.lock().unwrap().get(from).cloned() else {
            tracing::debug!(peer = %from, "data from a peer without a session; ignored");
            return;
        };
        let frame = match frame::decode(body) {
            Ok(f) => f,
            Err(e) => {
                self.reset_peer(from, &format!("bad data frame: {e}"));
                return;
            }
        };
        if matches!(frame, frame::DataFrame::Reset) {
            // 对端的传输失步了，它会重新做 IK 握手
            tracing::warn!(peer = %from, "the peer asked for a transport reset");
            self.drop_session(from);
            return;
        }
        let ciphertext = match sess.reassembler.lock().unwrap().push(frame) {
            Ok(Some(ct)) => ct,
            Ok(None) => return,
            Err(e) => {
                self.reset_peer(from, &format!("fragment error: {e}"));
                return;
            }
        };
        let plain = match sess.transport.lock().unwrap().decrypt(&ciphertext) {
            Ok(p) => p,
            Err(e) => {
                self.reset_peer(from, &format!("decrypt failed: {e}"));
                return;
            }
        };
        let msg = match RpcMessage::decode(&plain) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(peer = %from, error = %e, "malformed rpc message");
                return;
            }
        };
        let RpcMessage::Request(req) = msg else { return };
        if !sess.submit(req) {
            tracing::debug!(peer = %from, "session stopped; request dropped");
        }
    }

    // ------------------------------------------------------------ 配对（host / responder）

    /// `node.pair_begin`：本地生成短码，把 `match_id` 交给 hub 登记，返回短码和链接。
    /// 短码和 `match_id` 都不进日志、不进审计。
    ///
    /// 还没有 device token 的 daemon（首次配对）用的是临时 hub 连接：hub 限制一条临时连接一生最多 3 次 `PairBegin`、
    /// 最长 10 分钟。所以连接用完 / 快到期了就先换一条新连接再发；被回 `rate_limited` 也换一次连接重试。
    pub async fn pair_begin(self: &Arc<Self>, caller: &Caller, long: bool) -> Result<NodePairBeginResult, RpcError> {
        if !self.outbox.connected() {
            return Err(RpcError::new(ErrorCode::Unsupported, "not connected to the hub; check `anycon doctor`"));
        }
        let hub_url = self
            .config
            .get()
            .hub
            .url
            .clone()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .ok_or_else(|| RpcError::new(ErrorCode::Unsupported, "hub.url is not configured"))?;
        let code = if long { anycon_crypto::generate_pair_code_long() } else { anycon_crypto::generate_pair_code() };
        // Argon2id（64 MiB、约 0.1–0.3 s）是 CPU 密集的同步计算：放进 blocking 线程，别堵住运行时。一次出 match_id 和 PSK
        let secrets = {
            let code = code.clone();
            tokio::task::spawn_blocking(move || anycon_crypto::derive_pair_secrets(&code))
                .await
                .map_err(|e| RpcError::new(ErrorCode::Internal, format!("pairing code: {e}")))?
                .map_err(|e| RpcError::new(ErrorCode::Internal, format!("pairing code: {e}")))?
        };
        // PSK 的副本放进 `Zeroizing`（`Phase` 里的也是），用完 / 配对作废时清零；`secrets` 自己 Drop 时清零
        let (psk, match_id) = (Zeroizing::new(secrets.psk), secrets.match_id.clone());
        let mut retried = false;
        let expires_at_ms = loop {
            let temp = self.identity.device_token().is_none();
            if temp && self.temp_connection_used_up() {
                self.fresh_connection().await?;
            }
            let (tx, rx) = oneshot::channel();
            let gen = self.pair_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            // 上一次还停在确认阶段：它暂存的 peer 记录不能留着
            self.abort_pairing().await;
            *self.pairing.lock().unwrap() = Some(PairState {
                phase: Phase::AwaitRegistered { psk: psk.clone(), tx },
                caller: caller.clone(),
                updated: Instant::now(),
                generation: gen,
            });
            let epoch = self.outbox.epoch();
            if !self.outbox.send_control(&Control::PairBegin { match_id: match_id.clone() }) {
                self.clear_pairing(gen);
                return Err(RpcError::new(ErrorCode::Unsupported, "not connected to the hub"));
            }
            self.note_pair_begin(epoch);
            match tokio::time::timeout(PAIR_CODE_TIMEOUT, rx).await {
                Ok(Ok(Ok(at))) => break at,
                Ok(Ok(Err(PairFailReason::RateLimited))) if temp && !retried => {
                    retried = true;
                    self.clear_pairing(gen);
                    tracing::info!(
                        "the hub rate-limited PairBegin on this temporary connection; retrying on a fresh one"
                    );
                    self.fresh_connection().await?;
                }
                Ok(Ok(Err(why))) => {
                    self.clear_pairing(gen);
                    return Err(RpcError::new(
                        ErrorCode::Unsupported,
                        format!("the hub refused the pairing request: {why:?}"),
                    ));
                }
                _ => {
                    self.clear_pairing(gen);
                    return Err(RpcError::new(ErrorCode::Internal, "the hub did not confirm the pairing request"));
                }
            }
        };
        self.audit.record(caller, "node.pair_begin", None, serde_json::json!({ "long": long })).await;
        let link = format!("{hub_url}/pair#code={code}");
        Ok(NodePairBeginResult { code, expires_at_ms, link })
    }

    /// 记一次 `PairBegin`（按 hub 连接计数）。
    fn note_pair_begin(&self, epoch: u64) {
        let mut b = self.begins_on_conn.lock().unwrap();
        if b.0 != epoch {
            *b = (epoch, 0);
        }
        b.1 += 1;
    }

    /// 这条临时连接的 `PairBegin` 额度用完了，或者快到 hub 的连接寿命了。
    fn temp_connection_used_up(&self) -> bool {
        let epoch = self.outbox.epoch();
        let used = {
            let b = self.begins_on_conn.lock().unwrap();
            if b.0 == epoch {
                b.1
            } else {
                0
            }
        };
        used >= TEMP_CONN_MAX_BEGINS || self.outbox.connected_for().is_some_and(|age| age >= TEMP_CONN_MAX_AGE)
    }

    /// 让 hub 连接换一条新的（重连），等到新连接认证成功。
    async fn fresh_connection(&self) -> Result<(), RpcError> {
        let old = self.outbox.epoch();
        self.reconnect.notify_one();
        let deadline = Instant::now() + FRESH_CONN_TIMEOUT;
        while Instant::now() < deadline {
            if self.outbox.connected() && self.outbox.epoch() != old {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(RpcError::new(
            ErrorCode::Unsupported,
            "could not open a fresh connection to the hub; check `anycon doctor`",
        ))
    }

    fn clear_pairing(&self, gen: u64) {
        let mut p = self.pairing.lock().unwrap();
        if p.as_ref().is_some_and(|s| s.generation == gen) {
            *p = None;
        }
    }

    /// hub 控制消息里和配对有关的部分。
    pub async fn on_control(self: &Arc<Self>, c: Control) {
        match c {
            Control::PairRegistered { expires_at_ms } => {
                let mut p = self.pairing.lock().unwrap();
                if let Some(ps) = p.as_mut() {
                    if matches!(ps.phase, Phase::AwaitRegistered { .. }) {
                        if let Phase::AwaitRegistered { psk, tx } =
                            std::mem::replace(&mut ps.phase, Phase::AwaitMatch { psk: Zeroizing::new([0; 32]) })
                        {
                            ps.phase = Phase::AwaitMatch { psk };
                            ps.updated = Instant::now();
                            let _ = tx.send(Ok(expires_at_ms));
                        }
                    }
                }
            }
            Control::PairMatched { peer, role } => {
                if role != PairRole::Host {
                    return;
                }
                let gen = {
                    let mut p = self.pairing.lock().unwrap();
                    let Some(ps) = p.as_mut() else { return };
                    let psk: Zeroizing<[u8; 32]> = match &ps.phase {
                        Phase::AwaitMatch { psk } | Phase::AwaitRegistered { psk, .. } => psk.clone(),
                        _ => return,
                    };
                    let kp = self.identity.keypair();
                    match Handshake::pairing(Role::Responder, &kp, &psk) {
                        Ok(hs) => {
                            ps.phase = Phase::Handshaking { peer: peer.clone(), hs: Box::new(hs) };
                            ps.updated = Instant::now();
                            ps.generation
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "cannot start pairing handshake");
                            *p = None;
                            return;
                        }
                    }
                };
                tracing::info!(peer = %peer, "pairing matched; waiting for the joiner's handshake");
                // 看门狗：一定时间内没走完就清掉（停在确认阶段的话，暂存的 peer 记录也删掉）
                let this = self.clone();
                let step = *self.pair_step_timeout.lock().unwrap();
                tokio::spawn(async move {
                    tokio::time::sleep(step).await;
                    let expired = {
                        let mut p = this.pairing.lock().unwrap();
                        if p.as_ref()
                            .is_some_and(|s| s.generation == gen && !matches!(s.phase, Phase::AwaitMatch { .. }))
                        {
                            tracing::warn!("pairing timed out");
                            p.take()
                        } else {
                            None
                        }
                    };
                    if let Some(PairState { phase: Phase::Confirming { peer, .. }, .. }) = expired {
                        this.release_confirm(&ConfirmOwner::Pairing);
                        if !peer.is_empty() {
                            this.discard_staged(&peer).await;
                        }
                    }
                });
            }
            Control::PairDone { peer, device_id, device_token } => {
                self.on_pair_done(peer, device_id, device_token).await
            }
            Control::PairFailed { reason } => {
                // `pair_failed` 一律归配对：接入的失败（含收尾阶段的配额等）hub 只发带 id 的 `enroll_failed`，
                // 由接入引擎处理。确认槽在接入手里时来的 `pair_failed` 是别的配对的（它的 joiner 断线 / 超时），
                // 不能算给正在收尾的接入；这里只放自己的槽（`release_confirm` 只放 `Pairing` 占着的）。
                self.release_confirm(&ConfirmOwner::Pairing);
                let state = self.pairing.lock().unwrap().take();
                tracing::warn!(?reason, "pairing failed");
                match state {
                    Some(PairState { phase: Phase::AwaitRegistered { tx, .. }, .. }) => {
                        let _ = tx.send(Err(reason));
                    }
                    Some(PairState { phase: Phase::Confirming { peer, .. }, .. }) if !peer.is_empty() => {
                        // 暂存的 peer 作废
                        self.discard_staged(&peer).await;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    async fn on_pair_done(self: &Arc<Self>, peer: String, device_id: Option<String>, device_token: Option<String>) {
        // 谁的 pair_confirm 在等：接入会话的话，收尾交给接入引擎（不动进行中的配对状态）
        let owner = self.confirm_owner.lock().unwrap().take();
        if let Some(ConfirmOwner::Enroll(id)) = owner {
            if device_token.is_some() || device_id.is_some() {
                if let Err(e) = self.identity.set_device(device_id.as_deref(), device_token.as_deref()) {
                    tracing::error!(error = %e, "failed to persist the device identity");
                }
                if let Some(id) = &device_id {
                    self.outbox.set_device_id(id);
                }
            }
            self.enroll.on_pair_done(&id, peer, device_id).await;
            return;
        }
        let state = self.pairing.lock().unwrap().take();
        if device_token.is_some() || device_id.is_some() {
            // 同一条连接立刻成为正式设备：存盘并把后续信封的 from 换成正式 id，不需要重连
            if let Err(e) = self.identity.set_device(device_id.as_deref(), device_token.as_deref()) {
                tracing::error!(error = %e, "failed to persist the device identity");
            }
            if let Some(id) = &device_id {
                self.outbox.set_device_id(id);
            }
            tracing::info!("received the permanent device identity from the hub");
        }
        let Some(PairState { phase: Phase::Confirming { peer: tentative, transport }, caller, .. }) = state else {
            return;
        };
        // PairDone.peer 是对端的正式 id；握手期间用的是临时 id：把登记改到正式 id 下
        if let Err(e) = self.promote_staged(&tentative, &peer).await {
            // 没登记成功就不发 pair.ack：新设备拿不到确认、不会落盘，用户重新配对即可
            tracing::error!(error = %e.message, "cannot save the paired device; not confirming the pairing");
            return;
        }
        // 经刚建立的 Noise 传输发 pair.ack：joiner 校验通过才会落盘节点
        if let Some(mut t) = transport {
            let my_id =
                device_id.or_else(|| self.outbox.device_id()).or_else(|| self.identity.device_id()).unwrap_or_default();
            let ack = PairAck {
                node_id: my_id,
                peer_id: peer.clone(),
                name: self.config.get().node_name(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                fingerprint: anycon_crypto::fingerprint(&self.identity.public()),
            };
            self.send_pair_ack(&peer, &mut t, ack);
        } else {
            tracing::error!("pairing completed without a transport; the joiner will not receive pair.ack");
        }
        let name = self.record(&peer).map(|r| r.name).unwrap_or_default();
        self.audit.record(&caller, "pair.completed", Some(&peer), serde_json::json!({ "name": name })).await;
        tracing::info!(peer = %peer, name = %name, "pairing completed");
    }

    /// 把 `pair.ack` 作为第一条 `data` 消息发给新设备（配对用的 XXpsk3 传输，只用这一次）。
    fn send_pair_ack(&self, to: &str, transport: &mut Transport, ack: PairAck) {
        let msg = RpcMessage::notification(methods::PAIR_ACK, ack);
        let ct = match transport.encrypt(&msg.encode()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "cannot encrypt pair.ack");
                return;
            }
        };
        for body in frame::encode(&ct, 0) {
            if self.outbox.send_envelope(to, Kind::Data, body).is_none() {
                tracing::error!("hub not connected; pair.ack was not sent");
                return;
            }
        }
    }

    /// 如果这封 handshake 信封属于进行中的配对，处理并返回 true。
    async fn try_pairing_handshake(self: &Arc<Self>, from: &str, body: &[u8]) -> bool {
        enum Outcome {
            NotMine,
            Reply(Vec<Vec<u8>>),
            Done { replies: Vec<Vec<u8>>, remote: [u8; 32], payload: Vec<u8>, generation: u64 },
            Failed,
        }
        let outcome = {
            let mut p = self.pairing.lock().unwrap();
            enum Step {
                NotMine,
                Reply(Vec<Vec<u8>>),
                Finished { replies: Vec<Vec<u8>>, payload: Vec<u8> },
                Failed,
            }
            let step = match p.as_mut() {
                Some(PairState { phase: Phase::Handshaking { peer, hs }, updated, .. }) if peer == from => {
                    *updated = Instant::now();
                    let mut last_payload = Vec::new();
                    let mut replies = Vec::new();
                    // 第 2 条消息带节点信息 `{"name","version"}`（仅作显示；第 2 条不受 PSK 保护，joiner 不能信它）
                    let node_info = serde_json::to_vec(&serde_json::json!({ "name": self.config.get().node_name(), "version": env!("CARGO_PKG_VERSION") })).unwrap_or_default();
                    let r: Result<(), anycon_crypto::Error> = (|| {
                        last_payload = hs.read(body)?;
                        while !hs.is_finished() && hs.is_my_turn() {
                            replies.push(hs.write(&node_info)?);
                        }
                        Ok(())
                    })();
                    match r {
                        Err(e) => {
                            tracing::warn!(peer = %from, error = %e, "pairing handshake failed (wrong code or corrupted message)");
                            Step::Failed
                        }
                        Ok(()) if hs.is_finished() => Step::Finished { replies, payload: last_payload },
                        Ok(()) => Step::Reply(replies),
                    }
                }
                _ => Step::NotMine,
            };
            match step {
                Step::NotMine => Outcome::NotMine,
                Step::Reply(r) => Outcome::Reply(r),
                Step::Failed => Outcome::Failed,
                Step::Finished { replies, payload } => {
                    // 握手完成：取出 Handshake，换成 Confirming 并带上 Transport（pair.ack 要用）
                    let ps = p.as_mut().expect("pairing state present");
                    let Phase::Handshaking { hs, .. } =
                        std::mem::replace(&mut ps.phase, Phase::Confirming { peer: from.to_string(), transport: None })
                    else {
                        unreachable!("phase was Handshaking")
                    };
                    let hs: Handshake = *hs;
                    match (hs.remote_public(), hs.into_transport()) {
                        (Some(remote), Ok(t)) => {
                            ps.phase = Phase::Confirming { peer: from.to_string(), transport: Some(t) };
                            ps.updated = Instant::now();
                            Outcome::Done { replies, remote, payload, generation: ps.generation }
                        }
                        (_, r) => {
                            if let Err(e) = r {
                                tracing::warn!(error = %e, "cannot finalize the pairing transport");
                            }
                            Outcome::Failed
                        }
                    }
                }
            }
        };
        match outcome {
            Outcome::NotMine => false,
            Outcome::Reply(replies) => {
                for r in replies {
                    self.outbox.send_envelope(from, Kind::Handshake, r);
                }
                true
            }
            Outcome::Failed => {
                *self.pairing.lock().unwrap() = None;
                true
            }
            Outcome::Done { replies, remote, payload, generation } => {
                for r in replies {
                    self.outbox.send_envelope(from, Kind::Handshake, r);
                }
                let (name, kind) = parse_client_info(&payload, from);
                // 先登记再确认：对端收到 PairDone 后立刻发起 IK，不能因为我们这边还没处理 PairDone 而被拒。
                // 登记在临时 id 下，标成暂存：`node.devices` 看不到它，直到 `PairDone` 把它改键成正式 id
                self.staged.lock().unwrap().insert(from.to_string());
                #[cfg(any(test, feature = "testing"))]
                self.pair_save_gate().await;
                let saved = self
                    .save_record(PeerRow {
                        device_id: from.to_string(),
                        pubkey: remote,
                        name,
                        kind,
                        paired_at_ms: now_ms(),
                        last_seen_ms: None,
                    })
                    .await;
                if let Err(e) = saved {
                    tracing::error!(error = %e.message, "cannot register the new device; abandoning this pairing");
                    self.staged.lock().unwrap().remove(from);
                    *self.pairing.lock().unwrap() = None;
                    return true;
                }
                // 写库期间（`.await`）看门狗、hub 断线、新的 `pair_begin`、`PairFailed` 可能已经把这次配对作废、
                // 并且对暂存的 peer 做过 `discard_staged`——那时记录还没写进去，删了个空；现在它写进去了，
                // 却不在 `staged` 里，会以临时 id 出现在 `node.devices`。所以写完要核对：这次配对还是
                // 同一代、还停在对这个 peer 的确认阶段，否则立刻丢弃。
                let still_ours = {
                    let mut p = self.pairing.lock().unwrap();
                    match p.as_mut() {
                        Some(ps)
                            if ps.generation == generation
                                && matches!(&ps.phase, Phase::Confirming { peer, .. } if peer == from) =>
                        {
                            ps.updated = Instant::now();
                            true
                        }
                        _ => false,
                    }
                };
                if !still_ours {
                    tracing::info!("the pairing was cancelled while the new device was being saved; discarding it");
                    self.discard_staged(from).await;
                    return true;
                }
                // 确认槽被接入会话占着（它的 pair_done 还没到）：回执分不清是谁的，这一次配对放弃，新设备重新来一次即可
                if !self.claim_confirm(ConfirmOwner::Pairing) {
                    tracing::warn!("an enrollment is finishing at the same time; abandoning this pairing");
                    *self.pairing.lock().unwrap() = None;
                    self.discard_staged(from).await;
                    return true;
                }
                self.outbox.send_control(&Control::PairConfirm { peer: from.to_string() });
                true
            }
        }
    }

    // ------------------------------------------------------------ 账号内接入的收尾（ADR-36）

    /// 占住确认槽（发 `pair_confirm` 之前）。槽被别人占着返回 `false`。
    pub(crate) fn claim_confirm(&self, owner: ConfirmOwner) -> bool {
        let mut slot = self.confirm_owner.lock().unwrap();
        match &*slot {
            None => {
                *slot = Some(owner);
                true
            }
            Some(cur) => *cur == owner,
        }
    }

    /// 放掉确认槽（只有占着的是 `owner` 才放）。
    pub(crate) fn release_confirm(&self, owner: &ConfirmOwner) {
        let mut slot = self.confirm_owner.lock().unwrap();
        if slot.as_ref() == Some(owner) {
            *slot = None;
        }
    }

    /// 握手成功、等 `pair_done` 的接入设备：以临时 id 登记（新设备标成暂存，不出现在 `node.devices` 里；
    /// 老设备重新接入时它的记录本来就在，不标暂存）。返回暂存之前同 id 的老记录，失败时用它恢复。
    pub(crate) async fn enroll_stage(&self, id: &str, row: PeerRow) -> Result<Option<PeerRow>, RpcError> {
        let previous = self.records.lock().unwrap().get(id).cloned();
        if previous.is_none() {
            self.staged.lock().unwrap().insert(id.to_string());
        }
        if let Err(e) = self.save_record(row).await {
            if previous.is_none() {
                self.staged.lock().unwrap().remove(id);
            }
            return Err(e);
        }
        Ok(previous)
    }

    /// 接入失败 / 作废：新设备的暂存记录整个删掉；老设备恢复成原来的记录。
    pub(crate) async fn enroll_unstage(&self, id: &str, previous: Option<PeerRow>) {
        match previous {
            Some(prev) => {
                if let Err(e) = self.save_record(prev).await {
                    tracing::warn!(error = %e.message, "cannot restore the device record after a failed enrollment");
                }
            }
            None => self.discard_staged(id).await,
        }
    }

    /// `pair_done`：暂存的记录从临时 id 改键到正式 id（两者相同就只是不再暂存），从此才算「已配对」。
    pub(crate) async fn promote_staged(&self, tentative: &str, formal: &str) -> Result<(), RpcError> {
        if !tentative.is_empty() && tentative != formal {
            let moved = self.records.lock().unwrap().remove(tentative);
            self.staged.lock().unwrap().remove(tentative);
            if let Some(mut row) = moved {
                if let Err(e) = self.store.delete_peer(tentative).await {
                    tracing::warn!(error = %e, "cannot remove the tentative peer row");
                }
                row.device_id = formal.to_string();
                self.save_record(row).await?;
            }
        } else {
            // 临时 id 就是正式 id：登记已经在位，从这一刻起它才算「已配对」
            self.staged.lock().unwrap().remove(tentative);
        }
        Ok(())
    }

    /// 经接入握手的传输向新设备发 `pair.ack`（内容和配对的一样）。
    pub(crate) fn send_enroll_ack(&self, peer: &str, transport: &mut Transport, my_device_id: Option<String>) {
        let my_id =
            my_device_id.or_else(|| self.outbox.device_id()).or_else(|| self.identity.device_id()).unwrap_or_default();
        let ack = PairAck {
            node_id: my_id,
            peer_id: peer.to_string(),
            name: self.config.get().node_name(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            fingerprint: anycon_crypto::fingerprint(&self.identity.public()),
        };
        self.send_pair_ack(peer, transport, ack);
    }
}

fn read_peers_json(paths: &Paths) -> Vec<PeerRow> {
    let Ok(text) = std::fs::read_to_string(paths.peers_json()) else { return vec![] };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return vec![] };
    let mut out = Vec::new();
    for p in v.get("peers").and_then(Value::as_array).cloned().unwrap_or_default() {
        let (Some(id), Some(key)) =
            (p.get("device_id").and_then(Value::as_str), p.get("pubkey").and_then(Value::as_str))
        else {
            continue;
        };
        let Ok(bytes) = hex::decode(key) else { continue };
        let Ok(pubkey) = <[u8; 32]>::try_from(bytes.as_slice()) else { continue };
        out.push(PeerRow {
            device_id: id.to_string(),
            pubkey,
            name: p.get("name").and_then(Value::as_str).unwrap_or("device").to_string(),
            kind: p.get("kind").and_then(|k| serde_json::from_value(k.clone()).ok()).unwrap_or(DeviceKind::Web),
            paired_at_ms: p.get("paired_at_ms").and_then(Value::as_u64).unwrap_or(0),
            last_seen_ms: None,
        });
    }
    out
}

/// 通知：发给所有已配对设备（hub 只转发）。
impl NotifyOut for Peers {
    fn send(&self, body: NotifyBody) {
        let Ok(bytes) = rmp_serde::to_vec_named(&body) else { return };
        for id in self.paired_ids() {
            self.outbox.send_envelope(&id, Kind::Notify, bytes.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{eventually, SimHub};
    use anycon_protocol::{Envelope, Event, SessionEvent};

    fn ev(seq: u64, sid: &str, e: Event) -> RpcMessage {
        RpcMessage::notification(
            methods::SESSION_EVENT,
            SessionEvent { session_id: sid.into(), seq, ts_ms: seq, turn_id: Some(1), event: e },
        )
    }

    fn texts(q: &PeerQueue) -> Vec<(u64, String)> {
        q.inner
            .lock()
            .unwrap()
            .items
            .iter()
            .map(|m| match m {
                RpcMessage::Notification(n) => {
                    (n.params["seq"].as_u64().unwrap(), n.params["event"]["text"].as_str().unwrap_or("").to_string())
                }
                _ => (0, String::new()),
            })
            .collect()
    }

    #[test]
    fn no_coalescing_when_not_paused() {
        let q = PeerQueue::default();
        q.push(ev(1, "s", Event::TextDelta { text: "a".into() }));
        q.push(ev(2, "s", Event::TextDelta { text: "b".into() }));
        assert_eq!(texts(&q), vec![(1, "a".into()), (2, "b".into())]);
    }

    #[test]
    fn coalesces_adjacent_deltas_when_paused() {
        let q = PeerQueue::default();
        q.set_coalesce(true);
        q.push(ev(1, "s", Event::TextDelta { text: "a".into() }));
        q.push(ev(2, "s", Event::TextDelta { text: "b".into() }));
        q.push(ev(3, "s", Event::TextDelta { text: "c".into() }));
        assert_eq!(texts(&q), vec![(3, "abc".into())]);
        // 不同类型、不同会话、非 delta 事件都不合并，且不丢
        q.push(ev(4, "s", Event::ThinkingDelta { text: "t1".into() }));
        q.push(ev(5, "s", Event::ThinkingDelta { text: "t2".into() }));
        q.push(ev(6, "other", Event::ThinkingDelta { text: "x".into() }));
        q.push(ev(7, "other", Event::TextDone {}));
        q.push(ev(8, "other", Event::TextDelta { text: "y".into() }));
        let t = texts(&q);
        assert_eq!(t.len(), 5, "{t:?}");
        assert_eq!(t[1], (5, "t1t2".into()));
        // tool_output_delta 只在同一个 tool id 内合并
        let q = PeerQueue::default();
        q.set_coalesce(true);
        q.push(ev(1, "s", Event::ToolOutputDelta { id: "t1".into(), text: "x".into() }));
        q.push(ev(2, "s", Event::ToolOutputDelta { id: "t1".into(), text: "y".into() }));
        q.push(ev(3, "s", Event::ToolOutputDelta { id: "t2".into(), text: "z".into() }));
        assert_eq!(texts(&q), vec![(2, "xy".into()), (3, "z".into())]);
        // 响应和别的通知从不合并
        q.push(RpcMessage::result(anycon_protocol::RpcId::Num(1), serde_json::json!({})));
        q.push(RpcMessage::notification(methods::SESSION_CHANGED, serde_json::json!({})));
        assert_eq!(q.len(), 4);
        q.drop_events();
        assert_eq!(q.len(), 2);
    }

    /// 两个已握手的 Transport：(daemon 侧, 客户端侧)。
    fn transport_pair() -> (Transport, Transport) {
        use anycon_crypto::Keypair;
        let (daemon, client) = (Keypair::generate(), Keypair::generate());
        let mut ini = Handshake::reconnect(Role::Initiator, &client, Some(&daemon.public)).unwrap();
        let mut res = Handshake::reconnect(Role::Responder, &daemon, Some(&client.public)).unwrap();
        let m1 = ini.write(&[]).unwrap();
        res.read(&m1).unwrap();
        let m2 = res.write(&[]).unwrap();
        ini.read(&m2).unwrap();
        (res.into_transport().unwrap(), ini.into_transport().unwrap())
    }

    fn new_session(transport: Transport) -> Arc<PeerSession> {
        PeerSession::new(&"c".repeat(32), transport)
    }

    /// 客户端侧：拆框架头（整条）并解密。
    fn open(client: &mut Transport, body: &[u8]) -> Vec<u8> {
        match frame::decode(body).unwrap() {
            frame::DataFrame::Whole(ct) => client.decrypt(ct).unwrap(),
            other => panic!("expected a whole frame, got {other:?}"),
        }
    }

    async fn next_env(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Envelope {
        let bytes = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("envelope").unwrap();
        Envelope::decode(&bytes).unwrap()
    }

    fn event_text(plain: &[u8]) -> (u64, String) {
        match RpcMessage::decode(plain).unwrap() {
            RpcMessage::Notification(n) => {
                (n.params["seq"].as_u64().unwrap(), n.params["event"]["text"].as_str().unwrap().to_string())
            }
            other => panic!("{other:?}"),
        }
    }

    /// hub 退回 `seqs`（和真的 hub 一样：按序，每封一条回执）；同时让队列进入合并（`Peers::on_backpressure` 做的事）。
    fn bounce(outbox: &Arc<HubOutbox>, sess: &PeerSession, seqs: &[u64]) {
        for &seq in seqs {
            assert_eq!(outbox.on_backpressure(&sess.device_id, seq), Bounce::Scheduled);
        }
        sess.queue.set_coalesce(true);
    }

    #[tokio::test]
    async fn backpressure_pauses_coalesces_and_resends_the_same_ciphertext_in_ascending_order() {
        let (daemon_t, mut client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        let sess = new_session(daemon_t);
        tokio::spawn(sess.clone().writer(outbox.clone()));
        for i in 1..=3u64 {
            sess.queue.push(ev(i, "s", Event::TextDelta { text: format!("t{i}") }));
        }
        let e1 = next_env(&mut rx).await;
        let e2 = next_env(&mut rx).await;
        let e3 = next_env(&mut rx).await;
        assert_eq!((e1.kind, e1.to.as_str()), (Kind::Data, "c".repeat(32).as_str()));
        assert!(e1.seq < e2.seq && e2.seq < e3.seq);
        assert_eq!(event_text(&open(&mut client_t, &e1.body)), (1, "t1".to_string()));
        // hub 退回了 e2（目标缓冲满），粘住规则下 e3 当然也被退回。回执顺序可以是乱的：重发永远按 seq 升序
        let bounced_at = Instant::now();
        bounce(&outbox, &sess, &[e3.seq, e2.seq]);
        assert!(outbox.is_blocked(&sess.device_id));
        for i in 4..=6u64 {
            sess.queue.push(ev(i, "s", Event::TextDelta { text: format!("t{i}") }));
        }
        // 暂停期间新事件被合并成一条（也不会被发出去）；到点后先按原 seq 原密文重发 e2、e3，再发合并后的新消息
        let r2 = next_env(&mut rx).await;
        assert!(
            bounced_at.elapsed() >= Duration::from_millis(90),
            "paused before resending: {:?}",
            bounced_at.elapsed()
        );
        let r3 = next_env(&mut rx).await;
        assert_eq!((r2.seq, &r2.body), (e2.seq, &e2.body), "same seq, same ciphertext");
        assert_eq!((r3.seq, &r3.body), (e3.seq, &e3.body));
        assert_eq!(event_text(&open(&mut client_t, &r2.body)), (2, "t2".to_string()));
        assert_eq!(event_text(&open(&mut client_t, &r3.body)), (3, "t3".to_string()));
        let n = next_env(&mut rx).await;
        assert!(n.seq > e3.seq);
        assert_eq!(
            event_text(&open(&mut client_t, &n.body)),
            (6, "t4t5t6".to_string()),
            "deltas coalesced while paused; last seq kept"
        );
        // 恢复后恢复逐条发送
        sess.queue.push(ev(7, "s", Event::TextDelta { text: "t7".into() }));
        let n = next_env(&mut rx).await;
        assert_eq!(event_text(&open(&mut client_t, &n.body)), (7, "t7".to_string()));
        sess.stop();
    }

    #[tokio::test]
    async fn the_writer_fragments_big_messages_and_a_backpressure_resend_keeps_the_same_bytes() {
        let (daemon_t, mut client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        let sess = new_session(daemon_t);
        tokio::spawn(sess.clone().writer(outbox.clone()));
        // 700 KB 的一条事件 -> 4 片（每片 <= 200 KiB 密文）
        sess.queue.push(ev(1, "s", Event::TextDelta { text: "x".repeat(700_000) }));
        let mut envs = Vec::new();
        for _ in 0..4 {
            envs.push(next_env(&mut rx).await);
        }
        let mut reasm = frame::Reassembler::new();
        let mut ct = None;
        for env in &envs {
            assert_eq!(env.kind, Kind::Data);
            assert!(env.body.len() <= frame::MAX_FRAGMENT + 9, "{}", env.body.len());
            assert!(matches!(frame::decode(&env.body).unwrap(), frame::DataFrame::Fragment { count: 4, .. }));
            ct = reasm.push(frame::decode(&env.body).unwrap()).unwrap();
        }
        let plain = client_t.decrypt(&ct.expect("reassembled")).unwrap();
        let (seq, text) = event_text(&plain);
        assert_eq!((seq, text.len()), (1, 700_000));
        // 第二片起被 hub 退回（粘住：后面两片也是）：之后按升序、原样重发（同 seq、同字节）
        bounce(&outbox, &sess, &[envs[1].seq, envs[2].seq, envs[3].seq]);
        for orig in &envs[1..] {
            let again = next_env(&mut rx).await;
            assert_eq!((again.seq, &again.body), (orig.seq, &orig.body));
        }
        sess.stop();
    }

    /// 起一个对端会话 + 假 hub（目标缓冲 `cap` 字节、粘住规则）+ 读数据的「客户端」，返回它们。
    fn sim_peer(
        cap: usize,
        read_per_tick: usize,
    ) -> (Arc<PeerSession>, Arc<HubOutbox>, SimHub, Transport, frame::Reassembler) {
        let (daemon_t, client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        // 假 hub 立刻处理每一封，不需要生产环境 10 秒的保守等待（那个在 flow.rs 的单元测试里测）
        outbox.set_in_flight_guard(Duration::from_millis(200));
        let sess = new_session(daemon_t);
        let hub = SimHub::spawn(outbox.clone(), rx, &sess.device_id, cap, read_per_tick, Duration::from_millis(10));
        tokio::spawn(sess.clone().writer(outbox.clone()));
        (sess, outbox, hub, client_t, frame::Reassembler::new())
    }

    /// 从假 hub 交付的信封里取下一条完整的 RPC 消息（分片重组 + 解密）；解密失败 = 乱序 / 丢失 / 重复，直接 panic。
    async fn next_message(hub: &mut SimHub, client: &mut Transport, reasm: &mut frame::Reassembler) -> RpcMessage {
        loop {
            let env = tokio::time::timeout(Duration::from_secs(20), hub.delivered.recv())
                .await
                .expect("a delivery within 20 s")
                .expect("the sim hub is alive");
            let frame = frame::decode(&env.body).unwrap();
            assert!(!matches!(frame, frame::DataFrame::Reset), "no RESET placeholder expected (seq {})", env.seq);
            let ct = match reasm
                .push(frame)
                .unwrap_or_else(|e| panic!("fragments out of order at seq {}: {e:?}", env.seq))
            {
                Some(ct) => ct,
                None => continue,
            };
            let plain = client.decrypt(&ct).expect("the secure channel stays in sync (no gap, no duplicate)");
            return RpcMessage::decode(&plain).unwrap();
        }
    }

    #[tokio::test]
    async fn big_messages_cross_a_sticky_hub_with_a_small_buffer_in_order() {
        // 目标缓冲只有 1 MiB、读得很慢（256 KiB / 10 ms）：5 MiB 的消息一定会被退回很多次
        let (sess, outbox, mut hub, mut client_t, mut reasm) = sim_peer(1024 * 1024, 256 * 1024);
        let big = "x".repeat(5 * 1024 * 1024);
        sess.queue.push(ev(1, "s", Event::TextDelta { text: big.clone() }));
        sess.queue.push(ev(2, "s", Event::TextDelta { text: "after".into() }));
        let m = next_message(&mut hub, &mut client_t, &mut reasm).await;
        let RpcMessage::Notification(n) = m else { panic!("{m:?}") };
        assert_eq!(n.params["event"]["text"].as_str().unwrap().len(), big.len());
        let m = next_message(&mut hub, &mut client_t, &mut reasm).await;
        let RpcMessage::Notification(n) = m else { panic!("{m:?}") };
        assert_eq!(n.params["event"]["text"], "after", "the next message follows the big one, in order");
        let (bounces, forwarded) = {
            let t = hub.target.lock().unwrap();
            (t.bounces, t.forwarded.clone())
        };
        assert!(bounces > 0, "the scenario really did bounce envelopes");
        assert!(forwarded.windows(2).all(|w| w[0] < w[1]), "the target saw ascending seqs only: {forwarded:?}");
        assert!(
            eventually(Duration::from_secs(5), || async { !outbox.is_blocked(&sess.device_id) }).await,
            "the flow settles"
        );
        sess.stop();
    }

    #[tokio::test]
    async fn thirty_mib_of_traffic_through_a_sticky_hub_keeps_the_channel_in_sync() {
        let (sess, _outbox, mut hub, mut client_t, mut reasm) = sim_peer(2 * 1024 * 1024, 1024 * 1024);
        // 一个 6 MiB 的响应 + 24 条 1 MiB 的事件，一次全部排进队列
        sess.queue.push(ev(1, "s", Event::TextDelta { text: "a".repeat(6 * 1024 * 1024) }));
        for i in 2..=25u64 {
            sess.queue.push(ev(i, "s", Event::TextDelta { text: "b".repeat(1024 * 1024) }));
        }
        let mut total = 0usize;
        let mut seqs = Vec::new();
        for _ in 0..25 {
            let m = next_message(&mut hub, &mut client_t, &mut reasm).await;
            let RpcMessage::Notification(n) = m else { panic!("{m:?}") };
            total += n.params["event"]["text"].as_str().unwrap().len();
            seqs.push(n.params["seq"].as_u64().unwrap());
        }
        assert_eq!(total, 30 * 1024 * 1024);
        assert_eq!(seqs, (1..=25).collect::<Vec<_>>(), "no loss, no duplicate, no reordering");
        sess.stop();
    }

    #[tokio::test]
    async fn rate_limited_pauses_the_whole_connection_for_retry_after() {
        let (daemon_t, mut client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        let sess = new_session(daemon_t);
        tokio::spawn(sess.clone().writer(outbox.clone()));
        sess.queue.push(ev(1, "s", Event::TextDelta { text: "before".into() }));
        let e1 = next_env(&mut rx).await;
        assert_eq!(event_text(&open(&mut client_t, &e1.body)), (1, "before".to_string()));
        // hub：发送方超过了速率预算，停 400 ms
        let at = Instant::now();
        outbox.on_rate_limited(400);
        assert!(outbox.rate_pause_left() > Duration::from_millis(300));
        sess.queue.push(ev(2, "s", Event::TextDelta { text: "during".into() }));
        assert!(
            tokio::time::timeout(Duration::from_millis(250), rx.recv()).await.is_err(),
            "nothing is sent while the connection is paused"
        );
        let e2 = next_env(&mut rx).await;
        assert!(at.elapsed() >= Duration::from_millis(390), "resumed too early: {:?}", at.elapsed());
        assert_eq!(
            event_text(&open(&mut client_t, &e2.body)),
            (2, "during".to_string()),
            "and then it goes on in order"
        );
        // 一次 retry_after_ms 过大也有上限，不会把出站无限期停住
        outbox.on_rate_limited(u64::MAX);
        assert!(outbox.rate_pause_left() <= Duration::from_secs(60));
        sess.stop();
    }

    #[tokio::test]
    async fn a_bounced_seq_that_is_no_longer_retained_resets_the_peer() {
        let (daemon_t, _client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        let sess = new_session(daemon_t);
        let id = sess.device_id.clone();
        // 一个我们从没发过的 seq（等价于已被逐出）：不能重发，只能重置；占位的 RESET 用同一个 seq，让 hub 解开粘住
        let t = Instant::now();
        assert_eq!(outbox.on_backpressure(&id, 42), Bounce::Lost);
        // 占位帧已经在保留缓冲里：同一个 seq 再被退回就是普通的重发（不会成倍增加占位帧，也不会再次重置）
        assert_eq!(outbox.on_backpressure(&id, 42), Bounce::Scheduled);
        let env = next_env(&mut rx).await;
        assert_eq!((env.seq, env.kind, env.body), (42, Kind::Data, frame::reset_body()));
        assert!(t.elapsed() >= Duration::from_millis(90), "the placeholder waits out the pause like any resend");
        assert!(tokio::time::timeout(Duration::from_millis(400), rx.recv()).await.is_err(), "sent exactly once");
        sess.stop();
    }

    /// 把出站通道里已有的信封取出来（并告诉发送方它们「已经写进 WebSocket」，积压才会下降）。
    fn drain(outbox: &HubOutbox, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<Envelope> {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            outbox.wire_done(b.len());
            out.push(Envelope::decode(&b).unwrap());
        }
        out
    }

    #[tokio::test]
    async fn the_writer_waits_for_room_instead_of_evicting_what_may_still_be_in_flight() {
        let (daemon_t, mut client_t) = transport_pair();
        let outbox = HubOutbox::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        outbox.attach(&"d".repeat(32), tx);
        outbox.set_in_flight_guard(Duration::from_millis(400));
        let sess = new_session(daemon_t);
        let id = sess.device_id.clone();
        tokio::spawn(sess.clone().writer(outbox.clone()));
        // 刚刚发出去 20 MiB（批量传输中）：窗口（16 MiB）满了，而且全是可能还在路上的
        let chunk = vec![0u8; 100 * 1024];
        let filler: Vec<u64> =
            (0..200).map(|_| outbox.send_envelope(&id, Kind::Data, chunk.clone()).unwrap()).collect();
        assert_eq!(drain(&outbox, &mut rx).len(), 200);
        // 一条大消息（> 256 KiB）：不开始写，等最早的那批老过保护期腾出位置——而不是把它们逐出
        sess.queue.push(ev(1, "s", Event::TextDelta { text: "x".repeat(400_000) }));
        let t = Instant::now();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(drain(&outbox, &mut rx).is_empty(), "nothing is written while the window is full");
        assert!(filler.iter().all(|s| outbox.peer_of_seq(*s).is_some()), "and nothing in flight was evicted");
        let mut got = Vec::new();
        while got.is_empty() && t.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            got = drain(&outbox, &mut rx);
        }
        assert!(t.elapsed() >= Duration::from_millis(380), "resumed only once there was room: {:?}", t.elapsed());
        assert!(!got.is_empty() && got.iter().all(|e| e.seq > *filler.last().unwrap()));
        assert_eq!(outbox.lost_count(), 0);
        // 大消息按序到齐、能解开（nonce 没乱）
        let mut reasm = frame::Reassembler::new();
        let mut ct = None;
        let mut all = got;
        while ct.is_none() {
            if all.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
                all = drain(&outbox, &mut rx);
                continue;
            }
            let e = all.remove(0);
            ct = reasm.push(frame::decode(&e.body).unwrap()).unwrap();
        }
        let plain = client_t.decrypt(&ct.unwrap()).unwrap();
        assert_eq!(event_text(&plain).1.len(), 400_000);
        // 窗口又满了：小消息（权限请求、状态之类）不用等，大文件传输不该把交互卡住
        for _ in 0..200 {
            outbox.send_envelope(&id, Kind::Data, chunk.clone());
        }
        drain(&outbox, &mut rx);
        sess.queue.push(ev(2, "s", Event::TextDelta { text: "small".into() }));
        let t = Instant::now();
        let mut got = Vec::new();
        while got.is_empty() && t.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(5)).await;
            got = drain(&outbox, &mut rx);
        }
        assert!(!got.is_empty() && t.elapsed() < Duration::from_millis(300), "{:?}", t.elapsed());
        sess.stop();
    }

    #[tokio::test]
    async fn writer_gives_up_when_the_hub_is_gone() {
        let (daemon_t, _client_t) = transport_pair();
        let outbox = HubOutbox::new(); // 未连接
        let sess = new_session(daemon_t);
        let h = tokio::spawn(sess.clone().writer(outbox));
        sess.queue.push(ev(1, "s", Event::TextDelta { text: "x".into() }));
        tokio::time::timeout(Duration::from_secs(3), h).await.expect("writer exits").unwrap();
        assert!(
            sess.stopped.load(std::sync::atomic::Ordering::SeqCst),
            "a session whose encrypted message was lost must be dropped (peer re-handshakes)"
        );
    }

    #[test]
    fn client_info_parsing() {
        let ci = serde_json::to_vec(&ClientInfo {
            name: "Chrome · macOS".into(),
            kind: DeviceKind::Web,
            platform: "browser".into(),
            version: "1".into(),
            origin: None,
        })
        .unwrap();
        assert_eq!(parse_client_info(&ci, "abcdef0123456789"), ("Chrome · macOS".to_string(), DeviceKind::Web));
        assert_eq!(
            parse_client_info(br#"{"name":"my-laptop","kind":"cli"}"#, "abcdef0123456789"),
            ("my-laptop".to_string(), DeviceKind::Cli)
        );
        assert_eq!(parse_client_info(b"", "abcdef0123456789"), ("device-abcdef01".to_string(), DeviceKind::Web));
        assert_eq!(parse_client_info(b"not json", "abcdef0123456789").0, "device-abcdef01");
    }
}
