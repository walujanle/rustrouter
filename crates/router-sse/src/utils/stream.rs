//! The single SSE transform, with two modes.
//!
//! PASSTHROUGH normalises upstream chunks and extracts usage; TRANSLATE pushes
//! every chunk through `translate_response`, emits `[DONE]`, and synthesises a
//! Responses terminal when one never arrived. The state machine is `SseStream`
//! (`push`/`flush`) and `create_sse_stream` lifts it into the
//! `Fn(ByteStream) -> ByteStream` shape
//! `stream_handler::pipe_with_disconnect` consumes.
//!
//! Three behaviours are load-bearing:
//!
//! * **The UTF-8 decoder is per-stream and incremental.** A multi-byte
//!   character split across two upstream chunks must survive, so bytes are
//!   buffered until the sequence completes; only genuinely invalid bytes become
//!   U+FFFD, and an incomplete tail at flush time does too.
//! * **The usage tail runs from `push` as well as `flush`.** A client that
//!   closes right after the terminal event cancels the reader and `flush` never
//!   runs, so `finalize_stream` is called on the Responses terminal event too.
//! * **A `[DONE]` sentinel is not the end of a TRANSLATE stream** unless the
//!   target is Responses. The sentinel is skipped, and the flush null-chunk
//!   translation still has to run.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::credentials::Credentials;
use crate::executors::executor::ByteStream;
use crate::translator::concerns::primitives::js_truthy_opt;
use crate::translator::formats;
use crate::translator::{ResponseState, init_state, translate_response};
use crate::utils::responses_stream_helpers::{
    format_incomplete_openai_responses_stream_failure, get_openai_responses_event_name,
    is_openai_responses_terminal_event,
};
use crate::utils::stream_helpers::{
    fix_invalid_id, format_sse, has_valuable_content, parse_sse_line,
};
use crate::utils::usage_tracking::{
    add_buffer_to_usage, estimate_output_tokens, extract_usage, filter_usage_for_format,
    format_usage, has_valid_usage, merge_usage,
};

/// `STREAM_MODE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamMode {
    #[default]
    Translate,
    Passthrough,
}

/// `reqLogger?.appendProviderChunk / appendConvertedChunk / appendOpenAIChunk`.
/// Every method is optional; an implementor no-ops the ones it does not want.
pub trait StreamLog: Send + Sync {
    fn append_provider_chunk(&self, _text: &str) {}
    fn append_converted_chunk(&self, _text: &str) {}
    fn append_openai_chunk(&self, _text: &str) {}
}

/// Usage-DB callbacks: pending-request tracking and request-log appends.
/// The crate is DB-free, so the server layer persists them.
pub trait StreamHooks: Send + Sync {
    /// Records that a request started or finished. `error` marks the provider
    /// as the most recent failure in the stats payload; it only matters on the
    /// decrement.
    fn track_pending_request(
        &self,
        model: Option<&str>,
        provider: Option<&str>,
        connection_id: Option<&str>,
        pending: bool,
        error: bool,
    );
    fn append_request_log(&self, entry: Value);
}

/// Called once a stream completes, with the final usage and time-to-first-token.
pub type StreamCompleteFn = Arc<dyn Fn(Option<Value>, Option<i64>) + Send + Sync>;

/// Options for building the SSE transform.
#[derive(Clone)]
pub struct SseStreamOptions {
    pub mode: StreamMode,
    /// Provider format (TRANSLATE mode).
    pub target_format: Option<String>,
    /// Client format.
    pub source_format: String,
    pub provider: Option<String>,
    pub tool_name_map: Option<HashMap<String, String>>,
    pub custom_tool_names: Vec<String>,
    pub model: Option<String>,
    pub connection_id: Option<String>,
    /// Input tokens, estimated once from the request body when the stream is
    /// built. Holding the count instead of the body keeps a full request `Value`
    /// alive for the life of the stream.
    pub input_tokens: i64,
    pub api_key: Option<String>,
    pub credentials: Option<Credentials>,
    /// `credentials?._clientSessionId`, when the caller already resolved it.
    pub session_id: Option<String>,
    pub log: Option<Arc<dyn StreamLog>>,
    pub hooks: Option<Arc<dyn StreamHooks>>,
    pub on_stream_complete: Option<StreamCompleteFn>,
}

impl Default for SseStreamOptions {
    fn default() -> Self {
        Self {
            mode: StreamMode::Translate,
            target_format: None,
            source_format: formats::OPENAI.to_string(),
            provider: None,
            tool_name_map: None,
            custom_tool_names: Vec::new(),
            model: None,
            connection_id: None,
            input_tokens: 0,
            api_key: None,
            credentials: None,
            session_id: None,
            log: None,
            hooks: None,
            on_stream_complete: None,
        }
    }
}

impl SseStreamOptions {
    pub fn new(mode: StreamMode, source_format: impl Into<String>) -> Self {
        Self {
            mode,
            source_format: source_format.into(),
            ..Default::default()
        }
    }
}

/// Whether debug logging is enabled, via `tracing`'s level filter.
fn is_debug_enabled() -> bool {
    tracing::enabled!(tracing::Level::DEBUG)
}

/// JS `String.prototype.length`: UTF-16 code units, not bytes or chars.
fn js_len(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

/// One stream's transform state.
pub struct SseStream {
    opts: SseStreamOptions,
    buffer: String,
    /// Bytes held back because they end mid-UTF-8-sequence.
    pending: Vec<u8>,
    usage: Option<Value>,
    state: Option<ResponseState>,
    total_content_length: i64,
    ttft_at: Option<i64>,
    sse_line_count: u64,
    sse_emitted_count: u64,
    event_type_counts: Vec<(String, u64)>,
    current_openai_responses_event: Option<String>,
    openai_responses_terminal_seen: bool,
    openai_responses_done_sent: bool,
    stream_done_sent: bool,
    finalized: bool,
}

impl SseStream {
    pub fn new(opts: SseStreamOptions) -> Self {
        let session_id = opts.session_id.clone().or_else(|| {
            opts.credentials
                .as_ref()
                .and_then(|c| c.client_session_id.clone())
        });

        let state = (opts.mode == StreamMode::Translate).then(|| {
            let mut state = init_state(&opts.source_format);
            state.provider = opts.provider.clone();
            state.tool_name_map = opts.tool_name_map.clone();
            state.custom_tool_names = opts
                .custom_tool_names
                .iter()
                .cloned()
                .collect::<HashSet<_>>();
            state.model = opts.model.clone();
            state.session_id = session_id;
            state.target_format = opts.target_format.clone();
            state
        });

        Self {
            opts,
            buffer: String::new(),
            pending: Vec::new(),
            usage: None,
            state,
            total_content_length: 0,
            ttft_at: None,
            sse_line_count: 0,
            sse_emitted_count: 0,
            event_type_counts: Vec::new(),
            current_openai_responses_event: None,
            openai_responses_terminal_seen: false,
            openai_responses_done_sent: false,
            stream_done_sent: false,
            finalized: false,
        }
    }

    /// Feeds one upstream chunk through the transform, returning the bytes to
    /// emit.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Bytes> {
        let mut out = Vec::new();
        if self.ttft_at.is_none() {
            self.ttft_at = Some(router_db::time::now_ms() as i64);
        }

        let text = self.decode(chunk);
        if let Some(log) = &self.opts.log {
            log.append_provider_chunk(&text);
        }
        self.buffer.push_str(&text);

        let mut lines: Vec<String> = self.buffer.split('\n').map(str::to_string).collect();
        self.buffer = lines.pop().unwrap_or_default();
        for line in &lines {
            self.process_line(line, &mut out);
        }
        out
    }

    /// Flushes any buffered state at end-of-stream.
    pub fn flush(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();

        let summary = if self.event_type_counts.is_empty() {
            "none".to_string()
        } else {
            self.event_type_counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        tracing::debug!(
            provider = self.opts.provider.as_deref().unwrap_or("none"),
            model = self.opts.model.as_deref().unwrap_or("none"),
            recv_lines = self.sse_line_count,
            emitted = self.sse_emitted_count,
            events = %summary,
            "SSE flush"
        );
        if let Some(hooks) = &self.opts.hooks {
            hooks.track_pending_request(
                self.opts.model.as_deref(),
                self.opts.provider.as_deref(),
                self.opts.connection_id.as_deref(),
                false,
                false,
            );
        }

        let remaining = self.decoder_flush();
        if !remaining.is_empty() {
            self.buffer.push_str(&remaining);
        }

        if self.opts.mode == StreamMode::Passthrough {
            if !self.buffer.is_empty() {
                let mut output = self.buffer.clone();
                if self.buffer.starts_with("data:") && !self.buffer.starts_with("data: ") {
                    output = format!("data: {}", &self.buffer[5..]);
                }
                self.emit_raw(&output, &mut out);
            }

            // Gemini-family clients reject the OpenAI sentinel with a 400.
            let is_gemini_family = matches!(
                self.opts.provider.as_deref(),
                Some("gemini") | Some("vertex")
            );
            if !self.stream_done_sent && !is_gemini_family {
                self.emit_raw("data: [DONE]\n\n", &mut out);
            }

            self.finalize_stream();
            return out;
        }

        if !self.buffer.trim().is_empty() {
            let parsed = parse_sse_line(self.buffer.trim(), self.opts.target_format.as_deref());
            let is_done_sentinel = js_truthy_opt(parsed.as_ref().and_then(|p| p.get("done")));
            if let Some(parsed) = parsed
                && !is_done_sentinel
            {
                if let Some(extracted) = extract_usage(Some(&parsed)) {
                    let merged = merge_usage(self.state_usage(), Some(extracted));
                    self.set_state_usage(merged);
                }
                let chunks = self.translate(&parsed);
                for item in chunks {
                    if item.is_null() {
                        continue;
                    }
                    self.emit_frame(&item, &mut out, false);
                }
            }
        }

        let flushed = self.translate(&Value::Null);
        for item in flushed {
            if item.is_null() {
                continue;
            }
            self.emit_frame(&item, &mut out, false);
        }

        let keeps_responses = self.opts.target_format.as_deref() == Some(formats::OPENAI_RESPONSES)
            && self.opts.source_format == formats::OPENAI_RESPONSES;
        if keeps_responses && !self.openai_responses_terminal_seen {
            let failed = format_incomplete_openai_responses_stream_failure();
            self.emit_raw(&failed, &mut out);
            self.openai_responses_terminal_seen = true;
        }
        if keeps_responses && !self.openai_responses_done_sent && !self.stream_done_sent {
            self.emit_raw("data: [DONE]\n\n", &mut out);
            self.openai_responses_done_sent = true;
            self.stream_done_sent = true;
        }

        self.finalize_stream();
        out
    }

    // ── line dispatch ─────────────────────────────────────────────────────

    fn process_line(&mut self, line: &str, out: &mut Vec<Bytes>) {
        let trimmed = line.trim();

        if is_debug_enabled() && !trimmed.is_empty() {
            self.sse_line_count += 1;
            if let Some(evt) = trimmed.strip_prefix("event:") {
                let evt = evt.trim();
                match self
                    .event_type_counts
                    .iter_mut()
                    .find(|(k, _)| k.as_str() == evt)
                {
                    Some(entry) => entry.1 += 1,
                    None => self.event_type_counts.push((evt.to_string(), 1)),
                }
            }
        }

        // Capture Responses event framing for same-format passthrough (codex).
        if self.opts.mode == StreamMode::Translate
            && self.opts.target_format.as_deref() == Some(formats::OPENAI_RESPONSES)
            && let Some(evt) = trimmed.strip_prefix("event:")
        {
            self.current_openai_responses_event = Some(evt.trim().to_string());
        }

        if self.opts.mode == StreamMode::Passthrough {
            self.process_passthrough_line(line, trimmed, out);
            return;
        }

        self.process_translate_line(trimmed, out);
    }

    fn process_passthrough_line(&mut self, line: &str, trimmed: &str, out: &mut Vec<Bytes>) {
        let mut output: Option<String> = None;
        let mut injected_usage = false;
        let mut responses_terminal = false;

        let data = trimmed.strip_prefix("data:").map(str::trim);
        if let Some(data) = data.filter(|d| *d != "[DONE]") {
            let Ok(mut parsed) = serde_json::from_str::<Value>(data) else {
                // Non-JSON data lines are dropped rather than forwarded: upstream
                // plain-text errors would break downstream JSON decoders.
                return;
            };

            let id_fixed = fix_invalid_id(&mut parsed);

            // Letta and friends require `object`/`created` on every chunk.
            let mut fields_injected = false;
            if parsed.get("choices").is_some() {
                if !js_truthy_opt(parsed.get("object")) {
                    insert_field(&mut parsed, "object", json!("chat.completion.chunk"));
                    fields_injected = true;
                }
                if !js_truthy_opt(parsed.get("created")) {
                    insert_field(
                        &mut parsed,
                        "created",
                        json!(router_db::time::now_ms() / 1000),
                    );
                    fields_injected = true;
                }
            }

            // Azure-specific fields.
            if parsed.get("prompt_filter_results").is_some() {
                remove_field(&mut parsed, "prompt_filter_results");
                fields_injected = true;
            }

            if let Some(choices) = parsed.get_mut("choices").and_then(Value::as_array_mut) {
                for choice in choices.iter_mut() {
                    if choice.get("content_filter_results").is_some() {
                        if let Some(obj) = choice.as_object_mut() {
                            obj.shift_remove("content_filter_results");
                        }
                        fields_injected = true;
                    }
                    // An empty `tool_calls: []` makes @ai-sdk/openai-compatible
                    // end reasoning on every chunk.
                    if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut)
                        && js_truthy_opt(delta.get("tool_calls"))
                        && delta
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_some_and(|a| a.is_empty())
                    {
                        delta.shift_remove("tool_calls");
                        fields_injected = true;
                    }
                }
            }

            if !has_valuable_content(&parsed, Some(formats::OPENAI)) {
                return;
            }

            if let Some(delta) = parsed
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("delta"))
            {
                if let Some(content) = delta
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.total_content_length += js_len(content);
                }
                if let Some(reasoning) = delta
                    .get("reasoning_content")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.total_content_length += js_len(reasoning);
                }
            }

            if let Some(extracted) = extract_usage(Some(&parsed)) {
                self.usage = merge_usage(self.usage.take(), Some(extracted));
            }

            responses_terminal = is_openai_responses_terminal_event(
                self.current_openai_responses_event.as_deref(),
                Some(&parsed),
            );

            let is_finish_chunk = js_truthy_opt(
                parsed
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("finish_reason")),
            );
            if is_finish_chunk && !has_valid_usage(parsed.get("usage")) {
                let estimated = self.estimate(formats::OPENAI);
                insert_field(
                    &mut parsed,
                    "usage",
                    filter_usage_for_format(&estimated, formats::OPENAI),
                );
                output = Some(format!("data: {parsed}\n"));
                self.usage = Some(estimated);
                injected_usage = true;
            } else if is_finish_chunk && self.usage.is_some() {
                let buffered = add_buffer_to_usage(self.usage.as_ref().unwrap());
                insert_field(
                    &mut parsed,
                    "usage",
                    filter_usage_for_format(&buffered, formats::OPENAI),
                );
                output = Some(format!("data: {parsed}\n"));
                injected_usage = true;
            } else if id_fixed || fields_injected {
                output = Some(format!("data: {parsed}\n"));
                injected_usage = true;
            }
        }

        if !injected_usage {
            output = Some(
                if line.starts_with("data:") && !line.starts_with("data: ") {
                    format!("data: {}\n", &line[5..])
                } else {
                    format!("{line}\n")
                },
            );
        }

        if let Some(output) = output {
            self.emit_raw(&output, out);
        }
        if responses_terminal {
            self.finalize_stream();
        }
    }

    fn process_translate_line(&mut self, trimmed: &str, out: &mut Vec<Bytes>) {
        if trimmed.is_empty() {
            return;
        }
        let Some(parsed) = parse_sse_line(trimmed, self.opts.target_format.as_deref()) else {
            return;
        };

        let is_responses_stream =
            self.opts.target_format.as_deref() == Some(formats::OPENAI_RESPONSES);
        let keeps_responses =
            is_responses_stream && self.opts.source_format == formats::OPENAI_RESPONSES;
        let event_name = is_responses_stream
            .then(|| {
                get_openai_responses_event_name(
                    self.current_openai_responses_event.as_deref(),
                    Some(&parsed),
                )
            })
            .flatten();

        if is_responses_stream
            && is_openai_responses_terminal_event(event_name.as_deref(), Some(&parsed))
        {
            self.openai_responses_terminal_seen = true;
        }

        // `done: true` is the `[DONE]` sentinel.
        if js_truthy_opt(parsed.get("done")) {
            if keeps_responses && !self.openai_responses_terminal_seen {
                let failed = format_incomplete_openai_responses_stream_failure();
                self.emit_raw(&failed, out);
                self.openai_responses_terminal_seen = true;
                self.sse_emitted_count += 1;
            }
            if keeps_responses && !self.stream_done_sent {
                self.emit_raw("data: [DONE]\n\n", out);
            }
            self.stream_done_sent = true;
            if keeps_responses {
                self.openai_responses_done_sent = true;
            }
            return;
        }

        self.count_content_length(&parsed);

        if let Some(extracted) = extract_usage(Some(&parsed)) {
            let merged = merge_usage(self.state_usage(), Some(extracted));
            self.set_state_usage(merged);
        }

        if keeps_responses && let Some(event_name) = event_name {
            let output = format_sse(
                &json!({"event": event_name, "data": parsed}),
                Some(self.opts.source_format.as_str()),
            );
            self.emit_raw(&output, out);
            self.current_openai_responses_event = None;
            self.sse_emitted_count += 1;
            if self.openai_responses_terminal_seen {
                self.finalize_stream();
            }
            return;
        }

        self.current_openai_responses_event = None;

        let chunks = self.translate(&parsed);
        let has_finish_reason = self
            .state
            .as_ref()
            .and_then(|s| s.finish_reason.as_deref())
            .is_some_and(|s| !s.is_empty());

        for mut item in chunks {
            if item.is_null() {
                continue;
            }
            if !has_valuable_content(&item, Some(self.opts.source_format.as_str())) {
                continue;
            }

            let is_finish_chunk = item.get("type").and_then(Value::as_str) == Some("message_delta")
                || js_truthy_opt(
                    item.get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason")),
                );
            if has_finish_reason
                && is_finish_chunk
                && !has_valid_usage(item.get("usage"))
                && self.total_content_length > 0
            {
                let estimated = self.estimate(&self.opts.source_format);
                insert_field(
                    &mut item,
                    "usage",
                    filter_usage_for_format(&estimated, &self.opts.source_format),
                );
                self.set_state_usage(Some(estimated));
            } else if has_finish_reason
                && is_finish_chunk
                && let Some(usage) = self.state.as_ref().and_then(|s| s.usage.as_ref()).cloned()
            {
                let buffered = add_buffer_to_usage(&usage);
                insert_field(
                    &mut item,
                    "usage",
                    filter_usage_for_format(&buffered, &self.opts.source_format),
                );
            }

            self.emit_frame(&item, out, true);
        }
    }

    /// Counts emitted content and thinking, for the Claude, OpenAI and Gemini
    /// shapes. The counts feed the output-token estimate; the text itself is
    /// never retained.
    fn count_content_length(&mut self, parsed: &Value) {
        if let Some(text) = parsed
            .get("delta")
            .and_then(|d| d.get("text"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.total_content_length += js_len(text);
        }
        if let Some(thinking) = parsed
            .get("delta")
            .and_then(|d| d.get("thinking"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.total_content_length += js_len(thinking);
        }
        if let Some(content) = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.total_content_length += js_len(content);
        }
        if let Some(reasoning) = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("reasoning_content"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.total_content_length += js_len(reasoning);
        }
        if let Some(parts) = parsed
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(text) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    self.total_content_length += js_len(text);
                }
            }
        }
    }

    // ── translation plumbing ──────────────────────────────────────────────

    fn translate(&mut self, chunk: &Value) -> Vec<Value> {
        let target = self.opts.target_format.as_deref().unwrap_or_default();
        let Some(state) = self.state.as_mut() else {
            return Vec::new();
        };
        let translated = translate_response(target, &self.opts.source_format, chunk, state);

        if let Some(items) = &translated.openai_intermediate
            && let Some(log) = &self.opts.log
        {
            for item in items {
                log.append_openai_chunk(&format_sse(item, Some(formats::OPENAI)));
            }
        }
        translated.chunks
    }

    fn emit_frame(&mut self, item: &Value, out: &mut Vec<Bytes>, count: bool) {
        let output = format_sse(item, Some(self.opts.source_format.as_str()));
        self.emit_raw(&output, out);
        if count {
            self.sse_emitted_count += 1;
        }
    }

    fn emit_raw(&self, output: &str, out: &mut Vec<Bytes>) {
        if let Some(log) = &self.opts.log {
            log.append_converted_chunk(output);
        }
        out.push(Bytes::from(output.to_string()));
    }

    fn estimate(&self, format: &str) -> Value {
        format_usage(
            self.opts.input_tokens,
            estimate_output_tokens(self.total_content_length),
            format,
        )
    }

    fn state_usage(&mut self) -> Option<Value> {
        self.state.as_mut().and_then(|s| s.usage.take())
    }

    fn set_state_usage(&mut self, usage: Option<Value>) {
        if let Some(state) = self.state.as_mut() {
            state.usage = usage;
        }
    }

    /// Emits the terminal frame and runs the completion hooks, once.
    fn finalize_stream(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        let is_passthrough = self.opts.mode == StreamMode::Passthrough;
        let mut final_usage = if is_passthrough {
            self.usage.clone()
        } else {
            self.state.as_ref().and_then(|s| s.usage.clone())
        };

        if !has_valid_usage(final_usage.as_ref()) && self.total_content_length > 0 {
            let format = if is_passthrough {
                formats::OPENAI
            } else {
                self.opts.source_format.as_str()
            };
            final_usage = Some(self.estimate(format));
            if is_passthrough {
                self.usage = final_usage.clone();
            } else {
                self.set_state_usage(final_usage.clone());
            }
        }

        if has_valid_usage(final_usage.as_ref()) {
            let provider = if is_passthrough {
                self.opts.provider.as_deref()
            } else {
                self.state
                    .as_ref()
                    .and_then(|s| s.provider.as_deref())
                    .or(self.opts.target_format.as_deref())
            };
            tracing::debug!(
                provider = provider.unwrap_or("UNKNOWN"),
                model = self.opts.model.as_deref().unwrap_or("none"),
                usage = %final_usage.as_ref().map(|v| v.to_string()).unwrap_or_default(),
                "stream usage"
            );
        } else if let Some(hooks) = &self.opts.hooks {
            hooks.append_request_log(json!({
                "model": self.opts.model,
                "provider": self.opts.provider,
                "connectionId": self.opts.connection_id,
                "tokens": Value::Null,
                "status": "200 OK",
            }));
        }

        if let Some(callback) = &self.opts.on_stream_complete {
            callback(final_usage, self.ttft_at);
        }
    }

    // ── UTF-8 decoding ────────────────────────────────────────────────────

    /// `decoder.decode(chunk, {stream: true})`: emit every complete UTF-8
    /// sequence, hold an incomplete tail for the next chunk, and replace
    /// genuinely invalid bytes with U+FFFD.
    fn decode(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        loop {
            match validate_utf8(&self.pending) {
                None => {
                    out.push_str(std::str::from_utf8(&self.pending).unwrap_or_default());
                    self.pending.clear();
                    break;
                }
                Some((valid_up_to, error_len)) => {
                    out.push_str(&String::from_utf8_lossy(&self.pending[..valid_up_to]));
                    match error_len {
                        // A truncated tail may still complete next chunk.
                        None => {
                            self.pending.drain(..valid_up_to);
                            break;
                        }
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid_up_to + len);
                        }
                    }
                }
            }
        }
        out
    }

    /// `decoder.decode()`: flush the decoder, incomplete tail becomes U+FFFD.
    fn decoder_flush(&mut self) -> String {
        if self.pending.is_empty() {
            return String::new();
        }
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

/// The first invalid span in `bytes`: `(valid_up_to, error_len)`, or `None`
/// when the whole buffer is valid UTF-8. Returned by value so the caller can
/// mutate the buffer without holding a borrow of it.
fn validate_utf8(bytes: &[u8]) -> Option<(usize, Option<usize>)> {
    std::str::from_utf8(bytes)
        .err()
        .map(|e| (e.valid_up_to(), e.error_len()))
}

/// `insert` an owned field, no-op on a non-object payload.
fn insert_field(value: &mut Value, key: &str, field: Value) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(key.to_string(), field);
    }
}

/// `delete value[key]`, no-op on a non-object payload.
fn remove_field(value: &mut Value, key: &str) {
    if let Some(obj) = value.as_object_mut() {
        obj.shift_remove(key);
    }
}

/// Builds the SSE transform as the `Fn(ByteStream) -> ByteStream` shape
/// `pipe_with_disconnect` consumes. State is per-stream: the options are cloned
/// so the returned closure is `Fn`, and a fresh `SseStream` is built per call.
pub fn create_sse_stream(options: SseStreamOptions) -> impl Fn(ByteStream) -> ByteStream {
    move |input| {
        let options = options.clone();
        let stream: ByteStream = Box::pin(async_stream::stream! {
            let mut stream = SseStream::new(options);
            let mut input = input;
            while let Some(chunk) = input.next().await {
                match chunk {
                    Ok(bytes) => {
                        for frame in stream.push(&bytes) {
                            yield Ok(frame);
                        }
                    }
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
            for frame in stream.flush() {
                yield Ok(frame);
            }
        });
        stream
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(chunks: Vec<Bytes>) -> String {
        let mut out = String::new();
        for chunk in chunks {
            out.push_str(&String::from_utf8_lossy(&chunk));
        }
        out
    }

    fn passthrough() -> SseStream {
        SseStream::new(SseStreamOptions::new(
            StreamMode::Passthrough,
            formats::OPENAI,
        ))
    }

    fn translate(target: &str, source: &str) -> SseStream {
        let mut opts = SseStreamOptions::new(StreamMode::Translate, source);
        opts.target_format = Some(target.to_string());
        SseStream::new(opts)
    }

    #[test]
    fn passthrough_injects_the_openai_required_fields() {
        let mut stream = passthrough();
        let out =
            text_of(stream.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"));
        assert!(out.starts_with("data: {"), "{out}");
        assert!(
            out.contains("\"object\":\"chat.completion.chunk\""),
            "{out}"
        );
        assert!(out.contains("\"created\":"), "{out}");
        assert!(out.contains("\"content\":\"hi\""), "{out}");
        // Injected chunks are re-serialized with a single trailing newline; only
        // the un-injected passthrough path preserves the original `\n\n`.
        assert!(out.ends_with('\n'), "{out}");
    }

    #[test]
    fn passthrough_strips_azure_fields_and_empty_tool_calls() {
        let mut stream = passthrough();
        let out = text_of(stream.push(
            b"data: {\"prompt_filter_results\":[1],\"choices\":[{\"content_filter_results\":{},\"delta\":{\"content\":\"x\",\"tool_calls\":[]}}]}\n\n",
        ));
        assert!(!out.contains("prompt_filter_results"), "{out}");
        assert!(!out.contains("content_filter_results"), "{out}");
        assert!(!out.contains("tool_calls"), "{out}");
    }

    #[test]
    fn passthrough_estimates_usage_on_a_finish_chunk_without_one() {
        let mut stream = passthrough();
        stream.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n");
        let out = text_of(
            stream.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"),
        );
        assert!(out.contains("\"usage\":"), "{out}");
        assert!(out.contains("\"estimated\":true"), "{out}");
    }

    #[test]
    fn passthrough_drops_a_non_json_data_line() {
        let mut stream = passthrough();
        // A single trailing newline, so only the data line is complete.
        let out = text_of(stream.push(b"data: <html>oops</html>\n"));
        assert!(out.is_empty(), "{out}");
    }

    #[test]
    fn passthrough_flush_terminates_with_done_unless_gemini_family() {
        let mut stream = passthrough();
        assert!(text_of(stream.flush()).ends_with("data: [DONE]\n\n"));

        let mut opts = SseStreamOptions::new(StreamMode::Passthrough, formats::OPENAI);
        opts.provider = Some("gemini".to_string());
        let mut gemini = SseStream::new(opts);
        assert!(text_of(gemini.flush()).is_empty());
    }

    #[test]
    fn translate_emits_claude_frames_for_an_openai_provider() {
        let mut stream = translate(formats::OPENAI, formats::CLAUDE);
        let out = text_of(stream.push(
            b"data: {\"id\":\"chatcmpl-abcdefgh\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
        ));
        assert!(out.contains("event: message_start"), "{out}");
        assert!(out.contains("event: content_block_delta"), "{out}");
        assert!(out.contains("\"text\":\"hi\""), "{out}");
    }

    #[test]
    fn an_empty_translation_does_not_end_the_stream() {
        let mut stream = translate(formats::OPENAI, formats::CLAUDE);
        stream.push(
            b"data: {\"id\":\"chatcmpl-abcdefgh\",\"model\":\"m\",\"choices\":[{\"delta\":{}}]}\n\n",
        );
        // A second empty chunk translates to nothing; the stream must survive it.
        let middle = text_of(stream.push(
            b"data: {\"id\":\"chatcmpl-abcdefgh\",\"model\":\"m\",\"choices\":[{\"delta\":{}}]}\n\n",
        ));
        assert!(middle.is_empty(), "{middle}");
        let out = text_of(stream.push(
            b"data: {\"id\":\"chatcmpl-abcdefgh\",\"model\":\"m\",\"choices\":[{\"delta\":{\"content\":\"later\"}}]}\n\n",
        ));
        assert!(out.contains("\"text\":\"later\""), "{out}");
    }

    #[test]
    fn the_null_chunk_flush_emits_nothing_when_the_translator_returns_empty() {
        let mut stream = translate(formats::OPENAI, formats::CLAUDE);
        assert!(text_of(stream.flush()).is_empty());
    }

    #[test]
    fn an_unterminated_responses_passthrough_emits_response_failed() {
        let mut stream = translate(formats::OPENAI_RESPONSES, formats::OPENAI_RESPONSES);
        let first = text_of(stream.push(
            b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        ));
        assert!(
            first.contains("event: response.output_text.delta"),
            "{first}"
        );

        let out = text_of(stream.flush());
        assert!(out.contains("event: response.failed"), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        let mut stream = passthrough();
        // "é" is C3 A9, split across two upstream chunks.
        let first = text_of(stream.push(b"data: {\"choices\":[{\"delta\":{\"content\":\"\xc3"));
        assert!(first.is_empty(), "{first}");
        let second = text_of(stream.push(b"\xa9\"}}]}\n\n"));
        assert!(second.contains("é"), "{second}");
    }

    #[test]
    fn finalize_runs_exactly_once() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let mut opts = SseStreamOptions::new(StreamMode::Passthrough, formats::OPENAI);
        let callback: StreamCompleteFn = Arc::new(move |_usage, _ttft| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        opts.on_stream_complete = Some(callback);
        let mut stream = SseStream::new(opts);
        stream.flush();
        stream.flush();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn hooks_see_the_pending_request_and_a_tokened_log() {
        use std::sync::Arc;
        use std::sync::Mutex;

        #[derive(Default)]
        struct Hooks {
            tracked: Mutex<Vec<String>>,
            logs: Mutex<Vec<Value>>,
        }
        impl StreamHooks for Hooks {
            fn track_pending_request(
                &self,
                model: Option<&str>,
                provider: Option<&str>,
                _connection_id: Option<&str>,
                pending: bool,
                _error: bool,
            ) {
                self.tracked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!(
                        "{}|{}|{pending}",
                        model.unwrap_or(""),
                        provider.unwrap_or("")
                    ));
            }
            fn append_request_log(&self, entry: Value) {
                self.logs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(entry);
            }
        }

        let hooks = Arc::new(Hooks::default());
        let mut opts = SseStreamOptions::new(StreamMode::Passthrough, formats::OPENAI);
        opts.model = Some("m".to_string());
        opts.provider = Some("p".to_string());
        let dyn_hooks: Arc<dyn StreamHooks> = hooks.clone();
        opts.hooks = Some(dyn_hooks);
        let mut stream = SseStream::new(opts);
        stream.flush();

        assert_eq!(
            hooks
                .tracked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            vec!["m|p|false".to_string()]
        );
        // No content and no usage: a null-token 200 log is recorded.
        assert_eq!(
            hooks.logs.lock().unwrap_or_else(|e| e.into_inner()).len(),
            1
        );
    }
}
