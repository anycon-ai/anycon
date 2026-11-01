//! 两个存储后端共用的测试：同一套用例对 [`SqliteStore`] 和 [`PostgresStore`] 各跑一遍，两边的行为必须一致。
//!
//! 用例是这里的 `pub(crate) async fn xxx(db: &TestDb)`，只通过 [`Store`] 和 [`TestDb`] 的几个原始 SQL 小工具
//! （两个后端的占位符和语法不同，工具里各写一份）碰库。[`suite!`] 把它们生成成 `#[tokio::test]`，
//! `sqlite.rs` 和 `postgres.rs` 的测试模块各调用一次。
//!
//! Postgres 那一遍要设环境变量 `ANYCON_TEST_POSTGRES_URL`（一个能建 schema 的库，比如
//! `postgres://anycon:anycon@127.0.0.1:55432/anycon`）：每个用例在这个库里建一个独立的 schema（`search_path` 指过去），
//! 跑完（包括 panic）删掉。没设时 Postgres 的用例直接通过，并打印一行说明。

use std::io::Write;
use std::str::FromStr;
use std::sync::{Arc, Once};

use anycon_protocol::DeviceKind;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool, SqlitePool};

use super::*;
use crate::util::{hash_token, new_id, now_ms};

/// 指向测试用 Postgres 的环境变量。
pub(crate) const PG_URL_ENV: &str = "ANYCON_TEST_POSTGRES_URL";

/// 一个用例的库：存储本身，加一个原始 SQL 的口子（给「直接改库、看库」的断言用）。
pub(crate) struct TestDb {
    pub store: Arc<dyn Store>,
    raw: Raw,
    /// 放在最后：先释放连接，再删临时目录 / schema。
    _cleanup: Cleanup,
}

enum Raw {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

enum Cleanup {
    Dir(#[allow(dead_code)] tempfile::TempDir),
    Schema(#[allow(dead_code)] SchemaGuard),
}

/// 测试用的 Postgres schema：drop 时（包括测试 panic）删掉。
struct SchemaGuard {
    url: String,
    schema: String,
}

impl Drop for SchemaGuard {
    fn drop(&mut self) {
        // 在自己的线程和运行时里删：drop 可能发生在任何运行时的线程上（也可能是 panic 展开），不能在那里 block_on。
        let (url, schema) = (self.url.clone(), self.schema.clone());
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(async {
                let Ok(pool) = PgPoolOptions::new().max_connections(1).connect(&url).await else { return };
                // 失败的用例可能把一个没提交的事务留在连接池里（连接要等测试运行时回收，而这个线程正在 drop 里等）：
                // 先把还握着这个 schema 里的表的锁的会话踢掉，再删，免得 DROP 一直等下去。
                let _ = sqlx::query(
                    "SELECT pg_terminate_backend(l.pid) FROM pg_locks l \
                     JOIN pg_class c ON c.oid = l.relation JOIN pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND l.pid <> pg_backend_pid()",
                )
                .bind(&schema)
                .execute(&pool)
                .await;
                let drop = AssertSqlSafe(format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"));
                let _ = sqlx::query("SET lock_timeout = '10s'").execute(&pool).await;
                if let Err(e) = sqlx::query(drop).execute(&pool).await {
                    let _ = writeln!(std::io::stderr(), "warning: could not drop the test schema {schema}: {e}");
                }
                pool.close().await;
            });
        })
        .join();
    }
}

/// SQLite 的一份临时库。
pub(crate) async fn backend_sqlite() -> Option<TestDb> {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(None, dir.path()).await.unwrap();
    let raw = Raw::Sqlite(store.pool.clone());
    Some(TestDb { store: Arc::new(store), raw, _cleanup: Cleanup::Dir(dir) })
}

/// Postgres 里的一个新 schema。`ANYCON_TEST_POSTGRES_URL` 没设返回 `None`（用例跳过）。
pub(crate) async fn backend_postgres() -> Option<TestDb> {
    let Some(url) = postgres_url() else {
        static NOTE: Once = Once::new();
        NOTE.call_once(|| {
            // 直接写 stderr：测试框架只截获 `eprintln!`，这样没设变量时跳过的原因在输出里看得到。
            let _ = writeln!(
                std::io::stderr(),
                "note: {PG_URL_ENV} is not set, skipping the Postgres runs of the store tests"
            );
        });
        return None;
    };
    let schema = format!("t_{}", new_id());
    let admin = PgPoolOptions::new().max_connections(1).connect(&url).await.unwrap_or_else(|e| {
        panic!("{PG_URL_ENV} is set but the database is not reachable: {e}");
    });
    sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA \"{schema}\""))).execute(&admin).await.unwrap();
    admin.close().await;
    let guard = SchemaGuard { url: url.clone(), schema: schema.clone() };
    let options = PgConnectOptions::from_str(&url).unwrap().options([("search_path", schema.as_str())]);
    // 池小一点：用例并行跑，每个都有自己的池，加起来不能顶到 Postgres 的 max_connections。
    let store = PostgresStore::open_with(options, 6).await.unwrap();
    let raw = Raw::Postgres(store.pool.clone());
    Some(TestDb { store: Arc::new(store), raw, _cleanup: Cleanup::Schema(guard) })
}

/// `ANYCON_TEST_POSTGRES_URL`（没设或是空串就是 `None`）。
pub(crate) fn postgres_url() -> Option<String> {
    std::env::var(PG_URL_ENV).ok().map(|u| u.trim().to_string()).filter(|u| !u.is_empty())
}

impl TestDb {
    /// Postgres 那一边的连接池（只在 Postgres 专属的测试里用）。
    pub fn pg(&self) -> &PgPool {
        match &self.raw {
            Raw::Postgres(p) => p,
            Raw::Sqlite(_) => panic!("this test needs the Postgres backend"),
        }
    }

    /// `SELECT COUNT(*) FROM <from_where>`（`from_where` 是测试里写死的字面量，两个后端的 SQL 都认）。
    pub async fn count(&self, from_where: &'static str) -> i64 {
        let sql = AssertSqlSafe(format!("SELECT COUNT(*) FROM {from_where}"));
        match &self.raw {
            Raw::Sqlite(p) => sqlx::query_scalar(sql).fetch_one(p).await.unwrap(),
            Raw::Postgres(p) => sqlx::query_scalar(sql).fetch_one(p).await.unwrap(),
        }
    }

    /// 直接删账号行（绕过 `Store`），看外键级联删掉了什么。
    pub async fn delete_account_row(&self, account_id: &str) {
        match &self.raw {
            Raw::Sqlite(p) => {
                sqlx::query("DELETE FROM accounts WHERE id = ?").bind(account_id).execute(p).await.unwrap();
            }
            Raw::Postgres(p) => {
                sqlx::query("DELETE FROM accounts WHERE id = $1").bind(account_id).execute(p).await.unwrap();
            }
        }
    }

    /// 直接把一个邀请标成某个账号兑换的。
    pub async fn mark_invite_used(&self, invite_id: &str, by: &str, at_ms: u64) {
        match &self.raw {
            Raw::Sqlite(p) => {
                sqlx::query("UPDATE invites SET used_by = ?, used_at_ms = ? WHERE id = ?")
                    .bind(by)
                    .bind(at_ms as i64)
                    .bind(invite_id)
                    .execute(p)
                    .await
                    .unwrap();
            }
            Raw::Postgres(p) => {
                sqlx::query("UPDATE invites SET used_by = $1, used_at_ms = $2 WHERE id = $3")
                    .bind(by)
                    .bind(at_ms as i64)
                    .bind(invite_id)
                    .execute(p)
                    .await
                    .unwrap();
            }
        }
    }

    /// 库里所有邀请的 id，按字节序。
    pub async fn invite_ids(&self) -> Vec<String> {
        match &self.raw {
            Raw::Sqlite(p) => sqlx::query_scalar("SELECT id FROM invites ORDER BY id").fetch_all(p).await.unwrap(),
            Raw::Postgres(p) => {
                sqlx::query_scalar("SELECT id FROM invites ORDER BY id COLLATE \"C\"").fetch_all(p).await.unwrap()
            }
        }
    }

    /// 库里所有账号的显示名（不保证顺序）。
    pub async fn account_names(&self) -> Vec<String> {
        match &self.raw {
            Raw::Sqlite(p) => sqlx::query_scalar("SELECT display_name FROM accounts").fetch_all(p).await.unwrap(),
            Raw::Postgres(p) => sqlx::query_scalar("SELECT display_name FROM accounts").fetch_all(p).await.unwrap(),
        }
    }

    /// 写一行类别是「以后版本才有」的通知偏好。
    pub async fn insert_unknown_notification_category(&self, account_id: &str) {
        match &self.raw {
            Raw::Sqlite(p) => {
                sqlx::query(
                    "INSERT INTO notification_prefs (account_id, category, push, email) VALUES (?, 'from_the_future', 1, 1)",
                )
                .bind(account_id)
                .execute(p)
                .await
                .unwrap();
            }
            Raw::Postgres(p) => {
                sqlx::query(
                    "INSERT INTO notification_prefs (account_id, category, push, email) VALUES ($1, 'from_the_future', TRUE, TRUE)",
                )
                .bind(account_id)
                .execute(p)
                .await
                .unwrap();
            }
        }
    }

    /// 让往 `kv` 里写这个键的 `INSERT` 失败（用触发器模拟写失败 / 崩溃）。
    pub async fn make_kv_inserts_fail(&self, key: &str) {
        match &self.raw {
            Raw::Sqlite(p) => {
                let sql = AssertSqlSafe(format!(
                    "CREATE TRIGGER no_kv BEFORE INSERT ON kv WHEN NEW.key = '{key}' BEGIN SELECT RAISE(ABORT, 'no kv'); END"
                ));
                sqlx::query(sql).execute(p).await.unwrap();
            }
            Raw::Postgres(p) => {
                // 函数和触发器都建在测试自己的 schema 里（`search_path` 的第一个）。
                sqlx::query(
                    "CREATE FUNCTION no_kv() RETURNS trigger LANGUAGE plpgsql AS \
                     $$ BEGIN RAISE EXCEPTION 'no kv'; END $$",
                )
                .execute(p)
                .await
                .unwrap();
                let sql = AssertSqlSafe(format!(
                    "CREATE TRIGGER no_kv BEFORE INSERT ON kv FOR EACH ROW WHEN (NEW.key = '{key}') EXECUTE FUNCTION no_kv()"
                ));
                sqlx::query(sql).execute(p).await.unwrap();
            }
        }
    }
}

/// 把共用用例生成成 `#[tokio::test]`。`$backend` 是 `backend_sqlite` / `backend_postgres`；返回 `None`（Postgres 没配）就跳过。
macro_rules! suite {
    ($backend:path) => {
        $crate::store::conformance::suite!(@cases $backend;
            plain: [
                accounts_tokens_sessions
                invites_are_single_use
                invites_and_accounts_paginate_by_keyset_even_with_identical_timestamps
                devices_pairings_revoke
                text_ids_are_ordered_bytewise_whatever_the_database_collation
                an_account_and_its_recorded_id_are_written_in_one_transaction
                audit_and_kv
                timestamps_and_counters_round_trip_at_full_width
                an_email_belongs_to_one_account_and_can_be_cleared
                an_account_can_be_created_with_its_email_and_the_second_creator_of_that_email_loses
                a_new_code_does_not_void_the_old_ones_and_the_oldest_is_evicted_past_the_limit
                bind_codes_are_kept_per_account_and_email_so_another_account_cannot_void_yours
                an_attempt_counts_every_live_code_and_hands_back_exactly_the_rows_it_counted
                claiming_by_id_works_once_and_only_for_that_row
                expired_codes_are_cleaned_when_touched_and_purged_in_bulk
                push_subscriptions_are_one_per_device_and_follow_the_device_lifecycle
                notification_prefs_are_stored_per_account_and_category
                the_notification_tables_cascade_with_their_owners
                the_first_token_binds_the_label_and_a_second_node_cannot_take_it
                a_node_binds_at_most_the_configured_number_of_labels
                a_device_keeps_only_its_newest_tokens_and_expired_ones_do_not_count
                token_validity_and_purging_follow_the_expiry_time
                revoking_a_device_deletes_its_tokens_and_revoking_a_node_deletes_its_labels_and_tokens
                preview_rows_cascade_with_the_account
                counts_cover_live_devices_only_and_split_out_daemons
                devices_are_created_within_the_limits_and_a_batch_is_all_or_nothing
                the_daemon_limit_is_separate_and_reported_when_it_is_the_one_hit
                admin_flag_is_set_and_cleared_and_unknown_accounts_report_false
                deleting_an_account_removes_everything_it_owns_and_leaves_the_rest
                searching_accounts_matches_names_and_emails_in_any_ascii_case_and_exact_ids_with_keyset_pages
                like_wildcards_and_the_escape_character_in_a_search_are_literal
                an_identity_belongs_to_one_account_and_an_account_has_one_per_provider
                identities_and_states_go_away_with_the_account
                a_state_can_be_taken_once_and_not_after_it_expired
                expired_states_are_purged_in_bulk
                creating_an_account_with_its_identity_is_all_or_nothing
                creating_an_account_through_an_invite_redeems_it_atomically
            ];
            threaded: [
                concurrent_attempts_get_at_most_max_attempts_comparisons_in_total
                concurrent_requests_for_one_label_bind_it_to_exactly_one_node
                concurrent_creations_for_one_account_never_overshoot
                concurrent_takes_of_one_state_have_exactly_one_winner
                concurrent_redemptions_of_one_invite_have_exactly_one_winner
                concurrent_sign_ups_through_one_email_identity_or_invite_create_exactly_one_account
                concurrent_claims_of_one_email_address_or_external_identity_have_one_winner
                concurrent_registrations_of_codes_in_one_group_keep_exactly_the_configured_number
                concurrent_claims_of_one_email_code_have_exactly_one_winner
                concurrent_preview_requests_never_exceed_the_label_and_token_caps
                concurrent_deletions_of_one_account_write_exactly_one_tombstone
                revoking_a_device_while_it_is_being_paired_leaves_a_consistent_state
                batches_spanning_two_accounts_complete_whatever_the_order
                repeated_races_for_the_last_free_slot_never_overshoot
            ]
        );
    };
    (@cases $backend:path; plain: [$($plain:ident)*]; threaded: [$($threaded:ident)*]) => {
        $(
            #[tokio::test]
            async fn $plain() {
                if let Some(db) = $backend().await {
                    $crate::store::conformance::$plain(&db).await;
                }
            }
        )*
        $(
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $threaded() {
                if let Some(db) = $backend().await {
                    $crate::store::conformance::$threaded(&db).await;
                }
            }
        )*
    };
}
pub(crate) use suite;

// ---------------------------------------------------------------------------
// 账号、会话、邀请、设备、审计、邮箱验证码、推送、预览
// ---------------------------------------------------------------------------

pub(crate) fn new_device(account: &str, id: &str, token: &str) -> NewDevice {
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

pub(crate) async fn accounts_tokens_sessions(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = s.create_account("admin", true, now).await.unwrap();
    assert_eq!(s.first_admin().await.unwrap().unwrap().id, acc.id);

    let t = s.create_account_token(&acc.id, "laptop", &hash_token("tok"), now).await.unwrap();
    assert_eq!(s.find_account_by_token_hash(&hash_token("tok"), now + 5).await.unwrap().unwrap().id, acc.id);
    assert_eq!(s.list_account_tokens(&acc.id).await.unwrap()[0].last_used_ms, Some(now + 5));
    assert!(s.find_account_by_token_hash(&hash_token("nope"), now).await.unwrap().is_none());
    assert!(s.delete_account_token(&acc.id, &t.id).await.unwrap());
    assert!(!s.delete_account_token(&acc.id, &t.id).await.unwrap());

    let sess = s.create_web_session(&acc.id, &hash_token("cookie"), "ua", "1.2.3.0/24", now, now + 1000).await.unwrap();
    assert!(s.find_web_session(&hash_token("cookie"), now + 10).await.unwrap().is_some());
    assert!(s.find_web_session(&hash_token("cookie"), now + 1000).await.unwrap().is_none());
    s.touch_web_session(&sess.id, now + 10, now + 5000).await.unwrap();
    assert!(s.find_web_session(&hash_token("cookie"), now + 1000).await.unwrap().is_some());
    assert_eq!(s.list_web_sessions(&acc.id, now + 20).await.unwrap().len(), 1);
    assert!(s.delete_web_session(&acc.id, &sess.id).await.unwrap());
}

pub(crate) async fn invites_are_single_use(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn invites_and_accounts_paginate_by_keyset_even_with_identical_timestamps(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn devices_pairings_revoke(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn an_account_and_its_recorded_id_are_written_in_one_transaction(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account_and_record_id("admin", true, "bootstrap_admin_id", 5).await.unwrap();
    assert_eq!(s.kv_get("bootstrap_admin_id").await.unwrap().as_deref(), Some(a.id.as_str()));
    let got = s.get_account(&a.id).await.unwrap().unwrap();
    assert!((got.display_name.as_str(), got.is_admin, got.created_at_ms) == ("admin", true, 5));

    // 记录写不进去（这里用触发器模拟崩溃 / 写失败）：账号也不留下，没有没人记得的孤儿管理员
    db.make_kv_inserts_fail("fails").await;
    assert!(s.create_account_and_record_id("orphan", true, "fails", 6).await.is_err());
    assert_eq!(db.account_names().await, vec!["admin".to_string()], "the failed attempt left no account behind");
}

pub(crate) async fn audit_and_kv(db: &TestDb) {
    let s = &db.store;
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
    assert!(s.stats().await.unwrap().db_size_bytes > 0);
}

// ---- ADR-37：邮箱、验证码、推送订阅、通知偏好 ----

pub(crate) fn code_row(email: &str, purpose: EmailPurpose, account: Option<&str>, hash: &str, now: u64) -> EmailCode {
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

pub(crate) async fn an_email_belongs_to_one_account_and_can_be_cleared(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn an_account_can_be_created_with_its_email_and_the_second_creator_of_that_email_loses(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let CreateAccountOutcome::Created(a) =
        s.create_account_with_email("alice", Some("alice@example.com"), now).await.unwrap()
    else {
        panic!("the first creation wins")
    };
    assert_eq!((a.display_name.as_str(), a.email.as_deref(), a.is_admin), ("alice", Some("alice@example.com"), false));
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
async fn hashes(s: &dyn Store, email: &str, purpose: EmailPurpose) -> Vec<String> {
    s.list_email_codes(email, purpose).await.unwrap().into_iter().map(|c| c.code_hash).collect()
}

pub(crate) async fn a_new_code_does_not_void_the_old_ones_and_the_oldest_is_evicted_past_the_limit(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    for (i, h) in ["h1", "h2", "h3"].iter().enumerate() {
        s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, h, now + i as u64), 3).await.unwrap();
    }
    assert_eq!(hashes(&**s, "x@example.com", EmailPurpose::Login).await, ["h1", "h2", "h3"], "all three are valid");
    // 第 4 个顶掉最旧的，另外两个还在
    s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h4", now + 3), 3).await.unwrap();
    assert_eq!(hashes(&**s, "x@example.com", EmailPurpose::Login).await, ["h2", "h3", "h4"]);
    // 别的邮箱、别的用途各算各的
    s.put_email_code(code_row("y@example.com", EmailPurpose::Login, None, "hy", now), 3).await.unwrap();
    let acc = s.create_account("a", true, now).await.unwrap();
    s.put_email_code(code_row("x@example.com", EmailPurpose::Bind, Some(&acc.id), "hb", now), 3).await.unwrap();
    assert_eq!(hashes(&**s, "x@example.com", EmailPurpose::Login).await, ["h2", "h3", "h4"]);
    assert_eq!(hashes(&**s, "y@example.com", EmailPurpose::Login).await, ["hy"]);
    assert_eq!(hashes(&**s, "x@example.com", EmailPurpose::Bind).await, ["hb"]);
    // 过期的不占名额：新码进来时先把这一组里过期的清掉
    s.put_email_code(
        EmailCode { expires_at_ms: now + 5, ..code_row("z@example.com", EmailPurpose::Login, None, "old", now) },
        3,
    )
    .await
    .unwrap();
    s.put_email_code(code_row("z@example.com", EmailPurpose::Login, None, "fresh", now + 10), 3).await.unwrap();
    assert_eq!(hashes(&**s, "z@example.com", EmailPurpose::Login).await, ["fresh"]);
}

pub(crate) async fn bind_codes_are_kept_per_account_and_email_so_another_account_cannot_void_yours(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn an_attempt_counts_every_live_code_and_hands_back_exactly_the_rows_it_counted(db: &TestDb) {
    let s = &db.store;
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
    assert!(s.attempt_email_codes("nobody@example.com", EmailPurpose::Login, None, now, 5).await.unwrap().is_empty());
    // 次数用完的码不再交出来：h1、h2 再被尝试 3 次就到 5，之后只剩 h3
    for _ in 0..3 {
        s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 5, 5).await.unwrap();
    }
    let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 6, 5).await.unwrap();
    assert_eq!(got.iter().map(|c| (c.code_hash.as_str(), c.attempts)).collect::<Vec<_>>(), [("h3", 5)]);
    // 第 5 次提交时 h3 仍交出来了（所以仍能被认领）；再来一次就没有可猜的了
    assert!(s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 7, 5).await.unwrap().is_empty());
}

pub(crate) async fn concurrent_attempts_get_at_most_max_attempts_comparisons_in_total(db: &TestDb) {
    // 「先原子计数、再比较」：计数和取码是同一个原子操作，所以 30 个并发提交里只有 5 个能拿到哈希去比较，
    // 其余的什么都拿不到（哪怕它们带的是对的码）。如果实现成「先取、比较、再计数」，并发的提交都会拿到哈希。
    let s = db.store.clone();
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

pub(crate) async fn claiming_by_id_works_once_and_only_for_that_row(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h1", now), 3).await.unwrap();
    s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h2", now + 1), 3).await.unwrap();
    let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 2, 5).await.unwrap();
    assert!(s.take_email_code(got[0].id).await.unwrap());
    assert!(!s.take_email_code(got[0].id).await.unwrap(), "a second claim loses");
    assert_eq!(hashes(&**s, "x@example.com", EmailPurpose::Login).await, ["h2"], "the other code is untouched");
    // 被顶掉（过期清掉）的码认领不到
    assert!(!s.take_email_code(9_999_999).await.unwrap());
}

pub(crate) async fn expired_codes_are_cleaned_when_touched_and_purged_in_bulk(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    s.put_email_code(
        EmailCode { expires_at_ms: now + 10, ..code_row("x@example.com", EmailPurpose::Login, None, "h", now) },
        3,
    )
    .await
    .unwrap();
    // 过期了：交不出来，而且顺手删掉
    assert!(s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now + 10, 5).await.unwrap().is_empty());
    assert!(hashes(&**s, "x@example.com", EmailPurpose::Login).await.is_empty());
    // 批量清理
    s.put_email_code(
        EmailCode { expires_at_ms: now + 10, ..code_row("old@example.com", EmailPurpose::Login, None, "h", now) },
        3,
    )
    .await
    .unwrap();
    s.put_email_code(code_row("live@example.com", EmailPurpose::Login, None, "h", now), 3).await.unwrap();
    assert_eq!(s.purge_expired_email_codes(now + 10).await.unwrap(), 1);
    assert!(hashes(&**s, "old@example.com", EmailPurpose::Login).await.is_empty());
    assert_eq!(hashes(&**s, "live@example.com", EmailPurpose::Login).await.len(), 1);
}

pub(crate) async fn push_subscriptions_are_one_per_device_and_follow_the_device_lifecycle(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn notification_prefs_are_stored_per_account_and_category(db: &TestDb) {
    use anycon_protocol::NotifyCategory::*;
    let s = &db.store;
    let now = now_ms();
    let a = s.create_account("a", true, now).await.unwrap();
    let b = s.create_account("b", false, now).await.unwrap();
    assert!(s.get_notification_prefs(&a.id).await.unwrap().is_empty(), "no rows = defaults, filled in by the caller");
    let p = |category, push, email| NotificationPref { category, push, email };
    s.set_notification_prefs(&a.id, &[p(Error, false, true), p(Security, true, false)]).await.unwrap();
    s.set_notification_prefs(&a.id, &[p(Error, true, true)]).await.unwrap();
    s.set_notification_prefs(&b.id, &[p(Error, false, false)]).await.unwrap();
    let mut got = s.get_notification_prefs(&a.id).await.unwrap();
    got.sort_by_key(|x| category_to_str(x.category));
    assert_eq!(got, vec![p(Error, true, true), p(Security, true, false)]);
    assert_eq!(s.get_notification_prefs(&b.id).await.unwrap(), vec![p(Error, false, false)]);
    // 一个陌生的类别（以后版本写进来的）被跳过，不影响其它
    db.insert_unknown_notification_category(&a.id).await;
    assert_eq!(s.get_notification_prefs(&a.id).await.unwrap().len(), 2);
}

pub(crate) async fn the_notification_tables_cascade_with_their_owners(db: &TestDb) {
    let s = &db.store;
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
    db.delete_account_row(&acc.id).await;
    assert!(s.get_push_subscription_of_device("w").await.unwrap().is_none());
    assert!(hashes(&**s, "a@example.com", EmailPurpose::Bind).await.is_empty());
    assert!(s.get_notification_prefs(&acc.id).await.unwrap().is_empty());
}
// ---- 预览（ADR-39） ----

pub(crate) const PREVIEW_LIMITS: PreviewIssueLimits = PreviewIssueLimits { labels_per_node: 3, tokens_per_device: 3 };

pub(crate) fn new_token(
    hash: &str,
    label: &str,
    node: &str,
    device: &str,
    account: &str,
    expires: u64,
) -> NewPreviewToken {
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
pub(crate) async fn preview_fixture(s: &dyn Store, now: u64) -> String {
    let acc = s.create_account("a", true, now).await.unwrap();
    for (id, kind) in
        [("n1", DeviceKind::Daemon), ("n2", DeviceKind::Daemon), ("w", DeviceKind::Web), ("w2", DeviceKind::Web)]
    {
        s.create_device(NewDevice { kind, ..new_device(&acc.id, id, &format!("tok-{id}")) }, now).await.unwrap();
    }
    acc.id
}

pub(crate) async fn the_first_token_binds_the_label_and_a_second_node_cannot_take_it(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    let issue = |hash: &str, label: &str, node: &str, device: &str| {
        s.issue_preview_token(new_token(hash, label, node, device, &acc, now + 1000), now, PREVIEW_LIMITS)
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

pub(crate) async fn a_node_binds_at_most_the_configured_number_of_labels(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    let issue = |hash: &str, label: &str, node: &str| {
        s.issue_preview_token(new_token(hash, label, node, "w", &acc, now + 1000), now, PREVIEW_LIMITS)
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

pub(crate) async fn a_device_keeps_only_its_newest_tokens_and_expired_ones_do_not_count(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    // 一个已经过期的、一个别的设备的：都不占 w 的名额
    s.issue_preview_token(new_token("old", "l1", "n1", "w", &acc, now + 10), now - 1000, PREVIEW_LIMITS).await.unwrap();
    s.issue_preview_token(new_token("other", "l1", "n1", "w2", &acc, now + 10_000), now, PREVIEW_LIMITS).await.unwrap();
    for i in 0..5u64 {
        // 同一毫秒签发的也有先后（rowid）
        let t = new_token(&format!("t{i}"), "l1", "n1", "w", &acc, now + 10_000);
        assert_eq!(s.issue_preview_token(t, now + 100, PREVIEW_LIMITS).await.unwrap(), IssueOutcome::Issued);
    }
    let kept: Vec<String> =
        s.list_preview_tokens_of_device("w").await.unwrap().into_iter().map(|t| t.token_hash).collect();
    assert_eq!(kept, ["t2", "t3", "t4"], "the three newest; the expired one was cleaned up on the way");
    assert!(s.find_preview_token("t0").await.unwrap().is_none());
    assert!(s.find_preview_token("old").await.unwrap().is_none());
    assert!(s.find_preview_token("other").await.unwrap().is_some(), "another device's tokens are untouched");
}

pub(crate) async fn token_validity_and_purging_follow_the_expiry_time(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    s.issue_preview_token(new_token("short", "l1", "n1", "w", &acc, now + 100), now, PREVIEW_LIMITS).await.unwrap();
    s.issue_preview_token(new_token("long", "l1", "n1", "w2", &acc, now + 10_000), now, PREVIEW_LIMITS).await.unwrap();
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

pub(crate) async fn revoking_a_device_deletes_its_tokens_and_revoking_a_node_deletes_its_labels_and_tokens(
    db: &TestDb,
) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    let far = now + 100_000;
    s.issue_preview_token(new_token("a", "l1", "n1", "w", &acc, far), now, PREVIEW_LIMITS).await.unwrap();
    s.issue_preview_token(new_token("b", "l1", "n1", "w2", &acc, far), now, PREVIEW_LIMITS).await.unwrap();
    s.issue_preview_token(new_token("c", "l2", "n2", "w2", &acc, far), now, PREVIEW_LIMITS).await.unwrap();
    // 吊销签发设备 w：只删它签发的
    s.revoke_device("w", now + 1).await.unwrap();
    assert!(s.find_preview_token("a").await.unwrap().is_none());
    assert!(s.find_preview_token("b").await.unwrap().is_some() && s.find_preview_token("c").await.unwrap().is_some());
    assert_eq!(s.list_preview_labels("n1").await.unwrap(), ["l1"], "the label stays: another device may still use it");
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

pub(crate) async fn preview_rows_cascade_with_the_account(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let acc = preview_fixture(&**s, now).await;
    s.issue_preview_token(new_token("a", "l1", "n1", "w", &acc, now + 1000), now, PREVIEW_LIMITS).await.unwrap();
    db.delete_account_row(&acc).await;
    assert!(s.find_preview_token("a").await.unwrap().is_none());
    assert!(s.list_preview_labels("n1").await.unwrap().is_empty());
}

pub(crate) async fn concurrent_requests_for_one_label_bind_it_to_exactly_one_node(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let acc = preview_fixture(&*s, now).await;
    let tasks: Vec<_> = (0..12)
        .map(|i| {
            let (s, acc) = (s.clone(), acc.clone());
            let node = if i % 2 == 0 { "n1" } else { "n2" };
            tokio::spawn(async move {
                s.issue_preview_token(
                    new_token(&format!("h{i}"), "race", node, "w", &acc, now + 1000),
                    now,
                    PREVIEW_LIMITS,
                )
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

// ---------------------------------------------------------------------------
// multi：配额、账号删除、管理员标记、账号搜索
// ---------------------------------------------------------------------------

pub(crate) fn device(account: &str, id: &str, kind: DeviceKind) -> NewDevice {
    NewDevice {
        id: id.into(),
        account_id: account.into(),
        kind,
        name: id.into(),
        platform: "test".into(),
        origin: None,
        token_hash: hash_token(&format!("token-{id}")),
    }
}

pub(crate) const DEVICE_LIMITS: DeviceLimits = DeviceLimits { devices: 3, daemons: 2 };

fn account(outcome: CreateAccountOutcome) -> Account {
    match outcome {
        CreateAccountOutcome::Created(a) => a,
        CreateAccountOutcome::EmailTaken => panic!("the email is free"),
    }
}

async fn created(s: &dyn Store, devices: Vec<NewDevice>, limits: DeviceLimits) -> bool {
    matches!(s.create_devices_within_limits(devices, limits, now_ms()).await.unwrap(), CreateDevicesOutcome::Created(_))
}

pub(crate) async fn counts_cover_live_devices_only_and_split_out_daemons(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account("a", false, now_ms()).await.unwrap();
    let b = s.create_account("b", false, now_ms()).await.unwrap();
    assert_eq!(s.count_devices(&a.id).await.unwrap(), (0, 0));
    for (id, kind) in [("d1", DeviceKind::Daemon), ("w1", DeviceKind::Web), ("c1", DeviceKind::Cli)] {
        s.create_device(device(&a.id, id, kind), now_ms()).await.unwrap();
    }
    s.create_device(device(&b.id, "other", DeviceKind::Daemon), now_ms()).await.unwrap();
    assert_eq!(s.count_devices(&a.id).await.unwrap(), (3, 1));
    s.revoke_device("d1", now_ms()).await.unwrap();
    assert_eq!(s.count_devices(&a.id).await.unwrap(), (2, 0), "revoked devices do not count");
    assert_eq!(s.count_devices(&b.id).await.unwrap(), (1, 1));
}

pub(crate) async fn devices_are_created_within_the_limits_and_a_batch_is_all_or_nothing(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account("a", false, now_ms()).await.unwrap();
    let b = s.create_account("b", false, now_ms()).await.unwrap();
    assert!(created(&**s, vec![device(&a.id, "d1", DeviceKind::Daemon)], DEVICE_LIMITS).await);
    // 两台一批，只剩 2 个设备名额：整批进去（总数到上限 3）
    assert!(
        created(&**s, vec![device(&a.id, "w1", DeviceKind::Web), device(&a.id, "w2", DeviceKind::Web)], DEVICE_LIMITS)
            .await
    );
    assert_eq!(s.count_devices(&a.id).await.unwrap(), (3, 1));
    // 满了：再来一台就是设备总数超了，什么都不写
    match s
        .create_devices_within_limits(vec![device(&a.id, "w3", DeviceKind::Web)], DEVICE_LIMITS, now_ms())
        .await
        .unwrap()
    {
        CreateDevicesOutcome::Exceeded(DeviceLimit::Devices) => {}
        other => panic!("{other:?}"),
    }
    assert!(s.get_device("w3").await.unwrap().is_none());
    // 别的账号不受影响
    assert!(created(&**s, vec![device(&b.id, "b1", DeviceKind::Web)], DEVICE_LIMITS).await);

    // 吊销空出名额；一批里第二台超了，第一台也不留
    s.revoke_device("w1", now_ms()).await.unwrap();
    s.revoke_device("w2", now_ms()).await.unwrap();
    let batch = vec![
        device(&a.id, "x1", DeviceKind::Web),
        device(&a.id, "x2", DeviceKind::Web),
        device(&a.id, "x3", DeviceKind::Web),
    ];
    assert!(matches!(
        s.create_devices_within_limits(batch, DEVICE_LIMITS, now_ms()).await.unwrap(),
        CreateDevicesOutcome::Exceeded(DeviceLimit::Devices)
    ));
    assert_eq!(s.count_devices(&a.id).await.unwrap(), (1, 1), "the first two of the batch were rolled back");
    assert!(s.get_device("x1").await.unwrap().is_none());
}

pub(crate) async fn the_daemon_limit_is_separate_and_reported_when_it_is_the_one_hit(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account("a", false, now_ms()).await.unwrap();
    let limits = DeviceLimits { devices: 10, daemons: 2 };
    assert!(
        created(&**s, vec![device(&a.id, "d1", DeviceKind::Daemon), device(&a.id, "w1", DeviceKind::Web)], limits)
            .await
    );
    assert!(created(&**s, vec![device(&a.id, "d2", DeviceKind::Daemon)], limits).await);
    match s.create_devices_within_limits(vec![device(&a.id, "d3", DeviceKind::Daemon)], limits, now_ms()).await.unwrap()
    {
        CreateDevicesOutcome::Exceeded(DeviceLimit::Daemons) => {}
        other => panic!("{other:?}"),
    }
    // 非 daemon 设备不受 daemon 上限影响
    assert!(created(&**s, vec![device(&a.id, "w2", DeviceKind::Cli)], limits).await);
    // 吊销一台 daemon 就空出来
    s.revoke_device("d1", now_ms()).await.unwrap();
    assert!(created(&**s, vec![device(&a.id, "d3", DeviceKind::Daemon)], limits).await);
}

pub(crate) async fn concurrent_creations_for_one_account_never_overshoot(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account("a", false, now_ms()).await.unwrap();
    let limits = DeviceLimits { devices: 5, daemons: 2 };
    let tasks: Vec<_> = (0..40)
        .map(|i| {
            let (s, account) = (s.clone(), a.id.clone());
            // 一半是 daemon，一半是 web，其中每隔几个是两台一批
            tokio::spawn(async move {
                let kind = if i % 2 == 0 { DeviceKind::Daemon } else { DeviceKind::Web };
                let mut batch = vec![device(&account, &format!("n{i}"), kind)];
                if i % 5 == 0 {
                    batch.push(device(&account, &format!("m{i}"), DeviceKind::Web));
                }
                s.create_devices_within_limits(batch, limits, now_ms()).await.unwrap()
            })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        if matches!(t.await.unwrap(), CreateDevicesOutcome::Created(_)) {
            ok += 1;
        }
    }
    let (devices, daemons) = s.count_devices(&a.id).await.unwrap();
    assert!(ok >= 1);
    assert!(devices <= 5 && daemons <= 2, "never above the limits: {devices} devices, {daemons} daemons");
    assert_eq!(devices, 5, "and the free slots do get used: 40 contenders for 5 slots");
}

pub(crate) async fn admin_flag_is_set_and_cleared_and_unknown_accounts_report_false(db: &TestDb) {
    let s = &db.store;
    let a = s.create_account("a", false, now_ms()).await.unwrap();
    assert!(s.set_account_admin(&a.id, true).await.unwrap());
    assert!(s.get_account(&a.id).await.unwrap().unwrap().is_admin);
    assert!(s.set_account_admin(&a.id, false).await.unwrap());
    assert!(!s.get_account(&a.id).await.unwrap().unwrap().is_admin);
    assert!(!s.set_account_admin("nope", true).await.unwrap());
}

pub(crate) async fn deleting_an_account_removes_everything_it_owns_and_leaves_the_rest(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let a = s.create_account_with_email("alice", Some("alice@example.com"), now).await.unwrap();
    let b = s.create_account_with_email("bob", Some("bob@example.com"), now).await.unwrap();
    let (a, b) = (account(a), account(b));

    // alice 名下的东西：设备（带推送订阅、预览令牌）、配对、登录 token、浏览器会话、通知偏好、验证码、邀请、审计
    s.create_device(device(&a.id, "a-node", DeviceKind::Daemon), now).await.unwrap();
    s.create_device(device(&a.id, "a-web", DeviceKind::Web), now).await.unwrap();
    s.insert_pairing("a-node", "a-web", now).await.unwrap();
    s.upsert_push_subscription("a-web", "https://push.example/x", "p", "a", now).await.unwrap();
    s.create_account_token(&a.id, "laptop", &hash_token("alice-token"), now).await.unwrap();
    s.create_web_session(&a.id, &hash_token("alice-cookie"), "ua", "ip", now, now + 60_000).await.unwrap();
    s.set_notification_prefs(&a.id, &[NotificationPref { category: NotifyCategory::Error, push: true, email: false }])
        .await
        .unwrap();
    let token = NewPreviewToken {
        token_hash: "ptok".into(),
        label: "label".into(),
        node_id: "a-node".into(),
        account_id: a.id.clone(),
        device_id: "a-web".into(),
        expires_at_ms: now + 60_000,
    };
    let issued = s
        .issue_preview_token(token, now, PreviewIssueLimits { labels_per_node: 3, tokens_per_device: 3 })
        .await
        .unwrap();
    assert_eq!(issued, IssueOutcome::Issued);
    for (email, account) in [("alice@example.com", None), ("alice@example.com", Some(a.id.clone()))] {
        let purpose = if account.is_some() { EmailPurpose::Bind } else { EmailPurpose::Login };
        s.put_email_code(
            EmailCode {
                email: email.into(),
                purpose,
                account_id: account,
                code_hash: "h".into(),
                expires_at_ms: now + 60_000,
                attempts: 0,
                created_at_ms: now,
            },
            3,
        )
        .await
        .unwrap();
    }
    // 邀请：alice 建的（没人用 / bob 用了）、bob 建的并由 alice 兑换的、bob 建的没人用的
    let mk = |by: &str, hash: &str| {
        let (s, by, hash) = (s.clone(), by.to_string(), hash.to_string());
        async move { s.create_invite(&by, &hash, None, now, now + 600_000).await.unwrap().id }
    };
    let (_alice_unused, alice_used_by_bob) = (mk(&a.id, "h1").await, mk(&a.id, "h2").await);
    let (bob_used_by_alice, bob_unused) = (mk(&b.id, "h3").await, mk(&b.id, "h4").await);
    db.mark_invite_used(&alice_used_by_bob, &b.id, now).await;
    db.mark_invite_used(&bob_used_by_alice, &a.id, now).await;
    // bob 的东西不能被动
    s.create_device(device(&b.id, "b-web", DeviceKind::Web), now).await.unwrap();
    s.create_account_token(&b.id, "bob", &hash_token("bob-token"), now).await.unwrap();
    s.put_email_code(
        EmailCode {
            email: "bob@example.com".into(),
            purpose: EmailPurpose::Login,
            account_id: None,
            code_hash: "h".into(),
            expires_at_ms: now + 60_000,
            attempts: 0,
            created_at_ms: now,
        },
        3,
    )
    .await
    .unwrap();
    // 审计：alice 的账号行和设备行、bob 的行
    for (event, account, device_id) in
        [("login_ok", Some(&a.id), None), ("device_paired", None, Some("a-web")), ("login_ok", Some(&b.id), None)]
    {
        s.insert_audit(
            NewAudit {
                event,
                account_id: account.cloned(),
                device_id: device_id.map(str::to_string),
                ip_prefix: Some("10.0.0.0/24".into()),
                ..Default::default()
            },
            now,
        )
        .await
        .unwrap();
    }

    let tombstone = NewAudit {
        event: "account_deleted",
        account_id: Some(a.id.clone()),
        detail: Some(serde_json::json!({ "by": "self" })),
        ..Default::default()
    };
    assert!(s.delete_account(&a.id, tombstone, now + 5).await.unwrap());

    // alice 的一切都没了
    assert!(s.get_account(&a.id).await.unwrap().is_none());
    assert_eq!(db.count("devices WHERE id LIKE 'a-%'").await, 0);
    assert_eq!(db.count("pairings").await, 0);
    assert_eq!(db.count("push_subscriptions").await, 0);
    assert_eq!(db.count("account_tokens WHERE label = 'laptop'").await, 0);
    assert!(s.find_web_session(&hash_token("alice-cookie"), now).await.unwrap().is_none());
    assert_eq!(db.count("notification_prefs").await, 0);
    assert_eq!(db.count("preview_labels").await, 0);
    assert_eq!(db.count("preview_tokens").await, 0);
    assert_eq!(db.count("email_codes WHERE email = 'alice@example.com'").await, 0);
    let remaining = db.invite_ids().await;
    assert_eq!(remaining, vec![bob_unused], "invites alice created or redeemed are gone, bob's unused one stays");

    // bob 的完好
    assert!(s.get_account(&b.id).await.unwrap().is_some());
    assert!(s.get_device("b-web").await.unwrap().is_some());
    assert_eq!(db.count("account_tokens WHERE label = 'bob'").await, 1);
    assert_eq!(db.count("email_codes WHERE email = 'bob@example.com'").await, 1);

    // 审计：alice 的行（含设备行）都删了，只留一条 account_deleted；bob 的还在
    let audit = s.list_audit(50, None).await.unwrap();
    let alice_rows: Vec<_> = audit.iter().filter(|e| e.account_id.as_deref() == Some(a.id.as_str())).collect();
    assert_eq!(alice_rows.len(), 1, "{audit:?}");
    let row = alice_rows[0];
    assert_eq!(row.event, "account_deleted");
    assert_eq!(row.detail.as_ref().unwrap()["by"], "self");
    assert!(row.ip_prefix.is_none() && row.user_agent.is_none() && row.device_id.is_none());
    assert_eq!(row.ts_ms, now + 5);
    assert!(audit.iter().all(|e| e.device_id.as_deref() != Some("a-web")));
    assert_eq!(audit.iter().filter(|e| e.account_id.as_deref() == Some(b.id.as_str())).count(), 1);

    // 再删一次：不存在，不写第二条
    let again = NewAudit { event: "account_deleted", account_id: Some(a.id.clone()), ..Default::default() };
    assert!(!s.delete_account(&a.id, again, now + 6).await.unwrap());
    assert_eq!(s.list_audit(50, None).await.unwrap().len(), audit.len());
}

pub(crate) async fn searching_accounts_matches_names_and_emails_in_any_ascii_case_and_exact_ids_with_keyset_pages(
    db: &TestDb,
) {
    let s = &db.store;
    let t0 = now_ms();
    let mut ids = Vec::new();
    for i in 0..300u64 {
        let (name, email) = match i {
            7 => ("Émile Zola".to_string(), Some("emile@example.com".to_string())),
            250 => ("Someone Else".to_string(), Some("needle.person@example.com".to_string())),
            _ => (format!("user-{i}"), None),
        };
        let outcome = s.create_account_with_email(&name, email.as_deref(), t0 + i).await.unwrap();
        ids.push(account(outcome).id);
    }
    let names = |rows: Vec<Account>| rows.into_iter().map(|a| a.display_name).collect::<Vec<_>>();

    // 显示名：ASCII 部分不区分大小写；非 ASCII 的字符要大小写一致（SQLite 的 lower() 只折叠 ASCII，管理员搜索可以接受）
    assert_eq!(names(s.search_accounts("ZOLA", 10, None).await.unwrap()), ["Émile Zola"]);
    assert_eq!(names(s.search_accounts("  zola ", 10, None).await.unwrap()), ["Émile Zola"], "trimmed");
    assert_eq!(names(s.search_accounts("Émile", 10, None).await.unwrap()), ["Émile Zola"]);
    assert_eq!(names(s.search_accounts("éMILE", 10, None).await.unwrap()), Vec::<String>::new());
    // 邮箱子串，不区分大小写
    assert_eq!(names(s.search_accounts("NEEDLE.person", 10, None).await.unwrap()), ["Someone Else"]);
    assert_eq!(names(s.search_accounts("@Example.COM", 10, None).await.unwrap()), ["Émile Zola", "Someone Else"]);
    // 精确 id（子串不算）
    assert_eq!(s.search_accounts(&ids[200], 10, None).await.unwrap()[0].id, ids[200]);
    assert!(s.search_accounts(&ids[200][..10], 10, None).await.unwrap().is_empty());
    // 多个命中：按创建顺序，limit 截断，游标接着往后
    let all = s.search_accounts("user-10", 100, None).await.unwrap();
    assert_eq!(all.len(), 11, "user-10 and user-100..user-109");
    let first = s.search_accounts("user-10", 5, None).await.unwrap();
    let last = first.last().unwrap();
    let rest = s.search_accounts("user-10", 100, Some((last.created_at_ms, last.id.clone()))).await.unwrap();
    assert_eq!(first.len() + rest.len(), all.len());
    assert_eq!(rest.first().map(|a| &a.id), all.get(5).map(|a| &a.id));
    assert!(all.windows(2).all(|w| (w[0].created_at_ms, &w[0].id) < (w[1].created_at_ms, &w[1].id)));
    assert!(s.search_accounts("zzz-nothing", 10, None).await.unwrap().is_empty());
}

pub(crate) async fn like_wildcards_and_the_escape_character_in_a_search_are_literal(db: &TestDb) {
    let s = &db.store;
    let t0 = now_ms();
    let names_in = ["a_b", "axb", "100% sure", "1000 sure", r"back\slash", r"back\\slash", "plain"];
    for (i, name) in names_in.iter().enumerate() {
        s.create_account_with_email(name, None, t0 + i as u64).await.unwrap();
    }
    let names = |rows: Vec<Account>| rows.into_iter().map(|a| a.display_name).collect::<Vec<_>>();
    // `_` 只匹配下划线，不是「任意一个字符」
    assert_eq!(names(s.search_accounts("a_b", 10, None).await.unwrap()), ["a_b"]);
    assert_eq!(names(s.search_accounts("_", 10, None).await.unwrap()), ["a_b"]);
    // `%` 只匹配百分号，不是「任意串」
    assert_eq!(names(s.search_accounts("100%", 10, None).await.unwrap()), ["100% sure"]);
    assert_eq!(names(s.search_accounts("%", 10, None).await.unwrap()), ["100% sure"]);
    // 反斜杠本身（转义字符）也是字面字符：`k\s` 只命中一个反斜杠的，`k\\s` 只命中两个的，单独的 `\` 两个都命中
    assert_eq!(names(s.search_accounts(r"k\s", 10, None).await.unwrap()), [r"back\slash"]);
    assert_eq!(names(s.search_accounts(r"k\\s", 10, None).await.unwrap()), [r"back\\slash"]);
    assert_eq!(names(s.search_accounts(r"\", 10, None).await.unwrap()), [r"back\slash", r"back\\slash"]);
    // 空查询是「全部」（HTTP 层已经把空白的 q 当作没有 q）
    assert_eq!(s.search_accounts("  ", 100, None).await.unwrap().len(), 7);
}

// ---------------------------------------------------------------------------
// OAuth：身份与授权 state
// ---------------------------------------------------------------------------

fn oauth_state(hash: &str, provider: &str, purpose: OAuthPurpose, expires_at_ms: u64) -> NewOAuthState {
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

pub(crate) async fn an_identity_belongs_to_one_account_and_an_account_has_one_per_provider(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn identities_and_states_go_away_with_the_account(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    let a = s.create_account("a", false, now).await.unwrap();
    s.link_identity(&a.id, "github", "1", now).await.unwrap();
    let mut link = oauth_state("h1", "github", OAuthPurpose::Link, now + 60_000);
    link.account_id = Some(a.id.clone());
    s.put_oauth_state(link, now).await.unwrap();

    db.delete_account_row(&a.id).await;
    assert!(s.find_identity("github", "1").await.unwrap().is_none());
    assert!(s.take_oauth_state("h1", now).await.unwrap().is_none(), "the state of a deleted account is gone too");
}

pub(crate) async fn a_state_can_be_taken_once_and_not_after_it_expired(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    s.put_oauth_state(oauth_state("live", "github", OAuthPurpose::Login, now + 60_000), now).await.unwrap();
    s.put_oauth_state(oauth_state("old", "google", OAuthPurpose::Invite, now - 1), now - 100).await.unwrap();

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

pub(crate) async fn concurrent_takes_of_one_state_have_exactly_one_winner(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    s.put_oauth_state(oauth_state("one", "github", OAuthPurpose::Login, now + 60_000), now).await.unwrap();
    let s = s.clone();
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

pub(crate) async fn expired_states_are_purged_in_bulk(db: &TestDb) {
    let s = &db.store;
    let now = now_ms();
    s.put_oauth_state(oauth_state("a", "github", OAuthPurpose::Login, now - 5), now - 1000).await.unwrap();
    s.put_oauth_state(oauth_state("b", "github", OAuthPurpose::Login, now + 5000), now).await.unwrap();
    assert_eq!(s.purge_expired_oauth_states(now).await.unwrap(), 1);
    assert!(s.take_oauth_state("b", now).await.unwrap().is_some(), "the live one survives");
    assert_eq!(s.purge_expired_oauth_states(now).await.unwrap(), 0);
}

pub(crate) async fn creating_an_account_with_its_identity_is_all_or_nothing(db: &TestDb) {
    let s = &db.store;
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

pub(crate) async fn creating_an_account_through_an_invite_redeems_it_atomically(db: &TestDb) {
    let s = &db.store;
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

// ---------------------------------------------------------------------------
// 类型映射、排序规则
// ---------------------------------------------------------------------------

pub(crate) async fn text_ids_are_ordered_bytewise_whatever_the_database_collation(db: &TestDb) {
    // 配对按字节序归一化（`util::ordered`），列表的 id 兜底排序也按字节序：大写字母排在小写前面。
    // Postgres 默认的排序规则（比如 en_US）会把 `a1` 排在 `B1` 前面，所以库里和查询里都显式用了 `COLLATE "C"`。
    let s = &db.store;
    let now = now_ms();
    let acc = s.create_account("a", false, now).await.unwrap();
    for id in ["b1", "B1", "a1", "A1"] {
        s.create_device(new_device(&acc.id, id, &format!("token-{id}")), now).await.unwrap();
    }
    let ids: Vec<String> = s.list_devices(&acc.id).await.unwrap().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["A1", "B1", "a1", "b1"], "same timestamp: ordered by id, bytewise");
    s.insert_pairing("a1", "B1", now).await.unwrap();
    assert!(s.is_paired("B1", "a1").await.unwrap());
    assert_eq!(s.list_pairings_of_account(&acc.id).await.unwrap(), [("B1".to_string(), "a1".to_string())]);
    assert_eq!(s.peers_of("a1").await.unwrap(), ["B1"]);
}

pub(crate) async fn timestamps_and_counters_round_trip_at_full_width(db: &TestDb) {
    let s = &db.store;
    // 毫秒时间戳远大于 i32；`limit` 取 u32 的上限也不能出错
    let far = 4_102_444_800_000u64; // 2100-01-01
    let acc = s.create_account("a", false, far).await.unwrap();
    assert_eq!(s.get_account(&acc.id).await.unwrap().unwrap().created_at_ms, far);
    let sess = s.create_web_session(&acc.id, &hash_token("c"), "ua", "ip", far, far + 1000).await.unwrap();
    let (found, _) = s.find_web_session(&hash_token("c"), far + 1).await.unwrap().unwrap();
    assert_eq!((found.id, found.created_at_ms, found.expires_at_ms), (sess.id, far, far + 1000));
    assert_eq!(s.list_accounts(u32::MAX, None).await.unwrap().len(), 1);
    assert_eq!(s.list_invites(u32::MAX, None).await.unwrap().len(), 0);
    // 尝试次数上限取 u32::MAX：照样能算
    s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h", far), 3).await.unwrap();
    let got = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, far, u32::MAX).await.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].attempts, 1);
}

// ---------------------------------------------------------------------------
// 并发：每个用例让一批请求同时开跑，断言「先查再写」的逻辑没有被一起冲过
// ---------------------------------------------------------------------------

/// 起 `n` 个任务，等它们都就位后同时放行（尽量让它们在数据库里真的撞在一起），按序收集结果。
async fn together<T, F, Fut>(n: usize, f: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
{
    let gate = Arc::new(tokio::sync::Barrier::new(n));
    let tasks: Vec<_> = (0..n)
        .map(|i| {
            let (gate, work) = (gate.clone(), f(i));
            tokio::spawn(async move {
                gate.wait().await;
                work.await
            })
        })
        .collect();
    let mut out = Vec::with_capacity(n);
    for t in tasks {
        out.push(t.await.unwrap());
    }
    out
}

pub(crate) async fn concurrent_redemptions_of_one_invite_have_exactly_one_winner(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let admin = s.create_account("admin", true, now).await.unwrap();
    s.create_invite(&admin.id, &hash_token("inv"), None, now, now + 60_000).await.unwrap();
    let outcomes = together(16, |i| {
        let s = s.clone();
        async move { s.redeem_invite(&hash_token("inv"), &format!("guest-{i}"), now + 1).await.unwrap() }
    })
    .await;
    let redeemed = outcomes.iter().filter(|o| matches!(o, RedeemOutcome::Redeemed(_))).count();
    let used = outcomes.iter().filter(|o| matches!(o, RedeemOutcome::Used)).count();
    assert_eq!((redeemed, used), (1, 15), "{outcomes:?}");
    assert_eq!(s.stats().await.unwrap().accounts, 2, "admin plus the one guest: the losers left no account behind");
    let invites = s.list_invites(10, None).await.unwrap();
    assert_eq!(invites.len(), 1);
    assert!(invites[0].used_by.is_some());
}

pub(crate) async fn concurrent_sign_ups_through_one_email_identity_or_invite_create_exactly_one_account(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let admin = s.create_account("admin", true, now).await.unwrap();
    let accounts = || async { s.stats().await.unwrap().accounts };
    let base = accounts().await;

    // 同一个邮箱，不同的外部身份
    let outcomes = together(12, |i| {
        let s = s.clone();
        async move {
            let subject = format!("email-{i}");
            let new = NewIdentityAccount {
                display_name: "Ada",
                email: Some("ada@example.com"),
                provider: "github",
                subject: &subject,
                invite_hash: None,
            };
            s.create_account_with_identity(new, now).await.unwrap()
        }
    })
    .await;
    let created = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::Created(_))).count();
    let taken = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::EmailTaken)).count();
    assert_eq!((created, taken), (1, 11), "{outcomes:?}");
    assert_eq!(accounts().await, base + 1, "the losers left no account behind");

    // 同一个外部身份，没有邮箱
    let outcomes = together(12, |_| {
        let s = s.clone();
        async move {
            let new = NewIdentityAccount {
                display_name: "Bob",
                email: None,
                provider: "google",
                subject: "same-subject",
                invite_hash: None,
            };
            s.create_account_with_identity(new, now).await.unwrap()
        }
    })
    .await;
    let created = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::Created(_))).count();
    let taken = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::IdentityTaken)).count();
    assert_eq!((created, taken), (1, 11), "{outcomes:?}");
    assert_eq!(accounts().await, base + 2, "no orphan accounts from the lost races");

    // 同一个邀请，不同的外部身份：邀请只被用掉一次，输的几个不留账号
    s.create_invite(&admin.id, &hash_token("one-use"), None, now, now + 60_000).await.unwrap();
    let outcomes = together(12, |i| {
        let s = s.clone();
        async move {
            let subject = format!("invited-{i}");
            let new = NewIdentityAccount {
                display_name: "Guest",
                email: None,
                provider: "github",
                subject: &subject,
                invite_hash: Some(&hash_token("one-use")),
            };
            s.create_account_with_identity(new, now).await.unwrap()
        }
    })
    .await;
    let created = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::Created(_))).count();
    let used = outcomes.iter().filter(|o| matches!(o, IdentityAccountOutcome::InviteUsed)).count();
    assert_eq!((created, used), (1, 11), "{outcomes:?}");
    assert_eq!(accounts().await, base + 3);

    // 同一个邮箱，直接注册
    let outcomes = together(12, |i| {
        let s = s.clone();
        async move { s.create_account_with_email(&format!("u{i}"), Some("race@example.com"), now).await.unwrap() }
    })
    .await;
    let created = outcomes.iter().filter(|o| matches!(o, CreateAccountOutcome::Created(_))).count();
    assert_eq!(created, 1, "{outcomes:?}");
    assert_eq!(accounts().await, base + 4);
}

pub(crate) async fn concurrent_claims_of_one_email_address_or_external_identity_have_one_winner(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let mut ids = Vec::new();
    for i in 0..10 {
        ids.push(s.create_account(&format!("a{i}"), false, now).await.unwrap().id);
    }
    // 十个账号同时要同一个邮箱
    let outcomes = together(10, |i| {
        let (s, id) = (s.clone(), ids[i].clone());
        async move { s.set_account_email(&id, Some("shared@example.com")).await.unwrap() }
    })
    .await;
    assert_eq!(outcomes.iter().filter(|o| **o == SetEmailOutcome::Done).count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|o| **o == SetEmailOutcome::Taken).count(), 9, "{outcomes:?}");
    // 十个账号同时要同一个外部身份
    let outcomes = together(10, |i| {
        let (s, id) = (s.clone(), ids[i].clone());
        async move { s.link_identity(&id, "github", "shared", now).await.unwrap() }
    })
    .await;
    assert_eq!(outcomes.iter().filter(|o| **o == LinkIdentityOutcome::Linked).count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|o| **o == LinkIdentityOutcome::SubjectTaken).count(), 9, "{outcomes:?}");
    // 一个账号同时绑同一个提供方的十个不同身份：只有一个成
    let outcomes = together(10, |i| {
        let (s, id) = (s.clone(), ids[0].clone());
        async move { s.link_identity(&id, "google", &format!("g{i}"), now).await.unwrap() }
    })
    .await;
    assert_eq!(outcomes.iter().filter(|o| **o == LinkIdentityOutcome::Linked).count(), 1, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|o| **o == LinkIdentityOutcome::ProviderTaken).count(), 9, "{outcomes:?}");
    let google = s.list_identities(&ids[0]).await.unwrap().into_iter().filter(|i| i.provider == "google").count();
    assert_eq!(google, 1);
}

pub(crate) async fn concurrent_registrations_of_codes_in_one_group_keep_exactly_the_configured_number(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    // 多轮，每轮一个新的组：每个登记在插入之后修剪，只有同一组的登记排队，修剪才看得到别人刚插入的行
    for round in 0..15 {
        let email = format!("x{round}@example.com");
        together(12, |i| {
            let (s, email) = (s.clone(), email.clone());
            async move {
                let code = code_row(&email, EmailPurpose::Login, None, &format!("h{i}"), now + i as u64);
                s.put_email_code(code, 3).await.unwrap();
            }
        })
        .await;
        let kept = s.list_email_codes(&email, EmailPurpose::Login).await.unwrap();
        assert_eq!(kept.len(), 3, "round {round}: 12 racing registrations still leave exactly `keep` codes: {kept:?}");
    }
    // 别的组不受影响
    s.put_email_code(code_row("y@example.com", EmailPurpose::Login, None, "hy", now), 3).await.unwrap();
    assert_eq!(s.list_email_codes("y@example.com", EmailPurpose::Login).await.unwrap().len(), 1);
}

pub(crate) async fn concurrent_claims_of_one_email_code_have_exactly_one_winner(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    s.put_email_code(code_row("x@example.com", EmailPurpose::Login, None, "h1", now), 3).await.unwrap();
    let id = s.attempt_email_codes("x@example.com", EmailPurpose::Login, None, now, 5).await.unwrap()[0].id;
    let claims = together(12, |_| {
        let s = s.clone();
        async move { s.take_email_code(id).await.unwrap() }
    })
    .await;
    assert_eq!(claims.iter().filter(|c| **c).count(), 1);
}

pub(crate) async fn concurrent_preview_requests_never_exceed_the_label_and_token_caps(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let acc = preview_fixture(&*s, now).await;
    // 一个节点同时绑 20 个不同的 label：只有 3 个成
    let outcomes = together(20, |i| {
        let (s, acc) = (s.clone(), acc.clone());
        async move {
            s.issue_preview_token(
                new_token(&format!("label-{i}"), &format!("l{i}"), "n1", "w", &acc, now + 100_000),
                now,
                PREVIEW_LIMITS,
            )
            .await
            .unwrap()
        }
    })
    .await;
    assert_eq!(outcomes.iter().filter(|o| **o == IssueOutcome::Issued).count(), 3, "{outcomes:?}");
    assert_eq!(outcomes.iter().filter(|o| **o == IssueOutcome::TooManyLabels).count(), 17, "{outcomes:?}");
    assert_eq!(s.list_preview_labels("n1").await.unwrap().len(), 3);
    let label = s.list_preview_labels("n1").await.unwrap().remove(0);
    // 同一台设备同时要 20 个令牌：最后只留最新的 3 个
    let outcomes = together(20, |i| {
        let (s, acc, label) = (s.clone(), acc.clone(), label.clone());
        async move {
            s.issue_preview_token(
                new_token(&format!("tok-{i}"), &label, "n1", "w2", &acc, now + 100_000),
                now + 10 + i as u64,
                PREVIEW_LIMITS,
            )
            .await
            .unwrap()
        }
    })
    .await;
    assert!(outcomes.iter().all(|o| *o == IssueOutcome::Issued), "{outcomes:?}");
    assert_eq!(s.list_preview_tokens_of_device("w2").await.unwrap().len(), 3);
}

pub(crate) async fn concurrent_deletions_of_one_account_write_exactly_one_tombstone(db: &TestDb) {
    let s = db.store.clone();
    let now = now_ms();
    let acc = s.create_account_with_email("a", Some("a@example.com"), now).await.unwrap();
    let CreateAccountOutcome::Created(acc) = acc else { panic!("created") };
    s.create_device(new_device(&acc.id, "dev", "t"), now).await.unwrap();
    let results = together(8, |_| {
        let (s, id) = (s.clone(), acc.id.clone());
        async move {
            let tombstone = NewAudit {
                event: "account_deleted",
                account_id: Some(id.clone()),
                detail: Some(serde_json::json!({ "by": "self" })),
                ..Default::default()
            };
            s.delete_account(&id, tombstone, now).await.unwrap()
        }
    })
    .await;
    assert_eq!(results.iter().filter(|r| **r).count(), 1, "{results:?}");
    let tombstones = s.list_audit(50, None).await.unwrap().into_iter().filter(|e| e.event == "account_deleted").count();
    assert_eq!(tombstones, 1);
    assert!(s.get_account(&acc.id).await.unwrap().is_none());
    assert!(s.get_device("dev").await.unwrap().is_none());
}

pub(crate) async fn revoking_a_device_while_it_is_being_paired_leaves_a_consistent_state(db: &TestDb) {
    // 吊销和「同一台设备的配对」同时发生，两种先后都合法：配对先写，吊销把它删掉并报出对端；吊销先做，配对写在已吊销的设备上
    // （和 SQLite 一样，`insert_pairing` 不检查设备状态，调用方负责）。不合法的是两边对不上：报出了对端、配对却还在，或者
    // 配对不在、也没有报出对端。
    let s = db.store.clone();
    let now = now_ms();
    let acc = s.create_account("a", false, now).await.unwrap();
    for round in 0..10 {
        let (a, b) = (format!("a{round}"), format!("b{round}"));
        s.create_device(new_device(&acc.id, &a, &format!("ta{round}")), now).await.unwrap();
        s.create_device(new_device(&acc.id, &b, &format!("tb{round}")), now).await.unwrap();
        let (s1, s2) = (s.clone(), s.clone());
        let (a1, b1, a2) = (a.clone(), b.clone(), a.clone());
        let (pair, revoke) = tokio::join!(
            tokio::spawn(async move { s1.insert_pairing(&a1, &b1, now).await.unwrap() }),
            tokio::spawn(async move { s2.revoke_device(&a2, now + 1).await.unwrap() }),
        );
        pair.unwrap();
        let peers = revoke.unwrap();
        let paired = s.is_paired(&a, &b).await.unwrap();
        assert_eq!(peers.is_empty(), paired, "round {round}: revoke reported {peers:?}, paired = {paired}");
        assert!(s.get_device(&a).await.unwrap().unwrap().revoked_at_ms.is_some());
    }
}

pub(crate) async fn batches_spanning_two_accounts_complete_whatever_the_order(db: &TestDb) {
    // 一批设备可以跨账号：并发的几批以相反的账号顺序到达，不能互相卡死，也不能冲过各自账号的上限。
    let s = db.store.clone();
    let now = now_ms();
    let a = s.create_account("a", false, now).await.unwrap();
    let b = s.create_account("b", false, now).await.unwrap();
    let limits = DeviceLimits { devices: 10, daemons: 10 };
    let outcomes = together(20, |i| {
        let (s, a, b) = (s.clone(), a.id.clone(), b.id.clone());
        async move {
            let (first, second) = if i % 2 == 0 { (&a, &b) } else { (&b, &a) };
            let batch = vec![
                device(first, &format!("x{i}"), DeviceKind::Web),
                device(second, &format!("y{i}"), DeviceKind::Web),
            ];
            s.create_devices_within_limits(batch, limits, now).await.unwrap()
        }
    })
    .await;
    let created = outcomes.iter().filter(|o| matches!(o, CreateDevicesOutcome::Created(_))).count();
    assert_eq!(created, 10, "each account has room for 10 devices, every batch adds one to each: {outcomes:?}");
    assert_eq!(s.count_devices(&a.id).await.unwrap().0, 10);
    assert_eq!(s.count_devices(&b.id).await.unwrap().0, 10);
}

pub(crate) async fn repeated_races_for_the_last_free_slot_never_overshoot(db: &TestDb) {
    // 每一轮：一个账号只剩一个名额，8 个请求同时来要。多轮是为了让「数」和「插」之间的空档真的被撞上。
    let s = db.store.clone();
    let limits = DeviceLimits { devices: 1, daemons: 1 };
    for round in 0..25 {
        let a = s.create_account(&format!("a{round}"), false, now_ms()).await.unwrap();
        let outcomes = together(8, |i| {
            let (s, account) = (s.clone(), a.id.clone());
            async move {
                let batch = vec![device(&account, &format!("r{round}-{i}"), DeviceKind::Web)];
                s.create_devices_within_limits(batch, limits, now_ms()).await.unwrap()
            }
        })
        .await;
        let created = outcomes.iter().filter(|o| matches!(o, CreateDevicesOutcome::Created(_))).count();
        assert_eq!(created, 1, "round {round}: {outcomes:?}");
        assert_eq!(s.count_devices(&a.id).await.unwrap().0, 1, "round {round}");
    }
}
