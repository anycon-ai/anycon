//! JSON 请求体提取器：把 axum 的 `JsonRejection` 统一成 `{ "error": … }` 格式。

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(v)) => Ok(ApiJson(v)),
            Err(JsonRejection::MissingJsonContentType(_)) => Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "expected Content-Type: application/json",
            )),
            Err(JsonRejection::BytesRejection(e)) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", "request body too large"))
            }
            Err(e) => Err(ApiError::bad_request(e.body_text())),
        }
    }
}
