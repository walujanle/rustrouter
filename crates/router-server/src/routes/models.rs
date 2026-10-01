//! `models/*`: the model list, alias, disabled-model, custom-model and
//! availability routes.
//!
//! `models/test` and `models/catalog-sync` live in `models_test.rs` and
//! `catalog_sync.rs`: both need outbound HTTP or the sync scheduler, not just a
//! repo read.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use router_sse::catalog::get_capabilities_for_model;
use router_sse::providers::registry::registry;

use crate::error::ApiError;
use crate::state::AppState;

/// The `caps` projection `/api/models` puts on every row.
fn caps_json(provider: &str, model: &str) -> Value {
    let c = get_capabilities_for_model(Some(provider), model);
    json!({
        "vision": c.vision,
        "search": c.search,
        "reasoning": c.reasoning,
        "contextWindow": c.context_window,
        "maxOutput": c.max_output,
    })
}

/// `GET /api/models`.
pub async fn list(State(state): State<AppState>) -> Response {
    let aliases = state
        .read(router_db::repos::aliases::get_model_aliases)
        .await
        .unwrap_or_else(|_| json!({}));
    let disabled = state
        .read(router_db::repos::aliases::get_disabled_models)
        .await
        .unwrap_or_else(|_| json!({}));
    let custom = state
        .read(router_db::repos::aliases::get_custom_models)
        .await
        .unwrap_or_default();

    let alias_of = |full: &str| aliases.get(full).cloned().unwrap_or(Value::Null);
    let disabled_for = |alias: &str, provider: &str| -> Vec<String> {
        for key in [alias, provider] {
            if let Some(Value::Array(list)) = disabled.get(key) {
                return list
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
            }
        }
        Vec::new()
    };

    let mut models: Vec<Value> = Vec::new();
    for (key, list) in registry().models_iter() {
        let disabled_ids = disabled_for(key, key);
        for m in list {
            if disabled_ids.iter().any(|d| d == &m.id) {
                continue;
            }
            let full_model = format!("{key}/{}", m.id);
            let name = m.name.clone().unwrap_or_else(|| m.id.clone());
            models.push(json!({
                "provider": key,
                "model": m.id,
                "name": name,
                "fullModel": full_model,
                "routedModel": full_model,
                "alias": alias_of(&full_model),
                "caps": caps_json(key, &m.id),
            }));
        }
    }

    // Custom models ride along; their stored caps override the name heuristic.
    let seen: std::collections::HashSet<String> = models
        .iter()
        .filter_map(|m| {
            m.get("fullModel")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    for m in &custom {
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue;
        };
        let kind = m
            .get("kind")
            .or_else(|| m.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("llm");
        if kind != "llm" {
            continue;
        }
        let provider = m.get("providerAlias").and_then(Value::as_str).unwrap_or("");
        let full_model = format!("{provider}/{id}");
        if seen.contains(&full_model) {
            continue;
        }
        let mut caps = caps_json(provider, id);
        if let Some(Value::Object(overrides)) = m.get("caps")
            && let Value::Object(base) = &mut caps
        {
            for (k, v) in overrides {
                base.insert(k.clone(), v.clone());
            }
        }
        models.push(json!({
            "provider": provider,
            "model": id,
            "name": m.get("name").and_then(Value::as_str).unwrap_or(id),
            "fullModel": full_model,
            "routedModel": full_model,
            "alias": alias_of(&full_model),
            "caps": caps,
        }));
    }

    Json(json!({ "models": models })).into_response()
}

/// `PUT /api/models`. The parameter order is inverted relative to
/// [`set_alias`]: here the stored key is the model and the stored value is the
/// alias.
pub async fn update_alias(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update alias").into_response();
    };
    let (Some(model), Some(alias)) = (
        payload.get("model").and_then(Value::as_str),
        payload.get("alias").and_then(Value::as_str),
    ) else {
        return ApiError::bad_request("Model and alias required").into_response();
    };

    let aliases = state
        .read(router_db::repos::aliases::get_model_aliases)
        .await
        .unwrap_or_else(|_| json!({}));
    let in_use = aliases
        .as_object()
        .map(|m| {
            m.iter()
                .any(|(k, v)| v.as_str() == Some(alias) && k != model)
        })
        .unwrap_or(false);
    if in_use {
        return ApiError::bad_request("Alias already in use").into_response();
    }

    let (model, alias) = (model.to_string(), alias.to_string());
    let result = state
        .write({
            let model = model.clone();
            let alias = alias.clone();
            move |tx| {
                router_db::repos::aliases::set_model_alias(
                    tx,
                    &model,
                    &Value::String(alias.clone()),
                )
            }
        })
        .await;
    match result {
        Ok(()) => Json(json!({ "success": true, "model": model, "alias": alias })).into_response(),
        Err(_) => ApiError::internal("Failed to update alias").into_response(),
    }
}

/// `GET /api/models/alias`.
pub async fn list_aliases(State(state): State<AppState>) -> Response {
    match state
        .read(router_db::repos::aliases::get_model_aliases)
        .await
    {
        Ok(aliases) => Json(json!({ "aliases": aliases })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch aliases").into_response(),
    }
}

/// `PUT /api/models/alias` — `setModelAlias(alias, model)`, the sane order.
pub async fn set_alias(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update alias").into_response();
    };
    let (Some(model), Some(alias)) = (
        payload.get("model").and_then(Value::as_str),
        payload.get("alias").and_then(Value::as_str),
    ) else {
        return ApiError::bad_request("Model and alias required").into_response();
    };

    let (model, alias) = (model.to_string(), alias.to_string());
    let result = state
        .write({
            let model = model.clone();
            let alias = alias.clone();
            move |tx| {
                router_db::repos::aliases::set_model_alias(
                    tx,
                    &alias,
                    &Value::String(model.clone()),
                )
            }
        })
        .await;
    match result {
        Ok(()) => Json(json!({ "success": true, "model": model, "alias": alias })).into_response(),
        Err(_) => ApiError::internal("Failed to update alias").into_response(),
    }
}

/// `DELETE /api/models/alias?alias=`.
pub async fn delete_alias(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(alias) = params.get("alias").cloned() else {
        return ApiError::bad_request("Alias required").into_response();
    };
    match state
        .write(move |tx| router_db::repos::aliases::delete_model_alias(tx, &alias))
        .await
    {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(_) => ApiError::internal("Failed to delete alias").into_response(),
    }
}

/// `GET /api/models/disabled`.
pub async fn disabled_get(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let all = state
        .read(router_db::repos::aliases::get_disabled_models)
        .await;
    let Ok(all) = all else {
        return ApiError::internal("Failed to fetch disabled models").into_response();
    };
    match params.get("providerAlias") {
        Some(alias) => {
            let ids = all.get(alias).cloned().unwrap_or(json!([]));
            Json(json!({ "ids": ids })).into_response()
        }
        None => Json(json!({ "disabled": all })).into_response(),
    }
}

/// `POST /api/models/disabled`.
pub async fn disabled_post(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to disable models").into_response();
    };
    let (Some(alias), Some(ids)) = (
        payload.get("providerAlias").and_then(Value::as_str),
        payload.get("ids").and_then(Value::as_array),
    ) else {
        return ApiError::bad_request("providerAlias and ids[] required").into_response();
    };
    let ids: Vec<String> = ids
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let alias = alias.to_string();
    match state
        .write(move |tx| router_db::repos::aliases::disable_models(tx, &alias, &ids))
        .await
    {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(_) => ApiError::internal("Failed to disable models").into_response(),
    }
}

/// `DELETE /api/models/disabled?providerAlias=[&id=]`.
pub async fn disabled_delete(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(alias) = params.get("providerAlias").cloned() else {
        return ApiError::bad_request("providerAlias required").into_response();
    };
    let ids: Vec<String> = params.get("id").cloned().into_iter().collect();
    match state
        .write(move |tx| router_db::repos::aliases::enable_models(tx, &alias, &ids))
        .await
    {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(_) => ApiError::internal("Failed to enable models").into_response(),
    }
}

/// `CAPACITY_META` keys: the capability flags a custom model may carry.
const CAPACITY_META: [&str; 2] = ["vision", "reasoning"];

/// `sanitizeCaps(caps)`: keep only the whitelisted boolean keys.
fn sanitize_caps(caps: Option<&Value>) -> Option<Value> {
    let Value::Object(caps) = caps? else {
        return None;
    };
    let mut clean = Map::new();
    for key in CAPACITY_META {
        if let Some(v) = caps.get(key)
            && let Some(b) = v.as_bool()
        {
            clean.insert(key.to_string(), Value::Bool(b));
        }
    }
    (!clean.is_empty()).then_some(Value::Object(clean))
}

/// `GET /api/models/custom`.
pub async fn custom_list(State(state): State<AppState>) -> Response {
    match state
        .read(router_db::repos::aliases::get_custom_models)
        .await
    {
        Ok(models) => Json(json!({ "models": models })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch custom models").into_response(),
    }
}

/// `POST /api/models/custom`.
pub async fn custom_add(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to add custom model").into_response();
    };
    let (Some(provider_alias), Some(id)) = (
        payload.get("providerAlias").and_then(Value::as_str),
        payload.get("id").and_then(Value::as_str),
    ) else {
        return ApiError::bad_request("providerAlias and id required").into_response();
    };
    let model_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("llm")
        .to_string();
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let caps = sanitize_caps(payload.get("caps"));
    let provider_alias = provider_alias.to_string();
    let id = id.to_string();

    match state
        .write(move |tx| {
            router_db::repos::aliases::add_custom_model(
                tx,
                &provider_alias,
                &id,
                Some(&model_type),
                name.as_deref(),
                caps.as_ref(),
                None,
            )
        })
        .await
    {
        Ok(added) => Json(json!({ "success": true, "added": added })).into_response(),
        Err(_) => ApiError::internal("Failed to add custom model").into_response(),
    }
}

/// `DELETE /api/models/custom?providerAlias=&id=[&type=]`.
pub async fn custom_delete(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (Some(provider_alias), Some(id)) = (
        params.get("providerAlias").cloned(),
        params.get("id").cloned(),
    ) else {
        return ApiError::bad_request("providerAlias and id required").into_response();
    };
    let model_type = params
        .get("type")
        .cloned()
        .unwrap_or_else(|| "llm".to_string());
    match state
        .write(move |tx| {
            router_db::repos::aliases::delete_custom_model(
                tx,
                &provider_alias,
                &id,
                Some(&model_type),
            )
        })
        .await
    {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(_) => ApiError::internal("Failed to delete custom model").into_response(),
    }
}

/// `getActiveModelLocks(connection)`: unexpired `modelLock_*` values.
fn active_model_locks(connection: &Value, now_ms: i64) -> Vec<(String, Value)> {
    let Some(obj) = connection.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .filter(|(k, v)| {
            k.starts_with(router_db::repos::connections::MODEL_LOCK_PREFIX) && !v.is_null()
        })
        .filter_map(|(k, v)| {
            let model = k
                .strip_prefix(router_db::repos::connections::MODEL_LOCK_PREFIX)
                .filter(|s| !s.is_empty())
                .unwrap_or("__all");
            let until_ms = v
                .as_str()
                .and_then(router_db::time::parse_iso)
                .map(|d| d.timestamp_millis())?;
            (until_ms > now_ms).then(|| (model.to_string(), v.clone()))
        })
        .collect()
}

/// `GET /api/models/availability`.
pub async fn availability_get(State(state): State<AppState>) -> Response {
    let connections = state
        .read(|conn| router_db::repos::connections::get_provider_connections(conn, None, None))
        .await;
    let Ok(connections) = connections else {
        return ApiError::internal("Failed to fetch model availability").into_response();
    };

    let now_ms = router_db::time::now_ms();
    let mut models: Vec<Value> = Vec::new();
    for connection in &connections {
        let locks = active_model_locks(connection, now_ms);
        let provider = connection.get("provider").cloned().unwrap_or(Value::Null);
        let connection_id = connection.get("id").cloned().unwrap_or(Value::Null);
        let connection_name = connection
            .get("name")
            .or_else(|| connection.get("email"))
            .or_else(|| connection.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        let last_error = connection.get("lastError").cloned().unwrap_or(Value::Null);

        for (model, until) in &locks {
            models.push(json!({
                "provider": provider,
                "model": model,
                "status": "cooldown",
                "until": until,
                "connectionId": connection_id,
                "connectionName": connection_name,
                "lastError": last_error,
            }));
        }

        if locks.is_empty()
            && connection.get("testStatus").and_then(Value::as_str) == Some("unavailable")
        {
            models.push(json!({
                "provider": provider,
                "model": "__all",
                "status": "unavailable",
                "connectionId": connection_id,
                "connectionName": connection_name,
                "lastError": last_error,
            }));
        }
    }

    let count = models.len();
    Json(json!({ "models": models, "unavailableCount": count })).into_response()
}

/// `POST /api/models/availability` — `{action:"clearCooldown",provider,model}`.
pub async fn availability_post(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid request").into_response();
    };
    let (Some(provider), Some(model)) = (
        payload.get("provider").and_then(Value::as_str),
        payload.get("model").and_then(Value::as_str),
    ) else {
        return ApiError::bad_request("Invalid request").into_response();
    };
    if payload.get("action").and_then(Value::as_str) != Some("clearCooldown") {
        return ApiError::bad_request("Invalid request").into_response();
    }

    let lock_key = format!(
        "{}{model}",
        router_db::repos::connections::MODEL_LOCK_PREFIX
    );
    let provider = provider.to_string();
    match state
        .write(move |tx| {
            let connections =
                router_db::repos::connections::get_provider_connections(tx, Some(&provider), None)?;
            for connection in connections {
                if connection.get(&lock_key).is_none_or(Value::is_null) {
                    continue;
                }
                let id = connection.get("id").and_then(Value::as_str).unwrap_or("");
                let mut patch = Map::new();
                patch.insert(lock_key.clone(), Value::Null);
                if connection.get("testStatus").and_then(Value::as_str) == Some("unavailable") {
                    patch.insert("testStatus".into(), json!("active"));
                    patch.insert("lastError".into(), Value::Null);
                    patch.insert("lastErrorAt".into(), Value::Null);
                    patch.insert("backoffLevel".into(), json!(0));
                }
                router_db::repos::connections::update_provider_connection(
                    tx,
                    id,
                    &Value::Object(patch),
                )?;
            }
            Ok(())
        })
        .await
    {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(_) => ApiError::internal("Failed to clear cooldown").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_whitelist_keeps_only_known_booleans() {
        assert_eq!(
            sanitize_caps(Some(
                &json!({"vision": true, "bogus": true, "search": "yes"})
            )),
            Some(json!({"vision": true}))
        );
        assert_eq!(sanitize_caps(Some(&json!({"bogus": true}))), None);
        assert_eq!(sanitize_caps(None), None);
    }
}
