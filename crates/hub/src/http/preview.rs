//! 预览域名上的 HTTP 表面（ADR-39，docs/spec/hub.md § 13.2）。
//!
//! `Host` 是 `<label>.<预览域名>[:端口]` 的请求不走主站的路由，由这里的 [`dispatch`] 在最外层接走；其余请求照旧。
//! 没配 `ANYCON_PREVIEW_URL` 时这一层根本不装，现有行为不受任何影响。
//!
//! | 路径 | 说明 |
//! |---|---|
//! | `GET /v1/ws` | 预览连接（`ws::preview_ws_upgrade`） |
//! | `GET /__anycon/config.json` | `{ label, hub_origin, protocol_version }`，`no-store` |
//! | `GET /__anycon/enter` | 引导页（预览运行时的 `index.html`） |
//! | `GET /__anycon/sw.js` | 预览的 service worker，`Service-Worker-Allowed: /`、`no-cache` |
//! | `GET /__anycon/<其他文件>` | 预览运行时里的文件；`assets/*` 是 immutable，其余 `no-cache`；不存在 `404` |
//! | 其他 `GET` / `HEAD` | 引导页，`200`，`no-store`（service worker 还没接管时页面请求落到这里） |
//! | 其他方法 | `405` |
//!
//! 预览域名上没有 Web UI、`/api`、`/auth`；预览运行时的文件也只在这里提供（`assets::preview_file`），主域名取不到。
//! 响应头：HTML 带预览的 CSP；所有响应带 `X-Content-Type-Options`、`Referrer-Policy`；**不带** `X-Frame-Options` 和 COOP
//! （要能被 Web UI 嵌入，嵌入方由 CSP 的 `frame-ancestors` 限定）——所以必须绕开主站的 `security_headers` 层。
//!
//! 日志纪律（spec § 10）：请求日志只记 `(preview)`，不记路径、不记 label。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anycon_protocol::PROTOCOL_VERSION;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::app::AppState;
use crate::config::{HostMatch, PreviewUrl};
use crate::ws;

use super::assets;

/// 预览引导页的 CSP 的前半段，后面接 `frame-ancestors` 的来源（hub 的公网 origin）。
const CSP_BEFORE_ANCESTORS: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; \
img-src 'self' data:; style-src 'self' 'unsafe-inline'; worker-src 'self'; frame-ancestors ";
const CSP_AFTER_ANCESTORS: &str = "; base-uri 'none'; form-action 'none'";

/// 预览引导页的内容安全策略：只允许被 hub 的公网 origin 嵌入。dev app 的页面由 service worker 合成，头部另算
/// （docs/spec/web.md § 13.2）。
pub fn preview_csp(hub_origin: &str) -> String {
    format!("{CSP_BEFORE_ANCESTORS}{hub_origin}{CSP_AFTER_ANCESTORS}")
}

/// 预览域名上的请求处理所需的一切，启动时算好。
pub struct PreviewSurface {
    state: Arc<AppState>,
    url: PreviewUrl,
    csp: HeaderValue,
    hub_origin: String,
}

impl PreviewSurface {
    /// `None` = 没配预览。
    pub fn new(state: &Arc<AppState>) -> Option<Arc<Self>> {
        let url = state.config.preview.clone()?;
        let hub_origin = state.config.public_url.origin().ascii_serialization();
        let csp = HeaderValue::from_str(&preview_csp(&hub_origin)).expect("CSP is ascii");
        Some(Arc::new(Self { state: state.clone(), url, csp, hub_origin }))
    }
}

/// 最外层的按 `Host` 分流：预览域名上的请求在这里处理，其余交给主站的路由。
pub async fn dispatch(State(surface): State<Arc<PreviewSurface>>, req: Request, next: Next) -> Response {
    let Some(host) = request_host(&req) else { return next.run(req).await };
    match surface.url.match_host(&host) {
        HostMatch::Other => next.run(req).await,
        HostMatch::InvalidLabel => surface.finish(not_found(), false),
        HostMatch::Label(label) => surface.handle(label, req).await,
    }
}

/// 请求的主机：`Host` 头；HTTP/2 没有 `Host`，看 URI 里的 authority。
fn request_host(req: &Request) -> Option<String> {
    if let Some(h) = req.headers().get(header::HOST) {
        return h.to_str().ok().map(str::to_string);
    }
    req.uri().authority().map(|a| a.as_str().to_string())
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "not found\n").into_response()
}

fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "GET, HEAD"), (header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "method not allowed\n",
    )
        .into_response()
}

#[derive(Serialize)]
struct RuntimeConfig<'a> {
    label: &'a str,
    hub_origin: &'a str,
    protocol_version: u32,
}

impl PreviewSurface {
    async fn handle(&self, label: String, req: Request) -> Response {
        let start = Instant::now();
        let method = req.method().clone();
        // 304 没有 Content-Type：按路径判断是不是在重新验证一个 HTML 页面。
        let last_segment = req.uri().path().rsplit('/').next().unwrap_or("");
        let html_path = !last_segment.contains('.') || last_segment.ends_with(".html");
        let res = self.route(label, req).await;
        let res = self.finish(res, html_path);
        tracing::debug!(%method, route = "(preview)", status = res.status().as_u16(), elapsed_ms = start.elapsed().as_millis() as u64, "http");
        res
    }

    async fn route(&self, label: String, req: Request) -> Response {
        let path = req.uri().path().to_string();
        let read_only = matches!(*req.method(), Method::GET | Method::HEAD);
        match path.as_str() {
            "/v1/ws" => {
                if *req.method() != Method::GET {
                    return method_not_allowed();
                }
                self.upgrade(label, req).await
            }
            _ if !read_only => method_not_allowed(),
            "/__anycon/config.json" => self.config_json(&label),
            "/__anycon/enter" => self.file("index.html", req.headers(), "no-cache"),
            "/__anycon/sw.js" => {
                let mut res = self.file("sw.js", req.headers(), "no-cache");
                res.headers_mut()
                    .insert(HeaderName::from_static("service-worker-allowed"), HeaderValue::from_static("/"));
                res
            }
            // `/__anycon/` 本身不是文件：当作引导页。
            "/__anycon/" => self.guide(),
            p => match p.strip_prefix("/__anycon/") {
                Some(rel) => self.file(rel, req.headers(), assets::cache_control(rel)),
                None => self.guide(),
            },
        }
    }

    /// `GET /v1/ws`：预览连接。`Origin` 必须正好是这个预览 origin。
    async fn upgrade(&self, label: String, req: Request) -> Response {
        let (mut parts, _body) = req.into_parts();
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0)
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(u) => u,
            Err(rejection) => return rejection.into_response(),
        };
        let expected_origin = self.url.origin_for(&label);
        ws::preview_ws_upgrade(self.state.clone(), peer, parts.headers, upgrade, label, expected_origin).await
    }

    fn config_json(&self, label: &str) -> Response {
        let body = RuntimeConfig { label, hub_origin: &self.hub_origin, protocol_version: PROTOCOL_VERSION };
        let body = serde_json::to_string(&body).expect("static shape serializes");
        (StatusCode::OK, [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-store")], body)
            .into_response()
    }

    /// 预览运行时里的一个文件。找不到 `404`。
    fn file(&self, rel: &str, req_headers: &HeaderMap, cache: &'static str) -> Response {
        match assets::preview_file(rel) {
            Some(file) => assets::respond_with(file, req_headers, cache),
            None => not_found(),
        }
    }

    /// 引导页，`no-store`：这个 URL 对应的是 dev app 的某个页面，不能被缓存成引导页。
    fn guide(&self) -> Response {
        let Some(file) = assets::preview_file("index.html") else { return not_found() };
        let mime = file.metadata.mimetype().to_string();
        let mut res = (StatusCode::OK, file.data.into_owned()).into_response();
        let h = res.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("text/html")));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res
    }

    /// 所有预览域名上的响应都带的头；HTML 另带 CSP。不带 `X-Frame-Options` 和 COOP。
    fn finish(&self, mut res: Response, html_path: bool) -> Response {
        let is_html = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim_start().to_ascii_lowercase().starts_with("text/html"))
            || (res.status() == StatusCode::NOT_MODIFIED && html_path);
        let h = res.headers_mut();
        h.insert(HeaderName::from_static("x-content-type-options"), HeaderValue::from_static("nosniff"));
        h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
        if is_html {
            h.insert(header::CONTENT_SECURITY_POLICY, self.csp.clone());
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_only_lets_the_hub_origin_embed_the_page() {
        let csp = preview_csp("https://hub.example.com");
        let directives: Vec<&str> = csp.split("; ").collect();
        assert_eq!(
            directives,
            [
                "default-src 'self'",
                "script-src 'self' 'wasm-unsafe-eval'",
                "connect-src 'self'",
                "img-src 'self' data:",
                "style-src 'self' 'unsafe-inline'",
                "worker-src 'self'",
                "frame-ancestors https://hub.example.com",
                "base-uri 'none'",
                "form-action 'none'",
            ]
        );
        // 有端口的 origin 原样放进去
        assert!(preview_csp("http://127.0.0.1:8080").contains("frame-ancestors http://127.0.0.1:8080;"));
    }
}
