//! `max_tokens` adjustment.

use serde_json::Value;

use crate::runtime_config::{DEFAULT_MAX_TOKENS, DEFAULT_MIN_TOKENS};

/// `adjustMaxTokens(body, ceiling)`.
///
/// Callers with model context (openai-to-claude) pass the model's real
/// `maxOutput` so a high-output model (Opus 4.8 = 128000) is not pre-clamped to
/// the conservative 64000 default before the model-aware step sees it.
pub fn adjust_max_tokens(body: &Value, ceiling: i64) -> i64 {
    let mut max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_i64)
        .filter(|v| *v != 0)
        .unwrap_or(DEFAULT_MAX_TOKENS);

    // Auto-increase for tool calling to avoid truncated arguments.
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty())
        && max_tokens < DEFAULT_MIN_TOKENS
    {
        max_tokens = DEFAULT_MIN_TOKENS;
    }

    // Claude requires max_tokens > thinking.budget_tokens. A 1024 buffer is
    // added rather than using the ceiling, which could equal the budget
    // when budget_tokens >= ceiling.
    if let Some(budget) = body
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(Value::as_i64)
        && max_tokens <= budget
    {
        max_tokens = budget + 1024;
    }

    max_tokens.min(ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_when_absent_and_clamps_to_the_ceiling() {
        assert_eq!(
            adjust_max_tokens(&json!({}), DEFAULT_MAX_TOKENS),
            DEFAULT_MAX_TOKENS
        );
        assert_eq!(
            adjust_max_tokens(&json!({"max_tokens": 999_999}), DEFAULT_MAX_TOKENS),
            DEFAULT_MAX_TOKENS
        );
        // A caller-supplied ceiling above the default is honoured.
        assert_eq!(
            adjust_max_tokens(&json!({"max_tokens": 100_000}), 128_000),
            100_000
        );
    }

    #[test]
    fn tools_raise_a_small_max_tokens_to_the_minimum() {
        let body = json!({"max_tokens": 1000, "tools": [{"type": "function"}]});
        assert_eq!(
            adjust_max_tokens(&body, DEFAULT_MAX_TOKENS),
            DEFAULT_MIN_TOKENS
        );
        // An empty tools array does not trigger it.
        let body = json!({"max_tokens": 1000, "tools": []});
        assert_eq!(adjust_max_tokens(&body, DEFAULT_MAX_TOKENS), 1000);
    }

    #[test]
    fn a_thinking_budget_forces_max_tokens_above_it() {
        let body = json!({"max_tokens": 1000, "thinking": {"budget_tokens": 4000}});
        assert_eq!(adjust_max_tokens(&body, DEFAULT_MAX_TOKENS), 5024);
        // The buffer can push past the ceiling, which is then applied.
        let body = json!({"max_tokens": 1000, "thinking": {"budget_tokens": 100_000}});
        assert_eq!(
            adjust_max_tokens(&body, DEFAULT_MAX_TOKENS),
            DEFAULT_MAX_TOKENS
        );
    }
}
