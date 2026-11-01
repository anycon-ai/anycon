//! Postgres 实现（sqlx，ADR-12）。运行时查询（不用编译期宏、不用 `sqlx::Any`），迁移内嵌自 `migrations/postgres/`。
//!
//! 和 [`SqliteStore`](super::SqliteStore) 实现同一个 [`Store`]，行为一致；表结构、类型映射见
//! `migrations/postgres/0001_init.sql` 的开头（毫秒时间戳是 `BIGINT`，布尔是 `BOOLEAN`，自增 id 是 `GENERATED ALWAYS AS IDENTITY`）。
//!
//! # 并发语义
//!
//! SQLite 只有一个写者，「先查再写」的逻辑在 SQLite 里天然串行。Postgres 是多写者（默认 `READ COMMITTED`），
//! 同样的逻辑逐处用下面的办法保证相同的结果：
//!
//! | 操作 | SQLite | Postgres |
//! |---|---|---|
//! | 设备数 / daemon 数上限（`create_devices_within_limits`） | 写事务里 `INSERT … SELECT … WHERE 数量 < 上限` | 先 `SELECT … FOR NO KEY UPDATE` 锁住涉及的账号行（按 id 排序，批里有几个账号也不会互相死锁），再同样的 `INSERT … SELECT … WHERE`：同一账号的并发建设备排队，后到的数到的是前一个已提交的结果 |
//! | 邀请只能兑换一次（`redeem_invite`、`create_account_with_identity`） | 一条 `UPDATE … WHERE used_by IS NULL AND 未过期` | 同一条 `UPDATE`：行锁让并发的第二条等第一条提交，然后在新版本上重新判断 `used_by IS NULL`，不满足就是 0 行 |
//! | 邮箱 / 外部身份唯一（`create_account_with_email`、`set_account_email`、`link_identity`、`create_account_with_identity`） | 唯一约束 | 唯一约束（并发的第二个 `INSERT` 等第一个提交或回滚，再报冲突） |
//! | 一次性取走（`take_oauth_state`、`take_email_code`） | 一条 `DELETE`（`… RETURNING`） | 同一条 `DELETE`：并发的第二条等第一条提交，找不到行 |
//! | 验证码尝试次数（`attempt_email_codes`） | 一条 `UPDATE … SET attempts = attempts + 1 … RETURNING`，写入串行 | 事务里先 `SELECT … ORDER BY id FOR UPDATE` 锁住这一组里还能猜的码（拿到锁后重新判断 `attempts < max`），再 `UPDATE … RETURNING`；一条码一共只会被交出去 `max_attempts` 次，多条码按 id 顺序加锁不会死锁 |
//! | 验证码每组最多留 `keep` 条（`put_email_code`） | 写事务 | 事务里先拿这一组（邮箱，用途，账号）的 advisory 事务锁（`pg_advisory_xact_lock`）：同一组的并发登记排队，修剪看到的是前一个的结果 |
//! | 预览 label 先到先得、每节点 label 数、每设备令牌数、吊销竞争（`issue_preview_token`） | 写事务 | 事务里先 `SELECT … FOR NO KEY UPDATE` 锁住节点和签发设备的 `devices` 行（按 id 排序）：同一节点 / 设备的并发签发排队，数量上限不会被一起冲过；它和吊销设备（`FOR UPDATE`）冲突，所以吊销要么看见这条令牌并删掉它，要么先于签发完成。label 本身的先到先得靠主键上的 `ON CONFLICT DO NOTHING` |
//! | 吊销设备（`revoke_device`） | 写事务 | 先 `SELECT … FOR UPDATE` 锁设备行：和并发的配对 / 令牌签发（外键检查拿 `FOR KEY SHARE`）互斥，之后的删除看得到它们已提交的行；并发的第二次吊销在新版本上看到已吊销，返回空 |
//! | 删账号（`delete_account`） | 写事务 | 先 `SELECT … FOR UPDATE` 锁账号行，并发的第二个删除看不到它、返回 `false`，不会多写一条墓碑审计 |
//! | 吊销 / 登录方式一类的「读后改」 | hub 内存里按账号的锁（[`crate::accounts::LoginMethodLocks`]） | 同一把锁（不在库里），多条语句各自原子 |
//!
//! 每个多语句事务都包在 [`with_retry`] 里：Postgres 在死锁（`40P01`）和串行化失败（`40001`）时会中止其中一个事务，
//! 事务整体回滚后重来是安全的，所以重试几次而不是把错误报给调用方。
//!
//! 每日配对名额、限速计数、在线连接数在 hub 进程内存里，不在库里，所以不在上表中。
//!
//! # 和 SQLite 的差异
//!
//! - 排序规则：SQLite 的 `TEXT` 按字节比较；查询里需要和 Rust 一致的 `ORDER BY` / keyset 游标写 `COLLATE "C"`。
//! - 账号搜索的「只有 ASCII 字母不区分大小写」用 `translate()` 折叠 ASCII 字母实现，不用 `lower()`（`lower()` 随数据库的
//!   locale 折叠非 ASCII 字符），见 `multi.rs`。
//! - `TEXT` 里不能有 NUL：自由文本字段（设备名、备注、UA……）写入前去掉 NUL（[`nul`]）；SQLite 原样存。
//! - 数据库大小是 `pg_database_size(current_database())`，整个数据库的大小。

use std::borrow::Cow;
use std::fmt::Display;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use rand::RngCore;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow};
use sqlx::{Connection, PgConnection, PgPool, Row};

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

/// 连接池上限。hub 是单实例，同一时刻的写入不多；这个数够一批并发请求，又远低于 Postgres 默认的 `max_connections = 100`。
const MAX_CONNECTIONS: u32 = 10;
/// 建立连接（和从池里取连接）的超时：启动时连不上的库在这个时间之后报错，而不是一直挂着。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);
/// 事务因死锁 / 串行化失败被 Postgres 中止时，整个事务最多跑几次。
const TX_ATTEMPTS: u32 = 5;

fn be(e: impl Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// 毫秒数读出来：库里是 `BIGINT`，Rust 侧是 `u64`。
fn u(row: &PgRow, col: &str) -> StoreResult<u64> {
    let v: i64 = row.try_get(col).map_err(be)?;
    Ok(v.max(0) as u64)
}

fn u_opt(row: &PgRow, col: &str) -> StoreResult<Option<u64>> {
    let v: Option<i64> = row.try_get(col).map_err(be)?;
    Ok(v.map(|v| v.max(0) as u64))
}

fn s(row: &PgRow, col: &str) -> StoreResult<String> {
    row.try_get::<String, _>(col).map_err(be)
}

fn s_opt(row: &PgRow, col: &str) -> StoreResult<Option<String>> {
    row.try_get::<Option<String>, _>(col).map_err(be)
}

/// 自由文本写进 `TEXT` 之前去掉 NUL：Postgres 的 `TEXT` 不接受 `\0`（整条语句会失败），而这些字段（设备名、备注、UA……）
/// 来自客户端，没有别处保证它不含 NUL。
fn nul(text: &str) -> Cow<'_, str> {
    if text.contains('\0') {
        Cow::Owned(text.replace('\0', ""))
    } else {
        Cow::Borrowed(text)
    }
}

fn nul_opt(text: Option<&str>) -> Option<Cow<'_, str>> {
    text.map(nul)
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// Postgres 中止事务的两种「重来就好」的错误：死锁（`40P01`）、串行化失败（`40001`）。
fn is_retryable(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if matches!(db.code().as_deref(), Some("40P01" | "40001")))
}

/// 跑一个事务体（`op` 每次调用都新开一个事务）。撞上死锁 / 串行化失败就整个重来，最多 [`TX_ATTEMPTS`] 次，
/// 每次之间退避几毫秒（带抖动，免得两个互相死锁的事务同步重试）。其它错误原样转成 [`StoreError`]。
async fn with_retry<T, F, Fut>(mut op: F) -> StoreResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < TX_ATTEMPTS && is_retryable(&e) => {
                tracing::debug!(attempt, "transaction was aborted by the database (deadlock), retrying");
                let jitter = u64::from(rand::rngs::OsRng.next_u32() % 10);
                tokio::time::sleep(Duration::from_millis(5 * u64::from(attempt) + jitter)).await;
                attempt += 1;
            }
            Err(e) => return Err(be(e)),
        }
    }
}

fn account_from(row: &PgRow) -> StoreResult<Account> {
    Ok(Account {
        id: s(row, "id")?,
        display_name: s(row, "display_name")?,
        email: s_opt(row, "email")?,
        is_admin: row.try_get::<bool, _>("is_admin").map_err(be)?,
        created_at_ms: u(row, "created_at_ms")?,
    })
}

fn device_from(row: &PgRow) -> StoreResult<Device> {
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

fn session_from(row: &PgRow) -> StoreResult<WebSession> {
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

fn invite_from(row: &PgRow) -> StoreResult<Invite> {
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

fn preview_token_from(row: &PgRow) -> StoreResult<PreviewToken> {
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

/// 游标分页的 `after` 拆成两个可空参数（没有游标时两个都是 `NULL`，条件恒真）。
fn cursor(after: Option<(u64, String)>) -> (Option<i64>, Option<String>) {
    match after {
        Some((ms, id)) => (Some(ms as i64), Some(id)),
        None => (None, None),
    }
}

#[derive(Clone)]
pub struct PostgresStore {
    pub(super) pool: PgPool,
}

impl PostgresStore {
    /// 连接数据库并跑完迁移。`database_url` 是 `postgres://…` 或 `postgresql://…`（libpq 风格的连接 URL，
    /// `sslmode`、`sslrootcert` 等参数写在查询串里）。连不上、认证失败、迁移失败都返回带原因的错误（错误里不带密码）。
    pub async fn open(database_url: &str) -> StoreResult<Self> {
        let options = PgConnectOptions::from_str(database_url).map_err(|e| {
            StoreError::Backend(format!("ANYCON_DATABASE_URL is not a valid Postgres connection URL: {e}"))
        })?;
        Self::open_with(options, MAX_CONNECTIONS).await
    }

    pub(super) async fn open_with(options: PgConnectOptions, max_connections: u32) -> StoreResult<Self> {
        // 只描述「连的是哪里」（主机、端口、库名），不含用户名和密码，错误信息和日志里都可以放。
        let target = format!("{}:{}/{}", options.get_host(), options.get_port(), options.get_database().unwrap_or(""));
        let options = options.application_name("anycon-hub");
        // 先单独连一次：池在连不上时会一直重试到 `acquire_timeout`，最后只报「池超时」，看不到真正的原因（拒绝连接、
        // 认证失败、不支持 TLS……）。直接连一次，错误就是驱动给的那个。
        match tokio::time::timeout(ACQUIRE_TIMEOUT, PgConnection::connect_with(&options)).await {
            Err(_) => {
                return Err(StoreError::Backend(format!(
                    "cannot connect to Postgres at {target}: no answer within {} seconds",
                    ACQUIRE_TIMEOUT.as_secs()
                )))
            }
            Ok(Err(e)) => return Err(StoreError::Backend(format!("cannot connect to Postgres at {target}: {e}"))),
            Ok(Ok(conn)) => {
                let _ = conn.close().await;
            }
        }
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::Backend(format!("cannot connect to Postgres at {target}: {e}")))?;
        sqlx::migrate!("./migrations/postgres").run(&pool).await.map_err(|e| StoreError::Migrate(e.to_string()))?;
        tracing::info!(target = %target, "connected to Postgres, migrations are up to date");
        Ok(Self { pool })
    }
}

#[async_trait]
impl Store for PostgresStore {
    async fn ping(&self) -> StoreResult<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await.map_err(be)?;
        Ok(())
    }

    async fn stats(&self) -> StoreResult<StoreStats> {
        let accounts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM accounts").fetch_one(&self.pool).await.map_err(be)?;
        let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE revoked_at_ms IS NULL")
            .fetch_one(&self.pool)
            .await
            .map_err(be)?;
        let size: i64 = sqlx::query_scalar("SELECT pg_database_size(current_database())")
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
        sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES ($1, $2, $3, $4)")
            .bind(&id)
            .bind(nul(display_name))
            .bind(is_admin)
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
        let tx = async {
            let mut tx = self.pool.begin().await?;
            sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES ($1, $2, $3, $4)")
                .bind(&id)
                .bind(nul(display_name))
                .bind(is_admin)
                .bind(now_ms as i64)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO kv (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(kv_key)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await
        };
        tx.await.map_err(be)?;
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
            "INSERT INTO accounts (id, display_name, email, is_admin, created_at_ms) VALUES ($1, $2, $3, FALSE, $4)",
        )
        .bind(&id)
        .bind(nul(display_name))
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
            Err(e) if is_unique_violation(&e) => Ok(CreateAccountOutcome::EmailTaken),
            Err(e) => Err(be(e)),
        }
    }

    async fn get_account(&self, id: &str) -> StoreResult<Option<Account>> {
        let row = sqlx::query("SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn list_accounts(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Account>> {
        let (after_ms, after_id) = cursor(after);
        let rows = sqlx::query(
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts \
             WHERE ($1::BIGINT IS NULL OR (created_at_ms, id COLLATE \"C\") > ($1::BIGINT, $2::TEXT)) \
             ORDER BY created_at_ms, id COLLATE \"C\" LIMIT $3",
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
            "SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE is_admin \
             ORDER BY created_at_ms, id COLLATE \"C\" LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn update_display_name(&self, id: &str, display_name: &str) -> StoreResult<()> {
        sqlx::query("UPDATE accounts SET display_name = $1 WHERE id = $2")
            .bind(nul(display_name))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    // ---- 邮箱 ----

    async fn find_account_by_email(&self, email: &str) -> StoreResult<Option<Account>> {
        let row = sqlx::query("SELECT id, display_name, email, is_admin, created_at_ms FROM accounts WHERE email = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(account_from).transpose()
    }

    async fn set_account_email(&self, account_id: &str, email: Option<&str>) -> StoreResult<SetEmailOutcome> {
        let r = sqlx::query("UPDATE accounts SET email = $1 WHERE id = $2")
            .bind(email)
            .bind(account_id)
            .execute(&self.pool)
            .await;
        match r {
            Ok(_) => Ok(SetEmailOutcome::Done),
            Err(e) if is_unique_violation(&e) => Ok(SetEmailOutcome::Taken),
            Err(e) => Err(be(e)),
        }
    }

    async fn put_email_code(&self, c: EmailCode, keep: u32) -> StoreResult<()> {
        with_retry(|| self.put_email_code_tx(&c, keep)).await
    }

    async fn list_email_codes(&self, email: &str, purpose: EmailPurpose) -> StoreResult<Vec<EmailCode>> {
        let rows = sqlx::query(
            "SELECT email, account_id, code_hash, expires_at_ms, attempts, created_at_ms FROM email_codes \
             WHERE email = $1 AND purpose = $2 ORDER BY id",
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
                    attempts: attempts_from(r)?,
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
            "DELETE FROM email_codes WHERE email = $1 AND purpose = $2 AND account_id IS NOT DISTINCT FROM $3 \
             AND expires_at_ms <= $4",
        )
        .bind(email)
        .bind(purpose.as_str())
        .bind(account_id)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        let max = i32::try_from(max_attempts).unwrap_or(i32::MAX);
        with_retry(|| self.attempt_email_codes_tx(email, purpose, account_id, now_ms, max)).await
    }

    async fn take_email_code(&self, id: i64) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM email_codes WHERE id = $1").bind(id).execute(&self.pool).await.map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn purge_expired_email_codes(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM email_codes WHERE expires_at_ms <= $1")
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
            "INSERT INTO push_subscriptions (id, device_id, endpoint, p256dh, auth, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (device_id) DO UPDATE SET id = excluded.id, endpoint = excluded.endpoint, \
                 p256dh = excluded.p256dh, auth = excluded.auth, created_at_ms = excluded.created_at_ms",
        )
        .bind(&id)
        .bind(device_id)
        .bind(nul(endpoint))
        .bind(nul(p256dh))
        .bind(nul(auth))
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
             WHERE d.account_id = $1 AND d.revoked_at_ms IS NULL ORDER BY p.created_at_ms, p.id COLLATE \"C\"",
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
            "SELECT id, device_id, endpoint, p256dh, auth, created_at_ms FROM push_subscriptions WHERE device_id = $1",
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
            "DELETE FROM push_subscriptions WHERE id = $1 \
             AND device_id IN (SELECT id FROM devices WHERE account_id = $2)",
        )
        .bind(id)
        .bind(account_id)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_push_subscription_by_id(&self, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM push_subscriptions WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    // ---- 通知偏好 ----

    async fn get_notification_prefs(&self, account_id: &str) -> StoreResult<Vec<NotificationPref>> {
        let rows = sqlx::query("SELECT category, push, email FROM notification_prefs WHERE account_id = $1")
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
                push: r.try_get::<bool, _>("push").map_err(be)?,
                email: r.try_get::<bool, _>("email").map_err(be)?,
            });
        }
        Ok(out)
    }

    async fn set_notification_prefs(&self, account_id: &str, prefs: &[NotificationPref]) -> StoreResult<()> {
        // 按类别排序再写：同一个账号的两个并发更新以相同的顺序拿行锁，不会互相死锁。
        let mut prefs = prefs.to_vec();
        prefs.sort_by_key(|p| category_to_str(p.category));
        with_retry(|| self.set_notification_prefs_tx(account_id, &prefs)).await
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
        let label = nul(label);
        sqlx::query(
            "INSERT INTO account_tokens (id, account_id, token_hash, label, created_at_ms) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&id)
        .bind(account_id)
        .bind(token_hash)
        .bind(label.as_ref())
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(AccountToken { id, label: label.into_owned(), created_at_ms: now_ms, last_used_ms: None })
    }

    async fn list_account_tokens(&self, account_id: &str) -> StoreResult<Vec<AccountToken>> {
        let rows = sqlx::query(
            "SELECT id, label, created_at_ms, last_used_ms FROM account_tokens WHERE account_id = $1 \
             ORDER BY created_at_ms, id COLLATE \"C\"",
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
        let r = sqlx::query("DELETE FROM account_tokens WHERE id = $1 AND account_id = $2")
            .bind(id)
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn find_account_by_token_hash(&self, token_hash: &str, now_ms: u64) -> StoreResult<Option<Account>> {
        // 一条语句：刷新 `last_used_ms` 并取回账号（SQLite 版是先查后改，这里一步到位）。
        let row = sqlx::query(
            "UPDATE account_tokens t SET last_used_ms = $2 FROM accounts a \
             WHERE a.id = t.account_id AND t.token_hash = $1 \
             RETURNING a.id, a.display_name, a.email, a.is_admin, a.created_at_ms",
        )
        .bind(token_hash)
        .bind(now_ms as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.as_ref().map(account_from).transpose()
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&id)
        .bind(account_id)
        .bind(token_hash)
        .bind(nul(user_agent))
        .bind(nul(ip_prefix))
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
             WHERE s.token_hash = $1 AND s.expires_at_ms > $2",
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
            is_admin: row.try_get::<bool, _>("is_admin").map_err(be)?,
            created_at_ms: u(&row, "account_created_at_ms")?,
        };
        Ok(Some((session, account)))
    }

    async fn touch_web_session(&self, id: &str, now_ms: u64, expires_at_ms: u64) -> StoreResult<()> {
        sqlx::query("UPDATE web_sessions SET last_seen_ms = $1, expires_at_ms = $2 WHERE id = $3")
            .bind(now_ms as i64)
            .bind(expires_at_ms as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn list_web_sessions(&self, account_id: &str, now_ms: u64) -> StoreResult<Vec<WebSession>> {
        let rows = sqlx::query(session_select!(
            "WHERE account_id = $1 AND expires_at_ms > $2 ORDER BY last_seen_ms DESC, id COLLATE \"C\""
        ))
        .bind(account_id)
        .bind(now_ms as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(session_from).collect()
    }

    async fn delete_web_session(&self, account_id: &str, id: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE id = $1 AND account_id = $2")
            .bind(id)
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_web_session_by_hash(&self, token_hash: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn delete_web_sessions_of_account(&self, account_id: &str) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE account_id = $1")
            .bind(account_id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    async fn purge_expired_web_sessions(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM web_sessions WHERE expires_at_ms <= $1")
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
        let note = nul_opt(note);
        sqlx::query(
            "INSERT INTO invites (id, code_hash, created_by, note, created_at_ms, expires_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(code_hash)
        .bind(created_by)
        .bind(note.as_deref())
        .bind(now_ms as i64)
        .bind(expires_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(Invite {
            id,
            created_by: created_by.into(),
            note: note.map(Cow::into_owned),
            created_at_ms: now_ms,
            expires_at_ms,
            used_by: None,
            used_at_ms: None,
        })
    }

    async fn list_invites(&self, limit: u32, after: Option<(u64, String)>) -> StoreResult<Vec<Invite>> {
        // keyset 分页：`(created_at_ms, id)` 严格小于上一页最后一项。没有游标时条件恒真。
        let (after_ms, after_id) = cursor(after);
        let rows = sqlx::query(
            "SELECT id, created_by, note, created_at_ms, expires_at_ms, used_by, used_at_ms FROM invites \
             WHERE ($1::BIGINT IS NULL OR (created_at_ms, id COLLATE \"C\") < ($1::BIGINT, $2::TEXT)) \
             ORDER BY created_at_ms DESC, id COLLATE \"C\" DESC LIMIT $3",
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
        let r = sqlx::query("DELETE FROM invites WHERE id = $1").bind(id).execute(&self.pool).await.map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    async fn redeem_invite(&self, code_hash: &str, display_name: &str, now_ms: u64) -> StoreResult<RedeemOutcome> {
        with_retry(|| self.redeem_invite_tx(code_hash, display_name, now_ms)).await
    }

    // ---- 设备 ----

    async fn create_device(&self, d: NewDevice, now_ms: u64) -> StoreResult<Device> {
        let (name, platform, origin) = (nul(&d.name), nul(&d.platform), nul_opt(d.origin.as_deref()));
        sqlx::query(
            "INSERT INTO devices (id, account_id, kind, name, platform, origin, token_hash, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&d.id)
        .bind(&d.account_id)
        .bind(kind_to_str(d.kind))
        .bind(name.as_ref())
        .bind(platform.as_ref())
        .bind(origin.as_deref())
        .bind(&d.token_hash)
        .bind(now_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(Device {
            id: d.id,
            account_id: d.account_id,
            kind: d.kind,
            name: name.into_owned(),
            platform: platform.into_owned(),
            origin: origin.map(Cow::into_owned),
            created_at_ms: now_ms,
            last_seen_ms: None,
            revoked_at_ms: None,
        })
    }

    async fn get_device(&self, id: &str) -> StoreResult<Option<Device>> {
        let row = sqlx::query(device_select!("WHERE id = $1")).bind(id).fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(device_from).transpose()
    }

    async fn find_device_by_token_hash(&self, token_hash: &str) -> StoreResult<Option<Device>> {
        let row = sqlx::query(device_select!("WHERE token_hash = $1"))
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(device_from).transpose()
    }

    async fn list_devices(&self, account_id: &str) -> StoreResult<Vec<Device>> {
        let rows = sqlx::query(device_select!(
            "WHERE account_id = $1 AND revoked_at_ms IS NULL ORDER BY created_at_ms, id COLLATE \"C\""
        ))
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(device_from).collect()
    }

    async fn rename_device(&self, id: &str, name: &str) -> StoreResult<()> {
        sqlx::query("UPDATE devices SET name = $1 WHERE id = $2")
            .bind(nul(name))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn touch_device_seen(&self, id: &str, now_ms: u64) -> StoreResult<()> {
        sqlx::query("UPDATE devices SET last_seen_ms = $1 WHERE id = $2")
            .bind(now_ms as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }

    async fn revoke_device(&self, id: &str, now_ms: u64) -> StoreResult<Vec<String>> {
        with_retry(|| self.revoke_device_tx(id, now_ms)).await
    }

    // ---- 配对 ----

    async fn insert_pairing(&self, a: &str, b: &str, now_ms: u64) -> StoreResult<()> {
        let (a, b) = ordered(a, b);
        sqlx::query(
            "INSERT INTO pairings (device_a, device_b, created_at_ms) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
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
            "SELECT device_b FROM pairings WHERE device_a = $1 UNION SELECT device_a FROM pairings WHERE device_b = $1",
        )
        .bind(device_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)
    }

    async fn is_paired(&self, a: &str, b: &str) -> StoreResult<bool> {
        let (a, b) = ordered(a, b);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairings WHERE device_a = $1 AND device_b = $2")
            .bind(a)
            .bind(b)
            .fetch_one(&self.pool)
            .await
            .map_err(be)?;
        Ok(n > 0)
    }

    async fn list_pairings_of_account(&self, account_id: &str) -> StoreResult<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT p.device_a, p.device_b FROM pairings p JOIN devices d ON d.id = p.device_a WHERE d.account_id = $1",
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
            "INSERT INTO hub_audit (ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(now_ms as i64)
        .bind(e.event)
        .bind(e.account_id)
        .bind(e.device_id)
        .bind(e.ip_prefix.as_deref().map(nul))
        .bind(e.user_agent.as_deref().map(nul))
        .bind(detail)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(())
    }

    async fn list_audit(&self, limit: u32, before_id: Option<i64>) -> StoreResult<Vec<AuditEntry>> {
        let rows = sqlx::query(
            "SELECT id, ts_ms, event, account_id, device_id, ip_prefix, user_agent, detail FROM hub_audit \
             WHERE ($1::BIGINT IS NULL OR id < $1) ORDER BY id DESC LIMIT $2",
        )
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
        let r = sqlx::query("DELETE FROM hub_audit WHERE ts_ms < $1")
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
        with_retry(|| self.issue_preview_token_tx(&token, now_ms, limits)).await
    }

    async fn find_preview_token(&self, token_hash: &str) -> StoreResult<Option<PreviewToken>> {
        let row = sqlx::query(preview_token_select!("WHERE token_hash = $1"))
            .bind(token_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(preview_token_from).transpose()
    }

    async fn preview_token_valid(&self, token_hash: &str, now_ms: u64) -> StoreResult<bool> {
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM preview_tokens WHERE token_hash = $1 AND expires_at_ms > $2")
                .bind(token_hash)
                .bind(now_ms as i64)
                .fetch_optional(&self.pool)
                .await
                .map_err(be)?;
        Ok(found.is_some())
    }

    async fn purge_expired_preview_tokens(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM preview_tokens WHERE expires_at_ms <= $1")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }

    async fn list_preview_labels(&self, node_id: &str) -> StoreResult<Vec<String>> {
        sqlx::query_scalar("SELECT label FROM preview_labels WHERE node_id = $1 ORDER BY created_at_ms, seq")
            .bind(node_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)
    }

    async fn list_preview_tokens_of_device(&self, device_id: &str) -> StoreResult<Vec<PreviewToken>> {
        let rows = sqlx::query(preview_token_select!("WHERE device_id = $1 ORDER BY created_at_ms, seq"))
            .bind(device_id)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter().map(preview_token_from).collect()
    }

    // ---- kv ----

    async fn kv_get(&self, key: &str) -> StoreResult<Option<String>> {
        sqlx::query_scalar("SELECT value FROM kv WHERE key = $1").bind(key).fetch_optional(&self.pool).await.map_err(be)
    }

    async fn kv_set(&self, key: &str, value: &str) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO kv (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(nul(value))
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
        with_retry(|| self.create_devices_within_limits_tx(&devices, limits, now_ms)).await
    }

    async fn set_account_admin(&self, account_id: &str, is_admin: bool) -> StoreResult<bool> {
        self.set_account_admin_impl(account_id, is_admin).await
    }

    async fn delete_account(&self, account_id: &str, tombstone: NewAudit, now_ms: u64) -> StoreResult<bool> {
        with_retry(|| self.delete_account_tx(account_id, &tombstone, now_ms)).await
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

/// [`PostgresStore::put_email_code_tx`] 串行化同一组（邮箱，用途，账号）的登记用的 advisory 锁的键（文本，交给 `hashtextextended`）。
fn email_group_key(email: &str, purpose: EmailPurpose, account_id: Option<&str>) -> String {
    format!("anycon:email_codes\u{1}{email}\u{1}{}\u{1}{}", purpose.as_str(), account_id.unwrap_or(""))
}

/// `email_codes.attempts` 是 `INTEGER`（i32）。
fn attempts_from(row: &PgRow) -> StoreResult<u32> {
    let v: i32 = row.try_get("attempts").map_err(be)?;
    Ok(v.max(0) as u32)
}

// ---------------------------------------------------------------------------
// 事务体：`Store` 的方法用 [`with_retry`] 包着调它们
// ---------------------------------------------------------------------------

impl PostgresStore {
    async fn put_email_code_tx(&self, c: &EmailCode, keep: u32) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // 这一组（邮箱，用途，账号）的并发登记排队：行锁锁不住「还没有的行」，用 advisory 事务锁；
        // 之后「清过期 → 插入 → 只留最新的 keep 条」看到的是前一个登记已经提交的结果。
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(email_group_key(&c.email, c.purpose, c.account_id.as_deref()))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM email_codes WHERE email = $1 AND purpose = $2 AND account_id IS NOT DISTINCT FROM $3 \
             AND expires_at_ms <= $4",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(c.created_at_ms as i64)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO email_codes (email, purpose, account_id, code_hash, expires_at_ms, attempts, created_at_ms) \
             VALUES ($1, $2, $3, $4, $5, 0, $6)",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(&c.code_hash)
        .bind(c.expires_at_ms as i64)
        .bind(c.created_at_ms as i64)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM email_codes WHERE email = $1 AND purpose = $2 AND account_id IS NOT DISTINCT FROM $3 \
             AND id NOT IN (SELECT id FROM email_codes WHERE email = $1 AND purpose = $2 \
                            AND account_id IS NOT DISTINCT FROM $3 ORDER BY id DESC LIMIT $4)",
        )
        .bind(&c.email)
        .bind(c.purpose.as_str())
        .bind(&c.account_id)
        .bind(i64::from(keep.max(1)))
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// 计数和取码是一个原子操作：先锁住这一组里还能猜的码（拿到锁之后 Postgres 在最新版本上重新判断 `attempts < max`，
    /// 别的并发提交刚把次数用完的码不会出现在这里），再对锁住的这些行各加一次。并发的提交一共只能拿到 `max_attempts` 次比较的机会。
    async fn attempt_email_codes_tx(
        &self,
        email: &str,
        purpose: EmailPurpose,
        account_id: Option<&str>,
        now_ms: u64,
        max: i32,
    ) -> Result<Vec<CodeCandidate>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM email_codes \
             WHERE email = $1 AND purpose = $2 AND account_id IS NOT DISTINCT FROM $3 \
               AND expires_at_ms > $4 AND attempts < $5 ORDER BY id FOR UPDATE",
        )
        .bind(email)
        .bind(purpose.as_str())
        .bind(account_id)
        .bind(now_ms as i64)
        .bind(max)
        .fetch_all(&mut *tx)
        .await?;
        if ids.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "UPDATE email_codes SET attempts = attempts + 1 WHERE id = ANY($1) AND attempts < $2 \
             RETURNING id, code_hash, attempts",
        )
        .bind(&ids)
        .bind(max)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut out = rows
            .iter()
            .map(|r| {
                Ok(CodeCandidate {
                    id: r.try_get("id")?,
                    code_hash: r.try_get("code_hash")?,
                    attempts: attempts_from(r).map_err(|e| sqlx::Error::Decode(e.to_string().into()))?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()?;
        out.sort_by_key(|c| c.id);
        Ok(out)
    }

    async fn set_notification_prefs_tx(&self, account_id: &str, prefs: &[NotificationPref]) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for p in prefs {
            sqlx::query(
                "INSERT INTO notification_prefs (account_id, category, push, email) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (account_id, category) DO UPDATE SET push = excluded.push, email = excluded.email",
            )
            .bind(account_id)
            .bind(category_to_str(p.category))
            .bind(p.push)
            .bind(p.email)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// 校验邀请 + 建账号 + 标记已用，一个事务。「未用 + 未过期」的判断就是那一条 `UPDATE` 的 `WHERE`：并发兑换同一个邀请时，
    /// 第二条 `UPDATE` 等第一条提交，再在新版本上判断，`used_by IS NULL` 不成立就是 0 行。
    async fn redeem_invite_tx(
        &self,
        code_hash: &str,
        display_name: &str,
        now_ms: u64,
    ) -> Result<RedeemOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let account_id = new_id();
        sqlx::query("INSERT INTO accounts (id, display_name, is_admin, created_at_ms) VALUES ($1, $2, FALSE, $3)")
            .bind(&account_id)
            .bind(nul(display_name))
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await?;
        let redeemed = sqlx::query(
            "UPDATE invites SET used_by = $1, used_at_ms = $2 \
             WHERE code_hash = $3 AND used_by IS NULL AND expires_at_ms > $2",
        )
        .bind(&account_id)
        .bind(now_ms as i64)
        .bind(code_hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if redeemed == 1 {
            tx.commit().await?;
            return Ok(RedeemOutcome::Redeemed(Account {
                id: account_id,
                display_name: display_name.into(),
                email: None,
                is_admin: false,
                created_at_ms: now_ms,
            }));
        }
        tx.rollback().await?;
        let row = sqlx::query("SELECT used_by FROM invites WHERE code_hash = $1")
            .bind(code_hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            None => RedeemOutcome::NotFound,
            Some(r) if r.try_get::<Option<String>, _>("used_by")?.is_some() => RedeemOutcome::Used,
            Some(_) => RedeemOutcome::Expired,
        })
    }

    /// 吊销：先 `FOR UPDATE` 锁住设备行。它和并发的配对 / 令牌签发（外键检查对设备行拿 `FOR KEY SHARE`）互斥：
    /// 要么那些事务先提交、下面的删除能看到它们写的行，要么它们等这里提交。已经吊销的设备在锁等待之后重新判断，返回空。
    async fn revoke_device_tx(&self, id: &str, now_ms: u64) -> Result<Vec<String>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let live: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM devices WHERE id = $1 AND revoked_at_ms IS NULL FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if live.is_none() {
            tx.rollback().await?;
            return Ok(Vec::new());
        }
        sqlx::query("UPDATE devices SET revoked_at_ms = $1 WHERE id = $2")
            .bind(now_ms as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        let removed =
            sqlx::query("DELETE FROM pairings WHERE device_a = $1 OR device_b = $1 RETURNING device_a, device_b")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        let mut peers = Vec::with_capacity(removed.len());
        for r in &removed {
            let (a, b): (String, String) = (r.try_get("device_a")?, r.try_get("device_b")?);
            peers.push(if a == id { b } else { a });
        }
        peers.sort();
        // 被吊销的设备不再收推送（ADR-37）。
        sqlx::query("DELETE FROM push_subscriptions WHERE device_id = $1").bind(id).execute(&mut *tx).await?;
        // 预览（ADR-39）：它签发的令牌作废；它是节点的话，全部 label 和用到这些 label 的令牌也删。
        sqlx::query("DELETE FROM preview_tokens WHERE device_id = $1 OR node_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM preview_labels WHERE node_id = $1").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(peers)
    }

    /// 绑定 label（第一次见到时）+ 签发令牌 + 清理这台设备的旧令牌，一个事务。
    ///
    /// 先锁住节点和签发设备的 `devices` 行（`FOR NO KEY UPDATE`，按 id 排序）：同一个节点的并发签发排队，
    /// 「节点已绑定几个 label」不会被一起数过；同一台设备的并发签发排队，「只留最新几条」不会漏删；
    /// 它和吊销（`FOR UPDATE`）冲突，所以吊销不会漏掉一条刚签发的令牌。
    async fn issue_preview_token_tx(
        &self,
        token: &NewPreviewToken,
        now_ms: u64,
        limits: PreviewIssueLimits,
    ) -> Result<IssueOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let lock_ids = vec![token.node_id.clone(), token.device_id.clone()];
        sqlx::query("SELECT id FROM devices WHERE id = ANY($1) ORDER BY id COLLATE \"C\" FOR NO KEY UPDATE")
            .bind(&lock_ids)
            .fetch_all(&mut *tx)
            .await?;
        // label 已存在就不动；是新的，而且节点还没绑满，才插入。label 的先到先得靠主键：并发的另一个节点同时插入同一个
        // label 时，后到的等先到的提交，然后什么都不做。
        sqlx::query(
            "INSERT INTO preview_labels (label, node_id, account_id, created_at_ms) \
             SELECT $1, $2, $3, $4 WHERE (SELECT COUNT(*) FROM preview_labels WHERE node_id = $2) < $5 \
             ON CONFLICT (label) DO NOTHING",
        )
        .bind(&token.label)
        .bind(&token.node_id)
        .bind(&token.account_id)
        .bind(now_ms as i64)
        .bind(i64::from(limits.labels_per_node))
        .execute(&mut *tx)
        .await?;
        let bound: Option<String> = sqlx::query_scalar("SELECT node_id FROM preview_labels WHERE label = $1")
            .bind(&token.label)
            .fetch_optional(&mut *tx)
            .await?;
        match bound {
            None => {
                tx.rollback().await?;
                return Ok(IssueOutcome::TooManyLabels);
            }
            Some(node) if node != token.node_id => {
                tx.rollback().await?;
                return Ok(IssueOutcome::LabelTaken);
            }
            Some(_) => {}
        }
        sqlx::query("DELETE FROM preview_tokens WHERE device_id = $1 AND expires_at_ms <= $2")
            .bind(&token.device_id)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO preview_tokens (token_hash, label, node_id, account_id, device_id, created_at_ms, expires_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&token.token_hash)
        .bind(&token.label)
        .bind(&token.node_id)
        .bind(&token.account_id)
        .bind(&token.device_id)
        .bind(now_ms as i64)
        .bind(token.expires_at_ms as i64)
        .execute(&mut *tx)
        .await?;
        // 这台设备只留最新的 `tokens_per_device` 条（同一毫秒签发的按 `seq`，也就是插入顺序排先后）。
        sqlx::query(
            "DELETE FROM preview_tokens WHERE token_hash IN ( \
                 SELECT token_hash FROM preview_tokens WHERE device_id = $1 \
                 ORDER BY created_at_ms DESC, seq DESC OFFSET $2)",
        )
        .bind(&token.device_id)
        .bind(i64::from(limits.tokens_per_device))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(IssueOutcome::Issued)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use anycon_protocol::DeviceKind;
    use tokio::task::JoinHandle;

    use super::*;
    use crate::store::conformance::{self, backend_postgres, code_row, device, new_device, new_token, PREVIEW_LIMITS};
    use crate::store::DeviceLimit;
    use crate::util::{hash_token, now_ms};

    // 和 SQLite 共用的那一套（见 `store/conformance.rs`）；`ANYCON_TEST_POSTGRES_URL` 没设时直接通过。
    conformance::suite!(backend_postgres);

    // ---- 连接：下面两个不需要数据库，没设 ANYCON_TEST_POSTGRES_URL 也跑 ----

    #[tokio::test]
    async fn a_malformed_url_is_reported_without_echoing_it() {
        let err = PostgresStore::open("postgres://user:hunter2@host:notaport/db").await.err().expect("rejected");
        let msg = err.to_string();
        assert!(msg.contains("not a valid Postgres connection URL"), "{msg}");
        assert!(!msg.contains("hunter2"), "the password must not appear in the error: {msg}");
    }

    #[tokio::test]
    async fn an_unreachable_database_is_reported_with_its_address_but_not_the_password() {
        // 端口 1 上没有服务：连接被拒，立刻返回
        let err = PostgresStore::open("postgres://anycon:hunter2@127.0.0.1:1/hub").await.err().expect("unreachable");
        let msg = err.to_string();
        assert!(msg.contains("cannot connect to Postgres at 127.0.0.1:1/hub"), "{msg}");
        assert!(
            msg.contains("error communicating with database"),
            "the driver's own error is shown, not a pool timeout: {msg}"
        );
        assert!(!msg.contains("hunter2") && !msg.contains("anycon:"), "credentials must not appear: {msg}");
    }

    // ---- 以下需要 Postgres ----

    #[tokio::test]
    async fn opening_the_same_database_again_leaves_the_applied_migrations_alone() {
        let Some(db) = backend_postgres().await else { return };
        let options = (*db.pg().connect_options()).clone();
        let again = PostgresStore::open_with(options, 2).await.expect("a second open re-runs the migrator as a no-op");
        again.ping().await.unwrap();
        let applied: Vec<String> =
            sqlx::query_scalar("SELECT description FROM _sqlx_migrations WHERE success ORDER BY version")
                .fetch_all(db.pg())
                .await
                .unwrap();
        assert_eq!(applied, ["init", "preview", "identities"]);
    }

    #[tokio::test]
    async fn nul_characters_in_free_text_are_stripped_instead_of_failing_the_statement() {
        let Some(db) = backend_postgres().await else { return };
        let s = &db.store;
        let now = now_ms();
        let acc = s.create_account("a\0b", false, now).await.unwrap();
        assert_eq!(s.get_account(&acc.id).await.unwrap().unwrap().display_name, "ab");
        let mut dev = new_device(&acc.id, "dev", "t");
        dev.name = "lap\0top".into();
        dev.platform = "li\0nux".into();
        dev.origin = Some("https://x\0.example".into());
        let created = s.create_device(dev, now).await.unwrap();
        assert_eq!((created.name.as_str(), created.platform.as_str()), ("laptop", "linux"));
        let got = s.get_device("dev").await.unwrap().unwrap();
        assert_eq!(
            (got.name.as_str(), got.platform.as_str(), got.origin.as_deref()),
            ("laptop", "linux", Some("https://x.example"))
        );
        let token = s.create_account_token(&acc.id, "la\0bel", &hash_token("tok"), now).await.unwrap();
        assert_eq!(token.label, "label");
        let invite = s.create_invite(&acc.id, &hash_token("i"), Some("no\0te"), now, now + 1000).await.unwrap();
        assert_eq!(invite.note.as_deref(), Some("note"));
        s.create_web_session(&acc.id, &hash_token("c"), "u\0a", "ip", now, now + 1000).await.unwrap();
        let (session, _) = s.find_web_session(&hash_token("c"), now).await.unwrap().unwrap();
        assert_eq!(session.user_agent, "ua");
    }

    #[tokio::test]
    async fn the_pool_replaces_connections_the_server_has_closed() {
        let Some(db) = backend_postgres().await else { return };
        let pool = db.pg().clone();
        // 取出池里的两条连接，记下服务器端的进程号，再放回去
        let (mut a, mut b) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
        let mut pids: Vec<i32> = Vec::new();
        for conn in [&mut a, &mut b] {
            pids.push(sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut **conn).await.unwrap());
        }
        drop((a, b));
        // 另开一条连接把它们踢掉（模拟数据库重启 / 网络断开）
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&conformance::postgres_url().unwrap())
            .await
            .unwrap();
        for pid in pids {
            sqlx::query("SELECT pg_terminate_backend($1)").bind(pid).execute(&admin).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        // 池发现空闲连接已死，换新的：不需要重启 hub
        db.store.ping().await.expect("the pool connects again");
        db.store.create_account("after the outage", false, now_ms()).await.unwrap();
    }

    // ---- 锁：对着一个手工开着的事务，确认这些操作真的在等锁，放开以后才完成 ----

    /// 等一小会儿，确认 `task` 被挡住了（还没做完）。
    async fn assert_waiting<T>(task: &JoinHandle<T>) {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!task.is_finished(), "the operation should be waiting for a lock held by another transaction");
    }

    async fn finishes<T>(task: JoinHandle<T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), task).await.expect("finished once the lock was released").unwrap()
    }

    #[tokio::test]
    async fn creating_devices_waits_for_the_account_row_lock_and_counts_after_it() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        let acc = s.create_account("a", false, now).await.unwrap();
        let limits = DeviceLimits { devices: 1, daemons: 1 };
        // 另一个「建设备」的事务已经锁住了这个账号，并且插了唯一的一个名额
        let mut other = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM accounts WHERE id = $1 FOR NO KEY UPDATE")
            .bind(&acc.id)
            .execute(&mut *other)
            .await
            .unwrap();
        sqlx::query("INSERT INTO devices (id, account_id, kind, name, token_hash, created_at_ms) VALUES ('first', $1, 'web', 'n', 'h', 1)")
            .bind(&acc.id)
            .execute(&mut *other)
            .await
            .unwrap();
        let task = tokio::spawn({
            let (s, account) = (s.clone(), acc.id.clone());
            async move {
                s.create_devices_within_limits(vec![device(&account, "second", DeviceKind::Web)], limits, now).await
            }
        });
        assert_waiting(&task).await;
        other.commit().await.unwrap();
        // 拿到锁之后才数：数到了刚提交的那一台，名额已满
        let outcome = finishes(task).await.unwrap();
        assert!(matches!(outcome, CreateDevicesOutcome::Exceeded(DeviceLimit::Devices)), "{outcome:?}");
        assert!(s.get_device("second").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn issuing_a_preview_token_waits_for_a_lock_on_the_node_row() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        let acc = s.create_account("a", false, now).await.unwrap();
        for (id, kind) in [("n1", DeviceKind::Daemon), ("w", DeviceKind::Web)] {
            s.create_device(NewDevice { kind, ..new_device(&acc.id, id, &format!("t-{id}")) }, now).await.unwrap();
        }
        // 吊销节点的事务（`FOR UPDATE`）正在进行
        let mut revoking = pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM devices WHERE id = 'n1' FOR UPDATE").execute(&mut *revoking).await.unwrap();
        let task = tokio::spawn({
            let (s, account) = (s.clone(), acc.id.clone());
            async move {
                s.issue_preview_token(new_token("h", "l", "n1", "w", &account, now + 1000), now, PREVIEW_LIMITS).await
            }
        });
        assert_waiting(&task).await;
        revoking.rollback().await.unwrap();
        assert_eq!(finishes(task).await.unwrap(), IssueOutcome::Issued);
    }

    #[tokio::test]
    async fn registering_an_email_code_waits_for_the_advisory_lock_of_its_group() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        let mut other = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(email_group_key("x@example.com", EmailPurpose::Login, None))
            .execute(&mut *other)
            .await
            .unwrap();
        let task = tokio::spawn({
            let s = s.clone();
            async move { s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h", now), 3).await }
        });
        assert_waiting(&task).await;
        // 别的组不受影响
        s.put_email_code(code_row("y@example.com", EmailPurpose::Login, None, "h", now), 3).await.unwrap();
        other.commit().await.unwrap();
        finishes(task).await.unwrap();
        assert_eq!(s.list_email_codes("x@example.com", EmailPurpose::Login).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_attempt_waits_for_the_rows_another_attempt_is_counting() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h", now), 3).await.unwrap();
        let mut other = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM email_codes WHERE email = 'x@example.com' FOR UPDATE")
            .execute(&mut *other)
            .await
            .unwrap();
        sqlx::query("UPDATE email_codes SET attempts = 4 WHERE email = 'x@example.com'")
            .execute(&mut *other)
            .await
            .unwrap();
        let task = tokio::spawn({
            let s = s.clone();
            async move { s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now, 5).await }
        });
        assert_waiting(&task).await;
        other.commit().await.unwrap();
        // 前一个提交把次数加到了 4：这一次是第 5 次，仍然交出来；再来一次就没有了
        let got = finishes(task).await.unwrap();
        assert_eq!(got.iter().map(|c| c.attempts).collect::<Vec<_>>(), [5]);
        assert!(s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now, 5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn revoking_waits_for_a_pairing_that_is_being_written_and_then_removes_it() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        let acc = s.create_account("a", false, now).await.unwrap();
        s.create_device(new_device(&acc.id, "a", "ta"), now).await.unwrap();
        s.create_device(new_device(&acc.id, "b", "tb"), now).await.unwrap();
        // 配对已经插入、还没提交
        let mut pairing = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO pairings (device_a, device_b, created_at_ms) VALUES ('a', 'b', 1)")
            .execute(&mut *pairing)
            .await
            .unwrap();
        let task = tokio::spawn({
            let s = s.clone();
            async move { s.revoke_device("a", now + 1).await }
        });
        assert_waiting(&task).await;
        pairing.commit().await.unwrap();
        // 吊销在配对提交之后才做删除，所以删得到它，也报得出对端
        assert_eq!(finishes(task).await.unwrap(), ["b"]);
        assert!(!s.is_paired("a", "b").await.unwrap());
    }

    #[tokio::test]
    async fn deleting_an_account_that_another_transaction_is_deleting_reports_false_and_writes_nothing() {
        let Some(db) = backend_postgres().await else { return };
        let (s, pool) = (db.store.clone(), db.pg().clone());
        let now = now_ms();
        let acc = s.create_account("a", false, now).await.unwrap();
        let mut other = pool.begin().await.unwrap();
        sqlx::query("DELETE FROM accounts WHERE id = $1").bind(&acc.id).execute(&mut *other).await.unwrap();
        let task = tokio::spawn({
            let (s, id) = (s.clone(), acc.id.clone());
            async move {
                let tombstone =
                    NewAudit { event: "account_deleted", account_id: Some(id.clone()), ..Default::default() };
                s.delete_account(&id, tombstone, now).await
            }
        });
        assert_waiting(&task).await;
        other.commit().await.unwrap();
        assert!(!finishes(task).await.unwrap(), "the account was already gone when the lock was released");
        assert!(s.list_audit(10, None).await.unwrap().is_empty(), "no tombstone for a deletion that did not happen");
    }

    // ---- 死锁重试 ----

    #[tokio::test]
    async fn a_transaction_aborted_by_a_real_deadlock_is_retried_and_both_sides_finish() {
        let Some(db) = backend_postgres().await else { return };
        let pool = db.pg().clone();
        sqlx::query("CREATE TABLE lock_test (id INT PRIMARY KEY)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO lock_test VALUES (1), (2)").execute(&pool).await.unwrap();
        let both_hold_one = Arc::new(tokio::sync::Barrier::new(2));
        let runs = Arc::new(AtomicU32::new(0));
        let side = |first: i32, second: i32| {
            let (pool, barrier, runs) = (pool.clone(), both_hold_one.clone(), runs.clone());
            tokio::spawn(async move {
                let first_run = AtomicBool::new(true);
                with_retry(|| async {
                    runs.fetch_add(1, Ordering::SeqCst);
                    let mut tx = pool.begin().await?;
                    sqlx::query("SELECT id FROM lock_test WHERE id = $1 FOR UPDATE")
                        .bind(first)
                        .execute(&mut *tx)
                        .await?;
                    // 第一次两边各握一把锁再去拿对方的（死锁）；重试时对方已经回滚，不再等
                    if first_run.swap(false, Ordering::SeqCst) {
                        barrier.wait().await;
                    }
                    sqlx::query("SELECT id FROM lock_test WHERE id = $1 FOR UPDATE")
                        .bind(second)
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await
                })
                .await
            })
        };
        let (a, b) = (side(1, 2), side(2, 1));
        finishes(a).await.expect("side a finishes");
        finishes(b).await.expect("side b finishes");
        assert!(runs.load(Ordering::SeqCst) >= 3, "one side was aborted by the deadlock detector and ran again");
    }

    #[tokio::test]
    async fn only_deadlocks_and_serialization_failures_are_retried() {
        let attempts = AtomicU32::new(0);
        let err = with_retry(|| async {
            attempts.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(sqlx::Error::RowNotFound)
        })
        .await
        .unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "other errors are returned at once");
    }
}
