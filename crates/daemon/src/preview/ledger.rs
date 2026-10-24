//! 隧道传输的账本：端到端回执和出站帧队列（docs/protocol.md § 11.3 / § 11.5）。纯状态，时间由调用方传入。
//!
//! - **回执**：每条隧道消息带 `ack` = 发送方此刻已经收到并解密的对端消息条数（这条传输建立以来累计）。发送方给自己发出的
//!   每条消息从 0 编号，保留它的信封原文，直到对端的 `ack` 大于这个编号才释放；未被回执的密文不超过 8 MiB（daemon → 预览页），
//!   满了就等回执，纯回执消息不受这个限制；
//! - **何时回执**：每条发出去的消息都带当前的 `ack`。手里没有别的要发时：收到 ≥ 256 KiB 或 ≥ 32 条还没回执过的消息，立即发
//!   一条纯回执；否则最迟 50 ms 后发。收到纯回执本身不触发回执（避免来回）；
//! - **出站队列**：`WINDOW` / `RESPONSE` / `RESET` / `GOAWAY` 走加急队列，`DATA` / `END` / `WS_*` 走普通队列，
//!   大响应不会挡住窗口归还；一条消息装满 128 KiB 为止，帧不跨消息。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anycon_protocol::tunnel::{self, Frame};

/// 发送 `DATA` / `WS_MESSAGE` 时每帧的数据量：比协议上限（64 KiB）小一点，这样一条消息（≤ 128 KiB）能装两帧再加几个小帧。
pub const DATA_CHUNK: usize = 62 * 1024;

/// 这条隧道上有未回执的消息，而对端的回执这么久没有前进，就认为对端不行了。
pub const STALL_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Flight {
    msg_no: u64,
    /// 信封的 `seq`（hub 出站通道里的编号），回执越过这条消息时据此释放保留。
    seq: u64,
    bytes: usize,
    /// 纯回执消息：对端不会单独回执它（收到纯回执不触发回执），所以不参与「对端不回执了」的判断。
    pure: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// 对端回执了我们没发过的消息。
    AckBeyondSent { ack: u64, sent: u64 },
}

/// 什么时候要发纯回执。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDue {
    /// 没有欠着的回执。
    Never,
    /// 现在就发。
    Now,
    /// 最迟到这个时刻发。
    At(Instant),
}

#[derive(Debug)]
pub struct Ledger {
    /// 已发出的消息条数（下一条消息的编号）。
    sent: u64,
    flights: VecDeque<Flight>,
    flight_bytes: usize,
    /// 其中带帧的（需要对端回执的）消息条数。
    data_flights: usize,
    /// 已收到并解密的消息条数（就是我们发出去的 `ack`）。
    recv: u64,
    /// 自上一条发出的消息之后，收到的、带帧的消息的字节数和条数（纯回执不算）。
    unacked_bytes: usize,
    unacked_msgs: u64,
    first_unacked: Option<Instant>,
    /// 对端回执过的最大值。
    peer_ack: u64,
    /// 有带帧的未回执消息时：最近一次「对端的回执前进了 / 我们开始等」的时刻。
    waiting_since: Option<Instant>,
}

impl Default for Ledger {
    fn default() -> Self {
        Ledger::new()
    }
}

impl Ledger {
    pub fn new() -> Ledger {
        Ledger {
            sent: 0,
            flights: VecDeque::new(),
            flight_bytes: 0,
            data_flights: 0,
            recv: 0,
            unacked_bytes: 0,
            unacked_msgs: 0,
            first_unacked: None,
            peer_ack: 0,
            waiting_since: None,
        }
    }

    /// 下一条发出的消息里该带的 `ack`。
    pub fn ack(&self) -> u64 {
        self.recv
    }

    /// 已发出多少条消息。
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// 未被回执的密文字节数。
    pub fn inflight_bytes(&self) -> usize {
        self.flight_bytes
    }

    pub fn inflight_messages(&self) -> usize {
        self.flights.len()
    }

    /// 在途窗口里还放得下一条 `cipher_len` 字节的消息吗？空的时候总是放得下（再大也要能发出去）。
    pub fn has_room(&self, cipher_len: usize) -> bool {
        self.flights.is_empty() || self.flight_bytes + cipher_len <= tunnel::DAEMON_INFLIGHT
    }

    /// 记一条发出的消息（`seq` 是 hub 出站通道给它的编号，`cipher_len` 是信封体的字节数，`pure` 是不是纯回执）。
    /// 它带着当前的 `ack`，所以欠着的回执清零。返回它的编号。
    pub fn on_sent(&mut self, seq: u64, cipher_len: usize, pure: bool, now: Instant) -> u64 {
        let msg_no = self.sent;
        self.sent += 1;
        if !pure {
            if self.data_flights == 0 {
                self.waiting_since = Some(now);
            }
            self.data_flights += 1;
        }
        self.flights.push_back(Flight { msg_no, seq, bytes: cipher_len, pure });
        self.flight_bytes += cipher_len;
        self.unacked_bytes = 0;
        self.unacked_msgs = 0;
        self.first_unacked = None;
        msg_no
    }

    /// 收到并解密了对端的一条消息：`plain_len` 是明文长度，`has_frames` 是它带不带帧，`ack` 是它带的回执。
    /// 回执越过的消息被释放，返回其中最大的 `seq`（调用方据此放掉 hub 出站通道里保留的信封原文）。
    pub fn on_received(
        &mut self,
        plain_len: usize,
        has_frames: bool,
        ack: u64,
        now: Instant,
    ) -> Result<Option<u64>, LedgerError> {
        if ack > self.sent {
            return Err(LedgerError::AckBeyondSent { ack, sent: self.sent });
        }
        self.recv += 1;
        if has_frames {
            self.unacked_bytes += plain_len;
            self.unacked_msgs += 1;
            self.first_unacked.get_or_insert(now);
        }
        if ack <= self.peer_ack {
            return Ok(None);
        }
        self.peer_ack = ack;
        let mut released = None;
        while let Some(f) = self.flights.front() {
            if f.msg_no >= ack {
                break;
            }
            self.flight_bytes -= f.bytes;
            if !f.pure {
                self.data_flights -= 1;
            }
            released = Some(f.seq);
            self.flights.pop_front();
        }
        self.waiting_since = if self.data_flights == 0 { None } else { Some(now) };
        Ok(released)
    }

    /// 欠着的回执什么时候该发。
    pub fn ack_due(&self, now: Instant) -> AckDue {
        let Some(first) = self.first_unacked.filter(|_| self.unacked_msgs > 0) else { return AckDue::Never };
        if self.unacked_bytes >= tunnel::ACK_EVERY_BYTES || self.unacked_msgs >= tunnel::ACK_EVERY_MESSAGES {
            return AckDue::Now;
        }
        let due = first + Duration::from_millis(tunnel::ACK_DELAY_MS);
        if due <= now {
            AckDue::Now
        } else {
            AckDue::At(due)
        }
    }

    /// 有带帧的未回执消息，但对端的回执已经 `limit` 没有前进了（纯回执不算：对端空闲时不会回执它）。
    pub fn stalled(&self, now: Instant, limit: Duration) -> bool {
        self.waiting_since.is_some_and(|t| now.saturating_duration_since(t) >= limit)
    }

    /// 什么时候会判定为 stalled（没有在等回执就是 `None`）。
    pub fn stall_deadline(&self, limit: Duration) -> Option<Instant> {
        self.waiting_since.map(|t| t + limit)
    }
}

/// 出站帧队列。
#[derive(Debug, Default)]
pub struct OutQueue {
    urgent: VecDeque<Frame>,
    bulk: VecDeque<Frame>,
}

impl OutQueue {
    pub fn new() -> OutQueue {
        OutQueue::default()
    }

    pub fn is_empty(&self) -> bool {
        self.urgent.is_empty() && self.bulk.is_empty()
    }

    pub fn len(&self) -> usize {
        self.urgent.len() + self.bulk.len()
    }

    /// 排着的普通帧的数据字节数（诊断 / 测试用）。
    pub fn bulk_payload_bytes(&self) -> usize {
        self.bulk.iter().map(|f| f.payload.len()).sum()
    }

    pub fn push_urgent(&mut self, f: Frame) {
        self.urgent.push_back(f);
    }

    pub fn push_bulk(&mut self, f: Frame) {
        self.bulk.push_back(f);
    }

    /// 去掉某个 stream 排着的普通帧（它被 `RESET` 了，数据不用再发）。
    pub fn purge_stream(&mut self, stream: u32) {
        self.bulk.retain(|f| f.stream != stream);
        // 加急队列里这个 stream 的 WINDOW 也没用了；`RESET` 自己随后才入队，不会被误删
        self.urgent.retain(|f| f.stream != stream);
    }

    /// 取出下一条消息的帧：先加急后普通，装到 [`tunnel::MAX_MESSAGE`] 为止，至少一帧。
    pub fn pop_message(&mut self) -> Vec<Frame> {
        let mut frames: Vec<Frame> = Vec::new();
        let mut len = tunnel::message_len(&[]);
        loop {
            let next = match (self.urgent.front(), self.bulk.front()) {
                (Some(f), _) => f,
                (None, Some(f)) => f,
                (None, None) => break,
            };
            if !frames.is_empty() && len + next.encoded_len() > tunnel::MAX_MESSAGE {
                break;
            }
            let f = self.urgent.pop_front().or_else(|| self.bulk.pop_front()).expect("front exists");
            len += f.encoded_len();
            frames.push(f);
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anycon_protocol::tunnel::FrameType;

    fn t0() -> Instant {
        Instant::now()
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn acks_release_exactly_the_messages_they_cover() {
        let t = t0();
        let mut l = Ledger::new();
        assert_eq!(l.on_sent(100, 1000, false, t), 0);
        assert_eq!(l.on_sent(101, 2000, false, t), 1);
        assert_eq!(l.on_sent(103, 3000, false, t), 2); // seq 不连续（中间夹着别的信封）也没关系
        assert_eq!((l.inflight_messages(), l.inflight_bytes()), (3, 6000));
        // ack = 2 表示对端收到了 2 条（编号 0 和 1）
        assert_eq!(l.on_received(10, true, 2, t), Ok(Some(101)));
        assert_eq!((l.inflight_messages(), l.inflight_bytes()), (1, 3000));
        // 同样的 ack 不再释放；落后的 ack 忽略
        assert_eq!(l.on_received(10, true, 2, t), Ok(None));
        assert_eq!(l.on_received(10, true, 1, t), Ok(None));
        assert_eq!(l.inflight_messages(), 1);
        assert_eq!(l.on_received(10, true, 3, t), Ok(Some(103)));
        assert_eq!((l.inflight_messages(), l.inflight_bytes()), (0, 0));
    }

    #[test]
    fn an_ack_beyond_what_was_sent_is_an_error() {
        let t = t0();
        let mut l = Ledger::new();
        l.on_sent(1, 10, false, t);
        assert_eq!(l.on_received(10, true, 2, t), Err(LedgerError::AckBeyondSent { ack: 2, sent: 1 }));
        // 出错的消息没有计入
        assert_eq!(l.ack(), 0);
    }

    #[test]
    fn the_window_is_8_mib_of_unacked_ciphertext_and_an_empty_window_always_fits() {
        let t = t0();
        let mut l = Ledger::new();
        assert!(l.has_room(tunnel::DAEMON_INFLIGHT + 1), "an empty window fits any one message");
        let chunk = 128 * 1024;
        let n = tunnel::DAEMON_INFLIGHT / chunk;
        for i in 0..n {
            assert!(l.has_room(chunk), "{i}");
            l.on_sent(i as u64 + 1, chunk, false, t);
        }
        assert_eq!(l.inflight_bytes(), tunnel::DAEMON_INFLIGHT);
        assert!(!l.has_room(1), "full: wait for acks");
        // 回执一半，又能发了
        l.on_received(1, true, (n / 2) as u64, t).unwrap();
        assert!(l.has_room(chunk));
    }

    #[test]
    fn acks_are_owed_after_32_messages_256_kib_or_50_ms() {
        let t = t0();
        let mut l = Ledger::new();
        assert_eq!(l.ack_due(t), AckDue::Never);
        // 一条小消息：最迟 50 ms
        l.on_sent(1, 10, false, t);
        l.on_received(100, true, 0, t).unwrap();
        assert_eq!(l.ack(), 1);
        assert_eq!(l.ack_due(t), AckDue::At(ms(t, 50)));
        assert_eq!(l.ack_due(ms(t, 49)), AckDue::At(ms(t, 50)));
        assert_eq!(l.ack_due(ms(t, 50)), AckDue::Now);
        // 发一条消息（带着 ack）之后不再欠
        l.on_sent(2, 10, false, ms(t, 10));
        assert_eq!(l.ack_due(ms(t, 100)), AckDue::Never);
        // 31 条还不急，32 条立即
        for i in 0..31 {
            l.on_received(10, true, 0, ms(t, 200 + i)).unwrap();
        }
        assert!(matches!(l.ack_due(ms(t, 231)), AckDue::At(_)));
        l.on_received(10, true, 0, ms(t, 232)).unwrap();
        assert_eq!(l.ack_due(ms(t, 232)), AckDue::Now);
        l.on_sent(3, 10, false, ms(t, 233));
        // 256 KiB 立即（一条就够）
        l.on_received(tunnel::ACK_EVERY_BYTES, true, 0, ms(t, 300)).unwrap();
        assert_eq!(l.ack_due(ms(t, 300)), AckDue::Now);
    }

    #[test]
    fn a_pure_ack_does_not_make_us_owe_one() {
        let t = t0();
        let mut l = Ledger::new();
        l.on_sent(1, 10, false, t);
        // 对端发来纯回执（没有帧）：计入 recv（ack 数的是消息条数），但不触发回执
        assert_eq!(l.on_received(8, false, 1, t), Ok(Some(1)));
        assert_eq!(l.ack(), 1);
        assert_eq!(l.ack_due(ms(t, 1000)), AckDue::Never);
    }

    #[test]
    fn a_stalled_peer_is_detected_only_while_something_is_unacked() {
        let t = t0();
        let mut l = Ledger::new();
        assert!(!l.stalled(ms(t, 1_000_000), STALL_LIMIT));
        l.on_sent(1, 10, false, t);
        assert!(!l.stalled(ms(t, 119_999), STALL_LIMIT));
        assert!(l.stalled(ms(t, 120_000), STALL_LIMIT));
        assert_eq!(l.stall_deadline(STALL_LIMIT), Some(ms(t, 120_000)));
        // 回执前进了：重新计时
        l.on_sent(2, 10, false, ms(t, 100_000));
        l.on_received(1, true, 1, ms(t, 110_000)).unwrap();
        assert!(!l.stalled(ms(t, 200_000), STALL_LIMIT));
        assert!(l.stalled(ms(t, 230_000), STALL_LIMIT));
        // 全回执了：没有在等
        l.on_received(1, true, 2, ms(t, 231_000)).unwrap();
        assert!(!l.stalled(ms(t, 9_999_999), STALL_LIMIT));
        assert_eq!(l.stall_deadline(STALL_LIMIT), None);
    }

    fn frame(ty: FrameType, stream: u32, len: usize) -> Frame {
        Frame::new(ty, stream, vec![0; len])
    }

    #[test]
    fn an_idle_peer_that_never_acks_our_pure_acks_is_not_stalled() {
        let t = t0();
        let mut l = Ledger::new();
        // 对端发了东西，我们回了一条纯回执；之后对端空闲（它下一条消息才会带上对这条的回执）
        l.on_received(100, true, 0, t).unwrap();
        l.on_sent(1, 10, true, ms(t, 50));
        assert_eq!(l.inflight_messages(), 1, "still retained until the peer acks it");
        assert!(!l.stalled(ms(t, 10_000_000), STALL_LIMIT));
        assert_eq!(l.stall_deadline(STALL_LIMIT), None);
        // 带帧的消息才让我们等回执
        l.on_sent(2, 10, false, ms(t, 100));
        assert!(l.stalled(ms(t, 100 + 120_000), STALL_LIMIT));
        // 对端的下一条消息把两条都回执了
        l.on_received(1, true, 2, ms(t, 200)).unwrap();
        assert_eq!((l.inflight_messages(), l.stall_deadline(STALL_LIMIT)), (0, None));
    }

    #[test]
    fn messages_pack_two_data_frames_and_the_small_ones_around_them() {
        let mut q = OutQueue::new();
        for _ in 0..4 {
            q.push_bulk(frame(FrameType::Data, 1, DATA_CHUNK));
        }
        q.push_urgent(frame(FrameType::Window, 3, 4));
        q.push_urgent(frame(FrameType::Response, 5, 300));
        q.push_bulk(frame(FrameType::End, 1, 0));
        let m1 = q.pop_message();
        // 加急的先走，然后两个 DATA；第三个装不下
        let kinds: Vec<FrameType> = m1.iter().map(|f| f.ty).collect();
        assert_eq!(kinds, vec![FrameType::Window, FrameType::Response, FrameType::Data, FrameType::Data]);
        assert!(tunnel::message_len(&m1) <= tunnel::MAX_MESSAGE);
        let m2 = q.pop_message();
        assert_eq!(m2.iter().map(|f| f.ty).collect::<Vec<_>>(), vec![FrameType::Data, FrameType::Data, FrameType::End]);
        assert!(q.pop_message().is_empty() && q.is_empty());
        // 编码得进一条消息
        assert!(tunnel::encode_message(7, &m1).is_ok() && tunnel::encode_message(7, &m2).is_ok());
    }

    #[test]
    fn a_single_oversized_frame_still_goes_out_alone_and_order_is_kept_per_queue() {
        let mut q = OutQueue::new();
        q.push_bulk(frame(FrameType::Data, 1, 60_000));
        q.push_bulk(frame(FrameType::Data, 2, 60_000));
        q.push_bulk(frame(FrameType::Data, 3, 60_000));
        let m = q.pop_message();
        assert_eq!(m.iter().map(|f| f.stream).collect::<Vec<_>>(), vec![1, 2], "FIFO");
        let m = q.pop_message();
        assert_eq!(m.iter().map(|f| f.stream).collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn purging_a_stream_drops_its_queued_frames_only() {
        let mut q = OutQueue::new();
        q.push_bulk(frame(FrameType::Data, 1, 10));
        q.push_bulk(frame(FrameType::Data, 2, 10));
        q.push_urgent(frame(FrameType::Window, 1, 4));
        q.push_urgent(frame(FrameType::Window, 2, 4));
        q.purge_stream(1);
        assert_eq!(q.len(), 2);
        let m = q.pop_message();
        assert!(m.iter().all(|f| f.stream == 2));
        // 清掉之后再入队的 RESET 不受影响
        q.push_urgent(frame(FrameType::Reset, 1, 5));
        assert_eq!(q.pop_message().len(), 1);
    }
}
