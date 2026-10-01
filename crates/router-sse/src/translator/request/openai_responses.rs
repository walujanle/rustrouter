//! OpenAI Responses → OpenAI Chat, and back.
//!
//! The forward leg flattens `input[]` + `instructions` into `messages[]`. Three
//! things there are not obvious:
//!
//! - A `pendingToolResults` buffer would be flushed in three places but never
//!   pushed into — `function_call_output` goes straight to `messages`. It is
//!   dead state, so it is not carried over.
//! - Custom-tool names are translator-only metadata. They would otherwise hang
//!   on the returned body as `_customToolNames` and be stripped by the caller;
//!   here they go to `ctx.meta.custom_tool_names` and never reach the wire.
//! - `max_output_tokens` is renamed to `max_tokens` only when `max_tokens` is
//!   absent, and `reasoning.effort` is lifted to `reasoning_effort` before the
//!   `reasoning` object is dropped.
//!
//! The reverse leg re-emits a `reasoning` item ahead of an assistant turn when
//! the chat history carried reasoning text or an `encrypted_content` blob. That
//! is what keeps `store: false` backends (Codex, Grok CLI) continuous across
//! turns — without it the encrypted continuity token is lost.
//!
//! JS `undefined` and JSON `null` are different keys on the wire: a key whose
//! value would be `undefined` is omitted, one written as `null` is kept.
//! `serde_json` cannot hold the former, so every site decides explicitly.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value, json};

use crate::session_manager::now_ms;
use crate::translator::RequestContext;
use crate::translator::concerns::primitives::{js_string_or_empty, js_truthy};
use crate::translator::formats::responses_api::{
    clamp_responses_call_id, coerce_responses_arguments, coerce_responses_output,
    normalize_responses_input,
};
use crate::translator::schema::{openai_block, responses_item, role};

/// `MAX_TOOL_NAME_LEN`: strict upstreams reject longer tool names (#444).
const MAX_TOOL_NAME_LEN: usize = 128;

/// `responsesCallIdSeq`: keeps same-millisecond fallback ids unique so
/// `function_call` ↔ `function_call_output` correlation never collides.
static RESPONSES_CALL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

/// `name.slice(0, MAX_TOOL_NAME_LEN)`. Slicing UTF-16 units would split a
/// surrogate pair; slicing at a char boundary instead is deliberate, and it is
/// invisible for the ASCII names tools use.
fn clamp_tool_name(name: &str) -> &str {
    match name.char_indices().nth(MAX_TOOL_NAME_LEN) {
        Some((idx, _)) => &name[..idx],
        None => name,
    }
}

/// A text part `{type, text}`; `text` is dropped when the source had none,
/// because writing `c.text` straight through and `JSON.stringify` omits an
/// `undefined` value.
fn text_part(type_: &str, text: Option<&Value>) -> Value {
    let mut part = Map::new();
    part.insert("type".into(), json!(type_));
    if let Some(text) = text {
        part.insert("text".into(), text.clone());
    }
    Value::Object(part)
}

/// `openaiResponsesToOpenAIRequest(model, body, stream, credentials)`.
pub fn openai_responses_to_openai_request(ctx: &mut RequestContext<'_>, body: Value) -> Value {
    // `if (!body.input) return body` — a missing, `null` or empty-string input
    // short-circuits; an empty `input[]` is truthy in JS and does not.
    if !body.get("input").is_some_and(js_truthy) {
        return body;
    }

    let mut result = body.clone();
    if let Some(obj) = result.as_object_mut() {
        obj.insert("messages".into(), json!([]));
    }

    if let Some(instructions) = body.get("instructions").filter(|v| js_truthy(v)) {
        push_message(
            &mut result,
            json!({"role": role::SYSTEM, "content": instructions}),
        );
    }

    let mut current_assistant_msg: Option<Value> = None;
    let mut pending_reasoning = String::new();
    let mut pending_reasoning_encrypted = String::new();
    let mut additional_tools: Vec<Value> = Vec::new();
    // Insertion-ordered set; `[...customToolNames]` keeps first-seen order.
    let mut custom_tool_names: Vec<String> = Vec::new();

    let Some(input_items) = normalize_responses_input(body.get("input")) else {
        return body;
    };

    for item in &input_items {
        // Droid CLI sends role-based items without a `type`; a bare `role`
        // makes it a message. A truthy non-string `type` matches no branch.
        let item_type = match item.get("type") {
            Some(t) if js_truthy(t) => t.as_str(),
            _ => item
                .get("role")
                .and_then(Value::as_str)
                .map(|_| responses_item::MESSAGE),
        };

        if item_type == Some(responses_item::MESSAGE) {
            if let Some(assistant) = current_assistant_msg.take() {
                push_message(&mut result, assistant);
            }

            // `Array.isArray(item.content)` — a non-array content is carried
            // through as-is, an absent one drops the key (JS stringify).
            let content = match item.get("content") {
                Some(Value::Array(blocks)) => Some(Value::Array(
                    blocks
                        .iter()
                        .map(|c| match c.get("type").and_then(Value::as_str) {
                            Some(responses_item::INPUT_TEXT)
                            | Some(responses_item::OUTPUT_TEXT) => {
                                text_part(openai_block::TEXT, c.get("text"))
                            }
                            Some(responses_item::INPUT_IMAGE) => {
                                // `c.image_url || c.file_id || ""`; a present but
                                // falsy value falls through to the next.
                                let url = c
                                    .get("image_url")
                                    .filter(|v| js_truthy(v))
                                    .or_else(|| c.get("file_id").filter(|v| js_truthy(v)))
                                    .cloned()
                                    .unwrap_or_else(|| json!(""));
                                let detail = c
                                    .get("detail")
                                    .filter(|v| js_truthy(v))
                                    .cloned()
                                    .unwrap_or_else(|| json!("auto"));
                                json!({
                                    "type": openai_block::IMAGE_URL,
                                    "image_url": {"url": url, "detail": detail},
                                })
                            }
                            _ => c.clone(),
                        })
                        .collect(),
                )),
                Some(other) => Some(other.clone()),
                None => None,
            };

            let mut msg = Map::new();
            if let Some(role_) = item.get("role") {
                msg.insert("role".into(), role_.clone());
            }
            if let Some(content) = content {
                msg.insert("content".into(), content);
            }

            // Buffered reasoning attaches to the assistant turn only; any other
            // role clears it (xiaomi-mimo + store=false continuity).
            if item.get("role").and_then(Value::as_str) == Some(role::ASSISTANT) {
                attach_pending_reasoning(
                    &mut msg,
                    &mut pending_reasoning,
                    &mut pending_reasoning_encrypted,
                );
            } else {
                pending_reasoning.clear();
                pending_reasoning_encrypted.clear();
            }

            push_message(&mut result, Value::Object(msg));
        } else if item_type == Some(responses_item::FUNCTION_CALL)
            || item_type == Some(responses_item::CUSTOM_TOOL_CALL)
        {
            if current_assistant_msg.is_none() {
                let mut msg = Map::new();
                msg.insert("role".into(), json!(role::ASSISTANT));
                msg.insert("content".into(), Value::Null);
                msg.insert("tool_calls".into(), json!([]));
                attach_pending_reasoning(
                    &mut msg,
                    &mut pending_reasoning,
                    &mut pending_reasoning_encrypted,
                );
                current_assistant_msg = Some(Value::Object(msg));
            }

            // Skip items with an empty/missing name — upstreams reject nameless
            // tool calls (#444).
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            if name.trim().is_empty() {
                continue;
            }

            if item_type == Some(responses_item::CUSTOM_TOOL_CALL)
                && !custom_tool_names.iter().any(|n| n == name)
            {
                custom_tool_names.push(name.to_string());
            }

            // A custom call wraps its freeform `input` in an `{input}` object;
            // a function call uses `arguments` as-is. Both single-stringify.
            let arguments = if item_type == Some(responses_item::CUSTOM_TOOL_CALL) {
                let input = match item.get("input") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) if !v.is_null() => v.to_string(),
                    // `JSON.stringify(item.input ?? "")`
                    _ => "\"\"".to_string(),
                };
                json!({"input": input}).to_string()
            } else {
                match item.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) if !v.is_null() => v.to_string(),
                    // `JSON.stringify(toolInput ?? {})`
                    _ => "{}".to_string(),
                }
            };

            let mut call = Map::new();
            if let Some(call_id) = item.get("call_id") {
                call.insert("id".into(), call_id.clone());
            }
            call.insert("type".into(), json!(openai_block::FUNCTION));
            call.insert(
                "function".into(),
                json!({"name": name, "arguments": arguments}),
            );

            if let Some(tool_calls) = current_assistant_msg
                .as_mut()
                .and_then(|a| a.get_mut("tool_calls"))
                .and_then(Value::as_array_mut)
            {
                tool_calls.push(Value::Object(call));
            }
        } else if item_type == Some(responses_item::FUNCTION_CALL_OUTPUT)
            || item_type == Some(responses_item::CUSTOM_TOOL_CALL_OUTPUT)
        {
            if let Some(assistant) = current_assistant_msg.take() {
                push_message(&mut result, assistant);
            }
            // `typeof item.output === "string" ? item.output : JSON.stringify(item.output)`
            // — an absent output stringifies to `undefined` and drops the key.
            let output = match item.get("output") {
                Some(Value::String(s)) => Some(json!(s)),
                Some(other) => Some(json!(other.to_string())),
                None => None,
            };
            let mut msg = Map::new();
            msg.insert("role".into(), json!(role::TOOL));
            if let Some(call_id) = item.get("call_id") {
                msg.insert("tool_call_id".into(), call_id.clone());
            }
            if let Some(output) = output {
                msg.insert("content".into(), output);
            }
            push_message(&mut result, Value::Object(msg));
        } else if item_type == Some(responses_item::ADDITIONAL_TOOLS) {
            if let Some(tools) = item.get("tools").and_then(Value::as_array) {
                additional_tools.extend(tools.iter().cloned());
            }
        } else if item_type == Some(responses_item::REASONING) {
            // Buffer reasoning text; attached to the next assistant
            // message/function_call. `encrypted_content` is stashed so a later
            // openai→responses hop can restore the store=false continuity blob.
            let txt = extract_reasoning_text(item);
            if !txt.is_empty() {
                pending_reasoning = if pending_reasoning.is_empty() {
                    txt
                } else {
                    format!("{pending_reasoning}\n{txt}")
                };
            }
            if let Some(encrypted) = item.get("encrypted_content").and_then(Value::as_str)
                && !encrypted.is_empty()
            {
                pending_reasoning_encrypted = encrypted.to_string();
            }
            continue;
        }
    }

    if let Some(assistant) = current_assistant_msg {
        push_message(&mut result, assistant);
    }

    // Hosted tools (e.g. `{type: "request_user_input"}`) carry no `name` and
    // cannot be declared as Chat Completions functions; they are dropped so
    // Gemini-style strict upstreams do not see a nameless declaration.
    let mut response_tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    response_tools.extend(additional_tools);

    if !response_tools.is_empty() {
        let mapped: Vec<Value> = response_tools
            .iter()
            .filter_map(|tool| {
                if tool.get("function").is_some_and(js_truthy) {
                    return Some(tool.clone());
                }
                let name = tool.get("name").and_then(Value::as_str)?;
                if name.trim().is_empty() {
                    return None;
                }
                if tool.get("type").and_then(Value::as_str) == Some("custom") {
                    if !custom_tool_names.iter().any(|n| n == name) {
                        custom_tool_names.push(name.to_string());
                    }
                    let format_hint = [
                        tool.get("format")
                            .and_then(|f| f.get("syntax"))
                            .map(js_string_or_empty),
                        tool.get("format")
                            .and_then(|f| f.get("definition"))
                            .map(js_string_or_empty),
                    ]
                    .into_iter()
                    .flatten()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                    let description = [
                        js_string_or_empty(tool.get("description").unwrap_or(&Value::Null)),
                        format_hint,
                    ]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                    return Some(json!({
                        "type": openai_block::FUNCTION,
                        "function": {
                            "name": name,
                            "description": description,
                            "parameters": {
                                "type": "object",
                                "properties": {
                                    "input": {
                                        "type": "string",
                                        "description": "Raw freeform input for this custom tool",
                                    },
                                },
                                "required": ["input"],
                                "additionalProperties": false,
                            },
                        },
                    }));
                }
                let mut function = Map::new();
                function.insert("name".into(), json!(name));
                function.insert(
                    "description".into(),
                    json!(js_string_or_empty(
                        tool.get("description").unwrap_or(&Value::Null)
                    )),
                );
                function.insert(
                    "parameters".into(),
                    normalize_tool_parameters(tool.get("parameters")),
                );
                if let Some(strict) = tool.get("strict") {
                    function.insert("strict".into(), strict.clone());
                }
                Some(json!({
                    "type": openai_block::FUNCTION,
                    "function": Value::Object(function),
                }))
            })
            .collect();
        if let Some(obj) = result.as_object_mut() {
            obj.insert("tools".into(), Value::Array(mapped));
        }
    }

    if !custom_tool_names.is_empty() {
        ctx.meta.custom_tool_names = custom_tool_names;
    }

    // Cleanup Responses-only fields.
    let Some(obj) = result.as_object_mut() else {
        return result;
    };
    if let Some(max_output) = obj.get("max_output_tokens").cloned() {
        if !obj.contains_key("max_tokens") {
            obj.insert("max_tokens".into(), max_output);
        }
        obj.shift_remove("max_output_tokens");
    }
    obj.shift_remove("input");
    obj.shift_remove("instructions");
    obj.shift_remove("include");
    obj.shift_remove("prompt_cache_key");
    obj.shift_remove("store");
    if let Some(effort) = obj
        .get("reasoning")
        .and_then(|r| r.get("effort"))
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        obj.insert("reasoning_effort".into(), json!(effort));
    }
    obj.shift_remove("reasoning");
    obj.shift_remove("client_metadata");

    result
}

/// `extractReasoningText(item)`: `summary[].text` first, then `content[].text`,
/// joined with newlines; non-empty parts only.
fn extract_reasoning_text(item: &Value) -> String {
    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
        let txt = summary
            .iter()
            .map(|s| s.get("text").map(js_string_or_empty).unwrap_or_default())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !txt.is_empty() {
            return txt;
        }
    }
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        let txt = content
            .iter()
            .map(|c| c.get("text").map(js_string_or_empty).unwrap_or_default())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !txt.is_empty() {
            return txt;
        }
    }
    String::new()
}

/// `attachPendingReasoning(msg)`: writes the buffers only when non-empty, then
/// clears them. `reasoning_content` is emitted before `encrypted_content`.
fn attach_pending_reasoning(
    msg: &mut Map<String, Value>,
    pending: &mut String,
    encrypted: &mut String,
) {
    if !pending.is_empty() {
        msg.insert("reasoning_content".into(), json!(pending.clone()));
    }
    if !encrypted.is_empty() {
        msg.insert("encrypted_content".into(), json!(encrypted.clone()));
    }
    pending.clear();
    encrypted.clear();
}

fn push_message(result: &mut Value, msg: Value) {
    if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(msg);
    }
}

/// `extractInstructionsText(content)`: a string passes through, text parts of an
/// array join with newlines, anything else is `""` rather than
/// `"[object Object]"`.
fn extract_instructions_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|c| {
                if let Some(text) = c.get("text").and_then(Value::as_str) {
                    return text.to_string();
                }
                if let Some(content) = c.get("content").and_then(Value::as_str) {
                    return content.to_string();
                }
                String::new()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `normalizeToolParameters(params)`: Codex requires an object schema to carry
/// `properties`, and a missing schema becomes one.
fn normalize_tool_parameters(params: Option<&Value>) -> Value {
    let Some(params) = params.filter(|p| js_truthy(p)) else {
        return json!({"type": "object", "properties": {}});
    };
    if params.get("type").and_then(Value::as_str) == Some("object")
        && !params.get("properties").is_some_and(js_truthy)
    {
        let mut out = params.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("properties".into(), json!({}));
        }
        return out;
    }
    params.clone()
}

/// `buildReasoningInputItem(msg)`: the continuity item, or `None` when there is
/// neither an encrypted blob nor summary text.
fn build_reasoning_input_item(msg: &Value) -> Option<Value> {
    if !msg.is_object() {
        return None;
    }

    let encrypted = ["encrypted_content", "reasoning_encrypted_content"]
        .iter()
        .find_map(|k| {
            msg.get(*k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
        .or_else(|| {
            msg.get("reasoning")
                .and_then(|r| r.get("encrypted_content"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();

    let summary_text = if let Some(s) = msg
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        s.to_string()
    } else if let Some(s) = msg
        .get("reasoning")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        s.to_string()
    } else if let Some(details) = msg.get("reasoning_details").and_then(Value::as_array) {
        details
            .iter()
            .map(|d| {
                if let Some(text) = d.get("text").and_then(Value::as_str) {
                    return text.to_string();
                }
                if let Some(content) = d.get("content").and_then(Value::as_str) {
                    return content.to_string();
                }
                String::new()
            })
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    };

    if encrypted.is_empty() && summary_text.is_empty() {
        return None;
    }

    let mut item = Map::new();
    item.insert("type".into(), json!(responses_item::REASONING));
    if !summary_text.is_empty() {
        item.insert(
            "summary".into(),
            json!([{"type": responses_item::SUMMARY_TEXT, "text": summary_text}]),
        );
    }
    // `encrypted_content` is the continuity token for store=false backends.
    if !encrypted.is_empty() {
        item.insert("encrypted_content".into(), json!(encrypted));
    }
    Some(Value::Object(item))
}

/// `openaiToOpenAIResponsesRequest(model, body, stream, credentials)`.
pub fn openai_to_openai_responses_request(ctx: &mut RequestContext<'_>, body: Value) -> Value {
    // A body already in Responses shape (Cursor CLI posting `input[]` to
    // `/chat/completions`) is passed through with the model and stream forced.
    if body.get("input").is_some_and(js_truthy) {
        let mut out = body.clone();
        if let Some(obj) = out.as_object_mut() {
            obj.insert("model".into(), json!(ctx.model));
            obj.insert("stream".into(), json!(true));
            if !obj.contains_key("max_output_tokens") {
                if let Some(v) = obj.get("max_completion_tokens").cloned() {
                    obj.insert("max_output_tokens".into(), v);
                } else if let Some(v) = obj.get("max_tokens").cloned() {
                    obj.insert("max_output_tokens".into(), v);
                }
            }
            obj.shift_remove("max_tokens");
            obj.shift_remove("max_completion_tokens");
        }
        return out;
    }

    let mut result = Map::new();
    result.insert("model".into(), json!(ctx.model));
    result.insert("input".into(), json!([]));
    result.insert("stream".into(), json!(true));
    result.insert("store".into(), json!(false));

    let mut has_system_message = false;
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for msg in &messages {
        let msg_role = msg.get("role").and_then(Value::as_str).unwrap_or_default();

        if msg_role == role::SYSTEM || msg_role == role::DEVELOPER {
            // The first instruction-bearing message becomes `instructions`;
            // the rest are dropped from `input`.
            if !has_system_message {
                result.insert(
                    "instructions".into(),
                    json!(extract_instructions_text(msg.get("content"))),
                );
                has_system_message = true;
            }
            continue;
        }

        if msg_role == role::USER || msg_role == role::ASSISTANT {
            // store=false continuity: re-emit the reasoning item before the
            // assistant turn it belonged to.
            if msg_role == role::ASSISTANT
                && let Some(reasoning_item) = build_reasoning_input_item(msg)
            {
                push_input(&mut result, reasoning_item);
            }

            let content_type = if msg_role == role::USER {
                responses_item::INPUT_TEXT
            } else {
                responses_item::OUTPUT_TEXT
            };

            let content: Vec<Value> = match msg.get("content") {
                Some(Value::String(s)) => vec![json!({"type": content_type, "text": s})],
                Some(Value::Array(parts)) => parts
                    .iter()
                    .map(|c| {
                        if c.get("type").and_then(Value::as_str) == Some(openai_block::TEXT) {
                            return text_part(content_type, c.get("text"));
                        }
                        if c.get("type").and_then(Value::as_str) == Some(openai_block::IMAGE_URL) {
                            // Chat Completions sends `image_url: {url, detail}`;
                            // Responses wants a bare url string plus `detail`.
                            let url = c.get("image_url").and_then(|u| {
                                if u.is_string() {
                                    Some(u.clone())
                                } else {
                                    u.get("url").cloned()
                                }
                            });
                            let detail = c
                                .get("image_url")
                                .and_then(|u| u.get("detail"))
                                .filter(|d| js_truthy(d))
                                .cloned()
                                .unwrap_or_else(|| json!("auto"));
                            let mut part = Map::new();
                            part.insert("type".into(), json!(responses_item::INPUT_IMAGE));
                            if let Some(url) = url {
                                part.insert("image_url".into(), url);
                            }
                            part.insert("detail".into(), detail);
                            return Value::Object(part);
                        }
                        if c.get("type").and_then(Value::as_str)
                            == Some(responses_item::INPUT_IMAGE)
                        {
                            return c.clone();
                        }
                        // Any unknown block (tool_use, tool_result, thinking)
                        // becomes text rather than being lost.
                        let text = c
                            .get("text")
                            .filter(|t| js_truthy(t))
                            .or_else(|| c.get("content").filter(|t| js_truthy(t)))
                            .cloned()
                            .unwrap_or_else(|| json!(c.to_string()));
                        let text = match text {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        json!({"type": content_type, "text": text})
                    })
                    .collect(),
                _ => Vec::new(),
            };

            // Assistant turns with only tool_calls have `content: null`; the
            // empty block is skipped and the calls are emitted below.
            if !content.is_empty() {
                push_input(
                    &mut result,
                    json!({"type": responses_item::MESSAGE, "role": msg_role, "content": content}),
                );
            }
        }

        if msg_role == role::ASSISTANT
            && let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array)
        {
            for tc in tool_calls {
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                push_input(
                    &mut result,
                    json!({
                        "type": responses_item::FUNCTION_CALL,
                        "call_id": clamp_responses_call_id(
                            tc.get("id"),
                            now_ms() as i64,
                            RESPONSES_CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
                        ),
                        "name": clamp_tool_name(name),
                        "arguments": coerce_responses_arguments(tc.get("function").and_then(|f| f.get("arguments"))),
                    }),
                );
            }
        }

        if msg_role == role::TOOL {
            push_input(
                &mut result,
                json!({
                    "type": responses_item::FUNCTION_CALL_OUTPUT,
                    "call_id": clamp_responses_call_id(
                        msg.get("tool_call_id"),
                        now_ms() as i64,
                        RESPONSES_CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
                    ),
                    "output": coerce_responses_output(msg.get("content")),
                }),
            );
        }
    }

    // No system message: emit an empty `instructions` for the executor to
    // replace.
    if !has_system_message {
        result.insert("instructions".into(), json!(""));
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                if tool.get("type").and_then(Value::as_str) == Some(openai_block::FUNCTION) {
                    let name = tool
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default();
                    if name.is_empty() {
                        return None;
                    }
                    let mut out = Map::new();
                    out.insert("type".into(), json!(openai_block::FUNCTION));
                    out.insert("name".into(), json!(clamp_tool_name(name)));
                    out.insert(
                        "description".into(),
                        json!(js_string_or_empty(
                            tool.get("function")
                                .and_then(|f| f.get("description"))
                                .unwrap_or(&Value::Null),
                        )),
                    );
                    out.insert(
                        "parameters".into(),
                        normalize_tool_parameters(
                            tool.get("function").and_then(|f| f.get("parameters")),
                        ),
                    );
                    if let Some(strict) = tool.get("function").and_then(|f| f.get("strict")) {
                        out.insert("strict".into(), strict.clone());
                    }
                    return Some(Value::Object(out));
                }
                Some(tool.clone())
            })
            .collect();
        result.insert("tools".into(), Value::Array(mapped));
    }

    // Pass through the other relevant fields. `!== undefined` means a key that
    // is present but `null` is copied, not skipped.
    if let Some(temperature) = body.get("temperature") {
        result.insert("temperature".into(), temperature.clone());
    }
    if let Some(v) = body.get("max_output_tokens") {
        result.insert("max_output_tokens".into(), v.clone());
    } else if let Some(v) = body.get("max_completion_tokens") {
        result.insert("max_output_tokens".into(), v.clone());
    } else if let Some(v) = body.get("max_tokens") {
        result.insert("max_output_tokens".into(), v.clone());
    }
    if let Some(top_p) = body.get("top_p") {
        result.insert("top_p".into(), top_p.clone());
    }
    if let Some(reasoning) = body.get("reasoning") {
        result.insert("reasoning".into(), reasoning.clone());
    }
    if let Some(effort) = body.get("reasoning_effort") {
        result.insert(
            "reasoning".into(),
            json!({"effort": effort, "summary": "auto"}),
        );
    }
    if let Some(service_tier) = body.get("service_tier") {
        result.insert("service_tier".into(), service_tier.clone());
    }
    if let Some(prompt_cache_key) = body.get("prompt_cache_key") {
        result.insert("prompt_cache_key".into(), prompt_cache_key.clone());
    }

    Value::Object(result)
}

fn push_input(result: &mut Map<String, Value>, item: Value) {
    if let Some(input) = result.get_mut("input").and_then(Value::as_array_mut) {
        input.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Credentials;
    use crate::translator::RequestMeta;

    fn run(body: Value) -> (Value, RequestMeta) {
        let credentials = Credentials::default();
        let mut meta = RequestMeta::default();
        let mut ctx = RequestContext {
            model: "m",
            stream: true,
            credentials: &credentials,
            meta: &mut meta,
        };
        let out = openai_responses_to_openai_request(&mut ctx, body);
        (out, meta)
    }

    fn run_back(body: Value) -> Value {
        let credentials = Credentials::default();
        let mut meta = RequestMeta::default();
        let mut ctx = RequestContext {
            model: "m",
            stream: true,
            credentials: &credentials,
            meta: &mut meta,
        };
        openai_to_openai_responses_request(&mut ctx, body)
    }

    #[test]
    fn instructions_become_a_system_message() {
        let (out, _) = run(json!({"input": "hi", "instructions": "be brief"}));
        assert_eq!(
            out["messages"][0],
            json!({"role": "system", "content": "be brief"})
        );
        assert_eq!(out["messages"][1]["role"], json!("user"));
        assert!(out.get("instructions").is_none(), "instructions is dropped");
    }

    #[test]
    fn a_body_without_input_is_returned_untouched() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let (out, meta) = run(body.clone());
        assert_eq!(out, body);
        assert!(meta.custom_tool_names.is_empty());
    }

    #[test]
    fn message_items_convert_text_and_image_content() {
        let (out, _) = run(json!({"input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"},
                {"type": "input_image", "image_url": "data:image/png;base64,AA", "detail": "low"},
            ]},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "ok"},
            ]},
        ]}));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(
            msgs[0]["content"][1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,AA", "detail": "low"}})
        );
        assert_eq!(msgs[1]["content"][0], json!({"type": "text", "text": "ok"}));
    }

    #[test]
    fn function_calls_group_into_one_assistant_turn_and_results_follow_it() {
        let (out, _) = run(json!({"input": [
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{\"a\":1}"},
            {"type": "function_call", "call_id": "c2", "name": "  ", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"},
        ]}));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], json!("assistant"));
        assert_eq!(msgs[0]["content"], Value::Null);
        assert_eq!(
            msgs[0]["tool_calls"].as_array().unwrap().len(),
            1,
            "nameless call skipped"
        );
        assert_eq!(
            msgs[0]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
        assert_eq!(
            msgs[1],
            json!({"role": "tool", "tool_call_id": "c1", "content": "ok"})
        );
    }

    #[test]
    fn a_custom_tool_call_wraps_its_input_and_records_the_name() {
        let (out, meta) = run(json!({"input": [
            {"type": "custom_tool_call", "call_id": "c1", "name": "apply_patch", "input": {"patch": "x"}},
            {"type": "custom_tool_call", "call_id": "c2", "name": "shell"},
        ]}));
        assert_eq!(
            meta.custom_tool_names,
            vec!["apply_patch".to_string(), "shell".to_string()]
        );
        let calls = out["messages"][0]["tool_calls"].as_array().unwrap();
        assert_eq!(
            calls[0]["function"]["arguments"],
            json!("{\"input\":\"{\\\"patch\\\":\\\"x\\\"}\"}")
        );
        // `input ?? ""` then `JSON.stringify("")` is the two-quote string.
        assert_eq!(
            calls[1]["function"]["arguments"],
            json!("{\"input\":\"\\\"\\\"\"}")
        );
        assert!(
            out.get("_customToolNames").is_none(),
            "metadata never reaches the wire"
        );
    }

    #[test]
    fn reasoning_text_buffers_onto_the_next_assistant_turn() {
        let (out, _) = run(json!({"input": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "why"}]},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
        ]}));
        assert_eq!(out["messages"][0]["reasoning_content"], json!("why"));
    }

    #[test]
    fn a_non_assistant_message_clears_buffered_reasoning() {
        let (out, _) = run(json!({"input": [
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "why"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]},
        ]}));
        assert!(out["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn custom_and_function_tools_are_both_declared() {
        let (out, meta) = run(json!({"input": [], "tools": [
            {"type": "custom", "name": "apply_patch", "description": "d", "format": {"syntax": "diff"}},
            {"type": "function", "name": "read", "description": "r", "parameters": {"type": "object"}},
            {"type": "request_user_input", "description": "hosted, no name"},
        ]}));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2, "the nameless hosted tool is dropped");
        assert_eq!(tools[0]["function"]["name"], json!("apply_patch"));
        assert_eq!(
            tools[0]["function"]["parameters"]["required"],
            json!(["input"])
        );
        assert_eq!(tools[1]["function"]["name"], json!("read"));
        assert_eq!(tools[1]["function"]["parameters"]["properties"], json!({}));
        assert!(
            tools[1]["function"].get("strict").is_none(),
            "an absent strict is not written"
        );
        assert!(meta.custom_tool_names.contains(&"apply_patch".to_string()));
    }

    #[test]
    fn additional_tools_append_to_the_declared_tools() {
        let (out, _) = run(json!({"input": [
            {"type": "additional_tools", "tools": [{"type": "function", "name": "extra"}]},
        ], "tools": [{"type": "function", "name": "base"}]}));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["name"], json!("base"));
        assert_eq!(tools[1]["function"]["name"], json!("extra"));
    }

    #[test]
    fn max_output_tokens_is_renamed_only_when_max_tokens_is_absent() {
        let (out, _) = run(json!({"input": "hi", "max_output_tokens": 50}));
        assert_eq!(out["max_tokens"], json!(50));
        assert!(out.get("max_output_tokens").is_none());

        let (out, _) = run(json!({"input": "hi", "max_output_tokens": 50, "max_tokens": 7}));
        assert_eq!(out["max_tokens"], json!(7));
        assert!(out.get("max_output_tokens").is_none());
    }

    #[test]
    fn reasoning_effort_is_lifted_before_reasoning_is_dropped() {
        let (out, _) = run(json!({"input": "hi", "reasoning": {"effort": "high"}}));
        assert_eq!(out["reasoning_effort"], json!("high"));
        assert!(out.get("reasoning").is_none());
    }

    #[test]
    fn responses_only_fields_are_cleaned_up() {
        let (out, _) = run(json!({
            "input": "hi", "include": ["x"], "prompt_cache_key": "k",
            "store": true, "client_metadata": {"a": 1},
        }));
        for key in [
            "input",
            "include",
            "prompt_cache_key",
            "store",
            "client_metadata",
        ] {
            assert!(out.get(key).is_none(), "{key} removed");
        }
    }

    #[test]
    fn a_responses_body_passes_through_with_the_model_and_stream_forced() {
        let out =
            run_back(json!({"input": [{"type": "message"}], "max_tokens": 9, "model": "old"}));
        assert_eq!(out["model"], json!("m"));
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["max_output_tokens"], json!(9));
        assert!(out.get("max_tokens").is_none());
    }

    #[test]
    fn chat_messages_become_responses_input() {
        let out = run_back(json!({
            "messages": [
                {"role": "system", "content": [{"type": "text", "text": "be brief"}]},
                {"role": "user", "content": "hi"},
                {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "f", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "c1", "content": "ok"},
            ],
        }));
        assert_eq!(out["instructions"], json!("be brief"));
        assert_eq!(out["store"], json!(false));
        let input = out["input"].as_array().unwrap();
        assert_eq!(
            input[0],
            json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]})
        );
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["name"], json!("f"));
        assert_eq!(
            input[2],
            json!({"type": "function_call_output", "call_id": "c1", "output": "ok"})
        );
    }

    #[test]
    fn an_assistant_turn_with_reasoning_re_emits_a_reasoning_item_first() {
        let out = run_back(json!({
            "messages": [
                {"role": "assistant", "content": "done", "reasoning_content": "why", "encrypted_content": "blob"},
            ],
        }));
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], json!("reasoning"));
        assert_eq!(input[0]["summary"][0]["text"], json!("why"));
        assert_eq!(input[0]["encrypted_content"], json!("blob"));
        assert_eq!(input[1]["type"], json!("message"));
    }

    #[test]
    fn no_system_message_leaves_instructions_empty() {
        let out = run_back(json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(out["instructions"], json!(""));
    }

    #[test]
    fn image_url_objects_flatten_to_a_bare_url() {
        let out = run_back(json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "http://x/y.png", "detail": "high"}},
        ]}]}));
        let part = &out["input"][0]["content"][0];
        assert_eq!(part["type"], json!("input_image"));
        assert_eq!(part["image_url"], json!("http://x/y.png"));
        assert_eq!(part["detail"], json!("high"));
    }

    #[test]
    fn unknown_content_blocks_serialize_as_text() {
        let out = run_back(json!({"messages": [{"role": "user", "content": [
            {"type": "tool_use", "id": "t1", "name": "f"},
        ]}]}));
        let part = &out["input"][0]["content"][0];
        assert_eq!(part["type"], json!("input_text"));
        assert!(part["text"].as_str().unwrap().contains("tool_use"));
    }

    #[test]
    fn function_tools_convert_and_nameless_ones_drop() {
        let out = run_back(json!({"messages": [], "tools": [
            {"type": "function", "function": {"name": "f", "description": "d"}},
            {"type": "function", "function": {"name": "  "}},
        ]}));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("f"));
        assert_eq!(tools[0]["parameters"]["properties"], json!({}));
        assert!(tools[0].get("strict").is_none());
    }

    #[test]
    fn reasoning_effort_becomes_a_reasoning_object() {
        let out = run_back(json!({"messages": [], "reasoning_effort": "low", "temperature": 0.2}));
        assert_eq!(
            out["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert_eq!(out["temperature"], json!(0.2));
    }

    #[test]
    fn a_null_pass_through_field_is_copied_not_skipped() {
        // `body.temperature !== undefined` is true for null.
        let out = run_back(json!({"messages": [], "temperature": null}));
        assert_eq!(out["temperature"], Value::Null);
    }
}
