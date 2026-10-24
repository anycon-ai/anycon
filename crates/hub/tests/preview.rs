//! dev server 预览（ADR-39，docs/spec/hub.md § 13）：预览域名上的 HTTP 表面、`POST /api/preview/tokens`、预览连接。
//!
//! 测试 hub 的预览域名是 `http://*.preview.localhost`（没有端口）：请求直连 hub 的随机端口，自己带 `Host`
//! （`<label>.preview.localhost`），这样不需要 DNS，也不需要知道端口。
//!
//! 预览运行时的前端文件可能是真实的构建产物（`web/dist-preview`），也可能是占位目录：这里只断言状态码和响应头，
//! 不断言文件内容。

mod common;

use std::time::{Duration, Instant};

use anycon_hub::Config;
use anycon_protocol::{AuthFailReason, Control, DeviceKind, DropReason, Envelope, Kind, PROTOCOL_VERSION};
use common::*;
use futures::future::join_all;
use reqwest::Method;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Error as WsError;

/// 预览引导页的 CSP（docs/spec/hub.md § 13.2），`frame-ancestors` 是测试 hub 的公网 origin。
const PREVIEW_CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; \
img-src 'self' data:; style-src 'self' 'unsafe-inline'; worker-src 'self'; frame-ancestors https://hub.test; \
base-uri 'none'; form-action 'none'";

fn assert_preview_headers(res: &reqwest::Response) {
    let h = res.headers();
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "no-referrer");
    // 要能被 Web UI 嵌入：不带 X-Frame-Options，也不带 COOP
    assert!(h.get("x-frame-options").is_none(), "{h:?}");
    assert!(h.get("cross-origin-opener-policy").is_none(), "{h:?}");
}

// ---------------------------------------------------------------------------
// 预览域名上的 HTTP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_json_tells_the_page_its_label_and_the_hub_origin() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let res = hub.preview_get(&label, "/__anycon/config.json").await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "application/json");
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_preview_headers(&res);
    assert!(res.headers().get("content-security-policy").is_none(), "only HTML carries the CSP");
    let text = res.text().await.unwrap();
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body, json!({ "label": label, "hub_origin": "https://hub.test", "protocol_version": PROTOCOL_VERSION }));
    assert!(text.starts_with(r#"{"label":"#), "field order follows the spec: {text}");
}

#[tokio::test]
async fn the_enter_page_is_html_with_the_preview_csp_and_can_be_revalidated() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let res = hub.preview_get(&label, "/__anycon/enter").await;
    assert_eq!(res.status(), 200);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    assert_eq!(res.headers()["content-security-policy"], PREVIEW_CSP);
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert_preview_headers(&res);
    let etag = res.headers()["etag"].to_str().unwrap().to_string();
    assert!(!res.bytes().await.unwrap().is_empty());

    // 条件请求的 304 也带 CSP（浏览器用它更新缓存里的头）
    let res = hub
        .preview_request(Method::GET, &label, "/__anycon/enter")
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["content-security-policy"], PREVIEW_CSP);
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert_preview_headers(&res);
}

#[tokio::test]
async fn the_service_worker_may_control_the_whole_origin_and_is_never_cached_blindly() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let res = hub.preview_get(&label, "/__anycon/sw.js").await;
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["service-worker-allowed"], "/");
    assert_eq!(res.headers()["cache-control"], "no-cache");
    assert!(res.headers()["content-type"].to_str().unwrap().contains("javascript"), "{:?}", res.headers());
    assert_preview_headers(&res);
    assert!(res.headers().get("content-security-policy").is_none(), "not an HTML response");
    let etag = res.headers()["etag"].to_str().unwrap().to_string();

    let res = hub
        .preview_request(Method::GET, &label, "/__anycon/sw.js")
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 304);
    assert_eq!(res.headers()["service-worker-allowed"], "/", "the header must survive revalidation");
    assert_eq!(res.headers()["cache-control"], "no-cache");
}

#[tokio::test]
async fn any_other_page_request_gets_the_enter_page_uncached() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let enter = hub.preview_get(&label, "/__anycon/enter").await.bytes().await.unwrap();
    for path in
        ["/", "/some/page?x=1", "/index.html", "/favicon.ico", "/__anycon/", "/api/config", "/healthz", "/auth/logout"]
    {
        let res = hub.preview_get(&label, path).await;
        assert_eq!(res.status(), 200, "{path}");
        assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"), "{path}");
        assert_eq!(res.headers()["cache-control"], "no-store", "{path}");
        assert_eq!(res.headers()["content-security-policy"], PREVIEW_CSP, "{path}");
        assert_preview_headers(&res);
        assert_eq!(res.bytes().await.unwrap(), enter, "{path}: the enter page, not the hub's API or Web UI");
    }
    // HEAD：同样的头，没有正文
    let res = hub.preview_request(Method::HEAD, &label, "/some/page").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["cache-control"], "no-store");
    assert_eq!(res.headers()["content-security-policy"], PREVIEW_CSP);
    assert!(res.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn files_that_do_not_exist_are_404_and_other_methods_are_405() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    for path in ["/__anycon/missing.js", "/__anycon/assets/missing.js", "/__anycon/a/b/c", "/__anycon/sw.js.map"] {
        let res = hub.preview_get(&label, path).await;
        assert_eq!(res.status(), 404, "{path}");
        assert_preview_headers(&res);
    }
    for (method, path) in [
        (Method::POST, "/"),
        (Method::POST, "/some/page"),
        (Method::PUT, "/__anycon/enter"),
        (Method::DELETE, "/__anycon/sw.js"),
        (Method::POST, "/__anycon/config.json"),
        (Method::PATCH, "/__anycon/anything"),
        (Method::POST, "/v1/ws"),
        (Method::HEAD, "/v1/ws"),
        // 预览域名上没有 /api、/auth：写操作一律 405
        (Method::POST, "/api/preview/tokens"),
        (Method::POST, "/auth/token"),
    ] {
        let res = hub.preview_request(method.clone(), &label, path).send().await.unwrap();
        assert_eq!(res.status(), 405, "{method} {path}");
        assert_eq!(res.headers()["allow"], "GET, HEAD");
        assert_preview_headers(&res);
    }
}

#[tokio::test]
async fn hosts_under_the_preview_domain_need_a_valid_label_and_the_configured_port() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let get = |host: String| {
        let req = hub.http.get(hub.url("/__anycon/config.json")).header("host", host);
        async move { req.send().await.unwrap() }
    };

    // label 不合法：纯文本 404（不是引导页，也不是 Web UI）
    for host in [
        "www.preview.localhost".to_string(),
        format!("{label}x.preview.localhost"),
        format!("{}.preview.localhost", &label[1..]),
        format!("sub.{label}.preview.localhost"),
        format!("{}.preview.localhost", "1".repeat(26)),
    ] {
        let res = get(host.clone()).await;
        assert_eq!(res.status(), 404, "{host}");
        assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/plain"), "{host}");
        assert_preview_headers(&res);
        assert!(
            res.headers().get("x-frame-options").is_none() && res.headers().get("content-security-policy").is_none()
        );
    }
    // 大小写不分、默认端口可省可写
    for host in [
        TestHub::preview_host(&label).to_uppercase(),
        format!("{}:80", TestHub::preview_host(&label)),
        format!("{}.", TestHub::preview_host(&label)),
    ] {
        assert_eq!(get(host.clone()).await.status(), 200, "{host}");
    }
    // 端口不对，或者根本不是预览域名：这是主域名，照旧（config.json 在主域名上是 SPA 之外的 404）
    for host in [format!("{}:8081", TestHub::preview_host(&label)), PREVIEW_DOMAIN.to_string(), "hub.test".to_string()]
    {
        let res = get(host.clone()).await;
        assert_eq!(res.status(), 404, "{host}");
        assert_eq!(res.headers()["x-frame-options"], "DENY", "{host}: the main domain's own headers");
    }
}

#[tokio::test]
async fn the_main_domain_is_unaffected_and_never_serves_the_preview_runtime() {
    let hub = TestHub::start_preview().await;
    let label = new_label();

    // 健康检查、/api/config 照旧；/api/config 多一个模板
    let res = hub.http.get(hub.url("/healthz")).header("host", "localhost").send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap()["status"], "ok");
    let cfg: Value = hub.http.get(hub.url("/api/config")).send().await.unwrap().json().await.unwrap();
    assert_eq!(cfg["preview_origin_template"], "http://{label}.preview.localhost");

    // Web UI（或占位页）：主站自己的安全头
    let main = hub.http.get(hub.url("/")).send().await.unwrap();
    assert_eq!(main.status(), 200);
    assert_eq!(main.headers()["x-frame-options"], "DENY");
    assert_eq!(main.headers()["cross-origin-opener-policy"], "same-origin");
    assert!(main.headers()["content-security-policy"].to_str().unwrap().contains("frame-ancestors 'none'"));
    let main_html = main.bytes().await.unwrap();

    // 主域名上取不到预览运行时的任何文件：带扩展名的 404；没有扩展名的走 SPA 回退，拿到的是 Web UI 的页面
    for path in ["/__anycon/sw.js", "/__anycon/config.json", "/__anycon/assets/app.js", "/__anycon/shim.js"] {
        let res = hub.http.get(hub.url(path)).send().await.unwrap();
        assert_eq!(res.status(), 404, "{path}");
    }
    let res = hub.http.get(hub.url("/__anycon/enter")).send().await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["x-frame-options"], "DENY", "served by the Web UI's fallback");
    assert_eq!(res.bytes().await.unwrap(), main_html, "/__anycon/enter on the main domain is just the SPA fallback");
    let preview_enter = hub.preview_get(&label, "/__anycon/enter").await.bytes().await.unwrap();
    assert_ne!(preview_enter, main_html, "the two embedded front ends are different files");

    // 预览域名的 Host 不影响别的路径：同一个 hub，换 Host 就是另一个站
    let res = hub.http.get(hub.url("/api/config")).header("host", "hub.test").send().await.unwrap();
    assert_eq!(res.json::<Value>().await.unwrap()["public_url"], "https://hub.test");
}

#[tokio::test]
async fn without_a_preview_url_nothing_changes() {
    let hub = TestHub::start().await;
    let label = new_label();
    // 预览域名的 Host 就是个普通的 Host：主域名的路由
    let res = hub.preview_get(&label, "/__anycon/sw.js").await;
    assert_eq!(res.status(), 404);
    assert_eq!(res.headers()["x-frame-options"], "DENY");
    let res = hub.preview_get(&label, "/api/config").await;
    let cfg: Value = res.json().await.unwrap();
    assert_eq!(cfg["preview_origin_template"], Value::Null);
    // CSP 没有 frame-src
    let res = hub.http.get(hub.url("/")).send().await.unwrap();
    let csp = res.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(!csp.contains("frame-src"), "{csp}");
    assert_eq!(csp, anycon_hub::http::CONTENT_SECURITY_POLICY);
    // 令牌接口：404 preview_disabled
    let node = hub.seed_device(DeviceKind::Daemon, "node").await;
    let web = hub.seed_device(DeviceKind::Web, "web").await;
    hub.seed_paired(&node, &web).await;
    let res = hub.request_preview_token(&web.token, &node.id, &label).await;
    assert_error(res, 404, "preview_disabled").await;
}

// ---------------------------------------------------------------------------
// POST /api/preview/tokens
// ---------------------------------------------------------------------------

/// 一个节点（daemon）和一台 web 设备，同账号、已配对。
struct Fx {
    hub: TestHub,
    node: Seeded,
    web: Seeded,
}

impl Fx {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let hub = TestHub::start_preview_with(tweak).await;
        let node = hub.seed_device(DeviceKind::Daemon, "node").await;
        let web = hub.seed_device(DeviceKind::Web, "web").await;
        hub.seed_paired(&node, &web).await;
        Self { hub, node, web }
    }

    async fn node_conn(&self) -> FakeDevice {
        FakeDevice::connect(&self.hub, Some(&self.node.token), DeviceKind::Daemon, "node").await
    }

    async fn web_conn(&self) -> FakeDevice {
        FakeDevice::connect(&self.hub, Some(&self.web.token), DeviceKind::Web, "web").await
    }

    /// 为 `label` 申请令牌（由 web 设备）并连上预览连接。
    async fn preview(&self, label: &str) -> FakeDevice {
        let token = self.hub.preview_token(&self.web, &self.node, label).await;
        FakeDevice::connect_preview(&self.hub, label, &token).await
    }
}

/// 断言响应是这个状态码和这个错误码（`{ "error": { "code" } }`）。
async fn assert_error(res: reqwest::Response, status: u16, code: &str) {
    assert_eq!(res.status().as_u16(), status);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["error"]["code"], code, "{body}");
}

#[tokio::test]
async fn a_paired_device_gets_a_token_for_a_label_and_the_hub_stores_only_its_hash() {
    let fx = Fx::new().await;
    let label = new_label();
    let before = now_ms_plus(0);
    let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, &label).await;
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    let token = body["token"].as_str().unwrap();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(body["origin"], format!("http://{label}.preview.localhost"));
    let expires = body["expires_at_ms"].as_u64().unwrap();
    let day = 24 * 3600 * 1000;
    assert!((before + day..=before + day + 10_000).contains(&expires), "24 h from now: {expires} vs {before}");

    // 库里：label 绑定到节点，令牌只有哈希
    let store = fx.hub.handle.store();
    assert_eq!(store.list_preview_labels(&fx.node.id).await.unwrap(), std::slice::from_ref(&label));
    let row = store.find_preview_token(&anycon_hub::util::hash_token(token)).await.unwrap().expect("stored by hash");
    assert_eq!(
        (row.label.as_str(), row.node_id.as_str(), row.device_id.as_str()),
        (label.as_str(), fx.node.id.as_str(), fx.web.id.as_str())
    );
    assert_eq!(row.account_id, fx.hub.admin_account_id());
    assert_eq!(row.expires_at_ms, expires);
    assert!(store.find_preview_token(token).await.unwrap().is_none(), "the plaintext is not a key");

    // 同一个 label 再要一次：另一个令牌，label 不重复绑定
    let again = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    assert_ne!(again, token);
    assert_eq!(store.list_preview_labels(&fx.node.id).await.unwrap().len(), 1);
}

fn now_ms_plus(delta: u64) -> u64 {
    anycon_hub::util::now_ms() + delta
}

#[tokio::test]
async fn only_a_device_token_works_and_the_usual_csrf_header_is_still_required() {
    let fx = Fx::new().await;
    let label = new_label();
    let body = json!({ "node": fx.node.id, "label": label });
    let post = |req: reqwest::RequestBuilder| {
        let body = body.clone();
        async move { req.json(&body).send().await.unwrap() }
    };
    let url = fx.hub.url("/api/preview/tokens");

    // 没有 Authorization：401
    let res = post(fx.hub.http.post(&url).header("x-anycon", "1")).await;
    assert_error(res, 401, "unauthorized").await;
    // 登录 cookie 不行（这个接口不看 cookie），哪怕是管理员
    let cookie = fx.hub.login_admin().await;
    let res = post(fx.hub.http.post(&url).header("x-anycon", "1").header("cookie", &cookie)).await;
    assert_eq!(res.status(), 401);
    // Authorization 在、cookie 也在：只看 Authorization，坏的就是 401（不回退到 cookie）
    let res = post(
        fx.hub.http.post(&url).header("x-anycon", "1").header("cookie", &cookie).header("authorization", "Bearer nope"),
    )
    .await;
    assert_eq!(res.status(), 401);
    // 不是 Bearer、空 token、登录 token（不是设备 token）
    for auth in ["Basic abc".to_string(), "Bearer ".to_string(), format!("Bearer {}", fx.hub.admin_token)] {
        let res = post(fx.hub.http.post(&url).header("x-anycon", "1").header("authorization", &auth)).await;
        assert_eq!(res.status(), 401, "{auth}");
    }
    // 缺 X-Anycon：CSRF 中间件照常拦
    let res = post(fx.hub.http.post(&url).header("authorization", format!("Bearer {}", fx.web.token))).await;
    assert_error(res, 403, "csrf").await;
    // 设备 token + X-Anycon：通过
    let res = post(
        fx.hub.http.post(&url).header("x-anycon", "1").header("authorization", format!("bearer {}", fx.web.token)),
    )
    .await;
    assert_eq!(res.status(), 200, "the scheme is case-insensitive");

    // 吊销之后同一个 token 不再有效
    let admin = fx.hub.login_admin().await;
    let res = fx.hub.send(Method::DELETE, &format!("/api/devices/{}", fx.web.id), &admin, Value::Null).await;
    assert_eq!(res.status(), 204);
    let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, &label).await;
    assert_eq!(res.status(), 401);
}

#[tokio::test]
async fn bad_labels_and_unusable_nodes_are_refused_without_binding_anything() {
    let fx = Fx::new().await;
    let store = fx.hub.handle.store();
    let label = new_label();

    // label 格式不对
    let bad_labels = [
        String::new(),
        "short".to_string(),
        label.to_uppercase(),
        format!("{label}a"),
        label.replace(&label[..1], "1"),
        "é".repeat(13),
    ];
    for bad in &bad_labels {
        let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, bad).await;
        assert_error(res, 400, "invalid_label").await;
    }
    // body 缺字段：400
    let res = fx
        .hub
        .http
        .post(fx.hub.url("/api/preview/tokens"))
        .header("x-anycon", "1")
        .header("authorization", format!("Bearer {}", fx.web.token))
        .json(&json!({ "node": fx.node.id }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);

    // 节点：不存在 / 没配对 / 别的账号的 / 不是 daemon / 已吊销，一律 404 not_found
    let unpaired = fx.hub.seed_device(DeviceKind::Daemon, "unpaired").await;
    let (other_account, _cookie) = fx.hub.invite_account(&fx.hub.login_admin().await).await;
    let foreign = fx.hub.seed_device_in(&other_account, DeviceKind::Daemon, "foreign").await;
    fx.hub.seed_paired(&foreign, &fx.web).await; // 库里硬配上也不行：账号不同
    let not_daemon = fx.hub.seed_device(DeviceKind::Cli, "cli").await;
    fx.hub.seed_paired(&not_daemon, &fx.web).await;
    let revoked = fx.hub.seed_device(DeviceKind::Daemon, "revoked").await;
    fx.hub.seed_paired(&revoked, &fx.web).await;
    assert!(fx.hub.handle.state.revoke_device(&revoked.id, None).await.unwrap());
    for (what, node) in [
        ("random id", "0".repeat(32)),
        ("unpaired", unpaired.id.clone()),
        ("another account's", foreign.id.clone()),
        ("not a daemon", not_daemon.id.clone()),
        ("revoked", revoked.id.clone()),
    ] {
        let res = fx.hub.request_preview_token(&fx.web.token, &node, &label).await;
        assert_error(res, 404, "not_found").await;
        assert!(store.list_preview_labels(&node).await.unwrap().is_empty(), "{what}: nothing was bound");
    }
    // 别的账号的设备不能借自己账号里的设备去用这个节点
    let outsider = fx.hub.seed_device_in(&other_account, DeviceKind::Web, "outsider").await;
    let res = fx.hub.request_preview_token(&outsider.token, &fx.node.id, &label).await;
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn a_label_belongs_to_the_first_node_that_asks_and_nodes_have_a_label_limit() {
    let fx = Fx::with(|c| c.limits.preview_labels_per_node = 2).await;
    let other_node = fx.hub.seed_device(DeviceKind::Daemon, "other-node").await;
    fx.hub.seed_paired(&other_node, &fx.web).await;
    let label = new_label();
    fx.hub.preview_token(&fx.web, &fx.node, &label).await;

    // 同账号的另一个节点要同一个 label：label_taken，什么都不绑
    let res = fx.hub.request_preview_token(&fx.web.token, &other_node.id, &label).await;
    assert_error(res, 409, "label_taken").await;
    assert!(fx.hub.handle.store().list_preview_labels(&other_node.id).await.unwrap().is_empty());

    // 节点最多绑 2 个 label；已经绑定的 label 照常可以再要令牌
    fx.hub.preview_token(&fx.web, &fx.node, &new_label()).await;
    let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, &new_label()).await;
    assert_error(res, 409, "too_many_labels").await;
    fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    assert_eq!(fx.hub.handle.store().list_preview_labels(&fx.node.id).await.unwrap().len(), 2);
    // 另一个节点有自己的额度
    fx.hub.preview_token(&fx.web, &other_node, &new_label()).await;
}

#[tokio::test]
async fn a_device_keeps_a_bounded_number_of_tokens_and_requests_are_rate_limited_per_device() {
    let fx = Fx::with(|c| {
        c.limits.preview_tokens_per_device = 3;
        c.limits.preview_tokens_per_min_per_device = 6;
    })
    .await;
    let label = new_label();
    let mut tokens = Vec::new();
    for _ in 0..5 {
        tokens.push(fx.hub.preview_token(&fx.web, &fx.node, &label).await);
    }
    let store = fx.hub.handle.store();
    let stored = |t: &String| {
        let hash = anycon_hub::util::hash_token(t);
        let store = store.clone();
        async move { store.find_preview_token(&hash).await.unwrap().is_some() }
    };
    assert!(!stored(&tokens[0]).await && !stored(&tokens[1]).await, "the oldest two were evicted");
    for t in &tokens[2..] {
        assert!(stored(t).await);
    }
    // 第 6 次还行，第 7 次限速：429 + Retry-After；别的设备不受影响
    fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, &label).await;
    assert_eq!(res.status(), 429);
    assert!(res.headers().get("retry-after").is_some());
    let web2 = fx.hub.seed_device(DeviceKind::Web, "web2").await;
    fx.hub.seed_paired(&web2, &fx.node).await;
    assert_eq!(fx.hub.request_preview_token(&web2.token, &fx.node.id, &label).await.status(), 200);
}

// ---------------------------------------------------------------------------
// 预览连接：升级与认证
// ---------------------------------------------------------------------------

async fn expect_preview_peer(node: &mut FakeDevice) -> (String, String) {
    match node.recv_control().await {
        Control::PreviewPeer { peer, label } => (peer, label),
        other => panic!("expected preview_peer, got {other:?}"),
    }
}

/// 期望下一条是 `auth_fail { reason }`，随后连接以 `close` 关闭。
async fn expect_auth_fail(dev: &mut FakeDevice, reason: AuthFailReason, close: u16) {
    match dev.recv_control().await {
        Control::AuthFail { reason: got } => assert_eq!(got, reason),
        other => panic!("expected auth_fail {reason:?}, got {other:?}"),
    }
    assert_eq!(dev.expect_close().await, Some(close));
}

#[tokio::test]
async fn the_upgrade_needs_exactly_the_preview_origin_of_that_request() {
    let hub = TestHub::start_preview().await;
    let label = new_label();
    let host = TestHub::preview_host(&label);
    let right = TestHub::preview_origin(&label);
    let status_of = |origin: Option<String>| {
        let host = host.clone();
        let url = hub.ws_url();
        async move {
            let mut headers = vec![("host", host)];
            if let Some(o) = origin {
                headers.push(("origin", o));
            }
            let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            match FakeDevice::open_with(&url, &headers).await {
                Ok(_) => 101,
                Err(WsError::Http(res)) => res.status().as_u16(),
                Err(e) => panic!("{e}"),
            }
        }
    };
    assert_eq!(status_of(Some(right.clone())).await, 101);
    for wrong in [
        None,
        Some(TestHub::preview_origin(&new_label())),
        Some("https://hub.test".to_string()),
        Some(format!("https://{host}")),
        Some(format!("{right}:8080")),
        Some(format!("{right}/")),
        Some(right.to_uppercase()),
        Some("null".to_string()),
        Some(String::new()),
    ] {
        assert_eq!(status_of(wrong.clone()).await, 403, "{wrong:?}");
    }
}

#[tokio::test]
async fn preview_upgrades_have_their_own_ip_rate_limit() {
    let hub = TestHub::start_preview_with(|c| c.limits.preview_ws_upgrade_per_min = 3).await;
    let label = new_label();
    let mut open = Vec::new();
    for _ in 0..3 {
        open.push(FakeDevice::open_preview(&hub, &label).await.expect("within the limit"));
    }
    match FakeDevice::open_preview(&hub, &label).await {
        Err(WsError::Http(res)) => {
            assert_eq!(res.status(), 429);
            assert!(res.headers().get("retry-after").is_some());
        }
        other => panic!("expected 429, got {:?}", other.map(|_| ())),
    }
    // 主域名的升级桶是另一个，不受影响
    let main = FakeDevice::open_with(&hub.ws_url(), &[]).await;
    assert!(main.is_ok(), "the main domain's WebSocket bucket is separate");
}

#[tokio::test]
async fn auth_must_be_a_preview_token_on_the_preview_domain_and_only_there() {
    let fx = Fx::new().await;
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let preview_auth = |device_token: Option<&str>, kind: DeviceKind, token: Option<&str>| Control::Auth {
        device_token: device_token.map(str::to_string),
        protocol_version: PROTOCOL_VERSION,
        client: client_info(kind, "x"),
        preview_token: token.map(str::to_string),
    };

    // 带了 device_token、kind 不是 web、没有 preview_token（含「临时连接」）：protocol
    for (what, auth) in [
        ("device token too", preview_auth(Some(&fx.web.token), DeviceKind::Web, Some(&token))),
        ("device token only", preview_auth(Some(&fx.web.token), DeviceKind::Web, None)),
        ("daemon kind", preview_auth(None, DeviceKind::Daemon, Some(&token))),
        ("cli kind", preview_auth(None, DeviceKind::Cli, Some(&token))),
        ("temporary connection", preview_auth(None, DeviceKind::Web, None)),
    ] {
        let mut d = FakeDevice::open_preview(&fx.hub, &label).await.unwrap();
        d.send_control(&auth).await;
        match d.recv_control().await {
            Control::AuthFail { reason } => assert_eq!(reason, AuthFailReason::Protocol, "{what}"),
            other => panic!("{what}: {other:?}"),
        }
        assert!(d.expect_close().await.is_some());
    }

    // 预览令牌只在预览域名上：主域名上带 preview_token 也是 protocol
    let mut d = FakeDevice::open_with(&fx.hub.ws_url(), &[]).await.unwrap();
    d.send_preview_auth(Some(&token)).await;
    expect_auth_fail(&mut d, AuthFailReason::Protocol, 4005).await;
    assert_eq!(fx.hub.handle.state.registry.preview_count(&fx.hub.admin_account_id()), 0);
}

#[tokio::test]
async fn unknown_expired_and_misplaced_tokens_are_all_just_invalid() {
    let fx = Fx::with(|c| c.timing.preview_token_ttl = Duration::from_millis(300)).await;
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let other_label = new_label();
    let other_token = fx.hub.preview_token(&fx.web, &fx.node, &other_label).await;

    // 令牌是真的，但用在了别的 label 的主机名上
    let mut d = FakeDevice::open_preview(&fx.hub, &other_label).await.unwrap();
    d.send_preview_auth(Some(&token)).await;
    expect_auth_fail(&mut d, AuthFailReason::Invalid, 1008).await;
    // 不存在的令牌（含超长的）
    for bogus in [anycon_hub::util::new_secret(), String::new(), "x".repeat(10_000)] {
        let mut d = FakeDevice::open_preview(&fx.hub, &label).await.unwrap();
        d.send_preview_auth(Some(&bogus)).await;
        expect_auth_fail(&mut d, AuthFailReason::Invalid, 1008).await;
    }
    // 过期
    tokio::time::sleep(Duration::from_millis(400)).await;
    for (l, t) in [(&label, &token), (&other_label, &other_token)] {
        let mut d = FakeDevice::open_preview(&fx.hub, l).await.unwrap();
        d.send_preview_auth(Some(t)).await;
        expect_auth_fail(&mut d, AuthFailReason::Invalid, 1008).await;
    }
}

#[tokio::test]
async fn a_preview_connection_is_not_a_device_and_has_no_temporary_connection_time_limit() {
    let fx = Fx::with(|c| c.timing.temp_conn_ttl = Duration::from_millis(400)).await;
    let state = fx.hub.handle.state.clone();
    let account = fx.hub.admin_account_id();
    let mut node = fx.node_conn().await;
    let devices_before = state.registry.permanent_count();
    let label = new_label();
    let mut p = fx.preview(&label).await;
    let (peer, l) = expect_preview_peer(&mut node).await;
    assert_eq!((peer.as_str(), l.as_str()), (p.id.as_str(), label.as_str()));
    // 临时 id：不是设备，不计入设备数，也不算「web 设备在线」
    assert_ne!(p.id, fx.web.id);
    assert_eq!(state.registry.permanent_count(), devices_before);
    assert!(!state.registry.account_has_web_online(&account));
    assert_eq!(state.registry.preview_count(&account), 1);
    assert!(fx.hub.handle.store().get_device(&p.id).await.unwrap().is_none());
    // 同时，真正的临时连接在 400 ms 后被关；预览连接没有这个限制
    let mut temp = FakeDevice::connect(&fx.hub, None, DeviceKind::Web, "temp").await;
    assert_eq!(temp.expect_close().await, Some(4005));
    p.send_env(&node.id, Kind::Data, b"still here".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"still here");
    assert_eq!(state.registry.preview_count(&account), 1);
}

#[tokio::test]
async fn the_number_of_preview_connections_per_account_is_limited() {
    let fx = Fx::with(|c| c.limits.preview_max_connections = 2).await;
    let state = fx.hub.handle.state.clone();
    let account = fx.hub.admin_account_id();
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let first = FakeDevice::connect_preview(&fx.hub, &label, &token).await;
    let _second = FakeDevice::connect_preview(&fx.hub, &label, &token).await;
    assert_eq!(state.registry.preview_count(&account), 2);

    let mut third = FakeDevice::open_preview(&fx.hub, &label).await.unwrap();
    third.send_preview_auth(Some(&token)).await;
    expect_auth_fail(&mut third, AuthFailReason::Quota, 1008).await;
    assert_eq!(state.registry.preview_count(&account), 2, "a refused connection is not counted");

    // 另一个账号有自己的额度
    let (other, _cookie) = fx.hub.invite_account(&fx.hub.login_admin().await).await;
    let (node2, web2) = (
        fx.hub.seed_device_in(&other, DeviceKind::Daemon, "n2").await,
        fx.hub.seed_device_in(&other, DeviceKind::Web, "w2").await,
    );
    fx.hub.seed_paired(&node2, &web2).await;
    let label2 = new_label();
    let t2 = fx.hub.preview_token(&web2, &node2, &label2).await;
    let _theirs = FakeDevice::connect_preview(&fx.hub, &label2, &t2).await;

    // 断开一条之后额度回来
    drop(first);
    wait_until(|| state.registry.preview_count(&account) == 1).await;
    let _again = FakeDevice::connect_preview(&fx.hub, &label, &token).await;
    assert_eq!(state.registry.preview_count(&account), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_connections_cannot_get_past_the_limit() {
    let fx = Fx::with(|c| c.limits.preview_max_connections = 2).await;
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let attempts = (0..8).map(|_| async {
        let mut d = FakeDevice::open_preview(&fx.hub, &label).await.unwrap();
        d.send_preview_auth(Some(&token)).await;
        match d.recv_control().await {
            Control::AuthOk { .. } => Some(d),
            Control::AuthFail { reason } => {
                assert_eq!(reason, AuthFailReason::Quota);
                None
            }
            other => panic!("{other:?}"),
        }
    });
    let results = join_all(attempts).await;
    assert_eq!(results.iter().filter(|r| r.is_some()).count(), 2);
    assert_eq!(fx.hub.handle.state.registry.preview_count(&fx.hub.admin_account_id()), 2);
}

async fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// 预览连接：上下线与路由
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_node_hears_preview_peer_not_peer_online_and_peer_offline_when_it_leaves() {
    let fx = Fx::new().await;
    let label = new_label();
    let mut node = fx.node_conn().await;
    assert!(node.peers_online.is_empty());
    let p = fx.preview(&label).await;
    assert_eq!(p.peers_online, std::slice::from_ref(&fx.node.id), "the node is online: auth_ok lists it");

    let (peer, got_label) = expect_preview_peer(&mut node).await;
    assert_eq!((peer.as_str(), got_label.as_str()), (p.id.as_str(), label.as_str()));
    node.expect_silence(Duration::from_millis(200)).await; // 没有 peer_online
    let node_conn = fx.hub.handle.state.registry.get(&fx.node.id).unwrap();
    assert!(node_conn.has_peer(&p.id), "the node's peer set now includes the preview connection");

    // 第二条预览连接：又一条 preview_peer
    let p2 = fx.preview(&new_label()).await;
    assert_eq!(expect_preview_peer(&mut node).await.0, p2.id);

    // 断开：节点收到 peer_offline，对端集合里也去掉了
    let id = p.id.clone();
    drop(p);
    match node.recv_control().await {
        Control::PeerOffline { device } => assert_eq!(device, id),
        other => panic!("{other:?}"),
    }
    assert!(!node_conn.has_peer(&id));
    assert!(node_conn.has_peer(&p2.id));
    assert_eq!(fx.hub.handle.state.registry.permanent_count(), 1, "only the node itself is a device");
}

#[tokio::test]
async fn a_preview_connection_that_arrives_before_its_node_is_introduced_when_the_node_comes_online() {
    let fx = Fx::new().await;
    let label = new_label();
    let mut p = fx.preview(&label).await;
    assert!(p.peers_online.is_empty(), "the node is offline");

    // 节点上线：auth_ok 里没有预览连接，随后逐条 preview_peer；预览连接收到 peer_online
    let mut node = fx.node_conn().await;
    assert!(node.peers_online.is_empty());
    let (peer, got_label) = expect_preview_peer(&mut node).await;
    assert_eq!((peer.as_str(), got_label.as_str()), (p.id.as_str(), label.as_str()));
    node.expect_silence(Duration::from_millis(200)).await;
    match p.recv_control().await {
        Control::PeerOnline { device } => assert_eq!(device, fx.node.id),
        other => panic!("{other:?}"),
    }
    // 现在能通了
    p.send_env(&fx.node.id, Kind::Handshake, b"hi".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"hi");
}

#[tokio::test]
async fn when_the_node_leaves_and_returns_its_previews_are_told_and_re_introduced() {
    let fx = Fx::new().await;
    let label = new_label();
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&label).await;
    let mut p_b = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    expect_preview_peer(&mut node).await;

    // 节点下线：每条预览连接收到 peer_offline { node }；这时发给节点是 undeliverable
    drop(node);
    for dev in [&mut p, &mut p_b] {
        match dev.recv_control().await {
            Control::PeerOffline { device } => assert_eq!(device, fx.node.id),
            other => panic!("{other:?}"),
        }
    }
    let seq = p.send_env(&fx.node.id, Kind::Data, b"anyone?".to_vec()).await;
    match p.recv_control().await {
        Control::Undeliverable { to, seq: s } => assert_eq!((to, s), (fx.node.id.clone(), seq)),
        other => panic!("{other:?}"),
    }

    // 节点重连：两条预览连接各一条 preview_peer（顺序不定），各自收到 peer_online；节点的 auth_ok 里没有它们
    let mut node = fx.node_conn().await;
    assert!(node.peers_online.is_empty());
    let mut peers = vec![expect_preview_peer(&mut node).await.0, expect_preview_peer(&mut node).await.0];
    peers.sort();
    let mut expected = vec![p.id.clone(), p_b.id.clone()];
    expected.sort();
    assert_eq!(peers, expected);
    node.expect_silence(Duration::from_millis(200)).await;
    for dev in [&mut p, &mut p_b] {
        match dev.recv_control().await {
            Control::PeerOnline { device } => assert_eq!(device, fx.node.id),
            other => panic!("{other:?}"),
        }
    }
    // 双向都通
    node.send_env(&p.id, Kind::Data, b"welcome back".to_vec()).await;
    assert_eq!(p.recv_env().await.body, b"welcome back");
    p_b.send_env(&fx.node.id, Kind::Handshake, b"again".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"again");
}

#[tokio::test]
async fn a_node_that_reconnects_over_its_old_connection_gets_its_previews_again() {
    let fx = Fx::new().await;
    let mut old = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut old).await;

    let mut new = fx.node_conn().await; // 同一个设备再连一次：顶掉旧的
    assert_eq!(old.expect_close().await, Some(4001));
    assert_eq!(expect_preview_peer(&mut new).await.0, p.id);
    match p.recv_control().await {
        Control::PeerOnline { device } => assert_eq!(device, fx.node.id),
        other => panic!("{other:?}"),
    }
    // 旧连接收尾不会让预览连接以为节点下线了
    p.expect_silence(Duration::from_millis(300)).await;
    new.send_env(&p.id, Kind::Data, b"from the new one".to_vec()).await;
    assert_eq!(p.recv_env().await.body, b"from the new one");
}

#[tokio::test]
async fn a_preview_connection_only_talks_handshake_and_data_with_its_own_node() {
    let fx = Fx::new().await;
    let mut node = fx.node_conn().await;
    let mut web = fx.web_conn().await; // 节点的另一个配对设备，同账号
    let _ = node.drain_controls(Duration::from_millis(200)).await; // web 上线的 peer_online
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;

    // 双向转发，原样，from 是预览连接的临时 id
    for (kind, body) in
        [(Kind::Handshake, b"h1".to_vec()), (Kind::Data, b"d1".to_vec()), (Kind::Data, vec![0u8; 100_000])]
    {
        p.send_env(&fx.node.id, kind, body.clone()).await;
        let e = node.recv_env().await;
        assert_eq!((e.kind, e.from.as_str(), e.to.as_str()), (kind, p.id.as_str(), fx.node.id.as_str()));
        assert_eq!(e.body, body);
        node.send_env(&p.id, kind, body.clone()).await;
        let e = p.recv_env().await;
        assert_eq!((e.kind, e.from.as_str(), e.to.as_str()), (kind, fx.node.id.as_str(), p.id.as_str()));
        assert_eq!(e.body, body);
    }

    // 发给别人（同账号的 web 设备、随机 id、自己）：dropped { not_paired }，收件人什么都收不到。
    // 回执每秒一条，所以每个用例之间隔开。
    for to in [fx.web.id.clone(), "9".repeat(32)] {
        let seq = p.send_env(&to, Kind::Data, b"not for you".to_vec()).await;
        match p.recv_control().await {
            Control::Dropped { seq: s, reason } => assert_eq!((s, reason), (seq, DropReason::NotPaired)),
            other => panic!("{other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    // notify：预览连接发出的、发给预览连接的，都是 not_paired
    let seq = p.send_env(&fx.node.id, Kind::Notify, b"x".to_vec()).await;
    match p.recv_control().await {
        Control::Dropped { seq: s, reason } => assert_eq!((s, reason), (seq, DropReason::NotPaired)),
        other => panic!("{other:?}"),
    }
    let seq = node.send_env(&p.id, Kind::Notify, b"x".to_vec()).await;
    match node.recv_control().await {
        Control::Dropped { seq: s, reason } => assert_eq!((s, reason), (seq, DropReason::NotPaired)),
        other => panic!("{other:?}"),
    }
    // 没有任何东西漏给 web 设备或节点
    web.expect_silence(Duration::from_millis(200)).await;
    node.expect_silence(Duration::from_millis(200)).await;
    // 节点发给不认识的 id：照旧 not_paired（回执每秒一条，等一下）
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let seq = node.send_env(&"9".repeat(32), Kind::Data, b"x".to_vec()).await;
    assert!(matches!(node.recv_control().await, Control::Dropped { seq: s, .. } if s == seq));
}

#[tokio::test]
async fn control_messages_from_a_preview_connection_are_ignored() {
    let fx = Fx::new().await;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    let (code, match_id) = new_code();
    let _ = code;
    for ctl in [
        Control::Revoke { device: fx.node.id.clone() },
        Control::Revoke { device: fx.web.id.clone() },
        Control::PairBegin { match_id: match_id.clone() },
        Control::PairJoin { match_id },
        Control::EnrollBegin { node: fx.node.id.clone() },
        Control::PairConfirm { peer: fx.node.id.clone() },
        Control::QueueDrop { seq: 1 },
    ] {
        p.send_control(&ctl).await;
    }
    // 一条回执都没有，什么都没发生
    p.expect_silence(Duration::from_millis(400)).await;
    node.expect_silence(Duration::from_millis(100)).await;
    let store = fx.hub.handle.store();
    for id in [&fx.node.id, &fx.web.id] {
        assert!(store.get_device(id).await.unwrap().unwrap().revoked_at_ms.is_none());
    }
    assert!(store.is_paired(&fx.node.id, &fx.web.id).await.unwrap());
    // 连接本身还好
    p.send_env(&fx.node.id, Kind::Data, b"ok".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"ok");
}

// ---------------------------------------------------------------------------
// 流量预算
// ---------------------------------------------------------------------------

async fn send_data_with_seq(from: &mut FakeDevice, to: &str, seq: u64, len: usize) {
    let env = Envelope::new(from.id.clone(), to, Kind::Data, seq, vec![7u8; len]);
    from.send_raw(env.encode().unwrap()).await;
}

fn backpressured(controls: &[Control], to: &str) -> Vec<u64> {
    controls
        .iter()
        .filter_map(|c| match c {
            Control::Backpressure { to: t, seq } if t == to => Some(*seq),
            _ => None,
        })
        .collect()
}

fn rate_limits(controls: &[Control]) -> Vec<(u64, Option<String>)> {
    controls
        .iter()
        .filter_map(|c| match c {
            Control::RateLimited { retry_after_ms, to } => Some((*retry_after_ms, to.clone())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn preview_traffic_is_paid_from_the_account_budget_and_only_that_peer_is_paused() {
    // 600 000 字节 / 分钟 = 每秒回填 10 KB
    let fx = Fx::with(|c| c.limits.preview_bytes_per_min = 600_000).await;
    let account = fx.hub.admin_account_id();
    let budget = &fx.hub.handle.state.preview_budget;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    let mut web = fx.web_conn().await; // 节点的另一个对端，不受预览预算影响
    let _ = node.drain_controls(Duration::from_millis(100)).await;

    // 节点 → 预览：预算抽干后，两封都被退回，data 粘住
    budget.charge(&account, 600_000);
    send_data_with_seq(&mut node, &p.id, 1, 10_000).await;
    send_data_with_seq(&mut node, &p.id, 2, 10_000).await;
    let controls = node.drain_controls(Duration::from_millis(400)).await;
    assert_eq!(backpressured(&controls, &p.id), [1, 2], "{controls:?}");
    let limits = rate_limits(&controls);
    assert_eq!(limits.len(), 1, "at most one rate_limited per second: {controls:?}");
    assert_eq!(limits[0].1.as_deref(), Some(p.id.as_str()), "`to` is the paused peer");
    assert!((100..=30_000).contains(&limits[0].0), "{limits:?}");
    p.expect_silence(Duration::from_millis(100)).await;

    // 只暂停发往预览连接的信封：同一条连接上发给别的对端（web 设备）的照常转发
    node.send_env(&fx.web.id, Kind::Data, b"unaffected".to_vec()).await;
    assert_eq!(web.recv_env().await.body, b"unaffected");

    // 粘住：等额度回来之后，按升序重发被退回的才会放行
    tokio::time::sleep(Duration::from_millis(2400)).await;
    send_data_with_seq(&mut node, &p.id, 3, 100).await; // 抢在 1、2 前面：退回
    let controls = node.drain_controls(Duration::from_millis(200)).await;
    assert_eq!(backpressured(&controls, &p.id), [3], "a later seq cannot overtake the returned ones: {controls:?}");
    for seq in [1, 2, 3] {
        send_data_with_seq(&mut node, &p.id, seq, if seq == 3 { 100 } else { 10_000 }).await;
    }
    let seqs: Vec<u64> = vec![p.recv_env().await.seq, p.recv_env().await.seq, p.recv_env().await.seq];
    assert_eq!(seqs, [1, 2, 3]);
    assert!(node.drain_controls(Duration::from_millis(200)).await.is_empty());

    // 反方向用同一个桶：预览 → 节点
    budget.charge(&account, 600_000);
    p.send_env(&fx.node.id, Kind::Handshake, vec![1u8; 10_000]).await;
    let controls = p.drain_controls(Duration::from_millis(300)).await;
    let limits = rate_limits(&controls);
    assert_eq!(limits.len(), 1, "{controls:?}");
    assert_eq!(limits[0].1.as_deref(), Some(fx.node.id.as_str()));
    assert!(
        matches!(controls.iter().find(|c| matches!(c, Control::Backpressure { .. })), Some(Control::Backpressure { to, .. }) if to == &fx.node.id)
    );
    node.expect_silence(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn running_out_of_preview_budget_never_gets_a_connection_closed_for_being_rate_limited() {
    // 预算小到一封都送不出去（6000 字节 / 分钟），而「一直被限速退回」的断开阈值只有 0.8 秒
    let fx = Fx::with(|c| {
        c.limits.preview_bytes_per_min = 6_000;
        c.timing.rate_violation_close_after = Duration::from_millis(800);
    })
    .await;
    let account = fx.hub.admin_account_id();
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    fx.hub.handle.state.preview_budget.charge(&account, 6_000);

    // 节点和预览连接都持续发，持续 2.5 秒（是阈值的三倍多）
    let start = Instant::now();
    let mut rounds = 0u64;
    while start.elapsed() < Duration::from_millis(2500) {
        // 重发被退回的那封，同时还有新的 seq 不管不顾地往下发（比如已经在路上的）：粘住之后一律退回
        for seq in [1, 2 + rounds] {
            send_data_with_seq(&mut node, &p.id, seq, 10_000).await;
            send_data_with_seq(&mut p, &fx.node.id, seq, 10_000).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        rounds += 1;
    }
    assert!(rounds > 20);
    // 两条连接都还在（drain 遇到 Close 会 panic），而且收到的都是退回和暂停
    let controls = node.drain_controls(Duration::from_millis(100)).await;
    assert!(!backpressured(&controls, &p.id).is_empty() && !rate_limits(&controls).is_empty(), "{controls:?}");
    let controls = p.drain_controls(Duration::from_millis(100)).await;
    assert!(!backpressured(&controls, &fx.node.id).is_empty());
    // 节点发给别的对端的流量照样通：连接没有被当成违规
    let mut web = fx.web_conn().await;
    node.send_env(&fx.web.id, Kind::Data, b"still fine".to_vec()).await;
    assert_eq!(web.recv_env().await.body, b"still fine");
}

#[tokio::test]
async fn preview_traffic_does_not_spend_the_per_connection_byte_budget() {
    // 每连接的字节预算压到下限（约 201 KiB / 分钟），预览预算是默认的 512 MiB
    let fx = Fx::with(|c| {
        c.limits.client_bytes_per_min = 205_824;
        c.limits.daemon_bytes_per_min = 205_824;
    })
    .await;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    // 两个方向各 6 × 100 KB = 600 KB，远超每连接的预算，一封都没被退回
    for i in 0..6 {
        let kind = if i == 0 { Kind::Handshake } else { Kind::Data };
        p.send_env(&fx.node.id, kind, vec![1u8; 100_000]).await;
        assert_eq!(node.recv_env().await.body.len(), 100_000);
        node.send_env(&p.id, kind, vec![2u8; 100_000]).await;
        assert_eq!(p.recv_env().await.body.len(), 100_000);
    }
    assert!(node.drain_controls(Duration::from_millis(200)).await.is_empty());
    assert!(p.drain_controls(Duration::from_millis(100)).await.is_empty());
    // 同样大小的非预览流量照旧受每连接预算限制：节点发给 web 设备，两封还装得下（201 KiB），第三封被退回
    let mut web = fx.web_conn().await;
    let _ = node.drain_controls(Duration::from_millis(100)).await;
    for _ in 0..2 {
        node.send_env(&fx.web.id, Kind::Data, vec![3u8; 100_000]).await;
        assert_eq!(web.recv_env().await.body.len(), 100_000);
    }
    node.send_env(&fx.web.id, Kind::Data, vec![3u8; 100_000]).await;
    let controls = node.drain_controls(Duration::from_millis(300)).await;
    assert!(controls.iter().any(|c| matches!(c, Control::RateLimited { to: None, .. })), "{controls:?}");
    assert!(controls.iter().any(|c| matches!(c, Control::Backpressure { .. })), "{controls:?}");
    // 节点自己的字节桶现在见底了，但预览流量不看它：照样转发
    node.send_env(&p.id, Kind::Data, vec![4u8; 100_000]).await;
    assert_eq!(p.recv_env().await.body.len(), 100_000);
    assert!(node.drain_controls(Duration::from_millis(200)).await.is_empty());
}

#[tokio::test]
async fn the_per_connection_envelope_rate_still_applies_to_previews() {
    let fx = Fx::with(|c| c.limits.conn_envelopes_per_sec = 5).await;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    for _ in 0..30 {
        p.send_env(&fx.node.id, Kind::Data, b"x".to_vec()).await;
    }
    let controls = p.drain_controls(Duration::from_millis(500)).await;
    let limits = rate_limits(&controls);
    assert!(limits.iter().any(|(_, to)| to.is_none()), "the whole connection is asked to pause: {controls:?}");
    assert!(controls.iter().any(|c| matches!(c, Control::Backpressure { .. })), "{controls:?}");
}

// ---------------------------------------------------------------------------
// 吊销、过期、清理
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoking_the_issuing_device_closes_the_previews_that_use_its_tokens_only() {
    let fx = Fx::new().await;
    let web2 = fx.hub.seed_device(DeviceKind::Web, "web2").await;
    fx.hub.seed_paired(&web2, &fx.node).await;
    let mut node = fx.node_conn().await;
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let mut p = FakeDevice::connect_preview(&fx.hub, &label, &token).await;
    let token2 = fx.hub.preview_token(&web2, &fx.node, &label).await;
    let mut p2 = FakeDevice::connect_preview(&fx.hub, &label, &token2).await;
    expect_preview_peer(&mut node).await;
    expect_preview_peer(&mut node).await;

    let admin = fx.hub.login_admin().await;
    let res = fx.hub.send(Method::DELETE, &format!("/api/devices/{}", fx.web.id), &admin, Value::Null).await;
    assert_eq!(res.status(), 204);
    assert_eq!(p.expect_close().await, Some(4004));
    // 节点听说了被吊销的设备下线（pairing 没了），也听说了它的预览连接断开
    let controls = node.drain_controls(Duration::from_millis(300)).await;
    assert!(controls.contains(&Control::PeerOffline { device: p.id.clone() }), "{controls:?}");
    assert!(!controls.contains(&Control::PeerOffline { device: p2.id.clone() }), "{controls:?}");
    let store = fx.hub.handle.store();
    assert!(store.find_preview_token(&anycon_hub::util::hash_token(&token)).await.unwrap().is_none());
    // 另一台设备签发的令牌和连接不受影响
    p2.send_env(&fx.node.id, Kind::Data, b"alive".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"alive");
    // 被吊销设备的令牌也不能再用来连接
    let mut again = FakeDevice::open_preview(&fx.hub, &label).await.unwrap();
    again.send_preview_auth(Some(&token)).await;
    expect_auth_fail(&mut again, AuthFailReason::Invalid, 1008).await;
}

#[tokio::test]
async fn a_device_revoked_over_the_websocket_takes_its_preview_tokens_with_it() {
    let fx = Fx::new().await;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    // 节点通过 control.revoke 吊销配对的 web 设备（daemon 侧 node.revoke_device 的 hub 那一半）
    node.send_control(&Control::Revoke { device: fx.web.id.clone() }).await;
    assert_eq!(p.expect_close().await, Some(4004));
    let controls = node.drain_controls(Duration::from_millis(300)).await;
    assert!(controls.contains(&Control::PeerOffline { device: p.id.clone() }), "{controls:?}");
}

#[tokio::test]
async fn revoking_the_node_removes_its_labels_and_tokens_and_closes_its_previews() {
    let fx = Fx::new().await;
    let mut node = fx.node_conn().await;
    let label = new_label();
    let token = fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    let mut p = FakeDevice::connect_preview(&fx.hub, &label, &token).await;
    expect_preview_peer(&mut node).await;
    let store = fx.hub.handle.store();
    assert_eq!(store.list_preview_labels(&fx.node.id).await.unwrap().len(), 1);

    let admin = fx.hub.login_admin().await;
    let res = fx.hub.send(Method::DELETE, &format!("/api/devices/{}", fx.node.id), &admin, Value::Null).await;
    assert_eq!(res.status(), 204);
    assert_eq!(node.expect_close().await, Some(4004));
    assert_eq!(p.expect_close().await, Some(4004));
    assert!(store.list_preview_labels(&fx.node.id).await.unwrap().is_empty());
    assert!(store.find_preview_token(&anycon_hub::util::hash_token(&token)).await.unwrap().is_none());
    wait_until(|| fx.hub.handle.state.registry.preview_count(&fx.hub.admin_account_id()) == 0).await;
    // 不能再为这个节点申请令牌
    let res = fx.hub.request_preview_token(&fx.web.token, &fx.node.id, &label).await;
    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn an_expired_token_closes_the_connection_with_4004() {
    let fx = Fx::with(|c| c.timing.preview_token_ttl = Duration::from_millis(1500)).await;
    let mut node = fx.node_conn().await;
    let mut p = fx.preview(&new_label()).await;
    expect_preview_peer(&mut node).await;
    p.send_env(&fx.node.id, Kind::Data, b"before".to_vec()).await;
    assert_eq!(node.recv_env().await.body, b"before");
    // 每秒一次的 tick 发现过期
    assert_eq!(p.expect_close().await, Some(4004));
    match node.recv_control().await {
        Control::PeerOffline { device } => assert_eq!(device, p.id),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_token_that_disappears_from_the_database_is_noticed_within_a_few_seconds() {
    // 每台设备只留 1 个令牌：签发第二个会把第一个挤掉，用第一个的连接随后被关
    let fx = Fx::with(|c| c.limits.preview_tokens_per_device = 1).await;
    let label = new_label();
    let mut p = fx.preview(&label).await;
    p.expect_silence(Duration::from_millis(200)).await;
    fx.hub.preview_token(&fx.web, &fx.node, &label).await;
    // 库里的复核每 5 秒一次
    match p.recv_within(Duration::from_secs(10)).await {
        Some(Recv::Closed(code)) => assert_eq!(code, Some(4004)),
        other => panic!("expected the connection to be closed with 4004, got {other:?}"),
    }
}

#[tokio::test]
async fn shutting_the_hub_down_tells_previews_to_reconnect_like_everyone_else() {
    let fx = Fx::new().await;
    let mut p = fx.preview(&new_label()).await;
    let Fx { hub, .. } = fx;
    let shutdown = tokio::spawn(async move { hub.handle.shutdown().await });
    assert_eq!(p.expect_close().await, Some(4002));
    shutdown.await.unwrap();
}
