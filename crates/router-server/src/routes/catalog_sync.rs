//! `models/catalog-sync`: the model-catalog sync status and trigger routes.
//! The work lives in `router_sse::services::model_catalog_sync`.

use axum::Json;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// `GET /api/models/catalog-sync`.
pub async fn get() -> Response {
    Json(router_sse::services::model_catalog_sync::sync_status().await).into_response()
}

/// `POST /api/models/catalog-sync`.
pub async fn post() -> Response {
    match router_sse::services::model_catalog_sync::sync_model_catalog().await {
        Some(result) => Json(json!({ "success": true, "result": result })).into_response(),
        None => {
            let state = router_sse::services::model_catalog_sync::get_sync_state().await;
            let message = state
                .last_error
                .unwrap_or_else(|| "sync in progress".to_string());
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": message })),
            )
                .into_response()
        }
    }
}
