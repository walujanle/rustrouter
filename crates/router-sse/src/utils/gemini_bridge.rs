//! The Gemini API facade.
//!
//! Gemini CLI speaks its own wire format. The route converts the Gemini request
//! into the internal OpenAI-shaped body, runs the normal chat pipeline, then
//! converts the OpenAI response (or SSE stream) back into Gemini
//! `GenerateContentResponse` shapes.
//!
//! Native Gemini TTS (`responseModalities: ["AUDIO"]`) is not supported, so
//! every request goes through the translator.

use serde_json::{Map, Value, json};

use crate::executors::executor::ByteStream;
use crate::handlers::chat_core::{ChatBody, ChatResult};
use crate::translator::concerns::primitives::js_truthy;

/// `FINISH_REASON_MAP`: OpenAI `finish_reason` → Gemini `finishReason`.
fn finish_reason_map(reason: &str) -> Option<&'static str> {
    match reason {
        "stop" => Some("STOP"),
        "length" => Some("MAX_TOKENS"),
        "tool_calls" => Some("STOP"),
        "content_filter" => Some("SAFETY"),
        _ => None,
    }
}

/// `convertGeminiToInternal(geminiBody, model, stream)`.
pub fn convert_gemini_to_internal(gemini_body: &Value, model: &str, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = gemini_body.get("systemInstruction") {
        let text = system
            .get("parts")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    if let Some(contents) = gemini_body.get("contents").and_then(Value::as_array) {
        for content in contents {
            let role = if content.get("role").and_then(Value::as_str) == Some("model") {
                "assistant"
            } else {
                "user"
            };
            let text = content
                .get("parts")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            messages.push(json!({ "role": role, "content": text }));
        }
    }

    let generation = gemini_body.get("generationConfig");
    let mut out = Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), Value::Array(messages));
    out.insert("stream".into(), json!(stream));
    // `?? null`: the key is always emitted, `null` when absent.
    out.insert(
        "max_tokens".into(),
        generation
            .and_then(|g| g.get("maxOutputTokens"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert(
        "temperature".into(),
        generation
            .and_then(|g| g.get("temperature"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    out.insert(
        "top_p".into(),
        generation
            .and_then(|g| g.get("topP"))
            .cloned()
            .unwrap_or(Value::Null),
    );
    Value::Object(out)
}

/// `usageMetadata` from an OpenAI `usage` object, or `None` when absent.
fn usage_metadata(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?;
    let mut meta = Map::new();
    meta.insert(
        "promptTokenCount".into(),
        json!(
            usage
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        ),
    );
    meta.insert(
        "candidatesTokenCount".into(),
        json!(
            usage
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        ),
    );
    meta.insert(
        "totalTokenCount".into(),
        json!(
            usage
                .get("total_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0)
        ),
    );
    if let Some(reasoning) = usage
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_i64)
        .filter(|n| *n != 0)
    {
        meta.insert("thoughtsTokenCount".into(), json!(reasoning));
    }
    Some(Value::Object(meta))
}

/// One OpenAI SSE `data:` payload → the Gemini SSE frame it becomes, or `None`
/// to drop it (empty, `[DONE]`, unparseable, no choice, or a role-only delta).
fn sse_line_to_gemini(data: &str, model: &str) -> Option<String> {
    if data.is_empty() || data == "[DONE]" {
        return None;
    }
    let parsed: Value = serde_json::from_str(data).ok()?;
    let choice = parsed.get("choices").and_then(|c| c.get(0))?;
    let delta = choice.get("delta");

    let mut parts: Vec<Value> = Vec::new();
    if let Some(reasoning) = delta
        .and_then(|d| d.get("reasoning_content"))
        .filter(|v| js_truthy(v))
    {
        parts.push(json!({ "text": reasoning, "thought": true }));
    }
    if let Some(content) = delta
        .and_then(|d| d.get("content"))
        .filter(|v| js_truthy(v))
    {
        parts.push(json!({ "text": content }));
    }

    let finish_reason = choice.get("finish_reason").filter(|v| js_truthy(v));
    if parts.is_empty() && finish_reason.is_none() {
        return None;
    }

    let mut candidate = Map::new();
    candidate.insert(
        "content".into(),
        json!({
            "role": "model",
            "parts": if parts.is_empty() { vec![json!({"text": ""})] } else { parts },
        }),
    );
    candidate.insert("index".into(), json!(0));
    if let Some(reason) = finish_reason.and_then(Value::as_str) {
        candidate.insert(
            "finishReason".into(),
            json!(finish_reason_map(reason).unwrap_or("STOP")),
        );
    }

    let mut chunk = Map::new();
    chunk.insert("candidates".into(), json!([Value::Object(candidate)]));

    if finish_reason.is_some()
        && let Some(meta) = usage_metadata(parsed.get("usage"))
    {
        chunk.insert("usageMetadata".into(), meta);
        chunk.insert(
            "modelVersion".into(),
            parsed
                .get("model")
                .filter(|v| js_truthy(v))
                .cloned()
                .unwrap_or_else(|| json!(model)),
        );
    }

    Some(format!("data: {}\r\n\r\n", Value::Object(chunk)))
}

/// `transformOpenAISSEToGeminiSSE(response, model)`: a no-op unless the upstream
/// succeeded with a live stream.
pub fn transform_openai_sse_to_gemini_sse(result: ChatResult, model: &str) -> ChatResult {
    if !result.success() || !matches!(result.body, ChatBody::Stream(_)) {
        return result;
    }
    let ChatResult {
        body,
        log,
        usage_stats,
        error,
        resets_at_ms,
        ..
    } = result;
    let ChatBody::Stream(stream) = body else {
        unreachable!("checked above")
    };

    let model = model.to_string();
    let output: ByteStream = Box::pin(async_stream::stream! {
        let mut stream = stream;
        let mut buffer = String::new();
        let mut partial: Vec<u8> = Vec::new();
        while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
            let Ok(bytes) = chunk else { continue };
            partial.extend_from_slice(&bytes);
            let text = match std::str::from_utf8(&partial) {
                Ok(text) => {
                    let text = text.to_string();
                    partial.clear();
                    text
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    let text = String::from_utf8_lossy(&partial[..valid]).into_owned();
                    partial.drain(..valid);
                    text
                }
            };
            buffer.push_str(&text);
            let mut lines: Vec<&str> = buffer.split('\n').collect();
            let tail = lines.pop().unwrap_or("").to_string();
            for line in lines {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                if let Some(frame) = sse_line_to_gemini(data.trim(), &model) {
                    yield Ok(bytes::Bytes::from(frame));
                }
            }
            buffer = tail;
        }
    });

    ChatResult {
        status: 200,
        headers: vec![
            ("Content-Type".to_string(), "text/event-stream".to_string()),
            ("Cache-Control".to_string(), "no-cache".to_string()),
            ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
        ],
        body: ChatBody::Stream(output),
        log,
        usage_stats,
        error,
        resets_at_ms,
    }
}

fn gemini_json_headers() -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
    ]
}

/// `convertOpenAIResponseToGemini(response, model)`.
pub fn convert_openai_response_to_gemini(result: ChatResult, model: &str) -> ChatResult {
    if !result.success() {
        return result;
    }
    let ChatBody::Json(body) = &result.body else {
        return result;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return result;
    };

    // Already Gemini-shaped, or an error payload: pass through unchanged.
    if parsed.get("candidates").is_some() || parsed.get("error").is_some() {
        return result;
    }

    let Some(choice) = parsed.get("choices").and_then(|c| c.get(0)) else {
        return result;
    };

    let message = choice.get("message");
    let mut parts: Vec<Value> = Vec::new();
    if let Some(reasoning) = message
        .and_then(|m| m.get("reasoning_content"))
        .filter(|v| js_truthy(v))
    {
        parts.push(json!({ "text": reasoning, "thought": true }));
    }
    parts.push(json!({
        "text": message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    }));

    let finish_reason = choice
        .get("finish_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut gemini = Map::new();
    gemini.insert(
        "candidates".into(),
        json!([{
            "content": { "role": "model", "parts": parts },
            "finishReason": finish_reason_map(finish_reason).unwrap_or("STOP"),
            "index": 0,
        }]),
    );
    gemini.insert(
        "modelVersion".into(),
        parsed
            .get("model")
            .filter(|v| js_truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(model)),
    );
    if let Some(meta) = usage_metadata(parsed.get("usage")) {
        gemini.insert("usageMetadata".into(), meta);
    }

    ChatResult {
        status: 200,
        headers: gemini_json_headers(),
        body: ChatBody::Json(Value::Object(gemini).to_string()),
        ..result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_conversion_keeps_the_null_generation_fields() {
        let body = json!({
            "systemInstruction": { "parts": [{ "text": "sys" }, { "text": "two" }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "hi" }] },
                { "role": "model", "parts": [{ "text": "yo" }] },
            ],
            "generationConfig": { "maxOutputTokens": 128, "temperature": 0.5 },
        });
        let out = convert_gemini_to_internal(&body, "gemini/x", true);
        assert_eq!(
            out,
            json!({
                "model": "gemini/x",
                "messages": [
                    { "role": "system", "content": "sys\ntwo" },
                    { "role": "user", "content": "hi" },
                    { "role": "assistant", "content": "yo" },
                ],
                "stream": true,
                "max_tokens": 128,
                "temperature": 0.5,
                "top_p": null,
            })
        );
    }

    #[test]
    fn response_conversion_maps_finish_reason_and_usage() {
        let result = ChatResult {
            status: 200,
            headers: vec![],
            body: ChatBody::Json(
                json!({
                    "model": "gpt-x",
                    "choices": [{
                        "message": { "content": "hi", "reasoning_content": "why" },
                        "finish_reason": "length",
                    }],
                    "usage": {
                        "prompt_tokens": 3,
                        "completion_tokens": 4,
                        "total_tokens": 7,
                        "completion_tokens_details": { "reasoning_tokens": 2 },
                    },
                })
                .to_string(),
            ),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        };
        let out = convert_openai_response_to_gemini(result, "fallback");
        let ChatBody::Json(body) = out.body else {
            panic!("expected json")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [
                        { "text": "why", "thought": true },
                        { "text": "hi" },
                    ]},
                    "finishReason": "MAX_TOKENS",
                    "index": 0,
                }],
                "modelVersion": "gpt-x",
                "usageMetadata": {
                    "promptTokenCount": 3,
                    "candidatesTokenCount": 4,
                    "totalTokenCount": 7,
                    "thoughtsTokenCount": 2,
                },
            })
        );
    }

    #[test]
    fn sse_frames_skip_role_only_deltas_and_emit_usage_on_finish() {
        let role_only = r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#;
        assert_eq!(sse_line_to_gemini(role_only, "m"), None);
        assert_eq!(sse_line_to_gemini("[DONE]", "m"), None);
        assert_eq!(sse_line_to_gemini("", "m"), None);

        let last = r#"{"model":"g","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":0,"total_tokens":1}}"#;
        let frame = sse_line_to_gemini(last, "m").unwrap();
        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\r\n\r\n"));
        let payload: Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(
            payload,
            json!({
                "candidates": [{
                    "content": { "role": "model", "parts": [{ "text": "" }] },
                    "index": 0,
                    "finishReason": "STOP",
                }],
                "usageMetadata": {
                    "promptTokenCount": 1,
                    "candidatesTokenCount": 0,
                    "totalTokenCount": 1,
                },
                "modelVersion": "g",
            })
        );
    }
}
