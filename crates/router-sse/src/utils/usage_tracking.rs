//! Extract, normalise, canonicalise and estimate token usage.
//!
//! Two things here are easy to get wrong and both cost money or context:
//!
//! * **Anthropic splits usage across two events.** `message_start` carries real
//!   input and cache counts with a placeholder output, `message_delta` carries
//!   the real cumulative output with input absent. Overwriting instead of
//!   max-merging loses the cache counts on every request.
//! * **Canonicalisation must be idempotent.** Claude reports a prompt that
//!   *excludes* cache, so the cache counts are folded into `prompt_tokens`;
//!   OpenAI and Gemini already include them. The discriminator is the presence
//!   of `cached_tokens` in the input, which the folded output always sets — so
//!   re-running the fold on its own output takes the passthrough branch.

use serde_json::{Map, Value, json};

use crate::translator::formats;

/// `BUFFER_TOKENS`: headroom added to estimated usage so a client's own
/// accounting does not trip its context limit.
pub const BUFFER_TOKENS: i64 = 2000;

/// `addBufferToUsage(usage)`.
pub fn add_buffer_to_usage(usage: &Value) -> Value {
    let Some(map) = usage.as_object() else {
        return usage.clone();
    };
    let mut result = map.clone();

    let bump = |map: &mut Map<String, Value>, key: &str| {
        if let Some(value) = map.get(key).and_then(Value::as_i64) {
            map.insert(key.to_string(), json!(value + BUFFER_TOKENS));
        }
    };
    bump(&mut result, "input_tokens");
    bump(&mut result, "prompt_tokens");

    if result.get("total_tokens").and_then(Value::as_i64).is_some() {
        bump(&mut result, "total_tokens");
    } else if let (Some(prompt), Some(completion)) = (
        result.get("prompt_tokens").and_then(Value::as_i64),
        result.get("completion_tokens").and_then(Value::as_i64),
    ) {
        result.insert("total_tokens".to_string(), json!(prompt + completion));
    }

    Value::Object(result)
}

/// `filterUsageForFormat(usage, targetFormat)`.
pub fn filter_usage_for_format(usage: &Value, target_format: &str) -> Value {
    let Some(map) = usage.as_object() else {
        return usage.clone();
    };

    const CLAUDE_FIELDS: [&str; 5] = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
        "estimated",
    ];
    const GEMINI_FIELDS: [&str; 6] = [
        "promptTokenCount",
        "candidatesTokenCount",
        "totalTokenCount",
        "cachedContentTokenCount",
        "thoughtsTokenCount",
        "estimated",
    ];
    const RESPONSES_FIELDS: [&str; 5] = [
        "input_tokens",
        "output_tokens",
        "input_tokens_details",
        "output_tokens_details",
        "estimated",
    ];
    const OPENAI_FIELDS: [&str; 8] = [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "cached_tokens",
        "reasoning_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
        "estimated",
    ];

    let fields: &[&str] = match target_format {
        formats::CLAUDE => &CLAUDE_FIELDS,
        formats::GEMINI => &GEMINI_FIELDS,
        formats::OPENAI_RESPONSES | formats::OPENAI_RESPONSE => &RESPONSES_FIELDS,
        _ => &OPENAI_FIELDS,
    };

    let mut filtered = Map::new();
    for field in fields {
        if let Some(value) = map.get(*field) {
            filtered.insert((*field).to_string(), value.clone());
        }
    }
    Value::Object(filtered)
}

/// `normalizeUsage(usage)`: every value coerced to a finite number, `None` when
/// nothing survives.
pub fn normalize_usage(usage: Option<&Value>) -> Option<Value> {
    let map = usage?.as_object()?;

    let mut normalized = Map::new();
    let mut assign = |key: &str, value: Option<&Value>| {
        let Some(value) = value else {
            return;
        };
        if value.is_null() {
            return;
        }
        let numeric = match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        };
        if let Some(numeric) = numeric.filter(|n| n.is_finite()) {
            normalized.insert(key.to_string(), json!(numeric));
        }
    };

    assign("prompt_tokens", map.get("prompt_tokens"));
    assign("completion_tokens", map.get("completion_tokens"));
    assign("total_tokens", map.get("total_tokens"));
    assign(
        "cache_read_input_tokens",
        map.get("cache_read_input_tokens"),
    );
    assign(
        "cache_creation_input_tokens",
        map.get("cache_creation_input_tokens"),
    );
    assign("cached_tokens", map.get("cached_tokens"));
    assign("reasoning_tokens", map.get("reasoning_tokens"));

    // Nested details objects are forwarded verbatim.
    for key in ["prompt_tokens_details", "completion_tokens_details"] {
        if let Some(value) = map.get(key).filter(|v| v.is_object()) {
            normalized.insert(key.to_string(), value.clone());
        }
    }

    if normalized.is_empty() {
        return None;
    }
    Some(Value::Object(normalized))
}

/// `canonicalizeUsage(usage)`.
///
/// Output convention: `prompt_tokens` is the **total input including cache**,
/// `cached_tokens` is the cache-read subset, `cache_creation_input_tokens` the
/// cache-write subset, and `total_tokens` is recomputed rather than trusted.
pub fn canonicalize_usage(usage: Option<&Value>) -> Option<Value> {
    let map = usage?.as_object()?;
    let num = |v: Option<&Value>| -> i64 {
        match v {
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
            _ => 0,
        }
    };

    let completion = num(map
        .get("completion_tokens")
        .or_else(|| map.get("output_tokens")));
    let reasoning = num(map.get("reasoning_tokens"));
    let cache_creation = num(map.get("cache_creation_input_tokens").or_else(|| {
        map.get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_tokens"))
    }));

    let mut prompt = num(map.get("prompt_tokens").or_else(|| map.get("input_tokens")));
    let cached;

    // The `cached_tokens === undefined` guard is what makes the fold
    // idempotent: canonical output always sets that key, so a second pass takes
    // the passthrough branch instead of folding the cache in twice.
    if map.get("cached_tokens").is_none()
        && (map.get("cache_read_input_tokens").is_some()
            || map.get("cache_creation_input_tokens").is_some())
    {
        cached = num(map.get("cache_read_input_tokens"));
        prompt += cached + cache_creation;
    } else {
        cached = num(map.get("cached_tokens").or_else(|| {
            map.get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
        }));
    }

    let mut result = Map::new();
    result.insert("prompt_tokens".into(), json!(prompt));
    result.insert("completion_tokens".into(), json!(completion));
    // Recomputed, not passed through: an upstream total would be stale after
    // the fold.
    result.insert("total_tokens".into(), json!(prompt + completion));
    result.insert("cached_tokens".into(), json!(cached));
    result.insert("cache_creation_input_tokens".into(), json!(cache_creation));
    if reasoning > 0 {
        result.insert("reasoning_tokens".into(), json!(reasoning));
    }
    Some(Value::Object(result))
}

/// `hasValidUsage(usage)`: at least one known token field above zero.
pub fn has_valid_usage(usage: Option<&Value>) -> bool {
    let Some(map) = usage.and_then(Value::as_object) else {
        return false;
    };
    const TOKEN_FIELDS: [&str; 6] = [
        "prompt_tokens",
        "completion_tokens",
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "promptTokenCount",
    ];
    const GEMINI_FIELDS: [&str; 1] = ["candidatesTokenCount"];
    TOKEN_FIELDS
        .iter()
        .chain(GEMINI_FIELDS.iter())
        .any(|field| {
            map.get(*field)
                .and_then(Value::as_f64)
                .is_some_and(|n| n > 0.0)
        })
}

/// `extractUsage(chunk)`: whichever of the five wire shapes this chunk is.
pub fn extract_usage(chunk: Option<&Value>) -> Option<Value> {
    let chunk = chunk?;
    if !chunk.is_object() {
        return None;
    }
    let kind = chunk.get("type").and_then(Value::as_str);

    // Claude `message_start`: input and cache are real, output is a placeholder.
    if kind == Some("message_start")
        && let Some(usage) = chunk
            .get("message")
            .and_then(|m| m.get("usage"))
            .filter(|u| u.is_object())
    {
        return normalize_usage(Some(&json!({
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "cache_read_input_tokens": usage.get("cache_read_input_tokens").cloned().unwrap_or(Value::Null),
            "cache_creation_input_tokens": usage.get("cache_creation_input_tokens").cloned().unwrap_or(Value::Null),
        })));
    }

    // Claude `message_delta`: the real cumulative output.
    if kind == Some("message_delta")
        && let Some(usage) = chunk.get("usage").filter(|u| u.is_object())
    {
        return normalize_usage(Some(&json!({
            "prompt_tokens": usage.get("input_tokens").cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").cloned().unwrap_or(json!(0)),
            "cache_read_input_tokens": usage.get("cache_read_input_tokens").cloned().unwrap_or(Value::Null),
            "cache_creation_input_tokens": usage.get("cache_creation_input_tokens").cloned().unwrap_or(Value::Null),
        })));
    }

    // OpenAI Responses.
    if matches!(kind, Some("response.completed") | Some("response.done"))
        && let Some(usage) = chunk
            .get("response")
            .and_then(|r| r.get("usage"))
            .filter(|u| u.is_object())
    {
        let cached = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"));
        return normalize_usage(Some(&json!({
            "prompt_tokens": usage.get("input_tokens").or_else(|| usage.get("prompt_tokens")).cloned().unwrap_or(json!(0)),
            "completion_tokens": usage.get("output_tokens").or_else(|| usage.get("completion_tokens")).cloned().unwrap_or(json!(0)),
            "cached_tokens": cached.cloned().unwrap_or(Value::Null),
            "reasoning_tokens": usage.get("output_tokens_details").and_then(|d| d.get("reasoning_tokens")).cloned().unwrap_or(Value::Null),
            "prompt_tokens_details": if cached.is_some() { json!({ "cached_tokens": cached }) } else { Value::Null },
        })));
    }

    // OpenAI — also covers DeepSeek's `prompt_cache_hit_tokens`.
    if let Some(usage) = chunk.get("usage").filter(|u| u.is_object())
        && usage.get("prompt_tokens").is_some()
    {
        let cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .or_else(|| usage.get("prompt_cache_hit_tokens"));
        return normalize_usage(Some(&json!({
            "prompt_tokens": usage.get("prompt_tokens"),
            "completion_tokens": usage.get("completion_tokens").cloned().unwrap_or(json!(0)),
            "cached_tokens": cached.cloned().unwrap_or(Value::Null),
            "reasoning_tokens": usage.get("completion_tokens_details").and_then(|d| d.get("reasoning_tokens")).cloned().unwrap_or(Value::Null),
            "prompt_tokens_details": usage.get("prompt_tokens_details").cloned().unwrap_or(Value::Null),
            "completion_tokens_details": usage.get("completion_tokens_details").cloned().unwrap_or(Value::Null),
        })));
    }

    // Gemini, with the gemini-cli `{response:{…}}` wrapper.
    if let Some(meta) = chunk
        .get("usageMetadata")
        .or_else(|| chunk.get("response").and_then(|r| r.get("usageMetadata")))
        && meta.is_object()
    {
        return normalize_usage(Some(&json!({
            "prompt_tokens": meta.get("promptTokenCount").cloned().unwrap_or(json!(0)),
            "completion_tokens": meta.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),
            "total_tokens": meta.get("totalTokenCount").cloned().unwrap_or(Value::Null),
            "cached_tokens": meta.get("cachedContentTokenCount").cloned().unwrap_or(Value::Null),
            "reasoning_tokens": meta.get("thoughtsTokenCount").cloned().unwrap_or(Value::Null),
        })));
    }

    None
}

/// `mergeUsage(prev, next)`: field-wise max, so a placeholder never clobbers a
/// real count.
///
/// NaN is guarded for: `Math.max(x, NaN)` is NaN, and one malformed chunk would
/// poison the whole accumulation.
pub fn merge_usage(prev: Option<Value>, next: Option<Value>) -> Option<Value> {
    let Some(prev) = prev else {
        return next;
    };
    let Some(next) = next else {
        return Some(prev);
    };
    let (Some(prev_map), Some(next_map)) = (prev.as_object(), next.as_object()) else {
        return Some(prev);
    };

    let mut merged = prev_map.clone();
    for (key, value) in next_map {
        if let Some(number) = value.as_f64().filter(|n| n.is_finite()) {
            let existing = merged
                .get(key)
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .unwrap_or(0.0);
            merged.insert(key.clone(), json!(existing.max(number)));
        } else if value.is_object() {
            // Nested details objects take the latest wholesale.
            merged.insert(key.clone(), value.clone());
        }
    }
    Some(Value::Object(merged))
}

/// `estimateInputTokens(body)`: about four characters per token.
pub fn estimate_input_tokens(body: &Value) -> i64 {
    if !body.is_object() {
        return 0;
    }
    match serde_json::to_string(body) {
        Ok(text) => (text.chars().count() as f64 / 4.0).ceil() as i64,
        Err(_) => 0,
    }
}

/// `estimateOutputTokens(contentLength)`.
pub fn estimate_output_tokens(content_length: i64) -> i64 {
    if content_length <= 0 {
        return 0;
    }
    (content_length / 4).max(1)
}

/// `formatUsage(inputTokens, outputTokens, targetFormat)`.
pub fn format_usage(input_tokens: i64, output_tokens: i64, target_format: &str) -> Value {
    if target_format == formats::CLAUDE {
        return add_buffer_to_usage(&json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "estimated": true,
        }));
    }
    add_buffer_to_usage(&json!({
        "prompt_tokens": input_tokens,
        "completion_tokens": output_tokens,
        "total_tokens": input_tokens + output_tokens,
        "estimated": true,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_is_added_to_every_present_token_field() {
        let out = add_buffer_to_usage(
            &json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}),
        );
        assert_eq!(out["prompt_tokens"], json!(10 + BUFFER_TOKENS));
        assert_eq!(out["completion_tokens"], json!(5));
        assert_eq!(out["total_tokens"], json!(15 + BUFFER_TOKENS));

        // A missing total is computed from the *bumped* prompt.
        let out = add_buffer_to_usage(&json!({"prompt_tokens": 10, "completion_tokens": 5}));
        assert_eq!(out["total_tokens"], json!(10 + BUFFER_TOKENS + 5));

        // Claude's shape.
        let out = add_buffer_to_usage(&json!({"input_tokens": 10, "output_tokens": 5}));
        assert_eq!(out["input_tokens"], json!(10 + BUFFER_TOKENS));
        assert!(out.get("total_tokens").is_none());
    }

    #[test]
    fn the_buffer_leaves_non_objects_alone() {
        assert_eq!(add_buffer_to_usage(&json!("text")), json!("text"));
        assert_eq!(add_buffer_to_usage(&Value::Null), Value::Null);
    }

    #[test]
    fn filtering_picks_the_format_s_fields() {
        let usage = json!({
            "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3,
            "cached_tokens": 4, "reasoning_tokens": 5,
            "input_tokens": 6, "output_tokens": 7,
            "cache_read_input_tokens": 8, "cache_creation_input_tokens": 9,
            "promptTokenCount": 10, "candidatesTokenCount": 11, "totalTokenCount": 12,
            "cachedContentTokenCount": 13, "thoughtsTokenCount": 14,
            "estimated": true,
        });
        let claude = filter_usage_for_format(&usage, formats::CLAUDE);
        assert_eq!(claude["input_tokens"], json!(6));
        assert!(claude.get("prompt_tokens").is_none());

        let filtered = filter_usage_for_format(&usage, formats::GEMINI);
        assert_eq!(filtered["promptTokenCount"], json!(10));
        assert!(filtered.get("prompt_tokens").is_none());

        // Responses keeps its nested details.
        let responses = filter_usage_for_format(
            &json!({"input_tokens": 1, "prompt_tokens": 99}),
            formats::OPENAI_RESPONSES,
        );
        assert_eq!(responses["input_tokens"], json!(1));
        assert!(responses.get("prompt_tokens").is_none());

        // Anything else gets the OpenAI shape.
        let openai = filter_usage_for_format(&usage, formats::OPENAI);
        assert_eq!(openai["prompt_tokens"], json!(1));
        assert!(openai.get("input_tokens").is_none());
    }

    #[test]
    fn normalisation_coerces_and_drops_junk() {
        let out = normalize_usage(Some(&json!({
            "prompt_tokens": "12",
            "completion_tokens": null,
            "total_tokens": 3.5,
            "reasoning_tokens": "not a number",
            "cached_tokens": true,
        })))
        .unwrap();
        assert_eq!(out["prompt_tokens"], json!(12.0));
        assert_eq!(out["total_tokens"], json!(3.5));
        assert!(out.get("completion_tokens").is_none(), "null is dropped");
        assert!(out.get("reasoning_tokens").is_none());
        assert_eq!(out["cached_tokens"], json!(1.0));
    }

    #[test]
    fn normalisation_returns_none_when_nothing_survives() {
        assert!(normalize_usage(Some(&json!({}))).is_none());
        assert!(normalize_usage(Some(&json!({"unknown": 1}))).is_none());
        assert!(normalize_usage(None).is_none());
        assert!(normalize_usage(Some(&json!([1, 2]))).is_none());
    }

    #[test]
    fn canonicalisation_folds_the_claude_cache_into_the_prompt() {
        let out = canonicalize_usage(Some(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 5,
        })))
        .unwrap();
        assert_eq!(out["prompt_tokens"], json!(135));
        assert_eq!(out["cached_tokens"], json!(30));
        assert_eq!(out["cache_creation_input_tokens"], json!(5));
        assert_eq!(out["total_tokens"], json!(155));
    }

    #[test]
    fn a_first_write_with_no_cache_read_still_folds() {
        let out = canonicalize_usage(Some(&json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 40,
        })))
        .unwrap();
        assert_eq!(out["prompt_tokens"], json!(140));
        assert_eq!(out["cached_tokens"], json!(0));
        assert_eq!(out["completion_tokens"], json!(20));
    }

    #[test]
    fn canonicalisation_is_idempotent() {
        let first = canonicalize_usage(Some(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 5,
        })))
        .unwrap();
        let second = canonicalize_usage(Some(&first)).unwrap();
        assert_eq!(first, second, "a second pass must not fold again");
    }

    #[test]
    fn the_openai_path_passes_the_prompt_through() {
        let out = canonicalize_usage(Some(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cached_tokens": 30,
        })))
        .unwrap();
        assert_eq!(
            out["prompt_tokens"],
            json!(100),
            "the cache is already inside"
        );
        assert_eq!(out["cached_tokens"], json!(30));
    }

    #[test]
    fn a_nested_details_object_is_read_when_the_top_level_field_is_absent() {
        let out = canonicalize_usage(Some(&json!({
            "prompt_tokens": 100,
            "prompt_tokens_details": {"cached_tokens": 30, "cache_creation_tokens": 7},
        })))
        .unwrap();
        assert_eq!(out["cached_tokens"], json!(30));
        assert_eq!(out["cache_creation_input_tokens"], json!(7));
    }

    #[test]
    fn reasoning_survives_only_when_non_zero() {
        assert_eq!(
            canonicalize_usage(Some(&json!({"prompt_tokens": 1, "reasoning_tokens": 9}))).unwrap()
                ["reasoning_tokens"],
            json!(9)
        );
        assert!(
            canonicalize_usage(Some(&json!({"prompt_tokens": 1})))
                .unwrap()
                .get("reasoning_tokens")
                .is_none()
        );
    }

    #[test]
    fn valid_usage_needs_a_positive_known_field() {
        assert!(has_valid_usage(Some(&json!({"prompt_tokens": 1}))));
        assert!(has_valid_usage(Some(&json!({"promptTokenCount": 5}))));
        assert!(!has_valid_usage(Some(&json!({"prompt_tokens": 0}))));
        assert!(!has_valid_usage(Some(&json!({"unknown": 100}))));
        assert!(!has_valid_usage(Some(&json!({}))));
        assert!(!has_valid_usage(None));
    }

    #[test]
    fn a_claude_message_start_is_extracted() {
        let chunk = json!({"type": "message_start", "message": {"usage": {
            "input_tokens": 100, "output_tokens": 1,
            "cache_read_input_tokens": 30, "cache_creation_input_tokens": 5,
        }}});
        let out = extract_usage(Some(&chunk)).unwrap();
        assert_eq!(out["prompt_tokens"], json!(100.0));
        assert_eq!(out["completion_tokens"], json!(1.0));
        assert_eq!(out["cache_read_input_tokens"], json!(30.0));
        assert_eq!(out["cache_creation_input_tokens"], json!(5.0));
    }

    #[test]
    fn merging_takes_the_max_so_the_placeholder_never_wins() {
        let start = extract_usage(Some(
            &json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 100, "output_tokens": 1, "cache_read_input_tokens": 30,
            }}}),
        ));
        let delta = extract_usage(Some(
            &json!({"type": "message_delta", "usage": {"output_tokens": 250}}),
        ));
        let merged = merge_usage(start, delta).unwrap();
        // The placeholder 1 loses to the real 250, and the input survives.
        assert_eq!(merged["completion_tokens"], json!(250.0));
        assert_eq!(merged["prompt_tokens"], json!(100.0));
        assert_eq!(merged["cache_read_input_tokens"], json!(30.0));
    }

    #[test]
    fn merging_is_idempotent_for_a_single_complete_object() {
        let usage = extract_usage(Some(
            &json!({"usage": {"prompt_tokens": 10, "completion_tokens": 2}}),
        ));
        let once = merge_usage(None, usage.clone()).unwrap();
        let twice = merge_usage(Some(once.clone()), usage).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn merging_tolerates_a_missing_side() {
        let usage = json!({"prompt_tokens": 10});
        assert_eq!(merge_usage(None, Some(usage.clone())), Some(usage.clone()));
        assert_eq!(merge_usage(Some(usage.clone()), None), Some(usage));
        assert_eq!(merge_usage(None, None), None);
    }

    #[test]
    fn a_non_finite_value_cannot_poison_the_merge() {
        let prev = Some(json!({"prompt_tokens": 10}));
        let next = Some(json!({"prompt_tokens": "NaN"}));
        let merged = merge_usage(prev, next).unwrap();
        assert_eq!(merged["prompt_tokens"], json!(10));
    }

    #[test]
    fn the_openai_usage_shape_is_extracted_including_deepseek() {
        let out = extract_usage(Some(&json!({"usage": {
            "prompt_tokens": 10, "completion_tokens": 2, "prompt_cache_hit_tokens": 4,
        }})))
        .unwrap();
        assert_eq!(out["prompt_tokens"], json!(10.0));
        assert_eq!(out["cached_tokens"], json!(4.0));
    }

    #[test]
    fn a_responses_completion_is_extracted() {
        let out = extract_usage(Some(
            &json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 10, "output_tokens": 2,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens_details": {"reasoning_tokens": 1},
            }}}),
        ))
        .unwrap();
        assert_eq!(out["prompt_tokens"], json!(10.0));
        assert_eq!(out["completion_tokens"], json!(2.0));
        assert_eq!(out["cached_tokens"], json!(3.0));
        assert_eq!(out["reasoning_tokens"], json!(1.0));
        assert_eq!(out["prompt_tokens_details"]["cached_tokens"], json!(3));
    }

    #[test]
    fn a_gemini_usage_metadata_is_extracted_from_both_nesting_levels() {
        let flat = extract_usage(Some(&json!({"usageMetadata": {
            "promptTokenCount": 10, "candidatesTokenCount": 2, "totalTokenCount": 12,
            "cachedContentTokenCount": 3, "thoughtsTokenCount": 1,
        }})))
        .unwrap();
        assert_eq!(flat["prompt_tokens"], json!(10.0));
        assert_eq!(flat["total_tokens"], json!(12.0));
        assert_eq!(flat["reasoning_tokens"], json!(1.0));

        let nested = extract_usage(Some(
            &json!({"response": {"usageMetadata": {"promptTokenCount": 10}}}),
        ))
        .unwrap();
        assert_eq!(nested["prompt_tokens"], json!(10.0));
    }

    #[test]
    fn chunks_with_no_usage_yield_none() {
        assert!(extract_usage(None).is_none());
        assert!(extract_usage(Some(&json!("text"))).is_none());
        assert!(extract_usage(Some(&json!({"choices": []}))).is_none());
    }

    #[test]
    fn estimation_uses_the_body_size_and_a_floor_of_one() {
        let body = json!({"messages": [{"role": "user", "content": "12345678"}]});
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(
            estimate_input_tokens(&body),
            (text.chars().count() as f64 / 4.0).ceil() as i64
        );
        assert_eq!(estimate_input_tokens(&json!("text")), 0);

        assert_eq!(estimate_output_tokens(0), 0);
        assert_eq!(estimate_output_tokens(1), 1);
        assert_eq!(estimate_output_tokens(40), 10);
    }

    #[test]
    fn format_usage_emits_the_target_shape_with_the_buffer() {
        let claude = format_usage(100, 20, formats::CLAUDE);
        assert_eq!(claude["input_tokens"], json!(100 + BUFFER_TOKENS));
        assert_eq!(claude["output_tokens"], json!(20));
        assert_eq!(claude["estimated"], json!(true));
        assert!(claude.get("prompt_tokens").is_none());

        let openai = format_usage(100, 20, formats::OPENAI);
        assert_eq!(openai["prompt_tokens"], json!(100 + BUFFER_TOKENS));
        assert_eq!(openai["completion_tokens"], json!(20));
        // `total_tokens` is bumped once, not per component: the buffer is added
        // to the total it was handed, not recomputed.
        assert_eq!(openai["total_tokens"], json!(100 + 20 + BUFFER_TOKENS));
    }

    #[test]
    fn the_two_estimates_compose_into_a_usage_body() {
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let out = format_usage(
            estimate_input_tokens(&body),
            estimate_output_tokens(40),
            formats::OPENAI,
        );
        assert_eq!(out["completion_tokens"], json!(10));
        assert_eq!(out["estimated"], json!(true));
        assert!(out["prompt_tokens"].as_i64().unwrap() > BUFFER_TOKENS);
    }
}
