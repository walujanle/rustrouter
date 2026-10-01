//! SSE constants and framing.
//!
//! Frames are byte-compared downstream, so the two `\n\n` terminators and the
//! `"data: "` prefix (space included) are part of the contract, not cosmetics.
//!
//! `chat_chunk_sse` is deliberately a thin wrapper over
//! `translator::concerns::primitives::build_chunk`: both emit exactly the same
//! object, and a second hand-rolled copy is a key-order divergence waiting to
//! happen.

use serde_json::Value;

use crate::translator::concerns::primitives::build_chunk;

/// `SSE_DONE`.
pub const SSE_DONE: &str = "data: [DONE]\n\n";

/// `SSE_HEADERS`. Header-name casing is preserved verbatim — it is wire-visible.
pub const SSE_HEADERS: [(&str, &str); 3] = [
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("Connection", "keep-alive"),
];

/// `SSE_HEADERS_NO_BUFFER`: the web-cookie executor variant behind nginx.
/// `X-Accel-Buffering: no` disables proxy buffering, which otherwise holds the
/// stream until the response completes.
pub const SSE_HEADERS_NO_BUFFER: [(&str, &str); 3] = [
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("X-Accel-Buffering", "no"),
];

/// `SSE_HEADERS_CORS`: the client-facing variant, permissive origin added.
pub const SSE_HEADERS_CORS: [(&str, &str); 4] = [
    ("Content-Type", "text/event-stream"),
    ("Cache-Control", "no-cache"),
    ("Connection", "keep-alive"),
    ("Access-Control-Allow-Origin", "*"),
];

/// `sseChunk(data)`.
///
/// `Value`'s `Display` is the compact serializer, so it matches
/// `JSON.stringify(data)` byte for byte; there is no fallible step to handle.
pub fn sse_chunk(data: &Value) -> String {
    format!("data: {data}\n\n")
}

/// `chatChunkSse({ id, created, model, delta, finishReason })`.
///
/// `finish_reason` is always emitted — `null` when absent, never omitted.
pub fn chat_chunk_sse(
    id: &Value,
    created: &Value,
    model: &Value,
    delta: Value,
    finish_reason: Option<&str>,
) -> String {
    sse_chunk(&build_chunk(id, created, model, delta, finish_reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn done_sentinel_is_byte_exact() {
        assert_eq!(SSE_DONE, "data: [DONE]\n\n");
    }

    #[test]
    fn header_lists_keep_the_expected_order_and_casing() {
        assert_eq!(SSE_HEADERS[0], ("Content-Type", "text/event-stream"));
        assert_eq!(SSE_HEADERS[1], ("Cache-Control", "no-cache"));
        assert_eq!(SSE_HEADERS[2], ("Connection", "keep-alive"));
        assert_eq!(SSE_HEADERS_NO_BUFFER[2], ("X-Accel-Buffering", "no"));
        assert_eq!(SSE_HEADERS_CORS[3], ("Access-Control-Allow-Origin", "*"));
    }

    #[test]
    fn sse_chunk_uses_the_data_prefix_and_blank_line() {
        assert_eq!(sse_chunk(&json!({"a": 1})), "data: {\"a\":1}\n\n");
        assert_eq!(sse_chunk(&json!("x")), "data: \"x\"\n\n");
        assert_eq!(sse_chunk(&json!(null)), "data: null\n\n");
    }

    #[test]
    fn chat_chunk_frame_is_byte_exact_with_a_null_finish_reason() {
        let out = chat_chunk_sse(
            &json!("c1"),
            &json!(1),
            &json!("m"),
            json!({"content": "hi"}),
            None,
        );
        let expected = concat!(
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,",
            "\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},",
            "\"finish_reason\":null}]}\n\n"
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn chat_chunk_carries_an_explicit_finish_reason() {
        let out = chat_chunk_sse(
            &json!("c1"),
            &json!(2),
            &json!("m"),
            json!({}),
            Some("stop"),
        );
        assert!(out.contains("\"finish_reason\":\"stop\""));
        assert!(out.contains("\"delta\":{}"));
        assert!(out.ends_with("\n\n"));
    }
}
