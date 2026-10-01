//! The `providerNodes` table.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{opt_string, parse_json, stringify_json, take_rest};
use crate::time::now_iso;

/// Map one `providerNodes` row to the dashboard shape: blob first, then the
/// fixed columns.
pub fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let data: String = row.get("data")?;
    let mut map = match parse_json(&data, json!({})) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    map.insert("id".into(), Value::String(row.get("id")?));
    map.insert("type".into(), opt_string(row.get("type")?));
    map.insert("name".into(), opt_string(row.get("name")?));
    map.insert("createdAt".into(), Value::String(row.get("createdAt")?));
    map.insert("updatedAt".into(), Value::String(row.get("updatedAt")?));
    Ok(Value::Object(map))
}

pub fn node_to_row(n: &Value) -> DbResult<NodeRow> {
    let obj = n.as_object().cloned().unwrap_or_default();
    let rest = take_rest(n, &["id", "type", "name", "createdAt", "updatedAt"]);
    Ok(NodeRow {
        id: obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        node_type: obj.get("type").and_then(Value::as_str).map(str::to_string),
        name: obj.get("name").and_then(Value::as_str).map(str::to_string),
        data: stringify_json(&rest),
        created_at: obj
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        updated_at: obj
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

#[derive(Debug, Clone)]
pub struct NodeRow {
    pub id: String,
    pub node_type: Option<String>,
    pub name: Option<String>,
    pub data: String,
    pub created_at: String,
    pub updated_at: String,
}

pub fn upsert(conn: &Connection, n: &Value) -> DbResult<()> {
    let r = node_to_row(n)?;
    conn.execute(
        "INSERT INTO providerNodes(id, type, name, data, createdAt, updatedAt)
         VALUES(?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           type=excluded.type, name=excluded.name, data=excluded.data, updatedAt=excluded.updatedAt",
        rusqlite::params![r.id, r.node_type, r.name, r.data, r.created_at, r.updated_at],
    )?;
    Ok(())
}

pub fn get_provider_nodes(conn: &Connection, node_type: Option<&str>) -> DbResult<Vec<Value>> {
    match node_type {
        Some(t) => {
            let mut stmt = conn.prepare("SELECT * FROM providerNodes WHERE type = ?")?;
            let rows = stmt
                .query_map([t], row_to_node)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        }
        None => {
            let mut stmt = conn.prepare("SELECT * FROM providerNodes")?;
            let rows = stmt
                .query_map([], row_to_node)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        }
    }
}

pub fn get_provider_node_by_id(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    conn.query_row(
        "SELECT * FROM providerNodes WHERE id = ?",
        [id],
        row_to_node,
    )
    .optional()
    .map_err(Into::into)
}

/// Create a provider node. The field order here is the stored key order.
pub fn create_provider_node(conn: &Connection, data: &Value) -> DbResult<Value> {
    let now = now_iso();
    let mut node = Map::new();
    node.insert(
        "id".into(),
        data.get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(uuid::Uuid::new_v4().to_string())),
    );
    // `data.type` is copied as-is; a missing type is not defaulted.
    node.insert(
        "type".into(),
        data.get("type").cloned().unwrap_or(Value::Null),
    );
    node.insert(
        "name".into(),
        data.get("name").cloned().unwrap_or(Value::Null),
    );
    for key in ["prefix", "apiType", "baseUrl"] {
        node.insert(key.into(), data.get(key).cloned().unwrap_or(Value::Null));
    }
    node.insert("createdAt".into(), json!(now));
    node.insert("updatedAt".into(), json!(now));

    let node = Value::Object(node);
    upsert(conn, &node)?;
    Ok(node)
}

/// Merge a patch into a provider node. Must run inside a transaction.
pub fn update_provider_node(conn: &Connection, id: &str, data: &Value) -> DbResult<Option<Value>> {
    let Some(existing) = get_provider_node_by_id(conn, id)? else {
        return Ok(None);
    };
    let mut merged = match existing {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(d) = data {
        for (k, v) in d {
            merged.insert(k.clone(), v.clone());
        }
    }
    merged.insert("updatedAt".into(), json!(now_iso()));
    let merged = Value::Object(merged);
    upsert(conn, &merged)?;
    Ok(Some(merged))
}

/// Delete a provider node; returns the removed node. Must run inside a
/// transaction.
pub fn delete_provider_node(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    let Some(removed) = get_provider_node_by_id(conn, id)? else {
        return Ok(None);
    };
    conn.execute("DELETE FROM providerNodes WHERE id = ?", [id])?;
    Ok(Some(removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("providerNodes").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn create_keeps_reference_field_order_and_nulls() {
        let conn = db();
        let n = create_provider_node(
            &conn,
            &json!({ "type": "openai", "name": "n", "prefix": "p", "baseUrl": "u" }),
        )
        .unwrap();
        let keys: Vec<&String> = n.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "id");
        assert_eq!(keys[1], "type");
        assert_eq!(keys[2], "name");
        assert_eq!(keys[3], "prefix");
        assert_eq!(keys[4], "apiType");
        assert_eq!(keys[5], "baseUrl");
        assert_eq!(n["apiType"], Value::Null, "missing apiType kept as null");
    }

    #[test]
    fn create_uses_the_provided_id_when_present() {
        let conn = db();
        let n = create_provider_node(&conn, &json!({ "id": "fixed", "type": "t", "name": "n" }))
            .unwrap();
        assert_eq!(n["id"], json!("fixed"));
        assert_eq!(get_provider_nodes(&conn, Some("t")).unwrap().len(), 1);
    }

    #[test]
    fn update_merges_and_touches_updated_at() {
        let conn = db();
        let n =
            create_provider_node(&conn, &json!({ "id": "x", "type": "t", "name": "n" })).unwrap();
        let updated = update_provider_node(&conn, "x", &json!({ "name": "renamed" }))
            .unwrap()
            .unwrap();
        assert_eq!(updated["name"], json!("renamed"));
        assert_eq!(updated["type"], json!("t"));
        assert_eq!(updated["createdAt"], n["createdAt"]);
    }

    #[test]
    fn delete_returns_the_removed_node() {
        let conn = db();
        create_provider_node(&conn, &json!({ "id": "x", "type": "t", "name": "n" })).unwrap();
        let removed = delete_provider_node(&conn, "x").unwrap().unwrap();
        assert_eq!(removed["id"], json!("x"));
        assert!(get_provider_node_by_id(&conn, "x").unwrap().is_none());
        assert!(delete_provider_node(&conn, "x").unwrap().is_none());
    }

    #[test]
    fn row_to_node_puts_fixed_columns_last() {
        let conn = db();
        conn.execute(
            "INSERT INTO providerNodes(id, type, name, data, createdAt, updatedAt)
             VALUES('x','t','n','{\"baseUrl\":\"u\"}','c','u2')",
            [],
        )
        .unwrap();
        let n = get_provider_node_by_id(&conn, "x").unwrap().unwrap();
        let keys: Vec<&String> = n.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec!["baseUrl", "id", "type", "name", "createdAt", "updatedAt"]
        );
    }
}
