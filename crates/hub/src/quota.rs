//! 配额（仅 `multi`，docs/spec/hub.md § 7.1）：用量快照、`quota_update` 扇出、每日配对记账。
//!
//! `single` 没有配额（`Config.quota` 为 `None`）：这里的函数在 `single` 里要么返回 `None`，要么什么都不做，
//! 调用方不用自己再判断模式。四项配额各自在哪里执行：
//!
//! - **设备 / daemon 数**：配对和接入收尾建新设备那一步，由 [`crate::store::Store::create_devices_within_limits`]
//!   在一个写事务里「数 + 插」，并发收尾不会冲过上限（`pairing::try_finalize`）；
//! - **在线连接数**：永久设备认证时，[`crate::ws::registry::Registry::register_counted`] 在登记的同一把锁里检查；
//! - **每日配对与接入次数**：上限只有一份，[`crate::config::Limits::daily_pairings_per_account`]。`pair_begin` 和
//!   `enroll_begin`（发起一次配对 / 接入）在已知账号时检查并记一次（[`record_pairing_attempt`]）；`pair_join` 只检查、
//!   不记（[`pairing_quota_left`]）——输错的短码不该耗掉额度，一次成功的配对也不该让双方账号各算一次；滚动 24 小时的窗口；
//! - **用量展示**：`auth_ok.quota`、`quota_update`（[`broadcast`]）和 `GET /api/quota`（[`usage`]）。
//!
//! 日志纪律（docs/spec/hub.md § 10）：配额日志只记哪一项（`limit = "devices"` 之类），不记账号 id、邮箱。

use anycon_protocol::{Control, Quota};
use serde::Serialize;

use crate::app::AppState;
use crate::config::QuotaLimits;
use crate::store::{DeviceLimits, StoreResult};

/// 配额里和「新建设备」有关的两项。
pub fn device_limits(limits: &QuotaLimits) -> DeviceLimits {
    DeviceLimits { devices: limits.devices, daemons: limits.daemons }
}

/// 拼出协议里的 [`Quota`]：`*_max` 都是 `Some`（`multi` 的配额总是有上限；无上限的 `single` 根本不发 `Quota`）。
pub fn build(limits: &QuotaLimits, devices: u32, daemons: u32, connections: usize) -> Quota {
    Quota {
        devices,
        devices_max: Some(limits.devices),
        daemons,
        daemons_max: Some(limits.daemons),
        connections: u32::try_from(connections).unwrap_or(u32::MAX),
        connections_max: Some(limits.connections),
    }
}

/// 账号的设备、daemon 或连接数变了：把新的用量作为 `quota_update` 发给它此刻在线的每条永久连接。
/// `skip_conn` 是刚认证的那条连接（它在 `auth_ok` 里已经拿到了）。`single` 里什么都不做。
///
/// 设备数从库里读，连接数和收件人在发送的同一刻从注册表读：并发的两次变化可能让设备数晚一拍，下一次变化会补正。
/// 没有收件人就不查库（常见情形：账号只有一条连接）。
pub async fn broadcast(state: &AppState, account_id: &str, skip_conn: Option<u64>) {
    let Some(limits) = &state.config.quota else { return };
    if state.shutdown.is_cancelled() {
        return;
    }
    let has_recipient =
        |conns: &[std::sync::Arc<crate::ws::registry::Conn>]| conns.iter().any(|c| Some(c.id) != skip_conn);
    if !has_recipient(&state.registry.permanent_conns_of(account_id)) {
        return;
    }
    let (devices, daemons) = match state.store.count_devices(account_id).await {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read device counts for a quota update");
            return;
        }
    };
    let recipients = state.registry.permanent_conns_of(account_id);
    let msg = Control::QuotaUpdate { quota: build(limits, devices, daemons, recipients.len()) };
    for conn in recipients.iter().filter(|c| Some(c.id) != skip_conn) {
        conn.send_control(&msg);
    }
}

/// 检查并记一次「发起配对 / 接入」（`pair_begin`、`enroll_begin`，已知账号时）。窗口里已经满了返回 `false`
/// （这次不记）。没配每日上限（`single` 默认）永远 `true`。
pub fn record_pairing_attempt(state: &AppState, account_id: &str) -> bool {
    let Some(window) = &state.account_limits.daily_pairings else { return true };
    if window.try_record(&account_id.to_string()).is_err() {
        tracing::info!(limit = "daily_pairings", "quota exceeded");
        return false;
    }
    true
}

/// 只检查今天还有没有额度，不记（`pair_join`，已知账号时）：额度已经满了返回 `false`。
/// 加入方不消耗额度——配对是发起方（`pair_begin`）开的头，输错的短码也不该烧掉加入方的额度。
pub fn pairing_quota_left(state: &AppState, account_id: &str) -> bool {
    let Some(window) = &state.account_limits.daily_pairings else { return true };
    if window.is_full(&account_id.to_string()) {
        tracing::info!(limit = "daily_pairings", "quota exceeded");
        return false;
    }
    true
}

/// `GET /api/quota` 的响应。`*_max` 为 `null` 表示不限（`single` 总是）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaView {
    pub devices: u32,
    pub devices_max: Option<u32>,
    pub daemons: u32,
    pub daemons_max: Option<u32>,
    pub connections: u32,
    pub connections_max: Option<u32>,
    /// 滚动 24 小时内的配对与接入尝试次数。
    pub daily_pairings: u32,
    pub daily_pairings_max: Option<u32>,
}

/// 账号的用量，`single` 里计数照样是真的（上限为 `None`，`daily_pairings` 在没配每日上限时是 0）。
/// 每日配对的上限和计数器来自同一个配置值（`limits.daily_pairings_per_account`，限速器就是用它建的）。
pub async fn usage(state: &AppState, account_id: &str) -> StoreResult<QuotaView> {
    let (devices, daemons) = state.store.count_devices(account_id).await?;
    let limits = state.config.quota.as_ref();
    let daily = state.account_limits.daily_pairings.as_ref().map_or(0, |window| window.count(&account_id.to_string()));
    Ok(QuotaView {
        devices,
        devices_max: limits.map(|l| l.devices),
        daemons,
        daemons_max: limits.map(|l| l.daemons),
        connections: u32::try_from(state.registry.permanent_count_of(account_id)).unwrap_or(u32::MAX),
        connections_max: limits.map(|l| l.connections),
        daily_pairings: u32::try_from(daily).unwrap_or(u32::MAX),
        daily_pairings_max: state.config.limits.daily_pairings_per_account,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_reports_every_limit_and_saturates_the_connection_count() {
        let limits = QuotaLimits { devices: 10, daemons: 3, connections: 5 };
        let q = build(&limits, 4, 2, 3);
        assert_eq!(
            q,
            Quota {
                devices: 4,
                devices_max: Some(10),
                daemons: 2,
                daemons_max: Some(3),
                connections: 3,
                connections_max: Some(5),
            }
        );
        assert_eq!(build(&limits, 0, 0, usize::MAX).connections, u32::MAX);
        assert_eq!(device_limits(&limits), DeviceLimits { devices: 10, daemons: 3 });
    }
}
