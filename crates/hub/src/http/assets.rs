//! 嵌入的 Web UI（`web/dist`，由 `build.rs` 选定目录；没构建过 web 时是一个占位页）。
//!
//! SPA 回退：不匹配 `/api`、`/auth`、`/v1` 的路径都回 `index.html`，由前端路由处理。
//! 例外：`/assets/*` 和带扩展名的路径找不到时返回 404（别把 HTML 当 JS 返回，排查起来很痛苦）。

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};

use crate::error::ApiError;

#[derive(RustEmbed)]
#[folder = "$ANYCON_WEB_DIST_DIR"]
struct Web;

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

fn respond(rel: &str, file: EmbeddedFile, req_headers: &HeaderMap) -> Response {
    let etag = format!("\"{}\"", hex::encode(file.metadata.sha256_hash()));
    let immutable = rel.starts_with("assets/");
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };

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
    }
}
