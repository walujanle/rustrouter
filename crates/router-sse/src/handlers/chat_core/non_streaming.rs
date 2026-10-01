//! The response the provider sent as one JSON body.
//!
//! Most of the conversions here are hand-written because no registry entry
//! exists for the pair — the registry's response translators are streaming
//! shaped, and a non-streaming body needs one whole-body conversion instead.
//! Three of them matter:
//!
//! * **Gemini** — the provider's `candidates[0].content.parts` is
//!   flattened, with `thought` parts routed to `reasoning_content` and inline
//!   image data inlined as a markdown data URL.
//! * **Claude** — `content` blocks are flattened, and the ```json fence some
//!   providers (kimi) wrap their JSON in is stripped.
//! * **Responses** — `chat.completion` becomes a Responses `output` array so a
//!   Codex client's `stream:false` request does not lose its tool calls.
//!
//! `unwrap_cline_envelope` runs before anything reads `choices`/`usage`, and
//! `decloakToolNames` runs on the *raw* provider body, before translation.
//! Both orders are load-bearing.
//!
//! DB-free: usage saving, log appending and pending-request tracking become
//! [`ChatResult`] fields the server persists.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::executors::executor::UpstreamResponse;
use crate::handlers::chat_core::request_detail::{
    extract_usage_from_response, format_done_line, save_usage_stats,
};
use crate::handlers::chat_core::sse_to_json::parse_sse_to_openai_response;
use crate::handlers::chat_core::{ChatBody, ChatContext, ChatResult};
use crate::runtime_config::http_status;
use crate::translator::concerns::finish_reason::from_openai_finish;
use crate::translator::concerns::primitives::{js_number, js_string, js_truthy};
use crate::translator::formats;
use crate::translator::schema::role;
use crate::utils::claude_cloaking::decloak_tool_names;
use crate::utils::fingerprint::restore_tool_names;

/// `parseToolArguments(value)`.
///
/// A falsy value (including `""` and `0`) is `{}`; an object passes through;
/// anything else is JSON, and a parse failure is `{}` rather than the raw text.
pub fn parse_tool_arguments(value: Option<&Value>) -> Value {
    let Some(value) = value.filter(|v| js_truthy(v)) else {
        return json!({});
    };
    if value.is_object() {
        return value.clone();
    }
    match value {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    }
}

/// `openAICompletionToClaudeMessage(responseBody)`.
pub fn open_ai_completion_to_claude_message(response_body: &Value) -> Value {
    let Some(choice) = response_body.get("choices").and_then(|c| c.get(0)) else {
        return response_body.clone();
    };
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content: Vec<Value> = Vec::new();

    let reasoning = message
        .get("reasoning_content")
        .filter(|v| js_truthy(v))
        .or_else(|| {
            message
                .get("provider_specific_fields")
                .and_then(|f| f.get("reasoning_content"))
                .filter(|v| js_truthy(v))
        });
    if let Some(reasoning) = reasoning {
        content.push(json!({ "type": "thinking", "thinking": reasoning }));
    }
    if let Some(text) = message.get("content").and_then(Value::as_str)
        && !text.is_empty()
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for tool_call in tool_calls {
            let function = tool_call.get("function").cloned().unwrap_or(json!({}));
            let id = tool_call
                .get("id")
                .filter(|v| js_truthy(v))
                .map(js_string)
                .unwrap_or_else(|| {
                    format!("toolu_{}_{}", router_db::time::now_ms(), content.len())
                });
            let name = function
                .get("name")
                .filter(|v| js_truthy(v))
                .or_else(|| tool_call.get("name").filter(|v| js_truthy(v)))
                .map(js_string)
                .unwrap_or_default();
            let arguments = function
                .get("arguments")
                .or_else(|| tool_call.get("arguments"));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": parse_tool_arguments(arguments),
            }));
        }
    }
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let usage = response_body.get("usage").cloned().unwrap_or(json!({}));
    let id = response_body
        .get("id")
        .filter(|v| js_truthy(v))
        .map(js_string)
        .unwrap_or_else(|| format!("msg_{}", router_db::time::now_ms()));
    let id = match id.strip_prefix("chatcmpl-") {
        Some(rest) => rest.to_string(),
        None => id,
    };

    json!({
        "id": id,
        "type": "message",
        "role": role::ASSISTANT,
        "model": response_body
            .get("model")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "unknown".to_string()),
        "content": content,
        "stop_reason": from_openai_finish(
            choice.get("finish_reason").and_then(Value::as_str),
            formats::CLAUDE,
        ),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage
                .get("prompt_tokens")
                .filter(|v| js_truthy(v))
                .or_else(|| usage.get("input_tokens").filter(|v| js_truthy(v)))
                .map(|v| js_number(Some(v)))
                .unwrap_or(0),
            "output_tokens": usage
                .get("completion_tokens")
                .filter(|v| js_truthy(v))
                .or_else(|| usage.get("output_tokens").filter(|v| js_truthy(v)))
                .map(|v| js_number(Some(v)))
                .unwrap_or(0),
        },
    })
}

/// `openAICompletionToResponses(responseBody, customToolNames)`.
///
/// A `length` (or other) `finish_reason` surfaces as the `status`; the shared
/// converter derives that when the caller passes no explicit status.
pub fn open_ai_completion_to_responses(
    response_body: &Value,
    custom_tool_names: Option<&HashSet<String>>,
) -> Value {
    super::responses_convert::completion_to_responses(response_body, custom_tool_names, None)
}

/// JS string concatenation for `textContent += part.text`: `null` becomes
/// `"null"`, an array joins with `,`, an object is `"[object Object]"`.
fn js_concat(target: &mut String, value: &Value) {
    match value {
        Value::String(s) => target.push_str(s),
        Value::Number(n) => target.push_str(&n.to_string()),
        Value::Bool(b) => target.push_str(if *b { "true" } else { "false" }),
        Value::Null => target.push_str("null"),
        Value::Array(items) => {
            let joined: Vec<String> = items
                .iter()
                .map(|i| match i {
                    Value::Null => String::new(),
                    other => js_string(other),
                })
                .collect();
            target.push_str(&joined.join(","));
        }
        Value::Object(_) => target.push_str("[object Object]"),
    }
}

/// `Math.floor(new Date(createTime || Date.now()).getTime() / 1000)`.
fn created_seconds(create_time: Option<&Value>) -> i64 {
    let millis = create_time
        .filter(|v| js_truthy(v))
        .and_then(|v| match v {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => router_db::time::parse_iso(s).map(|d| d.timestamp_millis()),
            _ => None,
        })
        .unwrap_or_else(router_db::time::now_ms);
    millis / 1000
}

/// `translateNonStreamingResponse(responseBody, targetFormat, sourceFormat, customToolNames)`.
///
/// The parameter names read backwards from the request direction:
/// `targetFormat` is the format the *provider answered in*, `sourceFormat` is
/// the format the *client speaks*. Both branches below depend on that, so do
/// not "fix" the names.
pub fn translate_non_streaming_response(
    response_body: Value,
    target_format: &str,
    source_format: &str,
    custom_tool_names: Option<&HashSet<String>>,
) -> Value {
    if target_format == source_format {
        return response_body;
    }
    if target_format == formats::OPENAI && source_format == formats::OPENAI_RESPONSES {
        return open_ai_completion_to_responses(&response_body, custom_tool_names);
    }
    if target_format == formats::OPENAI && source_format == formats::CLAUDE {
        return open_ai_completion_to_claude_message(&response_body);
    }
    if target_format == formats::OPENAI {
        return response_body;
    }

    // Gemini family
    if matches!(
        target_format,
        formats::GEMINI | formats::GEMINI_CLI | formats::VERTEX
    ) {
        let response = response_body.get("response").unwrap_or(&response_body);
        if response.get("candidates").and_then(|c| c.get(0)).is_none() {
            return response_body;
        }
        let candidate = &response["candidates"][0];
        let content = candidate.get("content");
        let usage = response
            .get("usageMetadata")
            .or_else(|| response_body.get("usageMetadata"));

        let mut text_content = String::new();
        let mut reasoning_content = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();

        if let Some(parts) = content
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if part.get("thought").and_then(Value::as_bool) == Some(true)
                    && part.get("text").is_some_and(js_truthy)
                {
                    js_concat(&mut reasoning_content, &part["text"]);
                } else if let Some(text) = part.get("text") {
                    js_concat(&mut text_content, text);
                }

                if let Some(function_call) = part.get("functionCall") {
                    let now = router_db::time::now_ms();
                    tool_calls.push(json!({
                        "id": format!(
                            "call_{}_{now}_{}",
                            function_call.get("name").map(js_string).unwrap_or_default(),
                            tool_calls.len()
                        ),
                        "type": "function",
                        "function": {
                            "name": function_call.get("name").map(js_string).unwrap_or_default(),
                            "arguments": serde_json::to_string(
                                function_call.get("args").filter(|v| js_truthy(v)).unwrap_or(&json!({}))
                            ).unwrap_or_else(|_| "{}".to_string()),
                        },
                    }));
                }

                let inline_data = part.get("inlineData").or_else(|| part.get("inline_data"));
                if let Some(data) = inline_data
                    .and_then(|d| d.get("data"))
                    .filter(|v| js_truthy(v))
                {
                    let mime_type = inline_data
                        .and_then(|d| d.get("mimeType").or_else(|| d.get("mime_type")))
                        .filter(|v| js_truthy(v))
                        .map(js_string)
                        .unwrap_or_else(|| "image/png".to_string());
                    text_content.push_str(&format!(
                        "\n![image](data:{mime_type};base64,{})\n",
                        js_string(data)
                    ));
                }
            }
        }

        let mut message = Map::new();
        message.insert("role".into(), json!(role::ASSISTANT));
        if !text_content.is_empty() {
            message.insert("content".into(), json!(text_content));
        }
        if !reasoning_content.is_empty() {
            message.insert("reasoning_content".into(), json!(reasoning_content));
        }
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        if !message.contains_key("content") && !message.contains_key("tool_calls") {
            message.insert("content".into(), json!(""));
        }

        let has_tool_calls = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty());
        let mut finish_reason = candidate
            .get("finishReason")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "stop".to_string())
            .to_lowercase();
        if finish_reason == "stop" && has_tool_calls {
            finish_reason = "tool_calls".to_string();
        }

        let mut result = Map::new();
        result.insert(
            "id".into(),
            json!(format!(
                "chatcmpl-{}",
                response
                    .get("responseId")
                    .filter(|v| js_truthy(v))
                    .map(js_string)
                    .unwrap_or_else(|| router_db::time::now_ms().to_string())
            )),
        );
        result.insert("object".into(), json!("chat.completion"));
        result.insert(
            "created".into(),
            json!(created_seconds(response.get("createTime"))),
        );
        result.insert(
            "model".into(),
            json!(
                response
                    .get("modelVersion")
                    .filter(|v| js_truthy(v))
                    .map(js_string)
                    .unwrap_or_else(|| "gemini".to_string())
            ),
        );
        result.insert(
            "choices".into(),
            Value::Array(vec![json!({
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish_reason,
            })]),
        );

        if let Some(usage) = usage {
            let thoughts = usage
                .get("thoughtsTokenCount")
                .filter(|v| js_truthy(v))
                .map(|v| js_number(Some(v)))
                .unwrap_or(0);
            let mut usage_obj = Map::new();
            usage_obj.insert(
                "prompt_tokens".into(),
                json!(
                    usage
                        .get("promptTokenCount")
                        .filter(|v| js_truthy(v))
                        .map(|v| js_number(Some(v)))
                        .unwrap_or(0)
                        + thoughts
                ),
            );
            usage_obj.insert(
                "completion_tokens".into(),
                json!(
                    usage
                        .get("candidatesTokenCount")
                        .filter(|v| js_truthy(v))
                        .map(|v| js_number(Some(v)))
                        .unwrap_or(0)
                ),
            );
            usage_obj.insert(
                "total_tokens".into(),
                json!(
                    usage
                        .get("totalTokenCount")
                        .filter(|v| js_truthy(v))
                        .map(|v| js_number(Some(v)))
                        .unwrap_or(0)
                ),
            );
            if thoughts > 0 {
                usage_obj.insert(
                    "completion_tokens_details".into(),
                    json!({ "reasoning_tokens": thoughts }),
                );
            }
            result.insert("usage".into(), Value::Object(usage_obj));
        }
        return Value::Object(result);
    }

    // Claude
    if target_format == formats::CLAUDE {
        // Translate even when `content` is missing or null: M3 with
        // `max_tokens: 1` spends the budget on thinking and returns
        // `content: null`, and handing that back would leave the OpenAI client
        // without a `choices` array.
        //
        // The early return covers two cases: a body that is already OpenAI
        // (has `choices`), and one whose `content` is a non-array scalar —
        // xiaomi-tokenplan answers in OpenAI shape even though the request was
        // translated to Claude.
        if response_body.get("choices").is_some()
            || response_body
                .get("content")
                .is_some_and(|c| js_truthy(c) && !c.is_array())
        {
            return response_body;
        }

        let mut text_content = String::new();
        let mut thinking_content = String::new();
        let mut tool_calls: Vec<Value> = Vec::new();

        if let Some(blocks) = response_body.get("content").and_then(Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        // Some providers (kimi) wrap JSON in a ```json fence.
                        let raw = block.get("text").map(js_string).unwrap_or_default();
                        let text = strip_code_fence(&raw);
                        text_content.push_str(&text);
                    }
                    Some("thinking") => {
                        if let Some(thinking) = block.get("thinking").filter(|v| js_truthy(v)) {
                            thinking_content.push_str(&js_string(thinking));
                        }
                    }
                    Some("tool_use") => {
                        let mut tool_call = Map::new();
                        if let Some(id) = block.get("id").filter(|v| js_truthy(v)) {
                            tool_call.insert("id".into(), id.clone());
                        }
                        tool_call.insert("type".into(), json!("function"));
                        let mut function = Map::new();
                        if let Some(name) = block.get("name").filter(|v| js_truthy(v)) {
                            function.insert("name".into(), name.clone());
                        }
                        function.insert(
                            "arguments".into(),
                            json!(
                                serde_json::to_string(
                                    block
                                        .get("input")
                                        .filter(|v| js_truthy(v))
                                        .unwrap_or(&json!({}))
                                )
                                .unwrap_or_else(|_| "{}".to_string())
                            ),
                        );
                        tool_call.insert("function".into(), Value::Object(function));
                        tool_calls.push(Value::Object(tool_call));
                    }
                    _ => {}
                }
            }
        }

        let mut message = Map::new();
        message.insert("role".into(), json!(role::ASSISTANT));
        if !text_content.is_empty() {
            message.insert("content".into(), json!(text_content));
        }
        if !thinking_content.is_empty() {
            message.insert("reasoning_content".into(), json!(thinking_content));
        }
        if !tool_calls.is_empty() {
            message.insert("tool_calls".into(), Value::Array(tool_calls));
        }
        if !message.contains_key("content") && !message.contains_key("tool_calls") {
            message.insert("content".into(), json!(""));
        }

        let mut finish_reason = response_body
            .get("stop_reason")
            .filter(|v| js_truthy(v))
            .map(js_string)
            .unwrap_or_else(|| "stop".to_string());
        if finish_reason == "end_turn" {
            finish_reason = "stop".to_string();
        }
        if finish_reason == "tool_use" {
            finish_reason = "tool_calls".to_string();
        }

        let mut result = Map::new();
        result.insert(
            "id".into(),
            json!(format!(
                "chatcmpl-{}",
                response_body
                    .get("id")
                    .filter(|v| js_truthy(v))
                    .map(js_string)
                    .unwrap_or_else(|| router_db::time::now_ms().to_string())
            )),
        );
        result.insert("object".into(), json!("chat.completion"));
        result.insert("created".into(), json!(router_db::time::now_ms() / 1000));
        result.insert(
            "model".into(),
            json!(
                response_body
                    .get("model")
                    .filter(|v| js_truthy(v))
                    .map(js_string)
                    .unwrap_or_else(|| "claude".to_string())
            ),
        );
        result.insert(
            "choices".into(),
            Value::Array(vec![json!({
                "index": 0,
                "message": Value::Object(message),
                "finish_reason": finish_reason,
            })]),
        );

        if let Some(usage) = response_body.get("usage").filter(|v| js_truthy(v)) {
            let input = usage
                .get("input_tokens")
                .filter(|v| js_truthy(v))
                .map(|v| js_number(Some(v)))
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .filter(|v| js_truthy(v))
                .map(|v| js_number(Some(v)))
                .unwrap_or(0);
            result.insert(
                "usage".into(),
                json!({
                    "prompt_tokens": input,
                    "completion_tokens": output,
                    "total_tokens": input + output,
                }),
            );
        }
        return Value::Object(result);
    }

    // Ollama is deliberately absent: no kept provider speaks it, so the branch
    // can never be reached.

    response_body
}

/// `raw.replace(/^\s*```\s*json\s*\n?/i, "").replace(/\n?\s*```\s*$/i, "")`.
fn strip_code_fence(raw: &str) -> String {
    static OPEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^\s*```\s*json\s*\n?").unwrap());
    static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\n?\s*```\s*$").unwrap());
    let opened = OPEN.replace(raw, "");
    CLOSE.replace(&opened, "").into_owned()
}

/// `unwrapClineEnvelope(body, provider)`.
///
/// No-op unless the transport opts in via `quirks.clineEnvelope`; the error
/// envelope (`{"success":false,…}`) never matches.
///
/// No kept provider declares the quirk after `cline` left the registry, but the
/// handler is registry-keyed, not provider-keyed — the same shape as the other
/// quirks — so it stays reachable the moment an entry declares it again.
pub fn unwrap_cline_envelope(body: Value, provider: &str) -> Value {
    let opted_in = crate::providers::registry::registry()
        .transport(provider)
        .and_then(|t| t.quirks.as_ref())
        .and_then(|q| q.get("clineEnvelope"))
        .is_some_and(js_truthy);
    if !opted_in || !body.is_object() {
        return body;
    }
    let success = body.get("success").and_then(Value::as_bool) == Some(true);
    if success && body.get("data").is_some_and(Value::is_object) {
        return body.get("data").cloned().unwrap_or(body);
    }
    body
}

/// `upstreamResponseHeaders(headers)`: only the retry hints are forwarded.
/// Lives here because this handler is its only non-streaming caller.
pub fn upstream_response_headers(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in headers.iter() {
        let key = name.as_str().to_lowercase();
        if (key == "retry-after"
            || key == "x-should-retry"
            || key.starts_with("anthropic-ratelimit-"))
            && let Ok(value) = value.to_str()
        {
            out.push((key, value.to_string()));
        }
    }
    out
}

/// `handleNonStreamingResponse({…})`.
///
/// Always returns a body: a non-SSE upstream that fails to parse JSON becomes
/// a 502 rather than an error.
pub async fn handle_non_streaming_response(
    provider_response: UpstreamResponse,
    ctx: &ChatContext<'_>,
) -> ChatResult {
    let headers = provider_response.headers.clone();
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body_text = match provider_response.text().await {
        Ok(text) => text,
        Err(error) => {
            tracing::error!(
                target: "router_sse::chat_core",
                "[ChatCore] Failed to read response body from {}: {error}",
                ctx.provider
            );
            return ChatResult::error_logged(
                http_status::BAD_GATEWAY,
                &format!("Invalid JSON response from {}", ctx.provider),
            );
        }
    };

    let mut response_body = if content_type.contains("text/event-stream") {
        match parse_sse_to_openai_response(&body_text, Some(ctx.model)) {
            Some(parsed) => parsed,
            None => {
                return ChatResult::error_logged(
                    http_status::BAD_GATEWAY,
                    "Invalid SSE response for non-streaming request",
                );
            }
        }
    } else {
        match serde_json::from_str::<Value>(&body_text) {
            Ok(parsed) => parsed,
            Err(error) => {
                tracing::error!(
                    target: "router_sse::chat_core",
                    "[ChatCore] Failed to parse JSON from {}: {error}",
                    ctx.provider
                );
                return ChatResult::error_logged(
                    http_status::BAD_GATEWAY,
                    &format!("Invalid JSON response from {}", ctx.provider),
                );
            }
        }
    };

    // Unwrap before anything reads choices/usage, so usage tracking sees the
    // inner `data` object.
    response_body = unwrap_cline_envelope(response_body, ctx.provider);

    tracing::debug!(
        target: "router_sse::chat_core",
        "[ChatCore] {} provider response: {}",
        ctx.provider,
        response_body
    );

    // Decloak on the raw Claude body, before any translation.
    let mut response_body = response_body;
    decloak_tool_names(&mut response_body, ctx.tool_name_map);

    let usage = extract_usage_from_response(&response_body);
    let latency = router_db::time::now_ms() - ctx.request_start_time_ms;
    if let Some(usage) = &usage {
        tracing::info!(
            target: "router_sse::chat_core",
            "{} {}",
            ctx.req_tag,
            format_done_line(Some(usage), &json!({"total": latency, "ttft": latency}))
        );
    }
    let usage_stats = save_usage_stats(
        ctx.provider,
        ctx.model,
        usage.as_ref(),
        ctx.connection_id,
        ctx.api_key,
        ctx.client_endpoint,
        "USAGE",
        true,
    );

    let mut translated_response = if ctx.target_format != ctx.source_format {
        translate_non_streaming_response(
            response_body.clone(),
            ctx.target_format,
            ctx.source_format,
            ctx.custom_tool_names,
        )
    } else {
        response_body.clone()
    };

    let is_claude_message_response = ctx.source_format == formats::CLAUDE
        && translated_response.get("type").and_then(Value::as_str) == Some("message");
    // A Responses translation has no `choices`; skip the Chat-Completions-only
    // post-processing below for it.
    let is_responses_response = ctx.source_format == formats::OPENAI_RESPONSES
        && translated_response.get("object").and_then(Value::as_str) == Some("response");

    // Some providers return a non-standard finish_reason (e.g. "other") even
    // though they emitted tool calls.
    if let Some(choices) = translated_response
        .get_mut("choices")
        .and_then(Value::as_array_mut)
        && let Some(choice) = choices.get_mut(0)
    {
        let has_tool_calls = choice
            .get("message")
            .and_then(|m| m.get("tool_calls"))
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty());
        if has_tool_calls
            && choice.get("finish_reason").and_then(Value::as_str) != Some("tool_calls")
        {
            choice["finish_reason"] = json!("tool_calls");
        }
    }

    // Ensure OpenAI-required fields.
    if !is_claude_message_response
        && !is_responses_response
        && let Some(obj) = translated_response.as_object_mut()
    {
        if obj.get("object").is_none_or(|v| !js_truthy(v)) {
            obj.insert("object".into(), json!("chat.completion"));
        }
        if obj.get("created").is_none_or(|v| !js_truthy(v)) {
            obj.insert("created".into(), json!(router_db::time::now_ms() / 1000));
        }
    }

    // Strip Azure-specific fields.
    if !is_claude_message_response && !is_responses_response {
        if let Some(obj) = translated_response.as_object_mut() {
            obj.shift_remove("prompt_filter_results");
        }
        if let Some(choices) = translated_response
            .get_mut("choices")
            .and_then(Value::as_array_mut)
        {
            for choice in choices.iter_mut() {
                if let Some(obj) = choice.as_object_mut() {
                    obj.shift_remove("content_filter_results");
                }
            }
        }
    }

    if let Some(usage) = translated_response.get("usage").filter(|u| js_truthy(u)) {
        let buffered = crate::utils::usage_tracking::add_buffer_to_usage(usage);
        let filtered =
            crate::utils::usage_tracking::filter_usage_for_format(&buffered, ctx.source_format);
        translated_response["usage"] = filtered;
    }

    // Strip reasoning_content only when content is non-empty: a thinking model
    // that used its whole budget on reasoning has nothing else to show.
    if !is_claude_message_response
        && !is_responses_response
        && let Some(choices) = translated_response
            .get_mut("choices")
            .and_then(Value::as_array_mut)
    {
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

    tracing::debug!(
        target: "router_sse::chat_core",
        "[ChatCore] {} converted response: {}",
        ctx.provider,
        translated_response
    );

    restore_tool_names(&mut translated_response, ctx.tool_name_map);

    let mut out_headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
    ];
    out_headers.extend(upstream_response_headers(&headers));

    ChatResult {
        status: 200,
        body: ChatBody::Json(translated_response.to_string()),
        headers: out_headers,
        log: Some(json!({ "tokens": usage.clone().unwrap_or(json!({})), "status": "200 OK" })),
        usage_stats,
        error: None,
        resets_at_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_arguments_falls_back_to_an_empty_object() {
        assert_eq!(parse_tool_arguments(None), json!({}));
        assert_eq!(parse_tool_arguments(Some(&json!(""))), json!({}));
        assert_eq!(parse_tool_arguments(Some(&json!(0))), json!({}));
        assert_eq!(parse_tool_arguments(Some(&json!("not json"))), json!({}));
        assert_eq!(
            parse_tool_arguments(Some(&json!("{\"a\":1}"))),
            json!({"a": 1})
        );
        assert_eq!(
            parse_tool_arguments(Some(&json!({"b": 2}))),
            json!({"b": 2})
        );
    }

    #[test]
    fn claude_to_openai_translates_text_thinking_and_tools() {
        let body = json!({
            "id": "msg_1",
            "model": "claude-x",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "```json\n{\"a\":1}\n```"},
                {"type": "tool_use", "id": "tu_1", "name": "shell", "input": {"cmd": "ls"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 11, "output_tokens": 4},
        });
        let out = translate_non_streaming_response(body, formats::CLAUDE, formats::OPENAI, None);

        assert_eq!(out["id"], json!("chatcmpl-msg_1"));
        assert_eq!(out["object"], json!("chat.completion"));
        assert_eq!(out["model"], json!("claude-x"));
        let message = &out["choices"][0]["message"];
        assert_eq!(
            message["content"],
            json!("{\"a\":1}"),
            "the fence is stripped"
        );
        assert_eq!(message["reasoning_content"], json!("hmm"));
        assert_eq!(message["tool_calls"][0]["function"]["name"], json!("shell"));
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            json!("{\"cmd\":\"ls\"}")
        );
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(out["usage"]["prompt_tokens"], json!(11));
        assert_eq!(out["usage"]["total_tokens"], json!(15));
    }

    #[test]
    fn claude_to_openai_handles_a_null_content_body() {
        // M3 with max_tokens:1 returns content: null; the client still needs a
        // `choices` array.
        let body = json!({
            "id": "msg_2",
            "model": "m3",
            "content": null,
            "stop_reason": "end_turn",
        });
        let out = translate_non_streaming_response(body, formats::CLAUDE, formats::OPENAI, None);
        assert_eq!(out["choices"][0]["message"]["content"], json!(""));
        assert_eq!(out["choices"][0]["finish_reason"], json!("stop"));
    }

    #[test]
    fn claude_to_openai_passes_through_an_already_openai_body() {
        let body = json!({"choices": [{"message": {"content": "x"}}]});
        let out =
            translate_non_streaming_response(body.clone(), formats::CLAUDE, formats::OPENAI, None);
        assert_eq!(out, body);

        let scalar = json!({"content": "plain string"});
        let out = translate_non_streaming_response(
            scalar.clone(),
            formats::CLAUDE,
            formats::OPENAI,
            None,
        );
        assert_eq!(out, scalar);
    }

    #[test]
    fn openai_to_responses_keeps_tool_calls_and_status() {
        let body = json!({
            "id": "chatcmpl-zz",
            "created": 42,
            "model": "gpt",
            "choices": [{
                "message": {
                    "content": "done",
                    "tool_calls": [{"id": "call_9", "function": {"name": "shell", "arguments": "{}"}}],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2},
        });
        let out = open_ai_completion_to_responses(&body, None);
        assert_eq!(out["id"], json!("resp_zz"));
        assert_eq!(out["status"], json!("completed"));
        assert_eq!(out["created_at"], json!(42));
        assert_eq!(out["output"][1]["type"], json!("function_call"));
        assert_eq!(out["usage"]["total_tokens"], json!(3));

        // A `length` finish is surfaced verbatim, unlike the sse_to_json copy.
        let mut length_body = body.clone();
        length_body["choices"][0]["finish_reason"] = json!("length");
        let out = open_ai_completion_to_responses(&length_body, None);
        assert_eq!(out["status"], json!("length"));
    }

    #[test]
    fn openai_to_claude_message_shapes_the_message() {
        let body = json!({
            "id": "chatcmpl-abc",
            "model": "m",
            "choices": [{
                "message": {"content": "hi", "reasoning_content": "why"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 2, "completion_tokens": 1},
        });
        let out = open_ai_completion_to_claude_message(&body);
        assert_eq!(out["id"], json!("abc"));
        assert_eq!(out["type"], json!("message"));
        assert_eq!(out["stop_reason"], json!("end_turn"));
        assert_eq!(out["stop_sequence"], Value::Null);
        assert_eq!(
            out["content"][0],
            json!({"type": "thinking", "thinking": "why"})
        );
        assert_eq!(out["content"][1], json!({"type": "text", "text": "hi"}));
        assert_eq!(out["usage"]["input_tokens"], json!(2));
    }

    #[test]
    fn gemini_to_openai_splits_thought_parts_and_inlines_images() {
        let body = json!({
            "candidates": [{
                "content": {"parts": [
                    {"thought": true, "text": "plan"},
                    {"text": "answer"},
                    {"functionCall": {"name": "shell", "args": {"cmd": "ls"}}},
                    {"inlineData": {"mimeType": "image/png", "data": "AAA"}},
                ]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 3, "totalTokenCount": 9, "thoughtsTokenCount": 2},
            "modelVersion": "gemini-x",
            "responseId": "resp-1",
            "createTime": "2026-01-01T00:00:00.000Z",
        });
        let out = translate_non_streaming_response(body, formats::GEMINI, formats::OPENAI, None);
        let message = &out["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], json!("plan"));
        assert_eq!(
            message["content"],
            json!("answer\n![image](data:image/png;base64,AAA)\n")
        );
        assert_eq!(message["tool_calls"][0]["function"]["name"], json!("shell"));
        assert_eq!(out["choices"][0]["finish_reason"], json!("tool_calls"));
        assert_eq!(out["model"], json!("gemini-x"));
        assert_eq!(out["created"], json!(1_767_225_600));
        // `thoughtsTokenCount` folds into prompt_tokens and is echoed as a detail.
        assert_eq!(out["usage"]["prompt_tokens"], json!(7));
        assert_eq!(
            out["usage"]["completion_tokens_details"]["reasoning_tokens"],
            json!(2)
        );
    }

    #[test]
    fn same_format_is_the_identity() {
        let body = json!({"anything": true});
        assert_eq!(
            translate_non_streaming_response(body.clone(), formats::OPENAI, formats::OPENAI, None),
            body
        );
    }

    #[test]
    fn code_fence_stripping_matches_the_reference_regexes() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_code_fence("```JSON\nx```"), "x");
        assert_eq!(strip_code_fence("plain"), "plain");
        // The opening fence is stripped even without a closing one.
        assert_eq!(strip_code_fence("```json\nx"), "x");
    }

    #[test]
    fn upstream_headers_forward_only_the_retry_hints() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after", "30".parse().unwrap());
        headers.insert(
            "anthropic-ratelimit-requests-remaining",
            "5".parse().unwrap(),
        );
        headers.insert("content-type", "application/json".parse().unwrap());
        let mut out = upstream_response_headers(&headers);
        out.sort();
        assert_eq!(
            out,
            vec![
                (
                    "anthropic-ratelimit-requests-remaining".to_string(),
                    "5".to_string()
                ),
                ("retry-after".to_string(), "30".to_string()),
            ]
        );
    }

    #[test]
    fn cline_envelope_is_unwrapped_only_for_opted_in_providers() {
        let body = json!({"success": true, "data": {"choices": []}});
        // No kept provider declares the quirk after `cline` left the registry,
        // so nothing is unwrapped and the body passes through untouched.
        assert_eq!(unwrap_cline_envelope(body.clone(), "cline"), body);
        assert_eq!(unwrap_cline_envelope(body.clone(), "not-a-provider"), body);
        // The error envelope never matches.
        let failure = json!({"success": false, "data": {"choices": []}});
        assert_eq!(unwrap_cline_envelope(failure.clone(), "cline"), failure);
    }
}
