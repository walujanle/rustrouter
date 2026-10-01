//! Aliases, custom models and disabled models over the `kv` table.
//!
//! There are no `mitmAlias` accessors, but the constant stays in `kv_store` so
//! rows written by another install remain readable.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{parse_json, stringify_json};
use crate::kv_store::{self, SCOPE_CUSTOM_MODELS, SCOPE_DISABLED_MODELS, SCOPE_MODEL_ALIASES};

// ─── modelAliases: key = alias, value = model string ─────────────────────

/// All model aliases as a map, in insertion order.
pub fn get_model_aliases(conn: &Connection) -> DbResult<Value> {
    let mut out = Map::new();
    for (k, v) in kv_store::get_all(conn, SCOPE_MODEL_ALIASES)? {
        out.insert(k, parse_json(&v, Value::Null));
    }
    Ok(Value::Object(out))
}

pub fn set_model_alias(conn: &Connection, alias: &str, model: &Value) -> DbResult<()> {
    kv_store::set(conn, SCOPE_MODEL_ALIASES, alias, &stringify_json(model))
}

pub fn delete_model_alias(conn: &Connection, alias: &str) -> DbResult<()> {
    kv_store::remove(conn, SCOPE_MODEL_ALIASES, alias)
}

// ─── customModels: key = `${providerAlias}|${id}|${type}` ────────────────

/// The `providerAlias|id|type` key a custom model is stored under.
pub fn custom_key(provider_alias: &str, id: &str, model_type: &str) -> String {
    format!("{provider_alias}|{id}|{model_type}")
}

/// A spread of an optional field: only a truthy value is written.
fn truthy(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !crate::json_col::is_falsy(v))
}

/// All custom models, in insertion order.
pub fn get_custom_models(conn: &Connection) -> DbResult<Vec<Value>> {
    Ok(kv_store::get_all(conn, SCOPE_CUSTOM_MODELS)?
        .into_iter()
        .map(|(_, v)| parse_json(&v, Value::Null))
        .collect())
}

/// Add a custom model; returns `true` when a new row was inserted.
///
/// Re-adding merges `name`, `caps` and `transport` without resetting omitted
/// fields. Must run inside a transaction: the read and the write are one unit.
pub fn add_custom_model(
    conn: &Connection,
    provider_alias: &str,
    id: &str,
    model_type: Option<&str>,
    name: Option<&str>,
    caps: Option<&Value>,
    transport: Option<&Value>,
) -> DbResult<bool> {
    let model_type = model_type.unwrap_or("llm");
    let key = custom_key(provider_alias, id, model_type);

    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM kv WHERE scope = 'customModels' AND key = ?",
            [&key],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(raw) = existing {
        // `parseJson(row.value) || {}` — a parsed `null` or `0` falls back.
        let mut prev = match parse_json(&raw, json!({})) {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        if let Some(name) = name.filter(|s| !s.is_empty()) {
            prev.insert("name".into(), json!(name));
        }
        if let Some(caps) = truthy(caps) {
            prev.insert("caps".into(), caps.clone());
        }
        if let Some(transport) = truthy(transport) {
            prev.insert("transport".into(), transport.clone());
        }
        conn.execute(
            "UPDATE kv SET value = ? WHERE scope = 'customModels' AND key = ?",
            rusqlite::params![stringify_json(&Value::Object(prev)), key],
        )?;
        return Ok(false);
    }

    let mut value = Map::new();
    value.insert("providerAlias".into(), json!(provider_alias));
    value.insert("id".into(), json!(id));
    value.insert("type".into(), json!(model_type));
    value.insert(
        "name".into(),
        json!(name.filter(|s| !s.is_empty()).unwrap_or(id)),
    );
    if let Some(caps) = truthy(caps) {
        value.insert("caps".into(), caps.clone());
    }
    if let Some(transport) = truthy(transport) {
        value.insert("transport".into(), transport.clone());
    }
    conn.execute(
        "INSERT INTO kv(scope, key, value) VALUES('customModels', ?, ?)",
        rusqlite::params![key, stringify_json(&Value::Object(value))],
    )?;
    Ok(true)
}

pub fn delete_custom_model(
    conn: &Connection,
    provider_alias: &str,
    id: &str,
    model_type: Option<&str>,
) -> DbResult<()> {
    let key = custom_key(provider_alias, id, model_type.unwrap_or("llm"));
    kv_store::remove(conn, SCOPE_CUSTOM_MODELS, &key)
}

// ─── disabledModels: key = providerAlias, value = string[] ───────────────

/// A map of provider to disabled model ids.
pub fn get_disabled_models(conn: &Connection) -> DbResult<Value> {
    let mut out = Map::new();
    for (k, v) in kv_store::get_all(conn, SCOPE_DISABLED_MODELS)? {
        out.insert(k, parse_json(&v, json!([])));
    }
    Ok(Value::Object(out))
}

/// The disabled model ids for one provider.
pub fn get_disabled_by_provider(conn: &Connection, provider_alias: &str) -> DbResult<Vec<String>> {
    let row = kv_store::get(conn, SCOPE_DISABLED_MODELS, provider_alias)?;
    let Some(raw) = row else {
        return Ok(Vec::new());
    };
    // `parseJson(row.value, []) || []`
    match parse_json(&raw, json!([])) {
        Value::Array(a) => Ok(a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()),
        _ => Ok(Vec::new()),
    }
}

/// Union the given ids into the provider's disabled set, preserving first-seen
/// order.
pub fn disable_models(conn: &Connection, provider_alias: &str, ids: &[String]) -> DbResult<()> {
    if provider_alias.is_empty() {
        return Ok(());
    }
    let mut merged = get_disabled_by_provider(conn, provider_alias)?;
    for id in ids {
        if !merged.contains(id) {
            merged.push(id.clone());
        }
    }
    kv_store::set(
        conn,
        SCOPE_DISABLED_MODELS,
        provider_alias,
        &stringify_json(&json!(merged)),
    )
}

/// Subtract the given ids from the provider's disabled set. An empty result
/// deletes the row rather than storing `[]`.
pub fn enable_models(conn: &Connection, provider_alias: &str, ids: &[String]) -> DbResult<()> {
    if provider_alias.is_empty() {
        return Ok(());
    }
    if ids.is_empty() {
        return kv_store::remove(conn, SCOPE_DISABLED_MODELS, provider_alias);
    }
    let remove: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let next: Vec<String> = get_disabled_by_provider(conn, provider_alias)?
        .into_iter()
        .filter(|id| !remove.contains(id.as_str()))
        .collect();

    if next.is_empty() {
        kv_store::remove(conn, SCOPE_DISABLED_MODELS, provider_alias)
    } else {
        kv_store::set(
            conn,
            SCOPE_DISABLED_MODELS,
            provider_alias,
            &stringify_json(&json!(next)),
        )
    }
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

    #[test]
    fn aliases_round_trip_as_a_map() {
        let conn = db();
        set_model_alias(&conn, "fast", &json!("gpt-4o-mini")).unwrap();
        set_model_alias(&conn, "smart", &json!("gpt-4o")).unwrap();
        let aliases = get_model_aliases(&conn).unwrap();
        assert_eq!(aliases["fast"], json!("gpt-4o-mini"));
        assert_eq!(aliases["smart"], json!("gpt-4o"));
        delete_model_alias(&conn, "fast").unwrap();
        assert!(get_model_aliases(&conn).unwrap().get("fast").is_none());
    }

    #[test]
    fn custom_model_key_shape() {
        assert_eq!(custom_key("oc", "m1", "llm"), "oc|m1|llm");
        assert_eq!(custom_key("oc", "m1", "embedding"), "oc|m1|embedding");
    }

    #[test]
    fn add_custom_model_reports_added_only_once() {
        let conn = db();
        assert!(add_custom_model(&conn, "oc", "m1", None, None, None, None).unwrap());
        assert!(!add_custom_model(&conn, "oc", "m1", None, None, None, None).unwrap());
        assert_eq!(get_custom_models(&conn).unwrap().len(), 1);
    }

    #[test]
    fn add_custom_model_defaults_name_to_id() {
        let conn = db();
        add_custom_model(&conn, "oc", "m1", None, None, None, None).unwrap();
        let m = &get_custom_models(&conn).unwrap()[0];
        assert_eq!(m["name"], json!("m1"));
        assert_eq!(m["type"], json!("llm"));
        assert!(m.get("caps").is_none());
    }

    #[test]
    fn re_adding_merges_without_resetting_omitted_fields() {
        let conn = db();
        add_custom_model(
            &conn,
            "oc",
            "m1",
            None,
            Some("First"),
            Some(&json!(["vision"])),
            None,
        )
        .unwrap();
        add_custom_model(&conn, "oc", "m1", None, Some("Second"), None, None).unwrap();

        let m = &get_custom_models(&conn).unwrap()[0];
        assert_eq!(m["name"], json!("Second"));
        assert_eq!(m["caps"], json!(["vision"]), "caps was reset by a merge");
    }

    #[test]
    fn custom_model_transport_is_stored_and_merged() {
        let conn = db();
        add_custom_model(
            &conn,
            "oc",
            "m1",
            Some("stt"),
            None,
            None,
            Some(&json!("openai")),
        )
        .unwrap();
        let m = &get_custom_models(&conn).unwrap()[0];
        assert_eq!(m["transport"], json!("openai"));
        // Key order: transport is appended after the base fields.
        assert_eq!(
            m.as_object().unwrap().keys().collect::<Vec<_>>(),
            vec!["providerAlias", "id", "type", "name", "transport"]
        );

        // A re-add without transport keeps it; a falsy transport is ignored.
        add_custom_model(&conn, "oc", "m1", Some("stt"), Some("Renamed"), None, None).unwrap();
        let m = &get_custom_models(&conn).unwrap()[0];
        assert_eq!(m["transport"], json!("openai"));
        add_custom_model(&conn, "oc", "m1", Some("stt"), None, None, Some(&json!(""))).unwrap();
        assert_eq!(
            get_custom_models(&conn).unwrap()[0]["transport"],
            json!("openai"),
            "a falsy transport must not overwrite"
        );
    }

    #[test]
    fn custom_models_are_isolated_by_type() {
        let conn = db();
        add_custom_model(&conn, "oc", "m1", Some("llm"), None, None, None).unwrap();
        add_custom_model(&conn, "oc", "m1", Some("embedding"), None, None, None).unwrap();
        assert_eq!(get_custom_models(&conn).unwrap().len(), 2);
        delete_custom_model(&conn, "oc", "m1", Some("llm")).unwrap();
        assert_eq!(get_custom_models(&conn).unwrap().len(), 1);
    }

    #[test]
    fn disable_models_unions_in_first_seen_order() {
        let conn = db();
        disable_models(&conn, "oc", &["a".into(), "b".into()]).unwrap();
        disable_models(&conn, "oc", &["b".into(), "c".into()]).unwrap();
        assert_eq!(
            get_disabled_by_provider(&conn, "oc").unwrap(),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn enable_models_subtracts_and_deletes_when_empty() {
        let conn = db();
        disable_models(&conn, "oc", &["a".into(), "b".into()]).unwrap();
        enable_models(&conn, "oc", &["a".into()]).unwrap();
        assert_eq!(get_disabled_by_provider(&conn, "oc").unwrap(), vec!["b"]);

        enable_models(&conn, "oc", &["b".into()]).unwrap();
        assert!(
            kv_store::get(&conn, SCOPE_DISABLED_MODELS, "oc")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn enable_with_an_empty_list_clears_the_provider() {
        let conn = db();
        disable_models(&conn, "oc", &["a".into()]).unwrap();
        enable_models(&conn, "oc", &[]).unwrap();
        assert!(get_disabled_by_provider(&conn, "oc").unwrap().is_empty());
    }

    #[test]
    fn disabled_models_map_is_per_provider() {
        let conn = db();
        disable_models(&conn, "oc", &["a".into()]).unwrap();
        disable_models(&conn, "oa", &["x".into()]).unwrap();
        let all = get_disabled_models(&conn).unwrap();
        assert_eq!(all["oc"], json!(["a"]));
        assert_eq!(all["oa"], json!(["x"]));
    }

    #[test]
    fn unknown_provider_reads_as_empty() {
        let conn = db();
        assert!(get_disabled_by_provider(&conn, "nope").unwrap().is_empty());
    }
}
