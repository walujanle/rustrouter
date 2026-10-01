//! `pricing/*`: the pricing table read, patch and reset routes.
//!
//! The built-in table is `catalog::default_pricing()`; user overrides live in
//! the `pricing` kv scope. GET returns the merge, PATCH validates and writes,
//! DELETE resets.

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use router_sse::catalog::default_pricing;

use crate::error::ApiError;
use crate::state::AppState;

/// The fields the PATCH validator accepts.
const VALID_FIELDS: [&str; 5] = ["input", "output", "cached", "reasoning", "cache_creation"];

/// `GET /api/pricing`.
pub async fn get(State(state): State<AppState>) -> Response {
    match state
        .read(|conn| router_db::repos::pricing::get_pricing(conn, &default_pricing()))
        .await
    {
        Ok(pricing) => Json(pricing).into_response(),
        Err(_) => ApiError::internal("Failed to fetch pricing").into_response(),
    }
}

/// The nested-shape validation. `Err(message)` is a 400 body.
fn validate(body: &Value) -> Result<(), String> {
    let Value::Object(providers) = body else {
        return Err("Invalid pricing data format".to_string());
    };
    for (provider, models) in providers {
        let Value::Object(models) = models else {
            return Err(format!("Invalid pricing for provider: {provider}"));
        };
        for (model, pricing) in models {
            let Value::Object(pricing) = pricing else {
                return Err(format!("Invalid pricing for model: {provider}/{model}"));
            };
            for (key, value) in pricing {
                if !VALID_FIELDS.contains(&key.as_str()) {
                    return Err(format!(
                        "Invalid pricing field: {key} for {provider}/{model}"
                    ));
                }
                // Must be a finite, non-negative number. `as_f64` rejects
                // strings, bools and null; a non-finite JSON number cannot
                // occur here.
                match value.as_f64() {
                    Some(n) if n.is_finite() && n >= 0.0 => {}
                    _ => {
                        return Err(format!(
                            "Invalid pricing value for {key} in {provider}/{model}: must be non-negative number"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// `PATCH /api/pricing`.
pub async fn patch(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid pricing data format").into_response();
    };
    if let Err(message) = validate(&payload) {
        return ApiError::bad_request(&message).into_response();
    }

    match state
        .write(move |tx| router_db::repos::pricing::update_pricing(tx, &payload))
        .await
    {
        Ok(pricing) => Json(pricing).into_response(),
        Err(_) => ApiError::internal("Failed to update pricing").into_response(),
    }
}

/// `DELETE /api/pricing?provider=&model=`.
pub async fn delete(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let provider = params.get("provider").cloned();
    let model = params.get("model").cloned();

    let result = state
        .write(move |tx| match (provider.as_deref(), model.as_deref()) {
            (Some(provider), Some(model)) => {
                router_db::repos::pricing::reset_pricing(tx, Some(provider), Some(model))
            }
            (Some(provider), None) => {
                router_db::repos::pricing::reset_pricing(tx, Some(provider), None)
            }
            _ => router_db::repos::pricing::reset_all_pricing(tx),
        })
        .await;
    if result.is_err() {
        return ApiError::internal("Failed to reset pricing").into_response();
    }

    match state
        .read(|conn| router_db::repos::pricing::get_pricing(conn, &default_pricing()))
        .await
    {
        Ok(pricing) => Json(pricing).into_response(),
        Err(_) => ApiError::internal("Failed to reset pricing").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_unknown_fields_and_negative_values() {
        assert!(validate(&json!({"p": {"m": {"input": 1.0}}})).is_ok());
        assert!(validate(&json!({"p": {"m": {"bogus": 1.0}}})).is_err());
        assert!(validate(&json!({"p": {"m": {"input": -1.0}}})).is_err());
        assert!(validate(&json!({"p": {"m": {"input": "1"}}})).is_err());
        assert!(validate(&json!({"p": 1})).is_err());
    }
}
