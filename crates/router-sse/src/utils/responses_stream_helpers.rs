//! Termination and framing for OpenAI Responses API passthrough streams.
//!
//! A Responses stream that closes without a terminal event leaves the client
//! waiting forever, so an aborted or stalled stream is closed with a synthetic
//! `response.failed` followed by `[DONE]`. Without this the client hangs on a
//! dead socket instead of surfacing an error.

use serde_json::{Value, json};

use crate::translator::formats;
use crate::utils::stream_helpers::format_sse;

/// The events that mean a Responses stream is over.
const TERMINAL_EVENTS: [&str; 4] = [
    "response.completed",
    "response.done",
    "response.failed",
    "error",
];

/// `getOpenAIResponsesEventName(eventName, chunk)`.
pub fn get_openai_responses_event_name(
    event_name: Option<&str>,
    chunk: Option<&Value>,
) -> Option<String> {
    if let Some(name) = event_name.filter(|n| !n.is_empty()) {
        return Some(name.to_string());
    }
    chunk
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `isOpenAIResponsesTerminalEvent(eventName, chunk)`.
///
/// The status check is the second chance: a stream whose event name is missing
/// or unrecognised still ends if the payload says so.
pub fn is_openai_responses_terminal_event(event_name: Option<&str>, chunk: Option<&Value>) -> bool {
    if let Some(name) = get_openai_responses_event_name(event_name, chunk)
        && TERMINAL_EVENTS.contains(&name.as_str())
    {
        return true;
    }
    matches!(
        chunk
            .and_then(|c| c.get("response"))
            .and_then(|r| r.get("status"))
            .and_then(Value::as_str),
        Some("completed") | Some("failed")
    )
}

/// `formatIncompleteOpenAIResponsesStreamFailure()`.
///
/// The `id` is `resp_` plus the current millisecond clock, matching
/// `Date.now()`.
pub fn format_incomplete_openai_responses_stream_failure() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format_sse(
        &json!({
            "event": "response.failed",
            "data": {
                "type": "response.failed",
                "response": {
                    "id": format!("resp_{now}"),
                    "status": "failed",
                    "error": {
                        "type": "stream_error",
                        "code": "stream_disconnected",
                        "message": "stream closed before response.completed",
                    },
                },
            },
        }),
        Some(formats::OPENAI_RESPONSES),
    )
}

/// `buildAbortedResponsesTerminalBytes()`.
pub fn build_aborted_responses_terminal_bytes() -> Vec<u8> {
    format!(
        "{}data: [DONE]\n\n",
        format_incomplete_openai_responses_stream_failure()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_name_comes_from_the_argument_then_the_chunk() {
        assert_eq!(
            get_openai_responses_event_name(Some("response.completed"), None).as_deref(),
            Some("response.completed")
        );
        // An empty argument falls through to the chunk.
        assert_eq!(
            get_openai_responses_event_name(Some(""), Some(&json!({"type": "response.done"})))
                .as_deref(),
            Some("response.done")
        );
        assert!(get_openai_responses_event_name(None, Some(&json!({}))).is_none());
        assert!(get_openai_responses_event_name(None, None).is_none());
    }

    #[test]
    fn every_terminal_event_is_recognised() {
        for name in [
            "response.completed",
            "response.done",
            "response.failed",
            "error",
        ] {
            assert!(
                is_openai_responses_terminal_event(Some(name), None),
                "{name}"
            );
        }
        assert!(!is_openai_responses_terminal_event(
            Some("response.output_text.delta"),
            None
        ));
    }

    #[test]
    fn a_response_status_can_end_the_stream_on_its_own() {
        assert!(is_openai_responses_terminal_event(
            None,
            Some(&json!({"response": {"status": "completed"}}))
        ));
        assert!(is_openai_responses_terminal_event(
            None,
            Some(&json!({"response": {"status": "failed"}}))
        ));
        assert!(!is_openai_responses_terminal_event(
            None,
            Some(&json!({"response": {"status": "in_progress"}}))
        ));
        assert!(!is_openai_responses_terminal_event(
            None,
            Some(&json!({"response": {}}))
        ));
    }

    #[test]
    fn the_synthetic_failure_is_a_responses_event_frame() {
        let frame = format_incomplete_openai_responses_stream_failure();
        assert!(
            frame.starts_with("event: response.failed\ndata: {"),
            "{frame}"
        );
        assert!(frame.ends_with("\n\n"), "{frame}");
        // The payload carries the failure the client surfaces.
        let data =
            frame.lines().find(|l| l.starts_with("data: ")).unwrap()["data: ".len()..].to_string();
        let parsed: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["type"], json!("response.failed"));
        assert_eq!(parsed["response"]["status"], json!("failed"));
        assert_eq!(
            parsed["response"]["error"]["code"],
            json!("stream_disconnected")
        );
        assert!(
            parsed["response"]["id"]
                .as_str()
                .unwrap()
                .starts_with("resp_")
        );
    }

    #[test]
    fn the_terminal_bytes_end_with_done() {
        let bytes = build_aborted_responses_terminal_bytes();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert!(text.contains("response.failed"));
    }
}
