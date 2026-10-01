//! The `combos` table.
//!
//! `models` is JSON-in-TEXT in every case, including on the create path.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::error::DbResult;
use crate::json_col::{is_falsy, opt_string, parse_json, stringify_json};
use crate::time::now_iso;

/// A value bound to the TEXT `kind` column the way the reference driver does:
/// a string is stored as-is, `null` as SQL NULL, and any other truthy value is
/// stored as its JSON text (SQLite would coerce a JS number or boolean the same
/// way). Binding `as_str()` alone would turn a non-string kind into NULL.
fn kind_bind(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Map one `combos` row to the dashboard shape.
pub fn row_to_combo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let models: String = row.get("models")?;
    let mut map = serde_json::Map::new();
    map.insert("id".into(), Value::String(row.get("id")?));
    map.insert("name".into(), Value::String(row.get("name")?));
    map.insert("kind".into(), opt_string(row.get("kind")?));
    map.insert("models".into(), parse_json(&models, json!([])));
    map.insert("createdAt".into(), Value::String(row.get("createdAt")?));
    map.insert("updatedAt".into(), Value::String(row.get("updatedAt")?));
    Ok(Value::Object(map))
}

/// All combos, oldest first.
pub fn get_combos(conn: &Connection) -> DbResult<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT * FROM combos ORDER BY createdAt ASC")?;
    let rows = stmt
        .query_map([], row_to_combo)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_combo_by_id(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    conn.query_row("SELECT * FROM combos WHERE id = ?", [id], row_to_combo)
        .optional()
        .map_err(Into::into)
}

pub fn get_combo_by_name(conn: &Connection, name: &str) -> DbResult<Option<Value>> {
    conn.query_row("SELECT * FROM combos WHERE name = ?", [name], row_to_combo)
        .optional()
        .map_err(Into::into)
}

/// Create a combo.
pub fn create_combo(conn: &Connection, data: &Value) -> DbResult<Value> {
    let now = now_iso();
    let name = data.get("name").and_then(Value::as_str).unwrap_or_default();
    // `data.kind || null` — a falsy value (`""`, `false`, `0`) becomes null.
    let kind = match data.get("kind") {
        Some(v) if !is_falsy(v) => v.clone(),
        _ => Value::Null,
    };
    // `data.models || []` — a falsy value becomes an empty array.
    let models = match data.get("models") {
        Some(v) if !is_falsy(v) => v.clone(),
        _ => json!([]),
    };
    let id = uuid::Uuid::new_v4().to_string();

    conn.execute(
        "INSERT INTO combos(id, name, kind, models, createdAt, updatedAt) VALUES(?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            id,
            name,
            kind_bind(&kind),
            stringify_json(&models),
            now,
            now
        ],
    )?;

    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(id));
    out.insert("name".into(), json!(name));
    out.insert("kind".into(), kind);
    out.insert("models".into(), models);
    out.insert("createdAt".into(), json!(now));
    out.insert("updatedAt".into(), json!(now));
    Ok(Value::Object(out))
}

/// Merge a patch into a combo. Must run inside a transaction.
pub fn update_combo(conn: &Connection, id: &str, data: &Value) -> DbResult<Option<Value>> {
    let Some(existing) = get_combo_by_id(conn, id)? else {
        return Ok(None);
    };
    let mut merged = match existing {
        Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    if let Value::Object(d) = data {
        for (k, v) in d {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged.insert("updatedAt".into(), json!(now_iso()));

    // `merged.models || []` on the write path too.
    let models = match merged.get("models") {
        Some(v) if !is_falsy(v) => v.clone(),
        _ => json!([]),
    };
    conn.execute(
        "UPDATE combos SET name = ?, kind = ?, models = ?, updatedAt = ? WHERE id = ?",
        rusqlite::params![
            merged.get("name").and_then(Value::as_str),
            // No `|| null` on the update path, unlike create: the reference
            // binds the merged value straight through.
            kind_bind(merged.get("kind").unwrap_or(&Value::Null)),
            stringify_json(&models),
            merged.get("updatedAt").and_then(Value::as_str),
            id
        ],
    )?;

    // The returned object keeps the patched `models` — not the coerced value
    // that was written.
    Ok(Some(Value::Object(merged)))
}

/// Delete a combo; returns whether a row was removed.
pub fn delete_combo(conn: &Connection, id: &str) -> DbResult<bool> {
    let changed = conn.execute("DELETE FROM combos WHERE id = ?", [id])?;
    Ok(changed > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("combos").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn create_stores_models_as_json_text() {
        let conn = db();
        let c = create_combo(&conn, &json!({ "name": "fast", "models": ["a", "b"] })).unwrap();
        let raw: String = conn
            .query_row(
                "SELECT models FROM combos WHERE id = ?",
                [c["id"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, r#"["a","b"]"#);
        assert_eq!(c["kind"], Value::Null);
        assert_eq!(c["models"], json!(["a", "b"]));
    }

    #[test]
    fn missing_models_becomes_an_empty_array() {
        let conn = db();
        let c = create_combo(&conn, &json!({ "name": "n" })).unwrap();
        assert_eq!(c["models"], json!([]));
    }

    #[test]
    fn read_back_matches_created_shape() {
        let conn = db();
        let c = create_combo(&conn, &json!({ "name": "n", "kind": "k", "models": ["m"] })).unwrap();
        let read = get_combo_by_id(&conn, c["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(read, c);
    }

    #[test]
    fn lookup_by_name_works() {
        let conn = db();
        create_combo(&conn, &json!({ "name": "fast", "models": [] })).unwrap();
        assert!(get_combo_by_name(&conn, "fast").unwrap().is_some());
        assert!(get_combo_by_name(&conn, "slow").unwrap().is_none());
    }

    #[test]
    fn update_merges_and_keeps_created_at() {
        let conn = db();
        let c = create_combo(&conn, &json!({ "name": "n", "models": ["a"] })).unwrap();
        let u = update_combo(
            &conn,
            c["id"].as_str().unwrap(),
            &json!({ "models": ["b", "c"] }),
        )
        .unwrap()
        .unwrap();
        assert_eq!(u["models"], json!(["b", "c"]));
        assert_eq!(u["name"], json!("n"));
        assert_eq!(u["createdAt"], c["createdAt"]);
    }

    #[test]
    fn delete_reports_whether_a_row_went() {
        let conn = db();
        let c = create_combo(&conn, &json!({ "name": "n" })).unwrap();
        let id = c["id"].as_str().unwrap();
        assert!(delete_combo(&conn, id).unwrap());
        assert!(!delete_combo(&conn, id).unwrap());
    }
}
