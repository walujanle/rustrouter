//! `settings/*`: the settings, database and proxy-test routes.
//!
//! `GET` and `PATCH` never emit the stored password hash or the OIDC client
//! secret; they emit `hasPassword` and `oidcConfigured` instead. Both carry
//! `Cache-Control: no-store`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use router_sse::executors::http::apply_outbound_proxy_settings;
use router_sse::modalities::ssrf;
use router_sse::services::combo::reset_combo_rotation;
use router_sse::services::proxy_test::test_proxy_url;

use crate::auth::guard::CLI_TOKEN_HEADER;
use crate::auth::session::{hash_password, verify_dashboard_password};
use crate::error::ApiError;
use crate::state::AppState;

const PASSWORD_HEADER: &str = "x-9r-password";
/// Secrets must never be mass-assigned from a request body (CWE-915).
const PROTECTED_SETTING_KEYS: [&str; 2] = ["password", "mitmSudoEncrypted"];

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}

/// The response projection: drop the two secrets, add the two derived flags.
///
/// `has_password` is only added by `GET`.
fn safe_settings(settings: &Value, has_password: bool) -> Value {
    let mut out = match settings {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    let has_password_stored = out
        .get("password")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    out.shift_remove("password");
    let oidc_client_secret = out
        .get("oidcClientSecret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    out.shift_remove("oidcClientSecret");
    let oidc_configured = out
        .get("oidcIssuerUrl")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
        && out
            .get("oidcClientId")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        && !oidc_client_secret.is_empty();
    out.insert("oidcConfigured".into(), json!(oidc_configured));
    if has_password {
        out.insert("hasPassword".into(), json!(has_password_stored));
    }
    Value::Object(out)
}

/// `GET /api/settings`.
pub async fn get(State(state): State<AppState>) -> Response {
    let settings = state.read(router_db::repos::settings::get_settings).await;
    let Ok(settings) = settings else {
        return ApiError::internal("Failed to get settings").into_response();
    };
    let mut body = match safe_settings(&settings, true) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    body.insert(
        "enableRequestLogs".into(),
        json!(std::env::var("ENABLE_REQUEST_LOGS").as_deref() == Ok("true")),
    );
    body.insert(
        "enableTranslator".into(),
        json!(std::env::var("ENABLE_TRANSLATOR").as_deref() == Ok("true")),
    );
    no_store(Json(Value::Object(body)).into_response())
}

/// `PATCH /api/settings`.
pub async fn patch(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update settings").into_response();
    };
    let mut updates = match payload {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    for key in PROTECTED_SETTING_KEYS {
        updates.shift_remove(key);
    }

    // A new password is hashed after verifying the current one.
    if let Some(new_password) = updates.get("newPassword").and_then(Value::as_str) {
        let new_password = new_password.to_string();
        let current_hash = state
            .read(|conn| {
                let raw = router_db::repos::settings::read_raw(conn)?;
                Ok(raw
                    .get("password")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string))
            })
            .await
            .unwrap_or(None);
        let current_password = updates
            .get("currentPassword")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(hash) = current_hash {
            let Some(current) = current_password else {
                return ApiError::bad_request("Current password required").into_response();
            };
            if !verify_dashboard_password(Some(&hash), &current) {
                return ApiError::unauthorized("Invalid current password").into_response();
            }
        } else if current_password.is_some_and(|c| c != "123456") {
            // First-time setup: no password or the default seed password only.
            return ApiError::unauthorized("Invalid current password").into_response();
        }
        match hash_password(&new_password) {
            Ok(hash) => {
                updates.insert("password".into(), json!(hash));
            }
            Err(_) => return ApiError::internal("Failed to update settings").into_response(),
        }
        updates.shift_remove("newPassword");
        updates.shift_remove("currentPassword");
    }

    // An empty OIDC client secret is deleted, not stored.
    if updates
        .get("oidcClientSecret")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
        && updates.contains_key("oidcClientSecret")
    {
        updates.shift_remove("oidcClientSecret");
    }

    let touched_proxy = updates.contains_key("outboundProxyEnabled")
        || updates.contains_key("outboundProxyUrl")
        || updates.contains_key("outboundNoProxy");
    let touched_combo = updates.contains_key("comboStrategy")
        || updates.contains_key("comboStickyRoundRobinLimit")
        || updates.contains_key("comboStrategies");
    let touched_quota_ping =
        updates.contains_key("codexAutoPing") || updates.contains_key("quotaAutoTrackerEnabled");
    let touched_update_check = updates.contains_key("autoUpdateCheck");
    let updates = Value::Object(updates);

    let settings = state
        .write(move |tx| router_db::repos::settings::update_settings(tx, updates.clone()))
        .await;
    let Ok(settings) = settings else {
        return ApiError::internal("Failed to update settings").into_response();
    };

    if touched_proxy {
        apply_outbound_proxy_settings(&settings);
    }
    if touched_combo {
        reset_combo_rotation(None);
    }
    if touched_quota_ping {
        crate::services::schedulers::configure_quota_auto_ping(state.clone(), &settings);
    }
    if touched_update_check {
        crate::services::update_check::configure(state, &settings);
    }

    no_store(Json(safe_settings(&settings, false)).into_response())
}

/// `isCliRequest(request)`: any CLI-token header counts, the value is checked by
/// the caller.
fn is_cli_request(headers: &HeaderMap) -> bool {
    headers.contains_key(CLI_TOKEN_HEADER)
}

/// `GET /api/settings/database`: export, gated by CLI token or the password.
pub async fn database_export(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_cli_request(&headers)
        && !password_ok(
            &state,
            crate::routes::misc::header(&headers, PASSWORD_HEADER),
        )
    {
        return ApiError::unauthorized("Invalid password").into_response();
    }
    match state.read(router_db::export::export_db).await {
        Ok(payload) => Json(payload).into_response(),
        Err(_) => ApiError::internal("Failed to export database").into_response(),
    }
}

/// `POST /api/settings/database`: import, gated the same way.
pub async fn database_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Failed to import database").into_response();
    };
    let mut payload = match payload {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let password = payload
        .shift_remove("password")
        .and_then(|v| v.as_str().map(str::to_string));
    if !is_cli_request(&headers) && !password_ok(&state, password.as_deref()) {
        return ApiError::unauthorized("Invalid password").into_response();
    }
    let payload = Value::Object(payload);

    let result = state
        .write(move |tx| router_db::export::import_db(tx, &payload))
        .await;
    match result {
        Ok(_) => {
            // Re-apply proxy settings so an import takes effect immediately.
            if let Ok(settings) = state.read(router_db::repos::settings::get_settings).await {
                apply_outbound_proxy_settings(&settings);
            }
            Json(json!({ "success": true })).into_response()
        }
        Err(e) => ApiError::new(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// `verifyDashboardPassword(password)` against the stored hash.
fn password_ok(state: &AppState, password: Option<&str>) -> bool {
    let Some(password) = password else {
        return false;
    };
    let stored = state
        .db
        .with_conn(|conn| {
            let raw = router_db::repos::settings::read_raw(conn)?;
            Ok(raw
                .get("password")
                .and_then(Value::as_str)
                .map(str::to_string))
        })
        .unwrap_or(None);
    verify_dashboard_password(stored.as_deref(), password)
}

/// `POST /api/settings/proxy-test`.
pub async fn proxy_test(
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return Json(json!({ "ok": false, "error": "Proxy test failed" })).into_response();
    };
    // `testUrl` is caller-supplied and is fetched through the caller's own
    // proxy, so it goes through the SSRF guard: a public URL only. Absent, the
    // helper uses its own default.
    if let Some(test_url) = payload
        .get("testUrl")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && ssrf::assert_public_url_resolved(test_url).await.is_err()
    {
        return Json(json!({ "ok": false, "error": "URL not allowed" })).into_response();
    }
    let result = test_proxy_url(
        payload.get("proxyUrl").and_then(Value::as_str),
        payload.get("testUrl").and_then(Value::as_str),
        payload.get("timeoutMs").and_then(Value::as_u64),
    )
    .await;

    if result.get("ok") == Some(&Value::Bool(true)) {
        return Json(result).into_response();
    }
    let status = result
        .get("status")
        .and_then(Value::as_u64)
        .unwrap_or(500)
        .clamp(100, 599) as u16;
    let error = result
        .get("error")
        .cloned()
        .unwrap_or_else(|| json!("Proxy test failed"));
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(json!({ "ok": false, "error": error })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_settings_strips_secrets_and_derives_flags() {
        let settings = json!({
            "password": "$2b$10$hash",
            "oidcIssuerUrl": "https://idp",
            "oidcClientId": "client",
            "oidcClientSecret": "shh",
            "requireLogin": true,
        });
        let out = safe_settings(&settings, true);
        assert!(out.get("password").is_none());
        assert!(out.get("oidcClientSecret").is_none());
        assert_eq!(out["oidcConfigured"], json!(true));
        assert_eq!(out["hasPassword"], json!(true));
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "oidcIssuerUrl",
                "oidcClientId",
                "requireLogin",
                "oidcConfigured",
                "hasPassword"
            ]
        );
    }

    #[test]
    fn oidc_not_configured_without_a_secret() {
        let out = safe_settings(
            &json!({ "oidcIssuerUrl": "https://idp", "oidcClientId": "c" }),
            false,
        );
        assert_eq!(out["oidcConfigured"], json!(false));
        assert!(out.get("hasPassword").is_none());
    }
}
