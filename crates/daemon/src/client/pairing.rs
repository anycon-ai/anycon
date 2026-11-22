//! 短码配对的加入方（Noise XXpsk3 initiator，ADR-29）。
//!
//! 短码只在本机出现——发给 hub 的是 `match_id`；握手完成、收到 `PairDone` 之后还要等对端经新建立的 Noise 传输发来的
//! `pair.ack`，校验 `peer_id` 是自己的正式 id、`node_id` 等于 `PairDone.peer`、`fingerprint` 等于握手得到的节点公钥的指纹，
//! **通过才落盘**；hub 伪造 `PairDone` 骗不到它（docs/protocol.md § 4.1）。
//!
//! `ClientInfo` 由调用方给（CLI 是 `cli`，分身设备是 `twin`），它随第 3 条握手消息发给对端，对端据此记设备名和种类。

use std::time::Duration;

use anycon_crypto::{fingerprint, Handshake, Keypair, Role, Transport};
use anycon_protocol::{AuthFailReason, ClientInfo, Control, Kind, PairAck, PairFailReason, PairRole};

use super::error::{ClientError, ErrorClass, Result};
use super::hubws::{ConnectFail, HubWs, STEP};
use super::state::{host_of, sanitize_label, HubState, NodeEntry, NAME_MAX_CHARS};
use crate::config::Paths;

/// `PairDone` 之后等对端的 `pair.ack` 最多多久；超时 = 配对失败、什么都不落盘。
pub const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// 加入方的撮合和握手：发 `pair_join`，等 `pair_matched`，和节点（临时 id）走完 XXpsk3 握手，再发 `pair_confirm`。
/// 返回配对传输和握手得到的节点公钥；之后由 [`finish_pairing`] 等 `pair_done` 和 `pair.ack`。
pub async fn join_handshake(
    h: &mut HubWs,
    kp: &Keypair,
    psk: &[u8; 32],
    match_id: String,
    client: &ClientInfo,
) -> Result<(Transport, [u8; 32])> {
    h.send_control(&Control::PairJoin { match_id }).await?;
    let host_temp =
        match h.wait_control(STEP, |c| matches!(c, Control::PairMatched { .. } | Control::PairFailed { .. })).await? {
            Control::PairMatched { peer, role } => {
                if role != PairRole::Joiner {
                    return Err(ClientError::general("unexpected pairing role"));
                }
                peer
            }
            Control::PairFailed { reason } => return Err(pair_failed(reason)),
            _ => unreachable!(),
        };
    let mut hs = Handshake::pairing(Role::Initiator, kp, psk).map_err(|e| ClientError::general(e.to_string()))?;
    let m1 = hs.write(&[]).map_err(|e| ClientError::general(e.to_string()))?;
    h.send_env(&host_temp, Kind::Handshake, m1).await?;
    let m2 = loop {
        match h.recv(STEP).await? {
            Some(env) if env.kind == Kind::Handshake && env.from == host_temp => break env.body,
            Some(env) if env.kind == Kind::Control => {
                if let Ok(Control::PairFailed { reason }) = env.control_body() {
                    return Err(pair_failed(reason));
                }
                h.bystanders.push_back(env);
            }
            Some(env) => h.bystanders.push_back(env),
            None => return Err(ClientError::offline("the node did not answer the pairing handshake")),
        }
    };
    // 第 2 条的 payload（节点名等）不受 PSK 保护，不信、不用；节点名取自后面认证过的 `pair.ack`
    hs.read(&m2).map_err(|e| ClientError::general(format!("pairing handshake failed: {e}")))?;
    // 第三条的 payload 带上本设备信息（对端据此记名字和类型）
    let info = serde_json::to_vec(client)?;
    let m3 = hs.write(&info).map_err(|e| ClientError::general(e.to_string()))?;
    let node_pub = hs.remote_public().ok_or_else(|| ClientError::general("pairing finished without a node key"))?;
    let pair_transport = hs.into_transport().map_err(|e| ClientError::general(e.to_string()))?;
    h.send_env(&host_temp, Kind::Handshake, m3).await?;
    h.send_control(&Control::PairConfirm { peer: host_temp }).await?;
    Ok((pair_transport, node_pub))
}

/// 校验 `pair.ack`：`peer_id` 是自己的正式 id，`node_id` 是 hub 在 `PairDone` 里说的节点，指纹是握手得到的节点公钥的指纹。
/// 任何一项不符都不落盘。
pub fn verify_pair_ack(ack: &PairAck, my_id: &str, node_id: &str, node_pub: &[u8; 32]) -> Result<()> {
    let bad = |what: &str| {
        ClientError::new(
            ErrorClass::NotPaired,
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

/// 作为 joiner 用短码配对（`anycon connect`）。`client` 是本设备自报的信息（名字、种类）。成功（`pair.ack` 校验通过）后
/// 才把节点记入本地状态。
pub async fn pair(paths: &Paths, hub_url: &str, code_arg: &str, client: ClientInfo) -> Result<NodeEntry> {
    pair_with(paths, hub_url, code_arg, client, ACK_TIMEOUT).await
}

/// 短码换出 PSK 和 `match_id`（一次 Argon2id，CPU 密集，放进 blocking 线程）。
pub async fn derive_secrets(code: &str) -> Result<(zeroize::Zeroizing<[u8; 32]>, String)> {
    let code = code.to_string();
    let secrets = tokio::task::spawn_blocking(move || anycon_crypto::derive_pair_secrets(&code))
        .await
        .map_err(|e| ClientError::general(format!("pairing key derivation failed: {e}")))?
        .map_err(|_| ClientError::usage("invalid pairing code"))?;
    // PSK 的局部副本用 `Zeroizing`：离开作用域时清零（`secrets` 自己 Drop 时也清零）
    let out = (zeroize::Zeroizing::new(secrets.psk), secrets.match_id.clone());
    drop(secrets);
    Ok(out)
}

/// [`pair`]，`ack_timeout` 可调（测试用）。
pub(crate) async fn pair_with(
    paths: &Paths,
    hub_url: &str,
    code_arg: &str,
    client: ClientInfo,
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    let hub_url = hub_url.trim_end_matches('/').to_string();
    let dir = paths.client_dir(&host_of(&hub_url)?);
    let mut state = HubState::load(&dir, &hub_url);
    state.check()?;
    state.hub_url = hub_url;
    pair_state(&mut state, client, code_arg, ack_timeout).await
}

/// 用一条新的 hub 连接、以 `state` 里的设备身份（没有就生成新密钥）配对；成功后 `state` 已更新并落盘。
/// 带着已存的 token 认证：同一设备在 hub 上只留一条连接，所以别在同一个身份已经有长连接（[`super::Client`]）的时候调它。
pub async fn pair_state(
    state: &mut HubState,
    client: ClientInfo,
    code_arg: &str,
    ack_timeout: Duration,
) -> Result<NodeEntry> {
    let code = parse_code_arg(code_arg)?;
    // 一次 Argon2id（64 MiB，约 0.1–0.3 s）同时得到 match_id 和 PSK
    let (psk, match_id) = derive_secrets(&code).await?;
    let hub_url = state.hub_url.clone();
    let kp = match state.keypair() {
        Some(k) => k,
        None => {
            let secret = crate::util::random_bytes::<32>();
            state.secret = Some(secret);
            Keypair::from_secret(secret)
        }
    };
    // 带着已存的 token 认证；hub 说 invalid / revoked（被吊销、hub 丢了数据）就丢掉旧身份，以临时连接重新配对
    let mut h = match HubWs::connect_raw(&hub_url, state.token.clone(), client.clone(), None).await {
        Ok(h) => h,
        Err(ConnectFail::Auth(AuthFailReason::Invalid | AuthFailReason::Revoked)) if state.token.is_some() => {
            super::notice("note: the hub no longer accepts this device's saved credentials; pairing as a new device");
            state.forget_credentials()?;
            HubWs::connect(&hub_url, None, client.clone()).await?
        }
        Err(f) => return Err(f.into_error()),
    };
    let (mut pair_transport, node_pub) = join_handshake(&mut h, &kp, &psk, match_id, &client).await?;
    finish_pairing(state, &mut h, &mut pair_transport, node_pub, ack_timeout).await
}

/// 握手（含第 3 条和 `pair_confirm`）之后的收尾，短码配对和账号内接入共用：等 `pair_done`，再等 daemon 经配对传输发来的
/// `pair.ack`，校验 `peer_id`、`node_id`、公钥指纹，**通过才落盘**；本地已有同 id 但公钥不同的节点报 `key_mismatch`、不覆盖。
pub async fn finish_pairing(
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
pub struct PairDoneInfo {
    pub peer: String,
    pub device_id: Option<String>,
    pub device_token: Option<String>,
}

/// [`finish_pairing`] 的后半段：`pair_done` 已经在手里（账号内接入自己在读 `pair_done`，因为 daemon 随时可能先回一条 `rejected`）。
pub async fn complete_pairing(
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
            return Err(ClientError::new(
                ErrorClass::NotPaired,
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
    let entry = NodeEntry { id: node_id, name, pubkey, paired_at_ms: crate::util::now_ms() };
    state.device_id = Some(my_id);
    if let Some(t) = token {
        state.token = Some(t);
        state.token_issued_at_ms = Some(crate::util::now_ms());
    }
    state.nodes.retain(|n| n.id != entry.id);
    state.nodes.push(entry.clone());
    state.save()?;
    Ok(entry)
}

pub fn pair_failed(reason: PairFailReason) -> ClientError {
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
    ClientError::new(ErrorClass::NotPaired, "pair_failed", msg)
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
        Err(ClientError::usage(
            "invalid pairing code (expected 8 or 16 characters like 7K3P-Q2MX, or the link printed by `anycon pair`)",
        ))
    }
}
