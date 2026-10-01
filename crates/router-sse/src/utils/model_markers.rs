//! Claude Code's 1M-context annotation.
//!
//! When the 1M-context beta is on, the client appends `[1m]` to the model name.
//! The marker is a client-side annotation, not part of any model id: it matches
//! no combo name, no alias and no `provider/model` pair, so a request carrying
//! it dies at model resolution with "Invalid model format". The capability
//! itself travels in the `anthropic-beta` header, which is forwarded untouched
//! — stripping the marker is enough for the request to route normally.

use serde_json::Value;

/// `stripModelContextMarker(modelStr)`.
pub struct StrippedModel<'a> {
    pub model: &'a str,
    /// The marker without its brackets, lower-cased: `"1m"`.
    pub context_marker: Option<String>,
}

pub fn strip_model_context_marker(model: &str) -> StrippedModel<'_> {
    let trimmed = model.trim();
    if trimmed.to_lowercase().ends_with("[1m]") {
        // Slice the last four *bytes*, not an index from the lowercased copy: a
        // Unicode case fold can change a string's byte length.
        return StrippedModel {
            model: &trimmed[..trimmed.len() - 4],
            context_marker: Some("1m".to_string()),
        };
    }
    StrippedModel {
        model,
        context_marker: None,
    }
}

/// The same, applied to a request body's `model` field in place. Returns the
/// stripped marker when there was one.
pub fn strip_model_marker_from_body(body: &mut Value) -> Option<String> {
    let model = body.get("model").and_then(Value::as_str)?;
    let stripped = strip_model_context_marker(model);
    let marker = stripped.context_marker.clone();
    if marker.is_some() {
        body["model"] = Value::String(stripped.model.to_string());
    }
    marker
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_marker_is_stripped_and_reported() {
        let stripped = strip_model_context_marker("claude-opus-5[1m]");
        assert_eq!(stripped.model, "claude-opus-5");
        assert_eq!(stripped.context_marker.as_deref(), Some("1m"));
    }

    #[test]
    fn the_marker_is_case_insensitive() {
        for model in ["claude-opus-5[1M]", "claude-opus-5[1m]"] {
            let stripped = strip_model_context_marker(model);
            assert_eq!(stripped.model, "claude-opus-5", "{model}");
            assert_eq!(stripped.context_marker.as_deref(), Some("1m"));
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_off_the_model() {
        let stripped = strip_model_context_marker("  claude-opus-5[1m]  ");
        assert_eq!(stripped.model, "claude-opus-5");
    }

    #[test]
    fn a_model_without_the_marker_is_untouched() {
        for model in ["claude-opus-5", "claude-opus-5[2m]", "gpt[1m]x", ""] {
            let stripped = strip_model_context_marker(model);
            assert_eq!(stripped.model, model, "{model}");
            assert!(stripped.context_marker.is_none(), "{model}");
        }
    }

    #[test]
    fn the_body_helper_rewrites_only_a_marked_model() {
        let mut body = json!({"model": "claude-opus-5[1m]", "stream": true});
        assert_eq!(
            strip_model_marker_from_body(&mut body).as_deref(),
            Some("1m")
        );
        assert_eq!(body["model"], json!("claude-opus-5"));
        assert_eq!(body["stream"], json!(true));

        let mut body = json!({"model": "claude-opus-5"});
        assert!(strip_model_marker_from_body(&mut body).is_none());
        assert_eq!(body["model"], json!("claude-opus-5"));

        // No model field at all is a no-op, not a panic.
        let mut body = json!({});
        assert!(strip_model_marker_from_body(&mut body).is_none());
    }
}
