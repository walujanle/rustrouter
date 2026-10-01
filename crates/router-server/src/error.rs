//! The JSON error shape every dashboard route returns.
//!
//! Most routes answer with `{ error: <message> }` at a status code, plus a
//! handful of hand-written `{ error, retryAfter, resetHint }` bodies. The
//! frontend reads `.error` as a string, so the field name and the string-ness
//! are the contract.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

/// An error that becomes `{"error": "..."}` with a status code.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    /// Extra top-level fields, e.g. `retryAfter` on a lockout.
    pub extra: Option<Value>,
    /// Response headers, e.g. `Retry-After`.
    pub headers: Vec<(&'static str, String)>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            extra: None,
            headers: Vec::new(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = Some(extra);
        self
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

impl From<router_db::DbError> for ApiError {
    fn from(e: router_db::DbError) -> Self {
        Self::internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::Map::new();
        body.insert("error".into(), json!(self.message));
        if let Some(Value::Object(extra)) = self.extra {
            for (k, v) in extra {
                body.insert(k, v);
            }
        }
        let mut response = (self.status, Json(Value::Object(body))).into_response();
        for (name, value) in self.headers {
            if let Ok(v) = axum::http::HeaderValue::from_str(&value) {
                response.headers_mut().insert(name, v);
            }
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn error_body_is_an_object_with_a_string_error() {
        let response = ApiError::unauthorized("Unauthorized").into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(response).await;
        assert_eq!(body, json!({ "error": "Unauthorized" }));
    }

    #[tokio::test]
    async fn extras_and_headers_survive() {
        let response = ApiError::too_many_requests("locked")
            .with_extra(json!({ "retryAfter": 30, "resetHint": "hint" }))
            .with_header("Retry-After", "30")
            .into_response();
        assert_eq!(response.headers()["retry-after"], "30");
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("locked"));
        assert_eq!(body["retryAfter"], json!(30));
        assert_eq!(body["resetHint"], json!("hint"));
        // `error` first, then the extras, matching the literal the route writes.
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["error", "retryAfter", "resetHint"]);
    }
}
