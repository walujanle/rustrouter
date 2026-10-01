//! `combos/*`: the model-combo CRUD and preset routes.
//!
//! Rotation state is process-local (`services/combo.rs`), so a rename or a
//! delete has to invalidate it explicitly — the DB row changing does not.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use router_sse::services::combo::reset_combo_rotation;
use router_sse::services::combo_presets::{build_preset_items, is_preset_source};

use crate::error::ApiError;
use crate::state::AppState;

/// `VALID_NAME_REGEX = /^[a-zA-Z0-9_.\-]+$/`.
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

const INVALID_NAME: &str = "Name can only contain letters, numbers, -, _ and .";

/// `GET /api/combos`.
pub async fn list(State(state): State<AppState>) -> Response {
    match state.read(router_db::repos::combos::get_combos).await {
        Ok(combos) => Json(json!({ "combos": combos })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch combos").into_response(),
    }
}

/// `POST /api/combos`.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to create combo").into_response();
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return ApiError::bad_request("Name is required").into_response();
    };
    if !is_valid_name(name) {
        return ApiError::bad_request(INVALID_NAME).into_response();
    }

    let existing = state
        .read({
            let name = name.to_string();
            move |conn| router_db::repos::combos::get_combo_by_name(conn, &name)
        })
        .await
        .ok()
        .flatten();
    if existing.is_some() {
        return ApiError::bad_request("Combo name already exists").into_response();
    }

    let data = json!({
        "name": name,
        "models": payload.get("models").cloned().unwrap_or(json!([])),
        "kind": payload.get("kind").cloned().unwrap_or(Value::Null),
    });
    match state
        .write(move |tx| router_db::repos::combos::create_combo(tx, &data))
        .await
    {
        Ok(combo) => (StatusCode::CREATED, Json(combo)).into_response(),
        Err(_) => ApiError::internal("Failed to create combo").into_response(),
    }
}

/// `GET /api/combos/{id}`.
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state
        .read(move |conn| router_db::repos::combos::get_combo_by_id(conn, &id))
        .await
    {
        Ok(Some(combo)) => Json(combo).into_response(),
        Ok(None) => ApiError::not_found("Combo not found").into_response(),
        Err(_) => ApiError::internal("Failed to fetch combo").into_response(),
    }
}

/// `PUT /api/combos/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update combo").into_response();
    };

    if let Some(name) = payload.get("name").and_then(Value::as_str) {
        if !is_valid_name(name) {
            return ApiError::bad_request(INVALID_NAME).into_response();
        }
        let existing = state
            .read({
                let name = name.to_string();
                move |conn| router_db::repos::combos::get_combo_by_name(conn, &name)
            })
            .await
            .ok()
            .flatten();
        if let Some(existing) = existing
            && existing.get("id").and_then(Value::as_str) != Some(id.as_str())
        {
            return ApiError::bad_request("Combo name already exists").into_response();
        }
    }

    let prev = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::combos::get_combo_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();

    let Ok(combo) = state
        .write(move |tx| router_db::repos::combos::update_combo(tx, &id, &payload))
        .await
    else {
        return ApiError::internal("Failed to update combo").into_response();
    };
    let Some(combo) = combo else {
        return ApiError::not_found("Combo not found").into_response();
    };

    let prev_name = prev
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str);
    let new_name = combo.get("name").and_then(Value::as_str);
    if let Some(prev_name) = prev_name {
        reset_combo_rotation(Some(prev_name));
    }
    if let Some(new_name) = new_name
        && Some(new_name) != prev_name
    {
        reset_combo_rotation(Some(new_name));
    }

    Json(combo).into_response()
}

/// `DELETE /api/combos/{id}`.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let prev = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::combos::get_combo_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();

    match state
        .write(move |tx| router_db::repos::combos::delete_combo(tx, &id))
        .await
    {
        Ok(true) => {
            if let Some(name) = prev
                .as_ref()
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
            {
                reset_combo_rotation(Some(name));
            }
            Json(json!({ "success": true })).into_response()
        }
        Ok(false) => ApiError::not_found("Combo not found").into_response(),
        Err(_) => ApiError::internal("Failed to delete combo").into_response(),
    }
}

/// Existing combo names, in list order.
async fn existing_names(state: &AppState) -> Result<Vec<String>, ()> {
    let combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .map_err(|_| ())?;
    Ok(combos
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str).map(str::to_string))
        .collect())
}

fn invalid_source() -> Response {
    ApiError::bad_request("source must be 'cursor' or 'claude'").into_response()
}

/// `GET /api/combos/presets?source=`.
pub async fn presets(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(source) = params.get("source").map(String::as_str) else {
        return invalid_source();
    };
    if !is_preset_source(source) {
        return invalid_source();
    }

    let Ok(names) = existing_names(&state).await else {
        return ApiError::internal("Failed to preview combo presets").into_response();
    };
    let items = build_preset_items(source, &names);
    let to_create = items
        .iter()
        .filter(|i| i.get("exists") == Some(&Value::Bool(false)))
        .count();
    let to_skip = items.len() - to_create;
    Json(json!({
        "source": source,
        "items": items,
        "toCreate": to_create,
        "toSkip": to_skip,
    }))
    .into_response()
}

/// `POST /api/combos/presets`.
pub async fn create_presets(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let payload = body.map(|Json(v)| v).unwrap_or(json!({}));
    let Some(source) = payload.get("source").and_then(Value::as_str) else {
        return invalid_source();
    };
    if !is_preset_source(source) {
        return invalid_source();
    }

    let Ok(names) = existing_names(&state).await else {
        return ApiError::internal("Failed to create combo presets").into_response();
    };
    let items = build_preset_items(source, &names);

    let Ok((created, skipped)) = state
        .write(move |tx| {
            let mut created = Vec::new();
            let mut skipped = Vec::new();
            for item in &items {
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                if item.get("exists") == Some(&Value::Bool(true)) {
                    skipped.push(json!(name));
                    continue;
                }
                let data = json!({
                    "name": name,
                    "models": item.get("models").cloned().unwrap_or(json!([])),
                });
                created.push(router_db::repos::combos::create_combo(tx, &data)?);
            }
            Ok((created, skipped))
        })
        .await
    else {
        return ApiError::internal("Failed to create combo presets").into_response();
    };

    Json(json!({
        "source": source,
        "createdCount": created.len(),
        "skippedCount": skipped.len(),
        "created": created,
        "skipped": skipped,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation_matches_the_reference_regex() {
        assert!(is_valid_name("my-combo_1.0"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("slash/name"));
    }
}
