//! 移除账号的一种登录方式（解绑邮箱、解除 OAuth 绑定、删登录 token）：账号至少要留一种，
//! 而且三种操作之间不能互相指望对方还在——它们共用一把按账号的锁，并且在锁里重新读账号再判断。
//!
//! 用 `single` 的 hub（有 token，也可以配 SMTP 和 GitHub），身份直接写库里，不走 OAuth 往返。

mod common;

use std::time::Duration;

use anycon_hub::config::OAuthClient;
use anycon_hub::util::now_ms;
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

async fn single_hub() -> TestHub {
    let (hub, _mailer) = TestHub::start_mail_with(|c| {
        c.oauth_github = Some(OAuthClient { client_id: "id".into(), client_secret: "secret".into() });
    })
    .await;
    hub
}

/// 受邀账号：没有邮箱、没有 token、没有身份，一种登录方式都还没有。
async fn guest(hub: &TestHub) -> (String, String) {
    let admin = hub.login_admin().await;
    hub.invite_account(&admin).await
}

async fn bind_email(hub: &TestHub, id: &str, email: &str) {
    hub.handle.store().set_account_email(id, Some(email)).await.unwrap();
}

async fn link_github(hub: &TestHub, id: &str, subject: &str) {
    hub.handle.store().link_identity(id, "github", subject, now_ms()).await.unwrap();
}

async fn add_token(hub: &TestHub, cookie: &str, label: &str) -> String {
    let res = hub.send(Method::POST, "/api/me/tokens", cookie, json!({ "label": label })).await;
    assert_eq!(res.status(), 200);
    res.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string()
}

async fn delete_token(hub: &TestHub, cookie: &str, token_id: &str) -> reqwest::Response {
    hub.send(Method::DELETE, &format!("/api/me/tokens/{token_id}"), cookie, Value::Null).await
}

async fn unbind_email(hub: &TestHub, cookie: &str) -> reqwest::Response {
    hub.send(Method::DELETE, "/api/me/email", cookie, Value::Null).await
}

async fn unlink_github(hub: &TestHub, cookie: &str) -> reqwest::Response {
    hub.send(Method::DELETE, "/api/me/identities/github", cookie, Value::Null).await
}

async fn methods(hub: &TestHub, cookie: &str) -> Value {
    hub.get("/api/me", cookie).await.json::<Value>().await.unwrap()["login_methods"].clone()
}

async fn token_count(hub: &TestHub, id: &str) -> usize {
    hub.handle.store().list_account_tokens(id).await.unwrap().len()
}

async fn assert_last_method(res: reqwest::Response) {
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap();
    assert_eq!((status, &body["error"]["code"]), (409, &json!("last_login_method")), "{body}");
}

// ---------------------------------------------------------------------------
// 删登录 token（single）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_only_login_token_of_an_account_with_nothing_else_cannot_be_deleted() {
    let hub = single_hub().await;
    let (id, cookie) = guest(&hub).await;
    let first = add_token(&hub, &cookie, "laptop").await;
    assert_eq!(methods(&hub, &cookie).await, json!(["token"]));

    assert_last_method(delete_token(&hub, &cookie, &first).await).await;
    assert_eq!(token_count(&hub, &id).await, 1, "untouched");
    // 不存在的 token 仍然是 404，不是 409
    assert_eq!(delete_token(&hub, &cookie, "no-such-token").await.status(), 404);

    // 有两个的时候可以删一个，剩下的那个又是最后一种方式
    let second = add_token(&hub, &cookie, "phone").await;
    assert_eq!(delete_token(&hub, &cookie, &first).await.status(), 204);
    assert_last_method(delete_token(&hub, &cookie, &second).await).await;
    assert_eq!(token_count(&hub, &id).await, 1);
    let audit = hub.handle.store().list_audit(100, None).await.unwrap();
    assert_eq!(audit.iter().filter(|e| e.event == "token_deleted").count(), 1, "only the allowed delete is audited");
}

#[tokio::test]
async fn the_last_login_token_can_go_when_an_email_or_an_identity_remains() {
    let hub = single_hub().await;
    // 绑了邮箱：邮箱是另一种登录方式
    let (id, cookie) = guest(&hub).await;
    let token = add_token(&hub, &cookie, "laptop").await;
    bind_email(&hub, &id, "guest@example.com").await;
    assert_eq!(methods(&hub, &cookie).await, json!(["email", "token"]));
    assert_eq!(delete_token(&hub, &cookie, &token).await.status(), 204);
    assert_eq!(methods(&hub, &cookie).await, json!(["email"]));
    // 之后邮箱成了唯一的方式
    assert_last_method(unbind_email(&hub, &cookie).await).await;

    // 绑了（配置里有的）OAuth 身份也一样
    let (id, cookie) = guest(&hub).await;
    let token = add_token(&hub, &cookie, "laptop").await;
    link_github(&hub, &id, "gh-1").await;
    assert_eq!(delete_token(&hub, &cookie, &token).await.status(), 204);
    assert_eq!(methods(&hub, &cookie).await, json!(["github"]));
    assert_last_method(unlink_github(&hub, &cookie).await).await;
}

#[tokio::test]
async fn the_bootstrap_admin_can_delete_its_login_tokens_because_the_bootstrap_token_stays() {
    let hub = single_hub().await;
    let admin = hub.login_admin().await;
    let token = add_token(&hub, &admin, "spare").await;
    assert_eq!(methods(&hub, &admin).await, json!(["token"]));
    assert_eq!(delete_token(&hub, &admin, &token).await.status(), 204);
    assert_eq!(token_count(&hub, &hub.admin_account_id()).await, 0);
    assert_eq!(methods(&hub, &admin).await, json!(["token"]), "the bootstrap token is not a row, it is always there");
}

// ---------------------------------------------------------------------------
// 三种移除共用一把按账号的锁，并在锁里重读账号
// ---------------------------------------------------------------------------

/// 持有账号的锁 `hold_ms` 毫秒，期间执行 `meanwhile`（模拟另一个已经赢了锁的移除）；同时发 `request`，
/// 它在请求开始时看到的账号是旧的。返回 `request` 的响应。
async fn request_while_another_removal_holds_the_lock<F, Fut>(
    hub: &TestHub,
    account_id: &str,
    request: Fut,
    meanwhile: impl FnOnce() -> F,
) -> reqwest::Response
where
    Fut: std::future::Future<Output = reqwest::Response>,
    F: std::future::Future<Output = ()>,
{
    let state = hub.handle.state.clone();
    let guard = state.login_method_locks.lock(account_id).await;
    let other = async {
        // 给请求足够的时间到达处理函数（拿到旧的账号快照）并在锁上等着
        tokio::time::sleep(Duration::from_millis(300)).await;
        meanwhile().await;
        drop(guard);
    };
    let (res, ()) = tokio::join!(request, other);
    res
}

#[tokio::test]
async fn unlinking_an_identity_judges_the_account_as_it_is_inside_the_lock() {
    let hub = single_hub().await;
    let (id, cookie) = guest(&hub).await;
    bind_email(&hub, &id, "guest@example.com").await;
    link_github(&hub, &id, "gh-1").await;
    assert_eq!(methods(&hub, &cookie).await, json!(["github", "email"]));

    // 请求开始时账号还有邮箱（快照里有第二种方式）；等它拿到锁的时候，邮箱已经被另一个请求解绑了
    let res = request_while_another_removal_holds_the_lock(&hub, &id, unlink_github(&hub, &cookie), || async {
        hub.handle.store().set_account_email(&id, None).await.unwrap();
    })
    .await;
    assert_last_method(res).await;
    assert_eq!(hub.handle.store().list_identities(&id).await.unwrap().len(), 1, "the identity is still linked");
    assert_eq!(methods(&hub, &cookie).await, json!(["github"]));
}

#[tokio::test]
async fn unbinding_the_email_judges_the_account_as_it_is_inside_the_lock() {
    let hub = single_hub().await;
    let (id, cookie) = guest(&hub).await;
    bind_email(&hub, &id, "guest@example.com").await;
    link_github(&hub, &id, "gh-1").await;

    // 请求之间，另一个移除（解绑 GitHub）赢了锁；邮箱现在是唯一的方式
    let res = request_while_another_removal_holds_the_lock(&hub, &id, unbind_email(&hub, &cookie), || async {
        hub.handle.store().unlink_identity(&id, "github").await.unwrap();
    })
    .await;
    assert_last_method(res).await;
    let me: Value = hub.get("/api/me", &cookie).await.json().await.unwrap();
    assert_eq!(me["email"], "guest@example.com", "still bound");
}

#[tokio::test]
async fn deleting_a_token_judges_the_account_as_it_is_inside_the_lock() {
    let hub = single_hub().await;
    let (id, cookie) = guest(&hub).await;
    bind_email(&hub, &id, "guest@example.com").await;
    let token = add_token(&hub, &cookie, "laptop").await;

    // 请求开始时账号有邮箱；拿到锁的时候邮箱已经被另一个请求解绑了，这个 token 成了唯一的方式
    let res = request_while_another_removal_holds_the_lock(&hub, &id, delete_token(&hub, &cookie, &token), || async {
        hub.handle.store().set_account_email(&id, None).await.unwrap();
    })
    .await;
    assert_last_method(res).await;
    assert_eq!(token_count(&hub, &id).await, 1);
}

#[tokio::test]
async fn simultaneous_removals_never_leave_an_account_without_a_way_to_sign_in() {
    let hub = single_hub().await;
    for round in 0..6 {
        // 邮箱 + GitHub + 一个 token：同时发三个移除，只能成功两个
        let (id, cookie) = guest(&hub).await;
        bind_email(&hub, &id, &format!("guest{round}@example.com")).await;
        link_github(&hub, &id, &format!("gh-{round}")).await;
        let token = add_token(&hub, &cookie, "laptop").await;

        let (a, b, c) = tokio::join!(
            unbind_email(&hub, &cookie),
            unlink_github(&hub, &cookie),
            delete_token(&hub, &cookie, &token)
        );
        let statuses = [a.status().as_u16(), b.status().as_u16(), c.status().as_u16()];
        assert_eq!(statuses.iter().filter(|s| **s == 204).count(), 2, "round {round}: {statuses:?}");
        assert_eq!(statuses.iter().filter(|s| **s == 409).count(), 1, "round {round}: {statuses:?}");
        assert_eq!(methods(&hub, &cookie).await.as_array().unwrap().len(), 1, "round {round}");
    }
}
