//! 账号内接入撮合（纯内存）。协议序列见 docs/protocol.md § 4.4，约束见 docs/spec/hub.md § 4.1，决定见 ADR-36。
//!
//! 和 [`crate::pairing`] 一样，hub 在这里只做**路由**：确认「发起方和这台 daemon 属于同一账号」（调用方在
//! `ws::conn` 里做完），分配一个 `enroll_id`，然后放行两端之间的 `handshake` 信封。握手本身（扫码密钥 / 接入码 /
//! 节点口令，CPace，XXpsk3）hub 看不到也不解析：`handshake` 的 body 一个字节都不读、不记、不存；能不能接入由 daemon 验证。
//!
//! 一个会话的一生：
//! `EnrollBegin { node }` → [`Enrolls::begin`] 建会话（5 分钟 TTL，每台 daemon 同时最多 3 个，每条发起连接同时最多 1 个）并通知双方
//! （发起方 `EnrollStarted`，daemon `EnrollRequest`）→ 两端互发 `handshake`（[`Enrolls::is_active`] 放行）→
//! 双方都 `PairConfirm`（[`Enrolls::confirm`]）→ `pairing::finalize` 照配对的收尾写库、签发 token、`PairDone`。
//!
//! 会话以这些方式提前结束，对端都会收到 `EnrollFailed`：`EnrollCancel`（`cancelled`）、任一端断线（`peer_gone`）、
//! 设备被吊销（`peer_gone`）、超时（`timeout`）。hub 重启即作废，不落库。
//!
//! **`enroll_id` 不是秘密**（可以出现在日志和审计里）：安全性来自 daemon 的验证，不来自 hub 的撮合。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_protocol::{Control, EnrollFailReason};

use crate::util::{new_id, now_ms, ordered};
use crate::ws::registry::Conn;

/// 每条发起连接同时最多几个进行中的接入会话。
pub const MAX_PER_INITIATOR: usize = 1;

struct Session {
    id: String,
    /// 账号：发起方登录的账号，也是目标 daemon 所属的账号。
    account: String,
    initiator: Arc<Conn>,
    daemon: Arc<Conn>,
    /// 建会话时两端的设备 id（发起方是临时连接时是临时 id）。放行 `handshake` 和核对 `PairConfirm.peer` 都用它们。
    initiator_id: String,
    daemon_id: String,
    initiator_confirmed: bool,
    daemon_confirmed: bool,
    expires: Instant,
}

impl Session {
    fn edge(&self) -> (String, String) {
        edge(&self.initiator_id, &self.daemon_id)
    }

    /// 通知两端（`except` 指定的那一端不通知：它自己就是原因，或者正在被关闭）。
    fn notify(&self, reason: EnrollFailReason, except: Option<&Arc<Conn>>) {
        let fail = Control::EnrollFailed { enroll_id: Some(self.id.clone()), reason };
        for conn in [&self.initiator, &self.daemon] {
            if except.is_none_or(|e| !Arc::ptr_eq(e, conn)) {
                conn.send_control(&fail);
            }
        }
    }
}

fn edge(a: &str, b: &str) -> (String, String) {
    let (x, y) = ordered(a, b);
    (x.to_string(), y.to_string())
}

#[derive(Default)]
struct Inner {
    /// `enroll_id` → 会话。
    sessions: HashMap<String, Session>,
    /// 进行中的会话的设备对（归一化顺序）→ `enroll_id`。用于放行 `handshake`。
    edges: HashMap<(String, String), String>,
}

impl Inner {
    fn remove(&mut self, id: &str) -> Option<Session> {
        let s = self.sessions.remove(id)?;
        self.edges.remove(&s.edge());
        Some(s)
    }

    /// 满足条件的会话 id。
    fn ids_where(&self, f: impl Fn(&Session) -> bool) -> Vec<String> {
        self.sessions.values().filter(|s| f(s)).map(|s| s.id.clone()).collect()
    }
}

/// `begin` 成功的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub id: String,
    pub expires_at_ms: u64,
}

/// `begin` 被拒绝的原因（对应 `enroll_failed.reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginError {
    /// 这条连接已经有一个进行中的会话，或这台 daemon 的会话已满。
    Busy,
    /// daemon 或发起方的连接已经关了。
    Offline,
}

impl From<BeginError> for EnrollFailReason {
    fn from(e: BeginError) -> Self {
        match e {
            BeginError::Busy => EnrollFailReason::Busy,
            BeginError::Offline => EnrollFailReason::Offline,
        }
    }
}

/// `confirm` 的结果。
pub enum ConfirmOutcome {
    /// 不是任何进行中会话的参与方，或 `peer` 对不上。
    Unknown,
    /// 已记录，等另一边。
    Waiting,
    /// 双方都确认了，可以落库（会话已经从表里移除）。
    Ready(Finished),
}

/// 双方都 `PairConfirm` 之后交给 [`crate::pairing::finalize`] 的东西。
pub struct Finished {
    pub id: String,
    pub account: String,
    pub daemon: Arc<Conn>,
    pub initiator: Arc<Conn>,
}

pub struct Enrolls {
    inner: Mutex<Inner>,
    ttl: Duration,
    per_daemon: usize,
}

/// 只输出数量，永远不输出会话内容。
impl std::fmt::Debug for Enrolls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enrolls").field("sessions", &self.lock().sessions.len()).finish_non_exhaustive()
    }
}

impl Enrolls {
    pub fn new(ttl: Duration, per_daemon: u32) -> Self {
        Self { inner: Mutex::new(Inner::default()), ttl, per_daemon: per_daemon.max(1) as usize }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("enroll broker")
    }

    /// 建会话并通知双方：发起方 `EnrollStarted`，daemon `EnrollRequest`（先后顺序固定，都在持锁时入队）。
    ///
    /// 调用方已经校验过：发起方有账号、不是 daemon、`daemon` 是这个账号下未吊销的在线 daemon。
    pub fn begin(&self, account: &str, initiator: &Arc<Conn>, daemon: &Arc<Conn>) -> Result<Started, BeginError> {
        if initiator.out.is_closed() || daemon.out.is_closed() {
            return Err(BeginError::Offline);
        }
        let initiator_id = initiator.device_id();
        let daemon_id = daemon.device_id();
        let mut g = self.lock();
        let by_initiator = g.sessions.values().filter(|s| Arc::ptr_eq(&s.initiator, initiator)).count();
        let by_daemon = g.sessions.values().filter(|s| s.daemon_id == daemon_id).count();
        if by_initiator >= MAX_PER_INITIATOR || by_daemon >= self.per_daemon {
            return Err(BeginError::Busy);
        }
        let id = new_id();
        let expires_at_ms = now_ms() + self.ttl.as_millis() as u64;
        initiator.send_control(&Control::EnrollStarted {
            enroll_id: id.clone(),
            node: daemon_id.clone(),
            expires_at_ms,
        });
        daemon.send_control(&Control::EnrollRequest {
            enroll_id: id.clone(),
            peer: initiator_id.clone(),
            expires_at_ms,
        });
        let s = Session {
            id: id.clone(),
            account: account.to_string(),
            initiator: initiator.clone(),
            daemon: daemon.clone(),
            initiator_id,
            daemon_id,
            initiator_confirmed: false,
            daemon_confirmed: false,
            expires: Instant::now() + self.ttl,
        };
        g.edges.insert(s.edge(), id.clone());
        g.sessions.insert(id.clone(), s);
        Ok(Started { id, expires_at_ms })
    }

    /// 这对设备是否处于进行中的接入会话（放行 `handshake` 信封用，两个方向都放行）。
    pub fn is_active(&self, a: &str, b: &str) -> bool {
        self.lock().edges.contains_key(&edge(a, b))
    }

    /// `PairConfirm { peer }`：记下这一边的确认；双方都确认了就结束会话，交给调用方落库。
    pub fn confirm(&self, conn: &Arc<Conn>, peer: &str) -> ConfirmOutcome {
        let mut g = self.lock();
        let found = g.sessions.values().find_map(|s| {
            if Arc::ptr_eq(&s.initiator, conn) && s.daemon_id == peer {
                Some((s.id.clone(), false))
            } else if Arc::ptr_eq(&s.daemon, conn) && s.initiator_id == peer {
                Some((s.id.clone(), true))
            } else {
                None
            }
        });
        let Some((id, is_daemon)) = found else { return ConfirmOutcome::Unknown };
        let s = g.sessions.get_mut(&id).expect("just found");
        if is_daemon {
            s.daemon_confirmed = true;
        } else {
            s.initiator_confirmed = true;
        }
        if !(s.initiator_confirmed && s.daemon_confirmed) {
            return ConfirmOutcome::Waiting;
        }
        let s = g.remove(&id).expect("just found");
        ConfirmOutcome::Ready(Finished { id: s.id, account: s.account, daemon: s.daemon, initiator: s.initiator })
    }

    /// `EnrollCancel { enroll_id }`：参与方取消，另一端收到 `EnrollFailed { cancelled }`。
    /// 返回会话是否存在且 `conn` 是它的参与方（不是参与方的人猜到 id 也取消不了，和不存在一样处理）。
    pub fn cancel(&self, conn: &Arc<Conn>, enroll_id: &str) -> bool {
        let mut g = self.lock();
        let is_party =
            g.sessions.get(enroll_id).is_some_and(|s| Arc::ptr_eq(&s.initiator, conn) || Arc::ptr_eq(&s.daemon, conn));
        if !is_party {
            return false;
        }
        let s = g.remove(enroll_id).expect("checked above");
        tracing::debug!(enroll = %s.id, "enrollment cancelled");
        s.notify(EnrollFailReason::Cancelled, Some(conn));
        true
    }

    /// 连接断开：它参与的会话作废，另一端收到 `EnrollFailed { peer_gone }`。
    pub fn conn_closed(&self, conn: &Arc<Conn>) {
        let mut g = self.lock();
        for id in g.ids_where(|s| Arc::ptr_eq(&s.initiator, conn) || Arc::ptr_eq(&s.daemon, conn)) {
            if let Some(s) = g.remove(&id) {
                tracing::debug!(enroll = %s.id, "enrollment aborted: connection closed");
                s.notify(EnrollFailReason::PeerGone, Some(conn));
            }
        }
    }

    /// 某设备被吊销：它参与的会话作废，另一端收到 `EnrollFailed { peer_gone }`（被吊销的那一端马上会被 4004 断开，不通知）。
    pub fn abort_device(&self, device_id: &str) {
        let mut g = self.lock();
        for id in g.ids_where(|s| s.initiator_id == device_id || s.daemon_id == device_id) {
            if let Some(s) = g.remove(&id) {
                tracing::info!(enroll = %s.id, "enrollment aborted: device revoked");
                let revoked = if s.initiator_id == device_id { &s.initiator } else { &s.daemon };
                s.notify(EnrollFailReason::PeerGone, Some(revoked));
            }
        }
    }

    /// 清理超时的会话（到点没走到 `pair_done` → 双方 `timeout`），以及连接已经没了却没人通知到的会话（后台每秒调一次）。
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut g = self.lock();
        let ids = g.ids_where(|s| s.expires <= now || s.initiator.out.is_closed() || s.daemon.out.is_closed());
        for id in ids {
            let Some(s) = g.remove(&id) else { continue };
            if s.expires <= now {
                tracing::info!(enroll = %s.id, "enrollment timed out");
                s.notify(EnrollFailReason::Timeout, None);
            } else {
                tracing::debug!(enroll = %s.id, "enrollment dropped: connection gone");
                let gone = if s.initiator.out.is_closed() { &s.initiator } else { &s.daemon };
                s.notify(EnrollFailReason::PeerGone, Some(gone));
            }
        }
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize) {
        let g = self.lock();
        (g.sessions.len(), g.edges.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::outq::{OutQueue, OutRx, Outbound};
    use crate::ws::registry::ConnInit;
    use anycon_protocol::{ClientInfo, DeviceKind, Envelope};
    use std::collections::HashSet;

    fn conn(id: u64, device: &str, kind: DeviceKind, permanent: bool) -> (Arc<Conn>, OutRx) {
        let (out, rx) = OutQueue::new(1 << 20);
        let c = Conn::new(ConnInit {
            id,
            ip: "127.0.0.1".parse().unwrap(),
            client: ClientInfo { name: "t".into(), kind, platform: "test".into(), version: "0".into(), origin: None },
            out,
            device_id: device.into(),
            account_id: Some("acc".into()),
            permanent,
            peers: HashSet::new(),
            preview: None,
        });
        (Arc::new(c), rx)
    }

    /// 取出队列里已经入队的所有控制消息。
    async fn drain(rx: &mut OutRx) -> Vec<Control> {
        let mut out = Vec::new();
        while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(30), rx.recv()).await {
            if let Outbound::Frame(b) = &item.msg {
                out.push(Envelope::decode(b).unwrap().control_body().unwrap());
            }
        }
        out
    }

    fn broker() -> Enrolls {
        Enrolls::new(Duration::from_secs(300), 3)
    }

    #[tokio::test]
    async fn begin_notifies_both_sides_with_the_same_id() {
        let b = broker();
        let (n, mut nrx) = conn(1, "tmp-n", DeviceKind::Web, false);
        let (d, mut drx) = conn(2, "node", DeviceKind::Daemon, true);
        let before = now_ms();
        let started = b.begin("acc", &n, &d).unwrap();
        assert_eq!(started.id.len(), 32);
        assert!(started.id.bytes().all(|c| c.is_ascii_hexdigit()));
        assert!((before + 299_000..=now_ms() + 300_000).contains(&started.expires_at_ms), "{}", started.expires_at_ms);
        match drain(&mut nrx).await.as_slice() {
            [Control::EnrollStarted { enroll_id, node, expires_at_ms }] => {
                assert_eq!((enroll_id, node.as_str(), *expires_at_ms), (&started.id, "node", started.expires_at_ms));
            }
            other => panic!("{other:?}"),
        }
        match drain(&mut drx).await.as_slice() {
            [Control::EnrollRequest { enroll_id, peer, expires_at_ms }] => {
                assert_eq!((enroll_id, peer.as_str(), *expires_at_ms), (&started.id, "tmp-n", started.expires_at_ms));
            }
            other => panic!("{other:?}"),
        }
        assert!(b.is_active("tmp-n", "node") && b.is_active("node", "tmp-n"));
        assert!(!b.is_active("tmp-n", "other") && !b.is_active("other", "node"));
        // 两个会话的 id 不同
        let (n2, _r) = conn(3, "tmp-n2", DeviceKind::Cli, false);
        assert_ne!(b.begin("acc", &n2, &d).unwrap().id, started.id);
    }

    #[test]
    fn one_session_per_initiator_connection_and_three_per_daemon() {
        let b = broker();
        let (d, _drx) = conn(1, "node", DeviceKind::Daemon, true);
        let (other_d, _orx) = conn(2, "node2", DeviceKind::Daemon, true);
        let (n1, _r1) = conn(10, "n1", DeviceKind::Web, false);
        b.begin("acc", &n1, &d).unwrap();
        // 同一条连接再来一个：busy，哪怕换一台 daemon
        assert_eq!(b.begin("acc", &n1, &d), Err(BeginError::Busy));
        assert_eq!(b.begin("acc", &n1, &other_d), Err(BeginError::Busy));
        let (n2, _r2) = conn(11, "n2", DeviceKind::Web, false);
        let (n3, _r3) = conn(12, "n3", DeviceKind::Web, false);
        let (n4, _r4) = conn(13, "n4", DeviceKind::Web, false);
        b.begin("acc", &n2, &d).unwrap();
        b.begin("acc", &n3, &d).unwrap();
        assert_eq!(b.begin("acc", &n4, &d), Err(BeginError::Busy), "4th session for one daemon");
        // 别的 daemon 不受影响
        b.begin("acc", &n4, &other_d).unwrap();
        assert_eq!(b.counts(), (4, 4));
    }

    #[test]
    fn a_closed_connection_cannot_begin_or_be_the_target() {
        let b = broker();
        let (n, nrx) = conn(1, "n", DeviceKind::Web, false);
        let (d, drx) = conn(2, "node", DeviceKind::Daemon, true);
        drop(drx);
        assert_eq!(b.begin("acc", &n, &d), Err(BeginError::Offline));
        let (d2, _drx2) = conn(3, "node2", DeviceKind::Daemon, true);
        drop(nrx);
        assert_eq!(b.begin("acc", &n, &d2), Err(BeginError::Offline));
        assert_eq!(b.counts(), (0, 0));
    }

    #[test]
    fn both_sides_must_confirm_and_peer_must_match() {
        let b = broker();
        let (n, _nrx) = conn(1, "n", DeviceKind::Web, false);
        let (d, _drx) = conn(2, "node", DeviceKind::Daemon, true);
        let (stranger, _srx) = conn(3, "x", DeviceKind::Web, false);
        let id = b.begin("acc", &n, &d).unwrap().id;
        assert!(matches!(b.confirm(&n, "wrong"), ConfirmOutcome::Unknown));
        assert!(matches!(b.confirm(&d, "wrong"), ConfirmOutcome::Unknown));
        assert!(matches!(b.confirm(&stranger, "node"), ConfirmOutcome::Unknown));
        assert!(matches!(b.confirm(&n, "node"), ConfirmOutcome::Waiting));
        assert!(
            matches!(b.confirm(&n, "node"), ConfirmOutcome::Waiting),
            "confirming twice is not confirming for the other side"
        );
        assert!(b.is_active("n", "node"), "still waiting for the daemon");
        match b.confirm(&d, "n") {
            ConfirmOutcome::Ready(f) => {
                assert_eq!((f.id.as_str(), f.account.as_str()), (id.as_str(), "acc"));
                assert!(Arc::ptr_eq(&f.initiator, &n) && Arc::ptr_eq(&f.daemon, &d));
            }
            _ => panic!("expected ready"),
        }
        assert_eq!(b.counts(), (0, 0));
        assert!(!b.is_active("n", "node"));
        assert!(matches!(b.confirm(&d, "n"), ConfirmOutcome::Unknown), "a finished session cannot finish twice");
    }

    #[tokio::test]
    async fn cancel_tells_the_other_side_and_only_parties_can_cancel() {
        let b = broker();
        let (n, mut nrx) = conn(1, "n", DeviceKind::Web, false);
        let (d, mut drx) = conn(2, "node", DeviceKind::Daemon, true);
        let (stranger, mut srx) = conn(3, "x", DeviceKind::Web, false);
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut drx).await;
        assert!(!b.cancel(&stranger, &id), "a non-party guessing the id cannot cancel");
        assert!(!b.cancel(&n, "00000000000000000000000000000000"));
        assert_eq!(b.counts(), (1, 1));
        assert!(b.cancel(&n, &id));
        assert_eq!(b.counts(), (0, 0));
        assert!(matches!(
            drain(&mut drx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::Cancelled }] if *e == id
        ));
        assert!(drain(&mut nrx).await.is_empty(), "the canceller is not told about its own cancel");
        assert!(drain(&mut srx).await.is_empty());
        assert!(!b.cancel(&d, &id), "already gone");
        // 另一个方向：daemon 取消
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut drx).await;
        assert!(b.cancel(&d, &id));
        assert!(matches!(
            drain(&mut nrx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::Cancelled }] if *e == id
        ));
    }

    #[tokio::test]
    async fn closing_a_connection_ends_its_sessions_with_peer_gone() {
        let b = broker();
        let (n, mut nrx) = conn(1, "n", DeviceKind::Web, false);
        let (n2, mut n2rx) = conn(2, "n2", DeviceKind::Web, false);
        let (d, mut drx) = conn(3, "node", DeviceKind::Daemon, true);
        let id1 = b.begin("acc", &n, &d).unwrap().id;
        let id2 = b.begin("acc", &n2, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut n2rx).await;
        drain(&mut drx).await;
        // 发起方断开：只有它的会话没了，daemon 收到一条 peer_gone
        b.conn_closed(&n);
        assert_eq!(b.counts(), (1, 1));
        assert!(matches!(
            drain(&mut drx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::PeerGone }] if *e == id1
        ));
        // daemon 断开：剩下的会话没了，发起方收到 peer_gone
        b.conn_closed(&d);
        assert_eq!(b.counts(), (0, 0));
        assert!(matches!(
            drain(&mut n2rx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::PeerGone }] if *e == id2
        ));
        b.conn_closed(&d); // 幂等
    }

    #[tokio::test]
    async fn revoking_a_device_ends_its_sessions_and_tells_the_other_side_only() {
        let b = broker();
        let (n, mut nrx) = conn(1, "n", DeviceKind::Web, true);
        let (d, mut drx) = conn(2, "node", DeviceKind::Daemon, true);
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut drx).await;
        b.abort_device("unrelated");
        assert_eq!(b.counts(), (1, 1));
        b.abort_device("n");
        assert_eq!(b.counts(), (0, 0));
        assert!(matches!(
            drain(&mut drx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::PeerGone }] if *e == id
        ));
        assert!(drain(&mut nrx).await.is_empty());
        // daemon 被吊销：发起方收到 peer_gone
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut drx).await;
        b.abort_device("node");
        assert!(matches!(
            drain(&mut nrx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::PeerGone }] if *e == id
        ));
    }

    #[tokio::test]
    async fn expired_sessions_are_swept_with_timeout_for_both_sides() {
        let b = Enrolls::new(Duration::from_millis(0), 3);
        let (n, mut nrx) = conn(1, "n", DeviceKind::Web, false);
        let (d, mut drx) = conn(2, "node", DeviceKind::Daemon, true);
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut nrx).await;
        drain(&mut drx).await;
        b.sweep();
        assert_eq!(b.counts(), (0, 0));
        for rx in [&mut nrx, &mut drx] {
            assert!(matches!(
                drain(rx).await.as_slice(),
                [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::Timeout }] if *e == id
            ));
        }
    }

    #[tokio::test]
    async fn sessions_whose_connection_vanished_are_swept_as_peer_gone() {
        let b = broker();
        let (n, nrx) = conn(1, "n", DeviceKind::Web, false);
        let (d, mut drx) = conn(2, "node", DeviceKind::Daemon, true);
        let id = b.begin("acc", &n, &d).unwrap().id;
        drain(&mut drx).await;
        drop(nrx); // 写端没了，但 conn_closed 还没来得及调
        b.sweep();
        assert_eq!(b.counts(), (0, 0));
        assert!(matches!(
            drain(&mut drx).await.as_slice(),
            [Control::EnrollFailed { enroll_id: Some(e), reason: EnrollFailReason::PeerGone }] if *e == id
        ));
    }

    #[test]
    fn debug_output_shows_only_counts() {
        let b = broker();
        let (n, _nrx) = conn(1, "secret-initiator-id", DeviceKind::Web, false);
        let (d, _drx) = conn(2, "secret-node-id", DeviceKind::Daemon, true);
        let id = b.begin("acc", &n, &d).unwrap().id;
        let dump = format!("{b:?}");
        assert!(dump.contains("sessions: 1"), "{dump}");
        assert!(!dump.contains("secret-") && !dump.contains(&id), "{dump}");
    }
}
