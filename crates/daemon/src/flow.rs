//! 发送方流控（docs/protocol.md § 3.1 的「发送方规则」）：daemon（`hub::HubOutbox`）和 CLI 的远程驱动共用。
//!
//! hub 对已配对的 `(from, to)` 之间的 `data` / `notify` 只有「转发」和「退回 `backpressure { to, seq }`」两种处理，
//! 从不静默丢；一个 seq 被退回之后，同一 `(from, to)` 的后续信封一律退回，直到那个 seq 的重发被转发。
//! 所以发送方要：
//!
//! 1. **保留**每个对端最近发出的信封原文。**凡是可能还在路上、hub 还没处理的信封都不逐出**：
//!    发出不到 [`IN_FLIGHT_GUARD`] 的、在最小的待重发 seq 之后的（hub 会把它们全部退回）、同一条消息正在写的所有分片
//!    （同组，最新的一组），一律保留。保留量超过 [`RETAIN_BYTES`] 时**不逐出**这些，而是让调用方**暂停写入新消息**
//!    （[`SendFlow::has_room`]）——把 [`RETAIN_BYTES`] 当作在途窗口用；只有既老于 [`IN_FLIGHT_GUARD`]、又不在待重发集合里、
//!    又不属于正在写的那一整条消息的才会被逐出；
//! 2. 被退回后**暂停**向该对端发新信封（新的只入保留缓冲、排队），等一会儿（`rate_limited` 给的 `retry_after_ms`，
//!    没有就 100–300 ms 加抖动，连续几轮没送完则加倍）；
//! 3. 把所有被退回的 seq **按升序、原样**重发（不重新加密），再继续发新的；重发期间又被退回就再等一轮；
//!    重发出去的信封又算「刚发出」（重新计时，[`SendFlow::take_batch`]）；
//! 4. 被退回的 seq 已经不在保留缓冲里 → 这个对端的传输少了一封，只能重置（[`SendFlow::bounce`] 返回 `false`）。
//!
//! 本模块只有状态机，不碰时钟以外的东西：时间由调用方传入，所以单元测试不用睡觉。

use std::collections::{BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use anycon_protocol::Kind;
use rand::Rng;

/// 每个对端保留最近发出的信封原文的字节上限，同时是**在途窗口**：超过它就暂停写入新消息，而不是逐出可能还在路上的信封。
/// （和 web / sdk 的 `resendBufferBytes` 一样是 16 MiB。）
pub const RETAIN_BYTES: usize = 16 * 1024 * 1024;
/// 发出不到这么久的信封可能还在路上（出站通道、内核缓冲、hub 的读缓冲里），hub 随时可能退回它，所以不逐出。
/// hub 不给「已转发」的回执，发送方无从得知哪些已经被处理，只能按时间保守估计。
pub const IN_FLIGHT_GUARD: Duration = Duration::from_secs(10);
/// 窗口满了（保留量超过 [`RETAIN_BYTES`]）时，不超过这么大的小消息（权限请求、状态之类）仍然可以发，
/// 只要保留量没超过 [`RETAIN_BYTES`] + [`SMALL_HEADROOM`]——大文件传输不该把交互卡住 10 秒。
pub const SMALL_MESSAGE_BYTES: usize = 256 * 1024;
/// 见 [`SMALL_MESSAGE_BYTES`]。
pub const SMALL_HEADROOM: usize = 4 * 1024 * 1024;
/// 被退回后默认等多久再重发（hub 没有用 `rate_limited` 给出 `retry_after_ms` 时）：100–300 ms 加抖动。
const BOUNCE_DELAY_MS: std::ops::Range<u64> = 100..300;
/// 连续几轮没送完时默认等待逐轮加倍，最多到这个值。
const BOUNCE_DELAY_MAX: Duration = Duration::from_secs(2);
/// 刚重发过（不到这么久）又被退回，算「上一轮没送完」。
const SAME_STREAK: Duration = Duration::from_secs(2);
/// `rate_limited` 的 `retry_after_ms` 在它到达后这么久之内，用于计算退回后的等待。
const RATE_HINT_TTL: Duration = Duration::from_secs(2);
/// 单次 `retry_after_ms` 的上限（防止异常的 hub 把出站无限期停住）。
pub const RATE_RETRY_MAX: Duration = Duration::from_secs(60);

/// 保留的信封超过这么久就不会再被 hub 退回了（回执在一个往返之内就到），可以放掉：
/// 空闲的对端不应该一直占着最多 8 MiB 的内存。
pub const RETAIN_MAX_AGE: Duration = Duration::from_secs(60);

/// 一封已发出的 `data` / `notify` 信封的原文。
#[derive(Debug, Clone)]
pub struct Retained {
    pub seq: u64,
    pub kind: Kind,
    pub body: Vec<u8>,
    /// 同一条消息的所有分片同组；逐出时最新的一组永远保留。
    pub group: u64,
}

/// 整条 hub 连接的速率暂停（`rate_limited { retry_after_ms }`）和最近一次给的提示。
#[derive(Debug, Default, Clone, Copy)]
pub struct RatePause {
    until: Option<Instant>,
    hint: Option<(Instant, Duration)>,
}

impl RatePause {
    /// hub 说超了速率预算，要停 `retry_after`（有上限）。
    pub fn limited(&mut self, now: Instant, retry_after: Duration) {
        let d = retry_after.min(RATE_RETRY_MAX);
        self.hint = Some((now, d));
        let until = now + d;
        self.until = Some(self.until.map_or(until, |t| t.max(until)));
    }

    /// 暂停到什么时候（已经过去的算没有）。
    pub fn until(&self, now: Instant) -> Option<Instant> {
        self.until.filter(|t| *t > now)
    }

    pub fn left(&self, now: Instant) -> Duration {
        self.until(now).map_or(Duration::ZERO, |t| t - now)
    }

    /// 最近一条 `rate_limited` 给的等待（太久以前的不算）。
    pub fn hint(&self, now: Instant) -> Option<Duration> {
        self.hint.filter(|(at, _)| now.saturating_duration_since(*at) < RATE_HINT_TTL).map(|(_, d)| d)
    }

    pub fn clear(&mut self) {
        *self = RatePause::default();
    }
}

/// [`SendFlow::take_batch`] 取出的一封待发信封：seq，以及它的 (kind, 原文)——保留缓冲里已经没有就是 `None`。
pub type Pending = (u64, Option<(Kind, Vec<u8>)>);

/// 发往一个对端的信封流的状态。
#[derive(Debug)]
pub struct SendFlow {
    /// 按 seq 升序；每项带**最近一次发出**的时刻（入保留缓冲时、每次重发时更新；[`SendFlow::expire`] 和逐出都看它）。
    retained: VecDeque<(Retained, Instant)>,
    bytes: usize,
    /// 要（重新）发出的 seq，升序：hub 退回的，加上暂停期间新到的。非空 = 这个对端的出站被卡住了。
    outstanding: BTreeSet<u64>,
    paused_until: Option<Instant>,
    /// 连续没能一次送完的轮数（决定默认等待的加倍）。
    rounds: u32,
    last_flush: Option<Instant>,
    /// 多久之内发出的信封不逐出（默认 [`IN_FLIGHT_GUARD`]；测试里可以缩短）。
    guard: Duration,
}

impl Default for SendFlow {
    fn default() -> Self {
        SendFlow::with_guard(IN_FLIGHT_GUARD)
    }
}

fn default_delay(rounds: u32) -> Duration {
    let base = Duration::from_millis(rand::thread_rng().gen_range(BOUNCE_DELAY_MS));
    (base * (1u32 << rounds.min(4))).min(BOUNCE_DELAY_MAX)
}

impl SendFlow {
    fn find(&self, seq: u64) -> Option<usize> {
        self.retained.binary_search_by_key(&seq, |(r, _)| r.seq).ok()
    }

    /// 这个 seq 还在保留缓冲里。
    pub fn contains(&self, seq: u64) -> bool {
        self.find(seq).is_some()
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.retained.is_empty() && self.outstanding.is_empty()
    }

    /// 向这个对端的发送现在是否应该暂停：有等着（重）发的、退回后的等待还没到点、或者整条连接在限速暂停。
    pub fn blocked(&self, now: Instant, rate: &RatePause) -> bool {
        !self.outstanding.is_empty() || self.paused_until.is_some_and(|t| t > now) || rate.until(now).is_some()
    }

    /// 保留一封新发出的（或排队的）信封。超过字节上限时尽量从旧的一端逐出（只逐出不可能还在路上的，见下），但逐出不了就让它超：
    /// 调用方负责在开始写下一条消息之前看 [`SendFlow::has_room`] 并暂停，这里不会为了守上限丢掉可能还在路上的信封。
    pub fn retain(&mut self, r: Retained) {
        self.retain_at(r, Instant::now());
    }

    /// 同 [`SendFlow::retain`]，时间由调用方传入（测试用）。
    pub fn retain_at(&mut self, r: Retained, now: Instant) {
        self.bytes += r.body.len();
        self.retained.push_back((r, now));
        self.evict(now, true);
    }

    /// 逐出超出上限的部分。**只**逐出队首里同时满足下面三条的信封：
    ///
    /// 1. 发出（或重发）已经超过 `guard`——否则它可能还在路上，hub 随后退回它时就找不到原文了；
    /// 2. seq 比最小的待重发 seq 小——那个 seq 之后的，hub 都会退回（粘住），回执可能还没到；
    /// 3. `keep_newest` 时，不属于最新的一组（正在写的那一整条消息的所有分片）。
    ///
    /// 遇到第一个不满足的就停（seq 有序，发出时间基本单调，后面的只会更新）。
    fn evict(&mut self, now: Instant, keep_newest: bool) {
        while self.bytes > RETAIN_BYTES {
            let Some((front, at)) = self.retained.front() else { return };
            let in_flight = now.saturating_duration_since(*at) <= self.guard;
            let after_bounce = self.outstanding.first().is_some_and(|f| front.seq >= *f);
            let newest = keep_newest && self.retained.back().is_some_and(|(b, _)| b.group == front.group);
            if in_flight || after_bounce || newest {
                return;
            }
            if let Some((old, _)) = self.retained.pop_front() {
                self.bytes -= old.body.len();
            }
        }
    }

    /// 现在能不能开始写一条 `msg_bytes` 字节（密文）的新消息？不能就是**窗口满了**：保留量超过 [`RETAIN_BYTES`]，
    /// 而能逐出的都逐出了（剩下的都可能还在路上），写下去只能逐出在途的信封——调用方应该等一会儿再问（等它们老到
    /// [`IN_FLIGHT_GUARD`]、或者重发完成）。小消息（≤ [`SMALL_MESSAGE_BYTES`]）可以多用 [`SMALL_HEADROOM`]。
    ///
    /// 只看已有的保留量，不算这条消息自己：一条消息总是整条保留，所以保留量最多超出一条消息（≤ 16 MiB）。
    pub fn has_room(&mut self, now: Instant, msg_bytes: usize) -> bool {
        self.evict(now, false);
        self.bytes <= RETAIN_BYTES || (msg_bytes <= SMALL_MESSAGE_BYTES && self.bytes <= RETAIN_BYTES + SMALL_HEADROOM)
    }

    /// 多久之内发出的信封不逐出。
    pub fn set_guard(&mut self, guard: Duration) {
        self.guard = guard;
    }

    pub fn with_guard(guard: Duration) -> SendFlow {
        SendFlow {
            retained: VecDeque::new(),
            bytes: 0,
            outstanding: BTreeSet::new(),
            paused_until: None,
            rounds: 0,
            last_flush: None,
            guard,
        }
    }

    /// 放掉保留了超过 `max_age` 的信封（还要重发的不动）。返回这个对端是否已经没有任何保留，可以整个删掉。
    pub fn expire(&mut self, now: Instant, max_age: Duration) -> bool {
        while let Some((r, at)) = self.retained.front() {
            if now.saturating_duration_since(*at) <= max_age || self.outstanding.first().is_some_and(|f| r.seq >= *f) {
                break;
            }
            if let Some((old, _)) = self.retained.pop_front() {
                self.bytes -= old.body.len();
            }
        }
        self.retained.is_empty() && self.outstanding.is_empty()
    }

    /// 把一封保留缓冲里已经没有原文的信封以占位帧顶替进来（按 seq 有序插入）：hub 退回了它，我们重发不了原文，
    /// 但 hub 的粘住状态要靠这个 seq 的重发才能解开。占位帧（RESET）走和别的信封一样的暂停 / 重发流程，
    /// 再次被退回时也是按退避等待，不会和 hub 来回狂发。
    pub fn adopt(&mut self, r: Retained) {
        let idx = self.retained.partition_point(|(x, _)| x.seq < r.seq);
        if self.retained.get(idx).is_some_and(|(x, _)| x.seq == r.seq) {
            return;
        }
        self.bytes += r.body.len();
        self.retained.insert(idx, (r, Instant::now()));
    }

    /// 暂停期间到的新信封：只入保留缓冲，等重发时一起按序发出。
    pub fn defer(&mut self, seq: u64) {
        self.outstanding.insert(seq);
    }

    /// hub 退回了 `seq`。返回 `false` = 它已经不在保留缓冲里（传输必须重置）。
    ///
    /// `hint` 是最近一条 `rate_limited` 给的等待；没有就按默认（100–300 ms 加抖动，连续几轮没送完则加倍）。
    /// 同一轮里后到的退回不会延长等待。
    pub fn bounce(&mut self, seq: u64, now: Instant, hint: Option<Duration>) -> bool {
        if !self.contains(seq) {
            return false;
        }
        if self.paused_until.is_none_or(|t| t <= now) {
            // 新的一轮：刚重发过（不到 2 秒）又被退回 = 对端还是接不住
            self.rounds = match self.last_flush {
                Some(t) if now.saturating_duration_since(t) < SAME_STREAK => self.rounds + 1,
                _ => 0,
            };
            let delay = match hint {
                Some(d) => d + Duration::from_millis(rand::thread_rng().gen_range(0..30)),
                None => default_delay(self.rounds),
            };
            self.paused_until = Some(now + delay);
        }
        self.outstanding.insert(seq);
        true
    }

    /// 有东西要（重）发时，最早什么时候可以发：退回的等待、整条连接的限速暂停，取晚的。没有要发的返回 `None`。
    pub fn ready_at(&self, rate: &RatePause, now: Instant) -> Option<Instant> {
        if self.outstanding.is_empty() {
            return None;
        }
        Some([self.paused_until, rate.until(now), Some(now)].into_iter().flatten().max().unwrap_or(now))
    }

    /// 在没有待发信封的情况下，向这个对端的发送什么时候不再被暂停（退回后的等待、整条连接的限速暂停，取晚的）。
    /// 已经解除的返回 `None`。调用方据此在暂停结束时醒来重新判断 [`SendFlow::blocked`]。
    pub fn unblock_at(&self, rate: &RatePause, now: Instant) -> Option<Instant> {
        [self.paused_until, rate.until(now)].into_iter().flatten().filter(|t| *t > now).max()
    }

    /// 取出下一批要发的（按 seq 升序，至少一封，累计字节到 `max_bytes` 为止），并把它们从待发里去掉。
    /// 保留缓冲里已经没有的 seq（不该发生）以 `None` 的 body 返回，让调用方用占位帧顶替，别让 hub 一直等。
    pub fn take_batch(&mut self, now: Instant, max_bytes: usize) -> Vec<Pending> {
        let mut out = Vec::new();
        let mut sent = 0usize;
        for &seq in self.outstanding.iter() {
            let k = self.find(seq);
            let item = k.map(|k| (self.retained[k].0.kind, self.retained[k].0.body.clone()));
            if let Some(k) = k {
                // 重发出去又是「刚发出」的：在途期间不能逐出
                self.retained[k].1 = now;
            }
            sent += item.as_ref().map_or(0, |(_, b)| b.len());
            out.push((seq, item));
            if sent >= max_bytes {
                break;
            }
        }
        for (seq, _) in &out {
            self.outstanding.remove(seq);
        }
        self.last_flush = Some(now);
        out
    }

    /// 还没发出去的 seq，升序（不取走）。
    pub fn outstanding(&self) -> Vec<u64> {
        self.outstanding.iter().copied().collect()
    }

    /// 取走所有还没发出去的 seq（会话作废时用占位帧顶替它们）。
    pub fn take_outstanding(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.outstanding).into_iter().collect()
    }

    /// 某个 seq 的原文（测试 / 诊断用）。
    pub fn get(&self, seq: u64) -> Option<&Retained> {
        self.find(seq).map(|k| &self.retained[k].0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(seq: u64, group: u64, len: usize) -> Retained {
        Retained { seq, kind: Kind::Data, body: vec![seq as u8; len], group }
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn a_bounce_pauses_then_the_resend_is_ascending_and_original() {
        let t0 = Instant::now();
        let rate = RatePause::default();
        let mut f = SendFlow::default();
        for s in 1..=5 {
            f.retain(r(s, s, 10));
        }
        assert!(!f.blocked(t0, &rate));
        // 回执乱序到达：4、3、5
        assert!(f.bounce(4, t0, None));
        assert!(f.bounce(3, at(t0, 1), None));
        assert!(f.bounce(5, at(t0, 2), None));
        assert!(f.blocked(at(t0, 50), &rate));
        // 暂停期间新信封排在后面
        f.retain(r(6, 6, 10));
        f.defer(6);
        let ready = f.ready_at(&rate, t0).unwrap();
        let wait = ready - t0;
        assert!(wait >= Duration::from_millis(100) && wait <= Duration::from_millis(300), "{wait:?}");
        // 同一轮后到的退回没有延长等待
        assert_eq!(ready, f.ready_at(&rate, at(t0, 2)).unwrap());
        let batch = f.take_batch(ready, usize::MAX);
        let seqs: Vec<u64> = batch.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![3, 4, 5, 6], "ascending, and the deferred new one comes last");
        for (seq, item) in batch {
            let (kind, body) = item.expect("retained");
            assert_eq!((kind, body), (Kind::Data, vec![seq as u8; 10]), "byte-identical to the original");
        }
        assert!(!f.blocked(ready, &rate) && f.ready_at(&rate, ready).is_none());
    }

    #[test]
    fn a_placeholder_for_a_lost_seq_is_inserted_in_order_and_resent_like_any_other() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        for s in [5, 6, 9] {
            f.retain(r(s, s, 10));
        }
        assert!(!f.bounce(7, t0, None), "7 was never retained");
        f.adopt(Retained { seq: 7, kind: Kind::Data, body: vec![0xAA], group: 99 });
        f.adopt(Retained { seq: 7, kind: Kind::Data, body: vec![0xBB], group: 99 }); // 重复：不动
        assert!(f.bounce(7, t0, None) && f.bounce(9, t0, None));
        let b = f.take_batch(t0 + Duration::from_secs(3), usize::MAX);
        let got: Vec<(u64, Vec<u8>)> = b.into_iter().map(|(s, i)| (s, i.unwrap().1)).collect();
        assert_eq!(got, vec![(7, vec![0xAA]), (9, vec![9; 10])], "ascending; the placeholder stands in for 7");
        assert!(f.get(6).is_some() && f.contains(5), "the order of the retained list is intact");
    }

    #[test]
    fn a_seq_that_is_no_longer_retained_cannot_be_resent() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        f.retain(r(7, 1, 10));
        assert!(!f.bounce(3, t0, None), "never retained / evicted");
        assert!(!f.blocked(t0, &RatePause::default()), "an unknown seq does not block the flow");
        assert!(f.bounce(7, t0, None));
    }

    #[test]
    fn a_resend_that_is_bounced_again_waits_longer_each_round_but_a_calm_spell_resets_it() {
        let t0 = Instant::now();
        let rate = RatePause::default();
        let mut f = SendFlow::default();
        f.retain(r(1, 1, 10));
        let mut now = t0;
        let mut waits = Vec::new();
        for _ in 0..6 {
            assert!(f.bounce(1, now, None));
            let ready = f.ready_at(&rate, now).unwrap();
            waits.push(ready - now);
            now = ready;
            f.take_batch(now, usize::MAX);
            now += Duration::from_millis(5); // 重发之后很快又被退回
        }
        for w in &waits {
            assert!(*w <= BOUNCE_DELAY_MAX, "{w:?}");
        }
        assert!(waits[1] >= Duration::from_millis(200) && waits[2] >= Duration::from_millis(400), "{waits:?}");
        assert!(waits[5] >= Duration::from_millis(1600), "grows to the cap: {waits:?}");
        // 安静了一阵（> 2 秒没有再被退回）：下一轮回到默认
        now += Duration::from_secs(5);
        assert!(f.bounce(1, now, None));
        let w = f.ready_at(&rate, now).unwrap() - now;
        assert!(w <= Duration::from_millis(300), "reset after a calm spell: {w:?}");
    }

    #[test]
    fn rate_limited_overrides_the_default_wait_and_pauses_the_whole_connection() {
        let t0 = Instant::now();
        let mut rate = RatePause::default();
        let mut f = SendFlow::default();
        f.retain(r(1, 1, 10));
        rate.limited(t0, Duration::from_millis(900));
        assert_eq!(rate.left(t0), Duration::from_millis(900));
        assert!(f.blocked(t0, &rate), "a rate-limited connection is blocked even with nothing bounced");
        assert!(!f.blocked(at(t0, 901), &rate));
        // 退回的等待用 hub 给的 retry_after_ms（加一点抖动）
        assert!(f.bounce(1, at(t0, 10), rate.hint(at(t0, 10))));
        let ready = f.ready_at(&rate, at(t0, 10)).unwrap();
        let w = ready - at(t0, 10);
        assert!(w >= Duration::from_millis(900) && w < Duration::from_millis(1000), "{w:?}");
        // 提示太旧就不用了
        assert!(rate.hint(at(t0, 5000)).is_none());
        // 上限
        let mut rate = RatePause::default();
        rate.limited(t0, Duration::from_secs(3600));
        assert_eq!(rate.left(t0), RATE_RETRY_MAX);
        // 限速暂停比退回的等待晚结束时，取晚的
        let mut f = SendFlow::default();
        f.retain(r(1, 1, 10));
        let mut rate = RatePause::default();
        assert!(f.bounce(1, t0, None));
        rate.limited(t0, Duration::from_secs(5));
        assert!(f.ready_at(&rate, t0).unwrap() >= at(t0, 5000));
    }

    const CHUNK: usize = 100 * 1024;

    /// `n` 条各自一组的小消息（每条 `CHUNK` 字节），seq 从 `first` 起，都在 `at` 发出。
    fn fill(f: &mut SendFlow, first: u64, n: u64, at: Instant) {
        for s in first..first + n {
            f.retain_at(r(s, s, CHUNK), at);
        }
    }

    #[test]
    fn nothing_that_may_still_be_in_flight_is_evicted_over_the_cap_the_writer_is_told_to_wait_instead() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        // 20 MiB 都是刚发出的：上限 16 MiB 也一个都不逐出（之前的实现会逐出，hub 随后退回它们时就找不到原文了）
        fill(&mut f, 1, 200, t0);
        assert!(f.retained_bytes() > RETAIN_BYTES);
        assert!((1..=200).all(|s| f.contains(s)), "everything sent within the guard is kept");
        assert!(!f.has_room(at(t0, 1000), 4 * 1024 * 1024), "the window is full: do not start another big message");
        assert!(f.has_room(at(t0, 1000), 64 * 1024), "a small message still fits in the headroom");
        fill(&mut f, 201, 30, at(t0, 2000));
        assert!(!f.has_room(at(t0, 2000), 64 * 1024), "past the headroom even small ones wait");
        assert!((1..=230).all(|s| f.contains(s)));
        // 老过了在途保护（10 秒）的才能逐出，而且只逐出到上限为止
        let later = t0 + IN_FLIGHT_GUARD + Duration::from_secs(1);
        assert!(f.has_room(later, 4 * 1024 * 1024));
        assert!(f.retained_bytes() <= RETAIN_BYTES && f.retained_bytes() > RETAIN_BYTES - CHUNK);
        assert!(!f.contains(1) && f.contains(230), "evicted from the oldest end");
        // 刚好在保护期内（9.9 秒）不算老
        let mut g = SendFlow::default();
        fill(&mut g, 1, 200, t0);
        assert!(!g.has_room(t0 + IN_FLIGHT_GUARD - Duration::from_millis(100), 4 * 1024 * 1024));
        assert!(g.contains(1));
    }

    #[test]
    fn the_newest_message_is_never_evicted_while_it_is_being_written_and_old_ones_make_room_for_it() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        fill(&mut f, 1, 200, t0); // 20 MiB 的旧消息
        let now = t0 + Duration::from_secs(30);
        // 一条 12 MiB 的新消息（60 片，同组）：写它的时候旧的（已经老过保护期）让位，它自己整条保留
        for s in 1000..1060u64 {
            f.retain_at(r(s, 7777, 200 * 1024), now);
        }
        assert!((1000..1060).all(|s| f.contains(s)), "every fragment of the message being written is kept");
        assert!(f.retained_bytes() <= RETAIN_BYTES, "{}", f.retained_bytes());
        assert!(!f.contains(1) && f.contains(200), "the oldest were evicted, the newer old ones were not needed");
        // 一条比整个窗口还大的消息：同样整条保留（保留量最多超出一条消息）
        let mut f = SendFlow::default();
        for s in 1..=90u64 {
            f.retain_at(r(s, 1, 200 * 1024), t0); // 17.6 MiB，同组
        }
        assert!((1..=90).all(|s| f.contains(s)));
    }

    #[test]
    fn a_pending_resend_and_everything_after_it_are_protected_however_old() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        for s in 1..=100u64 {
            f.retain_at(r(s, s, 400 * 1024), t0); // 39 MiB
        }
        // hub 退回了 30：它之后的信封 hub 都会（或已经）退回，回执可能还没到——不能逐出
        assert!(f.bounce(30, t0, None));
        let old = t0 + Duration::from_secs(120);
        assert!(
            !f.has_room(old, 4 * 1024 * 1024),
            "only the 29 before the bounced one could go, and that is not enough"
        );
        assert!((1..30).all(|s| !f.contains(s)), "the old ones before it are gone");
        assert!(f.contains(30) && (30..=100).all(|s| f.contains(s)), "the bounced one and all after it stay");
        assert_eq!(f.outstanding(), vec![30]);
        assert_eq!(f.retained_bytes(), 71 * 400 * 1024);
        // 重发出去之后它们不再是「待重发」，但又是刚发出的（重新计时）：仍然不逐出
        let batch = f.take_batch(old, usize::MAX);
        assert_eq!(batch.len(), 1);
        assert!(!f.has_room(old + Duration::from_secs(1), 4 * 1024 * 1024));
        assert!(f.contains(30), "a resent envelope is in flight again");
        // 老过保护期之后才轮到
        assert!(f.has_room(old + IN_FLIGHT_GUARD + Duration::from_secs(1), 4 * 1024 * 1024));
        assert!(!f.contains(30));
    }

    #[test]
    fn old_retained_envelopes_expire_unless_they_are_still_waiting_for_a_resend() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        for s in 1..=4u64 {
            f.retain_at(r(s, s, 100), t0);
        }
        assert!(f.bounce(2, t0, None));
        let later = t0 + RETAIN_MAX_AGE + Duration::from_secs(1);
        // 过期的放掉，但队首（1）之后的 2 还要重发：停在它前面
        assert!(!f.expire(later, RETAIN_MAX_AGE));
        assert!(!f.contains(1) && f.contains(2) && f.contains(3), "{}", f.retained_bytes());
        let _ = f.take_batch(later, usize::MAX);
        assert!(!f.expire(later, RETAIN_MAX_AGE), "the resent ones count from the resend");
        let much_later = later + RETAIN_MAX_AGE + Duration::from_secs(1);
        assert!(f.expire(much_later, RETAIN_MAX_AGE), "everything is old now and nothing waits: the flow can go");
        assert_eq!(f.retained_bytes(), 0);
        // 还没过期的不动
        f.retain(r(9, 9, 10));
        assert!(!f.expire(Instant::now(), RETAIN_MAX_AGE));
        assert!(f.contains(9));
    }

    #[test]
    fn batches_are_bounded_by_bytes_but_always_carry_at_least_one() {
        let t0 = Instant::now();
        let mut f = SendFlow::default();
        for s in 1..=10u64 {
            f.retain(r(s, s, 1000));
            f.defer(s);
        }
        let b = f.take_batch(t0, 2500);
        assert_eq!(
            b.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "stops once the byte budget is reached"
        );
        let b = f.take_batch(t0, 1);
        assert_eq!(b.len(), 1);
        assert_eq!(f.outstanding(), vec![5, 6, 7, 8, 9, 10]);
        assert_eq!(f.take_outstanding(), vec![5, 6, 7, 8, 9, 10]);
        assert!(f.outstanding().is_empty());
    }
}
