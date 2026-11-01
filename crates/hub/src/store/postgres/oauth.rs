//! OAuth 身份与授权 state 的 Postgres 实现（`identities`、`oauth_states`，迁移 `0003_identities.sql`）。
//!
//! [`Store`](crate::store::Store) 里对应的方法只是转调这里的 `oauth_*`，实现集中在这一个文件里。并发语义见 `postgres.rs` 开头的表。

use sqlx::postgres::PgRow;

use super::{be, is_unique_violation, s, s_opt, u, with_retry, PostgresStore};
use crate::store::{
    Account, Identity, IdentityAccountOutcome, LinkIdentityOutcome, NewIdentityAccount, NewOAuthState, OAuthPurpose,
    OAuthState, StoreResult,
};
use crate::util::new_id;

use super::{nul, nul_opt};

fn identity_from(row: &PgRow) -> StoreResult<Identity> {
    Ok(Identity {
        provider: s(row, "provider")?,
        subject: s(row, "subject")?,
        account_id: s(row, "account_id")?,
        created_at_ms: u(row, "created_at_ms")?,
    })
}

impl PostgresStore {
    pub(super) async fn oauth_find_identity(&self, provider: &str, subject: &str) -> StoreResult<Option<Identity>> {
        let row = sqlx::query(
            "SELECT provider, subject, account_id, created_at_ms FROM identities WHERE provider = $1 AND subject = $2",
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
            "SELECT provider, subject, account_id, created_at_ms FROM identities WHERE account_id = $1 \
             ORDER BY created_at_ms, provider COLLATE \"C\"",
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
        with_retry(|| self.oauth_link_identity_tx(account_id, provider, subject, now_ms)).await
    }

    /// 插入（两条唯一约束任何一条冲突都当没插：`ON CONFLICT DO NOTHING`，并发的另一个插入没提交时先等它）→ 没插进去就查是哪一种冲突。
    async fn oauth_link_identity_tx(
        &self,
        account_id: &str,
        provider: &str,
        subject: &str,
        now_ms: u64,
    ) -> Result<LinkIdentityOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO identities (provider, subject, account_id, created_at_ms) VALUES ($1, $2, $3, $4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(provider)
        .bind(subject)
        .bind(account_id)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if inserted == 1 {
            tx.commit().await?;
            return Ok(LinkIdentityOutcome::Linked);
        }
        // 先看这个外部身份在谁名下（同一个账号 = 重复绑定，别的账号 = 被占用），没有就一定是这个账号在该提供方已有别的身份。
        let owner: Option<String> =
            sqlx::query_scalar("SELECT account_id FROM identities WHERE provider = $1 AND subject = $2")
                .bind(provider)
                .bind(subject)
                .fetch_optional(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(match owner {
            Some(owner) if owner == account_id => LinkIdentityOutcome::AlreadyLinked,
            Some(_) => LinkIdentityOutcome::SubjectTaken,
            None => LinkIdentityOutcome::ProviderTaken,
        })
    }

    pub(super) async fn oauth_unlink_identity(&self, account_id: &str, provider: &str) -> StoreResult<bool> {
        let r = sqlx::query("DELETE FROM identities WHERE account_id = $1 AND provider = $2")
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
        with_retry(|| self.oauth_create_account_with_identity_tx(new, now_ms)).await
    }

    /// 建账号 + （带邀请码时）兑换邀请 + 绑身份，一个事务；任何一步失败都什么都不留。三处的并发竞争各有各的裁判：
    /// 邮箱和外部身份是唯一约束（后到的 `INSERT` 等先到的提交，再报冲突），邀请是一条带 `used_by IS NULL` 的 `UPDATE`。
    async fn oauth_create_account_with_identity_tx(
        &self,
        new: NewIdentityAccount<'_>,
        now_ms: u64,
    ) -> Result<IdentityAccountOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let id = new_id();
        let inserted = sqlx::query(
            "INSERT INTO accounts (id, display_name, email, is_admin, created_at_ms) VALUES ($1, $2, $3, FALSE, $4)",
        )
        .bind(&id)
        .bind(nul(new.display_name))
        .bind(new.email)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                tx.rollback().await?;
                return Ok(IdentityAccountOutcome::EmailTaken);
            }
            Err(e) => return Err(e),
        }
        // 邀请：和 `redeem_invite` 一样，一条 UPDATE 同时检查「未用 + 未过期」。
        if let Some(invite_hash) = new.invite_hash {
            let redeemed = sqlx::query(
                "UPDATE invites SET used_by = $1, used_at_ms = $2 \
                 WHERE code_hash = $3 AND used_by IS NULL AND expires_at_ms > $2",
            )
            .bind(&id)
            .bind(now_ms as i64)
            .bind(invite_hash)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if redeemed != 1 {
                tx.rollback().await?;
                let used_by: Option<Option<String>> =
                    sqlx::query_scalar("SELECT used_by FROM invites WHERE code_hash = $1")
                        .bind(invite_hash)
                        .fetch_optional(&self.pool)
                        .await?;
                return Ok(match used_by {
                    None => IdentityAccountOutcome::InviteNotFound,
                    Some(Some(_)) => IdentityAccountOutcome::InviteUsed,
                    Some(None) => IdentityAccountOutcome::InviteExpired,
                });
            }
        }
        let linked = sqlx::query(
            "INSERT INTO identities (provider, subject, account_id, created_at_ms) VALUES ($1, $2, $3, $4)",
        )
        .bind(new.provider)
        .bind(new.subject)
        .bind(&id)
        .bind(now_ms as i64)
        .execute(&mut *tx)
        .await;
        match linked {
            Ok(_) => {}
            Err(e) if is_unique_violation(&e) => {
                tx.rollback().await?;
                return Ok(IdentityAccountOutcome::IdentityTaken);
            }
            Err(e) => return Err(e),
        }
        tx.commit().await?;
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&state.state_hash)
        .bind(&state.provider)
        .bind(state.purpose.as_str())
        .bind(&state.account_id)
        .bind(&state.invite_hash)
        .bind(nul_opt(state.next_path.as_deref()))
        .bind(&state.pkce_verifier)
        .bind(now_ms as i64)
        .bind(state.expires_at_ms as i64)
        .execute(&self.pool)
        .await
        .map_err(be)?;
        Ok(())
    }

    pub(super) async fn oauth_take_state(&self, state_hash: &str, now_ms: u64) -> StoreResult<Option<OAuthState>> {
        // 一条 DELETE ... RETURNING：并发的两次回调只有一个能拿到这一行（第二条 `DELETE` 等第一条提交，找不到行）。
        // 过期的也删（它本来就该被清理），但不交出去。
        let row = sqlx::query(
            "DELETE FROM oauth_states WHERE state_hash = $1 \
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
        let r = sqlx::query("DELETE FROM oauth_states WHERE expires_at_ms <= $1")
            .bind(now_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(r.rows_affected())
    }
}
