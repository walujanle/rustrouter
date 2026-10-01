//! `translator/*`: the console-log, trace-file and translate debug routes.
//!
//! These are debug routes: `load`/`save` expose a fixed set of trace files, and
//! `translate` reproduces each half of the translation pipeline so the
//! dashboard can show what the engine would send. Only `send` actually talks to
//! an upstream, and it is the reason the credential-refresh path is duplicated
//! here rather than routed through `handle_chat`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

use router_sse::credentials::Credentials;
use router_sse::executors::executor::ExecuteRequest;
use router_sse::executors::get_executor;
use router_sse::executors::http::ProxyOptions;
use router_sse::executors::oauth::merge_refreshed_credentials;
use router_sse::providers::service::{detect_format, get_target_format};
use router_sse::services::auth::credentials_from_connection;
use router_sse::services::token_refresh::{apply_refresh_patch, update_provider_credentials};
use router_sse::translator::concerns::primitives::js_truthy;
use router_sse::translator::{TranslateRequestArgs, formats, translate_request};

use crate::routes::v1::resolve_model_info;
use crate::services::console_log::{self, ConsoleEvent};
use crate::state::AppState;

/// The exact filenames `load`/`save` accept, in pipeline order.
const ALLOWED_FILES: [&str; 8] = [
    "1_req_client.json",
    "2_req_source.json",
    "3_req_openai.json",
    "4_req_target.json",
    "5_res_provider.txt",
    "6_res_openai.txt",
    "7_res_client.txt",
    "7_res_client.json",
];

/// The `{ success: false, error }` body every failure path returns. Not
/// `ApiError`: that shape is `{ error }` alone.
fn fail(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "success": false, "error": message.into() })),
    )
        .into_response()
}

fn internal(message: impl Into<String>) -> Response {
    fail(StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// `path.join(process.cwd(), "logs", "translator")`.
fn translator_dir() -> std::io::Result<PathBuf> {
    Ok(std::env::current_dir()?.join("logs").join("translator"))
}

/// `GET /api/translator/console-logs`.
pub async fn console_logs_get() -> Response {
    Json(json!({ "success": true, "logs": console_log::logs() })).into_response()
}

/// `DELETE /api/translator/console-logs`.
pub async fn console_logs_delete() -> Response {
    console_log::clear();
    Json(json!({ "success": true })).into_response()
}

/// `data: {json}\n\n`, the frame hand-written for this stream.
fn sse_frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

/// `GET /api/translator/console-logs/stream`.
pub async fn console_logs_stream() -> Response {
    let mut events = console_log::subscribe();
    let buffered = console_log::logs();

    let stream = async_stream::stream! {
        if !buffered.is_empty() {
            yield Ok::<Bytes, std::io::Error>(sse_frame(&json!({"type": "init", "logs": buffered})));
        }

        let mut keepalive = tokio::time::interval(Duration::from_secs(25));
        // `setInterval` fires first after the interval, not immediately.
        keepalive.tick().await;

        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(ConsoleEvent::Line(line)) => {
                        yield Ok(sse_frame(&json!({"type": "line", "line": line})));
                    }
                    Ok(ConsoleEvent::Lines(lines)) => {
                        if !lines.is_empty() {
                            yield Ok(sse_frame(&json!({"type": "lines", "lines": lines})));
                        }
                    }
                    Ok(ConsoleEvent::Clear) => {
                        yield Ok(sse_frame(&json!({"type": "clear"})));
                    }
                    // A slow client dropped events; the next ones still arrive.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = keepalive.tick() => {
                    yield Ok(Bytes::from_static(b": ping\n\n"));
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    let headers = response.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        "cache-control",
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert("connection", HeaderValue::from_static("keep-alive"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// `GET /api/translator/load?file=`.
pub async fn load(Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(file) = params.get("file").filter(|f| !f.is_empty()) else {
        return fail(StatusCode::BAD_REQUEST, "File parameter required");
    };
    if !ALLOWED_FILES.contains(&file.as_str()) {
        return fail(StatusCode::BAD_REQUEST, "Invalid file name");
    }
    let file_path = match translator_dir() {
        Ok(dir) => dir.join(file),
        Err(error) => return internal(error.to_string()),
    };
    if !file_path.exists() {
        return fail(StatusCode::NOT_FOUND, "File not found");
    }
    match std::fs::read(&file_path) {
        // `readFileSync(path, "utf-8")` is lossy on invalid bytes.
        Ok(bytes) => Json(json!({
            "success": true,
            "content": String::from_utf8_lossy(&bytes),
        }))
        .into_response(),
        Err(error) => internal(error.to_string()),
    }
}

/// `POST /api/translator/save`.
pub async fn save(body: Result<Json<Value>, JsonRejection>) -> Response {
    let Ok(Json(payload)) = body else {
        return fail(StatusCode::BAD_REQUEST, "File and content required");
    };
    let file = payload.get("file");
    if !file.is_some_and(js_truthy) || payload.get("content").is_none() {
        return fail(StatusCode::BAD_REQUEST, "File and content required");
    }
    let Some(file) = file.and_then(Value::as_str) else {
        return fail(StatusCode::BAD_REQUEST, "Invalid file name");
    };
    if !ALLOWED_FILES.contains(&file) {
        return fail(StatusCode::BAD_REQUEST, "Invalid file name");
    }
    let Some(content) = payload.get("content").and_then(Value::as_str) else {
        // A non-string body is rejected here and surfaces as a 500.
        return internal("content must be a string");
    };

    let file_path = match translator_dir() {
        Ok(dir) => dir.join(file),
        Err(error) => return internal(error.to_string()),
    };
    if let Some(dir) = file_path.parent()
        && let Err(error) = std::fs::create_dir_all(dir)
    {
        return internal(error.to_string());
    }
    match std::fs::write(&file_path, content) {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(error) => internal(error.to_string()),
    }
}

/// `body.body || body` — the request may nest the real payload one level deep.
fn inner_body(body: &Value) -> &Value {
    body.get("body").filter(|v| js_truthy(v)).unwrap_or(body)
}

/// `body.stream !== false`: absent means streaming.
fn wants_stream(body: &Value) -> bool {
    body.get("stream") != Some(&Value::Bool(false))
}

/// The first connection that is not explicitly inactive.
fn active_connection(connections: &[Value]) -> Option<&Value> {
    connections
        .iter()
        .find(|c| c.get("isActive") != Some(&Value::Bool(false)))
}

/// `persistRefreshedCredentials(connection, newCredentials)`.
fn persist_refreshed_credentials(state: &AppState, connection: &Value, patch: &Value) {
    let mut patch = patch.clone();
    if let Some(obj) = patch.as_object_mut() {
        obj.insert(
            "existingProviderSpecificData".into(),
            connection
                .get("providerSpecificData")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
    }
    let Some(connection_id) = connection.get("id").and_then(Value::as_str) else {
        return;
    };
    update_provider_credentials(&state.db, connection_id, &patch);
}

/// `POST /api/translator/send`.
pub async fn send(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return fail(
            StatusCode::BAD_REQUEST,
            "provider, model, and body required",
        );
    };
    let provider = payload
        .get("provider")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let inner = payload.get("body").filter(|v| js_truthy(v));
    let (Some(provider), Some(model), Some(inner)) = (provider, model, inner) else {
        return fail(
            StatusCode::BAD_REQUEST,
            "provider, model, and body required",
        );
    };

    let connections = state
        .read({
            let provider = provider.to_string();
            move |conn| {
                router_db::repos::connections::get_provider_connections(conn, Some(&provider), None)
            }
        })
        .await;
    let Ok(connections) = connections else {
        return internal("Failed to load connections");
    };
    let Some(connection) = active_connection(&connections) else {
        return fail(
            StatusCode::BAD_REQUEST,
            format!("No active connection for provider: {provider}"),
        );
    };

    let mut credentials = credentials_from_connection(connection);

    let executor = get_executor(provider);
    let stream = wants_stream(inner);
    let proxy_options = ProxyOptions::default();

    let mut response = match executor
        .execute(ExecuteRequest::new(
            model,
            inner.clone(),
            stream,
            &credentials,
            proxy_options.clone(),
        ))
        .await
    {
        Ok(response) => response,
        Err(error) => return internal(error.to_string()),
    };

    // Auto-refresh on 401/403 and retry once, the same shape as the chat path.
    if response.status == 401 || response.status == 403 {
        let refreshed = executor
            .refresh_credentials(&credentials, None, &proxy_options)
            .await;
        if let Some(refreshed) = refreshed
            && (refreshed.access_token.is_some() || refreshed.copilot_token.is_some())
        {
            if let Some(patch) = merge_refreshed_credentials(
                provider,
                &credentials,
                &refreshed,
                router_db::time::now_ms(),
            ) {
                apply_refresh_patch(&mut credentials, &patch);
                if let Some(token) = refreshed.copilot_token.clone() {
                    credentials
                        .extra
                        .insert("copilotToken".into(), json!(token));
                }
                persist_refreshed_credentials(&state, connection, &patch);
            }
            response = match executor
                .execute(ExecuteRequest::new(
                    model,
                    inner.clone(),
                    stream,
                    &credentials,
                    proxy_options,
                ))
                .await
            {
                Ok(response) => response,
                Err(error) => return internal(error.to_string()),
            };
        }
    }

    if !(200..300).contains(&response.status) {
        let upstream_status = response.status;
        let status =
            StatusCode::from_u16(upstream_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let details = response.text().await.unwrap_or_default();
        return (
            status,
            Json(json!({
                "success": false,
                "error": format!("Provider error: {upstream_status}"),
                "details": details,
            })),
        )
            .into_response();
    }

    let mut out = Response::new(Body::from_stream(crate::reclaim::tracked(
        response.into_byte_stream(),
    )));
    let headers = out.headers_mut();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert("connection", HeaderValue::from_static("keep-alive"));
    out
}

/// A `HeaderMap` as the JSON object the debug view shows.
///
/// Header names come back lower-cased: `HeaderName` does not retain the casing
/// the executor wrote, so `Content-Type` reads as `content-type` here. HTTP
/// names are case-insensitive, so this is display-only.
fn headers_json(headers: &HeaderMap) -> Value {
    let mut map = Map::new();
    for (name, value) in headers.iter() {
        map.insert(
            name.as_str().to_string(),
            json!(value.to_str().unwrap_or_default()),
        );
    }
    Value::Object(map)
}

/// `POST /api/translator/translate`.
pub async fn translate(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return fail(StatusCode::BAD_REQUEST, "Step and body required");
    };
    let step = payload.get("step");
    let inner = payload.get("body").filter(|v| js_truthy(v));
    let (Some(step), Some(inner)) = (step, inner) else {
        return fail(StatusCode::BAD_REQUEST, "Step and body required");
    };
    if !js_truthy(step) {
        return fail(StatusCode::BAD_REQUEST, "Step and body required");
    }

    match step.as_i64() {
        Some(1) => {
            let client_body = inner_body(inner);
            let info = resolve_model_info(
                &state,
                client_body
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
            .await;
            let source_format = detect_format(client_body);
            let target_format = get_target_format(&info.provider, None);
            Json(json!({
                "success": true,
                "result": {
                    "provider": info.provider,
                    "model": info.model,
                    "sourceFormat": source_format,
                    "targetFormat": target_format,
                },
            }))
            .into_response()
        }

        Some(2) => {
            let client_body = inner_body(inner);
            let info = resolve_model_info(
                &state,
                client_body
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            )
            .await;
            let source_format = detect_format(client_body);
            let stream = wants_stream(client_body);

            let mut credentials = Credentials::default();
            let mut translated = translate_request(
                &TranslateRequestArgs {
                    source_format,
                    target_format: formats::OPENAI,
                    model: &info.model,
                    stream,
                    provider: Some(&info.provider),
                    strip_list: &[],
                    connection_id: None,
                },
                client_body.clone(),
                &mut credentials,
            )
            .body;
            if let Some(obj) = translated.as_object_mut() {
                obj.shift_remove("_toolNameMap");
            }
            Json(json!({ "success": true, "result": { "body": translated } })).into_response()
        }

        Some(3) => {
            let openai_body = inner_body(inner);
            let provider = inner
                .get("provider")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let model = inner
                .get("model")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let (Some(provider), Some(model)) = (provider, model) else {
                return fail(StatusCode::BAD_REQUEST, "provider and model required");
            };

            let target_format = get_target_format(provider, None);
            let stream = wants_stream(openai_body);

            let mut credentials = Credentials::default();
            let mut translated = translate_request(
                &TranslateRequestArgs {
                    source_format: formats::OPENAI,
                    target_format,
                    model,
                    stream,
                    provider: Some(provider),
                    strip_list: &[],
                    connection_id: None,
                },
                openai_body.clone(),
                &mut credentials,
            )
            .body;
            if let Some(obj) = translated.as_object_mut() {
                obj.shift_remove("_toolNameMap");
            }

            let connections = state
                .read({
                    let provider = provider.to_string();
                    move |conn| {
                        router_db::repos::connections::get_provider_connections(
                            conn,
                            Some(&provider),
                            None,
                        )
                    }
                })
                .await;
            let Ok(connections) = connections else {
                return internal("Failed to load connections");
            };
            let Some(connection) = active_connection(&connections) else {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("No active connection for provider: {provider}"),
                );
            };

            let credentials = credentials_from_connection(connection);
            let executor = get_executor(provider);
            let url = match executor.build_url(model, stream, 0, &credentials) {
                Ok(url) => url,
                Err(error) => return internal(error.to_string()),
            };
            // Header building is called with no url/model/body, so the provider
            // hooks that read them see `undefined`. Passed empty to match.
            let headers = match executor.build_headers(&credentials, stream, "", "", None) {
                Ok(headers) => headers,
                Err(error) => return internal(error.to_string()),
            };
            let final_body = executor.transform_request(model, translated, stream, &credentials);

            Json(json!({
                "success": true,
                "result": {
                    "url": url,
                    "headers": headers_json(&headers),
                    "body": final_body,
                },
            }))
            .into_response()
        }

        _ => fail(StatusCode::BAD_REQUEST, "Invalid step (1-3)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_is_exactly_the_reference_list() {
        assert_eq!(ALLOWED_FILES.len(), 8);
        assert!(ALLOWED_FILES.contains(&"7_res_client.txt"));
        assert!(ALLOWED_FILES.contains(&"7_res_client.json"));
        assert!(!ALLOWED_FILES.contains(&"../../etc/passwd"));
    }

    #[test]
    fn stream_flag_only_rejects_a_literal_false() {
        assert!(wants_stream(&json!({})));
        assert!(wants_stream(&json!({"stream": true})));
        assert!(wants_stream(&json!({"stream": null})));
        assert!(!wants_stream(&json!({"stream": false})));
    }

    #[test]
    fn inner_body_prefers_a_truthy_nested_body() {
        let nested = json!({"body": {"model": "x"}});
        assert_eq!(inner_body(&nested)["model"], json!("x"));
        let flat = json!({"model": "y"});
        assert_eq!(inner_body(&flat)["model"], json!("y"));
        // `{}` is truthy in JS, so a present-but-empty nested body still wins.
        let empty = json!({"body": {}, "model": "z"});
        assert!(inner_body(&empty).get("model").is_none());
        // `null` is falsy, so the outer object is used.
        let nulled = json!({"body": null, "model": "z"});
        assert_eq!(inner_body(&nulled)["model"], json!("z"));
    }

    #[test]
    fn an_inactive_connection_is_skipped() {
        let rows = vec![
            json!({"id": "a", "isActive": false}),
            json!({"id": "b", "isActive": true}),
        ];
        assert_eq!(active_connection(&rows).unwrap()["id"], json!("b"));
        assert!(active_connection(&[json!({"id": "a", "isActive": false})]).is_none());
        // A missing flag counts as active, as `isActive !== false` does.
        assert!(active_connection(&[json!({"id": "a"})]).is_some());
    }

    #[test]
    fn header_names_are_lowercased_in_the_debug_view() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));
        let out = headers_json(&headers);
        assert_eq!(out["content-type"], json!("application/json"));
    }
}
