//! Gemini text helpers.
//!
//! Only `extract_text_content` survives here: it is used by the combo text
//! strategy and the OpenAI→Claude system-prompt builder.

use serde_json::Value;

use crate::translator::schema::openai_block;

/// `extractTextContent(content, separator)`.
pub fn extract_text_content(content: &Value, separator: &str) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some(openai_block::TEXT))
            .map(|c| c.get("text").and_then(Value::as_str).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(separator),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_extraction_joins_only_text_parts() {
        assert_eq!(extract_text_content(&json!("hi"), ""), "hi");
        assert_eq!(
            extract_text_content(
                &json!([{"type": "text", "text": "a"}, {"type": "image_url"}, {"type": "text", "text": "b"}]),
                "\n"
            ),
            "a\nb"
        );
        assert_eq!(extract_text_content(&json!({"x": 1}), ""), "");
    }
}
