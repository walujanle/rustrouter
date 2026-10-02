//! Thinking/reasoning normalization.
//!
//! This is where the client's reasoning intent is read off the request and
//! rewritten into whatever wire shape the target provider expects. Every
//! provider-specific quirk lives in `apply_format`; the maps and the intent
//! extraction are shared.
//!
//! The one thing to keep in mind: `apply_thinking` **strips every known
//! thinking field first**, then applies the resolved format. A field that
//! survives that strip is a bug — the provider sees two contradictory
//! declarations and either 400s or ignores the one the client asked for.

use serde_json::{Map, Value, json};

use crate::catalog::{Capabilities, get_capabilities_for_model};
use crate::translator::concerns::primitives::js_json_number;

/// `EFFORT_LEVELS`, ordered low to high.
pub const EFFORT_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// `LEVEL_TO_BUDGET`. `none` is present here but absent from `EFFORT_LEVELS`.
fn level_to_budget(level: &str) -> Option<i64> {
    Some(match level {
        "none" => 0,
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" => 32768,
        "max" => 128000,
        _ => return None,
    })
}

/// `effortToBudget(effort)`.
pub fn effort_to_budget(effort: &str) -> Option<i64> {
    level_to_budget(&effort.to_lowercase())
}

/// `effortToThinkingLevel(effort)`: OpenAI effort to the Gemini 3 enum.
/// Gemini 3 cannot fully disable thinking, so `none`/`off` clamp to `minimal`.
pub fn effort_to_thinking_level(effort: &str) -> String {
    let e = effort.to_lowercase();
    let e = e.trim();
    match e {
        "none" | "off" => "minimal".to_string(),
        "xhigh" | "max" => "high".to_string(),
        other => other.to_string(),
    }
}

/// `budgetToLevel(budget)`: nearest discrete level via the midpoints between
/// `LEVEL_TO_BUDGET` values. `null` when the budget is zero or negative.
pub fn budget_to_level(budget: f64) -> Option<&'static str> {
    if budget <= 0.0 || budget.is_nan() {
        return None;
    }
    Some(if budget <= 768.0 {
        "minimal"
    } else if budget <= 4096.0 {
        "low"
    } else if budget <= 16384.0 {
        "medium"
    } else if budget <= 28672.0 {
        "high"
    } else if budget <= 80384.0 {
        "xhigh"
    } else {
        "max"
    })
}

/// The unified thinking intent: `{ mode, budget?, level? }`.
#[derive(Debug, Clone, PartialEq)]
pub enum ThinkingMode {
    None,
    Auto,
    Budget(f64),
    Level(String),
}

/// `extractThinking(body)`: read the client's intent from a post-translation
/// body. Returns `None` when no thinking intent is present at all.
pub fn extract_thinking(body: &Value) -> Option<ThinkingMode> {
    let obj = body.as_object()?;

    // Claude output_config.effort, explicit, beats adaptive thinking.
    if let Some(oc) = obj
        .get("output_config")
        .and_then(|v| v.get("effort"))
        .and_then(Value::as_str)
        && !oc.is_empty()
    {
        return Some(mode_from_effort(oc));
    }

    // OpenAI chat / Responses. `reasoning_effort` first: zai sends both a
    // thinking object and reasoning.effort.
    let effort = obj
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
        });
    if let Some(effort) = effort.filter(|s| !s.is_empty()) {
        return Some(mode_from_effort(effort));
    }

    // Claude shape.
    if let Some(t) = obj.get("thinking").and_then(Value::as_object) {
        match t.get("type").and_then(Value::as_str) {
            Some("disabled") => return Some(ThinkingMode::None),
            Some("adaptive") | Some("enabled") => {
                let budget = t.get("budget_tokens").and_then(Value::as_f64);
                return Some(match budget.filter(|b| *b > 0.0) {
                    Some(b) => ThinkingMode::Budget(b),
                    None => ThinkingMode::Auto,
                });
            }
            _ => {}
        }
    }

    // Gemini shape: top-level, generationConfig, or the request envelope.
    let tc = obj
        .get("thinkingConfig")
        .or_else(|| {
            obj.get("generationConfig")
                .and_then(|g| g.get("thinkingConfig"))
        })
        .or_else(|| {
            obj.get("request")
                .and_then(|r| r.get("generationConfig"))
                .and_then(|g| g.get("thinkingConfig"))
        })
        .and_then(Value::as_object);
    if let Some(tc) = tc {
        if let Some(level) = tc.get("thinkingLevel").and_then(Value::as_str) {
            return Some(ThinkingMode::Level(level.to_lowercase()));
        }
        if let Some(tb) = tc.get("thinkingBudget").and_then(Value::as_f64) {
            return Some(if tb == 0.0 {
                ThinkingMode::None
            } else if tb < 0.0 {
                ThinkingMode::Auto
            } else {
                ThinkingMode::Budget(tb)
            });
        }
    }

    // Qwen shape.
    match obj.get("enable_thinking").and_then(Value::as_bool) {
        Some(false) => return Some(ThinkingMode::None),
        Some(true) => {
            let tb = obj.get("thinking_budget").and_then(Value::as_f64);
            return Some(match tb.filter(|b| *b > 0.0) {
                Some(b) => ThinkingMode::Budget(b),
                None => ThinkingMode::Auto,
            });
        }
        None => {}
    }

    None
}

/// `captureThinking` is an alias for `extractThinking`, named for the call site
/// where the intent is snapshotted before format translation.
pub use extract_thinking as capture_thinking;

fn mode_from_effort(effort: &str) -> ThinkingMode {
    match effort.to_lowercase().as_str() {
        "none" | "off" => ThinkingMode::None,
        "auto" => ThinkingMode::Auto,
        other => ThinkingMode::Level(other.to_string()),
    }
}

/// The `model(value)` override parsed off a model id.
#[derive(Debug, Clone, PartialEq)]
pub enum SuffixOverride {
    Mode(ThinkingMode),
}

/// `parseSuffix(model)`: `{ cleanModel, override }`.
pub fn parse_suffix(model: &str) -> (String, Option<ThinkingMode>) {
    let Some((clean, raw)) = split_trailing_paren(model) else {
        return (model.to_string(), None);
    };
    let raw = raw.trim().to_lowercase();
    let override_ = match raw.as_str() {
        "none" | "off" => Some(ThinkingMode::None),
        "auto" => Some(ThinkingMode::Auto),
        "ultra" => Some(ThinkingMode::Level("ultra".to_string())),
        _ if raw.chars().all(|c| c.is_ascii_digit()) && !raw.is_empty() => {
            raw.parse::<f64>().ok().map(ThinkingMode::Budget)
        }
        _ if level_to_budget(&raw).is_some() => Some(ThinkingMode::Level(raw)),
        _ => None,
    };
    (clean, override_)
}

/// `stripThinkingSuffix(model)`: drop a trailing `(...)`, no-op when absent.
pub fn strip_thinking_suffix(model: &str) -> String {
    match split_trailing_paren(model) {
        Some((clean, _)) => clean,
        None => model.to_string(),
    }
}

/// Split a trailing `(...)` group, returning the trimmed base and the raw
/// (untrimmed) group contents. Mirrors `/^(.*)\(([^()]+)\)\s*$/`.
fn split_trailing_paren(model: &str) -> Option<(String, String)> {
    let trimmed = model.trim_end();
    if !trimmed.ends_with(')') {
        return None;
    }
    let open = trimmed.rfind('(')?;
    let inner = &trimmed[open + 1..trimmed.len() - 1];
    // `[^()]+`: the group must be non-empty and contain no nested parens.
    if inner.is_empty() || inner.contains('(') || inner.contains(')') {
        return None;
    }
    Some((trimmed[..open].trim().to_string(), inner.to_string()))
}

/// `FORMAT_TO_NATIVE`: the native thinking format for a wire format that
/// declares none.
fn format_to_native(target_format: &str) -> &'static str {
    match target_format {
        "openai" | "openai-responses" | "openai-response" | "codex" => "openai",
        "claude" => "claude-budget",
        "commandcode" => "commandcode",
        _ => "openai",
    }
}

/// Formats that are meaningless on an OpenAI wire and must be replaced by the
/// wire-native one.
fn is_native_only_format(fmt: &str) -> bool {
    matches!(
        fmt,
        "gemini-level" | "gemini-budget" | "claude-budget" | "claude-adaptive"
    )
}

/// `resolve_format(targetFormat, provider, caps)`.
///
/// The provider override is read off the *built transport*, not the registry
/// entry — the dump hoists `thinking_format` there, and only ten providers
/// declare one. The model is already resolved into `caps` by the caller, so it
/// is not a parameter here.
fn resolve_format(target_format: &str, provider: Option<&str>, caps: &Capabilities) -> String {
    if target_format == "commandcode" {
        return "commandcode".to_string();
    }
    if let Some(provider_fmt) = provider
        .and_then(|p| crate::providers::registry::registry().transport(p))
        .and_then(|t| t.thinking_format.as_deref())
    {
        return provider_fmt.to_string();
    }
    let is_openai_wire = target_format == "openai" || target_format == "openai-responses";
    if let Some(fmt) = caps.thinking_format.as_deref()
        && !(is_openai_wire && is_native_only_format(fmt))
    {
        return fmt.to_string();
    }
    format_to_native(target_format).to_string()
}

/// `toBudget(cfg, range)`: `Some(-1)` means "auto", `None` means "unresolvable".
fn to_budget(cfg: &ThinkingMode, caps: &Capabilities) -> Option<f64> {
    let mut budget = match cfg {
        ThinkingMode::Budget(b) => *b,
        ThinkingMode::Level(level) => {
            let b = effort_to_budget(level)?;
            b as f64
        }
        ThinkingMode::Auto => return Some(-1.0),
        ThinkingMode::None => return None,
    };
    if let Some(range) = &caps.thinking_range {
        if let Some(min) = range.min
            && budget < min as f64
        {
            budget = min as f64;
        }
        if let Some(max) = range.max
            && budget > max as f64
        {
            budget = max as f64;
        }
    }
    Some(budget)
}

/// `toLevel(cfg)`.
fn to_level(cfg: &ThinkingMode) -> Option<String> {
    match cfg {
        ThinkingMode::Level(l) => Some(l.clone()),
        ThinkingMode::Budget(b) => Some(budget_to_level(*b).unwrap_or("medium").to_string()),
        ThinkingMode::Auto => Some("auto".to_string()),
        ThinkingMode::None => None,
    }
}

/// `normalizeOpenAILevel(level, supportedLevels)`.
fn normalize_openai_level(level: &str, supported: Option<&[String]>) -> String {
    if level != "max" && level != "ultra" {
        return level.to_string();
    }
    if supported.is_some_and(|s| s.iter().any(|l| l == level)) {
        return level.to_string();
    }
    if level == "ultra" && supported.is_some_and(|s| s.iter().any(|l| l == "max")) {
        return "max".to_string();
    }
    "xhigh".to_string()
}

/// `toGeminiThinkingLevel(cfg)`.
fn to_gemini_thinking_level(cfg: &ThinkingMode) -> String {
    let raw = match cfg {
        ThinkingMode::Auto => "high".to_string(),
        other => to_level(other).unwrap_or_else(|| "high".to_string()),
    };
    effort_to_thinking_level(&raw)
}

/// `toKimiReasoningEffort(cfg)`.
fn to_kimi_reasoning_effort(cfg: &ThinkingMode) -> Option<String> {
    let level = to_level(cfg)?;
    Some(match level.as_str() {
        "auto" => "high".to_string(),
        "minimal" => "low".to_string(),
        "xhigh" => "max".to_string(),
        "low" | "medium" | "high" | "max" => level,
        _ => return None,
    })
}

/// `geminiBudgetOutputFloor(budget)`.
fn gemini_budget_output_floor(budget: f64) -> i64 {
    if budget == -1.0 || !budget.is_finite() {
        return 32768;
    }
    if budget <= 1024.0 {
        8192
    } else if budget <= 8192.0 {
        16384
    } else if budget <= 24576.0 {
        32768
    } else {
        65535
    }
}

/// `geminiLevelOutputFloor(level)`.
fn gemini_level_output_floor(level: &str) -> i64 {
    match level {
        "minimal" => 4096,
        "low" => 8192,
        "medium" => 16384,
        _ => 65535,
    }
}

/// `getGeminiGenerationConfig(body)`: gemini-cli wraps the whole request in
/// `{ request: { generationConfig } }`, so the envelope wins when present.
fn gemini_generation_config(body: &mut Value) -> &mut Map<String, Value> {
    let use_envelope = body.get("request").is_some_and(Value::is_object);
    let holder = if use_envelope {
        body.get_mut("request").expect("checked above")
    } else {
        body
    };
    let holder = holder.as_object_mut().expect("caller holds an object");
    holder
        .entry("generationConfig".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    holder
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
        .expect("just inserted")
}

/// `setGeminiThinking(body, tc)`.
fn set_gemini_thinking(body: &mut Value, tc: Value) {
    gemini_generation_config(body).insert("thinkingConfig".to_string(), tc);
}

/// `ensureGeminiOutputFloor(body, floor, caps)`.
fn ensure_gemini_output_floor(body: &mut Value, floor: i64, caps: &Capabilities) {
    let cap = caps.max_output;
    let target = floor.min(cap);
    let gc = gemini_generation_config(body);
    let current = gc.get("maxOutputTokens").and_then(Value::as_i64);
    if current.is_none_or(|c| c < target) {
        gc.insert("maxOutputTokens".to_string(), json!(target));
    }
}

/// `stripAll(body)`: remove every known thinking field.
fn strip_all(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for key in [
        "thinking",
        "reasoning_effort",
        "reasoning",
        "thinkingConfig",
        "enable_thinking",
        "thinking_budget",
        "output_config",
    ] {
        obj.remove(key);
    }
    if let Some(gc) = obj
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
    {
        gc.remove("thinkingConfig");
    }
    if let Some(gc) = obj
        .get_mut("request")
        .and_then(|r| r.get_mut("generationConfig"))
        .and_then(Value::as_object_mut)
    {
        gc.remove("thinkingConfig");
    }
    if let Some(params) = obj.get_mut("params").and_then(Value::as_object_mut) {
        params.remove("reasoning_effort");
        params.remove("thinking");
    }
}

/// `applyFormat(fmt, body, cfg, caps, supportedLevels, display)`.
fn apply_format(
    fmt: &str,
    body: &mut Value,
    cfg: &ThinkingMode,
    caps: &Capabilities,
    supported_levels: Option<&[String]>,
    display: Option<&str>,
) {
    let none = *cfg == ThinkingMode::None;
    let can_disable = caps.thinking_can_disable;
    // A model that cannot disable thinking clamps "none" to minimal effort.
    let eff = if none && !can_disable {
        ThinkingMode::Level("minimal".to_string())
    } else {
        cfg.clone()
    };
    let display_field = display.map(|d| ("display".to_string(), json!(d)));

    let obj = body.as_object_mut().expect("caller holds an object");

    match fmt {
        "openai" => {
            if none && can_disable {
                obj.insert("reasoning_effort".into(), json!("none"));
                return;
            }
            if let Some(level) = to_level(&eff) {
                obj.insert(
                    "reasoning_effort".into(),
                    json!(normalize_openai_level(&level, supported_levels)),
                );
            }
        }
        "claude-adaptive" => {
            if none && can_disable {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
                return;
            }
            if can_disable {
                let mut thinking = Map::new();
                thinking.insert("type".into(), json!("adaptive"));
                if let Some((k, v)) = &display_field {
                    thinking.insert(k.clone(), v.clone());
                }
                obj.insert("thinking".into(), Value::Object(thinking));
            } else {
                obj.remove("thinking");
            }
            let level = to_level(&eff);
            let effort = match level.as_deref() {
                Some("xhigh") | Some("auto") => "high".to_string(),
                other => other.unwrap_or("high").to_string(),
            };
            obj.insert("output_config".into(), json!({ "effort": effort }));
        }
        "claude-budget" => {
            if none && can_disable {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
                return;
            }
            let budget = to_budget(&eff, caps);
            let mut thinking = Map::new();
            thinking.insert("type".into(), json!("enabled"));
            if budget != Some(-1.0) {
                thinking.insert(
                    "budget_tokens".into(),
                    js_json_number(budget.filter(|b| *b > 0.0).unwrap_or(8192.0)),
                );
            }
            if let Some((k, v)) = &display_field {
                thinking.insert(k.clone(), v.clone());
            }
            obj.insert("thinking".into(), Value::Object(thinking));
        }
        "gemini-level" => {
            let level = if none {
                "minimal".to_string()
            } else {
                to_gemini_thinking_level(&eff)
            };
            set_gemini_thinking(
                body,
                json!({ "thinkingLevel": level, "includeThoughts": level != "minimal" }),
            );
            ensure_gemini_output_floor(body, gemini_level_output_floor(&level), caps);
        }
        "gemini-budget" => {
            if none && can_disable {
                set_gemini_thinking(
                    body,
                    json!({ "thinkingBudget": 0, "includeThoughts": false }),
                );
                return;
            }
            let budget = to_budget(&eff, caps);
            let value = budget.unwrap_or(-1.0);
            set_gemini_thinking(
                body,
                json!({ "thinkingBudget": js_json_number(value), "includeThoughts": true }),
            );
            ensure_gemini_output_floor(body, gemini_budget_output_floor(value), caps);
        }
        "zai" => {
            // Z.ai ignores thinking.disabled, so it must be turned off with
            // enable_thinking:false.
            if none && can_disable {
                let obj = body.as_object_mut().expect("caller holds an object");
                obj.insert("enable_thinking".into(), json!(false));
                obj.remove("thinking");
                return;
            }
            obj.insert("thinking".into(), json!({"type": "enabled"}));
            // reasoning_effort is only read from GLM-5.2 onward; older GLM
            // ignores it, so skip on unsupported models rather than send a
            // field the API does not recognise.
            if caps.thinking_effort_supported {
                let level = to_level(&eff);
                let effort = match level.as_deref() {
                    Some("low") | Some("minimal") => "low",
                    Some("high") | Some("medium") => "high",
                    _ => "max",
                };
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
        "qwen" => {
            if none && can_disable {
                obj.insert("enable_thinking".into(), json!(false));
                return;
            }
            obj.insert("enable_thinking".into(), json!(true));
            let budget = to_budget(&eff, caps);
            if let Some(b) = budget.filter(|b| *b > 0.0) {
                obj.insert("thinking_budget".into(), js_json_number(b));
            }
        }
        "deepseek" => {
            if none && can_disable {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
                return;
            }
            obj.insert("thinking".into(), json!({"type": "enabled"}));
            let level = to_level(&eff);
            let effort = match level.as_deref() {
                Some("xhigh") | Some("max") => "max",
                _ => "high",
            };
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        "kimi" => {
            if none && can_disable {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
                return;
            }
            if let Some(effort) = to_kimi_reasoning_effort(&eff) {
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
        "minimax" => {
            let kind = if none && can_disable {
                "disabled"
            } else {
                "adaptive"
            };
            obj.insert("thinking".into(), json!({ "type": kind }));
        }
        "hunyuan" => {
            if none && can_disable {
                obj.insert("thinking".into(), json!({"type": "disabled"}));
                return;
            }
            let budget = to_budget(&eff, caps);
            let thinking = if budget == Some(-1.0) {
                json!({ "type": "enabled" })
            } else {
                json!({
                    "type": "enabled",
                    "budget_tokens": js_json_number(budget.filter(|b| *b > 0.0).unwrap_or(8192.0)),
                })
            };
            obj.insert("thinking".into(), thinking);
        }
        "step" => {
            if none && can_disable {
                return;
            }
            if let Some(level) = to_level(&eff) {
                let effort = match level.as_str() {
                    "xhigh" | "max" => "high",
                    other => other,
                };
                obj.insert("reasoning_effort".into(), json!(effort));
            }
        }
        "commandcode" => {
            // The native CLI sends reasoning_effort inside params of the
            // /alpha/generate envelope.
            obj.entry("params".to_string())
                .or_insert_with(|| Value::Object(Map::new()));
            let obj = body.as_object_mut().expect("caller holds an object");
            let params = obj
                .get_mut("params")
                .and_then(Value::as_object_mut)
                .expect("just inserted");
            if none && can_disable {
                params.remove("reasoning_effort");
                return;
            }
            if let Some(level) = to_level(&eff) {
                params.insert("reasoning_effort".into(), json!(level));
            }
        }
        _ => {}
    }
}

/// `applyThinking(targetFormat, model, body, provider, intent)`: normalize
/// thinking for the resolved target format, in place.
///
/// `intent` is the config captured off the original body before translation;
/// when omitted the current body is read instead.
pub fn apply_thinking(
    target_format: &str,
    model: &str,
    body: &mut Value,
    provider: Option<&str>,
    intent: Option<&ThinkingMode>,
) {
    if !body.is_object() {
        return;
    }

    let (clean_model, override_) = parse_suffix(model);
    let extracted = extract_thinking(body);
    let cfg = override_.as_ref().or(intent).or(extracted.as_ref());

    let caps = get_capabilities_for_model(provider, &clean_model);

    // A model that cannot reason must not carry stray thinking fields.
    if !caps.reasoning {
        strip_all(body);
        return;
    }
    let Some(cfg) = cfg else {
        return;
    };

    let fmt = resolve_format(target_format, provider, &caps);
    let supported_levels = crate::catalog::get_thinking_levels(provider, &clean_model);
    let supported_levels: Option<Vec<String>> =
        supported_levels.map(|levels| levels.into_iter().map(str::to_string).collect());
    // Anthropic's `display` (summarized | omitted) decides whether thinking text
    // comes back at all, so keep what the client asked for instead of resetting it.
    let display = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .and_then(Value::as_str)
        .map(str::to_string);
    strip_all(body);
    apply_format(
        &fmt,
        body,
        cfg,
        &caps,
        supported_levels.as_deref(),
        display.as_deref(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_parsing_covers_level_number_auto_and_none() {
        assert_eq!(
            parse_suffix("claude-sonnet-4.5(high)"),
            (
                "claude-sonnet-4.5".into(),
                Some(ThinkingMode::Level("high".into()))
            )
        );
        assert_eq!(
            parse_suffix("gpt-5(8192)"),
            ("gpt-5".into(), Some(ThinkingMode::Budget(8192.0)))
        );
        assert_eq!(
            parse_suffix("gpt-5(auto)"),
            ("gpt-5".into(), Some(ThinkingMode::Auto))
        );
        assert_eq!(
            parse_suffix("gpt-5(none)"),
            ("gpt-5".into(), Some(ThinkingMode::None))
        );
        assert_eq!(
            parse_suffix("gpt-5(off)"),
            ("gpt-5".into(), Some(ThinkingMode::None))
        );
        assert_eq!(
            parse_suffix("gpt-5(ultra)"),
            ("gpt-5".into(), Some(ThinkingMode::Level("ultra".into())))
        );
        // An unknown word is not an override, and neither is a bare model id.
        assert_eq!(parse_suffix("gpt-5(bogus)"), ("gpt-5".into(), None));
        assert_eq!(parse_suffix("gpt-5"), ("gpt-5".into(), None));
        // Nested parens do not match the `[^()]+` group.
        assert_eq!(
            parse_suffix("gpt-5((high))"),
            ("gpt-5((high))".into(), None)
        );
    }

    #[test]
    fn strip_thinking_suffix_is_a_noop_without_a_group() {
        assert_eq!(
            strip_thinking_suffix("claude-sonnet-4.5(high)"),
            "claude-sonnet-4.5"
        );
        assert_eq!(
            strip_thinking_suffix("claude-sonnet-4.5"),
            "claude-sonnet-4.5"
        );
    }

    #[test]
    fn budget_and_level_maps_round_trip_at_the_boundaries() {
        assert_eq!(effort_to_budget("high"), Some(24576));
        assert_eq!(effort_to_budget("bogus"), None);
        assert_eq!(budget_to_level(768.0), Some("minimal"));
        assert_eq!(budget_to_level(769.0), Some("low"));
        assert_eq!(budget_to_level(80384.0), Some("xhigh"));
        assert_eq!(budget_to_level(80385.0), Some("max"));
        assert_eq!(budget_to_level(0.0), None);
        assert_eq!(effort_to_thinking_level("none"), "minimal");
        assert_eq!(effort_to_thinking_level("max"), "high");
        assert_eq!(effort_to_thinking_level("medium"), "medium");
    }

    #[test]
    fn intent_is_read_from_every_client_shape() {
        assert_eq!(
            extract_thinking(&json!({"output_config": {"effort": "xhigh"}})),
            Some(ThinkingMode::Level("xhigh".into()))
        );
        assert_eq!(
            extract_thinking(&json!({"reasoning_effort": "low"})),
            Some(ThinkingMode::Level("low".into()))
        );
        assert_eq!(
            extract_thinking(&json!({"reasoning": {"effort": "medium"}})),
            Some(ThinkingMode::Level("medium".into()))
        );
        assert_eq!(
            extract_thinking(&json!({"thinking": {"type": "enabled", "budget_tokens": 4096}})),
            Some(ThinkingMode::Budget(4096.0))
        );
        assert_eq!(
            extract_thinking(&json!({"thinking": {"type": "disabled"}})),
            Some(ThinkingMode::None)
        );
        assert_eq!(
            extract_thinking(&json!({"thinking": {"type": "adaptive"}})),
            Some(ThinkingMode::Auto)
        );
        assert_eq!(
            extract_thinking(
                &json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "HIGH"}}})
            ),
            Some(ThinkingMode::Level("high".into()))
        );
        assert_eq!(
            extract_thinking(
                &json!({"request": {"generationConfig": {"thinkingConfig": {"thinkingBudget": -1}}}})
            ),
            Some(ThinkingMode::Auto)
        );
        assert_eq!(
            extract_thinking(&json!({"enable_thinking": false})),
            Some(ThinkingMode::None)
        );
        assert_eq!(extract_thinking(&json!({"messages": []})), None);
    }

    #[test]
    fn claude_budget_format_maps_a_level_to_tokens() {
        let mut body = json!({"model": "claude-sonnet-4.5"});
        apply_thinking("claude", "claude-sonnet-4.5(high)", &mut body, None, None);
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["thinking"]["budget_tokens"], json!(24576));
    }

    #[test]
    fn a_non_reasoning_model_has_every_thinking_field_stripped() {
        let mut body = json!({
            "reasoning_effort": "high",
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 100}},
        });
        // llama-3 has no reasoning capability.
        apply_thinking("openai", "llama-3.1-8b", &mut body, None, None);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn gemini_level_format_nests_under_generation_config() {
        let mut body = json!({});
        apply_thinking(
            "gemini",
            "gemini-3-pro",
            &mut body,
            None,
            Some(&ThinkingMode::Level("high".into())),
        );
        let tc = &body["generationConfig"]["thinkingConfig"];
        assert_eq!(tc["thinkingLevel"], json!("high"));
        assert_eq!(tc["includeThoughts"], json!(true));
        // The output floor is raised so the thinking budget fits.
        assert_eq!(body["generationConfig"]["maxOutputTokens"], json!(65535));
    }

    #[test]
    fn gemini_cli_envelope_is_targeted_not_the_top_level() {
        let mut body = json!({"request": {"contents": []}});
        apply_thinking(
            "gemini-cli",
            "gemini-3-pro",
            &mut body,
            None,
            Some(&ThinkingMode::Level("low".into())),
        );
        assert!(
            body.get("generationConfig").is_none(),
            "envelope must be used"
        );
        assert_eq!(
            body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("low")
        );
    }

    #[test]
    fn a_native_only_format_is_not_used_on_an_openai_wire() {
        // claude-adaptive would be meaningless on the openai wire, so the wire's
        // native format wins and the level lands in reasoning_effort.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "claude-opus-4.8",
            &mut body,
            None,
            Some(&ThinkingMode::Level("medium".into())),
        );
        assert_eq!(body["reasoning_effort"], json!("medium"));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn openai_format_clamps_max_to_xhigh_when_unsupported() {
        // codex's base level set stops at xhigh, so "max" clamps down.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "gpt-5.1-codex",
            &mut body,
            Some("codex"),
            Some(&ThinkingMode::Level("max".into())),
        );
        assert_eq!(body["reasoning_effort"], json!("xhigh"));

        // gpt-5.6-sol's set carries "max", so it survives.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "gpt-5.6-sol",
            &mut body,
            Some("codex"),
            Some(&ThinkingMode::Level("max".into())),
        );
        assert_eq!(body["reasoning_effort"], json!("max"));
    }

    #[test]
    fn zai_uses_enable_thinking_to_disable_and_gates_reasoning_effort() {
        // glm-5.3 resolves through the `*glm-5.3*` pattern, which carries
        // thinkingEffortSupported, so reasoning_effort is sent.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "glm-5.3",
            &mut body,
            None,
            Some(&ThinkingMode::Level("xhigh".into())),
        );
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("max"));

        // glm-5.1 ignores reasoning_effort, so it must not be sent.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "glm-5.1",
            &mut body,
            None,
            Some(&ThinkingMode::Level("high".into())),
        );
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert!(body.get("reasoning_effort").is_none());

        // Disabling goes through enable_thinking, not thinking.disabled.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "glm-5.3",
            &mut body,
            None,
            Some(&ThinkingMode::None),
        );
        assert_eq!(body["enable_thinking"], json!(false));
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn the_exact_capability_row_beats_the_pattern_for_zai() {
        // glm-5.2 has an exact MODEL_CAPABILITIES row, and exact wins over the
        // `*glm-5.2*` pattern, so the pattern's thinkingEffortSupported never
        // applies and reasoning_effort stays off.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "glm-5.2",
            &mut body,
            None,
            Some(&ThinkingMode::Level("xhigh".into())),
        );
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn a_model_that_cannot_disable_clamps_none_to_minimal() {
        // kimi-k3 declares thinkingCanDisable:false, so "none" becomes minimal.
        let mut body = json!({});
        apply_thinking(
            "openai",
            "kimi-k3",
            &mut body,
            None,
            Some(&ThinkingMode::None),
        );
        assert_eq!(
            body["reasoning_effort"],
            json!("low"),
            "kimi maps minimal to low"
        );
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn commandcode_writes_reasoning_effort_into_params() {
        let mut body = json!({});
        apply_thinking(
            "commandcode",
            "some-model",
            &mut body,
            Some("commandcode"),
            Some(&ThinkingMode::Level("high".into())),
        );
        assert_eq!(body["params"]["reasoning_effort"], json!("high"));
    }

    #[test]
    fn the_display_field_survives_a_claude_adaptive_rewrite() {
        let mut body = json!({"thinking": {"type": "adaptive", "display": "summarized"}});
        apply_thinking("claude", "claude-opus-4.8", &mut body, None, None);
        assert_eq!(body["thinking"]["type"], json!("adaptive"));
        assert_eq!(body["thinking"]["display"], json!("summarized"));
        assert_eq!(body["output_config"]["effort"], json!("high"));
    }
}
