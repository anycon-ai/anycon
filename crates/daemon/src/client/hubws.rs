//! 到 hub 的一条已认证连接（`HubWs`）：认证、收发信封、等 control，以及设备 token 轮换的落盘。
//!
//! 设备 token 轮换（docs/spec/hub.md § 5、spec/cli.md § 2）：连上 hub 之后，token 满 30 天（或不知道签发时间）就在
//! 第一帧发 `rotate_token`，**不等回复**：回执在之后的任何一次读里被截下来并落盘（先 token、后签发时间），所以老 hub
//! （不认识这条消息、不回复）不会拖慢调用方。hub 发回执在转发别的帧之前，调用方不可能在读到它之前就读到后面的东西。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anycon_crypto::Transport;
use anycon_protocol::envelope::frame;
use anycon_protocol::{
    close_code, methods, AuthFailReason, ClientInfo, Control, Envelope, Kind, PairAck, RpcMessage, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use super::error::{ClientError, ErrorClass, Result};
use super::pairing::pair_failed;
use super::state::{write_token_files, HubState};
use crate::hub::{connect_ws_with_cookie, WsStream};

/// 等 hub 的一步最多多久（认证回复、撮合……）。
pub const STEP: Duration = Duration::from_secs(12);

// ---------------------------------------------------------------- hub 上的一条已认证连接

pub struct HubWs {
    pub(crate) ws: WsStream,
    pub(crate) seq: u64,
    pub device_id: String,
    pub controls: Vec<Control>,
    /// 等 control 的时候先到的 data / notify 信封（比如 `PairDone` 之前就到了的 `pair.ack`）。
    pub(crate) data: VecDeque<Envelope>,
    /// 配对等阻塞式等待期间收到、但不是它们要等的信封（别的节点的数据、`peer_online` 之类）：长连接的客户端事后按序处理。
    pub(crate) bystanders: VecDeque<Envelope>,
    pub peers_online: Vec<String>,
    /// hub 关连接时的 close code。
    pub(crate) close_code: Option<u16>,
    /// 设了就表示这条连接发过 `rotate_token`：`token_rotated` 回执在 [`HubWs::recv`] 里被截下来、落盘到这个状态目录。
    pub(crate) rotation_dir: Option<PathBuf>,
}

/// 认证失败的几种结局。
pub enum ConnectFail {
    Auth(AuthFailReason),
    Other(ClientError),
}

impl ConnectFail {
    /// 折成一个 [`ClientError`]（认证失败按原因给出可操作的提示）。
    pub fn into_error(self) -> ClientError {
        match self {
            ConnectFail::Auth(r) => auth_fail_error(r),
            ConnectFail::Other(e) => e,
        }
    }
}

/// hub 以 `protocol` 拒绝认证时 [`ClientError::reason`] 的值。
pub const HUB_PROTOCOL_REASON: &str = "hub_protocol";

pub(crate) fn auth_fail_error(reason: AuthFailReason) -> ClientError {
    match reason {
        AuthFailReason::Invalid | AuthFailReason::Revoked => ClientError::not_paired(format!(
            "the hub does not accept this device any more ({}); pair again with `anycon connect <hub-url> --code <code>`",
            if reason == AuthFailReason::Revoked { "revoked" } else { "unknown token" }
        )),
        AuthFailReason::Version => ClientError::protocol_mismatch(
            "the hub speaks an incompatible protocol version; update anycon (or the hub) so the versions are adjacent",
        ),
        AuthFailReason::Quota => ClientError::new(ErrorClass::General, "quota", "the hub's device quota for this account is used up"),
        // `reason = "hub_protocol"`：hub 不认这次认证（比如旧 hub 不认识设备种类）。哪种情况由调用方按上下文解释，
        // 比如建立分身连接时（`twin_link::link_error`）说成「hub 太旧」。
        AuthFailReason::Protocol => {
            ClientError::general("the hub rejected the connection (protocol error)").with_reason(HUB_PROTOCOL_REASON)
        }
    }
}

/// 把 hub 的 close code 翻成人话。
pub(crate) fn close_reason(code: Option<u16>) -> String {
    match code {
        Some(close_code::SUPERSEDED) => "another anycon process on this device connected to the hub and took over (a device may hold one hub connection at a time; run remote commands one after another)".into(),
        Some(close_code::RESTARTING) => "the hub is restarting; try again in a few seconds".into(),
        Some(close_code::SLOW_CONSUMER) => "the hub dropped this connection because it could not keep up".into(),
        Some(close_code::REVOKED) => "this device was revoked; pair again with `anycon connect`".into(),
        Some(close_code::AUTH_TIMEOUT) => "the hub closed the connection (authentication timed out)".into(),
        Some(c) => format!("the hub closed the connection (code {c})"),
        None => "the hub closed the connection".into(),
    }
}

impl HubWs {
    /// `client`：认证时自报的设备信息（名字、种类）。`cookie`：升级请求里带的登录 cookie（`name=value`）。
    /// 账号内接入时临时连接靠它让 hub 知道属于哪个账号。
    pub async fn connect_raw(
        hub_url: &str,
        token: Option<String>,
        client: ClientInfo,
        cookie: Option<&str>,
    ) -> std::result::Result<HubWs, ConnectFail> {
        let ws = connect_ws_with_cookie(hub_url, cookie).await.map_err(|e| {
            ConnectFail::Other(if e.is_rate_limited() {
                ClientError::rate_limited(hub_url, e.retry_after())
            } else {
                ClientError::offline(format!("cannot reach the hub at {hub_url}: {e}"))
            })
        })?;
        let mut h = HubWs {
            ws,
            seq: 0,
            device_id: String::new(),
            controls: vec![],
            data: VecDeque::new(),
            bystanders: VecDeque::new(),
            peers_online: vec![],
            close_code: None,
            rotation_dir: None,
        };
        h.send_control(&Control::Auth {
            device_token: token,
            protocol_version: PROTOCOL_VERSION,
            client,
            preview_token: None,
        })
        .await
        .map_err(ConnectFail::Other)?;
        loop {
            match h.recv(STEP).await {
                Ok(Some(env)) if env.kind == Kind::Control => match env.control_body() {
                    Ok(Control::AuthOk { device_id, peers_online, .. }) => {
                        h.device_id = device_id;
                        h.peers_online = peers_online;
                        return Ok(h);
                    }
                    Ok(Control::AuthFail { reason }) => return Err(ConnectFail::Auth(reason)),
                    _ => continue,
                },
                Ok(Some(_)) => continue,
                Ok(None) => {
                    return Err(ConnectFail::Other(ClientError::offline("the hub did not answer the authentication")))
                }
                Err(e) => {
                    // AuthFail 总是先于 close；万一只收到了 4004，也当作已吊销
                    if h.close_code == Some(close_code::REVOKED) {
                        return Err(ConnectFail::Auth(AuthFailReason::Revoked));
                    }
                    return Err(ConnectFail::Other(e));
                }
            }
        }
    }

    pub async fn connect(hub_url: &str, token: Option<String>, client: ClientInfo) -> Result<HubWs> {
        Self::connect_raw(hub_url, token, client, None).await.map_err(ConnectFail::into_error)
    }

    pub async fn send_control(&mut self, c: &Control) -> Result<()> {
        self.seq += 1;
        let bytes = Envelope::control("", self.seq, c)
            .and_then(|e| e.encode())
            .map_err(|e| ClientError::general(e.to_string()))?;
        self.ws.send(Message::Binary(bytes.into())).await.map_err(|e| ClientError::offline(e.to_string()))
    }

    pub async fn send_env(&mut self, to: &str, kind: Kind, body: Vec<u8>) -> Result<()> {
        self.seq += 1;
        let bytes = Envelope::new(self.device_id.clone(), to, kind, self.seq, body)
            .encode()
            .map_err(|e| ClientError::general(e.to_string()))?;
        self.ws.send(Message::Binary(bytes.into())).await.map_err(|e| ClientError::offline(e.to_string()))
    }

    /// 这封信封是不是我们等着的 `token_rotated` 回执（只在发过 `rotate_token` 的连接上认）。
    pub(crate) fn take_rotated_token(&self, env: &Envelope) -> Option<String> {
        if self.rotation_dir.is_none() || env.kind != Kind::Control {
            return None;
        }
        match env.control_body() {
            Ok(Control::TokenRotated { device_token }) => Some(device_token),
            _ => None,
        }
    }

    /// 先到的 data / notify 信封留着（`pair.ack` 可能比我们读到 `pair_done` 更早被收进来）。
    pub fn stash_data(&mut self, env: Envelope) {
        self.data.push_back(env);
    }

    /// 读下一封信封；超时返回 `Ok(None)`，连接关闭返回离线错误（并记下 close code）。
    pub async fn recv(&mut self, timeout: Duration) -> Result<Option<Envelope>> {
        loop {
            match tokio::time::timeout(timeout, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err(ClientError::offline("the hub closed the connection")),
                Ok(Some(Err(e))) => return Err(ClientError::offline(e.to_string())),
                Ok(Some(Ok(Message::Binary(b)))) => {
                    let env = Envelope::decode(&b).map_err(|e| ClientError::general(format!("bad envelope: {e}")))?;
                    if let Some(token) = self.take_rotated_token(&env) {
                        save_rotated_token(self.rotation_dir.as_deref(), &token);
                        continue;
                    }
                    return Ok(Some(env));
                }
                Ok(Some(Ok(Message::Ping(_)))) => {
                    let _ = self.ws.flush().await;
                }
                Ok(Some(Ok(Message::Close(f)))) => {
                    self.close_code = f.map(|f| u16::from(f.code));
                    return Err(ClientError::offline(close_reason(self.close_code)));
                }
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    /// 等到满足条件的 control，其它 control 暂存；先到的 data / notify 信封也暂存。
    pub async fn wait_control(&mut self, timeout: Duration, pred: impl Fn(&Control) -> bool) -> Result<Control> {
        if let Some(i) = self.controls.iter().position(&pred) {
            return Ok(self.controls.remove(i));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.recv(left).await? {
                None => return Err(ClientError::offline("timed out waiting for the hub")),
                Some(env) if env.kind == Kind::Control => {
                    if let Ok(c) = env.control_body() {
                        if pred(&c) {
                            return Ok(c);
                        }
                        self.controls.push(c);
                    }
                }
                Some(env) if env.kind == Kind::Data => self.data.push_back(env),
                Some(env) => self.bystanders.push_back(env),
            }
        }
    }

    /// 等 daemon 经配对传输发来的第一条 data 消息，并要求它是 `pair.ack`。
    pub async fn wait_pair_ack(
        &mut self,
        transport: &mut Transport,
        node_id: &str,
        timeout: Duration,
    ) -> Result<PairAck> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut reasm = frame::Reassembler::new();
        loop {
            let env = match self.data.pop_front() {
                Some(e) => e,
                None => {
                    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                    match self.recv(left).await? {
                        None => {
                            return Err(ClientError::new(
                                ErrorClass::NotPaired,
                                "pair_failed",
                                format!(
                                    "the node did not confirm the pairing within {} s; nothing was saved. If the code was entered correctly, the hub may be interfering — try a self-hosted hub or a 16-character code (`anycon pair`, without `--short`)",
                                    timeout.as_secs()
                                ),
                            ))
                        }
                        Some(e) => e,
                    }
                }
            };
            match env.kind {
                Kind::Control => {
                    if let Ok(Control::PairFailed { reason }) = env.control_body() {
                        return Err(pair_failed(reason));
                    }
                    self.bystanders.push_back(env);
                }
                Kind::Data if env.from == node_id => {
                    let frame = frame::decode(&env.body)
                        .map_err(|e| ClientError::general(format!("bad frame in the pairing confirmation: {e}")))?;
                    let Some(ct) = reasm
                        .push(frame)
                        .map_err(|e| ClientError::general(format!("bad pairing confirmation: {e}")))?
                    else {
                        continue;
                    };
                    let plain = transport.decrypt(&ct).map_err(|_| {
                        ClientError::new(
                            ErrorClass::NotPaired,
                            "pair_failed",
                            "the pairing confirmation could not be decrypted: the node does not hold the pairing secret. Nothing was saved",
                        )
                    })?;
                    return match RpcMessage::decode(&plain) {
                        Ok(RpcMessage::Notification(n)) if n.method == methods::PAIR_ACK => {
                            serde_json::from_value(n.params)
                                .map_err(|e| ClientError::general(format!("malformed pair.ack: {e}")))
                        }
                        _ => {
                            Err(ClientError::general("the first message from the node was not a pairing confirmation"))
                        }
                    };
                }
                _ => self.bystanders.push_back(env),
            }
        }
    }
}

/// 设备 token 满 30 天（或不知道签发时间）就向 hub 请求轮换。必须是认证之后的第一帧，**不等回复**：
/// 回执由 [`HubWs::recv`]（以及握手之后的收发循环）截下来落盘。发不出去（连接已经坏了）不管：后面的读会报真正的错误。
pub async fn request_rotation_if_due(h: &mut HubWs, state: &HubState) {
    if !crate::identity::token_due(state.token_issued_at_ms, crate::util::now_ms()) {
        return;
    }
    if h.send_control(&Control::RotateToken {}).await.is_ok() {
        h.rotation_dir = Some(state.dir.clone());
        tracing::debug!("the device token is due for rotation; asked the hub for a new one");
    }
}

/// 把 `token_rotated` 带来的新 token 落盘（`dir` 是状态目录）。hub 已经换了、旧 token 只再有效 60 秒，写不进去时调用方照常
/// 做完，但要告诉用户：下一次认证会失败，得重新配对。返回是否写成功。
pub(crate) fn save_rotated_token(dir: Option<&Path>, token: &str) -> bool {
    let Some(dir) = dir else { return false };
    match write_token_files(dir, token, crate::util::now_ms()) {
        Ok(()) => {
            tracing::debug!("device token rotated");
            true
        }
        Err(e) => {
            super::notice(&format!(
                "warning: the hub issued a new device token but it could not be saved ({e}); the previous token stops working within a minute, so the next command will have to pair again (`anycon connect`)"
            ));
            false
        }
    }
}
