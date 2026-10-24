//! HTTP 表面的集成测试：登录 / 登出、邀请、账号 token、设备 API、CSRF、cookie 属性、限速。

mod common;

use anycon_protocol::DeviceKind;
use common::*;
use reqwest::Method;
use serde_json::{json, Value};

#[tokio::test]
async fn token_login_me_logout() {
    let hub = TestHub::start().await;

    // 未登录
    let res = hub.http.get(hub.url("/api/me")).send().await.unwrap();
    assert_eq!(res.status(), 401);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    // 错 token
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": "not-the-token" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "invalid_token");

    // 对 token：204 + cookie
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": hub.admin_token }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let raw = set_cookie_raw(&res);
    assert!(raw.starts_with("anycon_session="), "{raw}");
    assert!(raw.contains("HttpOnly"));
    assert!(raw.contains("SameSite=Lax"));
    assert!(raw.contains("Path=/"));
    assert!(!raw.contains("Domain"), "no Domain attribute");
    assert!(
        raw.contains("; Secure"),
        "ANYCON_PUBLIC_URL is https => Secure, even for a request that looks like plain http"
    );
    let cookie = cookie_pair(&res);

    // /api/me
    let me: Value = hub.get("/api/me", &cookie).await.json().await.unwrap();
    assert_eq!(me["is_admin"], true);
    assert_eq!(me["login_methods"], json!(["token"]));
    assert!(me["id"].as_str().unwrap().len() >= 16);

    // /api/config 不需要登录
    let cfg: Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["mode"], "single");
    assert_eq!(cfg["public_url"], "https://hub.test");
    assert_eq!(cfg["login_methods"], json!(["token"]));
    assert_eq!(cfg["protocol_version"], 1);

    // 改显示名
    let res = hub.send(Method::PATCH, "/api/me", &cookie, json!({ "display_name": "  Alice  " })).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap()["display_name"], "Alice");
    let res = hub.send(Method::PATCH, "/api/me", &cookie, json!({ "display_name": "   " })).await;
    assert_eq!(res.status(), 400);

    // 会话列表
    let sessions: Value = hub.get("/api/me/sessions", &cookie).await.json().await.unwrap();
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    assert_eq!(sessions[0]["current"], true);

    // 登出
    let res = hub.send(Method::POST, "/auth/logout", &cookie, Value::Null).await;
    assert_eq!(res.status(), 204);
    assert!(set_cookie_raw(&res).contains("Max-Age=0"));
    assert_eq!(hub.get("/api/me", &cookie).await.status(), 401);
}

#[tokio::test]
async fn writes_require_the_anycon_header() {
    let hub = TestHub::start().await;
    let res = hub.http.post(hub.url("/auth/token")).json(&json!({ "token": hub.admin_token })).send().await.unwrap();
    assert_eq!(res.status(), 403);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "csrf");

    let cookie = hub.login_admin().await;
    for method in [Method::PATCH, Method::DELETE, Method::POST] {
        let res = hub
            .http
            .request(method.clone(), hub.url("/api/me/tokens/xyz"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 403, "{method} without header");
    }
    // GET 不需要
    assert_eq!(hub.get("/api/me", &cookie).await.status(), 200);
    // 非 JSON body 的 POST（带了头）
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .header("content-type", "text/plain")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 415);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "unsupported_media_type");
}

#[tokio::test]
async fn invite_flow_creates_an_account() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;

    let res = hub.send(Method::POST, "/api/invites", &admin, json!({ "note": "for bob", "ttl_hours": 24 })).await;
    assert_eq!(res.status(), 200);
    let inv: Value = res.json().await.unwrap();
    let code = inv["code"].as_str().unwrap().to_string();
    assert_eq!(inv["url"], format!("https://hub.test/invite/{code}"));
    assert!(inv["expires_at_ms"].as_u64().unwrap() > 0);

    // 列表里没有明文 code
    let list: Value = hub.get("/api/invites", &admin).await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["note"], "for bob");
    assert_eq!(list[0]["used"], false);
    assert!(list[0].get("code").is_none());

    // 落地：创建账号 + 登录
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": code, "display_name": "Bob" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    let bob = cookie_pair(&res);
    let me: Value = hub.get("/api/me", &bob).await.json().await.unwrap();
    assert_eq!(me["display_name"], "Bob");
    assert_eq!(me["is_admin"], false);
    assert_ne!(me["id"], hub.admin_account_id());
    assert_eq!(hub.handle.store().stats().await.unwrap().accounts, 2);

    // 用过了
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": code }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 410);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "invite_used");
    // 不存在
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    // 非管理员不能用邀请 / 管理接口
    for (m, p) in [(Method::POST, "/api/invites"), (Method::GET, "/api/invites"), (Method::GET, "/api/admin/status")] {
        let res = hub.send(m, p, &bob, json!({})).await;
        assert_eq!(res.status(), 403, "{p}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "forbidden");
    }

    // 撤销一个邀请
    let inv2: Value = hub.send(Method::POST, "/api/invites", &admin, json!({})).await.json().await.unwrap();
    let id = inv2["id"].as_str().unwrap();
    assert_eq!(hub.send(Method::DELETE, &format!("/api/invites/{id}"), &admin, Value::Null).await.status(), 204);
    assert_eq!(hub.send(Method::DELETE, &format!("/api/invites/{id}"), &admin, Value::Null).await.status(), 404);

    // 受邀者可以为自己生成登录 token，并用它在另一个浏览器登录
    let res = hub.send(Method::POST, "/api/me/tokens", &bob, json!({ "label": "laptop" })).await;
    assert_eq!(res.status(), 200);
    let created: Value = res.json().await.unwrap();
    let token = created["token"].as_str().unwrap().to_string();
    let other = hub.login(&token).await;
    assert_eq!(hub.get("/api/me", &other).await.json::<Value>().await.unwrap()["display_name"], "Bob");
    let tokens: Value = hub.get("/api/me/tokens", &bob).await.json().await.unwrap();
    assert_eq!(tokens.as_array().unwrap().len(), 1);
    assert!(tokens[0].get("token").is_none());
    assert!(tokens[0]["last_used_ms"].as_u64().is_some());
    let tid = created["id"].as_str().unwrap();
    assert_eq!(hub.send(Method::DELETE, &format!("/api/me/tokens/{tid}"), &bob, Value::Null).await.status(), 204);
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": token }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // 踢掉另一个浏览器会话
    let sessions: Value = hub.get("/api/me/sessions", &bob).await.json().await.unwrap();
    let other_id = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["current"] == false)
        .map(|s| s["id"].as_str().unwrap().to_string());
    if let Some(id) = other_id {
        assert_eq!(hub.send(Method::DELETE, &format!("/api/me/sessions/{id}"), &bob, Value::Null).await.status(), 204);
        assert_eq!(hub.get("/api/me", &other).await.status(), 401);
    }
}

#[tokio::test]
async fn token_login_is_rate_limited_per_ip() {
    let hub = TestHub::start().await;
    let attempt = || async {
        hub.http
            .post(hub.url("/auth/token"))
            .header("x-anycon", "1")
            // 不可信来源带的 XFF 必须被无视，否则攻击者可以随意换桶
            .header("x-forwarded-for", format!("9.9.9.{}", rand_octet()))
            .json(&json!({ "token": "wrong" }))
            .send()
            .await
            .unwrap()
    };
    for i in 0..10 {
        assert_eq!(attempt().await.status(), 401, "attempt {}", i + 1);
    }
    let res = attempt().await;
    assert_eq!(res.status(), 429, "11th attempt within a minute");
    let retry: u64 = res.headers().get("retry-after").expect("Retry-After").to_str().unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], "rate_limited");
    // 限速期间正确的 token 也进不去
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": hub.admin_token }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 429);
}

fn rand_octet() -> u8 {
    use std::sync::atomic::{AtomicU8, Ordering};
    static N: AtomicU8 = AtomicU8::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

#[tokio::test]
async fn trusted_proxy_headers_drive_ip_and_secure_cookie() {
    let hub = TestHub::start_with(|c| c.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()]).await;
    let try_login = |xff: &'static str| {
        let hub = &hub;
        async move {
            hub.http
                .post(hub.url("/auth/token"))
                .header("x-anycon", "1")
                .header("x-forwarded-for", xff)
                .json(&json!({ "token": "wrong" }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    for _ in 0..10 {
        assert_eq!(try_login("1.1.1.1").await, 401);
    }
    assert_eq!(try_login("1.1.1.1").await, 429);
    // 另一个真实 IP 有自己的桶
    assert_eq!(try_login("2.2.2.2").await, 401);

    // 可信代理转发的 https => Secure cookie
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .header("x-forwarded-for", "3.3.3.3")
        .header("x-forwarded-proto", "https")
        .json(&json!({ "token": hub.admin_token }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 204);
    assert!(set_cookie_raw(&res).contains("; Secure"));
    // 会话里只存 IP 前缀
    let cookie = cookie_pair(&res);
    let sessions: Value = hub.get("/api/me/sessions", &cookie).await.json().await.unwrap();
    assert_eq!(sessions[0]["ip_prefix"], "3.3.3.0/24");
}

/// `GET /api/devices` 也认设备 token（`Authorization: Bearer`，只读）：CLI 已经是设备时用它列节点，不用再登录。
#[tokio::test]
async fn devices_list_accepts_a_device_token_read_only() {
    let hub = TestHub::start().await;
    let cookie = hub.login_admin().await;
    let daemon = hub.seed_device(DeviceKind::Daemon, "mac-mini").await;
    let cli = hub.seed_device(DeviceKind::Cli, "laptop-cli").await;
    hub.seed_paired(&daemon, &cli).await;
    let _live = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "mac-mini").await;

    let bearer =
        |token: &str| hub.http.get(hub.url("/api/devices")).header("authorization", format!("Bearer {token}")).send();

    // hub 在 AuthOk 之后才在后台写 last_seen_ms；等它落库，两次列表之间才不会因为这个字段出现与否而不同。
    let mut by_cookie: Value = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    for _ in 0..200 {
        if by_cookie.as_array().unwrap().iter().any(|d| d["id"] == daemon.id && d.get("last_seen_ms").is_some()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        by_cookie = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    }
    // 和 cookie 登录时一样的列表（含 online / paired_with）。
    let res = bearer(&cli.token).await.unwrap();
    assert_eq!(res.status(), 200);
    let by_token: Value = res.json().await.unwrap();
    assert_eq!(by_token, by_cookie);
    let d = by_token.as_array().unwrap().iter().find(|d| d["id"] == daemon.id).unwrap();
    assert_eq!((d["kind"].as_str(), d["online"].as_bool()), (Some("daemon"), Some(true)));
    assert_eq!(d["paired_with"], json!([cli.id]));
    // 方案名不分大小写。
    let res = hub
        .http
        .get(hub.url("/api/devices"))
        .header("authorization", format!("bearer {}", cli.token))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    // 别的账号的设备 token 只看得到自己账号的设备。
    let inv: Value = hub.send(Method::POST, "/api/invites", &cookie, json!({})).await.json().await.unwrap();
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": inv["code"] }))
        .send()
        .await
        .unwrap();
    let other_cookie = cookie_pair(&res);
    let other_account =
        hub.get("/api/me", &other_cookie).await.json::<Value>().await.unwrap()["id"].as_str().unwrap().to_string();
    let other_token = anycon_hub::util::new_secret();
    hub.handle
        .store()
        .create_device(
            anycon_hub::store::NewDevice {
                id: anycon_hub::util::new_id(),
                account_id: other_account,
                kind: DeviceKind::Cli,
                name: "their-cli".into(),
                platform: "test".into(),
                origin: None,
                token_hash: anycon_hub::util::hash_token(&other_token),
            },
            anycon_hub::util::now_ms(),
        )
        .await
        .unwrap();
    let theirs: Value = bearer(&other_token).await.unwrap().json().await.unwrap();
    assert_eq!(theirs.as_array().unwrap().len(), 1);
    assert_eq!(theirs[0]["name"], "their-cli");

    // 只读：写操作和别的接口不认设备 token。
    let patch = hub
        .http
        .patch(hub.url(&format!("/api/devices/{}", daemon.id)))
        .header("x-anycon", "1")
        .header("authorization", format!("Bearer {}", cli.token))
        .json(&json!({ "name": "renamed" }))
        .send()
        .await
        .unwrap();
    assert_eq!(patch.status(), 401);
    let del = hub
        .http
        .delete(hub.url(&format!("/api/devices/{}", daemon.id)))
        .header("x-anycon", "1")
        .header("authorization", format!("Bearer {}", cli.token))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 401);
    let me = hub.http.get(hub.url("/api/me")).header("authorization", format!("Bearer {}", cli.token)).send().await;
    assert_eq!(me.unwrap().status(), 401);
    let unchanged: Value = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    assert_eq!(unchanged, by_cookie);

    // 无效的 token / 不是 Bearer / 空 token 都是 401；带了 Authorization 就不回退到 cookie。
    for bad in ["Bearer not-a-token", "Bearer ", "Basic abc", &format!("Token {}", cli.token), &cli.token] {
        let res = hub
            .http
            .get(hub.url("/api/devices"))
            .header("authorization", bad)
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 401, "{bad:?}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "unauthorized");
    }
    assert_eq!(hub.http.get(hub.url("/api/devices")).send().await.unwrap().status(), 401);

    // 被吊销的设备 token → 401（设备记录还在库里，只是 revoked）。
    let res = hub.send(Method::DELETE, &format!("/api/devices/{}", cli.id), &cookie, Value::Null).await;
    assert_eq!(res.status(), 204);
    let res = bearer(&cli.token).await.unwrap();
    assert_eq!(res.status(), 401);
    assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn devices_api_lists_renames_and_revokes() {
    let hub = TestHub::start().await;
    let cookie = hub.login_admin().await;
    let daemon = hub.seed_device(DeviceKind::Daemon, "mac-mini").await;
    let web = hub.seed_device(DeviceKind::Web, "Chrome").await;
    hub.seed_paired(&daemon, &web).await;

    let mut live = FakeDevice::connect(&hub, Some(&daemon.token), DeviceKind::Daemon, "mac-mini").await;
    let list: Value = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    let d = list.iter().find(|d| d["id"] == daemon.id).unwrap();
    assert_eq!(d["online"], true);
    assert_eq!(d["kind"], "daemon");
    assert_eq!(d["paired_with"], json!([web.id]));
    let w = list.iter().find(|d| d["id"] == web.id).unwrap();
    assert_eq!(w["online"], false);

    // 改名
    let res = hub.send(Method::PATCH, &format!("/api/devices/{}", web.id), &cookie, json!({ "name": "Safari" })).await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap()["name"], "Safari");

    // 别的账号看不到 / 动不了
    let inv: Value = hub.send(Method::POST, "/api/invites", &cookie, json!({})).await.json().await.unwrap();
    let res = hub
        .http
        .post(hub.url("/auth/invite"))
        .header("x-anycon", "1")
        .json(&json!({ "code": inv["code"] }))
        .send()
        .await
        .unwrap();
    let other = cookie_pair(&res);
    assert_eq!(hub.get("/api/devices", &other).await.json::<Value>().await.unwrap(), json!([]));
    assert_eq!(
        hub.send(Method::DELETE, &format!("/api/devices/{}", daemon.id), &other, Value::Null).await.status(),
        404
    );

    // 吊销在线的 daemon：连接以 4004 断开，列表里消失，token 不再可用
    let res = hub.send(Method::DELETE, &format!("/api/devices/{}", daemon.id), &cookie, Value::Null).await;
    assert_eq!(res.status(), 204);
    assert_eq!(live.expect_close().await, Some(4004));
    let list: Value = hub.get("/api/devices", &cookie).await.json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["paired_with"], json!([]));
    assert_eq!(
        hub.send(Method::DELETE, &format!("/api/devices/{}", daemon.id), &cookie, Value::Null).await.status(),
        404
    );

    let mut raw = FakeDevice::open_with(&hub.ws_url(), &[]).await.unwrap();
    raw.send_auth(Some(&daemon.token), DeviceKind::Daemon, "mac-mini").await;
    assert!(matches!(
        raw.recv_control().await,
        anycon_protocol::Control::AuthFail { reason: anycon_protocol::AuthFailReason::Revoked }
    ));
    assert_eq!(raw.expect_close().await, Some(4004));

    // hub 审计里有记录，且只有元数据
    let audit: Value = hub.get("/api/admin/audit", &cookie).await.json().await.unwrap();
    let events: Vec<&str> = audit.as_array().unwrap().iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert!(events.contains(&"login_ok"));
    assert!(events.contains(&"device_revoked"));
}

#[tokio::test]
async fn static_files_spa_fallback_and_unknown_api_paths() {
    let hub = TestHub::start().await;
    let res = hub.http.get(hub.url("/")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    // SPA 路由回退
    let res = hub.http.get(hub.url("/n/abc/s/def")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    // 缺失的静态资源是 404，不是 index.html
    assert_eq!(hub.http.get(hub.url("/assets/missing.js")).send().await.unwrap().status(), 404);
    // API / auth / v1 下的未知路径是 JSON 404
    for p in ["/api/nope", "/auth/nope", "/v1/nope"] {
        let res = hub.http.get(hub.url(p)).send().await.unwrap();
        assert_eq!(res.status(), 404, "{p}");
    }
    let body: Value = hub.http.get(hub.url("/api/nope")).send().await.unwrap().json().await.unwrap();
    assert_eq!(body["error"]["code"], "not_found");
    // 健康检查
    let h: Value = hub.http.get(hub.url("/healthz")).send().await.unwrap().json().await.unwrap();
    assert_eq!(h["status"], "ok");
    assert_eq!(hub.http.get(hub.url("/readyz")).send().await.unwrap().status(), 200);
}

// ---------------------------------------------------------------------------
// cookie 的 Secure、IPv6 /64 限速
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cookie_secure_follows_the_public_url_scheme() {
    // https 的 PUBLIC_URL：始终 Secure——反代不在信任列表里、X-Forwarded-Proto 不被采信时也一样；登出清 cookie 同理
    let hub = TestHub::start().await;
    let res = hub
        .http
        .post(hub.url("/auth/token"))
        .header("x-anycon", "1")
        .json(&json!({ "token": hub.admin_token }))
        .send()
        .await
        .unwrap();
    assert!(set_cookie_raw(&res).contains("; Secure"));
    let cookie = cookie_pair(&res);
    let out = hub.send(Method::POST, "/auth/logout", &cookie, Value::Null).await;
    assert!(set_cookie_raw(&out).contains("; Secure") && set_cookie_raw(&out).contains("Max-Age=0"));

    // http 的 PUBLIC_URL（本机开发）：不带 Secure；不可信来源声称 https 也不行
    let hub =
        TestHub::start_with(|c| c.public_url = anycon_hub::config::parse_public_url("http://127.0.0.1:8080").unwrap())
            .await;
    for proto in [None, Some("https")] {
        let mut req = hub.http.post(hub.url("/auth/token")).header("x-anycon", "1");
        if let Some(p) = proto {
            req = req.header("x-forwarded-proto", p);
        }
        let res = req.json(&json!({ "token": hub.admin_token })).send().await.unwrap();
        assert_eq!(res.status(), 204);
        assert!(!set_cookie_raw(&res).contains("Secure"), "{proto:?}");
    }
}

#[tokio::test]
async fn ipv6_clients_are_rate_limited_per_slash_64() {
    let hub = TestHub::start_with(|c| c.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()]).await;
    let try_login = |xff: String| {
        let hub = &hub;
        async move {
            hub.http
                .post(hub.url("/auth/token"))
                .header("x-anycon", "1")
                .header("x-forwarded-for", xff)
                .json(&json!({ "token": "wrong" }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    // 同一个 /64 里轮换地址（每次都是新地址）：共用一个桶
    for i in 0..10u16 {
        assert_eq!(try_login(format!("2001:db8:1:2::{:x}", i + 1)).await, 401, "attempt {}", i + 1);
    }
    assert_eq!(try_login("2001:db8:1:2:dead:beef:cafe:f00d".into()).await, 429, "rotating inside a /64 must not help");
    // 另一个 /64 有自己的桶
    assert_eq!(try_login("2001:db8:1:3::1".into()).await, 401);
}

// ---------------------------------------------------------------------------
// HTML 响应带 CSP / COOP
// ---------------------------------------------------------------------------

const EXPECTED_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; \
img-src 'self' data: blob:; style-src 'self' 'unsafe-inline'; font-src 'self' data:; worker-src 'self'; \
manifest-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";

#[tokio::test]
async fn html_responses_carry_csp_and_coop() {
    let hub = TestHub::start().await;
    for path in ["/", "/n/abc/s/def", "/pair"] {
        let res = hub.http.get(hub.url(path)).send().await.unwrap();
        assert_eq!(res.status(), 200, "{path}");
        let h = res.headers();
        assert_eq!(h["content-security-policy"], EXPECTED_CSP, "{path}");
        assert_eq!(h["cross-origin-opener-policy"], "same-origin", "{path}");
        assert_eq!(h["x-content-type-options"], "nosniff");
        assert_eq!(h["x-frame-options"], "DENY");
        assert_eq!(h["referrer-policy"], "no-referrer");
    }
    // 没有 unsafe-inline / unsafe-eval 的脚本权限，也没有 http(s): 的图片 / 连接来源
    let csp = EXPECTED_CSP;
    let script = csp.split(';').find(|d| d.trim().starts_with("script-src")).unwrap();
    assert!(!script.contains("unsafe-inline") && !script.contains("'unsafe-eval'") && !script.contains("http"));
    assert!(csp.split(';').filter(|d| d.contains("img-src") || d.contains("connect-src")).all(|d| !d.contains("http")));

    // 条件请求的 304 也带上（浏览器会用它更新缓存里的头）
    let first = hub.http.get(hub.url("/")).send().await.unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let res = hub.http.get(hub.url("/")).header("if-none-match", etag).send().await.unwrap();
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["content-security-policy"], EXPECTED_CSP);

    // JSON 接口不需要（也不带）CSP，但基础安全头照旧
    let res = hub.http.get(hub.url("/api/config")).send().await.unwrap();
    assert!(res.headers().get("content-security-policy").is_none());
    assert_eq!(res.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn html_csp_gets_a_frame_src_for_the_preview_domain_only_when_preview_is_configured() {
    // 没配预览：字符串一个字符都不变（上面的 EXPECTED_CSP），也没有 frame-src
    let plain = TestHub::start().await;
    let res = plain.http.get(plain.url("/")).send().await.unwrap();
    assert_eq!(res.headers()["content-security-policy"], EXPECTED_CSP);
    assert!(!EXPECTED_CSP.contains("frame-src"));

    // 配了预览：frame-ancestors 前面多一段 `frame-src <scheme>://*.<预览域名>[:端口];`，其余不变
    let hub = TestHub::start_preview().await;
    let with_preview =
        EXPECTED_CSP.replace("frame-ancestors 'none'", "frame-src http://*.preview.localhost; frame-ancestors 'none'");
    for path in ["/", "/n/abc/s/def", "/pair"] {
        let res = hub.http.get(hub.url(path)).send().await.unwrap();
        let csp = res.headers()["content-security-policy"].to_str().unwrap();
        assert_eq!(csp, with_preview, "{path}");
        // Web UI 自己仍然不能被别人嵌入
        assert!(csp.contains("frame-ancestors 'none'"));
        assert_eq!(res.headers()["x-frame-options"], "DENY");
    }
    // 条件请求的 304 也带
    let first = hub.http.get(hub.url("/")).send().await.unwrap();
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let res = hub.http.get(hub.url("/")).header("if-none-match", etag).send().await.unwrap();
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["content-security-policy"], with_preview);

    // 预览域名带端口时，端口也在 frame-src 里（浏览器的来源表达式要带端口）
    let hub = TestHub::start_with(|c| {
        c.preview =
            Some(anycon_hub::config::parse_preview_url("http://*.preview.localhost:8080", &c.public_url).unwrap());
    })
    .await;
    let res = hub.http.get(hub.url("/")).send().await.unwrap();
    let csp = res.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("frame-src http://*.preview.localhost:8080; frame-ancestors 'none';"), "{csp}");
}

#[tokio::test]
async fn config_exposes_the_preview_origin_template_or_null() {
    let plain = TestHub::start().await;
    let cfg: serde_json::Value = plain.http.get(plain.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert!(cfg["preview_origin_template"].is_null());
    assert!(cfg.as_object().unwrap().contains_key("preview_origin_template"), "the key is always there: {cfg}");

    let hub = TestHub::start_preview().await;
    let cfg: serde_json::Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["preview_origin_template"], "http://{label}.preview.localhost");
    // 不需要登录
    assert_eq!(cfg["mode"], "single");

    let hub = TestHub::start_with(|c| {
        c.preview = Some(anycon_hub::config::parse_preview_url("https://*.aiplace.app", &c.public_url).unwrap());
    })
    .await;
    let cfg: serde_json::Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["preview_origin_template"], "https://{label}.aiplace.app");
}

// ---------------------------------------------------------------------------
// 列表分页：`?limit=&cursor=`，下一页游标在 X-Next-Cursor 头里，响应体仍是数组
// ---------------------------------------------------------------------------

fn next_cursor(res: &reqwest::Response) -> Option<String> {
    res.headers().get("x-next-cursor").map(|v| v.to_str().unwrap().to_string())
}

/// 一页一页翻完，返回（每页条目数，所有条目）。
async fn walk(hub: &TestHub, cookie: &str, path: &str, limit: u32) -> (Vec<usize>, Vec<Value>) {
    let mut sizes = Vec::new();
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..50 {
        let mut url = format!("{path}?limit={limit}");
        if let Some(c) = &cursor {
            url.push_str(&format!("&cursor={c}"));
        }
        let res = hub.get(&url, cookie).await;
        assert_eq!(res.status(), 200, "{url}");
        cursor = next_cursor(&res);
        let items: Vec<Value> = res.json().await.unwrap();
        sizes.push(items.len());
        all.extend(items);
        if cursor.is_none() {
            return (sizes, all);
        }
    }
    panic!("pagination never ended for {path}");
}

#[tokio::test]
async fn invites_are_paginated() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        let inv: Value = hub
            .send(Method::POST, "/api/invites", &admin, json!({ "note": format!("n{i}") }))
            .await
            .json()
            .await
            .unwrap();
        ids.push(inv["id"].as_str().unwrap().to_string());
    }
    let (sizes, all) = walk(&hub, &admin, "/api/invites", 2).await;
    assert_eq!(sizes, vec![2, 2, 1]);
    let mut got: Vec<String> = all.iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    got.sort();
    ids.sort();
    assert_eq!(got, ids, "every invite exactly once");
    let times: Vec<u64> = all.iter().map(|i| i["created_at_ms"].as_u64().unwrap()).collect();
    assert!(times.windows(2).all(|w| w[0] >= w[1]), "newest first: {times:?}");

    // 刚好一页：没有游标；默认（不带参数）返回全部（≤ 100），也没有游标
    let res = hub.get("/api/invites?limit=5", &admin).await;
    assert!(next_cursor(&res).is_none());
    let res = hub.get("/api/invites", &admin).await;
    assert!(next_cursor(&res).is_none());
    assert_eq!(res.json::<Value>().await.unwrap().as_array().unwrap().len(), 5);
    // limit 夹在 1..=500；不是数字 → 400；游标坏了 → 400
    assert_eq!(
        hub.get("/api/invites?limit=0", &admin).await.json::<Value>().await.unwrap().as_array().unwrap().len(),
        1
    );
    assert_eq!(hub.get("/api/invites?limit=abc", &admin).await.status(), 400);
    for bad in ["zzz", "1.", ".x", "1.a%20b"] {
        let res = hub.get(&format!("/api/invites?cursor={bad}"), &admin).await;
        assert_eq!(res.status(), 400, "{bad}");
        assert_eq!(res.json::<Value>().await.unwrap()["error"]["code"], "bad_request");
    }
}

#[tokio::test]
async fn audit_log_is_paginated_newest_first() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    for _ in 0..4 {
        hub.login_admin().await; // 每次登录一条 login_ok
    }
    let total = hub.handle.store().list_audit(1000, None).await.unwrap().len();
    assert!(total >= 5);
    let (sizes, all) = walk(&hub, &admin, "/api/admin/audit", 2).await;
    assert_eq!(sizes.iter().sum::<usize>(), total);
    assert!(sizes.iter().all(|n| (1..=2).contains(n)));
    let ids: Vec<i64> = all.iter().map(|e| e["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "strictly newest first, no repeats: {ids:?}");
    // 旧的数字游标写法仍然有效（游标就是上一页最后一条的 id）
    let res = hub.get(&format!("/api/admin/audit?limit=1&cursor={}", ids[0]), &admin).await;
    assert_eq!(res.json::<Value>().await.unwrap()[0]["id"].as_i64().unwrap(), ids[1]);
    assert_eq!(hub.get("/api/admin/audit?cursor=abc", &admin).await.status(), 400);
}

#[tokio::test]
async fn admin_accounts_list_is_paginated_and_admin_only() {
    let hub = TestHub::start().await;
    let admin = hub.login_admin().await;
    let mut bob = String::new();
    for name in ["bob", "carol"] {
        let inv: Value = hub.send(Method::POST, "/api/invites", &admin, json!({})).await.json().await.unwrap();
        let res = hub
            .http
            .post(hub.url("/auth/invite"))
            .header("x-anycon", "1")
            .json(&json!({ "code": inv["code"], "display_name": name }))
            .send()
            .await
            .unwrap();
        bob = cookie_pair(&res);
    }
    let (sizes, all) = walk(&hub, &admin, "/api/admin/accounts", 2).await;
    assert_eq!(sizes, vec![2, 1]);
    let names: Vec<&str> = all.iter().map(|a| a["display_name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 3);
    assert!(names.contains(&"bob") && names.contains(&"carol"));
    assert_eq!(all.iter().filter(|a| a["is_admin"] == true).count(), 1);
    assert!(all.iter().all(|a| a.get("token_hash").is_none()));
    // 非管理员不行
    assert_eq!(hub.get("/api/admin/accounts", &bob).await.status(), 403);
    assert_eq!(hub.http.get(hub.url("/api/admin/accounts")).send().await.unwrap().status(), 401);
}
