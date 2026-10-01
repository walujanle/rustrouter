//! `usage/*`: the nine kept dashboard usage routes.
//!
//! `usage/request-details` is dropped: it reads the observability store, which
//! this server never writes.
//!
//! Three things shape this module:
//!
//! * **`getUsageStats` assembles its own context.** The Rust repo takes the
//!   connection, node and api-key maps as `StatsContext`, so this module owns
//!   those three reads.
//! * **`history` and `stream` call `getUsageStats()` with no argument**, which
//!   is the service default `"all"` — not the route-level default `"7d"`.
//! * **`usage/[connectionId]` is the only route here where a provider failure
//!   becomes an HTTP error.** Every non-Codex provider returns a `{message}`
//!   body at 200; Codex errors, and the route turns that into a 500.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};

use router_sse::executors::http::ProxyOptions;
use router_sse::executors::oauth::should_refresh_credentials;
use router_sse::providers::registry::registry;
use router_sse::services::auth::credentials_from_connection;
use router_sse::services::connection_proxy::resolve_connection_proxy_config;
use router_sse::services::stats_emitter::{self, StatsEvent};
use router_sse::services::token_refresh::{
    refresh_provider_credentials, update_provider_credentials,
};
use router_sse::services::usage::{
    consume_codex_rate_limit_reset_credit, get_codex_rate_limit_reset_credits,
    get_usage_for_provider,
};
use router_sse::translator::concerns::primitives::{js_json_number, js_truthy};

use crate::error::ApiError;
use crate::state::AppState;

/// `VALID_PERIODS`.
const VALID_PERIODS: [&str; 6] = ["today", "24h", "7d", "30d", "60d", "all"];

/// `AUTH_EXPIRED_PATTERNS`.
const AUTH_EXPIRED_PATTERNS: [&str; 5] = [
    "expired",
    "authentication",
    "unauthorized",
    "401",
    "re-authorize",
];

/// The `?period=` parameter, defaulted and validated. `Err` is the message the
/// caller turns into a 400.
fn period_param(params: &HashMap<String, String>) -> Result<String, &'static str> {
    let period = params
        .get("period")
        .filter(|p| !p.is_empty())
        .cloned()
        .unwrap_or_else(|| "7d".to_string());
    if !VALID_PERIODS.contains(&period.as_str()) {
        return Err("Invalid period");
    }
    Ok(period)
}

/// `getUsageStats(period)`, with the context reads the Rust repo needs folded in.
///
/// The three context reads and the aggregation share one connection and one
/// `spawn_blocking` handoff: they all run on the same pool connection anyway,
/// and the SSE stream calls this on every stats event, so the round-trip count
/// is the cost that matters under load.
async fn compute_stats(state: &AppState, period: &str) -> Option<Value> {
    let period = period.to_string();
    let now_ms = router_db::repos::usage::current_ms();

    state
        .read(move |conn| {
            let connections =
                router_db::repos::connections::get_provider_connections(conn, None, None)?;
            let nodes = router_db::repos::nodes::get_provider_nodes(conn, None)?;
            let api_keys = router_db::repos::api_keys::get_api_keys(conn)?;

            // `c.id || c.email || c.id`. The maps are indexmaps, but their type
            // is inferred from the `StatsContext` fields, so this crate never
            // has to name `indexmap`.
            let connection_map = connections
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str)?;
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            c.get("email")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or(id);
                    Some((id.to_string(), name.to_string()))
                })
                .collect();
            let provider_node_name_map = nodes
                .iter()
                .filter_map(|n| {
                    let id = n
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())?;
                    let name = n
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())?;
                    Some((id.to_string(), name.to_string()))
                })
                .collect();

            let mut api_key_map = Map::new();
            for k in &api_keys {
                let Some(key) = k.get("key").and_then(Value::as_str) else {
                    continue;
                };
                api_key_map.insert(
                    key.to_string(),
                    json!({
                        "name": k.get("name").cloned().unwrap_or(Value::Null),
                        "id": k.get("id").cloned().unwrap_or(Value::Null),
                        "createdAt": k.get("createdAt").cloned().unwrap_or(Value::Null),
                    }),
                );
            }

            let ctx = router_db::repos::usage::StatsContext {
                connection_map: &connection_map,
                provider_node_name_map: &provider_node_name_map,
                api_key_map: &api_key_map,
                now_ms,
            };
            router_db::repos::usage::get_usage_stats(conn, &period, &ctx)
        })
        .await
        .ok()
}

/// `GET /api/usage/stats`.
pub async fn stats(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let period = match period_param(&params) {
        Ok(p) => p,
        Err(message) => return ApiError::bad_request(message).into_response(),
    };
    match compute_stats(&state, &period).await {
        Some(stats) => Json(stats).into_response(),
        None => ApiError::internal("Failed to fetch usage stats").into_response(),
    }
}

/// `GET /api/usage/chart`.
pub async fn chart(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let period = match period_param(&params) {
        Ok(p) => p,
        Err(message) => return ApiError::bad_request(message).into_response(),
    };
    let now_ms = router_db::repos::usage::current_ms();
    match state
        .read(move |conn| router_db::repos::usage::get_chart_data(conn, &period, now_ms))
        .await
    {
        Ok(data) => Json(data).into_response(),
        Err(_) => ApiError::internal("Failed to fetch chart data").into_response(),
    }
}

/// `GET /api/usage/history` — `getUsageStats()` with the service default
/// `"all"`, not the route default `"7d"`.
pub async fn history(State(state): State<AppState>) -> Response {
    match compute_stats(&state, "all").await {
        Some(stats) => Json(stats).into_response(),
        None => ApiError::internal("Failed to fetch usage stats").into_response(),
    }
}

/// `getRecentLogs(200)`, with the connection map it builds for account labels.
async fn recent_logs(state: &AppState) -> Option<Vec<String>> {
    state
        .read(|conn| {
            let connections =
                router_db::repos::connections::get_provider_connections(conn, None, None)?;
            // `c.name || c.email || ""`.
            let map = connections
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str)?;
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            c.get("email")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or("");
                    Some((id.to_string(), name.to_string()))
                })
                .collect();
            router_db::repos::usage::get_recent_logs(conn, 200, &map)
        })
        .await
        .ok()
}

/// `GET /api/usage/logs`.
pub async fn logs(State(state): State<AppState>) -> Response {
    match recent_logs(&state).await {
        Some(logs) => Json(logs).into_response(),
        None => ApiError::internal("Failed to fetch logs").into_response(),
    }
}

/// `GET /api/usage/request-logs` — the same `getRecentLogs(200)`.
pub async fn request_logs(State(state): State<AppState>) -> Response {
    match recent_logs(&state).await {
        Some(logs) => Json(logs).into_response(),
        None => ApiError::internal("Failed to fetch logs").into_response(),
    }
}

/// `getProviderByAlias(id)?.name || AI_PROVIDERS[id]?.name`.
fn provider_display_name(provider_id: &str) -> Option<String> {
    let registry = registry();
    let id = registry.resolve_alias(provider_id);
    registry
        .get(id)?
        .display
        .as_ref()?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

/// `GET /api/usage/providers`.
pub async fn providers(State(state): State<AppState>) -> Response {
    let provider_ids = state
        .read(router_db::repos::usage::get_distinct_providers)
        .await;
    let Ok(provider_ids) = provider_ids else {
        return ApiError::internal("Failed to fetch providers").into_response();
    };
    let nodes = state
        .read(|conn| router_db::repos::nodes::get_provider_nodes(conn, None))
        .await
        .unwrap_or_default();

    let node_map: HashMap<String, String> = nodes
        .iter()
        .filter_map(|n| {
            let id = n
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let name = n
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            Some((id.to_string(), name.to_string()))
        })
        .collect();

    let providers: Vec<Value> = provider_ids
        .into_iter()
        .map(|provider_id| {
            let name = node_map
                .get(&provider_id)
                .cloned()
                .or_else(|| provider_display_name(&provider_id))
                .unwrap_or_else(|| provider_id.clone());
            json!({ "id": provider_id, "name": name })
        })
        .collect();

    Json(json!({ "providers": providers })).into_response()
}

// ─── usage/stream (SSE) ───────────────────────────────────────────────────

/// `GET /api/usage/stream`.
///
/// This subscribes to `statsEmitter`'s `"update"` and `"pending"`; the chat
/// handler emits both (`DbHooks::track_pending_request` and
/// `build_save_usage`).
///
/// **Every frame is the lightweight one.** A full stats payload would be
/// rebuilt on every `"update"`, but the client merges only `activeRequests`,
/// `recentRequests`, `errorProvider` and `pending` out of the frame — the
/// tables and charts come from the separate `/api/usage/stats` and
/// `/api/usage/chart` fetches. Rebuilding the rest would mean a full
/// `usageHistory` scan per completed request, for a payload the client
/// discards. The live frame carries those four fields and nothing else is
/// recomputed, so a burst of traffic costs one index-only `LIMIT 100` probe per
/// debounce window instead of an aggregate over the whole table.
///
/// The snapshot is still sent once on connect, so a client that attaches
/// mid-session gets a full first frame.
pub async fn stream(State(state): State<AppState>) -> Response {
    let stream = async_stream::stream! {
        let Some(cached) = compute_stats(&state, "all").await else {
            return;
        };
        yield Ok::<Bytes, Infallible>(sse_frame(&cached));

        let mut rx = stats_emitter::subscribe();
        let mut ticker = tokio::time::interval(Duration::from_secs(25));
        // The first tick fires immediately; consume it so the snapshot is not
        // followed straight away by a ping.
        ticker.tick().await;

        loop {
            tokio::select! {
                event = rx.recv() => match event {
                    Ok(StatsEvent::Update | StatsEvent::Pending) => {
                        yield Ok(sse_frame(&live_fields(&state, &cached).await));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = ticker.tick() => {
                    yield Ok(Bytes::from_static(b": ping\n\n"));
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
        .headers_mut()
        .insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    response
}

fn sse_frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

/// `getActiveRequests()`'s live fields, overlaid on the cached snapshot so the
/// frame keeps the full payload's shape and key order.
async fn live_fields(state: &AppState, cached: &Value) -> Value {
    let now_ms = router_db::repos::usage::current_ms();

    // The map changes only when a connection is added, renamed or removed, so
    // the 30 s cache usually answers without a query. Read the table only on a
    // miss; the SSE handler runs this on every pending/update event.
    let connection_map = match router_db::stats::fresh_connection_map(now_ms) {
        Some(map) => map,
        None => {
            let pairs: Vec<(String, String)> = state
                .read(|conn| {
                    router_db::repos::connections::get_provider_connections(conn, None, None)
                })
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(|c| {
                    let id = c.get("id").and_then(Value::as_str)?;
                    let name = c
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            c.get("email")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or(id);
                    Some((id.to_string(), name.to_string()))
                })
                .collect();
            router_db::stats::cached_connection_map(now_ms, move || pairs)
        }
    };

    // `recentRequests` comes from the same `ORDER BY id DESC LIMIT 100` probe
    // the full aggregation uses: index-only, ~200 rows, not a table scan.
    let recent = state
        .read(|conn| router_db::repos::usage::recent_requests_from_history(conn, 100, true))
        .await
        .unwrap_or_default();

    let mut obj = cached.as_object().cloned().unwrap_or_default();
    obj.insert(
        "activeRequests".into(),
        json!(router_db::stats::active_requests(&connection_map)),
    );
    obj.insert("recentRequests".into(), json!(recent));
    obj.insert(
        "errorProvider".into(),
        json!(router_db::stats::recent_error_provider(now_ms)),
    );
    obj.insert("pending".into(), router_db::stats::pending_snapshot());
    Value::Object(obj)
}

// ─── usage/[connectionId] ─────────────────────────────────────────────────

/// `refreshAndUpdateCredentials(connection, force, proxyOptions)`: refresh if
/// due, persist the patch, and hand back the connection the caller should use.
///
/// `Err` carries the failure the routes map to a 401.
async fn refresh_and_update_credentials(
    state: &AppState,
    connection: Value,
    force: bool,
    proxy_options: &ProxyOptions,
) -> Result<Value, String> {
    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let credentials = credentials_from_connection(&connection);

    if !force && !should_refresh_credentials(&provider, &credentials, router_db::time::now_ms()) {
        return Ok(connection);
    }

    let refreshed = refresh_provider_credentials(&provider, &credentials, proxy_options).await;
    let patch = match refreshed {
        // `Some({error})` is the unrecoverable case, not a patch to persist.
        Some(patch) if patch.get("error").is_none() => patch,
        _ => {
            // A refresh that produced nothing is only fatal without a token to
            // fall back on.
            let has_token = connection
                .get("accessToken")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if has_token {
                return Ok(connection);
            }
            return Err(
                "Failed to refresh credentials. Please re-authorize the connection.".to_string(),
            );
        }
    };

    let Some(id) = connection
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(connection);
    };
    // `update_provider_credentials` normalizes the expiry pair and merges
    // `providerSpecificData`.
    update_provider_credentials(&state.db, &id, &patch);
    let reread = state
        .read(move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id))
        .await;
    Ok(reread.ok().flatten().unwrap_or(connection))
}

/// The `proxyOptions` threaded from a resolved connection config, with
/// `strictProxy` forced false so quota calls fall back to direct.
fn proxy_options_for(connection: &Value, state: &AppState) -> ProxyOptions {
    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object);
    let resolved = resolve_connection_proxy_config(&state.db, psd);
    ProxyOptions {
        enabled: resolved.connection_proxy_enabled,
        url: Some(resolved.connection_proxy_url).filter(|s| !s.is_empty()),
        no_proxy: Some(resolved.connection_no_proxy).filter(|s| !s.is_empty()),
        strict_proxy: false,
        vercel_relay_url: Some(resolved.vercel_relay_url).filter(|s| !s.is_empty()),
    }
}

/// `isAuthExpiredMessage(usage)`.
fn is_auth_expired_message(usage: &Value) -> bool {
    usage
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .is_some_and(|m| AUTH_EXPIRED_PATTERNS.iter().any(|p| m.contains(p)))
}

/// `GET /api/usage/{connectionId}`.
pub async fn connection_usage(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let force = params.get("force").is_some_and(|v| v == "1");

    let connection = state
        .read(move |conn| {
            router_db::repos::connections::get_provider_connection_by_id(conn, &connection_id)
        })
        .await;
    let Ok(Some(mut connection)) = connection else {
        return ApiError::not_found("Connection not found").into_response();
    };

    let auth_type = connection.get("authType").and_then(Value::as_str);
    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_oauth = auth_type == Some("oauth");
    let is_apikey_auth = matches!(auth_type, Some("apikey") | Some("api_key"));
    // Kiro persists "api_key"; generic providers persist "apikey".
    let is_apikey_eligible =
        is_apikey_auth && registry().usage_apikey().contains(&provider.as_str());
    if !is_oauth && !is_apikey_eligible {
        return Json(json!({ "message": "Usage not available for this connection" }))
            .into_response();
    }

    let proxy_options = proxy_options_for(&connection, &state);

    if is_oauth {
        match refresh_and_update_credentials(&state, connection, false, &proxy_options).await {
            Ok(updated) => connection = updated,
            Err(message) => {
                return ApiError::unauthorized(format!("Credential refresh failed: {message}"))
                    .into_response();
            }
        }
    }

    let mut usage = match get_usage_for_provider(&connection, &proxy_options, force).await {
        Ok(usage) => usage,
        Err(message) => return ApiError::internal(message).into_response(),
    };

    let has_refresh_token = connection
        .get("refreshToken")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if is_oauth
        && is_auth_expired_message(&usage)
        && has_refresh_token
        && let Ok(updated) =
            refresh_and_update_credentials(&state, connection, true, &proxy_options).await
    {
        connection = updated;
        if let Ok(retried) = get_usage_for_provider(&connection, &proxy_options, force).await {
            usage = retried;
        }
    }

    Json(usage).into_response()
}

// ─── usage/[connectionId]/codex-reset-credits ─────────────────────────────

/// `isAuthExpiredResult(result)` over the four fields it inspects.
fn is_auth_expired_result(result: &Value) -> bool {
    let raw = result.get("raw");
    [
        result.get("message"),
        result.get("code"),
        raw.and_then(|r| r.get("detail")),
        raw.and_then(|r| r.get("error")),
    ]
    .into_iter()
    .flatten()
    .filter(|v| js_truthy(v))
    .map(|v| match v {
        Value::String(s) => s.to_lowercase(),
        other => other.to_string().to_lowercase(),
    })
    .any(|s| AUTH_EXPIRED_PATTERNS.iter().any(|p| s.contains(p)))
}

/// `getCodexConnection(connectionId)` plus the OAuth refresh both verbs share.
/// `Err` is the finished response the caller returns as-is.
async fn codex_ready(
    state: &AppState,
    connection_id: &str,
) -> Result<(Value, bool, ProxyOptions), Response> {
    let connection = state
        .read({
            let id = connection_id.to_string();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await;
    let Ok(Some(connection)) = connection else {
        return Err(ApiError::not_found("Connection not found").into_response());
    };

    if connection.get("provider").and_then(Value::as_str) != Some("codex") {
        return Err(ApiError::bad_request(
            "Codex reset credits are only available for Codex connections.",
        )
        .into_response());
    }

    let auth_type = connection.get("authType").and_then(Value::as_str);
    let is_oauth = auth_type == Some("oauth");
    let is_access_token = auth_type == Some("access_token");
    if !is_oauth && !is_access_token {
        return Err(ApiError::bad_request(
            "Codex reset credits require an OAuth or access-token connection.",
        )
        .into_response());
    }

    let proxy_options = proxy_options_for(&connection, state);
    let connection = if is_oauth {
        match refresh_and_update_credentials(state, connection, false, &proxy_options).await {
            Ok(updated) => updated,
            Err(message) => {
                return Err(ApiError::unauthorized(format!(
                    "Credential refresh failed: {message}"
                ))
                .into_response());
            }
        }
    } else {
        connection
    };
    Ok((connection, is_oauth, proxy_options))
}

fn access_token(connection: &Value) -> &str {
    connection
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// `GET /api/usage/{connectionId}/codex-reset-credits`.
pub async fn codex_reset_credits_get(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> Response {
    let (mut connection, is_oauth, proxy_options) = match codex_ready(&state, &connection_id).await
    {
        Ok(ready) => ready,
        Err(response) => return response,
    };

    let result = match get_codex_rate_limit_reset_credits(
        access_token(&connection),
        &proxy_options,
        connection.get("providerSpecificData"),
    )
    .await
    {
        Ok(result) => result,
        Err(message) => {
            // A provider that returns an auth-expired *error* gets one forced
            // refresh and retry, OAuth only.
            let has_refresh_token = connection
                .get("refreshToken")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty());
            if !is_oauth
                || !has_refresh_token
                || !is_auth_expired_result(&json!({ "message": message }))
            {
                return ApiError::internal(message).into_response();
            }
            match refresh_and_update_credentials(&state, connection, true, &proxy_options).await {
                Ok(updated) => connection = updated,
                Err(message) => return ApiError::internal(message).into_response(),
            }
            match get_codex_rate_limit_reset_credits(
                access_token(&connection),
                &proxy_options,
                connection.get("providerSpecificData"),
            )
            .await
            {
                Ok(retried) => retried,
                Err(message) => return ApiError::internal(message).into_response(),
            }
        }
    };

    Json(result).into_response()
}

/// `getResponseForConsumeResult(result, redeemRequestId)`.
fn consume_response(result: &Value, redeem_request_id: &str) -> Response {
    let windows_reset = result
        .get("windowsReset")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let code = result.get("code").cloned().unwrap_or(Value::Null);

    if result.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        let credit = result
            .get("raw")
            .and_then(|r| r.get("credit"))
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or(Value::Null);
        return Json(json!({
            "code": code,
            "reset": true,
            "windows_reset": js_json_number(windows_reset),
            "redeemRequestId": redeem_request_id,
            "credit": credit,
        }))
        .into_response();
    }

    if result
        .get("noCredit")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "code": "no_credit",
                "reset": false,
                "windows_reset": js_json_number(windows_reset),
                "message": "No Codex reset credits available.",
            })),
        )
            .into_response();
    }

    let status = result.get("status").and_then(Value::as_u64).unwrap_or(0);
    let http_status = match u16::try_from(status) {
        Ok(s) if (400..500).contains(&s) => {
            StatusCode::from_u16(s).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        _ => StatusCode::BAD_GATEWAY,
    };
    let code = code
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown_response");
    let message = result
        .get("message")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("Codex reset credit consume returned an unexpected response.");
    (
        http_status,
        Json(json!({
            "code": code,
            "reset": false,
            "windows_reset": js_json_number(windows_reset),
            "message": message,
        })),
    )
        .into_response()
}

/// `POST /api/usage/{connectionId}/codex-reset-credits`.
pub async fn codex_reset_credits_post(
    State(state): State<AppState>,
    Path(connection_id): Path<String>,
) -> Response {
    let (mut connection, is_oauth, proxy_options) = match codex_ready(&state, &connection_id).await
    {
        Ok(ready) => ready,
        Err(response) => return response,
    };

    // Server-generated so a client cannot replay a redeem id.
    let redeem_request_id = uuid::Uuid::new_v4().to_string();
    let mut result = match consume_codex_rate_limit_reset_credit(
        access_token(&connection),
        &redeem_request_id,
        &proxy_options,
    )
    .await
    {
        Ok(result) => result,
        Err(message) => return ApiError::internal(message).into_response(),
    };

    let has_refresh_token = connection
        .get("refreshToken")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if is_oauth
        && is_auth_expired_result(&result)
        && has_refresh_token
        && let Ok(updated) =
            refresh_and_update_credentials(&state, connection, true, &proxy_options).await
    {
        connection = updated;
        if let Ok(retried) = consume_codex_rate_limit_reset_credit(
            access_token(&connection),
            &redeem_request_id,
            &proxy_options,
        )
        .await
        {
            result = retried;
        }
    }

    consume_response(&result, &redeem_request_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live frame must stand on its own.
    ///
    /// The stream no longer rebuilds the full stats payload on every event, so
    /// anything the client merges from an SSE frame has to come from
    /// `live_fields`. If a field is missing here the realtime view silently
    /// stops updating it.
    #[tokio::test]
    async fn live_frame_carries_every_field_the_client_merges() {
        let dir =
            std::env::temp_dir().join(format!("rustrouter-usage-test-{}", std::process::id()));
        let paths = router_db::Paths::new(dir);
        paths.ensure_dirs().expect("create scratch data dir");
        let db = router_db::Db::open(&paths.data_file, 1).expect("open scratch db");
        let state = AppState::new(db, paths).expect("build app state");

        let cached = json!({ "byModel": {}, "byProvider": {} });
        let frame = live_fields(&state, &cached).await;

        for key in [
            "activeRequests",
            "recentRequests",
            "errorProvider",
            "pending",
        ] {
            assert!(frame.get(key).is_some(), "live frame is missing `{key}`");
        }
        // The cached snapshot is overlaid, not replaced.
        assert!(frame.get("byModel").is_some());
    }

    #[test]
    fn period_defaults_and_rejects_unknown() {
        let mut params = HashMap::new();
        assert_eq!(period_param(&params).unwrap(), "7d");
        params.insert("period".into(), String::new());
        assert_eq!(period_param(&params).unwrap(), "7d");
        params.insert("period".into(), "24h".into());
        assert_eq!(period_param(&params).unwrap(), "24h");
        params.insert("period".into(), "1y".into());
        assert_eq!(period_param(&params).unwrap_err(), "Invalid period");
    }

    #[test]
    fn auth_expired_reads_message_code_and_raw_fields() {
        assert!(is_auth_expired_result(
            &json!({ "message": "Token expired" })
        ));
        assert!(is_auth_expired_result(&json!({ "code": "unauthorized" })));
        assert!(is_auth_expired_result(
            &json!({ "raw": { "detail": "401 from upstream" } })
        ));
        assert!(!is_auth_expired_result(&json!({ "code": "no_credit" })));
    }

    #[test]
    fn consume_response_statuses_match_the_expected() {
        let ok = consume_response(
            &json!({ "ok": true, "noCredit": false, "windowsReset": 2.0, "code": "reset", "raw": {} }),
            "rid",
        );
        assert_eq!(ok.status(), StatusCode::OK);

        let no_credit = consume_response(
            &json!({ "ok": false, "noCredit": true, "windowsReset": 0.0 }),
            "rid",
        );
        assert_eq!(no_credit.status(), StatusCode::CONFLICT);

        let client_error = consume_response(
            &json!({ "ok": false, "noCredit": false, "windowsReset": 0.0, "status": 429 }),
            "rid",
        );
        assert_eq!(client_error.status(), StatusCode::TOO_MANY_REQUESTS);

        let server_error = consume_response(
            &json!({ "ok": false, "noCredit": false, "windowsReset": 0.0, "status": 500 }),
            "rid",
        );
        assert_eq!(server_error.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn unknown_provider_id_has_no_display_name() {
        assert!(provider_display_name("definitely-not-a-provider").is_none());
    }
}
