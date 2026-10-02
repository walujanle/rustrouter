//! `finish_reason` / `stop_reason` mapping.

use crate::translator::formats;
use crate::translator::schema::{claude_stop, openai_finish};

/// Map a provider finish reason to its OpenAI spelling. `None` and the empty
/// string both fall to `"stop"`.
pub fn to_openai_finish(reason: Option<&str>, format: &str) -> String {
    let reason = reason.filter(|r| !r.is_empty());
    match format {
        formats::CLAUDE => match reason {
            Some(claude_stop::END_TURN) => openai_finish::STOP.into(),
            Some(claude_stop::MAX_TOKENS) => openai_finish::LENGTH.into(),
            Some(claude_stop::TOOL_USE) => openai_finish::TOOL_CALLS.into(),
            Some(claude_stop::STOP_SEQUENCE) => openai_finish::STOP.into(),
            // A refusal is a blocked turn, not a clean stop. Mapping it to
            // "stop" left an OpenAI client unable to tell it from an empty answer.
            Some(claude_stop::REFUSAL) => openai_finish::CONTENT_FILTER.into(),
            _ => openai_finish::STOP.into(),
        },
        formats::COMMANDCODE => match reason {
            Some("stop") => openai_finish::STOP.into(),
            Some("length") => openai_finish::LENGTH.into(),
            Some("tool-calls") | Some("tool_use") => openai_finish::TOOL_CALLS.into(),
            Some("content-filter") => openai_finish::CONTENT_FILTER.into(),
            Some("error") => openai_finish::STOP.into(),
            Some(other) => other.to_string(),
            None => openai_finish::STOP.into(),
        },
        _ => reason.unwrap_or(openai_finish::STOP).to_string(),
    }
}

/// Map an OpenAI finish reason to the target format's spelling.
pub fn from_openai_finish(reason: Option<&str>, format: &str) -> String {
    let reason = reason.filter(|r| !r.is_empty());
    match format {
        formats::CLAUDE => match reason {
            Some(openai_finish::STOP) => claude_stop::END_TURN.into(),
            Some(openai_finish::LENGTH) => claude_stop::MAX_TOKENS.into(),
            Some(openai_finish::TOOL_CALLS) => claude_stop::TOOL_USE.into(),
            Some(openai_finish::CONTENT_FILTER) => claude_stop::REFUSAL.into(),
            _ => claude_stop::END_TURN.into(),
        },
        _ => reason.unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_refusal_maps_to_content_filter() {
        assert_eq!(to_openai_finish(Some("end_turn"), formats::CLAUDE), "stop");
        assert_eq!(
            to_openai_finish(Some("max_tokens"), formats::CLAUDE),
            "length"
        );
        assert_eq!(
            to_openai_finish(Some("tool_use"), formats::CLAUDE),
            "tool_calls"
        );
        assert_eq!(
            to_openai_finish(Some("stop_sequence"), formats::CLAUDE),
            "stop"
        );
        assert_eq!(
            to_openai_finish(Some("refusal"), formats::CLAUDE),
            "content_filter"
        );
        // Unknown and absent both land on stop.
        assert_eq!(to_openai_finish(None, formats::CLAUDE), "stop");
        assert_eq!(to_openai_finish(Some("bogus"), formats::CLAUDE), "stop");
    }

    #[test]
    fn commandcode_carries_its_own_spellings() {
        assert_eq!(
            to_openai_finish(Some("tool-calls"), formats::COMMANDCODE),
            "tool_calls"
        );
        assert_eq!(
            to_openai_finish(Some("content-filter"), formats::COMMANDCODE),
            "content_filter"
        );
        // An unmapped commandcode reason passes through verbatim.
        assert_eq!(
            to_openai_finish(Some("weird"), formats::COMMANDCODE),
            "weird"
        );
    }

    #[test]
    fn the_default_format_passes_the_reason_through() {
        assert_eq!(to_openai_finish(Some("stop"), formats::OPENAI), "stop");
        assert_eq!(to_openai_finish(Some("length"), formats::OPENAI), "length");
        assert_eq!(to_openai_finish(None, formats::OPENAI), "stop");
    }

    #[test]
    fn reverse_mapping_only_specialises_claude() {
        assert_eq!(
            from_openai_finish(Some("stop"), formats::CLAUDE),
            "end_turn"
        );
        assert_eq!(
            from_openai_finish(Some("length"), formats::CLAUDE),
            "max_tokens"
        );
        assert_eq!(
            from_openai_finish(Some("tool_calls"), formats::CLAUDE),
            "tool_use"
        );
        assert_eq!(
            from_openai_finish(Some("content_filter"), formats::CLAUDE),
            "refusal"
        );
        assert_eq!(from_openai_finish(Some("stop"), formats::OPENAI), "stop");
    }
}
