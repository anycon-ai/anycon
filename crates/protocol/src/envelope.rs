//! 外层信封：hub 唯一能看懂的东西。MessagePack 编码（rmp-serde，紧凑数组格式关闭，用字段名）。

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// 信封种类。hub 对每种的处理见 docs/protocol.md § 3。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Noise 握手消息。hub 转发；配对期间按短码撮合。
    Handshake,
    /// Noise 密文。hub 转发；目标离线回 `undeliverable`。
    Data,
    /// `NotifyBody`（明文类别 + 可选密文详情）。hub 转发并触发 Web Push。
    Notify,
    /// hub ↔ 设备的控制消息，body 是 msgpack 的 [`crate::Control`]。hub 自己处理，不转发。
    Control,
}

/// 外层信封。**这是唯一含原始字节的类型，不进 JSON Schema**；TS 侧的编解码手写（packages/protocol/src/envelope.ts）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// 信封版本，见 [`crate::ENVELOPE_VERSION`]。
    pub v: u8,
    /// 发送设备。hub 校验它等于当前连接的设备 id。
    pub from: DeviceId,
    /// 目标设备。`control` 消息发给 hub 时填空字符串。
    pub to: DeviceId,
    pub kind: Kind,
    /// 发送方单调递增，用于去重、`undeliverable` 和 `backpressure` 回指。
    pub seq: u64,
    #[serde(with = "serde_bytes")]
    pub body: Vec<u8>,
}

impl Envelope {
    pub fn new(from: impl Into<DeviceId>, to: impl Into<DeviceId>, kind: Kind, seq: u64, body: Vec<u8>) -> Self {
        Self { v: crate::ENVELOPE_VERSION, from: from.into(), to: to.into(), kind, seq, body }
    }

    pub fn control(
        from: impl Into<DeviceId>,
        seq: u64,
        msg: &crate::Control,
    ) -> Result<Self, rmp_serde::encode::Error> {
        Ok(Self::new(from, "", Kind::Control, seq, rmp_serde::to_vec_named(msg)?))
    }

    pub fn encode(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
        rmp_serde::from_slice(bytes)
    }

    pub fn control_body(&self) -> Result<crate::Control, rmp_serde::decode::Error> {
        rmp_serde::from_slice(&self.body)
    }
}

/// `kind = notify` 的 body（msgpack）。`category` 是外层唯一有语义的明文字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotifyBody {
    pub category: NotifyCategory,
    /// 会话 id，明文。随机不透明串，只用于客户端点开通知后直达。
    pub session_id: Option<String>,
    /// Noise 密文详情，base64。客户端解密后得到 [`crate::NotifyDetail`]。
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotifyCategory {
    PermissionRequested,
    TurnFinished,
    Error,
    SessionIdle,
    /// 有设备请求批准接入（ADR-36）。没有 `session_id`。
    EnrollRequested,
    /// 接入相关的安全事件：新设备经接入加入、口令连续失败 / 锁定、口令或 `[enroll]` 被改。没有 `session_id`。
    Security,
}

/// 通知的密文详情（JSON，解密后）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NotifyDetail {
    pub session_id: String,
    pub title: String,
    pub body: String,
}

/// `kind = data` 的 body 框架。hub 对单个信封有大小上限（默认 256 KiB），而一条 RPC 消息（如一页历史）
/// 可能更大，所以 Noise 密文在信封层分片。hub 只看到若干个 `data` 信封。
///
/// body 第一个字节是 `flags`：
/// - `0x00` WHOLE：后面是一整条 `Transport::encrypt` 的输出
/// - `0x01` FRAGMENT：`[u32 be msg_id][u16 be index][u16 be count]` + 密文切片。同一 `msg_id` 的分片按
///   `index` 顺序、不与其他消息交错地发送；接收方收齐 `count` 片后拼接再解密
/// - `0x02` RESET：无载荷。发送方发现自己的传输层失步（解密失败 / 丢片）时发给对端，对端丢弃传输并重新握手
///   （IK 由客户端发起；daemon 发 RESET 是在请求客户端重新握手）
///
/// Noise 的 nonce 是顺序的，所以**任何一片丢失都意味着整条传输失步**：收到 `Backpressure` / `Dropped` 时
/// 发送方必须原样重发同一片（不能重新加密）；接收方解密失败时按 RESET 处理。
pub mod frame {
    pub const WHOLE: u8 = 0x00;
    pub const FRAGMENT: u8 = 0x01;
    pub const RESET: u8 = 0x02;
    /// 每个 `data` 信封里最多放多少密文字节（留出信封头和 hub 上限的余量）。
    pub const MAX_FRAGMENT: usize = 200 * 1024;
    /// 重组时单条消息密文总长上限，防内存耗尽。
    pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
    const HEADER: usize = 1 + 4 + 2 + 2;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum DataFrame<'a> {
        Whole(&'a [u8]),
        Fragment { msg_id: u32, index: u16, count: u16, data: &'a [u8] },
        Reset,
    }

    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum FrameError {
        #[error("empty body")]
        Empty,
        #[error("unknown flags {0}")]
        UnknownFlags(u8),
        #[error("truncated fragment header")]
        Truncated,
        #[error("fragment out of order: expected {expected}, got {got}")]
        OutOfOrder { expected: u16, got: u16 },
        #[error("fragment count mismatch")]
        CountMismatch,
        #[error("message too large")]
        TooLarge,
    }

    /// 把一条密文切成若干 body。短消息一个 `WHOLE`，长消息若干 `FRAGMENT`。
    pub fn encode(ciphertext: &[u8], msg_id: u32) -> Vec<Vec<u8>> {
        if ciphertext.len() <= MAX_FRAGMENT {
            let mut b = Vec::with_capacity(1 + ciphertext.len());
            b.push(WHOLE);
            b.extend_from_slice(ciphertext);
            return vec![b];
        }
        let chunks: Vec<&[u8]> = ciphertext.chunks(MAX_FRAGMENT).collect();
        let count = chunks.len() as u16;
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                let mut b = Vec::with_capacity(HEADER + c.len());
                b.push(FRAGMENT);
                b.extend_from_slice(&msg_id.to_be_bytes());
                b.extend_from_slice(&(i as u16).to_be_bytes());
                b.extend_from_slice(&count.to_be_bytes());
                b.extend_from_slice(c);
                b
            })
            .collect()
    }

    pub fn reset_body() -> Vec<u8> {
        vec![RESET]
    }

    pub fn decode(body: &[u8]) -> Result<DataFrame<'_>, FrameError> {
        let (&flags, rest) = body.split_first().ok_or(FrameError::Empty)?;
        match flags {
            WHOLE => Ok(DataFrame::Whole(rest)),
            RESET => Ok(DataFrame::Reset),
            FRAGMENT => {
                if rest.len() < HEADER - 1 {
                    return Err(FrameError::Truncated);
                }
                let msg_id = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                let index = u16::from_be_bytes([rest[4], rest[5]]);
                let count = u16::from_be_bytes([rest[6], rest[7]]);
                Ok(DataFrame::Fragment { msg_id, index, count, data: &rest[8..] })
            }
            other => Err(FrameError::UnknownFlags(other)),
        }
    }

    /// 正在重组的一条消息。
    #[derive(Debug)]
    struct Partial {
        msg_id: u32,
        count: u16,
        next: u16,
        buf: Vec<u8>,
    }

    /// 每个发送方一个。`push` 返回 `Some(ciphertext)` 表示一条完整消息可解密了。
    ///
    /// 严格按序：`index` 必须等于期望的下一片，`index < count`，同一 `msg_id` 的 `count` 不能变；
    /// 任何不符都返回错误，调用方据此丢弃传输（不会 panic，也不会被对端推着做 u16 溢出）。
    #[derive(Debug, Default)]
    pub struct Reassembler {
        current: Option<Partial>,
    }

    impl Reassembler {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn push(&mut self, frame: DataFrame<'_>) -> Result<Option<Vec<u8>>, FrameError> {
            match frame {
                DataFrame::Whole(c) => {
                    self.current = None;
                    Ok(Some(c.to_vec()))
                }
                DataFrame::Reset => {
                    self.current = None;
                    Ok(None)
                }
                DataFrame::Fragment { msg_id, index, count, data } => {
                    if count == 0 {
                        return Err(FrameError::CountMismatch);
                    }
                    if index >= count {
                        self.current = None;
                        return Err(FrameError::OutOfOrder { expected: 0, got: index });
                    }
                    let mut cur = match self.current.take() {
                        Some(cur) if cur.msg_id == msg_id => {
                            if cur.count != count {
                                return Err(FrameError::CountMismatch);
                            }
                            cur
                        }
                        _ => Partial { msg_id, count, next: 0, buf: Vec::new() },
                    };
                    if index != cur.next {
                        return Err(FrameError::OutOfOrder { expected: cur.next, got: index });
                    }
                    if cur.buf.len().saturating_add(data.len()) > MAX_MESSAGE {
                        return Err(FrameError::TooLarge);
                    }
                    cur.buf.extend_from_slice(data);
                    // index < count ≤ u16::MAX，所以 index + 1 不会溢出。
                    if u32::from(index) + 1 == u32::from(count) {
                        Ok(Some(cur.buf))
                    } else {
                        cur.next = index + 1;
                        self.current = Some(cur);
                        Ok(None)
                    }
                }
            }
        }

        pub fn clear(&mut self) {
            self.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let small = vec![7u8; 100];
        let bodies = frame::encode(&small, 1);
        assert_eq!(bodies.len(), 1);
        let mut r = frame::Reassembler::new();
        assert_eq!(r.push(frame::decode(&bodies[0]).unwrap()).unwrap(), Some(small.clone()));

        let big = (0..(frame::MAX_FRAGMENT * 2 + 10)).map(|i| i as u8).collect::<Vec<_>>();
        let bodies = frame::encode(&big, 42);
        assert_eq!(bodies.len(), 3);
        let mut out = None;
        for b in &bodies {
            out = r.push(frame::decode(b).unwrap()).unwrap();
        }
        assert_eq!(out, Some(big));
        // 乱序报错
        let mut r2 = frame::Reassembler::new();
        assert!(r2.push(frame::decode(&bodies[1]).unwrap()).is_err());
        assert_eq!(frame::decode(&frame::reset_body()).unwrap(), frame::DataFrame::Reset);
    }

    #[test]
    fn reassembler_rejects_hostile_headers_without_panicking() {
        use frame::{DataFrame, FrameError, Reassembler};
        let frag = |msg_id, index, count| DataFrame::Fragment { msg_id, index, count, data: b"x" };
        // index >= count
        let mut r = Reassembler::new();
        assert!(matches!(r.push(frag(1, 2, 2)), Err(FrameError::OutOfOrder { .. })));
        assert!(matches!(r.push(frag(1, 65535, 2)), Err(FrameError::OutOfOrder { .. })));
        // count 中途变化
        let mut r = Reassembler::new();
        assert_eq!(r.push(frag(7, 0, 3)).unwrap(), None);
        assert_eq!(r.push(frag(7, 1, 4)), Err(FrameError::CountMismatch));
        // count = u16::MAX 走到最后一片也不会溢出（只验证头部逻辑，用空片省内存）
        let mut r = Reassembler::new();
        let empty = |index| DataFrame::Fragment { msg_id: 9, index, count: u16::MAX, data: b"" };
        for i in 0..u16::MAX - 1 {
            assert_eq!(r.push(empty(i)).unwrap(), None);
        }
        assert_eq!(r.push(empty(u16::MAX - 1)).unwrap(), Some(Vec::new()));
        // count = 1 的单片 FRAGMENT 也能收
        let mut r = Reassembler::new();
        assert_eq!(r.push(frag(3, 0, 1)).unwrap(), Some(b"x".to_vec()));
        // 一条完成后同一 msg_id 再来要从 0 开始
        assert!(matches!(r.push(frag(3, 1, 2)), Err(FrameError::OutOfOrder { expected: 0, got: 1 })));
    }

    #[test]
    fn envelope_roundtrip() {
        let e = Envelope::new("a".repeat(32), "b".repeat(32), Kind::Data, 7, vec![1, 2, 3]);
        let bytes = e.encode().unwrap();
        assert_eq!(Envelope::decode(&bytes).unwrap(), e);
    }
}
