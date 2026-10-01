//! `POST /api/version/update`.
//!
//! A static Rust binary cannot update itself in place, so the update action is
//! refused with a 403. The route stays so the dashboard's Update button has
//! something to call and gets a clear answer instead of a 404.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// `POST /api/version/update`.
pub async fn update() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "success": false,
            "message": "Update is only available in production build (9router CLI)",
        })),
    )
        .into_response()
}
