//! The OpenAI-compatible LLM surface: `/v1/**` and `/v1beta/**`.
//!
//! The chat routes are thin. Everything real — the API-key gate, bypass
//! requests, combo rotation, the account loop — lives inside
//! [`router_sse::handlers::chat::handle_chat`], so a chat handler only parses
//! JSON, builds the lower-cased header map, and maps a `ChatResult` onto an
//! axum response. The client-facing path travels as `endpoint` because
//! `detectFormatByEndpoint` keys off it.
//!
//! The modality routes are the reverse: the engine cores are auth-free and take
//! resolved credentials, so this layer owns the `requireApiKey` gate, credential
//! selection, the multi-account fallback loop, the capability-scoped lock keys
//! and the two usage writes.
//!
//! CORS is `CorsLayer::permissive()` on the router (`docs/MODALITIES.md`), so
//! handlers do not add `Access-Control-*` themselves; the chat mapper drops the
//! ones the engine sets, because two `Access-Control-Allow-Origin` headers on
//! one response is a browser network error.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use router_sse::credentials::Credentials;
use router_sse::executors::executor::ByteStream;
use router_sse::executors::http::ProxyOptions;
use router_sse::executors::oauth::merge_refreshed_credentials;
use router_sse::handlers::chat::{ChatRequest, handle_chat};
use router_sse::handlers::chat_core::{ChatBody, ChatResult};
use router_sse::modalities::ssrf::assert_public_url_resolved;
use router_sse::modalities::{
    ModalityError, ModalityResponse, embeddings_core, fetch_core, search_core, systemone_core,
    webfetch_lock_key, websearch_lock_key,
};
use router_sse::providers::registry::registry;
use router_sse::services::auth::{
    AccountSelection, AllRateLimited, SelectedAccount, clear_account_error, extract_api_key,
    get_provider_credentials, is_valid_api_key, mark_account_unavailable, resolve_provider_id,
};
use router_sse::services::combo::{
    ComboAttempt, ComboOutcome, get_combo_models_from_data, handle_combo_chat,
};
use router_sse::services::model::{ModelInfo, get_model_info};
use router_sse::services::model_catalog::{
    INTERNAL_MODELS_FETCH_HEADER, build_models_list, estimate_anthropic_input_tokens,
    gemini_models_list, kind_slug_map, model_info_lookup,
};
use router_sse::services::token_refresh::{
    apply_refresh_patch, check_and_refresh_token, refresh_with_retry, update_provider_credentials,
};
use router_sse::translator::concerns::primitives::js_truthy;
use router_sse::utils::error::{build_error_body, unavailable_response};
use router_sse::utils::gemini_bridge::{
    convert_gemini_to_internal, convert_openai_response_to_gemini,
    transform_openai_sse_to_gemini_sse,
};
use router_sse::utils::ollama_transform::transform_to_ollama;

use crate::state::AppState;

// ─── response mapping ─────────────────────────────────────────────────────

fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// The standard error envelope produced by `build_error_body`.
fn json_err(code: u16, message: &str) -> Response {
    (status(code), Json(build_error_body(code, message))).into_response()
}

/// The combo loop's no-retry failure: a bare `{ error: { message } }`, which is
/// deliberately not the standard `build_error_body` envelope.
fn bare_error(code: u16, message: &str) -> Response {
    (
        status(code),
        Json(json!({ "error": { "message": message } })),
    )
        .into_response()
}

/// `unavailableResponse(...)`, retry-after header included.
fn unavailable(code: u16, message: &str, retry_after: &str, human: &str) -> Response {
    let (code, body, retry_sec) = unavailable_response(code, message, retry_after, human);
    let mut response = (status(code), Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_sec) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

/// A `ChatResult` on the wire. Engine-set CORS headers are dropped: the router's
/// `CorsLayer` owns them.
fn chat_response(result: ChatResult) -> Response {
    let mut response = Response::new(match result.body {
        ChatBody::Json(text) => Body::from(text),
        ChatBody::Bytes(bytes) => Body::from(bytes),
        ChatBody::Stream(stream) => Body::from_stream(crate::reclaim::tracked(stream)),
    });
    *response.status_mut() = status(result.status);
    for (name, value) in result.headers {
        if name.to_ascii_lowercase().starts_with("access-control-") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn modality_ok(response: ModalityResponse) -> Response {
    let content_type = response.content_type.clone();
    let bytes = response.body_bytes();
    let mut out = Response::new(Body::from(bytes));
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        out.headers_mut().insert("content-type", value);
    }
    out
}

fn modality_err(error: ModalityError) -> Response {
    (status(error.status), Json(error.body)).into_response()
}

/// The core's own status, with a 502 fallback for a zero.
fn upstream_err(status: u16, message: &str) -> Response {
    json_err(if status == 0 { 502 } else { status }, message)
}

fn lower_headers(headers: &HeaderMap) -> HashMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Parse a JSON body, or a 400 `"Invalid JSON body"`.
#[allow(clippy::result_large_err)] // `Response` is the natural error here.
fn parse_json(bytes: &Bytes) -> Result<Value, Response> {
    serde_json::from_slice(bytes).map_err(|_| json_err(400, "Invalid JSON body"))
}

fn one_chunk_stream(bytes: Vec<u8>) -> ByteStream {
    Box::pin(futures::stream::once(async move { Ok(Bytes::from(bytes)) }))
}

/// The 2-source API-key extraction used by chat: `Bearer` then `x-api-key`.
/// The guard's 4-source variant is a different function.
fn handler_api_key(headers: &HeaderMap) -> Option<String> {
    extract_api_key(
        header(headers, "authorization"),
        header(headers, "x-api-key"),
    )
}

/// `settings.requireApiKey` gate shared by the modality handlers.
///
/// A settings read that fails is an error, not a licence to skip the check:
/// treating it like `requireApiKey: false` would open the gate exactly when
/// the database is unhealthy. Answer 503 instead.
async fn api_key_gate(state: &AppState, api_key: Option<&str>) -> Option<Response> {
    let settings = match state.read(router_db::repos::settings::get_settings).await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "api key gate: settings read failed");
            return Some(json_err(503, "Settings unavailable"));
        }
    };
    if !settings.get("requireApiKey").is_some_and(js_truthy) {
        return None;
    }
    let Some(api_key) = api_key.filter(|k| !k.is_empty()) else {
        return Some(json_err(401, "Missing API key"));
    };
    if !is_valid_api_key(&state.db, api_key) {
        return Some(json_err(401, "Invalid API key"));
    }
    None
}

/// `getModelInfo(modelStr)` with the DB-backed alias table, combos and nodes.
/// Shared with `translator.rs`.
pub(crate) async fn resolve_model_info(state: &AppState, model_str: &str) -> ModelInfo {
    let aliases = state
        .read(router_db::repos::aliases::get_model_aliases)
        .await
        .ok()
        .and_then(|v| v.as_object().cloned());
    let nodes = state
        .read(|conn| router_db::repos::nodes::get_provider_nodes(conn, None))
        .await
        .unwrap_or_default();
    let combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .unwrap_or_default();
    let combo_lookup = |name: &str| -> Option<Value> {
        combos
            .iter()
            .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
            .cloned()
    };
    get_model_info(model_str, aliases.as_ref(), combo_lookup, &nodes)
}

fn proxy_options(credentials: &Credentials) -> ProxyOptions {
    ProxyOptions::from_value(Some(&Value::Object(
        credentials.provider_specific_data.clone(),
    )))
}

/// `settings.comboStrategies[name].fallbackStrategy || settings.comboStrategy
/// || "fallback"`.
fn combo_strategy<'a>(settings: &'a Value, name: &str) -> &'a str {
    settings
        .get("comboStrategies")
        .and_then(|c| c.get(name))
        .and_then(|c| c.get("fallbackStrategy"))
        .and_then(Value::as_str)
        .or_else(|| settings.get("comboStrategy").and_then(Value::as_str))
        .unwrap_or("fallback")
}

/// `saveRequestUsage(entry)`, awaited instead of fire-and-forget.
async fn save_usage(
    state: &AppState,
    provider: &str,
    model: &str,
    connection_id: &str,
    api_key: Option<&str>,
    endpoint: &str,
    tokens: Value,
) {
    let row = json!({
        "provider": provider,
        "model": model,
        "connectionId": connection_id,
        "apiKey": api_key,
        "endpoint": endpoint,
        "tokens": tokens,
        "status": "success",
    });
    let inserted = state
        .write(move |tx| {
            let mut row = row.clone();
            let cost = |provider: Option<&str>, model: Option<&str>, tokens: &Value| -> f64 {
                router_sse::catalog::get_pricing_for_model(provider, model.unwrap_or(""))
                    .map(|pricing| router_sse::catalog::calculate_cost_from_tokens(tokens, pricing))
                    .unwrap_or(0.0)
            };
            router_db::repos::usage::save_request_usage(tx, &mut row, &cost)
        })
        .await;
    // `saveRequestUsage` fires `update` only when a new row landed.
    if matches!(inserted, Ok(true)) {
        router_sse::services::stats_emitter::emit_update();
    }
}

/// `a ?? b` over object keys: fall through only on `null`/absent.
fn nullish<'a>(obj: &'a Map<String, Value>, a: &str, b: &str) -> Option<&'a Value> {
    match obj.get(a) {
        Some(v) if !v.is_null() => Some(v),
        _ => obj.get(b),
    }
}

/// `exactEmbeddingUsage(raw)`.
fn exact_embedding_usage(raw: Option<&Value>) -> Option<Value> {
    let raw = raw?.as_object()?;
    if raw.get("estimated") == Some(&Value::Bool(true)) {
        return None;
    }
    // `as_i64` rejects floats and strings, which is the safe-integer check.
    let prompt = nullish(raw, "prompt_tokens", "input_tokens")?.as_i64()?;
    let completion = nullish(raw, "completion_tokens", "output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let total = raw.get("total_tokens")?.as_i64()?;
    if prompt <= 0 || completion != 0 || total != prompt {
        return None;
    }
    Some(json!({
        "prompt_tokens": prompt,
        "completion_tokens": 0,
        "total_tokens": total,
    }))
}

// ─── chat family ──────────────────────────────────────────────────────────

async fn chat_route(
    state: &AppState,
    headers: &HeaderMap,
    endpoint: &str,
    body: &Bytes,
) -> Response {
    let Ok(body) = parse_json(body) else {
        return json_err(400, "Invalid JSON body");
    };
    let client_headers = lower_headers(headers);
    let result = handle_chat(
        &state.db,
        ChatRequest {
            body,
            headers: &client_headers,
            endpoint: Some(endpoint),
            api_key: handler_api_key(headers),
        },
    )
    .await;
    chat_response(result)
}

pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    chat_route(&state, &headers, "/v1/chat/completions", &body).await
}

pub async fn messages(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    chat_route(&state, &headers, "/v1/messages", &body).await
}

pub async fn responses(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    chat_route(&state, &headers, "/v1/responses", &body).await
}

/// `/codex/*` is rewritten onto `/api/v1/responses` by `next.config.mjs`.
pub async fn codex(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    chat_route(&state, &headers, "/v1/responses", &body).await
}

/// `/v1/responses/compact`: the same pipeline, with `body._compact = true`.
pub async fn responses_compact(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(mut body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert("_compact".into(), json!(true));
    }
    let client_headers = lower_headers(&headers);
    let result = handle_chat(
        &state.db,
        ChatRequest {
            body,
            headers: &client_headers,
            endpoint: Some("/v1/responses/compact"),
            api_key: handler_api_key(&headers),
        },
    )
    .await;
    chat_response(result)
}

/// `POST /v1/api/chat`: the chat pipeline, then the Ollama NDJSON transform.
pub async fn ollama_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "llama3.2".to_string());
    let Ok(body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };

    let client_headers = lower_headers(&headers);
    let result = handle_chat(
        &state.db,
        ChatRequest {
            body,
            headers: &client_headers,
            endpoint: Some("/v1/api/chat"),
            api_key: handler_api_key(&headers),
        },
    )
    .await;

    // Whatever body the upstream returned is piped through the transform,
    // error bodies included; a non-`data:` body produces only the flush line.
    let stream = match result.body {
        ChatBody::Stream(stream) => stream,
        ChatBody::Json(text) => one_chunk_stream(text.into_bytes()),
        ChatBody::Bytes(bytes) => one_chunk_stream(bytes),
    };
    let mut response = Response::new(Body::from_stream(crate::reclaim::tracked(
        transform_to_ollama(stream, &model),
    )));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
}

// ─── models family ────────────────────────────────────────────────────────

/// `GET /v1` and `GET /v1/models`.
pub async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let skip = header(&headers, INTERNAL_MODELS_FETCH_HEADER) == Some("1");
    let data = build_models_list(&state.db, &["llm"], skip).await;
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// `GET /v1/models/{kind}` and `GET /v1/models/{provider}/{model}`.
pub async fn model_by_id(State(state): State<AppState>, Path(rest): Path<String>) -> Response {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    let identifier = segments.join("/");

    if segments.len() == 1
        && let Some(kinds) = kind_slug_map(&identifier)
    {
        let data = build_models_list(&state.db, kinds, false).await;
        return Json(json!({ "object": "list", "data": data })).into_response();
    }

    let models = build_models_list(&state.db, &["llm"], false).await;
    let matched = models
        .into_iter()
        .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(identifier.as_str()));
    match matched {
        Some(model) => Json(model).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "message": format!("The model '{identifier}' does not exist or you do not have access to it."),
                    "type": "invalid_request_error",
                    "code": "model_not_found",
                }
            })),
        )
            .into_response(),
    }
}

/// `GET /v1/models/info?id=&kind=`.
pub async fn model_info(Query(params): Query<HashMap<String, String>>) -> Response {
    let Some(id) = params.get("id").filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "message": "Missing required query param: id (e.g. ?id=openai/dall-e-3)",
                    "type": "invalid_request_error",
                }
            })),
        )
            .into_response();
    };
    let kind = params.get("kind").map(String::as_str);
    match model_info_lookup(id, kind) {
        Some(info) => Json(info).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "message": format!("Model not found: {id}"), "type": "not_found" }
            })),
        )
            .into_response(),
    }
}

/// `POST /v1/messages/count_tokens`.
pub async fn count_tokens(body: Bytes) -> Response {
    let Ok(parsed) = serde_json::from_slice::<Value>(&body) else {
        // A bare string body here, not the error envelope.
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid JSON body" })),
        )
            .into_response();
    };
    Json(json!({ "input_tokens": estimate_anthropic_input_tokens(&parsed) })).into_response()
}

// ─── v1beta (Gemini facade) ───────────────────────────────────────────────

/// `GET /v1beta/models`.
pub async fn gemini_models() -> Response {
    Json(gemini_models_list()).into_response()
}

/// `POST /v1beta/models/{provider}/{model}:generateContent`.
///
/// The native Gemini TTS branch (`responseModalities: ["AUDIO"]`) left with the
/// media subsystems, so every request goes through the translator.
pub async fn gemini_generate(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some(action_segment) = segments.last() else {
        return gemini_500("Invalid model path");
    };
    let stream = action_segment.contains(":streamGenerateContent");
    let model = if segments.len() >= 2 {
        format!("{}/{}", segments[0], strip_gemini_action(segments[1]))
    } else {
        strip_gemini_action(segments[0])
    };

    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return gemini_500("Invalid JSON body");
    };

    let converted = convert_gemini_to_internal(&body, &model, stream);
    let api_key = gemini_client_api_key(&headers, &params);
    let client_headers = lower_headers(&headers);
    let endpoint = format!("/v1beta/models/{path}");
    let result = handle_chat(
        &state.db,
        ChatRequest {
            body: converted,
            headers: &client_headers,
            endpoint: Some(&endpoint),
            api_key,
        },
    )
    .await;

    if stream {
        chat_response(transform_openai_sse_to_gemini_sse(result, &model))
    } else {
        chat_response(convert_openai_response_to_gemini(result, &model))
    }
}

fn strip_gemini_action(segment: &str) -> String {
    segment
        .replace(":streamGenerateContent", "")
        .replace(":generateContent", "")
}

/// `extractGeminiClientApiKey`: `Bearer`, then `x-goog-api-key`, then `?key=`.
fn gemini_client_api_key(headers: &HeaderMap, params: &HashMap<String, String>) -> Option<String> {
    header(headers, "authorization")
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| header(headers, "x-goog-api-key").map(str::to_string))
        .or_else(|| params.get("key").filter(|s| !s.is_empty()).cloned())
}

fn gemini_500(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": { "message": message, "code": 500 } })),
    )
        .into_response()
}

// ─── shared modality plumbing ─────────────────────────────────────────────

/// What one credential-loop iteration decided.
enum Attempt {
    /// Success — this response wins.
    Done(Response),
    /// `shouldFallback` — exclude the account and try the next, carrying the
    /// failure forward so the all-rate-limited exit reports the last cause.
    Next { status: u16, message: String },
    /// No fallback — this response is final.
    Terminal(Response),
}

/// The `{allRateLimited}` / `{no credentials}` / `{no more accounts}` triple.
fn selection_failure(
    provider: &str,
    label: &str,
    exclude_empty: bool,
    last_error: Option<&str>,
    last_status: Option<u16>,
    limited: Option<&AllRateLimited>,
) -> Response {
    if let Some(limited) = limited {
        let message = last_error
            .or(limited.last_error.as_deref())
            .unwrap_or("Unavailable");
        let code = last_status
            .or_else(|| {
                limited
                    .last_error_code
                    .as_ref()
                    .and_then(Value::as_u64)
                    .map(|n| n.clamp(100, 599) as u16)
            })
            .unwrap_or(503);
        return unavailable(
            code,
            &format!("[{label}] {message}"),
            &limited.retry_after,
            &limited.retry_after_human,
        );
    }
    if exclude_empty {
        return json_err(400, &format!("No credentials for provider: {provider}"));
    }
    json_err(
        last_status.unwrap_or(503),
        last_error.unwrap_or("All accounts unavailable"),
    )
}

/// The account loop: exclusion, `getProviderCredentials`, the three exits.
/// `attempt` receives a fresh `AppState` clone and the selected account, so the
/// returned future owns everything it needs and the `FnMut` can be called again.
async fn account_loop<F, Fut>(
    state: AppState,
    provider: &str,
    model: Option<&str>,
    label: &str,
    mut attempt: F,
) -> Response
where
    F: FnMut(SelectedAccount) -> Fut,
    Fut: Future<Output = Attempt>,
{
    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_error: Option<String> = None;
    let mut last_status: Option<u16> = None;

    loop {
        let selection =
            get_provider_credentials(&state.db, provider, Some(&exclude), model, None).await;
        let selected = match selection {
            AccountSelection::Selected(account) => *account,
            AccountSelection::AllRateLimited(limited) => {
                return selection_failure(
                    provider,
                    label,
                    false,
                    last_error.as_deref(),
                    last_status,
                    Some(&limited),
                );
            }
            AccountSelection::None => {
                return selection_failure(
                    provider,
                    label,
                    exclude.is_empty(),
                    last_error.as_deref(),
                    last_status,
                    None,
                );
            }
        };

        let connection_id = selected
            .credentials
            .connection_id
            .clone()
            .unwrap_or_default();
        match attempt(selected).await {
            Attempt::Done(response) | Attempt::Terminal(response) => return response,
            Attempt::Next { status, message } => {
                exclude.insert(connection_id);
                last_error = Some(message);
                last_status = Some(status);
            }
        }
    }
}

/// `checkAndRefreshToken` + persist, shared by every credentialed modality.
async fn refreshed_credentials(
    state: &AppState,
    provider: &str,
    selected: &SelectedAccount,
) -> Credentials {
    let proxy = proxy_options(&selected.credentials);
    let outcome = check_and_refresh_token(provider, &selected.credentials, &proxy, false).await;
    if let Some(patch) = &outcome.patch {
        let connection_id = selected
            .credentials
            .connection_id
            .clone()
            .unwrap_or_default();
        update_provider_credentials(&state.db, &connection_id, patch);
    }
    outcome.credentials
}

/// `markAccountUnavailable` and its `shouldFallback` flag.
fn should_fallback(
    state: &AppState,
    selected: &SelectedAccount,
    provider: &str,
    model: Option<&str>,
    error: &ModalityError,
) -> bool {
    let connection_id = selected
        .credentials
        .connection_id
        .clone()
        .unwrap_or_default();
    mark_account_unavailable(
        &state.db,
        &connection_id,
        error.status,
        &error.message,
        Some(provider),
        model,
        None,
    )
    .should_fallback
}

/// `markAccountUnavailable` + the fallback decision, in the loop's vocabulary.
fn mark_failure(
    state: &AppState,
    selected: &SelectedAccount,
    provider: &str,
    model: Option<&str>,
    error: &ModalityError,
) -> Attempt {
    if should_fallback(state, selected, provider, model, error) {
        Attempt::Next {
            status: error.status,
            message: error.message.clone(),
        }
    } else {
        Attempt::Terminal(modality_err(error.clone()))
    }
}

// ─── embeddings ───────────────────────────────────────────────────────────

/// `POST /v1/embeddings`.
pub async fn embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };
    let api_key = handler_api_key(&headers);
    if let Some(response) = api_key_gate(&state, api_key.as_deref()).await {
        return response;
    }

    let Some(model_str) = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return json_err(400, "Missing model");
    };
    if !body.get("input").is_some_and(js_truthy) {
        return json_err(400, "Missing required field: input");
    }

    let info = resolve_model_info(&state, model_str).await;
    if info.provider.is_empty() {
        return json_err(400, "Invalid model format");
    }
    let (provider, model) = (info.provider, info.model);
    let label = format!("{provider}/{model}");

    // The core receives the fully-qualified model, as `{...body, model}`.
    let mut core_body = body.clone();
    if let Some(obj) = core_body.as_object_mut() {
        obj.insert("model".into(), json!(label));
    }

    account_loop(state.clone(), &provider, Some(&model), &label, |selected| {
        let state = state.clone();
        let core_body = core_body.clone();
        let provider = provider.clone();
        let model = model.clone();
        let api_key = api_key.clone();
        async move {
            let mut credentials = refreshed_credentials(&state, &provider, &selected).await;
            let proxy = proxy_options(&credentials);

            let mut result =
                embeddings_core(&core_body, &provider, &model, Some(&credentials), &proxy).await;

            // The 401/403 refresh-and-retry half lives here, not in the core.
            if let Err(error) = &result
                && matches!(error.status, 401 | 403)
            {
                result = refresh_and_retry_embeddings(
                    &state,
                    &provider,
                    &model,
                    &core_body,
                    &mut credentials,
                    &proxy,
                    &selected,
                )
                .await;
            }

            match result {
                Ok(response) => {
                    let connection_id = selected
                        .credentials
                        .connection_id
                        .clone()
                        .unwrap_or_default();
                    clear_account_error(
                        &state.db,
                        &connection_id,
                        &selected.connection,
                        Some(&model),
                    );
                    if let Some(tokens) = exact_embedding_usage(response.usage.as_ref()) {
                        save_usage(
                            &state,
                            &provider,
                            &model,
                            &connection_id,
                            api_key.as_deref(),
                            "/v1/embeddings",
                            tokens,
                        )
                        .await;
                    }
                    Attempt::Done(modality_ok(response))
                }
                Err(error) => mark_failure(&state, &selected, &provider, Some(&model), &error),
            }
        }
    })
    .await
}

/// `refreshWithRetry` around the executor's `refreshCredentials`, then one more
/// core call. Mirrors `chat_core.rs`'s 401/403 block.
async fn refresh_and_retry_embeddings(
    state: &AppState,
    provider: &str,
    model: &str,
    core_body: &Value,
    credentials: &mut Credentials,
    proxy: &ProxyOptions,
    selected: &SelectedAccount,
) -> Result<ModalityResponse, ModalityError> {
    let executor = router_sse::executors::get_executor(provider);
    if executor.no_auth() {
        return embeddings_core(core_body, provider, model, Some(credentials), proxy).await;
    }

    let refreshed = refresh_with_retry(credentials, 3, |creds| {
        let creds = creds.clone();
        let executor = Arc::clone(&executor);
        let proxy = proxy.clone();
        async move { executor.refresh_credentials(&creds, None, &proxy).await }
    })
    .await;

    let Some(refreshed) = refreshed else {
        return embeddings_core(core_body, provider, model, Some(credentials), proxy).await;
    };
    if refreshed.access_token.is_none() && refreshed.api_key.is_none() {
        return embeddings_core(core_body, provider, model, Some(credentials), proxy).await;
    }

    let patch =
        merge_refreshed_credentials(provider, credentials, &refreshed, router_db::time::now_ms());
    if let Some(patch) = &patch {
        apply_refresh_patch(credentials, patch);
    }
    if let Some(mut patch) = patch {
        if let Some(obj) = patch.as_object_mut() {
            obj.insert(
                "existingProviderSpecificData".into(),
                Value::Object(selected.credentials.provider_specific_data.clone()),
            );
            obj.insert("testStatus".into(), json!("active"));
        }
        let connection_id = selected
            .credentials
            .connection_id
            .clone()
            .unwrap_or_default();
        update_provider_credentials(&state.db, &connection_id, &patch);
    }

    embeddings_core(core_body, provider, model, Some(credentials), proxy).await
}

// ─── systemone ────────────────────────────────────────────────────────────

/// `POST /v1/systemone`.
pub async fn systemone(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };
    let api_key = handler_api_key(&headers);
    if let Some(response) = api_key_gate(&state, api_key.as_deref()).await {
        return response;
    }

    let Some(model_str) = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return json_err(400, "Missing model");
    };
    if body.get("state").is_none_or(Value::is_null) {
        return json_err(400, "Missing required field: state");
    }
    if !matches!(body.get("questions"), Some(Value::Object(_))) {
        return json_err(400, "Missing required field: questions");
    }

    let info = resolve_model_info(&state, model_str).await;
    if info.provider.is_empty() {
        return json_err(400, "Invalid model format");
    }
    let (provider, model) = (info.provider, info.model);
    let label = format!("{provider}/{model}");

    account_loop(state.clone(), &provider, Some(&model), &label, |selected| {
        let state = state.clone();
        let body = body.clone();
        let provider = provider.clone();
        let model = model.clone();
        let api_key = api_key.clone();
        async move {
            // The systemone core never refreshes; only the proactive
            // `checkAndRefreshToken` in the handler runs.
            let credentials = refreshed_credentials(&state, &provider, &selected).await;
            let proxy = proxy_options(&credentials);

            match systemone_core(&body, &provider, &model, Some(&credentials), &proxy).await {
                Ok(response) => {
                    let connection_id = selected
                        .credentials
                        .connection_id
                        .clone()
                        .unwrap_or_default();
                    clear_account_error(
                        &state.db,
                        &connection_id,
                        &selected.connection,
                        Some(&model),
                    );
                    if let Some(usage) = response.usage.as_ref().and_then(Value::as_object) {
                        let prompt = usage
                            .get("prompt_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        let completion = usage
                            .get("completion_tokens")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        let mut tokens = usage.clone();
                        tokens.insert("total_tokens".into(), json!(prompt + completion));
                        save_usage(
                            &state,
                            &provider,
                            &model,
                            &connection_id,
                            api_key.as_deref(),
                            "/v1/systemone",
                            Value::Object(tokens),
                        )
                        .await;
                    }
                    Attempt::Done(modality_ok(response))
                }
                Err(error) => mark_failure(&state, &selected, &provider, Some(&model), &error),
            }
        }
    })
    .await
}

// ─── search ───────────────────────────────────────────────────────────────

/// `POST /v1/search`.
pub async fn search(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };
    let api_key = handler_api_key(&headers);
    if let Some(response) = api_key_gate(&state, api_key.as_deref()).await {
        return response;
    }

    let Some(provider_input) = body
        .get("provider")
        .or_else(|| body.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return json_err(400, "Missing required field: provider (or model)");
    };
    if body
        .get("query")
        .and_then(Value::as_str)
        .is_none_or(|q| q.trim().is_empty())
    {
        return json_err(400, "Missing required field: query");
    }

    let settings = state
        .read(router_db::repos::settings::get_settings)
        .await
        .unwrap_or(Value::Null);
    let combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .unwrap_or_default();

    if let Some(models) = get_combo_models_from_data(&provider_input, &Value::Array(combos)) {
        return run_modality_combo(
            &state,
            &body,
            &settings,
            &provider_input,
            &models,
            |state, provider| {
                let body = body.clone();
                async move { single_provider_search(&state, body, provider).await }
            },
        )
        .await;
    }

    single_provider_search(&state, body, provider_input).await
}

async fn single_provider_search(state: &AppState, body: Value, provider_input: String) -> Response {
    let provider_id = resolve_provider_id(&provider_input);
    let Some(provider) = registry().get(&provider_id) else {
        return json_err(400, &format!("Unknown provider: {provider_input}"));
    };
    if !provider.extra.contains_key("searchConfig") {
        return json_err(
            400,
            &format!("Provider {provider_id} does not support web search"),
        );
    }

    let core_body = json!({
        "query": body.get("query").and_then(Value::as_str).unwrap_or("").trim(),
        "provider": provider_id,
        "max_results": body.get("max_results").cloned().unwrap_or(Value::Null),
        "search_type": body.get("search_type").cloned().unwrap_or(Value::Null),
        "country": body.get("country").cloned().unwrap_or(Value::Null),
        "language": body.get("language").cloned().unwrap_or(Value::Null),
        "time_range": body.get("time_range").cloned().unwrap_or(Value::Null),
        "offset": body.get("offset").cloned().unwrap_or(Value::Null),
        "domain_filter": body.get("domain_filter").cloned().unwrap_or(Value::Null),
        "content_options": body.get("content_options").cloned().unwrap_or(Value::Null),
        "provider_options": body.get("provider_options").cloned().unwrap_or(Value::Null),
    });

    if provider.no_auth == Some(true) {
        return match search_core(&core_body, &provider_id, None, &ProxyOptions::default()).await {
            Ok(response) => modality_ok(response),
            Err(error) => modality_err(error),
        };
    }

    // The lock scope, not the model: a failed search must not take the shared
    // chat key offline.
    let lock_key = websearch_lock_key(&provider_id);
    let label = provider_id.clone();

    account_loop(
        state.clone(),
        &provider_id,
        Some(&lock_key),
        &label,
        |selected| {
            let state = state.clone();
            let core_body = core_body.clone();
            let provider_id = provider_id.clone();
            let lock_key = lock_key.clone();
            async move {
                let credentials = refreshed_credentials(&state, &provider_id, &selected).await;
                let proxy = proxy_options(&credentials);
                match search_core(&core_body, &provider_id, Some(&credentials), &proxy).await {
                    Ok(response) => {
                        let connection_id = selected
                            .credentials
                            .connection_id
                            .clone()
                            .unwrap_or_default();
                        clear_account_error(
                            &state.db,
                            &connection_id,
                            &selected.connection,
                            Some(&lock_key),
                        );
                        Attempt::Done(modality_ok(response))
                    }
                    Err(error) => {
                        mark_failure(&state, &selected, &provider_id, Some(&lock_key), &error)
                    }
                }
            }
        },
    )
    .await
}

// ─── web fetch ────────────────────────────────────────────────────────────

/// `POST /v1/web/fetch`.
pub async fn web_fetch(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Ok(body) = parse_json(&body) else {
        return json_err(400, "Invalid JSON body");
    };
    let api_key = handler_api_key(&headers);
    if let Some(response) = api_key_gate(&state, api_key.as_deref()).await {
        return response;
    }

    let Some(provider_input) = body
        .get("provider")
        .or_else(|| body.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return json_err(400, "Missing required field: provider (or model)");
    };
    let Some(target_url) = body.get("url").and_then(Value::as_str) else {
        return json_err(400, "Missing required field: url");
    };
    if url::Url::parse(target_url).is_err() {
        return json_err(400, "Invalid URL format");
    }
    if let Err(blocked) = assert_public_url_resolved(target_url).await {
        return json_err(400, &blocked.0);
    }

    let settings = state
        .read(router_db::repos::settings::get_settings)
        .await
        .unwrap_or(Value::Null);
    let combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .unwrap_or_default();

    if let Some(models) = get_combo_models_from_data(&provider_input, &Value::Array(combos)) {
        return run_modality_combo(
            &state,
            &body,
            &settings,
            &provider_input,
            &models,
            |state, provider| {
                let body = body.clone();
                async move { single_provider_fetch(&state, body, provider).await }
            },
        )
        .await;
    }

    single_provider_fetch(&state, body, provider_input).await
}

async fn single_provider_fetch(state: &AppState, body: Value, provider_input: String) -> Response {
    let provider_id = resolve_provider_id(&provider_input);
    let Some(provider) = registry().get(&provider_id) else {
        return json_err(400, &format!("Unknown provider: {provider_input}"));
    };
    if !provider.extra.contains_key("fetchConfig") {
        return json_err(
            400,
            &format!("Provider {provider_id} does not support web fetch"),
        );
    }

    if provider.no_auth == Some(true) {
        return match fetch_core(&body, &provider_id, None, &ProxyOptions::default()).await {
            Ok(response) => modality_ok(response),
            Err(error) => upstream_err(error.status, &error.message),
        };
    }

    let lock_key = webfetch_lock_key(&provider_id);
    let label = provider_id.clone();

    account_loop(
        state.clone(),
        &provider_id,
        Some(&lock_key),
        &label,
        |selected| {
            let state = state.clone();
            let body = body.clone();
            let provider_id = provider_id.clone();
            let lock_key = lock_key.clone();
            async move {
                let credentials = refreshed_credentials(&state, &provider_id, &selected).await;
                let proxy = proxy_options(&credentials);
                match fetch_core(&body, &provider_id, Some(&credentials), &proxy).await {
                    Ok(response) => {
                        let connection_id = selected
                            .credentials
                            .connection_id
                            .clone()
                            .unwrap_or_default();
                        clear_account_error(
                            &state.db,
                            &connection_id,
                            &selected.connection,
                            Some(&lock_key),
                        );
                        Attempt::Done(modality_ok(response))
                    }
                    // The failure is rebuilt rather than forwarding the core's
                    // body.
                    Err(error) => {
                        if should_fallback(&state, &selected, &provider_id, Some(&lock_key), &error)
                        {
                            Attempt::Next {
                                status: error.status,
                                message: error.message,
                            }
                        } else {
                            Attempt::Terminal(upstream_err(error.status, &error.message))
                        }
                    }
                }
            }
        },
    )
    .await
}

// ─── combo over the modality single-provider handlers ─────────────────────

/// `handleComboChat({...})` for search/fetch. The winning `Response` rides out
/// through a slot because `ComboOutcome` only carries the index.
async fn run_modality_combo<F, Fut>(
    state: &AppState,
    body: &Value,
    settings: &Value,
    combo_name: &str,
    models: &[String],
    mut single: F,
) -> Response
where
    F: FnMut(AppState, String) -> Fut,
    Fut: Future<Output = Response>,
{
    let slot: Arc<Mutex<Option<Response>>> = Arc::new(Mutex::new(None));
    let strategy = combo_strategy(settings, combo_name).to_string();
    let sticky = settings
        .get("comboStickyRoundRobinLimit")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    let state = state.clone();
    let write_slot = Arc::clone(&slot);

    let outcome = handle_combo_chat(
        body,
        models,
        Some(combo_name),
        &strategy,
        sticky,
        true,
        move |_index, model| {
            let slot = Arc::clone(&write_slot);
            let fut = single(state.clone(), model.to_string());
            async move {
                let response = fut.await;
                if response.status().is_success() {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(response);
                    ComboAttempt::ok()
                } else {
                    combo_failure(response).await
                }
            }
        },
    )
    .await;

    match outcome {
        ComboOutcome::Success { .. } => slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| json_err(500, "combo produced no response")),
        ComboOutcome::AllFailed {
            status,
            message,
            retry_after,
            retry_after_human,
        } => match retry_after {
            Some(retry_after) => unavailable(status, &message, &retry_after, &retry_after_human),
            None => bare_error(status, &message),
        },
    }
}

/// Pull the message and the retry-after out of a failed attempt.
async fn combo_failure(response: Response) -> ComboAttempt {
    let code = response.status().as_u16();
    let (parts, body) = response.into_parts();
    let retry_after = parts
        .headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let message = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error"))
                .cloned()
        })
        .map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
        .unwrap_or_default();
    ComboAttempt {
        ok: false,
        status: code,
        error_text: message,
        retry_after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_usage_accepts_only_the_expected_shape() {
        assert_eq!(
            exact_embedding_usage(Some(&json!({ "prompt_tokens": 12, "total_tokens": 12 }))),
            Some(json!({ "prompt_tokens": 12, "completion_tokens": 0, "total_tokens": 12 }))
        );
        assert_eq!(
            exact_embedding_usage(Some(
                &json!({ "input_tokens": 5, "output_tokens": 0, "total_tokens": 5 })
            )),
            Some(json!({ "prompt_tokens": 5, "completion_tokens": 0, "total_tokens": 5 }))
        );
        // `??` falls through on `null`, not just on absence.
        assert_eq!(
            exact_embedding_usage(Some(
                &json!({ "prompt_tokens": null, "input_tokens": 5, "total_tokens": 5 })
            )),
            Some(json!({ "prompt_tokens": 5, "completion_tokens": 0, "total_tokens": 5 }))
        );
        for rejected in [
            Value::Null,
            json!({}),
            json!({ "prompt_tokens": 0, "total_tokens": 0 }),
            json!({ "prompt_tokens": "12", "total_tokens": 12 }),
            json!({ "prompt_tokens": 12, "total_tokens": 13 }),
            json!({ "prompt_tokens": 12, "completion_tokens": 1, "total_tokens": 12 }),
            json!({ "prompt_tokens": 12, "total_tokens": 12, "estimated": true }),
            json!([1, 2]),
        ] {
            assert_eq!(exact_embedding_usage(Some(&rejected)), None, "{rejected}");
        }
        assert_eq!(exact_embedding_usage(None), None);
    }

    #[test]
    fn gemini_action_suffixes_strip_in_any_order() {
        assert_eq!(
            strip_gemini_action("gemini-2.0:streamGenerateContent"),
            "gemini-2.0"
        );
        assert_eq!(
            strip_gemini_action("gemini-2.0:generateContent"),
            "gemini-2.0"
        );
        assert_eq!(strip_gemini_action("plain"), "plain");
    }

    #[test]
    fn chat_mapper_drops_engine_cors_headers() {
        let result = ChatResult {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
            ],
            body: ChatBody::Json("{}".to_string()),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        };
        let response = chat_response(result);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "application/json");
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }

    /// A streaming chat response has to move the reclaim counter, or the idle
    /// gate opens mid-traffic and the trim faults pages out under load. The
    /// response is dropped at the end, which is what the counter has to see.
    #[test]
    fn a_streaming_chat_response_is_counted_while_it_lives() {
        let before = crate::reclaim::in_flight();
        let stream: router_sse::executors::executor::ByteStream =
            Box::pin(futures::stream::empty());
        let response = chat_response(ChatResult {
            status: 200,
            headers: Vec::new(),
            body: ChatBody::Stream(stream),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        });
        assert_eq!(crate::reclaim::in_flight(), before + 1);
        drop(response);
        assert_eq!(crate::reclaim::in_flight(), before);
    }

    #[test]
    fn unavailable_body_and_header_come_from_the_engine_helper() {
        let response = unavailable(
            503,
            "[a/b] nope",
            "2000-01-01T00:00:00.000Z",
            "reset after 1s",
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
    }
}
