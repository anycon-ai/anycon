//! 已连接的客户端：本机 socket 连接和经 hub 的 Noise 对端是同一种东西。
//!
//! 每个客户端有一个 [`ClientSink`]（通知出口）和一个 watch 集合。事件按 watcher 逐个发，
//! 列表级通知（`session.changed`、`permission.request` 等）发给所有客户端（docs/spec/daemon.md § 12）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use anycon_protocol::{DeviceKind, RpcNotification};
use serde::{Deserialize, Serialize};

/// 调用方身份，审计用（docs/spec/daemon.md § 13）。
#[derive(Debug, Clone)]
pub struct Caller {
    pub device_id: String,
    pub device_name: String,
    pub kind: DeviceKind,
    /// 经本机 Unix socket 进来的。
    pub local: bool,
}

impl Caller {
    /// daemon 自己发起的操作（PR 合并后自动归档、保留期清理…）的审计身份。
    pub fn daemon() -> Self {
        Caller { device_id: "daemon".into(), device_name: "daemon".into(), kind: DeviceKind::Daemon, local: true }
    }

    pub fn local_cli() -> Self {
        Self::local(LocalClientKind::Cli)
    }

    /// 本机通道上的调用方。`device_id` 固定是 `local`，`device_name` 随自报的客户端种类变（审计里分得出来）。
    pub fn local(kind: LocalClientKind) -> Self {
        Caller { device_id: "local".into(), device_name: kind.device_name().into(), kind: DeviceKind::Cli, local: true }
    }
}

/// 本机通道上的客户端自报的种类（`local.set_client`）。固定枚举，不接受自由文本：审计里的设备名只会是下面几个值之一，
/// 一个本机进程没法把自己写成别的名字。这只是「谁在用本机通道」的标注，不是权限：本机 socket 的权限由文件属主决定（ADR-27）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalClientKind {
    /// 人敲的 `anycon` 命令（缺省，不声明就是它）。
    Cli,
    /// `anycon mcp`：别的 agent 经 MCP 工具在操作这个节点（ADR-49）。
    Mcp,
}

impl LocalClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LocalClientKind::Cli => "cli",
            LocalClientKind::Mcp => "mcp",
        }
    }

    /// 审计里的设备名。
    pub fn device_name(self) -> &'static str {
        match self {
            LocalClientKind::Cli => "local-cli",
            LocalClientKind::Mcp => "local-mcp",
        }
    }
}

/// 通知出口。实现必须是非阻塞的（把消息放进队列就返回）。
pub trait ClientSink: Send + Sync {
    fn send(&self, n: RpcNotification);
}

/// 一个客户端最多 watch 的会话数。
pub const MAX_WATCHES: usize = 5;

pub struct ClientHandle {
    pub conn_id: u64,
    /// 本机连接可以用 `local.set_client` 改自己的审计身份，所以放在锁里；读用 [`ClientHandle::caller`]。
    caller: RwLock<Caller>,
    sink: Box<dyn ClientSink>,
    watching: Mutex<HashSet<String>>,
    /// 连接已经断开（[`Clients::unregister`] 置位）。长请求（导出 / 导入）据此取消。
    gone: AtomicBool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WatchError {
    LimitReached,
}

impl ClientHandle {
    /// 当前的调用方身份（一份拷贝）。
    pub fn caller(&self) -> Caller {
        self.caller.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 本机连接声明自己的客户端种类。非本机连接返回 `false`，身份不变：经 hub 的连接身份由配对的设备决定。
    pub fn set_local_kind(&self, kind: LocalClientKind) -> bool {
        let mut c = self.caller.write().unwrap_or_else(|e| e.into_inner());
        if !c.local || c.kind == DeviceKind::Daemon {
            return false;
        }
        *c = Caller::local(kind);
        true
    }

    pub fn notify(&self, n: &RpcNotification) {
        self.sink.send(n.clone());
    }

    /// 这个连接已经断开了。
    pub fn is_gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }

    pub fn watch(&self, session_id: &str) -> Result<(), WatchError> {
        let mut w = self.watching.lock().unwrap();
        if !w.contains(session_id) && w.len() >= MAX_WATCHES {
            return Err(WatchError::LimitReached);
        }
        w.insert(session_id.to_string());
        Ok(())
    }

    pub fn unwatch(&self, session_id: &str) {
        self.watching.lock().unwrap().remove(session_id);
    }

    pub fn is_watching(&self, session_id: &str) -> bool {
        self.watching.lock().unwrap().contains(session_id)
    }

    pub fn watched(&self) -> Vec<String> {
        self.watching.lock().unwrap().iter().cloned().collect()
    }
}

#[derive(Default)]
pub struct Clients {
    next: AtomicU64,
    map: Mutex<HashMap<u64, Arc<ClientHandle>>>,
}

impl Clients {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn register(&self, caller: Caller, sink: Box<dyn ClientSink>) -> Arc<ClientHandle> {
        let conn_id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let h = Arc::new(ClientHandle {
            conn_id,
            caller: RwLock::new(caller),
            sink,
            watching: Mutex::new(HashSet::new()),
            gone: AtomicBool::new(false),
        });
        self.map.lock().unwrap().insert(conn_id, h.clone());
        h
    }

    pub fn unregister(&self, conn_id: u64) {
        if let Some(h) = self.map.lock().unwrap().remove(&conn_id) {
            h.gone.store(true, Ordering::SeqCst);
        }
    }

    /// 经 hub 的对端（非本机）按设备 id 取。
    pub fn by_device(&self, device_id: &str) -> Option<Arc<ClientHandle>> {
        self.map
            .lock()
            .unwrap()
            .values()
            .find(|c| {
                let caller = c.caller();
                !caller.local && caller.device_id == device_id
            })
            .cloned()
    }

    /// 注销某设备的所有（非本机）连接。
    pub fn unregister_device(&self, device_id: &str) {
        self.map.lock().unwrap().retain(|_, c| {
            let caller = c.caller();
            let keep = caller.local || caller.device_id != device_id;
            if !keep {
                c.gone.store(true, Ordering::SeqCst);
            }
            keep
        });
    }

    fn snapshot(&self) -> Vec<Arc<ClientHandle>> {
        self.map.lock().unwrap().values().cloned().collect()
    }

    /// 发给所有在线客户端。
    pub fn broadcast(&self, method: &str, params: impl Serialize) {
        let n = match RpcNotification::new(method, params) {
            Some(n) => n,
            None => return,
        };
        for c in self.snapshot() {
            c.notify(&n);
        }
    }

    /// 只发给 watch 了该会话的客户端。返回送达的客户端数。
    pub fn to_watchers(&self, session_id: &str, method: &str, params: impl Serialize) -> usize {
        let Some(n) = RpcNotification::new(method, params) else { return 0 };
        let mut count = 0;
        for c in self.snapshot() {
            if c.is_watching(session_id) {
                c.notify(&n);
                count += 1;
            }
        }
        count
    }

    pub fn has_watchers(&self, session_id: &str) -> bool {
        self.snapshot().iter().any(|c| c.is_watching(session_id))
    }

    /// 会话被删除后，从所有 watch 集合里去掉。
    pub fn forget_session(&self, session_id: &str) {
        for c in self.snapshot() {
            c.unwatch(session_id);
        }
    }

    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `RpcNotification` 的构造便利。
pub trait NotificationExt: Sized {
    fn new(method: &str, params: impl Serialize) -> Option<Self>;
}

impl NotificationExt for RpcNotification {
    fn new(method: &str, params: impl Serialize) -> Option<Self> {
        let params = serde_json::to_value(params).ok()?;
        Some(RpcNotification { jsonrpc: "2.0".into(), method: method.to_string(), params })
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use tokio::sync::mpsc;

    /// 测试用：把通知收进 channel。
    pub struct ChanSink(pub mpsc::UnboundedSender<RpcNotification>);

    impl ClientSink for ChanSink {
        fn send(&self, n: RpcNotification) {
            let _ = self.0.send(n);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::ChanSink;
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn watch_limit_and_routing() {
        let clients = Clients::new();
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        let a = clients.register(Caller::local_cli(), Box::new(ChanSink(tx1)));
        let b = clients.register(Caller::local_cli(), Box::new(ChanSink(tx2)));
        a.watch("s1").unwrap();
        for i in 0..MAX_WATCHES - 1 {
            a.watch(&format!("x{i}")).unwrap();
        }
        assert_eq!(a.watch("one-too-many"), Err(WatchError::LimitReached));
        a.watch("s1").unwrap(); // 已在集合里，不算超限
        assert_eq!(clients.to_watchers("s1", "session.event", serde_json::json!({"n": 1})), 1);
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        clients.broadcast("session.changed", serde_json::json!({}));
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
        assert!(clients.has_watchers("s1"));
        clients.forget_session("s1");
        assert!(!clients.has_watchers("s1"));
        clients.unregister(b.conn_id);
        assert_eq!(clients.len(), 1);
    }

    #[test]
    fn local_kind_changes_only_the_device_name_and_only_for_local_connections() {
        let clients = Clients::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let local = clients.register(Caller::local_cli(), Box::new(ChanSink(tx)));
        assert_eq!(local.caller().device_name, "local-cli");
        assert!(local.set_local_kind(LocalClientKind::Mcp));
        let c = local.caller();
        assert_eq!((c.device_id.as_str(), c.device_name.as_str(), c.local), ("local", "local-mcp", true));
        assert!(local.set_local_kind(LocalClientKind::Cli));
        assert_eq!(local.caller().device_name, "local-cli");

        // 经 hub 的连接：身份由配对的设备决定，声明被拒绝，身份不变
        let (tx, _rx) = mpsc::unbounded_channel();
        let remote = clients.register(
            Caller { device_id: "dev1".into(), device_name: "Phone".into(), kind: DeviceKind::Web, local: false },
            Box::new(ChanSink(tx)),
        );
        assert!(!remote.set_local_kind(LocalClientKind::Mcp));
        assert_eq!((remote.caller().device_id.as_str(), remote.caller().device_name.as_str()), ("dev1", "Phone"));
    }

    #[test]
    fn local_kind_is_a_closed_set() {
        for (text, kind) in [("cli", LocalClientKind::Cli), ("mcp", LocalClientKind::Mcp)] {
            assert_eq!(serde_json::from_value::<LocalClientKind>(serde_json::json!(text)).unwrap(), kind);
            assert_eq!(kind.as_str(), text);
        }
        for bad in ["", "CLI", "local-mcp", "web", "anything I like"] {
            assert!(serde_json::from_value::<LocalClientKind>(serde_json::json!(bad)).is_err(), "{bad:?}");
        }
    }
}
