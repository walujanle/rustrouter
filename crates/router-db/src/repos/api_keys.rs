//! The `apiKeys` table.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};

use crate::error::{DbError, DbResult};
use crate::identity::generate_api_key_with_machine;
use crate::json_col::{is_falsy, opt_string};
use crate::time::now_iso;

/// Map one `apiKeys` row to the dashboard shape.
pub fn row_to_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let is_active: Option<i64> = row.get("isActive")?;
    let mut map = serde_json::Map::new();
    map.insert("id".into(), Value::String(row.get("id")?));
    map.insert("key".into(), Value::String(row.get("key")?));
    map.insert("name".into(), opt_string(row.get("name")?));
    map.insert("machineId".into(), opt_string(row.get("machineId")?));
    map.insert("isActive".into(), json!(is_active == Some(1)));
    map.insert("createdAt".into(), Value::String(row.get("createdAt")?));
    Ok(Value::Object(map))
}

/// All API keys, oldest first.
pub fn get_api_keys(conn: &Connection) -> DbResult<Vec<Value>> {
    let mut stmt = conn.prepare("SELECT * FROM apiKeys ORDER BY createdAt ASC")?;
    let rows = stmt
        .query_map([], row_to_key)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn get_api_key_by_id(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    conn.query_row("SELECT * FROM apiKeys WHERE id = ?", [id], row_to_key)
        .optional()
        .map_err(Into::into)
}

/// Create an API key for a machine id.
///
/// The returned object's key order differs from the table's column order —
/// `id, name, key, machineId, isActive, createdAt` — and that is what the
/// dashboard serialises.
pub fn create_api_key(conn: &Connection, name: Option<&str>, machine_id: &str) -> DbResult<Value> {
    if machine_id.is_empty() {
        return Err(DbError::Invalid("machineId is required".into()));
    }
    let secret = crate::identity::api_key_secret();
    let key = generate_api_key_with_machine(&secret, machine_id);
    let created_at = now_iso();
    let id = uuid::Uuid::new_v4().to_string();

    conn.execute(
        "INSERT INTO apiKeys(id, key, name, machineId, isActive, createdAt) VALUES(?, ?, ?, ?, ?, ?)",
        rusqlite::params![id, key, name, machine_id, 1i64, created_at],
    )?;

    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(id));
    out.insert("name".into(), name.map_or(Value::Null, |s| json!(s)));
    out.insert("key".into(), json!(key));
    out.insert("machineId".into(), json!(machine_id));
    out.insert("isActive".into(), json!(true));
    out.insert("createdAt".into(), json!(created_at));
    Ok(Value::Object(out))
}

/// Merge a patch into an API key. Must run inside a transaction.
pub fn update_api_key(conn: &Connection, id: &str, data: &Value) -> DbResult<Option<Value>> {
    let Some(existing) = get_api_key_by_id(conn, id)? else {
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

    // `merged.isActive ? 1 : 0` — JS truthiness, not `as_bool`. A patch that
    // omits `isActive` keeps the existing `true` from `rowToKey`; a string or
    // number is truthy rather than silently deactivating the key.
    let is_active = merged.get("isActive").is_none_or(|v| !is_falsy(v));
    conn.execute(
        "UPDATE apiKeys SET key = ?, name = ?, machineId = ?, isActive = ? WHERE id = ?",
        rusqlite::params![
            merged.get("key").and_then(Value::as_str),
            merged.get("name").and_then(Value::as_str),
            merged.get("machineId").and_then(Value::as_str),
            if is_active { 1i64 } else { 0i64 },
            id
        ],
    )?;
    Ok(Some(Value::Object(merged)))
}

/// Delete an API key; returns whether a row was removed.
pub fn delete_api_key(conn: &Connection, id: &str) -> DbResult<bool> {
    let changed = conn.execute("DELETE FROM apiKeys WHERE id = ?", [id])?;
    Ok(changed > 0)
}

/// A key is valid when it exists and is active.
pub fn validate_api_key(conn: &Connection, key: &str) -> DbResult<bool> {
    let row: Option<Option<i64>> = conn
        .query_row("SELECT isActive FROM apiKeys WHERE key = ?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(row.flatten() == Some(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("apiKeys").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn create_returns_the_dashboard_key_order() {
        let conn = db();
        let k = create_api_key(&conn, Some("laptop"), "machine1").unwrap();
        let keys: Vec<&String> = k.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["id", "name", "key", "machineId", "isActive", "createdAt"]
        );
        assert_eq!(k["isActive"], json!(true));
        assert!(k["key"].as_str().unwrap().starts_with("sk-machine1-"));
    }

    #[test]
    fn create_requires_a_machine_id() {
        let conn = db();
        assert!(create_api_key(&conn, Some("n"), "").is_err());
    }

    #[test]
    fn validate_checks_active() {
        let conn = db();
        let k = create_api_key(&conn, Some("n"), "m").unwrap();
        let key = k["key"].as_str().unwrap().to_string();
        assert!(validate_api_key(&conn, &key).unwrap());

        update_api_key(
            &conn,
            k["id"].as_str().unwrap(),
            &json!({ "isActive": false }),
        )
        .unwrap();
        assert!(!validate_api_key(&conn, &key).unwrap());
        assert!(!validate_api_key(&conn, "sk-nope").unwrap());
    }

    #[test]
    fn list_is_oldest_first() {
        let conn = db();
        create_api_key(&conn, Some("first"), "m").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        create_api_key(&conn, Some("second"), "m").unwrap();
        let list = get_api_keys(&conn).unwrap();
        assert_eq!(list[0]["name"], json!("first"));
        assert_eq!(list[1]["name"], json!("second"));
    }

    #[test]
    fn delete_reports_whether_a_row_went() {
        let conn = db();
        let k = create_api_key(&conn, Some("n"), "m").unwrap();
        assert!(delete_api_key(&conn, k["id"].as_str().unwrap()).unwrap());
        assert!(!delete_api_key(&conn, k["id"].as_str().unwrap()).unwrap());
    }

    #[test]
    fn update_missing_row_returns_none() {
        let conn = db();
        assert!(update_api_key(&conn, "nope", &json!({})).unwrap().is_none());
    }
}
