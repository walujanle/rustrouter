//! The OpenAI SSE → Ollama NDJSON bridge for `POST /v1/api/chat`.
//!
//! Two behaviours are load-bearing:
//!
//! * `[DONE]` **abandons the rest of the chunk** (`return` inside the
//!   `transform` callback, not `continue`), and the `flush` that follows emits
//!   a second `done` line. A stream that ends with `[DONE]` therefore carries
//!   two.
//! * Tool calls accumulate across chunks by `index` and are flushed only on a
//!   `tool_calls` or `stop` finish reason; `tool_calls` with nothing
//!   accumulated emits nothing, while `stop` always emits the `done` line.
//!
//! The decode is incremental, so a multi-byte character split across a chunk
//! boundary survives; the line framing above is unchanged.

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use crate::executors::executor::ByteStream;

/// `pendingToolCalls[idx]`: the `function.name` / `function.arguments`
/// fragments accumulated so far. The `id` field is never read back out, so it
/// is not kept.
#[derive(Debug, Default, Clone)]
struct PendingToolCall {
    name: String,
    arguments: String,
}

/// `{ model, message: { role: "assistant", content: "" }, done: true }`.
fn done_line(model: &str) -> String {
    json!({
        "model": model,
        "message": { "role": "assistant", "content": "" },
        "done": true,
    })
    .to_string()
}

/// One `data:` payload → the NDJSON lines it produces, plus whether the rest of
/// the chunk must be abandoned (`[DONE]`).
fn process_data(
    data: &str,
    model: &str,
    pending: &mut BTreeMap<u64, PendingToolCall>,
) -> (Vec<String>, bool) {
    if data == "[DONE]" {
        return (vec![done_line(model)], true);
    }

    let Ok(parsed) = serde_json::from_str::<Value>(data) else {
        return (Vec::new(), false);
    };

    let choice = parsed.get("choices").and_then(|c| c.get(0));
    let delta = choice.and_then(|c| c.get("delta"));
    let content = delta
        .and_then(|d| d.get("content"))
        .filter(|c| crate::translator::concerns::primitives::js_truthy(c));

    if let Some(tool_calls) = delta
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in tool_calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let entry = pending.entry(index).or_default();
            if let Some(name) = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                entry.name.push_str(name);
            }
            if let Some(arguments) = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                entry.arguments.push_str(arguments);
            }
        }
    }

    let mut out = Vec::new();
    if let Some(content) = content {
        out.push(
            json!({
                "model": model,
                "message": { "role": "assistant", "content": content },
                "done": false,
            })
            .to_string(),
        );
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if finish_reason == "tool_calls" || finish_reason == "stop" {
        if !pending.is_empty() {
            // `Object.values` over integer-like keys: ascending numeric order,
            // which `BTreeMap` gives for free.
            let formatted: Vec<Value> = pending
                .values()
                .map(|call| {
                    let arguments =
                        serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                    json!({
                        "function": { "name": call.name, "arguments": arguments }
                    })
                })
                .collect();
            out.push(
                json!({
                    "model": model,
                    "message": {
                        "role": "assistant",
                        "content": "",
                        "tool_calls": formatted,
                    },
                    "done": true,
                })
                .to_string(),
            );
            pending.clear();
        } else if finish_reason == "stop" {
            out.push(done_line(model));
        }
    }

    (out, false)
}

/// `transformToOllama(response, model)`.
pub fn transform_to_ollama(input: ByteStream, model: &str) -> ByteStream {
    let model = model.to_string();
    Box::pin(async_stream::stream! {
        let mut input = input;
        // Raw bytes still missing a complete UTF-8 sequence.
        let mut partial: Vec<u8> = Vec::new();
        // The trailing, still-incomplete line.
        let mut buffer = String::new();
        let mut pending: BTreeMap<u64, PendingToolCall> = BTreeMap::new();

        while let Some(chunk) = input.next().await {
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
                let (frames, done) = process_data(data.trim(), &model, &mut pending);
                for frame in frames {
                    yield Ok(Bytes::from(format!("{frame}\n")));
                }
                if done {
                    break;
                }
            }

            buffer = tail;
        }

        yield Ok(Bytes::from(format!("{}\n", done_line(&model))));
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(value: Value) -> String {
        format!("data: {value}\n\n")
    }

    /// Feed raw byte chunks (not `&str` — a test needs to split a character
    /// across the boundary) and collect the whole NDJSON output.
    fn run_bytes(chunks: Vec<Vec<u8>>, model: &str) -> String {
        let input: ByteStream = Box::pin(futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from(c)))
                .collect::<Vec<_>>(),
        ));
        let mut stream = transform_to_ollama(input, model);
        let mut out = Vec::new();
        futures::executor::block_on(async {
            while let Some(Ok(bytes)) = stream.next().await {
                out.extend_from_slice(&bytes);
            }
        });
        String::from_utf8(out).unwrap()
    }

    fn run(chunks: &[&str], model: &str) -> String {
        run_bytes(
            chunks.iter().map(|c| c.as_bytes().to_vec()).collect(),
            model,
        )
    }

    #[test]
    fn content_becomes_ndjson_then_a_done_line() {
        let first = sse(json!({"choices":[{"delta":{"role":"assistant","content":"Hi"}}]}));
        let second = sse(json!({"choices":[{"delta":{},"finish_reason":"stop"}]}));
        let out = run(&[&first, &second], "llama3.2");
        let lines: Vec<Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            json!({"model":"llama3.2","message":{"role":"assistant","content":"Hi"},"done":false})
        );
        assert_eq!(
            lines[1],
            json!({"model":"llama3.2","message":{"role":"assistant","content":""},"done":true})
        );
        // The flush appends its own done line on top of the `stop` one.
        assert_eq!(lines.len(), 3, "{out}");
        assert_eq!(lines[2], lines[1]);
    }

    #[test]
    fn done_sentinel_abandons_the_rest_of_the_chunk_and_flush_adds_another() {
        // The `[DONE]` is followed by a content frame that must be dropped, and
        // the flush still emits its own done line.
        let chunk = format!(
            "data: [DONE]\n{}",
            sse(json!({"choices":[{"delta":{"content":"ignored"}}]}))
        );
        let out = run(&[&chunk], "m");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert!(lines[0].contains("\"done\":true"));
        assert!(lines[1].contains("\"done\":true"));
        assert!(!out.contains("ignored"));
    }

    #[test]
    fn tool_calls_accumulate_across_chunks_and_flush_once() {
        let first = sse(json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"get_","arguments":"{\"a\":"}}
        ]}}]}));
        let second = sse(json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"name":"weather","arguments":"1}"}}
        ]},"finish_reason":"tool_calls"}]}));
        let out = run(&[&first, &second], "m");
        // Second-to-last: the flush's done line is last.
        let flushed: Value = serde_json::from_str(out.lines().rev().nth(1).unwrap()).unwrap();
        assert_eq!(
            flushed,
            json!({
                "model":"m",
                "message":{"role":"assistant","content":"","tool_calls":[
                    {"function":{"name":"get_weather","arguments":{"a":1}}}
                ]},
                "done":true,
            })
        );
    }

    #[test]
    fn a_split_character_survives_a_chunk_boundary() {
        // `é` is the two bytes `c3 a9`; split the frame between them.
        let chunk1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"h\xc3".to_vec();
        let chunk2 = b"\xa9llo\"}}]}\n".to_vec();
        let out = run_bytes(vec![chunk1, chunk2], "m");
        assert!(out.contains("héllo"), "{out}");
    }
}
