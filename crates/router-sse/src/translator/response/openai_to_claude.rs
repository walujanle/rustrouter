//! OpenAI → Claude response.
//!
//! Three decisions here are easy to get wrong:
//!
//! - Tool arguments are **buffered, not streamed**. A non-Anthropic model emits
//!   argument fragments that are individually valid JSON only by accident, and
//!   `Read` calls routinely carry out-of-range `offset`/`limit` or a `pages`
//!   value the real tool rejects. The whole payload is sanitized once at
//!   `finish_reason` and emitted as a single `input_json_delta`.
//! - Thinking and text blocks are mutually exclusive. Opening either one closes
//!   the other first, and the close is guarded so a block never stops twice
//!   (`textBlockClosed` tracks exactly that).
//! - Block indices and the buffered-argument map have no home in
//!   `ResponseState`, so they live in `extra` under their own field names.
//!   `textBlockStarted` / `thinkingBlockStarted` / `toolCalls` are the wired
//!   fields and are used directly.

use serde_json::{Map, Value, json};

use crate::session_manager::now_ms;
use crate::translator::ResponseState;
use crate::translator::concerns::finish_reason::from_openai_finish;
use crate::translator::concerns::primitives::{
    extract_reasoning_text, js_number, js_string, js_truthy, js_truthy_opt,
};
use crate::translator::formats;
use crate::translator::schema::{MODEL_FALLBACK, claude_block, role};

/// Legacy `proxy_` prefix from older request translators. The response strips
/// it defensively so a `proxy_Read` resolves back to `Read` for arg
/// sanitization. The current request translator emits no prefix, so the strip
/// is usually a no-op; kept intentionally.
const CLAUDE_OAUTH_TOOL_PREFIX: &str = "proxy_";

/// `openaiToClaudeResponse(chunk, state)`.
pub fn openai_to_claude_response(chunk: &Value, state: &mut ResponseState) -> Vec<Value> {
    if !js_truthy(chunk) {
        return Vec::new();
    }
    let Some(choice) = chunk
        .get("choices")
        .and_then(|c| c.get(0))
        .filter(|c| js_truthy(c))
    else {
        return Vec::new();
    };

    let mut results: Vec<Value> = Vec::new();
    let delta = choice.get("delta");

    // `typeof chunk.usage === "object"`: an object or array, not a scalar.
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object() || u.is_array()) {
        let prompt_tokens = js_number(usage.get("prompt_tokens"));
        let output_tokens = js_number(usage.get("completion_tokens"));
        let cached_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"));
        let cache_creation_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_tokens"));
        let cache_read_tokens = js_number(cached_tokens);
        let cache_create_tokens = js_number(cache_creation_tokens);

        // OpenAI's prompt_tokens already includes the cached side, so the
        // Claude figure is the remainder.
        let input_tokens = prompt_tokens - cache_read_tokens - cache_create_tokens;

        let mut tracked = Map::new();
        tracked.insert("input_tokens".into(), json!(input_tokens));
        tracked.insert("output_tokens".into(), json!(output_tokens));
        if cache_read_tokens > 0 {
            tracked.insert("cache_read_input_tokens".into(), json!(cache_read_tokens));
        }
        if cache_create_tokens > 0 {
            tracked.insert(
                "cache_creation_input_tokens".into(),
                json!(cache_create_tokens),
            );
        }
        state.usage = Some(Value::Object(tracked));
    }

    if !extra_bool(state, "messageStartSent") {
        state.extra.insert("messageStartSent".into(), json!(true));

        state.message_id = Some(
            chunk
                .get("id")
                .and_then(Value::as_str)
                .map(|s| s.replacen("chatcmpl-", "", 1))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("msg_{}", now_ms())),
        );
        let needs_override = state
            .message_id
            .as_deref()
            .is_none_or(|s| s.is_empty() || s == "chat" || s.len() < 8);
        if needs_override {
            let extend = chunk.get("extend_fields");
            let fallback = extend
                .and_then(|e| e.get("requestId"))
                .filter(|v| js_truthy(v))
                .or_else(|| {
                    extend
                        .and_then(|e| e.get("traceId"))
                        .filter(|v| js_truthy(v))
                });
            state.message_id = Some(match fallback {
                Some(v) => js_string(v),
                None => format!("msg_{}", now_ms()),
            });
        }

        state.model = Some(match chunk.get("model") {
            Some(v) if js_truthy(v) => js_string(v),
            _ => MODEL_FALLBACK.to_string(),
        });
        state.extra.insert("nextBlockIndex".into(), json!(0));

        results.push(json!({
            "type": "message_start",
            "message": {
                "id": state.message_id.clone(),
                "type": "message",
                "role": role::ASSISTANT,
                "model": state.model.clone(),
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {"input_tokens": 0, "output_tokens": 0},
            },
        }));
    }

    let reasoning = extract_reasoning_text(delta.unwrap_or(&Value::Null));
    if !reasoning.is_empty() {
        stop_text_block(state, &mut results);

        if !state.thinking_block_started {
            let index = next_block_index(state);
            state
                .extra
                .insert("thinkingBlockIndex".into(), json!(index));
            state.thinking_block_started = true;
            results.push(json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": claude_block::THINKING, "thinking": ""},
            }));
        }

        let index = js_number(Some(&state.scratch("thinkingBlockIndex")));
        results.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": reasoning},
        }));
    }

    if let Some(content) = delta
        .and_then(|d| d.get("content"))
        .filter(|c| js_truthy(c))
    {
        stop_thinking_block(state, &mut results);

        if !state.text_block_started {
            let index = next_block_index(state);
            state.extra.insert("textBlockIndex".into(), json!(index));
            state.text_block_started = true;
            state.extra.insert("textBlockClosed".into(), json!(false));
            results.push(json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {"type": claude_block::TEXT, "text": ""},
            }));
        }

        let index = js_number(Some(&state.scratch("textBlockIndex")));
        results.push(json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": content.clone()},
        }));
    }

    // A non-array `tool_calls` would throw on `for…of`; the wire can carry `{}`,
    // so only arrays are iterated.
    if let Some(Value::Array(calls)) = delta
        .and_then(|d| d.get("tool_calls"))
        .filter(|c| js_truthy(c))
    {
        for tc in calls {
            let idx = tool_index_key(tc);

            // GLM/fireworks repeat id+null-name on every arg chunk; open the
            // block once per idx.
            if js_truthy_opt(tc.get("id")) && !state.tool_calls.contains_key(&idx) {
                stop_thinking_block(state, &mut results);
                stop_text_block(state, &mut results);

                let block_index = next_block_index(state);
                let name = match tc.get("function").and_then(|f| f.get("name")) {
                    Some(v) if js_truthy(v) => v.clone(),
                    _ => json!(""),
                };
                state.tool_calls.insert(
                    idx.clone(),
                    json!({
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "name": name,
                        "blockIndex": block_index,
                    }),
                );

                let raw_name = name.as_str().unwrap_or("");
                let tool_name = raw_name
                    .strip_prefix(CLAUDE_OAUTH_TOOL_PREFIX)
                    .unwrap_or(raw_name);
                results.push(json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": claude_block::TOOL_USE,
                        "id": tc.get("id").cloned().unwrap_or(Value::Null),
                        "name": tool_name,
                        "input": {},
                    },
                }));
            }

            if let Some(arguments) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .filter(|a| js_truthy(a))
                && state.tool_calls.contains_key(&idx)
            {
                buffer_args(state, &idx, &js_string(arguments));
            }
        }
    }

    if let Some(finish_reason) = choice.get("finish_reason").filter(|r| js_truthy(r)) {
        stop_thinking_block(state, &mut results);
        stop_text_block(state, &mut results);

        let entries: Vec<(String, Value)> = state
            .tool_calls
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let buffers = state.scratch("toolArgBuffers");
        for (idx, tool_info) in entries {
            let block_index = tool_info.get("blockIndex").cloned().unwrap_or(Value::Null);
            let buffered = buffers
                .get(idx.as_str())
                .and_then(Value::as_str)
                .unwrap_or("");
            if !buffered.is_empty() {
                let name = tool_info.get("name").cloned().unwrap_or(Value::Null);
                let sanitized = sanitize_tool_args(&name, buffered);
                results.push(json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "input_json_delta", "partial_json": sanitized},
                }));
            }
            results.push(json!({
                "type": "content_block_stop",
                "index": block_index,
            }));
        }

        state.finish_reason = Some(js_string(finish_reason));

        let final_usage = state
            .usage
            .clone()
            .filter(js_truthy)
            .unwrap_or_else(|| json!({"input_tokens": 0, "output_tokens": 0}));
        let stop_reason =
            from_openai_finish(Some(finish_reason.as_str().unwrap_or("")), formats::CLAUDE);
        results.push(json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason},
            "usage": final_usage,
        }));
        results.push(json!({"type": "message_stop"}));
    }

    results
}

/// `stopThinkingBlock(state, results)`.
fn stop_thinking_block(state: &mut ResponseState, results: &mut Vec<Value>) {
    if !state.thinking_block_started {
        return;
    }
    let index = js_number(Some(&state.scratch("thinkingBlockIndex")));
    results.push(json!({"type": "content_block_stop", "index": index}));
    state.thinking_block_started = false;
}

/// `stopTextBlock(state, results)`: `textBlockClosed` keeps a close from
/// firing twice when the block was already stopped by a thinking transition.
fn stop_text_block(state: &mut ResponseState, results: &mut Vec<Value>) {
    if !state.text_block_started || extra_bool(state, "textBlockClosed") {
        return;
    }
    state.extra.insert("textBlockClosed".into(), json!(true));
    let index = js_number(Some(&state.scratch("textBlockIndex")));
    results.push(json!({"type": "content_block_stop", "index": index}));
    state.text_block_started = false;
}

/// `state.nextBlockIndex++`.
fn next_block_index(state: &mut ResponseState) -> i64 {
    let current = js_number(Some(&state.scratch("nextBlockIndex")));
    state
        .extra
        .insert("nextBlockIndex".into(), json!(current + 1));
    current
}

fn extra_bool(state: &ResponseState, key: &str) -> bool {
    js_truthy(&state.scratch(key))
}

/// `tc.index ?? 0`, stringified because `toolCalls` is keyed by string here.
fn tool_index_key(tc: &Value) -> String {
    match tc.get("index") {
        Some(v) if !v.is_null() => js_string(v),
        _ => "0".to_string(),
    }
}

/// `state.toolArgBuffers.set(idx, (buf.get(idx) || "") + arguments)`.
fn buffer_args(state: &mut ResponseState, idx: &str, arguments: &str) {
    let mut buffers = match state.scratch("toolArgBuffers") {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let existing = buffers
        .get(idx)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    buffers.insert(idx.to_string(), json!(format!("{existing}{arguments}")));
    state
        .extra
        .insert("toolArgBuffers".into(), Value::Object(buffers));
}

/// `sanitizeToolArgs(toolName, argsJson)`.
fn sanitize_tool_args(tool_name: &Value, args_json: &str) -> String {
    // A non-string name would throw on `.startsWith`, so the payload is returned
    // untouched.
    let Some(tool_name) = tool_name.as_str() else {
        return args_json.to_string();
    };
    let Ok(mut args) = serde_json::from_str::<Value>(args_json) else {
        return args_json.to_string();
    };

    let name = tool_name
        .strip_prefix(CLAUDE_OAUTH_TOOL_PREFIX)
        .unwrap_or(tool_name);
    if name == "Read" {
        // `"pages" in args` throws on a primitive (but not on an array), in
        // which case the original text is returned.
        if !args.is_object() && !args.is_array() {
            return args_json.to_string();
        }
        sanitize_read_args(&mut args);
    }

    serde_json::to_string(&args).unwrap_or_else(|_| args_json.to_string())
}

/// `sanitizeReadArgs(args)`.
fn sanitize_read_args(args: &mut Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };

    let coerced_limit = obj
        .get("limit")
        .and_then(Value::as_str)
        .filter(|s| is_unsigned_digits(s))
        .map(js_number_from_str);
    if let Some(limit) = coerced_limit {
        obj.insert("limit".into(), limit);
    }
    let coerced_offset = obj
        .get("offset")
        .and_then(Value::as_str)
        .filter(|s| is_signed_digits(s))
        .map(js_number_from_str);
    if let Some(offset) = coerced_offset {
        obj.insert("offset".into(), offset);
    }

    if let Some(limit) = obj.get("limit").and_then(Value::as_f64) {
        if limit > 2000.0 {
            obj.insert("limit".into(), json!(2000));
        } else if limit < 1.0 {
            obj.shift_remove("limit");
        }
    }
    if obj
        .get("offset")
        .and_then(Value::as_f64)
        .is_some_and(|o| o < 0.0)
    {
        obj.insert("offset".into(), json!(0));
    }

    if obj.contains_key("pages") && !is_valid_pdf_pages_arg(obj.get("file_path"), obj.get("pages"))
    {
        obj.shift_remove("pages");
    }
}

/// `isValidPdfPagesArg(filePath, pages)`.
fn is_valid_pdf_pages_arg(file_path: Option<&Value>, pages: Option<&Value>) -> bool {
    let Some(file_path) = file_path.and_then(Value::as_str) else {
        return false;
    };
    let Some(pages) = pages.and_then(Value::as_str) else {
        return false;
    };
    file_path.to_lowercase().ends_with(".pdf") && is_pdf_pages(pages)
}

/// `^\d+$`.
fn is_unsigned_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `^-?\d+$`.
fn is_signed_digits(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    is_unsigned_digits(digits)
}

/// `^\d+(?:-\d+)?$`.
fn is_pdf_pages(s: &str) -> bool {
    let mut parts = s.splitn(2, '-');
    let first = parts.next().unwrap_or("");
    if !is_unsigned_digits(first) {
        return false;
    }
    match parts.next() {
        Some(second) => is_unsigned_digits(second),
        None => true,
    }
}

/// `Number(str)` for the two digit-only patterns above.
fn js_number_from_str(s: &str) -> Value {
    match s.parse::<i64>() {
        Ok(i) => json!(i),
        // Beyond i64 JS still produces a float; keep it rather than dropping it.
        Err(_) => s.parse::<f64>().map(|f| json!(f)).unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResponseState {
        ResponseState::default()
    }

    fn input_json_delta(events: &[Value]) -> Option<&str> {
        events
            .iter()
            .find(|e| {
                e.get("type").and_then(Value::as_str) == Some("content_block_delta")
                    && e.get("delta")
                        .and_then(|d| d.get("type"))
                        .and_then(Value::as_str)
                        == Some("input_json_delta")
            })
            .and_then(|e| e.get("delta"))
            .and_then(|d| d.get("partial_json"))
            .and_then(Value::as_str)
    }

    #[test]
    fn first_chunk_emits_message_start_with_the_wire_key_order() {
        let mut s = state();
        let out = openai_to_claude_response(
            &json!({"id": "chatcmpl-abc12345", "model": "gpt-4o", "choices": [{"delta": {}}]}),
            &mut s,
        );
        assert_eq!(out.len(), 1);
        let message = &out[0]["message"];
        assert_eq!(out[0]["type"], json!("message_start"));
        let keys: Vec<&str> = message
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "type",
                "role",
                "model",
                "content",
                "stop_reason",
                "stop_sequence",
                "usage"
            ]
        );
        assert_eq!(message["id"], json!("abc12345"));
        assert_eq!(message["role"], json!("assistant"));
        assert_eq!(message["model"], json!("gpt-4o"));
        assert_eq!(message["stop_reason"], Value::Null);
        assert_eq!(
            message["usage"],
            json!({"input_tokens": 0, "output_tokens": 0})
        );
    }

    #[test]
    fn a_missing_model_falls_back_and_a_short_id_is_replaced() {
        let mut s = state();
        let out = openai_to_claude_response(
            &json!({"id": "chatcmpl-x", "choices": [{"delta": {}}]}),
            &mut s,
        );
        assert_eq!(out[0]["message"]["model"], json!(MODEL_FALLBACK));
        // "x" is shorter than 8 characters, so the id is regenerated.
        assert!(
            out[0]["message"]["id"]
                .as_str()
                .unwrap()
                .starts_with("msg_")
        );
    }

    #[test]
    fn reasoning_opens_a_thinking_block_and_content_closes_it() {
        let mut s = state();
        let first = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"reasoning_content": "hmm"}}],
            }),
            &mut s,
        );
        assert_eq!(first[1]["type"], json!("content_block_start"));
        assert_eq!(
            first[1]["content_block"],
            json!({"type": "thinking", "thinking": ""})
        );
        assert_eq!(
            first[2]["delta"],
            json!({"type": "thinking_delta", "thinking": "hmm"})
        );

        let second = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"content": "hi"}}],
            }),
            &mut s,
        );
        assert_eq!(second[0], json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(
            second[1]["content_block"],
            json!({"type": "text", "text": ""})
        );
        assert_eq!(
            second[2]["delta"],
            json!({"type": "text_delta", "text": "hi"})
        );
    }

    #[test]
    fn a_finish_closes_open_blocks_and_emits_message_delta_then_stop() {
        let mut s = state();
        openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"content": "hi"}}],
            }),
            &mut s,
        );
        let out = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {}, "finish_reason": "stop"}],
            }),
            &mut s,
        );
        assert_eq!(out[0], json!({"type": "content_block_stop", "index": 0}));
        assert_eq!(out[1]["delta"], json!({"stop_reason": "end_turn"}));
        assert_eq!(out[2], json!({"type": "message_stop"}));
        assert_eq!(s.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn tool_arguments_are_buffered_and_sanitized_once_at_finish() {
        let mut s = state();
        let opened = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "toolu_read", "function": {"name": "Read"}},
                ]}}],
            }),
            &mut s,
        );
        assert_eq!(
            opened[1]["content_block"],
            json!({"type": "tool_use", "id": "toolu_read", "name": "Read", "input": {}})
        );

        let args = serde_json::to_string(&json!({
            "file_path": "F:/repo/file.js", "offset": -5, "limit": 999999999, "pages": "",
        }))
        .unwrap();
        let out = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": args}},
                ]}, "finish_reason": "tool_calls"}],
            }),
            &mut s,
        );
        let sanitized: Value = serde_json::from_str(input_json_delta(&out).unwrap()).unwrap();
        assert_eq!(
            sanitized,
            json!({"file_path": "F:/repo/file.js", "offset": 0, "limit": 2000})
        );
        // The stop follows the delta, and the reason maps to tool_use.
        assert!(out.iter().any(|e| e["type"] == json!("content_block_stop")));
        let message_delta = out
            .iter()
            .find(|e| e["type"] == json!("message_delta"))
            .unwrap();
        assert_eq!(message_delta["delta"], json!({"stop_reason": "tool_use"}));
    }

    #[test]
    fn a_proxy_prefixed_read_keeps_valid_pdf_pages() {
        let mut s = state();
        openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "toolu_pdf", "function": {"name": "proxy_Read"}},
                ]}}],
            }),
            &mut s,
        );
        let args = serde_json::to_string(&json!({"file_path": "F:/repo/doc.pdf", "pages": "1-3"}))
            .unwrap();
        let out = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": args}},
                ]}, "finish_reason": "tool_calls"}],
            }),
            &mut s,
        );
        let sanitized: Value = serde_json::from_str(input_json_delta(&out).unwrap()).unwrap();
        assert_eq!(
            sanitized,
            json!({"file_path": "F:/repo/doc.pdf", "pages": "1-3"})
        );
    }

    #[test]
    fn usage_is_folded_into_claude_counters_with_cache_split_out() {
        let mut s = state();
        openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "usage": {
                    "prompt_tokens": 100, "completion_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 20, "cache_creation_tokens": 5},
                },
                "choices": [{"delta": {"content": "hi"}}],
            }),
            &mut s,
        );
        let usage = s.usage.clone().unwrap();
        assert_eq!(usage["input_tokens"], json!(75));
        assert_eq!(usage["output_tokens"], json!(50));
        assert_eq!(usage["cache_read_input_tokens"], json!(20));
        assert_eq!(usage["cache_creation_input_tokens"], json!(5));

        // No cache counters means no cache keys at all.
        let mut s = state();
        openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                "choices": [{"delta": {"content": "x"}}],
            }),
            &mut s,
        );
        let usage = s.usage.clone().unwrap();
        assert!(usage.get("cache_read_input_tokens").is_none());
        assert_eq!(usage["input_tokens"], json!(10));
    }

    #[test]
    fn the_tracked_usage_is_what_message_delta_reports() {
        let mut s = state();
        let out = openai_to_claude_response(
            &json!({
                "id": "chatcmpl-abcdefgh", "model": "m",
                "usage": {"prompt_tokens": 10, "completion_tokens": 4},
                "choices": [{"delta": {}, "finish_reason": "length"}],
            }),
            &mut s,
        );
        let message_delta = out
            .iter()
            .find(|e| e["type"] == json!("message_delta"))
            .unwrap();
        assert_eq!(
            message_delta["usage"],
            json!({"input_tokens": 10, "output_tokens": 4})
        );
        assert_eq!(message_delta["delta"], json!({"stop_reason": "max_tokens"}));
    }

    #[test]
    fn a_chunk_without_choices_emits_nothing() {
        let mut s = state();
        assert!(openai_to_claude_response(&json!({"id": "x"}), &mut s).is_empty());
        assert!(openai_to_claude_response(&json!({"choices": []}), &mut s).is_empty());
        assert!(openai_to_claude_response(&Value::Null, &mut s).is_empty());
    }

    #[test]
    fn an_empty_delta_after_message_start_emits_nothing_further() {
        let mut s = state();
        openai_to_claude_response(
            &json!({"id": "chatcmpl-abcdefgh", "model": "m", "choices": [{"delta": {}}]}),
            &mut s,
        );
        let out = openai_to_claude_response(
            &json!({"id": "chatcmpl-abcdefgh", "model": "m", "choices": [{"delta": {}}]}),
            &mut s,
        );
        assert!(out.is_empty());
    }
}
