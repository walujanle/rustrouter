//! User pricing overrides.
//!
//! The built-in `PROVIDER_PRICING` table lives in the provider registry crate.
//! This module owns only the `pricing` kv scope and the merge rules; the
//! catalog is injected as a `&Value` so the two crates stay independent.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{parse_json, stringify_json};
use crate::kv_store::{self, SCOPE_PRICING};

/// The pricing cache TTL.
pub const CACHE_TTL_MS: i64 = 5000;

/// User pricing as a map of provider to model to pricing.
pub fn get_user_pricing(conn: &Connection) -> DbResult<Value> {
    let mut out = Map::new();
    for (k, v) in kv_store::get_all(conn, SCOPE_PRICING)? {
        out.insert(k, parse_json(&v, json!({})));
    }
    Ok(Value::Object(out))
}

/// Built-in pricing with user overrides merged over it.
///
/// The result is not cached: a per-process mutable global with a five-second
/// TTL would need an invalidation surface a read of a few dozen rows does not
/// justify. Callers that need it can memoise at their own layer.
pub fn get_pricing(conn: &Connection, provider_pricing: &Value) -> DbResult<Value> {
    let user_pricing = get_user_pricing(conn)?;
    let mut merged: Map<String, Value> = Map::new();

    if let Value::Object(builtin) = provider_pricing {
        for (provider, models) in builtin {
            let mut entry = match models {
                Value::Object(m) => m.clone(),
                _ => Map::new(),
            };
            if let Some(Value::Object(user_models)) = user_pricing.get(provider) {
                for (model, pricing) in user_models {
                    let next = match entry.get(model) {
                        Some(Value::Object(existing)) => {
                            let mut merged_model = existing.clone();
                            if let Value::Object(overrides) = pricing {
                                for (k, v) in overrides {
                                    merged_model.insert(k.clone(), v.clone());
                                }
                            }
                            Value::Object(merged_model)
                        }
                        _ => pricing.clone(),
                    };
                    entry.insert(model.clone(), next);
                }
            }
            merged.insert(provider.clone(), Value::Object(entry));
        }
    }

    // User-only providers, and user models under a provider the built-in table
    // does not list, are appended rather than dropped.
    if let Value::Object(user) = &user_pricing {
        for (provider, models) in user {
            match merged.get_mut(provider) {
                None => {
                    merged.insert(provider.clone(), models.clone());
                }
                Some(Value::Object(existing)) => {
                    if let Value::Object(user_models) = models {
                        for (model, pricing) in user_models {
                            if !existing.contains_key(model) {
                                existing.insert(model.clone(), pricing.clone());
                            }
                        }
                    }
                }
                Some(_) => {}
            }
        }
    }

    Ok(Value::Object(merged))
}

/// Pricing for one model: user pricing wins over the catalog.
pub fn get_pricing_for_model(
    conn: &Connection,
    provider: Option<&str>,
    model: &str,
    catalog_lookup: impl Fn(Option<&str>, &str) -> Option<Value>,
) -> DbResult<Option<Value>> {
    if model.is_empty() {
        return Ok(None);
    }
    let user = get_user_pricing(conn)?;
    if let Some(provider) = provider
        && let Some(v) = user.get(provider).and_then(|m| m.get(model))
    {
        return Ok(Some(v.clone()));
    }
    Ok(catalog_lookup(provider, model))
}

/// Per-provider read-modify-write of user pricing. Must run inside a
/// transaction.
pub fn update_pricing(conn: &Connection, pricing_data: &Value) -> DbResult<Value> {
    let Value::Object(providers) = pricing_data else {
        return get_user_pricing(conn);
    };
    for (provider, models) in providers {
        let existing: Option<String> = conn
            .query_row(
                "SELECT value FROM kv WHERE scope = 'pricing' AND key = ?",
                [provider],
                |r| r.get(0),
            )
            .optional()?;
        // `parseJson(row.value, {}) || {}`
        let mut merged = match existing {
            Some(raw) => match parse_json(&raw, json!({})) {
                Value::Object(m) => m,
                _ => Map::new(),
            },
            None => Map::new(),
        };
        if let Value::Object(models) = models {
            for (model, pricing) in models {
                merged.insert(model.clone(), pricing.clone());
            }
        }
        kv_store::set(
            conn,
            SCOPE_PRICING,
            provider,
            &stringify_json(&Value::Object(merged)),
        )?;
    }
    get_user_pricing(conn)
}

/// Reset user pricing for a provider, or a single model. Must run inside a
/// transaction.
pub fn reset_pricing(
    conn: &Connection,
    provider: Option<&str>,
    model: Option<&str>,
) -> DbResult<Value> {
    let Some(provider) = provider else {
        return get_user_pricing(conn);
    };
    let Some(model) = model else {
        kv_store::remove(conn, SCOPE_PRICING, provider)?;
        return get_user_pricing(conn);
    };

    let existing = kv_store::get(conn, SCOPE_PRICING, provider)?;
    let mut current = match existing {
        Some(raw) => match parse_json(&raw, json!({})) {
            Value::Object(m) => m,
            _ => Map::new(),
        },
        None => Map::new(),
    };
    current.shift_remove(model);

    if current.is_empty() {
        kv_store::remove(conn, SCOPE_PRICING, provider)?;
    } else {
        kv_store::set(
            conn,
            SCOPE_PRICING,
            provider,
            &stringify_json(&Value::Object(current)),
        )?;
    }
    get_user_pricing(conn)
}

/// Clear every user pricing override.
pub fn reset_all_pricing(conn: &Connection) -> DbResult<Value> {
    kv_store::clear(conn, SCOPE_PRICING)?;
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("kv").unwrap())
            .unwrap();
        conn
    }

    fn catalog() -> Value {
        json!({
            "openai": {
                "gpt-4o": { "input": 2.5, "output": 10 },
                "gpt-4o-mini": { "input": 0.15, "output": 0.6 }
            }
        })
    }

    #[test]
    fn user_override_merges_over_the_catalog() {
        let conn = db();
        update_pricing(&conn, &json!({ "openai": { "gpt-4o": { "input": 1 } } })).unwrap();
        let merged = get_pricing(&conn, &catalog()).unwrap();
        assert_eq!(merged["openai"]["gpt-4o"]["input"], json!(1));
        assert_eq!(
            merged["openai"]["gpt-4o"]["output"],
            json!(10),
            "sibling lost"
        );
        assert_eq!(merged["openai"]["gpt-4o-mini"]["input"], json!(0.15));
    }

    #[test]
    fn user_only_provider_is_appended() {
        let conn = db();
        update_pricing(&conn, &json!({ "custom": { "m": { "input": 1 } } })).unwrap();
        let merged = get_pricing(&conn, &catalog()).unwrap();
        assert_eq!(merged["custom"]["m"]["input"], json!(1));
        assert_eq!(merged["openai"]["gpt-4o"]["input"], json!(2.5));
    }

    #[test]
    fn user_model_under_a_known_provider_is_appended() {
        let conn = db();
        update_pricing(&conn, &json!({ "openai": { "brand-new": { "input": 9 } } })).unwrap();
        let merged = get_pricing(&conn, &catalog()).unwrap();
        assert_eq!(merged["openai"]["brand-new"]["input"], json!(9));
        assert_eq!(merged["openai"]["gpt-4o"]["input"], json!(2.5));
    }

    #[test]
    fn update_is_a_merge_not_a_replace() {
        let conn = db();
        update_pricing(&conn, &json!({ "openai": { "a": { "input": 1 } } })).unwrap();
        update_pricing(&conn, &json!({ "openai": { "b": { "input": 2 } } })).unwrap();
        let user = get_user_pricing(&conn).unwrap();
        assert_eq!(user["openai"]["a"]["input"], json!(1));
        assert_eq!(user["openai"]["b"]["input"], json!(2));
    }

    #[test]
    fn reset_one_model_keeps_the_rest() {
        let conn = db();
        update_pricing(
            &conn,
            &json!({ "openai": { "a": { "input": 1 }, "b": { "input": 2 } } }),
        )
        .unwrap();
        reset_pricing(&conn, Some("openai"), Some("a")).unwrap();
        let user = get_user_pricing(&conn).unwrap();
        assert!(user["openai"].get("a").is_none());
        assert_eq!(user["openai"]["b"]["input"], json!(2));
    }

    #[test]
    fn resetting_the_last_model_drops_the_provider_row() {
        let conn = db();
        update_pricing(&conn, &json!({ "openai": { "a": { "input": 1 } } })).unwrap();
        reset_pricing(&conn, Some("openai"), Some("a")).unwrap();
        assert!(
            kv_store::get(&conn, SCOPE_PRICING, "openai")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reset_provider_and_reset_all() {
        let conn = db();
        update_pricing(
            &conn,
            &json!({ "a": { "m": { "input": 1 } }, "b": { "m": { "input": 2 } } }),
        )
        .unwrap();
        reset_pricing(&conn, Some("a"), None).unwrap();
        assert_eq!(
            get_user_pricing(&conn).unwrap(),
            json!({ "b": { "m": { "input": 2 } } })
        );

        assert_eq!(reset_all_pricing(&conn).unwrap(), json!({}));
        assert_eq!(get_user_pricing(&conn).unwrap(), json!({}));
    }

    #[test]
    fn reset_without_a_provider_is_a_read() {
        let conn = db();
        update_pricing(&conn, &json!({ "a": { "m": { "input": 1 } } })).unwrap();
        let out = reset_pricing(&conn, None, None).unwrap();
        assert_eq!(out["a"]["m"]["input"], json!(1));
    }

    #[test]
    fn per_model_lookup_prefers_user_pricing() {
        let conn = db();
        update_pricing(&conn, &json!({ "openai": { "gpt-4o": { "input": 1 } } })).unwrap();
        let found = get_pricing_for_model(&conn, Some("openai"), "gpt-4o", |_, _| {
            Some(json!({ "input": 2.5 }))
        })
        .unwrap()
        .unwrap();
        assert_eq!(found["input"], json!(1));

        let catalog_hit =
            get_pricing_for_model(&conn, Some("openai"), "other", |_, m| Some(json!(m)))
                .unwrap()
                .unwrap();
        assert_eq!(catalog_hit, json!("other"));

        assert!(
            get_pricing_for_model(&conn, Some("openai"), "", |_, _| Some(json!(1)))
                .unwrap()
                .is_none(),
            "empty model must short-circuit"
        );
    }
}
