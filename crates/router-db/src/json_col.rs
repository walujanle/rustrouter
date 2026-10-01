//! JSON-in-TEXT column helpers.
//!
//! Every write goes through `JSON.stringify(value ?? null)`. Two consequences
//! the Rust side must reproduce:
//!
//! 1. `undefined` becomes `null` **with the key kept** — so structs must never
//!    carry `skip_serializing_if`. Serialising a `serde_json::Value` satisfies
//!    this by construction.
//! 2. Key order is insertion order. `serde_json` is built with
//!    `preserve_order`, so `Value::Object` is an `IndexMap`.

use serde_json::Value;

/// Parse a JSON column, falling back when the text is absent or malformed.
pub fn parse_json(s: &str, fallback: Value) -> Value {
    serde_json::from_str(s).unwrap_or(fallback)
}

/// Parse a JSON column that may be NULL, falling back on absence or garbage.
pub fn parse_json_opt(s: Option<&str>, fallback: Value) -> Value {
    match s {
        Some(s) => parse_json(s, fallback),
        None => fallback,
    }
}

/// Serialise a value the way `JSON.stringify(value ?? null)` does.
///
/// `serde_json::to_string` on a `Value` is infallible in practice (no maps with
/// non-string keys, no custom serializers), so a failure here would be a bug in
/// serde itself; the `null` fallback keeps the write path panic-free.
pub fn stringify_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// `stringify_json` where the value may be absent; absence writes `null`.
pub fn stringify_json_or_null(value: Option<&Value>) -> String {
    match value {
        Some(v) => stringify_json(v),
        None => "null".to_string(),
    }
}

/// Truthiness for a JSON value: `null`, `false`, `0`, `""`, and `NaN` are
/// falsy; everything else — including `[]` and `{}` — is truthy.
pub fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f == 0.0),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// A falsy value becomes JSON `null`; anything else is kept.
pub fn falsy_to_null(v: Option<&Value>) -> Value {
    match v {
        None => Value::Null,
        Some(v) if is_falsy(v) => Value::Null,
        Some(v) => v.clone(),
    }
}

/// A column read: an absent `Option<String>` is JSON `null`. Every row mapper
/// needs this, so it lives here rather than once per repo file.
pub fn opt_string(v: Option<String>) -> Value {
    match v {
        Some(s) => Value::String(s),
        None => Value::Null,
    }
}

/// Remove `keys` from an object and return the remainder, like a destructuring
/// rest: `const { a, b, ...rest } = obj`.
///
/// Order of the remaining keys is preserved, matching object spread.
pub fn take_rest(obj: &Value, keys: &[&str]) -> Value {
    match obj {
        Value::Object(map) => {
            let mut out = map.clone();
            for k in keys {
                out.shift_remove(*k);
            }
            Value::Object(out)
        }
        _ => Value::Object(Default::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stringify_keeps_insertion_order() {
        let v = json!({ "z": 1, "a": 2, "m": 3 });
        assert_eq!(stringify_json(&v), r#"{"z":1,"a":2,"m":3}"#);
    }

    #[test]
    fn null_is_written_as_null() {
        assert_eq!(stringify_json_or_null(None), "null");
        assert_eq!(stringify_json(&Value::Null), "null");
    }

    #[test]
    fn undefined_becomes_null_with_key_kept() {
        // `JSON.stringify(v ?? null)` on an object whose field was `undefined`
        // emits the key with a null value. A `Value::Null` in an object is the
        // same shape.
        let v = json!({ "a": Value::Null, "b": 1 });
        assert_eq!(stringify_json(&v), r#"{"a":null,"b":1}"#);
    }

    #[test]
    fn take_rest_removes_and_preserves_order() {
        let v = json!({ "id": "x", "name": "n", "keep1": 1, "keep2": 2 });
        assert_eq!(
            stringify_json(&take_rest(&v, &["id", "name"])),
            r#"{"keep1":1,"keep2":2}"#
        );
    }

    #[test]
    fn parse_json_uses_fallback_on_garbage() {
        assert_eq!(parse_json("{", json!({})), json!({}));
        assert_eq!(parse_json_opt(None, json!([])), json!([]));
    }

    #[test]
    fn falsiness_matches_javascript() {
        for falsy in [json!(null), json!(false), json!(0), json!("")] {
            assert!(is_falsy(&falsy), "{falsy} should be falsy");
        }
        // `[]` and `{}` are truthy.
        for truthy in [json!(1), json!(true), json!("x"), json!([]), json!({})] {
            assert!(!is_falsy(&truthy), "{truthy} should be truthy");
        }
        assert_eq!(falsy_to_null(None), json!(null));
        assert_eq!(falsy_to_null(Some(&json!(""))), json!(null));
        assert_eq!(falsy_to_null(Some(&json!("x"))), json!("x"));
    }
}
