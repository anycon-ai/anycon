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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::util::{hash_token, now_ms};

    async fn store() -> (SqliteStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let s = SqliteStore::open(None, dir.path()).await.unwrap();
        (s, dir)
    }

    fn state(hash: &str, provider: &str, purpose: OAuthPurpose, expires_at_ms: u64) -> NewOAuthState {
        NewOAuthState {
            state_hash: hash.into(),
            provider: provider.into(),
            purpose,
            account_id: None,
            invite_hash: None,
            next_path: Some("/chat".into()),
            pkce_verifier: "verifier".into(),
            expires_at_ms,
        }
    }

    #[tokio::test]
    async fn an_identity_belongs_to_one_account_and_an_account_has_one_per_provider() {
        let (s, _d) = store().await;
        let now = now_ms();
        let a = s.create_account("a", false, now).await.unwrap();
        let b = s.create_account("b", false, now).await.unwrap();

        assert_eq!(s.link_identity(&a.id, "github", "1", now).await.unwrap(), LinkIdentityOutcome::Linked);
        // 同一个账号再绑同一个身份：无操作
        assert_eq!(s.link_identity(&a.id, "github", "1", now + 5).await.unwrap(), LinkIdentityOutcome::AlreadyLinked);
        // 别的账号想要这个身份：被占用
        assert_eq!(s.link_identity(&b.id, "github", "1", now).await.unwrap(), LinkIdentityOutcome::SubjectTaken);
        // 这个账号在 github 上已经有别的身份了
        assert_eq!(s.link_identity(&a.id, "github", "2", now).await.unwrap(), LinkIdentityOutcome::ProviderTaken);
        // 同一个 subject 在另一个提供方是另一个身份
        assert_eq!(s.link_identity(&a.id, "google", "1", now + 10).await.unwrap(), LinkIdentityOutcome::Linked);
        // 身份被占用比「已有别的身份」优先报告
        assert_eq!(s.link_identity(&b.id, "github", "1", now).await.unwrap(), LinkIdentityOutcome::SubjectTaken);

        let found = s.find_identity("github", "1").await.unwrap().unwrap();
        assert_eq!((found.account_id.as_str(), found.created_at_ms), (a.id.as_str(), now));
        assert!(s.find_identity("github", "nope").await.unwrap().is_none());
        let mine: Vec<_> = s.list_identities(&a.id).await.unwrap().into_iter().map(|i| i.provider).collect();
        assert_eq!(mine, ["github", "google"], "oldest first");
        assert!(s.list_identities(&b.id).await.unwrap().is_empty());
        assert!(!format!("{found:?}").contains("subject"), "Debug does not print the subject: {found:?}");

        // 解绑：删的是这个账号的；没有就是 false
        assert!(!s.unlink_identity(&b.id, "github").await.unwrap());
        assert!(s.unlink_identity(&a.id, "github").await.unwrap());
        assert!(!s.unlink_identity(&a.id, "github").await.unwrap());
        assert!(s.find_identity("github", "1").await.unwrap().is_none());
        // 解绑之后，这个身份可以被别的账号绑
        assert_eq!(s.link_identity(&b.id, "github", "1", now).await.unwrap(), LinkIdentityOutcome::Linked);
    }

    #[tokio::test]
    async fn identities_and_states_go_away_with_the_account() {
        let (s, _d) = store().await;
        let now = now_ms();
        let a = s.create_account("a", false, now).await.unwrap();
        s.link_identity(&a.id, "github", "1", now).await.unwrap();
        let mut link = state("h1", "github", OAuthPurpose::Link, now + 60_000);
        link.account_id = Some(a.id.clone());
        s.put_oauth_state(link, now).await.unwrap();

        sqlx::query("DELETE FROM accounts WHERE id = ?").bind(&a.id).execute(&s.pool).await.unwrap();
        assert!(s.find_identity("github", "1").await.unwrap().is_none());
        assert!(s.take_oauth_state("h1", now).await.unwrap().is_none(), "the state of a deleted account is gone too");
    }

    #[tokio::test]
    async fn a_state_can_be_taken_once_and_not_after_it_expired() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_oauth_state(state("live", "github", OAuthPurpose::Login, now + 60_000), now).await.unwrap();
        s.put_oauth_state(state("old", "google", OAuthPurpose::Invite, now - 1), now - 100).await.unwrap();

        let taken = s.take_oauth_state("live", now).await.unwrap().expect("live state");
        assert_eq!(taken.provider, "github");
        assert_eq!(taken.purpose, OAuthPurpose::Login);
        assert_eq!(taken.next_path.as_deref(), Some("/chat"));
        assert_eq!(taken.pkce_verifier, "verifier");
        assert_eq!((taken.created_at_ms, taken.expires_at_ms), (now, now + 60_000));
        assert!(s.take_oauth_state("live", now).await.unwrap().is_none(), "single use");
        assert!(s.take_oauth_state("old", now).await.unwrap().is_none(), "expired states are not handed out");
        assert!(s.take_oauth_state("old", now).await.unwrap().is_none(), "and are gone after the attempt");
        assert!(s.take_oauth_state("never-existed", now).await.unwrap().is_none());
        assert!(!format!("{taken:?}").contains("verifier"), "{taken:?}");
    }

    #[tokio::test]
    async fn concurrent_takes_of_one_state_have_exactly_one_winner() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_oauth_state(state("one", "github", OAuthPurpose::Login, now + 60_000), now).await.unwrap();
        let s = std::sync::Arc::new(s);
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let s = s.clone();
                tokio::spawn(async move { s.take_oauth_state("one", now).await.unwrap().is_some() })
            })
            .collect();
        let mut winners = 0;
        for t in tasks {
            winners += usize::from(t.await.unwrap());
        }
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn expired_states_are_purged_in_bulk() {
        let (s, _d) = store().await;
        let now = now_ms();
        s.put_oauth_state(state("a", "github", OAuthPurpose::Login, now - 5), now - 1000).await.unwrap();
        s.put_oauth_state(state("b", "github", OAuthPurpose::Login, now + 5000), now).await.unwrap();
        assert_eq!(s.purge_expired_oauth_states(now).await.unwrap(), 1);
        assert!(s.take_oauth_state("b", now).await.unwrap().is_some(), "the live one survives");
        assert_eq!(s.purge_expired_oauth_states(now).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn creating_an_account_with_its_identity_is_all_or_nothing() {
        let (s, _d) = store().await;
        let now = now_ms();
        let new = |email, subject| NewIdentityAccount {
            display_name: "Ada",
            email,
            provider: "github",
            subject,
            invite_hash: None,
        };
        let IdentityAccountOutcome::Created(first) =
            s.create_account_with_identity(new(Some("ada@example.com"), "1"), now).await.unwrap()
        else {
            panic!("created")
        };
        assert_eq!(first.email.as_deref(), Some("ada@example.com"));
        assert!(!first.is_admin);
        assert_eq!(s.find_identity("github", "1").await.unwrap().unwrap().account_id, first.id);
        assert_eq!(s.find_account_by_email("ada@example.com").await.unwrap().unwrap().id, first.id);

        // 邮箱被占：什么都没写（包括没有留下一个孤儿身份）
        let r = s.create_account_with_identity(new(Some("ada@example.com"), "2"), now).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::EmailTaken), "{r:?}");
        assert!(s.find_identity("github", "2").await.unwrap().is_none());
        // 身份被占：不留下一个空账号
        let before = s.stats().await.unwrap().accounts;
        let r = s.create_account_with_identity(new(None, "1"), now).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::IdentityTaken), "{r:?}");
        assert_eq!(s.stats().await.unwrap().accounts, before, "no orphan account");
        // 没邮箱的账号也行，两个没邮箱的账号互不冲突
        for subject in ["3", "4"] {
            let r = s.create_account_with_identity(new(None, subject), now).await.unwrap();
            assert!(matches!(r, IdentityAccountOutcome::Created(ref a) if a.email.is_none()), "{r:?}");
        }
    }

    #[tokio::test]
    async fn creating_an_account_through_an_invite_redeems_it_atomically() {
        let (s, _d) = store().await;
        let now = now_ms();
        let admin = s.create_account("admin", true, now).await.unwrap();
        for (code, ttl) in [("live", 60_000u64), ("short", 1)] {
            s.create_invite(&admin.id, &hash_token(code), None, now - 10, now + ttl).await.unwrap();
        }
        fn with<'a>(invite_hash: Option<&'a str>, subject: &'a str) -> NewIdentityAccount<'a> {
            NewIdentityAccount { display_name: "Guest", email: None, provider: "google", subject, invite_hash }
        }
        let (live, short) = (hash_token("live"), hash_token("short"));

        // 不存在 / 过期的邀请：不建账号
        let before = s.stats().await.unwrap().accounts;
        let r = s.create_account_with_identity(with(Some("nope"), "1"), now).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::InviteNotFound), "{r:?}");
        let r = s.create_account_with_identity(with(Some(&short), "1"), now + 10).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::InviteExpired), "{r:?}");
        assert_eq!(s.stats().await.unwrap().accounts, before);
        assert!(s.find_identity("google", "1").await.unwrap().is_none());

        // 身份已被占用：邀请不会被白白用掉
        s.link_identity(&admin.id, "google", "taken", now).await.unwrap();
        let r = s.create_account_with_identity(with(Some(&live), "taken"), now).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::IdentityTaken), "{r:?}");
        assert!(s.list_invites(10, None).await.unwrap().iter().all(|i| i.used_by.is_none()), "invite untouched");

        // 成功：账号、身份、邀请一起落地；再用一次就是已用
        let IdentityAccountOutcome::Created(guest) =
            s.create_account_with_identity(with(Some(&live), "1"), now).await.unwrap()
        else {
            panic!("created")
        };
        assert_eq!(s.find_identity("google", "1").await.unwrap().unwrap().account_id, guest.id);
        let used = s.list_invites(10, None).await.unwrap().into_iter().filter(|i| i.used_by.is_some()).count();
        assert_eq!(used, 1);
        let r = s.create_account_with_identity(with(Some(&live), "2"), now).await.unwrap();
        assert!(matches!(r, IdentityAccountOutcome::InviteUsed), "{r:?}");
        assert!(s.find_identity("google", "2").await.unwrap().is_none());
    }
}
