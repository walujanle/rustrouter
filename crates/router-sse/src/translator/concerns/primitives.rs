//! Small shared translator helpers.

use serde_json::{Map, Value, json};

use crate::translator::schema::{openai_block, role};

/// `buildChunk({id, created, model}, delta, finishReason)`.
///
/// The key order is the wire order (`id, object, created, model, choices`) and
/// the caller supplies `id`/`created` so each translator keeps its own
/// id-generation semantics. `finish_reason` is always present, `null` when the
/// caller passes none — the key is emitted, never omitted.
pub fn build_chunk(
    id: &Value,
    created: &Value,
    model: &Value,
    delta: Value,
    finish_reason: Option<&str>,
) -> Value {
    let mut choice = Map::new();
    choice.insert("index".into(), json!(0));
    choice.insert("delta".into(), delta);
    choice.insert(
        "finish_reason".into(),
        finish_reason.map_or(Value::Null, |r| Value::String(r.to_string())),
    );

    let mut chunk = Map::new();
    chunk.insert("id".into(), id.clone());
    chunk.insert("object".into(), json!("chat.completion.chunk"));
    chunk.insert("created".into(), created.clone());
    chunk.insert("model".into(), model.clone());
    chunk.insert("choices".into(), Value::Array(vec![Value::Object(choice)]));
    Value::Object(chunk)
}

/// `safeParseJSON(str, fallback)`: a non-string passes through untouched.
pub fn safe_parse_json(value: Value, fallback: Value) -> Value {
    match value {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(fallback),
        other => other,
    }
}

/// JS truthiness: `null`, `undefined`, `false`, `0`, `NaN` and `""` are falsy;
/// `{}` and `[]` are truthy.
///
/// This is the single most common source of a silent parity break: Rust's
/// `Option::is_some` and `!v.is_null()` both disagree with `if (x)` on the
/// empty string and on zero.
pub fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `js_truthy` over an absent value, which JS reads as `undefined` (falsy).
pub fn js_truthy_opt(value: Option<&Value>) -> bool {
    value.is_some_and(js_truthy)
}

/// `a ?? b`: fall through only on `null`/absent, never on `""` or `0`.
pub fn js_nullish<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(v) if !v.is_null() => Some(v),
        _ => b,
    }
}

/// `String(value)`.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `String(value || "")` — the `||` runs first, so every falsy value is `""`.
pub fn js_string_or_empty(value: &Value) -> String {
    if js_truthy(value) {
        js_string(value)
    } else {
        String::new()
    }
}

/// `Number(value) || 0` for the token-count reads: non-numbers are zero.
pub fn js_number(value: Option<&Value>) -> i64 {
    value.and_then(Value::as_f64).unwrap_or(0.0) as i64
}

/// `JSON.stringify(n)`: JS has one number type, so a whole value loses its
/// decimal point. `json!(100.0f64)` would write `100.0` — a different byte
/// string from the expected `100`, and payloads here are byte-compared.
pub fn js_json_number(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

/// `collapseTextParts(parts)`: a lone text part collapses to its string.
pub fn collapse_text_parts(parts: Value) -> Value {
    if let Value::Array(items) = &parts
        && items.len() == 1
        && items[0].get("type").and_then(Value::as_str) == Some(openai_block::TEXT)
        && let Some(text) = items[0].get("text")
    {
        return text.clone();
    }
    parts
}

/// `reasoningDelta(text, withRole)`.
pub fn reasoning_delta(text: &str, with_role: bool) -> Value {
    let mut delta = Map::new();
    if with_role {
        delta.insert("role".into(), json!(role::ASSISTANT));
    }
    delta.insert("reasoning_content".into(), json!(text));
    Value::Object(delta)
}

/// `extractReasoningText(delta)`: `reasoning_content`, then `reasoning`, then
/// `reasoning_details[]`. Returns `""` when none.
pub fn extract_reasoning_text(delta: &Value) -> String {
    if let Some(s) = delta.get("reasoning_content").and_then(Value::as_str)
        && !s.is_empty()
    {
        return s.to_string();
    }
    if let Some(s) = delta.get("reasoning").and_then(Value::as_str)
        && !s.is_empty()
    {
        return s.to_string();
    }
    if let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) {
        return details
            .iter()
            .map(|d| {
                d.as_str()
                    .map(str::to_string)
                    .or_else(|| d.get("text").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| d.get("content").and_then(Value::as_str).map(str::to_string))
                    .unwrap_or_default()
            })
            .collect();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_truthy_matches_js_semantics() {
        assert!(js_truthy(&json!("x")));
        assert!(!js_truthy(&json!("")));
        assert!(js_truthy(&json!(1)));
        assert!(!js_truthy(&json!(0)));
        // `{}` and `[]` are truthy in JS, unlike Rust's `is_empty()`.
        assert!(js_truthy(&json!([1])));
        assert!(js_truthy(&json!([])));
        assert!(js_truthy(&json!({"a": 1})));
        assert!(js_truthy(&json!({})));
        assert!(!js_truthy_opt(None));
        assert!(!js_truthy_opt(Some(&Value::Null)));
    }

    #[test]
    fn chunk_keeps_the_wire_key_order() {
        let chunk = build_chunk(
            &json!("c1"),
            &json!(1),
            &json!("m"),
            json!({"content": "x"}),
            None,
        );
        let keys: Vec<&str> = chunk
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["id", "object", "created", "model", "choices"]);
        let choice = &chunk["choices"][0];
        assert_eq!(
            choice["finish_reason"],
            Value::Null,
            "the key is kept as null"
        );
        assert_eq!(choice["index"], json!(0));
    }

    #[test]
    fn safe_parse_passes_through_non_strings() {
        assert_eq!(
            safe_parse_json(json!({"a": 1}), json!(null)),
            json!({"a": 1})
        );
        assert_eq!(
            safe_parse_json(json!("{\"a\":1}"), json!(null)),
            json!({"a": 1})
        );
        assert_eq!(safe_parse_json(json!("nope"), json!("fb")), json!("fb"));
    }

    #[test]
    fn collapse_only_fires_on_a_single_text_part() {
        assert_eq!(
            collapse_text_parts(json!([{"type": "text", "text": "hi"}])),
            json!("hi")
        );
        assert_eq!(
            collapse_text_parts(json!([{"type": "image_url", "image_url": {}}])),
            json!([{"type": "image_url", "image_url": {}}])
        );
        assert_eq!(
            collapse_text_parts(
                json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
        );
    }

    #[test]
    fn reasoning_extraction_reads_each_vendor_shape() {
        assert_eq!(
            extract_reasoning_text(&json!({"reasoning_content": "a"})),
            "a"
        );
        assert_eq!(extract_reasoning_text(&json!({"reasoning": "b"})), "b");
        assert_eq!(
            extract_reasoning_text(
                &json!({"reasoning_details": [{"text": "c"}, "d", {"content": "e"}]})
            ),
            "cde"
        );
        assert_eq!(extract_reasoning_text(&json!({"content": "x"})), "");
    }

    #[test]
    fn reasoning_delta_can_lead_with_the_role() {
        assert_eq!(
            reasoning_delta("t", false),
            json!({"reasoning_content": "t"})
        );
        assert_eq!(
            reasoning_delta("t", true),
            json!({"role": "assistant", "reasoning_content": "t"})
        );
    }
}
