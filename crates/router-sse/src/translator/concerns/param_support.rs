//! Strip request params a provider/model rejects upstream.
//!
//! Config-driven on purpose: a new quirk is a new rule, not another
//! ad-hoc deletion scattered through the executors. The rules are data, so the
//! only logic is the match test and the per-rule action.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// The model matcher for a rule.
enum Matcher {
    /// No `match` field: the rule applies to every model of the provider.
    Any,
    /// A regex tested against the model id.
    Re(&'static str),
}

impl Matcher {
    fn matches(&self, model: &str) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Re(pattern) => compile(pattern).is_match(model),
        }
    }
}

/// One strip rule.
struct Rule {
    provider: Option<&'static str>,
    matcher: Matcher,
    drop: &'static [&'static str],
    drop_message_fields: &'static [&'static str],
}

/// Every strip rule. The order does not matter (rules are independent).
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // All Claude models: temperature deprecated/rejected upstream (Anthropic 400).
        Rule {
            provider: None,
            matcher: Matcher::Re("(?i)claude"),
            drop: &["temperature"],
            drop_message_fields: &[],
        },
        // Strict validators reject replayed reasoning on assistant turns.
        Rule {
            provider: Some("mistral"),
            matcher: Matcher::Any,
            drop: &[],
            drop_message_fields: &["reasoning_content", "reasoning", "reasoning_details"],
        },
    ]
});

/// Compiled rule patterns. The set is tiny and all patterns are literals, so a
/// process-wide map built on first use is enough; `Regex` clones share the
/// compiled program.
fn compile(pattern: &'static str) -> Regex {
    static CACHE: LazyLock<dashmap::DashMap<&'static str, Regex>> =
        LazyLock::new(dashmap::DashMap::new);
    CACHE
        .entry(pattern)
        .or_insert_with(|| Regex::new(pattern).expect("static strip-rule pattern"))
        .clone()
}

/// Strip the params this provider/model rejects, in place.
pub fn strip_unsupported_params(provider: Option<&str>, model: &str, body: &mut Value) {
    if model.is_empty() || !body.is_object() {
        return;
    }
    for rule in RULES.iter() {
        if rule.provider.is_some_and(|p| Some(p) != provider) {
            continue;
        }
        if !rule.matcher.matches(model) {
            continue;
        }
        if let Some(obj) = body.as_object_mut() {
            for key in rule.drop {
                obj.remove(*key);
            }
        }
        if !rule.drop_message_fields.is_empty()
            && let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut)
        {
            for msg in messages {
                if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                    continue;
                }
                if let Some(obj) = msg.as_object_mut() {
                    for key in rule.drop_message_fields {
                        obj.remove(*key);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_models_lose_temperature_everywhere() {
        let mut body = json!({"temperature": 0.7});
        strip_unsupported_params(None, "claude-sonnet-4.5", &mut body);
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn a_non_claude_model_keeps_temperature() {
        let mut body = json!({"temperature": 0.7});
        strip_unsupported_params(Some("openai"), "gpt-5", &mut body);
        assert_eq!(body["temperature"], json!(0.7));
    }

    #[test]
    fn rules_are_scoped_to_their_provider() {
        let mut body = json!({"thinking": {"type": "enabled"}});
        strip_unsupported_params(Some("anthropic"), "claude-sonnet-4.5", &mut body);
        assert!(body.get("thinking").is_some());
    }
}
