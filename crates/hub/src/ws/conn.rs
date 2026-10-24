//! 单条 WebSocket 连接的生命周期：认证 → 登记 → 读循环（转发 / 控制消息）→ 收尾。
//!
//! 日志纪律（docs/spec/hub.md § 10）：这里只记设备 id、信封 kind、大小、seq 和丢弃原因，
//! **绝不**记 `body` 字节（接入期间的 `handshake` 体也一样：`EnrollFrame` 一个字节都不解析）、device token、cookie，
//! 也不记配对用的 `match_id`（它是短码的函数，拿到就能离线穷举，按秘密对待）。`enroll_id` 不是秘密，可以记。
//!
//! 预览连接（ADR-39，docs/spec/hub.md § 13.4）：预览域名上带 `preview_token` 认证的连接，不是设备。它和绑定的节点之间的
//! `data` / `handshake` 从账号的预览预算里扣，不占每连接的字节预算；它发的控制消息一律忽略；令牌过期或被删就以 `4004` 关掉。
//!
//! 转发的两条铁律（docs/protocol.md § 3.1）：已配对 `(from, to)` 的 `data` / `notify` 只有「转发」和「退回
//! `backpressure`」两种结果，从不静默丢弃；被退回的 `seq` 记在一个有序集合里，集合非空时只有最小值的重发才会被尝试转发，
//! 其它 `seq` 一律退回并加入集合（[`super::sticky`]）。速率超限走同一条退回路径。

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anycon_protocol::{
    close_code, AuthFailReason, ClientInfo, Control, DeviceKind, DropReason, EnrollFailReason, Envelope, Kind,
    PairFailReason, ENVELOPE_VERSION, PROTOCOL_VERSION,
};
use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::app::AppState;
use crate::config::{Limits, Timing};
use crate::enroll;
use crate::pairing::{self, BeginError, ConfirmOutcome, Origin};
use crate::store::{Device, NewAudit};
use crate::util::{hash_token, ip_prefix, new_id, now_ms};

use super::outq::{OutQueue, OutRx, Outbound, PushError};
use super::registry::{Conn, ConnInit, PreviewMeta};
use super::sticky::{Cause, TooManyReturned, MAX_RETURNED};
use super::UpgradeCtx;

/// 只解信封头：跳过 `body`，转发路径上不复制、不解析负载。
#[derive(Deserialize)]
struct Header {
    v: u8,
    from: String,
    to: String,
    kind: Kind,
    seq: u64,
}

struct Authed {
    client: ClientInfo,
    /// `Some` = 带了有效 device token 的正式设备；`None` = 只为配对而来的临时连接（或预览连接）。
    device: Option<Device>,
    /// `Some` = 预览域名上凭有效预览令牌认证的预览连接。
    preview: Option<PreviewMeta>,
}

fn control_bytes(to: &str, seq: u64, ctl: &Control) -> Option<Bytes> {
    let body = rmp_serde::to_vec_named(ctl).ok()?;
    Envelope::new("", to, Kind::Control, seq, body).encode().ok().map(Bytes::from)
}

fn close_reason(code: u16) -> &'static str {
    match code {
        close_code::SUPERSEDED => "superseded",
        close_code::RESTARTING => "restarting",
        close_code::SLOW_CONSUMER => "slow consumer",
        close_code::REVOKED => "revoked",
        close_code::AUTH_TIMEOUT => "auth failed",
        _ => "closing",
    }
}

/// 认证阶段（尚未拆分 socket）的失败收尾。
async fn reject(socket: &mut WebSocket, reason: AuthFailReason) {
    let code = match reason {
        AuthFailReason::Revoked => close_code::REVOKED,
        AuthFailReason::Protocol => close_code::AUTH_TIMEOUT,
        _ => 1008,
    };
    if let Some(b) = control_bytes("", 1, &Control::AuthFail { reason }) {
        let _ = socket.send(Message::Binary(b)).await;
    }
    close_raw(socket, code).await;
}

async fn close_raw(socket: &mut WebSocket, code: u16) {
    let _ = socket.send(Message::Close(Some(CloseFrame { code, reason: close_reason(code).into() }))).await;
}

/// `preview_label` 是升级请求所在的预览域名里的 label：`Some` = 这是预览域名上的连接，只接受预览令牌；
/// `None` = 主域名，只接受设备 token（或没有 token 的临时连接）。
async fn authenticate(state: &Arc<AppState>, socket: &mut WebSocket, preview_label: Option<&str>) -> Option<Authed> {
    let deadline = tokio::time::Instant::now() + state.config.timing.auth_timeout;
    let frame = loop {
        match tokio::time::timeout_at(deadline, socket.recv()).await {
            Err(_) => {
                tracing::debug!("auth timeout");
                close_raw(socket, close_code::AUTH_TIMEOUT).await;
                return None;
            }
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_)))) => return None,
            Ok(Some(Ok(Message::Binary(b)))) => break b,
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => continue,
            Ok(Some(Ok(Message::Text(_)))) => {
                reject(socket, AuthFailReason::Protocol).await;
                return None;
            }
        }
    };

    let parsed = (frame.len() <= state.config.max_envelope_bytes)
        .then(|| Envelope::decode(&frame).ok())
        .flatten()
        .filter(|e| e.kind == Kind::Control && e.v == ENVELOPE_VERSION)
        .and_then(|e| e.control_body().ok());
    let Some(Control::Auth { device_token, protocol_version, client, preview_token }) = parsed else {
        reject(socket, AuthFailReason::Protocol).await;
        return None;
    };
    if protocol_version.abs_diff(PROTOCOL_VERSION) > 1 {
        reject(socket, AuthFailReason::Version).await;
        return None;
    }

    // 预览连接只在预览域名上，预览域名上也只有预览连接：不带 device_token、client.kind = web，否则 `protocol`。
    match (preview_label, preview_token) {
        (None, None) => {}
        (None, Some(_)) => {
            reject(socket, AuthFailReason::Protocol).await;
            return None;
        }
        (Some(label), token) => {
            let Some(token) = token.filter(|_| device_token.is_none() && client.kind == DeviceKind::Web) else {
                reject(socket, AuthFailReason::Protocol).await;
                return None;
            };
            // 令牌不存在、过期、或绑定的不是主机名里的 label：一律 `invalid`（不说是哪一种）。
            let found = match state.store.find_preview_token(&hash_token(&token)).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "preview token lookup failed during auth");
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame { code: 1011, reason: "internal error".into() })))
                        .await;
                    return None;
                }
            };
            let Some(t) = found.filter(|t| t.label == label && t.expires_at_ms > now_ms() && token.len() <= 256) else {
                reject(socket, AuthFailReason::Invalid).await;
                return None;
            };
            let meta = PreviewMeta {
                account_id: t.account_id,
                node_id: t.node_id,
                label: t.label,
                device_id: t.device_id,
                token_hash: t.token_hash,
                expires_at_ms: t.expires_at_ms,
            };
            return Some(Authed { client, device: None, preview: Some(meta) });
        }
    }

    let device = match device_token {
        None => None,
        Some(token) => match state.store.find_device_by_token_hash(&hash_token(&token)).await {
            Ok(Some(d)) if d.revoked_at_ms.is_none() => Some(d),
            Ok(Some(_)) => {
                reject(socket, AuthFailReason::Revoked).await;
                return None;
            }
            Ok(None) => {
                reject(socket, AuthFailReason::Invalid).await;
                return None;
            }
            Err(e) => {
                tracing::error!(error = %e, "device lookup failed during auth");
                let _ =
                    socket.send(Message::Close(Some(CloseFrame { code: 1011, reason: "internal error".into() }))).await;
                return None;
            }
        },
    };
    Some(Authed { client, device, preview: None })
}

pub async fn run_socket(state: Arc<AppState>, mut socket: WebSocket, ctx: UpgradeCtx) {
    // 连接建立各阶段的耗时（等 auth 消息 + 查设备、查配对关系），记在下面的 "device connected" 日志里，分析连接慢时用
    let upgraded = Instant::now();
    let Some(authed) = authenticate(&state, &mut socket, ctx.preview_label.as_deref()).await else { return };
    let auth_ms = upgraded.elapsed().as_millis() as u64;
    let timing = state.config.timing.clone();

    let preview = authed.preview;
    let (device_id, account_id, permanent, peers) = match (&preview, &authed.device) {
        // 预览连接：临时 id，唯一的对端是令牌绑定的节点。
        (Some(meta), _) => (new_id(), Some(meta.account_id.clone()), false, HashSet::from([meta.node_id.clone()])),
        (None, Some(d)) => match state.store.peers_of(&d.id).await {
            Ok(p) => (d.id.clone(), Some(d.account_id.clone()), true, p.into_iter().collect::<HashSet<_>>()),
            Err(e) => {
                tracing::error!(error = %e, "failed to load peers");
                let _ =
                    socket.send(Message::Close(Some(CloseFrame { code: 1011, reason: "internal error".into() }))).await;
                return;
            }
        },
        (None, None) => (new_id(), ctx.cookie_account.clone(), false, HashSet::new()),
    };
    let peers_ms = upgraded.elapsed().as_millis() as u64 - auth_ms;

    let (out, out_rx) = OutQueue::new(state.config.outbound_buffer_bytes);
    let conn = Arc::new(Conn::new(ConnInit {
        id: state.registry.next_conn_id(),
        ip: ctx.ip,
        client: authed.client,
        out,
        device_id: device_id.clone(),
        account_id,
        permanent,
        peers,
        preview,
    }));
    let (sink, mut stream) = socket.split();
    let mut writer = tokio::spawn(write_loop(sink, out_rx, conn.clone()));
    let mut writer_done = false;

    // 登记并发 AuthOk（持锁期间入队，保证它是第一条）。
    let auth_ok = |peers_online| Control::AuthOk {
        device_id: device_id.clone(),
        server_time_ms: now_ms(),
        peers_online,
        quota: None,
    };
    if let Some(meta) = &conn.preview {
        // 预览连接：账号的预览连接数检查和登记是同一把锁里的一步（并发接入也不会超）。节点在线时它会收到 `preview_peer`，
        // 节点收到的是 `preview_peer` 而不是 `peer_online`（`Registry::register_preview`）。
        let max = state.config.limits.preview_max_connections as usize;
        if state.registry.register_preview(&conn, max, auth_ok).is_err() {
            tracing::info!(limit = "preview_connections", "quota exceeded");
            conn.send_control(&Control::AuthFail { reason: AuthFailReason::Quota });
            conn.close(1008, "closing");
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut writer).await;
            writer.abort();
            return;
        }
        tracing::info!(peer = %device_id, node = %meta.node_id, auth_ms, "preview connected");
    } else {
        let reg = state.registry.register(&conn, auth_ok);
        if let Some(old) = reg.superseded {
            tracing::info!(device = %device_id, "superseding previous connection");
            old.close(close_code::SUPERSEDED, "superseded");
        }
        for peer in &reg.peers_online {
            if let Some(c) = state.registry.get(peer) {
                c.send_control(&Control::PeerOnline { device: device_id.clone() });
            }
        }
        if permanent {
            // 写库放到后台：AuthOk 已经发出，对端马上会发握手，在这里等写库会让读循环晚开始
            let (st, id) = (state.clone(), device_id.clone());
            tokio::spawn(async move {
                if let Err(e) = st.store.touch_device_seen(&id, now_ms()).await {
                    tracing::warn!(error = %e, "failed to update last_seen");
                }
            });
            tracing::info!(
                device = %device_id,
                kind = ?conn.client.kind,
                peers_online = reg.peers_online.len(),
                auth_ms,
                peers_ms,
                "device connected"
            );
        } else {
            tracing::debug!(temp_id = %device_id, "temporary connection");
        }
    }
    if state.shutdown.is_cancelled() {
        conn.close(close_code::RESTARTING, "restarting");
    }

    let mut cs = ConnState::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let started = Instant::now();
    let mut last_rx = started;
    let mut last_ping = started;
    // 对端发来 Close 后，回一个 Close 再收尾；最多等 2 秒。
    let mut closing: Option<Instant> = None;
    let mut preview_ticks = 0u64;
    let mut preview_ended = false;

    loop {
        tokio::select! {
            _ = &mut writer => { writer_done = true; break; }
            msg = stream.next() => match msg {
                None | Some(Err(_)) => break,
                Some(Ok(m)) => {
                    last_rx = Instant::now();
                    match m {
                        _ if closing.is_some() => {}
                        Message::Binary(b) => on_binary(&state, &conn, &mut cs, b).await,
                        Message::Close(_) => {
                            conn.close(1000, "closing");
                            closing = Some(Instant::now());
                        }
                        // Text 帧不属于协议，忽略；Ping / Pong 只用来证明对端还活着。
                        Message::Text(_) | Message::Ping(_) | Message::Pong(_) => {}
                    }
                }
            },
            _ = tick.tick() => {
                if closing.is_some_and(|t| t.elapsed() >= Duration::from_secs(2)) {
                    break;
                }
                if last_rx.elapsed() >= timing.pong_timeout {
                    tracing::debug!(device = %conn.device_id(), "no pong, dropping connection");
                    break;
                }
                // 不读 socket 的连接：发送缓冲（数据或控制消息，不分种类）持续满超过阈值就踢掉。
                if conn.out.full_for().is_some_and(|d| d >= timing.slow_consumer_timeout) {
                    tracing::warn!(device = %conn.device_id(), "slow consumer, closing");
                    conn.kill_with(close_code::SLOW_CONSUMER);
                    break;
                }
                // 临时连接（没有 device token）只为配对而来，不能无限期挂着占注册表和文件描述符。
                // 预览连接不受这个限制：它活到令牌过期为止。
                if !conn.is_permanent() && conn.preview.is_none() && started.elapsed() >= timing.temp_conn_ttl {
                    tracing::info!(temp_id = %conn.device_id(), "temporary connection expired, closing");
                    conn.kill_with(close_code::AUTH_TIMEOUT);
                    break;
                }
                // 预览令牌过期或被删：以 4004 关掉，预览页把它当终态。过期在内存里比；被删（设备 / 节点被吊销时
                // 已经主动关过一次，这里兜底别的删法和并发接入）每 5 秒查一次库。
                if let Some(meta) = &conn.preview {
                    preview_ticks += 1;
                    if !preview_ended && token_gone(&state, meta, preview_ticks).await {
                        tracing::info!(peer = %conn.device_id(), "preview token expired or removed, closing");
                        conn.close(close_code::REVOKED, "revoked");
                        preview_ended = true;
                    }
                }
                if last_ping.elapsed() >= timing.ping_interval {
                    conn.out.ping();
                    last_ping = Instant::now();
                }
            }
        }
    }

    // ---- 收尾 ----
    if !writer_done {
        if conn.kill.is_cancelled() {
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut writer).await;
        }
        writer.abort();
    }
    let id = conn.device_id();
    let removed = state.registry.unregister(&id, conn.id);
    state.broker.conn_closed(&conn);
    state.enrolls.conn_closed(&conn);
    if conn.is_permanent() {
        if let Err(e) = state.store.touch_device_seen(&id, now_ms()).await {
            tracing::warn!(error = %e, "failed to update last_seen");
        }
        if removed {
            for peer in conn.peers_snapshot() {
                if let Some(c) = state.registry.get(&peer) {
                    c.send_control(&Control::PeerOffline { device: id.clone() });
                }
            }
        }
        tracing::info!(device = %id, "device disconnected");
    } else if conn.preview.is_some() {
        // 节点那边的 `peer_offline` 在 `Registry::unregister` 里、持着路由表的锁发出。
        tracing::info!(peer = %id, "preview disconnected");
    } else {
        tracing::debug!(temp_id = %id, "temporary connection closed");
    }
}

/// 预览连接的令牌是不是已经过期或被删了。过期在内存里比（每秒）；库里还在不在，每 5 次 tick 查一次。
async fn token_gone(state: &AppState, meta: &PreviewMeta, tick: u64) -> bool {
    let now = now_ms();
    if now >= meta.expires_at_ms {
        return true;
    }
    if !tick.is_multiple_of(5) {
        return false;
    }
    match state.store.preview_token_valid(&meta.token_hash, now).await {
        Ok(valid) => !valid,
        Err(e) => {
            // 库暂时读不了不该把所有预览都断掉。
            tracing::warn!(error = %e, "failed to re-check a preview token");
            false
        }
    }
}

/// 写任务：把发送队列写到 socket。队列里的 Close 或 `kill` 都会让它收尾退出。
async fn write_loop(mut sink: SplitSink<WebSocket, Message>, mut rx: OutRx, conn: Arc<Conn>) {
    loop {
        let item = tokio::select! {
            biased;
            _ = conn.kill.cancelled() => {
                best_effort_close(&mut sink, conn.kill_code()).await;
                return;
            }
            item = rx.recv() => match item {
                Some(i) => i,
                None => return,
            },
        };
        let msg = match &item.msg {
            Outbound::Frame(b) => Message::Binary(b.clone()),
            Outbound::Ping => Message::Ping(Bytes::new()),
            Outbound::Close(code, _) => {
                best_effort_close(&mut sink, *code).await;
                return;
            }
        };
        tokio::select! {
            biased;
            _ = conn.kill.cancelled() => {
                best_effort_close(&mut sink, conn.kill_code()).await;
                return;
            }
            r = sink.send(msg) => {
                if r.is_err() {
                    return;
                }
            }
        }
        rx.done(&item);
    }
}

async fn best_effort_close(sink: &mut SplitSink<WebSocket, Message>, code: u16) {
    let frame = Message::Close(Some(CloseFrame { code, reason: close_reason(code).into() }));
    let _ = tokio::time::timeout(Duration::from_secs(1), sink.send(frame)).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
}

// ---------------------------------------------------------------------------
// 入站帧
// ---------------------------------------------------------------------------

/// 连接循环里的可变状态。
struct ConnState {
    limiter: ConnLimiter,
    receipts: ReceiptGate,
    /// 这条连接（临时连接时）已经发过几次 `PairBegin`。
    temp_pair_begins: u32,
    /// 持续被限速退回、一直没有任何进展，已经够久：这条连接该关了（处理完当前帧后由 [`on_binary`] 执行）。
    rate_violation: bool,
    /// hub 已经因为发送方违规（持续超限、被退回的信封太多）发起了关闭：之后读到的帧都不再处理。
    hub_closed: bool,
}

impl ConnState {
    fn new() -> Self {
        Self {
            limiter: ConnLimiter::new(),
            receipts: ReceiptGate::default(),
            temp_pair_begins: 0,
            rate_violation: false,
            hub_closed: false,
        }
    }

    /// 发一条「回执」：`Dropped`、`RateLimited`、限速的 `PairFailed` / `EnrollFailed`。所有种类共用一个预算，每秒至多一条，
    /// 其余悄悄丢掉——否则一个不读 socket 的发送方可以用一连串被丢弃的信封，让 hub 为它堆出无限多的回执。
    ///
    /// `Undeliverable` / `Backpressure` 不走这里：客户端靠它们给每一个 seq 判定结果，限成每秒一条会让别的请求只能等超时。
    /// 它们被连接的发送缓冲上限兜底（见 [`super::outq`]）。
    fn receipt(&mut self, conn: &Conn, ctl: Control) {
        if self.receipts.permit() {
            conn.send_control(&ctl);
        }
    }

    /// 这个帧超出了速率预算（`limiter` 已判定，`retry_after_ms` 是估算的恢复时间）：记入「持续被限速退回」，
    /// 再告诉发送方这次要停多久（`rate_limited`，每秒至多一条）。
    fn on_over_limit(&mut self, conn: &Conn, now: Instant, retry_after_ms: u64, timing: &Timing) {
        self.note_limited(now, timing);
        self.receipt(conn, Control::RateLimited { retry_after_ms, to: None });
    }

    /// 账号的预览预算不够转发 `to` 方向的这一封：告诉发送方只暂停发往 `to` 的信封（`rate_limited { to }`，
    /// 和别的回执共用每秒一条的预算）。**不**计入「持续被限速退回」：预览预算由账号下所有预览共用，
    /// 一个大流量的预览不该把 daemon 的连接整个断掉（docs/spec/hub.md § 13.5）。
    fn on_preview_over_budget(&mut self, conn: &Conn, to: &str, wait: Duration) {
        let retry_after_ms = (wait.as_secs_f64() * 1000.0).ceil() as u64;
        let retry_after_ms = retry_after_ms.clamp(MIN_RETRY_AFTER_MS, MAX_RETRY_AFTER_MS);
        self.receipt(conn, Control::RateLimited { retry_after_ms, to: Some(to.to_string()) });
    }

    /// 一个帧因为发送方自己超速而被退回（或者丢弃，控制消息）：「持续超限」的计时。
    ///
    /// 持续的意思是**一直被限速退回、中间没有任何进展**（没有一封信被转发）。照着 `retry_after_ms` 等够再发的发送方
    /// 每个周期都会有信封通过，计时随之清零，所以被匀速限流的老实发送方不会被断开；无视暂停、一直狂发的发送方在粘住之后
    /// 一封都过不去，10 秒就断开。目标太慢造成的退回（缓冲满）不是发送方的错，不算。
    fn note_limited(&mut self, now: Instant, timing: &Timing) {
        if self.limiter.note_limited(now, timing.rate_violation_close_after) {
            self.rate_violation = true;
        }
    }
}

/// 回执限频：距离上一条回执不到 1 秒就不放行。
#[derive(Default)]
struct ReceiptGate {
    last: Option<Instant>,
}

impl ReceiptGate {
    fn permit(&mut self) -> bool {
        self.permit_at(Instant::now())
    }

    fn permit_at(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1)) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

/// 这条连接适用哪个入站字节预算：已经有 device token 的 daemon 用 daemon 预算，其余（web / cli / sdk，
/// 以及还没有身份的临时连接，哪怕它自称 daemon）都按客户端算。
fn byte_budget(conn: &Conn, limits: &Limits) -> u64 {
    if conn.is_permanent() && conn.client.kind == DeviceKind::Daemon {
        limits.daemon_bytes_per_min
    } else {
        limits.client_bytes_per_min
    }
}

/// 这个帧是不是预览流量：预览连接发出的任何帧，或者节点发给它名下预览连接的 `data` / `handshake`。
/// 预览流量的字节从账号的预览预算里扣，不占发送连接自己的字节预算（docs/spec/hub.md § 13.5）。
fn is_preview_flow(state: &AppState, conn: &Conn, hdr: Option<&Header>) -> bool {
    if conn.preview.is_some() {
        return true;
    }
    let Some(h) = hdr.filter(|h| matches!(h.kind, Kind::Data | Kind::Handshake)) else { return false };
    let me = conn.device_id();
    state.registry.get(&h.to).is_some_and(|t| t.preview.as_ref().is_some_and(|m| m.node_id == me))
}

async fn on_binary(state: &Arc<AppState>, conn: &Arc<Conn>, cs: &mut ConnState, frame: Bytes) {
    if cs.hub_closed {
        return;
    }
    let now = Instant::now();
    let limits = &state.config.limits;
    // 先只解信封头（解不出来的帧照样计入信封速率）。
    let hdr = rmp_serde::from_slice::<Header>(&frame).ok();
    // 超限的帧不再被丢弃：`over` 带着「估计多久后额度恢复」，下面按帧的种类决定怎么退回（forward）或丢弃（control）。
    // 预览流量只算信封数；它的字节另有账号的预览预算（forward 里检查和扣除）。
    let over = if is_preview_flow(state, conn, hdr.as_ref()) {
        cs.limiter.check_envelope_rate(now, limits.conn_envelopes_per_sec)
    } else {
        cs.limiter.check(now, frame.len(), byte_budget(conn, limits), limits.conn_envelopes_per_sec)
    };
    if let Some(retry_after_ms) = over {
        cs.on_over_limit(conn, now, retry_after_ms, &state.config.timing);
    }
    on_frame(state, conn, cs, frame, hdr, over.is_some(), now).await;
    if cs.rate_violation && !cs.hub_closed {
        tracing::warn!(device = %conn.device_id(), "closing connection: sustained rate limit violation");
        conn.close(1008, "rate limited");
        cs.hub_closed = true;
    }
}

async fn on_frame(
    state: &Arc<AppState>,
    conn: &Arc<Conn>,
    cs: &mut ConnState,
    frame: Bytes,
    hdr: Option<Header>,
    over: bool,
    now: Instant,
) {
    // 超过 2 倍上限的帧到不了这里：底层 WebSocket 的 max_message_size 是 2 × 上限，更大的帧直接让连接出错断开。
    // 1–2 倍之间的帧完整收下，丢弃并告诉发送方（只需要信封头里的 seq）。
    let max = state.config.max_envelope_bytes;
    let Some(hdr) = hdr else {
        tracing::debug!(device = %conn.device_id(), len = frame.len(), "drop: malformed envelope");
        return;
    };
    if hdr.v != ENVELOPE_VERSION {
        tracing::debug!(device = %conn.device_id(), v = hdr.v, "drop: unsupported envelope version");
        return;
    }
    if frame.len() > max {
        tracing::debug!(device = %conn.device_id(), len = frame.len(), max, kind = ?hdr.kind, "drop: envelope too large");
        if hdr.kind != Kind::Control {
            cs.receipt(conn, Control::Dropped { seq: hdr.seq, reason: DropReason::TooLarge });
        }
        return;
    }
    match hdr.kind {
        Kind::Control => {
            // 预览连接不是设备，不能发任何控制消息：连解都不解（docs/spec/hub.md § 13.4）。
            if conn.preview.is_some() {
                tracing::debug!(peer = %conn.device_id(), "ignore control message from a preview connection");
                return;
            }
            // 控制消息没有「退回」可言（`to` 是空的）：超限就丢，和之前一样；发送方靠 `rate_limited` 知道要慢下来。
            if over {
                tracing::debug!(device = %conn.device_id(), len = frame.len(), "drop: control message over the rate limit");
                return;
            }
            cs.limiter.charge(frame.len());
            let ctl = Envelope::decode(&frame).ok().and_then(|e| e.control_body().ok());
            match ctl {
                Some(ctl) => on_control(state, conn, cs, ctl).await,
                None => tracing::debug!(device = %conn.device_id(), "drop: malformed control message"),
            }
        }
        Kind::Handshake | Kind::Data | Kind::Notify => forward(state, conn, cs, &hdr, frame, over, now),
    }
}

/// 预览流量的账号预算（`account` 为 `Some` 时）够不够转发 `len` 字节：不够返回估算的恢复时间。
fn preview_over_budget(state: &AppState, account: Option<&String>, len: usize) -> Option<Duration> {
    state.preview_budget.check(account?, len).err()
}

/// 退回一封没转发的信封：`backpressure { to, seq }`，不限频。
///
/// 这条通知要是丢了，发送方就永远不知道那封信没到，后面的顺序就乱了。控制消息队列满到装不下它，说明发送方已经
/// 很久没读 socket 了（和 4003 慢消费者同一回事）：不等 30 秒，现在就断开，让它重连后重新握手——
/// 断开是看得见的失败，静默丢失不是。
fn give_back(conn: &Conn, to: &str, seq: u64) {
    if conn.try_send_control(&Control::Backpressure { to: to.to_string(), seq }).is_err() {
        tracing::warn!(device = %conn.device_id(), "cannot report backpressure: control queue is full, closing as slow consumer");
        conn.kill_with(close_code::SLOW_CONSUMER);
    }
}

/// 退回一封 data / notify 并把它的 `seq` 记进粘住集合（`cause` 见 [`super::registry::Registry::stick`]）。
///
/// 集合满了（[`MAX_RETURNED`] 项）说明发送方无视回执一直在发新的，不守规则：不再退回、不再记，
/// 以 `1008` 断开它，之后读到的帧都不处理。
fn bounce(state: &AppState, conn: &Conn, cs: &mut ConnState, target: &Conn, to: &str, seq: u64, cause: Option<Cause>) {
    match state.registry.stick(conn.id, target, to, seq, cause) {
        Ok(()) => give_back(conn, to, seq),
        Err(TooManyReturned) => {
            tracing::warn!(
                device = %conn.device_id(),
                to,
                returned = MAX_RETURNED,
                "closing connection: too many returned envelopes outstanding"
            );
            conn.close(1008, "too many returned envelopes");
            cs.hub_closed = true;
        }
    }
}

/// 转发 `data` / `handshake` / `notify`：校验 from、配对关系、大小（已在上面检查），原样转发字节。
///
/// - 被丢弃（from 不符、未配对）时给发送方一条 `Dropped { not_paired }`（限频，见 [`ConnState::receipt`]）；
///   这是发送方的 bug，不是流控。
/// - 已配对 `(from, to)` 的 `data` / `notify` 只有两种结果：入队转发，或退回 `backpressure`（目标缓冲满 / 发送方超速）。
///   被退回的 `seq` 记在一个有序集合里（**粘住**）：集合非空时，只有 `seq` 等于集合最小值的信封才尝试转发
///   （成功就从集合去掉），其它任何 `seq`（更大的、重复到达的更小的）一律退回并加入集合；
///   所以回执晚到、发送方先重发了后面的 `seq`，也不会有更大的 `seq` 抢在前面（[`super::sticky`]）。
/// - `handshake` 不进这套顺序：不受集合影响，也不加入集合——超速或目标缓冲满时照旧退回 `backpressure`，否则转发。
///   转发成功的 `handshake`（**任一方向**：传输被重置后发送方不会再重发旧传输的信封，被退回的那封永远不会来）
///   清掉这一对设备**两个方向**的粘住状态，不清就永久卡死。
fn forward(
    state: &Arc<AppState>,
    conn: &Arc<Conn>,
    cs: &mut ConnState,
    hdr: &Header,
    frame: Bytes,
    over: bool,
    now: Instant,
) {
    let me = conn.device_id();
    if hdr.from != me {
        tracing::debug!(device = %me, kind = ?hdr.kind, len = frame.len(), "drop: from does not match connection");
        cs.receipt(conn, Control::Dropped { seq: hdr.seq, reason: DropReason::NotPaired });
        return;
    }
    // 预览连接只和它的节点互发 `handshake` / `data`；`notify` 没有意义（预览连接不是设备，也不会触发推送 / 邮件）。
    if conn.preview.is_some() && hdr.kind == Kind::Notify {
        tracing::debug!(peer = %me, "drop: notify from a preview connection");
        cs.receipt(conn, Control::Dropped { seq: hdr.seq, reason: DropReason::NotPaired });
        return;
    }
    // 握手信封：配对撮合或账号内接入会话进行中的两端之间放行（只看设备 id，不看 body）。
    let matched =
        hdr.kind == Kind::Handshake && (state.broker.is_matched(&me, &hdr.to) || state.enrolls.is_active(&me, &hdr.to));
    if !(conn.has_peer(&hdr.to) || matched) {
        tracing::debug!(device = %me, to = %hdr.to, kind = ?hdr.kind, len = frame.len(), "drop: not paired");
        cs.receipt(conn, Control::Dropped { seq: hdr.seq, reason: DropReason::NotPaired });
        return;
    }
    if matched {
        state.broker.touch(&me, &hdr.to);
    }
    let Some(target) = state.registry.get(&hdr.to) else {
        tracing::debug!(device = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, "target offline");
        if hdr.kind == Kind::Notify {
            // 收件设备不在线：触发 Web Push / 邮件（后台判定，不阻塞转发；在线的设备经 WebSocket 收到信封，不推）。
            crate::notify::on_undelivered(state, &me, &hdr.to, &frame);
        }
        conn.send_control(&Control::Undeliverable { to: hdr.to.clone(), seq: hdr.seq });
        return;
    };
    // 预览流量（预览连接 ↔ 它的节点）：字节从账号的预览预算里扣，不占发送连接自己的字节预算。
    let preview_account = conn.preview.as_ref().or(target.preview.as_ref()).map(|m| m.account_id.clone());
    if hdr.kind == Kind::Notify && target.preview.is_some() {
        tracing::debug!(from = %me, "drop: notify to a preview connection");
        cs.receipt(conn, Control::Dropped { seq: hdr.seq, reason: DropReason::NotPaired });
        return;
    }
    let len = frame.len();
    let sticky = state.registry.sticky();
    // 转发成功之后才扣额度（被退回的信封不占额度）。
    let charge = |cs: &mut ConnState| match &preview_account {
        Some(account) => state.preview_budget.charge(account, len),
        None => cs.limiter.charge(len),
    };

    if hdr.kind == Kind::Handshake {
        if over {
            tracing::debug!(from = %me, to = %hdr.to, seq = hdr.seq, len, "backpressure: sender over its rate budget (handshake)");
            give_back(conn, &hdr.to, hdr.seq);
            return;
        }
        if let Some(wait) = preview_over_budget(state, preview_account.as_ref(), len) {
            tracing::debug!(from = %me, to = %hdr.to, seq = hdr.seq, len, "backpressure: preview budget used up (handshake)");
            cs.on_preview_over_budget(conn, &hdr.to, wait);
            give_back(conn, &hdr.to, hdr.seq);
            return;
        }
        match target.push_frame(frame) {
            Ok(()) => {
                charge(cs);
                cs.limiter.note_progress();
                // 这一对设备的传输在重新握手：旧传输上两个方向的顺序约束都作废
                sticky.clear(conn.id, &hdr.to);
                sticky.clear(target.id, &me);
                tracing::trace!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, len, "forwarded");
            }
            Err(PushError::Full) => {
                tracing::debug!(from = %me, to = %hdr.to, seq = hdr.seq, len, "backpressure: target buffer full (handshake)");
                give_back(conn, &hdr.to, hdr.seq);
            }
            Err(PushError::Closed) => {
                conn.send_control(&Control::Undeliverable { to: hdr.to.clone(), seq: hdr.seq });
            }
        }
        return;
    }

    // data / notify
    let gate = sticky.gate(conn.id, &hdr.to, target.id);
    if over {
        tracing::debug!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, len, "backpressure: sender over its rate budget");
        bounce(state, conn, cs, &target, &hdr.to, hdr.seq, Some(Cause::RateLimited));
        return;
    }
    if let Some(g) = gate.filter(|g| g.min != hdr.seq) {
        tracing::debug!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, min = g.min, len, "backpressure: sticky");
        if g.cause == Cause::RateLimited {
            // 因为它超速而挂起的顺序：它还在发别的、没有重发被退回的那封。
            cs.note_limited(now, &state.config.timing);
        }
        bounce(state, conn, cs, &target, &hdr.to, hdr.seq, None);
        return;
    }
    // 预览预算用完：退回并粘住（和缓冲满一样不是发送方自己的错，不计入「持续被限速退回」）。
    if let Some(wait) = preview_over_budget(state, preview_account.as_ref(), len) {
        tracing::debug!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, len, "backpressure: preview budget used up");
        cs.on_preview_over_budget(conn, &hdr.to, wait);
        bounce(state, conn, cs, &target, &hdr.to, hdr.seq, Some(Cause::PreviewBudget));
        return;
    }
    match target.push_frame(frame) {
        Ok(()) => {
            charge(cs);
            cs.limiter.note_progress();
            if gate.is_some() {
                // 集合的最小值重发成功：从集合去掉，下一个最小值轮到它。
                let left = sticky.release(conn.id, &hdr.to, hdr.seq);
                tracing::debug!(from = %me, to = %hdr.to, seq = hdr.seq, left, "sticky released");
            }
            tracing::trace!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, len, "forwarded");
        }
        Err(PushError::Full) => {
            tracing::debug!(from = %me, to = %hdr.to, kind = ?hdr.kind, seq = hdr.seq, len, "backpressure: target buffer full");
            bounce(state, conn, cs, &target, &hdr.to, hdr.seq, Some(Cause::BufferFull));
        }
        Err(PushError::Closed) => {
            sticky.clear(conn.id, &hdr.to);
            conn.send_control(&Control::Undeliverable { to: hdr.to.clone(), seq: hdr.seq });
        }
    }
}

async fn on_control(state: &Arc<AppState>, conn: &Arc<Conn>, cs: &mut ConnState, ctl: Control) {
    match ctl {
        Control::PairBegin { match_id } => {
            // 正式设备都可以；临时连接只有 daemon 可以（daemon 第一次连 hub 还没有 token，要靠这次配对拿到身份），
            // 而且一条临时连接一生最多发 `temp_pair_begins_per_conn` 次。
            if !conn.is_permanent() {
                if conn.client.kind != DeviceKind::Daemon {
                    tracing::debug!(kind = ?conn.client.kind, "ignore pair_begin from unauthenticated non-daemon connection");
                    return;
                }
                if cs.temp_pair_begins >= state.config.limits.temp_pair_begins_per_conn {
                    tracing::info!(limit = "temp_pair_begin", "rate limited");
                    cs.receipt(conn, Control::PairFailed { reason: PairFailReason::RateLimited });
                    return;
                }
                cs.temp_pair_begins += 1;
            }
            // 这里和 broker 里都不要记 match_id。
            match state.broker.begin(conn, &match_id) {
                Ok(expires_at_ms) => conn.send_control(&Control::PairRegistered { expires_at_ms }),
                Err(BeginError::Malformed) => {
                    tracing::debug!(device = %conn.device_id(), "pair_begin rejected: malformed match id");
                    conn.send_control(&Control::PairFailed { reason: PairFailReason::NotFound });
                }
            }
        }
        Control::PairJoin { match_id } => {
            if state.limits.pair_join.check(conn.ip).is_err() {
                tracing::info!(limit = "pair_join", "rate limited");
                cs.receipt(conn, Control::PairFailed { reason: PairFailReason::RateLimited });
                return;
            }
            if let Err(reason) = state.broker.join(conn, &match_id) {
                conn.send_control(&Control::PairFailed { reason });
            }
        }
        Control::PairConfirm { peer } => match state.broker.confirm(conn, &peer) {
            ConfirmOutcome::Ready { host, joiner } => pairing::finalize(state, host, joiner, Origin::Pair).await,
            ConfirmOutcome::Waiting => {}
            // 不是配对撮合里的：看是不是账号内接入会话的确认（daemon 是 host，发起方是 joiner）。
            ConfirmOutcome::Unknown => {
                if let enroll::ConfirmOutcome::Ready(done) = state.enrolls.confirm(conn, &peer) {
                    let origin = Origin::Enroll { enroll_id: done.id, account: done.account };
                    pairing::finalize(state, done.daemon, done.initiator, origin).await;
                }
            }
        },
        Control::EnrollBegin { node } => on_enroll_begin(state, conn, cs, node).await,
        Control::EnrollCancel { enroll_id } => {
            if !state.enrolls.cancel(conn, &enroll_id) {
                // 没有这个会话（已经结束、过期，或者根本不是你的）：告诉它，让客户端不用再等。
                // 只回显长得像 enroll_id 的值，免得把任意长的字符串原样弹回去。
                let echoed =
                    (enroll_id.len() == 32 && enroll_id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(enroll_id);
                cs.receipt(conn, Control::EnrollFailed { enroll_id: echoed, reason: EnrollFailReason::NotFound });
            }
        }
        Control::Revoke { device } => {
            if !conn.is_permanent() || !conn.has_peer(&device) {
                tracing::debug!("ignore revoke: not a paired device");
                return;
            }
            let same_account = match state.store.get_device(&device).await {
                Ok(Some(d)) => conn.account_id().as_deref() == Some(d.account_id.as_str()),
                Ok(None) => false,
                Err(e) => {
                    tracing::error!(error = %e, "revoke lookup failed");
                    false
                }
            };
            if !same_account {
                return;
            }
            let actor = conn.device_id();
            if let Err(e) = state.revoke_device(&device, Some(&actor)).await {
                tracing::error!(error = %e, "revoke failed");
            }
        }
        // 认证后再发 auth、hub→设备方向的消息、v0.2 才有的功能：忽略。
        other => tracing::debug!(device = %conn.device_id(), msg = ?control_name(&other), "ignore control message"),
    }
}

/// `EnrollBegin { node }`（ADR-36，docs/spec/hub.md § 4.1）：校验归属和在线、限速，建接入会话。
///
/// 顺序：IP 限速 → 发起方是不是有账号的非 daemon → 账号限速 / 每日配额 → `node` 是不是本账号未吊销的 daemon →
/// 是否在线 → 并发上限。限速失败用共享的回执预算（每秒一条），其余失败直接回 `enroll_failed`。
/// 这里只确认「账号 + 节点」，能不能成为这台 daemon 的设备由 daemon 验证，hub 不看任何握手内容。
async fn on_enroll_begin(state: &Arc<AppState>, conn: &Arc<Conn>, cs: &mut ConnState, node: String) {
    let fail = |reason: EnrollFailReason| conn.send_control(&Control::EnrollFailed { enroll_id: None, reason });
    let rate_limited = |cs: &mut ConnState| {
        cs.receipt(conn, Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::RateLimited })
    };
    let internal = |what: &str, e: crate::store::StoreError| {
        tracing::error!(error = %e, "enroll_begin {what} failed");
        conn.send_control(&Control::EnrollFailed { enroll_id: None, reason: EnrollFailReason::Timeout });
    };

    if state.limits.enroll_begin.check(conn.ip).is_err() {
        tracing::info!(limit = "enroll_begin_ip", "rate limited");
        return rate_limited(cs);
    }
    // 账号：正式设备的账号，或升级请求带的 web 登录 cookie（和 `pair_join` 取账号是同一个来源）。daemon 不能发。
    let Some(account) = conn.account_id() else {
        tracing::debug!(device = %conn.device_id(), "enroll_begin refused: no account");
        return fail(EnrollFailReason::Forbidden);
    };
    if conn.client.kind == DeviceKind::Daemon {
        tracing::debug!(device = %conn.device_id(), "enroll_begin refused: daemon connection");
        return fail(EnrollFailReason::Forbidden);
    }
    if conn.is_permanent() {
        // 临时连接的 kind 是自报的；正式设备以库里的为准。
        match state.store.get_device(&conn.device_id()).await {
            Ok(Some(d)) if d.kind != DeviceKind::Daemon && d.revoked_at_ms.is_none() => {}
            Ok(_) => return fail(EnrollFailReason::Forbidden),
            Err(e) => return internal("initiator lookup", e),
        }
    }
    if state.account_limits.enroll_begin.check(&account).is_err() {
        tracing::info!(limit = "enroll_begin_account", "rate limited");
        return rate_limited(cs);
    }
    if state.account_limits.daily_pairings.as_ref().is_some_and(|q| q.check(&account).is_err()) {
        tracing::info!(limit = "daily_pairings", "quota exceeded");
        return fail(EnrollFailReason::Quota);
    }
    // 目标：本账号未吊销的 daemon（不区分「不存在」和「不是你的」）。
    match state.store.get_device(&node).await {
        Ok(Some(d)) if d.account_id == account && d.kind == DeviceKind::Daemon && d.revoked_at_ms.is_none() => {}
        Ok(_) => {
            tracing::debug!(device = %conn.device_id(), "enroll_begin refused: not an account daemon");
            return fail(EnrollFailReason::NotFound);
        }
        Err(e) => return internal("node lookup", e),
    }
    let Some(daemon) = state.registry.get(&node) else {
        tracing::debug!(node = %node, "enroll_begin refused: node offline");
        return fail(EnrollFailReason::Offline);
    };
    let started = match state.enrolls.begin(&account, conn, &daemon) {
        Ok(s) => s,
        Err(e) => {
            tracing::info!(node = %node, reason = ?e, "enroll_begin refused");
            return fail(e.into());
        }
    };
    tracing::info!(enroll = %started.id, node = %node, "enrollment started");
    state
        .audit(NewAudit {
            event: "enroll_started",
            account_id: Some(account),
            device_id: conn.is_permanent().then(|| conn.device_id()),
            ip_prefix: Some(ip_prefix(conn.ip)),
            user_agent: None,
            detail: Some(serde_json::json!({ "node": node, "enroll_id": started.id })),
        })
        .await;
}

fn control_name(c: &Control) -> &'static str {
    match c {
        Control::Auth { .. } => "auth",
        Control::QueueDrop { .. } => "queue_drop",
        Control::RotateToken {} => "rotate_token",
        _ => "hub_to_device",
    }
}

// ---------------------------------------------------------------------------
// 每连接限速（docs/spec/hub.md § 3.4）：500 信封/秒，加按连接种类的字节预算（客户端 10 MiB/分钟、daemon 100 MiB/分钟），
// 只为防止一个设备拖垮 hub。超限**不丢信封**：data / notify 退回 backpressure 并粘住，发送方暂停后重发。
// ---------------------------------------------------------------------------

/// `rate_limited { retry_after_ms }` 的下限：再短发送方也来不及反应。
const MIN_RETRY_AFTER_MS: u64 = 100;
/// 上限：客户端把这个提示封顶在 30 秒（它的语义是**整条连接**暂停，包括重发），hub 给的值不超过它。
/// 估算值超过 30 秒（预算被设得很低）时，发送方等满 30 秒再试，要是额度还没回来会再收到一个提示。
const MAX_RETRY_AFTER_MS: u64 = 30_000;
/// 两次被限速退回之间隔得比这个还久，就不算同一段「持续」了。
const VIOLATION_GAP: Duration = Duration::from_millis(1500);

/// 一段持续被限速退回的起止。
#[derive(Clone, Copy)]
struct Streak {
    start: Instant,
    last: Instant,
}

/// 入站预算：每秒信封数（固定的 1 秒窗口）+ 字节令牌桶（容量 = 每分钟字节数，匀速补充）。
///
/// - 令牌桶只在信封**真的转发（或控制消息被处理）之后**扣额度（[`ConnLimiter::charge`]）：被退回的信封没占用任何额度，
///   发送方重发它时不会被重复计费，等够 `retry_after_ms` 就一定放得过。信封数则是「来了几个」都算，
///   这样狂发的连接即使一封都过不了也依然受限。
/// - 超限时 [`ConnLimiter::check`] 返回估算的恢复时间（至少 100 ms）。
/// - 「持续超限」（[`ConnLimiter::note_limited`] / [`ConnLimiter::note_progress`]）：一直被限速退回、中间没有任何信封被转发，
///   持续够久才断开；有任何进展就重新计。
struct ConnLimiter {
    sec_start: Instant,
    sec_count: u32,
    /// 令牌桶：剩余字节。开始是满的（`INFINITY` 在第一次补充时被夹到容量）。
    tokens: f64,
    refilled_at: Instant,
    /// 上一次 `check` 用的容量，`charge` 要用。
    capacity: f64,
    streak: Option<Streak>,
}

impl ConnLimiter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            sec_start: now,
            sec_count: 0,
            tokens: f64::INFINITY,
            refilled_at: now,
            capacity: f64::INFINITY,
            streak: None,
        }
    }

    /// 来了一个帧（`len` 字节）。算一次信封数；返回 `Some(retry_after_ms)` 表示它超出预算（信封数或字节），
    /// `None` 表示放行。`retry_after_ms` 是额度恢复所需时间的估算，夹在 100 ms 到 30 s 之间
    /// （发送方据此暂停**整条连接**，所以不会大到让客户端的封顶生效）。这里不扣字节额度，转发成功后由 [`ConnLimiter::charge`] 扣。
    fn check(&mut self, now: Instant, len: usize, bytes_per_min: u64, envelopes_per_sec: u32) -> Option<u64> {
        self.count_envelope(now);

        let capacity = bytes_per_min as f64;
        let rate_per_sec = capacity / 60.0;
        let elapsed = now.saturating_duration_since(self.refilled_at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate_per_sec).min(capacity);
        self.refilled_at = now;
        self.capacity = capacity;

        // 比整个桶还大的信封（预算被设得比信封还小）：桶满时放行一个，不会永远过不去。
        let need = (len as f64).min(capacity);
        let over_rate = self.sec_count > envelopes_per_sec;
        let over_bytes = self.tokens < need;
        if !(over_rate || over_bytes) {
            return None;
        }
        let mut wait_ms = 0f64;
        if over_rate {
            let left = 1000u64.saturating_sub(now.duration_since(self.sec_start).as_millis() as u64);
            wait_ms = wait_ms.max(left as f64);
        }
        if over_bytes {
            wait_ms = wait_ms.max(((need - self.tokens) / rate_per_sec * 1000.0).ceil());
        }
        Some((wait_ms as u64).clamp(MIN_RETRY_AFTER_MS, MAX_RETRY_AFTER_MS))
    }

    /// 来了一个预览流量的帧：只算信封数。字节不看——它们由账号的预览预算管（`forward` 里检查和扣除），
    /// 也不动这条连接自己的字节桶。超限时返回恢复时间，同 [`ConnLimiter::check`]。
    fn check_envelope_rate(&mut self, now: Instant, envelopes_per_sec: u32) -> Option<u64> {
        self.count_envelope(now);
        if self.sec_count <= envelopes_per_sec {
            return None;
        }
        let left = 1000u64.saturating_sub(now.duration_since(self.sec_start).as_millis() as u64);
        Some(left.clamp(MIN_RETRY_AFTER_MS, MAX_RETRY_AFTER_MS))
    }

    /// 固定 1 秒窗口里的信封计数加一。
    fn count_envelope(&mut self, now: Instant) {
        if now.duration_since(self.sec_start) >= Duration::from_secs(1) {
            self.sec_start = now;
            self.sec_count = 0;
        }
        self.sec_count = self.sec_count.saturating_add(1);
    }

    /// 一个帧被接收（转发了，或控制消息被处理）：扣字节额度。
    fn charge(&mut self, len: usize) {
        self.tokens = (self.tokens - (len as f64).min(self.capacity)).max(0.0);
    }

    /// 又有一个帧因为发送方超速被退回（或丢弃）。返回这段「一直被退回、没有进展」的时间有没有到 `close_after`。
    fn note_limited(&mut self, now: Instant, close_after: Duration) -> bool {
        let streak = match self.streak {
            Some(s) if now.saturating_duration_since(s.last) <= VIOLATION_GAP => Streak { start: s.start, last: now },
            _ => Streak { start: now, last: now },
        };
        self.streak = Some(streak);
        now.duration_since(streak.start) >= close_after
    }

    /// 有一封信被转发了：发送方在照着暂停的要求办事，「持续超限」的计时清零。
    fn note_progress(&mut self) {
        self.streak = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn test_conn(kind: DeviceKind, permanent: bool) -> Conn {
        let (out, _rx) = OutQueue::new(1024);
        let client = ClientInfo { name: "t".into(), kind, platform: "test".into(), version: "0".into(), origin: None };
        Conn::new(ConnInit {
            id: 1,
            ip: "127.0.0.1".parse().unwrap(),
            client,
            out,
            device_id: "d".into(),
            account_id: None,
            permanent,
            peers: HashSet::new(),
            preview: None,
        })
    }

    #[test]
    fn byte_budget_depends_on_the_connection_kind_and_temporary_connections_count_as_clients() {
        let limits = Limits { client_bytes_per_min: 10, daemon_bytes_per_min: 100, ..Limits::default() };
        assert_eq!(byte_budget(&test_conn(DeviceKind::Daemon, true), &limits), 100);
        for kind in [DeviceKind::Web, DeviceKind::Cli, DeviceKind::Sdk] {
            assert_eq!(byte_budget(&test_conn(kind, true), &limits), 10, "{kind:?}");
        }
        // 还没有身份的临时连接：哪怕自称 daemon 也按客户端算
        assert_eq!(byte_budget(&test_conn(DeviceKind::Daemon, false), &limits), 10);
        assert_eq!(byte_budget(&test_conn(DeviceKind::Web, false), &limits), 10);
        let d = Limits::default();
        assert_eq!((d.client_bytes_per_min, d.daemon_bytes_per_min), (10 * MIB, 100 * MIB));
    }

    #[test]
    fn limiter_allows_normal_traffic() {
        let mut l = ConnLimiter::new();
        let now = Instant::now();
        for _ in 0..100 {
            assert!(l.check(now, 1000, 10 * MIB, 500).is_none());
            l.charge(1000);
        }
    }

    #[test]
    fn retry_hints_are_between_100_ms_and_30_s_and_realistic_for_normal_budgets() {
        // 默认客户端预算（10 MiB / 分钟），桶被抽干：一个满片（约 200 KiB）要等 1–2 秒，在 100 ms..5 s 之间
        let mut l = ConnLimiter::new();
        let t0 = l.sec_start;
        assert!(l.check(t0, 10 * MIB as usize, 10 * MIB, 500).is_none());
        l.charge(10 * MIB as usize);
        let r = l.check(t0, 205_000, 10 * MIB, 500).unwrap();
        assert!((100..=5_000).contains(&r), "{r}");
        // 不管预算和信封多极端，提示都不出 100 ms..30 s
        for budget in [1u64, 1000, 205_824, MIB, 10 * MIB, 100 * MIB, u64::MAX] {
            for len in [1usize, 1000, 205_000, 262_144, 4 * MIB as usize] {
                let mut l = ConnLimiter::new();
                let t0 = l.sec_start;
                l.check(t0, len, budget, 500);
                l.charge(len * 2);
                if let Some(r) = l.check(t0, len, budget, 500) {
                    assert!((100..=30_000).contains(&r), "budget {budget} len {len}: {r}");
                }
            }
        }
    }

    #[test]
    fn limiter_reports_over_rate_with_the_time_left_in_the_second() {
        let mut l = ConnLimiter::new();
        let t0 = l.sec_start;
        for _ in 0..3 {
            assert!(l.check(t0, 10, 10 * MIB, 3).is_none());
        }
        // 第 4 个超限：恢复时间 = 这一秒还剩多少，但至少 100 ms
        let r = l.check(t0 + Duration::from_millis(200), 10, 10 * MIB, 3).unwrap();
        assert!((790..=800).contains(&r), "{r}");
        let r = l.check(t0 + Duration::from_millis(990), 10, 10 * MIB, 3).unwrap();
        assert_eq!(r, 100, "never less than 100 ms");
        // 下一秒重新开始计数
        assert!(l.check(t0 + Duration::from_millis(1000), 10, 10 * MIB, 3).is_none());
    }

    #[test]
    fn limiter_drops_over_byte_budget_and_estimates_when_it_recovers() {
        let mut l = ConnLimiter::new();
        let t0 = l.sec_start;
        // 预算 6000 字节/分钟 = 100 字节/秒，桶容量 6000
        assert!(l.check(t0, 4000, 6000, 500).is_none());
        l.charge(4000);
        // 还剩 2000，再来 4000 装不下：缺 2000 字节，补满要 20 秒
        let r = l.check(t0, 4000, 6000, 500).unwrap();
        assert_eq!(r, 20_000);
        // 被退回的信封没扣额度：等够 20 秒，重发的同一封一定放得过
        let t1 = t0 + Duration::from_millis(r);
        assert!(l.check(t1, 4000, 6000, 500).is_none());
        l.charge(4000);
        // 桶又空了：1000 字节要再等 10 秒（先来的小信封能不能过只看额度，顺序由粘住保证，不是这里的事）
        assert_eq!(l.check(t1, 1000, 6000, 500), Some(10_000));
        assert!(l.check(t1 + Duration::from_secs(10), 1000, 6000, 500).is_none());
    }

    #[test]
    fn rejected_frames_are_not_charged_so_retries_do_not_drain_the_budget() {
        let mut l = ConnLimiter::new();
        let t0 = l.sec_start;
        assert!(l.check(t0, 1000, 1200, 500).is_none());
        l.charge(1000);
        // 同一封被退回 50 次，额度一点没少
        for _ in 0..50 {
            assert!(l.check(t0, 1000, 1200, 500).is_some());
        }
        let r = l.check(t0, 1000, 1200, 500).unwrap();
        // 缺 800 字节，速率 20 字节/秒 -> 40 秒，但提示最多 30 秒
        assert_eq!(r, 30_000);
        // 等满 30 秒：还差 200 字节，再来一个 10 秒的提示；再等 10 秒就放得过
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(l.check(t1, 1000, 1200, 500), Some(10_000));
        assert!(l.check(t1 + Duration::from_secs(10), 1000, 1200, 500).is_none());
    }

    #[test]
    fn an_envelope_bigger_than_the_whole_budget_passes_when_the_bucket_is_full() {
        let mut l = ConnLimiter::new();
        let t0 = l.sec_start;
        assert!(l.check(t0, 5000, 1000, 500).is_none(), "full bucket lets one oversized envelope through");
        l.charge(5000);
        let r = l.check(t0, 5000, 1000, 500).unwrap();
        assert_eq!(r, 30_000, "a full minute to refill, but the hint is capped at 30 s");
    }

    #[test]
    fn budget_follows_the_connection_kind_it_is_asked_with() {
        // 同一个限速器：临时连接升级成 daemon 之后改用更大的预算，桶里剩的不变，之后按新速率补
        let fresh = || {
            let mut l = ConnLimiter::new();
            assert!(l.check(l.sec_start, 9 * MIB as usize, 10 * MIB, 500).is_none());
            l.charge(9 * MIB as usize);
            l
        };
        let (mut client, mut daemon) = (fresh(), fresh());
        let later = client.sec_start + Duration::from_secs(6);
        // 客户端速率：6 秒只补 1 MiB（共 2 MiB），装不下 9 MiB
        assert!(client.check(later, 9 * MIB as usize, 10 * MIB, 500).is_some());
        // daemon 速率：6 秒补 10 MiB（共 11 MiB），装得下
        assert!(daemon.check(later, 9 * MIB as usize, 100 * MIB, 500).is_none());
    }

    #[test]
    fn receipts_are_limited_to_one_per_second_across_all_kinds() {
        let mut g = ReceiptGate::default();
        let t0 = Instant::now();
        assert!(g.permit_at(t0));
        // 同一秒内无论是哪种回执都不放行
        for ms in [0, 1, 500, 999] {
            assert!(!g.permit_at(t0 + Duration::from_millis(ms)));
        }
        assert!(g.permit_at(t0 + Duration::from_millis(1000)));
        assert!(!g.permit_at(t0 + Duration::from_millis(1500)));
        assert!(g.permit_at(t0 + Duration::from_millis(2000)));
    }

    #[test]
    fn being_handed_back_without_any_progress_for_ten_seconds_closes() {
        let ten = Duration::from_secs(10);
        let mut l = ConnLimiter::new();
        let t0 = Instant::now();
        // 每 100 ms 一个、一个都过不去、一直被限速退回
        let mut closed_at = None;
        for step in 0..=120u64 {
            if l.note_limited(t0 + Duration::from_millis(step * 100), ten) {
                closed_at = Some(step * 100);
                break;
            }
        }
        assert_eq!(closed_at, Some(10_000), "closed once the streak has lasted 10 s, not before");
    }

    #[test]
    fn any_progress_or_a_long_gap_restarts_the_streak() {
        let ten = Duration::from_secs(10);
        let t0 = Instant::now();
        // 每 8 秒有一封过去：永远攒不到 10 秒
        let mut l = ConnLimiter::new();
        for round in 0..5u64 {
            let base = t0 + Duration::from_secs(round * 9);
            for step in 0..80u64 {
                assert!(!l.note_limited(base + Duration::from_millis(step * 100), ten));
            }
            l.note_progress();
            assert!(l.streak.is_none());
        }
        // 中间安静了 2 秒（超过容忍的间隔）：重新计
        let mut l = ConnLimiter::new();
        for step in 0..80u64 {
            assert!(!l.note_limited(t0 + Duration::from_millis(step * 100), ten));
        }
        let later = t0 + Duration::from_secs(10);
        assert!(!l.note_limited(later, ten));
        assert_eq!(l.streak.unwrap().start, later);
    }

    #[test]
    fn a_throttled_but_polite_sender_is_never_closed() {
        // 被匀速限流：每个周期先放过一封（照着 retry_after 等够再重发的那封），之后的又被退回，周期只有 300 ms，持续一分钟。
        let ten = Duration::from_secs(10);
        let mut l = ConnLimiter::new();
        let mut now = Instant::now();
        for _ in 0..200 {
            l.note_progress();
            for _ in 0..5 {
                // 同一批里尾巴上的几封被粘住退回
                assert!(!l.note_limited(now, ten));
                now += Duration::from_millis(2);
            }
            now += Duration::from_millis(300);
        }
    }
}
