//! `preview.*` RPC（docs/protocol.md § 5.7，docs/spec/daemon.md § 19.2）：签发 / 复用票据、作废、列出、列端口。
//!
//! 票据密钥（`psk`）只出现在 `preview.open` 的结果里和库里：不写日志、不进审计。

use anycon_protocol::tunnel as wire;
use anycon_protocol::{
    ErrorCode, PreviewCloseParams, PreviewGoawayCode, PreviewInfo, PreviewListResult, PreviewOpenParams,
    PreviewOpenResult, PreviewPortsParams, PreviewPortsResult, RpcError,
};
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::json;

use super::ports::{self, Policy, Verdict};
use super::{Preview, MAX_TICKETS_PER_DEVICE};
use crate::clients::Caller;
use crate::store::PreviewTicketRow;
use crate::util::now_ms;

type RpcResult<T> = Result<T, RpcError>;

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e.to_string())
}

/// `forbidden`，`data.reason` 说明原因（`port_not_allowed` / `disabled`）。
fn forbidden(reason: &str, message: &str) -> RpcError {
    let mut e = RpcError::new(ErrorCode::Forbidden, message);
    if let Some(d) = e.data.as_mut() {
        d.reason = Some(reason.to_string());
    }
    e
}

fn random_array<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}

/// 剩余有效期超过票据总有效期的一半，复用它（docs/spec/daemon.md § 19.2）。
pub fn reusable(created_at_ms: u64, expires_at_ms: u64, now_ms: u64) -> bool {
    let total = expires_at_ms.saturating_sub(created_at_ms);
    let left = expires_at_ms.saturating_sub(now_ms);
    total > 0 && left.saturating_mul(2) > total
}

impl Preview {
    /// `preview.open { port, session_id? }`。
    pub async fn open(&self, caller: &Caller, p: PreviewOpenParams) -> RpcResult<PreviewOpenResult> {
        let cfg = self.d.config.get();
        if !cfg.preview.enabled {
            return Err(forbidden("disabled", "dev server preview is turned off on this node"));
        }
        if p.port == 0 {
            return Err(RpcError::new(ErrorCode::Invalid, "port must be 1-65535"));
        }
        if let Some(sid) = &p.session_id {
            self.d.sessions.get_summary(sid).await?;
        }
        match self.check_port(p.port, false).await {
            Verdict::Allowed => {}
            Verdict::Disabled => return Err(forbidden("disabled", "dev server preview is turned off on this node")),
            Verdict::NotAllowed => {
                return Err(forbidden(
                    "port_not_allowed",
                    "this port is not served by a session; add it to preview.allow_ports to preview it",
                ))
            }
        }

        let _guard = self.open_lock.lock().await;
        let now = now_ms();
        let label = match self.d.store.preview_label_get(p.port).await.map_err(internal)? {
            Some(l) => l,
            None => {
                let fresh = wire::label_from_bytes(&random_array::<16>());
                self.d.store.preview_label_ensure(p.port, &fresh, now).await.map_err(internal)?
            }
        };
        let existing = self.d.store.preview_ticket_latest(&caller.device_id, p.port, now).await.map_err(internal)?;
        let (row, reused) = match existing {
            Some(t) if reusable(t.created_at_ms, t.expires_at_ms, now) => (t, true),
            _ => {
                let t = PreviewTicketRow {
                    ticket_id: hex::encode(random_array::<{ wire::TICKET_ID_LEN }>()),
                    label: label.clone(),
                    port: p.port,
                    psk: hex::encode(random_array::<{ wire::PSK_LEN }>()),
                    device_id: caller.device_id.clone(),
                    session_id: p.session_id.clone(),
                    created_at_ms: now,
                    expires_at_ms: now + cfg.preview_ticket_ttl().as_millis() as u64,
                };
                self.d.store.preview_ticket_insert(&t).await.map_err(internal)?;
                self.trim_device_tickets(&caller.device_id, now).await;
                (t, false)
            }
        };
        let session_id = p.session_id.or_else(|| row.session_id.clone());
        self.d
            .audit
            .record(
                caller,
                "preview.open",
                Some(&p.port.to_string()),
                json!({
                    "label": row.label,
                    "ticket_id": row.ticket_id,
                    "session_id": session_id,
                    "expires_at_ms": row.expires_at_ms,
                    "reused": reused,
                }),
            )
            .await;
        Ok(PreviewOpenResult {
            label: row.label,
            port: row.port,
            ticket_id: row.ticket_id,
            psk: row.psk,
            expires_at_ms: row.expires_at_ms,
            session_id,
        })
    }

    /// 每个设备至多 [`MAX_TICKETS_PER_DEVICE`] 张未过期的票据，超了删最旧的（并断开用它的隧道）。
    async fn trim_device_tickets(&self, device_id: &str, now: u64) {
        let Ok(tickets) = self.d.store.preview_tickets_of_device(device_id, now).await else { return };
        let excess = tickets.len().saturating_sub(MAX_TICKETS_PER_DEVICE);
        for old in tickets.into_iter().take(excess) {
            if let Err(e) = self.d.store.preview_ticket_delete(&old.ticket_id).await {
                tracing::warn!(error = %e, "cannot remove an old preview ticket");
                continue;
            }
            self.goaway_where(PreviewGoawayCode::Closed, |i| i.ticket_id == old.ticket_id);
        }
    }

    /// `preview.close { label }`：作废这个 label 的全部票据，对用它们的隧道发 `GOAWAY { closed }`。不存在也算成功。
    pub async fn close(&self, caller: &Caller, p: PreviewCloseParams) -> RpcResult<()> {
        let ids = self.d.store.preview_tickets_delete_label(&p.label).await.map_err(internal)?;
        // 先删库再断隧道：握手在登记隧道之后会再确认一次票据还在，两边总有一边拦得住
        let tunnels = self.goaway_where(PreviewGoawayCode::Closed, |i| i.label == p.label);
        self.d
            .audit
            .record(caller, "preview.close", Some(&p.label), json!({ "tickets": ids.len(), "tunnels": tunnels }))
            .await;
        Ok(())
    }

    /// `preview.list`：全部未过期的票据。
    pub async fn list(&self) -> RpcResult<PreviewListResult> {
        let tickets = self.d.store.preview_tickets_live(now_ms()).await.map_err(internal)?;
        let in_use = self.tunnels_per_ticket();
        let previews = tickets
            .into_iter()
            .map(|t| {
                let created_by_name = match self.d.peers.record(&t.device_id) {
                    Some(r) => r.name,
                    None if t.device_id == Caller::local_cli().device_id => Caller::local_cli().device_name,
                    None => t.device_id.chars().take(8).collect(),
                };
                PreviewInfo {
                    tunnels: in_use.get(&t.ticket_id).copied().unwrap_or(0),
                    label: t.label,
                    port: t.port,
                    ticket_id: t.ticket_id,
                    session_id: t.session_id,
                    created_by: t.device_id,
                    created_by_name,
                    created_at_ms: t.created_at_ms,
                    expires_at_ms: t.expires_at_ms,
                }
            })
            .collect();
        Ok(PreviewListResult { previews })
    }

    /// `preview.ports { session_id? }`：可以预览的本机端口。会起子进程（`lsof` / `ps`），调用方放进 `runs_detached`。
    pub async fn ports(&self, p: PreviewPortsParams) -> RpcResult<PreviewPortsResult> {
        let cfg = self.d.config.get();
        let allow = cfg.preview_allow_ports();
        let policy = Policy {
            enabled: cfg.preview.enabled,
            allow_ports: &allow,
            hub_port: ports::local_hub_port(cfg.hub.url.as_deref()),
        };
        let snap = self.detector().snapshot().await;
        let sessions = self.d.sessions.preview_targets().await;
        let ports = ports::list_ports(&policy, &snap, &sessions, p.session_id.as_deref());
        Ok(PreviewPortsResult { ports, detection: snap.detection() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_reused_while_more_than_half_of_its_lifetime_is_left() {
        let day = 24 * 3600 * 1000u64;
        assert!(reusable(0, day, 0));
        assert!(reusable(0, day, day / 2 - 1));
        assert!(!reusable(0, day, day / 2), "exactly half is not more than half");
        assert!(!reusable(0, day, day));
        assert!(!reusable(0, day, 2 * day), "expired");
        assert!(!reusable(10, 10, 5), "a zero-length ticket is never reused");
        // 不依赖当前配置的 ttl：用票据自己的有效期
        assert!(reusable(1000, 1000 + 600_000, 1000 + 200_000));
    }

    #[test]
    fn forbidden_carries_the_reason() {
        let e = forbidden("port_not_allowed", "no");
        assert_eq!(e.stable_code(), Some(ErrorCode::Forbidden));
        assert_eq!(e.data.unwrap().reason.as_deref(), Some("port_not_allowed"));
    }
}
