//! Postgres 实现里 `multi` 模式用到的那几个操作：设备配额下的原子建设备、账号删除、管理员标记、管理页账号搜索。
//!
//! `Store` 里对应的方法（`// ---- multi：…` 那一节）只是转到这里；SQL 放在子模块里，是为了不和 `postgres.rs`
//! 里按表分节的主体交错。并发语义见 `postgres.rs` 开头的表。

use sqlx::Row;

use super::{account_from, be, cursor, PostgresStore};
use crate::store::{
    kind_to_str, like_contains, Account, CreateDevicesOutcome, Device, DeviceLimit, DeviceLimits, NewAudit, NewDevice,
    StoreResult,
};

use super::nul;

impl PostgresStore {
    pub(super) async fn count_devices_impl(&self, account_id: &str) -> StoreResult<(u32, u32)> {
        let row = sqlx::query(
            "SELECT COUNT(*) AS devices, COUNT(*) FILTER (WHERE kind = 'daemon') AS daemons \
             FROM devices WHERE account_id = $1 AND revoked_at_ms IS NULL",
        )
        .bind(account_id)
        .fetch_one(&self.pool)
        .await
        .map_err(be)?;
        let devices: i64 = row.try_get("devices").map_err(be)?;
        let daemons: i64 = row.try_get("daemons").map_err(be)?;
        Ok((devices.max(0) as u32, daemons.max(0) as u32))
    }

    /// 先锁住涉及的账号行（`FOR NO KEY UPDATE`，按 id 排序，一批设备跨几个账号也不会互相死锁），再每台设备一条
    /// `INSERT … SELECT … WHERE <没超上限>`。锁让同一个账号的并发事务排队：后到的在拿到锁之后才数，数到的是前一个已提交的结果；
    /// 同一个事务里前面插入的设备对后面的检查可见，一批设备整体不超。
    ///
    /// `NO KEY UPDATE` 而不是 `UPDATE`：它不挡别的事务往 `devices` 里插行时对账号行做的外键检查（`FOR KEY SHARE`），
    /// 只和同样要改账号行的事务（建设备、删账号、改名……）冲突。
    pub(super) async fn create_devices_within_limits_tx(
        &self,
        devices: &[NewDevice],
        limits: DeviceLimits,
        now_ms: u64,
    ) -> Result<CreateDevicesOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let mut accounts: Vec<String> = devices.iter().map(|d| d.account_id.clone()).collect();
        accounts.sort();
        accounts.dedup();
        sqlx::query("SELECT id FROM accounts WHERE id = ANY($1) ORDER BY id COLLATE \"C\" FOR NO KEY UPDATE")
            .bind(&accounts)
            .fetch_all(&mut *tx)
            .await?;
        let mut created = Vec::with_capacity(devices.len());
        for d in devices {
            let inserted = sqlx::query(
                "INSERT INTO devices (id, account_id, kind, name, platform, origin, token_hash, created_at_ms) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8 \
                 WHERE (SELECT COUNT(*) FROM devices WHERE account_id = $2 AND revoked_at_ms IS NULL) < $9 \
                   AND ($3 <> 'daemon' OR \
                        (SELECT COUNT(*) FROM devices \
                         WHERE account_id = $2 AND revoked_at_ms IS NULL AND kind = 'daemon') < $10)",
            )
            .bind(&d.id)
            .bind(&d.account_id)
            .bind(kind_to_str(d.kind))
            .bind(nul(&d.name))
            .bind(nul(&d.platform))
            .bind(super::nul_opt(d.origin.as_deref()))
            .bind(&d.token_hash)
            .bind(now_ms as i64)
            .bind(i64::from(limits.devices))
            .bind(i64::from(limits.daemons))
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if inserted == 0 {
                // 还握着账号行的锁，数出来的数字和刚才的判断一致：设备总数先满，就报设备总数。
                let total: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE account_id = $1 AND revoked_at_ms IS NULL")
                        .bind(&d.account_id)
                        .fetch_one(&mut *tx)
                        .await?;
                tx.rollback().await?;
                let hit = if total >= i64::from(limits.devices) { DeviceLimit::Devices } else { DeviceLimit::Daemons };
                return Ok(CreateDevicesOutcome::Exceeded(hit));
            }
            created.push(Device {
                id: d.id.clone(),
                account_id: d.account_id.clone(),
                kind: d.kind,
                name: nul(&d.name).into_owned(),
                platform: nul(&d.platform).into_owned(),
                origin: super::nul_opt(d.origin.as_deref()).map(std::borrow::Cow::into_owned),
                created_at_ms: now_ms,
                last_seen_ms: None,
                revoked_at_ms: None,
            });
        }
        tx.commit().await?;
        Ok(CreateDevicesOutcome::Created(created))
    }

    pub(super) async fn set_account_admin_impl(&self, account_id: &str, is_admin: bool) -> StoreResult<bool> {
        let r = sqlx::query("UPDATE accounts SET is_admin = $1 WHERE id = $2")
            .bind(is_admin)
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    /// 管理页搜索：账号 id 精确相等，或显示名 / 邮箱包含 `query`（`LIKE … ESCAPE`，通配符已转义），
    /// 和 `list_accounts` 一样按 `(created_at_ms, id)` 的 keyset 分页。过滤和分页都在 SQL 里，不把账号表读进内存。
    ///
    /// 规则和 SQLite 版一致：**只有 ASCII 字母不区分大小写**。SQLite 的 `lower()` 只折叠 ASCII；Postgres 的 `lower()` 随数据库的
    /// locale 连非 ASCII 字符一起折叠，所以这里用 `translate()` 只把 `A`–`Z` 换成 `a`–`z`，查询串在这边用
    /// `to_ascii_lowercase` 做同样的折叠。非 ASCII 的显示名（`É`、`Ü`）要大小写完全一致才命中。
    pub(super) async fn search_accounts_impl(
        &self,
        query: &str,
        limit: u32,
        after: Option<(u64, String)>,
    ) -> StoreResult<Vec<Account>> {
        let raw = query.trim();
        let pattern = like_contains(&raw.to_ascii_lowercase());
        let (after_ms, after_id) = cursor(after);
        let rows = sqlx::query(
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts \
             WHERE ($1::BIGINT IS NULL OR (created_at_ms, id COLLATE \"C\") > ($1::BIGINT, $2::TEXT)) \
               AND (id = $3 \
                    OR translate(display_name, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') LIKE $4 ESCAPE '\\' \
                    OR translate(email, 'ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz') LIKE $4 ESCAPE '\\') \
             ORDER BY created_at_ms, id COLLATE \"C\" LIMIT $5",
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

    /// 删账号，一个事务。先 `FOR UPDATE` 锁账号行：并发的第二个删除等到第一个提交之后看不到这个账号，返回 `false`，
    /// 不会多写一条墓碑审计。
    pub(super) async fn delete_account_tx(
        &self,
        account_id: &str,
        tombstone: &NewAudit,
        now_ms: u64,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // 邮箱在删账号之前取出来，用来清登录码。账号不存在：回滚，什么都不写。
        let email: Option<Option<String>> = sqlx::query_scalar("SELECT email FROM accounts WHERE id = $1 FOR UPDATE")
            .bind(account_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(email) = email else {
            tx.rollback().await?;
            return Ok(false);
        };
        // hub_audit 没有外键：按账号和它名下设备删。
        sqlx::query(
            "DELETE FROM hub_audit WHERE account_id = $1 OR device_id IN (SELECT id FROM devices WHERE account_id = $1)",
        )
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
        // `invites.used_by` 是 SET NULL：留着的话，被删账号兑换过的邀请会变回「未使用」，码还没过期就能再用一次。
        // （它创建的邀请由 `created_by` 的级联删掉。）
        sqlx::query("DELETE FROM invites WHERE used_by = $1").bind(account_id).execute(&mut *tx).await?;
        // 登录码按邮箱存、不带账号（`account_id` 为空），不会随账号级联；绑定码会。
        if let Some(email) = &email {
            sqlx::query("DELETE FROM email_codes WHERE email = $1").bind(email).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM accounts WHERE id = $1").bind(account_id).execute(&mut *tx).await?;
        let detail = tombstone.detail.as_ref().map(|v| v.to_string());
        sqlx::query(
            "INSERT INTO hub_audit (ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(now_ms as i64)
        .bind(tombstone.event)
        .bind(&tombstone.account_id)
        .bind(&tombstone.device_id)
        .bind(tombstone.ip_prefix.as_deref().map(nul))
        .bind(tombstone.user_agent.as_deref().map(nul))
        .bind(detail)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
