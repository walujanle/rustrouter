//! Scoped key/value access over the `kv` table.
//!
//! Every scope shares one table and one composite primary key. `ON CONFLICT`
//! must name both columns or the upsert throws.

use rusqlite::{Connection, OptionalExtension};

use crate::error::DbResult;

/// The four scopes the application actually uses. `mitmAlias` rows are read
/// back but never written, so they are addressed by literal in the export path.
pub const SCOPE_MODEL_ALIASES: &str = "modelAliases";
pub const SCOPE_CUSTOM_MODELS: &str = "customModels";
pub const SCOPE_PRICING: &str = "pricing";
pub const SCOPE_DISABLED_MODELS: &str = "disabledModels";

pub fn get(conn: &Connection, scope: &str, key: &str) -> DbResult<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM kv WHERE scope = ? AND key = ?",
            [scope, key],
            |r| r.get(0),
        )
        .optional()?)
}

/// `makeKv(scope).getAll()`.
///
/// No `ORDER BY`: the query relies on the table's natural order, which is
/// rowid — i.e. insertion order. Callers turn this into an object whose key
/// order is serialised, so sorting here would change the response bytes.
pub fn get_all(conn: &Connection, scope: &str) -> DbResult<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM kv WHERE scope = ?")?;
    let rows = stmt.query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn set(conn: &Connection, scope: &str, key: &str, value: &str) -> DbResult<()> {
    conn.execute(
        "INSERT INTO kv(scope, key, value) VALUES(?, ?, ?) ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
        [scope, key, value],
    )?;
    Ok(())
}

pub fn remove(conn: &Connection, scope: &str, key: &str) -> DbResult<()> {
    conn.execute("DELETE FROM kv WHERE scope = ? AND key = ?", [scope, key])?;
    Ok(())
}

pub fn clear(conn: &Connection, scope: &str) -> DbResult<()> {
    conn.execute("DELETE FROM kv WHERE scope = ?", [scope])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::create_table_sql("kv").unwrap().as_str())
            .unwrap();
        conn
    }

    #[test]
    fn upsert_needs_the_composite_conflict_target() {
        let conn = db();
        set(&conn, SCOPE_PRICING, "openai", r#"{"gpt":1}"#).unwrap();
        set(&conn, SCOPE_PRICING, "openai", r#"{"gpt":2}"#).unwrap();
        assert_eq!(
            get(&conn, SCOPE_PRICING, "openai").unwrap().unwrap(),
            r#"{"gpt":2}"#
        );
    }

    #[test]
    fn same_key_in_two_scopes_stays_separate() {
        let conn = db();
        set(&conn, SCOPE_MODEL_ALIASES, "k", "a").unwrap();
        set(&conn, SCOPE_PRICING, "k", "b").unwrap();
        assert_eq!(get(&conn, SCOPE_MODEL_ALIASES, "k").unwrap().unwrap(), "a");
        assert_eq!(get(&conn, SCOPE_PRICING, "k").unwrap().unwrap(), "b");
    }

    #[test]
    fn clear_is_scoped() {
        let conn = db();
        set(&conn, SCOPE_MODEL_ALIASES, "k", "a").unwrap();
        set(&conn, SCOPE_PRICING, "k", "b").unwrap();
        clear(&conn, SCOPE_MODEL_ALIASES).unwrap();
        assert!(get(&conn, SCOPE_MODEL_ALIASES, "k").unwrap().is_none());
        assert!(get(&conn, SCOPE_PRICING, "k").unwrap().is_some());
    }
}
