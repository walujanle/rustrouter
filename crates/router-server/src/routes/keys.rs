//! `keys/*`: the API-key CRUD routes.
//!
//! The machine id always comes from the server, never the request body — a
//! client-supplied id would mint keys bound to another machine.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

/// `KEY_ACCESS_MAX_ENTRIES`.
const KEY_ACCESS_MAX_ENTRIES: usize = 200;
/// `KEY_ACCESS_MAX_ENTRY_LENGTH`.
const KEY_ACCESS_MAX_ENTRY_LENGTH: usize = 256;

/// `validateKeyAccessInput`: reject rather than repair. A truncated allow list
/// would look accepted while silently granting less than the operator declared.
fn validate_key_access_input(input: &Value) -> Result<Value, String> {
    let Some(obj) = input.as_object() else {
        return Err("access must be an object { restricted, allow }".into());
    };
    let extra: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "restricted" && *k != "allow")
        .collect();
    if !extra.is_empty() {
        return Err(format!("access: unexpected field(s) {}", extra.join(", ")));
    }
    let Some(restricted) = obj.get("restricted").and_then(Value::as_bool) else {
        return Err("access.restricted must be a boolean".into());
    };
    let allow = match obj.get("allow") {
        None => Vec::new(),
        Some(Value::Array(list)) => {
            if list.iter().any(|e| !e.is_string()) {
                return Err("access.allow must be an array of strings".into());
            }
            list.clone()
        }
        Some(_) => return Err("access.allow must be an array of strings".into()),
    };
    if allow.len() > KEY_ACCESS_MAX_ENTRIES {
        return Err(format!(
            "access.allow has more than {KEY_ACCESS_MAX_ENTRIES} entries"
        ));
    }
    if allow.iter().any(|e| {
        e.as_str()
            .is_some_and(|s| s.trim().chars().count() > KEY_ACCESS_MAX_ENTRY_LENGTH)
    }) {
        return Err(format!(
            "access.allow entries must be at most {KEY_ACCESS_MAX_ENTRY_LENGTH} characters"
        ));
    }
    let normalized = router_db::repos::api_keys::normalize_allow_list(&allow);
    Ok(json!({ "restricted": restricted, "allow": normalized }))
}

/// `GET /api/keys`.
pub async fn list(State(state): State<AppState>) -> Response {
    match state.read(router_db::repos::api_keys::get_api_keys).await {
        Ok(keys) => Json(json!({ "keys": keys })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch keys").into_response(),
    }
}

/// `POST /api/keys`.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid JSON body").into_response();
    };
    let Some(name) = payload.get("name").and_then(Value::as_str) else {
        return ApiError::bad_request("Name is required").into_response();
    };
    let Ok(machine_id) = router_db::identity::consistent_machine_id(&state.paths, None) else {
        return ApiError::internal("Failed to create key").into_response();
    };

    let name = name.to_string();
    match state
        .write(move |tx| router_db::repos::api_keys::create_api_key(tx, Some(&name), &machine_id))
        .await
    {
        Ok(key) => (
            StatusCode::CREATED,
            Json(json!({
                "key": key.get("key").cloned().unwrap_or(Value::Null),
                "name": key.get("name").cloned().unwrap_or(Value::Null),
                "id": key.get("id").cloned().unwrap_or(Value::Null),
                "machineId": key.get("machineId").cloned().unwrap_or(Value::Null),
            })),
        )
            .into_response(),
        Err(_) => ApiError::internal("Failed to create key").into_response(),
    }
}

/// `GET /api/keys/{id}`.
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state
        .read(move |conn| router_db::repos::api_keys::get_api_key_by_id(conn, &id))
        .await
    {
        Ok(Some(key)) => Json(json!({ "key": key })).into_response(),
        Ok(None) => ApiError::not_found("Key not found").into_response(),
        Err(_) => ApiError::internal("Failed to fetch key").into_response(),
    }
}

/// `PUT /api/keys/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid JSON body").into_response();
    };
    let exists = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::api_keys::get_api_key_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten()
        .is_some();
    if !exists {
        return ApiError::not_found("Key not found").into_response();
    }

    let mut update_data = serde_json::Map::new();
    if let Some(is_active) = payload.get("isActive") {
        update_data.insert("isActive".into(), is_active.clone());
    }
    if let Some(access) = payload.get("access") {
        match validate_key_access_input(access) {
            Ok(value) => {
                update_data.insert("access".into(), value);
            }
            Err(message) => return ApiError::bad_request(message).into_response(),
        }
    }
    let update_data = Value::Object(update_data);

    match state
        .write(move |tx| router_db::repos::api_keys::update_api_key(tx, &id, &update_data))
        .await
    {
        Ok(updated) => Json(json!({ "key": updated })).into_response(),
        Err(_) => ApiError::internal("Failed to update key").into_response(),
    }
}

/// `DELETE /api/keys/{id}`.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state
        .write(move |tx| router_db::repos::api_keys::delete_api_key(tx, &id))
        .await
    {
        Ok(true) => Json(json!({ "message": "Key deleted successfully" })).into_response(),
        Ok(false) => ApiError::not_found("Key not found").into_response(),
        Err(_) => ApiError::internal("Failed to delete key").into_response(),
    }
}
