//! Provider usage → OpenAI usage.
//!
//! Each provider folds its raw counters differently (claude folds
//! cache and reasoning into the totals, the others do not), so the extractors
//! stay separate rather than sharing a "sum the obvious fields" helper that
//! would be wrong for half of them.

use serde_json::{Map, Value, json};

/// `buildUsage(args)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageArgs {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    pub reasoning_tokens: i64,
}

/// `buildUsage`: the detail objects appear only when their count is positive.
pub fn build_usage(args: UsageArgs) -> Value {
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(args.prompt_tokens));
    usage.insert("completion_tokens".into(), json!(args.completion_tokens));
    usage.insert("total_tokens".into(), json!(args.total_tokens));

    if args.cached_tokens > 0 || args.cache_creation_tokens > 0 {
        let mut details = Map::new();
        if args.cached_tokens > 0 {
            details.insert("cached_tokens".into(), json!(args.cached_tokens));
        }
        if args.cache_creation_tokens > 0 {
            details.insert(
                "cache_creation_tokens".into(),
                json!(args.cache_creation_tokens),
            );
        }
        usage.insert("prompt_tokens_details".into(), Value::Object(details));
    }
    if args.reasoning_tokens > 0 {
        usage.insert(
            "completion_tokens_details".into(),
            json!({"reasoning_tokens": args.reasoning_tokens}),
        );
    }
    Value::Object(usage)
}

/// `n(v)`: a non-number is zero. JSON numbers arrive as `f64`, so the cast is
/// explicit; a float token count is truncated the way `Number` math would then
/// be re-stringified by the caller.
fn n(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_f64).unwrap_or(0.0) as i64
}

/// `USAGE_EXTRACTORS[kind](raw)`.
pub fn extract_usage(raw: &Value, kind: &str) -> Option<UsageArgs> {
    if !raw.is_object() {
        return None;
    }
    let g = |key: &str| raw.get(key);
    Some(match kind {
        "claude" => {
            let input = n(g("input_tokens"));
            let output = n(g("output_tokens"));
            let cache_read = n(g("cache_read_input_tokens"));
            let cache_create = n(g("cache_creation_input_tokens"));
            let prompt = input + cache_read + cache_create;
            UsageArgs {
                prompt_tokens: prompt,
                completion_tokens: output,
                total_tokens: prompt + output,
                cached_tokens: cache_read,
                cache_creation_tokens: cache_create,
                ..Default::default()
            }
        }
        "commandcode" => {
            let input = n(g("inputTokens"));
            let output = n(g("outputTokens"));
            let total = match g("totalTokens").and_then(Value::as_f64) {
                Some(t) => t as i64,
                None => input + output,
            };
            UsageArgs {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                ..Default::default()
            }
        }
        _ => return None,
    })
}

/// `toOpenAIUsage(raw, kind)`: `None` when there is no extractor or no raw.
pub fn to_openai_usage(raw: Option<&Value>, kind: &str) -> Option<Value> {
    let raw = raw?;
    extract_usage(raw, kind).map(build_usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_folds_cache_into_the_prompt_total() {
        let raw = json!({
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 5,
        });
        let usage = to_openai_usage(Some(&raw), "claude").unwrap();
        assert_eq!(usage["prompt_tokens"], json!(125));
        assert_eq!(usage["completion_tokens"], json!(50));
        assert_eq!(usage["total_tokens"], json!(175));
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], json!(20));
        assert_eq!(
            usage["prompt_tokens_details"]["cache_creation_tokens"],
            json!(5)
        );
    }

    #[test]
    fn commandcode_uses_its_own_field_names() {
        let usage = to_openai_usage(
            Some(&json!({"inputTokens": 7, "outputTokens": 3, "totalTokens": 11})),
            "commandcode",
        )
        .unwrap();
        assert_eq!(
            usage["total_tokens"],
            json!(11),
            "an explicit total wins over the sum"
        );
    }

    #[test]
    fn unknown_kinds_and_non_objects_yield_nothing() {
        assert!(to_openai_usage(Some(&json!({})), "nope").is_none());
        assert!(to_openai_usage(Some(&json!("x")), "claude").is_none());
        assert!(to_openai_usage(None, "claude").is_none());
    }
}
