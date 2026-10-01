//! The `proxyPools` table.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{falsy_to_null, opt_string, parse_json, stringify_json, take_rest};
use crate::time::now_iso;

pub fn row_to_pool(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let data: String = row.get("data")?;
    let mut map = match parse_json(&data, json!({})) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let is_active: Option<i64> = row.get("isActive")?;
    map.insert("id".into(), Value::String(row.get("id")?));
    map.insert("isActive".into(), json!(is_active == Some(1)));
    map.insert("testStatus".into(), opt_string(row.get("testStatus")?));
    map.insert("createdAt".into(), Value::String(row.get("createdAt")?));
    map.insert("updatedAt".into(), Value::String(row.get("updatedAt")?));
    Ok(Value::Object(map))
}

pub fn pool_to_row(p: &Value) -> DbResult<PoolRow> {
    let obj = p.as_object().cloned().unwrap_or_default();
    let rest = take_rest(
        p,
        &["id", "isActive", "testStatus", "createdAt", "updatedAt"],
    );
    Ok(PoolRow {
        id: obj
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        is_active: match obj.get("isActive") {
            Some(Value::Bool(false)) => 0,
            _ => 1,
        },
        test_status: obj
            .get("testStatus")
            .and_then(Value::as_str)
            .map(str::to_string),
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
pub struct PoolRow {
    pub id: String,
    pub is_active: i64,
    pub test_status: Option<String>,
    pub data: String,
    pub created_at: String,
    pub updated_at: String,
}

pub fn upsert(conn: &Connection, p: &Value) -> DbResult<()> {
    let r = pool_to_row(p)?;
    conn.execute(
        "INSERT INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt)
         VALUES(?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           isActive=excluded.isActive, testStatus=excluded.testStatus,
           data=excluded.data, updatedAt=excluded.updatedAt",
        rusqlite::params![
            r.id,
            r.is_active,
            r.test_status,
            r.data,
            r.created_at,
            r.updated_at
        ],
    )?;
    Ok(())
}

/// Proxy pools, optionally filtered, newest `updatedAt` first.
pub fn get_proxy_pools(
    conn: &Connection,
    is_active: Option<bool>,
    test_status: Option<&str>,
) -> DbResult<Vec<Value>> {
    let mut sql = String::from("SELECT * FROM proxyPools");
    let mut where_parts = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(a) = is_active {
        where_parts.push("isActive = ?");
        params.push(Box::new(if a { 1i64 } else { 0i64 }));
    }
    if let Some(s) = test_status {
        where_parts.push("testStatus = ?");
        params.push(Box::new(s.to_string()));
    }
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut list: Vec<Value> = stmt
        .query_map(refs.as_slice(), row_to_pool)?
        .collect::<Result<_, _>>()?;

    list.sort_by_key(|p| {
        std::cmp::Reverse(
            p.get("updatedAt")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .and_then(crate::time::parse_iso)
                .map_or(0, |d| d.timestamp_millis()),
        )
    });
    Ok(list)
}

pub fn get_proxy_pool_by_id(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    conn.query_row("SELECT * FROM proxyPools WHERE id = ?", [id], row_to_pool)
        .optional()
        .map_err(Into::into)
}

/// Create a proxy pool.
pub fn create_proxy_pool(conn: &Connection, data: &Value) -> DbResult<Value> {
    let now = now_iso();
    let get = |k: &str| data.get(k).cloned().unwrap_or(Value::Null);
    let non_empty_or = |k: &str, fallback: &str| {
        data.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(fallback))
    };

    let mut pool = Map::new();
    pool.insert(
        "id".into(),
        data.get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| json!(s))
            .unwrap_or_else(|| json!(uuid::Uuid::new_v4().to_string())),
    );
    pool.insert("name".into(), get("name"));
    pool.insert("proxyUrl".into(), get("proxyUrl"));
    pool.insert("noProxy".into(), non_empty_or("noProxy", ""));
    pool.insert("type".into(), non_empty_or("type", "http"));
    pool.insert(
        "isActive".into(),
        data.get("isActive").cloned().unwrap_or(json!(true)),
    );
    // `strictProxy: data.strictProxy === true` — only literal true survives.
    pool.insert(
        "strictProxy".into(),
        json!(data.get("strictProxy") == Some(&json!(true))),
    );
    pool.insert("testStatus".into(), non_empty_or("testStatus", "unknown"));
    // `data.lastTestedAt || null` — falsy becomes null.
    pool.insert(
        "lastTestedAt".into(),
        falsy_to_null(data.get("lastTestedAt")),
    );
    pool.insert("lastError".into(), falsy_to_null(data.get("lastError")));
    pool.insert("createdAt".into(), json!(now));
    pool.insert("updatedAt".into(), json!(now));

    let pool = Value::Object(pool);
    upsert(conn, &pool)?;
    Ok(pool)
}

/// Merge a patch into a proxy pool. Must run inside a transaction.
pub fn update_proxy_pool(conn: &Connection, id: &str, data: &Value) -> DbResult<Option<Value>> {
    let Some(existing) = get_proxy_pool_by_id(conn, id)? else {
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

/// Delete a proxy pool; returns the removed pool.
pub fn delete_proxy_pool(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    let Some(removed) = get_proxy_pool_by_id(conn, id)? else {
        return Ok(None);
    };
    conn.execute("DELETE FROM proxyPools WHERE id = ?", [id])?;
    Ok(Some(removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("proxyPools").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn create_applies_reference_defaults_and_order() {
        let conn = db();
        let p = create_proxy_pool(&conn, &json!({ "name": "n", "proxyUrl": "http://p" })).unwrap();
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "name",
                "proxyUrl",
                "noProxy",
                "type",
                "isActive",
                "strictProxy",
                "testStatus",
                "lastTestedAt",
                "lastError",
                "createdAt",
                "updatedAt"
            ]
        );
        assert_eq!(p["noProxy"], json!(""));
        assert_eq!(p["type"], json!("http"));
        assert_eq!(p["isActive"], json!(true));
        assert_eq!(p["strictProxy"], json!(false));
        assert_eq!(p["testStatus"], json!("unknown"));
        assert_eq!(p["lastTestedAt"], Value::Null);
    }

    #[test]
    fn strict_proxy_only_accepts_literal_true() {
        let conn = db();
        let p = create_proxy_pool(&conn, &json!({ "name": "n", "strictProxy": "yes" })).unwrap();
        assert_eq!(p["strictProxy"], json!(false));
    }

    #[test]
    fn lists_are_newest_first() {
        let conn = db();
        for (id, ts) in [
            ("a", "2026-01-01T00:00:00.000Z"),
            ("b", "2026-06-01T00:00:00.000Z"),
        ] {
            conn.execute(
                "INSERT INTO proxyPools(id, isActive, testStatus, data, createdAt, updatedAt)
                 VALUES(?, 1, 'unknown', '{}', 't', ?)",
                rusqlite::params![id, ts],
            )
            .unwrap();
        }
        let list = get_proxy_pools(&conn, None, None).unwrap();
        assert_eq!(list[0]["id"], json!("b"));
        assert_eq!(list[1]["id"], json!("a"));
    }

    #[test]
    fn filters_apply() {
        let conn = db();
        create_proxy_pool(&conn, &json!({ "id": "a", "name": "n", "isActive": false })).unwrap();
        create_proxy_pool(
            &conn,
            &json!({ "id": "b", "name": "m", "testStatus": "ok" }),
        )
        .unwrap();
        assert_eq!(get_proxy_pools(&conn, Some(true), None).unwrap().len(), 1);
        assert_eq!(get_proxy_pools(&conn, Some(false), None).unwrap().len(), 1);
        assert_eq!(get_proxy_pools(&conn, None, Some("ok")).unwrap().len(), 1);
        assert_eq!(get_proxy_pools(&conn, None, Some("nope")).unwrap().len(), 0);
    }

    #[test]
    fn update_and_delete_round_trip() {
        let conn = db();
        create_proxy_pool(&conn, &json!({ "id": "a", "name": "n" })).unwrap();
        let u = update_proxy_pool(&conn, "a", &json!({ "name": "renamed" }))
            .unwrap()
            .unwrap();
        assert_eq!(u["name"], json!("renamed"));
        assert_eq!(u["type"], json!("http"));
        assert!(delete_proxy_pool(&conn, "a").unwrap().is_some());
        assert!(delete_proxy_pool(&conn, "a").unwrap().is_none());
    }
}
