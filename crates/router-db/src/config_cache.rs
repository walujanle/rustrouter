//! A process-wide snapshot of the read-mostly configuration tables.
//!
//! The chat hot path reads settings, combos, model aliases and provider nodes
//! several times per request: settings for `requireApiKey`, the combo strategy
//! and the token-saver flags; combos for model resolution; aliases and nodes for
//! `getModelInfo`. Each read is a `SELECT` plus a full JSON parse, and the data
//! only changes when the dashboard writes it.
//!
//! This cache turns that into one `Arc` clone per request. It is invalidated
//! explicitly by every writer (settings, combos, aliases, nodes) and lazily by a
//! short TTL, so a write from the *other* application sharing the SQLite file is
//! still picked up without a restart.
//!
//! The snapshot is a plain owned struct. Callers clone the `Arc` and borrow the
//! fields, so a hit allocates nothing beyond the refcount.

use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::error::DbResult;
use crate::repos::{aliases, combos, nodes, settings};

/// How long a snapshot may serve reads before it is re-validated against the
/// database.
///
/// The explicit invalidations cover this process's own writes, which are the
/// common case. The TTL covers the shared-file case: 9router (or a second
/// rustrouter) can change a row without this process knowing. Two seconds keeps
/// a cross-process change prompt while still collapsing a burst of requests
/// into a single read.
const TTL: Duration = Duration::from_secs(2);

/// The cached configuration a chat request needs.
#[derive(Debug, Clone, Default)]
pub struct ConfigSnapshot {
    /// `settings` with defaults merged, in declaration order.
    pub settings: Value,
    /// Every combo row, `createdAt ASC`.
    pub combos: Vec<Value>,
    /// The `modelAliases` kv scope as a map.
    pub aliases: Map<String, Value>,
    /// Every `providerNodes` row.
    pub nodes: Vec<Value>,
    /// The `disabledModels` kv scope, used by `/v1/models`.
    pub disabled_models: Value,
}

struct Cached {
    /// The database this snapshot was read from.
    key: usize,
    snapshot: Arc<ConfigSnapshot>,
    loaded_at: Instant,
}

static CACHE: LazyLock<RwLock<Option<Cached>>> = LazyLock::new(|| RwLock::new(None));

/// The database a snapshot belongs to.
///
/// One process normally serves one database, but tests and embedded callers can
/// open several. The key is the connection's address: a pooled connection keeps
/// the same address while it lives, and two databases always have different
/// connections. A miss only costs a reload.
fn cache_key(conn: &Connection) -> usize {
    conn as *const Connection as usize
}

/// Drop whatever snapshot is cached, whichever database it came from.
///
/// The update hook cannot see a connection, so it calls this. One process
/// normally serves one database, so clearing unconditionally is the same thing
/// there; with several databases it only costs a reload.
pub fn invalidate_all() {
    if let Ok(mut slot) = CACHE.write() {
        *slot = None;
    }
}

fn load(conn: &Connection) -> DbResult<ConfigSnapshot> {
    let settings = settings::get_settings(conn)?;
    let combos = combos::get_combos(conn)?;
    let aliases = aliases::get_model_aliases(conn)?;
    let nodes = nodes::get_provider_nodes(conn, None)?;
    let disabled_models = aliases::get_disabled_models(conn)?;
    Ok(ConfigSnapshot {
        settings,
        combos,
        aliases: aliases.as_object().cloned().unwrap_or_default(),
        nodes,
        disabled_models,
    })
}

/// The current snapshot, loading it on first use and when the TTL expires.
///
/// A write in this process drops the entry through the SQLite update hook, so
/// the request that follows a dashboard edit reloads rather than serving the
/// old rows. The TTL covers the other direction: a write by the application
/// sharing the database file, which this process never sees a hook for.
pub fn snapshot(conn: &Connection) -> DbResult<Arc<ConfigSnapshot>> {
    let key = cache_key(conn);
    if let Ok(slot) = CACHE.read()
        && let Some(cached) = slot.as_ref()
        && cached.key == key
        && cached.loaded_at.elapsed() < TTL
    {
        return Ok(Arc::clone(&cached.snapshot));
    }
    let snapshot = Arc::new(load(conn)?);
    if let Ok(mut slot) = CACHE.write() {
        *slot = Some(Cached {
            key,
            snapshot: Arc::clone(&snapshot),
            loaded_at: Instant::now(),
        });
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for table in ["settings", "combos", "kv", "providerNodes"] {
            conn.execute_batch(&schema::create_table_sql(table).unwrap())
                .unwrap();
        }
        conn
    }

    /// The cache must be process-global, so these tests take turns.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_snapshot_is_reused_until_it_is_invalidated() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        invalidate_all();
        let conn = db();
        settings::update_settings(&conn, serde_json::json!({"requireApiKey": true})).unwrap();
        let first = snapshot(&conn).unwrap();
        assert_eq!(first.settings["requireApiKey"], serde_json::json!(true));

        // A write is invisible until the hook drops the entry, which is the
        // point: readers are not paying for a query per request.
        settings::update_settings(&conn, serde_json::json!({"requireApiKey": false})).unwrap();
        let second = snapshot(&conn).unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "a fresh snapshot must be reused"
        );
        assert_eq!(second.settings["requireApiKey"], serde_json::json!(true));

        invalidate_all();
        let third = snapshot(&conn).unwrap();
        assert_eq!(third.settings["requireApiKey"], serde_json::json!(false));
        invalidate_all();
    }

    /// The hook `driver::open_connection` installs is what makes a dashboard
    /// write visible on the next request, so it is exercised here rather than
    /// only through the route tests.
    #[test]
    fn the_write_hook_drops_the_snapshot() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        invalidate_all();
        let conn = db();
        crate::driver::install_config_hook(&conn).unwrap();
        settings::update_settings(&conn, serde_json::json!({"requireApiKey": true})).unwrap();
        assert_eq!(
            snapshot(&conn).unwrap().settings["requireApiKey"],
            serde_json::json!(true)
        );
        settings::update_settings(&conn, serde_json::json!({"requireApiKey": false})).unwrap();
        assert_eq!(
            snapshot(&conn).unwrap().settings["requireApiKey"],
            serde_json::json!(false),
            "the update hook must drop the cached snapshot"
        );
        invalidate_all();
    }
}
