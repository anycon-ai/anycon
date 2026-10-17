//! CLI 作为客户端设备经 hub 连 daemon：配对（Noise XXpsk3 initiator）、重连（IK initiator）、加密 JSON-RPC。
//!
//! 每个 hub 一套状态，存在 `<home>/client/<hub-host>/`：
//! `client.key`（Noise 私钥 hex，0600）、`device-token`（0600）、`device-id`、`nodes.json`（已配对节点：id、名称、公钥）。
//!
//! 配对（docs/protocol.md § 4.1、ADR-29）：短码只在本机出现——发给 hub 的是 `match_id`；握手完成、收到 `PairDone` 之后还要等
//! daemon 经新建立的 Noise 传输发来的 `pair.ack`，校验 `peer_id` 是自己的正式 id、`node_id` 等于 `PairDone.peer`、
//! `fingerprint` 等于握手得到的节点公钥的指纹，**通过才落盘**；hub 伪造 `PairDone` 骗不到它。
//!
//! 数据信封的 body 是 `envelope::frame`（整条 / 分片 / 重置）：发送时按需分片，接收时重组后再解密。

use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use anycon_crypto::{fingerprint, Handshake, Keypair, Role, Transport};
use anycon_daemon::config::Paths;
use anycon_daemon::flow::{RatePause, Retained, SendFlow};
use anycon_daemon::hub::{connect_ws_with_cookie, platform, WsStream};
use anycon_protocol::envelope::frame;
use anycon_protocol::{
    close_code, methods, AuthFailReason, ClientInfo, Control, DeviceKind, Envelope, Kind, PairAck, PairFailReason,
    PairRole, RpcId, RpcMessage, RpcNotification, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::conn::{ClosedReason, Conn, Req};
use crate::error::{CliError, Result, EXIT_ERROR, EXIT_NOT_PAIRED};
use crate::output::sanitize_label;

pub(crate) const STEP: Duration = Duration::from_secs(12);
/// `PairDone` 之后等 daemon 的 `pair.ack` 最多多久；超时 = 配对失败、什么都不落盘。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(30);
/// 节点名最多显示 / 保存多少个字符。
const NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeEntry {
    pub id: String,
    pub name: String,
    /// 节点静态公钥 hex
    pub pubkey: String,
    pub paired_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct NodesFile {
    hub_url: String,
    nodes: Vec<NodeEntry>,
}

/// 一个 hub 对应的本地客户端状态。
pub struct HubState {
    pub dir: PathBuf,
    pub hub_url: String,
    pub secret: Option<[u8; 32]>,
    pub token: Option<String>,
    pub device_id: Option<String>,
    pub nodes: Vec<NodeEntry>,
    /// 状态文件存在但读不出来（损坏）：不能当作「没有」继续——`connect` 会生成新密钥把旧的覆盖掉。
    pub problem: Option<String>,
}

pub fn host_of(hub_url: &str) -> Result<String> {
    let (_, origin) = anycon_daemon::hub::ws_url(hub_url).map_err(CliError::usage)?;
    Ok(origin.split_once("://").map(|x| x.1).unwrap_or(&origin).to_string())
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl HubState {
    /// 读某个目录里的状态（目录不存在 = 空状态）。文件存在但损坏时记在 `problem` 里，由调用方决定怎么处理。
    pub fn load(dir: &Path, hub_url: &str) -> HubState {
        let mut problems = Vec::new();
        let nodes_file: NodesFile = match std::fs::read_to_string(dir.join("nodes.json")) {
            Ok(t) => serde_json::from_str(&t).unwrap_or_else(|e| {
                problems.push(format!("{} is corrupt ({e})", dir.join("nodes.json").display()));
                NodesFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NodesFile::default(),
            Err(e) => {
                problems.push(format!("cannot read {} ({e})", dir.join("nodes.json").display()));
                NodesFile::default()
            }
        };
        let key_path = dir.join("client.key");
        let secret = match read_trim(&key_path) {
            Some(h) => match hex::decode(h).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) {
                Some(s) => Some(s),
                None => {
                    problems.push(format!("{} is corrupt", key_path.display()));
                    None
                }
            },
            None if key_path.exists() => {
                problems.push(format!("{} is empty", key_path.display()));
                None
            }
            None => None,
        };
        HubState {
            dir: dir.to_path_buf(),
            hub_url: if nodes_file.hub_url.is_empty() { hub_url.to_string() } else { nodes_file.hub_url },
            secret,
            token: read_trim(&dir.join("device-token")),
            device_id: read_trim(&dir.join("device-id")),
            nodes: nodes_file.nodes,
            problem: (!problems.is_empty()).then(|| problems.join("; ")),
        }
    }

    /// 状态损坏时的错误（修好或移走文件之后才能继续）。
    pub fn check(&self) -> Result<()> {
        match &self.problem {
            Some(p) => Err(CliError::general(format!(
                "the saved client state for {} is unusable: {p}. Fix or move the files in {} (nothing was overwritten)",
                self.hub_url,
                self.dir.display()
            ))),
            None => Ok(()),
        }
    }

    pub fn keypair(&self) -> Option<Keypair> {
        self.secret.map(Keypair::from_secret)
    }

    fn write_private(&self, name: &str, content: &str) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        anycon_daemon::identity::write_private(&self.dir.join(name), content)?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        self.check()?;
        if let Some(s) = &self.secret {
            self.write_private("client.key", &hex::encode(s))?;
        }
        if let Some(t) = &self.token {
            self.write_private("device-token", t)?;
        }
        if let Some(i) = &self.device_id {
            self.write_private("device-id", i)?;
        }
        let f = NodesFile { hub_url: self.hub_url.clone(), nodes: self.nodes.clone() };
        self.write_private("nodes.json", &serde_json::to_string_pretty(&f)?)?;
        Ok(())
    }

    /// hub 不认这个设备了（token 无效 / 已被吊销）：丢掉保存的 token 和设备 id，之后以临时连接重新配对。
    pub fn forget_credentials(&mut self) -> Result<()> {
        self.token = None;
        self.device_id = None;
        for f in ["device-token", "device-id"] {
            match std::fs::remove_file(self.dir.join(f)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    pub fn find_node(&self, key: &str) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.id == key).or_else(|| {
            let by_name: Vec<&NodeEntry> = self.nodes.iter().filter(|n| n.name == key).collect();
            if by_name.len() == 1 {
                return Some(by_name[0]);
            }
            let by_prefix: Vec<&NodeEntry> =
                self.nodes.iter().filter(|n| !key.is_empty() && n.id.starts_with(key)).collect();
            (by_prefix.len() == 1).then(|| by_prefix[0])
        })
    }
}

/// 列出本机上所有 hub 的客户端状态。
pub fn list_hubs(paths: &Paths) -> Vec<HubState> {
    let root = paths.home.join("client");
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                let st = HubState::load(&e.path(), "");
                if !st.hub_url.is_empty() || st.problem.is_some() {
                    out.push(st);
                }
            }
        }
    }
    out.sort_by(|a, b| a.hub_url.cmp(&b.hub_url));
    out
}

pub fn client_info(name: &str) -> ClientInfo {
    ClientInfo {
        name: name.to_string(),
        kind: DeviceKind::Cli,
        platform: platform(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        origin: None,
    }
}

/// 从 `--code` 的值里取出短码：可以直接是短码（`7K3P-Q2MX`，大小写 / 连字符随意），也可以是 `anycon pair` 打印的链接
/// （`https://hub/pair#code=7K3P-Q2MX`，旧式 `?code=` 也认）。
pub fn parse_code_arg(arg: &str) -> Result<String> {
    let t = arg.trim();
    let raw = match t.find("code=") {
        Some(i) => t[i + 5..].split(['&', '#', ' ']).next().unwrap_or(""),
        None => t,
    };
    let raw = raw.replace("%2D", "-").replace("%2d", "-");
    // 只做格式校验（派生是一次完整的 Argon2id，不要为了校验白算一遍）
    let normalized = anycon_crypto::normalize_code(&raw);
    let well_formed = matches!(normalized.chars().count(), 8 | 16)
        && normalized.chars().all(|c| anycon_protocol::PAIR_CODE_ALPHABET.contains(c));
    if well_formed {
        Ok(raw)
    } else {
        Err(CliError::usage(
            "invalid pairing code (expected 8 or 16 characters like 7K3P-Q2MX, or the link printed by `anycon pair`)",
        ))
    }
}

// ---------------------------------------------------------------- hub 上的一条已认证连接

pub(crate) struct HubWs {
    ws: WsStream,
    seq: u64,
    pub(crate) device_id: String,
    pub(crate) controls: Vec<Control>,
    /// 等 control 的时候先到的 data / notify 信封（比如 `PairDone` 之前就到了的 `pair.ack`）。
    data: VecDeque<Envelope>,
    pub(crate) peers_online: Vec<String>,
    /// hub 关连接时的 close code。
    close_code: Option<u16>,
}

/// 认证失败的几种结局。
pub(crate) enum ConnectFail {
    Auth(AuthFailReason),
    Other(CliError),
}

impl ConnectFail {
    pub(crate) fn into_cli(self) -> CliError {
        match self {
            ConnectFail::Auth(r) => auth_fail_error(r),
            ConnectFail::Other(e) => e,
        }
    }
}

fn auth_fail_error(reason: AuthFailReason) -> CliError {
    match reason {
        AuthFailReason::Invalid | AuthFailReason::Revoked => CliError::not_paired(format!(
            "the hub does not accept this device any more ({}); pair again with `anycon connect <hub-url> --code <code>`",
            if reason == AuthFailReason::Revoked { "revoked" } else { "unknown token" }
        )),
        AuthFailReason::Version => CliError::protocol_mismatch(
            "the hub speaks an incompatible protocol version; update anycon (or the hub) so the versions are adjacent",
        ),
        AuthFailReason::Quota => CliError::new(EXIT_ERROR, "quota", "the hub's device quota for this account is used up"),
        AuthFailReason::Protocol => CliError::general("the hub rejected the connection (protocol error)"),
    }
}

/// 把 hub 的 close code 翻成人话。
fn close_reason(code: Option<u16>) -> String {
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
    /// `cookie`：升级请求里带的登录 cookie（`name=value`）。账号内接入时临时连接靠它让 hub 知道属于哪个账号。
    pub(crate) async fn connect_raw(
        hub_url: &str,
        token: Option<String>,
        name: &str,
        cookie: Option<&str>,
    ) -> std::result::Result<HubWs, ConnectFail> {
        let ws = connect_ws_with_cookie(hub_url, cookie)
            .await
            .map_err(|e| ConnectFail::Other(CliError::offline(format!("cannot reach the hub at {hub_url}: {e}"))))?;
        let mut h = HubWs {
            ws,
            seq: 0,
            device_id: String::new(),
            controls: vec![],
            data: VecDeque::new(),
            peers_online: vec![],
            close_code: None,
        };
        h.send_control(&Control::Auth {
            device_token: token,
            protocol_version: PROTOCOL_VERSION,
            client: client_info(name),
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
                    return Err(ConnectFail::Other(CliError::offline("the hub did not answer the authentication")))
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

    async fn connect(hub_url: &str, token: Option<String>, name: &str) -> Result<HubWs> {
        Self::connect_raw(hub_url, token, name, None).await.map_err(ConnectFail::into_cli)
    }

    pub(crate) async fn send_control(&mut self, c: &Control) -> Result<()> {
        self.seq += 1;
        let bytes = Envelope::control("", self.seq, c)
            .and_then(|e| e.encode())
            .map_err(|e| CliError::general(e.to_string()))?;
        self.ws.send(Message::Binary(bytes.into())).await.map_err(|e| CliError::offline(e.to_string()))
    }

    pub(crate) async fn send_env(&mut self, to: &str, kind: Kind, body: Vec<u8>) -> Result<()> {
        self.seq += 1;
        let bytes = Envelope::new(self.device_id.clone(), to, kind, self.seq, body)
            .encode()
            .map_err(|e| CliError::general(e.to_string()))?;
        self.ws.send(Message::Binary(bytes.into())).await.map_err(|e| CliError::offline(e.to_string()))
    }

    /// 先到的 data / notify 信封留着（`pair.ack` 可能比我们读到 `pair_done` 更早被收进来）。
    pub(crate) fn stash_data(&mut self, env: Envelope) {
        self.data.push_back(env);
    }

    /// 读下一封信封；超时返回 `Ok(None)`，连接关闭返回离线错误（并记下 close code）。
    pub(crate) async fn recv(&mut self, timeout: Duration) -> Result<Option<Envelope>> {
        loop {
            match tokio::time::timeout(timeout, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err(CliError::offline("the hub closed the connection")),
                Ok(Some(Err(e))) => return Err(CliError::offline(e.to_string())),
                Ok(Some(Ok(Message::Binary(b)))) => {
                    return Envelope::decode(&b).map(Some).map_err(|e| CliError::general(format!("bad envelope: {e}")));
                }
                Ok(Some(Ok(Message::Ping(_)))) => {
                    let _ = self.ws.flush().await;
                }
                Ok(Some(Ok(Message::Close(f)))) => {
                    self.close_code = f.map(|f| u16::from(f.code));
                    return Err(CliError::offline(close_reason(self.close_code)));
                }
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    /// 等到满足条件的 control，其它 control 暂存；先到的 data / notify 信封也暂存。
    pub(crate) async fn wait_control(&mut self, timeout: Duration, pred: impl Fn(&Control) -> bool) -> Result<Control> {
        if let Some(i) = self.controls.iter().position(&pred) {
            return Ok(self.controls.remove(i));
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.recv(left).await? {
                None => return Err(CliError::offline("timed out waiting for the hub")),
                Some(env) if env.kind == Kind::Control => {
                    if let Ok(c) = env.control_body() {
                        if pred(&c) {
                            return Ok(c);
                        }
                        self.controls.push(c);
                    }
                }
                Some(env) if env.kind == Kind::Data => self.data.push_back(env),
                Some(_) => {}
            }
        }
    }

    /// 等 daemon 经配对传输发来的第一条 data 消息，并要求它是 `pair.ack`。
    pub(crate) async fn wait_pair_ack(
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
                            return Err(CliError::new(
                                EXIT_NOT_PAIRED,
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
                }
                Kind::Data if env.from == node_id => {
                    let frame = frame::decode(&env.body)
                        .map_err(|e| CliError::general(format!("bad frame in the pairing confirmation: {e}")))?;
                    let Some(ct) =
                        reasm.push(frame).map_err(|e| CliError::general(format!("bad pairing confirmation: {e}")))?
                    else {
                        continue;
                    };
                    let plain = transport.decrypt(&ct).map_err(|_| {
                        CliError::new(
                            EXIT_NOT_PAIRED,
                            "pair_failed",
                            "the pairing confirmation could not be decrypted: the node does not hold the pairing secret. Nothing was saved",
                        )
                    })?;
                    return match RpcMessage::decode(&plain) {
                        Ok(RpcMessage::Notification(n)) if n.method == methods::PAIR_ACK => {
                            serde_json::from_value(n.params)
                                .map_err(|e| CliError::general(format!("malformed pair.ack: {e}")))
                        }
                        _ => Err(CliError::general("the first message from the node was not a pairing confirmation")),
                    };
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------- 配对

/// 校验 `pair.ack`：`peer_id` 是自己的正式 id，`node_id` 是 hub 在 `PairDone` 里说的节点，指纹是握手得到的节点公钥的指纹。
/// 任何一项不符都不落盘。
pub fn verify_pair_ack(ack: &PairAck, my_id: &str, node_id: &str, node_pub: &[u8; 32]) -> Result<()> {
    let bad = |what: &str| {
        CliError::new(
            EXIT_NOT_PAIRED,
            "pair_failed",
            format!("the node's pairing confirmation does not match ({what}); nothing was saved. The hub may be interfering with the pairing"),
        )
    };
    if ack.peer_id != my_id {
        return Err(bad("it names a different device id for this machine"));
    }
    if ack.node_id != node_id {
        return Err(bad("the node id differs from the one the hub reported"));
    }
    if ack.fingerprint != fingerprint(node_pub) {
        return Err(bad("the key fingerprint differs from the key used in the handshake"));
    }
    Ok(())
}

/// `anycon connect`：作为 joiner 配对。成功（`pair.ack` 校验通过）后才把节点记入本地状态。
pub async fn pair(paths: &Paths, hub_url: &str, code_arg: &str, device_name: &str) -> Result<NodeEntry> {
    pair_with(paths, hub_url, code_arg, device_name, ACK_TIMEOUT).await
}

pub(crate) async fn pair_with(
    paths: &Paths,
    hub_url: &str,
    code_arg: &str,
    device_name: &str,
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    let code = parse_code_arg(code_arg)?;
    // 一次 Argon2id（64 MiB，约 0.1–0.3 s）同时得到 match_id 和 PSK；CPU 密集，放进 blocking 线程
    let secrets = {
        let code = code.clone();
        tokio::task::spawn_blocking(move || anycon_crypto::derive_pair_secrets(&code))
            .await
            .map_err(|e| CliError::general(format!("pairing key derivation failed: {e}")))?
            .map_err(|_| CliError::usage("invalid pairing code"))?
    };
    // PSK 的局部副本用 `Zeroizing`：离开作用域时清零（`secrets` 自己 Drop 时也清零）
    let (psk, match_id) = (zeroize::Zeroizing::new(secrets.psk), secrets.match_id.clone());
    drop(secrets);
    let hub_url = hub_url.trim_end_matches('/').to_string();
    let dir = paths.client_dir(&host_of(&hub_url)?);
    let mut state = HubState::load(&dir, &hub_url);
    state.check()?;
    state.hub_url = hub_url.clone();
    let kp = match state.keypair() {
        Some(k) => k,
        None => {
            let secret = anycon_daemon::util::random_bytes::<32>();
            state.secret = Some(secret);
            Keypair::from_secret(secret)
        }
    };
    // 带着已存的 token 认证；hub 说 invalid / revoked（被吊销、hub 丢了数据）就丢掉旧身份，以临时连接重新配对
    let mut h = match HubWs::connect_raw(&hub_url, state.token.clone(), device_name, None).await {
        Ok(h) => h,
        Err(ConnectFail::Auth(AuthFailReason::Invalid | AuthFailReason::Revoked)) if state.token.is_some() => {
            eprintln!("note: the hub no longer accepts this device's saved credentials; pairing as a new device");
            state.forget_credentials()?;
            HubWs::connect(&hub_url, None, device_name).await?
        }
        Err(f) => return Err(f.into_cli()),
    };
    h.send_control(&Control::PairJoin { match_id }).await?;
    let host_temp =
        match h.wait_control(STEP, |c| matches!(c, Control::PairMatched { .. } | Control::PairFailed { .. })).await? {
            Control::PairMatched { peer, role } => {
                if role != PairRole::Joiner {
                    return Err(CliError::general("unexpected pairing role"));
                }
                peer
            }
            Control::PairFailed { reason } => return Err(pair_failed(reason)),
            _ => unreachable!(),
        };
    let mut hs = Handshake::pairing(Role::Initiator, &kp, &psk).map_err(|e| CliError::general(e.to_string()))?;
    let m1 = hs.write(&[]).map_err(|e| CliError::general(e.to_string()))?;
    h.send_env(&host_temp, Kind::Handshake, m1).await?;
    let m2 = loop {
        match h.recv(STEP).await? {
            Some(env) if env.kind == Kind::Handshake && env.from == host_temp => break env.body,
            Some(env) if env.kind == Kind::Control => {
                if let Ok(Control::PairFailed { reason }) = env.control_body() {
                    return Err(pair_failed(reason));
                }
            }
            Some(_) => {}
            None => return Err(CliError::offline("the node did not answer the pairing handshake")),
        }
    };
    // 第 2 条的 payload（节点名等）不受 PSK 保护，不信、不用；节点名取自后面认证过的 `pair.ack`
    hs.read(&m2).map_err(|e| CliError::general(format!("pairing handshake failed: {e}")))?;
    // 第三条的 payload 带上本设备信息（daemon 据此记名字和类型）
    let info = serde_json::to_vec(&client_info(device_name))?;
    let m3 = hs.write(&info).map_err(|e| CliError::general(e.to_string()))?;
    let node_pub = hs.remote_public().ok_or_else(|| CliError::general("pairing finished without a node key"))?;
    let mut pair_transport = hs.into_transport().map_err(|e| CliError::general(e.to_string()))?;
    h.send_env(&host_temp, Kind::Handshake, m3).await?;
    h.send_control(&Control::PairConfirm { peer: host_temp }).await?;
    finish_pairing(&mut state, &mut h, &mut pair_transport, node_pub, ack_timeout).await
}

/// 握手（含第 3 条和 `pair_confirm`）之后的收尾，短码配对和账号内接入共用：等 `pair_done`，再等 daemon 经配对传输发来的
/// `pair.ack`，校验 `peer_id`、`node_id`、公钥指纹，**通过才落盘**；本地已有同 id 但公钥不同的节点报 `key_mismatch`、不覆盖。
pub(crate) async fn finish_pairing(
    state: &mut HubState,
    h: &mut HubWs,
    pair_transport: &mut Transport,
    node_pub: [u8; 32],
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    let done =
        match h.wait_control(STEP * 2, |c| matches!(c, Control::PairDone { .. } | Control::PairFailed { .. })).await? {
            Control::PairDone { peer, device_id, device_token } => PairDoneInfo { peer, device_id, device_token },
            Control::PairFailed { reason } => return Err(pair_failed(reason)),
            _ => unreachable!(),
        };
    complete_pairing(state, h, pair_transport, node_pub, done, ack_timeout).await
}

/// hub 的 `pair_done`（`peer` 是节点的正式 id；`device_*` 只有之前没有身份的一方才有）。
pub(crate) struct PairDoneInfo {
    pub peer: String,
    pub device_id: Option<String>,
    pub device_token: Option<String>,
}

/// [`finish_pairing`] 的后半段：`pair_done` 已经在手里（账号内接入自己在读 `pair_done`，因为 daemon 随时可能先回一条 `rejected`）。
pub(crate) async fn complete_pairing(
    state: &mut HubState,
    h: &mut HubWs,
    pair_transport: &mut Transport,
    node_pub: [u8; 32],
    done: PairDoneInfo,
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    let PairDoneInfo { peer: node_id, device_id: dev_id, device_token: token } = done;
    // 这台机器的正式 id：刚签发的，或者本来就有的
    let my_id = dev_id.clone().unwrap_or_else(|| h.device_id.clone());
    if let Some(i) = &dev_id {
        h.device_id = i.clone();
    }
    // 等 daemon 经配对传输发来的确认，校验通过才算配对成功
    let ack = h.wait_pair_ack(pair_transport, &node_id, ack_timeout).await?;
    verify_pair_ack(&ack, &my_id, &node_id, &node_pub)?;
    let pubkey = hex::encode(node_pub);
    // 不静默覆盖已有节点的固定公钥
    if let Some(existing) = state.nodes.iter().find(|n| n.id == node_id) {
        if existing.pubkey != pubkey {
            return Err(CliError::new(
                EXIT_NOT_PAIRED,
                "key_mismatch",
                format!(
                    "this machine is already paired with a node that has the same id but a different key ({}). It will not be replaced silently: run `anycon disconnect {}` first, then pair again",
                    sanitize_label(&existing.name, NAME_MAX_CHARS, &existing.id),
                    sanitize_label(&existing.name, NAME_MAX_CHARS, &existing.id)
                ),
            ));
        }
    }
    let name =
        sanitize_label(&ack.name, NAME_MAX_CHARS, &format!("node-{}", node_id.chars().take(8).collect::<String>()));
    let entry = NodeEntry { id: node_id, name, pubkey, paired_at_ms: anycon_daemon::util::now_ms() };
    state.device_id = Some(my_id);
    if let Some(t) = token {
        state.token = Some(t);
    }
    state.nodes.retain(|n| n.id != entry.id);
    state.nodes.push(entry.clone());
    state.save()?;
    Ok(entry)
}

pub(crate) fn pair_failed(reason: PairFailReason) -> CliError {
    let msg = match reason {
        PairFailReason::NotFound => "the pairing code was not found, has expired, or was already used",
        PairFailReason::Timeout => "pairing timed out",
        PairFailReason::PeerGone => "the node disconnected during pairing",
        PairFailReason::RateLimited => "too many attempts; wait a minute and retry",
        PairFailReason::Quota => "device quota exceeded on the hub",
        PairFailReason::Forbidden => {
            "the hub refused this pairing (the devices belong to different accounts, or neither has one)"
        }
    };
    CliError::new(EXIT_NOT_PAIRED, "pair_failed", msg)
}

// ---------------------------------------------------------------- 连接节点（IK）

/// 在已认证的 hub 连接上对 `node_id` 做 IK 握手，并把连接交给后台任务驱动成 [`Conn`]。
async fn ik_connect(
    mut h: HubWs,
    kp: &Keypair,
    node_id: &str,
    node_pub: &[u8; 32],
    node_was_online: bool,
) -> Result<Conn> {
    let mut hs =
        Handshake::reconnect(Role::Initiator, kp, Some(node_pub)).map_err(|e| CliError::general(e.to_string()))?;
    let m1 = hs.write(&[]).map_err(|e| CliError::general(e.to_string()))?;
    h.send_env(node_id, Kind::Handshake, m1).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    let reply = loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match h.recv(left).await? {
            None => {
                // hub 说节点在线却不回握手：最常见的原因是节点已经不认这台设备的公钥了（被吊销）
                return Err(if node_was_online {
                    CliError::not_paired(
                        "the node is online but did not accept this device; it may have been revoked — pair again with `anycon connect <hub-url> --code <code>`",
                    )
                } else {
                    CliError::offline("the node did not answer; is the daemon running and connected to the hub?")
                });
            }
            Some(env) => match env.kind {
                Kind::Handshake if env.from == node_id => break env.body,
                Kind::Control => match env.control_body() {
                    Ok(Control::Undeliverable { to, .. }) if to == node_id => {
                        return Err(CliError::offline("the node is offline"))
                    }
                    Ok(Control::Dropped { reason, .. }) => {
                        return Err(CliError::general(format!("the hub dropped the handshake ({reason:?})")))
                    }
                    Ok(c) => h.controls.push(c),
                    Err(_) => {}
                },
                _ => {}
            },
        }
    };
    hs.read(&reply).map_err(|e| {
        CliError::not_paired(format!(
            "the node did not accept this device ({e}); it may have been revoked — pair again with `anycon connect`"
        ))
    })?;
    let transport = hs.into_transport().map_err(|e| CliError::general(e.to_string()))?;

    // 把 ws 和 transport 交给后台任务
    let HubWs { ws, seq, device_id, .. } = h;
    let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
    let (note_tx, note_rx) = mpsc::unbounded_channel::<RpcNotification>();
    let closed: ClosedReason = Arc::new(Mutex::new(None));
    let node = node_id.to_string();
    let driver = Driver::new(ws, transport, device_id, node.clone(), seq);
    let closed_d = closed.clone();
    tokio::spawn(async move {
        let reason = driver.run(req_rx, note_tx).await;
        *closed_d.lock().unwrap() = Some(reason);
    });
    Ok(Conn::from_parts(req_tx, note_rx, false, Some(node), closed))
}

/// 重发时一批最多多少字节（CLI 的上行很小，基本一批发完）。
const RESEND_BATCH_BYTES: usize = 2 * 1024 * 1024;

/// 到 daemon 的加密通道的驱动：发请求（分片、发送方规则）、收响应 / 通知（重组、解密）。
///
/// **发送方规则（docs/protocol.md § 3.1，状态机在 [`anycon_daemon::flow`]）**：保留最近发出的信封原文；hub 退回一封
/// （`backpressure`）后暂停发送（新请求等着），等 `retry_after_ms`（最近一条 `rate_limited` 给的，没有就 100–300 ms 加抖动），
/// 把所有被退回的 seq 按升序原样重发，再继续；重发期间又被退回就再等一轮；被退回的 seq 已不在保留缓冲里就结束这条连接
/// （加密通道少了一封，只能重来）。`rate_limited` 暂停整条连接的出站。
struct Driver {
    ws: WsStream,
    transport: Transport,
    device_id: String,
    node_id: String,
    seq: u64,
    msg_id: u32,
    group: u64,
    reasm: frame::Reassembler,
    flow: SendFlow,
    rate: RatePause,
}

impl Driver {
    fn new(ws: WsStream, transport: Transport, device_id: String, node_id: String, seq: u64) -> Driver {
        Driver {
            ws,
            transport,
            device_id,
            node_id,
            seq,
            msg_id: 0,
            group: 0,
            reasm: frame::Reassembler::new(),
            flow: SendFlow::default(),
            rate: RatePause::default(),
        }
    }

    fn envelope_bytes(&self, seq: u64, kind: Kind, body: Vec<u8>) -> std::result::Result<Vec<u8>, String> {
        Envelope::new(self.device_id.clone(), self.node_id.clone(), kind, seq, body).encode().map_err(|e| e.to_string())
    }

    /// 加密一条请求、分片、逐片发送（暂停中的片只入保留缓冲，等重发时按序发出）。
    async fn send_request(&mut self, msg: &RpcMessage) -> std::result::Result<(), String> {
        let ct = self.transport.encrypt(&msg.encode()).map_err(|e| format!("encrypt failed: {e}"))?;
        self.msg_id = self.msg_id.wrapping_add(1);
        self.group += 1;
        for body in frame::encode(&ct, self.msg_id) {
            self.seq += 1;
            let seq = self.seq;
            let blocked = self.flow.blocked(StdInstant::now(), &self.rate);
            let bytes = if blocked { None } else { Some(self.envelope_bytes(seq, Kind::Data, body.clone())?) };
            self.flow.retain(Retained { seq, kind: Kind::Data, body, group: self.group });
            match bytes {
                Some(b) => self
                    .ws
                    .send(Message::Binary(b.into()))
                    .await
                    .map_err(|e| format!("sending to the hub failed: {e}"))?,
                None => self.flow.defer(seq),
            }
        }
        Ok(())
    }

    /// hub 退回了 `seq`：暂停，之后按升序原样重发。不在保留缓冲里就返回错误（连接结束）。
    fn on_backpressure(&mut self, seq: u64) -> std::result::Result<(), String> {
        let now = StdInstant::now();
        if self.flow.bounce(seq, now, self.rate.hint(now)) {
            Ok(())
        } else {
            Err("the hub bounced a message that is no longer retained, so the secure channel to the node lost sync; run the command again".into())
        }
    }

    /// 暂停结束：把待发的（被退回的 + 暂停期间排队的）按升序原样发出去。
    async fn flush(&mut self) -> std::result::Result<(), String> {
        let now = StdInstant::now();
        if self.flow.ready_at(&self.rate, now).is_none_or(|t| t > now) {
            return Ok(());
        }
        for (seq, item) in self.flow.take_batch(now, RESEND_BATCH_BYTES) {
            let (kind, body) = item.unwrap_or_else(|| (Kind::Data, frame::reset_body()));
            let b = self.envelope_bytes(seq, kind, body)?;
            self.ws.send(Message::Binary(b.into())).await.map_err(|e| format!("sending to the hub failed: {e}"))?;
        }
        Ok(())
    }

    /// 返回连接结束的原因。
    async fn run(
        mut self,
        mut req_rx: mpsc::UnboundedReceiver<Req>,
        note_tx: mpsc::UnboundedSender<RpcNotification>,
    ) -> String {
        let mut pending: HashMap<u64, oneshot::Sender<std::result::Result<Value, anycon_protocol::RpcError>>> =
            HashMap::new();
        let mut next = 0u64;
        let reason = loop {
            let now = StdInstant::now();
            let blocked = self.flow.blocked(now, &self.rate);
            // 该醒的时候：有待发的 -> 可以重发的时刻；没有 -> 暂停结束的时刻（要重新判断能不能收新请求）
            let wake = self.flow.ready_at(&self.rate, now).or_else(|| self.flow.unblock_at(&self.rate, now));
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
                                Kind::Data if env.from == self.node_id => {
                                    let ct = match frame::decode(&env.body) {
                                        Ok(frame::DataFrame::Reset) => break "the node reset the connection; run the command again".to_string(),
                                        Ok(f) => match self.reasm.push(f) {
                                            Ok(Some(ct)) => ct,
                                            Ok(None) => continue,
                                            Err(e) => break format!("a message from the node was malformed ({e}); run the command again"),
                                        },
                                        Err(e) => break format!("a message from the node was malformed ({e}); run the command again"),
                                    };
                                    let Ok(plain) = self.transport.decrypt(&ct) else {
                                        break "a message from the node could not be decrypted (the secure channel is out of sync); run the command again".to_string();
                                    };
                                    match RpcMessage::decode(&plain) {
                                        Ok(RpcMessage::Response(r)) => {
                                            if let RpcId::Num(id) = r.id {
                                                if let Some(tx) = pending.remove(&id) {
                                                    let _ = tx.send(match r.error { Some(e) => Err(e), None => Ok(r.result.unwrap_or(Value::Null)) });
                                                }
                                            }
                                        }
                                        Ok(RpcMessage::Notification(n)) => { let _ = note_tx.send(n); }
                                        _ => {}
                                    }
                                }
                                Kind::Control => match env.control_body() {
                                    Ok(Control::Undeliverable { to, .. }) if to == self.node_id => break "the node went offline".to_string(),
                                    Ok(Control::PeerOffline { device }) if device == self.node_id => break "the node went offline".to_string(),
                                    Ok(Control::Backpressure { to, seq }) if to == self.node_id => {
                                        if let Err(e) = self.on_backpressure(seq) { break e; }
                                    }
                                    // 超过了速率预算：整条连接的出站暂停 retry_after_ms（被退回的信封之后按序重发）
                                    Ok(Control::RateLimited { retry_after_ms }) => {
                                        self.rate.limited(StdInstant::now(), Duration::from_millis(retry_after_ms));
                                    }
                                    // hub 丢了我们的某一封（不是退回）：加密通道已经少了一条，只能重来
                                    Ok(Control::Dropped { reason, .. }) => break format!("the hub dropped one of our messages ({reason:?}); run the command again"),
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
                    if let Err(e) = self.flush().await { break e; }
                }
                req = req_rx.recv(), if !blocked => {
                    let Some(req) = req else { break "closed locally".to_string() };
                    next += 1;
                    let msg = RpcMessage::request(next, req.method, req.params);
                    if let Err(e) = self.send_request(&msg).await { break e; }
                    pending.insert(next, req.reply);
                }
            }
        };
        drop(pending);
        let _ = self.ws.close(None).await;
        reason
    }
}

/// 打开到某个节点的远程连接。
pub async fn open(state: &HubState, node: &NodeEntry, device_name: &str) -> Result<Conn> {
    state.check()?;
    let token = state
        .token
        .clone()
        .ok_or_else(|| CliError::not_paired("this CLI has no device token for the hub; run `anycon connect` first"))?;
    let kp = state
        .keypair()
        .ok_or_else(|| CliError::not_paired("this CLI has no key for the hub; run `anycon connect` first"))?;
    let node_pub: [u8; 32] = hex::decode(&node.pubkey)
        .ok()
        .and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok())
        .ok_or_else(|| CliError::general("corrupt nodes.json"))?;
    let mut h = HubWs::connect(&state.hub_url, Some(token), device_name).await?;
    let mut online = h.peers_online.contains(&node.id);
    if !online {
        // 节点刚好在重连：给一点时间等 peer_online
        online = h
            .wait_control(Duration::from_secs(2), |c| matches!(c, Control::PeerOnline { device } if device == &node.id))
            .await
            .is_ok();
    }
    let mut conn = ik_connect(h, &kp, &node.id, &node_pub, online).await?;
    conn.node_name = Some(sanitize_label(&node.name, NAME_MAX_CHARS, &node.id));
    Ok(conn)
}

/// 节点在线状态（hub 的 `auth_ok.peers_online`）。连不上 hub 时返回 `None`。
pub async fn online_nodes(state: &HubState, device_name: &str) -> Option<Vec<String>> {
    let token = state.token.clone()?;
    let h = HubWs::connect(&state.hub_url, Some(token), device_name).await.ok()?;
    Some(h.peers_online)
}

/// 版本检查：本地协议版本和节点的差值不能超过 1（相邻 minor 兼容，docs/protocol.md § 9）。
pub fn check_protocol(remote: u32) -> Result<()> {
    if remote.abs_diff(PROTOCOL_VERSION) > 1 {
        return Err(CliError::protocol_mismatch(format!(
            "the node speaks protocol {remote}, this CLI speaks {PROTOCOL_VERSION}; update whichever is older"
        )));
    }
    Ok(())
}

/// 每次连上节点之后用 `node.info` 刷新本地保存的节点名（best effort；名字经过清理）。
pub fn refresh_node_name(state: &mut HubState, node_id: &str, name: &str) {
    let clean = sanitize_label(name, NAME_MAX_CHARS, "");
    if clean.is_empty() {
        return;
    }
    if let Some(n) = state.nodes.iter_mut().find(|n| n.id == node_id) {
        if n.name != clean {
            n.name = clean;
            if let Err(e) = state.save() {
                tracing::debug!(error = %e, "could not refresh the saved node name");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_arguments_accept_codes_and_links() {
        assert_eq!(parse_code_arg("7k3p-q2mx").unwrap(), "7k3p-q2mx");
        assert_eq!(parse_code_arg("  7K3PQ2MX ").unwrap(), "7K3PQ2MX");
        assert_eq!(parse_code_arg("https://hub.example.com/pair#code=7K3P-Q2MX").unwrap(), "7K3P-Q2MX");
        assert_eq!(parse_code_arg("https://hub.example.com/pair?code=7K3P-Q2MX&x=1").unwrap(), "7K3P-Q2MX");
        assert!(
            parse_code_arg("http://127.0.0.1:8080/pair#code=7K3P-Q2MX-ILO0-1111").is_err(),
            "I/L/O/0/1 are not in the alphabet"
        );
        let long = anycon_crypto::generate_pair_code_long();
        assert_eq!(parse_code_arg(&format!("https://h/pair#code={long}")).unwrap(), long);
        assert!(parse_code_arg("1").is_err());
        assert!(parse_code_arg("").is_err());
        assert!(parse_code_arg("https://hub.example.com/pair").is_err());
    }

    fn ack(node: &str, peer: &str, pubkey: &[u8; 32]) -> PairAck {
        PairAck {
            node_id: node.into(),
            peer_id: peer.into(),
            name: "n".into(),
            version: "0".into(),
            fingerprint: fingerprint(pubkey),
        }
    }

    #[test]
    fn a_pair_ack_must_match_the_ids_and_the_handshake_key() {
        let key = [7u8; 32];
        verify_pair_ack(&ack("node", "me", &key), "me", "node", &key).unwrap();
        let wrong_peer = verify_pair_ack(&ack("node", "someone-else", &key), "me", "node", &key).unwrap_err();
        assert_eq!((wrong_peer.exit, wrong_peer.code.as_str()), (EXIT_NOT_PAIRED, "pair_failed"));
        assert!(verify_pair_ack(&ack("other-node", "me", &key), "me", "node", &key).is_err());
        // 握手得到的公钥和 ack 里的指纹不是同一把：中间人换了钥匙
        assert!(verify_pair_ack(&ack("node", "me", &[8u8; 32]), "me", "node", &key).is_err());
    }

    #[test]
    fn protocol_versions_must_be_adjacent() {
        assert!(check_protocol(PROTOCOL_VERSION).is_ok());
        assert!(check_protocol(PROTOCOL_VERSION + 1).is_ok());
        assert!(check_protocol(PROTOCOL_VERSION + 2).is_err());
        assert_eq!(check_protocol(PROTOCOL_VERSION + 5).unwrap_err().code, "protocol_mismatch");
    }

    #[test]
    fn corrupt_state_files_are_reported_not_treated_as_missing() {
        let d = tempfile::tempdir().unwrap();
        // 没有文件：空状态，没有问题
        assert!(HubState::load(d.path(), "http://h").problem.is_none());
        std::fs::write(d.path().join("nodes.json"), "{ not json").unwrap();
        let st = HubState::load(d.path(), "http://h");
        assert!(st.problem.as_deref().unwrap().contains("nodes.json"));
        assert!(st.check().is_err() && st.save().is_err(), "a corrupt state is never overwritten");
        std::fs::remove_file(d.path().join("nodes.json")).unwrap();
        std::fs::write(d.path().join("client.key"), "zz").unwrap();
        let st = HubState::load(d.path(), "http://h");
        assert!(st.problem.as_deref().unwrap().contains("client.key"));
        assert_eq!(std::fs::read_to_string(d.path().join("client.key")).unwrap(), "zz");
    }

    #[test]
    fn forgetting_credentials_removes_the_files_but_keeps_the_key_and_nodes() {
        let d = tempfile::tempdir().unwrap();
        let mut st = HubState::load(d.path(), "http://h");
        st.secret = Some([1u8; 32]);
        st.token = Some("tok".into());
        st.device_id = Some("dev".into());
        st.nodes.push(NodeEntry { id: "n".into(), name: "n".into(), pubkey: "00".into(), paired_at_ms: 1 });
        st.save().unwrap();
        st.forget_credentials().unwrap();
        let again = HubState::load(d.path(), "http://h");
        assert!(again.token.is_none() && again.device_id.is_none());
        assert_eq!(again.secret, Some([1u8; 32]));
        assert_eq!(again.nodes.len(), 1);
        st.forget_credentials().unwrap(); // 幂等
    }

    // ------------------------------------------------------------ 恶意 hub：伪造 PairDone

    /// 一个假 hub：配合 joiner 走完撮合，用**错误的 PSK** 当 responder（hub 其实不知道短码），然后伪造 `PairDone`。
    /// `junk_ack` 为真时再发一条解密不过的「pair.ack」；为假时什么都不发。
    async fn evil_hub(junk_ack: bool) -> (String, tokio::task::JoinHandle<()>) {
        use anycon_protocol::Kind as K;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let h = tokio::spawn(async move {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let me = "joiner-temp".to_string();
            let mut seq = 0u64;
            let hub_kp = Keypair::generate();
            let mut hs = Handshake::pairing(Role::Responder, &hub_kp, &[9u8; 32]).unwrap();
            macro_rules! send_ctl {
                ($c:expr) => {{
                    seq += 1;
                    let b = Envelope::control("", seq, &$c).unwrap().encode().unwrap();
                    ws.send(Message::Binary(b.into())).await.unwrap();
                }};
            }
            while let Some(Ok(Message::Binary(b))) = ws.next().await {
                let env = Envelope::decode(&b).unwrap();
                match env.kind {
                    K::Control => match env.control_body().unwrap() {
                        Control::Auth { .. } => send_ctl!(Control::AuthOk {
                            device_id: me.clone(),
                            server_time_ms: 0,
                            peers_online: vec![],
                            quota: None
                        }),
                        Control::PairJoin { .. } => {
                            send_ctl!(Control::PairMatched { peer: "host-temp".into(), role: PairRole::Joiner })
                        }
                        Control::PairConfirm { .. } => {
                            send_ctl!(Control::PairDone {
                                peer: "forged-node".into(),
                                device_id: Some("new-device-id".into()),
                                device_token: Some("forged-token".into())
                            });
                            if junk_ack {
                                let mut body = vec![frame::WHOLE];
                                body.extend_from_slice(&[3u8; 80]);
                                seq += 1;
                                let b =
                                    Envelope::new("forged-node", "new-device-id", K::Data, seq, body).encode().unwrap();
                                ws.send(Message::Binary(b.into())).await.unwrap();
                            }
                        }
                        _ => {}
                    },
                    K::Handshake => {
                        let read_ok = hs.read(&env.body).is_ok();
                        if read_ok && !hs.is_finished() && hs.is_my_turn() {
                            let m2 = hs.write(br#"{"name":"EVIL","version":"0"}"#).unwrap();
                            seq += 1;
                            let b = Envelope::new("host-temp", me.clone(), K::Handshake, seq, m2).encode().unwrap();
                            ws.send(Message::Binary(b.into())).await.unwrap();
                        }
                    }
                    _ => {}
                }
            }
        });
        (url, h)
    }

    #[tokio::test]
    async fn a_forged_pair_done_without_a_valid_pair_ack_pairs_nothing_and_saves_nothing() {
        for junk in [false, true] {
            let (url, hub) = evil_hub(junk).await;
            let home = tempfile::tempdir().unwrap();
            let paths = Paths::new(home.path());
            let code = anycon_crypto::generate_pair_code();
            let t = std::time::Instant::now();
            let e = pair_with(&paths, &url, &code, "cli-test", Duration::from_millis(800)).await.unwrap_err();
            assert_eq!(e.exit, EXIT_NOT_PAIRED, "junk={junk}: {}", e.message);
            assert_eq!(e.code, "pair_failed", "junk={junk}: {}", e.message);
            let want = if junk { "could not be decrypted" } else { "did not confirm" };
            assert!(e.message.contains(want), "junk={junk}: {}", e.message);
            assert!(t.elapsed() < Duration::from_secs(8));
            // 什么都没落盘：没有节点、没有 token、没有设备 id、没有 hub 发的假身份
            let dir = paths.client_dir(&host_of(&url).unwrap());
            for f in ["nodes.json", "device-token", "device-id"] {
                assert!(!dir.join(f).exists(), "junk={junk}: {f} must not be written");
            }
            hub.abort();
        }
    }

    // ------------------------------------------------------------ 分片与背压

    #[tokio::test]
    async fn a_big_request_is_fragmented_and_a_dropped_fragment_is_resent_with_the_same_bytes() {
        use anycon_protocol::Kind as K;
        // 两个已握手的 Transport：(服务端, 客户端)
        let (srv_kp, cli_kp) = (Keypair::generate(), Keypair::generate());
        let mut ini = Handshake::reconnect(Role::Initiator, &cli_kp, Some(&srv_kp.public)).unwrap();
        let mut res = Handshake::reconnect(Role::Responder, &srv_kp, Some(&cli_kp.public)).unwrap();
        let m1 = ini.write(&[]).unwrap();
        res.read(&m1).unwrap();
        let m2 = res.write(&[]).unwrap();
        ini.read(&m2).unwrap();
        let (mut srv_t, cli_t) = (res.into_transport().unwrap(), ini.into_transport().unwrap());

        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut reasm = frame::Reassembler::new();
            let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut dropped: Option<(u64, Vec<u8>, std::time::Instant)> = None;
            let mut seq = 0u64;
            while let Some(Ok(Message::Binary(b))) = ws.next().await {
                let env = Envelope::decode(&b).unwrap();
                assert_eq!(env.kind, K::Data);
                let is_last =
                    matches!(frame::decode(&env.body).unwrap(), frame::DataFrame::Fragment { index: 1, count: 2, .. });
                if is_last && dropped.is_none() {
                    // 假装 hub 丢了第二片，并发背压回执
                    dropped = Some((env.seq, env.body.clone(), std::time::Instant::now()));
                    seq += 1;
                    let c = Control::Backpressure { to: "node-1".into(), seq: env.seq };
                    let b = Envelope::control("", seq, &c).unwrap().encode().unwrap();
                    ws.send(Message::Binary(b.into())).await.unwrap();
                    continue;
                }
                if is_last {
                    let (s0, body0, at) = dropped.take().unwrap();
                    assert_eq!((env.seq, &env.body), (s0, &body0), "resent with the original seq and the same bytes");
                    assert!(at.elapsed() >= Duration::from_millis(90), "it paused (100-300 ms) before resending");
                    dropped = Some((0, vec![], at));
                }
                got.push((env.seq, env.body.clone()));
                if let Some(ct) = reasm.push(frame::decode(&env.body).unwrap()).unwrap() {
                    let plain = srv_t.decrypt(&ct).unwrap();
                    let RpcMessage::Request(req) = RpcMessage::decode(&plain).unwrap() else {
                        panic!("request expected")
                    };
                    assert_eq!(req.params["blob"].as_str().unwrap().len(), 300_000);
                    let resp = RpcMessage::result(req.id, serde_json::json!({ "ok": true })).encode();
                    let ct = srv_t.encrypt(&resp).unwrap();
                    seq += 1;
                    let b = Envelope::new("node-1", "dev-1", K::Data, seq, frame::encode(&ct, 1).remove(0))
                        .encode()
                        .unwrap();
                    ws.send(Message::Binary(b.into())).await.unwrap();
                }
            }
            got.len()
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
        let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
        let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
        let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
        let run = tokio::spawn(driver.run(req_rx, note_tx));
        let (reply, rx) = oneshot::channel();
        req_tx
            .send(Req { method: "x.y".into(), params: serde_json::json!({ "blob": "b".repeat(300_000) }), reply })
            .unwrap();
        let v = tokio::time::timeout(Duration::from_secs(10), rx).await.expect("answered").unwrap().unwrap();
        assert_eq!(v, serde_json::json!({ "ok": true }));
        drop(req_tx);
        let reason = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        assert_eq!(reason, "closed locally");
        server.abort();
    }

    /// 两个已握手的 Transport：(服务端, 客户端)。
    fn transport_pair() -> (Transport, Transport) {
        let (srv_kp, cli_kp) = (Keypair::generate(), Keypair::generate());
        let mut ini = Handshake::reconnect(Role::Initiator, &cli_kp, Some(&srv_kp.public)).unwrap();
        let mut res = Handshake::reconnect(Role::Responder, &srv_kp, Some(&cli_kp.public)).unwrap();
        let m1 = ini.write(&[]).unwrap();
        res.read(&m1).unwrap();
        let m2 = res.write(&[]).unwrap();
        ini.read(&m2).unwrap();
        (res.into_transport().unwrap(), ini.into_transport().unwrap())
    }

    /// 起一个假 hub + 假 daemon 在同一个 WebSocket 连接的另一端：对 `data` 信封走 `StickyTarget`（容量 `cap`，每 10 ms 被读走
    /// `read_per_tick`），被退回的回 `backpressure`，被读走的按序重组 + 解密，每条请求回一个 `{ "len": <blob 长度> }`。
    /// `rate_limit_first`：第一次退回时再发一条 `rate_limited { retry_after_ms }`。返回 (ws url, 任务句柄；结果是看到的 blob 长度和退回次数)。
    async fn sticky_node(
        cap: usize,
        read_per_tick: usize,
        rate_limit_first: Option<u64>,
        mut srv_t: Transport,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<(Vec<usize>, usize)>) {
        use anycon_daemon::testutil::{Offer, StickyTarget};
        use anycon_protocol::Kind as K;
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let mut target = StickyTarget::new(cap);
            let mut reasm = frame::Reassembler::new();
            let mut seen = Vec::new();
            let mut ctl_seq = 0u64;
            let mut told_rate = false;
            let mut tick = tokio::time::interval(Duration::from_millis(10));
            loop {
                tokio::select! {
                    m = ws.next() => {
                        let Some(Ok(Message::Binary(b))) = m else { break };
                        let env = Envelope::decode(&b).unwrap();
                        assert_eq!(env.kind, K::Data);
                        if target.offer(env.seq, b.to_vec()) == Offer::Bounced {
                            ctl_seq += 1;
                            let c = Control::Backpressure { to: "node-1".into(), seq: env.seq };
                            ws.send(Message::Binary(Envelope::control("", ctl_seq, &c).unwrap().encode().unwrap().into())).await.unwrap();
                            if let (Some(ms), false) = (rate_limit_first, told_rate) {
                                told_rate = true;
                                ctl_seq += 1;
                                let c = Control::RateLimited { retry_after_ms: ms };
                                ws.send(Message::Binary(Envelope::control("", ctl_seq, &c).unwrap().encode().unwrap().into())).await.unwrap();
                            }
                        }
                    }
                    // 第一次退回之前目标不读：机器忙的时候线程可能在两个分片之间被调度走一阵，目标趁机把缓冲读空，
                    // 第二片就不会被退回，用例要测的场景（和 `told_rate`）根本没发生
                    _ = tick.tick(), if target.bounces > 0 => {
                        for (_, f) in target.read(read_per_tick) {
                            let env = Envelope::decode(&f).unwrap();
                            let Some(ct) = reasm.push(frame::decode(&env.body).unwrap()).expect("fragments arrive in order") else { continue };
                            let plain = srv_t.decrypt(&ct).expect("the secure channel stays in sync");
                            let RpcMessage::Request(req) = RpcMessage::decode(&plain).unwrap() else { panic!("request expected") };
                            let len = req.params["blob"].as_str().unwrap().len();
                            seen.push(len);
                            let resp = RpcMessage::result(req.id, serde_json::json!({ "len": len })).encode();
                            let ct = srv_t.encrypt(&resp).unwrap();
                            ctl_seq += 1;
                            let b = Envelope::new("node-1", "dev-1", K::Data, ctl_seq, frame::encode(&ct, ctl_seq as u32).remove(0)).encode().unwrap();
                            ws.send(Message::Binary(b.into())).await.unwrap();
                        }
                    }
                }
            }
            (seen, target.bounces)
        });
        (addr, h)
    }

    async fn run_driver(
        addr: std::net::SocketAddr,
        cli_t: Transport,
        blobs: Vec<usize>,
    ) -> (Vec<serde_json::Value>, Duration, String) {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
        let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
        let (req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
        let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
        let run = tokio::spawn(driver.run(req_rx, note_tx));
        let t = std::time::Instant::now();
        let mut replies = Vec::new();
        for len in &blobs {
            let (reply, rx) = oneshot::channel();
            req_tx
                .send(Req { method: "x.y".into(), params: serde_json::json!({ "blob": "b".repeat(*len) }), reply })
                .unwrap();
            replies.push(rx);
        }
        let mut out = Vec::new();
        for rx in replies {
            out.push(tokio::time::timeout(Duration::from_secs(30), rx).await.expect("answered").unwrap().unwrap());
        }
        let took = t.elapsed();
        drop(req_tx);
        let reason = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        (out, took, reason)
    }

    #[tokio::test]
    async fn requests_cross_a_sticky_hub_with_a_tiny_buffer_in_order_and_intact() {
        let (srv_t, cli_t) = transport_pair();
        // 目标缓冲 300 KB、读得慢：每个 1 MB 的请求（5 片）都会被退回好几次
        let (addr, server) = sticky_node(300_000, 100_000, None, srv_t).await;
        let blobs = vec![1_000_000, 10, 1_200_000, 700_000];
        let (replies, _took, reason) = run_driver(addr, cli_t, blobs.clone()).await;
        assert_eq!(reason, "closed locally");
        let lens: Vec<usize> = replies.iter().map(|r| r["len"].as_u64().unwrap() as usize).collect();
        assert_eq!(lens, blobs, "every request arrives intact and answers come back in order");
        let (seen, bounces) = tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        assert_eq!(seen, blobs);
        assert!(bounces > 0, "the scenario really bounced envelopes");
    }

    #[tokio::test]
    async fn rate_limited_pauses_the_cli_for_retry_after_ms() {
        let (srv_t, cli_t) = transport_pair();
        // 缓冲只有 150 KB：第一个请求的第二片就被退回，同时 hub 说停 700 ms
        let (addr, server) = sticky_node(150_000, 1_000_000, Some(700), srv_t).await;
        let (replies, took, reason) = run_driver(addr, cli_t, vec![500_000]).await;
        assert_eq!(reason, "closed locally");
        assert_eq!(replies[0]["len"], 500_000);
        assert!(took >= Duration::from_millis(690), "nothing was resent before retry_after_ms: {took:?}");
        let (_, bounces) = tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        assert!(bounces > 0);
    }

    #[tokio::test]
    async fn a_bounced_seq_that_is_not_retained_ends_the_connection_instead_of_desyncing() {
        // 假 hub 对一个我们从没发过的 seq 回 backpressure
        let (_srv_t, cli_t) = transport_pair();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = l.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let c = Control::Backpressure { to: "node-1".into(), seq: 999 };
            ws.send(Message::Binary(Envelope::control("", 1, &c).unwrap().encode().unwrap().into())).await.unwrap();
            while ws.next().await.is_some() {}
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}")).await.unwrap();
        let driver = Driver::new(ws, cli_t, "dev-1".into(), "node-1".into(), 0);
        let (_req_tx, req_rx) = mpsc::unbounded_channel::<Req>();
        let (note_tx, _note_rx) = mpsc::unbounded_channel::<RpcNotification>();
        let reason = tokio::time::timeout(Duration::from_secs(5), driver.run(req_rx, note_tx)).await.unwrap();
        assert!(reason.contains("lost sync"), "{reason}");
        server.abort();
    }
}
