//! `POST /api/models/test` — probe one model through this server's own `/v1`
//! surface.
//!
//! The probe calls back into the running server over loopback with the caller's
//! API key and the CLI token, so the request goes through the full pipeline
//! (executor, translator, credentials) exactly as a client would.

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::error::ApiError;
use crate::state::AppState;

/// `getInternalHeaders()`: the first active API key as a bearer, plus the CLI
/// token. The probe has no API key of its own, so it borrows one.
pub(crate) async fn internal_headers(state: &AppState) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    if let Ok(keys) = state.read(router_db::repos::api_keys::get_api_keys).await
        && let Some(key) = keys
            .iter()
            .find(|k| k.get("isActive") != Some(&Value::Bool(false)))
            .and_then(|k| k.get("key").and_then(Value::as_str))
    {
        headers.push(("Authorization".to_string(), format!("Bearer {key}")));
    }
    headers.push(("x-9r-cli-token".to_string(), state.cli_token().to_string()));
    headers
}

/// `POST /api/models/test`.
pub async fn test(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid JSON body").into_response();
    };
    let Some(model) = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ApiError::bad_request("Model required").into_response();
    };
    let kind = payload.get("kind").and_then(Value::as_str).unwrap_or("llm");

    let base = format!("http://127.0.0.1:{}", crate::state::resolve_port());
    let headers = internal_headers(&state).await;
    let result = router_sse::services::ping::ping_model_by_kind(model, kind, &base, &headers).await;
    Json(result).into_response()
}
