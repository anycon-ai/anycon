//! SQLite 实现里 `multi` 模式用到的那几个操作：设备配额下的原子建设备、账号删除、管理员标记、管理页账号搜索。
//!
//! `Store` 里对应的方法（`// ---- multi：…` 那一节）只是转到这里；SQL 放在子模块里，是为了不和 `sqlite.rs`
//! 里按表分节的主体交错。

use sqlx::Row;

use super::{account_from, be, SqliteStore};
use crate::store::{
    kind_to_str, like_contains, Account, CreateDevicesOutcome, Device, DeviceLimit, DeviceLimits, NewAudit, NewDevice,
    StoreResult,
};

impl SqliteStore {
    pub(super) async fn count_devices_impl(&self, account_id: &str) -> StoreResult<(u32, u32)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS devices, COALESCE(SUM(kind = 'daemon'), 0) AS daemons \
             FROM devices WHERE account_id = ? AND revoked_at_ms IS NULL",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(be)?;
        let devices: i64 = row.try_get("devices").map_err(be)?;
        let daemons: i64 = row.try_get("daemons").map_err(be)?;
        Ok((devices.max(0) as u32, daemons.max(0) as u32))
    }

    /// 每台设备一条 `INSERT … SELECT … WHERE <没超上限>`：一条语句里先拿写锁、再数、再插，所以两个并发的事务不会
    /// 都读到「还有空位」。同一个事务里前面插入的设备对后面的检查可见，一批设备整体不超。
    pub(super) async fn create_devices_within_limits_impl(
        &self,
        devices: Vec<NewDevice>,
        limits: DeviceLimits,
        now_ms: u64,
    ) -> StoreResult<CreateDevicesOutcome> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        let mut created = Vec::with_capacity(devices.len());
        for d in devices {
            let inserted = sqlx::query(
                "INSERT INTO devices (id, account_id, kind, name, platform, origin, token_hash, created_at_ms) \
                 SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8 \
                 WHERE (SELECT COUNT(*) FROM devices WHERE account_id = ?2 AND revoked_at_ms IS NULL) < ?9 \
                   AND (?3 <> 'daemon' OR \
                        (SELECT COUNT(*) FROM devices \
                         WHERE account_id = ?2 AND revoked_at_ms IS NULL AND kind = 'daemon') < ?10)",
            )
            .bind(&d.id)
            .bind(&d.account_id)
            .bind(kind_to_str(d.kind))
            .bind(&d.name)
            .bind(&d.platform)
            .bind(&d.origin)
            .bind(&d.token_hash)
            .bind(now_ms as i64)
            .bind(i64::from(limits.devices))
            .bind(i64::from(limits.daemons))
            .execute(&mut *tx)
            .await
            .map_err(be)?
            .rows_affected();
            if inserted == 0 {
                // 还握着写锁，数出来的数字和刚才的判断一致：设备总数先满，就报设备总数。
                let total: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE account_id = ? AND revoked_at_ms IS NULL")
                        .bind(&d.account_id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(be)?;
                tx.rollback().await.map_err(be)?;
                let hit = if total >= i64::from(limits.devices) { DeviceLimit::Devices } else { DeviceLimit::Daemons };
                return Ok(CreateDevicesOutcome::Exceeded(hit));
            }
            created.push(Device {
                id: d.id,
                account_id: d.account_id,
                kind: d.kind,
                name: d.name,
                platform: d.platform,
                origin: d.origin,
                created_at_ms: now_ms,
                last_seen_ms: None,
                revoked_at_ms: None,
            });
        }
        tx.commit().await.map_err(be)?;
        Ok(CreateDevicesOutcome::Created(created))
    }

    pub(super) async fn set_account_admin_impl(&self, account_id: &str, is_admin: bool) -> StoreResult<bool> {
        let r = sqlx::query("UPDATE accounts SET is_admin = ? WHERE id = ?")
            .bind(i64::from(is_admin))
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    /// 管理页搜索：账号 id 精确相等，或显示名 / 邮箱包含 `query`（`LIKE … ESCAPE`，通配符已转义），
    /// 和 `list_accounts` 一样按 `(created_at_ms, id)` 的 keyset 分页。过滤和分页都在 SQL 里，不把账号表读进内存。
    ///
    /// 不区分大小写靠 `lower()`，查询串在这边用同样的规则转小写（`to_ascii_lowercase`）。SQLite 的 `lower()` 只折叠
    /// ASCII：邮箱本来就存小写，不受影响；非 ASCII 的显示名（`É`、`Ü`）要大小写完全一致才命中，管理员搜索可以接受。
    pub(super) async fn search_accounts_impl(
        &self,
        query: &str,
        limit: u32,
        after: Option<(u64, String)>,
    ) -> StoreResult<Vec<Account>> {
        let raw = query.trim();
        let pattern = like_contains(&raw.to_ascii_lowercase());
        let (after_ms, after_id) = match after {
            Some((ms, id)) => (Some(ms as i64), Some(id)),
            None => (None, None),
        };
        let rows = sqlx::query(
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts \
             WHERE (?1 IS NULL OR (created_at_ms, id) > (?1, ?2)) \
               AND (id = ?3 OR lower(display_name) LIKE ?4 ESCAPE '\\' OR lower(email) LIKE ?4 ESCAPE '\\') \
             ORDER BY created_at_ms, id LIMIT ?5",
        )
        .bind(after_ms)
        .bind(after_id)
        .bind(raw)
        .bind(pattern)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(account_from).collect()
    }

    pub(super) async fn delete_account_impl(
        &self,
        account_id: &str,
        tombstone: NewAudit,
        now_ms: u64,
    ) -> StoreResult<bool> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        // hub_audit 没有外键：按账号和它名下设备删。第一条语句就是写，事务一开始就拿写锁（不会先读到旧快照再升级失败）。
        sqlx::query(
            "DELETE FROM hub_audit WHERE account_id = ?1 OR device_id IN (SELECT id FROM devices WHERE account_id = ?1)",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        // 邮箱在删账号之前取出来，用来清登录码。账号不存在：回滚（上面那条也没删到什么），什么都不写。
        let email: Option<Option<String>> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = ?")
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(be)?;
        let Some(email) = email else {
            tx.rollback().await.map_err(be)?;
            return Ok(false);
        };
        // `invites.used_by` 是 SET NULL：留着的话，被删账号兑换过的邀请会变回「未使用」，码还没过期就能再用一次。
        // （它创建的邀请由 `created_by` 的级联删掉。）
        sqlx::query("DELETE FROM invites WHERE used_by = ?").bind(account_id).execute(&mut *tx).await.map_err(be)?;
        // 登录码按邮箱存、不带账号（`account_id` 为空），不会随账号级联；绑定码会。
        if let Some(email) = &email {
            sqlx::query("DELETE FROM email_codes WHERE email = ?").bind(email).execute(&mut *tx).await.map_err(be)?;
        }
        sqlx::query("DELETE FROM accounts WHERE id = ?").bind(account_id).execute(&mut *tx).await.map_err(be)?;
        let detail = tombstone.detail.map(|v| v.to_string());
        sqlx::query(
            "INSERT INTO hub_audit (ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now_ms as i64)
        .bind(tombstone.event)
        .bind(tombstone.account_id)
        .bind(tombstone.device_id)
        .bind(tombstone.ip_prefix)
        .bind(tombstone.user_agent)
        .bind(detail)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(true)
    }
}
