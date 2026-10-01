//! The chat pipeline's dispatch layer.
//!
//! [`handle_chat_core`] is the orchestrator: it detects the wire format,
//! translates the request, runs the executor, then hands the upstream response
//! to one of three sibling handlers. This module owns the two types those
//! handlers share, so the non-streaming and streaming clusters stop carrying
//! parallel definitions of the same shape:
//!
//! * [`ChatContext`] — the request context, one struct carrying every field the
//!   handlers read. It is a superset of what each handler needs; a handler that
//!   does not use `user_agent` simply never touches it.
//! * [`ChatResult`] with [`ChatBody`] — one response shape for all three
//!   handlers. A non-streaming handler returns `ChatBody::Json`, the non-SSE
//!   guard returns `ChatBody::Bytes`, and the streaming handler returns
//!   `ChatBody::Stream`. The crate is HTTP-free, so this is a status, a header
//!   list and a body rather than an axum type.

pub mod non_streaming;
pub mod request_detail;
pub mod responses_convert;
pub mod sse_to_json;
pub mod streaming;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::catalog::get_capabilities_for_model;
use crate::credentials::Credentials;
use crate::executors::executor::{ByteStream, ExecError, ExecuteRequest, ExecutorLog};
use crate::executors::get_executor;
use crate::executors::grok_cli::supports_grok_cli_reasoning_effort;
use crate::executors::http::ProxyOptions;
use crate::executors::oauth::merge_refreshed_credentials;
use crate::handlers::chat_core::non_streaming::{
    handle_non_streaming_response, upstream_response_headers,
};
use crate::handlers::chat_core::sse_to_json::handle_forced_sse_to_json;
use crate::handlers::chat_core::streaming::{
    SaveUsageFn, StreamingBody, StreamingRequest, build_on_stream_complete,
    handle_streaming_response,
};
use crate::providers::lookup::{
    model_strip, model_supported_formats, model_target_format, model_type, model_upstream_id,
};
use crate::providers::registry::registry;
use crate::providers::service::{detect_format, get_target_format, resolve_transport};
use crate::rtk::{compress_messages, inject_caveman, inject_ponytail};
use crate::runtime_config::{TOKEN_SAVER_HEADER, http_status};
use crate::services::token_refresh::{apply_refresh_patch, refresh_with_retry};
use crate::session_manager::{SessionIdentityInput, resolve_session_id};
use crate::thinking::{apply_thinking, extract_thinking, strip_thinking_suffix};
use crate::translator::concerns::modality::strip_unsupported_modalities;
use crate::translator::concerns::prefetch::prefetch_remote_images;
use crate::translator::concerns::primitives::js_truthy;
use crate::translator::concerns::tool_call::{
    default_claude_tool_type, should_default_claude_tool_type,
};
use crate::translator::formats::claude::{anchor_claude_cache, normalize_claude_passthrough};
use crate::translator::{TranslateRequestArgs, formats, translate_request};
use crate::utils::bypass_handler::handle_bypass_request;
use crate::utils::chat_log::{fmt_think, tag_for_session};
use crate::utils::client_detector::{detect_client_tool, is_native_passthrough};
use crate::utils::error::{build_error_body, format_provider_error, parse_upstream_error};
use crate::utils::fingerprint::ToolNameMap;
use crate::utils::stream::{StreamCompleteFn, StreamHooks};
use crate::utils::tool_deduper::dedupe_tools;

/// `log?.line(tag, symbol, message)` / `log?.errorLine(...)`.
///
/// The logger is a `tracing` subscriber, so an implementor forwards these to
/// `tracing`. The trait exists so a test can capture the lines a handler emits.
///
/// It extends [`ExecutorLog`] because the executor is logged through the same
/// object (`log.debug("RETRY", …)` and friends), so one value serves both halves
/// of the pipeline.
pub trait ChatLog: ExecutorLog {
    fn line(&self, tag: &str, symbol: &str, message: &str);
    fn error_line(&self, tag: &str, symbol: &str, message: &str);
}

/// `onRequestSuccess()` — fire-and-forget.
pub type RequestSuccessFn = Arc<dyn Fn() + Send + Sync>;

/// The request context every chat handler reads.
pub struct ChatContext<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    /// The client's wire format.
    pub source_format: &'a str,
    /// The format we spoke to the provider in.
    pub target_format: &'a str,
    pub user_agent: Option<&'a str>,
    pub body: &'a Value,
    pub stream: bool,
    pub request_start_time_ms: i64,
    pub connection_id: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub client_endpoint: Option<&'a str>,
    /// `_toolNameMap`: sent name → caller's original name.
    pub tool_name_map: Option<&'a ToolNameMap>,
    /// `_customToolNames` (OpenAI Responses only).
    pub custom_tool_names: Option<&'a HashSet<String>>,
    pub req_tag: &'a str,
    pub log: Option<Arc<dyn ChatLog>>,
    pub credentials: Option<&'a Credentials>,
    pub on_stream_complete: Option<StreamCompleteFn>,
    pub on_request_success: Option<RequestSuccessFn>,
}

/// The response body a chat handler produces.
pub enum ChatBody {
    /// A serialized JSON body (non-streaming, forced-SSE-to-JSON).
    Json(String),
    /// A buffered body (the non-SSE guard's sanitised error page).
    Bytes(Vec<u8>),
    /// A live transformed SSE stream.
    Stream(ByteStream),
}

impl std::fmt::Debug for ChatBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(body) => f.debug_tuple("Json").field(body).finish(),
            Self::Bytes(body) => f.debug_tuple("Bytes").field(&body.len()).finish(),
            Self::Stream(_) => f.write_str("Stream(..)"),
        }
    }
}

/// What a chat handler returns.
///
/// `success` is derived from `status`: an error result always carries a
/// 4xx/5xx, and every success path a 2xx.
#[derive(Debug)]
pub struct ChatResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ChatBody,
    /// `appendRequestLog(extra)`.
    pub log: Option<Value>,
    /// The `saveRequestUsage` row, already canonicalized.
    pub usage_stats: Option<Value>,
    /// `result.error` — the message the account loop reads back.
    pub error: Option<String>,
    /// `result.resetsAtMs` — the precise cooldown expiry a provider reported.
    pub resets_at_ms: Option<f64>,
}

impl ChatResult {
    /// A JSON body with the two JSON headers.
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            headers: json_headers(),
            body: ChatBody::Json(body.to_string()),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        }
    }

    /// `createErrorResult(status, message)` without the trailing log line.
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            headers: json_headers(),
            body: ChatBody::Json(build_error_body(status, message).to_string()),
            log: None,
            usage_stats: None,
            error: Some(message.to_string()),
            resets_at_ms: None,
        }
    }

    /// `createErrorResult(status, message, resetsAtMs, extraHeaders)`.
    pub fn error_result(
        status: u16,
        message: &str,
        resets_at_ms: Option<f64>,
        extra_headers: Vec<(String, String)>,
    ) -> Self {
        let mut headers = json_headers();
        headers.extend(extra_headers);
        Self {
            status,
            headers,
            body: ChatBody::Json(build_error_body(status, message).to_string()),
            log: None,
            usage_stats: None,
            error: Some(message.to_string()),
            resets_at_ms,
        }
    }

    /// `appendLog({status: "FAILED N"})` then `createErrorResult(status, …)`.
    pub fn error_logged(status: u16, message: &str) -> Self {
        let mut result = Self::error(status, message);
        result.log = Some(json!({ "status": format!("FAILED {status}") }));
        result
    }

    pub fn success(&self) -> bool {
        self.status < 400
    }
}

fn json_headers() -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
    ]
}

/// `onCredentialsRefreshed(newCredentials)`.
pub type CredentialsRefreshedFn = Arc<dyn Fn(Value) + Send + Sync>;

/// Everything `handle_chat_core` reads.
pub struct ChatCoreRequest<'a> {
    pub body: Value,
    pub provider: &'a str,
    pub model: &'a str,
    /// Mutated in place: `runtimeTransport`, `rawHeaders`, and the rotated
    /// token fields a refresh writes back.
    pub credentials: &'a mut Credentials,
    pub log: Option<Arc<dyn ChatLog>>,
    pub hooks: Option<Arc<dyn StreamHooks>>,
    /// `clientRawRequest.headers`, lower-cased.
    pub client_headers: &'a HashMap<String, String>,
    /// `clientRawRequest.body` — the client-facing model the `▶` line prints.
    pub client_body: Option<&'a Value>,
    pub client_endpoint: Option<&'a str>,
    pub connection_id: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub cc_filter_naming: bool,
    pub rtk_enabled: bool,
    pub caveman_enabled: bool,
    pub caveman_level: Option<&'a str>,
    pub ponytail_enabled: bool,
    pub ponytail_level: Option<&'a str>,
    /// `detectFormatByEndpoint` result, when the caller resolved one.
    pub source_format_override: Option<&'a str>,
    pub provider_thinking: Option<&'a Value>,
    pub proxy_options: ProxyOptions,
    pub cancel: Option<CancellationToken>,
    pub on_credentials_refreshed: Option<CredentialsRefreshedFn>,
    pub on_request_success: Option<RequestSuccessFn>,
    pub save_usage: SaveUsageFn,
}

/// `stripContinuityFields(body)`: drop the translator-internal stash before the
/// body reaches an upstream that would reject the unknown assistant field.
fn strip_continuity_fields(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages {
        if let Some(obj) = msg.as_object_mut() {
            obj.shift_remove("encrypted_content");
            obj.shift_remove("reasoning_encrypted_content");
        }
    }
}

/// `v?.length || 0` for the arrays the `▶` line counts.
fn array_len(value: Option<&Value>) -> usize {
    value.and_then(Value::as_array).map_or(0, Vec::len)
}

/// `handleChatCore({…})`.
///
/// The orchestrator: resolve the formats, translate the request, run the
/// executor with a token-refresh retry, then hand the upstream response to the
/// forced-SSE, non-streaming or streaming handler. `headroom` and `pxpipe` are
/// not part of this build, so the token-saver chain is RTK, caveman and
/// ponytail only.
pub async fn handle_chat_core(request: ChatCoreRequest<'_>) -> ChatResult {
    let ChatCoreRequest {
        mut body,
        provider,
        model,
        credentials,
        log,
        hooks,
        client_headers,
        client_body,
        client_endpoint,
        connection_id,
        user_agent,
        api_key,
        cc_filter_naming,
        rtk_enabled,
        caveman_enabled,
        caveman_level,
        ponytail_enabled,
        ponytail_level,
        source_format_override,
        provider_thinking,
        proxy_options,
        cancel,
        on_credentials_refreshed,
        on_request_success,
        save_usage,
    } = request;

    let request_start_time_ms = router_db::time::now_ms();
    let session_seed = resolve_session_id(&SessionIdentityInput {
        headers: client_headers,
        body: &body,
        connection_id,
        workspace_id: None,
        scope: provider,
    });
    let req_tag = tag_for_session(&session_seed);

    let source_format: &str = source_format_override.unwrap_or_else(|| detect_format(&body));

    // Bypass patterns (warmup, skip, cc naming) answer locally.
    if let Some(bypass) =
        handle_bypass_request(&body, model, user_agent.unwrap_or(""), cc_filter_naming)
    {
        return ChatResult {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), bypass.content_type.to_string()),
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
            ],
            body: ChatBody::Bytes(bypass.bytes()),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        };
    }

    let alias = registry().alias_for(provider).to_string();
    let model_target_format = model_target_format(&alias, model);
    let model_supported_formats = model_supported_formats(&alias, model);
    let runtime_transport = resolve_transport(provider, source_format);
    // Per-model guard: a declared `supportedFormats` list limits which transport
    // applies, so a Claude-format request cannot route an OpenAI-only model to
    // /messages.
    let use_transport = match &model_supported_formats {
        None => runtime_transport,
        Some(formats) if formats.iter().any(|f| f == source_format) => runtime_transport,
        Some(_) => None,
    };
    let creds_value = json!({ "providerSpecificData": credentials.provider_specific_data.clone() });
    let target_format: String = use_transport
        .as_ref()
        .and_then(|t| t.format.clone())
        .or(model_target_format)
        .unwrap_or_else(|| get_target_format(provider, Some(&creds_value)).to_string());
    if let Some(transport) = &use_transport {
        credentials.runtime_transport = Some(transport.clone());
    }
    let strip_list = model_strip(&alias, model);
    let upstream_model = model_upstream_id(&alias, model);

    // Provider-level thinking override, only when the client set none.
    if let Some(thinking) = provider_thinking {
        let mode = thinking.get("mode").and_then(Value::as_str).unwrap_or("");
        if !mode.is_empty() && mode != "auto" {
            let has_thinking = body.get("thinking").is_some_and(js_truthy);
            let has_effort = body.get("reasoning_effort").is_some_and(js_truthy);
            if mode == "on" && !has_thinking {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "thinking".into(),
                        json!({"type": "enabled", "budget_tokens": 10000}),
                    );
                }
            } else if mode == "off" && !has_thinking {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("thinking".into(), json!({"type": "disabled"}));
                }
            } else if !has_effort && let Some(obj) = body.as_object_mut() {
                obj.insert("reasoning_effort".into(), json!(mode));
            }
        }
    }

    let token_saver_enabled = client_headers
        .get(TOKEN_SAVER_HEADER)
        .map(|v| v.to_lowercase())
        .as_deref()
        != Some("off");

    // Cursor's pre-translate RTK pass is gone with the provider.
    let client_requested_streaming = body.get("stream").and_then(Value::as_bool) == Some(true)
        || matches!(source_format, formats::GEMINI | formats::GEMINI_CLI);
    let provider_requires_streaming = registry()
        .transport(provider)
        .and_then(|t| t.force_stream)
        .unwrap_or(false);
    let mut stream = if provider_requires_streaming {
        true
    } else {
        body.get("stream").and_then(Value::as_bool) != Some(false)
    };

    let model_kind = model_type(&alias, model);

    let headers_value = Value::Object(
        client_headers
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect(),
    );
    let detected_tool = detect_client_tool(&headers_value, &body);
    if detected_tool == Some("deepseek-tui")
        && body.get("stream").and_then(Value::as_bool) != Some(true)
    {
        stream = false;
    }

    let accept_header = client_headers
        .get("accept")
        .map(String::as_str)
        .unwrap_or("");
    let client_prefers_json = accept_header.contains("application/json");
    let client_prefers_sse = accept_header.contains("text/event-stream");
    if client_prefers_json
        && !client_prefers_sse
        && body.get("stream").and_then(Value::as_bool) != Some(true)
        && !provider_requires_streaming
    {
        stream = false;
    }

    let client_tool = detected_tool;
    let passthrough = is_native_passthrough(client_tool, provider);

    // Expose the raw client headers to the translators and executors.
    credentials.raw_headers = client_headers.clone();

    if !passthrough {
        let caps = get_capabilities_for_model(Some(provider), model);
        if strip_unsupported_modalities(&mut body, source_format, &caps)
            && let Some(log) = &log
        {
            log.debug(
                "MODALITY",
                &format!("stripped unsupported media for {provider}/{model}"),
            );
        }
        let prefetched = prefetch_remote_images(&mut body, source_format, &target_format).await;
        if prefetched > 0
            && let Some(log) = &log
        {
            log.debug(
                "MODALITY",
                &format!("prefetched {prefetched} remote image(s) for {target_format}"),
            );
        }
    }

    let mut translated_body: Value;
    let mut tool_name_map: Option<ToolNameMap> = None;
    let mut custom_tool_names: Vec<String> = Vec::new();
    if passthrough {
        if let Some(log) = &log {
            log.debug(
                "PASSTHROUGH",
                &format!(
                    "{} → {provider} | native lossless",
                    client_tool.unwrap_or("")
                ),
            );
        }
        translated_body = body.clone();
        if let Some(obj) = translated_body.as_object_mut() {
            obj.insert(
                "model".into(),
                json!(strip_thinking_suffix(&upstream_model)),
            );
        }
        if provider == "codex" {
            let mut suffix_thinking = json!({});
            apply_thinking(
                source_format,
                &upstream_model,
                &mut suffix_thinking,
                Some(provider),
                None,
            );
            if let Some(effort) = suffix_thinking.get("reasoning_effort").cloned()
                && js_truthy(&effort)
                && let Some(obj) = translated_body.as_object_mut()
            {
                let mut reasoning = obj
                    .get("reasoning")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if let Some(map) = reasoning.as_object_mut() {
                    map.insert("effort".into(), effort);
                }
                obj.insert("reasoning".into(), reasoning);
                obj.shift_remove("reasoning_effort");
            }
        }
        if client_tool == Some("claude") {
            let model_for_normalize = translated_body
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            normalize_claude_passthrough(&mut translated_body, &model_for_normalize);
        }
    } else {
        let translated = translate_request(
            &TranslateRequestArgs {
                source_format,
                target_format: &target_format,
                model: &upstream_model,
                stream,
                provider: Some(provider),
                strip_list: &strip_list,
                connection_id,
            },
            body.clone(),
            credentials,
        );
        translated_body = translated.body;
        tool_name_map = translated.tool_name_map;
        custom_tool_names = translated.custom_tool_names;
        if let Some(obj) = translated_body.as_object_mut() {
            obj.insert(
                "model".into(),
                json!(strip_thinking_suffix(&upstream_model)),
            );
        }
        strip_continuity_fields(&mut translated_body);
    }

    // Dedupe built-in tools shadowed by equivalent MCP tools (Claude clients).
    if client_tool == Some("claude") && translated_body.get("tools").is_some_and(Value::is_array) {
        let tools = translated_body.get("tools").cloned().unwrap_or(Value::Null);
        let deduped = dedupe_tools(&tools);
        if !deduped.stripped.is_empty() {
            if let Some(obj) = translated_body.as_object_mut() {
                obj.insert("tools".into(), deduped.tools);
            }
            if let Some(log) = &log {
                let preview = deduped
                    .stripped
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let ellipsis = if deduped.stripped.len() > 3 {
                    "..."
                } else {
                    ""
                };
                log.debug(
                    "TOOLDEDUP",
                    &format!("stripped {}: {preview}{ellipsis}", deduped.stripped.len()),
                );
            }
        }
    }

    let final_format: &str = if passthrough {
        source_format
    } else {
        &target_format
    };

    if let Some(log) = &log {
        let client_model = client_body
            .and_then(|b| b.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("{provider}/{model}"));
        let msg_n = array_len(translated_body.get("messages"))
            .max(array_len(translated_body.get("input")))
            .max(array_len(translated_body.get("contents")))
            .max(array_len(body.get("messages")))
            .max(array_len(body.get("input")));
        let tool_n = array_len(translated_body.get("tools")).max(array_len(body.get("tools")));
        let fmt_str = if passthrough {
            format!("FMT: {source_format} (passthrough)")
        } else {
            format!("FMT: {source_format}→{target_format}")
        };
        let show_thinking = provider != "grok-cli" || supports_grok_cli_reasoning_effort(model);
        let think = if show_thinking {
            fmt_think(extract_thinking(&translated_body).as_ref())
        } else {
            None
        };
        let acc = credentials
            .connection_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| connection_id.map(|c| c.chars().take(8).collect()))
            .unwrap_or_else(|| "-".to_string());
        let mut parts = vec![
            format!("POST {client_model} → {provider}/{model}"),
            fmt_str,
            if stream {
                "STREAM".to_string()
            } else {
                "JSON".to_string()
            },
            format!("{msg_n} MSG"),
        ];
        if tool_n > 0 {
            parts.push(format!("{tool_n} TOOL"));
        }
        if let Some(think) = think {
            parts.push(format!("THINK:{think}"));
        }
        parts.push(format!("ACC:{acc}"));
        log.line(req_tag, "▶", &parts.join(" · "));
    }

    // TTS models reject tool messages and function calling.
    if model_kind.as_deref() == Some("tts")
        && let Some(obj) = translated_body.as_object_mut()
    {
        if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            messages.retain(|msg| msg.get("role").and_then(Value::as_str) != Some("tool"));
        }
        obj.shift_remove("tools");
    }

    let tools_value = translated_body.get("tools").cloned().unwrap_or(Value::Null);
    if should_default_claude_tool_type(Some(provider), final_format, &tools_value)
        && let Some(obj) = translated_body.as_object_mut()
    {
        obj.insert("tools".into(), default_claude_tool_type(&tools_value));
    }

    let rtk_stats = compress_messages(&mut translated_body, token_saver_enabled && rtk_enabled);

    let mut xf: Vec<String> = Vec::new();
    if let Some(stats) = &rtk_stats
        && !stats.hits.is_empty()
    {
        xf.push(format!("RTK:{}", stats.hits.len()));
    }
    if token_saver_enabled
        && caveman_enabled
        && let Some(level) = caveman_level
    {
        inject_caveman(&mut translated_body, final_format, level);
        xf.push(format!("CAVEMAN:{level}"));
    }
    if token_saver_enabled
        && ponytail_enabled
        && let Some(level) = ponytail_level
    {
        inject_ponytail(&mut translated_body, final_format, level);
        xf.push(format!("PONYTAIL:{level}"));
    }
    if !xf.is_empty()
        && let Some(log) = &log
    {
        log.line(req_tag, "⚙", &xf.join(" · "));
    }

    if passthrough && client_tool == Some("claude") {
        anchor_claude_cache(&mut translated_body);
    }

    let executor = get_executor(provider);
    if let Some(hooks) = &hooks {
        hooks.track_pending_request(Some(model), Some(provider), connection_id, true, false);
        hooks.append_request_log(json!({
            "model": model,
            "provider": provider,
            "connectionId": connection_id,
            "status": "PENDING",
        }));
    }

    let exec_log_line =
        || -> Option<&dyn ExecutorLog> { log.as_ref().map(|l| l.as_ref() as &dyn ExecutorLog) };

    let mut provider_response = match executor
        .execute(ExecuteRequest {
            model,
            body: translated_body.clone(),
            stream,
            credentials,
            cancel: cancel.clone(),
            log: exec_log_line(),
            proxy_options: proxy_options.clone(),
            provider_session_id: Some(session_seed.as_str()),
            client_tool,
        })
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if let Some(hooks) = &hooks {
                hooks.track_pending_request(
                    Some(model),
                    Some(provider),
                    connection_id,
                    false,
                    true,
                );
                let status = if matches!(error, ExecError::Cancelled) {
                    499
                } else {
                    502
                };
                hooks.append_request_log(json!({
                    "model": model,
                    "provider": provider,
                    "connectionId": connection_id,
                    "status": format!("FAILED {status}"),
                }));
            }
            if matches!(error, ExecError::Cancelled) {
                return ChatResult::error(499, "Request aborted");
            }
            let err_msg = format_provider_error(
                Some(&http_status::BAD_GATEWAY.to_string()),
                &error.to_string(),
                None,
                None,
            );
            if let Some(log) = &log {
                let elapsed = router_db::time::now_ms() - request_start_time_ms;
                log.error_line(
                    req_tag,
                    "✗",
                    &format!("ERROR 502 · {provider}/{model} · {elapsed}ms\n    {err_msg}"),
                );
            }
            return ChatResult::error(http_status::BAD_GATEWAY, &err_msg);
        }
    };

    // 401/403: refresh the token and retry once (skipped for no-auth providers).
    if !executor.no_auth()
        && (provider_response.status == http_status::UNAUTHORIZED
            || provider_response.status == http_status::FORBIDDEN)
    {
        let refreshed = refresh_with_retry(credentials, 3, |creds: &Credentials| {
            let creds = creds.clone();
            let executor = Arc::clone(&executor);
            let proxy_options = proxy_options.clone();
            async move {
                executor
                    .refresh_credentials(&creds, exec_log_line(), &proxy_options)
                    .await
            }
        })
        .await;

        match refreshed {
            Some(new_credentials)
                if new_credentials.access_token.is_some()
                    || new_credentials.copilot_token.is_some() =>
            {
                if let Some(log) = &log {
                    log.line(
                        req_tag,
                        "🔑",
                        &format!("TOKEN REFRESHED · {provider}/{model}"),
                    );
                }
                let patch = merge_refreshed_credentials(
                    provider,
                    credentials,
                    &new_credentials,
                    router_db::time::now_ms(),
                );
                if let Some(patch) = &patch {
                    apply_refresh_patch(credentials, patch);
                }
                if let Some(callback) = &on_credentials_refreshed {
                    callback(patch.unwrap_or_else(|| json!({})));
                }
                if let Ok(retry) = executor
                    .execute(ExecuteRequest {
                        model,
                        body: translated_body.clone(),
                        stream,
                        credentials,
                        cancel: cancel.clone(),
                        log: exec_log_line(),
                        proxy_options: proxy_options.clone(),
                        provider_session_id: Some(session_seed.as_str()),
                        client_tool,
                    })
                    .await
                    && retry.status < 400
                {
                    provider_response = retry;
                }
            }
            Some(_) | None => {
                if let Some(log) = &log {
                    log.error_line(
                        req_tag,
                        "⚠",
                        &format!("TOKEN REFRESH FAILED · {provider}/{model}"),
                    );
                }
            }
        }
    }

    if provider_response.status >= 400 {
        if let Some(hooks) = &hooks {
            hooks.track_pending_request(Some(model), Some(provider), connection_id, false, true);
        }
        let status = provider_response.status;
        let headers = provider_response.headers.clone();
        let body_text = provider_response.text().await.unwrap_or_default();
        let parsed = executor.parse_error(status, &body_text);
        let upstream = parse_upstream_error(status, &body_text, Some(&parsed));
        if let Some(hooks) = &hooks {
            hooks.append_request_log(json!({
                "model": model,
                "provider": provider,
                "connectionId": connection_id,
                "status": format!("FAILED {}", upstream.status_code),
            }));
        }
        let err_msg = format_provider_error(
            Some(&upstream.status_code.to_string()),
            &upstream.message,
            None,
            None,
        );
        if let Some(log) = &log {
            let elapsed = router_db::time::now_ms() - request_start_time_ms;
            log.error_line(
                req_tag,
                "✗",
                &format!(
                    "ERROR {} · {provider}/{model} · {elapsed}ms\n    {err_msg}",
                    upstream.status_code
                ),
            );
        }
        return ChatResult::error_result(
            upstream.status_code,
            &err_msg,
            upstream.resets_at_ms,
            upstream_response_headers(&headers),
        );
    }

    let provider_response_format = target_format.clone();

    let custom_set: HashSet<String> = custom_tool_names.iter().cloned().collect();
    let fire_success = || {
        if let Some(callback) = &on_request_success
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let callback = Arc::clone(callback);
            handle.spawn(async move { callback() });
        }
    };

    // Provider forced streaming, client asked for JSON: fold the SSE back.
    if !client_requested_streaming && provider_requires_streaming {
        let ctx = ChatContext {
            provider,
            model,
            source_format,
            target_format: &provider_response_format,
            user_agent,
            body: &body,
            stream,
            request_start_time_ms,
            connection_id,
            api_key,
            client_endpoint,
            tool_name_map: tool_name_map.as_ref(),
            custom_tool_names: Some(&custom_set),
            req_tag,
            log: log.clone(),
            credentials: Some(credentials),
            on_stream_complete: None,
            on_request_success: on_request_success.clone(),
        };
        match handle_forced_sse_to_json(provider_response, &ctx).await {
            Ok(result) => {
                fire_success();
                finish_non_streaming(&result, &hooks, model, provider, connection_id);
                return result;
            }
            Err(response) => provider_response = response,
        }
    }

    if !stream {
        let ctx = ChatContext {
            provider,
            model,
            source_format,
            target_format: &provider_response_format,
            user_agent,
            body: &body,
            stream,
            request_start_time_ms,
            connection_id,
            api_key,
            client_endpoint,
            tool_name_map: tool_name_map.as_ref(),
            custom_tool_names: Some(&custom_set),
            req_tag,
            log: log.clone(),
            credentials: Some(credentials),
            on_stream_complete: None,
            on_request_success: on_request_success.clone(),
        };
        let result = handle_non_streaming_response(provider_response, &ctx).await;
        fire_success();
        finish_non_streaming(&result, &hooks, model, provider, connection_id);
        return result;
    }

    let on_stream_complete = build_on_stream_complete(
        provider.to_string(),
        model.to_string(),
        connection_id.map(str::to_string),
        api_key.map(str::to_string),
        request_start_time_ms,
        client_endpoint.map(str::to_string),
        req_tag.to_string(),
        log.clone(),
        save_usage,
    );

    let streaming = handle_streaming_response(
        provider_response,
        StreamingRequest {
            provider,
            model,
            source_format,
            target_format: &provider_response_format,
            user_agent,
            body: &body,
            stream,
            request_start_time_ms,
            connection_id,
            api_key,
            client_endpoint,
            tool_name_map,
            custom_tool_names,
            req_tag,
            log,
            credentials: Some(credentials),
            on_stream_complete: Some(on_stream_complete),
            on_request_success,
            hooks: hooks.clone(),
        },
        cancel,
    )
    .await;

    ChatResult {
        status: streaming.status,
        headers: streaming.headers,
        body: match streaming.body {
            StreamingBody::Bytes(bytes) => ChatBody::Bytes(bytes),
            StreamingBody::Stream(stream) => ChatBody::Stream(stream),
        },
        log: None,
        usage_stats: None,
        error: if streaming.success {
            None
        } else {
            Some(format!("upstream non-SSE: {}", streaming.status))
        },
        resets_at_ms: None,
    }
}

/// The non-streaming/forced-SSE tail: `trackDone()` then
/// `appendLog({tokens, status: "200 OK"})`. The handlers return those fields
/// rather than calling the hooks, so the orchestrator fires them here.
fn finish_non_streaming(
    result: &ChatResult,
    hooks: &Option<Arc<dyn StreamHooks>>,
    model: &str,
    provider: &str,
    connection_id: Option<&str>,
) {
    let Some(hooks) = hooks else {
        return;
    };
    hooks.track_pending_request(Some(model), Some(provider), connection_id, false, false);
    if let Some(log) = &result.log {
        let mut entry = json!({
            "model": model,
            "provider": provider,
            "connectionId": connection_id,
        });
        if let (Some(obj), Some(extra)) = (entry.as_object_mut(), log.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        hooks.append_request_log(entry);
    }
}
