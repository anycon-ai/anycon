//! hub ↔ 设备的控制消息（`kind = control`）。msgpack，内部标签 `type`。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, TimestampMs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Daemon,
    Web,
    Cli,
    Sdk,
}

/// 设备自报信息。配对和每次 `auth` 都带。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ClientInfo {
    /// 显示名，如 "mac-mini"、"Chrome · macOS · hub.example.com"。
    pub name: String,
    pub kind: DeviceKind,
    /// 如 "macos-aarch64"、"linux-x86_64"、"browser"。
    pub platform: String,
    /// 客户端版本。
    pub version: String,
    /// 仅 web：访问 hub 的 origin，用于区分局域网 / 公网两份设备。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailReason {
    /// token 不存在或已轮换。
    Invalid,
    /// 设备已被吊销。
    Revoked,
    /// 协议版本不兼容。
    Version,
    /// 账号配额（仅 multi）。
    Quota,
    /// 第一帧不是 auth 或超时。
    Protocol,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PairFailReason {
    /// 短码不存在、过期或已被用掉。
    NotFound,
    /// 60 秒内双方未确认。
    Timeout,
    /// 对端在握手期间断开。
    PeerGone,
    /// 频率限制。
    RateLimited,
    /// 配额。
    Quota,
    /// 账号不匹配。
    Forbidden,
}

/// `enroll_failed.reason`（ADR-36，protocol.md § 4.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollFailReason {
    /// 发起方没有账号（既不是正式设备，升级请求也没带登录态）。
    Forbidden,
    /// `node` 不是本账号未吊销的 daemon（不区分「不存在」和「不是你的」），或 `enroll_id` 不存在。
    NotFound,
    /// 目标 daemon 不在线。
    Offline,
    /// 频率限制。
    RateLimited,
    /// 每日配对与接入配额（仅 multi）。
    Quota,
    /// 这台 daemon 进行中的接入已满（3 个），或这条连接已有一个接入在进行。
    Busy,
    /// 5 分钟内没走到 `pair_done`。
    Timeout,
    /// 对端断开。
    PeerGone,
    /// 对端 `enroll_cancel`。
    Cancelled,
}

/// 配对中的角色：daemon（或已配对设备代发）是 `host`，新设备是 `joiner`。
/// Noise XXpsk3 由 **joiner 发起**（initiator），host 是 responder。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PairRole {
    Host,
    Joiner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DropReason {
    /// 超过 `ANYCON_MAX_ENVELOPE_BYTES`。
    TooLarge,
    /// `(from, to)` 未配对。
    NotPaired,
    /// 每连接限速。
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Quota {
    pub devices: u32,
    pub devices_max: Option<u32>,
    pub daemons: u32,
    pub daemons_max: Option<u32>,
    pub connections: u32,
    pub connections_max: Option<u32>,
}

/// 控制消息集合。方向见注释：D = 设备，H = hub。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// D→H 连接后第一帧。`device_token` 为空表示「还没有身份的临时连接」：hub 分配临时 device id，
    /// 只允许 `pair_join`、`pair_confirm`、`enroll_begin`（升级请求要带账号登录态）、`enroll_cancel` 和撮合后的握手帧；`client.kind = daemon` 的临时连接还可以发
    /// `pair_begin`（daemon 首次启动靠这个拿到身份）。`pair_done` 后同一条连接升级为正式设备。
    Auth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
        protocol_version: u32,
        client: ClientInfo,
    },
    /// H→D
    AuthOk {
        device_id: DeviceId,
        server_time_ms: TimestampMs,
        /// 已配对且当前在线的对端。
        peers_online: Vec<DeviceId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quota: Option<Quota>,
    },
    /// H→D，随后 hub 关闭连接。
    AuthFail { reason: AuthFailReason },

    /// D→H：发起一次配对。**短码由 host 本地生成，不发给 hub**；这里只发 `match_id`（`derive_pair_secrets(code)`
    /// 的前 8 字节 hex，Argon2id），hub 用它撮合。正式设备都可以发（已配对客户端是替 daemon 发）；没有 token 的临时连接
    /// 只有 `kind = daemon` 的可以发，用于 daemon 首次获取身份。hub 不记录 match_id（它是短码的函数，拿到就能离线穷举）。
    PairBegin { match_id: String },
    /// H→D：已登记，5 分钟内有效。短码和链接由 host 自己持有 / 生成（`NodePairBeginResult`）。
    PairRegistered { expires_at_ms: TimestampMs },
    /// D→H：新设备输入短码后，发 `derive_match_id(code)`。
    PairJoin { match_id: String },
    /// H→D：双方都收到，之后开始互发 `handshake` 信封（`to` 填 `peer`）。
    PairMatched { peer: DeviceId, role: PairRole },
    /// D→H：Noise 握手成功后双方各发一次。账号内接入的收尾也用它（`peer` 是接入会话里的对端）。
    PairConfirm { peer: DeviceId },
    /// H→D：两边都确认后发给双方。`peer` 是对方的**正式** id。之前没有身份的一方（临时连接的 joiner，
    /// 或首次配对的 daemon host）收到 `device_id` + `device_token`，同一条连接原地升级为正式设备，
    /// 之后的信封 `from` 必须换成新 id；已是正式设备的一方这两个字段为空。紧接着双方各收到一条对方的 `peer_online`。
    /// **joiner 此时还不能落盘**：要等 daemon 通过刚建立的 Noise 传输发来 `pair.ack`（见 `methods::PAIR_ACK`），
    /// 校验其中的 id 与 `PairDone` 一致后才记录节点。hub 伪造 `PairDone` 无法伪造 `pair.ack`。
    PairDone {
        peer: DeviceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_id: Option<DeviceId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
    },
    /// H→D
    PairFailed { reason: PairFailReason },

    /// D→H：账号内接入（ADR-36）。请求接入本账号的 daemon `node`。本账号的正式设备，或升级请求带本账号 web 登录态
    /// （cookie）的临时连接才能发；`kind = daemon` 的连接不能发。
    EnrollBegin { node: DeviceId },
    /// H→D（发起方）：接入会话已建立，之后双方的 `handshake` 信封（body 是 `EnrollFrame`）按它放行。5 分钟有效。
    EnrollStarted { enroll_id: String, node: DeviceId, expires_at_ms: TimestampMs },
    /// H→D（daemon）：有设备请求接入。`peer` 是发起方的设备 id（临时连接是临时 id），`handshake` 信封的 `to` 填它。
    EnrollRequest { enroll_id: String, peer: DeviceId, expires_at_ms: TimestampMs },
    /// D→H：任一方结束接入会话，hub 给另一方发 `enroll_failed { cancelled }`。
    EnrollCancel { enroll_id: String },
    /// H→D
    EnrollFailed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enroll_id: Option<String>,
        reason: EnrollFailReason,
    },

    /// D→H：删除某设备的路由与配对（需与之配对）。daemon 侧的公钥删除另走 `node.revoke_device`。
    Revoke { device: DeviceId },
    /// H→D
    PeerOnline { device: DeviceId },
    /// H→D
    PeerOffline { device: DeviceId },
    /// H→D：对端离线且队列未开启，`seq` 那封已丢弃。
    Undeliverable { to: DeviceId, seq: u64 },
    /// H→D：队列开启时超限丢弃。
    QueueOverflow { to: DeviceId, dropped_seqs: Vec<u64> },
    /// D→H：撤回自己排队中的信封。
    QueueDrop { seq: u64 },
    /// H→D：`seq` 那封被退回（目标连接的发送缓冲满，或发送方超过速率预算）。`data` / `notify` 的退回会进这对设备
    /// 的「被退回 seq 有序集合」，hub 之后只放行其中最小的；发送方暂停后把所有被退回的 `seq` 按升序原样重发。
    /// `handshake` 被退回不进集合，发送方按 `retry_after_ms` 延后再握手（protocol.md § 3.1）。
    Backpressure { to: DeviceId, seq: u64 },
    /// H→D：`seq` 那封被 hub 丢弃（不是因为对端离线）。`too_large` 时发送方应改为分片（见 `envelope::frame`）。
    Dropped { seq: u64, reason: DropReason },
    /// H→D：发送方超过每连接速率预算，整条连接暂停 `retry_after_ms`（100 ms–30 s）再继续；伴随 `backpressure`
    /// 出现，信封本身没有丢。每连接每秒至多一条。
    RateLimited { retry_after_ms: u64 },
    /// H→D：配额用量（连接时和变化时）。
    QuotaUpdate { quota: Quota },
    /// D→H：轮换设备 token（v0.2）。
    RotateToken {},
    /// H→D
    TokenRotated { device_token: String },
}

/// WebSocket close code。
pub mod close_code {
    pub const SUPERSEDED: u16 = 4001;
    pub const RESTARTING: u16 = 4002;
    pub const SLOW_CONSUMER: u16 = 4003;
    pub const REVOKED: u16 = 4004;
    pub const AUTH_TIMEOUT: u16 = 4005;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_msgpack_roundtrip() {
        let c = Control::PairJoin { match_id: "0123456789abcdef".into() };
        let bytes = rmp_serde::to_vec_named(&c).unwrap();
        let back: Control = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, c);
    }
}
