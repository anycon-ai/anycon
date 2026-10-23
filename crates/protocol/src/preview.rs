//! dev server 预览（ADR-39，docs/protocol.md § 11）：daemon 的 `preview.*` 方法、预览页的启动参数、
//! 隧道握手和隧道消息的编解码。
//!
//! 预览隧道是浏览器预览 origin 里的 service worker 和 daemon 之间的一条**独立 Noise 传输**
//! （`Noise_NKpsk0`，见 `anycon_crypto::Handshake::preview_initiator` / `preview_responder`），不跑 JSON-RPC：
//! 每条传输消息的明文是「8 字节累计回执 + 若干二进制帧」，帧按 stream 多路复用 HTTP 请求和 WebSocket。
//! hub 只看到外层信封；这里的内容都在密文里，[`PreviewLaunch`] 在 URL fragment 里，也不经过 hub。

use serde::{Deserialize, Serialize};

use crate::{DeviceId, SessionId, TimestampMs};

/// `preview.open` 参数：为本机 `port` 签发一张预览票据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewOpenParams {
    pub port: u16,
    /// 从哪个会话发起（只用于界面展示和审计；端口是否允许不看它，见 spec/daemon.md § 19.3）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// `preview.open` 结果。`psk` 是票据的密钥，只交给预览页（放进 [`PreviewLaunch`]），不要写日志、不要发给 hub。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewOpenResult {
    /// 这个端口固定的预览 label（26 位小写 base32），预览 origin 是 `<label>.<预览域名>`。
    pub label: String,
    pub port: u16,
    /// 票据 id，32 个小写 hex（16 字节）。
    pub ticket_id: String,
    /// 票据密钥，64 个小写 hex（32 字节），Noise PSK。
    pub psk: String,
    pub expires_at_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// `preview.close` 参数：作废这个 label 的全部票据，断开它的全部隧道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewCloseParams {
    pub label: String,
}

/// `preview.list` 的一项：一张未过期的票据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewInfo {
    pub label: String,
    pub port: u16,
    pub ticket_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// 签发这张票据的设备。
    pub created_by: DeviceId,
    pub created_by_name: String,
    pub created_at_ms: TimestampMs,
    pub expires_at_ms: TimestampMs,
    /// 此刻用这张票据建立的隧道数。
    pub tunnels: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewListResult {
    pub previews: Vec<PreviewInfo>,
}

/// `preview.ports` 参数。带 `session_id` 只列这个会话的端口（外加配置里允许的端口）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewPortsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// 这个端口为什么允许预览（spec/daemon.md § 19.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewPortSource {
    /// 监听它的进程属于某个会话的 agent 进程树（或同一进程组）。
    Session,
    /// 监听它的进程的工作目录在某个会话的工作目录里。
    Workspace,
    /// 写在 `preview.allow_ports` 里。
    Config,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewPort {
    pub port: u16,
    pub source: PreviewPortSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 进程名（可执行文件名，不含参数）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

/// 端口探测是否可用。`unavailable`：这台机器上没有可用的探测手段（如没有 `lsof`），只能预览配置里允许的端口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewDetection {
    Ok,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewPortsResult {
    pub ports: Vec<PreviewPort>,
    pub detection: PreviewDetection,
}

/// 隧道握手第二条消息（daemon → 预览页）的 payload，JSON。到这一条预览页才确认对端持有票据、是钉住的 daemon。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewHello {
    pub port: u16,
    pub expires_at_ms: TimestampMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// 预览页的启动参数。Web UI 把它序列化成 JSON、再 base64url（无填充），放进
/// `<预览 origin>/__anycon/enter#<…>` 的 fragment（浏览器不会把 fragment 发给 hub）。引导页读出后存进预览 origin 的
/// IndexedDB，立即从地址栏抹掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewLaunch {
    /// 格式版本，目前是 1。
    pub v: u32,
    /// daemon 的设备 id。
    pub node: DeviceId,
    /// daemon 的 Noise 静态公钥，64 个小写 hex。预览页用它做 NK 握手（钉住 daemon）。
    pub node_public: String,
    pub label: String,
    pub port: u16,
    pub ticket_id: String,
    pub psk: String,
    /// hub 签发的预览令牌（`POST /api/preview/tokens`），预览连接凭它向 hub 认证。
    pub token: String,
    /// 票据和令牌里较早的那个过期时间。
    pub expires_at_ms: TimestampMs,
    /// 打开后先导航到的路径（以 `/` 开头，可带查询串）。
    pub path: String,
}

/// 隧道 `REQUEST` 帧的载荷（msgpack，字段名为键）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewRequestHead {
    pub method: String,
    /// 以 `/` 开头，可带查询串；不含 scheme 和主机。
    pub path: String,
    /// 请求头，按原顺序，名字小写。`cookie` 来自预览页自己的 cookie jar；daemon 转发前改写哪些头见
    /// spec/daemon.md § 19.5。
    pub headers: Vec<(String, String)>,
    /// `true`：这是一个 WebSocket 升级请求（`method` 为 `GET`，子协议在 `sec-websocket-protocol` 头里）。
    #[serde(default)]
    pub websocket: bool,
}

/// 隧道里响应体的编码。`gzip`：`DATA` 帧拼起来是 gzip 流，预览页要先解压；`headers` 里已经去掉
/// `content-encoding` / `content-length`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewBodyEncoding {
    #[default]
    Identity,
    Gzip,
}

/// 隧道 `RESPONSE` 帧的载荷（msgpack）。WebSocket 流里 `status = 101` 表示升级成功，其他状态码表示被拒，
/// 后面跟普通响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub encoding: PreviewBodyEncoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewResetCode {
    /// 连不上本机端口。
    Refused,
    /// 上游超时（等响应头超过 120 秒）。
    Timeout,
    /// 头部或单条 WebSocket 消息超过上限。
    TooLarge,
    /// 对端不要了（页面取消了请求、关掉了 WebSocket）。
    Cancelled,
    /// 超过了对端给的流量窗口。
    FlowControl,
    /// 帧顺序或格式不对。
    Protocol,
    /// 同时打开的 stream 超过上限。
    Limit,
    Internal,
}

/// 隧道 `RESET` 帧的载荷（msgpack）：终止这个 stream 的两个方向。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewReset {
    pub code: PreviewResetCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewGoawayCode {
    /// 票据过期。
    Expired,
    /// 票据被 `preview.close` 作废。
    Closed,
    /// 签发票据的设备被吊销。
    Revoked,
    /// 节点关掉了预览（`preview.enabled = false`）。
    Disabled,
    /// 端口不再允许预览。
    PortNotAllowed,
    /// daemon 在退出。
    Shutdown,
}

/// 隧道 `GOAWAY` 帧的载荷（msgpack，只有 daemon 发，stream 0）：daemon 要关掉这条隧道，之后不再处理它的帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PreviewGoaway {
    pub code: PreviewGoawayCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// daemon 拒绝隧道握手的原因（握手回复 `0x01` 后面的 UTF-8 串）。这一条不受 Noise 保护，只能当提示用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PreviewRejectReason {
    /// 没有这张票据（被关掉、被吊销，或者根本不存在）。
    UnknownTicket,
    Expired,
    /// hub 告知的 label 和票据的不一致。
    LabelMismatch,
    PortNotAllowed,
    Disabled,
    /// 这条隧道或这台 daemon 的隧道数已满。
    Busy,
    Malformed,
}

impl PreviewRejectReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownTicket => "unknown_ticket",
            Self::Expired => "expired",
            Self::LabelMismatch => "label_mismatch",
            Self::PortNotAllowed => "port_not_allowed",
            Self::Disabled => "disabled",
            Self::Busy => "busy",
            Self::Malformed => "malformed",
        }
    }
}

/// 预览隧道的二进制格式和常量（docs/protocol.md § 11）。Rust 和 TS（`packages/protocol/src/preview.ts`）各一份，
/// 字节级一致由 `packages/protocol/tests/vectors/preview.json` 保证。
pub mod tunnel {
    use super::PreviewRejectReason;

    /// 预览 label 的长度：26 个字符的小写 base32（RFC 4648 字母表，无填充），约 130 bit。
    pub const LABEL_LEN: usize = 26;
    pub const LABEL_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
    /// 票据 id 的字节数。
    pub const TICKET_ID_LEN: usize = 16;
    /// 票据密钥（Noise PSK）的字节数。
    pub const PSK_LEN: usize = 32;

    /// 握手第一条消息 body 的版本字节。
    pub const HANDSHAKE_VERSION: u8 = 1;
    /// 握手回复 body 的第一个字节：接受（后面是 Noise 第二条消息）。
    pub const HANDSHAKE_ACCEPT: u8 = 0x00;
    /// 握手回复 body 的第一个字节：拒绝（后面是 UTF-8 原因，见 [`PreviewRejectReason`]）。
    pub const HANDSHAKE_REJECT: u8 = 0x01;
    /// Noise prologue 的前缀。完整 prologue = 前缀 ‖ 0x00 ‖ label ‖ 0x00 ‖ ticket_id（16 字节）。
    pub const PROLOGUE_PREFIX: &[u8] = b"anycon-preview-v1";

    /// 一条隧道消息的明文上限（加密后仍小于一个 `WHOLE` 数据帧，所以预览隧道从不分片）。
    pub const MAX_MESSAGE: usize = 128 * 1024;
    /// 单个 `DATA` / `WS_MESSAGE` 帧的数据上限。
    pub const MAX_DATA_FRAME: usize = 64 * 1024;
    /// `REQUEST` / `RESPONSE` 头部（msgpack 编码后）上限。
    pub const MAX_HEAD: usize = 64 * 1024;
    /// 一条 WebSocket 消息（拼起来之后）的上限。
    pub const MAX_WS_MESSAGE: usize = 16 * 1024 * 1024;
    /// 每个 stream 每个方向初始的流量窗口（`DATA` 和 `WS_MESSAGE` 的数据字节）。
    pub const STREAM_WINDOW: u32 = 256 * 1024;
    /// 一条隧道上同时打开的 stream 上限。
    pub const MAX_STREAMS: usize = 256;
    /// 发送方未被回执的密文上限：daemon → 预览页。
    pub const DAEMON_INFLIGHT: usize = 8 * 1024 * 1024;
    /// 发送方未被回执的密文上限：预览页 → daemon。
    pub const CLIENT_INFLIGHT: usize = 2 * 1024 * 1024;
    /// 收到这么多字节（或 [`ACK_EVERY_MESSAGES`] 条）还没回执过、手里又没有别的要发时，立即发一条纯回执消息。
    pub const ACK_EVERY_BYTES: usize = 256 * 1024;
    pub const ACK_EVERY_MESSAGES: u64 = 32;
    /// 收到消息后最多等这么久就发纯回执。
    pub const ACK_DELAY_MS: u64 = 50;
    /// WebSocket 关闭原因的字节上限（RFC 6455）。
    pub const WS_CLOSE_REASON_MAX: usize = 123;

    /// `WS_MESSAGE` 标志位：二进制消息（否则是文本）。
    pub const WS_BINARY: u8 = 0x01;
    /// `WS_MESSAGE` 标志位：这是这条 WebSocket 消息的最后一段。
    pub const WS_FIN: u8 = 0x02;

    const MESSAGE_HEADER: usize = 8;
    /// 帧头：类型 1 字节 + stream 4 字节 + 长度 4 字节。
    pub const FRAME_HEADER: usize = 1 + 4 + 4;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[repr(u8)]
    pub enum FrameType {
        /// C→D，msgpack [`super::PreviewRequestHead`]，打开一个 stream。
        Request = 0x01,
        /// D→C，msgpack [`super::PreviewResponseHead`]。
        Response = 0x02,
        /// 双向，HTTP 消息体的一段。占流量窗口。
        Data = 0x03,
        /// 双向，无载荷：这个方向的消息体结束。
        End = 0x04,
        /// 双向，msgpack [`super::PreviewReset`]：终止 stream。
        Reset = 0x05,
        /// 双向，4 字节大端：给对端这个 stream 增加的窗口。
        Window = 0x06,
        /// 双向，1 字节标志 + 数据：WebSocket 消息的一段。占流量窗口。
        WsMessage = 0x07,
        /// 双向，2 字节大端关闭码 + UTF-8 原因。
        WsClose = 0x08,
        /// D→C，stream 0，msgpack [`super::PreviewGoaway`]。
        Goaway = 0x09,
    }

    impl FrameType {
        pub fn from_u8(b: u8) -> Option<FrameType> {
            Some(match b {
                0x01 => FrameType::Request,
                0x02 => FrameType::Response,
                0x03 => FrameType::Data,
                0x04 => FrameType::End,
                0x05 => FrameType::Reset,
                0x06 => FrameType::Window,
                0x07 => FrameType::WsMessage,
                0x08 => FrameType::WsClose,
                0x09 => FrameType::Goaway,
                _ => return None,
            })
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Frame {
        pub ty: FrameType,
        pub stream: u32,
        pub payload: Vec<u8>,
    }

    impl Frame {
        pub fn new(ty: FrameType, stream: u32, payload: Vec<u8>) -> Frame {
            Frame { ty, stream, payload }
        }

        /// 编码后占的字节数。
        pub fn encoded_len(&self) -> usize {
            FRAME_HEADER + self.payload.len()
        }
    }

    /// 解密后的一条隧道消息。
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct Message {
        /// 发送方此刻已经收到对端多少条传输消息（这条传输建立以来，累计）。
        pub ack: u64,
        pub frames: Vec<Frame>,
    }

    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum TunnelError {
        #[error("truncated")]
        Truncated,
        #[error("message too large")]
        TooLarge,
        #[error("unsupported handshake version {0}")]
        Version(u8),
        #[error("malformed: {0}")]
        Malformed(&'static str),
    }

    /// 消息明文的字节数（编码前就能算，用来决定还能不能再塞一帧）。
    pub fn message_len(frames: &[Frame]) -> usize {
        MESSAGE_HEADER + frames.iter().map(Frame::encoded_len).sum::<usize>()
    }

    /// 编码一条隧道消息。超过 [`MAX_MESSAGE`] 返回 `TooLarge`（调用方负责把帧分到多条消息里）。
    pub fn encode_message(ack: u64, frames: &[Frame]) -> Result<Vec<u8>, TunnelError> {
        let len = message_len(frames);
        if len > MAX_MESSAGE {
            return Err(TunnelError::TooLarge);
        }
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&ack.to_be_bytes());
        for f in frames {
            out.push(f.ty as u8);
            out.extend_from_slice(&f.stream.to_be_bytes());
            out.extend_from_slice(&(f.payload.len() as u32).to_be_bytes());
            out.extend_from_slice(&f.payload);
        }
        Ok(out)
    }

    /// 解码一条隧道消息。未知类型的帧跳过（前向兼容）；长度越界、超过上限报错。
    pub fn decode_message(buf: &[u8]) -> Result<Message, TunnelError> {
        if buf.len() > MAX_MESSAGE {
            return Err(TunnelError::TooLarge);
        }
        if buf.len() < MESSAGE_HEADER {
            return Err(TunnelError::Truncated);
        }
        let ack = u64::from_be_bytes(buf[..8].try_into().expect("8 bytes"));
        let mut rest = &buf[MESSAGE_HEADER..];
        let mut frames = Vec::new();
        while !rest.is_empty() {
            if rest.len() < FRAME_HEADER {
                return Err(TunnelError::Truncated);
            }
            let ty = rest[0];
            let stream = u32::from_be_bytes(rest[1..5].try_into().expect("4 bytes"));
            let len = u32::from_be_bytes(rest[5..9].try_into().expect("4 bytes")) as usize;
            if rest.len() - FRAME_HEADER < len {
                return Err(TunnelError::Truncated);
            }
            let payload = &rest[FRAME_HEADER..FRAME_HEADER + len];
            if let Some(ty) = FrameType::from_u8(ty) {
                frames.push(Frame { ty, stream, payload: payload.to_vec() });
            }
            rest = &rest[FRAME_HEADER + len..];
        }
        Ok(Message { ack, frames })
    }

    /// `REQUEST` / `RESPONSE` / `RESET` / `GOAWAY` 的载荷：msgpack，字段名为键（和外层信封同一种编码）。
    pub fn encode_head<T: serde::Serialize>(head: &T) -> Result<Vec<u8>, TunnelError> {
        let bytes = rmp_serde::to_vec_named(head).map_err(|_| TunnelError::Malformed("head encode"))?;
        if bytes.len() > MAX_HEAD {
            return Err(TunnelError::TooLarge);
        }
        Ok(bytes)
    }

    pub fn decode_head<T: serde::de::DeserializeOwned>(payload: &[u8]) -> Result<T, TunnelError> {
        if payload.len() > MAX_HEAD {
            return Err(TunnelError::TooLarge);
        }
        rmp_serde::from_slice(payload).map_err(|_| TunnelError::Malformed("head decode"))
    }

    pub fn window(increment: u32) -> Vec<u8> {
        increment.to_be_bytes().to_vec()
    }

    pub fn parse_window(payload: &[u8]) -> Result<u32, TunnelError> {
        let b: [u8; 4] = payload.try_into().map_err(|_| TunnelError::Malformed("window"))?;
        Ok(u32::from_be_bytes(b))
    }

    pub fn ws_message(flags: u8, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + data.len());
        out.push(flags);
        out.extend_from_slice(data);
        out
    }

    /// 返回 `(flags, data)`。
    pub fn parse_ws_message(payload: &[u8]) -> Result<(u8, &[u8]), TunnelError> {
        let (&flags, data) = payload.split_first().ok_or(TunnelError::Malformed("ws message"))?;
        Ok((flags, data))
    }

    /// 原因超过 [`WS_CLOSE_REASON_MAX`] 字节时在字符边界处截断。
    pub fn ws_close(code: u16, reason: &str) -> Vec<u8> {
        let mut end = reason.len().min(WS_CLOSE_REASON_MAX);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = Vec::with_capacity(2 + end);
        out.extend_from_slice(&code.to_be_bytes());
        out.extend_from_slice(&reason.as_bytes()[..end]);
        out
    }

    pub fn parse_ws_close(payload: &[u8]) -> Result<(u16, String), TunnelError> {
        if payload.len() < 2 {
            return Err(TunnelError::Malformed("ws close"));
        }
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        let reason = std::str::from_utf8(&payload[2..]).map_err(|_| TunnelError::Malformed("ws close reason"))?;
        Ok((code, reason.to_string()))
    }

    /// 握手第一条（预览页 → daemon）的信封 body：版本 ‖ ticket_id ‖ Noise 消息。
    pub fn handshake_init(ticket_id: &[u8; TICKET_ID_LEN], noise: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + TICKET_ID_LEN + noise.len());
        out.push(HANDSHAKE_VERSION);
        out.extend_from_slice(ticket_id);
        out.extend_from_slice(noise);
        out
    }

    /// 返回 `(ticket_id, Noise 消息)`。
    pub fn parse_handshake_init(body: &[u8]) -> Result<([u8; TICKET_ID_LEN], &[u8]), TunnelError> {
        let (&version, rest) = body.split_first().ok_or(TunnelError::Truncated)?;
        if version != HANDSHAKE_VERSION {
            return Err(TunnelError::Version(version));
        }
        if rest.len() < TICKET_ID_LEN {
            return Err(TunnelError::Truncated);
        }
        let id: [u8; TICKET_ID_LEN] = rest[..TICKET_ID_LEN].try_into().expect("16 bytes");
        Ok((id, &rest[TICKET_ID_LEN..]))
    }

    pub fn handshake_accept(noise: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + noise.len());
        out.push(HANDSHAKE_ACCEPT);
        out.extend_from_slice(noise);
        out
    }

    pub fn handshake_reject(reason: PreviewRejectReason) -> Vec<u8> {
        let mut out = vec![HANDSHAKE_REJECT];
        out.extend_from_slice(reason.as_str().as_bytes());
        out
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum HandshakeReply<'a> {
        Accept(&'a [u8]),
        /// 原因保持为字符串：新版本 daemon 可能给出这一版不认识的原因。
        Reject(String),
    }

    pub fn parse_handshake_reply(body: &[u8]) -> Result<HandshakeReply<'_>, TunnelError> {
        let (&kind, rest) = body.split_first().ok_or(TunnelError::Truncated)?;
        match kind {
            HANDSHAKE_ACCEPT => Ok(HandshakeReply::Accept(rest)),
            HANDSHAKE_REJECT => {
                let reason = std::str::from_utf8(rest).map_err(|_| TunnelError::Malformed("reject reason"))?;
                Ok(HandshakeReply::Reject(reason.to_string()))
            }
            _ => Err(TunnelError::Malformed("handshake reply")),
        }
    }

    /// Noise prologue：两端各自算，hub 告知的 label（daemon 侧）和预览页自己的主机名（浏览器侧）不一致时握手失败。
    pub fn prologue(label: &str, ticket_id: &[u8; TICKET_ID_LEN]) -> Vec<u8> {
        let mut out = Vec::with_capacity(PROLOGUE_PREFIX.len() + 2 + label.len() + TICKET_ID_LEN);
        out.extend_from_slice(PROLOGUE_PREFIX);
        out.push(0);
        out.extend_from_slice(label.as_bytes());
        out.push(0);
        out.extend_from_slice(ticket_id);
        out
    }

    pub fn is_valid_label(label: &str) -> bool {
        label.len() == LABEL_LEN && label.bytes().all(|b| LABEL_ALPHABET.contains(&b))
    }

    /// 把 16 字节随机数编码成 label（base32 小写，无填充：128 bit → 26 个字符）。
    pub fn label_from_bytes(bytes: &[u8; 16]) -> String {
        let mut out = String::with_capacity(LABEL_LEN);
        let mut acc: u32 = 0;
        let mut bits = 0;
        for &b in bytes {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(LABEL_ALPHABET[((acc >> bits) & 0x1f) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(LABEL_ALPHABET[((acc << (5 - bits)) & 0x1f) as usize] as char);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::tunnel::*;
    use super::*;

    #[test]
    fn message_roundtrip_and_unknown_frames_are_skipped() {
        let frames = vec![
            Frame::new(FrameType::Request, 1, vec![1, 2, 3]),
            Frame::new(FrameType::End, 1, vec![]),
            Frame::new(FrameType::Window, 3, window(4096)),
        ];
        let bytes = encode_message(7, &frames).unwrap();
        assert_eq!(bytes.len(), message_len(&frames));
        let m = decode_message(&bytes).unwrap();
        assert_eq!(m.ack, 7);
        assert_eq!(m.frames, frames);

        // 插一个未知类型 0x7f 的帧：解码时跳过，其余帧不受影响
        let mut with_unknown = bytes[..8].to_vec();
        with_unknown.extend_from_slice(&[0x7f, 0, 0, 0, 9, 0, 0, 0, 2, 0xaa, 0xbb]);
        with_unknown.extend_from_slice(&bytes[8..]);
        assert_eq!(decode_message(&with_unknown).unwrap().frames, frames);

        // 纯回执消息
        let ack_only = encode_message(42, &[]).unwrap();
        assert_eq!(decode_message(&ack_only).unwrap(), Message { ack: 42, frames: vec![] });
    }

    #[test]
    fn message_limits() {
        let big = Frame::new(FrameType::Data, 1, vec![0; MAX_MESSAGE]);
        assert_eq!(encode_message(0, &[big]), Err(TunnelError::TooLarge));
        assert_eq!(decode_message(&[0; 7]), Err(TunnelError::Truncated));
        // 帧长度越界
        let mut bad = vec![0u8; 8];
        bad.extend_from_slice(&[0x03, 0, 0, 0, 1, 0, 0, 0, 5, 1, 2]);
        assert_eq!(decode_message(&bad), Err(TunnelError::Truncated));
    }

    #[test]
    fn heads_roundtrip() {
        let req = PreviewRequestHead {
            method: "GET".into(),
            path: "/src/main.ts?t=1".into(),
            headers: vec![("accept".into(), "*/*".into())],
            websocket: false,
        };
        let back: PreviewRequestHead = decode_head(&encode_head(&req).unwrap()).unwrap();
        assert_eq!(back, req);
        let res = PreviewResponseHead { status: 200, headers: vec![], encoding: PreviewBodyEncoding::Gzip };
        let back: PreviewResponseHead = decode_head(&encode_head(&res).unwrap()).unwrap();
        assert_eq!(back, res);
        // 缺省字段
        #[derive(serde::Serialize)]
        struct Old {
            status: u16,
            headers: Vec<(String, String)>,
        }
        let old: PreviewResponseHead =
            decode_head(&encode_head(&Old { status: 204, headers: vec![] }).unwrap()).unwrap();
        assert_eq!(old.encoding, PreviewBodyEncoding::Identity);
    }

    #[test]
    fn ws_payloads() {
        let p = ws_message(WS_BINARY | WS_FIN, b"hi");
        assert_eq!(parse_ws_message(&p).unwrap(), (WS_BINARY | WS_FIN, &b"hi"[..]));
        let long = "é".repeat(100); // 200 字节
        let c = ws_close(1000, &long);
        let (code, reason) = parse_ws_close(&c).unwrap();
        assert_eq!(code, 1000);
        assert!(reason.len() <= WS_CLOSE_REASON_MAX && long.starts_with(&reason));
        assert_eq!(parse_window(&window(7)).unwrap(), 7);
        assert!(parse_window(&[1, 2]).is_err());
    }

    #[test]
    fn handshake_bodies() {
        let id = [9u8; TICKET_ID_LEN];
        let init = handshake_init(&id, b"noise");
        assert_eq!(parse_handshake_init(&init).unwrap(), (id, &b"noise"[..]));
        let mut wrong = init.clone();
        wrong[0] = 2;
        assert_eq!(parse_handshake_init(&wrong), Err(TunnelError::Version(2)));
        assert_eq!(parse_handshake_reply(&handshake_accept(b"m2")).unwrap(), HandshakeReply::Accept(b"m2"));
        assert_eq!(
            parse_handshake_reply(&handshake_reject(PreviewRejectReason::PortNotAllowed)).unwrap(),
            HandshakeReply::Reject("port_not_allowed".into())
        );
        // as_str 和 serde 的名字一致
        for r in [
            PreviewRejectReason::UnknownTicket,
            PreviewRejectReason::Expired,
            PreviewRejectReason::LabelMismatch,
            PreviewRejectReason::PortNotAllowed,
            PreviewRejectReason::Disabled,
            PreviewRejectReason::Busy,
            PreviewRejectReason::Malformed,
        ] {
            assert_eq!(serde_json::to_value(r).unwrap(), serde_json::json!(r.as_str()));
        }
        let p = prologue("abc", &id);
        assert!(p.starts_with(PROLOGUE_PREFIX) && p.ends_with(&id));
    }

    #[test]
    fn labels() {
        let l = label_from_bytes(&[0xff; 16]);
        assert_eq!(l.len(), LABEL_LEN);
        assert!(is_valid_label(&l));
        assert_eq!(label_from_bytes(&[0; 16]), "a".repeat(LABEL_LEN));
        assert!(!is_valid_label("ABCDEFGHIJKLMNOPQRSTUVWXYZ"));
        assert!(!is_valid_label("short"));
        assert!(!is_valid_label(&format!("{}1", "a".repeat(25)))); // 1 不在字母表里
    }
}
