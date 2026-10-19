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

/// Reject a save that would make combo expansion cyclic.
///
/// A combo's `models` entries that are not `provider/model` pairs may name
/// another combo, and `handlers::chat` expands those recursively. The runtime
/// guard there answers 400 instead of recursing, but the cycle is a property of
/// the stored rows: catching it at save time tells the user which pair is wrong
/// and keeps a broken graph out of the database in the first place.
///
/// `combos` is the full list after the write would land (the row being saved
/// already replaced or added), and `edited` is the name whose row changed, so a
/// pre-existing cycle elsewhere is not blamed on this edit.
fn cycle_error(combos: &[Value], edited: &str) -> Option<String> {
    // name -> models, for combo names only. A `provider/model` entry can never
    // be a combo: `get_combo_models` refuses any name containing `/`.
    let members: std::collections::HashMap<&str, Vec<&str>> = combos
        .iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?;
            let models = c
                .get("models")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .filter(|m| !m.contains('/'))
                .collect::<Vec<_>>();
            Some((name, models))
        })
        .collect();

    fn walk<'a>(
        members: &std::collections::HashMap<&'a str, Vec<&'a str>>,
        name: &'a str,
        path: &mut Vec<&'a str>,
        seen: &mut std::collections::HashSet<&'a str>,
    ) -> Option<Vec<String>> {
        if path.contains(&name) {
            let at = path.iter().position(|n| *n == name).unwrap_or(0);
            let mut cycle: Vec<String> = path[at..].iter().map(|n| (*n).to_string()).collect();
            cycle.push(name.to_string());
            return Some(cycle);
        }
        if !seen.insert(name) {
            return None;
        }
        path.push(name);
        if let Some(children) = members.get(name) {
            for child in children {
                if let Some(cycle) = walk(members, child, path, seen) {
                    return Some(cycle);
                }
            }
        }
        path.pop();
        None
    }

    let mut seen = std::collections::HashSet::new();
    walk(&members, edited, &mut Vec::new(), &mut seen)
        .map(|cycle| format!("Combo cycle: {}", cycle.join(" -> ")))
}

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

    // The new row is what the graph would look like; check it before the write.
    let existing_combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .unwrap_or_default();
    let mut after: Vec<Value> = existing_combos;
    after.push(data.clone());
    if let Some(error) = cycle_error(&after, name) {
        return ApiError::bad_request(&error).into_response();
    }

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

    // Check the graph the update would produce. The edited row is replaced in
    // place so its new `models` are what the walk follows.
    let edited_name = payload
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            prev.as_ref()
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    if let Some(edited_name) = edited_name {
        let existing = state
            .read(router_db::repos::combos::get_combos)
            .await
            .unwrap_or_default();
        let mut after: Vec<Value> = Vec::with_capacity(existing.len());
        for row in existing {
            if row.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                let mut merged = row.as_object().cloned().unwrap_or_default();
                if let Some(patch) = payload.as_object() {
                    for (k, v) in patch {
                        merged.insert(k.clone(), v.clone());
                    }
                }
                after.push(Value::Object(merged));
            } else {
                after.push(row);
            }
        }
        if let Some(error) = cycle_error(&after, &edited_name) {
            return ApiError::bad_request(&error).into_response();
        }
    }

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
    ApiError::bad_request("source must be 'claude'").into_response()
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

    fn combos(rows: &[(&str, &[&str])]) -> Vec<Value> {
        rows.iter()
            .map(|(name, models)| json!({"name": name, "models": models}))
            .collect()
    }

    #[test]
    fn a_self_reference_is_a_cycle() {
        let error = cycle_error(&combos(&[("loop", &["loop"])]), "loop").unwrap();
        assert_eq!(error, "Combo cycle: loop -> loop");
    }

    #[test]
    fn a_two_hop_cycle_is_reported_from_the_edited_name() {
        let error = cycle_error(&combos(&[("a", &["b"]), ("b", &["a"])]), "a").unwrap();
        assert_eq!(error, "Combo cycle: a -> b -> a");
    }

    #[test]
    fn provider_models_and_plain_leaves_are_not_cycles() {
        // `openai/gpt-5` is a provider/model pair, never a combo name.
        assert!(
            cycle_error(
                &combos(&[("a", &["openai/gpt-5", "b"]), ("b", &["x/y"])]),
                "a"
            )
            .is_none()
        );
        // A chain that ends at a model is fine.
        assert!(cycle_error(&combos(&[("a", &["b"]), ("b", &["c/1"])]), "a").is_none());
    }

    #[test]
    fn a_cycle_elsewhere_is_not_blamed_on_this_edit() {
        // `x -> y -> x` exists, but the edited combo does not reach it.
        let rows = combos(&[("x", &["y"]), ("y", &["x"]), ("clean", &["c/1"])]);
        assert!(cycle_error(&rows, "clean").is_none());
    }
}
