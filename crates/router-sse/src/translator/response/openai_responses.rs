//! OpenAI Chat Completions ↔ OpenAI Responses API responses.
//!
//! This is the only response translator that emits a *framed* event
//! (`{ event, data }`) rather than a bare chunk, and the only one carrying both
//! directions in a single file. Three decisions are not obvious:
//!
//! - **Completion is deferred on the direct route.** Upstreams report usage
//!   either on the finish chunk itself or on a trailing chunk whose `choices`
//!   array is empty (OpenAI does the latter). Emitting `response.completed` on
//!   the finish chunk would freeze the payload before that trailing chunk is
//!   parsed, so when usage is not known yet we leave completion to
//!   `flush_events()`, which runs once the upstream stream ends. That only holds
//!   when the converter is reached directly (`target_format == openai`); as the
//!   second hop of a pivot the terminal `null` chunk is dropped before reaching
//!   us, so deferring would swallow the terminal event. `state.target_format` is
//!   the discriminator.
//! - **Usage is stored under `responsesUsage`, not `usage`.** `state.usage` is
//!   owned by the stream layer, which fills it with `normalizeUsage()`-shaped
//!   counts for logging and cost accounting; overwriting it with the Responses
//!   shape silently drops cached/reasoning tokens from those stats.
//! - **A custom tool streams its input once, at close.** Chat wraps the
//!   freeform program in `{"input": "..."}`; streaming the raw JSON fragments
//!   would expose the wrapper instead of the program Codex expects, so the
//!   buffer is unwrapped only when the call closes.
//!
//! Translator-invented scratch (`responsesUsage`, `chatId`, `toolCallIndex`,
//! `currentToolCallId`, `respToolChatIndex`, `respToolArgsEmitted`, `error`)
//! lives in `state.extra`. One deliberate deviation: `String.prototype.includes`
//! / `replace` on `delta.content` would throw on a non-string; here such a value
//! is stringified instead.

use serde_json::{Map, Value, json};

use crate::session_manager::now_ms;
use crate::translator::ResponseState;
use crate::translator::concerns::primitives::{
    build_chunk, extract_reasoning_text, js_string, js_truthy, js_truthy_opt, reasoning_delta,
};
use crate::translator::concerns::tool_call::fallback_tool_call_id;
use crate::translator::concerns::usage::{UsageArgs, build_usage};
use crate::translator::formats;
use crate::translator::schema::{
    MODEL_FALLBACK, openai_block, openai_finish, responses_item, role,
};

/// `Number.isFinite(x)`: only a real number qualifies, so `"5"` and `null` do not.
fn finite_number(value: Option<&Value>) -> Option<&Value> {
    value.filter(|v| v.as_f64().is_some_and(f64::is_finite))
}

/// `a + b` for the two resolved token counts, keeping integers integral.
fn number_sum(a: &Value, b: &Value) -> Value {
    let sum = a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0);
    if sum.fract() == 0.0 {
        json!(sum as i64)
    } else {
        json!(sum)
    }
}

/// The key a JS object would use for this index. JS stringifies keys, so the
/// number `0` and the string `"0"` collide — reproduce that.
fn key_of(idx: &Value) -> String {
    match idx {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `parseInt(idx)`: a number truncates, a string reads its leading integer, and
/// anything unparseable serializes as `null` (`JSON.stringify(NaN)`).
fn js_parse_int(idx: &Value) -> Value {
    match idx {
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.is_finite() => json!(f.trunc() as i64),
            _ => Value::Null,
        },
        Value::String(s) => {
            let t = s.trim_start();
            let (sign, digits) = match t.strip_prefix('-') {
                Some(rest) => (-1i64, rest),
                None => (1i64, t.strip_prefix('+').unwrap_or(t)),
            };
            let digits: String = digits.chars().take_while(|c| c.is_ascii_digit()).collect();
            if digits.is_empty() {
                Value::Null
            } else {
                match digits.parse::<i64>() {
                    Ok(n) => json!(sign * n),
                    Err(_) => Value::Null,
                }
            }
        }
        _ => Value::Null,
    }
}

/// `for (const i in obj)` order: canonical integer-index keys ascending first,
/// then the remaining keys in insertion order.
fn js_key_order(map: &Map<String, Value>) -> Vec<String> {
    let mut ints: Vec<(u64, String)> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for key in map.keys() {
        match key.parse::<u64>() {
            Ok(n) if n <= 4_294_967_294 && n.to_string() == *key => ints.push((n, key.clone())),
            _ => rest.push(key.clone()),
        }
    }
    ints.sort_by_key(|(n, _)| *n);
    ints.into_iter().map(|(_, k)| k).chain(rest).collect()
}

/// `emit(eventType, data)`: `sequence_number` is assigned after the literal, so
/// it lands as the last key of the payload.
fn emit(state: &mut ResponseState, events: &mut Vec<Value>, event_type: &str, mut data: Value) {
    let seq = state.next_seq();
    if let Some(obj) = data.as_object_mut() {
        obj.insert("sequence_number".into(), json!(seq));
    }
    events.push(json!({"event": event_type, "data": data}));
}

/// `toResponsesUsage(usage)`: upstream Chat Completions usage → Responses shape.
/// `null` for a non-object.
fn to_responses_usage(usage: &Value) -> Value {
    if !usage.is_object() {
        return Value::Null;
    }

    let input_tokens = finite_number(usage.get("input_tokens"))
        .or_else(|| finite_number(usage.get("prompt_tokens")))
        .cloned()
        .unwrap_or(json!(0));
    let output_tokens = finite_number(usage.get("output_tokens"))
        .or_else(|| finite_number(usage.get("completion_tokens")))
        .cloned()
        .unwrap_or(json!(0));

    let mut response_usage = Map::new();
    response_usage.insert("input_tokens".into(), input_tokens.clone());
    response_usage.insert("output_tokens".into(), output_tokens.clone());
    response_usage.insert(
        "total_tokens".into(),
        finite_number(usage.get("total_tokens"))
            .cloned()
            .unwrap_or_else(|| number_sum(&input_tokens, &output_tokens)),
    );

    let cached_tokens = finite_number(usage.pointer("/input_tokens_details/cached_tokens"))
        .or_else(|| finite_number(usage.pointer("/prompt_tokens_details/cached_tokens")));
    let reasoning_tokens = finite_number(usage.pointer("/output_tokens_details/reasoning_tokens"))
        .or_else(|| finite_number(usage.pointer("/completion_tokens_details/reasoning_tokens")));
    if let Some(cached) = cached_tokens {
        response_usage.insert(
            "input_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    if let Some(reasoning) = reasoning_tokens {
        response_usage.insert(
            "output_tokens_details".into(),
            json!({"reasoning_tokens": reasoning}),
        );
    }

    Value::Object(response_usage)
}

/// `openaiToOpenAIResponsesResponse(chunk, state)`.
///
/// A `null` chunk is the flush signal and is handled here; the direct-route
/// stream layer drops it before calling, in which case the terminal events come
/// from the finish chunk instead.
pub fn openai_to_openai_responses_response(chunk: &Value, state: &mut ResponseState) -> Vec<Value> {
    if chunk.is_null() {
        return flush_events(state);
    }

    // Captured before the choices guard: the last OpenAI chunk may carry usage
    // together with an empty choices array, and it must not be dropped.
    if let Some(usage) = chunk.get("usage").filter(|u| js_truthy(u)) {
        let converted = to_responses_usage(usage);
        state.extra.insert("responsesUsage".into(), converted);
    }

    if !chunk
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|c| !c.is_empty())
    {
        return Vec::new();
    }

    let mut events: Vec<Value> = Vec::new();

    let choice = &chunk["choices"][0];
    let idx = choice
        .get("index")
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or(json!(0));
    let delta = choice
        .get("delta")
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| json!({}));

    if !state.started {
        state.started = true;
        if let Some(id) = chunk.get("id").filter(|v| js_truthy(v)) {
            state.response_id = format!("resp_{}", js_string(id));
        }
        let response_id = state.response_id.clone();
        let created = state.created;

        emit(
            state,
            &mut events,
            "response.created",
            json!({
                "type": "response.created",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created,
                    "status": "in_progress",
                    "background": false,
                    "error": null,
                    "output": [],
                },
            }),
        );

        emit(
            state,
            &mut events,
            "response.in_progress",
            json!({
                "type": "response.in_progress",
                "response": {
                    "id": response_id,
                    "object": "response",
                    "created_at": created,
                    "status": "in_progress",
                },
            }),
        );
    }

    let reasoning_text = extract_reasoning_text(&delta);
    if !reasoning_text.is_empty() {
        start_reasoning(state, &mut events, &idx);
        emit_reasoning_delta(state, &mut events, &reasoning_text);
    }

    if let Some(content_value) = delta.get("content").filter(|v| js_truthy(v)) {
        let mut content = js_string(content_value);

        if content.contains("<think>") {
            state.in_thinking = true;
            content = content.replacen("<think>", "", 1);
            start_reasoning(state, &mut events, &idx);
        }

        if content.contains("</think>") {
            let parts: Vec<&str> = content.split("</think>").collect();
            let think_part = parts[0];
            let text_part = parts[1..].join("</think>");
            if !think_part.is_empty() {
                emit_reasoning_delta(state, &mut events, think_part);
            }
            close_reasoning(state, &mut events);
            state.in_thinking = false;
            content = text_part;
        }

        if state.in_thinking && !content.is_empty() {
            emit_reasoning_delta(state, &mut events, &content);
            return events;
        }

        if !content.is_empty() {
            // Text after a reasoning block closes it first: the reasoning item
            // must be emitted before the message item it precedes.
            close_reasoning(state, &mut events);
            emit_text_content(state, &mut events, &idx, &content);
        }
    }

    // An empty `tool_calls` array is truthy in JS; require a real call.
    if let Some(tool_calls) = delta
        .get("tool_calls")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
    {
        close_reasoning(state, &mut events);
        close_message(state, &mut events, &idx);
        for tc in tool_calls {
            emit_tool_call(state, &mut events, tc);
        }
    }

    if choice.get("finish_reason").is_some_and(js_truthy) {
        let message_keys = js_key_order(&state.msg_item_added);
        for i in message_keys {
            close_message(state, &mut events, &json!(i));
        }
        close_reasoning(state, &mut events);
        let call_keys = js_key_order(&state.func_call_ids);
        for i in call_keys {
            close_tool_call(state, &mut events, &json!(i));
        }

        // Deferral only holds when the terminal chunk reaches us: as the second
        // hop of a pivot the stream layer drops it, so flush never runs and
        // deferring would swallow the terminal event.
        let flush_reaches_us = state.target_format.as_deref() == Some(formats::OPENAI);
        if state.extra.get("responsesUsage").is_some_and(js_truthy) || !flush_reaches_us {
            send_completed(state, &mut events);
        }
    }

    events
}

fn start_reasoning(state: &mut ResponseState, events: &mut Vec<Value>, idx: &Value) {
    if !state.reasoning_id.is_empty() {
        return;
    }
    state.reasoning_id = format!("rs_{}_{}", state.response_id, js_string(idx));
    state.reasoning_index = idx.as_i64().unwrap_or(0);
    let reasoning_id = state.reasoning_id.clone();

    emit(
        state,
        events,
        "response.output_item.added",
        json!({
            "type": "response.output_item.added",
            "output_index": idx,
            "item": {"id": reasoning_id, "type": responses_item::REASONING, "summary": []},
        }),
    );

    emit(
        state,
        events,
        "response.reasoning_summary_part.added",
        json!({
            "type": "response.reasoning_summary_part.added",
            "item_id": reasoning_id,
            "output_index": idx,
            "summary_index": 0,
            "part": {"type": responses_item::SUMMARY_TEXT, "text": ""},
        }),
    );
    state.reasoning_part_added = true;
}

fn emit_reasoning_delta(state: &mut ResponseState, events: &mut Vec<Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    state.reasoning_buf.push_str(text);
    let reasoning_id = state.reasoning_id.clone();
    let reasoning_index = state.reasoning_index;
    emit(
        state,
        events,
        "response.reasoning_summary_text.delta",
        json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": reasoning_id,
            "output_index": reasoning_index,
            "summary_index": 0,
            "delta": text,
        }),
    );
}

fn close_reasoning(state: &mut ResponseState, events: &mut Vec<Value>) {
    if state.reasoning_id.is_empty() || state.reasoning_done {
        return;
    }
    state.reasoning_done = true;
    let reasoning_id = state.reasoning_id.clone();
    let reasoning_index = state.reasoning_index;
    let reasoning_buf = state.reasoning_buf.clone();

    emit(
        state,
        events,
        "response.reasoning_summary_text.done",
        json!({
            "type": "response.reasoning_summary_text.done",
            "item_id": reasoning_id,
            "output_index": reasoning_index,
            "summary_index": 0,
            "text": reasoning_buf,
        }),
    );

    emit(
        state,
        events,
        "response.reasoning_summary_part.done",
        json!({
            "type": "response.reasoning_summary_part.done",
            "item_id": reasoning_id,
            "output_index": reasoning_index,
            "summary_index": 0,
            "part": {"type": responses_item::SUMMARY_TEXT, "text": reasoning_buf},
        }),
    );

    emit(
        state,
        events,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": reasoning_index,
            "item": {
                "id": reasoning_id,
                "type": responses_item::REASONING,
                "summary": [{"type": responses_item::SUMMARY_TEXT, "text": reasoning_buf}],
            },
        }),
    );
}

fn emit_text_content(
    state: &mut ResponseState,
    events: &mut Vec<Value>,
    idx: &Value,
    content: &str,
) {
    let key = key_of(idx);
    let response_id = state.response_id.clone();
    if !js_truthy_opt(state.msg_item_added.get(&key)) {
        state.msg_item_added.insert(key.clone(), json!(true));
        let msg_id = format!("msg_{}_{}", response_id, key);
        emit(
            state,
            events,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": idx,
                "item": {"id": msg_id, "type": responses_item::MESSAGE, "content": [], "role": role::ASSISTANT},
            }),
        );
    }

    if !js_truthy_opt(state.msg_content_added.get(&key)) {
        state.msg_content_added.insert(key.clone(), json!(true));
        emit(
            state,
            events,
            "response.content_part.added",
            json!({
                "type": "response.content_part.added",
                "item_id": format!("msg_{}_{}", response_id, key),
                "output_index": idx,
                "content_index": 0,
                "part": {"type": responses_item::OUTPUT_TEXT, "annotations": [], "logprobs": [], "text": ""},
            }),
        );
    }

    emit(
        state,
        events,
        "response.output_text.delta",
        json!({
            "type": "response.output_text.delta",
            "item_id": format!("msg_{}_{}", response_id, key),
            "output_index": idx,
            "content_index": 0,
            "delta": content,
            "logprobs": [],
        }),
    );

    let existing = state
        .msg_text_buf
        .get(&key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    state
        .msg_text_buf
        .insert(key, json!(format!("{existing}{content}")));
}

fn close_message(state: &mut ResponseState, events: &mut Vec<Value>, idx: &Value) {
    let key = key_of(idx);
    if !js_truthy_opt(state.msg_item_added.get(&key))
        || js_truthy_opt(state.msg_item_done.get(&key))
    {
        return;
    }
    state.msg_item_done.insert(key.clone(), json!(true));

    let full_text = state
        .msg_text_buf
        .get(&key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let msg_id = format!("msg_{}_{}", state.response_id, key);
    let parsed_index = js_parse_int(idx);

    emit(
        state,
        events,
        "response.output_text.done",
        json!({
            "type": "response.output_text.done",
            "item_id": msg_id,
            "output_index": parsed_index,
            "content_index": 0,
            "text": full_text,
            "logprobs": [],
        }),
    );

    emit(
        state,
        events,
        "response.content_part.done",
        json!({
            "type": "response.content_part.done",
            "item_id": msg_id,
            "output_index": parsed_index,
            "content_index": 0,
            "part": {"type": responses_item::OUTPUT_TEXT, "annotations": [], "logprobs": [], "text": full_text},
        }),
    );

    emit(
        state,
        events,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": parsed_index,
            "item": {
                "id": msg_id,
                "type": responses_item::MESSAGE,
                "content": [{"type": responses_item::OUTPUT_TEXT, "annotations": [], "logprobs": [], "text": full_text}],
                "role": role::ASSISTANT,
            },
        }),
    );
}

/// `isCustomTool(state, name)`.
fn is_custom_tool(state: &ResponseState, name: &str) -> bool {
    state.is_custom_tool(name)
}

/// `extractCustomToolInput(argumentsText)`: unwrap `{"input": "..."}`, else the
/// raw text. A non-string is `""`.
fn extract_custom_tool_input(arguments_text: &Value) -> String {
    let Some(text) = arguments_text.as_str() else {
        return String::new();
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(text)
        && parsed.is_object()
        && let Some(input) = parsed.get("input").and_then(Value::as_str)
    {
        return input.to_string();
    }
    text.to_string()
}

fn emit_tool_call(state: &mut ResponseState, events: &mut Vec<Value>, tc: &Value) {
    let tc_idx = tc
        .get("index")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or(json!(0));
    let key = key_of(&tc_idx);
    let new_call_id = tc.get("id").cloned().unwrap_or(Value::Null);
    let func_name = tc.pointer("/function/name").cloned().unwrap_or(Value::Null);

    if js_truthy(&func_name) {
        state.func_names.insert(key.clone(), func_name.clone());
    }
    if js_truthy(&new_call_id) {
        state.func_call_ids.insert(key.clone(), new_call_id.clone());
    }

    // Some providers split the call id and function name across chunks; wait for
    // both before announcing, or an `exec` call can be irreversibly announced as
    // a function_call.
    let call_id = state
        .func_call_ids
        .get(&key)
        .cloned()
        .unwrap_or(Value::Null);
    let name = state.func_names.get(&key).cloned().unwrap_or(Value::Null);
    if !js_truthy_opt(state.func_item_added.get(&key)) && js_truthy(&call_id) && js_truthy(&name) {
        state.func_item_added.insert(key.clone(), json!(true));
        let custom = is_custom_tool(state, name.as_str().unwrap_or(""));

        let mut item = Map::new();
        item.insert(
            "id".into(),
            json!(format!(
                "{}_{}",
                if custom { "ctc" } else { "fc" },
                js_string(&call_id)
            )),
        );
        item.insert(
            "type".into(),
            json!(if custom {
                responses_item::CUSTOM_TOOL_CALL
            } else {
                responses_item::FUNCTION_CALL
            }),
        );
        if custom {
            item.insert("input".into(), json!(""));
        } else {
            item.insert("arguments".into(), json!(""));
        }
        item.insert("call_id".into(), call_id.clone());
        item.insert("name".into(), json!(name.as_str().unwrap_or("")));

        emit(
            state,
            events,
            "response.output_item.added",
            json!({
                "type": "response.output_item.added",
                "output_index": tc_idx,
                "item": Value::Object(item),
            }),
        );
    }

    let existing = state
        .func_args_buf
        .get(&key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    state.func_args_buf.insert(key.clone(), json!(existing));

    if let Some(arguments) = tc.pointer("/function/arguments").filter(|v| js_truthy(v)) {
        let ref_call_id = state
            .func_call_ids
            .get(&key)
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or(new_call_id);
        if js_truthy_opt(state.func_item_added.get(&key))
            && js_truthy(&ref_call_id)
            && !is_custom_tool(state, name.as_str().unwrap_or(""))
        {
            emit(
                state,
                events,
                "response.function_call_arguments.delta",
                json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": format!("fc_{}", js_string(&ref_call_id)),
                    "output_index": tc_idx,
                    "delta": arguments,
                }),
            );
        }
        // Custom input is emitted once at close, after the Chat JSON wrapper can
        // be parsed and unwrapped.
        let buffered = state
            .func_args_buf
            .get(&key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        state
            .func_args_buf
            .insert(key, json!(format!("{buffered}{}", js_string(arguments))));
    }
}

fn close_tool_call(state: &mut ResponseState, events: &mut Vec<Value>, idx: &Value) {
    let key = key_of(idx);
    let call_id = state
        .func_call_ids
        .get(&key)
        .cloned()
        .unwrap_or(Value::Null);
    if !js_truthy(&call_id) || js_truthy_opt(state.func_item_done.get(&key)) {
        return;
    }

    // `state.funcArgsBuf[idx] || "{}"`: an empty buffer (set by emitToolCall when
    // the call carried no arguments) falls back to an empty object, exactly as
    // JS `||` does.
    let args = match state.func_args_buf.get(&key).and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "{}".to_string(),
    };
    let name = state
        .func_names
        .get(&key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let custom = is_custom_tool(state, &name);
    let parsed_index = js_parse_int(idx);

    if custom {
        let input = extract_custom_tool_input(&Value::String(args.clone()));
        emit(
            state,
            events,
            "response.custom_tool_call_input.delta",
            json!({
                "type": "response.custom_tool_call_input.delta",
                "item_id": format!("ctc_{}", js_string(&call_id)),
                "output_index": parsed_index,
                "delta": input,
            }),
        );
        emit(
            state,
            events,
            "response.custom_tool_call_input.done",
            json!({
                "type": "response.custom_tool_call_input.done",
                "item_id": format!("ctc_{}", js_string(&call_id)),
                "output_index": parsed_index,
                "input": input,
            }),
        );
    } else {
        emit(
            state,
            events,
            "response.function_call_arguments.done",
            json!({
                "type": "response.function_call_arguments.done",
                "item_id": format!("fc_{}", js_string(&call_id)),
                "output_index": parsed_index,
                "arguments": args,
            }),
        );
    }

    let mut item = Map::new();
    item.insert(
        "id".into(),
        json!(format!(
            "{}_{}",
            if custom { "ctc" } else { "fc" },
            js_string(&call_id)
        )),
    );
    item.insert(
        "type".into(),
        json!(if custom {
            responses_item::CUSTOM_TOOL_CALL
        } else {
            responses_item::FUNCTION_CALL
        }),
    );
    if custom {
        item.insert(
            "input".into(),
            json!(extract_custom_tool_input(&Value::String(args.clone()))),
        );
    } else {
        item.insert("arguments".into(), json!(args));
    }
    item.insert("call_id".into(), call_id);
    item.insert("name".into(), json!(name));

    emit(
        state,
        events,
        "response.output_item.done",
        json!({
            "type": "response.output_item.done",
            "output_index": parsed_index,
            "item": Value::Object(item),
        }),
    );

    state.func_item_done.insert(key.clone(), json!(true));
    state.func_args_done.insert(key, json!(true));
}

fn send_completed(state: &mut ResponseState, events: &mut Vec<Value>) {
    if state.completed_sent {
        return;
    }
    state.completed_sent = true;

    let response_id = state.response_id.clone();
    let created = state.created;
    let mut response = Map::new();
    response.insert("id".into(), json!(response_id));
    response.insert("object".into(), json!("response"));
    response.insert("created_at".into(), json!(created));
    response.insert("status".into(), json!("completed"));
    response.insert("background".into(), json!(false));
    response.insert("error".into(), Value::Null);
    let usage = state
        .extra
        .get("responsesUsage")
        .cloned()
        .unwrap_or(Value::Null);
    if js_truthy(&usage) {
        response.insert("usage".into(), usage);
    }

    emit(
        state,
        events,
        "response.completed",
        json!({"type": "response.completed", "response": Value::Object(response)}),
    );
}

fn flush_events(state: &mut ResponseState) -> Vec<Value> {
    if state.completed_sent {
        return Vec::new();
    }

    let mut events: Vec<Value> = Vec::new();

    let message_keys = js_key_order(&state.msg_item_added);
    for i in message_keys {
        close_message(state, &mut events, &json!(i));
    }
    close_reasoning(state, &mut events);
    let call_keys = js_key_order(&state.func_call_ids);
    for i in call_keys {
        close_tool_call(state, &mut events, &json!(i));
    }
    send_completed(state, &mut events);

    events
}

/// `computeFinishReason(state)`: `currentToolCallId` is intentionally sticky for
/// the turn so flush/completion can still finalize as tool_calls even if the call
/// was emitted before stream end.
fn compute_finish_reason(state: &ResponseState) -> String {
    let tool_call_index = state
        .extra
        .get("toolCallIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current_tool_call_id = state
        .extra
        .get("currentToolCallId")
        .cloned()
        .unwrap_or(Value::Null);
    if tool_call_index > 0 || js_truthy(&current_tool_call_id) {
        openai_finish::TOOL_CALLS.to_string()
    } else {
        openai_finish::STOP.to_string()
    }
}

/// `chatChunk(state, delta, finishReason)`: the `chatcmpl-` envelope used by the
/// reverse direction. `id`/`created`/`model` fall back the way JS `||` does.
fn chat_chunk(state: &ResponseState, delta: Value, finish_reason: Option<&str>) -> Value {
    let id = match state
        .extra
        .get("chatId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(id) => json!(id),
        None => json!(format!("chatcmpl-{}", now_ms())),
    };
    let created = if state.created != 0 {
        state.created
    } else {
        (now_ms() / 1000) as i64
    };
    let model = state
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(MODEL_FALLBACK);
    build_chunk(&id, &json!(created), &json!(model), delta, finish_reason)
}

/// `openaiResponsesToOpenAIResponse(chunk, state)`.
pub fn openai_responses_to_openai_response(chunk: &Value, state: &mut ResponseState) -> Vec<Value> {
    if chunk.is_null() {
        // Flush: send the final chunk with finish_reason.
        if state.finish_reason_sent || !state.started {
            return Vec::new();
        }

        let finish_reason = compute_finish_reason(state);
        state.finish_reason_sent = true;
        state.finish_reason = Some(finish_reason.clone());

        let mut final_chunk = chat_chunk(state, json!({}), Some(&finish_reason));
        if let Some(usage) = state.usage.clone().filter(|u| u.is_object()) {
            final_chunk["usage"] = usage;
        }
        return vec![final_chunk];
    }

    // `chunk.type || chunk.event`, then `chunk.data || chunk`: an empty string or
    // a null falls through exactly as JS `||` does.
    let event_type = chunk
        .get("type")
        .filter(|v| js_truthy(v))
        .or_else(|| chunk.get("event").filter(|v| js_truthy(v)))
        .cloned()
        .unwrap_or(Value::Null);
    let data = chunk
        .get("data")
        .filter(|v| js_truthy(v))
        .cloned()
        .unwrap_or_else(|| chunk.clone());

    if !state.started {
        state.started = true;
        state
            .extra
            .insert("chatId".into(), json!(format!("chatcmpl-{}", now_ms())));
        state.created = (now_ms() / 1000) as i64;
        state.extra.insert("toolCallIndex".into(), json!(0));
        state.extra.insert("currentToolCallId".into(), Value::Null);
        // item_id → chat tool_calls index. Keying on the server item id (not
        // stream position) keeps parallel calls separate when upstream emits all
        // output_item.added events before any done/delta.
        state
            .extra
            .entry("respToolChatIndex".to_string())
            .or_insert_with(|| json!({}));
        // Indices that already received argument deltas (guards done-with-args).
        state
            .extra
            .entry("respToolArgsEmitted".to_string())
            .or_insert_with(|| json!({}));
    }

    if event_type == json!("response.output_text.delta") {
        let delta = data.get("delta").cloned().unwrap_or(Value::Null);
        if !js_truthy(&delta) {
            return Vec::new();
        }
        return vec![chat_chunk(state, json!({"content": delta}), None)];
    }

    if event_type == json!("response.output_text.done") {
        return Vec::new();
    }

    if event_type == json!("response.output_item.added")
        && data
            .pointer("/item/type")
            .and_then(Value::as_str)
            .is_some_and(|t| {
                t == responses_item::FUNCTION_CALL || t == responses_item::CUSTOM_TOOL_CALL
            })
    {
        let item = data.get("item").cloned().unwrap_or_else(|| json!({}));
        let call_id = item
            .get("call_id")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(fallback_tool_call_id(None, now_ms() as i64)));
        state
            .extra
            .insert("currentToolCallId".into(), call_id.clone());
        state
            .extra
            .entry("respToolChatIndex".to_string())
            .or_insert_with(|| json!({}));
        let key = item
            .get("id")
            .filter(|v| js_truthy(v))
            .or_else(|| data.get("item_id").filter(|v| js_truthy(v)))
            .cloned()
            .unwrap_or_else(|| call_id.clone());

        let idx = match map_get(&state.extra, "respToolChatIndex", &key) {
            Some(existing) => existing,
            None => {
                let idx = state
                    .extra
                    .get("toolCallIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                state.extra.insert("toolCallIndex".into(), json!(idx + 1));
                if js_truthy(&key) {
                    map_set(&mut state.extra, "respToolChatIndex", &key, json!(idx));
                }
                json!(idx)
            }
        };

        let name = js_or_empty(item.get("name"));
        return vec![chat_chunk(
            state,
            json!({"tool_calls": [{
                "index": idx,
                "id": call_id,
                "type": openai_block::FUNCTION,
                "function": {"name": name, "arguments": ""},
            }]}),
            None,
        )];
    }

    if event_type == json!("response.function_call_arguments.delta")
        || event_type == json!("response.custom_tool_call_input.delta")
    {
        let args_delta = data.get("delta").cloned().unwrap_or(Value::Null);
        if !js_truthy(&args_delta) {
            return Vec::new();
        }

        let known = data
            .get("item_id")
            .filter(|v| js_truthy(v))
            .and_then(|item_id| map_get(&state.extra, "respToolChatIndex", item_id));
        let idx = known.unwrap_or_else(|| {
            let tool_call_index = state
                .extra
                .get("toolCallIndex")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            json!(std::cmp::max(0, tool_call_index - 1))
        });
        state
            .extra
            .entry("respToolArgsEmitted".to_string())
            .or_insert_with(|| json!({}));
        set_index_member(&mut state.extra, "respToolArgsEmitted", &idx, json!(true));
        return vec![chat_chunk(
            state,
            json!({"tool_calls": [{"index": idx, "function": {"arguments": args_delta}}]}),
            None,
        )];
    }

    if event_type == json!("response.output_item.done")
        && data
            .pointer("/item/type")
            .and_then(Value::as_str)
            .is_some_and(|t| {
                t == responses_item::FUNCTION_CALL || t == responses_item::CUSTOM_TOOL_CALL
            })
    {
        let key = data
            .pointer("/item/id")
            .filter(|v| js_truthy(v))
            .or_else(|| data.get("item_id").filter(|v| js_truthy(v)))
            .cloned();
        let idx = key
            .and_then(|k| map_get(&state.extra, "respToolChatIndex", &k))
            .unwrap_or_else(|| {
                let tool_call_index = state
                    .extra
                    .get("toolCallIndex")
                    .and_then(Value::as_i64)
                    .unwrap_or(1);
                json!(std::cmp::max(0, tool_call_index - 1))
            });
        // Some upstreams send complete arguments only here (no deltas).
        let full_args = data
            .pointer("/item/arguments")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(full_args) = full_args.as_str().filter(|s| !s.is_empty()) {
            state
                .extra
                .entry("respToolArgsEmitted".to_string())
                .or_insert_with(|| json!({}));
            if !has_index_member(&state.extra, "respToolArgsEmitted", &idx) {
                set_index_member(&mut state.extra, "respToolArgsEmitted", &idx, json!(true));
                return vec![chat_chunk(
                    state,
                    json!({"tool_calls": [{"index": idx, "function": {"arguments": full_args}}]}),
                    None,
                )];
            }
        }
        return Vec::new();
    }

    if event_type == json!("response.completed") || event_type == json!("response.done") {
        if let Some(response_usage) = data.pointer("/response/usage").filter(|u| u.is_object()) {
            let input_tokens = response_usage
                .get("input_tokens")
                .filter(|v| js_truthy(v))
                .or_else(|| response_usage.get("prompt_tokens").filter(|v| js_truthy(v)));
            let output_tokens = response_usage
                .get("output_tokens")
                .filter(|v| js_truthy(v))
                .or_else(|| {
                    response_usage
                        .get("completion_tokens")
                        .filter(|v| js_truthy(v))
                });
            let input = input_tokens.and_then(Value::as_f64).unwrap_or(0.0);
            let output = output_tokens.and_then(Value::as_f64).unwrap_or(0.0);
            // input_tokens already includes cached_tokens; the cache counters
            // live in input_tokens_details.
            let cache_read = response_usage
                .pointer("/input_tokens_details/cached_tokens")
                .filter(|v| js_truthy(v))
                .or_else(|| {
                    response_usage
                        .get("cache_read_input_tokens")
                        .filter(|v| js_truthy(v))
                })
                .and_then(Value::as_f64)
                .unwrap_or(0.0);

            state.usage = Some(build_usage(UsageArgs {
                prompt_tokens: input as i64,
                completion_tokens: output as i64,
                total_tokens: (input + output) as i64,
                cached_tokens: cache_read as i64,
                ..Default::default()
            }));
        }

        if !state.finish_reason_sent {
            let finish_reason = compute_finish_reason(state);
            state.finish_reason_sent = true;
            state.finish_reason = Some(finish_reason.clone());

            let mut final_chunk = chat_chunk(state, json!({}), Some(&finish_reason));
            if let Some(usage) = state.usage.clone().filter(|u| u.is_object()) {
                final_chunk["usage"] = usage;
            }
            return vec![final_chunk];
        }
        return Vec::new();
    }

    if event_type == json!("error") || event_type == json!("response.failed") {
        // `error` and `response.failed` arrive back-to-back; only surface one.
        if state.finish_reason_sent {
            return Vec::new();
        }

        let error = data
            .get("error")
            .filter(|v| js_truthy(v))
            .or_else(|| data.pointer("/response/error").filter(|v| js_truthy(v)));
        if let Some(error) = error {
            state.extra.insert("error".into(), error.clone());
            state.finish_reason_sent = true;

            let message = match error.get("message").filter(|v| js_truthy(v)) {
                Some(m) => js_string(m),
                None => error.to_string(),
            };
            return vec![chat_chunk(
                state,
                json!({"content": format!("[Error] {message}")}),
                Some(openai_finish::STOP),
            )];
        }
        return Vec::new();
    }

    if event_type == json!("response.reasoning_summary_text.delta") {
        let delta = data.get("delta").cloned().unwrap_or(Value::Null);
        if !js_truthy(&delta) {
            return Vec::new();
        }
        return vec![chat_chunk(
            state,
            reasoning_delta(delta.as_str().unwrap_or(""), false),
            None,
        )];
    }

    Vec::new()
}

/// `state.<name>.get(key)`, with the scratch map created on first use.
fn map_get(extra: &Map<String, Value>, name: &str, key: &Value) -> Option<Value> {
    extra
        .get(name)
        .and_then(Value::as_object)
        .and_then(|m| m.get(&key_of(key)))
        .cloned()
}

fn map_set(extra: &mut Map<String, Value>, name: &str, key: &Value, value: Value) {
    if let Some(map) = extra.get_mut(name).and_then(Value::as_object_mut) {
        map.insert(key_of(key), value);
    }
}

/// A JS `Set` is keyed by value, so the numeric index has to compare across the
/// `Number`/`String` split; store the index as a canonical key.
fn set_index_member(extra: &mut Map<String, Value>, name: &str, idx: &Value, value: Value) {
    if let Some(map) = extra.get_mut(name).and_then(Value::as_object_mut) {
        map.insert(key_of(idx), value);
    }
}

fn has_index_member(extra: &Map<String, Value>, name: &str, idx: &Value) -> bool {
    extra
        .get(name)
        .and_then(Value::as_object)
        .and_then(|m| m.get(&key_of(idx)))
        .is_some()
}

/// `x || ""`: any falsy `x` (including `0` and `""`) yields `""`, but a truthy
/// non-string keeps its own JSON type, as JS `||` does.
fn js_or_empty(value: Option<&Value>) -> Value {
    match value {
        Some(v) if js_truthy(v) => v.clone(),
        _ => json!(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResponseState {
        ResponseState::default()
    }

    fn direct(state: &mut ResponseState) {
        state.target_format = Some(formats::OPENAI.to_string());
    }

    fn events_of(events: &[Value]) -> Vec<&str> {
        events.iter().filter_map(|e| e["event"].as_str()).collect()
    }

    #[test]
    fn the_first_chunk_emits_created_and_in_progress_with_sequence_numbers() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"id": "chatcmpl-1", "choices": [{"index": 0, "delta": {"content": "hi"}}]}),
            &mut s,
        );
        assert_eq!(
            events_of(&out),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
            ]
        );
        assert_eq!(out[0]["data"]["sequence_number"], json!(1));
        assert_eq!(out[0]["data"]["response"]["id"], json!("resp_chatcmpl-1"));
        assert_eq!(out[1]["data"]["sequence_number"], json!(2));
        assert_eq!(out[4]["data"]["delta"], json!("hi"));
        assert_eq!(s.msg_text_buf["0"], json!("hi"));
    }

    #[test]
    fn usage_is_captured_before_the_empty_choices_guard() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"usage": {"prompt_tokens": 10, "completion_tokens": 4}, "choices": []}),
            &mut s,
        );
        assert!(out.is_empty(), "an empty choices array emits nothing");
        assert_eq!(s.extra["responsesUsage"]["input_tokens"], json!(10));
        assert_eq!(s.extra["responsesUsage"]["total_tokens"], json!(14));
    }

    #[test]
    fn a_think_tag_opens_and_closes_reasoning() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"content": "<think>hmm"}}]}),
            &mut s,
        );
        assert_eq!(
            events_of(&out),
            [
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta",
            ]
        );
        assert_eq!(out[4]["data"]["delta"], json!("hmm"));
        assert!(s.in_thinking);

        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"content": "</think>visible"}}]}),
            &mut s,
        );
        let names = events_of(&out);
        assert_eq!(names[0], "response.reasoning_summary_text.done");
        assert!(names.contains(&"response.output_text.delta"));
        assert!(names.contains(&"response.output_item.done"));
        assert_eq!(out.last().unwrap()["data"]["delta"], json!("visible"));
        assert!(!s.in_thinking);
    }

    #[test]
    fn reasoning_content_opens_a_reasoning_item_without_think_tags() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"reasoning_content": "why"}}]}),
            &mut s,
        );
        assert_eq!(events_of(&out)[2], "response.output_item.added");
        assert_eq!(out[2]["data"]["item"]["type"], json!("reasoning"));
        assert_eq!(out[4]["data"]["delta"], json!("why"));
    }

    #[test]
    fn a_tool_call_is_announced_then_streamed_then_closed() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "f", "arguments": ""}},
            ]}}]}),
            &mut s,
        );
        assert_eq!(events_of(&out)[2], "response.output_item.added");
        assert_eq!(out[2]["data"]["item"]["id"], json!("fc_call_1"));
        assert_eq!(out[2]["data"]["item"]["type"], json!("function_call"));

        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"a\":1}"}},
            ]}}]}),
            &mut s,
        );
        assert_eq!(out[0]["data"]["item_id"], json!("fc_call_1"));
        assert_eq!(out[0]["data"]["delta"], json!("{\"a\":1}"));

        // The finish chunk closes the call; usage present, so completion is sent.
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
            &mut s,
        );
        let names = events_of(&out);
        assert_eq!(
            names,
            [
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(out[0]["data"]["arguments"], json!("{\"a\":1}"));
        assert_eq!(
            out[2]["data"]["response"]["usage"]["input_tokens"],
            json!(1)
        );
    }

    #[test]
    fn a_custom_tool_unwraps_its_chat_json_wrapper_at_close() {
        let mut s = state();
        s.custom_tool_names.insert("exec".into());
        openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "exec"}},
            ]}}]}),
            &mut s,
        );
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": "{\"input\":\"ls -la\"}"}},
            ]}}]}),
            &mut s,
        );
        assert!(out.is_empty(), "custom input is not streamed as raw JSON");

        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1}}),
            &mut s,
        );
        assert_eq!(
            events_of(&out),
            [
                "response.custom_tool_call_input.delta",
                "response.custom_tool_call_input.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(out[0]["data"]["delta"], json!("ls -la"));
        assert_eq!(out[2]["data"]["item"]["type"], json!("custom_tool_call"));
        assert_eq!(out[2]["data"]["item"]["input"], json!("ls -la"));
    }

    #[test]
    fn completion_is_deferred_on_the_direct_route_until_usage_or_flush() {
        let mut s = state();
        direct(&mut s);
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}),
            &mut s,
        );
        assert!(
            !events_of(&out).contains(&"response.completed"),
            "usage still unknown"
        );

        let flushed = openai_to_openai_responses_response(&Value::Null, &mut s);
        assert_eq!(events_of(&flushed).last(), Some(&"response.completed"));
        assert!(openai_to_openai_responses_response(&Value::Null, &mut s).is_empty());
    }

    #[test]
    fn completion_is_not_deferred_when_flush_cannot_reach_us() {
        let mut s = state();
        // target_format left unset: a pivot hop, where the terminal chunk is dropped.
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"content": "x"}, "finish_reason": "stop"}]}),
            &mut s,
        );
        assert_eq!(events_of(&out).last(), Some(&"response.completed"));
    }

    #[test]
    fn a_non_string_content_is_stringified_instead_of_throwing() {
        let mut s = state();
        let out = openai_to_openai_responses_response(
            &json!({"choices": [{"index": 0, "delta": {"content": 5}}]}),
            &mut s,
        );
        assert_eq!(out.last().unwrap()["data"]["delta"], json!("5"));
    }

    #[test]
    fn reverse_text_delta_becomes_a_chat_chunk_and_flush_finishes_the_turn() {
        let mut s = state();
        s.model = Some("gpt".into());
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.output_text.delta", "delta": "hi"}),
            &mut s,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["choices"][0]["delta"], json!({"content": "hi"}));
        assert_eq!(out[0]["model"], json!("gpt"));

        let final_chunk = openai_responses_to_openai_response(&Value::Null, &mut s);
        assert_eq!(final_chunk[0]["choices"][0]["finish_reason"], json!("stop"));
        assert!(openai_responses_to_openai_response(&Value::Null, &mut s).is_empty());
    }

    #[test]
    fn reverse_assigns_tool_indices_by_item_id_and_routes_deltas() {
        let mut s = state();
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.output_item.added", "item": {
                "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "f",
            }}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
            json!("call_1")
        );

        let out = openai_responses_to_openai_response(
            &json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{}"}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            json!(0)
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{}")
        );

        // The index was assigned at added-time; a done with args already streamed
        // emits nothing.
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.output_item.done", "item": {
                "id": "fc_1", "type": "function_call", "arguments": "{}",
            }}),
            &mut s,
        );
        assert!(out.is_empty());

        let final_chunk = openai_responses_to_openai_response(&Value::Null, &mut s);
        assert_eq!(
            final_chunk[0]["choices"][0]["finish_reason"],
            json!("tool_calls")
        );
    }

    #[test]
    fn reverse_emits_done_arguments_when_no_deltas_arrived() {
        let mut s = state();
        openai_responses_to_openai_response(
            &json!({"type": "response.output_item.added", "item": {
                "id": "fc_1", "type": "function_call", "call_id": "call_1", "name": "f",
            }}),
            &mut s,
        );
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.output_item.done", "item": {
                "id": "fc_1", "type": "function_call", "arguments": "{\"a\":1}",
            }}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn reverse_completed_event_sets_usage_and_the_finish_chunk() {
        let mut s = state();
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 10, "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 2},
            }}}),
            &mut s,
        );
        assert_eq!(out[0]["choices"][0]["finish_reason"], json!("stop"));
        assert_eq!(out[0]["usage"]["prompt_tokens"], json!(10));
        assert_eq!(out[0]["usage"]["total_tokens"], json!(15));
        assert_eq!(
            out[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
            json!(2)
        );
    }

    #[test]
    fn reverse_surfaces_an_error_as_a_stop_chunk_and_ignores_the_duplicate() {
        let mut s = state();
        let out = openai_responses_to_openai_response(
            &json!({"type": "error", "error": {"message": "model_not_found"}}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"]["content"],
            json!("[Error] model_not_found")
        );
        assert_eq!(out[0]["choices"][0]["finish_reason"], json!("stop"));

        let out = openai_responses_to_openai_response(
            &json!({"type": "response.failed", "response": {"error": {"message": "model_not_found"}}}),
            &mut s,
        );
        assert!(out.is_empty(), "the back-to-back failure event is dropped");
    }

    #[test]
    fn reverse_reasoning_delta_maps_to_reasoning_content() {
        let mut s = state();
        let out = openai_responses_to_openai_response(
            &json!({"type": "response.reasoning_summary_text.delta", "delta": "hmm"}),
            &mut s,
        );
        assert_eq!(
            out[0]["choices"][0]["delta"],
            json!({"reasoning_content": "hmm"})
        );
    }

    #[test]
    fn to_responses_usage_reads_both_spellings_and_bails_on_non_objects() {
        let usage = to_responses_usage(&json!({
            "input_tokens": 3,
            "completion_tokens": 2,
            "output_tokens_details": {"reasoning_tokens": 1},
        }));
        assert_eq!(usage["input_tokens"], json!(3));
        assert_eq!(usage["total_tokens"], json!(5));
        assert_eq!(usage["output_tokens_details"]["reasoning_tokens"], json!(1));
        assert!(
            usage.get("input_tokens_details").is_none(),
            "no cached tokens → no detail key"
        );

        assert_eq!(to_responses_usage(&json!(null)), Value::Null);
        assert_eq!(to_responses_usage(&json!("x")), Value::Null);
    }

    #[test]
    fn js_helpers_keep_js_semantics() {
        assert_eq!(js_parse_int(&json!("7abc")), json!(7));
        assert_eq!(js_parse_int(&json!(2.9)), json!(2));
        assert_eq!(js_parse_int(&json!("nope")), Value::Null);
        assert_eq!(key_of(&json!(0)), "0");
        assert_eq!(key_of(&json!("0")), "0");

        let mut map = Map::new();
        map.insert("10".into(), json!(1));
        map.insert("2".into(), json!(1));
        map.insert("b".into(), json!(1));
        map.insert("a".into(), json!(1));
        assert_eq!(
            js_key_order(&map),
            ["2", "10", "b", "a"],
            "integer keys ascend first"
        );
    }
}
