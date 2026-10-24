//! 删账号：`DELETE /api/me`（自己删）和 `DELETE /api/admin/accounts/:id`（管理员删）共用这一条路径。
//!
//! 顺序：先在库里一个事务删干净（[`crate::store::Store::delete_account`]：外键级联删设备、配对、登录 token、浏览器会话、
//! 邮箱码、推送订阅、通知偏好、身份、预览 label 和令牌，以及它建的邀请；另外显式删它的审计行和它兑换过的邀请，
//! 并在同一个事务里写下唯一留下的 `account_deleted` 审计行），再清内存状态：中止进行中的配对 / 接入、
//! 以 `4004 revoked` 关掉它的全部连接（永久连接、带它登录 cookie 的临时连接、预览连接）。
//!
//! 先删库再关连接：关连接和删库之间设备想重连的话，token 已经查不到了。剩下的极小窗口是「认证已经通过、还没登记」
//! 的那一条连接，它会在注册表里多留到自己断开——设备记录已经没了，它发不出任何有意义的东西。
//!
//! 日志里只记是谁发起的、规模（设备数、连接数），不记账号 id（管理员删别人时那是别的账号的 id）。

use anycon_protocol::close_code;
use serde_json::json;

use crate::app::AppState;
use crate::store::{NewAudit, StoreResult};
use crate::util::now_ms;

/// 谁发起的删除；写进 `account_deleted` 审计的 `detail.by`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedBy {
    /// 账号主人自己（`DELETE /api/me`）。
    Owner,
    /// 管理员（`DELETE /api/admin/accounts/:id`）。
    Admin,
}

impl DeletedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            DeletedBy::Owner => "self",
            DeletedBy::Admin => "admin",
        }
    }
}

/// 是不是 `single` 的 bootstrap 管理员账号（它的 token 在配置里，删了就没法再登录管理这个 hub）。`multi` 没有。
pub fn is_bootstrap_admin(state: &AppState, account_id: &str) -> bool {
    state.admin.as_ref().is_some_and(|admin| admin.account_id == account_id)
}

/// 删账号。账号不存在返回 `false`（比如两个请求同时删它，输的一方）。
pub async fn delete_account(state: &AppState, account_id: &str, by: DeletedBy) -> StoreResult<bool> {
    // 库里删了就查不到设备了，要靠它们中止内存里的配对 / 接入：先记下来。
    let devices = state.store.list_devices(account_id).await?;
    let tombstone = NewAudit {
        event: "account_deleted",
        account_id: Some(account_id.to_string()),
        detail: Some(json!({ "by": by.as_str() })),
        ..Default::default()
    };
    if !state.store.delete_account(account_id, tombstone, now_ms()).await? {
        return Ok(false);
    }

    for device in &devices {
        state.broker.abort_device(&device.id);
        state.enrolls.abort_device(&device.id);
    }
    let conns = state.registry.conns_of_account(account_id);
    let connections = conns.len();
    for conn in conns {
        // 临时连接的配对 / 接入登记在临时 id 下；设备 id 上面已经处理过，重复中止无害。
        let id = conn.device_id();
        state.broker.abort_device(&id);
        state.enrolls.abort_device(&id);
        // 同 `AppState::revoke_device`：清对端集合（收尾时不再扇出 peer_offline），先移出路由表，再 4004 断开。
        conn.clear_peers();
        state.registry.unregister(&id, conn.id);
        conn.close(close_code::REVOKED, "revoked");
    }
    tracing::info!(by = by.as_str(), devices = devices.len(), connections, "account deleted");
    Ok(true)
}
