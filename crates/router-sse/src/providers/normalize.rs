//! Canonicalize a provider id a user typed.

use serde_json::Value;

use crate::providers::registry::registry;

/// `normalizeProviderId(provider)`.
///
/// Membership is tested against the registry (keyed by id), falling back to a
/// match on the display name. A non-string is returned unchanged, so the
/// caller's own validation rejects it.
pub fn normalize_provider_id(provider: &Value) -> Value {
    let Some(raw) = provider.as_str() else {
        return provider.clone();
    };
    let trimmed = raw.trim();
    if registry().get(trimmed).is_some() {
        return Value::String(trimmed.to_string());
    }

    let slug: String = {
        let lowered = trimmed.to_ascii_lowercase();
        let mut out = String::with_capacity(lowered.len());
        let mut prev_dash = true; // suppresses a leading dash
        for ch in lowered.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch);
                prev_dash = false;
            } else if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        out
    };
    if registry().get(&slug).is_some() {
        return Value::String(slug);
    }

    let target = trimmed.to_ascii_lowercase();
    for entry in registry().entries() {
        let name = entry
            .display
            .as_ref()
            .and_then(|d| d.get("name"))
            .and_then(Value::as_str);
        if name.is_some_and(|n| n.to_ascii_lowercase() == target) {
            return Value::String(entry.id.clone());
        }
    }

    Value::String(trimmed.to_string())
}

/// `normalizeProviderSpecificData(provider, body, providerSpecificData)`.
///
/// Only `ollama-local` ever had a special case; that provider is not shipped,
/// so this is "copy the object, or null when empty".
pub fn normalize_provider_specific_data(psd: Option<&Value>) -> Value {
    match psd {
        Some(Value::Object(m)) if !m.is_empty() => Value::Object(m.clone()),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_id_and_alias_pass_through() {
        assert_eq!(normalize_provider_id(&json!("claude")), json!("claude"));
        assert_eq!(normalize_provider_id(&json!("  claude  ")), json!("claude"));
        assert_eq!(normalize_provider_id(&json!("cc")), json!("cc"));
    }

    #[test]
    fn slugs_a_display_name_style_input() {
        // "Deepseek" lowercases to an id that exists.
        assert_eq!(normalize_provider_id(&json!("Deepseek")), json!("deepseek"));
        // The slug is `brave-search`; a display-name match would need the
        // registry to name it "Brave Search", which this tree does not.
        assert_eq!(
            normalize_provider_id(&json!("Brave Search")),
            json!("brave-search")
        );
    }

    #[test]
    fn unknown_input_is_returned_trimmed() {
        assert_eq!(normalize_provider_id(&json!(" nope ")), json!("nope"));
        assert_eq!(normalize_provider_id(&json!(42)), json!(42));
    }

    #[test]
    fn psd_is_null_when_absent_or_empty() {
        assert_eq!(normalize_provider_specific_data(None), Value::Null);
        assert_eq!(
            normalize_provider_specific_data(Some(&json!({}))),
            Value::Null
        );
        assert_eq!(
            normalize_provider_specific_data(Some(&json!({"a": 1}))),
            json!({"a": 1})
        );
    }
}
