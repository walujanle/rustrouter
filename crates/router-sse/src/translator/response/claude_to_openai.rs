//! Claude → OpenAI response.
//!
//! Cache tokens arrive split across two events: `message_start` carries
//! `input_tokens` plus the cache counters, `message_delta` carries only
//! `output_tokens`. Capturing cache at `message_start` and merging at
//! `message_delta` is what keeps `prompt_tokens` from collapsing to the
//! output-only figure; a translator that reads each event independently gets
//! the cache wrong on every turn.

use serde_json::{Map, Value, json};

use crate::session_manager::now_ms;
use crate::translator::ResponseState;
use crate::translator::concerns::finish_reason::to_openai_finish;
use crate::translator::concerns::primitives::{build_chunk, reasoning_delta};
use crate::translator::concerns::usage::to_openai_usage;
use crate::translator::schema::{claude_block, openai_block, openai_finish, role};

/// `createChunk(state, delta, finishReason)`.
fn create_chunk(state: &ResponseState, delta: Value, finish_reason: Option<&str>) -> Value {
    let id = json!(format!(
        "chatcmpl-{}",
        state.message_id.as_deref().unwrap_or("")
    ));
    let created = json!(now_ms() / 1000);
    let model = json!(state.model);
    build_chunk(&id, &created, &model, delta, finish_reason)
}

/// The map key for a numeric chunk index, `Map.set(index)`.
fn index_key(chunk: &Value) -> Option<String> {
    chunk.get("index").map(|i| match i {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

/// `claudeToOpenAIResponse(chunk, state)`.
pub fn claude_to_openai_response(chunk: &Value, state: &mut ResponseState) -> Vec<Value> {
    if chunk.is_null() {
        return Vec::new();
    }
    let mut results: Vec<Value> = Vec::new();
    let event = chunk.get("type").and_then(Value::as_str).unwrap_or("");

    match event {
        "message_start" => {
            let message = chunk.get("message");
            state.message_id = Some(
                message
                    .and_then(|m| m.get("id"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("msg_{}", now_ms())),
            );
            state.model = message
                .and_then(|m| m.get("model"))
                .and_then(Value::as_str)
                .map(str::to_string);
            state.extra.insert("toolCallIndex".into(), json!(0));

            // Cache arrives here; message_delta carries only output_tokens.
            if let Some(start_usage) = message
                .and_then(|m| m.get("usage"))
                .filter(|u| u.is_object())
            {
                let num = |key: &str| start_usage.get(key).and_then(Value::as_i64).unwrap_or(0);
                let input_tokens = num("input_tokens");
                let cache_read_tokens = num("cache_read_input_tokens");
                let cache_creation_tokens = num("cache_creation_input_tokens");
                let prompt_tokens = input_tokens + cache_read_tokens + cache_creation_tokens;

                let mut usage = Map::new();
                usage.insert("prompt_tokens".into(), json!(prompt_tokens));
                usage.insert("completion_tokens".into(), json!(0));
                usage.insert("total_tokens".into(), json!(prompt_tokens));
                usage.insert("input_tokens".into(), json!(input_tokens));
                usage.insert("output_tokens".into(), json!(0));
                if cache_read_tokens > 0 {
                    usage.insert("cache_read_input_tokens".into(), json!(cache_read_tokens));
                }
                if cache_creation_tokens > 0 {
                    usage.insert(
                        "cache_creation_input_tokens".into(),
                        json!(cache_creation_tokens),
                    );
                }
                state.usage = Some(Value::Object(usage));
            }
            results.push(create_chunk(state, json!({"role": role::ASSISTANT}), None));
        }

        "content_block_start" => {
            let block = chunk.get("content_block");
            match block.and_then(|b| b.get("type")).and_then(Value::as_str) {
                Some(claude_block::SERVER_TOOL_USE) => {
                    // Built-in tool (web search): Claude handles it internally.
                    state.extra.insert(
                        "serverToolBlockIndex".into(),
                        chunk.get("index").cloned().unwrap_or(Value::Null),
                    );
                }
                Some(claude_block::TEXT) => {
                    state.text_block_started = true;
                }
                Some(claude_block::THINKING) => {
                    // Thinking travels only in `reasoning_content` (see the
                    // `thinking_delta` arm). Emitting `<think>` into `content`
                    // would make an OpenAI-format client render the tag as
                    // literal text.
                    state.in_thinking_block = true;
                    state.current_block_index = chunk.get("index").and_then(Value::as_i64);
                }
                Some(claude_block::TOOL_USE) => {
                    let tool_call_index = state
                        .extra
                        .get("toolCallIndex")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    state
                        .extra
                        .insert("toolCallIndex".into(), json!(tool_call_index + 1));
                    // Restore the original tool name (Claude OAuth cloaking).
                    let raw_name = block
                        .and_then(|b| b.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let tool_name = state
                        .tool_name_map
                        .as_ref()
                        .and_then(|m| m.get(raw_name))
                        .cloned()
                        .unwrap_or_else(|| raw_name.to_string());
                    let tool_call = json!({
                        "index": tool_call_index,
                        "id": block.and_then(|b| b.get("id")),
                        "type": openai_block::FUNCTION,
                        "function": {"name": tool_name, "arguments": ""},
                    });
                    if let Some(key) = index_key(chunk) {
                        state.tool_calls.insert(key, tool_call.clone());
                    }
                    results.push(create_chunk(
                        state,
                        json!({"tool_calls": [tool_call]}),
                        None,
                    ));
                }
                _ => {}
            }
        }

        "content_block_delta" => {
            // Skip deltas for built-in server tool blocks.
            let is_server_tool = chunk.get("index") == state.extra.get("serverToolBlockIndex");
            if is_server_tool {
                return results;
            }
            let delta = chunk.get("delta");
            let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
            match delta_type {
                Some("text_delta") => {
                    if let Some(text) = delta.and_then(|d| d.get("text")).filter(|t| truthy(t)) {
                        results.push(create_chunk(state, json!({"content": text}), None));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(thinking) =
                        delta.and_then(|d| d.get("thinking")).filter(|t| truthy(t))
                    {
                        results.push(create_chunk(
                            state,
                            reasoning_delta(thinking.as_str().unwrap_or(""), false),
                            None,
                        ));
                    }
                }
                Some("input_json_delta") => {
                    if let Some(partial) = delta
                        .and_then(|d| d.get("partial_json"))
                        .filter(|t| truthy(t))
                        && let Some(key) = index_key(chunk)
                        && let Some(tool_call) = state.tool_calls.get_mut(&key)
                    {
                        let existing = tool_call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let appended = format!("{existing}{}", partial.as_str().unwrap_or(""));
                        if let Some(f) =
                            tool_call.get_mut("function").and_then(Value::as_object_mut)
                        {
                            f.insert("arguments".into(), json!(appended));
                        }
                        let index = tool_call.get("index").cloned().unwrap_or(Value::Null);
                        let id = tool_call.get("id").cloned().unwrap_or(Value::Null);
                        results.push(create_chunk(
                            state,
                            json!({"tool_calls": [{
                                "index": index,
                                "id": id,
                                "function": {"arguments": partial},
                            }]}),
                            None,
                        ));
                    }
                }
                _ => {}
            }
        }

        "content_block_stop" => {
            if chunk.get("index") == state.extra.get("serverToolBlockIndex") {
                state.extra.insert("serverToolBlockIndex".into(), json!(-1));
                return results;
            }
            let index = chunk.get("index").and_then(Value::as_i64);
            if state.in_thinking_block && index == state.current_block_index {
                state.in_thinking_block = false;
            }
            state.text_block_started = false;
            state.thinking_block_started = false;
        }

        "message_delta" => {
            if let Some(usage) = chunk.get("usage").filter(|u| u.is_object()) {
                let prev = state.usage.clone().unwrap_or(Value::Null);
                let prev_num = |key: &str| prev.get(key).and_then(Value::as_i64).unwrap_or(0);
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| prev_num("input_tokens"));
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let cache_read_tokens = usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| prev_num("cache_read_input_tokens"));
                let cache_creation_tokens = usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or_else(|| prev_num("cache_creation_input_tokens"));
                let prompt_tokens = input_tokens + cache_read_tokens + cache_creation_tokens;

                let mut merged = Map::new();
                merged.insert("prompt_tokens".into(), json!(prompt_tokens));
                merged.insert("completion_tokens".into(), json!(output_tokens));
                merged.insert("total_tokens".into(), json!(prompt_tokens + output_tokens));
                merged.insert("input_tokens".into(), json!(input_tokens));
                merged.insert("output_tokens".into(), json!(output_tokens));
                if cache_read_tokens > 0 {
                    merged.insert("cache_read_input_tokens".into(), json!(cache_read_tokens));
                }
                if cache_creation_tokens > 0 {
                    merged.insert(
                        "cache_creation_input_tokens".into(),
                        json!(cache_creation_tokens),
                    );
                }
                state.usage = Some(Value::Object(merged));
            }

            let delta = chunk.get("delta");
            if let Some(stop_reason) = delta
                .and_then(|d| d.get("stop_reason"))
                .filter(|r| truthy(r))
            {
                let stop_reason = stop_reason.as_str().unwrap_or("");
                state.finish_reason = Some(to_openai_finish(
                    Some(stop_reason),
                    crate::translator::formats::CLAUDE,
                ));
                // A refusal produces no content blocks. Surface Anthropic's own
                // explanation so the client shows why the turn is empty.
                if stop_reason == "refusal"
                    && let Some(explanation) = delta
                        .and_then(|d| d.get("stop_details"))
                        .and_then(|s| s.get("explanation"))
                        .filter(|e| truthy(e))
                {
                    results.push(create_chunk(state, json!({"content": explanation}), None));
                }
                let finish_reason = state.finish_reason.clone();
                let mut final_chunk = create_chunk(state, json!({}), finish_reason.as_deref());

                if let Some(usage) = state.usage.clone() {
                    // Merge cache from message_start with output from message_delta.
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let merged = json!({
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cache_read_input_tokens": usage.get("cache_read_input_tokens").cloned(),
                        "cache_creation_input_tokens": usage.get("cache_creation_input_tokens").cloned(),
                    });
                    if let Some(openai_usage) = to_openai_usage(Some(&merged), "claude")
                        && let Some(obj) = final_chunk.as_object_mut()
                    {
                        obj.insert("usage".into(), openai_usage);
                    }
                }

                results.push(final_chunk);
                state.finish_reason_sent = true;
            }
        }

        "message_stop" if !state.finish_reason_sent => {
            let finish_reason = state.finish_reason.clone().unwrap_or_else(|| {
                if !state.tool_calls.is_empty() {
                    openai_finish::TOOL_CALLS.to_string()
                } else {
                    openai_finish::STOP.to_string()
                }
            });
            let mut chunk = create_chunk(state, json!({}), Some(&finish_reason));
            if let Some(usage) = state.usage.clone() {
                let input_tokens = usage
                    .get("input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let output_tokens = usage
                    .get("output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if let Some(obj) = chunk.as_object_mut() {
                    obj.insert(
                        "usage".into(),
                        json!({
                            "prompt_tokens": input_tokens,
                            "completion_tokens": output_tokens,
                            "total_tokens": input_tokens + output_tokens,
                        }),
                    );
                }
            }
            results.push(chunk);
            state.finish_reason_sent = true;
        }

        _ => {}
    }

    results
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResponseState {
        ResponseState::default()
    }

    #[test]
    fn message_start_captures_cache_and_emits_the_role_chunk() {
        let mut s = state();
        let out = claude_to_openai_response(
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_1", "model": "claude-3",
                    "usage": {"input_tokens": 100, "cache_read_input_tokens": 20, "cache_creation_input_tokens": 5},
                },
            }),
            &mut s,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"], json!({"role": "assistant"}));
        assert_eq!(s.message_id.as_deref(), Some("msg_1"));
        let usage = s.usage.clone().unwrap();
        assert_eq!(usage["prompt_tokens"], json!(125));
        assert_eq!(usage["cache_read_input_tokens"], json!(20));
    }

    #[test]
    fn thinking_blocks_emit_reasoning_content_without_think_tags() {
        let mut s = state();
        let start = claude_to_openai_response(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
            &mut s,
        );
        // No `<think>` marker: an OpenAI-format client would render it as text.
        assert!(
            start.is_empty(),
            "the start of a thinking block emits no chunk"
        );
        let delta = claude_to_openai_response(
            &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            &mut s,
        );
        assert_eq!(
            delta[0]["choices"][0]["delta"],
            json!({"reasoning_content": "hmm"})
        );
        let stop =
            claude_to_openai_response(&json!({"type": "content_block_stop", "index": 0}), &mut s);
        // No `</think>` either; the block just closes.
        assert!(
            stop.is_empty(),
            "the end of a thinking block emits no chunk"
        );
        assert!(!s.in_thinking_block);
    }

    #[test]
    fn tool_use_accumulates_partial_json_arguments() {
        let mut s = state();
        claude_to_openai_response(
            &json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "f"}}),
            &mut s,
        );
        let out = claude_to_openai_response(
            &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":1}"}}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
        assert_eq!(
            s.tool_calls["0"]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn server_tool_blocks_are_skipped() {
        let mut s = state();
        claude_to_openai_response(
            &json!({"type": "content_block_start", "index": 3, "content_block": {"type": "server_tool_use"}}),
            &mut s,
        );
        let out = claude_to_openai_response(
            &json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "x"}}),
            &mut s,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn message_delta_merges_cache_from_message_start_and_sets_usage() {
        let mut s = state();
        claude_to_openai_response(
            &json!({"type": "message_start", "message": {"id": "m", "usage": {"input_tokens": 10, "cache_read_input_tokens": 5}}}),
            &mut s,
        );
        let out = claude_to_openai_response(
            &json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}}),
            &mut s,
        );
        let final_chunk = out.last().unwrap();
        assert_eq!(final_chunk["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(final_chunk["usage"]["prompt_tokens"], json!(15));
        assert_eq!(final_chunk["usage"]["completion_tokens"], json!(7));
        assert!(s.finish_reason_sent);
    }

    #[test]
    fn a_refusal_surfaces_its_explanation() {
        let mut s = state();
        let out = claude_to_openai_response(
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": "refusal", "stop_details": {"explanation": "blocked"}},
            }),
            &mut s,
        );
        assert_eq!(out[0]["choices"][0]["delta"], json!({"content": "blocked"}));
        assert_eq!(
            out[1]["choices"][0]["finish_reason"],
            json!("content_filter")
        );
    }

    #[test]
    fn message_stop_without_a_prior_finish_uses_tool_calls_when_tools_ran() {
        let mut s = state();
        s.message_id = Some("m".into());
        s.tool_calls.insert("0".into(), json!({"index": 0}));
        let out = claude_to_openai_response(&json!({"type": "message_stop"}), &mut s);
        assert_eq!(out[0]["choices"][0]["finish_reason"], json!("tool_calls"));
    }

    #[test]
    fn an_unknown_event_emits_nothing() {
        let mut s = state();
        assert!(claude_to_openai_response(&json!({"type": "ping"}), &mut s).is_empty());
        assert!(claude_to_openai_response(&Value::Null, &mut s).is_empty());
    }
}
