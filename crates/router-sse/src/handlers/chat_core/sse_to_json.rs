//! The provider forced streaming but the client asked for a single JSON body.
//!
//! Two paths share this entry point and they are not interchangeable:
//!
//! * A **Responses-API provider** (Codex) emits Responses SSE. It is decoded
//!   into a `response` object and then re-shaped into whatever the client
//!   speaks, because a Responses client and a chat client want different
//!   bodies out of the same upstream stream.
//! * Every other provider emits Chat Completions SSE, which
//!   [`parse_sse_to_openai_response`] flattens into one `chat.completion`.
//!
//! The branch keys on the *upstream* format, not the client's: a Responses
//! client routed to a chat-native forced-streaming provider still receives
//! chat chunks and must take the standard path.
//!
//! DB-free by construction. Saving usage, appending the request log and
//! tracking the pending counter all produce payloads that ride back on
//! [`ChatResult`] for the server layer to persist, the same way
//! `services/token_refresh.rs` returns a patch instead of writing.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use crate::executors::executor::UpstreamResponse;
use crate::handlers::chat_core::non_streaming::open_ai_completion_to_claude_message;
use crate::handlers::chat_core::request_detail::{format_done_line, save_usage_stats};
use crate::handlers::chat_core::{ChatContext, ChatResult};
use crate::runtime_config::http_status;
use crate::translator::concerns::primitives::{js_number, js_string, js_truthy};
use crate::translator::formats;
use crate::translator::schema::{responses_item, role};
use crate::utils::fingerprint::restore_tool_names;

/// `PROVIDERS[p]?.format === FORMATS.OPENAI_RESPONSES`.
pub fn is_responses_provider(provider: &str) -> bool {
    crate::providers::registry::registry()
        .transport(provider)
        .and_then(|t| t.format.as_deref())
        == Some(formats::OPENAI_RESPONSES)
}

fn log_done(ctx: &ChatContext<'_>, usage: &Value, latency_ms: i64) {
    tracing::info!(
        target: "router_sse::chat_core",
        "{} {}",
        ctx.req_tag,
        format_done_line(Some(usage), &json!({"total": latency_ms, "ttft": latency_ms}))
    );
}

// ─── Responses output helpers ──────────────────────────────────────────────

/// `textFromResponsesMessageItem(item)`.
pub fn text_from_responses_message_item(item: &Value) -> String {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    if let Some(text) = content
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some(responses_item::OUTPUT_TEXT))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    if let Some(text) = content
        .iter()
        .find(|c| c.get("text").and_then(Value::as_str).is_some())
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_string();
    }
    String::new()
}

/// `pickAssistantMessageForChatCompletion(output)`.
///
/// The last non-empty message wins; when every message is empty the last one
/// is returned with its (empty) text.
pub fn pick_assistant_message_for_chat_completion(
    output: &Value,
) -> (Option<Value>, Option<String>) {
    let Some(items) = output.as_array() else {
        return (None, None);
    };
    let messages: Vec<&Value> = items
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some(responses_item::MESSAGE))
        .collect();
    if messages.is_empty() {
        return (None, None);
    }
    for message in messages.iter().rev() {
        let text = text_from_responses_message_item(message);
        if !text.is_empty() {
            return (Some((*message).clone()), Some(text));
        }
    }
    let last = messages[messages.len() - 1];
    (
        Some(last.clone()),
        Some(text_from_responses_message_item(last)),
    )
}

/// `chatCompletionToResponses(responseBody, customToolNames)`.
///
/// Always reports `status: "completed"`, unlike the non-streaming copy.
pub fn chat_completion_to_responses(
    response_body: &Value,
    custom_tool_names: Option<&HashSet<String>>,
) -> Value {
    super::responses_convert::completion_to_responses(
        response_body,
        custom_tool_names,
        Some("completed"),
    )
}

// ─── SSE → chat completion ─────────────────────────────────────────────────

/// `parseSSEToOpenAIResponse(rawSSE, fallbackModel)`.
///
/// `Some({"error": …})` when a chunk carried an `error`; `None` when no chunk
/// survived, which the caller maps to a 502.
pub fn parse_sse_to_openai_response(raw_sse: &str, fallback_model: Option<&str>) -> Option<Value> {
    let mut chunks: Vec<Value> = Vec::new();
    let mut stream_error: Option<Value> = None;

    for line in raw_sse.split('\n') {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(chunk) = serde_json::from_str::<Value>(payload) {
            if let Some(error) = chunk.get("error").filter(|e| js_truthy(e)) {
                stream_error = Some(error.clone());
            } else {
                chunks.push(chunk);
            }
        }
    }

    if let Some(error) = stream_error {
        return Some(json!({ "error": error }));
    }
    if chunks.is_empty() {
        return None;
    }

    let first = &chunks[0];
    let mut content_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    // Keyed by index, which is also the emission order: the map's entries are
    // sorted numerically before emitting.
    let mut tool_call_map: BTreeMap<i64, Value> = BTreeMap::new();
    let mut finish_reason = "stop".to_string();
    let mut usage: Option<Value> = None;

    for chunk in &chunks {
        let choice = chunk.get("choices").and_then(|c| c.get(0));
        let delta = choice
            .and_then(|c| c.get("delta"))
            .cloned()
            .unwrap_or(json!({}));

        if let Some(content) = delta.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            content_parts.push(content.to_string());
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            reasoning_parts.push(reasoning.to_string());
        }
        if let Some(reason) = choice
            .and_then(|c| c.get("finish_reason"))
            .filter(|v| js_truthy(v))
        {
            finish_reason = js_string(reason);
        }
        if let Some(chunk_usage) = chunk.get("usage").filter(|u| u.is_object() || u.is_array()) {
            usage = Some(chunk_usage.clone());
        }

        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                let index = tool_call.get("index").and_then(Value::as_i64).unwrap_or(0);
                let existing = tool_call_map.entry(index).or_insert_with(|| {
                    json!({
                        "id": "",
                        "type": "function",
                        "function": { "name": "", "arguments": "" },
                    })
                });
                if let Some(id) = tool_call.get("id").filter(|v| js_truthy(v)) {
                    existing["id"] = id.clone();
                }
                let function = tool_call.get("function");
                if let Some(name) = function
                    .and_then(|f| f.get("name"))
                    .filter(|v| js_truthy(v))
                {
                    let previous = existing["function"]["name"].as_str().unwrap_or("");
                    existing["function"]["name"] = json!(format!("{previous}{}", js_string(name)));
                }
                if let Some(arguments) = function
                    .and_then(|f| f.get("arguments"))
                    .filter(|v| js_truthy(v))
                {
                    let previous = existing["function"]["arguments"].as_str().unwrap_or("");
                    existing["function"]["arguments"] =
                        json!(format!("{previous}{}", js_string(arguments)));
                }
            }
        }
    }

    let joined_content = content_parts.join("");
    let content = if !joined_content.is_empty() {
        json!(joined_content)
    } else if !tool_call_map.is_empty() {
        Value::Null
    } else {
        json!("")
    };

    let mut message = Map::new();
    message.insert("role".into(), json!(role::ASSISTANT));
    message.insert("content".into(), content);
    if !reasoning_parts.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning_parts.join("")));
    }
    if !tool_call_map.is_empty() {
        message.insert(
            "tool_calls".into(),
            Value::Array(tool_call_map.into_values().collect()),
        );
    }

    let mut choice = Map::new();
    choice.insert("index".into(), json!(0));
    choice.insert("message".into(), Value::Object(message));
    choice.insert("finish_reason".into(), json!(finish_reason));

    let mut result = Map::new();
    result.insert(
        "id".into(),
        first
            .get("id")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or(json!(format!("chatcmpl-{}", router_db::time::now_ms()))),
    );
    result.insert("object".into(), json!("chat.completion"));
    result.insert(
        "created".into(),
        first
            .get("created")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or(json!(router_db::time::now_ms() / 1000)),
    );
    result.insert(
        "model".into(),
        json!(
            first
                .get("model")
                .filter(|v| js_truthy(v))
                .map(js_string)
                .or_else(|| fallback_model.filter(|m| !m.is_empty()).map(str::to_string))
                .unwrap_or_else(|| "unknown".to_string())
        ),
    );
    result.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
    if let Some(usage) = usage {
        result.insert("usage".into(), usage);
    }
    Some(Value::Object(result))
}

// ─── the handler ───────────────────────────────────────────────────────────

/// `usage.input_tokens || 0`, `cache_read || cached_tokens || 0`, and friends:
/// `||` falls through on zero, so the first *truthy* key wins.
fn usage_first(usage: &Value, keys: &[&str]) -> i64 {
    for key in keys {
        if let Some(value) = usage.get(*key).filter(|v| js_truthy(v)) {
            return js_number(Some(value));
        }
    }
    0
}

fn header_string(headers: &reqwest::header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// `handleForcedSSEToJson({…})`.
///
/// `Err(provider_response)` when the upstream is not an SSE stream, which is
/// not this handler's job — the caller falls through to the streaming path.
/// The response is handed back because both success branches read its body to
/// the end, so ownership has to move into them.
pub async fn handle_forced_sse_to_json(
    provider_response: UpstreamResponse,
    ctx: &ChatContext<'_>,
) -> Result<ChatResult, UpstreamResponse> {
    let content_type = header_string(&provider_response.headers, "content-type");
    let is_sse = content_type.contains("text/event-stream")
        || (content_type.is_empty() && is_responses_provider(ctx.provider));
    if !is_sse {
        return Err(provider_response);
    }

    let is_codex_responses_api =
        is_responses_provider(ctx.provider) || ctx.target_format == formats::OPENAI_RESPONSES;
    if is_codex_responses_api {
        return Ok(handle_codex_responses_sse(provider_response, ctx).await);
    }

    Ok(handle_standard_sse(provider_response, ctx).await)
}

/// The Codex / Responses-API SSE branch.
///
/// Expects `crate::transformer::stream_to_json::convert_responses_stream_to_json`
/// to take the upstream byte stream and yield the Responses `response` object.
/// That module is another cluster's; this is the call shape it needs.
async fn handle_codex_responses_sse(
    provider_response: UpstreamResponse,
    ctx: &ChatContext<'_>,
) -> ChatResult {
    let stream = provider_response.into_byte_stream();
    let json_response =
        crate::transformer::stream_to_json::convert_responses_stream_to_json(stream).await;

    let latency = router_db::time::now_ms() - ctx.request_start_time_ms;
    let usage = json_response.get("usage").cloned().unwrap_or(json!({}));
    log_done(ctx, &usage, latency);

    let usage_stats = save_usage_stats(
        ctx.provider,
        ctx.model,
        Some(&usage),
        ctx.connection_id,
        ctx.api_key,
        ctx.client_endpoint,
        "USAGE",
        true,
    );

    let cache_read = usage_first(&usage, &["cache_read_input_tokens", "cached_tokens"]);
    let cache_create = usage_first(&usage, &["cache_creation_input_tokens"]);
    let (_, text_content) = pick_assistant_message_for_chat_completion(
        json_response.get("output").unwrap_or(&Value::Null),
    );

    // A Responses client gets the decoded body as-is.
    if ctx.source_format == formats::OPENAI_RESPONSES {
        let mut body = json_response;
        restore_tool_names(&mut body, ctx.tool_name_map);
        let mut result = ChatResult::json(200, &body);
        result.usage_stats = usage_stats;
        return result;
    }

    let cache_details = if cache_read > 0 || cache_create > 0 {
        let mut details = Map::new();
        if cache_read > 0 {
            details.insert("cached_tokens".into(), json!(cache_read));
        }
        if cache_create > 0 {
            details.insert("cache_creation_tokens".into(), json!(cache_create));
        }
        Some(json!({ "prompt_tokens_details": details }))
    } else {
        None
    };
    let in_tokens = usage_first(&usage, &["input_tokens"]) + cache_read + cache_create;
    let out_tokens = usage_first(&usage, &["output_tokens"]);

    let func_call_items: Vec<&Value> = json_response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|i| {
                    i.get("type").and_then(Value::as_str) == Some(responses_item::FUNCTION_CALL)
                })
                .collect()
        })
        .unwrap_or_default();
    let now = router_db::time::now_ms();
    let tool_calls: Vec<Value> = func_call_items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let call_id = item
                .get("call_id")
                .filter(|v| js_truthy(v))
                .map(js_string)
                .unwrap_or_else(|| {
                    format!(
                        "call_{}_{now}_{index}",
                        item.get("name").map(js_string).unwrap_or_default()
                    )
                });
            let arguments = match item.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
                None => "{}".to_string(),
            };
            json!({
                "id": call_id,
                "type": "function",
                "function": {
                    "name": item.get("name").map(js_string).unwrap_or_default(),
                    "arguments": arguments,
                },
            })
        })
        .collect();
    let has_tool_calls = !tool_calls.is_empty();

    let mut final_resp = Map::new();
    if matches!(ctx.source_format, formats::GEMINI | formats::GEMINI_CLI) {
        final_resp.insert(
            "response".into(),
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": text_content.clone().unwrap_or_default() }],
                    },
                    "finishReason": "STOP",
                    "index": 0,
                }],
                "usageMetadata": {
                    "promptTokenCount": in_tokens,
                    "candidatesTokenCount": out_tokens,
                    "totalTokenCount": in_tokens + out_tokens,
                },
                "modelVersion": ctx.model,
                "responseId": json_response
                    .get("id")
                    .filter(|v| js_truthy(v))
                    .cloned()
                    .unwrap_or(json!(format!("resp_{now}"))),
            }),
        );
    } else {
        let mut message = Map::new();
        message.insert("role".into(), json!(role::ASSISTANT));
        message.insert(
            "content".into(),
            text_content
                .clone()
                .filter(|t| !t.is_empty())
                .map(|t| json!(t))
                .unwrap_or_else(|| {
                    if has_tool_calls {
                        Value::Null
                    } else {
                        json!("")
                    }
                }),
        );
        if has_tool_calls {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }

        let status = json_response.get("status").and_then(Value::as_str);
        let response_done = status == Some("completed") || status == Some("done");
        let finish_reason = if has_tool_calls {
            "tool_calls".to_string()
        } else if response_done {
            "stop".to_string()
        } else {
            status.unwrap_or("stop").to_string()
        };

        let mut usage_obj = Map::new();
        usage_obj.insert("prompt_tokens".into(), json!(in_tokens));
        usage_obj.insert("completion_tokens".into(), json!(out_tokens));
        usage_obj.insert("total_tokens".into(), json!(in_tokens + out_tokens));
        if let Some(cache_details) = cache_details
            && let Some(details) = cache_details.get("prompt_tokens_details")
        {
            usage_obj.insert("prompt_tokens_details".into(), details.clone());
        }

        final_resp.insert(
            "id".into(),
            json_response
                .get("id")
                .filter(|v| js_truthy(v))
                .cloned()
                .unwrap_or(json!(format!("chatcmpl-{now}"))),
        );
        final_resp.insert("object".into(), json!("chat.completion"));
        final_resp.insert(
            "created".into(),
            json_response
                .get("created_at")
                .filter(|v| js_truthy(v))
                .map(|v| json!(js_number(Some(v))))
                .unwrap_or(json!(now / 1000)),
        );
        final_resp.insert(
            "model".into(),
            json_response
                .get("model")
                .filter(|v| js_truthy(v))
                .map(|v| Value::String(js_string(v)))
                .unwrap_or_else(|| Value::String(ctx.model.to_string())),
        );
        final_resp.insert(
            "choices".into(),
            Value::Array(vec![json!({
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish_reason,
            })]),
        );
        final_resp.insert("usage".into(), Value::Object(usage_obj));
    }

    let mut body = Value::Object(final_resp);
    if ctx.source_format == formats::CLAUDE {
        // Same forced-stream-to-JSON shape mismatch as the standard branch: a
        // Claude client must get an Anthropic `Message`, not `chat.completion`.
        body = open_ai_completion_to_claude_message(&body);
    }
    restore_tool_names(&mut body, ctx.tool_name_map);
    let mut result = ChatResult::json(200, &body);
    result.usage_stats = usage_stats;
    result
}

/// The standard Chat Completions SSE branch.
async fn handle_standard_sse(
    provider_response: UpstreamResponse,
    ctx: &ChatContext<'_>,
) -> ChatResult {
    let sse_text = match provider_response.text().await {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(
                target: "router_sse::chat_core",
                "[ChatCore] Chat Completions SSE→JSON failed: {error}"
            );
            return ChatResult::error(
                http_status::BAD_GATEWAY,
                "Failed to convert streaming response to JSON",
            );
        }
    };

    let Some(mut parsed) = parse_sse_to_openai_response(&sse_text, Some(ctx.model)) else {
        return ChatResult::error(
            http_status::BAD_GATEWAY,
            "Invalid SSE response for non-streaming request",
        );
    };

    if let Some(error) = parsed.get("error") {
        // A structured error chunk may carry the real upstream status (the
        // Qoder executor emits 403 for billing envelopes). Preserve a 4xx/5xx
        // so the account loop locks on the right status instead of a 502.
        let upstream_status = error.get("status").and_then(Value::as_u64);
        let status = upstream_status
            .filter(|s| (400..=599).contains(s))
            .map(|s| s as u16)
            .unwrap_or(http_status::BAD_GATEWAY);
        let message = error
            .get("message")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "Upstream SSE stream failed".to_string());
        return ChatResult::error(status, &message);
    }

    let latency = router_db::time::now_ms() - ctx.request_start_time_ms;
    let usage = parsed.get("usage").cloned().unwrap_or(json!({}));
    log_done(ctx, &usage, latency);

    let usage_stats = save_usage_stats(
        ctx.provider,
        ctx.model,
        Some(&usage),
        ctx.connection_id,
        ctx.api_key,
        ctx.client_endpoint,
        "USAGE",
        true,
    );

    // Re-attach the usage this handler already holds. Whatever drops it
    // between assembly and serialisation, a client that cannot see its own
    // token spend cannot tell a 90%-cached request from a cheap one.
    if usage.is_object() && usage.as_object().is_some_and(|u| !u.is_empty()) {
        parsed["usage"] = usage;
    }

    // Strip reasoning_content only when content is non-empty: a thinking model
    // that spent the whole budget on reasoning has nothing else to show.
    if let Some(choices) = parsed.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices.iter_mut() {
            let Some(message) = choice.get_mut("message").and_then(Value::as_object_mut) else {
                continue;
            };
            let has_content = message.get("content").is_some_and(js_truthy);
            if has_content && message.contains_key("reasoning_content") {
                message.shift_remove("reasoning_content");
            }
        }
    }

    let mut final_body = shape_forced_sse_body(parsed, ctx.source_format, ctx.custom_tool_names);
    restore_tool_names(&mut final_body, ctx.tool_name_map);

    let mut result = ChatResult::json(200, &final_body);
    result.usage_stats = usage_stats;
    result
}

/// Re-shape a parsed Chat Completions body into the format the client asked
/// for. `parse_sse_to_openai_response` always yields a `chat.completion`, so a
/// Claude client would otherwise receive `{"choices":…}` where it expects an
/// Anthropic `Message` — the "JSON but not a Message" failure a `/v1/messages`
/// request hits when a provider is forced to stream.
fn shape_forced_sse_body(
    parsed: Value,
    source_format: &str,
    custom_tool_names: Option<&HashSet<String>>,
) -> Value {
    if source_format == formats::OPENAI_RESPONSES {
        chat_completion_to_responses(&parsed, custom_tool_names)
    } else if source_format == formats::CLAUDE {
        open_ai_completion_to_claude_message(&parsed)
    } else {
        parsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accumulates_content_reasoning_and_tool_call_deltas() {
        let sse = concat!(
            "data: {\"id\":\"c1\",\"created\":1,\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"ge\",\"arguments\":\"{\\\"x\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"t\",\"arguments\":\"1}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n",
        );

        let parsed = parse_sse_to_openai_response(sse, Some("fallback")).unwrap();
        assert_eq!(parsed["id"], json!("c1"));
        assert_eq!(parsed["created"], json!(1));
        assert_eq!(parsed["model"], json!("m"));
        let message = &parsed["choices"][0]["message"];
        assert_eq!(message["role"], json!("assistant"));
        assert_eq!(message["content"], json!("Hello"));
        assert_eq!(message["reasoning_content"], json!("think"));
        let tool_calls = message["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 2, "indexed by tool_call index");
        assert_eq!(tool_calls[0]["id"], json!("call_a"));
        assert_eq!(tool_calls[0]["function"]["name"], json!("get"));
        assert_eq!(tool_calls[0]["function"]["arguments"], json!("{\"x\":1}"));
        assert_eq!(tool_calls[1]["id"], json!("call_b"));
        assert_eq!(parsed["choices"][0]["finish_reason"], json!("tool_calls"));
    }

    #[test]
    fn parse_returns_null_content_when_only_tool_calls_survive() {
        let sse = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]}}]}\n";
        let parsed = parse_sse_to_openai_response(sse, None).unwrap();
        assert_eq!(parsed["choices"][0]["message"]["content"], Value::Null);
    }

    #[test]
    fn parse_surfaces_a_structured_error_and_ignores_malformed_lines() {
        let sse = concat!(
            "data: not json\n",
            "data: {\"error\":{\"message\":\"billing\",\"status\":403}}\n",
        );
        let parsed = parse_sse_to_openai_response(sse, None).unwrap();
        assert_eq!(parsed["error"]["message"], json!("billing"));
        assert_eq!(parsed["error"]["status"], json!(403));
    }

    #[test]
    fn parse_returns_none_without_a_single_chunk() {
        assert!(parse_sse_to_openai_response("", None).is_none());
        assert!(parse_sse_to_openai_response("data: [DONE]\n", None).is_none());
        assert!(parse_sse_to_openai_response("data: {oops\n", None).is_none());
    }

    #[test]
    fn parse_falls_back_to_the_model_and_now_when_the_first_chunk_is_bare() {
        let parsed = parse_sse_to_openai_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
            Some("fb"),
        )
        .unwrap();
        assert_eq!(parsed["model"], json!("fb"));
        assert!(parsed["created"].as_i64().unwrap() > 0);
    }

    #[test]
    fn responses_message_text_prefers_output_text() {
        let item = json!({"content": [
            {"type": "output_text", "text": "hello"},
            {"type": "text", "text": "ignored"},
        ]});
        assert_eq!(text_from_responses_message_item(&item), "hello");
        let fallback = json!({"content": [{"type": "text", "text": "any"}]});
        assert_eq!(text_from_responses_message_item(&fallback), "any");
        assert_eq!(text_from_responses_message_item(&json!({})), "");
    }

    #[test]
    fn pick_assistant_message_takes_the_last_non_empty() {
        let output = json!([
            {"type": "reasoning", "summary": []},
            {"type": "message", "content": []},
            {"type": "message", "content": [{"type": "output_text", "text": "answer"}]},
            {"type": "message", "content": [{"type": "output_text", "text": ""}]},
        ]);
        let (item, text) = pick_assistant_message_for_chat_completion(&output);
        assert_eq!(text.as_deref(), Some("answer"));
        assert_eq!(item.unwrap()["content"][0]["text"], json!("answer"));

        let (none, _) = pick_assistant_message_for_chat_completion(&json!([]));
        assert!(none.is_none());
    }

    #[test]
    fn chat_completion_to_responses_shapes_reasoning_text_and_tools() {
        let body = json!({
            "id": "chatcmpl-abc",
            "created": 7,
            "model": "gpt",
            "choices": [{
                "message": {
                    "content": "hi",
                    "reasoning_content": "why",
                    "tool_calls": [
                        {"id": "call_1", "function": {"name": "shell", "arguments": "{\"a\":1}"}},
                        {"id": "call_2", "function": {"name": "patch", "arguments": "{\"input\":\"raw\"}"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7},
        });
        let custom: HashSet<String> = ["patch".to_string()].into_iter().collect();
        let out = chat_completion_to_responses(&body, Some(&custom));

        assert_eq!(out["id"], json!("resp_abc"));
        assert_eq!(out["object"], json!("response"));
        assert_eq!(out["status"], json!("completed"));
        assert_eq!(out["created_at"], json!(7));
        let output = out["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], json!("reasoning"));
        assert_eq!(output[0]["summary"][0]["text"], json!("why"));
        assert_eq!(output[1]["type"], json!("message"));
        assert_eq!(output[1]["content"][0]["text"], json!("hi"));
        assert_eq!(output[2]["type"], json!("function_call"));
        assert_eq!(output[2]["id"], json!("fc_call_1"));
        assert_eq!(output[2]["arguments"], json!("{\"a\":1}"));
        assert_eq!(output[3]["type"], json!("custom_tool_call"));
        assert_eq!(output[3]["input"], json!("raw"));
        assert_eq!(out["usage"]["total_tokens"], json!(7));
    }

    #[test]
    fn chat_completion_to_responses_passes_a_choice_less_body_through() {
        let body = json!({"error": "nope"});
        assert_eq!(chat_completion_to_responses(&body, None), body);
    }

    #[test]
    fn a_claude_client_gets_an_anthropic_message_not_a_chat_completion() {
        let parsed = json!({
            "id": "chatcmpl-abc",
            "model": "m",
            "choices": [{
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 4},
        });
        let out = shape_forced_sse_body(parsed, formats::CLAUDE, None);
        assert_eq!(out["type"], json!("message"));
        assert_eq!(out["role"], json!("assistant"));
        assert_eq!(out["id"], json!("abc"), "chatcmpl- prefix is stripped");
        assert_eq!(out["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(out["stop_reason"], json!("end_turn"));
        assert_eq!(out["usage"]["input_tokens"], json!(3));
        assert!(out.get("choices").is_none(), "no chat.completion shape");
    }

    #[test]
    fn an_openai_client_keeps_the_chat_completion_body() {
        let parsed = json!({"choices": [{"message": {"content": "hi"}}]});
        let out = shape_forced_sse_body(parsed.clone(), formats::OPENAI, None);
        assert_eq!(out, parsed);
    }
}
