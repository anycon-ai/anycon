//! 连接注册表（路由表）：device id → 当前连接。
//!
//! - 一个设备同时只有一条连接：新连接 [`Registry::register`] 顶掉旧的，旧的由调用方以 4001 关闭。
//! - 临时连接（`device_token = None`）用 hub 分配的临时 id 登记，配对完成后 [`Registry::upgrade`] 换成正式 id。
//! - 每条连接缓存自己的配对对端集合（认证时从库里读，之后随配对 / 吊销实时更新），
//!   转发时只查内存，不查库。
//! - 「粘住」状态（[`super::sticky`]）挂在路由表上：连接结束、设备下线 / 被顶掉 / 被吊销时在这里一并清理。

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anycon_protocol::{ClientInfo, Control, Envelope, Kind};
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

#[derive(Default)]
pub struct Registry {
    map: RwLock<HashMap<String, Arc<Conn>>>,
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
    pub fn register(&self, conn: &Arc<Conn>, auth_ok: impl FnOnce(Vec<String>) -> Control) -> Registered {
        let mut map = self.map.write().expect("registry");
        let device_id = conn.device_id();
        let superseded = map.insert(device_id.clone(), conn.clone());
        let peers_online: Vec<String> = conn.peers_snapshot().into_iter().filter(|p| map.contains_key(p)).collect();
        conn.send_control(&auth_ok(peers_online.clone()));
        let superseded = superseded.filter(|old| old.id != conn.id);
        if let Some(old) = &superseded {
            // 新连接是一条全新的传输（对端会收到 peer_online 重新握手）：旧连接留下的顺序约束作废。
            self.sticky.forget_toward(&device_id);
            self.sticky.forget_sender(old.id);
        }
        Registered { superseded, peers_online }
    }

    pub fn get(&self, device_id: &str) -> Option<Arc<Conn>> {
        self.map.read().expect("registry").get(device_id).cloned()
    }

    pub fn is_online(&self, device_id: &str) -> bool {
        self.map.read().expect("registry").contains_key(device_id)
    }

    /// 仅当登记的还是这条连接时才移除。返回是否真的移除了。
    ///
    /// 无论是否移除，这条连接作为发送方留下的粘住记录都清掉；真的移除了（设备下线）的话，发给这个设备的记录也一并清掉。
    pub fn unregister(&self, device_id: &str, conn_id: u64) -> bool {
        let mut map = self.map.write().expect("registry");
        self.sticky.forget_sender(conn_id);
        match map.get(device_id) {
            Some(c) if c.id == conn_id => {
                map.remove(device_id);
                self.sticky.forget_toward(device_id);
                true
            }
            _ => false,
        }
    }

    /// 临时连接换成正式身份。返回被顶掉的同 id 连接（正常不会有）。
    pub fn upgrade(&self, conn: &Arc<Conn>, new_id: &str, account_id: Option<String>) -> Option<Arc<Conn>> {
        let mut map = self.map.write().expect("registry");
        let old_id = conn.device_id();
        if map.get(&old_id).is_some_and(|c| c.id == conn.id) {
            map.remove(&old_id);
        }
        conn.set_identity(new_id.to_string(), account_id);
        let displaced = map.insert(new_id.to_string(), conn.clone()).filter(|old| old.id != conn.id);
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
        let map = self.map.read().expect("registry");
        if map.get(to).is_some_and(|c| c.id == target.id) {
            self.sticky.returned(sender, to, target.id, seq, cause)?;
        }
        Ok(())
    }

    /// 粘住状态（读 / 放行 / 清除用；登记用 [`Registry::stick`]）。
    pub fn sticky(&self) -> &Sticky {
        &self.sticky
    }

    pub fn len(&self) -> usize {
        self.map.read().expect("registry").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 账号此刻有没有任何 **web** 设备在线。邮件通知用它判断「用户正在看」（docs/spec/hub.md § 6.2）：
    /// 只有浏览器代表有人在看；daemon、一直连着的 CLI / SDK 设备（脚本、机器人）和还没有身份的临时连接都不算——
    /// 否则一台常驻的 SDK 节点会让邮件通知永远不发。
    pub fn account_has_web_online(&self, account_id: &str) -> bool {
        use anycon_protocol::DeviceKind;
        self.map.read().expect("registry").values().any(|c| {
            c.is_permanent() && c.client.kind == DeviceKind::Web && c.account_id().as_deref() == Some(account_id)
        })
    }

    pub fn permanent_count(&self) -> usize {
        self.map.read().expect("registry").values().filter(|c| c.is_permanent()).count()
    }

    /// 向所有连接发 Close（优雅关停用 4002）。
    pub fn close_all(&self, code: u16, reason: &'static str) {
        let conns: Vec<Arc<Conn>> = self.map.read().expect("registry").values().cloned().collect();
        for c in conns {
            c.close(code, reason);
        }
    }
}
