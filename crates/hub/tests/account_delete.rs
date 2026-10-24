//! 删账号（`DELETE /api/me`、`DELETE /api/admin/accounts/:id`）和管理员账号接口
//! （`?q=` 搜索、授予 / 取消管理员）的集成测试。

mod common;
mod multi_support;

use anycon_hub::util::now_ms;
use anycon_protocol::{Control, DeviceKind, EnrollFailReason, PairFailReason};
use common::*;
use multi_support::*;
use reqwest::Method;
use serde_json::{json, Value};

const BOSS: &str = "boss@example.com";

async fn multi_with_boss() -> (TestHub, String, String) {
    let hub =
        multi_hub_with(limits(10, 3, 10, 50), |c| c.admin_emails = vec![BOSS.into(), "root@example.com".into()]).await;
    let (boss, boss_cookie) = account(&hub, "Boss", Some(BOSS)).await;
    (hub, boss, boss_cookie)
}

async fn delete(hub: &TestHub, path: &str, cookie: &str) -> reqwest::Response {
    hub.send(Method::DELETE, path, cookie, Value::Null).await
}

// ---------------------------------------------------------------------------
// DELETE /api/me
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_my_account_removes_its_data_closes_its_connections_and_clears_the_cookie() {
    let (hub, _boss, boss_cookie) = multi_with_boss().await;
    let store = hub.handle.store();
    let (alice, alice_cookie) = account(&hub, "alice", Some("alice@example.com")).await;
    let second_browser = session_created_at(&hub, &alice, now_ms()).await;
    let (bob, bob_cookie) = account(&hub, "bob", Some("bob@example.com")).await;

    // alice：一台在线的 daemon、一台在线的浏览器（已配对），推送订阅，一条进行中的配对登记和一个进行中的接入会话
    let node = hub.seed_device_in(&alice, DeviceKind::Daemon, "node").await;
    let web = hub.seed_device_in(&alice, DeviceKind::Web, "web").await;
    hub.seed_paired(&node, &web).await;
    store.upsert_push_subscription(&web.id, "https://push.example/endpoint", "p256dh", "auth", now_ms()).await.unwrap();
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let (mut w, _) = connect_with_quota(&hub, Some(&web.token), DeviceKind::Web, "web").await;
    n.drain_controls(SHORT).await;
    let match_id = new_code().1;
    n.pair_begin(&match_id).await;
    let mut initiator = temp_with_cookie(&hub, &alice_cookie, DeviceKind::Web, "enrolling").await;
    initiator.send_control(&Control::EnrollBegin { node: node.id.clone() }).await;
    let enroll_id = match initiator.recv_control().await {
        Control::EnrollStarted { enroll_id, .. } => enroll_id,
        other => panic!("expected enroll_started, got {other:?}"),
    };
    assert!(matches!(n.recv_control().await, Control::EnrollRequest { .. }));
    let mut idle = temp_with_cookie(&hub, &alice_cookie, DeviceKind::Cli, "idle with her cookie").await;

    // bob：不相关的账号，在线，有审计行
    let theirs = hub.seed_device_in(&bob, DeviceKind::Web, "theirs").await;
    let (mut b, _) = connect_with_quota(&hub, Some(&theirs.token), DeviceKind::Web, "theirs").await;
    for (event, account_id, device_id) in [
        ("login_ok", Some(&alice), None),
        ("device_paired", Some(&alice), Some(web.id.as_str())),
        ("login_ok", Some(&bob), None),
    ] {
        store
            .insert_audit(
                anycon_hub::store::NewAudit {
                    event,
                    account_id: account_id.cloned(),
                    device_id: device_id.map(str::to_string),
                    ip_prefix: Some("10.1.2.0/24".into()),
                    user_agent: Some("agent".into()),
                    detail: None,
                },
                now_ms(),
            )
            .await
            .unwrap();
    }

    let res = delete(&hub, "/api/me", &alice_cookie).await;
    assert_eq!(res.status(), 204);
    let cleared = set_cookie_raw(&res);
    assert!(cleared.starts_with("anycon_session=;") && cleared.contains("Max-Age=0"), "{cleared}");

    // 连接：永久连接、带她 cookie 的临时连接都以 4004 关闭
    assert_eq!(closed_with(&mut n).await, Some(4004));
    assert_eq!(closed_with(&mut w).await, Some(4004));
    assert_eq!(closed_with(&mut idle).await, Some(4004));
    // 进行中的接入会话作废：发起方被告知，然后也被关掉
    assert_eq!(
        initiator.recv_control().await,
        Control::EnrollFailed { enroll_id: Some(enroll_id), reason: EnrollFailReason::PeerGone }
    );
    assert_eq!(closed_with(&mut initiator).await, Some(4004));
    assert_eq!(hub.handle.state.registry.permanent_count_of(&alice), 0);
    assert!(!hub.handle.state.registry.is_online(&node.id));
    // 进行中的配对登记作废：别人拿着短码也 join 不上
    let mut late = FakeDevice::connect(&hub, None, DeviceKind::Cli, "late").await;
    late.send_control(&Control::PairJoin { match_id }).await;
    assert_eq!(late.recv_control().await, Control::PairFailed { reason: PairFailReason::NotFound });

    // 库：账号、设备、配对、会话、订阅都没了；token 认证不了
    assert!(store.get_account(&alice).await.unwrap().is_none());
    assert!(store.find_account_by_email("alice@example.com").await.unwrap().is_none());
    assert!(store.get_device(&node.id).await.unwrap().is_none() && store.get_device(&web.id).await.unwrap().is_none());
    assert!(store.peers_of(&node.id).await.unwrap().is_empty());
    assert!(store.get_push_subscription_of_device(&web.id).await.unwrap().is_none());
    for cookie in [&alice_cookie, &second_browser] {
        assert_eq!(hub.get("/api/me", cookie).await.status(), 401, "all of her sessions are gone");
    }
    assert!(try_connect(&hub, Some(&node.token), DeviceKind::Daemon, "node").await.is_err());

    // 审计：alice 的行（账号行和设备行）都删了，只留一条 account_deleted（谁删的，没有 IP / UA）；别人的行不动
    let rows = audit_rows(&hub).await;
    let hers: Vec<_> = rows.iter().filter(|e| e.account_id.as_deref() == Some(alice.as_str())).collect();
    assert_eq!(hers.len(), 1, "{rows:?}");
    assert_eq!(hers[0].event, "account_deleted");
    assert_eq!(hers[0].detail, Some(json!({ "by": "self" })));
    assert!(hers[0].ip_prefix.is_none() && hers[0].user_agent.is_none() && hers[0].device_id.is_none());
    assert!(rows.iter().all(|e| e.device_id.as_deref() != Some(web.id.as_str())));
    assert_eq!(rows.iter().filter(|e| e.account_id.as_deref() == Some(bob.as_str())).count(), 1);
    // 管理员在审计页能看到
    let listed = json_of(hub.get("/api/admin/audit", &boss_cookie).await).await;
    assert!(listed
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["event"] == "account_deleted" && e["account_id"] == alice.as_str()));

    // bob 完全不受影响：还在线，没收到任何东西
    b.expect_silence(SHORT).await;
    assert_eq!(hub.get("/api/me", &bob_cookie).await.status(), 200);
    assert_eq!(hub.get("/api/devices", &bob_cookie).await.status(), 200);
}

#[tokio::test]
async fn recent_authentication_is_required_to_delete_an_account() {
    let (hub, _boss, _) = multi_with_boss().await;
    let (alice, _fresh) = account(&hub, "alice", Some("alice@example.com")).await;
    let node = hub.seed_device_in(&alice, DeviceKind::Daemon, "node").await;
    let minutes = |m: u64| now_ms() - m * 60_000;

    // 11 分钟前登录的会话：reauth_required，什么都没动
    let stale = session_created_at(&hub, &alice, minutes(11)).await;
    let res = delete(&hub, "/api/me", &stale).await;
    assert_eq!(res.status(), 403);
    assert_eq!(error_code(res).await, "reauth_required");
    assert!(hub.handle.store().get_account(&alice).await.unwrap().is_some());
    assert!(hub.handle.store().get_device(&node.id).await.unwrap().is_some());
    assert_eq!(hub.get("/api/me", &stale).await.status(), 200, "the session itself is still fine");
    assert!(audit_rows(&hub).await.iter().all(|e| e.event != "account_deleted"));

    // 滑动续期不算重新认证：会话一直在用也不行（创建时间才算）
    for _ in 0..2 {
        assert_eq!(hub.get("/api/me", &stale).await.status(), 200);
    }
    assert_eq!(delete(&hub, "/api/me", &stale).await.status(), 403);

    // 9 分钟前的会话可以
    let recent = session_created_at(&hub, &alice, minutes(9)).await;
    assert_eq!(delete(&hub, "/api/me", &recent).await.status(), 204);
    assert!(hub.handle.store().get_account(&alice).await.unwrap().is_none());
    // 再删一次：会话已经没了
    assert_eq!(delete(&hub, "/api/me", &recent).await.status(), 401);
}

#[tokio::test]
async fn deleting_requires_a_session_and_the_csrf_header() {
    let (hub, ..) = multi_with_boss().await;
    let (alice, cookie) = account(&hub, "alice", None).await;
    assert_eq!(hub.http.delete(hub.url("/api/me")).header("x-anycon", "1").send().await.unwrap().status(), 401);
    let no_header = hub.http.delete(hub.url("/api/me")).header("cookie", &cookie).send().await.unwrap();
    assert_eq!(no_header.status(), 403);
    assert_eq!(error_code(no_header).await, "csrf");
    assert!(hub.handle.store().get_account(&alice).await.unwrap().is_some());
}

#[tokio::test]
async fn single_mode_guards_the_bootstrap_admin_and_frees_invites_a_deleted_guest_used() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;

    // bootstrap 管理员删不了自己，会话再新也一样
    let res = delete(&hub, "/api/me", &admin).await;
    assert_eq!(res.status(), 409);
    assert_eq!(error_code(res).await, "bootstrap_admin");
    assert_eq!(hub.get("/api/me", &admin).await.status(), 200);

    // 受邀的账号可以删
    let invite: Value = json_of(hub.send(Method::POST, "/api/invites", &admin, json!({})).await).await;
    let code = invite["code"].as_str().unwrap().to_string();
    let redeem = |code: &str| {
        let req = hub.http.post(hub.url("/auth/invite")).header("x-anycon", "1").json(&json!({ "code": code }));
        async move { req.send().await.unwrap() }
    };
    let res = redeem(&code).await;
    assert_eq!(res.status(), 204);
    let guest_cookie = cookie_pair(&res);
    let guest: Value = json_of(hub.get("/api/me", &guest_cookie).await).await;
    let guest_id = guest["id"].as_str().unwrap().to_string();
    assert_eq!(delete(&hub, "/api/me", &guest_cookie).await.status(), 204);
    assert!(hub.handle.store().get_account(&guest_id).await.unwrap().is_none());

    // 邀请码不会因为兑换它的账号没了就重新可用（`invites.used_by` 的外键是 SET NULL，不处理的话它会变回「未使用」）
    let again = redeem(&code).await;
    assert_eq!(again.status(), 404);
    assert_eq!(error_code(again).await, "invite_invalid");
    let invites = json_of(hub.get("/api/invites", &admin).await).await;
    assert!(invites.as_array().unwrap().is_empty(), "the consumed invite row is gone: {invites}");
    // 管理员自己的审计没动，而且有被删账号的一条记录
    let rows = audit_rows(&hub).await;
    assert!(rows.iter().any(|e| e.event == "account_deleted" && e.account_id.as_deref() == Some(guest_id.as_str())));
    assert!(rows.iter().all(|e| e.event != "invite_used" || e.account_id.as_deref() != Some(guest_id.as_str())));
}

// ---------------------------------------------------------------------------
// DELETE /api/admin/accounts/:id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_admin_can_delete_another_account_with_the_same_effect() {
    let (hub, boss, boss_cookie) = multi_with_boss().await;
    let (victim, victim_cookie) = account(&hub, "victim", Some("victim@example.com")).await;
    let node = hub.seed_device_in(&victim, DeviceKind::Daemon, "node").await;
    let (mut n, _) = connect_with_quota(&hub, Some(&node.token), DeviceKind::Daemon, "node").await;
    let mut temp = temp_with_cookie(&hub, &victim_cookie, DeviceKind::Web, "temp").await;

    let res = delete(&hub, &format!("/api/admin/accounts/{victim}"), &boss_cookie).await;
    assert_eq!(res.status(), 204);
    assert!(res.headers().get("set-cookie").is_none(), "the admin's own session is untouched");
    assert_eq!(closed_with(&mut n).await, Some(4004));
    assert_eq!(closed_with(&mut temp).await, Some(4004));
    assert!(hub.handle.store().get_account(&victim).await.unwrap().is_none());
    assert_eq!(hub.get("/api/me", &victim_cookie).await.status(), 401);
    let rows = audit_rows(&hub).await;
    let deleted: Vec<_> = rows.iter().filter(|e| e.event == "account_deleted").collect();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        (deleted[0].account_id.as_deref(), &deleted[0].detail),
        (Some(victim.as_str()), &Some(json!({ "by": "admin" })))
    );
    assert!(deleted[0].ip_prefix.is_none() && deleted[0].user_agent.is_none());
    assert_eq!(hub.get("/api/me", &boss_cookie).await.status(), 200);

    // 已经删了：404；自己：400；没有这个 id：404
    let gone = delete(&hub, &format!("/api/admin/accounts/{victim}"), &boss_cookie).await;
    assert_failure(gone, 404, "not_found").await;
    let myself = delete(&hub, &format!("/api/admin/accounts/{boss}"), &boss_cookie).await;
    assert_failure(myself, 400, "cannot_modify_self").await;
    assert!(hub.handle.store().get_account(&boss).await.unwrap().is_some());
    let unknown = delete(&hub, "/api/admin/accounts/nope", &boss_cookie).await;
    assert_eq!(unknown.status(), 404);
}

#[tokio::test]
async fn only_admins_can_use_the_admin_account_endpoints() {
    let (hub, ..) = multi_with_boss().await;
    let (_, user_cookie) = account(&hub, "user", Some("user@example.com")).await;
    let (target, _) = account(&hub, "target", None).await;
    for (method, path) in [
        (Method::DELETE, format!("/api/admin/accounts/{target}")),
        (Method::POST, format!("/api/admin/accounts/{target}/admin")),
        (Method::DELETE, format!("/api/admin/accounts/{target}/admin")),
    ] {
        let res = hub.send(method.clone(), &path, &user_cookie, Value::Null).await;
        assert_failure(res, 403, "forbidden").await;
        let res = hub.http.request(method.clone(), hub.url(&path)).header("x-anycon", "1").send().await.unwrap();
        assert_eq!(res.status(), 401, "{method} {path} without a session");
    }
    assert!(hub.handle.store().get_account(&target).await.unwrap().is_some());
}

#[tokio::test]
async fn single_mode_admin_cannot_delete_the_bootstrap_admin_but_can_delete_guests() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let (guest, guest_cookie) = hub.invite_account(&admin).await;
    // 给 guest 管理员标记，让它去删 bootstrap 管理员
    let res = hub.send(Method::POST, &format!("/api/admin/accounts/{guest}/admin"), &admin, Value::Null).await;
    assert_eq!(res.status(), 200);
    let res = delete(&hub, &format!("/api/admin/accounts/{}", hub.admin_account_id()), &guest_cookie).await;
    assert_failure(res, 409, "bootstrap_admin").await;
    assert!(hub.handle.store().get_account(&hub.admin_account_id()).await.unwrap().is_some());
    // bootstrap 管理员删 guest：行
    assert_eq!(delete(&hub, &format!("/api/admin/accounts/{guest}"), &admin).await.status(), 204);
    assert_eq!(hub.get("/api/me", &guest_cookie).await.status(), 401);
}

// ---------------------------------------------------------------------------
// GET /api/admin/accounts?q=
// ---------------------------------------------------------------------------

#[tokio::test]
async fn account_search_matches_names_and_emails_case_insensitively_and_ids_exactly() {
    let (hub, boss, boss_cookie) = multi_with_boss().await;
    let (ada, _) = account(&hub, "Ada Lovelace", Some("ada@example.com")).await;
    let (emile, _) = account(&hub, "ÉMILE Zola", None).await;
    let (grace, _) = account(&hub, "G. Hopper", Some("grace.hopper@example.org")).await;
    let ids = |rows: &Value| -> Vec<String> {
        rows.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    };
    let search = |q: &str| {
        let path = format!("/api/admin/accounts?q={}", percent_encode(q));
        let (hub, cookie) = (&hub, &boss_cookie);
        async move {
            let res = hub.get(&path, cookie).await;
            assert_eq!(res.status(), 200, "{path}");
            json_of(res).await
        }
    };

    assert_eq!(ids(&search("lovelace").await), vec![ada.clone()]);
    assert_eq!(ids(&search("ADA LOVE").await), vec![ada.clone()]);
    assert_eq!(ids(&search("  ada@EXAMPLE.com ").await), vec![ada.clone()], "trimmed, email in any case");
    assert_eq!(ids(&search("émile").await), vec![emile.clone()], "non-ASCII names are case-insensitive too");
    assert_eq!(ids(&search("example.org").await), vec![grace.clone()]);
    assert_eq!(ids(&search("hopper").await), vec![grace.clone()]);
    assert_eq!(ids(&search(&grace).await), vec![grace.clone()], "exact id");
    assert!(ids(&search(&grace[..12]).await).is_empty(), "a part of an id is not a match");
    assert!(ids(&search("zzz").await).is_empty());
    // 空白的 q = 没有 q：全部（boss、ada、émile、grace），最早创建的在前
    assert_eq!(ids(&search("   ").await).len(), 4);
    assert_eq!(
        ids(&search("example.com").await),
        [boss.clone(), ada.clone()],
        "boss@ and ada@ share the domain, oldest first"
    );

    // 行的形状：有效管理员标记 + 由配置授予的标记
    let rows = search("boss").await;
    assert_eq!(rows.as_array().unwrap().len(), 1);
    let row = &rows[0];
    assert_eq!(
        (row["id"].as_str(), row["display_name"].as_str(), row["email"].as_str()),
        (Some(boss.as_str()), Some("Boss"), Some(BOSS))
    );
    assert_eq!((&row["is_admin"], &row["admin_by_config"]), (&json!(true), &json!(true)));
    assert!(row["created_at_ms"].as_u64().is_some());
    let row = &search("lovelace").await[0];
    assert_eq!((&row["is_admin"], &row["admin_by_config"]), (&json!(false), &json!(false)));
    assert!(row.get("email").is_some_and(|e| e == "ada@example.com"));
    let row = &search("zola").await[0];
    assert!(row.get("email").is_none(), "no email, no field");

    // 过长的 q
    let long = "a".repeat(201);
    assert_eq!(hub.get(&format!("/api/admin/accounts?q={long}"), &boss_cookie).await.status(), 400);
}

#[tokio::test]
async fn account_search_paginates_with_the_same_cursor_as_the_plain_list() {
    let (hub, _boss, boss_cookie) = multi_with_boss().await;
    let mut want = Vec::new();
    for i in 0..5 {
        want.push(account(&hub, &format!("Team Member {i}"), None).await.0);
        // 创建时间相同的账号也靠 id 分得出先后
    }
    account(&hub, "Somebody Else", None).await;

    let mut got = Vec::new();
    let mut path = "/api/admin/accounts?q=team&limit=2".to_string();
    let mut pages = 0;
    loop {
        let res = hub.get(&path, &boss_cookie).await;
        assert_eq!(res.status(), 200);
        let next = res.headers().get("x-next-cursor").map(|v| v.to_str().unwrap().to_string());
        let rows = json_of(res).await;
        got.extend(rows.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()));
        pages += 1;
        match next {
            Some(c) => path = format!("/api/admin/accounts?q=team&limit=2&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(pages, 3, "2 + 2 + 1");
    let mut sorted = want.clone();
    sorted.sort();
    let mut got_sorted = got.clone();
    got_sorted.sort();
    assert_eq!(got_sorted, sorted, "every match exactly once");
    assert_eq!(got.len(), 5);
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 授予 / 取消管理员
// ---------------------------------------------------------------------------

async fn grant_admin(hub: &TestHub, cookie: &str, id: String) -> reqwest::Response {
    hub.send(Method::POST, &format!("/api/admin/accounts/{id}/admin"), cookie, Value::Null).await
}

async fn revoke_admin(hub: &TestHub, cookie: &str, id: String) -> reqwest::Response {
    hub.send(Method::DELETE, &format!("/api/admin/accounts/{id}/admin"), cookie, Value::Null).await
}

async fn admin_events(hub: &TestHub, event: &str) -> Vec<anycon_hub::store::AuditEntry> {
    audit_rows(hub).await.into_iter().filter(|e| e.event == event).collect()
}

#[tokio::test]
async fn granting_and_revoking_the_admin_flag_changes_access_and_is_audited() {
    let (hub, boss, boss_cookie) = multi_with_boss().await;
    let (carol, carol_cookie) = account(&hub, "carol", Some("carol@example.com")).await;
    assert_eq!(hub.get("/api/admin/status", &carol_cookie).await.status(), 403);

    let grant = hub.send(Method::POST, &format!("/api/admin/accounts/{carol}/admin"), &boss_cookie, Value::Null).await;
    assert_eq!(grant.status(), 200);
    let row = json_of(grant).await;
    assert_eq!(
        (row["id"].as_str(), row["is_admin"].clone(), row["admin_by_config"].clone()),
        (Some(carol.as_str()), json!(true), json!(false))
    );
    assert_eq!(row["email"], "carol@example.com");
    assert_eq!(hub.get("/api/admin/status", &carol_cookie).await.status(), 200, "effective immediately");
    assert!(hub.handle.store().get_account(&carol).await.unwrap().unwrap().is_admin);
    let granted = admin_events(&hub, "admin_granted").await;
    assert_eq!(granted.len(), 1);
    assert_eq!(
        (granted[0].account_id.as_deref(), &granted[0].detail),
        (Some(carol.as_str()), &Some(json!({ "by": boss })))
    );
    // 再授予一次：幂等，不再写审计
    let again = hub.send(Method::POST, &format!("/api/admin/accounts/{carol}/admin"), &boss_cookie, Value::Null).await;
    assert_eq!(again.status(), 200);
    assert_eq!(admin_events(&hub, "admin_granted").await.len(), 1);

    // 取消
    let revoke = delete(&hub, &format!("/api/admin/accounts/{carol}/admin"), &boss_cookie).await;
    assert_eq!(revoke.status(), 200);
    let row = json_of(revoke).await;
    assert_eq!((row["is_admin"].clone(), row["admin_by_config"].clone()), (json!(false), json!(false)));
    assert_eq!(hub.get("/api/admin/status", &carol_cookie).await.status(), 403);
    let revoked = admin_events(&hub, "admin_revoked").await;
    assert_eq!(revoked.len(), 1);
    assert_eq!(
        (revoked[0].account_id.as_deref(), &revoked[0].detail),
        (Some(carol.as_str()), &Some(json!({ "by": boss })))
    );
    let noop = delete(&hub, &format!("/api/admin/accounts/{carol}/admin"), &boss_cookie).await;
    assert_eq!(noop.status(), 200);
    assert_eq!(
        admin_events(&hub, "admin_revoked").await.len(),
        1,
        "revoking a non-admin changes nothing and is not audited"
    );
}

#[tokio::test]
async fn admin_flag_changes_report_their_errors() {
    let (hub, boss, boss_cookie) = multi_with_boss().await;
    let (root, _) = account(&hub, "root", Some("root@example.com")).await; // 也是配置里的管理员
    let grant = |id: String| grant_admin(&hub, &boss_cookie, id);
    let revoke = |id: String| revoke_admin(&hub, &boss_cookie, id);

    // 自己
    for res in [grant(boss.clone()).await, revoke(boss.clone()).await] {
        assert_eq!(res.status(), 400);
        assert_eq!(error_code(res).await, "cannot_modify_self");
    }
    // 不存在
    for res in [grant("nope".into()).await, revoke("nope".into()).await] {
        assert_eq!(res.status(), 404);
        assert_eq!(error_code(res).await, "not_found");
    }
    // 配置里的管理员：取消不了（库里的标记没有，也一样），要改配置
    let res = revoke(root.clone()).await;
    assert_failure(res, 409, "admin_by_config").await;
    // 给它加上库里的标记也行（行里 admin_by_config 仍为真）；取消仍然 409，标记保留
    let res = grant(root.clone()).await;
    assert_eq!(res.status(), 200);
    let row = json_of(res).await;
    assert_eq!((row["is_admin"].clone(), row["admin_by_config"].clone()), (json!(true), json!(true)));
    let res = revoke(root.clone()).await;
    assert_failure(res, 409, "admin_by_config").await;
    assert!(hub.handle.store().get_account(&root).await.unwrap().unwrap().is_admin);
    // 配置派生的管理员没有被写进库里：boss 的库里标记仍然是 false
    assert!(!hub.handle.store().get_account(&boss).await.unwrap().unwrap().is_admin);
}

#[tokio::test]
async fn single_mode_bootstrap_admin_flag_cannot_be_revoked() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let (guest, guest_cookie) = hub.invite_account(&admin).await;
    let res = hub.send(Method::POST, &format!("/api/admin/accounts/{guest}/admin"), &admin, Value::Null).await;
    assert_eq!(res.status(), 200);
    // 被授予的管理员取消 bootstrap 管理员：409
    let boot = hub.admin_account_id();
    let res = delete(&hub, &format!("/api/admin/accounts/{boot}/admin"), &guest_cookie).await;
    assert_failure(res, 409, "bootstrap_admin").await;
    assert!(hub.handle.store().get_account(&boot).await.unwrap().unwrap().is_admin);
    // bootstrap 管理员取消 guest：行；取消自己：400
    let res = delete(&hub, &format!("/api/admin/accounts/{guest}/admin"), &admin).await;
    assert_eq!(res.status(), 200);
    assert_eq!(json_of(res).await["is_admin"], false);
    let res = delete(&hub, &format!("/api/admin/accounts/{boot}/admin"), &admin).await;
    assert_failure(res, 400, "cannot_modify_self").await;
}
