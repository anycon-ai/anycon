//! 通知触发（docs/spec/daemon.md § 14）。
//!
//! daemon 发 `kind = notify` 的信封，外层只带类别枚举 + 会话 id。同一会话同一类别 60 秒内只发一次。
//! 真正的发送（给所有已配对设备）由 [`NotifyOut`] 的实现完成（hub 连接侧）；这里只管「该不该发」。
//!
//! v0.1 不带密文详情（`detail = None`）：`Transport` 是有状态的计数器加密，带外的 notify 信封解密会打乱数据流的计数，
//! 而离线客户端本来就没有可解密的会话。详情留给后续版本（单独的封装密钥）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anycon_protocol::{NotifyBody, NotifyCategory};

use crate::clients::Clients;
use crate::config::ConfigStore;

pub trait NotifyOut: Send + Sync {
    fn send(&self, body: NotifyBody);
}

const DEDUPE_WINDOW: Duration = Duration::from_secs(60);

pub struct Notifier {
    out: Mutex<Option<Arc<dyn NotifyOut>>>,
    config: Arc<ConfigStore>,
    clients: Arc<Clients>,
    last: Mutex<HashMap<(String, NotifyCategory), Instant>>,
}

impl Notifier {
    pub fn new(config: Arc<ConfigStore>, clients: Arc<Clients>) -> Arc<Self> {
        Arc::new(Notifier { out: Mutex::new(None), config, clients, last: Mutex::new(HashMap::new()) })
    }

    /// 后绑定发送出口（hub 连接建好之后）。
    pub fn set_out(&self, out: Arc<dyn NotifyOut>) {
        *self.out.lock().unwrap() = Some(out);
    }

    fn fire(&self, category: NotifyCategory, session_id: &str) {
        if !self.admit(session_id, category) {
            return;
        }
        self.emit(category, Some(session_id.to_string()));
    }

    /// 同一（会话，类别）60 秒内只放行一次。
    fn admit(&self, key: &str, category: NotifyCategory) -> bool {
        let mut last = self.last.lock().unwrap();
        let key = (key.to_string(), category);
        if last.get(&key).is_some_and(|t| t.elapsed() < DEDUPE_WINDOW) {
            return false;
        }
        last.insert(key, Instant::now());
        // 顺手清理过期项，防止无限增长
        if last.len() > 1024 {
            last.retain(|_, t| t.elapsed() < DEDUPE_WINDOW);
        }
        true
    }

    fn emit(&self, category: NotifyCategory, session_id: Option<String>) {
        if let Some(out) = self.out.lock().unwrap().clone() {
            out.send(NotifyBody { category, session_id, detail: None });
        }
    }

    /// 每次 `permission` 事件，立即。
    pub fn permission_requested(&self, session_id: &str) {
        self.fire(NotifyCategory::PermissionRequested, session_id);
    }

    /// `turn_done`，按 `notify.turn_finished`。
    pub fn turn_finished(&self, session_id: &str) {
        match self.config.get().notify.turn_finished.as_str() {
            "never" => {}
            "always" => self.fire(NotifyCategory::TurnFinished, session_id),
            _ => {
                if !self.clients.has_watchers(session_id) {
                    self.fire(NotifyCategory::TurnFinished, session_id);
                }
            }
        }
    }

    /// 不可恢复的 error 事件，或会话进入 error。
    pub fn error(&self, session_id: &str) {
        self.fire(NotifyCategory::Error, session_id);
    }

    /// 会话 idle 超过 `session_idle_after` 且期间没人看过。
    pub fn session_idle(&self, session_id: &str) {
        self.fire(NotifyCategory::SessionIdle, session_id);
    }

    /// 有设备请求批准接入（ADR-36，`code` 且 `approval = true`）：每个请求一次，立即，不合并。没有 `session_id`，不带任何细节。
    pub fn enroll_requested(&self) {
        self.emit(NotifyCategory::EnrollRequested, None);
    }

    /// 接入相关的安全事件（新设备经接入加入、口令连续失败 / 锁定、口令或 `[enroll]` 被改）：没有会话，按节点算，
    /// 60 秒内的多条合并成一次（详情经 `node.security_event` 逐条发给在线客户端）。
    pub fn security(&self) {
        if self.admit("", NotifyCategory::Security) {
            self.emit(NotifyCategory::Security, None);
        }
    }

    pub fn forget(&self, session_id: &str) {
        self.last.lock().unwrap().retain(|(sid, _), _| sid != session_id);
    }
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;

    /// 测试用：记录发出的通知。
    #[derive(Default)]
    pub struct Recorder(pub Mutex<Vec<NotifyBody>>);

    impl NotifyOut for Recorder {
        fn send(&self, body: NotifyBody) {
            self.0.lock().unwrap().push(body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Recorder;
    use super::*;
    use crate::clients::testing::ChanSink;
    use crate::clients::Caller;
    use crate::config::{Config, Paths};

    #[test]
    fn enroll_requests_are_never_merged_and_security_pushes_merge_per_node() {
        let d = tempfile::tempdir().unwrap();
        let config = ConfigStore::with_config(Paths::new(d.path()), Config::default());
        let n = Notifier::new(config, Clients::new());
        let rec = Arc::new(Recorder::default());
        n.set_out(rec.clone());
        // 每个请求批准的接入各推一次，不带会话 id 和任何细节
        n.enroll_requested();
        n.enroll_requested();
        // security 没有会话，按节点算：60 秒内的多条合并成一次
        n.security();
        n.security();
        n.security();
        // 和会话类别互不影响
        n.permission_requested("s1");
        let got = rec.0.lock().unwrap().clone();
        let cats: Vec<NotifyCategory> = got.iter().map(|b| b.category).collect();
        assert_eq!(
            cats,
            vec![
                NotifyCategory::EnrollRequested,
                NotifyCategory::EnrollRequested,
                NotifyCategory::Security,
                NotifyCategory::PermissionRequested
            ]
        );
        assert!(got[..3].iter().all(|b| b.session_id.is_none() && b.detail.is_none()));
    }

    #[test]
    fn dedupe_and_turn_finished_rules() {
        let d = tempfile::tempdir().unwrap();
        let config = ConfigStore::with_config(Paths::new(d.path()), Config::default());
        let clients = Clients::new();
        let n = Notifier::new(config, clients.clone());
        let rec = Arc::new(Recorder::default());
        n.set_out(rec.clone());
        n.permission_requested("s1");
        n.permission_requested("s1"); // 60 秒内同类别去重
        n.permission_requested("s2");
        assert_eq!(rec.0.lock().unwrap().len(), 2);
        // when_unwatched：没人看才发
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let c = clients.register(Caller::local_cli(), Box::new(ChanSink(tx)));
        c.watch("s3").unwrap();
        n.turn_finished("s3");
        assert_eq!(rec.0.lock().unwrap().len(), 2);
        n.turn_finished("s4");
        let got = rec.0.lock().unwrap().clone();
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].category, NotifyCategory::TurnFinished);
        assert_eq!(got[2].session_id.as_deref(), Some("s4"));
        assert!(got.iter().all(|b| b.detail.is_none()));
    }
}
