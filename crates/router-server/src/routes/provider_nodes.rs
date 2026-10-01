//! `provider-nodes/*`: the provider-node CRUD and validate routes.
//!
//! A node is a self-hosted OpenAI-/Anthropic-compatible endpoint or a custom
//! embedding endpoint. Its `id` is the provider id a connection carries, so a
//! connection's provider id is literally the node id.

use axum::Json;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use std::net::SocketAddr;
use std::time::Duration;

use router_sse::executors::http::tls_builder;
use router_sse::modalities::ssrf;
use router_sse::providers::ui::{
    ANTHROPIC_COMPATIBLE_PREFIX, CUSTOM_EMBEDDING_PREFIX, OPENAI_COMPATIBLE_PREFIX,
    is_anthropic_compatible_provider, is_openai_compatible_provider,
};

use crate::error::ApiError;
use crate::state::AppState;

const OPENAI_COMPATIBLE_DEFAULT_BASE: &str = "https://api.openai.com/v1";
const ANTHROPIC_COMPATIBLE_DEFAULT_BASE: &str = "https://api.anthropic.com/v1";
const CUSTOM_EMBEDDING_DEFAULT_BASE: &str = "https://api.openai.com/v1";

/// `GET /api/provider-nodes`.
pub async fn list(State(state): State<AppState>) -> Response {
    match state
        .read(|conn| router_db::repos::nodes::get_provider_nodes(conn, None))
        .await
    {
        Ok(nodes) => Json(json!({ "nodes": nodes })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch provider nodes").into_response(),
    }
}

/// Strip a trailing `/`, then a trailing `/embeddings`.
pub(crate) fn sanitize_embedding_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/embeddings")
        .unwrap_or(trimmed)
        .to_string()
}

/// Strip a trailing `/`, then a trailing `/messages`.
pub(crate) fn sanitize_anthropic_base(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/messages")
        .unwrap_or(trimmed)
        .to_string()
}

/// `POST /api/provider-nodes`.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to create provider node").into_response();
    };
    let str_at = |k: &str| payload.get(k).and_then(Value::as_str);
    let name = str_at("name").unwrap_or("").trim();
    if name.is_empty() {
        return ApiError::bad_request("Name is required").into_response();
    }
    let prefix = str_at("prefix").unwrap_or("").trim();
    if prefix.is_empty() {
        return ApiError::bad_request("Prefix is required").into_response();
    }

    let node_type = str_at("type")
        .filter(|s| !s.is_empty())
        .unwrap_or("openai-compatible");
    let api_type = str_at("apiType");
    let base_url = str_at("baseUrl");

    let (id, node_type, base_url, api_type_value) = match node_type {
        "openai-compatible" => {
            let Some(api_type) = api_type.filter(|t| *t == "chat" || *t == "responses") else {
                return ApiError::bad_request("Invalid OpenAI compatible API type").into_response();
            };
            (
                format!(
                    "{OPENAI_COMPATIBLE_PREFIX}{api_type}-{}",
                    uuid::Uuid::new_v4()
                ),
                "openai-compatible",
                base_url
                    .unwrap_or(OPENAI_COMPATIBLE_DEFAULT_BASE)
                    .trim()
                    .to_string(),
                Value::String(api_type.to_string()),
            )
        }
        "custom-embedding" => (
            format!("{CUSTOM_EMBEDDING_PREFIX}{}", uuid::Uuid::new_v4()),
            "custom-embedding",
            sanitize_embedding_base(base_url.unwrap_or(CUSTOM_EMBEDDING_DEFAULT_BASE)),
            Value::Null,
        ),
        "anthropic-compatible" => (
            format!("{ANTHROPIC_COMPATIBLE_PREFIX}{}", uuid::Uuid::new_v4()),
            "anthropic-compatible",
            sanitize_anthropic_base(base_url.unwrap_or(ANTHROPIC_COMPATIBLE_DEFAULT_BASE)),
            Value::Null,
        ),
        _ => return ApiError::bad_request("Invalid provider node type").into_response(),
    };

    let mut data = Map::new();
    data.insert("id".into(), json!(id));
    data.insert("type".into(), json!(node_type));
    data.insert("prefix".into(), json!(prefix));
    if !api_type_value.is_null() {
        data.insert("apiType".into(), api_type_value);
    }
    data.insert("baseUrl".into(), json!(base_url));
    data.insert("name".into(), json!(name));
    let data = Value::Object(data);

    match state
        .write(move |tx| router_db::repos::nodes::create_provider_node(tx, &data))
        .await
    {
        Ok(node) => (StatusCode::CREATED, Json(json!({ "node": node }))).into_response(),
        Err(_) => ApiError::internal("Failed to create provider node").into_response(),
    }
}

/// `PUT /api/provider-nodes/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update provider node").into_response();
    };
    let node = match state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::nodes::get_provider_node_by_id(conn, &id)
        })
        .await
    {
        Ok(Some(node)) => node,
        Ok(None) => return ApiError::not_found("Provider node not found").into_response(),
        Err(_) => return ApiError::internal("Failed to update provider node").into_response(),
    };

    let str_at = |k: &str| payload.get(k).and_then(Value::as_str);
    let name = str_at("name").unwrap_or("").trim();
    if name.is_empty() {
        return ApiError::bad_request("Name is required").into_response();
    }
    let prefix = str_at("prefix").unwrap_or("").trim();
    if prefix.is_empty() {
        return ApiError::bad_request("Prefix is required").into_response();
    }
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    let api_type = str_at("apiType");
    if node_type == "openai-compatible"
        && !api_type.is_some_and(|t| t == "chat" || t == "responses")
    {
        return ApiError::bad_request("Invalid OpenAI compatible API type").into_response();
    }
    let base_url = str_at("baseUrl").unwrap_or("").trim();
    if base_url.is_empty() {
        return ApiError::bad_request("Base URL is required").into_response();
    }

    let sanitized_base = match node_type {
        "anthropic-compatible" => sanitize_anthropic_base(base_url),
        "custom-embedding" => sanitize_embedding_base(base_url),
        _ => base_url.to_string(),
    };

    let mut updates = Map::new();
    updates.insert("name".into(), json!(name));
    updates.insert("prefix".into(), json!(prefix));
    updates.insert("baseUrl".into(), json!(sanitized_base));
    if node_type == "openai-compatible" {
        updates.insert("apiType".into(), json!(api_type.unwrap()));
    }
    let updates = Value::Object(updates);

    // Propagate the node's identity into every connection that points at it, so
    // a rename or base-URL fix reaches existing rows.
    let (prefix_owned, sanitized_owned, api_type_owned) = (
        prefix.to_string(),
        sanitized_base.clone(),
        api_type.map(str::to_string),
    );
    let node_type_owned = node_type.to_string();
    let id_owned = id.clone();
    let result = state
        .write(move |tx| {
            let updated = router_db::repos::nodes::update_provider_node(tx, &id_owned, &updates)?;
            let Some(updated) = updated else {
                return Ok(None);
            };
            let node_name = updated
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let connections =
                router_db::repos::connections::get_provider_connections(tx, Some(&id_owned), None)?;
            for connection in connections {
                let conn_id = connection.get("id").and_then(Value::as_str).unwrap_or("");
                let mut psd = connection
                    .get("providerSpecificData")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                psd.insert("prefix".into(), json!(prefix_owned));
                if node_type_owned == "openai-compatible" {
                    psd.insert("apiType".into(), json!(api_type_owned));
                }
                psd.insert("baseUrl".into(), json!(sanitized_owned));
                psd.insert("nodeName".into(), json!(node_name));
                let mut patch = Map::new();
                patch.insert("providerSpecificData".into(), Value::Object(psd));
                router_db::repos::connections::update_provider_connection(
                    tx,
                    conn_id,
                    &Value::Object(patch),
                )?;
            }
            Ok(Some(updated))
        })
        .await;

    match result {
        Ok(Some(node)) => Json(json!({ "node": node })).into_response(),
        Ok(None) => ApiError::not_found("Provider node not found").into_response(),
        Err(_) => ApiError::internal("Failed to update provider node").into_response(),
    }
}

/// `DELETE /api/provider-nodes/{id}` — deletes the node and its connections.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let result = state
        .write(move |tx| {
            if router_db::repos::nodes::get_provider_node_by_id(tx, &id)?.is_none() {
                return Ok(false);
            }
            router_db::repos::connections::delete_provider_connections_by_provider(tx, &id)?;
            router_db::repos::nodes::delete_provider_node(tx, &id)?;
            Ok(true)
        })
        .await;
    match result {
        Ok(true) => Json(json!({ "success": true })).into_response(),
        Ok(false) => ApiError::not_found("Provider node not found").into_response(),
        Err(_) => ApiError::internal("Failed to delete provider node").into_response(),
    }
}

/// `isValidUrl(url)`: the string parses as an absolute URL.
pub(crate) fn is_valid_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok()
}

/// `getErrorMessage(err)` — a network error mapped to user-facing text.
pub(crate) fn get_error_message(err: &reqwest::Error) -> String {
    let text = err.to_string();
    if err.is_timeout() || text.contains("timed out") || text.contains("timeout") {
        return "Request timeout (>10s) - provider node not responding".to_string();
    }
    if err.is_connect() {
        return "Connection refused - provider node offline or unreachable".to_string();
    }
    if text.contains("certificate") || text.contains("cert") {
        return "SSL certificate verification failed".to_string();
    }
    if text.contains("dns")
        || text.contains("name or service not known")
        || text.contains("failed to lookup")
    {
        return "DNS lookup failed - invalid domain or network issue".to_string();
    }
    "Network connection failed - check URL and network connectivity".to_string()
}

pub(crate) fn models_error_message(status: u16) -> String {
    match status {
        401 | 403 => "API key unauthorized".to_string(),
        404 => "/models endpoint not found - try chat validation with model ID".to_string(),
        s if s >= 500 => "Server error - try again later".to_string(),
        s => format!("Unexpected response ({s})"),
    }
}

pub(crate) fn chat_error_message(status: u16) -> String {
    match status {
        401 | 403 => "API key unauthorized".to_string(),
        400 => "Invalid model or bad request".to_string(),
        404 => "Chat endpoint not found".to_string(),
        s if s >= 500 => "Server error - try again later".to_string(),
        s => format!("Chat request failed ({s})"),
    }
}

/// A 10-second probe client.
pub(crate) fn probe_client() -> Option<reqwest::Client> {
    tls_builder().timeout(Duration::from_secs(10)).build().ok()
}

/// `POST /api/provider-nodes/validate`.
pub async fn validate(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Validation failed").into_response();
    };
    let base_url = payload
        .get("baseUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let api_key = payload.get("apiKey").and_then(Value::as_str).unwrap_or("");
    if base_url.is_empty() || api_key.is_empty() {
        return ApiError::bad_request("Base URL and API key required").into_response();
    }
    if !is_valid_url(base_url) {
        return ApiError::bad_request("Invalid URL format").into_response();
    }

    // SSRF guard for remote callers; a local caller keeps self-hosted nodes.
    // Layer 2, not just the literal check: a hostname that resolves to a
    // private address must be refused too.
    let facts = crate::middleware::facts_from(&headers, peer);
    if !facts.is_local() && ssrf::assert_public_url_resolved(base_url).await.is_err() {
        return ApiError::bad_request("URL not allowed").into_response();
    }

    let node_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let model_id = payload
        .get("modelId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let Some(client) = probe_client() else {
        return ApiError::internal("Validation failed").into_response();
    };

    if node_type == "custom-embedding" {
        let normalized = base_url.trim_end_matches('/');
        if model_id.is_empty() {
            return Json(
                json!({ "valid": false, "error": "Model ID required for embedding validation" }),
            )
            .into_response();
        }
        let res = client
            .post(format!("{normalized}/embeddings"))
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&json!({ "model": model_id, "input": "ping" }))
            .send()
            .await;
        return match res {
            Ok(res) if res.status().is_success() => {
                let data: Option<Value> = res.json().await.ok();
                let dims = data
                    .as_ref()
                    .and_then(|d| d.pointer("/data/0/embedding"))
                    .and_then(Value::as_array)
                    .map(|a| a.len());
                Json(json!({ "valid": true, "method": "embeddings", "dimensions": dims }))
                    .into_response()
            }
            Ok(res) if matches!(res.status().as_u16(), 401 | 403) => {
                Json(json!({ "valid": false, "error": "API key unauthorized" })).into_response()
            }
            Ok(res) => {
                let status = res.status().as_u16();
                let body_text = res.text().await.unwrap_or_default();
                let suffix = if body_text.is_empty() {
                    String::new()
                } else {
                    format!(": {}", body_text.chars().take(200).collect::<String>())
                };
                Json(json!({
                    "valid": false,
                    "error": format!("Embeddings request failed ({status}){suffix}"),
                    "method": "embeddings",
                }))
                .into_response()
            }
            Err(e) => {
                Json(json!({ "valid": false, "error": get_error_message(&e) })).into_response()
            }
        };
    }

    if node_type == "anthropic-compatible" {
        let normalized = sanitize_anthropic_base(base_url);
        let res = client
            .get(format!("{normalized}/models"))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await;
        let res = match res {
            Ok(res) => res,
            Err(e) => {
                return Json(json!({ "valid": false, "error": get_error_message(&e) }))
                    .into_response();
            }
        };
        if res.status().is_success() {
            return Json(json!({ "valid": true })).into_response();
        }
        if matches!(res.status().as_u16(), 401 | 403) {
            return Json(json!({ "valid": false, "error": "API key unauthorized" }))
                .into_response();
        }
        if !model_id.is_empty() {
            let chat = client
                .post(format!("{normalized}/chat/completions"))
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .json(&json!({
                    "model": model_id,
                    "messages": [{ "role": "user", "content": "ping" }],
                    "max_tokens": 1,
                }))
                .send()
                .await;
            return match chat {
                Ok(chat) if chat.status().is_success() => {
                    Json(json!({ "valid": true, "method": "chat" })).into_response()
                }
                Ok(chat) => Json(json!({
                    "valid": false,
                    "error": chat_error_message(chat.status().as_u16()),
                    "method": "chat",
                }))
                .into_response(),
                Err(e) => {
                    Json(json!({ "valid": false, "error": get_error_message(&e) })).into_response()
                }
            };
        }
        return Json(
            json!({ "valid": false, "error": models_error_message(res.status().as_u16()) }),
        )
        .into_response();
    }

    // OpenAI-compatible (default).
    let normalized = base_url.trim_end_matches('/');
    let res = client
        .get(format!("{normalized}/models"))
        .header("Authorization", format!("Bearer {api_key}"))
        .send()
        .await;
    let res = match res {
        Ok(res) => res,
        Err(e) => {
            return Json(json!({ "valid": false, "error": get_error_message(&e) })).into_response();
        }
    };
    if res.status().is_success() {
        return Json(json!({ "valid": true })).into_response();
    }
    if matches!(res.status().as_u16(), 401 | 403) {
        return Json(json!({ "valid": false, "error": "API key unauthorized" })).into_response();
    }
    if !model_id.is_empty() {
        let chat = client
            .post(format!("{normalized}/chat/completions"))
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&json!({
                "model": model_id,
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
            }))
            .send()
            .await;
        return match chat {
            Ok(chat) if chat.status().is_success() => {
                Json(json!({ "valid": true, "method": "chat" })).into_response()
            }
            Ok(chat) => Json(json!({
                "valid": false,
                "error": chat_error_message(chat.status().as_u16()),
                "method": "chat",
            }))
            .into_response(),
            Err(e) => {
                Json(json!({ "valid": false, "error": get_error_message(&e) })).into_response()
            }
        };
    }
    Json(json!({ "valid": false, "error": models_error_message(res.status().as_u16()) }))
        .into_response()
}

/// The prefixes a provider id can carry, re-exported so `providers.rs` shares
/// the predicates.
pub fn is_compatible_provider(id: &str) -> bool {
    is_openai_compatible_provider(id) || is_anthropic_compatible_provider(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_base_strips_trailing_slash_and_endpoint() {
        assert_eq!(sanitize_embedding_base("https://x/v1/"), "https://x/v1");
        assert_eq!(
            sanitize_embedding_base("https://x/v1/embeddings"),
            "https://x/v1"
        );
        assert_eq!(
            sanitize_embedding_base("https://x/v1/embeddings/"),
            "https://x/v1"
        );
    }

    #[test]
    fn anthropic_base_strips_trailing_slash_and_endpoint() {
        assert_eq!(sanitize_anthropic_base("https://x/v1/"), "https://x/v1");
        assert_eq!(
            sanitize_anthropic_base("https://x/v1/messages"),
            "https://x/v1"
        );
    }

    #[test]
    fn compatible_predicate_covers_both_prefixes() {
        assert!(is_compatible_provider("openai-compatible-chat-abc"));
        assert!(is_compatible_provider("anthropic-compatible-abc"));
        assert!(!is_compatible_provider("claude"));
    }
}
