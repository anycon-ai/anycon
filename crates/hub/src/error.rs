//! HTTP 错误：统一成 `{ "error": { "code", "message", "retry_after"? } }`。
//!
//! `code` 是稳定的 snake_case 枚举，客户端据此做 i18n；`message` 是英文调试信息。

use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::store::StoreError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), retry_after: None }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "authentication required")
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", "not allowed")
    }

    pub fn csrf() -> Self {
        Self::new(StatusCode::FORBIDDEN, "csrf", "missing or invalid X-Anycon header")
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "not found")
    }

    /// 限速：429 + `Retry-After`（秒，向上取整）。
    pub fn rate_limited(wait: Duration) -> Self {
        let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "rate_limited",
            message: "too many requests".into(),
            retry_after: Some(secs),
        }
    }

    /// 内部错误：细节只进日志，不回给客户端。
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body { error: Inner { code: self.code, message: &self.message, retry_after: self.retry_after } };
        let mut res = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                res.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        res
    }
}
