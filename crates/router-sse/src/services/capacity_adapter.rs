//! Capacity-adapter pools.
//!
//! Each input modality (vision / pdf / audioInput / videoInput) can have a pool
//! of fallback models. The pool models are *appended behind* whatever models
//! were already going to be tried, and `combo::reorder_by_capabilities` floats
//! a capable pool model to the front only when none of the original models can
//! handle the request — so an adapter never overrides a combo that already
//! covers the capability.
//!
//! The second half of the module trims history when the request falls through
//! to an adapter model with a smaller context window.

use std::collections::HashSet;

use serde_json::Value;

use crate::catalog::catalog::get_capabilities_for_model;

/// `CAPABILITY_KEYS`.
const CAPABILITY_KEYS: [&str; 4] = ["vision", "pdf", "audioInput", "videoInput"];

/// `DEFAULT_FALLBACK_MODEL`.
pub const DEFAULT_FALLBACK_MODEL: &str = "oc/mimo-v2.6-flash-free";

/// `CHARS_PER_TOKEN` — a rough estimate, deliberately not a tokenizer.
const CHARS_PER_TOKEN: f64 = 4.0;
/// `HEAD_KEEP`: turns kept verbatim after the system block.
const HEAD_KEEP: usize = 6;

/// `upgradeLegacyModel(m)`.
fn upgrade_legacy_model(model: &str) -> String {
    if model == "oc/mimo-v2.5-free" {
        DEFAULT_FALLBACK_MODEL.to_string()
    } else {
        model.to_string()
    }
}

/// One normalized `capacityAdapter[cap]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapEntry {
    pub enabled: bool,
    pub round_robin: bool,
    pub models: Vec<String>,
}

/// `normalizeCapEntry(entry)`: accepts the object form and the legacy array
/// form (`[{model, enabled}]`, treated as enabled with the `fallback` strategy).
fn normalize_cap_entry(entry: Option<&Value>) -> CapEntry {
    match entry {
        Some(Value::Array(items)) => CapEntry {
            enabled: true,
            round_robin: false,
            models: items
                .iter()
                .map(|e| {
                    let name = e
                        .get("model")
                        .and_then(Value::as_str)
                        .or_else(|| e.as_str())
                        .unwrap_or("");
                    upgrade_legacy_model(name)
                })
                .filter(|m| !m.is_empty())
                .collect(),
        },
        Some(Value::Object(obj)) => CapEntry {
            // `entry.enabled !== false`: absent means enabled.
            enabled: obj.get("enabled").and_then(Value::as_bool) != Some(false),
            round_robin: obj
                .get("roundRobin")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            models: obj
                .get("models")
                .and_then(Value::as_array)
                .map(|models| {
                    models
                        .iter()
                        .filter_map(Value::as_str)
                        .map(upgrade_legacy_model)
                        .collect()
                })
                .unwrap_or_default(),
        },
        _ => CapEntry {
            enabled: false,
            round_robin: false,
            models: Vec::new(),
        },
    }
}

/// `getCapacityAdapterConfig(cap, settings)`: an enabled pool with no models
/// falls back to `DEFAULT_FALLBACK_MODEL`, so the toggle is never a no-op.
pub fn get_capacity_adapter_config(cap: &str, settings: &Value) -> CapEntry {
    let entry = normalize_cap_entry(settings.get("capacityAdapter").and_then(|c| c.get(cap)));
    if entry.enabled && entry.models.is_empty() {
        return CapEntry {
            models: vec![DEFAULT_FALLBACK_MODEL.to_string()],
            ..entry
        };
    }
    entry
}

/// `getCapacityAdapterModels(settings)`: enabled pools flattened in priority
/// order, deduped.
pub fn get_capacity_adapter_models(settings: &Value) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for cap in CAPABILITY_KEYS {
        let entry = get_capacity_adapter_config(cap, settings);
        if !entry.enabled {
            continue;
        }
        for m in entry.models {
            if seen.insert(m.clone()) {
                models.push(m);
            }
        }
    }
    models
}

/// `getCapacityAdapterStrategy(cap, settings)`.
pub fn get_capacity_adapter_strategy(cap: &str, settings: &Value) -> &'static str {
    let entry = get_capacity_adapter_config(cap, settings);
    if entry.enabled && entry.round_robin {
        "round-robin"
    } else {
        "fallback"
    }
}

/// `getActiveAdapterStrategy(requiredCapabilities, settings)`: the first hard
/// capability with a usable pool decides the strategy.
pub fn get_active_adapter_strategy(
    required_capabilities: &HashSet<String>,
    settings: &Value,
) -> &'static str {
    for cap in CAPABILITY_KEYS {
        if !required_capabilities.contains(cap) {
            continue;
        }
        let entry = get_capacity_adapter_config(cap, settings);
        if !entry.enabled || entry.models.is_empty() {
            continue;
        }
        return get_capacity_adapter_strategy(cap, settings);
    }
    "fallback"
}

/// The hard capabilities present in `required`, in `CAPABILITY_KEYS` order.
fn hard_caps(required: &HashSet<String>) -> Vec<&'static str> {
    CAPABILITY_KEYS
        .iter()
        .copied()
        .filter(|c| required.contains(*c))
        .collect()
}

/// `modelSatisfies(modelStr, requiredHard)`.
fn model_satisfies(model_str: &str, required_hard: &[&str]) -> bool {
    let slash = model_str.find('/');
    let (provider, model) = match slash {
        Some(0) | None => ("", model_str),
        Some(i) => (&model_str[..i], &model_str[i + 1..]),
    };
    let caps = get_capabilities_for_model((!provider.is_empty()).then_some(provider), model);
    required_hard.iter().all(|c| match *c {
        "vision" => caps.vision,
        "pdf" => caps.pdf,
        "audioInput" => caps.audio_input,
        "videoInput" => caps.video_input,
        _ => false,
    })
}

/// `augmentModelsWithCapacityAdapter(models, requiredCapabilities, settings)`.
///
/// Adapter models go **first** (priority); the originals follow as fallback.
/// Returns `models` untouched when the originals already cover the requirement,
/// or when no pool model does.
pub fn augment_models_with_capacity_adapter(
    models: &[String],
    required_capabilities: &HashSet<String>,
    settings: &Value,
) -> Vec<String> {
    let hard = hard_caps(required_capabilities);
    if hard.is_empty() || models.is_empty() {
        return models.to_vec();
    }
    if models.iter().any(|m| model_satisfies(m, &hard)) {
        return models.to_vec();
    }

    let pool: Vec<String> = get_capacity_adapter_models(settings)
        .into_iter()
        .filter(|m| !models.contains(m) && model_satisfies(m, &hard))
        .collect();
    if pool.is_empty() {
        return models.to_vec();
    }
    pool.into_iter().chain(models.iter().cloned()).collect()
}

/// `blockLength(content)`: string length, or the sum of `text` lengths with a
/// flat 50 for every non-text block.
fn block_length(content: Option<&Value>) -> usize {
    match content {
        Some(Value::String(s)) => s.chars().count(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(Value::as_str)
                    .map_or(50, |t| t.chars().count())
            })
            .sum(),
        _ => 0,
    }
}

/// `stripHistoryForContext(body, contextWindow)`: drop the middle of the
/// conversation to fit a smaller window, keeping every system message, the
/// first `HEAD_KEEP` turns after them, and the trailing user run carrying the
/// media the switch happened for.
pub fn strip_history_for_context(body: &Value, context_window: i64) -> Value {
    let key = if body.get("messages").and_then(Value::as_array).is_some() {
        "messages"
    } else if body.get("input").and_then(Value::as_array).is_some() {
        "input"
    } else if body.get("contents").and_then(Value::as_array).is_some() {
        "contents"
    } else {
        return body.clone();
    };

    let arr = body[key].as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        return body.clone();
    }

    let is_system = |m: &Value| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("system" | "developer")
        )
    };
    let system_msgs: Vec<Value> = arr.iter().filter(|m| is_system(m)).cloned().collect();
    let rest: Vec<Value> = arr.iter().filter(|m| !is_system(m)).cloned().collect();
    if rest.is_empty() {
        return body.clone();
    }

    let is_assistant = |m: &Value| {
        matches!(
            m.get("role").and_then(Value::as_str),
            Some("assistant" | "model")
        )
    };
    let mut i = rest.len();
    while i > 0 {
        if is_assistant(&rest[i - 1]) {
            break;
        }
        i -= 1;
    }
    let tail = rest[i..].to_vec();
    let older = rest[..i].to_vec();
    if older.is_empty() {
        return body.clone();
    }

    fn content_of(m: &Value) -> Option<&Value> {
        m.get("content").or_else(|| m.get("parts"))
    }
    // 80% of the adapter model's window, leaving room for the response.
    let window = if context_window > 0 {
        context_window
    } else {
        200_000
    };
    let budget_chars = (window as f64) * 0.8 * CHARS_PER_TOKEN;

    let mut head: Vec<Value> = older.iter().take(HEAD_KEEP).cloned().collect();
    let mut total: usize = system_msgs
        .iter()
        .chain(head.iter())
        .chain(tail.iter())
        .map(|m| block_length(content_of(m)))
        .sum();

    // Head overflow: drop head turns from the end (closest to the middle) first.
    while (total as f64) > budget_chars && !head.is_empty() {
        if let Some(dropped) = head.pop() {
            total = total.saturating_sub(block_length(content_of(&dropped)));
        }
    }

    if head.len() == older.len() {
        return body.clone();
    }

    let mut out = body.as_object().cloned().unwrap_or_default();
    let mut combined = system_msgs;
    combined.extend(head);
    combined.extend(tail);
    out.insert(key.into(), Value::Array(combined));
    Value::Object(out)
}

/// `withCapacityAdapterStripping(handleSingleModel, adapterModels)`: the body a
/// call to an adapter model receives, after history trimming. `None` means the
/// model is not an adapter, so the caller passes the body through untouched.
pub fn strip_for_adapter_model(
    body: &Value,
    model_str: &str,
    adapter_models: &HashSet<String>,
) -> Option<Value> {
    if !adapter_models.contains(model_str) {
        return None;
    }
    let slash = model_str.find('/');
    let (provider, model) = match slash {
        Some(0) | None => ("", model_str),
        Some(i) => (&model_str[..i], &model_str[i + 1..]),
    };
    let context_window =
        get_capabilities_for_model((!provider.is_empty()).then_some(provider), model)
            .context_window;
    Some(strip_history_for_context(body, context_window))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings_with(cap: &str, entry: Value) -> Value {
        json!({ "capacityAdapter": { cap: entry } })
    }

    #[test]
    fn an_enabled_pool_with_no_models_gets_the_default() {
        let settings = settings_with("vision", json!({"enabled": true, "models": []}));
        let cfg = get_capacity_adapter_config("vision", &settings);
        assert!(cfg.enabled);
        assert_eq!(cfg.models, vec![DEFAULT_FALLBACK_MODEL]);
    }

    #[test]
    fn a_missing_entry_is_disabled() {
        let cfg = get_capacity_adapter_config("vision", &json!({}));
        assert!(!cfg.enabled);
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn the_legacy_array_form_is_enabled_with_the_fallback_strategy() {
        let settings = settings_with("vision", json!([{"model": "a/1"}, "b/2"]));
        let cfg = get_capacity_adapter_config("vision", &settings);
        assert!(cfg.enabled);
        assert!(!cfg.round_robin);
        assert_eq!(cfg.models, vec!["a/1", "b/2"]);
        assert_eq!(
            get_capacity_adapter_strategy("vision", &settings),
            "fallback"
        );
    }

    #[test]
    fn the_legacy_model_name_is_upgraded() {
        let settings = settings_with("vision", json!({"models": ["oc/mimo-v2.5-free"]}));
        let cfg = get_capacity_adapter_config("vision", &settings);
        assert_eq!(cfg.models, vec![DEFAULT_FALLBACK_MODEL]);
    }

    #[test]
    fn round_robin_strategy_needs_both_flags() {
        let rr = settings_with(
            "vision",
            json!({"enabled": true, "roundRobin": true, "models": ["a/1"]}),
        );
        assert_eq!(get_capacity_adapter_strategy("vision", &rr), "round-robin");

        let disabled = settings_with(
            "vision",
            json!({"enabled": false, "roundRobin": true, "models": ["a/1"]}),
        );
        assert_eq!(
            get_capacity_adapter_strategy("vision", &disabled),
            "fallback"
        );
    }

    #[test]
    fn enabled_pools_flatten_in_capability_order_and_dedupe() {
        let settings = json!({"capacityAdapter": {
            "vision": {"enabled": true, "models": ["v/1", "shared/1"]},
            "pdf": {"enabled": false, "models": ["p/1"]},
            "audioInput": {"enabled": true, "models": ["shared/1", "a/1"]},
            "videoInput": {"enabled": true, "models": []},
        }});
        assert_eq!(
            get_capacity_adapter_models(&settings),
            vec!["v/1", "shared/1", "a/1", DEFAULT_FALLBACK_MODEL]
        );
    }

    #[test]
    fn augment_leaves_models_alone_when_nothing_is_required() {
        let models = vec!["a/1".to_string()];
        let settings = settings_with("vision", json!({"enabled": true, "models": ["v/1"]}));
        assert_eq!(
            augment_models_with_capacity_adapter(&models, &HashSet::new(), &settings),
            models
        );
    }

    #[test]
    fn augment_does_nothing_when_the_original_list_already_covers_the_capability() {
        // `vision` on a model the capability table marks vision-capable.
        let capable = "openrouter/google/gemini-2.5-flash".to_string();
        let models = vec![capable.clone()];
        let required: HashSet<String> = ["vision".to_string()].into_iter().collect();
        let settings = settings_with("vision", json!({"enabled": true, "models": ["v/1"]}));
        assert_eq!(
            augment_models_with_capacity_adapter(&models, &required, &settings),
            models
        );
    }

    #[test]
    fn augment_prepends_the_pool_when_no_original_model_covers_it() {
        let incapable = "openrouter/deepseek/deepseek-chat".to_string();
        let models = vec![incapable.clone()];
        let required: HashSet<String> = ["vision".to_string()].into_iter().collect();
        // The pool model must itself satisfy the capability to be added.
        let capable_pool = "openrouter/google/gemini-2.5-flash";
        let settings = settings_with("vision", json!({"enabled": true, "models": [capable_pool]}));
        let out = augment_models_with_capacity_adapter(&models, &required, &settings);
        assert_eq!(out, vec![capable_pool.to_string(), incapable]);
    }

    #[test]
    fn augment_skips_a_pool_model_that_cannot_satisfy_the_capability() {
        let incapable = "openrouter/deepseek/deepseek-chat".to_string();
        let models = vec![incapable.clone()];
        let required: HashSet<String> = ["vision".to_string()].into_iter().collect();
        let settings = settings_with(
            "vision",
            json!({"enabled": true, "models": [incapable.clone()]}),
        );
        assert_eq!(
            augment_models_with_capacity_adapter(&models, &required, &settings),
            models
        );
    }

    #[test]
    fn stripping_is_a_no_op_for_a_non_adapter_model() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        assert!(strip_for_adapter_model(&body, "a/1", &HashSet::new()).is_none());
    }

    #[test]
    fn history_stripping_keeps_system_head_and_tail() {
        // Build a long middle so the budget forces a drop. Budget = 200000 * 0.8 * 4
        // chars, so each message needs to be large for the trim to bite.
        let filler = "x".repeat(400_000);
        let mut messages = vec![json!({"role": "system", "content": "sys"})];
        for i in 0..10 {
            messages.push(json!({"role": "user", "content": format!("{filler}{i}")}));
            messages.push(json!({"role": "assistant", "content": "ok"}));
        }
        messages.push(json!({"role": "user", "content": "current turn"}));
        let body = json!({ "messages": messages });

        let out = strip_history_for_context(&body, 200_000);
        let kept = out["messages"].as_array().unwrap();
        assert_eq!(kept[0]["role"], json!("system"));
        assert_eq!(kept.last().unwrap()["content"], json!("current turn"));
        assert!(
            kept.len() < 22,
            "the middle must be dropped: kept {}",
            kept.len()
        );
    }

    #[test]
    fn history_stripping_leaves_a_short_conversation_untouched() {
        let body = json!({"messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "a"},
            {"role": "assistant", "content": "b"},
            {"role": "user", "content": "c"},
        ]});
        assert_eq!(strip_history_for_context(&body, 200_000), body);
    }

    #[test]
    fn history_stripping_handles_the_gemini_shape() {
        let filler = "x".repeat(400_000);
        let mut contents = vec![json!({"role": "user", "parts": [{"text": "first"}]})];
        for i in 0..10 {
            contents.push(json!({"role": "model", "parts": [{"text": format!("{filler}{i}")}]}));
            contents.push(json!({"role": "user", "parts": [{"text": format!("{filler}{i}")}]}));
        }
        contents.push(json!({"role": "user", "parts": [{"text": "current"}]}));
        let body = json!({ "contents": contents });

        let out = strip_history_for_context(&body, 100_000);
        let kept = out["contents"].as_array().unwrap();
        assert_eq!(kept.last().unwrap()["parts"][0]["text"], json!("current"));
        assert!(kept.len() < 22);
        // No `messages` key invented.
        assert!(out.get("messages").is_none());
    }

    #[test]
    fn history_stripping_returns_the_body_when_there_is_nothing_to_trim() {
        // No message array at all.
        assert_eq!(strip_history_for_context(&json!({}), 1000), json!({}));
        // Only system messages.
        let only_system = json!({"messages": [{"role": "system", "content": "s"}]});
        assert_eq!(strip_history_for_context(&only_system, 1000), only_system);
        // Only a tail (no assistant turn before it).
        let only_tail = json!({"messages": [{"role": "user", "content": "u"}]});
        assert_eq!(strip_history_for_context(&only_tail, 1000), only_tail);
    }

    #[test]
    fn block_length_counts_text_blocks_and_flat_rates_the_rest() {
        assert_eq!(block_length(Some(&json!("abcd"))), 4);
        assert_eq!(
            block_length(Some(&json!([{"text": "abc"}, {"type": "image_url"}]))),
            3 + 50
        );
        assert_eq!(block_length(Some(&json!(42))), 0);
        assert_eq!(block_length(None), 0);
    }
}
