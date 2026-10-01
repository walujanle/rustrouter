//! Answer Claude Code's bookkeeping requests without calling a provider.
//!
//! The CLI fires several requests that are not conversation turns — a warmup, a
//! token count, a topic-title extraction, a terminal-clearing "command". Each
//! costs an upstream call and returns text the CLI throws away, so they are
//! answered locally with a canned response.
//!
//! The gate is narrow on purpose: only `claude-cli` user agents, and only for
//! the exact shapes below. A near-miss falls through to the provider.

use serde_json::{Value, json};

use crate::runtime_config::SKIP_PATTERNS;
use crate::translator::formats;
use crate::translator::{init_state, translate_response};
use crate::utils::stream_helpers::format_sse;

/// `DEFAULT_BYPASS_TEXT`.
pub const DEFAULT_BYPASS_TEXT: &str = "CLI Command Execution: Clear Terminal";

/// Why a request was bypassed, so the caller can label the log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassKind {
    /// A title-extraction or skip-pattern match: the canned text is returned.
    Plain,
    /// The `isNewTopic` probe: the response is a synthesized title.
    Naming,
}

/// A synthesized response, already serialized the way the client expects.
pub struct BypassResponse {
    pub kind: BypassKind,
    /// `true` when the body should be an SSE stream.
    pub stream: bool,
    /// The SSE body when `stream`, otherwise the JSON body.
    pub body: String,
    pub content_type: &'static str,
}

impl BypassResponse {
    pub fn bytes(&self) -> Vec<u8> {
        self.body.as_bytes().to_vec()
    }
}

/// `handleBypassRequest(body, model, userAgent, ccFilterNaming)`.
///
/// `None` means "not a bypass": the caller proceeds to the provider.
pub fn handle_bypass_request(
    body: &Value,
    model: &str,
    user_agent: &str,
    cc_filter_naming: bool,
) -> Option<BypassResponse> {
    if !user_agent.contains("claude-cli") {
        return None;
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .filter(|m| !m.is_empty())?;

    let text_of = |content: Option<&Value>| -> String {
        match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        }
    };

    let mut should_bypass = false;
    let mut naming_bypass = false;

    // Pattern 1: title extraction — the assistant's last message is a bare "{".
    let last = messages.last();
    if last.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant")
        && last
            .and_then(|m| m.get("content"))
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            == Some("{")
    {
        should_bypass = true;
    }

    // Pattern 2: warmup.
    if !should_bypass && text_of(messages[0].get("content")) == "Warmup" {
        should_bypass = true;
    }

    // Pattern 3: count.
    if !should_bypass
        && messages.len() == 1
        && messages[0].get("role").and_then(Value::as_str) == Some("user")
        && text_of(messages[0].get("content")) == "count"
    {
        should_bypass = true;
    }

    // Pattern 4: a skip pattern anywhere in the user turns.
    if !should_bypass && !SKIP_PATTERNS.is_empty() {
        let user_text = messages
            .iter()
            .filter(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .map(|m| text_of(m.get("content")))
            .collect::<Vec<_>>()
            .join(" ");
        if SKIP_PATTERNS.iter().any(|p| user_text.contains(p)) {
            should_bypass = true;
        }
    }

    // Pattern 5: the naming probe. Claude carries `system` at the top level, but
    // a `system` role inside `messages` takes precedence when present.
    if !should_bypass && cc_filter_naming {
        let system_from_messages = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
            .map(|m| text_of(m.get("content")))
            .unwrap_or_default();
        let system_from_body = match body.get("system") {
            Some(Value::Array(parts)) => parts
                .iter()
                .filter(|s| s.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|s| s.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };
        // `||`: an empty messages-side text falls through to the body's.
        let system_text = if system_from_messages.is_empty() {
            system_from_body
        } else {
            system_from_messages
        };
        if system_text.contains("isNewTopic") {
            should_bypass = true;
            naming_bypass = true;
        }
    }

    if !should_bypass {
        return None;
    }

    let source_format = crate::providers::service::detect_format(body);
    let stream = body.get("stream").and_then(Value::as_bool) != Some(false);

    if naming_bypass {
        let user_text = messages
            .iter()
            .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
            .map(|m| text_of(m.get("content")))
            .unwrap_or_default();
        let title = user_text
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        let naming_text = json!({ "isNewTopic": true, "title": title }).to_string();
        return Some(if stream {
            streaming_response(source_format, model, &naming_text)
        } else {
            non_streaming_response(source_format, model, &naming_text)
        });
    }

    Some(if stream {
        streaming_response(source_format, model, DEFAULT_BYPASS_TEXT)
    } else {
        non_streaming_response(source_format, model, DEFAULT_BYPASS_TEXT)
    })
}

/// `createOpenAIResponse(model, text)`.
fn openai_response(model: &str, text: &str) -> Value {
    let now = crate::session_manager::now_ms();
    json!({
        "id": format!("chatcmpl-{now}"),
        "object": "chat.completion",
        "created": now / 1000,
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
    })
}

/// `createOpenAIStreamingChunks(completeResponse)`.
fn openai_streaming_chunks(response: &Value) -> Vec<Value> {
    let id = response.get("id").cloned().unwrap_or(Value::Null);
    let created = response.get("created").cloned().unwrap_or(Value::Null);
    let model = response.get("model").cloned().unwrap_or(Value::Null);
    let content = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .cloned()
        .unwrap_or(Value::Null);
    let usage = response.get("usage").cloned().unwrap_or(Value::Null);

    vec![
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": content },
                "finish_reason": Value::Null,
            }],
        }),
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
            "usage": usage,
        }),
    ]
}

/// `createStreamingResponse(sourceFormat, model, text)`.
fn streaming_response(source_format: &str, model: &str, text: &str) -> BypassResponse {
    let response = openai_response(model, text);
    let mut state = init_state(source_format);
    state.model = Some(model.to_string());

    let mut out = String::new();
    for chunk in openai_streaming_chunks(&response) {
        for translated in
            translate_response(formats::OPENAI, source_format, &chunk, &mut state).chunks
        {
            out.push_str(&format_sse(&translated, Some(source_format)));
        }
    }
    // The null-chunk flush: a translator may hold state that only a `null`
    // releases (a Responses `response.completed`, for one).
    for translated in
        translate_response(formats::OPENAI, source_format, &Value::Null, &mut state).chunks
    {
        out.push_str(&format_sse(&translated, Some(source_format)));
    }
    out.push_str("data: [DONE]\n\n");

    BypassResponse {
        kind: BypassKind::Plain,
        stream: true,
        body: out,
        content_type: "text/event-stream",
    }
}

/// `createNonStreamingResponse(sourceFormat, model, text)`.
fn non_streaming_response(source_format: &str, model: &str, text: &str) -> BypassResponse {
    let response = openai_response(model, text);

    if source_format == formats::OPENAI {
        return BypassResponse {
            kind: BypassKind::Plain,
            stream: false,
            body: response.to_string(),
            content_type: "application/json",
        };
    }

    let mut state = init_state(source_format);
    state.model = Some(model.to_string());

    let mut translated_all: Vec<Value> = Vec::new();
    for chunk in openai_streaming_chunks(&response) {
        translated_all
            .extend(translate_response(formats::OPENAI, source_format, &chunk, &mut state).chunks);
    }
    translated_all.extend(
        translate_response(formats::OPENAI, source_format, &Value::Null, &mut state).chunks,
    );

    let final_response = merge_chunks_to_response(&translated_all, source_format, model);
    BypassResponse {
        kind: BypassKind::Plain,
        stream: false,
        body: final_response.to_string(),
        content_type: "application/json",
    }
}

/// `mergeChunksToResponse(chunks, sourceFormat)`.
///
/// The last chunk usually *is* the response, except for Claude, where the
/// stream ends on `message_stop` and the real message is the `message_start`
/// payload — with its cache counters, which the `message_delta` usage omits.
fn merge_chunks_to_response(chunks: &[Value], source_format: &str, model: &str) -> Value {
    if chunks.is_empty() {
        return openai_response(model, DEFAULT_BYPASS_TEXT);
    }

    let mut final_chunk = chunks.last().cloned().unwrap_or(Value::Null);

    if source_format == formats::CLAUDE
        && chunks
            .iter()
            .any(|c| c.get("type").and_then(Value::as_str) == Some("message_stop"))
    {
        let find = |kind: &str| {
            chunks
                .iter()
                .find(|c| c.get("type").and_then(Value::as_str) == Some(kind))
        };
        let message_start = find("message_start");
        let message_delta = find("message_delta");
        let _content_delta = find("content_block_delta");

        if let Some(message) = message_start.and_then(|c| c.get("message")) {
            let mut merged = message.clone();
            let start_usage = message.get("usage");
            let delta_usage = message_delta.and_then(|d| d.get("usage"));

            if start_usage.is_some() || delta_usage.is_some() {
                let mut usage = serde_json::Map::new();
                for source in [start_usage, delta_usage] {
                    if let Some(Value::Object(map)) = source {
                        for (key, value) in map {
                            usage.insert(key.clone(), value.clone());
                        }
                    }
                }
                // The delta omits the cache counters, so they are re-applied
                // from the start frame after the merge.
                if let Some(Value::Object(start)) = start_usage {
                    for key in [
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                        "input_tokens",
                    ] {
                        if let Some(value) = start.get(key) {
                            usage.insert(key.to_string(), value.clone());
                        }
                    }
                }
                merged["usage"] = Value::Object(usage);
            }
            final_chunk = merged;
        }
    }

    final_chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_body(messages: Value) -> Value {
        json!({ "model": "claude-opus-5", "stream": false, "messages": messages })
    }

    #[test]
    fn a_non_claude_user_agent_never_bypasses() {
        let body = claude_body(json!([{"role": "user", "content": "Warmup"}]));
        assert!(handle_bypass_request(&body, "m", "curl/8.0", false).is_none());
    }

    #[test]
    fn an_empty_message_list_never_bypasses() {
        let body = claude_body(json!([]));
        assert!(handle_bypass_request(&body, "m", "claude-cli/1.0", false).is_none());
        let body = json!({"model": "m", "messages": null});
        assert!(handle_bypass_request(&body, "m", "claude-cli/1.0", false).is_none());
    }

    #[test]
    fn the_title_extraction_probe_is_bypassed() {
        let body = claude_body(json!([
            {"role": "user", "content": "summarize this"},
            {"role": "assistant", "content": [{"type": "text", "text": "{"}]},
        ]));
        let response =
            handle_bypass_request(&body, "m", "claude-cli/1.0", false).expect("bypasses");
        assert_eq!(response.kind, BypassKind::Plain);
        assert!(
            response.body.contains(DEFAULT_BYPASS_TEXT),
            "{}",
            response.body
        );
    }

    #[test]
    fn the_warmup_and_count_probes_are_bypassed() {
        let warmup = claude_body(json!([{"role": "user", "content": "Warmup"}]));
        assert!(handle_bypass_request(&warmup, "m", "claude-cli/1.0", false).is_some());

        let count =
            claude_body(json!([{"role": "user", "content": [{"type": "text", "text": "count"}]}]));
        assert!(handle_bypass_request(&count, "m", "claude-cli/1.0", false).is_some());

        // "count" only counts as a single user turn.
        let multi = claude_body(json!([
            {"role": "user", "content": "count"},
            {"role": "assistant", "content": "1"},
        ]));
        assert!(handle_bypass_request(&multi, "m", "claude-cli/1.0", false).is_none());
    }

    #[test]
    fn a_skip_pattern_in_any_user_turn_bypasses() {
        let pattern = SKIP_PATTERNS[0];
        let body = claude_body(json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "hi"},
            {"role": "user", "content": [{"type": "text", "text": format!("prefix {pattern} suffix")}]},
        ]));
        assert!(handle_bypass_request(&body, "m", "claude-cli/1.0", false).is_some());
    }

    #[test]
    fn the_naming_probe_only_fires_when_asked() {
        let body = json!({
            "model": "m", "stream": true,
            "system": [{"type": "text", "text": "Return isNewTopic and a title."}],
            "messages": [{"role": "user", "content": "fix the login bug in auth"}],
        });
        // Off: no bypass at all.
        assert!(handle_bypass_request(&body, "m", "claude-cli/1.0", false).is_none());

        let response = handle_bypass_request(&body, "m", "claude-cli/1.0", true).expect("bypasses");
        assert!(response.body.contains("isNewTopic"), "{}", response.body);
        // The title is the first three words of the user's message.
        assert!(response.body.contains("fix the login"), "{}", response.body);
    }

    #[test]
    fn a_system_role_inside_messages_wins_over_the_body_system() {
        let body = json!({
            "model": "m", "stream": true,
            "system": "nothing here",
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "isNewTopic please"}]},
                {"role": "user", "content": "a b c d"},
            ],
        });
        let response = handle_bypass_request(&body, "m", "claude-cli/1.0", true).expect("bypasses");
        assert!(response.body.contains("a b c"), "{}", response.body);
    }

    #[test]
    fn an_empty_messages_side_system_text_falls_through_to_the_body() {
        let body = json!({
            "model": "m", "stream": false,
            "system": [{"type": "text", "text": "isNewTopic"}],
            "messages": [
                {"role": "system", "content": ""},
                {"role": "user", "content": "one two three four"},
            ],
        });
        assert!(handle_bypass_request(&body, "m", "claude-cli/1.0", true).is_some());
    }

    #[test]
    fn a_streaming_bypass_ends_with_done() {
        let body = json!({
            "model": "m", "stream": true,
            "messages": [{"role": "user", "content": "Warmup"}],
        });
        let response =
            handle_bypass_request(&body, "m", "claude-cli/1.0", false).expect("bypasses");
        assert!(response.stream);
        assert_eq!(response.content_type, "text/event-stream");
        assert!(
            response.body.ends_with("data: [DONE]\n\n"),
            "{}",
            response.body
        );
        // An OpenAI-format client gets OpenAI chunk frames.
        assert!(
            response
                .body
                .contains("\"object\":\"chat.completion.chunk\""),
            "{}",
            response.body
        );
    }

    #[test]
    fn stream_defaults_to_true_when_absent() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Warmup"}],
        });
        let response =
            handle_bypass_request(&body, "m", "claude-cli/1.0", false).expect("bypasses");
        assert!(response.stream);
    }

    #[test]
    fn a_non_streaming_openai_client_gets_the_plain_response() {
        let body = json!({
            "model": "m", "stream": false,
            "messages": [{"role": "user", "content": "Warmup"}],
        });
        let response =
            handle_bypass_request(&body, "m", "claude-cli/1.0", false).expect("bypasses");
        assert!(!response.stream);
        assert_eq!(response.content_type, "application/json");
        let parsed: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(parsed["object"], json!("chat.completion"));
        assert_eq!(parsed["usage"]["total_tokens"], json!(2));
    }

    #[test]
    fn a_claude_client_gets_a_translated_message_object() {
        // `detect_format` keys off the body shape, not the model name: a bare
        // `messages[]` with string content reads as OpenAI. The `system` field
        // is what marks this as a Claude request.
        let body = json!({
            "model": "claude-opus-5", "stream": false,
            "system": "be brief",
            "messages": [{"role": "user", "content": "Warmup"}],
        });
        let response = handle_bypass_request(&body, "claude-opus-5", "claude-cli/1.0", false)
            .expect("bypasses");
        let parsed: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(parsed["type"], json!("message"), "{}", response.body);
        assert_eq!(parsed["role"], json!("assistant"), "{}", response.body);
    }
}
