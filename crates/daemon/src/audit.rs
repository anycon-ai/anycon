//! 审计（docs/spec/daemon.md § 13）。记录谁在什么时候做了什么，不记录内容（`session.send` 只记长度）。

use anycon_protocol::AuditEntry;
use serde_json::Value;

use crate::clients::Caller;
use crate::store::Store;
use crate::util::now_ms;

#[derive(Clone)]
pub struct Audit {
    store: Store,
}

impl Audit {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub async fn record(&self, caller: &Caller, action: &str, target: Option<&str>, detail: Value) {
        let entry = AuditEntry {
            ts_ms: now_ms(),
            device_id: caller.device_id.clone(),
            device_name: caller.device_name.clone(),
            action: action.to_string(),
            target: target.map(str::to_string),
            detail,
        };
        if let Err(e) = self.store.insert_audit(&entry).await {
            tracing::warn!(error = %e, action, "audit write failed");
        }
    }

    pub async fn query(&self, since_ms: Option<u64>, device_id: Option<&str>, limit: u32) -> Vec<AuditEntry> {
        self.store.query_audit(since_ms, device_id, limit).await.unwrap_or_default()
    }
}
