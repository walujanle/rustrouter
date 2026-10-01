//! Safety backups taken before a schema change.
//!
//! `requestDetails` is excluded: it is an auto-pruned observability log, and
//! including it turns a few-MB backup into a copy of the whole database.

use std::path::{Path, PathBuf};

use crate::error::{DbError, DbResult};
use crate::paths::Paths;

pub const KEEP_BACKUPS: usize = 3;
pub const BACKUP_EXCLUDE_TABLES: &[&str] = &["requestDetails"];

/// `makeBackupDir(label)`: `${label}-${version}-${timestampSlug()}`.
pub fn make_backup_dir(paths: &Paths, label: &str, app_version: &str) -> DbResult<PathBuf> {
    paths
        .ensure_dirs()
        .map_err(|e| DbError::io(&paths.data_dir, e))?;
    let dir = paths.backups_dir.join(format!(
        "{label}-{app_version}-{}",
        crate::time::timestamp_slug()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| DbError::io(&dir, e))?;
    Ok(dir)
}

/// `backupDbLite`: copy every table except the excluded ones into a fresh file
/// via `ATTACH`.
///
/// The structure is recreated by string-replacing `CREATE TABLE ` with
/// `CREATE TABLE bak.` on the stored DDL. SQLite's own backup API would be
/// cleaner, but the copy deliberately omits `requestDetails` — so the table
/// list is read from `sqlite_master` and filtered here too.
pub fn backup_db_lite(conn: &rusqlite::Connection, dest: &Path) -> DbResult<PathBuf> {
    if dest.exists() {
        std::fs::remove_file(dest).map_err(|e| DbError::io(dest, e))?;
    }

    // A bound parameter is not allowed in ATTACH, so the path is quoted by
    // doubling single quotes.
    let escaped = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS bak"))?;

    let result = (|| -> DbResult<()> {
        let mut stmt = conn.prepare(
            "SELECT name, sql FROM main.sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows: Vec<(String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        for (name, sql) in rows {
            if BACKUP_EXCLUDE_TABLES.contains(&name.as_str()) {
                continue;
            }
            let Some(sql) = sql else { continue };
            let create = rewrite_create_for_bak(&sql);
            conn.execute_batch(&create)?;
            conn.execute_batch(&format!("INSERT INTO bak.{name} SELECT * FROM main.{name}"))?;
        }
        Ok(())
    })();

    // DETACH must run even when the copy failed, or the attachment leaks.
    let detach = conn.execute_batch("DETACH DATABASE bak");
    result?;
    detach?;
    Ok(dest.to_path_buf())
}

/// `sql.replace(/CREATE TABLE\s+/i, "CREATE TABLE bak.")`.
///
/// Only the first occurrence, case-insensitively, matching JS `String.replace`
/// with a non-global regex.
fn rewrite_create_for_bak(sql: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let needle = "create table";
    if let Some(pos) = lower.find(needle) {
        let after = pos + needle.len();
        // Consume the whitespace run the regex's `\s+` would have matched.
        let ws_end = sql[after..]
            .find(|c: char| !c.is_whitespace())
            .map(|n| after + n)
            .unwrap_or(sql.len());
        let mut out = String::with_capacity(sql.len() + 4);
        out.push_str(&sql[..pos]);
        out.push_str("CREATE TABLE bak.");
        out.push_str(&sql[ws_end..]);
        return out;
    }
    sql.to_string()
}

/// `pruneOldBackups()`: keep the newest `KEEP_BACKUPS` directories by mtime.
pub fn prune_old_backups(paths: &Paths) {
    let Ok(entries) = std::fs::read_dir(&paths.backups_dir) else {
        return;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    for (_, path) in dirs.into_iter().skip(KEEP_BACKUPS) {
        let _ = std::fs::remove_dir_all(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrite_prefixes_only_the_first_create() {
        assert_eq!(
            rewrite_create_for_bak("CREATE TABLE IF NOT EXISTS kv (a TEXT)"),
            "CREATE TABLE bak.IF NOT EXISTS kv (a TEXT)"
        );
        assert_eq!(
            rewrite_create_for_bak("create table \"kv\" (a TEXT)"),
            "CREATE TABLE bak.\"kv\" (a TEXT)"
        );
        assert_eq!(
            rewrite_create_for_bak("CREATE INDEX i ON t(a)"),
            "CREATE INDEX i ON t(a)"
        );
    }

    #[test]
    fn lite_backup_skips_request_details_and_copies_the_rest() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("src.sqlite")).unwrap();
        conn.execute_batch(crate::schema::PRAGMA_SQL).unwrap();
        for t in crate::schema::TABLES {
            conn.execute_batch(&t.create_sql()).unwrap();
        }
        conn.execute(
            "INSERT INTO kv(scope, key, value) VALUES('pricing','openai','{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO requestDetails(id, timestamp, data) VALUES('r1','2026-01-01T00:00:00.000Z','{}')",
            [],
        )
        .unwrap();

        let dest = dir.path().join("bak.sqlite");
        backup_db_lite(&conn, &dest).unwrap();

        let bak = rusqlite::Connection::open(&dest).unwrap();
        let kv: i64 = bak
            .query_row("SELECT COUNT(*) FROM kv", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kv, 1);
        // `BACKUP_EXCLUDE_TABLES` drops the table outright, so the backup has
        // no `requestDetails` to query at all.
        let rd: i64 = bak
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='requestDetails'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rd, 0, "requestDetails must be excluded from lite backups");
    }

    #[test]
    fn prune_keeps_three_newest() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();

        // Touch the mtime explicitly: creation order alone is not a contract,
        // and the pruner sorts on mtime.
        let base = std::time::SystemTime::now();
        for i in 0..6 {
            let d = paths.backups_dir.join(format!("b{i}"));
            std::fs::create_dir_all(&d).unwrap();
            let f = d.join("stamp");
            std::fs::write(&f, b"").unwrap();
            let t = base + std::time::Duration::from_secs(i);
            let ft = filetime::FileTime::from_system_time(t);
            filetime::set_file_mtime(&f, ft).unwrap();
            filetime::set_file_mtime(&d, ft).unwrap();
        }

        prune_old_backups(&paths);
        let remaining = std::fs::read_dir(&paths.backups_dir).unwrap().count();
        assert_eq!(remaining, KEEP_BACKUPS);
    }
}
