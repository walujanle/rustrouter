//! The Codex-only OAuth callback proxy and its session map.
//!
//! Codex's OAuth client only allows a fixed redirect (`http://localhost:1455`),
//! so the server has to own that port, on a second axum listener.
//!
//! Two modes: when a session was registered for the callback's `state` the proxy
//! exchanges the code and saves the connection itself; otherwise it 302s to the
//! app's own `/callback` so the client can finish the exchange.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use serde_json::{Value, json};

use crate::state::AppState;

/// `CODEX_PORT`.
const CODEX_PORT: u16 = 1455;
/// How long the listener stays up waiting for the callback before it stops.
const CODEX_PROXY_TIMEOUT_MS: Duration = Duration::from_secs(300);

/// One pending server-side exchange.
#[derive(Debug, Clone)]
pub struct CodexSession {
    pub code_verifier: String,
    pub redirect_uri: String,
    pub status: String,
    pub error: Option<String>,
    pub connection_id: Option<String>,
    pub email: Option<String>,
    /// When the flow was registered, for TTL eviction of abandoned flows.
    created_at_ms: i64,
}

/// Abandoned flows are evicted after this long. Longer than the listener
/// timeout, so a flow still waiting for its callback is never dropped early.
const SESSION_TTL_MS: i64 = 10 * 60 * 1000;
/// Hard cap on live sessions. One flow at a time is the real steady state.
const SESSION_CAP: usize = 32;

static SESSIONS: LazyLock<Mutex<HashMap<String, CodexSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RUNNING: LazyLock<Mutex<bool>> = LazyLock::new(|| Mutex::new(false));

/// Records a pending exchange under the callback's `state` key.
///
/// Prunes expired entries and evicts the oldest when the map is at the cap, so
/// a caller looping the start endpoint cannot grow it without bound.
pub fn register_session(state: &str, code_verifier: &str, redirect_uri: &str) -> bool {
    if state.is_empty() || code_verifier.is_empty() || redirect_uri.is_empty() {
        return false;
    }
    let now = router_db::time::now_ms();
    if let Ok(mut map) = SESSIONS.lock() {
        map.retain(|_, s| now - s.created_at_ms < SESSION_TTL_MS);
        if map.len() >= SESSION_CAP
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, s)| s.created_at_ms)
                .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
        map.insert(
            state.to_string(),
            CodexSession {
                code_verifier: code_verifier.to_string(),
                redirect_uri: redirect_uri.to_string(),
                status: "pending".into(),
                error: None,
                connection_id: None,
                email: None,
                created_at_ms: now,
            },
        );
    }
    true
}

/// The pending exchange for a callback `state`, if any.
pub fn session_status(state: &str) -> Option<CodexSession> {
    SESSIONS.lock().ok()?.get(state).cloned()
}

/// Drops the pending exchange for a callback `state`.
pub fn clear_session(state: &str) {
    if let Ok(mut map) = SESSIONS.lock() {
        map.remove(state);
    }
}

/// Aborts the listener task, freeing the port.
pub fn stop_proxy() {
    if let Ok(mut running) = RUNNING.lock() {
        if !*running {
            return;
        }
        *running = false;
    }
    // The bound listener lives inside the spawned task; aborting it drops the
    // socket. The task handle is kept in the session-free static below.
    if let Some(handle) = TASK.lock().ok().and_then(|mut h| h.take()) {
        handle.abort();
    }
}

static TASK: LazyLock<Mutex<Option<tokio::task::JoinHandle<()>>>> =
    LazyLock::new(|| Mutex::new(None));

/// Binds the callback port and serves it. Idempotent: a second call while the
/// listener is up reports success without rebinding.
pub async fn start_proxy(state: AppState, app_port: u16) -> Value {
    {
        let running = RUNNING.lock().map(|r| *r).unwrap_or(false);
        if running {
            return json!({ "success": true });
        }
    }

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", CODEX_PORT)).await {
        Ok(l) => l,
        Err(e) => {
            let reason = if e.kind() == std::io::ErrorKind::AddrInUse {
                "port_busy".to_string()
            } else {
                e.to_string()
            };
            return json!({ "success": false, "reason": reason });
        }
    };

    let router: Router<(AppState, u16)> = Router::new()
        .route("/callback", get(callback))
        .route("/auth/callback", get(callback));
    let router = router.with_state((state, app_port));

    let handle = tokio::spawn(async move {
        let server = axum::serve(listener, router);
        tokio::select! {
            _ = server => {}
            _ = tokio::time::sleep(CODEX_PROXY_TIMEOUT_MS) => {}
        }
        if let Ok(mut running) = RUNNING.lock() {
            *running = false;
        }
    });
    if let Ok(mut task) = TASK.lock() {
        *task = Some(handle);
    }
    if let Ok(mut running) = RUNNING.lock() {
        *running = true;
    }
    json!({ "success": true })
}

/// The callback handler: server-side exchange when a session matches, else the
/// legacy 302 to the app's own callback route.
async fn callback(
    State((state, app_port)): State<(AppState, u16)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let state_key = params.get("state").cloned().unwrap_or_default();
    let session = SESSIONS
        .lock()
        .ok()
        .and_then(|m| m.get(&state_key).cloned());

    let Some(session) = session else {
        let target = legacy_redirect(app_port, &params);
        stop_proxy();
        return Redirect::temporary(&target).into_response();
    };

    if let Some(error) = params.get("error") {
        let message = params
            .get("error_description")
            .cloned()
            .unwrap_or_else(|| error.clone());
        return fail(&state_key, &message);
    }
    let Some(code) = params.get("code").cloned().filter(|c| !c.is_empty()) else {
        return fail(&state_key, "No authorization code received");
    };

    let meta = serde_json::Map::new();
    let tokens = match router_sse::services::oauth_flow::exchange_tokens(
        "codex",
        &code,
        &session.redirect_uri,
        &session.code_verifier,
        &state_key,
        &meta,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => return fail(&state_key, &e),
    };

    let data = connection_payload("codex", "oauth", tokens);
    let created = match state
        .write(move |tx| router_db::repos::connections::create_provider_connection(tx, &data))
        .await
    {
        Ok(c) => c,
        Err(e) => return fail(&state_key, &e.to_string()),
    };

    if let Ok(mut map) = SESSIONS.lock()
        && let Some(s) = map.get_mut(&state_key)
    {
        s.status = "done".into();
        s.connection_id = created
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        s.email = created
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    stop_proxy();
    Html(result_page(true, "You can close this window.")).into_response()
}

/// The legacy path: forward the query to the app's own `/callback`.
///
/// Kept out of the async body on purpose: `form_urlencoded::Serializer` is not
/// `Send`, so holding one across an await point would make `callback` fail its
/// `Handler` bound.
fn legacy_redirect(app_port: u16, params: &HashMap<String, String>) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    format!("http://localhost:{app_port}/callback?{}", ser.finish())
}

fn fail(state_key: &str, message: &str) -> Response {
    if let Ok(mut map) = SESSIONS.lock()
        && let Some(s) = map.get_mut(state_key)
    {
        s.status = "error".into();
        s.error = Some(message.to_string());
    }
    stop_proxy();
    Html(result_page(false, message)).into_response()
}

/// The row shape every OAuth completion writes: the token payload plus
/// `provider`, `authType`, `expiresAt` and `testStatus`.
pub fn connection_payload(provider: &str, auth_type: &str, tokens: Value) -> Value {
    let mut map = match tokens {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    map.insert("provider".into(), Value::String(provider.to_string()));
    map.insert("authType".into(), Value::String(auth_type.to_string()));
    let expires_at = map
        .get("expiresIn")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .map(|secs| {
            chrono::DateTime::from_timestamp_millis(
                chrono::Utc::now().timestamp_millis() + secs * 1000,
            )
            .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .unwrap_or_default()
        });
    map.insert(
        "expiresAt".into(),
        expires_at.map_or(Value::Null, Value::String),
    );
    map.insert("testStatus".into(), Value::String("active".into()));
    Value::Object(map)
}

fn result_page(success: bool, message: &str) -> String {
    let color = if success { "#22c55e" } else { "#ef4444" };
    let icon = if success { "&#10003;" } else { "&#10007;" };
    let title = if success {
        "Authentication Successful"
    } else {
        "Authentication Failed"
    };
    let safe = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>{title}</title>
<style>body{{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#f5f5f5}}.c{{text-align:center;padding:2rem;background:#fff;border-radius:8px;box-shadow:0 2px 10px rgba(0,0,0,.1)}}.i{{color:{color};font-size:3rem}}h1{{margin:1rem 0}}p{{color:#666}}</style>
</head><body><div class="c"><div class="i">{icon}</div><h1>{title}</h1><p>{safe}</p><p>Closing in <span id="cd">3</span>s...</p>
<script>let n=3;const c=document.getElementById("cd");const t=setInterval(()=>{{n--;c.textContent=n;if(n<=0){{clearInterval(t);window.close();}}}},1000);</script>
</div></body></html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_payload_computes_expiry_and_defaults() {
        let out = connection_payload(
            "codex",
            "oauth",
            json!({ "accessToken": "a", "expiresIn": 3600 }),
        );
        assert_eq!(out.get("provider").and_then(Value::as_str), Some("codex"));
        assert_eq!(out.get("authType").and_then(Value::as_str), Some("oauth"));
        assert_eq!(
            out.get("testStatus").and_then(Value::as_str),
            Some("active")
        );
        assert!(out.get("expiresAt").and_then(Value::as_str).is_some());
    }

    #[test]
    fn connection_payload_nulls_expiry_when_absent() {
        let out = connection_payload("codex", "oauth", json!({ "accessToken": "a" }));
        assert_eq!(out.get("expiresAt"), Some(&Value::Null));
    }

    #[test]
    fn session_registration_round_trips() {
        assert!(register_session(
            "s1",
            "v1",
            "http://localhost:1455/auth/callback"
        ));
        let s = session_status("s1").unwrap();
        assert_eq!(s.status, "pending");
        assert_eq!(s.code_verifier, "v1");
        clear_session("s1");
        assert!(session_status("s1").is_none());
    }

    #[test]
    fn register_rejects_missing_fields() {
        assert!(!register_session("", "v", "r"));
        assert!(!register_session("s", "", "r"));
    }
}
