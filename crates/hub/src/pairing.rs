//! 配对撮合（纯内存）。协议序列见 docs/protocol.md § 4.1，约束见 docs/spec/hub.md § 4。
//!
//! 状态机：
//! `PairBegin { match_id }` → 登记（一 host 一个登记，新的顶掉旧的，TTL 5 分钟）→ `PairJoin { match_id }`
//! 精确命中则登记立即失效，双方收到 `PairMatched`，hub 放行两人之间的 `handshake` 帧 →
//! 双方都 `PairConfirm` 后 [`finalize`] 才落库、签发设备 token、发 `PairDone`。
//! matched 之后 60 秒没有任何动静（握手帧 / confirm）则作废。
//!
//! **hub 看不到短码**：短码由 daemon 本地生成，两端各自算 `match_id = derive_match_id(code)` 发来撮合
//! （Noise XXpsk3 的 PSK 另从短码派生，hub 没有短码就无法做中间人，见 docs/protocol.md § 4.1 的残余风险）。
//! `match_id` 是短码的函数（拿到就能离线穷举），按**秘密**对待：不写日志、不落库、内存里也只存加盐哈希
//! （[`Broker`] 的 `Debug` 输出里只有数量），也不出现在任何错误信息里。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_protocol::{Control, EnrollFailReason, PairFailReason, PairRole};
use blake2::{Blake2s256, Digest};
use rand::rngs::OsRng;
use rand::RngCore;

use crate::app::AppState;
use crate::store::{NewAudit, NewDevice};
use crate::util::{clip, hash_token, ip_prefix, new_id, new_secret, now_ms, ordered};
use crate::ws::registry::Conn;

/// `match_id` 的格式：16 个 hex 字符（`derive_match_id` 的输出）。
const MATCH_ID_LEN: usize = 16;

/// 登记表里的键：`blake2s(进程内随机盐 || 小写 match_id)`。不存 match_id 本身。
type Key = [u8; 32];

/// 校验并规范化 `match_id`：恰好 16 个 ASCII hex 字符，统一成小写。格式不对返回 `None`。
pub fn normalize_match_id(input: &str) -> Option<String> {
    (input.len() == MATCH_ID_LEN && input.bytes().all(|b| b.is_ascii_hexdigit())).then(|| input.to_ascii_lowercase())
}

/// `PairBegin` 被拒绝的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginError {
    /// `match_id` 不是 16 位 hex。
    Malformed,
}

struct Registration {
    host: Arc<Conn>,
    expires: Instant,
}

struct Match {
    host: Arc<Conn>,
    joiner: Arc<Conn>,
    host_id: String,
    joiner_id: String,
    host_confirmed: bool,
    joiner_confirmed: bool,
    last_activity: Instant,
    created: Instant,
}

#[derive(Default)]
struct Inner {
    /// 登记键 → 登记。
    regs: HashMap<Key, Registration>,
    /// host 设备 id → 它当前登记的键（一个 host 同时只有一个有效登记）。
    by_host: HashMap<String, Key>,
    matches: HashMap<u64, Match>,
    /// 进行中的撮合：归一化的设备对 → match id。用于放行 handshake 帧。
    edges: HashMap<(String, String), u64>,
    next_match: u64,
}

fn edge(a: &str, b: &str) -> (String, String) {
    let (x, y) = ordered(a, b);
    (x.to_string(), y.to_string())
}

pub enum ConfirmOutcome {
    /// 不是任何进行中撮合的参与方，或 `peer` 对不上。
    Unknown,
    /// 已记录，等另一边。
    Waiting,
    /// 双方都确认了，可以落库。
    Ready { host: Arc<Conn>, joiner: Arc<Conn> },
}

pub struct Broker {
    inner: Mutex<Inner>,
    /// 进程内随机盐：登记键 = `blake2s(salt || match_id)`，内存转储 / `Debug` 输出里看不到 match_id。
    salt: [u8; 16],
    ttl: Duration,
    idle_timeout: Duration,
}

/// 只输出数量，永远不输出登记内容。
impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("Broker")
            .field("registrations", &g.regs.len())
            .field("matches", &g.matches.len())
            .finish_non_exhaustive()
    }
}

impl Broker {
    pub fn new(ttl: Duration, idle_timeout: Duration) -> Self {
        let mut salt = [0u8; 16];
        OsRng.fill_bytes(&mut salt);
        Self { inner: Mutex::new(Inner::default()), salt, ttl, idle_timeout }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("broker")
    }

    /// `normalize_match_id` 之后的 match_id → 登记键。
    fn key(&self, normalized_match_id: &str) -> Key {
        let mut h = Blake2s256::new();
        h.update(self.salt);
        h.update(normalized_match_id.as_bytes());
        h.finalize().into()
    }

    /// host 登记 `match_id`：作废它之前的登记（一 host 一个），返回新登记的过期时间（Unix 毫秒）。
    ///
    /// 别的 host 已用同一个 `match_id` 登记过（只可能是同一个短码的重试，比如 daemon 换了连接而旧连接还没被发现断开）：
    /// 新的顶掉旧的。能到这一步的人本来就得知道短码。
    pub fn begin(&self, host: &Arc<Conn>, match_id: &str) -> Result<u64, BeginError> {
        let key = self.key(&normalize_match_id(match_id).ok_or(BeginError::Malformed)?);
        let host_id = host.device_id();
        let mut g = self.lock();
        if let Some(old) = g.by_host.remove(&host_id) {
            g.regs.remove(&old);
        }
        if let Some(replaced) = g.regs.remove(&key) {
            let other = replaced.host.device_id();
            if g.by_host.get(&other) == Some(&key) {
                g.by_host.remove(&other);
            }
        }
        g.regs.insert(key, Registration { host: host.clone(), expires: Instant::now() + self.ttl });
        g.by_host.insert(host_id, key);
        Ok(now_ms() + self.ttl.as_millis() as u64)
    }

    /// joiner 发来 `match_id`，精确匹配。命中则通知双方 `PairMatched`。
    /// 格式不对、没有这个登记、已过期、host 已断开、自己 join 自己、joiner 已在另一场撮合里，一律 `NotFound`。
    pub fn join(&self, joiner: &Arc<Conn>, match_id: &str) -> Result<(), PairFailReason> {
        let key = self.key(&normalize_match_id(match_id).ok_or(PairFailReason::NotFound)?);
        let mut g = self.lock();
        let entry = g.regs.get(&key).ok_or(PairFailReason::NotFound)?;
        if entry.expires <= Instant::now() || entry.host.out.is_closed() {
            let host_id = entry.host.device_id();
            g.regs.remove(&key);
            g.by_host.remove(&host_id);
            return Err(PairFailReason::NotFound);
        }
        if Arc::ptr_eq(&entry.host, joiner) {
            return Err(PairFailReason::NotFound);
        }
        if g.matches.values().any(|m| Arc::ptr_eq(&m.joiner, joiner)) {
            // 同一个 joiner 同时只能有一场撮合。
            return Err(PairFailReason::NotFound);
        }
        let entry = g.regs.remove(&key).expect("checked above");
        let host = entry.host;
        let host_id = host.device_id();
        g.by_host.remove(&host_id);

        let joiner_id = joiner.device_id();
        g.next_match += 1;
        let id = g.next_match;
        g.edges.insert(edge(&host_id, &joiner_id), id);
        let now = Instant::now();
        host.send_control(&Control::PairMatched { peer: joiner_id.clone(), role: PairRole::Host });
        joiner.send_control(&Control::PairMatched { peer: host_id.clone(), role: PairRole::Joiner });
        g.matches.insert(
            id,
            Match {
                host,
                joiner: joiner.clone(),
                host_id,
                joiner_id,
                host_confirmed: false,
                joiner_confirmed: false,
                last_activity: now,
                created: now,
            },
        );
        Ok(())
    }

    /// 这对设备是否处于进行中的撮合（放行 `handshake` 帧用）。
    pub fn is_matched(&self, a: &str, b: &str) -> bool {
        self.lock().edges.contains_key(&edge(a, b))
    }

    /// 握手帧经过：刷新撮合的活动时间。
    pub fn touch(&self, a: &str, b: &str) {
        let mut g = self.lock();
        if let Some(id) = g.edges.get(&edge(a, b)).copied() {
            if let Some(m) = g.matches.get_mut(&id) {
                m.last_activity = Instant::now();
            }
        }
    }

    pub fn confirm(&self, conn: &Arc<Conn>, peer: &str) -> ConfirmOutcome {
        let mut g = self.lock();
        let found = g.matches.iter().find_map(|(id, m)| {
            if Arc::ptr_eq(&m.host, conn) && m.joiner_id == peer {
                Some((*id, true))
            } else if Arc::ptr_eq(&m.joiner, conn) && m.host_id == peer {
                Some((*id, false))
            } else {
                None
            }
        });
        let Some((id, is_host)) = found else { return ConfirmOutcome::Unknown };
        let m = g.matches.get_mut(&id).expect("just found");
        if is_host {
            m.host_confirmed = true;
        } else {
            m.joiner_confirmed = true;
        }
        m.last_activity = Instant::now();
        if !(m.host_confirmed && m.joiner_confirmed) {
            return ConfirmOutcome::Waiting;
        }
        let m = g.matches.remove(&id).expect("just found");
        g.edges.remove(&edge(&m.host_id, &m.joiner_id));
        ConfirmOutcome::Ready { host: m.host, joiner: m.joiner }
    }

    /// 连接断开：撤销它的登记；它参与的撮合作废并通知对方 `PeerGone`。
    pub fn conn_closed(&self, conn: &Arc<Conn>) {
        let mut g = self.lock();
        let host_id = conn.device_id();
        if let Some(key) = g.by_host.get(&host_id).copied() {
            if g.regs.get(&key).is_some_and(|e| Arc::ptr_eq(&e.host, conn)) {
                g.regs.remove(&key);
                g.by_host.remove(&host_id);
            }
        }
        let ids: Vec<u64> = g
            .matches
            .iter()
            .filter(|(_, m)| Arc::ptr_eq(&m.host, conn) || Arc::ptr_eq(&m.joiner, conn))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let m = g.matches.remove(&id).expect("listed above");
            g.edges.remove(&edge(&m.host_id, &m.joiner_id));
            let other = if Arc::ptr_eq(&m.host, conn) { &m.joiner } else { &m.host };
            other.send_control(&Control::PairFailed { reason: PairFailReason::PeerGone });
        }
    }

    /// 某设备被吊销：它作为 host 的登记和参与的撮合都作废。
    pub fn abort_device(&self, device_id: &str) {
        let mut g = self.lock();
        if let Some(key) = g.by_host.remove(device_id) {
            g.regs.remove(&key);
        }
        let ids: Vec<u64> = g
            .matches
            .iter()
            .filter(|(_, m)| m.host_id == device_id || m.joiner_id == device_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let m = g.matches.remove(&id).expect("listed above");
            g.edges.remove(&edge(&m.host_id, &m.joiner_id));
            m.host.send_control(&Control::PairFailed { reason: PairFailReason::PeerGone });
            m.joiner.send_control(&Control::PairFailed { reason: PairFailReason::PeerGone });
        }
    }

    /// 清理过期登记、超时撮合（后台每秒调一次）。
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut g = self.lock();
        let expired: Vec<Key> = g.regs.iter().filter(|(_, e)| e.expires <= now).map(|(k, _)| *k).collect();
        for key in expired {
            if let Some(e) = g.regs.remove(&key) {
                let host_id = e.host.device_id();
                if g.by_host.get(&host_id) == Some(&key) {
                    g.by_host.remove(&host_id);
                }
            }
        }
        let max_age = self.idle_timeout * 5;
        let stale: Vec<u64> = g
            .matches
            .iter()
            .filter(|(_, m)| {
                now.saturating_duration_since(m.last_activity) >= self.idle_timeout
                    || now.saturating_duration_since(m.created) >= max_age
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            let m = g.matches.remove(&id).expect("listed above");
            g.edges.remove(&edge(&m.host_id, &m.joiner_id));
            tracing::info!(host = %m.host_id, joiner = %m.joiner_id, "pairing timed out");
            m.host.send_control(&Control::PairFailed { reason: PairFailReason::Timeout });
            m.joiner.send_control(&Control::PairFailed { reason: PairFailReason::Timeout });
        }
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize, usize) {
        let g = self.lock();
        (g.regs.len(), g.matches.len(), g.edges.len())
    }
}

/// 收尾是被谁触发的：短码配对，还是账号内接入（[`crate::enroll`]，ADR-36）。
///
/// 两者共用同一段收尾代码，区别只有三处：失败时发 `PairFailed` 还是 `EnrollFailed { enroll_id }`；
/// 审计写 `device_paired`（只记新建的设备）还是 `device_enrolled`（接入完成一次记一条）；
/// 接入的账号在 `enroll_begin` 时就定了，收尾时要再核对一遍。
#[derive(Debug, Clone)]
pub enum Origin {
    Pair,
    Enroll { enroll_id: String, account: String },
}

impl Origin {
    fn enroll_id(&self) -> Option<&str> {
        match self {
            Origin::Pair => None,
            Origin::Enroll { enroll_id, .. } => Some(enroll_id),
        }
    }

    /// 收尾失败时发给双方的消息。
    fn failure(&self, reason: PairFailReason) -> Control {
        match self {
            Origin::Pair => Control::PairFailed { reason },
            Origin::Enroll { enroll_id, .. } => Control::EnrollFailed {
                enroll_id: Some(enroll_id.clone()),
                reason: match reason {
                    PairFailReason::NotFound => EnrollFailReason::NotFound,
                    PairFailReason::Timeout => EnrollFailReason::Timeout,
                    PairFailReason::PeerGone => EnrollFailReason::PeerGone,
                    PairFailReason::RateLimited => EnrollFailReason::RateLimited,
                    PairFailReason::Quota => EnrollFailReason::Quota,
                    PairFailReason::Forbidden => EnrollFailReason::Forbidden,
                },
            },
        }
    }
}

/// 双方都确认后：创建设备、写配对、签发 token、切换连接身份、发 `PairDone` 和 `PeerOnline`。
///
/// 两边都可能是「没有正式身份的临时连接」：
/// - joiner 临时（新浏览器 / CLI / SDK）：这里给它建设备记录并签发 token；
/// - host 临时（daemon 第一次连 hub，还没有 token，`ClientInfo.kind == daemon`）：同样建记录、签发 token。
///
/// 账号归属：joiner 和 host 里**有账号的那一边**（正式设备的账号，或临时连接升级请求带的 web 登录 cookie）；
/// 两边都有但不同 → `forbidden`；两边都没有 → `forbidden`（没有地方可以归属）。
///
/// 账号内接入（`origin = Enroll`）：host 是被接入的 daemon，joiner 是发起方（正式设备，或带登录 cookie 的临时连接）；
/// 收尾的账号必须还是 `enroll_begin` 时的那个，失败用 `enroll_failed` 通知双方。
pub async fn finalize(state: &Arc<AppState>, host: Arc<Conn>, joiner: Arc<Conn>, origin: Origin) {
    if let Err(reason) = try_finalize(state, &host, &joiner, &origin).await {
        let msg = origin.failure(reason);
        host.send_control(&msg);
        joiner.send_control(&msg);
    }
}

/// 新建设备的结果：正式 id 和明文 token（只在这里出现一次）。
struct Issued {
    id: String,
    token: String,
}

async fn issue_device(
    state: &Arc<AppState>,
    conn: &Arc<Conn>,
    account: &str,
    now: u64,
) -> Result<Issued, crate::store::StoreError> {
    let id = new_id();
    let token = new_secret();
    let c = &conn.client;
    let dev = NewDevice {
        id: id.clone(),
        account_id: account.to_string(),
        kind: c.kind,
        name: clip(&c.name, 100),
        platform: clip(&c.platform, 64),
        origin: c.origin.as_deref().map(|o| clip(o, 200)),
        token_hash: hash_token(&token),
    };
    state.store.create_device(dev, now).await?;
    Ok(Issued { id, token })
}

async fn try_finalize(
    state: &Arc<AppState>,
    host: &Arc<Conn>,
    joiner: &Arc<Conn>,
    origin: &Origin,
) -> Result<(), PairFailReason> {
    if host.out.is_closed() || joiner.out.is_closed() {
        return Err(PairFailReason::PeerGone);
    }
    let old_host_id = host.device_id();
    let account = match (host.account_id(), joiner.account_id()) {
        (Some(h), Some(j)) if h != j => {
            tracing::info!(host = %old_host_id, "pairing refused: account mismatch");
            return Err(PairFailReason::Forbidden);
        }
        (Some(a), _) | (None, Some(a)) => a,
        (None, None) => {
            tracing::info!(host = %old_host_id, "pairing refused: neither side has an account");
            return Err(PairFailReason::Forbidden);
        }
    };
    if let Origin::Enroll { account: expected, .. } = origin {
        // 被接入的一定是已有身份的 daemon（`enroll_begin` 只认库里未吊销的 daemon 设备），账号也不能变。
        if *expected != account || !host.is_permanent() {
            tracing::info!(host = %old_host_id, "enrollment refused: account changed since enroll_begin");
            return Err(PairFailReason::Forbidden);
        }
    }
    let now = now_ms();
    let internal = |what: &str, e: crate::store::StoreError| {
        tracing::error!(error = %e, "pairing {what} failed");
        PairFailReason::Timeout
    };

    // 先建设备记录；中途失败就把已建的作废，免得留下没人持有 token 的孤儿设备。
    let mut issued_host = None;
    let mut issued_joiner = None;
    let created: Result<(), PairFailReason> = async {
        if !host.is_permanent() {
            let i = issue_device(state, host, &account, now).await.map_err(|e| internal("host device creation", e))?;
            issued_host = Some(i);
        }
        if !joiner.is_permanent() {
            let i =
                issue_device(state, joiner, &account, now).await.map_err(|e| internal("joiner device creation", e))?;
            issued_joiner = Some(i);
        }
        Ok(())
    }
    .await;
    let host_id = issued_host.as_ref().map_or_else(|| old_host_id.clone(), |i| i.id.clone());
    let joiner_id = issued_joiner.as_ref().map_or_else(|| joiner.device_id(), |i| i.id.clone());
    let result = match created {
        Ok(()) => {
            state.store.insert_pairing(&host_id, &joiner_id, now).await.map_err(|e| internal("pairing insert", e))
        }
        Err(r) => Err(r),
    };
    if let Err(reason) = result {
        for issued in [&issued_host, &issued_joiner].into_iter().flatten() {
            let _ = state.store.revoke_device(&issued.id, now).await;
        }
        return Err(reason);
    }

    match origin {
        Origin::Pair => {
            for (issued, peer, conn) in [(&issued_host, &joiner_id, host), (&issued_joiner, &host_id, joiner)] {
                if let Some(i) = issued {
                    state
                        .audit(NewAudit {
                            event: "device_paired",
                            account_id: Some(account.clone()),
                            device_id: Some(i.id.clone()),
                            ip_prefix: Some(ip_prefix(conn.ip)),
                            user_agent: None,
                            detail: Some(serde_json::json!({ "peer": peer, "kind": conn.client.kind })),
                        })
                        .await;
                }
            }
        }
        // 接入：不管发起方是不是新设备都记一条（它对这个节点获得了访问权）。hub 不知道用的是哪种方法。
        Origin::Enroll { enroll_id, .. } => {
            state
                .audit(NewAudit {
                    event: "device_enrolled",
                    account_id: Some(account.clone()),
                    device_id: Some(joiner_id.clone()),
                    ip_prefix: Some(ip_prefix(joiner.ip)),
                    user_agent: None,
                    detail: Some(serde_json::json!({
                        "node": host_id,
                        "enroll_id": enroll_id,
                        "new_device": issued_joiner.is_some(),
                        "kind": joiner.client.kind,
                    })),
                })
                .await;
        }
    }

    // 运行时：先换身份、更新对端缓存，再发 PairDone，这样对方收到后立刻用新身份发的帧不会被丢。
    for (issued, conn) in [(&issued_host, host), (&issued_joiner, joiner)] {
        if let Some(i) = issued {
            if let Some(old) = state.registry.upgrade(conn, &i.id, Some(account.clone())) {
                old.close(anycon_protocol::close_code::SUPERSEDED, "superseded");
            }
        }
    }
    host.add_peer(&joiner_id);
    joiner.add_peer(&host_id);
    tracing::info!(
        host = %host_id,
        joiner = %joiner_id,
        new_host_device = issued_host.is_some(),
        new_joiner_device = issued_joiner.is_some(),
        enroll = ?origin.enroll_id(),
        "devices paired"
    );

    // PairDone：新建了设备的一边带正式 id + token，另一边什么都不带。
    host.send_control(&Control::PairDone {
        peer: joiner_id.clone(),
        device_id: issued_host.as_ref().map(|i| i.id.clone()),
        device_token: issued_host.map(|i| i.token),
    });
    joiner.send_control(&Control::PairDone {
        peer: host_id.clone(),
        device_id: issued_joiner.as_ref().map(|i| i.id.clone()),
        device_token: issued_joiner.map(|i| i.token),
    });
    // 两边此刻都在线（升级后的同一条连接）：互相发 peer_online，客户端据此直接开始 IK 握手，不用重连。
    host.send_control(&Control::PeerOnline { device: joiner_id });
    joiner.send_control(&Control::PeerOnline { device: host_id });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::outq::OutQueue;
    use crate::ws::registry::ConnInit;
    use anycon_protocol::{ClientInfo, DeviceKind};
    use std::collections::HashSet;

    fn conn(id: u64, device: &str, permanent: bool) -> (Arc<Conn>, crate::ws::outq::OutRx) {
        let (out, rx) = OutQueue::new(1 << 20);
        let c = Conn::new(ConnInit {
            id,
            ip: "127.0.0.1".parse().unwrap(),
            client: ClientInfo {
                name: "t".into(),
                kind: DeviceKind::Cli,
                platform: "test".into(),
                version: "0".into(),
                origin: None,
            },
            out,
            device_id: device.into(),
            account_id: permanent.then(|| "acc".to_string()),
            permanent,
            peers: HashSet::new(),
            preview: None,
        });
        (Arc::new(c), rx)
    }

    fn broker() -> Broker {
        Broker::new(Duration::from_secs(300), Duration::from_secs(60))
    }

    const M1: &str = "0123456789abcdef";
    const M2: &str = "fedcba9876543210";

    #[test]
    fn match_id_format_is_validated() {
        assert_eq!(normalize_match_id("0123456789abcdef").as_deref(), Some("0123456789abcdef"));
        assert_eq!(normalize_match_id("0123456789ABCDEF").as_deref(), Some("0123456789abcdef"));
        for bad in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "0123456789abcdeg",
            "0123-4567-89ab-cd",
            " 123456789abcdef",
            "é123456789abcde",
        ] {
            assert!(normalize_match_id(bad).is_none(), "{bad:?}");
        }
        let b = broker();
        let (host, _rx) = conn(1, "host", true);
        assert_eq!(b.begin(&host, "nope"), Err(BeginError::Malformed));
        assert_eq!(b.counts().0, 0);
    }

    #[test]
    fn new_registration_replaces_old_one_of_same_host() {
        let b = broker();
        let (host, _rx) = conn(1, "host", true);
        let (joiner, _rx2) = conn(2, "tmp", false);
        let before = now_ms();
        let expires = b.begin(&host, M1).unwrap();
        assert!(expires >= before + 299_000 && expires <= now_ms() + 300_000);
        b.begin(&host, M2).unwrap();
        assert_eq!(b.counts().0, 1);
        assert_eq!(b.join(&joiner, M1), Err(PairFailReason::NotFound));
        // 大小写不敏感
        assert!(b.join(&joiner, &M2.to_uppercase()).is_ok());
    }

    #[test]
    fn match_id_must_match_exactly_and_is_single_use() {
        let b = broker();
        let (host, _r1) = conn(1, "host", true);
        let (j1, _r2) = conn(2, "j1", false);
        let (j2, _r3) = conn(3, "j2", false);
        b.begin(&host, M1).unwrap();
        // 差一位也不行
        assert_eq!(b.join(&j1, "0123456789abcdee"), Err(PairFailReason::NotFound));
        assert_eq!(b.join(&j1, "1123456789abcdef"), Err(PairFailReason::NotFound));
        assert!(b.join(&j1, M1).is_ok());
        assert_eq!(b.join(&j2, M1), Err(PairFailReason::NotFound));
        assert!(b.is_matched("host", "j1"));
        assert!(b.is_matched("j1", "host"));
        assert!(!b.is_matched("host", "j2"));
    }

    #[test]
    fn host_cannot_join_own_registration_and_garbage_is_not_found() {
        let b = broker();
        let (host, _r1) = conn(1, "host", true);
        b.begin(&host, M1).unwrap();
        assert_eq!(b.join(&host, M1), Err(PairFailReason::NotFound));
        assert_eq!(b.join(&host, "not a match id"), Err(PairFailReason::NotFound));
    }

    #[test]
    fn same_match_id_from_a_new_connection_takes_over() {
        let b = broker();
        let (old, _r1) = conn(1, "old", false);
        let (new, _r2) = conn(2, "new", false);
        let (j, _r3) = conn(3, "j", false);
        b.begin(&old, M1).unwrap();
        b.begin(&new, M1).unwrap();
        assert_eq!(b.counts().0, 1);
        // 旧连接断开不能把新的登记带走
        b.conn_closed(&old);
        assert_eq!(b.counts().0, 1);
        assert!(b.join(&j, M1).is_ok());
        assert!(b.is_matched("new", "j"));
    }

    #[test]
    fn both_must_confirm() {
        let b = broker();
        let (host, _r1) = conn(1, "host", true);
        let (j, _r2) = conn(2, "tmp", false);
        b.begin(&host, M1).unwrap();
        b.join(&j, M1).unwrap();
        assert!(matches!(b.confirm(&host, "wrong"), ConfirmOutcome::Unknown));
        assert!(matches!(b.confirm(&host, "tmp"), ConfirmOutcome::Waiting));
        assert!(matches!(b.confirm(&j, "host"), ConfirmOutcome::Ready { .. }));
        assert_eq!(b.counts(), (0, 0, 0));
    }

    #[test]
    fn disconnect_aborts_match() {
        let b = broker();
        let (host, _r1) = conn(1, "host", true);
        let (j, _r2) = conn(2, "tmp", false);
        b.begin(&host, M1).unwrap();
        b.join(&j, M1).unwrap();
        b.conn_closed(&j);
        assert_eq!(b.counts(), (0, 0, 0));
        // host 断开撤销登记
        b.begin(&host, M2).unwrap();
        b.conn_closed(&host);
        assert_eq!(b.counts(), (0, 0, 0));
    }

    #[test]
    fn idle_match_times_out() {
        let b = Broker::new(Duration::from_secs(300), Duration::from_millis(0));
        let (host, _r1) = conn(1, "host", true);
        let (j, _r2) = conn(2, "tmp", false);
        b.begin(&host, M1).unwrap();
        b.join(&j, M1).unwrap();
        b.sweep();
        assert_eq!(b.counts(), (0, 0, 0));
    }

    #[test]
    fn expired_registration_is_swept_and_not_joinable() {
        let b = Broker::new(Duration::from_millis(0), Duration::from_secs(60));
        let (host, _r1) = conn(1, "host", true);
        let (j, _r2) = conn(2, "tmp", false);
        b.begin(&host, M1).unwrap();
        assert_eq!(b.join(&j, M1), Err(PairFailReason::NotFound));
        b.begin(&host, M1).unwrap();
        b.sweep();
        assert_eq!(b.counts().0, 0);
    }

    #[test]
    fn finalize_failures_use_the_message_of_the_flow_that_started_them() {
        use PairFailReason::*;
        let enroll = Origin::Enroll { enroll_id: "e".repeat(32), account: "acc".into() };
        assert_eq!(enroll.enroll_id(), Some("e".repeat(32).as_str()));
        assert_eq!(Origin::Pair.enroll_id(), None);
        for (reason, mapped) in [
            (NotFound, EnrollFailReason::NotFound),
            (Timeout, EnrollFailReason::Timeout),
            (PeerGone, EnrollFailReason::PeerGone),
            (RateLimited, EnrollFailReason::RateLimited),
            (Quota, EnrollFailReason::Quota),
            (Forbidden, EnrollFailReason::Forbidden),
        ] {
            assert!(matches!(Origin::Pair.failure(reason), Control::PairFailed { reason: r } if r == reason));
            assert!(matches!(
                enroll.failure(reason),
                Control::EnrollFailed { enroll_id: Some(id), reason: r } if id == "e".repeat(32) && r == mapped
            ));
        }
    }

    #[test]
    fn broker_state_does_not_hold_the_raw_match_id() {
        let b = broker();
        let (host, _rx) = conn(1, "host", true);
        b.begin(&host, M1).unwrap();
        let dump = format!("{b:?}");
        assert!(!dump.contains(M1) && !dump.contains(&M1.to_uppercase()), "{dump}");
        assert!(dump.contains("registrations: 1"));
        // 登记键是加盐哈希，不含 match_id 的原始字节
        let g = b.lock();
        for k in g.regs.keys() {
            assert!(!k.windows(M1.len()).any(|w| w == M1.as_bytes()));
        }
        // 盐不同 => 同一个 match_id 的键也不同
        drop(g);
        let other = broker();
        assert_ne!(b.key(M1), other.key(M1));
    }
}
