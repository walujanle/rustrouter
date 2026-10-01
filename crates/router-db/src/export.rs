//! Whole-database export and import.
//!
//! The payload shape is a frontend contract: field order here is the order the
//! dashboard sees. Export spreads the `data` blob first and then overlays the
//! fixed columns, so a column always wins over a same-named key in the blob.

use rusqlite::Connection;
use serde_json::{Map, Value, json};

use crate::error::{DbError, DbResult};
use crate::json_col::{parse_json, stringify_json, take_rest};
use crate::repos::settings;
use crate::time::now_iso;

/// `exportDb()`.
pub fn export_db(conn: &Connection) -> DbResult<Value> {
    let mut out = Map::new();
    out.insert("settings".into(), settings::export_settings(conn)?);

    let mut conns = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT * FROM providerConnections")?;
        let rows = stmt.query_map([], |r| {
            let data = parse_json(&r.get::<_, String>("data")?, json!({}));
            let mut map = match data {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            map.insert("id".into(), json!(r.get::<_, String>("id")?));
            map.insert("provider".into(), json!(r.get::<_, String>("provider")?));
            map.insert("authType".into(), json!(r.get::<_, String>("authType")?));
            map.insert("name".into(), opt(r.get::<_, Option<String>>("name")?));
            map.insert("email".into(), opt(r.get::<_, Option<String>>("email")?));
            map.insert("priority".into(), opt(r.get::<_, Option<i64>>("priority")?));
            map.insert("isActive".into(), json!(r.get::<_, i64>("isActive")? == 1));
            map.insert("createdAt".into(), json!(r.get::<_, String>("createdAt")?));
            map.insert("updatedAt".into(), json!(r.get::<_, String>("updatedAt")?));
            Ok(Value::Object(map))
        })?;
        for row in rows {
            conns.push(row?);
        }
    }
    out.insert("providerConnections".into(), Value::Array(conns));

    let mut node_list = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT * FROM providerNodes")?;
        let rows = stmt.query_map([], |r| {
            let data = parse_json(&r.get::<_, String>("data")?, json!({}));
            let mut map = match data {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            map.insert("id".into(), json!(r.get::<_, String>("id")?));
            map.insert("type".into(), opt(r.get::<_, Option<String>>("type")?));
            map.insert("name".into(), opt(r.get::<_, Option<String>>("name")?));
            map.insert("createdAt".into(), json!(r.get::<_, String>("createdAt")?));
            map.insert("updatedAt".into(), json!(r.get::<_, String>("updatedAt")?));
            Ok(Value::Object(map))
        })?;
        for row in rows {
            node_list.push(row?);
        }
    }
    out.insert("providerNodes".into(), Value::Array(node_list));

    let mut pools = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT * FROM proxyPools")?;
        let rows = stmt.query_map([], |r| {
            let data = parse_json(&r.get::<_, String>("data")?, json!({}));
            let mut map = match data {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            map.insert("id".into(), json!(r.get::<_, String>("id")?));
            map.insert("isActive".into(), json!(r.get::<_, i64>("isActive")? == 1));
            map.insert(
                "testStatus".into(),
                opt(r.get::<_, Option<String>>("testStatus")?),
            );
            map.insert("createdAt".into(), json!(r.get::<_, String>("createdAt")?));
            map.insert("updatedAt".into(), json!(r.get::<_, String>("updatedAt")?));
            Ok(Value::Object(map))
        })?;
        for row in rows {
            pools.push(row?);
        }
    }
    out.insert("proxyPools".into(), Value::Array(pools));

    let mut keys = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT * FROM apiKeys")?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>("id")?,
                "key": r.get::<_, String>("key")?,
                "name": r.get::<_, Option<String>>("name")?,
                "machineId": r.get::<_, Option<String>>("machineId")?,
                "isActive": r.get::<_, i64>("isActive")? == 1,
                "createdAt": r.get::<_, String>("createdAt")?,
            }))
        })?;
        for row in rows {
            keys.push(row?);
        }
    }
    out.insert("apiKeys".into(), Value::Array(keys));

    let mut combo_list = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT * FROM combos")?;
        let rows = stmt.query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, String>("id")?,
                "name": r.get::<_, String>("name")?,
                "kind": r.get::<_, Option<String>>("kind")?,
                "models": parse_json(&r.get::<_, String>("models")?, json!([])),
                "createdAt": r.get::<_, String>("createdAt")?,
                "updatedAt": r.get::<_, String>("updatedAt")?,
            }))
        })?;
        for row in rows {
            combo_list.push(row?);
        }
    }
    out.insert("combos".into(), Value::Array(combo_list));

    out.insert("modelAliases".into(), kv_map(conn, "modelAliases")?);
    out.insert("customModels".into(), kv_values(conn, "customModels")?);
    out.insert("mitmAlias".into(), kv_map(conn, "mitmAlias")?);
    out.insert("pricing".into(), kv_map(conn, "pricing")?);

    Ok(Value::Object(out))
}

fn opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map_or(Value::Null, Into::into)
}

fn kv_map(conn: &Connection, scope: &str) -> DbResult<Value> {
    let mut out = Map::new();
    for (k, v) in crate::kv_store::get_all(conn, scope)? {
        out.insert(k, parse_json(&v, Value::Null));
    }
    Ok(Value::Object(out))
}

fn kv_values(conn: &Connection, scope: &str) -> DbResult<Value> {
    Ok(Value::Array(
        crate::kv_store::get_all(conn, scope)?
            .into_iter()
            .map(|(_, v)| parse_json(&v, Value::Null))
            .collect(),
    ))
}

/// `importDb(payload)`.
///
/// Wipes every table except `_meta` and re-inserts. The whole thing runs in one
/// transaction, so a malformed payload leaves the database untouched. Must run
/// inside a transaction.
pub fn import_db(conn: &Connection, payload: &Value) -> DbResult<Value> {
    let Value::Object(payload) = payload else {
        return Err(DbError::Invalid("Invalid database payload".into()));
    };

    conn.execute_batch(
        "DELETE FROM settings;
         DELETE FROM providerConnections;
         DELETE FROM providerNodes;
         DELETE FROM proxyPools;
         DELETE FROM apiKeys;
         DELETE FROM combos;
         DELETE FROM kv WHERE scope IN ('modelAliases', 'customModels', 'mitmAlias', 'pricing');",
    )?;

    if let Some(s) = payload.get("settings") {
        conn.execute(
            "INSERT INTO settings(id, data) VALUES(1, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            [stringify_json(s)],
        )?;
    }

    if let Some(Value::Array(items)) = payload.get("providerConnections") {
        for c in items {
            let rest = take_rest(
                c,
                &[
                    "id",
                    "provider",
                    "authType",
                    "name",
                    "email",
                    "priority",
                    "isActive",
                    "createdAt",
                    "updatedAt",
                ],
            );
            conn.execute(
                "INSERT OR REPLACE INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    str_field(c, "id"),
                    str_field(c, "provider"),
                    str_field_or(c, "authType", "oauth"),
                    opt_str(c, "name"),
                    opt_str(c, "email"),
                    // `priority || null` — 0 is falsy, so it stores NULL.
                    c.get("priority")
                        .and_then(Value::as_i64)
                        .filter(|n| *n != 0),
                    // `isActive === false ? 0 : 1` — only literal false deactivates.
                    i64::from(c.get("isActive") != Some(&Value::Bool(false))),
                    stringify_json(&rest),
                    str_field_or(c, "createdAt", &now_iso()),
                    str_field_or(c, "updatedAt", &now_iso()),
                ],
            )?;
        }
    }

    if let Some(Value::Array(items)) = payload.get("providerNodes") {
        for n in items {
            let rest = take_rest(n, &["id", "type", "name", "createdAt", "updatedAt"]);
            conn.execute(
                "INSERT OR REPLACE INTO providerNodes(id, type, name, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    str_field(n, "id"),
                    opt_str(n, "type"),
                    opt_str(n, "name"),
                    stringify_json(&rest),
                    str_field_or(n, "createdAt", &now_iso()),
                    str_field_or(n, "updatedAt", &now_iso()),
                ],
            )?;
        }
    }

    if let Some(Value::Array(items)) = payload.get("proxyPools") {
        for p in items {
            let rest = take_rest(
                p,
                &["id", "isActive", "testStatus", "createdAt", "updatedAt"],
            );
            conn.execute(
                "INSERT OR REPLACE INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    str_field(p, "id"),
                    i64::from(p.get("isActive") != Some(&Value::Bool(false))),
                    str_field_or(p, "testStatus", "unknown"),
                    stringify_json(&rest),
                    str_field_or(p, "createdAt", &now_iso()),
                    str_field_or(p, "updatedAt", &now_iso()),
                ],
            )?;
        }
    }

    if let Some(Value::Array(items)) = payload.get("apiKeys") {
        for k in items {
            conn.execute(
                "INSERT OR REPLACE INTO apiKeys(id, key, name, machineId, isActive, createdAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    str_field(k, "id"),
                    str_field(k, "key"),
                    opt_str(k, "name"),
                    opt_str(k, "machineId"),
                    i64::from(k.get("isActive") != Some(&Value::Bool(false))),
                    str_field_or(k, "createdAt", &now_iso()),
                ],
            )?;
        }
    }

    if let Some(Value::Array(items)) = payload.get("combos") {
        for c in items {
            conn.execute(
                "INSERT OR REPLACE INTO combos(id, name, kind, models, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    str_field(c, "id"),
                    str_field(c, "name"),
                    truthy_str_or_null(c, "kind"),
                    // `stringifyJson(c.models || [])` — any falsy value becomes `[]`.
                    stringify_json(&falsy_to_array(c.get("models"))),
                    str_field_or(c, "createdAt", &now_iso()),
                    str_field_or(c, "updatedAt", &now_iso()),
                ],
            )?;
        }
    }

    if let Some(Value::Object(aliases)) = payload.get("modelAliases") {
        for (alias, model) in aliases {
            conn.execute(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('modelAliases', ?, ?)",
                rusqlite::params![alias, stringify_json(model)],
            )?;
        }
    }

    if let Some(Value::Array(models)) = payload.get("customModels") {
        for m in models {
            let key = format!(
                "{}|{}|{}",
                m.get("providerAlias")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                m.get("id").and_then(Value::as_str).unwrap_or_default(),
                m.get("type").and_then(Value::as_str).unwrap_or("llm"),
            );
            conn.execute(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('customModels', ?, ?)",
                rusqlite::params![key, stringify_json(m)],
            )?;
        }
    }

    if let Some(Value::Object(mappings)) = payload.get("mitmAlias") {
        for (tool, v) in mappings {
            conn.execute(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('mitmAlias', ?, ?)",
                rusqlite::params![
                    tool,
                    stringify_json(&json!(v.as_object().cloned().unwrap_or_default()))
                ],
            )?;
        }
    }

    if let Some(Value::Object(providers)) = payload.get("pricing") {
        for (provider, models) in providers {
            conn.execute(
                "INSERT OR REPLACE INTO kv(scope, key, value) VALUES('pricing', ?, ?)",
                rusqlite::params![
                    provider,
                    stringify_json(&json!(models.as_object().cloned().unwrap_or_default()))
                ],
            )?;
        }
    }

    export_db(conn)
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `v[key] || default` — an absent **or empty** string falls back. The
/// reference binds `x || "default"`, where `""` is falsy, so a plain
/// `unwrap_or` would keep an empty string the reference replaces.
fn str_field_or(v: &Value, key: &str, default: &str) -> String {
    str_field(v, key)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `v[key] || null` — a falsy value (`null`, `false`, `0`, `""`) becomes SQL
/// NULL; any other value is kept as its JSON text.
fn truthy_str_or_null(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(value) if !crate::json_col::is_falsy(value) => match value {
            Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        },
        _ => None,
    }
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `c.models || []` — a missing, `null`, `false`, `0` or `""` value becomes an
/// empty array; a truthy value is kept as-is.
fn falsy_to_array(v: Option<&Value>) -> Value {
    match v {
        Some(v) if !crate::json_col::is_falsy(v) => v.clone(),
        _ => json!([]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for t in crate::schema::TABLES {
            conn.execute_batch(&t.create_sql()).unwrap();
        }
        conn
    }

    #[test]
    fn round_trips_every_section() {
        let conn = db();
        let payload = json!({
            "settings": { "cloudEnabled": true },
            "providerConnections": [{
                "id": "c1", "provider": "openai", "authType": "oauth",
                "name": "Acct", "email": "a@b.c", "priority": 3, "isActive": true,
                "createdAt": "2026-01-01T00:00:00.000Z",
                "updatedAt": "2026-01-02T00:00:00.000Z",
                "accessToken": "tok"
            }],
            "providerNodes": [{ "id": "n1", "type": "openai-compatible", "name": "Node",
                "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
                "baseUrl": "http://x" }],
            "proxyPools": [{ "id": "p1", "isActive": true, "testStatus": "active",
                "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z",
                "proxyUrl": "http://y" }],
            "apiKeys": [{ "id": "k1", "key": "sk-x", "name": "Key", "machineId": "m",
                "isActive": true, "createdAt": "2026-01-01T00:00:00.000Z" }],
            "combos": [{ "id": "cb1", "name": "Combo", "kind": null, "models": ["a"],
                "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": "2026-01-01T00:00:00.000Z" }],
            "modelAliases": { "fast": "gpt-4o-mini" },
            "customModels": [{ "providerAlias": "oc", "id": "m1", "type": "llm", "name": "M" }],
            "mitmAlias": {},
            "pricing": { "openai": { "gpt-4o": { "input": 1 } } },
        });

        let exported = import_db(&conn, &payload).unwrap();
        assert_eq!(exported["settings"]["cloudEnabled"], json!(true));
        assert_eq!(
            exported["providerConnections"][0]["accessToken"],
            json!("tok")
        );
        assert_eq!(exported["providerConnections"][0]["isActive"], json!(true));
        assert_eq!(exported["providerNodes"][0]["baseUrl"], json!("http://x"));
        assert_eq!(exported["proxyPools"][0]["proxyUrl"], json!("http://y"));
        assert_eq!(exported["apiKeys"][0]["key"], json!("sk-x"));
        assert_eq!(exported["combos"][0]["models"], json!(["a"]));
        assert_eq!(exported["modelAliases"]["fast"], json!("gpt-4o-mini"));
        assert_eq!(exported["customModels"][0]["name"], json!("M"));
        assert_eq!(exported["pricing"]["openai"]["gpt-4o"]["input"], json!(1));
    }

    #[test]
    fn import_replaces_previous_rows() {
        let conn = db();
        import_db(
            &conn,
            &json!({ "apiKeys": [{ "id": "k1", "key": "sk-1" }] }),
        )
        .unwrap();
        let out = import_db(
            &conn,
            &json!({ "apiKeys": [{ "id": "k2", "key": "sk-2" }] }),
        )
        .unwrap();
        assert_eq!(out["apiKeys"].as_array().unwrap().len(), 1);
        assert_eq!(out["apiKeys"][0]["id"], json!("k2"));
    }

    #[test]
    fn is_active_only_trips_on_literal_false() {
        let conn = db();
        let out = import_db(
            &conn,
            &json!({ "providerConnections": [
                { "id": "a", "provider": "p", "isActive": false },
                { "id": "b", "provider": "p", "isActive": 0 },
                { "id": "c", "provider": "p" }
            ]}),
        )
        .unwrap();
        let by_id = |id: &str| {
            out["providerConnections"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == json!(id))
                .unwrap()
                .clone()
        };
        assert_eq!(by_id("a")["isActive"], json!(false));
        assert_eq!(
            by_id("b")["isActive"],
            json!(true),
            "0 is not literal false"
        );
        assert_eq!(by_id("c")["isActive"], json!(true));
    }

    #[test]
    fn import_coerces_falsy_combo_models_to_an_empty_array() {
        let conn = db();
        let out = import_db(
            &conn,
            &json!({ "combos": [
                { "id": "c0", "name": "n0", "models": 0 },
                { "id": "c1", "name": "n1", "models": "" },
                { "id": "c2", "name": "n2", "models": null },
                { "id": "c3", "name": "n3" },
                { "id": "c4", "name": "n4", "models": ["a", "b"] }
            ]}),
        )
        .unwrap();
        let models = |id: &str| {
            out["combos"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["id"] == json!(id))
                .unwrap()["models"]
                .clone()
        };
        for id in ["c0", "c1", "c2", "c3"] {
            assert_eq!(models(id), json!([]), "{id} should coerce to []");
        }
        assert_eq!(models("c4"), json!(["a", "b"]));
    }

    #[test]
    fn non_object_payload_is_rejected() {
        let conn = db();
        assert!(import_db(&conn, &json!([])).is_err());
        assert!(import_db(&conn, &json!("nope")).is_err());
    }

    #[test]
    fn export_defaults_missing_sections() {
        let conn = db();
        let out = export_db(&conn).unwrap();
        assert_eq!(out["modelAliases"], json!({}));
        assert_eq!(out["customModels"], json!([]));
        assert_eq!(out["mitmAlias"], json!({}));
        assert_eq!(out["pricing"], json!({}));
        assert_eq!(out["providerConnections"], json!([]));
    }
}
