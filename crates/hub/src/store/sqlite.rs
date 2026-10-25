//! SQLite 实现（sqlx）。运行时查询（不用编译期宏），迁移内嵌自 `migrations/sqlite/`。

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
    SetEmailOutcome, Store, StoreError, StoreResult, StoreStats, WebSession,
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
    pool: SqlitePool,
}

impl SqliteStore {
    /// 打开（必要时创建）数据库并跑完迁移。
    ///
    /// `database_url`：`None` 用 `<data_dir>/hub.db`；否则是 `sqlite://路径` / `sqlite:路径`。
    pub async fn open(database_url: Option<&str>, data_dir: &Path) -> StoreResult<Self> {
        let options = match database_url {
            None => SqliteConnectOptions::new().filename(data_dir.join("hub.db")),
            Some(url) => {
                let lower = url.to_ascii_lowercase();
                if lower.starts_with("postgres:") || lower.starts_with("postgresql:") {
                    return Err(StoreError::Backend(
                        "ANYCON_DATABASE_URL: Postgres is not implemented in v0.1 (use sqlite)".into(),
                    ));
                }
                if !lower.starts_with("sqlite:") {
                    return Err(StoreError::Backend(
                        "ANYCON_DATABASE_URL: only sqlite URLs are supported (sqlite:///path/to/hub.db)".into(),
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

    async fn find_device_by_token_hash(&self, token_hash: &str) -> StoreResult<Option<Device>> {
        let row = sqlx::query(device_select!("WHERE token_hash = ?"))
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(device_from).transpose()
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
    use crate::util::{hash_token, now_ms};
    use anycon_protocol::DeviceKind;

    async fn store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStore::open(None, dir.path()).await.unwrap();
        (s, dir)
    }

    fn new_device(account: &str, id: &str, token: &str) -> NewDevice {
        NewDevice {
            id: id.into(),
            account_id: account.into(),
            kind: DeviceKind::Daemon,
            name: "box".into(),
            platform: "linux-x86_64".into(),
            origin: None,
            token_hash: hash_token(token),
        }
    }

    #[tokio::test]
    async fn accounts_tokens_sessions() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = s.create_account("admin", true, now).await.unwrap();
        assert_eq!(s.first_admin().await.unwrap().unwrap().id, acc.id);

        let t = s.create_account_token(&acc.id, "laptop", &hash_token("tok"), now).await.unwrap();
        assert_eq!(s.find_account_by_token_hash(&hash_token("tok"), now + 5).await.unwrap().unwrap().id, acc.id);
        assert_eq!(s.list_account_tokens(&acc.id).await.unwrap()[0].last_used_ms, Some(now + 5));
        assert!(s.find_account_by_token_hash(&hash_token("nope"), now).await.unwrap().is_none());
        assert!(s.delete_account_token(&acc.id, &t.id).await.unwrap());
        assert!(!s.delete_account_token(&acc.id, &t.id).await.unwrap());

        let sess =
            s.create_web_session(&acc.id, &hash_token("cookie"), "ua", "1.2.3.0/24", now, now + 1000).await.unwrap();
        assert!(s.find_web_session(&hash_token("cookie"), now + 10).await.unwrap().is_some());
        assert!(s.find_web_session(&hash_token("cookie"), now + 1000).await.unwrap().is_none());
        s.touch_web_session(&sess.id, now + 10, now + 5000).await.unwrap();
        assert!(s.find_web_session(&hash_token("cookie"), now + 1000).await.unwrap().is_some());
        assert_eq!(s.list_web_sessions(&acc.id, now + 20).await.unwrap().len(), 1);
        assert!(s.delete_web_session(&acc.id, &sess.id).await.unwrap());
    }

    #[tokio::test]
    async fn invites_are_single_use() {
        let (s, _d) = store().await;
        let now = now_ms();
        let admin = s.create_account("admin", true, now).await.unwrap();
        s.create_invite(&admin.id, &hash_token("inv"), Some("for bob"), now, now + 10_000).await.unwrap();
        match s.redeem_invite(&hash_token("inv"), "bob", now + 1).await.unwrap() {
            RedeemOutcome::Redeemed(a) => assert_eq!(a.display_name, "bob"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(s.redeem_invite(&hash_token("inv"), "eve", now + 2).await.unwrap(), RedeemOutcome::Used));
        assert!(matches!(s.redeem_invite(&hash_token("zzz"), "eve", now + 2).await.unwrap(), RedeemOutcome::NotFound));
        s.create_invite(&admin.id, &hash_token("old"), None, now, now + 5).await.unwrap();
        assert!(matches!(s.redeem_invite(&hash_token("old"), "eve", now + 10).await.unwrap(), RedeemOutcome::Expired));
        // 失败的兑换不能留下账号
        assert_eq!(s.stats().await.unwrap().accounts, 2);
    }

    #[tokio::test]
    async fn invites_and_accounts_paginate_by_keyset_even_with_identical_timestamps() {
        let (s, _d) = store().await;
        let now = now_ms();
        let admin = s.create_account("admin", true, now).await.unwrap();
        // 时间戳全相同：只能靠 id 兜底排序，翻页不能漏、不能重
        for i in 0..7 {
            s.create_invite(&admin.id, &hash_token(&format!("inv{i}")), None, now, now + 10_000).await.unwrap();
            s.create_account(&format!("user{i}"), false, now).await.unwrap();
        }
        s.create_invite(&admin.id, &hash_token("older"), None, now - 5, now + 10_000).await.unwrap();

        let mut seen = Vec::new();
        let mut after = None;
        loop {
            let page = s.list_invites(3, after.clone()).await.unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 3);
            after = page.last().map(|i| (i.created_at_ms, i.id.clone()));
            seen.extend(page);
        }
        assert_eq!(seen.len(), 8);
        let keys: Vec<(u64, String)> = seen.iter().map(|i| (i.created_at_ms, i.id.clone())).collect();
        assert!(keys.windows(2).all(|w| w[0] > w[1]), "strictly descending by (created_at, id): {keys:?}");
        assert_eq!(seen.last().unwrap().created_at_ms, now - 5, "the older invite comes last");
        assert_eq!(s.list_invites(100, None).await.unwrap().len(), 8);

        let mut accounts = Vec::new();
        let mut after = None;
        loop {
            let page = s.list_accounts(3, after.clone()).await.unwrap();
            if page.is_empty() {
                break;
            }
            after = page.last().map(|a| (a.created_at_ms, a.id.clone()));
            accounts.extend(page);
        }
        assert_eq!(accounts.len(), 8); // admin + 7
        let keys: Vec<(u64, String)> = accounts.iter().map(|a| (a.created_at_ms, a.id.clone())).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "strictly ascending by (created_at, id): {keys:?}");
    }

    #[tokio::test]
    async fn devices_pairings_revoke() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = s.create_account("admin", true, now).await.unwrap();
        s.create_device(new_device(&acc.id, "bbbb", "t1"), now).await.unwrap();
        s.create_device(new_device(&acc.id, "aaaa", "t2"), now).await.unwrap();
        s.create_device(new_device(&acc.id, "cccc", "t3"), now).await.unwrap();
        s.insert_pairing("bbbb", "aaaa", now).await.unwrap();
        s.insert_pairing("aaaa", "bbbb", now).await.unwrap(); // 幂等
        s.insert_pairing("aaaa", "cccc", now).await.unwrap();

        assert!(s.is_paired("bbbb", "aaaa").await.unwrap());
        assert!(!s.is_paired("bbbb", "cccc").await.unwrap());
        let mut peers = s.peers_of("aaaa").await.unwrap();
        peers.sort();
        assert_eq!(peers, vec!["bbbb", "cccc"]);
        assert_eq!(s.list_pairings_of_account(&acc.id).await.unwrap().len(), 2);
        assert_eq!(s.find_device_by_token_hash(&hash_token("t1")).await.unwrap().unwrap().id, "bbbb");

        let mut former = s.revoke_device("aaaa", now + 1).await.unwrap();
        former.sort();
        assert_eq!(former, vec!["bbbb", "cccc"]);
        assert!(s.peers_of("bbbb").await.unwrap().is_empty());
        assert!(s.get_device("aaaa").await.unwrap().unwrap().revoked_at_ms.is_some());
        assert_eq!(s.list_devices(&acc.id).await.unwrap().len(), 2);
        assert!(s.revoke_device("aaaa", now + 2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_account_and_its_recorded_id_are_written_in_one_transaction() {
        let (s, _d) = store().await;
        let a = s.create_account_and_record_id("admin", true, "bootstrap_admin_id", 5).await.unwrap();
        assert_eq!(s.kv_get("bootstrap_admin_id").await.unwrap().as_deref(), Some(a.id.as_str()));
        let got = s.get_account(&a.id).await.unwrap().unwrap();
        assert!((got.display_name.as_str(), got.is_admin, got.created_at_ms) == ("admin", true, 5));

        // 记录写不进去（这里用触发器模拟崩溃 / 写失败）：账号也不留下，没有没人记得的孤儿管理员
        sqlx::query(
            "CREATE TRIGGER no_kv BEFORE INSERT ON kv WHEN NEW.key = 'fails' BEGIN SELECT RAISE(ABORT, 'no kv'); END",
        )
        .execute(&s.pool)
        .await
        .unwrap();
        assert!(s.create_account_and_record_id("orphan", true, "fails", 6).await.is_err());
        let names: Vec<String> =
            sqlx::query_scalar("SELECT display_name FROM accounts").fetch_all(&s.pool).await.unwrap();
        assert_eq!(names, vec!["admin".to_string()], "the failed attempt left no account behind");
    }

    #[tokio::test]
    async fn audit_and_kv() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.insert_audit(
            NewAudit { event: "login_ok", detail: Some(serde_json::json!({"method": "token"})), ..Default::default() },
            now,
        )
        .await
        .unwrap();
        s.insert_audit(NewAudit { event: "device_paired", ..Default::default() }, now + 1).await.unwrap();
        let all = s.list_audit(10, None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].event, "device_paired");
        assert_eq!(all[1].detail.as_ref().unwrap()["method"], "token");
        assert_eq!(s.list_audit(10, Some(all[0].id)).await.unwrap().len(), 1);
        assert_eq!(s.purge_audit_before(now + 1).await.unwrap(), 1);

        assert!(s.kv_get("k").await.unwrap().is_none());
        s.kv_set("k", "v1").await.unwrap();
        s.kv_set("k", "v2").await.unwrap();
        assert_eq!(s.kv_get("k").await.unwrap().as_deref(), Some("v2"));
        s.ping().await.unwrap();
    }

    #[tokio::test]
    async fn rejects_postgres_url() {
        let dir = tempfile::tempdir().unwrap();
        let err = SqliteStore::open(Some("postgres://u@h/db"), dir.path()).await.err().unwrap();
        assert!(err.to_string().contains("not implemented in v0.1"));
    }

    // ---- ADR-37：邮箱、验证码、推送订阅、通知偏好 ----

    fn code_row(email: &str, purpose: EmailPurpose, account: Option<&str>, hash: &str, now: u64) -> EmailCode {
        EmailCode {
            email: email.into(),
            purpose,
            account_id: account.map(Into::into),
            code_hash: hash.into(),
            expires_at_ms: now + 600_000,
            attempts: 0,
            created_at_ms: now,
        }
    }

    #[tokio::test]
    async fn an_email_belongs_to_one_account_and_can_be_cleared() {
        let (s, _d) = store().await;
        let now = now_ms();
        let a = s.create_account("a", true, now).await.unwrap();
        let b = s.create_account("b", false, now).await.unwrap();
        assert!(s.find_account_by_email("x@example.com").await.unwrap().is_none());
        assert_eq!(s.set_account_email(&a.id, Some("x@example.com")).await.unwrap(), SetEmailOutcome::Done);
        assert_eq!(s.find_account_by_email("x@example.com").await.unwrap().unwrap().id, a.id);
        assert_eq!(s.get_account(&a.id).await.unwrap().unwrap().email.as_deref(), Some("x@example.com"));
        // 唯一约束：第二个账号拿不到同一个邮箱；大小写不同的写法同样算占用（lower() 唯一索引）
        assert_eq!(s.set_account_email(&b.id, Some("x@example.com")).await.unwrap(), SetEmailOutcome::Taken);
        assert_eq!(s.set_account_email(&b.id, Some("X@Example.com")).await.unwrap(), SetEmailOutcome::Taken);
        assert!(s.get_account(&b.id).await.unwrap().unwrap().email.is_none());
        // 自己重复设置 / 换一个都行；换走之后旧地址可以给别人
        assert_eq!(s.set_account_email(&a.id, Some("x@example.com")).await.unwrap(), SetEmailOutcome::Done);
        assert_eq!(s.set_account_email(&a.id, Some("y@example.com")).await.unwrap(), SetEmailOutcome::Done);
        assert_eq!(s.set_account_email(&b.id, Some("x@example.com")).await.unwrap(), SetEmailOutcome::Done);
        // 清除：多个账号都没有邮箱不冲突
        assert_eq!(s.set_account_email(&a.id, None).await.unwrap(), SetEmailOutcome::Done);
        assert_eq!(s.set_account_email(&b.id, None).await.unwrap(), SetEmailOutcome::Done);
        assert!(s.find_account_by_email("y@example.com").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_account_can_be_created_with_its_email_and_the_second_creator_of_that_email_loses() {
        let (s, _d) = store().await;
        let now = now_ms();
        let CreateAccountOutcome::Created(a) =
            s.create_account_with_email("alice", Some("alice@example.com"), now).await.unwrap()
        else {
            panic!("the first creation wins")
        };
        assert_eq!(
            (a.display_name.as_str(), a.email.as_deref(), a.is_admin),
            ("alice", Some("alice@example.com"), false)
        );
        assert_eq!(s.find_account_by_email("alice@example.com").await.unwrap().unwrap().id, a.id);
        // 同一个邮箱（含大小写不同的写法）再建一个：EmailTaken，库里没有多出账号
        for email in ["alice@example.com", "Alice@Example.com"] {
            assert!(matches!(
                s.create_account_with_email("mallory", Some(email), now).await.unwrap(),
                CreateAccountOutcome::EmailTaken
            ));
        }
        assert_eq!(s.stats().await.unwrap().accounts, 1);
        // 没有邮箱的账号可以有很多个
        for name in ["n1", "n2"] {
            let CreateAccountOutcome::Created(n) = s.create_account_with_email(name, None, now).await.unwrap() else {
                panic!("accounts without an email never collide")
            };
            assert!(n.email.is_none());
        }
        assert_eq!(s.stats().await.unwrap().accounts, 3);
    }

    /// 这一组里现在的码的哈希，最旧的在前。
    async fn hashes(s: &SqliteStore, email: &str, purpose: EmailPurpose) -> Vec<String> {
        s.list_email_codes(email, purpose).await.unwrap().into_iter().map(|c| c.code_hash).collect()
    }

    #[tokio::test]
    async fn a_new_code_does_not_void_the_old_ones_and_the_oldest_is_evicted_past_the_limit() {
        let (s, _d) = store().await;
        let now = now_ms();
        for (i, h) in ["h1", "h2", "h3"].iter().enumerate() {
            s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, h, now + i as u64), 3).await.unwrap();
        }
        assert_eq!(hashes(&s, "x@example.com", EmailPurpose::Login).await, ["h1", "h2", "h3"], "all three are valid");
        // 第 4 个顶掉最旧的，另外两个还在
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h4", now + 3), 3).await.unwrap();
        assert_eq!(hashes(&s, "x@example.com", EmailPurpose::Login).await, ["h2", "h3", "h4"]);
        // 别的邮箱、别的用途各算各的
        s.put_email_code(code_row("y@example.com", EmailPurpose::Login, None, "hy", now), 3).await.unwrap();
        let acc = s.create_account("a", true, now).await.unwrap();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Bind, Some(&acc.id), "hb", now), 3).await.unwrap();
        assert_eq!(hashes(&s, "x@example.com", EmailPurpose::Login).await, ["h2", "h3", "h4"]);
        assert_eq!(hashes(&s, "y@example.com", EmailPurpose::Login).await, ["hy"]);
        assert_eq!(hashes(&s, "x@example.com", EmailPurpose::Bind).await, ["hb"]);
        // 过期的不占名额：新码进来时先把这一组里过期的清掉
        s.put_email_code(
            EmailCode { expires_at_ms: now + 5, ..code_row("z@example.com", EmailPurpose::Login, None, "old", now) },
            3,
        )
        .await
        .unwrap();
        s.put_email_code(code_row("z@example.com", EmailPurpose::Login, None, "fresh", now + 10), 3).await.unwrap();
        assert_eq!(hashes(&s, "z@example.com", EmailPurpose::Login).await, ["fresh"]);
    }

    #[tokio::test]
    async fn bind_codes_are_kept_per_account_and_email_so_another_account_cannot_void_yours() {
        let (s, _d) = store().await;
        let now = now_ms();
        let a = s.create_account("a", true, now).await.unwrap();
        let b = s.create_account("b", false, now).await.unwrap();
        // 一直只留最新一条的话（keep = 1），也只是同一个账号自己的新码顶自己的旧码
        s.put_email_code(code_row("x@example.com", EmailPurpose::Bind, Some(&a.id), "ha", now), 1).await.unwrap();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Bind, Some(&b.id), "hb", now + 1), 1).await.unwrap();
        let rows = s.list_email_codes("x@example.com", EmailPurpose::Bind).await.unwrap();
        assert_eq!(rows.len(), 2, "b's request did not replace a's code");
        // 验证按账号取：a 只看得到自己的，b 只看得到自己的，登录用途一条也看不到
        let max = 5;
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Bind, Some(&a.id), now, max).await.unwrap();
        assert_eq!(got.iter().map(|c| c.code_hash.as_str()).collect::<Vec<_>>(), ["ha"]);
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Bind, Some(&b.id), now, max).await.unwrap();
        assert_eq!(got.iter().map(|c| c.code_hash.as_str()).collect::<Vec<_>>(), ["hb"]);
        assert!(s.attempt_email_codes("x@example.com", EmailPurpose::Bind, None, now, max).await.unwrap().is_empty());
        assert!(s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now, max).await.unwrap().is_empty());
        let c = s.create_account("c", false, now).await.unwrap();
        assert!(s
            .attempt_email_codes("x@example.com", EmailPurpose::Bind, Some(&c.id), now, max)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_attempt_counts_every_live_code_and_hands_back_exactly_the_rows_it_counted() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h1", now), 3).await.unwrap();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h2", now + 1), 3).await.unwrap();
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 2, 5).await.unwrap();
        assert_eq!(got.iter().map(|c| (c.code_hash.as_str(), c.attempts)).collect::<Vec<_>>(), [("h1", 1), ("h2", 1)]);
        // 之后才发的码从 0 开始，不替前面的提交背次数
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h3", now + 3), 3).await.unwrap();
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 4, 5).await.unwrap();
        assert_eq!(
            got.iter().map(|c| (c.code_hash.as_str(), c.attempts)).collect::<Vec<_>>(),
            [("h1", 2), ("h2", 2), ("h3", 1)]
        );
        // 没有码 / 别的邮箱：空
        assert!(s
            .attempt_email_codes("nobody@example.com", EmailPurpose::Login, None, now, 5)
            .await
            .unwrap()
            .is_empty());
        // 次数用完的码不再交出来：h1、h2 再被尝试 3 次就到 5，之后只剩 h3
        for _ in 0..3 {
            s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 5, 5).await.unwrap();
        }
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 6, 5).await.unwrap();
        assert_eq!(got.iter().map(|c| (c.code_hash.as_str(), c.attempts)).collect::<Vec<_>>(), [("h3", 5)]);
        // 第 5 次提交时 h3 仍交出来了（所以仍能被认领）；再来一次就没有可猜的了
        assert!(s
            .attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 7, 5)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_attempts_get_at_most_max_attempts_comparisons_in_total() {
        // 「先原子计数、再比较」：计数和取码是同一个原子操作，所以 30 个并发提交里只有 5 个能拿到哈希去比较，
        // 其余的什么都拿不到（哪怕它们带的是对的码）。如果实现成「先取、比较、再计数」，并发的提交都会拿到哈希。
        let (s, _d) = store().await;
        let s = std::sync::Arc::new(s);
        let now = now_ms();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h1", now), 3).await.unwrap();
        let tasks: Vec<_> = (0..30)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move {
                    s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 1, 5).await.unwrap()
                })
            })
            .collect();
        let mut handed_out = Vec::new();
        for t in tasks {
            handed_out.extend(t.await.unwrap());
        }
        assert_eq!(handed_out.len(), 5, "only max_attempts submissions got to compare");
        let mut counts: Vec<u32> = handed_out.iter().map(|c| c.attempts).collect();
        counts.sort_unstable();
        assert_eq!(counts, [1, 2, 3, 4, 5], "each of them was counted before it compared");
        assert_eq!(s.list_email_codes("x@example.com", EmailPurpose::Login).await.unwrap()[0].attempts, 5);
    }

    #[tokio::test]
    async fn claiming_by_id_works_once_and_only_for_that_row() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h1", now), 3).await.unwrap();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h2", now + 1), 3).await.unwrap();
        let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 2, 5).await.unwrap();
        assert!(s.take_email_code(got[0].id).await.unwrap());
        assert!(!s.take_email_code(got[0].id).await.unwrap(), "a second claim loses");
        assert_eq!(hashes(&s, "x@example.com", EmailPurpose::Login).await, ["h2"], "the other code is untouched");
        // 被顶掉（过期清掉）的码认领不到
        assert!(!s.take_email_code(9_999_999).await.unwrap());
    }

    #[tokio::test]
    async fn expired_codes_are_cleaned_when_touched_and_purged_in_bulk() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_email_code(
            EmailCode { expires_at_ms: now + 10, ..code_row("x@example.com", EmailPurpose::Login, None, "h", now) },
            3,
        )
        .await
        .unwrap();
        // 过期了：交不出来，而且顺手删掉
        assert!(s
            .attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 10, 5)
            .await
            .unwrap()
            .is_empty());
        assert!(hashes(&s, "x@example.com", EmailPurpose::Login).await.is_empty());
        // 批量清理
        s.put_email_code(
            EmailCode { expires_at_ms: now + 10, ..code_row("old@example.com", EmailPurpose::Login, None, "h", now) },
            3,
        )
        .await
        .unwrap();
        s.put_email_code(code_row("live@example.com", EmailPurpose::Login, None, "h", now), 3).await.unwrap();
        assert_eq!(s.purge_expired_email_codes(now + 10).await.unwrap(), 1);
        assert!(hashes(&s, "old@example.com", EmailPurpose::Login).await.is_empty());
        assert_eq!(hashes(&s, "live@example.com", EmailPurpose::Login).await.len(), 1);
    }

    #[tokio::test]
    async fn push_subscriptions_are_one_per_device_and_follow_the_device_lifecycle() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = s.create_account("a", true, now).await.unwrap();
        let other = s.create_account("b", false, now).await.unwrap();
        s.create_device(
            NewDevice { kind: DeviceKind::Web, name: "Chrome".into(), ..new_device(&acc.id, "web1", "t1") },
            now,
        )
        .await
        .unwrap();
        s.create_device(
            NewDevice { kind: DeviceKind::Web, name: "Safari".into(), ..new_device(&acc.id, "web2", "t2") },
            now,
        )
        .await
        .unwrap();
        s.create_device(NewDevice { kind: DeviceKind::Web, ..new_device(&other.id, "web3", "t3") }, now).await.unwrap();

        let first = s.upsert_push_subscription("web1", "https://push.example/a", "pk1", "au1", now).await.unwrap();
        let second = s.upsert_push_subscription("web1", "https://push.example/b", "pk2", "au2", now + 1).await.unwrap();
        assert_ne!(first.id, second.id, "a new subscription replaces the old one under a new id");
        let got = s.get_push_subscription_of_device("web1").await.unwrap().unwrap();
        assert_eq!(
            (got.id.as_str(), got.endpoint.as_str(), got.p256dh.as_str(), got.auth.as_str()),
            (second.id.as_str(), "https://push.example/b", "pk2", "au2")
        );
        assert!(s.get_push_subscription_of_device("web2").await.unwrap().is_none());
        s.upsert_push_subscription("web2", "https://push.example/c", "pk3", "au3", now + 2).await.unwrap();
        s.upsert_push_subscription("web3", "https://push.example/d", "pk4", "au4", now + 3).await.unwrap();

        // 账号视图：只有自己的、带设备名
        let rows = s.list_push_subscriptions(&acc.id).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| (r.device_id.as_str(), r.device_name.as_str())).collect::<Vec<_>>(),
            vec![("web1", "Chrome"), ("web2", "Safari")]
        );
        assert_eq!(s.list_push_subscriptions(&other.id).await.unwrap().len(), 1);

        // 删除按账号隔离；按 id 删不认设备
        assert!(!s.delete_push_subscription(&other.id, &second.id).await.unwrap());
        assert!(!s.delete_push_subscription_by_id(&first.id).await.unwrap(), "the replaced id no longer exists");
        assert!(
            s.get_push_subscription_of_device("web1").await.unwrap().is_some(),
            "deleting a stale id does not touch the new subscription"
        );
        assert!(s.delete_push_subscription(&acc.id, &second.id).await.unwrap());
        assert!(s.get_push_subscription_of_device("web1").await.unwrap().is_none());

        // 吊销设备时一并删订阅（同一个事务）；已吊销的设备不出现在账号视图里
        s.revoke_device("web2", now + 10).await.unwrap();
        assert!(s.get_push_subscription_of_device("web2").await.unwrap().is_none());
        assert!(s.list_push_subscriptions(&acc.id).await.unwrap().is_empty());
        assert!(s.get_push_subscription_of_device("web3").await.unwrap().is_some(), "other devices keep theirs");
        // Debug 输出不带 endpoint 和密钥
        let dump = format!("{:?}", s.get_push_subscription_of_device("web3").await.unwrap().unwrap());
        assert!(!dump.contains("push.example") && !dump.contains("pk4") && !dump.contains("au4"), "{dump}");
    }

    #[tokio::test]
    async fn notification_prefs_are_stored_per_account_and_category() {
        use anycon_protocol::NotifyCategory::*;
        let (s, _d) = store().await;
        let now = now_ms();
        let a = s.create_account("a", true, now).await.unwrap();
        let b = s.create_account("b", false, now).await.unwrap();
        assert!(
            s.get_notification_prefs(&a.id).await.unwrap().is_empty(),
            "no rows = defaults, filled in by the caller"
        );
        let p = |category, push, email| NotificationPref { category, push, email };
        s.set_notification_prefs(&a.id, &[p(Error, false, true), p(Security, true, false)]).await.unwrap();
        s.set_notification_prefs(&a.id, &[p(Error, true, true)]).await.unwrap();
        s.set_notification_prefs(&b.id, &[p(Error, false, false)]).await.unwrap();
        let mut got = s.get_notification_prefs(&a.id).await.unwrap();
        got.sort_by_key(|x| category_to_str(x.category));
        assert_eq!(got, vec![p(Error, true, true), p(Security, true, false)]);
        assert_eq!(s.get_notification_prefs(&b.id).await.unwrap(), vec![p(Error, false, false)]);
        // 一个陌生的类别（以后版本写进来的）被跳过，不影响其它
        sqlx::query(
            "INSERT INTO notification_prefs (account_id, category, push, email) VALUES (?, 'from_the_future', 1, 1)",
        )
        .bind(&a.id)
        .execute(&s.pool)
        .await
        .unwrap();
        assert_eq!(s.get_notification_prefs(&a.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_notification_tables_cascade_with_their_owners() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = s.create_account("a", false, now).await.unwrap();
        s.create_device(NewDevice { kind: DeviceKind::Web, ..new_device(&acc.id, "w", "t") }, now).await.unwrap();
        s.upsert_push_subscription("w", "https://push.example/a", "pk", "au", now).await.unwrap();
        s.put_email_code(code_row("a@example.com", EmailPurpose::Bind, Some(&acc.id), "h", now), 3).await.unwrap();
        s.set_notification_prefs(
            &acc.id,
            &[NotificationPref { category: anycon_protocol::NotifyCategory::Error, push: true, email: true }],
        )
        .await
        .unwrap();
        sqlx::query("DELETE FROM accounts WHERE id = ?").bind(&acc.id).execute(&s.pool).await.unwrap();
        assert!(s.get_push_subscription_of_device("w").await.unwrap().is_none());
        assert!(hashes(&s, "a@example.com", EmailPurpose::Bind).await.is_empty());
        assert!(s.get_notification_prefs(&acc.id).await.unwrap().is_empty());
    }
    // ---- 预览（ADR-39） ----

    const LIMITS: PreviewIssueLimits = PreviewIssueLimits { labels_per_node: 3, tokens_per_device: 3 };

    fn new_token(hash: &str, label: &str, node: &str, device: &str, account: &str, expires: u64) -> NewPreviewToken {
        NewPreviewToken {
            token_hash: hash.into(),
            label: label.into(),
            node_id: node.into(),
            account_id: account.into(),
            device_id: device.into(),
            expires_at_ms: expires,
        }
    }

    /// 一个账号：两个节点 n1 / n2，一台 web 设备 w，一台别的 web 设备 w2。
    async fn preview_fixture(s: &SqliteStore, now: u64) -> String {
        let acc = s.create_account("a", true, now).await.unwrap();
        for (id, kind) in
            [("n1", DeviceKind::Daemon), ("n2", DeviceKind::Daemon), ("w", DeviceKind::Web), ("w2", DeviceKind::Web)]
        {
            s.create_device(NewDevice { kind, ..new_device(&acc.id, id, &format!("tok-{id}")) }, now).await.unwrap();
        }
        acc.id
    }

    #[tokio::test]
    async fn the_first_token_binds_the_label_and_a_second_node_cannot_take_it() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        let issue = |hash: &str, label: &str, node: &str, device: &str| {
            s.issue_preview_token(new_token(hash, label, node, device, &acc, now + 1000), now, LIMITS)
        };
        assert_eq!(issue("h1", "label-a", "n1", "w").await.unwrap(), IssueOutcome::Issued);
        // 同一个节点再要一次：同一个 label，多一个令牌
        assert_eq!(issue("h2", "label-a", "n1", "w2").await.unwrap(), IssueOutcome::Issued);
        // 别的节点要同一个 label：被拒，而且什么都没写
        assert_eq!(issue("h3", "label-a", "n2", "w").await.unwrap(), IssueOutcome::LabelTaken);
        assert!(s.find_preview_token("h3").await.unwrap().is_none());
        assert_eq!(s.list_preview_labels("n1").await.unwrap(), ["label-a"]);
        assert!(s.list_preview_labels("n2").await.unwrap().is_empty());
        let t = s.find_preview_token("h1").await.unwrap().unwrap();
        assert_eq!(
            (t.label.as_str(), t.node_id.as_str(), t.device_id.as_str(), t.account_id.as_str()),
            ("label-a", "n1", "w", acc.as_str())
        );
        assert_eq!((t.created_at_ms, t.expires_at_ms), (now, now + 1000));
        // Debug 不输出哈希
        assert!(!format!("{t:?}").contains("h1"));
    }

    #[tokio::test]
    async fn a_node_binds_at_most_the_configured_number_of_labels() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        let issue = |hash: &str, label: &str, node: &str| {
            s.issue_preview_token(new_token(hash, label, node, "w", &acc, now + 1000), now, LIMITS)
        };
        for (i, label) in ["l1", "l2", "l3"].iter().enumerate() {
            assert_eq!(issue(&format!("h{i}"), label, "n1").await.unwrap(), IssueOutcome::Issued);
        }
        // 第 4 个新 label：被拒；已经绑定的 label 照常可以再要令牌；别的节点有自己的额度
        assert_eq!(issue("h-over", "l4", "n1").await.unwrap(), IssueOutcome::TooManyLabels);
        assert!(s.find_preview_token("h-over").await.unwrap().is_none());
        assert_eq!(s.list_preview_labels("n1").await.unwrap(), ["l1", "l2", "l3"]);
        assert_eq!(issue("h-again", "l2", "n1").await.unwrap(), IssueOutcome::Issued);
        assert_eq!(issue("h-n2", "l4", "n2").await.unwrap(), IssueOutcome::Issued);
    }

    #[tokio::test]
    async fn a_device_keeps_only_its_newest_tokens_and_expired_ones_do_not_count() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        // 一个已经过期的、一个别的设备的：都不占 w 的名额
        s.issue_preview_token(new_token("old", "l1", "n1", "w", &acc, now + 10), now - 1000, LIMITS).await.unwrap();
        s.issue_preview_token(new_token("other", "l1", "n1", "w2", &acc, now + 10_000), now, LIMITS).await.unwrap();
        for i in 0..5u64 {
            // 同一毫秒签发的也有先后（rowid）
            let t = new_token(&format!("t{i}"), "l1", "n1", "w", &acc, now + 10_000);
            assert_eq!(s.issue_preview_token(t, now + 100, LIMITS).await.unwrap(), IssueOutcome::Issued);
        }
        let kept: Vec<String> =
            s.list_preview_tokens_of_device("w").await.unwrap().into_iter().map(|t| t.token_hash).collect();
        assert_eq!(kept, ["t2", "t3", "t4"], "the three newest; the expired one was cleaned up on the way");
        assert!(s.find_preview_token("t0").await.unwrap().is_none());
        assert!(s.find_preview_token("old").await.unwrap().is_none());
        assert!(s.find_preview_token("other").await.unwrap().is_some(), "another device's tokens are untouched");
    }

    #[tokio::test]
    async fn token_validity_and_purging_follow_the_expiry_time() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        s.issue_preview_token(new_token("short", "l1", "n1", "w", &acc, now + 100), now, LIMITS).await.unwrap();
        s.issue_preview_token(new_token("long", "l1", "n1", "w2", &acc, now + 10_000), now, LIMITS).await.unwrap();
        assert!(s.preview_token_valid("short", now + 99).await.unwrap());
        assert!(!s.preview_token_valid("short", now + 100).await.unwrap(), "expired exactly at expires_at_ms");
        assert!(!s.preview_token_valid("nope", now).await.unwrap());
        // find 不看过期（调用方判断）；purge 删掉过期的，label 的绑定保留
        assert!(s.find_preview_token("short").await.unwrap().is_some());
        assert_eq!(s.purge_expired_preview_tokens(now + 100).await.unwrap(), 1);
        assert!(s.find_preview_token("short").await.unwrap().is_none());
        assert!(s.find_preview_token("long").await.unwrap().is_some());
        assert_eq!(s.list_preview_labels("n1").await.unwrap(), ["l1"]);
        assert_eq!(s.purge_expired_preview_tokens(now + 100).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn revoking_a_device_deletes_its_tokens_and_revoking_a_node_deletes_its_labels_and_tokens() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        let far = now + 100_000;
        s.issue_preview_token(new_token("a", "l1", "n1", "w", &acc, far), now, LIMITS).await.unwrap();
        s.issue_preview_token(new_token("b", "l1", "n1", "w2", &acc, far), now, LIMITS).await.unwrap();
        s.issue_preview_token(new_token("c", "l2", "n2", "w2", &acc, far), now, LIMITS).await.unwrap();
        // 吊销签发设备 w：只删它签发的
        s.revoke_device("w", now + 1).await.unwrap();
        assert!(s.find_preview_token("a").await.unwrap().is_none());
        assert!(
            s.find_preview_token("b").await.unwrap().is_some() && s.find_preview_token("c").await.unwrap().is_some()
        );
        assert_eq!(
            s.list_preview_labels("n1").await.unwrap(),
            ["l1"],
            "the label stays: another device may still use it"
        );
        // 吊销节点 n1：它的 label 和用到这些 label 的令牌（不管谁签发的）都删
        s.revoke_device("n1", now + 2).await.unwrap();
        assert!(s.list_preview_labels("n1").await.unwrap().is_empty());
        assert!(s.find_preview_token("b").await.unwrap().is_none());
        // n2 不受影响；w2 签发的指向 n2 的令牌还在
        assert_eq!(s.list_preview_labels("n2").await.unwrap(), ["l2"]);
        assert!(s.find_preview_token("c").await.unwrap().is_some());
        // 吊销 n2 之后 l2 也没了
        s.revoke_device("n2", now + 3).await.unwrap();
        assert!(
            s.list_preview_labels("n2").await.unwrap().is_empty() && s.find_preview_token("c").await.unwrap().is_none()
        );
    }

    #[tokio::test]
    async fn preview_rows_cascade_with_the_account() {
        let (s, _d) = store().await;
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        s.issue_preview_token(new_token("a", "l1", "n1", "w", &acc, now + 1000), now, LIMITS).await.unwrap();
        sqlx::query("DELETE FROM accounts WHERE id = ?").bind(&acc).execute(&s.pool).await.unwrap();
        assert!(s.find_preview_token("a").await.unwrap().is_none());
        assert!(s.list_preview_labels("n1").await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_for_one_label_bind_it_to_exactly_one_node() {
        let (s, _d) = store().await;
        let s = std::sync::Arc::new(s);
        let now = now_ms();
        let acc = preview_fixture(&s, now).await;
        let tasks: Vec<_> = (0..12)
            .map(|i| {
                let (s, acc) = (s.clone(), acc.clone());
                let node = if i % 2 == 0 { "n1" } else { "n2" };
                tokio::spawn(async move {
                    s.issue_preview_token(new_token(&format!("h{i}"), "race", node, "w", &acc, now + 1000), now, LIMITS)
                        .await
                        .unwrap()
                })
            })
            .collect();
        let mut outcomes = Vec::new();
        for t in tasks {
            outcomes.push(t.await.unwrap());
        }
        let issued = outcomes.iter().filter(|o| **o == IssueOutcome::Issued).count();
        assert_eq!(issued, 6, "only the winning node's six requests are issued: {outcomes:?}");
        assert_eq!(outcomes.iter().filter(|o| **o == IssueOutcome::LabelTaken).count(), 6);
        let owners = [s.list_preview_labels("n1").await.unwrap(), s.list_preview_labels("n2").await.unwrap()];
        assert_eq!(owners.iter().filter(|l| l.as_slice() == ["race"]).count(), 1);
    }
}
