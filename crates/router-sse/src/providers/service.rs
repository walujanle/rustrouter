//! Provider resolution for a request.
//!
//! `detect_format` reads the *body shape*, not the endpoint, and its order is
//! load-bearing: Responses before Gemini before the OpenAI-specific field probe
//! before Claude. A `messages[]` body that carries `stream_options` is OpenAI,
//! and a Claude-shaped body is only Claude when nothing OpenAI-specific matched.

use serde_json::Value;

use crate::providers::model::Transport;
use crate::providers::registry::registry;
use crate::providers::ui::{is_anthropic_compatible_provider, is_openai_compatible_provider};
use crate::translator::formats;

/// `resolveOpenAICompatibleApiType(provider, credentials)`.
///
/// The stored `providerSpecificData.apiType` is authoritative; legacy nodes
/// created before it was persisted embed the type in their id
/// (`openai-compatible-<chat|responses>-<uuid>`).
pub fn resolve_openai_compatible_api_type(
    provider: &str,
    credentials: Option<&Value>,
) -> &'static str {
    if let Some(stored) = credentials
        .and_then(|c| c.get("providerSpecificData"))
        .and_then(|p| p.get("apiType"))
        .and_then(Value::as_str)
        && (stored == "chat" || stored == "responses")
    {
        return if stored == "responses" {
            "responses"
        } else {
            "chat"
        };
    }
    if provider.contains("responses") {
        "responses"
    } else {
        "chat"
    }
}

/// `detectFormat(body)`: the client's source format.
pub fn detect_format(body: &Value) -> &'static str {
    use crate::translator::concerns::primitives::js_truthy_opt;

    // Responses API: `input` (array or string) and no truthy `messages`.
    if let Some(input) = body.get("input")
        && (input.is_array() || input.is_string())
        && !js_truthy_opt(body.get("messages"))
    {
        return formats::OPENAI_RESPONSES;
    }

    if body.get("contents").is_some_and(Value::is_array) {
        return formats::GEMINI;
    }

    // OpenAI-specific fields, checked BEFORE Claude. Two kinds of test are in
    // play here and both matter: `stream_options`/`response_format`/
    // `logit_bias`/`user` are truthiness (a `null` or `""` does not count), while
    // the rest are presence (a `0` or `null` does count).
    let openai_only = js_truthy_opt(body.get("stream_options"))
        || js_truthy_opt(body.get("response_format"))
        || body.get("logprobs").is_some()
        || body.get("top_logprobs").is_some()
        || body.get("n").is_some()
        || body.get("presence_penalty").is_some()
        || body.get("frequency_penalty").is_some()
        || js_truthy_opt(body.get("logit_bias"))
        || js_truthy_opt(body.get("user"));
    if openai_only {
        return formats::OPENAI;
    }

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        let model_has_slash = body
            .get("model")
            .and_then(Value::as_str)
            .is_some_and(|m| m.contains('/'));
        if let Some(first) = messages.first()
            && let Some(content) = first.get("content").and_then(Value::as_array)
            && content
                .first()
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                == Some("text")
            && !model_has_slash
        {
            // Inside the text branch the test is truthiness, not presence.
            if js_truthy_opt(body.get("system")) || js_truthy_opt(body.get("anthropic_version")) {
                return formats::CLAUDE;
            }
            let has_claude_image = content.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("image")
                    && c.get("source")
                        .and_then(|s| s.get("type"))
                        .and_then(Value::as_str)
                        == Some("base64")
            });
            let has_openai_image = content.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("image_url")
                    && c.get("image_url").and_then(|i| i.get("url")).is_some()
            });
            if has_claude_image {
                return formats::CLAUDE;
            }
            if has_openai_image {
                return formats::OPENAI;
            }
            let has_claude_tool = content.iter().any(|c| {
                matches!(
                    c.get("type").and_then(Value::as_str),
                    Some("tool_use") | Some("tool_result")
                )
            });
            if has_claude_tool {
                return formats::CLAUDE;
            }
        }
        // The second Claude test is `!== undefined`: `system: null` still counts.
        if body.get("system").is_some() || js_truthy_opt(body.get("anthropic_version")) {
            return formats::CLAUDE;
        }
    }

    formats::OPENAI
}

/// `getTargetFormat(provider, credentials)`.
pub fn get_target_format(provider: &str, credentials: Option<&Value>) -> &'static str {
    if is_openai_compatible_provider(provider) {
        return if resolve_openai_compatible_api_type(provider, credentials) == "responses" {
            formats::OPENAI_RESPONSES
        } else {
            formats::OPENAI
        };
    }
    if is_anthropic_compatible_provider(provider) {
        return formats::CLAUDE;
    }
    registry()
        .transport(provider)
        .map(|t| t.format_or_default())
        .unwrap_or(formats::OPENAI)
}

/// `resolveTransport(provider, sourceFormat)`: the `transports[]` entry whose
/// format matches the client's, so a multi-endpoint provider can skip
/// translation entirely.
///
/// The provider entry's own `transports` list is what the dump copies into
/// `Provider::transports`; `Transport::transports` is the same data on the
/// built block, so either read finds it.
pub fn resolve_transport(provider: &str, source_format: &str) -> Option<Transport> {
    let entry = registry().get(provider)?;
    entry
        .transports
        .iter()
        .find(|t| t.format.as_deref() == Some(source_format))
        .cloned()
        .or_else(|| {
            registry()
                .transport(provider)
                .and_then(|t| {
                    t.transports
                        .iter()
                        .find(|e| e.format.as_deref() == Some(source_format))
                })
                .cloned()
        })
}

/// `hasThinkingConfig(body)`.
pub fn has_thinking_config(body: &Value) -> bool {
    let effort = body
        .get("reasoning_effort")
        .is_some_and(crate::translator::concerns::primitives::js_truthy);
    let enabled = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("enabled");
    effort || enabled
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_input_beats_messages() {
        assert_eq!(
            detect_format(&json!({"input": []})),
            formats::OPENAI_RESPONSES
        );
        assert_eq!(
            detect_format(&json!({"input": "hi"})),
            formats::OPENAI_RESPONSES
        );
        assert_eq!(
            detect_format(&json!({"input": [], "messages": []})),
            formats::OPENAI,
            "messages present disqualifies Responses"
        );
    }

    #[test]
    fn gemini_contents_are_detected_by_shape() {
        assert_eq!(detect_format(&json!({"contents": []})), formats::GEMINI);
    }

    #[test]
    fn openai_fields_win_over_the_claude_shape() {
        assert_eq!(
            detect_format(&json!({"messages": [{"role": "user", "content": "hi"}], "n": 1})),
            formats::OPENAI
        );
    }

    #[test]
    fn claude_needs_a_signal_not_just_a_messages_array() {
        assert_eq!(
            detect_format(&json!({"messages": [{"role": "user", "content": "hi"}]})),
            formats::OPENAI
        );
        assert_eq!(
            detect_format(&json!({"system": "s", "messages": [{"role": "user", "content": "hi"}]})),
            formats::CLAUDE
        );
        // A `provider/model` id disqualifies the Claude text heuristic.
        assert_eq!(
            detect_format(
                &json!({"model": "a/b", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]})
            ),
            formats::OPENAI
        );
    }

    #[test]
    fn api_type_prefers_the_stored_value() {
        assert_eq!(
            resolve_openai_compatible_api_type("openai-compatible-chat-x", None),
            "chat"
        );
        assert_eq!(
            resolve_openai_compatible_api_type("openai-compatible-responses-x", None),
            "responses"
        );
        let creds = json!({"providerSpecificData": {"apiType": "chat"}});
        assert_eq!(
            resolve_openai_compatible_api_type("openai-compatible-responses-x", Some(&creds)),
            "chat",
            "the stored value is authoritative"
        );
    }

    #[test]
    fn target_format_matches_the_registry() {
        assert_eq!(get_target_format("deepseek", None), formats::OPENAI);
        assert_eq!(
            get_target_format("anthropic-compatible-foo", None),
            formats::CLAUDE
        );
        assert_eq!(
            get_target_format("openai-compatible-responses-abc", None),
            formats::OPENAI_RESPONSES
        );
    }

    #[test]
    fn thinking_config_detection_reads_both_shapes() {
        assert!(has_thinking_config(&json!({"reasoning_effort": "high"})));
        assert!(has_thinking_config(
            &json!({"thinking": {"type": "enabled"}})
        ));
        assert!(!has_thinking_config(
            &json!({"thinking": {"type": "disabled"}})
        ));
        assert!(!has_thinking_config(&json!({"reasoning_effort": ""})));
    }
}
