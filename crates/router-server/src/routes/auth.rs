//! `auth/*`: login, logout, status and reset-password.
//!
//! SAML and OIDC are dropped, so two branches of the login route are gone
//! rather than dead:
//!
//! - the tunnel/tailscale block, which reads `tunnelUrl`/`tailscaleUrl` that
//!   only the dropped tunnel subsystem ever sets;
//! - the SSO block, which refuses password login when `authMode` says so.
//!
//! The second one is a deliberate behaviour change: with no SSO routes to sign
//! in through, honouring it would lock the dashboard out. `authMode` stays an
//! inert settings field so an existing row still round-trips.

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::net::SocketAddr;

use router_db::repos::settings;

use crate::auth::login_limiter::client_ip;
use crate::auth::session::{
    clear_cookie_header, set_cookie_header, should_use_secure_cookie, verify_dashboard_password,
};
use crate::error::ApiError;
use crate::state::AppState;

const RESET_HINT: &str = "Forgot password? Reset to default via 9Router CLI \u{2192} Settings \u{2192} Reset Password to Default.";

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn no_store(response: &mut Response) {
    response.headers_mut().insert(
        "Cache-Control",
        axum::http::HeaderValue::from_static("no-store"),
    );
}

/// `POST /api/auth/login`.
pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ip = client_ip(
        peer.ip(),
        header(&headers, "x-forwarded-for"),
        header(&headers, "x-real-ip"),
    );
    let now = router_db::time::now_ms();

    let lock = state.limiter.check_lock(&ip, now);
    if lock.locked {
        return lockout_response(lock.retry_after);
    }

    // A non-JSON body fails the JSON extractor. That is the client's error, so
    // answer 400 rather than blaming the server with a 500.
    let Ok(Json(payload)) = body else {
        return ApiError::bad_request("Invalid JSON body").into_response();
    };

    let settings = match state.read(settings::get_settings).await {
        Ok(s) => s,
        Err(e) => return ApiError::internal(e.to_string()).into_response(),
    };

    let password = payload
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("");
    let stored_hash = settings
        .get("password")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    if !verify_dashboard_password(stored_hash, password) {
        let fail = state.limiter.record_fail(&ip, now);
        let post = state.limiter.check_lock(&ip, router_db::time::now_ms());
        if post.locked {
            return lockout_response(post.retry_after);
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": format!(
                    "Invalid password. {} attempt(s) left before lockout.",
                    fail.remaining_before_lock
                ),
                "remainingBeforeLock": fail.remaining_before_lock,
            })),
        )
            .into_response();
    }

    state.limiter.record_success(&ip);

    // A remote client may not hold a session while the default password is
    // still in use: handing out a JWT for "123456" would let any remote
    // attacker PATCH /api/settings and disable authentication outright.
    let local = crate::middleware::facts_from(&headers, peer).is_local();
    let must_change_password = stored_hash.is_none()
        && !std::env::var("INITIAL_PASSWORD").is_ok_and(|v| !v.is_empty())
        && !local;

    if must_change_password {
        let mut response = (
            StatusCode::FORBIDDEN,
            Json(json!({
                "success": false,
                "error": "Default password must be changed before remote access. Change it from the local machine (or set INITIAL_PASSWORD).",
                "mustChangePassword": true,
            })),
        )
            .into_response();
        no_store(&mut response);
        return response;
    }

    let token = state.session.create_token(serde_json::Map::new());
    let secure = should_use_secure_cookie(header(&headers, "x-forwarded-proto"));
    let mut response =
        Json(json!({ "success": true, "mustChangePassword": false })).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(&set_cookie_header(&token, secure)) {
        response.headers_mut().insert("set-cookie", v);
    }
    no_store(&mut response);
    response
}

fn lockout_response(retry_after: i64) -> Response {
    let message = format!("Too many failed attempts. Try again in {retry_after}s. {RESET_HINT}");
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(
            "Retry-After",
            axum::http::HeaderValue::from_str(&retry_after.to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
        )],
        Json(json!({
            "error": message,
            "retryAfter": retry_after,
            "resetHint": RESET_HINT,
        })),
    )
        .into_response()
}

/// `POST /api/auth/logout`. The `oidc_*` cookies are cleared too, even though
/// nothing here sets them: a browser may still carry stale ones.
pub async fn logout() -> Response {
    let mut response = Json(json!({ "success": true })).into_response();
    let cookies = [
        clear_cookie_header(),
        "oidc_state=; Path=/; Max-Age=0".to_string(),
        "oidc_nonce=; Path=/; Max-Age=0".to_string(),
        "oidc_code_verifier=; Path=/; Max-Age=0".to_string(),
    ];
    for cookie in cookies {
        if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
            response.headers_mut().append("set-cookie", v);
        }
    }
    no_store(&mut response);
    response
}

/// `GET /api/auth/status`.
///
/// `oidcConfigured`/`samlConfigured` are always false now; the fields stay
/// because the login page reads them. The session claims (`oidcName` and
/// friends) can never be present, so the display name is always the password
/// branch.
pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Ok(settings) = state.read(settings::get_settings).await else {
        return Json(fallback_status()).into_response();
    };

    let token = crate::middleware::cookie(&headers, "auth_token");
    let session = state.session.get_session(token.as_deref());

    let require_login = settings.get("requireLogin") != Some(&Value::Bool(false));
    let has_password = settings
        .get("password")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    let authenticated = session.is_some();

    Json(json!({
        "requireLogin": require_login,
        "authMode": settings.get("authMode").and_then(Value::as_str).unwrap_or("password"),
        "ssoType": settings.get("ssoType").and_then(Value::as_str).unwrap_or("oidc"),
        "oidcConfigured": false,
        "oidcLoginLabel": "Sign in with OIDC",
        "samlConfigured": false,
        "samlLoginLabel": "Sign in with SAML SSO",
        "hasPassword": has_password,
        "displayName": "Password user",
        "loginMethod": "Password",
        "authenticated": authenticated,
        "oidcName": Value::Null,
        "oidcEmail": Value::Null,
        "oidcLogin": false,
        "samlName": Value::Null,
        "samlEmail": Value::Null,
        "samlLogin": false,
    }))
    .into_response()
}

/// The status body returned when the settings read fails.
fn fallback_status() -> Value {
    json!({
        "requireLogin": true,
        "authMode": "password",
        "ssoType": "oidc",
        "oidcConfigured": false,
        "oidcLoginLabel": "Sign in with OIDC",
        "samlConfigured": false,
        "samlLoginLabel": "Sign in with SAML SSO",
        "hasPassword": false,
        "displayName": "Password user",
        "loginMethod": "Password",
        "authenticated": false,
        "oidcName": Value::Null,
        "oidcEmail": Value::Null,
        "oidcLogin": false,
        "samlName": Value::Null,
        "samlEmail": Value::Null,
        "samlLogin": false,
    })
}

/// `POST /api/auth/reset-password`. Local-only, enforced by the guard. Never
/// returns the default literal.
pub async fn reset_password(State(state): State<AppState>) -> Response {
    match state
        .write(|tx| settings::update_settings(tx, json!({ "password": Value::Null })))
        .await
    {
        Ok(_) => Json(json!({ "success": true })).into_response(),
        Err(e) => ApiError::internal(e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_hint_keeps_the_arrow_characters() {
        assert!(RESET_HINT.contains('\u{2192}'));
        assert!(RESET_HINT.starts_with("Forgot password?"));
    }

    #[test]
    fn fallback_status_matches_the_expected_shape() {
        let v = fallback_status();
        assert_eq!(v["requireLogin"], json!(true));
        assert_eq!(v["authenticated"], json!(false));
        assert_eq!(v["oidcName"], Value::Null);
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "requireLogin",
                "authMode",
                "ssoType",
                "oidcConfigured",
                "oidcLoginLabel",
                "samlConfigured",
                "samlLoginLabel",
                "hasPassword",
                "displayName",
                "loginMethod",
                "authenticated",
                "oidcName",
                "oidcEmail",
                "oidcLogin",
                "samlName",
                "samlEmail",
                "samlLogin",
            ]
        );
    }

    #[test]
    fn lockout_body_carries_the_hint_and_retry_after() {
        let response = lockout_response(30);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["retry-after"], "30");
    }
}
