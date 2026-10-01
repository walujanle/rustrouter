//! `_meta` key/value access.

use rusqlite::{Connection, OptionalExtension};

use crate::error::DbResult;

/// Read a `_meta` value, falling back to the supplied default.
pub fn get_meta(conn: &Connection, key: &str, default: Option<&str>) -> DbResult<Option<String>> {
    let found: Option<String> = conn
        .query_row("SELECT value FROM _meta WHERE key = ?", [key], |r| r.get(0))
        .optional()?;
    Ok(found.or_else(|| default.map(str::to_string)))
}

/// Read a `_meta` value as an integer, falling back when absent or unparseable.
pub fn get_meta_i64(conn: &Connection, key: &str, default: i64) -> DbResult<i64> {
    Ok(get_meta(conn, key, None)?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(default))
}

/// Write a `_meta` value.
pub fn set_meta(conn: &Connection, key: &str, value: &str) -> DbResult<()> {
    conn.execute(
        "INSERT INTO _meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_default() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::create_table_sql("_meta").unwrap().as_str())
            .unwrap();

        assert_eq!(
            get_meta(&conn, "missing", Some("fallback"))
                .unwrap()
                .unwrap(),
            "fallback"
        );
        assert!(get_meta(&conn, "missing", None).unwrap().is_none());

        set_meta(&conn, "schemaVersion", "1").unwrap();
        assert_eq!(get_meta_i64(&conn, "schemaVersion", 0).unwrap(), 1);

        set_meta(&conn, "schemaVersion", "2").unwrap();
        assert_eq!(get_meta_i64(&conn, "schemaVersion", 0).unwrap(), 2);
    }
}
