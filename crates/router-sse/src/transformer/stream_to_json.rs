//! Fold a Responses-API SSE stream back into one `response` object.
//!
//! Used when the client asked for JSON but the provider forced streaming (Codex
//! does). Only four event types carry state: `response.created` seeds the id and
//! creation time, `response.output_item.done` accumulates output items by
//! `output_index`, `response.completed`/`response.done` closes the stream and
//! carries usage, and `response.failed` marks it failed.
//!
//! The output array is dense: every index from 0 to the highest seen is filled,
//! and a gap becomes an empty assistant message.

use std::collections::BTreeMap;

use futures::StreamExt;
use serde_json::{Map, Value, json};

use crate::executors::executor::ByteStream;
use crate::utils::stream_helpers::random_base36;

/// `EMPTY_RESPONSE`.
fn empty_usage() -> Value {
    json!({"input_tokens": 0, "output_tokens": 0, "total_tokens": 0})
}

struct State {
    response_id: String,
    created: i64,
    status: &'static str,
    usage: Value,
    items: BTreeMap<i64, Value>,
}

/// `processSSEMessage(msg, state)`.
fn process_message(msg: &str, state: &mut State) {
    if msg.trim().is_empty() {
        return;
    }

    // `/^event:\s*(.+)$/m` and `/^data:\s*(.+)$/m`: the first line of each kind.
    let mut event_type: Option<&str> = None;
    let mut data_str: Option<&str> = None;
    for line in msg.lines() {
        if event_type.is_none()
            && let Some(rest) = line.strip_prefix("event:")
        {
            event_type = Some(rest.trim());
        }
        if data_str.is_none()
            && let Some(rest) = line.strip_prefix("data:")
        {
            data_str = Some(rest.trim());
        }
    }
    let (Some(event_type), Some(data_str)) = (event_type, data_str) else {
        return;
    };
    if data_str == "[DONE]" {
        return;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(data_str) else {
        return;
    };

    match event_type {
        "response.created" => {
            let response = parsed.get("response");
            if let Some(id) = response.and_then(|r| r.get("id")).filter(|v| v.is_string()) {
                state.response_id = id.as_str().unwrap_or_default().to_string();
            }
            if let Some(created) = response.and_then(|r| r.get("created_at")) {
                state.created = created.as_i64().unwrap_or(state.created);
            }
        }
        "response.output_item.done" => {
            let index = parsed
                .get("output_index")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let item = parsed.get("item").cloned().unwrap_or(Value::Null);
            state.items.insert(index, item);
        }
        "response.completed" | "response.done" => {
            state.status = "completed";
            if let Some(usage) = parsed.get("response").and_then(|r| r.get("usage"))
                && let Some(obj) = usage.as_object()
            {
                let get = |key: &str| obj.get(key).and_then(Value::as_i64).unwrap_or(0);
                state.usage = json!({
                    "input_tokens": get("input_tokens"),
                    "output_tokens": get("output_tokens"),
                    "total_tokens": get("total_tokens"),
                });
            }
        }
        "response.failed" => {
            state.status = "failed";
        }
        _ => {}
    }
}

/// Fold the stream into one JSON object.
///
/// Decoding the whole body once and splitting on `\n\n` is safe because a JSON
/// string cannot contain a raw blank line (newlines are escaped).
pub async fn convert_responses_stream_to_json(mut stream: ByteStream) -> Value {
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();

    let mut state = State {
        response_id: String::new(),
        created: router_db::time::now_ms() / 1000,
        status: "in_progress",
        usage: empty_usage(),
        items: BTreeMap::new(),
    };

    let mut messages: Vec<&str> = text.split("\n\n").collect();
    let tail = messages.pop().unwrap_or_default();
    for message in messages {
        process_message(message, &mut state);
    }
    if !tail.trim().is_empty() {
        process_message(tail, &mut state);
    }

    let mut output: Vec<Value> = Vec::new();
    let max_index = state.items.keys().next_back().copied().unwrap_or(-1);
    for i in 0..=max_index {
        output.push(
            state
                .items
                .get(&i)
                .cloned()
                .unwrap_or_else(|| json!({"type": "message", "content": [], "role": "assistant"})),
        );
    }

    let id = if state.response_id.is_empty() {
        format!("resp_{}_{}", router_db::time::now_ms(), random_base36(6))
    } else {
        state.response_id.clone()
    };
    let status = state.status;

    let mut result = Map::new();
    result.insert("id".into(), json!(id));
    result.insert("object".into(), json!("response"));
    result.insert("created_at".into(), json!(state.created));
    result.insert("status".into(), json!(status));
    result.insert("output".into(), Value::Array(output));
    result.insert("usage".into(), state.usage);
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn sse(body: &str) -> ByteStream {
        Box::pin(futures::stream::iter(vec![Ok(Bytes::from(
            body.to_string(),
        ))]))
    }

    #[tokio::test]
    async fn folds_events_into_a_dense_response_object() {
        let body = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"created_at\":42}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"output_index\":1,\"item\":{\"type\":\"message\",\"content\":[]}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"total_tokens\":10}}}\n\n",
            "data: [DONE]\n\n",
        );
        let out = convert_responses_stream_to_json(sse(body)).await;
        assert_eq!(out["id"], json!("resp_1"));
        assert_eq!(out["created_at"], json!(42));
        assert_eq!(out["status"], json!("completed"));
        assert_eq!(out["usage"]["input_tokens"], json!(7));
        let output = out["output"].as_array().unwrap();
        assert_eq!(output.len(), 2, "index 0 is a synthesized empty message");
        assert_eq!(output[0]["role"], json!("assistant"));
        assert_eq!(output[1]["type"], json!("message"));
    }

    #[tokio::test]
    async fn a_failed_stream_reports_failed() {
        let body = "event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n";
        let out = convert_responses_stream_to_json(sse(body)).await;
        assert_eq!(out["status"], json!("failed"));
        assert_eq!(out["output"].as_array().unwrap().len(), 0);
    }
}
