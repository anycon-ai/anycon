//! 连接注册表（路由表）：device id → 当前连接。
//!
//! - 一个设备同时只有一条连接：新连接 [`Registry::register`] 顶掉旧的，旧的由调用方以 4001 关闭。
//! - 临时连接（`device_token = None`）用 hub 分配的临时 id 登记，配对完成后 [`Registry::upgrade`] 换成正式 id。
//! - 每条连接缓存自己的配对对端集合（认证时从库里读，之后随配对 / 吊销实时更新），
//!   转发时只查内存，不查库。
//! - 预览连接（ADR-39）用临时 id 登记在同一张表里，另有「节点 → 它的预览连接」和「账号 → 预览连接数」两个索引
//!   （和路由表同一把锁，所以「节点上线 / 预览连接接入」互相看到一致的状态）。预览连接的对端集合只有它的节点；
//!   节点（重新）上线时把在线的预览连接加回它的对端集合，不进 `peers_online`，而是逐条发 `preview_peer`。
//! - 「粘住」状态（[`super::sticky`]）挂在路由表上：连接结束、设备下线 / 被顶掉 / 被吊销时在这里一并清理。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anycon_protocol::{ClientInfo, Control, DeviceKind, Envelope, Kind};
use axum::body::Bytes;
use tokio_util::sync::CancellationToken;

use super::outq::{OutQueue, PushError};
use super::sticky::{Cause, Sticky, TooManyReturned};

#[derive(Debug, Clone)]
struct Identity {
    device_id: String,
    account_id: Option<String>,
    permanent: bool,
}

/// 预览连接的绑定信息（docs/spec/hub.md § 13.4）：认证时从预览令牌里读出来，之后不变。
#[derive(Clone)]
pub struct PreviewMeta {
    pub account_id: String,
    /// label 绑定的节点（daemon）：这条连接唯一的对端。
    pub node_id: String,
    pub label: String,
    /// 签发令牌的设备（吊销它时关掉这条连接）。
    pub device_id: String,
    /// 令牌的哈希（周期性复核令牌还在不在库里）。
    pub token_hash: String,
    pub expires_at_ms: u64,
}

/// `Debug` 不输出令牌哈希和 label。
impl std::fmt::Debug for PreviewMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewMeta")
            .field("account_id", &self.account_id)
            .field("node_id", &self.node_id)
            .field("expires_at_ms", &self.expires_at_ms)
            .finish_non_exhaustive()
    }
}

/// 一条已认证的 WebSocket 连接。
pub struct Conn {
    /// hub 内部递增的连接号（区分同一设备的新旧连接）。
    pub id: u64,
    pub ip: IpAddr,
    pub client: ClientInfo,
    pub out: OutQueue,
    /// 取消后写任务立刻收尾（慢消费者等场景，不等队列排空）。
    pub kill: CancellationToken,
    kill_code: AtomicU16,
    /// `Some` = 预览连接（不是设备，没有设备 token，只能和 `node_id` 互发 `handshake` / `data`）。
    pub preview: Option<PreviewMeta>,
    identity: RwLock<Identity>,
    peers: RwLock<HashSet<String>>,
    seq: AtomicU64,
}

/// 构造 [`Conn`] 的参数。
pub struct ConnInit {
    pub id: u64,
    pub ip: IpAddr,
    pub client: ClientInfo,
    pub out: OutQueue,
    pub device_id: String,
    pub account_id: Option<String>,
    pub permanent: bool,
    pub peers: HashSet<String>,
    pub preview: Option<PreviewMeta>,
}

impl Conn {
    pub fn new(init: ConnInit) -> Self {
        Self {
            id: init.id,
            ip: init.ip,
            client: init.client,
            out: init.out,
            kill: CancellationToken::new(),
            kill_code: AtomicU16::new(1000),
            preview: init.preview,
            identity: RwLock::new(Identity {
                device_id: init.device_id,
                account_id: init.account_id,
                permanent: init.permanent,
            }),
            peers: RwLock::new(init.peers),
            seq: AtomicU64::new(0),
        }
    }

    pub fn device_id(&self) -> String {
        self.identity.read().expect("conn identity").device_id.clone()
    }

    pub fn account_id(&self) -> Option<String> {
        self.identity.read().expect("conn identity").account_id.clone()
    }

    /// 是否已有正式设备身份（临时连接为 `false`）。
    pub fn is_permanent(&self) -> bool {
        self.identity.read().expect("conn identity").permanent
    }

    pub fn has_peer(&self, device: &str) -> bool {
        self.peers.read().expect("conn peers").contains(device)
    }

    pub fn peers_snapshot(&self) -> Vec<String> {
        self.peers.read().expect("conn peers").iter().cloned().collect()
    }

    pub fn add_peer(&self, device: &str) {
        self.peers.write().expect("conn peers").insert(device.to_string());
    }

    pub fn remove_peer(&self, device: &str) {
        self.peers.write().expect("conn peers").remove(device);
    }

    pub fn clear_peers(&self) {
        self.peers.write().expect("conn peers").clear();
    }

    /// hub → 设备的控制消息（`from` 为空，`to` 为该设备）。
    ///
    /// 控制消息计入连接的发送缓冲，另有自己的上限（见 [`super::outq`]）：连接一直不读、上限被撑满时，
    /// 新的控制消息被丢弃（只记 trace 日志），连接本身会在「满」持续 30 秒后以 4003 断开。
    /// 丢了也不能让发送方蒙在鼓里的消息（`backpressure`）用 [`Conn::try_send_control`] 自己处理这种情况。
    pub fn send_control(&self, ctl: &Control) {
        let _ = self.try_send_control(ctl);
    }

    /// 同 [`Conn::send_control`]，但把「控制消息队列满了，这条被丢弃」报告给调用方（`Err(PushError::Full)`）。
    /// 连接已经关了（`Closed`）不算失败：反正没人在等。
    pub fn try_send_control(&self, ctl: &Control) -> Result<(), PushError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        let body = match rmp_serde::to_vec_named(ctl) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "failed to encode control message");
                return Ok(());
            }
        };
        let env = Envelope::new("", self.device_id(), Kind::Control, seq, body);
        match env.encode() {
            Ok(bytes) => match self.out.push_control(Bytes::from(bytes)) {
                Err(PushError::Full) => {
                    tracing::trace!(device = %self.device_id(), "control queue full, dropping a control message");
                    Err(PushError::Full)
                }
                _ => Ok(()),
            },
            Err(e) => {
                tracing::error!(error = %e, "failed to encode envelope");
                Ok(())
            }
        }
    }

    /// 转发别人的信封（原样字节）。
    pub fn push_frame(&self, frame: Bytes) -> Result<(), PushError> {
        self.out.try_push_frame(frame)
    }

    /// 正常关闭：排在已入队的帧之后发 Close。
    pub fn close(&self, code: u16, reason: &'static str) {
        self.out.close(code, reason);
    }

    /// 立刻关闭：不等队列排空（写端会尽力发一个带 `code` 的 Close）。
    pub fn kill_with(&self, code: u16) {
        self.kill_code.store(code, Ordering::SeqCst);
        self.kill.cancel();
    }

    pub fn kill_code(&self) -> u16 {
        self.kill_code.load(Ordering::SeqCst)
    }

    fn set_identity(&self, device_id: String, account_id: Option<String>) {
        *self.identity.write().expect("conn identity") = Identity { device_id, account_id, permanent: true };
    }
}

/// `register` 的结果。
pub struct Registered {
    /// 被顶掉的旧连接（调用方负责以 4001 关闭它）。
    pub superseded: Option<Arc<Conn>>,
    /// 已配对且在线的对端。
    pub peers_online: Vec<String>,
}

/// 路由表和它的两个预览索引，放在同一把锁里。
#[derive(Default)]
struct Table {
    /// 设备 id（预览连接是临时 id）→ 当前连接。
    conns: HashMap<String, Arc<Conn>>,
    /// 节点 id → 它的预览连接的 id。
    previews_by_node: HashMap<String, HashSet<String>>,
    /// 账号 id → 预览连接数（`ANYCON_PREVIEW_MAX_CONNECTIONS`）。
    previews_by_account: HashMap<String, usize>,
}

impl Table {
    fn add_preview_index(&mut self, id: &str, meta: &PreviewMeta) {
        self.previews_by_node.entry(meta.node_id.clone()).or_default().insert(id.to_string());
        *self.previews_by_account.entry(meta.account_id.clone()).or_default() += 1;
    }

    fn remove_preview_index(&mut self, id: &str, meta: &PreviewMeta) {
        if let Some(ids) = self.previews_by_node.get_mut(&meta.node_id) {
            ids.remove(id);
            if ids.is_empty() {
                self.previews_by_node.remove(&meta.node_id);
            }
        }
        if let Some(n) = self.previews_by_account.get_mut(&meta.account_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.previews_by_account.remove(&meta.account_id);
            }
        }
    }
}

/// 账号的预览连接已经到上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewQuotaFull;

#[derive(Default)]
pub struct Registry {
    table: RwLock<Table>,
    next_conn: AtomicU64,
    sticky: Sticky,
}

/// 只输出数量。
impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry").field("connections", &self.len()).field("sticky", &self.sticky).finish()
    }
}

impl Registry {
    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 登记连接。`auth_ok` 在持锁期间收到「当前在线的对端」并生成 `AuthOk`，
    /// 保证它是该连接收到的第一条控制消息，且 `peers_online` 与登记原子一致。
    ///
    /// 节点（重新）上线时，它名下在线的预览连接一并加回它的对端集合：逐条给节点发 `preview_peer`（不进 `peers_online`，
    /// 也不发 `peer_online`），给预览连接发 `peer_online { node }`（docs/spec/hub.md § 13.4）。
    pub fn register(&self, conn: &Arc<Conn>, auth_ok: impl FnOnce(Vec<String>) -> Control) -> Registered {
        let mut t = self.table.write().expect("registry");
        let device_id = conn.device_id();
        let superseded = t.conns.insert(device_id.clone(), conn.clone());
        let peers_online: Vec<String> = conn.peers_snapshot().into_iter().filter(|p| t.conns.contains_key(p)).collect();
        conn.send_control(&auth_ok(peers_online.clone()));
        let superseded = superseded.filter(|old| old.id != conn.id);
        if let Some(old) = &superseded {
            // 新连接是一条全新的传输（对端会收到 peer_online 重新握手）：旧连接留下的顺序约束作废。
            self.sticky.forget_toward(&device_id);
            self.sticky.forget_sender(old.id);
        }
        if conn.preview.is_none() {
            for id in t.previews_by_node.get(&device_id).into_iter().flatten() {
                let Some(preview) = t.conns.get(id) else { continue };
                let Some(meta) = &preview.preview else { continue };
                conn.add_peer(id);
                conn.send_control(&Control::PreviewPeer { peer: id.clone(), label: meta.label.clone() });
                preview.send_control(&Control::PeerOnline { device: device_id.clone() });
            }
        }
        Registered { superseded, peers_online }
    }

    /// 登记一条预览连接（`conn.preview` 必须是 `Some`）。账号的预览连接已经到 `max` 个返回 `Err`，什么都不登记。
    ///
    /// 节点在线时：`auth_ok` 的 `peers_online` 是 `[节点]`，节点的对端集合加上它，并给节点发 `preview_peer { peer, label }`
    /// （**不发** `peer_online`）。检查数量、登记、通知节点都在同一把锁里，和节点的上下线不会交错。
    pub fn register_preview(
        &self,
        conn: &Arc<Conn>,
        max: usize,
        auth_ok: impl FnOnce(Vec<String>) -> Control,
    ) -> Result<(), PreviewQuotaFull> {
        let meta = conn.preview.as_ref().expect("register_preview needs a preview connection");
        let mut t = self.table.write().expect("registry");
        if t.previews_by_account.get(&meta.account_id).copied().unwrap_or(0) >= max {
            return Err(PreviewQuotaFull);
        }
        let id = conn.device_id();
        t.conns.insert(id.clone(), conn.clone());
        t.add_preview_index(&id, meta);
        let node = t.conns.get(&meta.node_id).cloned();
        conn.send_control(&auth_ok(node.iter().map(|_| meta.node_id.clone()).collect()));
        if let Some(node) = node {
            node.add_peer(&id);
            node.send_control(&Control::PreviewPeer { peer: id, label: meta.label.clone() });
        }
        Ok(())
    }

    pub fn get(&self, device_id: &str) -> Option<Arc<Conn>> {
        self.table.read().expect("registry").conns.get(device_id).cloned()
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.table.read().expect("registry").conns.contains_key(device_id)
    }

    /// 仅当登记的还是这条连接时才移除。返回是否真的移除了。
    ///
    /// 无论是否移除，这条连接作为发送方留下的粘住记录都清掉；真的移除了（设备下线）的话，发给这个设备的记录也一并清掉。
    /// 移除的是预览连接时，同一把锁里把它从节点的对端集合去掉，并给在线的节点发 `peer_offline { 预览连接 }`。
    pub fn unregister(&self, device_id: &str, conn_id: u64) -> bool {
        let mut t = self.table.write().expect("registry");
        self.sticky.forget_sender(conn_id);
        match t.conns.get(device_id) {
            Some(c) if c.id == conn_id => {
                let conn = t.conns.remove(device_id).expect("just looked up");
                self.sticky.forget_toward(device_id);
                if let Some(meta) = &conn.preview {
                    t.remove_preview_index(device_id, meta);
                    if let Some(node) = t.conns.get(&meta.node_id) {
                        node.remove_peer(device_id);
                        node.send_control(&Control::PeerOffline { device: device_id.to_string() });
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// 关掉与 `device_id` 相关的预览连接：用它签发的令牌的，或者它是节点的。设备被吊销时用（令牌已在库里删掉）。
    pub fn close_previews_of(&self, device_id: &str, code: u16, reason: &'static str) {
        let victims: Vec<Arc<Conn>> = self
            .table
            .read()
            .expect("registry")
            .conns
            .values()
            .filter(|c| c.preview.as_ref().is_some_and(|m| m.device_id == device_id || m.node_id == device_id))
            .cloned()
            .collect();
        for c in victims {
            c.close(code, reason);
        }
    }

    /// 这个账号此刻有几条预览连接。
    pub fn preview_count(&self, account_id: &str) -> usize {
        self.table.read().expect("registry").previews_by_account.get(account_id).copied().unwrap_or(0)
    }

    /// 临时连接换成正式身份。返回被顶掉的同 id 连接（正常不会有）。
    pub fn upgrade(&self, conn: &Arc<Conn>, new_id: &str, account_id: Option<String>) -> Option<Arc<Conn>> {
        let mut t = self.table.write().expect("registry");
        let old_id = conn.device_id();
        if t.conns.get(&old_id).is_some_and(|c| c.id == conn.id) {
            t.conns.remove(&old_id);
        }
        conn.set_identity(new_id.to_string(), account_id);
        let displaced = t.conns.insert(new_id.to_string(), conn.clone()).filter(|old| old.id != conn.id);
        if let Some(old) = &displaced {
            self.sticky.forget_toward(new_id);
            self.sticky.forget_sender(old.id);
        }
        displaced
    }

    /// 记录一封被退回的 `data` / `notify`（见 [`super::sticky`]）：把它的 `seq` 加入这对 `(发送连接, 目标)` 的有序集合。
    /// `cause` 是它自己没转发出去的原因；`None` 表示它是被粘住（不是集合的最小值）而退回的。
    /// 集合已经满了（[`super::sticky::MAX_RETURNED`]）返回 `Err`，调用方断开发送方。
    ///
    /// 只在目标还登记在路由表里时才记：持着路由表读锁检查，和 [`Registry::unregister`] 的清理互斥，
    /// 所以不会在目标刚下线之后又留下一条再也没人清的记录。
    pub fn stick(
        &self,
        sender: u64,
        target: &Conn,
        to: &str,
        seq: u64,
        cause: Option<Cause>,
    ) -> Result<(), TooManyReturned> {
        let t = self.table.read().expect("registry");
        if t.conns.get(to).is_some_and(|c| c.id == target.id) {
            self.sticky.returned(sender, to, target.id, seq, cause)?;
        }
        Ok(())
    }

    /// 粘住状态（读 / 放行 / 清除用；登记用 [`Registry::stick`]）。
    pub fn sticky(&self) -> &Sticky {
        &self.sticky
    }

    pub fn len(&self) -> usize {
        self.table.read().expect("registry").conns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 账号此刻有没有任何 **web** 设备在线。邮件通知用它判断「用户正在看」（docs/spec/hub.md § 6.2）：
    /// 只有浏览器代表有人在看；daemon、一直连着的 CLI / SDK 设备（脚本、机器人）和还没有身份的临时连接都不算——
    /// 否则一台常驻的 SDK 节点会让邮件通知永远不发。
    pub fn account_has_web_online(&self, account_id: &str) -> bool {
        self.table.read().expect("registry").conns.values().any(|c| {
            c.is_permanent() && c.client.kind == DeviceKind::Web && c.account_id().as_deref() == Some(account_id)
        })
    }

    pub fn permanent_count(&self) -> usize {
        self.table.read().expect("registry").conns.values().filter(|c| c.is_permanent()).count()
    }

    /// 向所有连接发 Close（优雅关停用 4002）。
    pub fn close_all(&self, code: u16, reason: &'static str) {
        let conns: Vec<Arc<Conn>> = self.table.read().expect("registry").conns.values().cloned().collect();
        for c in conns {
            c.close(code, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::outq::{OutQueue, OutRx, Outbound};
    use anycon_protocol::Envelope;

    fn client(kind: DeviceKind) -> ClientInfo {
        ClientInfo { name: "t".into(), kind, platform: "test".into(), version: "0".into(), origin: None }
    }

    fn device(registry: &Registry, id: &str, kind: DeviceKind) -> (Arc<Conn>, OutRx) {
        let (out, rx) = OutQueue::new(1 << 20);
        let conn = Arc::new(Conn::new(ConnInit {
            id: registry.next_conn_id(),
            ip: "127.0.0.1".parse().unwrap(),
            client: client(kind),
            out,
            device_id: id.into(),
            account_id: Some("acc".into()),
            permanent: true,
            peers: HashSet::new(),
            preview: None,
        }));
        (conn, rx)
    }

    fn preview(registry: &Registry, id: &str, account: &str, node: &str, label: &str) -> (Arc<Conn>, OutRx) {
        let (out, rx) = OutQueue::new(1 << 20);
        let conn = Arc::new(Conn::new(ConnInit {
            id: registry.next_conn_id(),
            ip: "127.0.0.1".parse().unwrap(),
            client: client(DeviceKind::Web),
            out,
            device_id: id.into(),
            account_id: Some(account.into()),
            permanent: false,
            peers: HashSet::from([node.to_string()]),
            preview: Some(PreviewMeta {
                account_id: account.into(),
                node_id: node.into(),
                label: label.into(),
                device_id: "issuer".into(),
                token_hash: format!("hash-{id}"),
                expires_at_ms: u64::MAX,
            }),
        }));
        (conn, rx)
    }

    fn auth_ok(id: &str) -> impl FnOnce(Vec<String>) -> Control + '_ {
        move |peers_online| Control::AuthOk { device_id: id.into(), server_time_ms: 0, peers_online, quota: None }
    }

    /// 取出队列里已经入队的控制消息。
    fn drain(rx: &mut OutRx) -> Vec<Control> {
        let mut out = Vec::new();
        while let Some(item) = rx.try_recv() {
            if let Outbound::Frame(b) = &item.msg {
                out.push(Envelope::decode(b).unwrap().control_body().unwrap());
            }
        }
        out
    }

    #[test]
    fn a_preview_is_indexed_by_node_and_account_and_the_indexes_are_cleaned_up() {
        let r = Registry::default();
        let (node, mut node_rx) = device(&r, "node", DeviceKind::Daemon);
        r.register(&node, auth_ok("node"));
        drain(&mut node_rx);

        let (p1, mut p1_rx) = preview(&r, "p1", "acc", "node", "label-1");
        let (p2, _p2_rx) = preview(&r, "p2", "acc", "node", "label-2");
        r.register_preview(&p1, 2, auth_ok("p1")).unwrap();
        r.register_preview(&p2, 2, auth_ok("p2")).unwrap();
        assert_eq!(r.preview_count("acc"), 2);
        assert_eq!(r.preview_count("someone-else"), 0);
        // 节点在线：p1 的 auth_ok 里有它；节点收到 preview_peer（不是 peer_online），并把预览连接加进对端集合
        assert!(matches!(&drain(&mut p1_rx)[..], [Control::AuthOk { peers_online, .. }] if peers_online == &["node"]));
        assert_eq!(
            drain(&mut node_rx),
            [
                Control::PreviewPeer { peer: "p1".into(), label: "label-1".into() },
                Control::PreviewPeer { peer: "p2".into(), label: "label-2".into() },
            ]
        );
        assert!(node.has_peer("p1") && node.has_peer("p2"));
        // 它们是路由表里的连接，但不是设备
        assert!(r.is_online("p1") && r.len() == 3 && r.permanent_count() == 1);

        // 满了：不登记，也不通知节点
        let (p3, _rx) = preview(&r, "p3", "acc", "node", "label-3");
        assert_eq!(r.register_preview(&p3, 2, auth_ok("p3")), Err(PreviewQuotaFull));
        assert!(!r.is_online("p3") && drain(&mut node_rx).is_empty() && !node.has_peer("p3"));
        // 别的账号有自己的额度
        let (q, _rx) = preview(&r, "q1", "acc-2", "node", "label-q");
        assert!(r.register_preview(&q, 2, auth_ok("q1")).is_ok());

        // 断开：索引、对端集合清掉，节点收到 peer_offline
        assert!(r.unregister("p1", p1.id));
        assert_eq!(r.preview_count("acc"), 1);
        assert!(!node.has_peer("p1"));
        assert!(drain(&mut node_rx).contains(&Control::PeerOffline { device: "p1".into() }));
        // 过期的连接号（已经被别的连接顶掉）不会误删
        assert!(!r.unregister("p2", p2.id + 100));
        assert_eq!(r.preview_count("acc"), 1);
    }

    #[test]
    fn a_node_that_comes_online_later_introduces_itself_to_the_waiting_previews() {
        let r = Registry::default();
        let (p, mut p_rx) = preview(&r, "p", "acc", "node", "label");
        r.register_preview(&p, 16, auth_ok("p")).unwrap();
        assert!(matches!(&drain(&mut p_rx)[..], [Control::AuthOk { peers_online, .. }] if peers_online.is_empty()));

        let (node, mut node_rx) = device(&r, "node", DeviceKind::Daemon);
        let reg = r.register(&node, auth_ok("node"));
        assert!(reg.peers_online.is_empty(), "previews are not in peers_online");
        let got = drain(&mut node_rx);
        assert!(matches!(&got[0], Control::AuthOk { peers_online, .. } if peers_online.is_empty()));
        assert_eq!(&got[1..], [Control::PreviewPeer { peer: "p".into(), label: "label".into() }]);
        assert_eq!(drain(&mut p_rx), [Control::PeerOnline { device: "node".into() }]);
        assert!(node.has_peer("p"));

        // 节点再来一条连接（顶掉旧的）：预览连接又收到 peer_online，新连接又收到 preview_peer
        let (node2, mut node2_rx) = device(&r, "node", DeviceKind::Daemon);
        let reg = r.register(&node2, auth_ok("node"));
        assert_eq!(reg.superseded.map(|c| c.id), Some(node.id));
        let got = drain(&mut node2_rx);
        assert_eq!(&got[1..], [Control::PreviewPeer { peer: "p".into(), label: "label".into() }]);
        assert_eq!(drain(&mut p_rx), [Control::PeerOnline { device: "node".into() }]);
        // 预览连接断开时，通知的是现在的节点连接
        r.unregister("p", p.id);
        assert!(drain(&mut node2_rx).contains(&Control::PeerOffline { device: "p".into() }));
        assert!(!node2.has_peer("p"));
    }

    #[test]
    fn closing_previews_of_a_device_matches_its_tokens_and_its_node_role() {
        let r = Registry::default();
        let (a, mut a_rx) = preview(&r, "a", "acc", "node-1", "l1"); // issuer = "issuer"
        let (b, mut b_rx) = preview(&r, "b", "acc", "node-2", "l2");
        r.register_preview(&a, 16, auth_ok("a")).unwrap();
        r.register_preview(&b, 16, auth_ok("b")).unwrap();
        drain(&mut a_rx);
        drain(&mut b_rx);
        let closed = |rx: &mut OutRx| {
            let mut closed = None;
            while let Some(item) = rx.try_recv() {
                if let Outbound::Close(code, _) = item.msg {
                    closed = Some(code);
                }
            }
            closed
        };
        r.close_previews_of("unrelated", 4004, "revoked");
        assert_eq!((closed(&mut a_rx), closed(&mut b_rx)), (None, None));
        r.close_previews_of("node-2", 4004, "revoked");
        assert_eq!((closed(&mut a_rx), closed(&mut b_rx)), (None, Some(4004)), "by node");
        r.close_previews_of("issuer", 4004, "revoked");
        assert_eq!(closed(&mut a_rx), Some(4004), "by issuing device");
    }
}
