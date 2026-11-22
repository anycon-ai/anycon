//! 单节点的一次性连接（CLI 用）：连 hub、认证、对一个节点做 IK 握手（Noise initiator），之后由后台的 [`Driver`] 驱动成
//! 一个请求 / 响应 / 通知的通道（[`NodeLink`]）。命令结束连接就关；长期在线、同时连多个节点的用 [`super::multi::Client`]。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use anycon_crypto::{Handshake, Keypair, Role, Transport};
use anycon_protocol::{ClientInfo, Control, Envelope, Kind, RpcId, RpcMessage, RpcNotification, PROTOCOL_VERSION};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use super::channel::NodeChannel;
use super::error::{ClientError, Result};
use super::hubws::{close_reason, request_rotation_if_due, save_rotated_token, HubWs};
use super::state::{sanitize_label, HubState, NodeEntry, NAME_MAX_CHARS};
use crate::flow::RatePause;
use crate::hub::WsStream;

/// 一个待发的请求：方法、参数，以及回应（节点的错误响应是 `Err`）的去处。
pub struct Req {
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<std::result::Result<Value, anycon_protocol::RpcError>>,
}

/// 连接结束的原因（驱动循环退出时写入）。
pub type ClosedReason = Arc<Mutex<Option<String>>>;

/// [`open`] 的结果：往 `tx` 发请求，从 `notes` 收通知；通道结束（节点下线、hub 断开）后原因记在 `closed` 里，
/// 未完成的请求的 `reply` 被丢弃。
pub struct NodeLink {
    pub tx: mpsc::UnboundedSender<Req>,
    pub notes: mpsc::UnboundedReceiver<RpcNotification>,
    pub closed: ClosedReason,
    /// 经过清理的节点名（本地保存的名字）。
    pub node_name: String,
}

/// 在已认证的 hub 连接上对 `node_id` 做 IK 握手，并把连接交给后台任务驱动成 [`Conn`]。
async fn ik_connect(
    mut h: HubWs,
    kp: &Keypair,
    node_id: &str,
    node_pub: &[u8; 32],
    node_was_online: bool,
) -> Result<NodeLink> {
    let mut hs =
        Handshake::reconnect(Role::Initiator, kp, Some(node_pub)).map_err(|e| ClientError::general(e.to_string()))?;
    let m1 = hs.write(&[]).map_err(|e| ClientError::general(e.to_string()))?;
    h.send_env(node_id, Kind::Handshake, m1).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    let reply = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match h.recv(left).await? {
            None => {
                // hub 说节点在线却不回握手：最常见的原因是节点已经不认这台设备的公钥了（被吊销）
                return Err(if node_was_online {
                    ClientError::not_paired(
                        "the node is online but did not accept this device; it may have been revoked — pair again with `anycon connect <hub-url> --code <code>`",
                    )
                } else {
                    ClientError::offline("the node did not answer; is the daemon running and connected to the hub?")
                });
            }
            Some(env) => match env.kind {
                Kind::Handshake if env.from == node_id => break env.body,
                Kind::Control => match env.control_body() {
                    Ok(Control::Undeliverable { to, .. }) if to == node_id => {
                        return Err(ClientError::offline("the node is offline"))
                    }
                    Ok(Control::Dropped { reason, .. }) => {
                        return Err(ClientError::general(format!("the hub dropped the handshake ({reason:?})")))
                    }
                    Ok(c) => h.controls.push(c),
                    Err(_) => {}
                },
                _ => {}
            },
        }
    };
    hs.read(&reply).map_err(|e| {
        ClientError::not_paired(format!(
            "the node did not accept this device ({e}); it may have been revoked — pair again with `anycon connect`"
        ))
    })?;
    let transport = hs.into_transport().map_err(|e| ClientError::general(e.to_string()))?;

    // 把 ws 和 transport 交给后台任务
    let HubWs { ws, seq, device_id, rotation_dir, .. } = h;
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let closed: ClosedReason = Arc::new(Mutex::new(None));
    let mut driver = Driver::new(ws, transport, device_id, node_id.to_string(), seq);
    driver.rotation_dir = rotation_dir;
    let closed_d = closed.clone();
    tokio::spawn(async move {
        let reason = driver.run(req_rx, note_tx).await;
        *closed_d.lock().unwrap() = Some(reason);
    });
    Ok(NodeLink { tx: req_tx, notes: note_rx, closed, node_name: String::new() })
}

/// 到 daemon 的加密通道的驱动：发请求（分片、发送方规则，见 [`NodeChannel`]）、收响应 / 通知（重组、解密）。
pub(crate) struct Driver {
    ws: WsStream,
    chan: NodeChannel,
    device_id: String,
    seq: u64,
    rate: RatePause,
    /// 发过 `rotate_token` 的连接：握手之后才到的 `token_rotated` 也落盘到这里（见 [`save_rotated_token`]）。
    pub(crate) rotation_dir: Option<PathBuf>,
}

impl Driver {
    pub(crate) fn new(ws: WsStream, transport: Transport, device_id: String, node_id: String, seq: u64) -> Driver {
        Driver {
            ws,
            chan: NodeChannel::new(node_id, transport),
            device_id,
            seq,
            rate: RatePause::default(),
            rotation_dir: None,
        }
    }

    async fn send_all(&mut self, frames: Vec<Vec<u8>>) -> std::result::Result<(), String> {
        for b in frames {
            self.ws.send(Message::Binary(b.into())).await.map_err(|e| format!("sending to the hub failed: {e}"))?;
        }
        Ok(())
    }

    /// 返回连接结束的原因。
    pub(crate) async fn run(
        mut self,
        mut req_rx: mpsc::UnboundedReceiver<Req>,
        note_tx: mpsc::UnboundedSender<RpcNotification>,
    ) -> String {
        let mut pending: HashMap<u64, oneshot::Sender<std::result::Result<Value, anycon_protocol::RpcError>>> =
            HashMap::new();
        let mut next = 0u64;
        let reason = loop {
            let now = StdInstant::now();
            let blocked = self.chan.blocked(&self.rate);
            let wake = self.chan.wake_at(&self.rate, now);
            tokio::select! {
                frame = self.ws.next() => {
                    let Some(frame) = frame else { break "the hub closed the connection".to_string() };
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => break format!("the hub connection failed: {e}"),
                    };
                    match frame {
                        Message::Binary(b) => {
                            let Ok(env) = Envelope::decode(&b) else { continue };
                            match env.kind {
                                Kind::Data if env.from == self.chan.node_id => {
                                    match self.chan.on_data(&env) {
                                        Err(e) => break e,
                                        Ok(Some(RpcMessage::Response(r))) => {
                                            if let RpcId::Num(id) = r.id {
                                                if let Some(tx) = pending.remove(&id) {
                                                    let _ = tx.send(match r.error { Some(e) => Err(e), None => Ok(r.result.unwrap_or(Value::Null)) });
                                                }
                                            }
                                        }
                                        Ok(Some(RpcMessage::Notification(n))) => { let _ = note_tx.send(n); }
                                        Ok(_) => {}
                                    }
                                }
                                Kind::Control => match env.control_body() {
                                    Ok(Control::Undeliverable { to, .. }) if to == self.chan.node_id => break "the node went offline".to_string(),
                                    Ok(Control::PeerOffline { device }) if device == self.chan.node_id => break "the node went offline".to_string(),
                                    Ok(Control::Backpressure { to, seq }) if to == self.chan.node_id => {
                                        if let Err(e) = self.chan.on_backpressure(seq, &self.rate) { break e; }
                                    }
                                    // 超过了速率预算：整条连接的出站暂停 retry_after_ms（被退回的信封之后按序重发）
                                    Ok(Control::RateLimited { retry_after_ms, .. }) => {
                                        self.rate.limited(StdInstant::now(), Duration::from_millis(retry_after_ms));
                                    }
                                    // hub 丢了我们的某一封（不是退回）：加密通道已经少了一条，只能重来
                                    Ok(Control::Dropped { reason, .. }) => break format!("the hub dropped one of our messages ({reason:?}); run the command again"),
                                    // 轮换的回执一般在握手期间就被 `HubWs::recv` 截下了；这里兜底
                                    Ok(Control::TokenRotated { device_token }) if self.rotation_dir.is_some() => { save_rotated_token(self.rotation_dir.as_deref(), &device_token); }
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                        Message::Ping(_) => { let _ = self.ws.flush().await; }
                        Message::Close(f) => break close_reason(f.map(|f| u16::from(f.code))),
                        _ => {}
                    }
                }
                // 暂停结束：把被退回的和排队的按升序原样发出去
                _ = async {
                    match wake {
                        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    let frames = match self.chan.take_resend(&self.device_id, &self.rate) { Ok(f) => f, Err(e) => break e };
                    if let Err(e) = self.send_all(frames).await { break e; }
                }
                req = req_rx.recv(), if !blocked => {
                    let Some(req) = req else { break "closed locally".to_string() };
                    next += 1;
                    let msg = RpcMessage::request(next, req.method, req.params);
                    let frames = match self.chan.encode_request(&msg, &self.device_id, &mut self.seq, &self.rate) {
                        Ok(f) => f,
                        Err(e) => break e,
                    };
                    if let Err(e) = self.send_all(frames).await { break e; }
                    pending.insert(next, req.reply);
                }
            }
        };
        drop(pending);
        let _ = self.ws.close(None).await;
        reason
    }
}

/// 打开到某个节点的一次性远程连接（CLI 每条命令一次）：连 hub、认证、按需轮换 token、对节点做 IK 握手，把连接交给后台任务。
pub async fn open(state: &HubState, node: &NodeEntry, client: ClientInfo) -> Result<NodeLink> {
    state.check()?;
    let token = state.token.clone().ok_or_else(|| {
        ClientError::not_paired("this CLI has no device token for the hub; run `anycon connect` first")
    })?;
    let kp = state
        .keypair()
        .ok_or_else(|| ClientError::not_paired("this CLI has no key for the hub; run `anycon connect` first"))?;
    let node_pub: [u8; 32] = hex::decode(&node.pubkey)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| ClientError::general("corrupt nodes.json"))?;
    let mut h = HubWs::connect(&state.hub_url, Some(token), client).await?;
    request_rotation_if_due(&mut h, state).await;
    let mut online = h.peers_online.contains(&node.id);
    if !online {
        // 节点刚好在重连：给一点时间等 peer_online
        online = h
            .wait_control(Duration::from_secs(2), |c| matches!(c, Control::PeerOnline { device } if device == &node.id))
            .await
            .is_ok();
    }
    let mut link = ik_connect(h, &kp, &node.id, &node_pub, online).await?;
    link.node_name = sanitize_label(&node.name, NAME_MAX_CHARS, &node.id);
    Ok(link)
}

/// 节点在线状态（hub 的 `auth_ok.peers_online`）。连不上 hub 时返回 `None`。
pub async fn online_nodes(state: &HubState, client: ClientInfo) -> Option<Vec<String>> {
    let token = state.token.clone()?;
    let h = HubWs::connect(&state.hub_url, Some(token), client).await.ok()?;
    Some(h.peers_online)
}

/// 版本检查：本地协议版本和节点的差值不能超过 1（相邻 minor 兼容，docs/protocol.md § 9）。
pub fn check_protocol(remote: u32) -> Result<()> {
    if remote.abs_diff(PROTOCOL_VERSION) > 1 {
        return Err(ClientError::protocol_mismatch(format!(
            "the node speaks protocol {remote}, this CLI speaks {PROTOCOL_VERSION}; update whichever is older"
        )));
    }
    Ok(())
}
