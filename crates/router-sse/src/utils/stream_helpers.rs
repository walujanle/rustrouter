//! SSE stream helpers.
//!
//! Two JS behaviours are load-bearing here:
//!
//! * `parseSSELine` returns any falsy `JSON.parse` result (`null`, `0`, `false`,
//!   `""`) straight through, and every caller guards with `if (!parsed)`. The
//!   Rust signature collapses both "parse failed" and "parsed to a falsy value"
//!   to `None`, which is what callers observe. Returning `Some(Value::Null)`
//!   would silently stop the guard from firing.
//! * `formatSSE` only recurses `cleanUsagePayload` into a nested `response`;
//!   every other key is left as-is, and a top-level array or scalar is returned
//!   untouched rather than walked.
//!
//! `data === undefined` has no JSON counterpart, so `Value::Null` covers both
//! and yields the same `data: null\n\n` frame.

use serde_json::{Value, json};

use crate::translator::concerns::primitives::{js_string, js_truthy, js_truthy_opt};
use crate::translator::formats;
use crate::utils::error::build_error_body;
use crate::utils::sse::SSE_DONE;

/// `parseSSELine(line, format = null)`.
///
/// `None` means "no usable chunk": an empty line, a non-`data:` line, a
/// malformed payload, or a payload that parsed to a JS-falsy value.
///
/// The `format` parameter is kept for signature parity. It only ever selected
/// the Ollama NDJSON branch, which no shipped provider uses; no format needs
/// different parsing now.
pub fn parse_sse_line(line: &str, _format: Option<&str>) -> Option<Value> {
    if line.is_empty() {
        return None;
    }

    // `charCodeAt(0) !== 100`: the line must begin with 'd'. Comparing the first
    // char rather than the byte keeps multi-byte openers from passing.
    if !line.starts_with('d') {
        return None;
    }

    // `slice(5)`: five UTF-16 units, then trim. `chars()` is the closest Rust
    // equivalent and avoids panicking on a short or non-ASCII line.
    let data: String = line.chars().skip(5).collect();
    let data = data.trim();
    if data == "[DONE]" {
        return Some(json!({ "done": true }));
    }

    match serde_json::from_str::<Value>(data) {
        Ok(value) if js_truthy(&value) => Some(value),
        Ok(_) => None,
        Err(_) => {
            if !data.is_empty() && data.chars().count() < 1000 {
                let head: String = data.chars().take(100).collect();
                tracing::warn!(
                    "Failed to parse SSE line ({} chars): {}...",
                    data.chars().count(),
                    head
                );
            }
            None
        }
    }
}

/// `formatSSE(data, sourceFormat)`.
///
/// `data` is the translated chunk; `source_format` is the *client's* format, so
/// only Claude clients get the `event:` line.
pub fn format_sse(data: &Value, source_format: Option<&str>) -> String {
    if data.is_null() {
        return "data: null\n\n".to_string();
    }
    if js_truthy_opt(data.get("done")) {
        return "data: [DONE]\n\n".to_string();
    }

    // OpenAI Responses API passthrough: the chunk already carries its own event
    // name, so the frame is `event: <name>` + a single cleaned `data:` line.
    if let Some(event) = data.get("event").filter(|e| js_truthy(e))
        && let Some(payload) = data.get("data").filter(|d| js_truthy(d))
    {
        let cleaned = clean_usage_payload(payload);
        return format!("event: {}\ndata: {}\n\n", js_string(event), cleaned);
    }

    let cleaned = clean_usage_payload(data);

    if source_format == Some(formats::CLAUDE)
        && let Some(kind) = cleaned.get("type").filter(|t| js_truthy(t))
    {
        return format!("event: {}\ndata: {}\n\n", js_string(kind), cleaned);
    }

    format!("data: {cleaned}\n\n")
}

/// `hasValuableContent(chunk, format)`.
///
/// The OpenAI branch returns `undefined` (falsy) when the delta has nothing in
/// it, and `delta.role` is returned as the last operand — a non-empty string is
/// truthy, `undefined` is not. `finish_reason` is checked with `||` so an empty
/// string falls through to `delta.role`.
pub fn has_valuable_content(chunk: &Value, format: Option<&str>) -> bool {
    if format == Some(formats::OPENAI)
        && let Some(delta) = chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
    {
        let non_empty = |key: &str| {
            delta
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        let has_tool_calls = delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let has_finish_reason = chunk
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finish_reason"))
            .is_some_and(js_truthy);
        let has_role = delta.get("role").is_some_and(js_truthy);
        return non_empty("content")
            || non_empty("reasoning_content")
            || has_tool_calls
            || has_finish_reason
            || has_role;
    }

    if format == Some(formats::CLAUDE) {
        let is_content_block_delta =
            chunk.get("type").and_then(Value::as_str) == Some("content_block_delta");
        let delta = chunk.get("delta");
        let non_empty = |key: &str| {
            delta
                .and_then(|d| d.get(key))
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
        };
        if is_content_block_delta
            && !non_empty("text")
            && !non_empty("thinking")
            && !non_empty("partial_json")
        {
            return false;
        }
        return true;
    }

    true
}

/// `fixInvalidId(parsed)`: rewrite a placeholder or too-short id in place.
///
/// Returns `true` when it rewrote — the callers ignore the return value, but the
/// parity is cheap to keep.
pub fn fix_invalid_id(parsed: &mut Value) -> bool {
    let Some(id) = parsed.get("id").and_then(Value::as_str) else {
        return false;
    };
    // `parsed.id.length < 8` counts UTF-16 units; the ids in play are ASCII, so
    // `chars()` and `len()` agree.
    if id != "chat" && id != "completion" && id.chars().count() >= 8 {
        return false;
    }

    let fallback = parsed
        .get("extend_fields")
        .and_then(|e| e.get("requestId"))
        .filter(|v| js_truthy(v))
        .or_else(|| {
            parsed
                .get("extend_fields")
                .and_then(|e| e.get("traceId"))
                .filter(|v| js_truthy(v))
        })
        .map(js_string)
        .unwrap_or_else(|| {
            // `Date.now().toString(36)`: base-36 milliseconds. Only reached when
            // the upstream sent no request id at all.
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            base36(ms)
        });

    if let Some(obj) = parsed.as_object_mut() {
        obj.insert("id".into(), json!(format!("chatcmpl-{fallback}")));
    }
    true
}

/// `Math.random().toString(36).slice(2, 2 + len)`: `len` lowercase base36
/// characters, the id suffix sprinkled on stream detail ids and fallback
/// response ids.
pub(crate) fn random_base36(len: usize) -> String {
    use rand::RngExt;
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| ALPHABET[rng.random_range(0..36)] as char)
        .collect()
}

/// `Number.prototype.toString(36)` for a non-negative integer.
fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `buildStreamErrorBytes(statusCode, message, clientFormat)`.
///
/// Terminal frame for a stream that aborted after HTTP 200 was already sent, so
/// the status code can no longer change. OpenAI-compatible clients raise on any
/// `data:` payload carrying an `error` key (checked before `[DONE]`), so the
/// error frame goes first and `[DONE]` second; Anthropic clients need
/// `event: error`.
///
/// A non-SSE client format gets SSE framing here, which is dead in practice
/// because `detectFormatByEndpoint` never resolves to a raw-NDJSON format.
pub fn build_stream_error_bytes(
    status_code: u16,
    message: &str,
    client_format: Option<&str>,
) -> Vec<u8> {
    let body = build_error_body(status_code, message);
    let error = body.get("error").cloned().unwrap_or(Value::Null);

    let sse = if client_format == Some(formats::CLAUDE) {
        format_sse(
            &json!({ "type": "error", "error": error }),
            Some(formats::CLAUDE),
        )
    } else {
        format!(
            "{}{}",
            format_sse(&json!({ "error": error }), client_format),
            SSE_DONE
        )
    };
    sse.into_bytes()
}

/// `cleanUsagePayload(payload)`: drop a `usage: null` key, and drop a nested
/// `usage.perf_metrics: null`, recursing only into `response`.
///
/// The removals exist because `JSON.stringify` would otherwise emit
/// `"usage":null` / `"perf_metrics":null` where clients expect the key to be
/// absent. Key order is preserved (`shift_remove`, and `insert` on an existing
/// key keeps its position), matching JS spread semantics.
fn clean_usage_payload(payload: &Value) -> Value {
    let Value::Object(map) = payload else {
        return payload.clone();
    };
    let mut cleaned = map.clone();

    // `.cloned()` before mutating: an `if let`/`match` borrow of `cleaned` that
    // outlives the body would fight the `insert`/`shift_remove` below.
    match cleaned.get("usage").cloned() {
        Some(Value::Null) => {
            cleaned.shift_remove("usage");
        }
        Some(Value::Object(mut usage)) => {
            if matches!(usage.get("perf_metrics"), Some(Value::Null)) {
                usage.shift_remove("perf_metrics");
                cleaned.insert("usage".into(), Value::Object(usage));
            }
        }
        _ => {}
    }

    if let Some(Value::Object(response)) = cleaned.get("response").cloned() {
        let before = Value::Object(response);
        let after = clean_usage_payload(&before);
        if after != before {
            cleaned.insert("response".into(), after);
        }
    }

    Value::Object(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_line_is_none() {
        assert_eq!(parse_sse_line("", None), None);
    }

    #[test]
    fn done_sentinel_becomes_the_done_marker() {
        assert_eq!(
            parse_sse_line("data: [DONE]", None),
            Some(json!({"done": true}))
        );
        assert_eq!(
            parse_sse_line("data: [DONE]   ", None),
            Some(json!({"done": true}))
        );
    }

    #[test]
    fn standard_data_line_parses() {
        let parsed = parse_sse_line(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}",
            None,
        );
        assert_eq!(
            parsed.unwrap()["choices"][0]["delta"]["content"],
            json!("hi")
        );
    }

    #[test]
    fn a_line_not_starting_with_d_is_none() {
        assert_eq!(parse_sse_line("event: ping", None), None);
        assert_eq!(parse_sse_line(":comment", None), None);
    }

    #[test]
    fn malformed_json_is_none() {
        assert_eq!(parse_sse_line("data: {oops", None), None);
    }

    #[test]
    fn a_payload_parsing_to_a_js_falsy_value_is_none() {
        // JSON.parse returns null / 0 / false / "", all of which the JS callers
        // discard via `if (!parsed)`.
        assert_eq!(parse_sse_line("data: null", None), None);
        assert_eq!(parse_sse_line("data: 0", None), None);
        assert_eq!(parse_sse_line("data: false", None), None);
        assert_eq!(parse_sse_line("data: \"\"", None), None);
    }

    #[test]
    fn format_null_keeps_the_key() {
        assert_eq!(format_sse(&Value::Null, None), "data: null\n\n");
    }

    #[test]
    fn format_done_uses_the_sentinel() {
        assert_eq!(format_sse(&json!({"done": true}), None), "data: [DONE]\n\n");
    }

    #[test]
    fn format_responses_passthrough_keeps_the_event_name() {
        let frame = format_sse(
            &json!({"event": "response.completed", "data": {"type": "response.completed"}}),
            None,
        );
        assert_eq!(
            frame,
            "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n"
        );
    }

    #[test]
    fn format_claude_emits_the_event_line_only_for_claude_clients() {
        let chunk = json!({"type": "content_block_delta", "delta": {"text": "x"}});
        assert_eq!(
            format_sse(&chunk, Some(formats::CLAUDE)),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}\n\n"
        );
        assert_eq!(
            format_sse(&chunk, Some(formats::OPENAI)),
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}\n\n"
        );
    }

    #[test]
    fn usage_null_key_is_dropped_in_place() {
        let out = format_sse(&json!({"id": "a", "usage": null, "x": 1}), None);
        assert_eq!(out, "data: {\"id\":\"a\",\"x\":1}\n\n");
    }

    #[test]
    fn usage_perf_metrics_null_is_dropped_but_usage_survives() {
        let out = format_sse(
            &json!({"usage": {"prompt_tokens": 3, "perf_metrics": null}}),
            None,
        );
        assert_eq!(out, "data: {\"usage\":{\"prompt_tokens\":3}}\n\n");
    }

    #[test]
    fn usage_with_a_missing_perf_metrics_key_is_untouched() {
        let out = format_sse(&json!({"usage": {"prompt_tokens": 3}}), None);
        assert_eq!(out, "data: {\"usage\":{\"prompt_tokens\":3}}\n\n");
    }

    #[test]
    fn clean_recurses_into_response_only() {
        let out = format_sse(
            &json!({"response": {"usage": null, "status": "completed"}, "other": {"usage": null}}),
            None,
        );
        assert_eq!(
            out,
            "data: {\"response\":{\"status\":\"completed\"},\"other\":{\"usage\":null}}\n\n"
        );
    }

    #[test]
    fn clean_leaves_arrays_and_scalars_alone() {
        // An array `usage` is not an object payload: `typeof [] === "object"` is
        // true, but the `Array.isArray` guard returns it untouched.
        let out = format_sse(&json!({"usage": [1, 2]}), None);
        assert_eq!(out, "data: {\"usage\":[1,2]}\n\n");
    }
}
