//! 审计（docs/spec/daemon.md § 13）。记录谁在什么时候做了什么，不记录内容（`session.send` 只记长度）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anycon_protocol::AuditEntry;
use serde_json::Value;

use crate::clients::Caller;
use crate::store::Store;
use crate::util::now_ms;

/// 分块读同一个文件时，同一台设备对同一路径的后续块在这个窗口内不再各记一条。
const FS_READ_DEDUP_MS: u64 = 5 * 60 * 1000;
/// 去重表超过这么多项就清掉已过期的。
const FS_READ_DEDUP_PRUNE_AT: usize = 1024;

#[derive(Clone)]
pub struct Audit {
    store: Store,
    /// `(device_id, path)` -> 上次记 `fs.read` 的时间。
    fs_reads: Arc<Mutex<HashMap<(String, String), u64>>>,
}

impl Audit {
    pub fn new(store: Store) -> Self {
        Self { store, fs_reads: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// `fs.read` 的审计。一个大文件要分很多块读（每块 512 KiB），逐块记会把审计表灌满、盖掉有意义的行；
    /// 所以同一台设备在窗口内对同一路径的读只记第一次。窗口按「路径」而不是按「第 0 块」算：
    /// 否则跳过第 0 块、只读后面的块就能绕开审计。窗口只在真正写了一行时刷新，持续读一个文件每个窗口至少有一行。
    pub async fn record_fs_read(&self, caller: &Caller, path: &str, bytes: u64) {
        let now = now_ms();
        {
            let mut seen = self.fs_reads.lock().unwrap_or_else(|e| e.into_inner());
            let key = (caller.device_id.clone(), path.to_string());
            if seen.get(&key).is_some_and(|&t| now.saturating_sub(t) < FS_READ_DEDUP_MS) {
                return;
            }
            if seen.len() >= FS_READ_DEDUP_PRUNE_AT {
                seen.retain(|_, &mut t| now.saturating_sub(t) < FS_READ_DEDUP_MS);
            }
            seen.insert(key, now);
        }
        self.record(caller, "fs.read", Some(path), serde_json::json!({ "bytes": bytes })).await;
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
