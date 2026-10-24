//! 「粘住」状态：保证 hub 对已配对 `(from, to)` 的 `data` / `notify` 只有两种结果——转发，或退回 `backpressure`，
//! 并且**转发出去的永远是升序**。
//!
//! 为什么需要它：Noise 传输用隐式递增的 nonce，一个信封被退回、后面的信封却被收下，接收方就会看到乱序或缺片，
//! 只能重置整条传输。所以 hub 为每对 `(发送连接, 目标设备)` 记一个**被退回 `seq` 的有序集合**：
//!
//! - 集合为空：信封按正常规则转发，或退回（退回的 `seq` 加入集合）；
//! - 集合非空：只有 `seq` 等于集合最小值的信封才尝试转发（成功就从集合去掉，失败仍留在集合里并退回）；
//!   其它任何 `seq`——更大的，以及重复到达的更小的——一律退回并加入集合；
//! - 集合最多 [`MAX_RETURNED`] 项，超过说明发送方不守规则，由调用方断开它（`conn.rs`，1008）。
//!
//! 这样顺序保证不依赖发送方是否已经收齐回执：回执在路上、发送方先重发了后面的 `seq`，hub 也只会再退回一次，
//! 不会先放行一个更大的。只记一个被退回的 `seq`、它一放行就整条清掉的旧做法，
//! 会在受限链路上放出 `246 → 248 → 249 → 247`。规则见 docs/protocol.md § 3.1、docs/spec/hub.md § 3.3。
//!
//! 记录按**发送连接**（不是设备）分，这样同一设备的新旧连接互不干扰；清理有这几条路：
//!
//! - 集合排空（最小值一个个放行）：[`Sticky::release`] 在最后一个 `seq` 放行时删掉整条记录；
//! - 任一方向成功转发了 `handshake`：[`Sticky::clear`]，两个方向都清（传输被重置后发送方不会再重发旧信封）；
//! - 发送连接结束：[`Sticky::forget_sender`]；
//! - 目标设备下线 / 被顶掉 / 被吊销：[`Sticky::forget_toward`]，由 [`super::registry::Registry`] 在持有路由表锁时调用，
//!   所以不会和「登记新记录」交错出泄漏；
//! - 兜底：查询时发现记录里的目标连接已不是当前那条，就当作过期丢掉。
//!
//! 这里只有设备 id、连接号、`seq` 和原因，**没有任何信封内容**。

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

/// 每对 `(发送连接, 目标)` 最多同时记多少个被退回的 `seq`。守规则的发送方一收到回执就暂停，在途的信封量有限，
/// 远到不了这个数；超过的是无视回执一直狂发的发送方，调用方以 `1008` 断开它。
pub const MAX_RETURNED: usize = 4096;

/// 信封被退回的原因。只影响「持续超限」的判定（见 `conn.rs`）：目标太慢不是发送方的错。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// 目标连接的发送缓冲满了。
    BufferFull,
    /// 发送方超过了自己的速率预算。
    RateLimited,
    /// 账号的预览流量预算用完了（docs/spec/hub.md § 13.5）。和 `BufferFull` 一样不是这条连接自己的错：
    /// 预算由账号下所有预览共用，不计入「一直被限速退回 10 秒就断开」。
    PreviewBudget,
}

/// 当前该放行的是哪一个：集合非空时才有。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// 集合的最小值：只有它的（重）发才能被尝试转发。
    pub min: u64,
    /// 最小值最近一次被退回的原因（它失败了、或集合刚建立时的那次）。
    pub cause: Cause,
}

/// 被退回的 `seq` 太多了（超过 [`MAX_RETURNED`]），没有记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyReturned;

/// 一对 `(发送连接, 目标)` 的记录。
#[derive(Debug)]
struct Entry {
    /// 被退回、还没放行的 `seq`，升序。
    returned: BTreeSet<u64>,
    /// 最小值最近一次被退回的原因。
    cause: Cause,
    /// 记录时目标设备的连接号；目标换了连接，记录就失效。
    target_conn: u64,
}

/// 全部粘住记录：发送连接号 → 目标设备 id → 记录。
#[derive(Default)]
pub struct Sticky {
    inner: Mutex<HashMap<u64, HashMap<String, Entry>>>,
}

/// `Debug` 只输出数量（没有 seq，更没有内容）。
impl std::fmt::Debug for Sticky {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sticky").field("entries", &self.len()).field("seqs", &self.seq_count()).finish()
    }
}

impl Sticky {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, HashMap<String, Entry>>> {
        self.inner.lock().expect("sticky")
    }

    /// `sender` 连接发往 `to` 的当前闸门（集合的最小值）。没有记录返回 `None`。
    /// 记录里的目标连接不是 `target_conn`（对方重连过）就当过期：删掉，返回 `None`。
    pub fn gate(&self, sender: u64, to: &str, target_conn: u64) -> Option<Gate> {
        let mut g = self.lock();
        let per_sender = g.get_mut(&sender)?;
        let entry = per_sender.get(to)?;
        if entry.target_conn == target_conn {
            let min = *entry.returned.first().expect("an entry is never empty");
            return Some(Gate { min, cause: entry.cause });
        }
        per_sender.remove(to);
        if per_sender.is_empty() {
            g.remove(&sender);
        }
        None
    }

    /// 一封信封被退回：把它的 `seq` 加入集合（已经在里面就不动）。
    ///
    /// - `cause = Some(..)` 是它自己没转发出去的原因（缓冲满 / 超速）：集合刚建立，或者它就是最小值时记为集合的原因；
    /// - `cause = None` 是被粘住（集合非空、它不是最小值）而退回的后续信封，不改原因。
    ///
    /// 集合已经有 [`MAX_RETURNED`] 项、这个 `seq` 又是新的，返回 `Err(TooManyReturned)`，不记录。
    pub fn returned(
        &self,
        sender: u64,
        to: &str,
        target_conn: u64,
        seq: u64,
        cause: Option<Cause>,
    ) -> Result<(), TooManyReturned> {
        let mut g = self.lock();
        let per_sender = g.entry(sender).or_default();
        match per_sender.get_mut(to) {
            Some(e) if e.target_conn == target_conn => {
                if !e.returned.contains(&seq) && e.returned.len() >= MAX_RETURNED {
                    return Err(TooManyReturned);
                }
                let is_min = e.returned.first().is_none_or(|m| seq <= *m);
                e.returned.insert(seq);
                if let Some(c) = cause.filter(|_| is_min) {
                    e.cause = c;
                }
            }
            // 没有记录，或记录属于目标的旧连接：重新开始。
            _ => {
                per_sender.insert(
                    to.to_string(),
                    Entry { returned: BTreeSet::from([seq]), cause: cause.unwrap_or(Cause::BufferFull), target_conn },
                );
            }
        }
        Ok(())
    }

    /// 集合里的最小值 `seq` 放行成功了：从集合去掉。集合排空就删掉整条记录。返回还剩几个。
    /// 记录已经被别处清掉了（比如同时转发了一个握手）也不是错，返回 0。
    pub fn release(&self, sender: u64, to: &str, seq: u64) -> usize {
        let mut g = self.lock();
        let Some(per_sender) = g.get_mut(&sender) else { return 0 };
        let Some(entry) = per_sender.get_mut(to) else { return 0 };
        entry.returned.remove(&seq);
        let left = entry.returned.len();
        if left == 0 {
            per_sender.remove(to);
            if per_sender.is_empty() {
                g.remove(&sender);
            }
        }
        left
    }

    /// 无条件清掉 `(sender, to)` 的记录：对方是离线 / 重新握手了，旧的顺序已经没有意义。
    pub fn clear(&self, sender: u64, to: &str) {
        let mut g = self.lock();
        if let Some(per_sender) = g.get_mut(&sender) {
            per_sender.remove(to);
            if per_sender.is_empty() {
                g.remove(&sender);
            }
        }
    }

    /// 发送连接结束。
    pub fn forget_sender(&self, sender: u64) {
        self.lock().remove(&sender);
    }

    /// 目标设备不在了（断开、被新连接顶掉、被吊销）：所有发给它的记录作废。
    pub fn forget_toward(&self, device: &str) {
        let mut g = self.lock();
        g.retain(|_, per_sender| {
            per_sender.remove(device);
            !per_sender.is_empty()
        });
    }

    /// 记录条数，即有几对 `(发送连接, 目标)` 被粘住（测试和 `Debug` 用）。
    pub fn len(&self) -> usize {
        self.lock().values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 所有记录里被退回的 `seq` 一共多少个（测试和 `Debug` 用）。
    pub fn seq_count(&self) -> usize {
        self.lock().values().flat_map(HashMap::values).map(|e| e.returned.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 照 `conn.rs::forward` 对 `data` / `notify` 的做法判定一封信封（发送连接 1 → 目标 "b"，目标连接 7）：
    /// `target_accepts` 是目标缓冲此刻装不装得下。返回它是否被转发。
    fn arrive(s: &Sticky, seq: u64, target_accepts: bool) -> bool {
        let gate = s.gate(1, "b", 7);
        if gate.is_some_and(|g| g.min != seq) {
            s.returned(1, "b", 7, seq, None).unwrap(); // 被粘住而退回
            return false;
        }
        if !target_accepts {
            s.returned(1, "b", 7, seq, Some(Cause::BufferFull)).unwrap();
            return false;
        }
        if gate.is_some() {
            s.release(1, "b", seq);
        }
        true
    }

    fn returned_seqs(s: &Sticky) -> Vec<u64> {
        let g = s.inner.lock().unwrap();
        g.values().flat_map(HashMap::values).flat_map(|e| e.returned.iter().copied()).collect()
    }

    /// 受限链路上的序列：旧实现放行顺序是 `246, 248, 249, 247`。
    #[test]
    fn a_larger_seq_cannot_overtake_a_returned_one_whose_receipt_is_still_in_flight() {
        let s = Sticky::default();
        // 246、247 都被退回（目标装不下），发送方只来得及收到 246 的回执
        assert!(!arrive(&s, 246, false));
        assert!(!arrive(&s, 247, false));
        assert_eq!(s.gate(1, "b", 7).unwrap().min, 246);
        // 246 重发，放行
        assert!(arrive(&s, 246, true));
        assert_eq!(s.gate(1, "b", 7).unwrap().min, 247, "247 is still owed, the record is not gone");
        // 发送方不知道 247 还欠着，接着重发它已知的 248：目标现在装得下，但必须被退回
        assert!(!arrive(&s, 248, true), "248 may not overtake 247");
        assert!(!arrive(&s, 249, true));
        assert_eq!(returned_seqs(&s), vec![247, 248, 249]);
        // 回执到了：升序重发 247、248、249，依次放行
        assert!(arrive(&s, 247, true));
        assert!(arrive(&s, 248, true));
        assert!(arrive(&s, 249, true));
        assert!(s.is_empty(), "the last release removes the record");
        // 之后的新信封按正常规则走
        assert!(arrive(&s, 250, true));
    }

    #[test]
    fn with_an_empty_set_envelopes_are_forwarded_or_returned_by_the_normal_rules() {
        let s = Sticky::default();
        assert!(s.gate(1, "b", 7).is_none());
        assert!(arrive(&s, 1, true));
        assert!(arrive(&s, 2, true));
        assert!(s.is_empty());
        assert!(!arrive(&s, 3, false));
        assert_eq!(s.gate(1, "b", 7), Some(Gate { min: 3, cause: Cause::BufferFull }));
    }

    #[test]
    fn only_the_minimum_is_tried_and_a_failed_try_keeps_it_in_the_set() {
        let s = Sticky::default();
        for seq in [10, 11, 12] {
            assert!(!arrive(&s, seq, false));
        }
        // 最小值重发又失败：留在集合里，原因更新；更大的照样被退回
        s.returned(1, "b", 7, 10, Some(Cause::RateLimited)).unwrap();
        assert_eq!(s.gate(1, "b", 7), Some(Gate { min: 10, cause: Cause::RateLimited }));
        assert!(!arrive(&s, 12, true));
        assert!(!arrive(&s, 11, true));
        assert_eq!(returned_seqs(&s), vec![10, 11, 12]);
        assert_eq!(s.seq_count(), 3);
        // 重复到达的最小值，这次装得下：放行；下一个最小值（11）这时才轮到
        assert!(arrive(&s, 10, true));
        assert!(!arrive(&s, 12, true), "12 still has to wait for 11");
        assert!(arrive(&s, 11, true));
        assert!(arrive(&s, 12, true));
        assert!(s.is_empty());
    }

    #[test]
    fn a_smaller_duplicate_is_returned_and_recorded_not_forwarded() {
        let s = Sticky::default();
        assert!(!arrive(&s, 20, false));
        assert!(!arrive(&s, 21, false));
        // 重复到达的更小的（发送方不守规矩）：退回并加入集合，成为新的最小值
        assert!(!arrive(&s, 5, true));
        assert_eq!(returned_seqs(&s), vec![5, 20, 21]);
        assert_eq!(s.gate(1, "b", 7).unwrap().min, 5);
        assert!(!arrive(&s, 20, true), "20 waits for 5");
    }

    #[test]
    fn a_resend_that_is_returned_again_is_not_counted_twice() {
        let s = Sticky::default();
        for _ in 0..10 {
            assert!(!arrive(&s, 7, false));
            assert!(!arrive(&s, 8, true));
        }
        assert_eq!(s.seq_count(), 2);
    }

    #[test]
    fn the_cause_follows_the_minimum_only() {
        let s = Sticky::default();
        s.returned(1, "b", 7, 10, Some(Cause::BufferFull)).unwrap();
        // 别的 seq 因为超速而退回：不改最小值的原因
        s.returned(1, "b", 7, 11, Some(Cause::RateLimited)).unwrap();
        s.returned(1, "b", 7, 12, None).unwrap();
        assert_eq!(s.gate(1, "b", 7), Some(Gate { min: 10, cause: Cause::BufferFull }));
        s.returned(1, "b", 7, 10, Some(Cause::RateLimited)).unwrap();
        assert_eq!(s.gate(1, "b", 7), Some(Gate { min: 10, cause: Cause::RateLimited }));
        // 最小值放行后原因保持（新的最小值下一次失败时才更新）
        assert_eq!(s.release(1, "b", 10), 2);
        assert_eq!(s.gate(1, "b", 7), Some(Gate { min: 11, cause: Cause::RateLimited }));
    }

    #[test]
    fn more_than_the_limit_of_returned_seqs_is_refused_and_not_recorded() {
        let s = Sticky::default();
        for seq in 1..=MAX_RETURNED as u64 {
            assert_eq!(s.returned(1, "b", 7, seq, Some(Cause::RateLimited)), Ok(()), "seq {seq}");
        }
        assert_eq!(s.seq_count(), MAX_RETURNED);
        // 已经在集合里的 seq 再退回一次不算新增
        assert_eq!(s.returned(1, "b", 7, 1, Some(Cause::RateLimited)), Ok(()));
        assert_eq!(s.returned(1, "b", 7, MAX_RETURNED as u64, None), Ok(()));
        // 第 4097 个不同的 seq：拒绝，集合不变
        let over = MAX_RETURNED as u64 + 1;
        assert_eq!(s.returned(1, "b", 7, over, None), Err(TooManyReturned));
        assert_eq!(s.seq_count(), MAX_RETURNED);
        assert!(!returned_seqs(&s).contains(&over));
        // 别的目标、别的发送连接各有各的集合
        assert_eq!(s.returned(1, "c", 8, over, Some(Cause::BufferFull)), Ok(()));
        assert_eq!(s.returned(2, "b", 7, over, Some(Cause::BufferFull)), Ok(()));
        // 放行一个之后又有空位
        assert_eq!(s.release(1, "b", 1), MAX_RETURNED - 1);
        assert_eq!(s.returned(1, "b", 7, over, None), Ok(()));
    }

    #[test]
    fn release_tolerates_a_record_that_is_already_gone() {
        let s = Sticky::default();
        assert_eq!(s.release(1, "b", 5), 0);
        s.returned(1, "b", 7, 5, Some(Cause::BufferFull)).unwrap();
        s.clear(1, "b"); // 比如同时转发了一个握手
        assert_eq!(s.release(1, "b", 5), 0);
        assert!(s.is_empty());
        assert!(s.inner.lock().unwrap().is_empty(), "no empty maps left behind");
    }

    #[test]
    fn clear_removes_only_that_pair() {
        let s = Sticky::default();
        s.returned(1, "b", 7, 10, Some(Cause::BufferFull)).unwrap();
        s.returned(1, "b", 7, 11, None).unwrap();
        s.returned(1, "c", 8, 3, Some(Cause::BufferFull)).unwrap();
        s.returned(2, "b", 7, 5, Some(Cause::RateLimited)).unwrap();
        s.clear(1, "b");
        assert!(s.gate(1, "b", 7).is_none());
        assert!(s.gate(1, "c", 8).is_some());
        assert!(s.gate(2, "b", 7).is_some());
        assert_eq!(s.len(), 2);
        assert_eq!(s.seq_count(), 2);
    }

    #[test]
    fn a_record_for_an_old_target_connection_is_stale() {
        let s = Sticky::default();
        s.returned(1, "b", 7, 10, Some(Cause::BufferFull)).unwrap();
        s.returned(1, "b", 7, 11, None).unwrap();
        // b 重连成了 8 号连接：旧记录不再适用，并且被清掉
        assert!(s.gate(1, "b", 8).is_none());
        assert!(s.is_empty());
        // 登记新记录时同样覆盖掉属于旧连接的记录（旧的 seq 不带过来）
        s.returned(1, "b", 7, 10, Some(Cause::BufferFull)).unwrap();
        s.returned(1, "b", 8, 99, Some(Cause::RateLimited)).unwrap();
        assert_eq!(s.gate(1, "b", 8), Some(Gate { min: 99, cause: Cause::RateLimited }));
        assert_eq!(s.seq_count(), 1);
    }

    #[test]
    fn forgetting_a_sender_or_a_target_leaves_nothing_behind() {
        let s = Sticky::default();
        s.returned(1, "b", 7, 10, Some(Cause::BufferFull)).unwrap();
        s.returned(1, "b", 7, 12, None).unwrap();
        s.returned(1, "c", 8, 3, Some(Cause::BufferFull)).unwrap();
        s.returned(2, "b", 7, 5, Some(Cause::RateLimited)).unwrap();
        s.forget_toward("b");
        assert_eq!(s.len(), 1);
        assert!(s.gate(1, "c", 8).is_some());
        s.forget_sender(1);
        assert!(s.is_empty());
        // 内层空 map 也不留（不泄漏）
        assert!(s.inner.lock().unwrap().is_empty());
    }

    #[test]
    fn debug_output_has_only_counts() {
        let s = Sticky::default();
        s.returned(1, "secret-device-id", 7, 123_456, Some(Cause::BufferFull)).unwrap();
        s.returned(1, "secret-device-id", 7, 654_321, None).unwrap();
        let d = format!("{s:?}");
        assert!(d.contains("entries: 1") && d.contains("seqs: 2"), "{d}");
        assert!(!d.contains("secret") && !d.contains("123456") && !d.contains("654321"), "{d}");
    }
}
