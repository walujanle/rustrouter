//! OpenAI Responses API helpers.
//!
//! `convertResponsesApiFormat` is the one entry point: it flattens the
//! Responses shape (`input[]` + `instructions`) into chat-completions
//! `messages[]`, grouping assistant `tool_calls` and deferring their
//! `function_call_output` results until the assistant turn is flushed.
//!
//! Two details are easy to get wrong. An empty `input[]` gets a placeholder
//! user message rather than an empty `messages[]`, because every upstream
//! rejects the latter. And `{ role, content: undefined }` is not `{ role,
//! content: null }` in JS — `JSON.stringify` drops the key — so a message with
//! no content must omit `content` entirely, not write `null`.

use serde_json::{Map, Value, json};

use crate::translator::concerns::primitives::js_string;
use crate::translator::schema::{openai_block, responses_item, role};

/// `normalizeResponsesInput(input)`: a string becomes one user message, an
/// empty array becomes a placeholder user message, `None` for anything else.
pub fn normalize_responses_input(input: Option<&Value>) -> Option<Vec<Value>> {
    match input {
        Some(Value::String(s)) => {
            let text = if s.trim().is_empty() {
                "..."
            } else {
                s.as_str()
            };
            Some(vec![json!({
                "type": responses_item::MESSAGE,
                "role": role::USER,
                "content": [{"type": responses_item::INPUT_TEXT, "text": text}],
            })])
        }
        Some(Value::Array(items)) => {
            if items.is_empty() {
                // An empty `input[]` would produce `messages: []`, which all
                // providers reject.
                Some(vec![json!({
                    "type": responses_item::MESSAGE,
                    "role": role::USER,
                    "content": [{"type": responses_item::INPUT_TEXT, "text": "..."}],
                })])
            } else {
                Some(items.clone())
            }
        }
        _ => None,
    }
}

/// Strict Responses upstreams reject overlong call ids with
/// `InputValidationError`.
pub const MAX_RESPONSES_CALL_ID_LEN: usize = 64;

/// `clampResponsesCallId(id)`. A per-process sequence keeps same-millisecond
/// fallback ids unique so `function_call` ↔ `function_call_output` correlation
/// never collides; `now_ms` is passed in so this stays testable without a clock.
pub fn clamp_responses_call_id(id: Option<&Value>, now_ms: i64, seq: u64) -> String {
    match id.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => {
            if s.len() > MAX_RESPONSES_CALL_ID_LEN {
                s[..MAX_RESPONSES_CALL_ID_LEN].to_string()
            } else {
                s.to_string()
            }
        }
        _ => format!("call_{now_ms}_{seq}"),
    }
}

/// `coerceResponsesArguments(value)`: objects stringify once, valid JSON
/// strings pass through, anything else falls back to `"{}"` instead of
/// double-encoding.
pub fn coerce_responses_arguments(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "{}".to_string();
    };
    match value {
        Value::Null => "{}".to_string(),
        Value::String(s) => {
            if s.is_empty() {
                "{}".to_string()
            } else if serde_json::from_str::<Value>(s).is_ok() {
                s.clone()
            } else {
                "{}".to_string()
            }
        }
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".to_string()),
    }
}

/// `coerceResponsesOutput(value)`: `function_call_output.output` must be a
/// string, never null or an object.
pub fn coerce_responses_output(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(items) => items
            .iter()
            .map(|c| {
                c.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| serde_json::to_string(c).ok())
                    .unwrap_or_else(|| js_string(c))
            })
            .collect(),
        other => serde_json::to_string(other).unwrap_or_else(|_| js_string(other)),
    }
}

/// `convertResponsesApiFormat(body)`: in place, when `input` is present.
///
/// The caller's body is untouched on the no-`input` path.
pub fn convert_responses_api_format(body: &mut Value) {
    if !body.get("input").is_some_and(|v| !v.is_null()) {
        return;
    }

    let instructions = body.get("instructions").cloned();
    let input = body.get("input").cloned();
    let Some(input_items) = normalize_responses_input(input.as_ref()) else {
        return;
    };

    let mut messages: Vec<Value> = Vec::new();
    if let Some(Value::String(text)) = instructions
        && !text.is_empty()
    {
        messages.push(json!({"role": role::SYSTEM, "content": text}));
    }

    let mut current_assistant: Option<Value> = None;
    let mut pending_tool_results: Vec<Value> = Vec::new();

    for item in &input_items {
        // Droid CLI sends role-based items without a `type`; a bare `role`
        // makes it a message.
        let item_type = item
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                item.get("role")
                    .and_then(Value::as_str)
                    .map(|_| responses_item::MESSAGE.to_string())
            });

        match item_type.as_deref() {
            Some(responses_item::MESSAGE) => {
                if let Some(assistant) = current_assistant.take() {
                    messages.push(assistant);
                }
                if !pending_tool_results.is_empty() {
                    messages.append(&mut pending_tool_results);
                }

                let content = match item.get("content") {
                    Some(Value::Array(blocks)) => Value::Array(
                        blocks
                            .iter()
                            .map(|c| match c.get("type").and_then(Value::as_str) {
                                Some(responses_item::INPUT_TEXT)
                                | Some(responses_item::OUTPUT_TEXT) => {
                                    json!({"type": openai_block::TEXT, "text": c.get("text")})
                                }
                                Some(responses_item::INPUT_IMAGE) => {
                                    let url = c
                                        .get("image_url")
                                        .or_else(|| c.get("file_id"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    let detail =
                                        c.get("detail").and_then(Value::as_str).unwrap_or("auto");
                                    json!({
                                        "type": openai_block::IMAGE_URL,
                                        "image_url": {"url": url, "detail": detail},
                                    })
                                }
                                _ => c.clone(),
                            })
                            .collect(),
                    ),
                    Some(other) => other.clone(),
                    // JS `content: undefined` drops the key on stringify.
                    None => continue,
                };

                let mut msg = Map::new();
                if let Some(role_) = item.get("role") {
                    msg.insert("role".into(), role_.clone());
                }
                msg.insert("content".into(), content);
                messages.push(Value::Object(msg));
            }
            Some(responses_item::FUNCTION_CALL) => {
                if current_assistant.is_none() {
                    current_assistant = Some(json!({
                        "role": role::ASSISTANT,
                        "content": null,
                        "tool_calls": [],
                    }));
                }
                // Upstream APIs reject nameless tool calls.
                let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.trim().is_empty() {
                    continue;
                }
                let call = json!({
                    "id": item.get("call_id"),
                    "type": openai_block::FUNCTION,
                    "function": {
                        "name": name,
                        "arguments": item.get("arguments"),
                    },
                });
                if let Some(tool_calls) = current_assistant
                    .as_mut()
                    .and_then(|a| a.get_mut("tool_calls"))
                    .and_then(Value::as_array_mut)
                {
                    tool_calls.push(call);
                }
            }
            Some(responses_item::FUNCTION_CALL_OUTPUT) => {
                if let Some(assistant) = current_assistant.take() {
                    messages.push(assistant);
                }
                pending_tool_results.push(json!({
                    "role": role::TOOL,
                    "tool_call_id": item.get("call_id"),
                    "content": coerce_responses_output(item.get("output")),
                }));
            }
            Some(responses_item::REASONING) => {
                // Display-only; dropped.
            }
            _ => {}
        }
    }

    if let Some(assistant) = current_assistant.take() {
        messages.push(assistant);
    }
    messages.append(&mut pending_tool_results);

    // Responses-only fields are not part of the chat body.
    if let Some(obj) = body.as_object_mut() {
        for key in [
            "input",
            "instructions",
            "include",
            "prompt_cache_key",
            "store",
            "reasoning",
        ] {
            obj.shift_remove(key);
        }
    }
    body["messages"] = Value::Array(messages);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_normalization_covers_string_empty_and_invalid() {
        assert!(normalize_responses_input(None).is_none());
        assert!(normalize_responses_input(Some(&json!(42))).is_none());

        let from_string = normalize_responses_input(Some(&json!("hi"))).unwrap();
        assert_eq!(from_string[0]["content"][0]["text"], json!("hi"));

        let blank = normalize_responses_input(Some(&json!("   "))).unwrap();
        assert_eq!(blank[0]["content"][0]["text"], json!("..."));

        let empty = normalize_responses_input(Some(&json!([]))).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0]["content"][0]["text"], json!("..."));
    }

    #[test]
    fn call_ids_clamp_and_fall_back_to_a_sequence() {
        assert_eq!(
            clamp_responses_call_id(Some(&json!("call_abc")), 1, 1),
            "call_abc"
        );
        let long = "x".repeat(80);
        assert_eq!(
            clamp_responses_call_id(Some(&json!(long)), 1, 1).len(),
            MAX_RESPONSES_CALL_ID_LEN
        );
        // Missing, empty and non-string all take the fallback.
        assert_eq!(clamp_responses_call_id(None, 7, 3), "call_7_3");
        assert_eq!(clamp_responses_call_id(Some(&json!("")), 7, 4), "call_7_4");
        assert_eq!(clamp_responses_call_id(Some(&json!(9)), 7, 5), "call_7_5");
    }

    #[test]
    fn arguments_coercion_never_double_encodes() {
        assert_eq!(coerce_responses_arguments(None), "{}");
        assert_eq!(coerce_responses_arguments(Some(&json!(null))), "{}");
        assert_eq!(coerce_responses_arguments(Some(&json!(""))), "{}");
        // A valid JSON string passes through untouched.
        assert_eq!(
            coerce_responses_arguments(Some(&json!(r#"{"a":1}"#))),
            r#"{"a":1}"#
        );
        // An object stringifies once.
        assert_eq!(
            coerce_responses_arguments(Some(&json!({"a": 1}))),
            r#"{"a":1}"#
        );
        // A fragment falls back rather than double-encoding.
        assert_eq!(coerce_responses_arguments(Some(&json!("{\"a\":"))), "{}");
    }

    #[test]
    fn output_coercion_flattens_arrays_of_text_parts() {
        assert_eq!(coerce_responses_output(Some(&json!("plain"))), "plain");
        assert_eq!(coerce_responses_output(Some(&json!(null))), "");
        assert_eq!(coerce_responses_output(None), "");
        assert_eq!(
            coerce_responses_output(Some(&json!([{"text": "a"}, {"text": "b"}]))),
            "ab"
        );
        assert_eq!(
            coerce_responses_output(Some(&json!({"a": 1}))),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn conversion_flattens_instructions_messages_and_tool_calls() {
        let mut body = json!({
            "model": "m",
            "instructions": "be brief",
            "store": true,
            "reasoning": {"effort": "high"},
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hi"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AA", "detail": "low"},
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{\"a\":1}"},
                {"type": "function_call", "call_id": "call_2", "name": "  ", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "reasoning", "summary": []},
            ],
        });
        convert_responses_api_format(&mut body);

        // Responses-only keys are gone.
        for key in ["input", "instructions", "store", "reasoning"] {
            assert!(body.get(key).is_none(), "{key} removed");
        }

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            4,
            "system, user, assistant-with-tool-call, tool"
        );
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "be brief"})
        );

        let user = &messages[1];
        assert_eq!(user["role"], json!("user"));
        assert_eq!(user["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(
            user["content"][1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AA", "detail": "low"}})
        );

        // The nameless function_call is skipped; the assistant turn keeps one.
        let assistant = &messages[2];
        assert_eq!(assistant["role"], json!("assistant"));
        assert_eq!(assistant["content"], Value::Null);
        assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
        assert_eq!(assistant["tool_calls"][0]["id"], json!("call_1"));

        // The deferred result follows the assistant turn.
        assert_eq!(messages[3]["role"], json!("tool"));
        assert_eq!(messages[3]["tool_call_id"], json!("call_1"));
        assert_eq!(messages[3]["content"], json!("ok"));
    }

    #[test]
    fn conversion_is_a_noop_without_input() {
        let mut body = json!({"messages": [{"role": "user", "content": "x"}]});
        let before = body.clone();
        convert_responses_api_format(&mut body);
        assert_eq!(body, before);
    }

    #[test]
    fn role_based_items_without_a_type_are_treated_as_messages() {
        let mut body = json!({"input": [{"role": "user", "content": "droid style"}]});
        convert_responses_api_format(&mut body);
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "droid style"}])
        );
    }
}
