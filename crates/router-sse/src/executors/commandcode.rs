//! `https://api.commandcode.ai/alpha/generate`, which answers AI SDK v5 NDJSON
//! rather than SSE.
//!
//! The executor is the decoder. The upstream body is read line by line, each
//! line translated to an OpenAI `chat.completion.chunk` by
//! `translator::response::commandcode_to_openai`, and re-framed as SSE so both
//! the streaming and the forced-SSE-to-JSON downstream handlers can consume it
//! without a second format translation.
//!
//! Three behaviours are load-bearing and easy to lose:
//!
//! * **The stream is peeked, not consumed.** Only the lines up to the first
//!   client-visible event are buffered and replayed; the rest of the upstream
//!   stream passes through untouched. Buffering the whole body would break the
//!   first-token latency the streaming handler depends on.
//! * **An `error` event becomes an HTTP error response**, not a chunk. The
//!   upstream reports mid-stream failures as a normal line, and turning that
//!   into fake `finish_reason: "stop"` content would hand the client a silent
//!   truncation.
//! * **A `502`/`503`/`504` wrapper is retried here**, not by the base loop: the
//!   status is only knowable after the first lines are read, so the base's
//!   status-based retry never sees it.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};

use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::{
    ByteStream, ExecError, ExecuteRequest, Executor, UpstreamBody, UpstreamResponse, insert_header,
};
use crate::providers::model::Transport;
use crate::runtime_config::http_status;
use crate::translator::ResponseState;
use crate::translator::concerns::primitives::{js_string, js_truthy};
use crate::translator::response::commandcode_to_openai::commandcode_to_openai_response;
use crate::utils::sse::SSE_DONE;

/// How many times a retryable wrapper status is retried.
const MAX_RETRIES: u32 = 2;

/// The event types that stop the peek: each one is something the client can
/// see, so the wrapper is not needed yet.
const CLIENT_VISIBLE_EVENTS: [&str; 6] = [
    "text-delta",
    "reasoning-delta",
    "tool-input-start",
    "tool-call",
    "finish",
    "finish-step",
];

// ─── error classification ────────────────────────────────────────────────

/// Classify an upstream error event into `(status, message, type)`.
///
/// The status is guessed from the message text when the event carries nothing
/// usable, because the upstream error shape is not stable.
pub fn parse_command_code_error(event: &Value) -> (u16, String, String) {
    let Some(obj) = event.as_object() else {
        return (
            503,
            "CommandCode upstream error".to_string(),
            "server_error".to_string(),
        );
    };

    // `event.error ?? event.message ?? "unknown"`.
    let err_val = obj
        .get("error")
        .filter(|v| !v.is_null())
        .or_else(|| obj.get("message").filter(|v| !v.is_null()))
        .cloned()
        .unwrap_or_else(|| json!("unknown"));

    let message: String;
    let mut status_code: Option<i64> = None;
    let mut error_type = "server_error".to_string();

    match &err_val {
        Value::Object(err) => {
            // `errVal.message || errVal.error || JSON.stringify(errVal)`.
            message = err
                .get("message")
                .filter(|v| js_truthy(v))
                .or_else(|| err.get("error").filter(|v| js_truthy(v)))
                .map(js_string)
                .unwrap_or_else(|| serde_json::to_string(&err_val).unwrap_or_default());
            // `Number.isInteger(Number(v))`: a numeric string counts too.
            status_code = err
                .get("statusCode")
                .and_then(integer_of)
                .or_else(|| err.get("status").and_then(integer_of));
            if let Some(t) = err.get("type").filter(|v| js_truthy(v)) {
                error_type = js_string(t);
            }
        }
        Value::String(s) => message = s.clone(),
        other => message = serde_json::to_string(other).unwrap_or_default(),
    }

    if let Some(from_event) = obj.get("statusCode").and_then(integer_of) {
        status_code = Some(from_event);
    }

    // `!statusCode || statusCode < 400 || statusCode > 599`.
    if !status_code.is_some_and(|c| (400..=599).contains(&c)) {
        let lower = message.to_lowercase();
        let (code, kind) = if lower.contains("rate limit") || lower.contains("too many requests") {
            (429, "rate_limit_error")
        } else if lower.contains("unauthorized")
            || lower.contains("invalid api key")
            || lower.contains("authentication")
        {
            (401, "authentication_error")
        } else if lower.contains("payment required") || lower.contains("billing") {
            (402, "billing_error")
        } else if lower.contains("quota")
            || lower.contains("forbidden")
            || lower.contains("permission")
        {
            (403, "permission_error")
        } else if lower.contains("not found") {
            (404, "invalid_request_error")
        } else {
            (503, "server_error")
        };
        status_code = Some(code);
        error_type = kind.to_string();
    }

    (status_code.unwrap_or(503) as u16, message, error_type)
}

/// `TextDecoder.decode(value, { stream: true })`.
///
/// A character split across two reads is held back until its remaining bytes
/// arrive. Decoding each chunk on its own with `from_utf8_lossy` would replace
/// both halves with U+FFFD, and the mangled line would then fail to parse and be
/// dropped.
fn decode_chunk(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let mut out = String::new();

    loop {
        // The split points are copied out first: matching on the borrowed
        // `Result` would hold the borrow across the `drain` below.
        let split = std::str::from_utf8(&pending[..])
            .err()
            .map(|e| (e.valid_up_to(), e.error_len()));
        let Some((valid, error_len)) = split else {
            out.push_str(&String::from_utf8_lossy(&pending[..]));
            pending.clear();
            break;
        };
        out.push_str(&String::from_utf8_lossy(&pending[..valid]));
        match error_len {
            // Truncated: the rest of the character is still in flight.
            None => {
                pending.drain(..valid);
                break;
            }
            // Bytes that can never form a character: replace, then resume.
            Some(len) => {
                out.push(char::REPLACEMENT_CHARACTER);
                pending.drain(..valid + len);
            }
        }
    }

    out
}

/// `Number.isInteger(Number(v)) ? Number(v) : null`.
fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        // `Number("")` is 0, which `Number.isInteger` accepts; `Number(" 12 ")`
        // is 12; anything with a trailing character is `NaN` and rejected.
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(0)
            } else {
                trimmed.parse::<i64>().ok()
            }
        }
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

// ─── NDJSON → SSE ────────────────────────────────────────────────────────

/// The verdict after reading the upstream head.
enum Inspection {
    /// An `error` event was found: answer with this instead of the stream.
    Error {
        status: u16,
        message: String,
        error_type: String,
    },
    /// The lines to replay ahead of the untouched remainder of the stream.
    PassThrough { prefix: String, rest: ByteStream },
    /// A read error during the peek: hand the original response back instead of
    /// wrapping it, so nothing is re-framed and the upstream status and headers
    /// stand. The bytes not yet read are still here; the bytes already consumed
    /// are gone either way.
    Unwrapped { rest: ByteStream },
}

/// Read the upstream head and decide how to answer it.
///
/// The synthesized chunks carry `model`; that happens in `frame_as_sse`, which
/// `execute` calls with the same value.
async fn inspect_and_wrap(response: reqwest::Response, _model: &str) -> Inspection {
    let mut upstream: ByteStream = Box::pin(
        response
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other)),
    );
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = String::new();
    let mut buffered_lines: Vec<String> = Vec::new();
    let mut detected_error: Option<Value> = None;
    // A peek that stopped early leaves a partial line that must not be parsed
    // as if the stream had ended.
    let mut stop_loop = false;

    while let Some(chunk) = upstream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return Inspection::Unwrapped { rest: upstream },
        };
        buffer.push_str(&decode_chunk(&mut pending, &chunk));

        let mut lines: Vec<String> = buffer.split('\n').map(str::to_string).collect();
        buffer = lines.pop().unwrap_or_default();

        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let json_str = trimmed
                .strip_prefix("data:")
                .map(str::trim)
                .unwrap_or(trimmed);
            if json_str.is_empty() || json_str == "[DONE]" {
                buffered_lines.push(trimmed.to_string());
                stop_loop = true;
                break;
            }

            let Ok(event) = serde_json::from_str::<Value>(json_str) else {
                buffered_lines.push(trimmed.to_string());
                continue;
            };

            if event.get("type").and_then(Value::as_str) == Some("error") {
                detected_error = Some(event);
                stop_loop = true;
                break;
            }

            buffered_lines.push(trimmed.to_string());
            if event
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| CLIENT_VISIBLE_EVENTS.contains(&t))
            {
                stop_loop = true;
                break;
            }
        }
        if stop_loop {
            break;
        }
    }

    // Whatever is left is parsed one last time, so a short body that ends
    // mid-line still reports its error rather than being replayed as a chunk.
    if !stop_loop {
        let trimmed = buffer.trim().to_string();
        if !trimmed.is_empty() {
            let json_str = trimmed
                .strip_prefix("data:")
                .map(str::trim)
                .unwrap_or(trimmed.as_str());
            match serde_json::from_str::<Value>(json_str) {
                Ok(parsed) if parsed.get("type").and_then(Value::as_str) == Some("error") => {
                    detected_error = Some(parsed);
                }
                _ => buffered_lines.push(trimmed),
            }
        }
    }

    if let Some(event) = detected_error {
        let (status, message, error_type) = parse_command_code_error(&event);
        return Inspection::Error {
            status,
            message,
            error_type,
        };
    }

    // The replay prefix: the buffered lines, then the partial line that was not
    // an event yet, joined so the decoder sees the same byte sequence. A
    // character still mid-flight is dropped here — the decoder's held-back bytes
    // do not survive into the replay stream.
    //
    // The tail parsed above is *also* still in `buffer`, so both are passed to
    // the replay: a body whose last line has no trailing newline is replayed
    // twice. Kept as-is; the duplicate is visible to the client, so "fixing" it
    // here would change observable output.
    let mut prefix = buffered_lines.join("\n");
    if !prefix.is_empty() && !buffer.is_empty() {
        prefix.push('\n');
        prefix.push_str(&buffer);
    } else if !buffer.is_empty() {
        prefix = buffer;
    } else if !prefix.is_empty() {
        prefix.push('\n');
    }

    Inspection::PassThrough {
        prefix,
        rest: upstream,
    }
}

/// Re-frame the replayed prefix and the live remainder as an SSE byte stream.
///
/// The prefix is fed through the same line decoder as the live bytes rather than
/// emitted directly, and `SSE_DONE` is appended once, after the upstream ends.
fn frame_as_sse(prefix: String, mut rest: ByteStream, model: &str) -> ByteStream {
    let state = ResponseState {
        model: Some(model.to_string()),
        ..Default::default()
    };

    Box::pin(async_stream::stream! {
        let mut state = state;
        let mut buffer = String::new();
        let mut pending: Vec<u8> = Vec::new();

        let emit = |text: &str, state: &mut ResponseState, buffer: &mut String| -> Vec<Result<Bytes, std::io::Error>> {
            let mut out = Vec::new();
            buffer.push_str(text);
            let mut lines: Vec<String> = buffer.split('\n').map(str::to_string).collect();
            *buffer = lines.pop().unwrap_or_default();
            for line in lines {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                for chunk in commandcode_to_openai_response(&Value::String(trimmed.to_string()), state) {
                    out.push(Ok(Bytes::from(format!("data: {chunk}\n\n"))));
                }
            }
            out
        };

        for frame in emit(&prefix, &mut state, &mut buffer) {
            yield frame;
        }

        while let Some(chunk) = rest.next().await {
            match chunk {
                Ok(bytes) => {
                    let text = decode_chunk(&mut pending, &bytes);
                    for frame in emit(&text, &mut state, &mut buffer) {
                        yield frame;
                    }
                }
                // `controller.error(err)`: the failure propagates rather than
                // ending the stream as if it had completed.
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }

        // `flush`: whatever is left in the buffer, then the terminator. The
        // decoder is not called again here, so bytes held back for an incomplete
        // character are dropped rather than flushed.
        let tail = buffer.trim().to_string();
        if !tail.is_empty() {
            for chunk in commandcode_to_openai_response(&Value::String(tail), &mut state) {
                yield Ok(Bytes::from(format!("data: {chunk}\n\n")));
            }
        }
        yield Ok(Bytes::from(SSE_DONE));
    })
}

/// The headers for a detected error event, which is answered as JSON rather
/// than SSE.
fn json_error_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    let _ = insert_header(&mut headers, "Content-Type", "application/json");
    let _ = insert_header(&mut headers, "Access-Control-Allow-Origin", "*");
    headers
}

/// The SSE trio, then every upstream header, then `content-type` forced back to
/// `text/event-stream`.
fn sse_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let _ = insert_header(&mut headers, "Content-Type", "text/event-stream");
    let _ = insert_header(&mut headers, "Cache-Control", "no-cache");
    let _ = insert_header(&mut headers, "Connection", "keep-alive");
    for (name, value) in upstream.iter() {
        if let Ok(value) = value.to_str() {
            let _ = insert_header(&mut headers, name.as_str(), value);
        }
    }
    let _ = insert_header(&mut headers, "content-type", "text/event-stream");
    headers
}

// ─── executor ────────────────────────────────────────────────────────────

/// The CommandCode executor.
pub struct CommandCodeExecutor {
    inner: DefaultExecutor,
}

impl CommandCodeExecutor {
    pub fn new() -> Self {
        Self {
            inner: DefaultExecutor::new("commandcode"),
        }
    }
}

impl Default for CommandCodeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for CommandCodeExecutor {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn config(&self) -> &Transport {
        self.inner.config()
    }

    /// The upstream always streams, whatever the client asked for.
    fn transform_request(
        &self,
        _model: &str,
        mut body: Value,
        _stream: bool,
        _credentials: &Credentials,
    ) -> Value {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("stream".into(), json!(true));
        }
        body
    }

    /// The registry headers, a fresh session id per request, and `Bearer` for
    /// whichever token is present.
    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "Content-Type", "application/json")?;
        if let Some(configured) = self.config().headers.as_ref() {
            for (k, v) in configured {
                insert_header(
                    &mut headers,
                    k,
                    &crate::translator::concerns::primitives::js_string(v),
                )?;
            }
        }
        insert_header(
            &mut headers,
            "x-session-id",
            &uuid::Uuid::new_v4().to_string(),
        )?;

        // `credentials?.apiKey || credentials?.accessToken`.
        let token = credentials
            .api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                credentials
                    .access_token
                    .as_deref()
                    .filter(|s| !s.is_empty())
            });
        if let Some(token) = token {
            insert_header(&mut headers, "Authorization", &format!("Bearer {token}"))?;
        }

        if stream {
            insert_header(&mut headers, "Accept", "text/event-stream")?;
        }
        Ok(headers)
    }

    /// Extract the error message from a decoded body.
    ///
    /// There is no status text on a decoded body, so the message chain stops at
    /// the body itself; the final fallback is a generic string used when every
    /// earlier operand is empty.
    fn parse_error(&self, status: u16, body_text: &str) -> Value {
        let parsed: Option<Value> = serde_json::from_str(body_text).ok();
        // `parsed?.error || parsed` — a falsy `error` falls back to the payload.
        let err_obj = parsed
            .as_ref()
            .and_then(|p| p.get("error"))
            .filter(|v| js_truthy(v))
            .or(parsed.as_ref());

        let message = err_obj
            .and_then(|e| e.get("message"))
            .filter(|v| js_truthy(v))
            .or_else(|| {
                parsed
                    .as_ref()
                    .and_then(|p| p.get("message"))
                    .filter(|v| js_truthy(v))
            })
            .map(js_string)
            .or_else(|| Some(body_text.to_string()).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| format!("CommandCode upstream error: {status}"));

        // `Number(errObj?.code || errObj?.statusCode || response.status) || response.status`.
        let resolved = err_obj
            .and_then(|e| {
                e.get("code")
                    .filter(|v| js_truthy(v))
                    .or_else(|| e.get("statusCode").filter(|v| js_truthy(v)))
            })
            .map(js_string)
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| *n != 0)
            .map(|n| n as u16)
            .unwrap_or(status);

        json!({ "status": resolved, "message": message })
    }

    /// The base loop, then the NDJSON decode with a `502`/`503`/`504` retry
    /// around it.
    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        let mut attempt = 0u32;
        loop {
            // The base loop owns URL fallback, per-status retry and the connect
            // deadline; this layer only adds the wrapper retry on top.
            let inner_req = ExecuteRequest {
                model: req.model,
                body: req.body.clone(),
                stream: req.stream,
                credentials: req.credentials,
                cancel: req.cancel.clone(),
                log: req.log,
                proxy_options: req.proxy_options.clone(),
                provider_session_id: req.provider_session_id,
                client_tool: req.client_tool,
            };
            let mut result = self.inner.execute(inner_req).await?;

            // `!result?.response?.ok || !result.response.body` — an error status
            // is handed back untouched, wrapper and all.
            if !(200..300).contains(&result.status) {
                return Ok(result);
            }

            match result.take_body() {
                UpstreamBody::Stream(response) => {
                    match inspect_and_wrap(response, req.model).await {
                        Inspection::Error {
                            status,
                            message,
                            error_type,
                        } => {
                            result.status = status;
                            result.headers = json_error_headers();
                            result.body = UpstreamBody::Buffered(Bytes::from(
                                json!({
                                    "error": {
                                        "message": format!("[CommandCode error: {message}]"),
                                        "type": error_type,
                                        "code": status,
                                    }
                                })
                                .to_string(),
                            ));
                        }
                        Inspection::PassThrough { prefix, rest } => {
                            result.headers = sse_headers(&result.headers);
                            result.body =
                                UpstreamBody::Synthesized(frame_as_sse(prefix, rest, req.model));
                        }
                        // The original response is handed back, so the upstream
                        // headers and status survive; only the body type changes,
                        // because the response has already been taken apart into
                        // a stream.
                        Inspection::Unwrapped { rest } => {
                            result.body = UpstreamBody::Synthesized(rest);
                        }
                    }
                }
                // A buffered or synthesized body is not the NDJSON stream this
                // wrapper understands; hand it back untouched.
                other => {
                    result.body = other;
                    return Ok(result);
                }
            }

            let retryable = matches!(
                result.status,
                http_status::BAD_GATEWAY
                    | http_status::SERVICE_UNAVAILABLE
                    | http_status::GATEWAY_TIMEOUT
            );
            if retryable && attempt < MAX_RETRIES {
                if let Some(log) = req.log {
                    log.debug(
                        "RETRY",
                        &format!(
                            "CommandCode upstream returned status {}, retrying {}/{}...",
                            result.status,
                            attempt + 1,
                            MAX_RETRIES
                        ),
                    );
                }
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(1000 * u64::from(attempt))).await;
                continue;
            }
            return Ok(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_event_classifies_by_message_when_it_has_no_status() {
        let (status, message, kind) =
            parse_command_code_error(&json!({"error": {"message": "Rate limit exceeded"}}));
        assert_eq!(status, 429);
        assert_eq!(message, "Rate limit exceeded");
        assert_eq!(kind, "rate_limit_error");
    }

    #[test]
    fn an_explicit_status_code_wins_over_the_message() {
        let (status, _, kind) =
            parse_command_code_error(&json!({"error": {"message": "quota", "statusCode": 500}}));
        assert_eq!(status, 500);
        assert_eq!(kind, "server_error");
    }

    #[test]
    fn an_out_of_range_status_falls_through_to_the_text_heuristics() {
        let (status, _, _) = parse_command_code_error(
            &json!({"error": {"message": "unauthorized", "statusCode": 200}}),
        );
        assert_eq!(status, 401);
    }

    #[test]
    fn a_numeric_string_status_code_counts() {
        let (status, _, _) =
            parse_command_code_error(&json!({"error": {"message": "x", "status": "503"}}));
        assert_eq!(status, 503);
    }

    #[test]
    fn a_non_object_event_names_unknown() {
        let (status, message, kind) = parse_command_code_error(&json!("nope"));
        assert_eq!(status, 503);
        assert_eq!(message, "CommandCode upstream error");
        assert_eq!(kind, "server_error");
    }

    #[test]
    fn an_error_payload_serializes_when_it_carries_no_message() {
        let (_, message, _) = parse_command_code_error(&json!({"error": {"code": 7}}));
        assert_eq!(message, "{\"code\":7}");
    }

    #[test]
    fn the_event_level_status_code_beats_the_nested_one() {
        let (status, _, _) = parse_command_code_error(&json!({
            "statusCode": 404,
            "error": {"message": "missing", "statusCode": 500},
        }));
        assert_eq!(status, 404);
    }

    #[test]
    fn the_transformed_body_always_streams() {
        let executor = CommandCodeExecutor::new();
        let out = executor.transform_request(
            "m",
            json!({"stream": false}),
            false,
            &Credentials::default(),
        );
        assert_eq!(out["stream"], json!(true));
    }

    #[test]
    fn headers_carry_the_registry_values_and_a_fresh_session_id() {
        let executor = CommandCodeExecutor::new();
        let credentials = Credentials {
            api_key: Some("user_abc".into()),
            ..Default::default()
        };
        let headers = executor
            .build_headers(&credentials, true, "https://x", "m", None)
            .unwrap();
        assert_eq!(headers.get("x-command-code-version").unwrap(), "0.25.7");
        assert_eq!(headers.get("x-cli-environment").unwrap(), "cli");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer user_abc");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert!(
            uuid::Uuid::parse_str(headers.get("x-session-id").unwrap().to_str().unwrap()).is_ok()
        );

        // A second call mints a new session id.
        let again = executor
            .build_headers(&credentials, true, "https://x", "m", None)
            .unwrap();
        assert_ne!(headers.get("x-session-id"), again.get("x-session-id"));
    }

    #[test]
    fn parse_error_reads_the_nested_error_object() {
        let executor = CommandCodeExecutor::new();
        let out = executor.parse_error(500, r#"{"error": {"message": "boom", "code": 429}}"#);
        assert_eq!(out["status"], json!(429));
        assert_eq!(out["message"], json!("boom"));
    }

    #[test]
    fn parse_error_falls_back_to_the_status_when_the_body_is_empty() {
        let executor = CommandCodeExecutor::new();
        let out = executor.parse_error(502, "");
        assert_eq!(out["status"], json!(502));
        assert_eq!(out["message"], json!("CommandCode upstream error: 502"));
    }

    #[test]
    fn parse_error_takes_a_bare_payload_as_the_error_object() {
        let executor = CommandCodeExecutor::new();
        let out = executor.parse_error(500, r#"{"message": "flat"}"#);
        assert_eq!(out["message"], json!("flat"));
        assert_eq!(out["status"], json!(500));
    }

    #[test]
    fn a_character_split_across_reads_is_rejoined() {
        let mut pending = Vec::new();
        // "é" is two bytes; the first read ends between them.
        let first = decode_chunk(&mut pending, &[0x61, 0xc3]);
        assert_eq!(first, "a");
        assert_eq!(pending, vec![0xc3]);

        let second = decode_chunk(&mut pending, &[0xa9, 0x62]);
        assert_eq!(second, "éb");
        assert!(pending.is_empty());
    }

    #[test]
    fn an_invalid_byte_is_replaced_and_the_rest_survives() {
        let mut pending = Vec::new();
        let text = decode_chunk(&mut pending, &[0xff, 0x61]);
        assert_eq!(text, "\u{fffd}a");
        assert!(pending.is_empty());
    }

    #[test]
    fn the_wrapper_headers_are_the_sse_trio_plus_the_upstream_set() {
        let mut upstream = HeaderMap::new();
        upstream.insert("x-request-id", "abc".parse().unwrap());
        let headers = sse_headers(&upstream);
        assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
        assert_eq!(headers.get("cache-control").unwrap(), "no-cache");
        assert_eq!(headers.get("connection").unwrap(), "keep-alive");
        assert_eq!(headers.get("x-request-id").unwrap(), "abc");
    }

    #[test]
    fn the_error_response_shape_matches_the_expected() {
        let headers = json_error_headers();
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
    }

    /// Drive the framing with a replayed prefix and a live tail that splits an
    /// event across two reads, which is the case the line buffer exists for.
    #[tokio::test]
    async fn the_framer_replays_the_prefix_and_joins_split_lines() {
        let rest: ByteStream = Box::pin(futures::stream::iter(vec![
            Ok(Bytes::from_static(b"{\"type\":\"text-delta\",\"te")),
            Ok(Bytes::from_static(b"xt\":\"tail\"}\n")),
        ]));
        let framed = frame_as_sse(
            "{\"type\":\"text-delta\",\"text\":\"head\"}\n".to_string(),
            rest,
            "m",
        );

        let mut out = Vec::new();
        let mut framed = framed;
        while let Some(chunk) = framed.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(out).unwrap();

        assert!(
            text.contains("\"content\":\"head\""),
            "the prefix was replayed"
        );
        assert!(
            text.contains("\"content\":\"tail\""),
            "the split line was joined"
        );
        assert!(text.ends_with(SSE_DONE));
        assert_eq!(text.matches("data: ").count(), 3, "two deltas plus [DONE]");
    }

    #[tokio::test]
    async fn a_trailing_partial_line_is_flushed() {
        // No newline at the end: only `flush` can emit it.
        let rest: ByteStream = Box::pin(futures::stream::iter(vec![Ok(Bytes::from_static(
            b"{\"type\":\"text-delta\",\"text\":\"last\"}",
        ))]));
        let mut framed = frame_as_sse(String::new(), rest, "m");
        let mut out = Vec::new();
        while let Some(chunk) = framed.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"content\":\"last\""));
        assert!(text.ends_with(SSE_DONE));
    }
}
