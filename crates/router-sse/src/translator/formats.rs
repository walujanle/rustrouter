//! Wire-format identifiers.
//!
//! Kept as `&str` constants rather than an enum because the format strings are
//! also registry keys (`"claude:openai"`) and model `targetFormat` values that
//! arrive as data. An enum would need a fallible parse at every boundary for no
//! gain — the set is closed, the strings are the wire vocabulary.
//!
//! The per-format request helpers live in the `formats/` directory beside this
//! file; Rust resolves `pub mod claude;` here to `formats/claude.rs`.

pub mod claude;
pub mod gemini;
pub mod max_tokens;
pub mod openai;
pub mod responses_api;

/// The wire vocabulary: the shared concerns branch on these strings.
///
/// `gemini`, `gemini-cli` and `vertex` are retained for the Gemini-family
/// detection and usage-tracking branches; no provider in the registry serves
/// them.
pub const OPENAI: &str = "openai";
pub const OPENAI_RESPONSES: &str = "openai-responses";
pub const OPENAI_RESPONSE: &str = "openai-response";
pub const CLAUDE: &str = "claude";
pub const GEMINI: &str = "gemini";
pub const GEMINI_CLI: &str = "gemini-cli";
pub const VERTEX: &str = "vertex";
pub const CODEX: &str = "codex";
pub const KIRO: &str = "kiro";
pub const CURSOR: &str = "cursor";
pub const COMMANDCODE: &str = "commandcode";

/// Every format constant, for exhaustive checks and tests.
pub const ALL: [&str; 11] = [
    OPENAI,
    OPENAI_RESPONSES,
    OPENAI_RESPONSE,
    CLAUDE,
    GEMINI,
    GEMINI_CLI,
    VERTEX,
    CODEX,
    KIRO,
    CURSOR,
    COMMANDCODE,
];

/// `detectFormatByEndpoint(pathname, body)`: `None` means "fall back to
/// body-based detection".
///
/// The `/v1/chat/completions` case is not a mistake: the Cursor CLI posts a
/// Responses-shaped body to the chat endpoint, so an `input[]` array there is
/// read as OpenAI chat, not Responses.
pub fn detect_format_by_endpoint(
    pathname: &str,
    body: Option<&serde_json::Value>,
) -> Option<&'static str> {
    if pathname.contains("/v1/responses") {
        return Some(OPENAI_RESPONSES);
    }
    if pathname.contains("/v1/messages") {
        return Some(CLAUDE);
    }
    if pathname.contains("/v1/chat/completions")
        && body
            .and_then(|b| b.get("input"))
            .is_some_and(serde_json::Value::is_array)
    {
        return Some(OPENAI);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_detection_covers_each_route() {
        assert_eq!(
            detect_format_by_endpoint("/v1/responses", None),
            Some(OPENAI_RESPONSES)
        );
        assert_eq!(
            detect_format_by_endpoint("/v1/messages", None),
            Some(CLAUDE)
        );
        assert_eq!(
            detect_format_by_endpoint("/v1/chat/completions", Some(&json!({"input": []}))),
            Some(OPENAI),
            "cursor CLI posts a Responses body to the chat endpoint"
        );
        // A chat body without `input` falls through to body detection.
        assert_eq!(
            detect_format_by_endpoint("/v1/chat/completions", Some(&json!({"messages": []}))),
            None
        );
        assert_eq!(detect_format_by_endpoint("/v1/embeddings", None), None);
    }
}
