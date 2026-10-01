//! The `providerConnections` table.
//!
//! Two details drive the whole file:
//!
//! - Rows are a fixed set of columns plus a free-form `data` JSON blob. The
//!   blob carries `modelLock_<model>` keys whose names contain `/` and `:`, so
//!   it must stay a map, never a struct.
//! - Create-time dedup is provider-specific. Collapsing it to email-only
//!   overwrites a second account's refresh token, which is a live-credential
//!   bug, not a cosmetic one.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::{DbError, DbResult};
use crate::json_col::{opt_string, parse_json, stringify_json, take_rest};
use crate::time::now_iso;

/// `OPTIONAL_FIELDS`: copied onto a new row only when present and non-null.
pub const OPTIONAL_FIELDS: &[&str] = &[
    "displayName",
    "email",
    "globalPriority",
    "defaultModel",
    "accessToken",
    "refreshToken",
    "expiresAt",
    "tokenType",
    "scope",
    "projectId",
    "apiKey",
    "testStatus",
    "lastTested",
    "lastError",
    "lastErrorAt",
    "rateLimitedUntil",
    "expiresIn",
    "errorCode",
    "consecutiveUseCount",
    "idToken",
    "lastRefreshAt",
];

/// The nine columns pulled out of the JSON blob. Order matters: it is the order
/// they are destructured in.
pub const FIXED_COLUMNS: &[&str] = &[
    "id",
    "provider",
    "authType",
    "name",
    "email",
    "priority",
    "isActive",
    "createdAt",
    "updatedAt",
];

pub const MODEL_LOCK_PREFIX: &str = "modelLock_";

/// The cleanup field list. Deliberately a different set from `OPTIONAL_FIELDS`
/// — it omits `errorCode`, `idToken` and `lastRefreshAt`.
const CLEANUP_FIELDS: &[&str] = &[
    "displayName",
    "email",
    "globalPriority",
    "defaultModel",
    "accessToken",
    "refreshToken",
    "expiresAt",
    "tokenType",
    "scope",
    "projectId",
    "apiKey",
    "testStatus",
    "lastTested",
    "lastError",
    "lastErrorAt",
    "rateLimitedUntil",
    "expiresIn",
    "consecutiveUseCount",
];

/// Map one `providerConnections` row to the dashboard shape.
///
/// The parsed blob is spread first, then the fixed columns override it. A key
/// already present in the blob keeps its original position — which
/// `IndexMap::insert` reproduces — so the exported JSON is byte-identical.
pub fn row_to_conn(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let data: String = row.get("data")?;
    let mut map = match parse_json(&data, json!({})) {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let is_active: Option<i64> = row.get("isActive")?;
    let priority: Option<i64> = row.get("priority")?;
    map.insert("id".into(), Value::String(row.get("id")?));
    map.insert("provider".into(), Value::String(row.get("provider")?));
    map.insert("authType".into(), Value::String(row.get("authType")?));
    map.insert("name".into(), opt_string(row.get("name")?));
    map.insert("email".into(), opt_string(row.get("email")?));
    map.insert(
        "priority".into(),
        priority.map_or(Value::Null, |p| json!(p)),
    );
    // `isActive === 1 || isActive === true`
    map.insert("isActive".into(), json!(is_active == Some(1)));
    map.insert("createdAt".into(), Value::String(row.get("createdAt")?));
    map.insert("updatedAt".into(), Value::String(row.get("updatedAt")?));
    Ok(Value::Object(map))
}

/// Strip the fixed columns from a connection, stringify the rest.
///
/// Returns the column values in insert order:
/// `(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt)`.
pub fn conn_to_row(c: &Value) -> DbResult<ConnRow> {
    let obj = c.as_object().cloned().unwrap_or_default();
    let rest = take_rest(c, FIXED_COLUMNS);

    let get_str = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let is_active = match obj.get("isActive") {
        // `isActive === false ? 0 : 1` — anything that is not literal false is 1.
        Some(Value::Bool(false)) => 0,
        _ => 1,
    };

    Ok(ConnRow {
        id: get_str("id").unwrap_or_default(),
        provider: get_str("provider").unwrap_or_default(),
        auth_type: get_str("authType").unwrap_or_default(),
        name: nullable_str(&obj, "name"),
        email: nullable_str(&obj, "email"),
        priority: obj.get("priority").and_then(Value::as_i64),
        is_active,
        data: stringify_json(&rest),
        created_at: get_str("createdAt").unwrap_or_default(),
        updated_at: get_str("updatedAt").unwrap_or_default(),
    })
}

fn nullable_str(obj: &Map<String, Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct ConnRow {
    pub id: String,
    pub provider: String,
    pub auth_type: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub priority: Option<i64>,
    pub is_active: i64,
    pub data: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Insert or update a connection row.
pub fn upsert(conn: &Connection, c: &Value) -> DbResult<()> {
    let r = conn_to_row(c)?;
    conn.execute(
        "INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt)
         VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
           provider=excluded.provider, authType=excluded.authType, name=excluded.name,
           email=excluded.email, priority=excluded.priority, isActive=excluded.isActive,
           data=excluded.data, updatedAt=excluded.updatedAt",
        rusqlite::params![
            r.id,
            r.provider,
            r.auth_type,
            r.name,
            r.email,
            r.priority,
            r.is_active,
            r.data,
            r.created_at,
            r.updated_at
        ],
    )?;
    Ok(())
}

/// Activating a connection clears its backoff and every per-model lock, but only
/// when the patch actually asks for `active`.
pub fn reset_health_state_on_activation(existing: &Value, patch: &Value) -> Value {
    let is_activation = patch.get("testStatus").and_then(Value::as_str) == Some("active");
    if !is_activation {
        return patch.clone();
    }

    let mut normalized = match patch {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    normalized.insert("testStatus".into(), json!("active"));
    // `Object.hasOwn(patch, "lastError") ? patch.lastError : null`
    normalized.insert(
        "lastError".into(),
        patch.get("lastError").cloned().unwrap_or(Value::Null),
    );
    normalized.insert(
        "lastErrorAt".into(),
        patch.get("lastErrorAt").cloned().unwrap_or(Value::Null),
    );
    normalized.insert("errorCode".into(), Value::Null);
    normalized.insert("rateLimitedUntil".into(), Value::Null);
    normalized.insert("backoffLevel".into(), json!(0));

    if let Value::Object(existing_map) = existing {
        for key in existing_map.keys() {
            if key.starts_with(MODEL_LOCK_PREFIX) {
                normalized.insert(key.clone(), Value::Null);
            }
        }
    }

    Value::Object(normalized)
}

/// The select used everywhere, so the column list stays in one place.
const SELECT_ALL: &str = "SELECT * FROM providerConnections";

/// Connections, optionally filtered by provider and active flag.
pub fn get_provider_connections(
    conn: &Connection,
    provider: Option<&str>,
    is_active: Option<bool>,
) -> DbResult<Vec<Value>> {
    let mut sql = String::from(SELECT_ALL);
    let mut where_parts = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = provider {
        where_parts.push("provider = ?");
        params.push(Box::new(p.to_string()));
    }
    if let Some(a) = is_active {
        where_parts.push("isActive = ?");
        params.push(Box::new(if a { 1i64 } else { 0i64 }));
    }
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows: Vec<Value> = stmt
        .query_map(refs.as_slice(), row_to_conn)?
        .collect::<Result<_, _>>()?;

    let mut list = rows;
    // `(a.priority || 999)` — zero and null both fall to 999.
    list.sort_by_key(|c| priority_or(c, 999));
    Ok(list)
}

/// One connection by id.
pub fn get_provider_connection_by_id(conn: &Connection, id: &str) -> DbResult<Option<Value>> {
    conn.query_row(
        "SELECT * FROM providerConnections WHERE id = ?",
        [id],
        row_to_conn,
    )
    .optional()
    .map_err(Into::into)
}

/// `a.priority || fallback`: `0`, `null` and a missing key are all falsy.
fn priority_or(c: &Value, fallback: i64) -> i64 {
    match c.get("priority").and_then(Value::as_i64) {
        Some(0) | None => fallback,
        Some(p) => p,
    }
}

/// Reassign priority 1..n by `(priority, updatedAt DESC)`. Must run inside a
/// transaction.
pub fn reorder_in_tx(conn: &Connection, provider_id: &str) -> DbResult<()> {
    let mut stmt = conn.prepare("SELECT * FROM providerConnections WHERE provider = ?")?;
    let mut list: Vec<Value> = stmt
        .query_map([provider_id], row_to_conn)?
        .collect::<Result<_, _>>()?;
    drop(stmt);

    list.sort_by(|a, b| {
        let p_diff = priority_or(a, 0) - priority_or(b, 0);
        if p_diff != 0 {
            return p_diff.cmp(&0);
        }
        // `new Date(b.updatedAt || 0) - new Date(a.updatedAt || 0)`: newest
        // first. Missing or unparseable timestamps sort as the epoch.
        let b_t = sort_time(b);
        let a_t = sort_time(a);
        b_t.cmp(&a_t)
    });

    for (i, c) in list.iter().enumerate() {
        let Some(id) = c.get("id").and_then(Value::as_str) else {
            continue;
        };
        conn.execute(
            "UPDATE providerConnections SET priority = ? WHERE id = ?",
            rusqlite::params![(i as i64) + 1, id],
        )?;
    }
    Ok(())
}

fn sort_time(c: &Value) -> i64 {
    c.get("updatedAt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .and_then(crate::time::parse_iso)
        .map_or(0, |d| d.timestamp_millis())
}

/// Create a provider connection. Must run inside a transaction.
pub fn create_provider_connection(conn: &Connection, data: &Value) -> DbResult<Value> {
    let now = now_iso();
    let provider = data
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let all = {
        let mut stmt = conn.prepare("SELECT * FROM providerConnections WHERE provider = ?")?;
        let rows: Vec<Value> = stmt
            .query_map([provider], row_to_conn)?
            .collect::<Result<_, _>>()?;
        rows
    };

    if let Some(existing) = find_existing(&all, data) {
        // A name collision used to silently replace the stored apiKey, so a
        // script reusing names ("Key 1", "Key 2", …) destroyed pool entries with
        // no 409. Callers that mean "update this one" pass allowOverwrite.
        if data.get("allowOverwrite") == Some(&Value::Bool(false)) {
            let name = existing
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(DbError::ProviderNameConflict {
                message: format!(
                    "A connection named \"{name}\" already exists for provider \"{provider}\". \
                     Pass allowOverwrite: true to replace it."
                ),
                existing_id: existing
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                existing_name: name.to_string(),
            });
        }
        let normalized = reset_health_state_on_activation(&existing, data);
        let merged = merge_objects(&existing, &normalized, &now);
        upsert(conn, &merged)?;
        return Ok(merged);
    }

    let auth_type = data
        .get("authType")
        .and_then(Value::as_str)
        .unwrap_or("oauth");

    let mut connection_name = data.get("name").and_then(Value::as_str).map(str::to_string);
    if connection_name.is_none() && (auth_type == "oauth" || auth_type == "access_token") {
        let fallback = data
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Account {}", all.len() + 1));
        connection_name = Some(fallback);
    }

    let mut priority = data.get("priority").and_then(Value::as_i64);
    // `if (!connectionPriority)` — zero is falsy too.
    if priority.is_none() || priority == Some(0) {
        let max = all.iter().map(|c| priority_or(c, 0)).max().unwrap_or(0);
        priority = Some(max + 1);
    }

    let mut conn_obj = Map::new();
    conn_obj.insert("id".into(), json!(uuid::Uuid::new_v4().to_string()));
    conn_obj.insert("provider".into(), json!(provider));
    conn_obj.insert("authType".into(), json!(auth_type));
    conn_obj.insert(
        "name".into(),
        connection_name.map_or(Value::Null, Value::String),
    );
    conn_obj.insert("priority".into(), json!(priority));
    conn_obj.insert(
        "isActive".into(),
        data.get("isActive").cloned().unwrap_or(json!(true)),
    );
    conn_obj.insert("createdAt".into(), json!(now));
    conn_obj.insert("updatedAt".into(), json!(now));

    for f in OPTIONAL_FIELDS {
        match data.get(*f) {
            Some(v) if !v.is_null() => {
                conn_obj.insert((*f).into(), v.clone());
            }
            _ => {}
        }
    }
    if let Some(psd) = data.get("providerSpecificData")
        && psd.as_object().is_some_and(|m| !m.is_empty())
    {
        conn_obj.insert("providerSpecificData".into(), psd.clone());
    }
    if let Some(email) = data.get("email") {
        conn_obj.insert("email".into(), email.clone());
    }

    let conn_obj = Value::Object(conn_obj);
    upsert(conn, &conn_obj)?;
    // No reorder_in_tx here (#4311): the new row is already MAX(priority)+1, so
    // it sorts last and the rewrite would only add O(pool) statements per insert.
    Ok(conn_obj)
}

/// `{...existing, ...normalized, updatedAt: now}`.
fn merge_objects(existing: &Value, patch: &Value, now: &str) -> Value {
    let mut out = match existing {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    if let Value::Object(p) = patch {
        for (k, v) in p {
            out.insert(k.clone(), v.clone());
        }
    }
    out.insert("updatedAt".into(), json!(now));
    Value::Object(out)
}

/// The create-time dedup rules, in order.
fn find_existing(all: &[Value], data: &Value) -> Option<Value> {
    let auth_type = data.get("authType").and_then(Value::as_str);
    let provider = data.get("provider").and_then(Value::as_str).unwrap_or("");
    let incoming_email = data.get("email").and_then(Value::as_str);
    let psd = data.get("providerSpecificData");
    let incoming_username = psd.and_then(|p| p.get("username")).and_then(Value::as_str);
    let incoming_ws = psd
        .and_then(|p| p.get("chatgptAccountId"))
        .and_then(Value::as_str);

    if let Some(email) = incoming_email.filter(|_| auth_type == Some("oauth")) {
        return all
            .iter()
            .find(|c| {
                if c.get("authType").and_then(Value::as_str) != Some("oauth") {
                    return false;
                }
                if c.get("email").and_then(Value::as_str) != Some(email) {
                    return false;
                }
                let existing_psd = c.get("providerSpecificData");
                let existing_ws = existing_psd
                    .and_then(|p| p.get("chatgptAccountId"))
                    .and_then(Value::as_str);

                if provider == "codex" {
                    // Multiple OAuth grants can share an email. Only collapse when
                    // both rows expose the same ChatGPT account id, or the second
                    // account's rotated refresh token overwrites the first's.
                    return incoming_ws.is_some()
                        && existing_ws.is_some()
                        && incoming_ws == existing_ws;
                }

                if incoming_ws.is_some() && existing_ws.is_some() {
                    return incoming_ws == existing_ws;
                }
                if incoming_ws.is_some() != existing_ws.is_some() {
                    return false;
                }

                let existing_username = existing_psd
                    .and_then(|p| p.get("username"))
                    .and_then(Value::as_str);
                if incoming_username.is_some() && existing_username.is_some() {
                    return incoming_username == existing_username;
                }
                if incoming_username.is_some() || existing_username.is_some() {
                    return false;
                }
                true
            })
            .cloned();
    }

    if auth_type == Some("apikey") {
        let name = data.get("name").and_then(Value::as_str)?;
        return all
            .iter()
            .find(|c| {
                c.get("authType").and_then(Value::as_str) == Some("apikey")
                    && c.get("name").and_then(Value::as_str) == Some(name)
            })
            .cloned();
    }

    // access_token never dedups; the user manages duplicates by hand.
    None
}

/// Merge a patch into a connection. Must run inside a transaction.
pub fn update_provider_connection(
    conn: &Connection,
    id: &str,
    data: &Value,
) -> DbResult<Option<Value>> {
    let Some(existing) = get_provider_connection_by_id(conn, id)? else {
        return Ok(None);
    };
    let normalized = reset_health_state_on_activation(&existing, data);
    let merged = merge_objects(&existing, &normalized, &now_iso());
    upsert(conn, &merged)?;
    if data.get("priority").is_some() {
        let provider = existing
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        reorder_in_tx(conn, provider)?;
    }
    Ok(Some(merged))
}

/// Delete a connection. Must run inside a transaction.
pub fn delete_provider_connection(conn: &Connection, id: &str) -> DbResult<bool> {
    let provider: Option<String> = conn
        .query_row(
            "SELECT provider FROM providerConnections WHERE id = ?",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(provider) = provider else {
        return Ok(false);
    };
    conn.execute("DELETE FROM providerConnections WHERE id = ?", [id])?;
    reorder_in_tx(conn, &provider)?;
    Ok(true)
}

/// Delete every connection for a provider; returns the count removed.
pub fn delete_provider_connections_by_provider(
    conn: &Connection,
    provider_id: &str,
) -> DbResult<i64> {
    let before: i64 = conn.query_row(
        "SELECT COUNT(*) AS n FROM providerConnections WHERE provider = ?",
        [provider_id],
        |r| r.get(0),
    )?;
    conn.execute(
        "DELETE FROM providerConnections WHERE provider = ?",
        [provider_id],
    )?;
    Ok(before)
}

/// Drop null optional fields and empty `providerSpecificData`. Returns the
/// number of values removed.
pub fn cleanup_provider_connections(conn: &Connection) -> DbResult<u32> {
    let mut cleaned = 0u32;
    let mut stmt = conn.prepare(SELECT_ALL)?;
    let rows: Vec<Value> = stmt.query_map([], row_to_conn)?.collect::<Result<_, _>>()?;
    drop(stmt);

    for row in rows {
        let Value::Object(mut conn_map) = row else {
            continue;
        };
        let mut dirty = false;
        for f in CLEANUP_FIELDS {
            if conn_map.get(*f).is_some_and(Value::is_null) {
                conn_map.shift_remove(*f);
                cleaned += 1;
                dirty = true;
            }
        }
        let empty_psd = conn_map
            .get("providerSpecificData")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty);
        if empty_psd {
            conn_map.shift_remove("providerSpecificData");
            cleaned += 1;
            dirty = true;
        }
        if dirty {
            upsert(conn, &Value::Object(conn_map))?;
        }
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("providerConnections").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn row_to_conn_places_fixed_columns_after_blob_keys() {
        let conn = db();
        conn.execute(
            "INSERT INTO providerConnections(id, provider, authType, name, email, priority, isActive, data, createdAt, updatedAt)
             VALUES('c1','openai','oauth','n','e',3,1,'{\"a\":1,\"z\":2}','t0','t1')",
            [],
        )
        .unwrap();
        let c = get_provider_connection_by_id(&conn, "c1").unwrap().unwrap();
        let keys: Vec<&String> = c.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "a");
        assert_eq!(keys[1], "z");
        assert_eq!(keys[2], "id");
        assert_eq!(keys[keys.len() - 1], "updatedAt");
        assert_eq!(c["isActive"], json!(true));
        assert_eq!(c["priority"], json!(3));
    }

    #[test]
    fn conn_to_row_keeps_provider_specific_data_in_the_blob() {
        let c = json!({
            "id": "c1", "provider": "openai", "authType": "oauth",
            "name": "n", "priority": 1, "isActive": true,
            "createdAt": "t0", "updatedAt": "t1",
            "providerSpecificData": { "modelLock_gpt/x": 5 },
            "displayName": "d"
        });
        let r = conn_to_row(&c).unwrap();
        assert_eq!(r.is_active, 1);
        let blob: Value = serde_json::from_str(&r.data).unwrap();
        assert_eq!(blob["providerSpecificData"]["modelLock_gpt/x"], json!(5));
        assert!(blob.get("id").is_none());
        assert!(blob.get("updatedAt").is_none());
    }

    #[test]
    fn is_active_false_is_the_only_zero() {
        let base = json!({ "id": "x", "provider": "p", "authType": "oauth", "createdAt": "t", "updatedAt": "t" });
        let mut c = base.clone();
        c["isActive"] = json!(false);
        assert_eq!(conn_to_row(&c).unwrap().is_active, 0);

        let mut c = base.clone();
        c["isActive"] = json!(0);
        assert_eq!(
            conn_to_row(&c).unwrap().is_active,
            1,
            "only literal false is 0"
        );

        assert_eq!(conn_to_row(&base).unwrap().is_active, 1);
    }

    #[test]
    fn priority_zero_falls_to_999_when_sorting() {
        assert_eq!(priority_or(&json!({ "priority": 0 }), 999), 999);
        assert_eq!(priority_or(&json!({ "priority": null }), 999), 999);
        assert_eq!(priority_or(&json!({}), 999), 999);
        assert_eq!(priority_or(&json!({ "priority": 2 }), 999), 2);
    }

    #[test]
    fn activation_clears_model_locks_and_backoff() {
        let existing = json!({
            "testStatus": "error", "backoffLevel": 4,
            "modelLock_gpt/x": 1710000000000u64, "modelLock_a:b": 1, "keep": 1
        });
        let patch = json!({ "testStatus": "active" });
        let out = reset_health_state_on_activation(&existing, &patch);
        assert_eq!(out["testStatus"], json!("active"));
        assert_eq!(out["backoffLevel"], json!(0));
        assert_eq!(out["modelLock_gpt/x"], Value::Null);
        assert_eq!(out["modelLock_a:b"], Value::Null);
        // The result is built from the patch alone: `existing` contributes only
        // the modelLock keys it nulls out, so an unrelated key is not carried.
        assert_eq!(out["keep"], Value::Null);
        assert_eq!(out["errorCode"], Value::Null);
        assert_eq!(out["rateLimitedUntil"], Value::Null);
        assert_eq!(out["lastError"], Value::Null);
    }

    #[test]
    fn non_activation_patch_passes_through_untouched() {
        let existing = json!({ "modelLock_a": 1 });
        let patch = json!({ "testStatus": "error", "lastError": "boom" });
        let out = reset_health_state_on_activation(&existing, &patch);
        assert_eq!(out, patch, "must not mutate a non-activating patch");
    }

    #[test]
    fn activation_keeps_an_explicit_last_error() {
        let patch = json!({ "testStatus": "active", "lastError": "kept" });
        let out = reset_health_state_on_activation(&json!({}), &patch);
        assert_eq!(out["lastError"], json!("kept"));
    }

    #[test]
    fn codex_does_not_dedup_on_email_alone() {
        let all = vec![json!({
            "id": "old", "provider": "codex", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": { "chatgptAccountId": "ws-1" }
        })];
        // Same email, no account id on the incoming side: must NOT collapse.
        let incoming = json!({
            "provider": "codex", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": {}
        });
        assert!(find_existing(&all, &incoming).is_none());

        // Same account id on both sides: collapse.
        let incoming = json!({
            "provider": "codex", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": { "chatgptAccountId": "ws-1" }
        });
        assert_eq!(find_existing(&all, &incoming).unwrap()["id"], json!("old"));
    }

    #[test]
    fn one_sided_username_is_a_distinct_identity() {
        let all = vec![json!({
            "id": "old", "provider": "claude", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": { "username": "u1" }
        })];
        let incoming = json!({
            "provider": "claude", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": {}
        });
        assert!(find_existing(&all, &incoming).is_none());

        let incoming = json!({
            "provider": "claude", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": { "username": "u1" }
        });
        assert!(find_existing(&all, &incoming).is_some());
    }

    #[test]
    fn bare_email_with_no_usernames_dedups() {
        let all = vec![json!({
            "id": "old", "provider": "claude", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": {}
        })];
        let incoming = json!({
            "provider": "claude", "authType": "oauth", "email": "a@b.c",
            "providerSpecificData": {}
        });
        assert_eq!(find_existing(&all, &incoming).unwrap()["id"], json!("old"));
    }

    #[test]
    fn access_token_never_dedups() {
        let all = vec![json!({
            "id": "old", "provider": "p", "authType": "access_token", "name": "same",
            "email": "a@b.c"
        })];
        let incoming = json!({
            "provider": "p", "authType": "access_token", "name": "same", "email": "a@b.c"
        });
        assert!(find_existing(&all, &incoming).is_none());
    }

    #[test]
    fn create_assigns_incrementing_priority_and_reorders() {
        let conn = db();
        let first = create_provider_connection(
            &conn,
            &json!({ "provider": "openai", "authType": "oauth", "email": "a@b.c" }),
        )
        .unwrap();
        assert_eq!(first["priority"], json!(1));
        assert_eq!(first["name"], json!("a@b.c"));

        let second = create_provider_connection(
            &conn,
            &json!({ "provider": "openai", "authType": "oauth", "email": "d@e.f" }),
        )
        .unwrap();
        assert_eq!(second["priority"], json!(2));
        assert_ne!(first["id"], second["id"]);
    }

    #[test]
    fn create_rejects_a_name_collision_when_overwrite_is_false() {
        let conn = db();
        let first = create_provider_connection(
            &conn,
            &json!({ "provider": "p", "authType": "apikey", "name": "Key 1", "apiKey": "k1" }),
        )
        .unwrap();

        let err = create_provider_connection(
            &conn,
            &json!({
                "provider": "p", "authType": "apikey", "name": "Key 1",
                "apiKey": "k2", "allowOverwrite": false
            }),
        )
        .unwrap_err();
        match err {
            DbError::ProviderNameConflict {
                existing_id,
                existing_name,
                ..
            } => {
                assert_eq!(existing_id, first["id"].as_str().unwrap());
                assert_eq!(existing_name, "Key 1");
            }
            other => panic!("expected ProviderNameConflict, got {other:?}"),
        }
        // The stored key is untouched.
        let stored = get_provider_connection_by_id(&conn, first["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(stored["apiKey"], json!("k1"));

        // Without the flag the collision still overwrites, as before.
        create_provider_connection(
            &conn,
            &json!({ "provider": "p", "authType": "apikey", "name": "Key 1", "apiKey": "k3" }),
        )
        .unwrap();
        let stored = get_provider_connection_by_id(&conn, first["id"].as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(stored["apiKey"], json!("k3"));
    }

    #[test]
    fn create_does_not_reorder_existing_rows() {
        let conn = db();
        // A gap in the priorities: `reorderInTx` would normalize these to 1..n.
        conn.execute(
            "INSERT INTO providerConnections(id, provider, authType, name, isActive, priority, data, createdAt, updatedAt)
             VALUES('old','p','apikey','old',1,7,'{}','t','t')",
            [],
        )
        .unwrap();

        create_provider_connection(
            &conn,
            &json!({ "provider": "p", "authType": "apikey", "name": "new", "apiKey": "k" }),
        )
        .unwrap();

        let old_priority: i64 = conn
            .query_row(
                "SELECT priority FROM providerConnections WHERE id = 'old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_priority, 7,
            "insert must not rewrite existing priorities"
        );
    }

    #[test]
    fn reorder_renumbers_by_priority() {
        let conn = db();
        for (id, pr) in [("a", 5), ("b", 9)] {
            conn.execute(
                "INSERT INTO providerConnections(id, provider, authType, isActive, priority, data, createdAt, updatedAt)
                 VALUES(?, 'p', 'oauth', 1, ?, '{}', 't', 't')",
                rusqlite::params![id, pr],
            )
            .unwrap();
        }
        reorder_in_tx(&conn, "p").unwrap();
        let get = |id: &str| -> i64 {
            conn.query_row(
                "SELECT priority FROM providerConnections WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(get("a"), 1);
        assert_eq!(get("b"), 2);
    }

    #[test]
    fn create_skips_null_optional_fields_but_keeps_email() {
        let conn = db();
        let c = create_provider_connection(
            &conn,
            &json!({
                "provider": "p", "authType": "apikey", "name": "n",
                "apiKey": "k", "displayName": null, "email": null
            }),
        )
        .unwrap();
        assert_eq!(c["apiKey"], json!("k"));
        assert!(
            c.get("displayName").is_none(),
            "null optional field was copied"
        );
        // `data.email !== undefined` keeps the key even when null.
        assert_eq!(c["email"], Value::Null);
    }

    #[test]
    fn create_drops_an_empty_provider_specific_data() {
        let conn = db();
        let c = create_provider_connection(
            &conn,
            &json!({ "provider": "p", "authType": "apikey", "name": "n", "providerSpecificData": {} }),
        )
        .unwrap();
        assert!(c.get("providerSpecificData").is_none());
    }

    #[test]
    fn reorder_uses_updated_at_desc_within_equal_priority() {
        let conn = db();
        for (id, pr, up) in [
            ("a", 1, "2026-01-01T00:00:00.000Z"),
            ("b", 1, "2026-06-01T00:00:00.000Z"),
            ("c", 2, "2026-01-01T00:00:00.000Z"),
        ] {
            conn.execute(
                "INSERT INTO providerConnections(id, provider, authType, isActive, data, createdAt, updatedAt)
                 VALUES(?, 'p', 'oauth', 1, '{}', 't', ?)",
                rusqlite::params![id, up],
            )
            .unwrap();
            conn.execute(
                "UPDATE providerConnections SET priority = ? WHERE id = ?",
                rusqlite::params![pr, id],
            )
            .unwrap();
        }
        reorder_in_tx(&conn, "p").unwrap();
        let get = |id: &str| -> i64 {
            conn.query_row(
                "SELECT priority FROM providerConnections WHERE id = ?",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(get("b"), 1, "newest first within equal priority");
        assert_eq!(get("a"), 2);
        assert_eq!(get("c"), 3);
    }

    #[test]
    fn update_merges_and_does_not_reorder_without_a_priority() {
        let conn = db();
        let c = create_provider_connection(
            &conn,
            &json!({ "provider": "p", "authType": "apikey", "name": "n", "apiKey": "k1" }),
        )
        .unwrap();
        let id = c["id"].as_str().unwrap().to_string();
        let updated = update_provider_connection(&conn, &id, &json!({ "apiKey": "k2" }))
            .unwrap()
            .unwrap();
        assert_eq!(updated["apiKey"], json!("k2"));
        assert_eq!(updated["name"], json!("n"));
        assert_eq!(updated["createdAt"], c["createdAt"]);
    }

    #[test]
    fn update_missing_row_returns_none() {
        let conn = db();
        assert!(
            update_provider_connection(&conn, "nope", &json!({ "apiKey": "k" }))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn delete_returns_false_for_unknown_id() {
        let conn = db();
        assert!(!delete_provider_connection(&conn, "nope").unwrap());
    }

    #[test]
    fn cleanup_removes_null_fields_and_counts_them() {
        let conn = db();
        conn.execute(
            "INSERT INTO providerConnections(id, provider, authType, isActive, data, createdAt, updatedAt)
             VALUES('c1','p','oauth',1,'{\"apiKey\":null,\"displayName\":\"d\",\"providerSpecificData\":{}}','t','t')",
            [],
        )
        .unwrap();
        let cleaned = cleanup_provider_connections(&conn).unwrap();
        // `rowToConn` always materializes email/name/priority from their
        // columns, so the NULL email column is a third removal.
        assert_eq!(
            cleaned, 3,
            "apiKey null + empty providerSpecificData + null email"
        );
        let c = get_provider_connection_by_id(&conn, "c1").unwrap().unwrap();
        assert!(c.get("apiKey").is_none());
        assert!(c.get("providerSpecificData").is_none());
        assert_eq!(c["displayName"], json!("d"));
    }

    #[test]
    fn get_filters_and_sorts() {
        let conn = db();
        create_provider_connection(
            &conn,
            &json!({ "provider": "a", "authType": "apikey", "name": "n1", "apiKey": "k" }),
        )
        .unwrap();
        create_provider_connection(
            &conn,
            &json!({ "provider": "b", "authType": "apikey", "name": "n2", "apiKey": "k" }),
        )
        .unwrap();

        assert_eq!(
            get_provider_connections(&conn, Some("a"), None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            get_provider_connections(&conn, None, Some(true))
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            get_provider_connections(&conn, None, Some(false))
                .unwrap()
                .len(),
            0
        );
    }
}
