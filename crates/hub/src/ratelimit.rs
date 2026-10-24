//! 内存令牌桶限速：[`KeyedLimiter`] 是通用实现（键可以是账号 id 等），[`RateLimiter`] 是按真实客户端 IP
//! （见 [`crate::clientip`]）的包装。IPv6 按 `/64` 归并：一个终端用户通常拿到整个 `/64`，不归并的话轮换地址就能
//! 绕过限速；IPv4 按完整地址。
//!
//! 每个桶容量 = 周期内的配额，匀速回填：配额 10/min 即一开始可连发 10 次，之后每 6 秒回一个。
//! 多实例部署时各实例独立计数，可接受（docs/spec/hub.md § 7.2）。

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 限速桶的键：IPv4 用完整地址，IPv6 清掉低 64 位（只留 `/64` 前缀）。
pub fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
    }
}

/// 通用令牌桶：键可以是 IP、账号 id 等。容量 = 周期内的配额，匀速回填。
pub struct KeyedLimiter<K> {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<K, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

impl<K: Hash + Eq + Clone> KeyedLimiter<K> {
    /// 每 `period` 最多 `n` 次（至少 1）。
    pub fn new(n: u32, period: Duration) -> Self {
        let capacity = f64::from(n.max(1));
        Self {
            capacity,
            refill_per_sec: capacity / period.as_secs_f64().max(0.001),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// 每小时 `n` 次。
    pub fn per_hour(n: u32) -> Self {
        Self::new(n, Duration::from_secs(3600))
    }

    /// 每天 `n` 次。
    pub fn per_day(n: u32) -> Self {
        Self::new(n, Duration::from_secs(24 * 3600))
    }

    /// 消耗一个令牌。超限返回需要等待的时间。
    pub fn check(&self, key: &K) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &K, now: Instant) -> Result<(), Duration> {
        let mut map = self.buckets.lock().expect("ratelimit mutex");
        let b = map.entry(key.clone()).or_insert(Bucket { tokens: self.capacity, last: now });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - b.tokens) / self.refill_per_sec;
            Err(Duration::from_secs_f64(wait))
        }
    }

    /// 清掉已经回满的桶，防止 map 无限增长。
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut map = self.buckets.lock().expect("ratelimit mutex");
        map.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
            b.tokens + elapsed * self.refill_per_sec < self.capacity
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// 按键（账号）的字节令牌桶：容量 = 每分钟字节数，匀速回填。预览流量的账号预算用它（docs/spec/hub.md § 13.5）。
///
/// 和 [`KeyedLimiter`] 不同，「能不能放行」（[`ByteBudgets::check`]）和「扣额度」（[`ByteBudgets::charge`]）是两步：
/// 只在转发**成功之后**才扣，被退回的信封不占额度，发送方等够提示的时间重发就一定放得过。
/// 两个方向的流量共用一个桶，所以并发时可能略微超出，下一次 `check` 会看到余额不足。
pub struct ByteBudgets<K> {
    capacity: f64,
    refill_per_sec: f64,
    buckets: Mutex<HashMap<K, Bucket>>,
}

impl<K: Hash + Eq + Clone> ByteBudgets<K> {
    /// 每分钟 `bytes` 字节（至少 1）。
    pub fn per_minute(bytes: u64) -> Self {
        let capacity = bytes.max(1) as f64;
        Self { capacity, refill_per_sec: capacity / 60.0, buckets: Mutex::new(HashMap::new()) }
    }

    /// 还能不能转发 `len` 字节。不够返回估算的恢复时间。不扣额度。
    /// 比整个桶还大的信封（预算被设得比信封还小）在桶满时放行一个，不会永远过不去。
    pub fn check(&self, key: &K, len: usize) -> Result<(), Duration> {
        self.check_at(key, len, Instant::now())
    }

    fn check_at(&self, key: &K, len: usize, now: Instant) -> Result<(), Duration> {
        let mut map = self.buckets.lock().expect("byte budget mutex");
        let b = self.refilled(&mut map, key, now);
        let need = (len as f64).min(self.capacity);
        if b.tokens >= need {
            Ok(())
        } else {
            Err(Duration::from_secs_f64((need - b.tokens) / self.refill_per_sec))
        }
    }

    /// 转发成功了：扣 `len` 字节。
    pub fn charge(&self, key: &K, len: usize) {
        self.charge_at(key, len, Instant::now());
    }

    fn charge_at(&self, key: &K, len: usize, now: Instant) {
        let mut map = self.buckets.lock().expect("byte budget mutex");
        let b = self.refilled(&mut map, key, now);
        b.tokens = (b.tokens - (len as f64).min(self.capacity)).max(0.0);
    }

    fn refilled<'a>(&self, map: &'a mut HashMap<K, Bucket>, key: &K, now: Instant) -> &'a mut Bucket {
        let b = map.entry(key.clone()).or_insert(Bucket { tokens: self.capacity, last: now });
        let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        b.last = now;
        b
    }

    /// 清掉已经回满的桶，防止 map 无限增长。
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut map = self.buckets.lock().expect("byte budget mutex");
        map.retain(|_, b| {
            let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
            b.tokens + elapsed * self.refill_per_sec < self.capacity
        });
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// 滚动窗口计数器：任意 `window` 长的区间内最多 `limit` 个事件。
///
/// 和令牌桶不同，它不会「回填」：令牌桶每天 10 个的配额在一个滚动的 24 小时里最多能用到 20 个（先用光一桶，再等回填），
/// 这里是严格的「最近 24 小时内不超过 10 个」。邮箱验证码的每日上限用它（不管请求来自哪个 IP，
/// 一个邮箱一天能发几个码、能被猜几次，都有硬上限）。
///
/// 键的数量有上限（`max_keys`），免得有人拿海量不同的邮箱把内存撑爆：满了而且清理过期条目之后还是满，
/// [`WindowCounter::try_record`] 对没见过的新键一律拒绝（宁可让新邮箱暂时要不了码，也不 OOM），
/// [`WindowCounter::record`] 则不再记新键（调用方另有兜底，见 `http::email`）。
pub struct WindowCounter<K> {
    limit: usize,
    window: Duration,
    max_keys: usize,
    events: Mutex<HashMap<K, VecDeque<Instant>>>,
}

/// 默认最多同时跟踪多少个键（每个键最多 `limit` 个时间戳，几十字节到几百字节）。
const DEFAULT_MAX_KEYS: usize = 200_000;

impl<K: Hash + Eq + Clone> WindowCounter<K> {
    /// `window` 内最多 `limit` 个事件（`limit` 至少 1）。
    pub fn new(limit: u32, window: Duration) -> Self {
        Self::with_max_keys(limit, window, DEFAULT_MAX_KEYS)
    }

    /// 同 [`WindowCounter::new`]，并限制同时跟踪的键数。
    pub fn with_max_keys(limit: u32, window: Duration, max_keys: usize) -> Self {
        Self { limit: limit.max(1) as usize, window, max_keys: max_keys.max(1), events: Mutex::new(HashMap::new()) }
    }

    /// 记一个事件。窗口里已经有 `limit` 个就不记，返回要等多久最老的那个才滑出窗口。
    pub fn try_record(&self, key: &K) -> Result<(), Duration> {
        self.try_record_at(key, Instant::now())
    }

    fn try_record_at(&self, key: &K, now: Instant) -> Result<(), Duration> {
        let mut map = self.events.lock().expect("window counter mutex");
        if !map.contains_key(key) && map.len() >= self.max_keys {
            Self::sweep_map(&mut map, now, self.window);
            if map.len() >= self.max_keys {
                return Err(self.window);
            }
        }
        let q = map.entry(key.clone()).or_default();
        Self::expire(q, now, self.window);
        if q.len() >= self.limit {
            let oldest = q.front().copied().unwrap_or(now);
            return Err((oldest + self.window).saturating_duration_since(now).max(Duration::from_secs(1)));
        }
        q.push_back(now);
        Ok(())
    }

    /// 窗口里是不是已经有 `limit` 个事件。不创建键。
    pub fn is_full(&self, key: &K) -> bool {
        self.is_full_at(key, Instant::now())
    }

    fn is_full_at(&self, key: &K, now: Instant) -> bool {
        let map = self.events.lock().expect("window counter mutex");
        map.get(key).is_some_and(|q| {
            q.iter().filter(|t| now.saturating_duration_since(**t) < self.window).count() >= self.limit
        })
    }

    /// 无条件记一个事件（已经有 `limit` 个时挤掉最老的）。跟踪的键满了就不记新键。
    pub fn record(&self, key: &K) {
        self.record_at(key, Instant::now());
    }

    fn record_at(&self, key: &K, now: Instant) {
        let mut map = self.events.lock().expect("window counter mutex");
        if !map.contains_key(key) && map.len() >= self.max_keys {
            Self::sweep_map(&mut map, now, self.window);
            if map.len() >= self.max_keys {
                return;
            }
        }
        let q = map.entry(key.clone()).or_default();
        Self::expire(q, now, self.window);
        if q.len() >= self.limit {
            q.pop_front();
        }
        q.push_back(now);
    }

    fn expire(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while q.front().is_some_and(|t| now.saturating_duration_since(*t) >= window) {
            q.pop_front();
        }
    }

    fn sweep_map(map: &mut HashMap<K, VecDeque<Instant>>, now: Instant, window: Duration) {
        map.retain(|_, q| {
            Self::expire(q, now, window);
            !q.is_empty()
        });
    }

    /// 清掉已经没有事件落在窗口里的键，防止 map 无限增长。
    pub fn sweep(&self) {
        let mut map = self.events.lock().expect("window counter mutex");
        Self::sweep_map(&mut map, Instant::now(), self.window);
    }

    /// 现在跟踪着多少个键。
    pub fn len(&self) -> usize {
        self.events.lock().expect("window counter mutex").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 按真实客户端 IP 的限速（IPv6 按 `/64` 归并）。
pub struct RateLimiter(KeyedLimiter<IpAddr>);

impl RateLimiter {
    /// 每分钟 `n` 次。
    pub fn per_minute(n: u32) -> Self {
        Self(KeyedLimiter::new(n, Duration::from_secs(60)))
    }

    /// 每小时 `n` 次。
    pub fn per_hour(n: u32) -> Self {
        Self(KeyedLimiter::per_hour(n))
    }

    /// 消耗一个令牌。超限返回需要等待的时间。
    pub fn check(&self, ip: IpAddr) -> Result<(), Duration> {
        self.0.check(&bucket_key(ip))
    }

    #[cfg(test)]
    fn check_at(&self, ip: IpAddr, now: Instant) -> Result<(), Duration> {
        self.0.check_at(&bucket_key(ip), now)
    }

    /// 清掉已经回满的桶，防止 map 无限增长。
    pub fn sweep(&self) {
        self.0.sweep();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_limits() {
        let rl = RateLimiter::per_minute(10);
        let ip: IpAddr = "1.1.1.1".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(rl.check_at(ip, t0).is_ok());
        }
        let wait = rl.check_at(ip, t0).unwrap_err();
        assert!(wait > Duration::from_secs(5) && wait <= Duration::from_secs(6));
    }

    #[test]
    fn refills_over_time_and_isolates_ips() {
        let rl = RateLimiter::per_minute(10);
        let a: IpAddr = "1.1.1.1".parse().unwrap();
        let b: IpAddr = "2.2.2.2".parse().unwrap();
        let t0 = Instant::now();
        for _ in 0..10 {
            rl.check_at(a, t0).unwrap();
        }
        assert!(rl.check_at(a, t0).is_err());
        assert!(rl.check_at(b, t0).is_ok());
        assert!(rl.check_at(a, t0 + Duration::from_secs(7)).is_ok());
    }

    #[test]
    fn ipv6_is_counted_per_slash_64() {
        let rl = RateLimiter::per_minute(3);
        let t0 = Instant::now();
        // 同一个 /64 里轮换地址：共用一个桶
        for last in 1..=3u16 {
            let ip: IpAddr = format!("2001:db8:1:2::{last:x}").parse().unwrap();
            assert!(rl.check_at(ip, t0).is_ok());
        }
        let rotated: IpAddr = "2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap();
        assert!(rl.check_at(rotated, t0).is_err(), "rotating inside a /64 must not reset the limit");
        // 另一个 /64 是另一个桶
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(rl.check_at(other, t0).is_ok());
        // IPv4 仍按完整地址
        for last in 1..=3u8 {
            let ip: IpAddr = format!("192.0.2.{last}").parse().unwrap();
            assert!(rl.check_at(ip, t0).is_ok());
        }
        assert_eq!(
            bucket_key("2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap()),
            "2001:db8:1:2::".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn keyed_limiter_isolates_keys_and_refills_slowly() {
        let rl: KeyedLimiter<String> = KeyedLimiter::per_hour(30);
        let t0 = Instant::now();
        let (a, b) = ("acc-a".to_string(), "acc-b".to_string());
        for _ in 0..30 {
            rl.check_at(&a, t0).unwrap();
        }
        let wait = rl.check_at(&a, t0).unwrap_err();
        assert!(wait > Duration::from_secs(110) && wait <= Duration::from_secs(120), "{wait:?}");
        assert!(rl.check_at(&b, t0).is_ok(), "another key has its own bucket");
        assert!(rl.check_at(&a, t0 + Duration::from_secs(121)).is_ok());
        assert!(rl.check_at(&a, t0 + Duration::from_secs(121)).is_err());
        // 每天 N 次
        let daily: KeyedLimiter<String> = KeyedLimiter::per_day(2);
        daily.check_at(&a, t0).unwrap();
        daily.check_at(&a, t0).unwrap();
        assert!(daily.check_at(&a, t0 + Duration::from_secs(3600)).is_err());
        assert!(daily.check_at(&a, t0 + Duration::from_secs(12 * 3600 + 1)).is_ok());
        // 配额至少 1
        assert!(KeyedLimiter::<String>::per_hour(0).check_at(&a, t0).is_ok());
    }

    #[test]
    fn window_counter_is_a_strict_rolling_window_not_a_refilling_bucket() {
        let day = Duration::from_secs(24 * 3600);
        let c: WindowCounter<String> = WindowCounter::new(3, day);
        let k = "a@example.com".to_string();
        let t0 = Instant::now();
        for i in 0..3 {
            c.try_record_at(&k, t0 + Duration::from_secs(i * 60)).unwrap();
        }
        // 满了：要等最老的那个（t0）滑出窗口
        let wait = c.try_record_at(&k, t0 + Duration::from_secs(3600)).unwrap_err();
        assert_eq!(wait, day - Duration::from_secs(3600), "{wait:?}");
        // 令牌桶这时已经回填了一个；滑动窗口不会，一整天内都只有 3 个
        assert!(c.try_record_at(&k, t0 + Duration::from_secs(12 * 3600)).is_err());
        assert!(c.try_record_at(&k, t0 + day - Duration::from_secs(1)).is_err());
        // 最老的滑出之后只空出一个
        c.try_record_at(&k, t0 + day).unwrap();
        assert!(c.try_record_at(&k, t0 + day).is_err(), "the other two are still inside the window");
        c.try_record_at(&k, t0 + day + Duration::from_secs(60)).unwrap();
        // 别的键各算各的
        assert!(c.try_record_at(&"b@example.com".to_string(), t0).is_ok());
    }

    #[test]
    fn window_counter_failures_lock_after_the_limit_and_unlock_as_they_age_out() {
        let day = Duration::from_secs(24 * 3600);
        let c: WindowCounter<String> = WindowCounter::new(30, day);
        let k = "login:a@example.com".to_string();
        let t0 = Instant::now();
        assert!(!c.is_full_at(&k, t0), "an unknown key is not full, and checking does not create it");
        assert_eq!(c.len(), 0);
        for i in 0..29 {
            c.record_at(&k, t0 + Duration::from_secs(i));
            assert!(!c.is_full_at(&k, t0 + Duration::from_secs(i)), "{i}");
        }
        c.record_at(&k, t0 + Duration::from_secs(29));
        assert!(c.is_full_at(&k, t0 + Duration::from_secs(30)), "the 30th failure locks the email");
        assert!(c.is_full_at(&k, t0 + day - Duration::from_secs(1)));
        // 第一个失败滑出窗口，解锁
        assert!(!c.is_full_at(&k, t0 + day));
        // 锁着的时候继续记不会无限增长（只留最近 30 个）
        for i in 0..100 {
            c.record_at(&k, t0 + Duration::from_secs(40 + i));
        }
        assert_eq!(c.events.lock().unwrap()[&k].len(), 30);
    }

    #[test]
    fn window_counter_bounds_how_many_keys_it_tracks() {
        let hour = Duration::from_secs(3600);
        let c: WindowCounter<String> = WindowCounter::with_max_keys(2, hour, 3);
        let t0 = Instant::now();
        for k in ["a", "b", "c"] {
            c.try_record_at(&k.to_string(), t0).unwrap();
        }
        // 跟踪满了：没见过的新键被拒绝，见过的照常计数
        assert_eq!(c.try_record_at(&"d".to_string(), t0).unwrap_err(), hour);
        c.record_at(&"d".to_string(), t0);
        assert_eq!(c.len(), 3, "record() does not track a new key either");
        assert!(c.try_record_at(&"a".to_string(), t0).is_ok());
        // 过了窗口，旧键被清掉，腾出位置
        c.try_record_at(&"d".to_string(), t0 + hour).unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn sweep_drops_idle_buckets() {
        let rl = RateLimiter::per_minute(1);
        rl.check("1.1.1.1".parse().unwrap()).unwrap();
        assert_eq!(rl.len(), 1);
        rl.sweep(); // 刚用过，还没回满
        assert_eq!(rl.len(), 1);
    }
    #[test]
    fn byte_budget_charges_only_on_success_and_estimates_the_wait() {
        // 6000 字节 / 分钟 = 100 字节 / 秒
        let b: ByteBudgets<String> = ByteBudgets::per_minute(6000);
        let (a, other) = ("acc-a".to_string(), "acc-b".to_string());
        let t0 = Instant::now();
        assert!(b.check_at(&a, 4000, t0).is_ok());
        // check 不扣额度：同一封再问一次仍然放行
        assert!(b.check_at(&a, 4000, t0).is_ok());
        b.charge_at(&a, 4000, t0);
        // 还剩 2000，再来 4000 装不下：缺 2000 字节，补满要 20 秒
        assert_eq!(b.check_at(&a, 4000, t0).unwrap_err(), Duration::from_secs(20));
        // 被退回的信封没扣额度：等够提示的时间，重发的同一封一定放得过
        assert!(b.check_at(&a, 4000, t0 + Duration::from_secs(20)).is_ok());
        // 别的账号有自己的桶
        assert!(b.check_at(&other, 6000, t0).is_ok());
        // 扣到见底也不会变负
        b.charge_at(&a, 100_000, t0);
        assert_eq!(b.check_at(&a, 100, t0).unwrap_err(), Duration::from_secs(1));
    }

    #[test]
    fn byte_budget_lets_one_oversized_envelope_through_a_full_bucket() {
        let b: ByteBudgets<u8> = ByteBudgets::per_minute(1000);
        let t0 = Instant::now();
        assert!(b.check_at(&1, 5000, t0).is_ok(), "a full bucket admits one envelope bigger than itself");
        b.charge_at(&1, 5000, t0);
        // 之后要等一整分钟才补满
        assert_eq!(b.check_at(&1, 5000, t0).unwrap_err(), Duration::from_secs(60));
        assert!(b.check_at(&1, 5000, t0 + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn byte_budget_sweep_drops_refilled_buckets() {
        let b: ByteBudgets<u8> = ByteBudgets::per_minute(1000);
        b.charge(&1, 500);
        assert_eq!(b.len(), 1);
        b.sweep(); // 刚扣过，还没回满
        assert_eq!(b.len(), 1);
        let idle: ByteBudgets<u8> = ByteBudgets::per_minute(1000);
        idle.check(&2, 10).unwrap(); // 只看一眼会建一个满的桶，sweep 把它清掉
        idle.sweep();
        assert_eq!(idle.len(), 0);
    }
}
