//! CommandCode → OpenAI response.
//!
//! The upstream is AI-SDK v5 NDJSON, one event per line, already split by the
//! commandcode executor. Four decisions are not obvious:
//!
//! - **The tool map is keyed by the raw id value.** String and number ids must
//!   stay distinct, and an absent id needs its own key; `id_key` encodes the
//!   JSON type so collapsing two ids to one string cannot silently merge two
//!   tool calls.
//! - **`undefined` and `null` differ on the wire.** `JSON.stringify` drops a key
//!   whose value is `undefined` but keeps `null`, so a `tool-call` with no
//!   `toolCallId` omits `id`; a `null` id keeps the key.
//! - **The `error` event aborts the stream.** It deliberately refuses to emit
//!   the error as fake `finish_reason: "stop"` content so the stream is marked
//!   errored. `ResponseFn` cannot return an error, so it emits an OpenAI error
//!   frame instead: the client sees `{"error":{…}}` and the stream stops, with
//!   no panic to unwind a worker thread.
//! - **`responseId`/`created`/`chunkIndex`/`toolIndex`/`toolIndexById` are
//!   invented here**, so they live in `state.extra` rather than on the state
//!   struct. `openTools` and `openText` are written but never read; they are
//!   dropped rather than carried as dead scratch.

use serde_json::{Map, Value, json};

use crate::session_manager::now_ms;
use crate::translator::ResponseState;
use crate::translator::concerns::finish_reason::to_openai_finish;
use crate::translator::concerns::primitives::{
    build_chunk, js_number, js_string, js_truthy, reasoning_delta,
};
use crate::translator::concerns::tool_call::fallback_tool_call_id;
use crate::translator::concerns::usage::to_openai_usage;
use crate::translator::formats;
use crate::translator::schema::{openai_block, openai_finish, role};

/// `ensureState(state, model)`: the first call seeds the id/created triple and
/// the tool bookkeeping. Later calls are a no-op.
fn ensure_state(state: &mut ResponseState, event: &Map<String, Value>) {
    if !state.scratch_str("responseId").is_empty() {
        return;
    }
    state
        .extra
        .insert("responseId".into(), json!(format!("chatcmpl-{}", now_ms())));
    state.extra.insert("created".into(), json!(now_ms() / 1000));
    // `state.model || model || "commandcode"`: a falsy either side falls through.
    let event_model = event.get("model").filter(|v| js_truthy(v)).map(js_string);
    state.model = state
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .or(event_model)
        .or_else(|| Some("commandcode".to_string()));
    state.extra.insert("chunkIndex".into(), json!(0));
    state.extra.insert("toolIndex".into(), json!(0));
    state.extra.insert("toolIndexById".into(), json!({}));
    state.finish_reason = None;
    state.usage = None;
}

/// `makeChunk(state, delta, finishReason = null)`.
fn make_chunk(state: &ResponseState, delta: Value, finish_reason: Option<&str>) -> Value {
    let id = json!(state.scratch_str("responseId"));
    let created = state.scratch("created");
    let model = json!(state.model);
    build_chunk(&id, &created, &model, delta, finish_reason)
}

/// Map a commandcode finish reason to its OpenAI spelling.
fn map_finish_reason(reason: Option<&Value>) -> String {
    match reason {
        Some(Value::String(s)) => to_openai_finish(Some(s.as_str()), formats::COMMANDCODE),
        // A non-string reason is not something this wire carries, so it is
        // stringified rather than dropped.
        Some(other) if js_truthy(other) => js_string(other),
        _ => to_openai_finish(None, formats::COMMANDCODE),
    }
}

/// The JS `Map` key for a tool id. Strings and non-strings cannot collide, and
/// an absent id (`undefined`) is its own key.
fn id_key(id: Option<&Value>) -> String {
    match id {
        Some(Value::String(s)) => format!("s:{s}"),
        Some(other) => format!("j:{other}"),
        None => "u:undefined".to_string(),
    }
}

/// `state.chunkIndex`, zero before the first event.
fn chunk_index(state: &ResponseState) -> i64 {
    js_number(Some(&state.scratch("chunkIndex")))
}

/// `state.toolIndex`, zero before the first event.
fn tool_index(state: &ResponseState) -> i64 {
    js_number(Some(&state.scratch("toolIndex")))
}

/// `state.toolIndexById.get(key)`.
fn tool_index_for(state: &ResponseState, key: &str) -> Option<i64> {
    state
        .extra
        .get("toolIndexById")
        .and_then(Value::as_object)
        .and_then(|m| m.get(key))
        .and_then(Value::as_i64)
}

/// `state.toolIndexById.set(key, idx)`.
fn set_tool_index_for(state: &mut ResponseState, key: &str, idx: i64) {
    let entry = state
        .extra
        .entry("toolIndexById")
        .or_insert_with(|| json!({}));
    if let Some(map) = entry.as_object_mut() {
        map.insert(key.to_string(), json!(idx));
    }
}

/// `event.toolName || ""`: a falsy name is the empty string, not the raw value.
fn tool_name_of(event: &Map<String, Value>) -> Value {
    event
        .get("toolName")
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!(""))
}

/// `parseLine(chunk)`: a blank line, `[DONE]`, or unparseable JSON yields
/// nothing.
fn parse_line(raw: &str) -> Option<Value> {
    let line = raw.trim();
    if line.is_empty() {
        return None;
    }
    // Tolerate raw `data: {...}` framing if the upstream wrapper inserts it.
    let json = match line.strip_prefix("data:") {
        Some(rest) => rest.trim(),
        None => line,
    };
    if json.is_empty() || json == "[DONE]" {
        return None;
    }
    serde_json::from_str(json).ok()
}

/// `commandCodeToOpenAIResponse(chunk, state)`.
pub fn commandcode_to_openai_response(chunk: &Value, state: &mut ResponseState) -> Vec<Value> {
    if !js_truthy(chunk) {
        return Vec::new();
    }

    // Already-OpenAI chunk: pass through.
    if chunk.get("object").and_then(Value::as_str) == Some("chat.completion.chunk") {
        return vec![chunk.clone()];
    }

    let event: Value = match chunk {
        Value::String(raw) => match parse_line(raw) {
            Some(parsed) => parsed,
            None => return Vec::new(),
        },
        other => other.clone(),
    };

    // `typeof event !== "object"` rejects numbers and strings; an array carries
    // no `type` and falls to the same nothing.
    let Some(event) = event.as_object() else {
        return Vec::new();
    };
    if !event.get("type").is_some_and(js_truthy) {
        return Vec::new();
    }

    ensure_state(state, event);
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let mut out: Vec<Value> = Vec::new();

    match event_type {
        "text-delta" => {
            let text = event
                .get("text")
                .filter(|v| js_truthy(v))
                .or_else(|| event.get("delta").filter(|v| js_truthy(v)))
                .cloned()
                .unwrap_or_else(|| json!(""));
            if js_truthy(&text) {
                let ci = chunk_index(state);
                let delta = if ci == 0 {
                    json!({"role": role::ASSISTANT, "content": text})
                } else {
                    json!({"content": text})
                };
                state.extra.insert("chunkIndex".into(), json!(ci + 1));
                out.push(make_chunk(state, delta, None));
            }
        }

        "reasoning-delta" => {
            let text = event
                .get("text")
                .filter(|v| js_truthy(v))
                .cloned()
                .unwrap_or_else(|| json!(""));
            if js_truthy(&text) {
                let ci = chunk_index(state);
                // `reasoningDelta` puts the raw value in, so a non-string text
                // is carried as-is rather than coerced.
                let delta = match text.as_str() {
                    Some(text) => reasoning_delta(text, ci == 0),
                    None => {
                        let mut delta = Map::new();
                        if ci == 0 {
                            delta.insert("role".into(), json!(role::ASSISTANT));
                        }
                        delta.insert("reasoning_content".into(), text);
                        Value::Object(delta)
                    }
                };
                state.extra.insert("chunkIndex".into(), json!(ci + 1));
                out.push(make_chunk(state, delta, None));
            }
        }

        "tool-input-start" => {
            // `id || toolCallId || fallback`: both operands must be truthy for
            // the chain to stop short of the generated id.
            let id = event
                .get("id")
                .filter(|v| js_truthy(v))
                .or_else(|| event.get("toolCallId").filter(|v| js_truthy(v)))
                .cloned()
                .unwrap_or_else(|| {
                    json!(fallback_tool_call_id(
                        Some(tool_index(state) as usize),
                        now_ms() as i64
                    ))
                });

            let key = id_key(Some(&id));
            let idx = match tool_index_for(state, &key) {
                Some(idx) => idx,
                None => {
                    let idx = tool_index(state);
                    state.extra.insert("toolIndex".into(), json!(idx + 1));
                    set_tool_index_for(state, &key, idx);
                    idx
                }
            };

            let ci = chunk_index(state);
            let mut delta = Map::new();
            if ci == 0 {
                delta.insert("role".into(), json!(role::ASSISTANT));
            }
            let mut function = Map::new();
            function.insert("name".into(), tool_name_of(event));
            function.insert("arguments".into(), json!(""));
            let mut tool_call = Map::new();
            tool_call.insert("index".into(), json!(idx));
            tool_call.insert("id".into(), id);
            tool_call.insert("type".into(), json!(openai_block::FUNCTION));
            tool_call.insert("function".into(), Value::Object(function));
            delta.insert(
                "tool_calls".into(),
                Value::Array(vec![Value::Object(tool_call)]),
            );
            state.extra.insert("chunkIndex".into(), json!(ci + 1));
            out.push(make_chunk(state, Value::Object(delta), None));
        }

        "tool-input-delta" => {
            // `event.id || event.toolCallId`: a truthy `id` wins, otherwise the
            // raw `toolCallId` is the key — falsy-but-present included, and an
            // absent one is the `undefined` key.
            let id = event
                .get("id")
                .filter(|v| js_truthy(v))
                .or_else(|| event.get("toolCallId"));
            if let Some(idx) = tool_index_for(state, &id_key(id)) {
                let arguments = event
                    .get("delta")
                    .filter(|v| js_truthy(v))
                    .or_else(|| event.get("inputTextDelta").filter(|v| js_truthy(v)))
                    .cloned()
                    .unwrap_or_else(|| json!(""));
                let mut function = Map::new();
                function.insert("arguments".into(), arguments);
                let mut tool_call = Map::new();
                tool_call.insert("index".into(), json!(idx));
                tool_call.insert("function".into(), Value::Object(function));
                out.push(make_chunk(
                    state,
                    json!({"tool_calls": [Value::Object(tool_call)]}),
                    None,
                ));
            }
        }

        "tool-call" => {
            // Final consolidated tool call — only emit if we never saw the
            // tool-input-* deltas.
            let id = event.get("toolCallId");
            let key = id_key(id);
            if tool_index_for(state, &key).is_none() {
                let idx = tool_index(state);
                state.extra.insert("toolIndex".into(), json!(idx + 1));
                set_tool_index_for(state, &key, idx);

                let args_str = match event.get("input") {
                    Some(Value::String(s)) => s.clone(),
                    other => {
                        // `event.input ?? {}`: only null/undefined become `{}`.
                        let input = other
                            .filter(|v| !v.is_null())
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                    }
                };

                let ci = chunk_index(state);
                let mut delta = Map::new();
                if ci == 0 {
                    delta.insert("role".into(), json!(role::ASSISTANT));
                }
                let mut function = Map::new();
                function.insert("name".into(), tool_name_of(event));
                function.insert("arguments".into(), json!(args_str));
                let mut tool_call = Map::new();
                tool_call.insert("index".into(), json!(idx));
                // `id: undefined` is dropped by `JSON.stringify`; a `null` id is
                // kept, so the key is inserted only when the event carried it.
                if let Some(id) = id {
                    tool_call.insert("id".into(), id.clone());
                }
                tool_call.insert("type".into(), json!(openai_block::FUNCTION));
                tool_call.insert("function".into(), Value::Object(function));
                delta.insert(
                    "tool_calls".into(),
                    Value::Array(vec![Value::Object(tool_call)]),
                );
                state.extra.insert("chunkIndex".into(), json!(ci + 1));
                out.push(make_chunk(state, Value::Object(delta), None));
            }
        }

        "finish-step" => {
            state.finish_reason = Some(map_finish_reason(event.get("finishReason")));
            if let Some(usage) = event.get("usage").filter(|v| js_truthy(v)) {
                state.usage = Some(usage.clone());
            }
        }

        "finish" => {
            // An `error` event already emitted the terminal frame; a trailing
            // `finish` must not follow it with a second one.
            if state.finish_reason_sent {
                return Vec::new();
            }
            let finish_reason = state
                .finish_reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| {
                    let raw = event
                        .get("finishReason")
                        .filter(|v| js_truthy(v))
                        .cloned()
                        .unwrap_or_else(|| json!("stop"));
                    map_finish_reason(Some(&raw))
                });
            state.finish_reason_sent = true;
            let mut final_chunk = make_chunk(state, json!({}), Some(finish_reason.as_str()));

            let total_usage = event
                .get("totalUsage")
                .filter(|v| js_truthy(v))
                .cloned()
                .or_else(|| state.usage.clone().filter(js_truthy));
            if let Some(usage) = to_openai_usage(total_usage.as_ref(), formats::COMMANDCODE)
                && let Some(obj) = final_chunk.as_object_mut()
            {
                obj.insert("usage".into(), usage);
            }
            out.push(final_chunk);
        }

        "error" => {
            // `event.error ?? event.message ?? "unknown"`.
            let err_val = event
                .get("error")
                .filter(|v| !v.is_null())
                .or_else(|| event.get("message").filter(|v| !v.is_null()))
                .cloned()
                .unwrap_or_else(|| json!("unknown"));
            let err_str = match &err_val {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_else(|_| "undefined".to_string()),
            };
            // `error` and a later `finish` can both arrive; surface one only.
            if !state.finish_reason_sent {
                state.extra.insert("error".into(), err_val);
                state.finish_reason_sent = true;
                // An error frame, not fake `finish_reason: "stop"` content: the
                // stream is marked errored so the client can tell. Same shape
                // the openai-responses translator emits for its `error` event.
                out.push(make_chunk(
                    state,
                    json!({"content": format!("[Error] {err_str}")}),
                    Some(openai_finish::STOP),
                ));
            }
        }

        // `start`, `start-step`, `reasoning-start`, `text-start`, `text-end`,
        // provider-metadata and friends carry no client-visible content.
        _ => {}
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResponseState {
        ResponseState::default()
    }

    fn run(s: &mut ResponseState, chunk: Value) -> Vec<Value> {
        commandcode_to_openai_response(&chunk, s)
    }

    #[test]
    fn the_first_text_delta_leads_with_the_role() {
        let mut s = state();
        let out = run(&mut s, json!("{\"type\":\"text-delta\",\"text\":\"hi\"}"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["object"], json!("chat.completion.chunk"));
        assert!(out[0]["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert_eq!(out[0]["model"], json!("commandcode"));
        assert_eq!(
            out[0]["choices"][0]["delta"],
            json!({"role": "assistant", "content": "hi"})
        );
        assert_eq!(out[0]["choices"][0]["finish_reason"], Value::Null);

        // The second chunk drops the role.
        let out = run(&mut s, json!({"type": "text-delta", "text": "!"}));
        assert_eq!(out[0]["choices"][0]["delta"], json!({"content": "!"}));
    }

    #[test]
    fn text_falls_back_to_the_delta_field() {
        let mut s = state();
        let out = run(&mut s, json!({"type": "text-delta", "delta": "via-delta"}));
        assert_eq!(out[0]["choices"][0]["delta"]["content"], json!("via-delta"));
    }

    #[test]
    fn reasoning_delta_becomes_reasoning_content() {
        let mut s = state();
        let out = run(&mut s, json!({"type": "reasoning-delta", "text": "why"}));
        assert_eq!(
            out[0]["choices"][0]["delta"],
            json!({"role": "assistant", "reasoning_content": "why"})
        );
        let out = run(&mut s, json!({"type": "reasoning-delta", "text": "more"}));
        assert_eq!(
            out[0]["choices"][0]["delta"],
            json!({"reasoning_content": "more"})
        );
    }

    #[test]
    fn tool_input_start_and_delta_share_the_index() {
        let mut s = state();
        let start = run(
            &mut s,
            json!({"type": "tool-input-start", "id": "c1", "toolName": "f"}),
        );
        let tool_call = &start[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tool_call["index"], json!(0));
        assert_eq!(tool_call["id"], json!("c1"));
        assert_eq!(tool_call["type"], json!("function"));
        assert_eq!(tool_call["function"], json!({"name": "f", "arguments": ""}));

        let delta = run(
            &mut s,
            json!({"type": "tool-input-delta", "id": "c1", "delta": "{\"a\""}),
        );
        assert_eq!(
            delta[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\"")
        );

        // An id we never opened emits nothing.
        let unknown = run(
            &mut s,
            json!({"type": "tool-input-delta", "id": "nope", "delta": "x"}),
        );
        assert!(unknown.is_empty());
    }

    #[test]
    fn a_consolidated_tool_call_is_skipped_when_its_deltas_ran() {
        let mut s = state();
        run(
            &mut s,
            json!({"type": "tool-input-start", "id": "c9", "toolName": "g"}),
        );
        let skipped = run(
            &mut s,
            json!({"type": "tool-call", "toolCallId": "c9", "toolName": "g", "input": {"x": 1}}),
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn a_consolidated_tool_call_serializes_its_input() {
        let mut s = state();
        let out = run(
            &mut s,
            json!({"type": "tool-call", "toolCallId": "c1", "toolName": "g", "input": {"x": 1}}),
        );
        let tool_call = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tool_call["id"], json!("c1"));
        assert_eq!(tool_call["function"]["arguments"], json!("{\"x\":1}"));

        // A string input is used verbatim, not re-quoted.
        let out = run(
            &mut s,
            json!({"type": "tool-call", "toolCallId": "c2", "toolName": "g", "input": "raw"}),
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("raw")
        );
    }

    #[test]
    fn a_tool_call_without_an_id_omits_the_key() {
        let mut s = state();
        let out = run(
            &mut s,
            json!({"type": "tool-call", "toolName": "f", "input": {}}),
        );
        let tool_call = &out[0]["choices"][0]["delta"]["tool_calls"][0];
        assert!(
            tool_call.get("id").is_none(),
            "undefined is dropped, null is kept"
        );

        let keys: Vec<&str> = tool_call
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["index", "type", "function"]);
    }

    #[test]
    fn finish_step_carries_reason_and_usage_into_finish() {
        let mut s = state();
        run(
            &mut s,
            json!({
                "type": "finish-step",
                "finishReason": "tool-calls",
                "usage": {"inputTokens": 7, "outputTokens": 3, "totalTokens": 11},
            }),
        );
        let out = run(&mut s, json!({"type": "finish"}));
        let final_chunk = out.last().unwrap();
        assert_eq!(
            final_chunk["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
        assert_eq!(final_chunk["choices"][0]["delta"], json!({}));
        assert_eq!(final_chunk["usage"]["total_tokens"], json!(11));
        assert_eq!(final_chunk["usage"]["prompt_tokens"], json!(7));
    }

    #[test]
    fn finish_without_a_prior_step_falls_back_to_stop() {
        let mut s = state();
        let out = run(&mut s, json!({"type": "finish"}));
        assert_eq!(out[0]["choices"][0]["finish_reason"], json!("stop"));
        assert!(out[0].get("usage").is_none());
    }

    #[test]
    fn an_openai_chunk_passes_through_untouched() {
        let mut s = state();
        let chunk =
            json!({"object": "chat.completion.chunk", "choices": [{"delta": {"content": "x"}}]});
        assert_eq!(run(&mut s, chunk.clone()), vec![chunk]);
    }

    #[test]
    fn data_framing_done_and_junk_emit_nothing() {
        let mut s = state();
        assert!(run(&mut s, json!("data: {\"type\":\"start\"}")).is_empty());
        assert!(run(&mut s, json!("[DONE]")).is_empty());
        assert!(run(&mut s, json!("not json")).is_empty());
        assert!(run(&mut s, json!("   ")).is_empty());
    }

    #[test]
    fn uninteresting_and_non_object_events_emit_nothing() {
        let mut s = state();
        assert!(run(&mut s, json!({"type": "start"})).is_empty());
        assert!(run(&mut s, json!({"type": "text-start", "id": "t"})).is_empty());
        assert!(run(&mut s, Value::Null).is_empty());
        assert!(run(&mut s, json!(0)).is_empty());
        assert!(run(&mut s, json!([1, 2])).is_empty());
    }

    #[test]
    fn an_error_event_emits_an_error_frame_and_stops() {
        let mut s = state();
        let out = run(&mut s, json!({"type": "error", "error": "boom"}));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "[Error] boom");
        assert_eq!(out[0]["choices"][0]["finish_reason"], "stop");
        assert_eq!(s.extra.get("error"), Some(&json!("boom")));
        // A `finish` after the error must not emit a second chunk.
        assert!(run(&mut s, json!({"type": "finish"})).is_empty());
    }

    #[test]
    fn an_error_without_a_payload_names_unknown() {
        let mut s = state();
        let out = run(&mut s, json!({"type": "error"}));
        assert_eq!(out[0]["choices"][0]["delta"]["content"], "[Error] unknown");
    }
}
