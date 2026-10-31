//! SQLite 实现（sqlx）。运行时查询（不用编译期宏），迁移内嵌自 `migrations/sqlite/`。
//!
//! SQLite 只有一个写者：「先查再写」的逻辑放在写事务里（或一条语句里先拿写锁）就天然串行。Postgres 版（`postgres.rs`）
//! 对同样的操作用行锁 / advisory 锁保证相同的结果，那边的模块文档里有逐项对照。

use std::fmt::Display;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

use crate::util::{new_id, ordered};

mod multi;
mod oauth;

use super::{
    category_from_str, category_to_str, kind_from_str, kind_to_str, Account, AccountToken, AuditEntry, CodeCandidate,
    CreateAccountOutcome, Device, EmailCode, EmailPurpose, Invite, IssueOutcome, NewAudit, NewDevice, NewPreviewToken,
    NotificationPref, PreviewIssueLimits, PreviewToken, PushSubscription, PushSubscriptionRow, RedeemOutcome,
    RotateOutcome, SetEmailOutcome, Store, StoreError, StoreResult, StoreStats, WebSession,
};
use super::{CreateDevicesOutcome, DeviceLimits};
use super::{Identity, IdentityAccountOutcome, LinkIdentityOutcome, NewIdentityAccount, NewOAuthState, OAuthState};

fn be(e: impl Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn u(row: &SqliteRow, col: &str) -> StoreResult<u64> {
    let v: i64 = row.try_get(col).map_err(be)?;
    Ok(v.max(0) as u64)
}

fn u_opt(row: &SqliteRow, col: &str) -> StoreResult<Option<u64>> {
    let v: Option<i64> = row.try_get(col).map_err(be)?;
    Ok(v.map(|v| v.max(0) as u64))
}

fn s(row: &SqliteRow, col: &str) -> StoreResult<String> {
    row.try_get::<String, _>(col).map_err(be)
}

fn s_opt(row: &SqliteRow, col: &str) -> StoreResult<Option<String>> {
    row.try_get::<Option<String>, _>(col).map_err(be)
}

fn account_from(row: &SqliteRow) -> StoreResult<Account> {
    Ok(Account {
        id: s(row, "id")?,
        display_name: s(row, "display_name")?,
        email: s_opt(row, "email")?,
        is_admin: row.try_get::<i64, _>("is_admin").map_err(be)? != 0,
        created_at_ms: u(row, "created_at_ms")?,
    })
}

fn device_from(row: &SqliteRow) -> StoreResult<Device> {
    Ok(Device {
        id: s(row, "id")?,
        account_id: s(row, "account_id")?,
        kind: kind_from_str(&s(row, "kind")?)?,
        name: s(row, "name")?,
        platform: s(row, "platform")?,
        origin: s_opt(row, "origin")?,
        created_at_ms: u(row, "created_at_ms")?,
        last_seen_ms: u_opt(row, "last_seen_ms")?,
        revoked_at_ms: u_opt(row, "revoked_at_ms")?,
    })
}

fn session_from(row: &SqliteRow) -> StoreResult<WebSession> {
    Ok(WebSession {
        id: s(row, "id")?,
        account_id: s(row, "account_id")?,
        user_agent: s(row, "user_agent")?,
        ip_prefix: s(row, "ip_prefix")?,
        created_at_ms: u(row, "created_at_ms")?,
        last_seen_ms: u(row, "last_seen_ms")?,
        expires_at_ms: u(row, "expires_at_ms")?,
    })
}

fn invite_from(row: &SqliteRow) -> StoreResult<Invite> {
    Ok(Invite {
        id: s(row, "id")?,
        created_by: s(row, "created_by")?,
        note: s_opt(row, "note")?,
        created_at_ms: u(row, "created_at_ms")?,
        expires_at_ms: u(row, "expires_at_ms")?,
        used_by: s_opt(row, "used_by")?,
        used_at_ms: u_opt(row, "used_at_ms")?,
    })
}

// sqlx 0.9 只接受 `&'static str` 的 SQL，列清单用宏拼成字面量。
macro_rules! device_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, account_id, kind, name, platform, origin, created_at_ms, last_seen_ms, revoked_at_ms FROM devices ",
            $tail
        )
    };
}

macro_rules! session_select {
    ($tail:literal) => {
        concat!(
            "SELECT id, account_id, user_agent, ip_prefix, created_at_ms, last_seen_ms, expires_at_ms FROM web_sessions ",
            $tail
        )
    };
}

macro_rules! preview_token_select {
    ($tail:literal) => {
        concat!(
            "SELECT token_hash, label, node_id, account_id, device_id, created_at_ms, expires_at_ms FROM preview_tokens ",
            $tail
        )
    };
}

fn preview_token_from(row: &SqliteRow) -> StoreResult<PreviewToken> {
    Ok(PreviewToken {
        token_hash: s(row, "token_hash")?,
        label: s(row, "label")?,
        node_id: s(row, "node_id")?,
        account_id: s(row, "account_id")?,
        device_id: s(row, "device_id")?,
        created_at_ms: u(row, "created_at_ms")?,
        expires_at_ms: u(row, "expires_at_ms")?,
    })
}

#[derive(Clone)]
pub struct SqliteStore {
    pub(super) pool: SqlitePool,
}

impl SqliteStore {
    /// 打开（必要时创建）数据库并跑完迁移。
    ///
    /// `database_url`：`None` 用 `<data_dir>/hub.db`；否则是 `sqlite://路径` / `sqlite:路径`。
    pub async fn open(database_url: Option<&str>, data_dir: &Path) -> StoreResult<Self> {
        let options = match database_url {
            None => SqliteConnectOptions::new().filename(data_dir.join("hub.db")),
            Some(url) => {
                // Postgres 的 URL 由 [`super::open`] 分流到 `PostgresStore`，到这里的只能是 SQLite。
                if !url.to_ascii_lowercase().starts_with("sqlite:") {
                    return Err(StoreError::Backend(
                        "ANYCON_DATABASE_URL: expected sqlite:///path/to/hub.db or postgres://user:password@host/db"
                            .into(),
                    ));
                }
                SqliteConnectOptions::from_str(url).map_err(be)?
            }
        };
        if let Some(parent) = options.get_filename().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| StoreError::Backend(format!("cannot create {}: {e}", parent.display())))?;
            }
        }
        let options = options
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await.map_err(be)?;
        sqlx::migrate!("./migrations/sqlite").run(&pool).await.map_err(|e| StoreError::Migrate(e.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn ping(&self) -> StoreResult<()> {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await.map_err(be)?;
        Ok(())
    }

    async fn stats(&self) -> StoreResult<StoreStats> {
        let accounts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM accounts").fetch_one(&self.pool).await.map_err(be)?;
        let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE revoked_at_ms IS NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(be)?;
        let size: i64 =
            sqlx::query_scalar("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()")
                .fetch_one(&self.pool)
                .await
                .map_err(be)?;
        Ok(StoreStats {
            accounts: accounts.max(0) as u64,
            devices: devices.max(0) as u64,
            db_size_bytes: size.max(0) as u64,
        })
    }

    // ---- 账号 ----

    async fn create_account(&self, display_name: &str, is_admin: bool, now_ms: u64) -> StoreResult<Account> {
        let id = new_id();
        sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(display_name)
            .bind(i64::from(is_admin))
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(Account { id, display_name: display_name.into(), email: None, is_admin, created_at_ms: now_ms })
    }

    async fn create_account_and_record_id(
        &self,
        display_name: &str,
        is_admin: bool,
        kv_key: &str,
        now_ms: u64,
    ) -> StoreResult<Account> {
        let id = new_id();
        let mut tx = self.pool.begin().await.map_err(be)?;
        sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(display_name)
            .bind(i64::from(is_admin))
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        sqlx::query("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(kv_key)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(Account { id, display_name: display_name.into(), email: None, is_admin, created_at_ms: now_ms })
    }

    async fn create_account_with_email(
        &self,
        display_name: &str,
        email: Option<&str>,
        now_ms: u64,
    ) -> StoreResult<CreateAccountOutcome> {
        let id = new_id();
        let r = sqlx::query(
            "INSERT INTO accounts (id, display_name, email, is_admin, created_at_ms) VALUES (?, ?, ?, 0, ?)",
        )
        .bind(&id)
        .bind(display_name)
        .bind(email)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await;
        match r {
            Ok(_) => Ok(CreateAccountOutcome::Created(Account {
                id,
                display_name: display_name.into(),
                email: email.map(str::to_string),
                is_admin: false,
                created_at_ms: now_ms,
            })),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(CreateAccountOutcome::EmailTaken),
            Err(e) => Err(be(e)),
        }
    }

    async fn get_account(&self, id: &str) -> StoreResult<Option<Account>> {
        let row = sqlx::query("SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn list_accounts(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Account>> {
        let (after_ms, after_id) = match after {
            Some((ms, id)) => (Some(ms as i64), Some(id)),
            None => (None, None),
        };
        let rows = sqlx::query(
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts \
             WHERE (?1 IS NULL OR (created_at_ms, id) > (?1, ?2)) ORDER BY created_at_ms, id LIMIT ?3",
        )
        .bind(after_ms)
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(account_from).collect()
    }

    async fn first_admin(&self) -> StoreResult<Option<Account>> {
        let row = sqlx::query(
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE is_admin = 1 ORDER BY created_at_ms, id LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn update_display_name(&self, id: &str, display_name: &str) -> StoreResult<()> {
        sqlx::query("UPDATE accounts SET display_name = ? WHERE id = ?")
            .bind(display_name)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    // ---- 邮箱 ----

    async fn find_account_by_email(&self, email: &str) -> StoreResult<Option<Account>> {
        let row = sqlx::query("SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE email = ?")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn set_account_email(&self, account_id: &str, email: Option<&str>) -> StoreResult<SetEmailOutcome> {
        let r = sqlx::query("UPDATE accounts SET email = ? WHERE id = ?")
            .bind(email)
            .bind(account_id)
            .execute(&self.pool)
            .await;
        match r {
            Ok(_) => Ok(SetEmailOutcome::Done),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(SetEmailOutcome::Taken),
            Err(e) => Err(be(e)),
        }
    }

    async fn put_email_code(&self, c: EmailCode, keep: u32) -> StoreResult<()> {
        // 一个事务：清过期的 → 插入 → 只留这一组里最新的 `keep` 条。第一条语句就是写，一开始就拿写锁，不会在中途升级失败。
        let mut tx = self.pool.begin().await.map_err(be)?;
        sqlx::query(
            "DELETE FROM email_codes WHERE email = ?1 AND purpose = ?2 AND account_id IS ?3 AND expires_at_ms <= ?4",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(c.created_at_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        sqlx::query(
            "INSERT INTO email_codes (email, purpose, account_id, code_hash, expires_at_ms, attempts, created_at_ms) \
             VALUES (?, ?, ?, ?, ?, 0, ?)",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(&c.code_hash)
        .bind(c.expires_at_ms as i64)
        .bind(c.created_at_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        sqlx::query(
            "DELETE FROM email_codes WHERE email = ?1 AND purpose = ?2 AND account_id IS ?3 AND id NOT IN \
             (SELECT id FROM email_codes WHERE email = ?1 AND purpose = ?2 AND account_id IS ?3 ORDER BY id DESC LIMIT ?4)",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(i64::from(keep.max(1)))
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(())
    }

    async fn list_email_codes(&self, email: &str, purpose: EmailPurpose) -> StoreResult<Vec<EmailCode>> {
        let rows = sqlx::query(
            "SELECT email, account_id, code_hash, expires_at_ms, attempts, created_at_ms FROM email_codes \
             WHERE email = ? AND purpose = ? ORDER BY id",
        )
        .bind(email)
        .bind(purpose.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter()
            .map(|r| {
                Ok(EmailCode {
                    email: s(r, "email")?,
                    purpose,
                    account_id: s_opt(r, "account_id")?,
                    code_hash: s(r, "code_hash")?,
                    expires_at_ms: u(r, "expires_at_ms")?,
                    attempts: u(r, "attempts")?.min(u64::from(u32::MAX)) as u32,
                    created_at_ms: u(r, "created_at_ms")?,
                })
            })
            .collect()
    }

    async fn attempt_email_codes(
        &self,
        email: &str,
        purpose: EmailPurpose,
        account_id: Option<&str>,
        now_ms: u64,
        max_attempts: u32,
    ) -> StoreResult<Vec<CodeCandidate>> {
        // 顺手清掉这一组里已经过期的（它们本来就交不出来了，只是不让它们占着行）。次数用完的不在这里删：
        // 这次提交刚好把某条码计到第 `max` 次时它还要参加比较，而且别的并发提交在这里删掉它，会让它的认领落空；
        // 次数用完的码由调用方在比较失败之后按 id 删（`attempts` 随行交回），剩下的到期被清理。
        sqlx::query(
            "DELETE FROM email_codes WHERE email = ?1 AND purpose = ?2 AND account_id IS ?3 AND expires_at_ms <= ?4",
        )
        .bind(email)
        .bind(purpose.as_str())
        .bind(account_id)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        // 计数和取码是同一条语句：SQLite 的写入串行，并发的提交各自拿到的是「计过数之后」还没用完次数的那几行，
        // 一条码一共只会被交出去 `max_attempts` 次（`attempts < max` 才加一：第 `max` 次提交仍然能比较，第 `max+1` 次起拿不到）。
        let rows = sqlx::query(
            "UPDATE email_codes SET attempts = attempts + 1 \
             WHERE email = ?1 AND purpose = ?2 AND account_id IS ?3 AND expires_at_ms > ?4 AND attempts < ?5 \
             RETURNING id, code_hash, attempts",
        )
        .bind(email)
        .bind(purpose.as_str())
        .bind(account_id)
        .bind(now_ms as i64)
        .bind(i64::from(max_attempts))
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter()
            .map(|r| {
                Ok(CodeCandidate {
                    id: r.try_get("id").map_err(be)?,
                    code_hash: s(r, "code_hash")?,
                    attempts: u(r, "attempts")?.min(u64::from(u32::MAX)) as u32,
                })
            })
            .collect()
    }

    async fn take_email_code(&self, id: i64) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM email_codes WHERE id = ?").bind(id).execute(&self.pool).await.map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn purge_expired_email_codes(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM email_codes WHERE expires_at_ms <= ?")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    // ---- Web Push 订阅 ----

    async fn upsert_push_subscription(
        &self,
        device_id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        now_ms: u64,
    ) -> StoreResult<PushSubscription> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO push_subscriptions (id, device_id, endpoint, p256dh, auth, created_at_ms) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(device_id) DO UPDATE SET id = excluded.id, endpoint = excluded.endpoint, p256dh = excluded.p256dh, \
                 auth = excluded.auth, created_at_ms = excluded.created_at_ms",
        )
        .bind(&id)
        .bind(device_id)
        .bind(endpoint)
        .bind(p256dh)
        .bind(auth)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(PushSubscription {
            id,
            device_id: device_id.into(),
            endpoint: endpoint.into(),
            p256dh: p256dh.into(),
            auth: auth.into(),
            created_at_ms: now_ms,
        })
    }

    async fn list_push_subscriptions(&self, account_id: &str) -> StoreResult<Vec<PushSubscriptionRow>> {
        let rows = sqlx::query(
            "SELECT p.id, p.device_id, p.endpoint, p.created_at_ms, d.name AS device_name \
             FROM push_subscriptions p JOIN devices d ON d.id = p.device_id \
             WHERE d.account_id = ? AND d.revoked_at_ms IS NULL ORDER BY p.created_at_ms, p.id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter()
            .map(|r| {
                Ok(PushSubscriptionRow {
                    id: s(r, "id")?,
                    device_id: s(r, "device_id")?,
                    device_name: s(r, "device_name")?,
                    endpoint: s(r, "endpoint")?,
                    created_at_ms: u(r, "created_at_ms")?,
                })
            })
            .collect()
    }

    async fn get_push_subscription_of_device(&self, device_id: &str) -> StoreResult<Option<PushSubscription>> {
        let row = sqlx::query(
            "SELECT id, device_id, endpoint, p256dh, auth, created_at_ms FROM push_subscriptions WHERE device_id = ?",
        )
        .bind(device_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.map(|r| {
            Ok(PushSubscription {
                id: s(&r, "id")?,
                device_id: s(&r, "device_id")?,
                endpoint: s(&r, "endpoint")?,
                p256dh: s(&r, "p256dh")?,
                auth: s(&r, "auth")?,
                created_at_ms: u(&r, "created_at_ms")?,
            })
        })
        .transpose()
    }

    async fn delete_push_subscription(&self, account_id: &str, id: &str) -> StoreResult<bool> {
        let r = sqlx::query(
            "DELETE FROM push_subscriptions WHERE id = ? AND device_id IN (SELECT id FROM devices WHERE account_id = ?)",
        )
        .bind(id)
        .bind(account_id)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_push_subscription_by_id(&self, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM push_subscriptions WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    // ---- 通知偏好 ----

    async fn get_notification_prefs(&self, account_id: &str) -> StoreResult<Vec<NotificationPref>> {
        let rows = sqlx::query("SELECT category, push, email FROM notification_prefs WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            // 库里有不认识的类别（比如以后版本写的）就跳过，不报错。
            let Some(category) = category_from_str(&s(r, "category")?) else { continue };
            out.push(NotificationPref {
                category,
                push: r.try_get::<i64, _>("push").map_err(be)? != 0,
                email: r.try_get::<i64, _>("email").map_err(be)? != 0,
            });
        }
        Ok(out)
    }

    async fn set_notification_prefs(&self, account_id: &str, prefs: &[NotificationPref]) -> StoreResult<()> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        for p in prefs {
            sqlx::query(
                "INSERT INTO notification_prefs (account_id, category, push, email) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(account_id, category) DO UPDATE SET push = excluded.push, email = excluded.email",
            )
            .bind(account_id)
            .bind(category_to_str(p.category))
            .bind(i64::from(p.push))
            .bind(i64::from(p.email))
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        }
        tx.commit().await.map_err(be)?;
        Ok(())
    }

    // ---- 登录 token ----

    async fn create_account_token(
        &self,
        account_id: &str,
        label: &str,
        token_hash: &str,
        now_ms: u64,
    ) -> StoreResult<AccountToken> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO account_tokens (id, account_id, token_hash, label, created_at_ms) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(account_id)
        .bind(token_hash)
        .bind(label)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(AccountToken { id, label: label.into(), created_at_ms: now_ms, last_used_ms: None })
    }

    async fn list_account_tokens(&self, account_id: &str) -> StoreResult<Vec<AccountToken>> {
        let rows = sqlx::query(
            "SELECT id, label, created_at_ms, last_used_ms FROM account_tokens WHERE account_id = ? ORDER BY created_at_ms, id",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter()
            .map(|r| {
                Ok(AccountToken {
                    id: s(r, "id")?,
                    label: s(r, "label")?,
                    created_at_ms: u(r, "created_at_ms")?,
                    last_used_ms: u_opt(r, "last_used_ms")?,
                })
            })
            .collect()
    }

    async fn delete_account_token(&self, account_id: &str, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM account_tokens WHERE id = ? AND account_id = ?")
            .bind(id)
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn find_account_by_token_hash(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<Account>> {
        let row = sqlx::query(
            "SELECT a.id, a.display_name, a.email, a.is_admin, a.created_at_ms, t.id AS token_id \
             FROM account_tokens t JOIN accounts a ON a.id = t.account_id WHERE t.token_hash = ?",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        let Some(row) = row else { return Ok(None) };
        let token_id = s(&row, "token_id")?;
        sqlx::query("UPDATE account_tokens SET last_used_ms = ? WHERE id = ?")
            .bind(now_ms as i64)
            .bind(token_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(Some(account_from(&row)?))
    }

    // ---- 浏览器会话 ----

    async fn create_web_session(
        &self,
        account_id: &str,
        token_hash: &str,
        user_agent: &str,
        ip_prefix: &str,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> StoreResult<WebSession> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO web_sessions (id, account_id, token_hash, user_agent, ip_prefix, created_at_ms, last_seen_ms, expires_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(account_id)
        .bind(token_hash)
        .bind(user_agent)
        .bind(ip_prefix)
        .bind(now_ms as i64)
        .bind(now_ms as i64)
        .bind(expires_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(WebSession {
            id,
            account_id: account_id.into(),
            user_agent: user_agent.into(),
            ip_prefix: ip_prefix.into(),
            created_at_ms: now_ms,
            last_seen_ms: now_ms,
            expires_at_ms,
        })
    }

    async fn find_web_session(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<(WebSession, Account)>> {
        let row = sqlx::query(
            "SELECT s.id, s.account_id, s.user_agent, s.ip_prefix, s.created_at_ms AS created_at_ms, s.last_seen_ms, s.expires_at_ms, \
                    a.display_name, a.email, a.is_admin, a.created_at_ms AS account_created_at_ms \
             FROM web_sessions s JOIN accounts a ON a.id = s.account_id \
             WHERE s.token_hash = ? AND s.expires_at_ms > ?",
        )
        .bind(token_hash)
        .bind(now_ms as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        let Some(row) = row else { return Ok(None) };
        let session = session_from(&row)?;
        let account = Account {
            id: session.account_id.clone(),
            display_name: s(&row, "display_name")?,
            email: s_opt(&row, "email")?,
            is_admin: row.try_get::<i64, _>("is_admin").map_err(be)? != 0,
            created_at_ms: u(&row, "account_created_at_ms")?,
        };
        Ok(Some((session, account)))
    }

    async fn touch_web_session(&self, id: &str, now_ms: u64, expires_at_ms: u64) -> StoreResult<()> {
        sqlx::query("UPDATE web_sessions SET last_seen_ms = ?, expires_at_ms = ? WHERE id = ?")
            .bind(now_ms as i64)
            .bind(expires_at_ms as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn list_web_sessions(&self, account_id: &str, now_ms: u64) -> StoreResult<Vec<WebSession>> {
        let rows =
            sqlx::query(session_select!("WHERE account_id = ? AND expires_at_ms > ? ORDER BY last_seen_ms DESC, id"))
                .bind(account_id)
                .bind(now_ms as i64)
                .fetch_all(&self.pool)
                .await
                .map_err(be)?;
        rows.iter().map(session_from).collect()
    }

    async fn delete_web_session(&self, account_id: &str, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE id = ? AND account_id = ?")
            .bind(id)
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_web_session_by_hash(&self, token_hash: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_web_sessions_of_account(&self, account_id: &str) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE account_id = ?")
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    async fn purge_expired_web_sessions(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE expires_at_ms <= ?")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    // ---- OAuth 身份与授权 state（实现在 oauth.rs） ----

    async fn find_identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>> {
        self.oauth_find_identity(provider, subject).await
    }

    async fn list_identities(&self, account_id: &str) -> StoreResult<Vec<Identity>> {
        self.oauth_list_identities(account_id).await
    }

    async fn link_identity(
        &self,
        account_id: &str,
        provider: &str,
        subject: &str,
        now_ms: u64,
    ) -> StoreResult<LinkIdentityOutcome> {
        self.oauth_link_identity(account_id, provider, subject, now_ms).await
    }

    async fn unlink_identity(&self, account_id: &str, provider: &str) -> StoreResult<bool> {
        self.oauth_unlink_identity(account_id, provider).await
    }

    async fn create_account_with_identity(
        &self,
        new: NewIdentityAccount<'_>,
        now_ms: u64,
    ) -> StoreResult<IdentityAccountOutcome> {
        self.oauth_create_account_with_identity(new, now_ms).await
    }

    async fn put_oauth_state(&self, state: NewOAuthState, now_ms: u64) -> StoreResult<()> {
        self.oauth_put_state(state, now_ms).await
    }

    async fn take_oauth_state(&self, state_hash: &str, now_ms: u64) -> StoreResult<Option<OAuthState>> {
        self.oauth_take_state(state_hash, now_ms).await
    }

    async fn purge_expired_oauth_states(&self, now_ms: u64) -> StoreResult<u64> {
        self.oauth_purge_expired_states(now_ms).await
    }

    // ---- 邀请 ----

    async fn create_invite(
        &self,
        created_by: &str,
        code_hash: &str,
        note: Option<&str>,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> StoreResult<Invite> {
        let id = new_id();
        sqlx::query(
            "INSERT INTO invites (id, code_hash, created_by, note, created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(code_hash)
        .bind(created_by)
        .bind(note)
        .bind(now_ms as i64)
        .bind(expires_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(Invite {
            id,
            created_by: created_by.into(),
            note: note.map(Into::into),
            created_at_ms: now_ms,
            expires_at_ms,
            used_by: None,
            used_at_ms: None,
        })
    }

    async fn list_invites(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Invite>> {
        // keyset 分页：`(created_at_ms, id)` 严格小于上一页最后一项。没有游标时 `?1 IS NULL` 让条件恒真。
        let (after_ms, after_id) = match after {
            Some((ms, id)) => (Some(ms as i64), Some(id)),
            None => (None, None),
        };
        let rows = sqlx::query(
            "SELECT id, created_by, note, created_at_ms, expires_at_ms, used_by, used_at_ms FROM invites \
             WHERE (?1 IS NULL OR (created_at_ms, id) < (?1, ?2)) ORDER BY created_at_ms DESC, id DESC LIMIT ?3",
        )
        .bind(after_ms)
        .bind(after_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(invite_from).collect()
    }

    async fn delete_invite(&self, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM invites WHERE id = ?").bind(id).execute(&self.pool).await.map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn redeem_invite(&self, code_hash: &str, display_name: &str, now_ms: u64) -> StoreResult<RedeemOutcome> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        let account_id = new_id();
        sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES (?, ?, 0, ?)")
            .bind(&account_id)
            .bind(display_name)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        // 单条 UPDATE 同时检查「未用 + 未过期」，保证邀请只能被兑换一次。
        let r = sqlx::query(
            "UPDATE invites SET used_by = ?, used_at_ms = ? WHERE code_hash = ? AND used_by IS NULL AND expires_at_ms > ?",
        )
        .bind(&account_id)
        .bind(now_ms as i64)
        .bind(code_hash)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        if r.rows_affected() == 1 {
            tx.commit().await.map_err(be)?;
            return Ok(RedeemOutcome::Redeemed(Account {
                id: account_id,
                display_name: display_name.into(),
                email: None,
                is_admin: false,
                created_at_ms: now_ms,
            }));
        }
        tx.rollback().await.map_err(be)?;
        let row = sqlx::query("SELECT used_by, expires_at_ms FROM invites WHERE code_hash = ?")
            .bind(code_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        Ok(match row {
            None => RedeemOutcome::NotFound,
            Some(r) if s_opt(&r, "used_by")?.is_some() => RedeemOutcome::Used,
            Some(_) => RedeemOutcome::Expired,
        })
    }

    // ---- 设备 ----

    async fn create_device(&self, d: NewDevice, now_ms: u64) -> StoreResult<Device> {
        sqlx::query(
            "INSERT INTO devices (id, account_id, kind, name, platform, origin, token_hash, created_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&d.id)
        .bind(&d.account_id)
        .bind(kind_to_str(d.kind))
        .bind(&d.name)
        .bind(&d.platform)
        .bind(&d.origin)
        .bind(&d.token_hash)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(Device {
            id: d.id,
            account_id: d.account_id,
            kind: d.kind,
            name: d.name,
            platform: d.platform,
            origin: d.origin,
            created_at_ms: now_ms,
            last_seen_ms: None,
            revoked_at_ms: None,
        })
    }

    async fn get_device(&self, id: &str) -> StoreResult<Option<Device>> {
        let row = sqlx::query(device_select!("WHERE id = ?")).bind(id).fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(device_from).transpose()
    }

    async fn find_device_by_token_hash(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<Device>> {
        let row = sqlx::query(device_select!(
            "WHERE token_hash = ? OR (prev_token_hash = ? AND prev_token_expires_at_ms > ?) LIMIT 1"
        ))
        .bind(token_hash)
        .bind(token_hash)
        .bind(now_ms as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.as_ref().map(device_from).transpose()
    }

    async fn rotate_device_token(
        &self,
        id: &str,
        new_token_hash: &str,
        now_ms: u64,
        grace_ms: u64,
        min_interval_ms: u64,
    ) -> StoreResult<RotateOutcome> {
        // 一条 UPDATE 同时做检查（没吊销、距上次轮换够久）和写入；SQL 里等号右边引用的是更新前的值，
        // 所以 `prev_token_hash = token_hash` 取到的是原来的当前 token。
        let r = sqlx::query(
            "UPDATE devices SET prev_token_hash = token_hash, prev_token_expires_at_ms = ?, token_hash = ?, \
             token_rotated_at_ms = ? \
             WHERE id = ? AND revoked_at_ms IS NULL AND (token_rotated_at_ms IS NULL OR token_rotated_at_ms <= ?)",
        )
        .bind(now_ms.saturating_add(grace_ms) as i64)
        .bind(new_token_hash)
        .bind(now_ms as i64)
        .bind(id)
        .bind(now_ms.saturating_sub(min_interval_ms) as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        if r.rows_affected() > 0 {
            return Ok(RotateOutcome::Rotated);
        }
        // 没改到：区分「设备不在 / 已吊销」和「刚轮换过」
        let alive: Option<i64> = sqlx::query_scalar("SELECT 1 FROM devices WHERE id = ? AND revoked_at_ms IS NULL")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        Ok(if alive.is_some() { RotateOutcome::TooSoon } else { RotateOutcome::Unavailable })
    }

    async fn list_devices(&self, account_id: &str) -> StoreResult<Vec<Device>> {
        let rows =
            sqlx::query(device_select!("WHERE account_id = ? AND revoked_at_ms IS NULL ORDER BY created_at_ms, id"))
                .bind(account_id)
                .fetch_all(&self.pool)
                .await
                .map_err(be)?;
        rows.iter().map(device_from).collect()
    }

    async fn rename_device(&self, id: &str, name: &str) -> StoreResult<()> {
        sqlx::query("UPDATE devices SET name = ? WHERE id = ?")
            .bind(name)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn touch_device_seen(&self, id: &str, now_ms: u64) -> StoreResult<()> {
        sqlx::query("UPDATE devices SET last_seen_ms = ? WHERE id = ?")
            .bind(now_ms as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn revoke_device(&self, id: &str, now_ms: u64) -> StoreResult<Vec<String>> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        let r = sqlx::query("UPDATE devices SET revoked_at_ms = ? WHERE id = ? AND revoked_at_ms IS NULL")
            .bind(now_ms as i64)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        if r.rows_affected() == 0 {
            tx.rollback().await.map_err(be)?;
            return Ok(Vec::new());
        }
        let peers: Vec<String> = sqlx::query_scalar(
            "SELECT device_b FROM pairings WHERE device_a = ? UNION SELECT device_a FROM pairings WHERE device_b = ?",
        )
        .bind(id)
        .bind(id)
        .fetch_all(&mut *tx)
        .await
        .map_err(be)?;
        sqlx::query("DELETE FROM pairings WHERE device_a = ? OR device_b = ?")
            .bind(id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        // 被吊销的设备不再收推送（ADR-37）。
        sqlx::query("DELETE FROM push_subscriptions WHERE device_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        // 预览（ADR-39）：它签发的令牌作废；它是节点的话，全部 label 和用到这些 label 的令牌也删。
        sqlx::query("DELETE FROM preview_tokens WHERE device_id = ? OR node_id = ?")
            .bind(id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        sqlx::query("DELETE FROM preview_labels WHERE node_id = ?").bind(id).execute(&mut *tx).await.map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(peers)
    }

    // ---- 配对 ----

    async fn insert_pairing(&self, a: &str, b: &str, now_ms: u64) -> StoreResult<()> {
        let (a, b) = ordered(a, b);
        sqlx::query("INSERT OR IGNORE INTO pairings (device_a, device_b, created_at_ms) VALUES (?, ?, ?)")
            .bind(a)
            .bind(b)
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn peers_of(&self, device_id: &str) -> StoreResult<Vec<String>> {
        sqlx::query_scalar(
            "SELECT device_b FROM pairings WHERE device_a = ? UNION SELECT device_a FROM pairings WHERE device_b = ?",
        )
        .bind(device_id)
        .bind(device_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)
    }

    async fn is_paired(&self, a: &str, b: &str) -> StoreResult<bool> {
        let (a, b) = ordered(a, b);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairings WHERE device_a = ? AND device_b = ?")
            .bind(a)
            .bind(b)
            .fetch_one(&self.pool)
            .await
            .map_err(be)?;
        Ok(n > 0)
    }

    async fn list_pairings_of_account(&self, account_id: &str) -> StoreResult<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT p.device_a, p.device_b FROM pairings p JOIN devices d ON d.id = p.device_a WHERE d.account_id = ?",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(|r| Ok((s(r, "device_a")?, s(r, "device_b")?))).collect()
    }

    // ---- 审计 ----

    async fn insert_audit(&self, e: NewAudit, now_ms: u64) -> StoreResult<()> {
        let detail = e.detail.map(|v| v.to_string());
        sqlx::query(
            "INSERT INTO hub_audit (ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now_ms as i64)
        .bind(e.event)
        .bind(e.account_id)
        .bind(e.device_id)
        .bind(e.ip_prefix)
        .bind(e.user_agent)
        .bind(detail)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(())
    }

    async fn list_audit(&self, limit: u32, before_id: Option<i64>) -> StoreResult<Vec<AuditEntry>> {
        let rows = sqlx::query(
            "SELECT id, ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail FROM hub_audit \
             WHERE (? IS NULL OR id < ?) ORDER BY id DESC LIMIT ?",
        )
        .bind(before_id)
        .bind(before_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter()
            .map(|r| {
                let detail = s_opt(r, "detail")?.and_then(|d| serde_json::from_str(&d).ok());
                Ok(AuditEntry {
                    id: r.try_get("id").map_err(be)?,
                    ts_ms: u(r, "ts_ms")?,
                    event: s(r, "event")?,
                    account_id: s_opt(r, "account_id")?,
                    device_id: s_opt(r, "device_id")?,
                    ip_prefix: s_opt(r, "ip_prefix")?,
                    user_agent: s_opt(r, "user_agent")?,
                    detail,
                })
            })
            .collect()
    }

    async fn purge_audit_before(&self, ts_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM hub_audit WHERE ts_ms < ?")
            .bind(ts_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    // ---- 预览 ----

    async fn issue_preview_token(
        &self,
        token: NewPreviewToken,
        now_ms: u64,
        limits: PreviewIssueLimits,
    ) -> StoreResult<IssueOutcome> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        // 第一条语句就是写：事务一开始就拿写锁，并发的两个申请不会各自读到旧快照再互相冲突。
        // label 已存在就不动；是新的，而且节点还没绑满，才插入。
        sqlx::query(
            "INSERT INTO preview_labels (label, node_id, account_id, created_at_ms) \
             SELECT ?, ?, ?, ? WHERE (SELECT COUNT(*) FROM preview_labels WHERE node_id = ?) < ? \
             ON CONFLICT(label) DO NOTHING",
        )
        .bind(&token.label)
        .bind(&token.node_id)
        .bind(&token.account_id)
        .bind(now_ms as i64)
        .bind(&token.node_id)
        .bind(i64::from(limits.labels_per_node))
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        let bound: Option<String> = sqlx::query_scalar("SELECT node_id FROM preview_labels WHERE label = ?")
            .bind(&token.label)
            .fetch_optional(&mut *tx)
            .await
            .map_err(be)?;
        match bound {
            None => {
                tx.rollback().await.map_err(be)?;
                return Ok(IssueOutcome::TooManyLabels);
            }
            Some(node) if node != token.node_id => {
                tx.rollback().await.map_err(be)?;
                return Ok(IssueOutcome::LabelTaken);
            }
            Some(_) => {}
        }
        sqlx::query("DELETE FROM preview_tokens WHERE device_id = ? AND expires_at_ms <= ?")
            .bind(&token.device_id)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(be)?;
        sqlx::query(
            "INSERT INTO preview_tokens (token_hash, label, node_id, account_id, device_id, created_at_ms, expires_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&token.token_hash)
        .bind(&token.label)
        .bind(&token.node_id)
        .bind(&token.account_id)
        .bind(&token.device_id)
        .bind(now_ms as i64)
        .bind(token.expires_at_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        // 这台设备只留最新的 `tokens_per_device` 条。
        sqlx::query(
            "DELETE FROM preview_tokens WHERE token_hash IN ( \
                 SELECT token_hash FROM preview_tokens WHERE device_id = ? \
                 ORDER BY created_at_ms DESC, rowid DESC LIMIT -1 OFFSET ?)",
        )
        .bind(&token.device_id)
        .bind(i64::from(limits.tokens_per_device))
        .execute(&mut *tx)
        .await
        .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(IssueOutcome::Issued)
    }

    async fn find_preview_token(&self, token_hash: &str) -> StoreResult<Option<PreviewToken>> {
        let row = sqlx::query(preview_token_select!("WHERE token_hash = ?"))
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(preview_token_from).transpose()
    }

    async fn preview_token_valid(&self, token_hash: &str, now_ms: u64) -> StoreResult<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM preview_tokens WHERE token_hash = ? AND expires_at_ms > ?")
                .bind(token_hash)
                .bind(now_ms as i64)
                .fetch_optional(&self.pool)
                .await
                .map_err(be)?;
        Ok(found.is_some())
    }

    async fn purge_expired_preview_tokens(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM preview_tokens WHERE expires_at_ms <= ?")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    async fn list_preview_labels(&self, node_id: &str) -> StoreResult<Vec<String>> {
        sqlx::query_scalar("SELECT label FROM preview_labels WHERE node_id = ? ORDER BY created_at_ms, rowid")
            .bind(node_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)
    }

    async fn list_preview_tokens_of_device(&self, device_id: &str) -> StoreResult<Vec<PreviewToken>> {
        let rows = sqlx::query(preview_token_select!("WHERE device_id = ? ORDER BY created_at_ms, rowid"))
            .bind(device_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter().map(preview_token_from).collect()
    }

    // ---- kv ----

    async fn kv_get(&self, key: &str) -> StoreResult<Option<String>> {
        sqlx::query_scalar("SELECT value FROM kv WHERE key = ?").bind(key).fetch_optional(&self.pool).await.map_err(be)
    }

    async fn kv_set(&self, key: &str, value: &str) -> StoreResult<()> {
        sqlx::query("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    // ---- multi：配额、账号删除、管理员标记、账号搜索（SQL 在 `multi.rs`） ----

    async fn count_devices(&self, account_id: &str) -> StoreResult<(u32, u32)> {
        self.count_devices_impl(account_id).await
    }

    async fn create_devices_within_limits(
        &self,
        devices: Vec<NewDevice>,
        limits: DeviceLimits,
        now_ms: u64,
    ) -> StoreResult<CreateDevicesOutcome> {
        self.create_devices_within_limits_impl(devices, limits, now_ms).await
    }

    async fn set_account_admin(&self, account_id: &str, is_admin: bool) -> StoreResult<bool> {
        self.set_account_admin_impl(account_id, is_admin).await
    }

    async fn delete_account(&self, account_id: &str, tombstone: NewAudit, now_ms: u64) -> StoreResult<bool> {
        self.delete_account_impl(account_id, tombstone, now_ms).await
    }

    async fn search_accounts(
        &self,
        query: &str,
        limit: u32,
        after: Option<(u64, String)>,
    ) -> StoreResult<Vec<Account>> {
        self.search_accounts_impl(query, limit, after).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::conformance::{self, backend_sqlite};

    // 和 Postgres 共用的那一套（见 `store/conformance.rs`）。
    conformance::suite!(backend_sqlite);

    #[tokio::test]
    async fn a_url_that_is_not_sqlite_is_rejected_with_the_accepted_forms() {
        let dir = tempfile::tempdir().unwrap();
        for url in ["mysql://u@h/db", "postgres://u@h/db", "/just/a/path"] {
            let err = SqliteStore::open(Some(url), dir.path()).await.err().expect("rejected");
            let msg = err.to_string();
            assert!(msg.contains("sqlite://") && msg.contains("postgres://"), "{msg}");
        }
    }

    #[tokio::test]
    async fn a_sqlite_url_points_the_store_at_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("elsewhere.db");
        let url = format!("sqlite://{}", file.display());
        let store = SqliteStore::open(Some(&url), dir.path()).await.unwrap();
        store.kv_set("k", "v").await.unwrap();
        assert!(file.exists());
        assert!(!dir.path().join("hub.db").exists());
    }
}
