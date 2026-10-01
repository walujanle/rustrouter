//! Usage extraction, the "done" summary line and the usage row.
//!
//! DB-free by construction. Saving a usage row is pure here —
//! [`save_usage_stats`] returns the canonicalised row and the server layer
//! persists it, the same way `services/token_refresh.rs` returns a patch instead
//! of writing.
//!
//! `undefined` has no `serde_json` representation, so the `JSON.stringify`
//! behaviour is reproduced explicitly: a key whose JS value would be
//! `undefined` is **omitted**, a key whose value is `null` is kept.

use serde_json::{Map, Value, json};

use crate::translator::concerns::primitives::{js_nullish, js_number, js_string, js_truthy};

/// `extractUsageFromResponse(responseBody)`.
///
/// A key left `undefined` is **absent** from the returned object (the caller
/// stringifies it), so `cached_tokens` and the cache counters appear only when
/// the provider reported them. `null` keys survive.
pub fn extract_usage_from_response(response_body: &Value) -> Option<Value> {
    if !(response_body.is_object() || response_body.is_array()) {
        return None;
    }

    fn count(value: Option<&Value>) -> i64 {
        js_number(value)
    }
    fn insert_if_some(out: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
        if let Some(value) = value {
            out.insert(key.to_string(), value.clone());
        }
    }
    let mut out = Map::new();

    // Claude shape. The Responses usage ({input_tokens, input_tokens_details})
    // shares `input_tokens`, so it lands here too.
    if let Some(usage) = response_body.get("usage")
        && usage.get("input_tokens").is_some()
    {
        out.insert(
            "prompt_tokens".into(),
            json!(count(usage.get("input_tokens"))),
        );
        out.insert(
            "completion_tokens".into(),
            json!(count(usage.get("output_tokens"))),
        );
        insert_if_some(
            &mut out,
            "cached_tokens",
            js_nullish(
                usage.get("cached_tokens"),
                usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens")),
            ),
        );
        insert_if_some(
            &mut out,
            "cache_read_input_tokens",
            usage.get("cache_read_input_tokens"),
        );
        insert_if_some(
            &mut out,
            "cache_creation_input_tokens",
            usage.get("cache_creation_input_tokens"),
        );
        return Some(Value::Object(out));
    }

    // OpenAI shape.
    if let Some(usage) = response_body.get("usage")
        && usage.get("prompt_tokens").is_some()
    {
        out.insert(
            "prompt_tokens".into(),
            json!(count(usage.get("prompt_tokens"))),
        );
        out.insert(
            "completion_tokens".into(),
            json!(count(usage.get("completion_tokens"))),
        );
        insert_if_some(
            &mut out,
            "cached_tokens",
            js_nullish(
                usage.get("cached_tokens"),
                usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens")),
            ),
        );
        insert_if_some(
            &mut out,
            "reasoning_tokens",
            usage
                .get("completion_tokens_details")
                .and_then(|d| d.get("reasoning_tokens")),
        );
        return Some(Value::Object(out));
    }

    // Gemini shape, with the gemini-cli `{response:{…}}` wrapper.
    let usage_metadata = response_body
        .get("usageMetadata")
        .filter(|v| js_truthy(v))
        .or_else(|| {
            response_body
                .get("response")
                .and_then(|r| r.get("usageMetadata"))
                .filter(|v| js_truthy(v))
        });
    if let Some(meta) = usage_metadata {
        out.insert(
            "prompt_tokens".into(),
            json!(count(meta.get("promptTokenCount"))),
        );
        out.insert(
            "completion_tokens".into(),
            json!(count(meta.get("candidatesTokenCount"))),
        );
        out.insert(
            "cached_tokens".into(),
            json!(count(meta.get("cachedContentTokenCount"))),
        );
        out.insert(
            "reasoning_tokens".into(),
            json!(count(meta.get("thoughtsTokenCount"))),
        );
        return Some(Value::Object(out));
    }

    None
}

/// `u[key]` on an optional usage object.
fn usage_get<'a>(usage: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    usage.and_then(|u| u.get(key))
}

/// `formatDoneLine({usage, latency})`.
///
/// `latency` is the object; `ttft` is printed only when truthy, `total` falls
/// back to 0 on `null`/`undefined`. `usage` is `null`-tolerant (`usage || {}`).
pub fn format_done_line(usage: Option<&Value>, latency: &Value) -> String {
    let in_tok = js_number(js_nullish(
        usage_get(usage, "prompt_tokens"),
        usage_get(usage, "input_tokens"),
    ));
    let out_tok = js_number(js_nullish(
        usage_get(usage, "completion_tokens"),
        usage_get(usage, "output_tokens"),
    ));
    let cache_read = js_number(js_nullish(
        js_nullish(
            usage_get(usage, "cache_read_input_tokens"),
            usage_get(usage, "cached_tokens"),
        ),
        usage_get(usage, "prompt_tokens_details").and_then(|d| d.get("cached_tokens")),
    ));
    let cache_create = js_number(usage_get(usage, "cache_creation_input_tokens"));

    let mut in_str = format!("IN {in_tok}");
    if cache_read != 0 || cache_create != 0 {
        let mut parts: Vec<String> = Vec::new();
        if cache_read != 0 {
            parts.push(format!("↻{cache_read}"));
        }
        if cache_create != 0 {
            parts.push(format!("+{cache_create}"));
        }
        in_str += &format!(" (CACHE {})", parts.join(" "));
    }

    let ttft_str = match latency.get("ttft").filter(|v| js_truthy(v)) {
        Some(ttft) => format!(" · TTFT {}ms", js_string(ttft)),
        None => String::new(),
    };
    let total = match latency.get("total") {
        Some(v) if !v.is_null() => js_string(v),
        _ => "0".to_string(),
    };

    format!("DONE {total}ms{ttft_str} · {in_str} · OUT {out_tok}")
}

/// `saveUsageStats({provider, model, tokens, connectionId, apiKey, endpoint,
/// label, silent})`, minus the write.
///
/// Returns the row the caller hands to `router_db::repos::usage::save_request_usage`,
/// or `None` when this returns early: no token object, or both the input and
/// output counts zero. `label`/`silent` only gate the console line.
// Eight-key options object; a struct would be a single-use wrapper for one call
// site.
#[allow(clippy::too_many_arguments)]
pub fn save_usage_stats(
    provider: &str,
    model: &str,
    tokens: Option<&Value>,
    connection_id: Option<&str>,
    api_key: Option<&str>,
    endpoint: Option<&str>,
    label: &str,
    silent: bool,
) -> Option<Value> {
    let tokens = tokens.filter(|t| t.is_object() || t.is_array())?;

    let in_tokens = js_number(js_nullish(
        tokens.get("input_tokens"),
        tokens.get("prompt_tokens"),
    ));
    let out_tokens = js_number(js_nullish(
        tokens.get("output_tokens"),
        tokens.get("completion_tokens"),
    ));
    if in_tokens == 0 && out_tokens == 0 {
        return None;
    }

    if !silent {
        let time = chrono::Local::now().format("%H:%M:%S").to_string();
        let account = connection_id
            .filter(|s| !s.is_empty())
            .map(|id| format!(" | account={}...", id.chars().take(8).collect::<String>()))
            .unwrap_or_default();
        // `COLORS.green`/`COLORS.reset` are ANSI escapes; tracing owns the
        // stream now, so the text is emitted without them.
        tracing::info!(
            target: "router_sse::chat_core",
            "[{}] 📊 [{}] {} | in={} | out={}{}",
            time,
            label,
            provider.to_uppercase(),
            in_tokens,
            out_tokens,
            account,
        );
    }

    // One storage convention (prompt_tokens cache-inclusive) so cached and
    // cache-creation tokens survive to cost calc + stats. See canonicalizeUsage.
    let normalized = crate::utils::usage_tracking::canonicalize_usage(Some(tokens)).unwrap_or_else(|| {
        json!({
            "prompt_tokens": js_number(js_nullish(tokens.get("prompt_tokens"), tokens.get("input_tokens"))),
            "completion_tokens": js_number(js_nullish(tokens.get("completion_tokens"), tokens.get("output_tokens"))),
        })
    });

    let mut row = Map::new();
    row.insert(
        "provider".into(),
        json!(if provider.is_empty() {
            "unknown"
        } else {
            provider
        }),
    );
    row.insert(
        "model".into(),
        json!(if model.is_empty() { "unknown" } else { model }),
    );
    row.insert("tokens".into(), normalized);
    row.insert("timestamp".into(), json!(router_db::time::now_iso()));
    if let Some(connection_id) = connection_id.filter(|s| !s.is_empty()) {
        row.insert("connectionId".into(), json!(connection_id));
    }
    if let Some(api_key) = api_key.filter(|s| !s.is_empty()) {
        row.insert("apiKey".into(), json!(api_key));
    }
    row.insert(
        "endpoint".into(),
        endpoint
            .filter(|s| !s.is_empty())
            .map_or(Value::Null, |e| json!(e)),
    );
    Some(Value::Object(row))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_extraction_claude_shape() {
        let body = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 30,
                "cache_creation_input_tokens": 5
            }
        });
        let out = extract_usage_from_response(&body).unwrap();
        assert_eq!(out["prompt_tokens"], json!(100));
        assert_eq!(out["completion_tokens"], json!(20));
        assert_eq!(out["cache_read_input_tokens"], json!(30));
        assert_eq!(out["cache_creation_input_tokens"], json!(5));
        assert!(out.get("cached_tokens").is_none());
    }

    #[test]
    fn usage_extraction_claude_cached_tokens_from_details() {
        let body =
            json!({"usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 7}}});
        let out = extract_usage_from_response(&body).unwrap();
        assert_eq!(out["cached_tokens"], json!(7));
    }

    #[test]
    fn usage_extraction_openai_shape() {
        let body = json!({
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 2,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 3}
            }
        });
        let out = extract_usage_from_response(&body).unwrap();
        assert_eq!(out["prompt_tokens"], json!(11));
        assert_eq!(out["completion_tokens"], json!(2));
        assert_eq!(out["cached_tokens"], json!(4));
        assert_eq!(out["reasoning_tokens"], json!(3));
    }

    #[test]
    fn usage_extraction_gemini_shape_both_places() {
        let flat = json!({"usageMetadata": {"promptTokenCount": 8, "candidatesTokenCount": 1, "cachedContentTokenCount": 2, "thoughtsTokenCount": 3}});
        let out = extract_usage_from_response(&flat).unwrap();
        assert_eq!(out["prompt_tokens"], json!(8));
        assert_eq!(out["completion_tokens"], json!(1));
        assert_eq!(out["cached_tokens"], json!(2));
        assert_eq!(out["reasoning_tokens"], json!(3));

        let wrapped = json!({"response": {"usageMetadata": {"promptTokenCount": 5}}});
        let out = extract_usage_from_response(&wrapped).unwrap();
        assert_eq!(out["prompt_tokens"], json!(5));

        assert!(extract_usage_from_response(&json!({})).is_none());
        assert!(extract_usage_from_response(&Value::Null).is_none());
    }

    #[test]
    fn done_line_without_cache_counters() {
        let usage = json!({"prompt_tokens": 100, "completion_tokens": 20});
        let latency = json!({"ttft": 250, "total": 1200});
        assert_eq!(
            format_done_line(Some(&usage), &latency),
            "DONE 1200ms · TTFT 250ms · IN 100 · OUT 20"
        );
    }

    #[test]
    fn done_line_with_cache_counters_and_no_ttft() {
        let usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 5
        });
        let latency = json!({"ttft": 0, "total": 900});
        assert_eq!(
            format_done_line(Some(&usage), &latency),
            "DONE 900ms · IN 100 (CACHE ↻30 +5) · OUT 20"
        );
    }

    #[test]
    fn done_line_tolerates_a_null_usage_and_latency() {
        assert_eq!(
            format_done_line(None, &Value::Null),
            "DONE 0ms · IN 0 · OUT 0"
        );
    }

    #[test]
    fn usage_stats_returns_none_when_both_counts_are_zero() {
        let tokens = json!({"prompt_tokens": 0, "completion_tokens": 0});
        assert!(
            save_usage_stats("p", "m", Some(&tokens), None, None, None, "USAGE", true).is_none()
        );
        assert!(save_usage_stats("p", "m", None, None, None, None, "USAGE", true).is_none());
    }

    #[test]
    fn usage_stats_row_is_canonical_and_ordered() {
        let tokens = json!({"input_tokens": 10, "output_tokens": 2});
        let row = save_usage_stats(
            "codex",
            "gpt",
            Some(&tokens),
            Some("conn-12345678"),
            Some("sk-1"),
            Some("/v1/chat"),
            "USAGE",
            true,
        )
        .unwrap();
        let keys: Vec<&String> = row.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "provider",
                "model",
                "tokens",
                "timestamp",
                "connectionId",
                "apiKey",
                "endpoint"
            ]
        );
        assert_eq!(row["endpoint"], json!("/v1/chat"));
        assert_eq!(row["tokens"]["prompt_tokens"], json!(10));
    }
}
