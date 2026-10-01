//! Versioned migration chain plus the additive schema sync.
//!
//! The one-time legacy-JSON import is deliberately not implemented.
//! `docs/DB-PARITY.md` records why: the data file pre-exists, so re-running an
//! importer could only ever re-import stale JSON over live rows.

use rusqlite::{Connection, TransactionBehavior};

use crate::error::DbResult;
use crate::meta_store::{get_meta_i64, set_meta};
use crate::schema::{SCHEMA_VERSION, TABLES};

/// One versioned migration.
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub up: fn(&Connection) -> DbResult<()>,
}

/// The migration chain, sorted by version. `m001` creates every table and index
/// from `TABLES`.
pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "initial",
    up: m001_initial,
}];

/// The highest known migration version.
pub fn latest_version() -> i64 {
    MIGRATIONS.iter().map(|m| m.version).max().unwrap_or(0)
}

fn m001_initial(conn: &Connection) -> DbResult<()> {
    for table in TABLES {
        conn.execute_batch(&table.create_sql())?;
        for idx in table.indexes {
            conn.execute_batch(idx)?;
        }
    }
    Ok(())
}

/// Run every migration newer than the stored `schemaVersion`.
pub fn run_versioned_migrations(conn: &Connection) -> DbResult<()> {
    conn.execute_batch(&crate::schema::create_table_sql("_meta").expect("_meta is declared"))?;

    let current = get_meta_i64(conn, "schemaVersion", 0)?;
    let target = latest_version();
    if current > target {
        // The shared file was written by a newer build (9router or a later
        // rustrouter). This build knows fewer migrations, so it must not
        // downgrade the stamp or invent one: leave the file as the newer side
        // wrote it and continue read-mostly. See `docs/PLAN.md` D3.
        tracing::warn!(
            "[DB][migrate] stored schemaVersion {current} is ahead of this build's {target}; \
             leaving the database as written"
        );
        return Ok(());
    }
    if current == target {
        return Ok(());
    }

    for m in MIGRATIONS.iter().filter(|m| m.version > current) {
        // IMMEDIATE, per the write rule: a deferred transaction that later
        // upgrades can fail with SQLITE_BUSY_SNAPSHOT under WAL. The
        // `&Connection` entry point keeps the borrow shared; `new_unchecked` is
        // the only constructor that takes one.
        let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        (m.up)(&tx)?;
        set_meta(&tx, "schemaVersion", &m.version.to_string())?;
        tx.commit()?;
        tracing::info!("[DB][migrate] applied #{} {}", m.version, m.name);
    }
    Ok(())
}

/// Additive schema sync: creates missing tables, adds missing columns,
/// re-creates indexes. Never drops or renames.
pub fn sync_schema_from_tables(conn: &Connection) -> DbResult<()> {
    for table in TABLES {
        conn.execute_batch(&table.create_sql())?;

        let existing = table_columns(conn, table.name)?;
        for (col, def) in table.columns {
            if existing.iter().any(|c| c == col) {
                continue;
            }
            // SQLite rejects PRIMARY KEY / UNIQUE in ALTER TABLE ADD COLUMN,
            // so those clauses are stripped with a regex and the result is
            // trimmed, leaving any interior whitespace intact.
            let safe = strip_column_constraints(def);
            let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table.name, col, safe);
            match conn.execute_batch(&sql) {
                Ok(()) => tracing::info!("[DB][sync] +column {}.{}", table.name, col),
                Err(e) => {
                    tracing::warn!("[DB][sync] add column {}.{} failed: {e}", table.name, col)
                }
            }
        }

        for idx in table.indexes {
            let _ = conn.execute_batch(idx);
        }
    }
    Ok(())
}

fn table_columns(conn: &Connection, name: &str) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({name})"))?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Remove `PRIMARY KEY [AUTOINCREMENT]` and `UNIQUE`, case-insensitively, then
/// trim.
fn strip_column_constraints(def: &str) -> String {
    let lower = def.to_ascii_lowercase();
    let mut out = String::with_capacity(def.len());

    let bytes = def.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if lower[i..].starts_with("primary key") {
            let after = i + "primary key".len();
            let rest = &lower[after..];
            let skip = if rest.starts_with(" autoincrement") {
                after + " autoincrement".len()
            } else {
                after
            };
            i = skip;
            continue;
        }
        if lower[i..].starts_with("unique") {
            i += "unique".len();
            continue;
        }
        // Copy the byte. Column definitions are ASCII, so byte-wise stepping
        // is safe here.
        out.push(bytes[i] as char);
        i += 1;
    }
    out.trim().to_string()
}

/// Apply migrations and the additive sync in one call.
pub fn ensure_schema(conn: &Connection) -> DbResult<()> {
    run_versioned_migrations(conn)?;
    sync_schema_from_tables(conn)?;
    Ok(())
}

/// The boot-time entry point.
///
/// Order matters. Freshness is captured before anything stamps `_meta`, a
/// lightweight backup is taken when the stored backup version is behind the
/// code's `SCHEMA_VERSION`, and only then do the migrations and the additive
/// sync run. The backup is best-effort: a failure logs and continues, because
/// refusing to boot over a backup would be worse than the missing safety net.
pub fn migrate_on_boot(
    conn: &Connection,
    paths: &crate::paths::Paths,
    app_version: &str,
) -> DbResult<()> {
    let fresh = is_fresh_db(conn);
    crate::backup::prune_old_backups(paths);

    // `_meta` has to exist before the stored version can be read.
    conn.execute_batch(&crate::schema::create_table_sql("_meta").expect("_meta is declared"))?;

    let stored = stored_backup_schema_version(conn)?;
    if !fresh && stored < SCHEMA_VERSION {
        let label = format!("schema-{stored}-to-{SCHEMA_VERSION}");
        match backup_now(conn, paths, app_version, &label) {
            Ok(dir) => {
                tracing::info!(
                    "[DB][migrate] pre-schema backup {stored} -> {SCHEMA_VERSION}: {}",
                    dir.display()
                );
                crate::backup::prune_old_backups(paths);
            }
            Err(error) => {
                tracing::warn!("[DB][migrate] pre-schema backup failed (continuing): {error}")
            }
        }
    }

    run_versioned_migrations(conn)?;
    sync_schema_from_tables(conn)?;
    stamp_backup_schema_version(conn)?;
    Ok(())
}

/// `_meta` is empty or absent.
fn is_fresh_db(conn: &Connection) -> bool {
    conn.query_row("SELECT COUNT(*) FROM _meta", [], |r| r.get::<_, i64>(0))
        .map(|c| c == 0)
        .unwrap_or(true)
}

/// Create the backup directory and write the lite backup into it.
fn backup_now(
    conn: &Connection,
    paths: &crate::paths::Paths,
    app_version: &str,
    label: &str,
) -> DbResult<std::path::PathBuf> {
    let dir = crate::backup::make_backup_dir(paths, label, app_version)?;
    crate::backup::backup_db_lite(conn, &dir.join("data.sqlite"))?;
    Ok(dir)
}

/// The stored `backupSchemaVersion`, used to decide whether a pre-change
/// backup is due.
pub fn stored_backup_schema_version(conn: &Connection) -> DbResult<i64> {
    get_meta_i64(conn, "backupSchemaVersion", 0)
}

/// True when the stored backup version is behind the code's schema version.
pub fn schema_change_pending(conn: &Connection) -> DbResult<bool> {
    Ok(stored_backup_schema_version(conn)? < SCHEMA_VERSION)
}

/// Stamp the version just reached so future boots skip the backup.
pub fn stamp_backup_schema_version(conn: &Connection) -> DbResult<()> {
    set_meta(conn, "backupSchemaVersion", &SCHEMA_VERSION.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::schema::PRAGMA_SQL).unwrap();
        conn
    }

    #[test]
    fn ensure_schema_creates_everything_and_stamps_version() {
        let conn = mem();
        ensure_schema(&conn).unwrap();
        assert_eq!(get_meta_i64(&conn, "schemaVersion", 0).unwrap(), 1);

        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, TABLES.len() as i64);
    }

    #[test]
    fn ensure_schema_is_idempotent() {
        let conn = mem();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        assert_eq!(get_meta_i64(&conn, "schemaVersion", 0).unwrap(), 1);
    }

    #[test]
    fn sync_adds_a_missing_column() {
        let conn = mem();
        // Simulate an older DB that lacks the column.
        conn.execute_batch(
            "CREATE TABLE providerConnections (id TEXT PRIMARY KEY, provider TEXT NOT NULL, data TEXT NOT NULL)",
        )
        .unwrap();
        sync_schema_from_tables(&conn).unwrap();
        let cols = table_columns(&conn, "providerConnections").unwrap();
        assert!(cols.contains(&"updatedAt".to_string()), "{cols:?}");
    }

    #[test]
    fn strip_removes_pk_and_unique_but_keeps_the_rest() {
        assert_eq!(strip_column_constraints("TEXT PRIMARY KEY"), "TEXT");
        // `PRIMARY KEY` is removed with a bare replace, leaving both spaces.
        assert_eq!(
            strip_column_constraints("INTEGER PRIMARY KEY CHECK (id = 1)"),
            "INTEGER  CHECK (id = 1)"
        );
        assert_eq!(
            strip_column_constraints("TEXT UNIQUE NOT NULL"),
            "TEXT  NOT NULL"
        );
        assert_eq!(
            strip_column_constraints("INTEGER PRIMARY KEY AUTOINCREMENT"),
            "INTEGER"
        );
    }

    #[test]
    fn backup_schema_version_gates_the_backup() {
        let conn = mem();
        ensure_schema(&conn).unwrap();
        assert!(schema_change_pending(&conn).unwrap());
        stamp_backup_schema_version(&conn).unwrap();
        assert!(!schema_change_pending(&conn).unwrap());
    }

    #[test]
    fn boot_backs_up_before_a_schema_change_and_stamps_after() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = crate::paths::Paths::new(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();

        // First boot: fresh, so no backup; the version is stamped.
        let conn = Connection::open(dir.path().join("data.sqlite")).unwrap();
        conn.execute_batch(crate::schema::PRAGMA_SQL).unwrap();
        migrate_on_boot(&conn, &paths, "0.2.0").unwrap();
        assert!(!schema_change_pending(&conn).unwrap());
        assert_eq!(std::fs::read_dir(&paths.backups_dir).unwrap().count(), 0);

        // Simulate an older file whose backup version is behind: boot again with
        // the stored gate cleared and expect exactly one backup directory.
        conn.execute(
            "INSERT INTO _meta(key, value) VALUES('backupSchemaVersion','0')
             ON CONFLICT(key) DO UPDATE SET value='0'",
            [],
        )
        .unwrap();
        migrate_on_boot(&conn, &paths, "0.2.0").unwrap();
        assert_eq!(std::fs::read_dir(&paths.backups_dir).unwrap().count(), 1);
        assert!(!schema_change_pending(&conn).unwrap());
    }
}
