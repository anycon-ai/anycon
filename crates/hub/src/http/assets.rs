//! 嵌入的静态文件，一共两份，分开构建、分开嵌入（`build.rs` 选定目录；没构建过 web 时是占位页）：
//!
//! - Web UI（`web/dist`）：[`serve`] 在主域名上提供。SPA 回退：不匹配 `/api`、`/auth`、`/v1` 的路径都回 `index.html`，
//!   由前端路由处理。例外：`/assets/*` 和带扩展名的路径找不到时返回 404（别把 HTML 当 JS 返回，排查起来很痛苦）。
//! - 预览运行时（`web/dist-preview`）：只通过 [`preview_file`] 给预览域名上的处理器（`http::preview`）取，
//!   [`serve`] 永远碰不到它。否则 Web UI 的 origin 上就能注册一个隧道到别人节点的 service worker
//!   （docs/spec/hub.md § 13.2、ADR-39）。

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};

use crate::error::ApiError;

#[derive(RustEmbed)]
#[folder = "$ANYCON_WEB_DIST_DIR"]
struct Web;

/// 预览运行时。**只有** [`preview_file`] 读它，别的地方不要引用这个类型。
#[derive(RustEmbed)]
#[folder = "$ANYCON_WEB_PREVIEW_DIST_DIR"]
struct PreviewRuntime;

/// 预览运行时里的一个文件（`rel` 是相对 `web/dist-preview` 的路径，如 `index.html`、`sw.js`、`assets/x.js`）。
pub(super) fn preview_file(rel: &str) -> Option<EmbeddedFile> {
    if !is_safe(rel) {
        return None;
    }
    PreviewRuntime::get(rel)
}

/// 这些前缀属于 hub 自己，不走 SPA 回退。
fn is_reserved(path: &str) -> bool {
    ["/api", "/auth", "/v1"].iter().any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

fn is_safe(rel: &str) -> bool {
    !rel.contains('\\') && !rel.contains('\0') && !rel.split('/').any(|seg| seg == "..")
}

fn has_extension(rel: &str) -> bool {
    rel.rsplit('/').next().is_some_and(|name| name.contains('.'))
}

/// `assets/*`（文件名带哈希）可以永久缓存，其余每次向 hub 验证。
pub(super) fn cache_control(rel: &str) -> &'static str {
    if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn respond(rel: &str, file: EmbeddedFile, req_headers: &HeaderMap) -> Response {
    respond_with(file, req_headers, cache_control(rel))
}

/// 带 ETag 的文件响应：`If-None-Match` 命中回 304。`Cache-Control` 由调用方定。
pub(super) fn respond_with(file: EmbeddedFile, req_headers: &HeaderMap, cache: &'static str) -> Response {
    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));

    if req_headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v == etag) {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        let h = res.headers_mut();
        h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex etag"));
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        return res;
    }

    let mime = file.metadata.mimetype().to_string();
    let mut res = (StatusCode::OK, file.data.into_owned()).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime).unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex etag"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}

/// 路由回退处理器。
pub async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if is_reserved(path) {
        return ApiError::not_found().into_response();
    }
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if !is_safe(rel) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(file) = Web::get(rel) {
        return respond(rel, file, &headers);
    }
    if rel.starts_with("assets/") || has_extension(rel) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match Web::get("index.html") {
        Some(file) => respond("index.html", file, &headers),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_prefixes() {
        assert!(is_reserved("/api"));
        assert!(is_reserved("/api/me"));
        assert!(is_reserved("/auth/token"));
        assert!(is_reserved("/v1/ws"));
        assert!(!is_reserved("/apix"));
        assert!(!is_reserved("/pair"));
        assert!(!is_reserved("/"));
    }

    #[test]
    fn path_safety() {
        assert!(is_safe("assets/app.js"));
        assert!(!is_safe("../etc/passwd"));
        assert!(!is_safe("a/../../b"));
        assert!(!is_safe("a\\b"));
    }

    #[test]
    fn embedded_index_exists() {
        // 无论嵌的是 web/dist 还是占位页，都必须有 index.html。
        assert!(Web::get("index.html").is_some());
        assert!(preview_file("index.html").is_some());
        // 预览运行时的 service worker 也必须在（占位目录里放了一个）
        assert!(preview_file("sw.js").is_some());
    }

    #[test]
    fn preview_files_are_separate_from_the_web_ui() {
        // 两份嵌入互不包含对方的文件：预览的 service worker 不在 Web UI 里，Web UI 的 manifest 之类不在预览里
        assert!(Web::get("sw.js").is_none() || Web::get("sw.js").unwrap().data != preview_file("sw.js").unwrap().data);
        assert!(Web::get("index.html").unwrap().data != preview_file("index.html").unwrap().data);
        assert!(preview_file("../Cargo.toml").is_none());
        assert!(preview_file("a/../../b").is_none());
        assert!(preview_file("no-such-file.js").is_none());
    }

    #[test]
    fn cache_rules() {
        assert_eq!(cache_control("assets/app-abc.js"), "public, max-age=31536000, immutable");
        assert_eq!(cache_control("sw.js"), "no-cache");
        assert_eq!(cache_control("index.html"), "no-cache");
    }
}
