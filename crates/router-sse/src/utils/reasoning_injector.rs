//! Reasoning-placeholder injection.
//!
//! Some thinking-mode providers (DeepSeek, Kimi, MiniMax) reject an assistant
//! turn that carries tool calls but no `reasoning_content`. OpenAI-format
//! clients never send it, so a single space is injected to pass validation.
//! The provider-level rule comes from `transport.reasoningInject`; the
//! model-level rules are predicates on the model id.

use serde_json::{Value, json};

use crate::providers::registry::registry;

/// `PLACEHOLDER`.
const PLACEHOLDER: &str = " ";

/// `DEEPSEEK_V4_PRO`.
const DEEPSEEK_V4_PRO: &str = "deepseek-v4-pro";

/// `injectReasoningContent({ provider, model, body })`.
pub fn inject_reasoning_content(provider: Option<&str>, model: &str, body: Value) -> Value {
    let provider_rule = provider
        .and_then(|p| registry().transport(p))
        .and_then(|t| t.reasoning_inject.as_ref())
        .and_then(|r| r.get("scope"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let model_rule = if starts_with_ci(model, "kimi-") {
        Some("toolCalls".to_string())
    } else if model.to_ascii_lowercase().contains("deepseek") {
        Some("all".to_string())
    } else {
        None
    };

    let rule = provider_rule.or(model_rule);
    let next = apply_deepseek_v4_pro_alias(provider, model, body);
    apply_rule(next, rule.as_deref())
}

/// `applyDeepSeekV4ProAlias`: the `-max`/`-none` suffixes rewrite the model id
/// and set the thinking knobs.
fn apply_deepseek_v4_pro_alias(provider: Option<&str>, model: &str, body: Value) -> Value {
    if provider != Some("deepseek") {
        return body;
    }
    let (thinking_type, reasoning_effort) = match model {
        "deepseek-v4-pro-max" => ("enabled", Some("max")),
        "deepseek-v4-pro-none" => ("disabled", None),
        _ => return body,
    };
    let mut next = match body {
        Value::Object(_) => body,
        other => other,
    };
    let Some(obj) = next.as_object_mut() else {
        return next;
    };

    obj.insert("model".into(), json!(DEEPSEEK_V4_PRO));
    let mut extra_body = obj
        .get("extra_body")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut thinking = extra_body
        .get("thinking")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    thinking.insert("type".into(), json!(thinking_type));
    extra_body.insert("thinking".into(), Value::Object(thinking));
    obj.insert("extra_body".into(), Value::Object(extra_body));

    match reasoning_effort {
        Some(effort) => {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        None => {
            obj.remove("reasoning_effort");
        }
    }
    next
}

/// `shouldInject(message, scope)`.
fn should_inject(message: &Value, scope: &str) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    if let Some(rc) = message.get("reasoning_content").and_then(Value::as_str)
        && !rc.is_empty()
    {
        return false;
    }
    if scope == "toolCalls" {
        return message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
    }
    true
}

/// `applyRule(body, rule)`: a no-op when the rule is absent or there is no
/// `messages` array.
fn apply_rule(body: Value, rule: Option<&str>) -> Value {
    let Some(rule) = rule else {
        return body;
    };
    let Some(messages) = body.get("messages").and_then(Value::as_array) else {
        return body;
    };
    let mapped: Vec<Value> = messages
        .iter()
        .map(|m| {
            if should_inject(m, rule) {
                let mut m = m.clone();
                if let Some(obj) = m.as_object_mut() {
                    obj.insert("reasoning_content".into(), json!(PLACEHOLDER));
                }
                m
            } else {
                m.clone()
            }
        })
        .collect();
    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("messages".into(), Value::Array(mapped));
    }
    body
}

fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_only_injects_on_tool_call_turns() {
        let body = json!({"messages": [
            {"role": "assistant", "content": "hi"},
            {"role": "assistant", "tool_calls": [{"id": "t1"}]}
        ]});
        let out = inject_reasoning_content(Some("kimi"), "kimi-k2", body);
        assert!(out["messages"][0].get("reasoning_content").is_none());
        assert_eq!(out["messages"][1]["reasoning_content"], json!(" "));
    }

    #[test]
    fn deepseek_injects_on_every_assistant_turn() {
        let body = json!({"messages": [{"role": "assistant", "content": "hi"}]});
        let out = inject_reasoning_content(Some("deepseek"), "deepseek-v3", body);
        assert_eq!(out["messages"][0]["reasoning_content"], json!(" "));
    }

    #[test]
    fn an_existing_reasoning_content_is_left_alone() {
        let body =
            json!({"messages": [{"role": "assistant", "content": "hi", "reasoning_content": "r"}]});
        let out = inject_reasoning_content(Some("deepseek"), "deepseek-v3", body);
        assert_eq!(out["messages"][0]["reasoning_content"], json!("r"));
    }

    #[test]
    fn deepseek_v4_pro_aliases_rewrite_the_model_and_thinking() {
        let body = json!({"model": "deepseek-v4-pro-max", "messages": []});
        let out = inject_reasoning_content(Some("deepseek"), "deepseek-v4-pro-max", body);
        assert_eq!(out["model"], json!("deepseek-v4-pro"));
        assert_eq!(out["extra_body"]["thinking"]["type"], json!("enabled"));
        assert_eq!(out["reasoning_effort"], json!("max"));

        let body =
            json!({"model": "deepseek-v4-pro-none", "reasoning_effort": "high", "messages": []});
        let out = inject_reasoning_content(Some("deepseek"), "deepseek-v4-pro-none", body);
        assert_eq!(out["extra_body"]["thinking"]["type"], json!("disabled"));
        assert!(out.get("reasoning_effort").is_none());
    }
}
