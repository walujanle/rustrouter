//! `https://chatgpt.com/backend-api/codex/responses` validates every function
//! tool's `parameters` with a regex engine that does not implement Unicode
//! property escapes. A `pattern` like
//!
//! ```text
//! ^(?!__.*__$)[^\p{Cc}\p{Cf}\p{Zl}\p{Zp}"\\./\[\]]{1,200}$
//! ```
//!
//! is valid ECMAScript, but Codex answers
//! `400 Invalid schema for function 'Artifact': '^\p{Cc}...' is not a 'regex'`.
//! The request is then deterministically malformed for this provider, so every
//! account fails identically and a combo pays a full failover before landing
//! somewhere that accepts it.
//!
//! Scope guardrail: this is **not** a global schema sanitiser. Providers that
//! do support `\p{...}` keep the constraint untouched — the strip runs only on
//! the Codex dispatch path, and only on `pattern` strings that actually contain
//! a property escape. Everything else passes through byte-identical.

use serde_json::{Map, Value};

/// The backslash count matters: `\p{Cc}` is an escape, `\\p{Cc}` is a literal
/// `p`. So the run of backslashes immediately before the `p` must be odd.
pub fn has_unicode_property_escape(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        // Count the whole backslash run starting here.
        let run_start = index;
        while index < bytes.len() && bytes[index] == b'\\' {
            index += 1;
        }
        let run = index - run_start;
        if run % 2 == 1
            && index < bytes.len()
            && matches!(bytes[index], b'p' | b'P')
            && bytes.get(index + 1) == Some(&b'{')
        {
            return true;
        }
    }
    false
}

/// How many `pattern` constraints the pass removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StripStats {
    pub removed: usize,
}

/// Copy-on-write: an untouched subtree is returned by value unchanged, and the
/// caller compares the result to detect a no-op. `properties` is walked as a
/// map of arbitrary property *names* — a property literally called `pattern` is
/// a name, not a schema keyword, and must never be dropped.
pub fn strip_codex_unsupported_patterns(schema: &Value) -> (Value, StripStats) {
    let mut stats = StripStats::default();
    let out = strip_node(schema, &mut stats);
    (out, stats)
}

fn strip_node(node: &Value, stats: &mut StripStats) -> Value {
    match node {
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| strip_node(item, stats)).collect())
        }
        Value::Object(map) => {
            let mut next = Map::new();
            for (key, value) in map {
                if key == "pattern" && value.as_str().is_some_and(has_unicode_property_escape) {
                    stats.removed += 1;
                    continue;
                }
                if key == "properties"
                    && let Value::Object(props) = value
                {
                    // Keys here are property names, so recurse into each
                    // *value* without ever reading the name as a keyword.
                    let mut cleaned = Map::new();
                    for (prop_name, prop_schema) in props {
                        cleaned.insert(prop_name.clone(), strip_node(prop_schema, stats));
                    }
                    next.insert(key.clone(), Value::Object(cleaned));
                    continue;
                }
                next.insert(key.clone(), strip_node(value, stats));
            }
            Value::Object(next)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_property_escape_is_detected_with_an_odd_backslash_run() {
        assert!(has_unicode_property_escape(r"^\p{Cc}+$"));
        assert!(has_unicode_property_escape(r"\P{L}"));
        // Three backslashes: the last one still escapes the `p`.
        assert!(has_unicode_property_escape(r"\\\p{Cc}"));
    }

    #[test]
    fn an_escaped_backslash_is_not_an_escape() {
        // `\\p{Cc}` is a literal backslash followed by a literal "p".
        assert!(!has_unicode_property_escape(r"\\p{Cc}"));
        assert!(!has_unicode_property_escape("plain"));
        assert!(!has_unicode_property_escape(r"\pCc")); // no brace
        assert!(!has_unicode_property_escape(r"\d{2}")); // not a property
        assert!(!has_unicode_property_escape(r"p{Cc}")); // no backslash
        assert!(!has_unicode_property_escape(""));
    }

    #[test]
    fn only_patterns_with_a_property_escape_are_removed() {
        let schema = json!({
            "type": "object",
            "properties": {
                "artifact": {"type": "string", "pattern": r"^(?!__.*__$)[^\p{Cc}\p{Cf}]{1,200}$"},
                "safe": {"type": "string", "pattern": r"^\d{4}-\d{2}-\d{2}$"},
                "escaped": {"type": "string", "pattern": r"\\p{Cc}"},
            },
        });
        let (out, stats) = strip_codex_unsupported_patterns(&schema);
        assert_eq!(stats.removed, 1);
        assert!(out["properties"]["artifact"].get("pattern").is_none());
        assert_eq!(out["properties"]["artifact"]["type"], json!("string"));
        // The valid pattern survives byte-identical.
        assert_eq!(
            out["properties"]["safe"]["pattern"],
            json!(r"^\d{4}-\d{2}-\d{2}$")
        );
        assert_eq!(out["properties"]["escaped"]["pattern"], json!(r"\\p{Cc}"));
    }

    #[test]
    fn a_compatible_schema_comes_back_unchanged() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string", "pattern": "^a+$"}},
            "required": ["a"],
        });
        let (out, stats) = strip_codex_unsupported_patterns(&schema);
        assert_eq!(out, schema);
        assert_eq!(stats.removed, 0);
    }

    #[test]
    fn a_property_named_pattern_is_a_name_not_a_keyword() {
        let schema = json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "the pattern to use"},
            },
        });
        let (out, stats) = strip_codex_unsupported_patterns(&schema);
        assert_eq!(stats.removed, 0);
        assert_eq!(out, schema);
    }

    #[test]
    fn nested_arrays_and_objects_are_walked() {
        let schema = json!({
            "type": "object",
            "properties": {
                "list": {
                    "type": "array",
                    "items": {"anyOf": [
                        {"type": "string", "pattern": r"\p{L}"},
                        {"type": "string", "pattern": "^ok$"},
                    ]},
                },
            },
        });
        let (out, stats) = strip_codex_unsupported_patterns(&schema);
        assert_eq!(stats.removed, 1);
        let any_of = &out["properties"]["list"]["items"]["anyOf"];
        assert!(any_of[0].get("pattern").is_none());
        assert_eq!(any_of[1]["pattern"], json!("^ok$"));
    }

    #[test]
    fn scalar_and_empty_inputs_are_safe() {
        for schema in [json!(null), json!(42), json!("text"), json!([]), json!({})] {
            let (out, stats) = strip_codex_unsupported_patterns(&schema);
            assert_eq!(out, schema);
            assert_eq!(stats.removed, 0);
        }
    }
}
