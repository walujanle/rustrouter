//! The SSE streaming path.
//!
//! Three things live here, and the seams between them matter:
//!
//! * [`build_transform_stream`] picks the transform — a Responses-provider
//!   (Codex) source is decoded into the client's format, a format mismatch goes
//!   through `translate_response`, everything else passes through. It returns
//!   the `Fn(ByteStream) -> ByteStream` shape `utils/stream.rs` produces.
//! * [`handle_streaming_response`] guards against a non-SSE upstream body
//!   (an HTML error page) by converting it to a sanitised JSON error, then
//!   pipes the stream through `pipe_with_disconnect` with the terminal bytes
//!   the abort path emits.
//! * [`build_on_stream_complete`] builds the callback that persists the final
//!   usage row and logs the `📊 done` line.
//!
//! DB-free: the `saveUsage` callback arrives injected, and it returns the
//! canonicalised usage row for the server layer to write.
//!
//! `pipe_with_disconnect` comes from `utils/stream_handler.rs`. The
//! `AbortController` contract becomes a `tokio_util::sync::CancellationToken`
//! (`docs/CHAT-PIPELINE.md`), so the assumed contract is
//! `pipe_with_disconnect(input, transform, cancel, on_abort_terminal, stall_timeout_ms) -> ByteStream`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::credentials::Credentials;
use crate::executors::executor::{ByteStream, UpstreamResponse};
use crate::handlers::chat_core::non_streaming::upstream_response_headers;
use crate::handlers::chat_core::request_detail::{format_done_line, save_usage_stats};
use crate::handlers::chat_core::sse_to_json::is_responses_provider;
use crate::handlers::chat_core::{ChatLog, RequestSuccessFn};
use crate::providers::registry::registry;
use crate::runtime_config::{SSE_KEEPALIVE_INTERVAL_MS, STREAM_STALL_TIMEOUT_MS, http_status};
use crate::translator::{formats, needs_translation};
use crate::utils::responses_stream_helpers::build_aborted_responses_terminal_bytes;
use crate::utils::sse::SSE_HEADERS_CORS;
use crate::utils::stream::{
    SseStreamOptions, StreamCompleteFn, StreamHooks, StreamMode, create_sse_stream,
};
use crate::utils::stream_handler::{AbortTerminalFn, pipe_with_disconnect};
use crate::utils::stream_helpers::build_stream_error_bytes;
use crate::utils::usage_tracking::estimate_input_tokens;

/// `CODEX_SOURCE_TO_TARGET`: which client format a Responses-provider stream is
/// translated into, by the request's source format. An unknown source falls back
/// to OpenAI.
fn codex_source_to_target(source_format: &str) -> &'static str {
    match source_format {
        formats::OPENAI_RESPONSES => formats::OPENAI_RESPONSES,
        formats::CLAUDE => formats::CLAUDE,
        _ => formats::OPENAI,
    }
}

/// `saveRequestUsage(row)`.
pub type SaveUsageFn = Arc<dyn Fn(Value) + Send + Sync>;

/// `buildTransformStream({…})`.
#[allow(clippy::too_many_arguments)]
pub fn build_transform_stream(
    provider: &str,
    source_format: &str,
    target_format: &str,
    user_agent: Option<&str>,
    tool_name_map: Option<HashMap<String, String>>,
    custom_tool_names: Vec<String>,
    model: Option<&str>,
    connection_id: Option<&str>,
    body: Option<&Value>,
    on_stream_complete: Option<StreamCompleteFn>,
    api_key: Option<&str>,
    credentials: Option<&Credentials>,
    hooks: Option<Arc<dyn StreamHooks>>,
) -> impl Fn(ByteStream) -> ByteStream {
    let lower = user_agent.unwrap_or("").to_lowercase();
    let is_droid_cli = lower.contains("droid") || lower.contains("codex-cli");
    // Responses-API providers (e.g. codex) emit Responses SSE, so the stream is
    // translated into the client's format rather than passed through.
    let responses_provider = is_responses_provider(provider);
    let needs_codex_translation =
        responses_provider && target_format == formats::OPENAI_RESPONSES && !is_droid_cli;

    let options = if needs_codex_translation {
        let codex_target = codex_source_to_target(source_format);
        let mut options = SseStreamOptions::new(StreamMode::Translate, codex_target);
        options.target_format = Some(formats::OPENAI_RESPONSES.to_string());
        options
    } else if needs_translation(target_format, source_format) {
        let mut options = SseStreamOptions::new(StreamMode::Translate, source_format);
        options.target_format = Some(target_format.to_string());
        options
    } else {
        // The passthrough builder omits `sourceFormat`; `openai` is this
        // crate's default for that field.
        SseStreamOptions::new(StreamMode::Passthrough, formats::OPENAI)
    };

    let options = SseStreamOptions {
        provider: Some(provider.to_string()),
        tool_name_map,
        custom_tool_names,
        model: model.map(str::to_string),
        connection_id: connection_id.map(str::to_string),
        input_tokens: body.map(estimate_input_tokens).unwrap_or(0),
        api_key: api_key.map(str::to_string),
        credentials: credentials.cloned(),
        on_stream_complete,
        hooks,
        ..options
    };
    create_sse_stream(options)
}

/// What `handleStreamingResponse` returns. The crate is HTTP-free, so the
/// response is a status + header list + body, not an axum type.
pub struct StreamingResult {
    pub success: bool,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: StreamingBody,
}

/// The response body: a buffered error page or the live transformed stream.
pub enum StreamingBody {
    Bytes(Vec<u8>),
    Stream(ByteStream),
}

/// The context `handleStreamingResponse` reads.
pub struct StreamingRequest<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub source_format: &'a str,
    pub target_format: &'a str,
    pub user_agent: Option<&'a str>,
    pub body: &'a Value,
    pub stream: bool,
    pub request_start_time_ms: i64,
    pub connection_id: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub client_endpoint: Option<&'a str>,
    pub tool_name_map: Option<HashMap<String, String>>,
    pub custom_tool_names: Vec<String>,
    pub req_tag: &'a str,
    pub log: Option<Arc<dyn ChatLog>>,
    pub credentials: Option<&'a Credentials>,
    pub on_stream_complete: Option<StreamCompleteFn>,
    pub on_request_success: Option<RequestSuccessFn>,
    /// `trackPendingRequest` / `appendRequestLog` from the streaming flush.
    pub hooks: Option<Arc<dyn StreamHooks>>,
}

/// `handleStreamingResponse({…})`.
pub async fn handle_streaming_response(
    provider_response: UpstreamResponse,
    request: StreamingRequest<'_>,
    cancel: Option<CancellationToken>,
) -> StreamingResult {
    if let Some(on_request_success) = &request.on_request_success {
        // `Promise.resolve().then(onRequestSuccess).catch(…)`: the failure must
        // not affect the stream. Without a runtime the call runs inline.
        let callback = Arc::clone(on_request_success);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()))
                        .is_err()
                    {
                        tracing::error!("[ChatCore] onRequestSuccess failed");
                    }
                });
            }
            Err(_) => callback(),
        }
    }

    // When the upstream returns HTML/text instead of SSE (a Cloudflare 5xx page,
    // say), piping it through the SSE transform crashes the client. Read the
    // body, pull a short message from the <title>, sanitise it, and return a
    // clean JSON error. Untrusted upstream text must never reach the client
    // verbatim — the UI may render error.message as HTML.
    let upstream_status = provider_response.status;
    let headers = provider_response.headers.clone();
    let upstream_content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if !upstream_content_type.is_empty()
        && !upstream_content_type.contains("text/event-stream")
        && !upstream_content_type.contains("application/json")
    {
        let body_text = provider_response.text().await.unwrap_or_default();
        let short_msg = sanitize_upstream_message(&body_text, &upstream_content_type);
        let status = if upstream_status == 0 {
            http_status::BAD_GATEWAY
        } else {
            upstream_status
        };
        if let Some(log) = &request.log {
            log.error_line(
                request.req_tag,
                "✗",
                &format!(
                    "BLOCKED {status} · {}/{} · non-SSE ({upstream_content_type})\n    {short_msg}",
                    request.provider, request.model
                ),
            );
        } else {
            tracing::warn!(
                target: "router_sse::chat_core",
                "[STREAM] {} | {} | blocked pipe: {} [{}]",
                request.provider,
                request.model,
                short_msg,
                status
            );
        }
        // `streamController?.handleError?.(new Error("upstream non-SSE: …"))`:
        // the stream stops here, which is what cancelling the token does.
        if let Some(cancel) = &cancel {
            cancel.cancel();
        }
        return StreamingResult {
            success: false,
            status,
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
            ],
            body: StreamingBody::Bytes(
                json!({"error": {"message": format!("[{status}]: {short_msg}")}})
                    .to_string()
                    .into_bytes(),
            ),
        };
    }

    let transform_stream = build_transform_stream(
        request.provider,
        request.source_format,
        request.target_format,
        request.user_agent,
        request.tool_name_map.clone(),
        request.custom_tool_names.clone(),
        Some(request.model),
        request.connection_id,
        Some(request.body),
        request.on_stream_complete.clone(),
        request.api_key,
        request.credentials,
        request.hooks.clone(),
    );

    // Terminal bytes when the stream aborts after HTTP 200 was already sent, so
    // the client sees a real error instead of a silently truncated stream. A
    // Responses passthrough keeps its own response.failed shape; every other
    // client format gets the OpenAI error frame + [DONE] (or `event: error` for
    // Claude).
    let is_responses_passthrough = request.source_format == formats::OPENAI_RESPONSES
        && request.target_format == formats::OPENAI_RESPONSES;
    let on_abort_terminal: AbortTerminalFn = if is_responses_passthrough {
        Arc::new(|_message| build_aborted_responses_terminal_bytes())
    } else {
        let source_format = request.source_format.to_string();
        Arc::new(move |message| {
            build_stream_error_bytes(
                http_status::GATEWAY_TIMEOUT,
                message,
                Some(source_format.as_str()),
            )
        })
    };
    let stall_timeout_ms = registry()
        .transport(request.provider)
        .and_then(|t| t.stall_timeout_ms)
        .filter(|v| *v > 0)
        .map(|v| v as u64)
        .unwrap_or(STREAM_STALL_TIMEOUT_MS);
    // `into_byte_stream()` is the executor contract's uniform body view — the
    // stream handler must not care which executor produced it.
    let transformed_body = pipe_with_disconnect(
        provider_response.into_byte_stream(),
        transform_stream,
        cancel,
        Some(on_abort_terminal),
        stall_timeout_ms,
        SSE_KEEPALIVE_INTERVAL_MS,
    );

    let mut out_headers: Vec<(String, String)> = SSE_HEADERS_CORS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    out_headers.extend(upstream_response_headers(&headers));

    StreamingResult {
        success: true,
        status: 200,
        headers: out_headers,
        body: StreamingBody::Stream(transformed_body),
    }
}

static TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<title>([^<]+)</title>").expect("title regex is valid"));
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]*>").expect("tag regex is valid"));
static NEWLINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\r\n]+").expect("newline regex is valid"));

/// The `<title>` extraction, tag strip, newline collapse, trim and 160-char
/// clamp — the sanitisation that keeps an upstream HTML page from reaching the
/// client verbatim.
fn sanitize_upstream_message(body_text: &str, content_type: &str) -> String {
    let (title_re, tag_re, newline_re) = (&*TITLE_RE, &*TAG_RE, &*NEWLINE_RE);

    let title = title_re
        .captures(body_text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
        .unwrap_or("");
    let sanitized_title: String = newline_re
        .replace_all(&tag_re.replace_all(title, ""), " ")
        .trim()
        .chars()
        .take(160)
        .collect();
    if !sanitized_title.is_empty() {
        return sanitized_title;
    }

    if body_text.chars().count() < 200 {
        tag_re
            .replace_all(body_text, "")
            .trim()
            .chars()
            .take(160)
            .collect()
    } else {
        format!("Upstream returned non-SSE response ({content_type})")
    }
}

/// `buildOnStreamComplete({…})`.
///
/// The callback persists the stream's usage row and logs the `📊 done` line.
#[allow(clippy::too_many_arguments)]
pub fn build_on_stream_complete(
    provider: String,
    model: String,
    connection_id: Option<String>,
    api_key: Option<String>,
    request_start_time_ms: i64,
    client_endpoint: Option<String>,
    req_tag: String,
    log: Option<Arc<dyn ChatLog>>,
    save_usage: SaveUsageFn,
) -> StreamCompleteFn {
    let callback = move |usage: Option<Value>, ttft_at: Option<i64>| {
        let now = router_db::time::now_ms();
        let elapsed = now - request_start_time_ms;
        let latency = json!({
            // `ttftAt ? ttftAt - start : now - start`: a zero ttft is falsy and
            // falls back to the elapsed time.
            "ttft": ttft_at.filter(|t| *t != 0).map_or(elapsed, |t| t - request_start_time_ms),
            "total": elapsed
        });

        // Persist the stream usage row (the "📊 done" line below is authoritative).
        if let Some(row) = save_usage_stats(
            &provider,
            &model,
            usage.as_ref(),
            connection_id.as_deref(),
            api_key.as_deref(),
            client_endpoint.as_deref(),
            "STREAM USAGE",
            true,
        ) {
            save_usage(row);
        }
        if let Some(log) = &log {
            log.line(&req_tag, "📊", &format_done_line(usage.as_ref(), &latency));
        }
    };

    Arc::new(callback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use reqwest::header::HeaderMap;

    use crate::executors::executor::UpstreamBody;

    fn text_response(status: u16, content_type: &str, body: &str) -> UpstreamResponse {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", content_type.parse().unwrap());
        UpstreamResponse {
            status,
            headers,
            body: UpstreamBody::Buffered(Bytes::from(body.to_string())),
            url: "https://upstream.test".into(),
            request_headers: HeaderMap::new(),
        }
    }

    fn guard_request<'a>(body: &'a Value) -> StreamingRequest<'a> {
        StreamingRequest {
            provider: "cloudflare",
            model: "gpt",
            source_format: formats::OPENAI,
            target_format: formats::OPENAI,
            user_agent: None,
            body,
            stream: true,
            request_start_time_ms: 0,
            connection_id: None,
            api_key: None,
            client_endpoint: None,
            tool_name_map: None,
            custom_tool_names: Vec::new(),
            req_tag: "🟢",
            log: None,
            credentials: None,
            on_stream_complete: None,
            on_request_success: None,
            hooks: None,
        }
    }

    #[tokio::test]
    async fn non_sse_guard_uses_the_sanitised_title() {
        let response = text_response(
            502,
            "text/html",
            "<html><head><title>  Cloudflare\nError  </title></head><body><p>x</p></body></html>",
        );
        let body = json!({"model": "gpt"});
        let result = handle_streaming_response(response, guard_request(&body), None).await;
        assert!(!result.success);
        assert_eq!(result.status, 502);
        match result.body {
            StreamingBody::Bytes(bytes) => {
                let text = String::from_utf8(bytes).unwrap();
                assert_eq!(text, r#"{"error":{"message":"[502]: Cloudflare Error"}}"#);
            }
            StreamingBody::Stream(_) => panic!("guard path must buffer the body"),
        }
        assert_eq!(result.headers[0].0, "Content-Type");
        assert_eq!(result.headers[1].1, "*");
    }

    #[tokio::test]
    async fn non_sse_guard_strips_tags_from_a_short_body() {
        let response = text_response(503, "text/plain", "<b>oops</b> upstream");
        let body = json!({});
        let result = handle_streaming_response(response, guard_request(&body), None).await;
        match result.body {
            StreamingBody::Bytes(bytes) => {
                assert_eq!(
                    String::from_utf8(bytes).unwrap(),
                    r#"{"error":{"message":"[503]: oops upstream"}}"#
                );
            }
            StreamingBody::Stream(_) => panic!("guard path must buffer the body"),
        }
    }

    #[tokio::test]
    async fn non_sse_guard_falls_back_for_a_long_body_without_a_title() {
        let long = "x".repeat(300);
        let response = text_response(500, "text/html", &long);
        let body = json!({});
        let result = handle_streaming_response(response, guard_request(&body), None).await;
        match result.body {
            StreamingBody::Bytes(bytes) => {
                assert_eq!(
                    String::from_utf8(bytes).unwrap(),
                    r#"{"error":{"message":"[500]: Upstream returned non-SSE response (text/html)"}}"#
                );
            }
            StreamingBody::Stream(_) => panic!("guard path must buffer the body"),
        }
    }

    #[test]
    fn codex_source_to_target_keeps_known_formats_and_falls_back() {
        assert_eq!(codex_source_to_target(formats::CLAUDE), formats::CLAUDE);
        assert_eq!(
            codex_source_to_target(formats::OPENAI_RESPONSES),
            formats::OPENAI_RESPONSES
        );
        assert_eq!(codex_source_to_target("unknown"), formats::OPENAI);
    }

    #[test]
    fn on_stream_complete_persists_usage_and_logs_the_done_line() {
        use std::sync::Mutex;

        let usages: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        struct TestLog(Arc<Mutex<Vec<String>>>);
        impl ChatLog for TestLog {
            fn line(&self, _tag: &str, _symbol: &str, message: &str) {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(message.to_string());
            }
            fn error_line(&self, _tag: &str, _symbol: &str, _message: &str) {}
        }
        impl crate::executors::executor::ExecutorLog for TestLog {
            fn debug(&self, _tag: &str, _message: &str) {}
            fn info(&self, _tag: &str, _message: &str) {}
            fn error(&self, _tag: &str, _message: &str) {}
        }

        let usages_cb = Arc::clone(&usages);
        let on_complete = build_on_stream_complete(
            "codex".into(),
            "gpt".into(),
            Some("conn-1".into()),
            Some("sk-1".into()),
            1_000,
            Some("/v1/chat".into()),
            "🟢".into(),
            Some(Arc::new(TestLog(Arc::clone(&lines)))),
            Arc::new(move |row| {
                usages_cb
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(row)
            }),
        );

        on_complete(
            Some(json!({"prompt_tokens": 10, "completion_tokens": 4})),
            Some(1_500),
        );

        assert_eq!(usages.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
        let line = lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .cloned()
            .unwrap();
        assert!(line.starts_with("DONE "), "{line}");
        assert!(line.contains("TTFT 500ms"), "{line}");
        assert!(line.contains("IN 10"), "{line}");
        assert!(line.contains("OUT 4"), "{line}");
    }
}
