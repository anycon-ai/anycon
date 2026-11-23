//! 到一个节点的加密通道（握手之后的 Noise 传输）的收发状态：分片、重组、发送方流控。单节点的 [`super::link`] 和多节点的
//! [`super::multi`] 共用它，各自负责 WebSocket 的读写和请求的配对。
//!
//! **发送方规则（docs/protocol.md § 3.1，状态机在 [`crate::flow`]）**：保留最近发出的信封原文；hub 退回一封
//! （`backpressure`）后暂停发送（新请求等着），等 `retry_after_ms`（最近一条 `rate_limited` 给的，没有就 100–300 ms 加抖动），
//! 把所有被退回的 seq 按升序原样重发，再继续；重发期间又被退回就再等一轮；被退回的 seq 已不在保留缓冲里就结束这条通道
//! （加密通道少了一封，只能重来）。`rate_limited` 暂停整条 hub 连接的出站（[`RatePause`]，由调用方持有）。
//! 数据信封的 body 是 `envelope::frame`（整条 / 分片 / 重置）：发送时按需分片，接收时重组后再解密。

use std::time::Instant;

use anycon_crypto::Transport;
use anycon_protocol::envelope::frame;
use anycon_protocol::{Envelope, Kind, RpcMessage};

use crate::flow::{RatePause, Retained, SendFlow};

/// 重发时一批最多多少字节（上行很小，基本一批发完）。
const RESEND_BATCH_BYTES: usize = 2 * 1024 * 1024;

pub(crate) struct NodeChannel {
    pub node_id: String,
    transport: Transport,
    msg_id: u32,
    group: u64,
    reasm: frame::Reassembler,
    pub flow: SendFlow,
}

impl NodeChannel {
    pub fn new(node_id: String, transport: Transport) -> NodeChannel {
        NodeChannel {
            node_id,
            transport,
            msg_id: 0,
            group: 0,
            reasm: frame::Reassembler::new(),
            flow: SendFlow::default(),
        }
    }

    fn envelope_bytes(
        &self,
        device_id: &str,
        seq: u64,
        kind: Kind,
        body: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, String> {
        Envelope::new(device_id.to_string(), self.node_id.clone(), kind, seq, body).encode().map_err(|e| e.to_string())
    }

    /// 加密一条请求、分片，返回要立刻发出的信封（`seq` 是整条 hub 连接共用的序号，逐片递增）。暂停中的片只入保留缓冲，
    /// 等 [`NodeChannel::take_resend`] 时按序发出。
    pub fn encode_request(
        &mut self,
        msg: &RpcMessage,
        device_id: &str,
        seq: &mut u64,
        rate: &RatePause,
    ) -> std::result::Result<Vec<Vec<u8>>, String> {
        let ct = self.transport.encrypt(&msg.encode()).map_err(|e| format!("encrypt failed: {e}"))?;
        self.msg_id = self.msg_id.wrapping_add(1);
        self.group += 1;
        let mut out = Vec::new();
        for body in frame::encode(&ct, self.msg_id) {
            *seq += 1;
            let s = *seq;
            let blocked = self.flow.blocked(Instant::now(), rate);
            let bytes =
                if blocked { None } else { Some(self.envelope_bytes(device_id, s, Kind::Data, body.clone())?) };
            self.flow.retain(Retained { seq: s, kind: Kind::Data, body, group: self.group });
            match bytes {
                Some(b) => out.push(b),
                None => self.flow.defer(s),
            }
        }
        Ok(out)
    }

    /// hub 退回了 `seq`：暂停，之后按升序原样重发。不在保留缓冲里就返回错误（通道结束）。
    pub fn on_backpressure(&mut self, seq: u64, rate: &RatePause) -> std::result::Result<(), String> {
        let now = Instant::now();
        if self.flow.bounce(seq, now, rate.hint(now)) {
            Ok(())
        } else {
            Err("the hub bounced a message that is no longer retained, so the secure channel to the node lost sync; run the command again".into())
        }
    }

    /// 暂停结束：把待发的（被退回的 + 暂停期间排队的）按升序原样取出来。
    pub fn take_resend(&mut self, device_id: &str, rate: &RatePause) -> std::result::Result<Vec<Vec<u8>>, String> {
        let now = Instant::now();
        if self.flow.ready_at(rate, now).is_none_or(|t| t > now) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for (seq, item) in self.flow.take_batch(now, RESEND_BATCH_BYTES) {
            let (kind, body) = item.unwrap_or_else(|| (Kind::Data, frame::reset_body()));
            out.push(self.envelope_bytes(device_id, seq, kind, body)?);
        }
        Ok(out)
    }

    /// 该醒的时刻：有待发的 -> 可以重发的时刻；没有 -> 暂停结束的时刻（要重新判断能不能收新请求）。
    pub fn wake_at(&self, rate: &RatePause, now: Instant) -> Option<Instant> {
        self.flow.ready_at(rate, now).or_else(|| self.flow.unblock_at(rate, now))
    }

    pub fn blocked(&self, rate: &RatePause) -> bool {
        self.flow.blocked(Instant::now(), rate)
    }

    /// 收到节点发来的一封 `data` 信封：重组、解密、解码。分片没收齐、或者解不出 JSON-RPC 消息时是 `Ok(None)`。
    /// 重置 / 格式错 / 解不开密：通道已经不同步，返回原因，只能重来。
    pub fn on_data(&mut self, env: &Envelope) -> std::result::Result<Option<RpcMessage>, String> {
        let ct = match frame::decode(&env.body) {
            Ok(frame::DataFrame::Reset) => return Err("the node reset the connection; run the command again".into()),
            Ok(f) => match self.reasm.push(f) {
                Ok(Some(ct)) => ct,
                Ok(None) => return Ok(None),
                Err(e) => return Err(format!("a message from the node was malformed ({e}); run the command again")),
            },
            Err(e) => return Err(format!("a message from the node was malformed ({e}); run the command again")),
        };
        let Ok(plain) = self.transport.decrypt(&ct) else {
            return Err("a message from the node could not be decrypted (the secure channel is out of sync); run the command again".into());
        };
        Ok(RpcMessage::decode(&plain).ok())
    }
}
