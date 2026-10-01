//! The `/v1/responses` dispatch.
//!
//! Three things happen: rewrite the Responses body into Chat Completions, call
//! `handleChatCore`, then post-process whatever came back. The first two belong
//! to the chat core (see `handlers/chat_core.rs`), so what lives here is the
//! post-processing: given the chat core's [`ChatResult`] and the client's
//! `stream` preference, decide the body the client receives.
//!
//! Two cases survive, and one does not:
//!
//! * **A JSON client that hit a forced-streaming provider.** The chat core
//!   returned SSE; fold it back into one `response` object.
//! * **A streaming client.** The chat core already translated the upstream
//!   stream into Responses SSE, so a second `pipeThrough` would be the
//!   identity here. That is why the stream conversion lives in
//!   `transformer/mod.rs` rather than as a pass of its own.
//! * **A non-SSE body.** Returned unchanged.

use crate::handlers::chat_core::{ChatBody, ChatResult};
use crate::transformer::stream_to_json::convert_responses_stream_to_json;

/// `response.headers.get("Content-Type")`, lowercased.
fn content_type(result: &ChatResult) -> String {
    result
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.to_lowercase())
        .unwrap_or_default()
}

/// `handleResponsesCore({ body, … })`, from the point `handleChatCore` returns.
pub async fn handle_responses_core(
    result: ChatResult,
    client_requested_streaming: bool,
) -> ChatResult {
    if !result.success()
        || !content_type(&result).contains("text/event-stream")
        || client_requested_streaming
    {
        return result;
    }

    // A JSON client landed on an SSE body: fold it. `mem::replace` keeps the
    // usage/log fields the server still has to persist.
    let mut result = result;
    let body = std::mem::replace(&mut result.body, ChatBody::Bytes(Vec::new()));
    let ChatBody::Stream(stream) = body else {
        result.body = body;
        return result;
    };

    let json_response = convert_responses_stream_to_json(stream).await;
    let mut out = ChatResult::json(200, &json_response);
    out.headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Cache-Control".to_string(), "no-cache".to_string()),
        ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
    ];
    out.log = result.log.take();
    out.usage_stats = result.usage_stats.take();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures::StreamExt;
    use serde_json::json;

    fn sse_result(body: &str) -> ChatResult {
        let stream: crate::executors::executor::ByteStream =
            Box::pin(futures::stream::iter(vec![Ok(Bytes::from(
                body.to_string(),
            ))]));
        let mut result = ChatResult::json(200, &json!({}));
        result.headers = vec![("Content-Type".to_string(), "text/event-stream".to_string())];
        result.body = ChatBody::Stream(stream);
        result.usage_stats = Some(json!({"prompt_tokens": 1}));
        result
    }

    const RESPONSES_SSE: &str = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_9\",\"created_at\":1}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}}\n\n",
    );

    #[tokio::test]
    async fn a_json_client_gets_the_stream_folded_back() {
        let result = handle_responses_core(sse_result(RESPONSES_SSE), false).await;
        let ChatBody::Json(text) = result.body else {
            panic!("expected a JSON body");
        };
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["id"], json!("resp_9"));
        assert_eq!(parsed["status"], json!("completed"));
        // The persisted row rides through the fold.
        assert_eq!(result.usage_stats, Some(json!({"prompt_tokens": 1})));
    }

    #[tokio::test]
    async fn a_streaming_client_keeps_the_stream_untouched() {
        let result = handle_responses_core(sse_result(RESPONSES_SSE), true).await;
        let ChatBody::Stream(mut stream) = result.body else {
            panic!("expected a stream body");
        };
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .contains("response.created")
        );
    }

    #[tokio::test]
    async fn a_non_sse_body_is_returned_unchanged() {
        let mut result = ChatResult::json(200, &json!({"ok": true}));
        result.headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        let result = handle_responses_core(result, false).await;
        let ChatBody::Json(text) = result.body else {
            panic!("expected a JSON body");
        };
        assert_eq!(text, json!({"ok": true}).to_string());
    }
}
