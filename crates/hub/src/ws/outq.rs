//! 每条连接的有界发送队列（按字节计，默认 4 MiB）。
//!
//! - 转发的 `data` / `handshake` / `notify` 信封走 [`OutQueue::try_push_frame`]：超过上限就拒绝，
//!   调用方回 `Backpressure`，**不无限缓存**。
//! - hub 自己发的控制消息走 [`OutQueue::push_control`]：**同样计入** `pending` 字节数（所以会挤占数据信封的额度），
//!   另有一份独立的控制消息上限（[`control_cap_for`]，默认 256 KiB）。数据缓冲满了也还有这点余量发 `Backpressure`、
//!   `PeerOffline` 之类的小消息；但控制消息自己超过上限就直接丢弃，不会无限堆积（不读 socket 的连接
//!   可以用任意控制消息灌爆内存）。
//! - 任何一种消息被拒绝都会记下「满」的起点；写端排空到一半以下才清除。持续满超过阈值由连接循环以 4003 断开，
//!   与消息种类无关。所以一条连接占用的内存上界是 `cap + control_cap`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use tokio::sync::mpsc;

/// 写端要做的事。
#[derive(Debug)]
pub enum Outbound {
    Frame(Bytes),
    Ping,
    Close(u16, &'static str),
}

#[derive(Debug)]
pub struct Item {
    pub msg: Outbound,
    size: usize,
    control: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    /// 缓冲已满。
    Full,
    /// 连接已关闭。
    Closed,
}

struct Shared {
    /// 已入队、还没写出的字节数（数据 + 控制）。
    pending: AtomicUsize,
    /// 其中控制消息的字节数。
    control_pending: AtomicUsize,
    cap: usize,
    control_cap: usize,
    full_since: Mutex<Option<Instant>>,
}

/// 控制消息的独立上限：数据上限的 1/16，夹在 64 KiB..256 KiB 之间（默认 4 MiB 的缓冲对应 256 KiB，
/// 约 2000 条 `Backpressure` 回执）。
pub fn control_cap_for(cap: usize) -> usize {
    (cap / 16).clamp(64 * 1024, 256 * 1024)
}

/// 生产端（放在 `Conn` 里，可多处并发调用）。
pub struct OutQueue {
    tx: mpsc::UnboundedSender<Item>,
    shared: Arc<Shared>,
}

/// 消费端（写任务独占）。
pub struct OutRx {
    rx: mpsc::UnboundedReceiver<Item>,
    shared: Arc<Shared>,
}

impl OutQueue {
    pub fn new(cap: usize) -> (OutQueue, OutRx) {
        Self::with_caps(cap, control_cap_for(cap))
    }

    /// 数据上限和控制消息上限分别指定（测试用）。
    pub fn with_caps(cap: usize, control_cap: usize) -> (OutQueue, OutRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            pending: AtomicUsize::new(0),
            control_pending: AtomicUsize::new(0),
            cap,
            control_cap,
            full_since: Mutex::new(None),
        });
        (OutQueue { tx, shared: shared.clone() }, OutRx { rx, shared })
    }

    fn mark_full(&self) {
        let mut f = self.shared.full_since.lock().expect("outq mutex");
        f.get_or_insert_with(Instant::now);
    }

    /// 转发用：超过上限返回 [`PushError::Full`]。
    pub fn try_push_frame(&self, frame: Bytes) -> Result<(), PushError> {
        let len = frame.len();
        let prev = self.shared.pending.fetch_add(len, Ordering::SeqCst);
        // 空队列时允许放入单个超过上限的信封（上限 < 信封大小只会出现在测试里）。
        if prev > 0 && prev + len > self.shared.cap {
            self.shared.pending.fetch_sub(len, Ordering::SeqCst);
            self.mark_full();
            return Err(PushError::Full);
        }
        self.send(Outbound::Frame(frame), len, false)
    }

    /// hub 自己的控制消息：计入 `pending`，但只受控制消息自己的上限约束（数据缓冲满了也还能发）。
    /// 超过控制上限返回 [`PushError::Full`]，消息被丢弃，并开始计「满」的时间——
    /// 连接一直不读的话，30 秒后会被以 4003 断开。
    pub fn push_control(&self, frame: Bytes) -> Result<(), PushError> {
        let len = frame.len();
        let prev = self.shared.control_pending.fetch_add(len, Ordering::SeqCst);
        if prev > 0 && prev + len > self.shared.control_cap {
            self.shared.control_pending.fetch_sub(len, Ordering::SeqCst);
            self.mark_full();
            return Err(PushError::Full);
        }
        self.shared.pending.fetch_add(len, Ordering::SeqCst);
        self.send(Outbound::Frame(frame), len, true)
    }

    pub fn ping(&self) {
        let _ = self.send(Outbound::Ping, 0, false);
    }

    pub fn close(&self, code: u16, reason: &'static str) {
        let _ = self.send(Outbound::Close(code, reason), 0, false);
    }

    fn send(&self, msg: Outbound, size: usize, control: bool) -> Result<(), PushError> {
        if self.tx.send(Item { msg, size, control }).is_err() {
            self.shared.pending.fetch_sub(size, Ordering::SeqCst);
            if control {
                self.shared.control_pending.fetch_sub(size, Ordering::SeqCst);
            }
            return Err(PushError::Closed);
        }
        Ok(())
    }

    /// 写端已退出。
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// 缓冲「满」持续了多久（没满返回 `None`）。
    pub fn full_for(&self) -> Option<Duration> {
        self.shared.full_since.lock().expect("outq mutex").map(|t| t.elapsed())
    }

    /// 已入队、还没写出的字节数（数据 + 控制）。
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// 其中控制消息的字节数。
    pub fn control_pending(&self) -> usize {
        self.shared.control_pending.load(Ordering::SeqCst)
    }

    /// 内存占用的理论上界：`cap + control_cap`（再加上单个超限信封）。
    pub fn max_pending(&self) -> usize {
        self.shared.cap + self.shared.control_cap
    }
}

impl OutRx {
    pub async fn recv(&mut self) -> Option<Item> {
        self.rx.recv().await
    }

    /// 一个条目已写出：释放配额；排空到一半以下就清除「满」标记。
    pub fn done(&self, item: &Item) {
        if item.size == 0 {
            return;
        }
        let left = self.shared.pending.fetch_sub(item.size, Ordering::SeqCst).saturating_sub(item.size);
        let control_left = if item.control {
            self.shared.control_pending.fetch_sub(item.size, Ordering::SeqCst).saturating_sub(item.size)
        } else {
            self.shared.control_pending.load(Ordering::SeqCst)
        };
        if left <= self.shared.cap / 2 && control_left <= self.shared.control_cap / 2 {
            *self.shared.full_since.lock().expect("outq mutex") = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: usize) -> Bytes {
        Bytes::from(vec![0u8; n])
    }

    #[tokio::test]
    async fn rejects_when_full_but_control_still_goes_through() {
        let (q, mut rx) = OutQueue::new(100);
        q.try_push_frame(frame(60)).unwrap();
        assert_eq!(q.try_push_frame(frame(60)), Err(PushError::Full));
        assert!(q.full_for().is_some());
        // 数据缓冲满了，控制消息仍有自己的余量
        q.push_control(frame(10)).unwrap();
        assert_eq!(q.pending(), 70);
        assert_eq!(q.control_pending(), 10);

        let first = rx.recv().await.unwrap();
        rx.done(&first);
        // 60 -> 10 字节，低于一半：清除「满」
        assert!(q.full_for().is_none());
        q.try_push_frame(frame(60)).unwrap();
    }

    #[tokio::test]
    async fn control_messages_are_bounded_and_start_the_full_clock() {
        // 控制消息有自己的上限；超过就丢，并开始计「满」，不会无限堆积。
        let (q, mut rx) = OutQueue::with_caps(1000, 100);
        for _ in 0..10 {
            q.push_control(frame(10)).unwrap();
        }
        assert!(q.full_for().is_none());
        assert_eq!(q.push_control(frame(10)), Err(PushError::Full));
        assert_eq!(q.control_pending(), 100);
        assert_eq!(q.pending(), 100);
        assert!(q.full_for().is_some(), "an unread connection that keeps getting control messages must age out");
        // 一直灌也不会超过上限
        for _ in 0..10_000 {
            let _ = q.push_control(frame(10));
        }
        assert!(q.pending() <= q.max_pending());
        assert_eq!(q.control_pending(), 100);

        // 写端排空到一半以下，清除「满」
        for _ in 0..6 {
            let it = rx.recv().await.unwrap();
            rx.done(&it);
        }
        assert!(q.full_for().is_none());
        q.push_control(frame(10)).unwrap();
    }

    #[tokio::test]
    async fn control_bytes_count_against_the_data_cap() {
        let (q, _rx) = OutQueue::with_caps(100, 100);
        q.push_control(frame(70)).unwrap();
        // 控制消息占了 70，数据只剩 30
        assert_eq!(q.try_push_frame(frame(40)), Err(PushError::Full));
        q.try_push_frame(frame(30)).unwrap();
    }

    #[tokio::test]
    async fn single_oversized_frame_is_allowed_into_empty_queue() {
        let (q, _rx) = OutQueue::new(10);
        q.try_push_frame(frame(50)).unwrap();
        assert_eq!(q.try_push_frame(frame(1)), Err(PushError::Full));
    }

    #[tokio::test]
    async fn closed_after_receiver_dropped() {
        let (q, rx) = OutQueue::new(10);
        drop(rx);
        assert!(q.is_closed());
        assert_eq!(q.try_push_frame(frame(1)), Err(PushError::Closed));
        assert_eq!(q.pending(), 0);
    }
}
