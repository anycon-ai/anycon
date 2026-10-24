//! SQLite 实现里 `multi` 模式用到的那几个操作：设备配额下的原子建设备、账号删除、管理员标记。
//!
//! `Store` 里对应的方法（`// ---- multi：…` 那一节）只是转到这里；SQL 放在子模块里，是为了不和 `sqlite.rs`
//! 里按表分节的主体交错。

use sqlx::Row;

use super::{be, SqliteStore};
use crate::store::{
    kind_to_str, CreateDevicesOutcome, Device, DeviceLimit, DeviceLimits, NewAudit, NewDevice, StoreResult,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anycon_protocol::{DeviceKind, NotifyCategory};

    use super::*;
    use crate::store::{
        Account, CreateAccountOutcome, EmailCode, EmailPurpose, IssueOutcome, NewPreviewToken, NotificationPref,
        PreviewIssueLimits, Store,
    };
    use crate::util::{hash_token, now_ms};

    async fn store() -> (Arc<SqliteStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Arc::new(SqliteStore::open(None, dir.path()).await.unwrap()), dir)
    }

    fn device(account: &str, id: &str, kind: DeviceKind) -> NewDevice {
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

    const LIMITS: DeviceLimits = DeviceLimits { devices: 3, daemons: 2 };

    fn account(outcome: CreateAccountOutcome) -> Account {
        match outcome {
            CreateAccountOutcome::Created(a) => a,
            CreateAccountOutcome::EmailTaken => panic!("the email is free"),
        }
    }

    async fn created(s: &SqliteStore, devices: Vec<NewDevice>, limits: DeviceLimits) -> bool {
        matches!(
            s.create_devices_within_limits(devices, limits, now_ms()).await.unwrap(),
            CreateDevicesOutcome::Created(_)
        )
    }

    #[tokio::test]
    async fn counts_cover_live_devices_only_and_split_out_daemons() {
        let (s, _d) = store().await;
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

    #[tokio::test]
    async fn devices_are_created_within_the_limits_and_a_batch_is_all_or_nothing() {
        let (s, _d) = store().await;
        let a = s.create_account("a", false, now_ms()).await.unwrap();
        let b = s.create_account("b", false, now_ms()).await.unwrap();
        assert!(created(&s, vec![device(&a.id, "d1", DeviceKind::Daemon)], LIMITS).await);
        // 两台一批，只剩 2 个设备名额：整批进去（总数到上限 3）
        assert!(
            created(&s, vec![device(&a.id, "w1", DeviceKind::Web), device(&a.id, "w2", DeviceKind::Web)], LIMITS).await
        );
        assert_eq!(s.count_devices(&a.id).await.unwrap(), (3, 1));
        // 满了：再来一台就是设备总数超了，什么都不写
        match s
            .create_devices_within_limits(vec![device(&a.id, "w3", DeviceKind::Web)], LIMITS, now_ms())
            .await
            .unwrap()
        {
            CreateDevicesOutcome::Exceeded(DeviceLimit::Devices) => {}
            other => panic!("{other:?}"),
        }
        assert!(s.get_device("w3").await.unwrap().is_none());
        // 别的账号不受影响
        assert!(created(&s, vec![device(&b.id, "b1", DeviceKind::Web)], LIMITS).await);

        // 吊销空出名额；一批里第二台超了，第一台也不留
        s.revoke_device("w1", now_ms()).await.unwrap();
        s.revoke_device("w2", now_ms()).await.unwrap();
        let batch = vec![
            device(&a.id, "x1", DeviceKind::Web),
            device(&a.id, "x2", DeviceKind::Web),
            device(&a.id, "x3", DeviceKind::Web),
        ];
        assert!(matches!(
            s.create_devices_within_limits(batch, LIMITS, now_ms()).await.unwrap(),
            CreateDevicesOutcome::Exceeded(DeviceLimit::Devices)
        ));
        assert_eq!(s.count_devices(&a.id).await.unwrap(), (1, 1), "the first two of the batch were rolled back");
        assert!(s.get_device("x1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_daemon_limit_is_separate_and_reported_when_it_is_the_one_hit() {
        let (s, _d) = store().await;
        let a = s.create_account("a", false, now_ms()).await.unwrap();
        let limits = DeviceLimits { devices: 10, daemons: 2 };
        assert!(
            created(&s, vec![device(&a.id, "d1", DeviceKind::Daemon), device(&a.id, "w1", DeviceKind::Web)], limits)
                .await
        );
        assert!(created(&s, vec![device(&a.id, "d2", DeviceKind::Daemon)], limits).await);
        match s
            .create_devices_within_limits(vec![device(&a.id, "d3", DeviceKind::Daemon)], limits, now_ms())
            .await
            .unwrap()
        {
            CreateDevicesOutcome::Exceeded(DeviceLimit::Daemons) => {}
            other => panic!("{other:?}"),
        }
        // 非 daemon 设备不受 daemon 上限影响
        assert!(created(&s, vec![device(&a.id, "w2", DeviceKind::Cli)], limits).await);
        // 吊销一台 daemon 就空出来
        s.revoke_device("d1", now_ms()).await.unwrap();
        assert!(created(&s, vec![device(&a.id, "d3", DeviceKind::Daemon)], limits).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_creations_for_one_account_never_overshoot() {
        let (s, _d) = store().await;
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

    #[tokio::test]
    async fn admin_flag_is_set_and_cleared_and_unknown_accounts_report_false() {
        let (s, _d) = store().await;
        let a = s.create_account("a", false, now_ms()).await.unwrap();
        assert!(s.set_account_admin(&a.id, true).await.unwrap());
        assert!(s.get_account(&a.id).await.unwrap().unwrap().is_admin);
        assert!(s.set_account_admin(&a.id, false).await.unwrap());
        assert!(!s.get_account(&a.id).await.unwrap().unwrap().is_admin);
        assert!(!s.set_account_admin("nope", true).await.unwrap());
    }

    async fn count(s: &SqliteStore, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&s.pool).await.unwrap()
    }

    #[tokio::test]
    async fn deleting_an_account_removes_everything_it_owns_and_leaves_the_rest() {
        let (s, _d) = store().await;
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
        s.set_notification_prefs(
            &a.id,
            &[NotificationPref { category: NotifyCategory::Error, push: true, email: false }],
        )
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
        sqlx::query("UPDATE invites SET used_by = ?, used_at_ms = ? WHERE id = ?")
            .bind(&b.id)
            .bind(now as i64)
            .bind(&alice_used_by_bob)
            .execute(&s.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE invites SET used_by = ?, used_at_ms = ? WHERE id = ?")
            .bind(&a.id)
            .bind(now as i64)
            .bind(&bob_used_by_alice)
            .execute(&s.pool)
            .await
            .unwrap();
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
        assert_eq!(count(&s, "SELECT COUNT(*) FROM devices WHERE id LIKE 'a-%'").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM pairings").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM push_subscriptions").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM account_tokens WHERE label = 'laptop'").await, 0);
        assert!(s.find_web_session(&hash_token("alice-cookie"), now).await.unwrap().is_none());
        assert_eq!(count(&s, "SELECT COUNT(*) FROM notification_prefs").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM preview_labels").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM preview_tokens").await, 0);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM email_codes WHERE email = 'alice@example.com'").await, 0);
        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT id FROM invites ORDER BY id").fetch_all(&s.pool).await.unwrap();
        assert_eq!(remaining, vec![bob_unused], "invites alice created or redeemed are gone, bob's unused one stays");

        // bob 的完好
        assert!(s.get_account(&b.id).await.unwrap().is_some());
        assert!(s.get_device("b-web").await.unwrap().is_some());
        assert_eq!(count(&s, "SELECT COUNT(*) FROM account_tokens WHERE label = 'bob'").await, 1);
        assert_eq!(count(&s, "SELECT COUNT(*) FROM email_codes WHERE email = 'bob@example.com'").await, 1);

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

    #[tokio::test]
    async fn searching_accounts_matches_names_and_emails_in_any_case_and_exact_ids_across_chunks() {
        let (s, _d) = store().await;
        let t0 = now_ms();
        let mut ids = Vec::new();
        // 1100 个账号，跨过扫描的批大小（500）；两个要找的人分别在第一批和最后一批
        for i in 0..1100u64 {
            let (name, email) = match i {
                7 => ("Émile Zola".to_string(), Some("emile@example.com".to_string())),
                1050 => ("Someone Else".to_string(), Some("Needle.Person@Example.com".to_lowercase())),
                _ => (format!("user-{i}"), None),
            };
            let outcome = s.create_account_with_email(&name, email.as_deref(), t0 + i).await.unwrap();
            ids.push(account(outcome).id);
        }
        let names = |rows: Vec<Account>| rows.into_iter().map(|a| a.display_name).collect::<Vec<_>>();

        // 非 ASCII 的显示名也不区分大小写（SQLite 的 lower() 做不到）
        assert_eq!(names(s.search_accounts("éMILE", 10, None).await.unwrap()), ["Émile Zola"]);
        assert_eq!(names(s.search_accounts("  zola ", 10, None).await.unwrap()), ["Émile Zola"], "trimmed");
        // 邮箱子串，在最后一批里
        assert_eq!(names(s.search_accounts("NEEDLE.person", 10, None).await.unwrap()), ["Someone Else"]);
        // 精确 id（子串不算）
        assert_eq!(s.search_accounts(&ids[500], 10, None).await.unwrap()[0].id, ids[500]);
        assert!(s.search_accounts(&ids[500][..10], 10, None).await.unwrap().is_empty());
        // 多个命中：按创建顺序，limit 截断，游标接着往后
        let all = s.search_accounts("user-100", 100, None).await.unwrap();
        assert_eq!(all.len(), 11, "user-100 and user-1000..user-1009");
        let first = s.search_accounts("user-100", 5, None).await.unwrap();
        let last = first.last().unwrap();
        let rest = s.search_accounts("user-100", 100, Some((last.created_at_ms, last.id.clone()))).await.unwrap();
        assert_eq!(first.len() + rest.len(), all.len());
        assert_eq!(rest.first().map(|a| &a.id), all.get(5).map(|a| &a.id));
        // LIKE 通配符不是通配符
        assert!(s.search_accounts("%", 10, None).await.unwrap().is_empty());
        assert!(s.search_accounts("zzz-nothing", 10, None).await.unwrap().is_empty());
    }
}
