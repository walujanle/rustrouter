//! The Cursor and Claude default combo presets.
//!
//! Both sources are kept as route contracts but yield nothing: there is no
//! `cu` model table, and `cc` is not a live alias either. `is_preset_source`
//! still accepts both so the route answers with an empty list rather than a
//! 400 for a source the dashboard still offers.

use std::collections::HashSet;

use serde_json::Value;

/// `VALID_COMBO_NAME_REGEX`.
#[cfg(test)]
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// `PRESET_SOURCES`.
pub fn is_preset_source(source: &str) -> bool {
    matches!(source, "cursor" | "claude")
}

/// `buildCursorPresetItems()`. Always empty here: there is no `cu` table.
fn build_cursor_preset_items() -> Vec<Value> {
    Vec::new()
}

/// `buildClaudePresetItems()`. Always empty here: `cc` is no longer a live alias.
fn build_claude_preset_items() -> Vec<Value> {
    Vec::new()
}

/// `buildPresetItems(source, { existingNames })`: each item plus an `exists`
/// flag.
pub fn build_preset_items(source: &str, existing_names: &[String]) -> Vec<Value> {
    if !is_preset_source(source) {
        return Vec::new();
    }
    let items = if source == "cursor" {
        build_cursor_preset_items()
    } else {
        build_claude_preset_items()
    };
    let existing: HashSet<&str> = existing_names.iter().map(String::as_str).collect();
    items
        .into_iter()
        .map(|mut item| {
            let exists = item
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| existing.contains(n));
            if let Value::Object(map) = &mut item {
                map.insert("exists".into(), Value::Bool(exists));
            }
            item
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_source_is_empty_after_the_provider_left() {
        assert!(build_preset_items("cursor", &[]).is_empty());
    }

    #[test]
    fn claude_source_is_empty_after_the_alias_left() {
        assert!(build_preset_items("claude", &[]).is_empty());
        assert!(build_preset_items("claude", &["sonnet".to_string()]).is_empty());
    }

    #[test]
    fn an_unknown_source_is_not_a_preset() {
        assert!(!is_preset_source("nope"));
        assert!(build_preset_items("nope", &[]).is_empty());
    }

    #[test]
    fn preset_names_are_valid() {
        assert!(is_valid_name("sonnet"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
    }
}
