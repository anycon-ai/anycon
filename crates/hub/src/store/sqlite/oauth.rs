//! OAuth 身份与授权 state 的 SQLite 实现（`identities`、`oauth_states`，迁移 `0003_identities.sql`）。
//!
//! [`Store`](crate::store::Store) 里对应的方法只是转调这里的 `oauth_*`，实现集中在这一个文件里。

use sqlx::sqlite::SqliteRow;

use super::{be, s, s_opt, u, SqliteStore};
use crate::store::{
    Account, Identity, IdentityAccountOutcome, LinkIdentityOutcome, NewIdentityAccount, NewOAuthState, OAuthPurpose,
    OAuthState, StoreResult,
};
use crate::util::new_id;

fn identity_from(row: &SqliteRow) -> StoreResult<Identity> {
    Ok(Identity {
        provider: s(row, "provider")?,
        subject: s(row, "subject")?,
        account_id: s(row, "account_id")?,
        created_at_ms: u(row, "created_at_ms")?,
    })
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

impl SqliteStore {
    pub(super) async fn oauth_find_identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>> {
        let row = sqlx::query(
            "SELECT provider, subject, account_id, created_at_ms FROM identities WHERE provider = ? AND subject = ?",
        )
        .bind(provider)
        .bind(subject)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        row.as_ref().map(identity_from).transpose()
    }

    pub(super) async fn oauth_list_identities(&self, account_id: &str) -> StoreResult<Vec<Identity>> {
        let rows = sqlx::query(
            "SELECT provider, subject, account_id, created_at_ms FROM identities WHERE account_id = ? \
             ORDER BY created_at_ms, provider",
        )
        .bind(account_id)
        .fetch_all(&self.pool)
        .await
        .map_err(be)?;
        rows.iter().map(identity_from).collect()
    }

    pub(super) async fn oauth_link_identity(
        &self,
        account_id: &str,
        provider: &str,
        subject: &str,
        now_ms: u64,
    ) -> StoreResult<LinkIdentityOutcome> {
        // 一个事务：插入（两条唯一约束任何一条冲突都当没插）→ 没插进去就查是哪一种冲突。
        let mut tx = self.pool.begin().await.map_err(be)?;
        let inserted = sqlx::query(
            "INSERT INTO identities (provider, subject, account_id, created_at_ms) VALUES (?, ?, ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(provider)
        .bind(subject)
        .bind(account_id)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await
        .map_err(be)?
        .rows_affected();
        if inserted == 1 {
            tx.commit().await.map_err(be)?;
            return Ok(LinkIdentityOutcome::Linked);
        }
        // 先看这个外部身份在谁名下（同一个账号 = 重复绑定，别的账号 = 被占用），没有就一定是这个账号在该提供方已有别的身份。
        let owner: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM identities WHERE provider = ? AND subject = ?")
                .bind(provider)
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await
                .map_err(be)?;
        tx.commit().await.map_err(be)?;
        Ok(match owner {
            Some(owner) if owner == account_id => LinkIdentityOutcome::AlreadyLinked,
            Some(_) => LinkIdentityOutcome::SubjectTaken,
            None => LinkIdentityOutcome::ProviderTaken,
        })
    }

    pub(super) async fn oauth_unlink_identity(&self, account_id: &str, provider: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM identities WHERE account_id = ? AND provider = ?")
            .bind(account_id)
            .bind(provider)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected() > 0)
    }

    pub(super) async fn oauth_create_account_with_identity(
        &self,
        new: NewIdentityAccount<'_>,
        now_ms: u64,
    ) -> StoreResult<IdentityAccountOutcome> {
        let mut tx = self.pool.begin().await.map_err(be)?;
        let id = new_id();
        let inserted = sqlx::query(
            "INSERT INTO accounts (id, display_name, email, is_admin, created_at_ms) VALUES (?, ?, ?, 0, ?)",
        )
        .bind(&id)
        .bind(new.display_name)
        .bind(new.email)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                tx.rollback().await.map_err(be)?;
                return Ok(IdentityAccountOutcome::EmailTaken);
            }
            Err(e) => return Err(be(e)),
        }
        // 邀请：和 `redeem_invite` 一样，一条 UPDATE 同时检查「未用 + 未过期」。
        if let Some(invite_hash) = new.invite_hash {
            let redeemed = sqlx::query(
                "UPDATE invites SET used_by = ?, used_at_ms = ? \
                 WHERE code_hash = ? AND used_by IS NULL AND expires_at_ms > ?",
            )
            .bind(&id)
            .bind(now_ms as i64)
            .bind(invite_hash)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(be)?
            .rows_affected();
            if redeemed != 1 {
                tx.rollback().await.map_err(be)?;
                let row = sqlx::query("SELECT used_by FROM invites WHERE code_hash = ?")
                    .bind(invite_hash)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(be)?;
                return Ok(match row {
                    None => IdentityAccountOutcome::InviteNotFound,
                    Some(r) if s_opt(&r, "used_by")?.is_some() => IdentityAccountOutcome::InviteUsed,
                    Some(_) => IdentityAccountOutcome::InviteExpired,
                });
            }
        }
        let linked =
            sqlx::query("INSERT INTO identities (provider, subject, account_id, created_at_ms) VALUES (?, ?, ?, ?)")
                .bind(new.provider)
                .bind(new.subject)
                .bind(&id)
                .bind(now_ms as i64)
                .execute(&mut *tx)
                .await;
        match linked {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                tx.rollback().await.map_err(be)?;
                return Ok(IdentityAccountOutcome::IdentityTaken);
            }
            Err(e) => return Err(be(e)),
        }
        tx.commit().await.map_err(be)?;
        Ok(IdentityAccountOutcome::Created(Account {
            id,
            display_name: new.display_name.to_string(),
            email: new.email.map(str::to_string),
            is_admin: false,
            created_at_ms: now_ms,
        }))
    }

    pub(super) async fn oauth_put_state(&self, state: NewOAuthState, now_ms: u64) -> StoreResult<()> {
        sqlx::query(
            "INSERT INTO oauth_states \
             (state_hash, provider, purpose, account_id, invite_hash, next_path, pkce_verifier, created_at_ms, expires_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&state.state_hash)
        .bind(&state.provider)
        .bind(state.purpose.as_str())
        .bind(&state.account_id)
        .bind(&state.invite_hash)
        .bind(&state.next_path)
        .bind(&state.pkce_verifier)
        .bind(now_ms as i64)
        .bind(state.expires_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(())
    }

    pub(super) async fn oauth_take_state(&self, state_hash: &str, now_ms: u64) -> StoreResult<Option<OAuthState>> {
        // 匿名请求只要带一对一致的 cookie 和 state 参数就能走到这里：先用只读查询挡掉不存在的 state，
        // 免得它们白白去抢写锁（`DELETE` 即使没有匹配的行也要拿写锁）。
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM oauth_states WHERE state_hash = ?")
            .bind(state_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        if exists.is_none() {
            return Ok(None);
        }
        // 一条 DELETE ... RETURNING：并发的两次回调只有一个能拿到这一行。过期的也删（它本来就该被清理），但不交出去。
        let row = sqlx::query(
            "DELETE FROM oauth_states WHERE state_hash = ? \
             RETURNING provider, purpose, account_id, invite_hash, next_path, pkce_verifier, created_at_ms, expires_at_ms",
        )
        .bind(state_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(be)?;
        let Some(row) = row else { return Ok(None) };
        let state = OAuthState {
            provider: s(&row, "provider")?,
            purpose: OAuthPurpose::parse(&s(&row, "purpose")?)?,
            account_id: s_opt(&row, "account_id")?,
            invite_hash: s_opt(&row, "invite_hash")?,
            next_path: s_opt(&row, "next_path")?,
            pkce_verifier: s(&row, "pkce_verifier")?,
            created_at_ms: u(&row, "created_at_ms")?,
            expires_at_ms: u(&row, "expires_at_ms")?,
        };
        Ok((state.expires_at_ms > now_ms).then_some(state))
    }

    pub(super) async fn oauth_purge_expired_states(&self, now_ms: u64) -> StoreResult<u64> {
        let r = sqlx::query("DELETE FROM oauth_states WHERE expires_at_ms <= ?")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }
}
