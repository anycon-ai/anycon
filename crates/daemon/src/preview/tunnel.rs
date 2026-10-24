//! 一个预览对端（hub 告知的预览连接）上的隧道（docs/spec/daemon.md § 19.5，docs/protocol.md § 11）。
//!
//! 每个已登记的预览对端一个任务（[`PeerTask`]），从 `control.preview_peer` 到 `peer_offline`。它串行处理这个对端的一切：
//! 握手、入站数据、hub 的退回 / 离线通知、上游任务的事件、定时器。握手成功后任务里有一条 [`Tunnel`]：Noise 传输、
//! [`Ledger`]（端到端回执）、[`OutQueue`]（出站帧）、[`StreamTable`]（stream 状态机）和每个 stream 的上游任务句柄。
//!
//! 这里是 I/O 的接线；要点的逻辑都在纯模块里（`stream.rs` / `ledger.rs` / `rewrite.rs` / `ports.rs`）。
//!
//! 发送：消息先进 [`OutQueue`]，由 [`PeerTask::flush`] 在 hub 没有暂停这个对端、在途窗口（未被回执的密文 ≤ 8 MiB）有空位时
//! 装成一条消息（≤ 128 KiB 明文）、加密、交给 hub 出站通道（`send_acked`：按回执释放保留的原文）。加密发生在真正发送前一刻，
//! nonce 顺序 = 发送顺序。手里没有帧、又欠着回执时，按 § 11.5 发纯回执。
//!
//! 日志：只在 debug 级记 label 和对端 id；不记路径、头、正文、票据 id 和密钥。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_crypto::{Handshake, Transport};
use anycon_protocol::envelope::frame;
use anycon_protocol::tunnel::{self, Frame, FrameType};
use anycon_protocol::{
    Kind, PreviewGoaway, PreviewGoawayCode, PreviewHello, PreviewRejectReason, PreviewReset, PreviewResetCode,
    PreviewResponseHead,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::ledger::{AckDue, Ledger, OutQueue, STALL_LIMIT};
use super::ports::Verdict;
use super::stream::{Action, StreamTable, UpKind, UpOut};
use super::upstream::{self, Credit, Emit, StreamCtx, ToUp, UpEvent};
use super::{Preview, TunnelInfo};
use crate::util::now_ms;

/// 一次 `flush` 最多发多少条消息，然后回到事件循环（别让一个大下载饿死入站处理）。
const FLUSH_BATCH: usize = 32;
/// 处理完一条消息后，最多再顺手处理队列里已有的这么多条，然后才发送。
const BATCH_RECV: usize = 64;
/// 被 hub 暂停（退回 / 限速）时多久再看一次能不能发。
const BLOCKED_POLL: Duration = Duration::from_millis(5);
/// 有 stream 活动时多久复核一次端口策略。
pub const RECHECK_EVERY: Duration = Duration::from_secs(30);
/// 归还窗口的最小粒度：攒到这么多再发一个 `WINDOW` 帧（窗口 256 KiB 的 1/8）。
const WINDOW_BATCH: u32 = 32 * 1024;
/// GOAWAY 之后保留出站原文多久（让它有机会在被 hub 退回时重发），然后释放。
const GOAWAY_LINGER: Duration = Duration::from_secs(10);

/// 发给对端任务的消息。
pub(super) enum Msg {
    /// 预览页的握手消息（hub 信封的 body）。
    Handshake(Vec<u8>),
    /// 预览页的 `data` 信封体。
    Data(Vec<u8>),
    /// hub 退回了发给这个对端的 `seq`。
    Backpressure(u64),
    /// 对端已经断开（`undeliverable`）。
    Undeliverable,
    /// hub 丢了我们发给这个对端的一封信封（`dropped`）：传输少了一封，只能重置。
    Dropped,
    /// 通知预览页这条隧道要关了。
    Goaway(PreviewGoawayCode),
    /// 上游任务的事件。`epoch` 是它所属的隧道，隧道换了之后晚到的事件直接丢弃。
    Up { epoch: u64, id: u32, ev: UpEvent },
    /// 端口策略复核的结果。
    Policy { epoch: u64, verdict: Verdict },
    /// 对端离线 / 被顶掉 / hub 断开：收尾退出。
    Stop,
}

struct StreamHandle {
    to_up: mpsc::UnboundedSender<ToUp>,
    credit: Arc<Credit>,
    task: JoinHandle<()>,
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        self.credit.close();
        self.task.abort();
    }
}

struct Tunnel {
    epoch: u64,
    transport: Transport,
    info: TunnelInfo,
    ledger: Ledger,
    out: OutQueue,
    streams: StreamTable,
    handles: HashMap<u32, StreamHandle>,
    /// 已经被上游读走、还没还给预览页的请求体 / WebSocket 数据字节。
    pending_window: HashMap<u32, u32>,
    /// 下一次想复核端口策略的时刻；`activity` 记自上次复核以来有没有 stream 活动。
    next_recheck: Instant,
    activity: bool,
    recheck_inflight: bool,
    /// hub 暂停了这个对端 / 一次 flush 没发完时，什么时候再试。
    retry_at: Option<Instant>,
}

/// 一个预览对端的任务。
pub(super) struct PeerTask {
    pv: Arc<Preview>,
    peer: String,
    label: String,
    tx: mpsc::UnboundedSender<Msg>,
    rx: mpsc::UnboundedReceiver<Msg>,
    tunnel: Option<Tunnel>,
    epoch: u64,
    /// 当前（或最近一条）隧道的握手回复的 `seq`：比它小的被退回的 `seq` 属于上一条传输。
    start_seq: u64,
    /// GOAWAY 之后什么时候释放出站原文。
    forget_at: Option<Instant>,
}

fn goaway_frame(code: PreviewGoawayCode) -> Frame {
    let payload = tunnel::encode_head(&PreviewGoaway { code, message: None }).unwrap_or_default();
    Frame::new(FrameType::Goaway, 0, payload)
}

fn reset_frame(id: u32, code: PreviewResetCode, message: &str) -> Frame {
    let payload = tunnel::encode_head(&PreviewReset { code, message: Some(message.to_string()) }).unwrap_or_default();
    Frame::new(FrameType::Reset, id, payload)
}

impl PeerTask {
    pub(super) fn spawn(pv: Arc<Preview>, peer: String, label: String) -> (mpsc::UnboundedSender<Msg>, JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let task =
            PeerTask { pv, peer, label, tx: tx.clone(), rx, tunnel: None, epoch: 0, start_seq: 0, forget_at: None };
        (tx, tokio::spawn(task.run()))
    }

    async fn run(mut self) {
        'outer: loop {
            let deadline = self.next_deadline();
            let msg = tokio::select! {
                m = self.rx.recv() => match m {
                    Some(m) => Some(m),
                    None => break,
                },
                _ = async {
                    match deadline {
                        Some(t) => tokio::time::sleep_until(t.into()).await,
                        None => std::future::pending().await,
                    }
                } => None,
            };
            if let Some(m) = msg {
                if matches!(m, Msg::Stop) {
                    break;
                }
                self.handle(m).await;
                // 队列里已有的一并处理完再发送：上游任务连着来的几段数据装进同一条消息，不会一段一条
                for _ in 0..BATCH_RECV {
                    match self.rx.try_recv() {
                        Ok(Msg::Stop) => break 'outer,
                        Ok(m) => self.handle(m).await,
                        Err(_) => break,
                    }
                }
            }
            self.pump();
        }
        self.teardown(false);
        tracing::debug!(label = %self.label, peer = %self.peer, "preview peer closed");
    }

    // ------------------------------------------------------------ 定时器

    fn next_deadline(&self) -> Option<Instant> {
        let mut at: Option<Instant> = self.forget_at;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                at = Some(at.map_or(t, |a| a.min(t)));
            }
        };
        if let Some(t) = &self.tunnel {
            consider(t.retry_at);
            consider(t.ledger.stall_deadline(STALL_LIMIT));
            if let AckDue::At(i) = t.ledger.ack_due(Instant::now()) {
                consider(Some(i));
            }
            if !t.recheck_inflight && (t.activity || !t.streams.is_empty()) {
                consider(Some(t.next_recheck));
            }
        }
        at
    }

    /// 每次循环（处理完一条消息或定时器到点）之后：定时器、归还窗口、发送。
    fn pump(&mut self) {
        let now = Instant::now();
        if self.forget_at.is_some_and(|t| t <= now) {
            self.forget_at = None;
            self.pv.d.outbox.forget(&self.peer);
        }
        let Some(t) = self.tunnel.as_mut() else { return };
        if t.ledger.stalled(now, STALL_LIMIT) {
            tracing::debug!(label = %self.label, "preview peer stopped acknowledging; dropping the tunnel");
            self.drop_tunnel(true);
            return;
        }
        if !t.recheck_inflight && now >= t.next_recheck && (t.activity || !t.streams.is_empty()) {
            t.recheck_inflight = true;
            t.next_recheck = now + self.pv.recheck_every();
            t.activity = false;
            let (pv, tx, epoch, port) = (self.pv.clone(), self.tx.clone(), t.epoch, t.info.port);
            tokio::spawn(async move {
                let verdict = pv.check_port(port, true).await;
                let _ = tx.send(Msg::Policy { epoch, verdict });
            });
        }
        self.queue_windows();
        self.flush();
    }

    // ------------------------------------------------------------ 消息分派

    async fn handle(&mut self, m: Msg) {
        match m {
            Msg::Handshake(body) => self.on_handshake(&body).await,
            Msg::Data(body) => self.on_data(&body),
            Msg::Backpressure(seq) => self.on_backpressure(seq),
            Msg::Undeliverable => {
                // 预览页已经断开：丢掉传输，等它重新连上来握手
                tracing::debug!(label = %self.label, "preview peer is gone");
                self.drop_tunnel(false);
            }
            Msg::Dropped => self.reset_transport("the hub dropped one of our envelopes"),
            Msg::Goaway(code) => self.goaway(code),
            Msg::Up { epoch, id, ev } => {
                if self.tunnel.as_ref().is_some_and(|t| t.epoch == epoch) {
                    self.on_up(id, ev);
                }
            }
            Msg::Policy { epoch, verdict } => {
                if let Some(t) = self.tunnel.as_mut().filter(|t| t.epoch == epoch) {
                    t.recheck_inflight = false;
                    match verdict {
                        Verdict::Allowed => {}
                        Verdict::Disabled => self.goaway(PreviewGoawayCode::Disabled),
                        Verdict::NotAllowed => self.goaway(PreviewGoawayCode::PortNotAllowed),
                    }
                }
            }
            Msg::Stop => {}
        }
    }

    // ------------------------------------------------------------ 握手

    async fn on_handshake(&mut self, body: &[u8]) {
        let reply = match self.try_handshake(body).await {
            Ok(noise) => tunnel::handshake_accept(&noise),
            Err(reason) => {
                tracing::debug!(label = %self.label, reason = reason.as_str(), "preview handshake rejected");
                self.drop_tunnel(false);
                tunnel::handshake_reject(reason)
            }
        };
        let accepted = reply.first() == Some(&tunnel::HANDSHAKE_ACCEPT);
        match self.pv.d.outbox.send_envelope(&self.peer, Kind::Handshake, reply) {
            Some(seq) if accepted => {
                self.start_seq = seq;
                tracing::debug!(label = %self.label, peer = %self.peer, "preview tunnel established");
            }
            None if accepted => {
                // hub 没连上：这条隧道没有人知道
                self.drop_tunnel(false);
            }
            _ => {}
        }
    }

    /// 校验票据和端口策略，完成 Noise 握手，登记隧道。成功返回 Noise 第二条消息。
    async fn try_handshake(&mut self, body: &[u8]) -> Result<Vec<u8>, PreviewRejectReason> {
        use PreviewRejectReason as R;
        let (ticket_id, noise) = tunnel::parse_handshake_init(body).map_err(|_| R::Malformed)?;
        // 新的第一条消息顶掉旧传输和上面的全部 stream
        self.drop_tunnel(false);
        let ticket_hex = hex::encode(ticket_id);
        let ticket = match self.pv.d.store.preview_ticket_get(&ticket_hex).await {
            Ok(Some(t)) => t,
            Ok(None) => return Err(R::UnknownTicket),
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the preview ticket");
                return Err(R::UnknownTicket);
            }
        };
        if ticket.label != self.label {
            return Err(R::LabelMismatch);
        }
        if ticket.expires_at_ms <= now_ms() {
            return Err(R::Expired);
        }
        if !self.pv.d.config.get().preview.enabled {
            return Err(R::Disabled);
        }
        match self.pv.check_port(ticket.port, true).await {
            Verdict::Allowed => {}
            Verdict::Disabled => return Err(R::Disabled),
            Verdict::NotAllowed => return Err(R::PortNotAllowed),
        }
        let psk: [u8; 32] = hex::decode(&ticket.psk).ok().and_then(|b| b.try_into().ok()).ok_or(R::Malformed)?;
        let kp = self.pv.d.identity.keypair();
        let mut hs = Handshake::preview_responder(&kp, &psk, &tunnel::prologue(&self.label, &ticket_id))
            .map_err(|_| R::Malformed)?;
        // 密钥不对 / prologue 不对 / 报文损坏：读第一条就失败
        hs.read(noise).map_err(|_| R::Malformed)?;
        let hello = PreviewHello {
            port: ticket.port,
            expires_at_ms: ticket.expires_at_ms,
            session_id: ticket.session_id.clone(),
        };
        let payload = serde_json::to_vec(&hello).map_err(|_| R::Malformed)?;
        let reply = hs.write(&payload).map_err(|_| R::Malformed)?;
        let transport = hs.into_transport().map_err(|_| R::Malformed)?;

        let info = TunnelInfo {
            label: self.label.clone(),
            ticket_id: ticket_hex.clone(),
            device_id: ticket.device_id.clone(),
            port: ticket.port,
            expires_at_ms: ticket.expires_at_ms,
        };
        if !self.pv.register_tunnel(&self.peer, info.clone()) {
            return Err(R::Busy);
        }
        // 登记之后再确认一次票据还在：`preview.close` / 吊销设备在我们读票据到登记之间发生的话，它们的断开通知
        // 看不到这条隧道，靠这一步兜住
        match self.pv.d.store.preview_ticket_get(&ticket_hex).await {
            Ok(Some(_)) => {}
            _ => {
                self.pv.unregister_tunnel(&self.peer);
                return Err(R::UnknownTicket);
            }
        }
        self.epoch += 1;
        let now = Instant::now();
        self.forget_at = None;
        self.tunnel = Some(Tunnel {
            epoch: self.epoch,
            transport,
            info,
            ledger: Ledger::new(),
            out: OutQueue::new(),
            streams: StreamTable::new(),
            handles: HashMap::new(),
            pending_window: HashMap::new(),
            next_recheck: now + self.pv.recheck_every(),
            activity: false,
            recheck_inflight: false,
            retry_at: None,
        });
        Ok(reply)
    }

    // ------------------------------------------------------------ 拆除

    /// 丢掉隧道：上游任务全部 abort，登记撤销，出站的保留原文释放（还没发出去的 seq 用 RESET 顶替，hub 的粘住状态才能解开）。
    /// `send_reset`：再发一帧 `RESET` 体，让预览页重新握手。
    fn drop_tunnel(&mut self, send_reset: bool) {
        let had = self.tunnel.take().is_some();
        if had {
            self.pv.unregister_tunnel(&self.peer);
        }
        self.forget_at = None;
        self.pv.d.outbox.forget(&self.peer);
        if send_reset && had {
            self.pv.d.outbox.send_envelope(&self.peer, Kind::Data, frame::reset_body());
        }
    }

    /// 传输失步（解密失败、收到 `FRAGMENT`、消息格式不对）：丢掉传输，发 `RESET` 体。
    fn reset_transport(&mut self, why: &str) {
        tracing::debug!(label = %self.label, reason = why, "resetting the preview transport");
        self.drop_tunnel(true);
    }

    /// 任务结束时的清理。
    fn teardown(&mut self, send_reset: bool) {
        self.drop_tunnel(send_reset);
    }

    /// `GOAWAY`：告诉预览页这条隧道要关了，随后丢掉传输。消息排在已经加密的消息之后（nonce 顺序），不受在途窗口限制。
    fn goaway(&mut self, code: PreviewGoawayCode) {
        let Some(mut t) = self.tunnel.take() else { return };
        self.pv.unregister_tunnel(&self.peer);
        let sent = Self::encrypt_and_send(&self.pv, &self.peer, &mut t.transport, &mut t.ledger, &[goaway_frame(code)]);
        tracing::debug!(label = %self.label, ?code, sent, "preview tunnel closed by the daemon");
        // 上游任务随 `t` 丢弃；出站原文留一会儿，GOAWAY 万一被 hub 退回还能重发
        self.forget_at = Some(Instant::now() + GOAWAY_LINGER);
    }

    // ------------------------------------------------------------ hub 的退回

    fn on_backpressure(&mut self, seq: u64) {
        use crate::hub::Bounce;
        match self.pv.d.outbox.on_backpressure(&self.peer, seq) {
            Bounce::Scheduled => {}
            Bounce::Lost => {
                // 出站通道已经用 RESET 占住了这个 seq（hub 的粘住状态因此能解开）。属于上一条传输的残余不影响现在这条
                if self.tunnel.is_some() && seq >= self.start_seq {
                    self.reset_transport("the hub bounced an envelope that is no longer retained");
                }
            }
        }
    }

    // ------------------------------------------------------------ 入站数据

    fn on_data(&mut self, body: &[u8]) {
        if self.tunnel.is_none() {
            tracing::debug!(label = %self.label, "data without a tunnel; ignored");
            return;
        }
        let ct = match frame::decode(body) {
            Ok(frame::DataFrame::Whole(ct)) => ct,
            Ok(frame::DataFrame::Reset) => {
                // 预览页的传输失步了，它会重新握手
                tracing::debug!(label = %self.label, "the preview page asked for a transport reset");
                self.drop_tunnel(false);
                return;
            }
            // 预览隧道从不分片
            Ok(frame::DataFrame::Fragment { .. }) => {
                return self.reset_transport("fragmented data on a preview tunnel")
            }
            Err(_) => return self.reset_transport("bad data frame"),
        };
        let t = self.tunnel.as_mut().expect("checked above");
        let plain = match t.transport.decrypt(ct) {
            Ok(p) => p,
            Err(_) => return self.reset_transport("decrypt failed"),
        };
        let msg = match tunnel::decode_message(&plain) {
            Ok(m) => m,
            Err(_) => return self.reset_transport("malformed tunnel message"),
        };
        let now = Instant::now();
        match t.ledger.on_received(plain.len(), !msg.frames.is_empty(), msg.ack, now) {
            Ok(Some(seq)) => self.pv.d.outbox.release_through(&self.peer, seq),
            Ok(None) => {}
            Err(_) => return self.reset_transport("acknowledged a message that was never sent"),
        }
        for f in &msg.frames {
            let Some(t) = self.tunnel.as_mut() else { return };
            t.activity = true;
            let acts = t.streams.on_frame(f);
            self.apply(acts);
        }
    }

    /// 执行 stream 表给出的动作。
    fn apply(&mut self, acts: Vec<Action>) {
        for a in acts {
            let Some(t) = self.tunnel.as_mut() else { return };
            match a {
                Action::Open { id, head } => self.open_stream(id, head),
                Action::Body { id, data } => {
                    let n = data.len() as u32;
                    let delivered = t.handles.get(&id).is_some_and(|h| h.to_up.send(ToUp::Body(data)).is_ok());
                    if !delivered {
                        // 上游已经不读请求体了（早早答完、或者出错了）：窗口照还，预览页的上传不会卡住
                        *t.pending_window.entry(id).or_default() += n;
                    }
                }
                Action::BodyEnd { id } => {
                    if let Some(h) = t.handles.get(&id) {
                        let _ = h.to_up.send(ToUp::End);
                    }
                }
                Action::WsMessage { id, binary, data } => {
                    if let Some(h) = t.handles.get(&id) {
                        let _ = h.to_up.send(ToUp::WsMessage { binary, data });
                    }
                }
                Action::WsClose { id, code, reason } => {
                    if let Some(h) = t.handles.get(&id) {
                        let _ = h.to_up.send(ToUp::WsClose { code, reason });
                    }
                }
                Action::Credit { id, n } => {
                    let ok = t.handles.get(&id).is_none_or(|h| h.credit.add(n));
                    if !ok {
                        t.streams.remove(id);
                        self.reset_stream(id, PreviewResetCode::FlowControl, "the stream window overflowed");
                    }
                }
                Action::ReturnWindow { id, n } => {
                    *t.pending_window.entry(id).or_default() += n;
                }
                Action::Reset { id, code, message } => self.reset_stream(id, code, message),
                Action::Drop { id } => {
                    t.handles.remove(&id);
                    t.pending_window.remove(&id);
                    t.out.purge_stream(id);
                }
            }
        }
    }

    /// 给预览页发 `RESET`，丢掉这个 stream 的上游任务和还在排队的帧。
    fn reset_stream(&mut self, id: u32, code: PreviewResetCode, message: &str) {
        let Some(t) = self.tunnel.as_mut() else { return };
        t.handles.remove(&id);
        t.pending_window.remove(&id);
        t.out.purge_stream(id);
        t.out.push_urgent(reset_frame(id, code, message));
    }

    fn open_stream(&mut self, id: u32, head: anycon_protocol::PreviewRequestHead) {
        let timeouts = self.pv.timeouts();
        let Some(t) = self.tunnel.as_mut() else { return };
        let credit = Credit::new(tunnel::STREAM_WINDOW);
        let (to_up, rx) = mpsc::unbounded_channel();
        let (tx, epoch) = (self.tx.clone(), t.epoch);
        let emit: Emit = Arc::new(move |ev| tx.send(Msg::Up { epoch, id, ev }).is_ok());
        let ctx = StreamCtx {
            port: t.info.port,
            label: self.label.clone(),
            head,
            rx,
            credit: credit.clone(),
            emit,
            timeouts,
        };
        let task = tokio::spawn(upstream::run(ctx));
        t.handles.insert(id, StreamHandle { to_up, credit, task });
    }

    // ------------------------------------------------------------ 上游任务的事件

    fn on_up(&mut self, id: u32, ev: UpEvent) {
        let Some(t) = self.tunnel.as_mut() else { return };
        t.activity = true;
        match ev {
            UpEvent::Head(h) | UpEvent::WsOpen(h) => self.push_head(id, h),
            UpEvent::Data(d) => {
                if t.streams.on_up(id, UpKind::Data) != UpOut::Ignore {
                    t.out.push_bulk(Frame::new(FrameType::Data, id, d));
                }
            }
            UpEvent::End => {
                let out = t.streams.on_up(id, UpKind::End);
                if out != UpOut::Ignore {
                    t.out.push_bulk(Frame::new(FrameType::End, id, Vec::new()));
                }
                if out == UpOut::SendAndFinish {
                    self.finish_stream(id);
                }
            }
            UpEvent::Reset { code, message } => {
                if t.streams.on_up(id, UpKind::Reset) != UpOut::Ignore {
                    self.reset_stream(id, code, message);
                }
            }
            UpEvent::WsSegment { binary, fin, data } => {
                if t.streams.on_up(id, UpKind::WsMessage) != UpOut::Ignore {
                    let flags = (if binary { tunnel::WS_BINARY } else { 0 }) | (if fin { tunnel::WS_FIN } else { 0 });
                    t.out.push_bulk(Frame::new(FrameType::WsMessage, id, tunnel::ws_message(flags, &data)));
                }
            }
            UpEvent::WsClose { code, reason } => {
                let out = t.streams.on_up(id, UpKind::WsClose);
                if out != UpOut::Ignore {
                    t.out.push_bulk(Frame::new(FrameType::WsClose, id, tunnel::ws_close(code, &reason)));
                }
                if out == UpOut::SendAndFinish {
                    self.finish_stream(id);
                }
            }
            UpEvent::Consumed(n) => {
                if t.streams.contains(id) {
                    *t.pending_window.entry(id).or_default() += n;
                }
            }
        }
    }

    fn push_head(&mut self, id: u32, head: PreviewResponseHead) {
        let Some(t) = self.tunnel.as_mut() else { return };
        let payload = match tunnel::encode_head(&head) {
            Ok(p) => p,
            Err(_) => {
                // 响应头超过 64 KiB：这个 stream 到此为止
                if t.streams.on_up(id, UpKind::Reset) != UpOut::Ignore {
                    self.reset_stream(id, PreviewResetCode::TooLarge, "response head too large");
                }
                return;
            }
        };
        if t.streams.on_up(id, UpKind::Head { status: head.status }) != UpOut::Ignore {
            t.out.push_urgent(Frame::new(FrameType::Response, id, payload));
        }
    }

    /// stream 正常结束（表里已经去掉）：释放上游任务句柄。
    fn finish_stream(&mut self, id: u32) {
        if let Some(t) = self.tunnel.as_mut() {
            t.handles.remove(&id);
            t.pending_window.remove(&id);
        }
    }

    // ------------------------------------------------------------ 发送

    /// 攒够了的窗口归还变成 `WINDOW` 帧。请求体已经结束的 HTTP stream 不再需要窗口。
    fn queue_windows(&mut self) {
        let Some(t) = self.tunnel.as_mut() else { return };
        let ids: Vec<u32> = t.pending_window.iter().filter(|(_, n)| **n >= WINDOW_BATCH).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(n) = t.pending_window.remove(&id) else { continue };
            if !t.streams.contains(id) {
                continue;
            }
            t.streams.window_granted(id, n);
            t.out.push_urgent(Frame::new(FrameType::Window, id, tunnel::window(n)));
        }
        // 小额的不立刻发，但流结束了就不用惦记了
        t.pending_window.retain(|id, _| t.streams.contains(*id));
    }

    /// 加密一条消息并交给 hub 出站通道，记入账本。返回是否发出去了。
    fn encrypt_and_send(
        pv: &Preview,
        peer: &str,
        transport: &mut Transport,
        ledger: &mut Ledger,
        frames: &[Frame],
    ) -> bool {
        let Ok(plain) = tunnel::encode_message(ledger.ack(), frames) else { return false };
        let Ok(ct) = transport.encrypt(&plain) else { return false };
        let mut bodies = frame::encode(&ct, 0);
        if bodies.len() != 1 {
            // 128 KiB 明文加密后不会超过一片；超过就是 bug，不发
            tracing::error!(bytes = ct.len(), "a preview tunnel message does not fit in one data frame");
            return false;
        }
        let body = bodies.remove(0);
        let len = body.len();
        match pv.d.outbox.send_acked(peer, body) {
            Some(seq) => {
                ledger.on_sent(seq, len, frames.is_empty(), Instant::now());
                true
            }
            None => false,
        }
    }

    /// 把能发的发出去：被 hub 暂停（退回 / 限速）时不加密新消息；在途窗口满了只发纯回执。
    fn flush(&mut self) {
        let outbox = self.pv.d.outbox.clone();
        let peer = self.peer.clone();
        let Some(t) = self.tunnel.as_mut() else { return };
        t.retry_at = None;
        let mut broken = false;
        // 正常结束循环（没有 break）= 发满了一批还有东西：马上再来一轮，中间让入站消息插进来
        let mut yielded = true;
        for _ in 0..FLUSH_BATCH {
            let now = Instant::now();
            if outbox.is_blocked(&peer) {
                t.retry_at = Some(now + BLOCKED_POLL);
                yielded = false;
                break;
            }
            let ack_due = t.ledger.ack_due(now);
            if t.out.is_empty() {
                if ack_due == AckDue::Now {
                    if !Self::encrypt_and_send(&self.pv, &peer, &mut t.transport, &mut t.ledger, &[]) {
                        broken = true;
                        break;
                    }
                    continue;
                }
                yielded = false;
                break;
            }
            if !t.ledger.has_room(tunnel::MAX_MESSAGE) {
                // 在途窗口满了：等对端的回执（它到了会触发下一轮 pump）；欠着的回执不受这个限制
                if ack_due == AckDue::Now {
                    if !Self::encrypt_and_send(&self.pv, &peer, &mut t.transport, &mut t.ledger, &[]) {
                        broken = true;
                        break;
                    }
                    continue;
                }
                yielded = false;
                break;
            }
            let frames = t.out.pop_message();
            if !Self::encrypt_and_send(&self.pv, &peer, &mut t.transport, &mut t.ledger, &frames) {
                broken = true;
                break;
            }
        }
        if broken {
            // hub 没连上（或加密失败）：这条消息已经占了 nonce，对端必须重握手
            tracing::debug!(label = %self.label, "cannot send on the preview tunnel; dropping it");
            self.drop_tunnel(false);
        } else if yielded {
            t.retry_at = Some(Instant::now());
        }
    }
}
