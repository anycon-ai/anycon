//! 已连接的客户端：本机 socket 连接和经 hub 的 Noise 对端是同一种东西。
//!
//! 每个客户端有一个 [`ClientSink`]（通知出口）和一个 watch 集合。事件按 watcher 逐个发，
//! 列表级通知（`session.changed`、`permission.request` 等）发给所有客户端（docs/spec/daemon.md § 12）。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anycon_protocol::{DeviceKind, RpcNotification};
use serde::Serialize;

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
        Caller { device_id: "local".into(), device_name: "local-cli".into(), kind: DeviceKind::Cli, local: true }
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
    pub caller: Caller,
    sink: Box<dyn ClientSink>,
    watching: Mutex<HashSet<String>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WatchError {
    LimitReached,
}

impl ClientHandle {
    pub fn notify(&self, n: &RpcNotification) {
        self.sink.send(n.clone());
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
        let h = Arc::new(ClientHandle { conn_id, caller, sink, watching: Mutex::new(HashSet::new()) });
        self.map.lock().unwrap().insert(conn_id, h.clone());
        h
    }

    pub fn unregister(&self, conn_id: u64) {
        self.map.lock().unwrap().remove(&conn_id);
    }

    /// 经 hub 的对端（非本机）按设备 id 取。
    pub fn by_device(&self, device_id: &str) -> Option<Arc<ClientHandle>> {
        self.map.lock().unwrap().values().find(|c| !c.caller.local && c.caller.device_id == device_id).cloned()
    }

    /// 注销某设备的所有（非本机）连接。
    pub fn unregister_device(&self, device_id: &str) {
        self.map.lock().unwrap().retain(|_, c| c.caller.local || c.caller.device_id != device_id);
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
}
