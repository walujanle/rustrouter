//! Connection management: a small pool, the PRAGMA set, `BEGIN IMMEDIATE`
//! write transactions, busy retry, and the WAL checkpoint timer.
//!
//! A single connection behind a statement cache would serialise every reader
//! behind every writer. This pool keeps the same single-writer semantics —
//! SQLite allows one writer regardless — but lets concurrent readers proceed,
//! which is the whole point of WAL.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{DbError, DbResult};
use crate::schema::{MEMORY_PRAGMA_SQL, PRAGMA_SQL};

/// How often the WAL is folded back into the main file.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// `busy_timeout = 5000` in `PRAGMA_SQL` handles ordinary contention. These
/// retries exist for `SQLITE_BUSY_SNAPSHOT`, which the busy handler does not
/// cover: a deferred read that later upgrades to a write finds its snapshot
/// stale and must restart the whole transaction.
const BUSY_RETRIES: u32 = 6;
const BUSY_BACKOFF_MS: u64 = 25;

/// Upper bound on pooled connections. SQLite serialises writers anyway, so
/// beyond a handful the pool only buys reader parallelism. Each connection
/// carries its own page cache, so this also bounds resident memory.
///
/// Four is deliberate: the request path calls `with_conn` directly inside async
/// functions (a short synchronous statement, no `.await` under it), so a worker
/// thread parks on `checkout` only for the length of a statement, never the
/// timeout. The chat path's one blocking write is the stream-complete usage row,
/// marked at its call site.
pub const DEFAULT_POOL_SIZE: usize = 4;

/// How long `checkout` waits for a free connection before giving up. Every
/// caller's work is a single short statement or transaction, so a wait this
/// long means a slot leaked or the database is wedged; failing beats hanging
/// the request forever.
const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(30);

struct Inner {
    idle: Mutex<Vec<Connection>>,
    available: Condvar,
    /// Total connections handed out or idle, to cap growth.
    open: Mutex<usize>,
    max_size: usize,
}

/// A cloneable handle to the connection pool.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
    path: Arc<std::path::PathBuf>,
}

impl Db {
    /// Open (creating if needed) the database at `path` and apply the PRAGMA
    /// set. The directory must already exist.
    pub fn open(path: impl AsRef<Path>, max_size: usize) -> DbResult<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = open_connection(&path)?;
        Ok(Self {
            inner: Arc::new(Inner {
                idle: Mutex::new(vec![conn]),
                available: Condvar::new(),
                open: Mutex::new(1),
                max_size: max_size.max(1),
            }),
            path: Arc::new(path),
        })
    }

    /// The file backing this pool.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow a connection for the duration of `f`.
    ///
    /// The connection returns to the pool on every path, including a panic in
    /// `f`: the guard's `Drop` checks it back in. Without that, a panicking
    /// closure would drop the connection while `open` still counts it, and
    /// enough of them exhaust the pool for good. A poisoned mutex is not fatal
    /// here, so the pool guards are recovered rather than propagated.
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> DbResult<T>) -> DbResult<T> {
        let conn = self.checkout()?;
        let guard = ConnGuard {
            db: self,
            conn: Some(conn),
        };
        f(guard.conn())
    }

    /// Run `f` inside a `BEGIN IMMEDIATE` transaction, committing on `Ok` and
    /// rolling back on `Err`.
    ///
    /// `IMMEDIATE` takes the write lock up front. A deferred transaction that
    /// reads first and writes later can fail with `SQLITE_BUSY_SNAPSHOT` under
    /// WAL, so writes always take the lock up front.
    ///
    /// `f` is `Fn`, not `FnOnce`: a retry after a transient lock conflict runs
    /// it a second time, so it must not consume captured state.
    pub fn write<T>(&self, f: impl Fn(&rusqlite::Transaction<'_>) -> DbResult<T>) -> DbResult<T> {
        self.retry_busy(|| {
            let conn = self.checkout()?;
            // Hold the connection in a guard so a panic in `f` still checks it
            // back in rather than leaking a pool slot.
            let mut guard = ConnGuard {
                db: self,
                conn: Some(conn),
            };
            let conn = guard.conn_mut();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
    }

    /// Retry `f` while it reports a transient lock conflict.
    ///
    /// Safe to retry only because every caller's unit of work is a single
    /// transaction: a partially applied attempt cannot survive a rollback.
    pub fn retry_busy<T>(&self, mut f: impl FnMut() -> DbResult<T>) -> DbResult<T> {
        let mut attempt = 0u32;
        loop {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) if e.is_busy() && attempt + 1 < BUSY_RETRIES => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(BUSY_BACKOFF_MS * u64::from(attempt)));
                }
                Err(e) if e.is_busy() => {
                    return Err(DbError::Busy {
                        attempts: attempt + 1,
                    });
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)`: fold the WAL back into the main file
    /// and shrink it. Without a periodic checkpoint a busy gateway leaves a
    /// multi-gigabyte `-wal` beside the database.
    pub fn checkpoint(&self) -> DbResult<()> {
        self.with_conn(|conn| {
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
            Ok(())
        })
    }

    /// Spawn the periodic checkpoint on the current tokio runtime.
    pub fn spawn_checkpoint_task(&self) {
        let db = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(CHECKPOINT_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick fires immediately; skip it so startup does not
            // block on a checkpoint nobody asked for.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let db = db.clone();
                // Blocking SQLite work must not stall the runtime.
                let _ = tokio::task::spawn_blocking(move || {
                    db.checkpoint()?;
                    // Release the idle connections' page caches after the
                    // checkpoint. A no-op while requests are in flight, so a
                    // burst is never interrupted.
                    db.shrink_idle();
                    Ok::<(), crate::error::DbError>(())
                })
                .await;
            }
        });
    }

    /// Drop all but one idle connection and ask the survivor to release its
    /// page cache.
    ///
    /// `PRAGMA shrink_memory` frees the cache into the process's C heap; it
    /// hands nothing back to the OS on its own, so this pairs with the per-OS
    /// trim in `router_server::reclaim` that runs once the requests go idle.
    ///
    /// A no-op while any connection is checked out, so it can never shrink the
    /// pool under a live request: `open` counts both idle and checked-out
    /// connections, so `open == idle.len()` means nothing is in flight. Callers
    /// are the periodic tickers, not the request path.
    pub fn shrink_idle(&self) {
        // Take the survivors out of the pool first: the SQLite call below must
        // not run while the pool lock is held, or a concurrent `checkout` would
        // block behind it. Releasing the lock makes the window where `open`
        // overcounts momentarily, which is harmless.
        let survivors: Vec<Connection> = {
            let mut idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
            let mut open = self.inner.open.lock().unwrap_or_else(|e| e.into_inner());
            if *open != idle.len() {
                return;
            }
            let keep = idle.pop();
            let dropped = idle.len();
            idle.clear();
            *open = open.saturating_sub(dropped);
            keep.into_iter().collect()
        };
        for conn in survivors {
            // `shrink_memory` frees the connection's heap cache into the C
            // heap. It does not change `cache_size`, so the cap still applies to
            // the next read; the OS reclaim is `router_server::reclaim`'s job.
            let _ = conn.execute_batch("PRAGMA shrink_memory");
            self.checkin(conn);
        }
    }

    /// Close every idle connection. In-flight connections close on return.
    pub fn close(&self) {
        if let Ok(mut idle) = self.inner.idle.lock() {
            let n = idle.len();
            idle.clear();
            if let Ok(mut open) = self.inner.open.lock() {
                *open = open.saturating_sub(n);
            }
        }
    }

    fn checkout(&self) -> DbResult<Connection> {
        let mut idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(conn) = idle.pop() {
                return Ok(conn);
            }
            let mut open = self.inner.open.lock().unwrap_or_else(|e| e.into_inner());
            if *open < self.inner.max_size {
                *open += 1;
                drop(open);
                drop(idle);
                match open_connection(&self.path) {
                    Ok(conn) => return Ok(conn),
                    Err(e) => {
                        // Re-take `idle` before the notify. A waiter checks
                        // `open < max_size` while holding `idle`, then parks on
                        // the same mutex, so notifying without it can be lost in
                        // that window and the waiter would sleep out the full
                        // 30s timeout before returning a spurious `PoolClosed`.
                        // The lock order matches `checkout`: `idle` then `open`.
                        let idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
                        let mut open = self.inner.open.lock().unwrap_or_else(|e| e.into_inner());
                        *open = open.saturating_sub(1);
                        drop(open);
                        self.inner.available.notify_one();
                        drop(idle);
                        return Err(e);
                    }
                }
            }
            drop(open);
            let (next, timeout) = self
                .inner
                .available
                .wait_timeout(idle, CHECKOUT_TIMEOUT)
                .unwrap_or_else(|e| e.into_inner());
            idle = next;
            if timeout.timed_out() {
                return Err(DbError::PoolClosed(CHECKOUT_TIMEOUT));
            }
        }
    }

    fn checkin(&self, conn: Connection) {
        let mut idle = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner());
        idle.push(conn);
        self.inner.available.notify_one();
    }
}

/// Returns a checked-out connection to its pool on drop, panic included.
struct ConnGuard<'a> {
    db: &'a Db,
    conn: Option<Connection>,
}

impl ConnGuard<'_> {
    fn conn(&self) -> &Connection {
        self.conn.as_ref().expect("guard holds a connection")
    }

    fn conn_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("guard holds a connection")
    }
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            self.db.checkin(conn);
        }
    }
}

fn open_connection(path: &Path) -> DbResult<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(PRAGMA_SQL)?;
    // The memory overlay runs second so it cannot disturb the shared-file set.
    conn.execute_batch(MEMORY_PRAGMA_SQL)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.sqlite"), 4).unwrap();
        (dir, db)
    }

    #[test]
    fn pragmas_apply() {
        let (_d, db) = temp_db();
        db.with_conn(|c| {
            let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert_eq!(mode, "wal");
            let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert_eq!(fk, 1);
            let busy: i64 = c.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            assert_eq!(busy, 5000);
            // The memory overlay: connection-local, never in the file header.
            let cache: i64 = c.query_row("PRAGMA cache_size", [], |r| r.get(0))?;
            assert_eq!(cache, -2000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn write_rolls_back_on_error() {
        let (_d, db) = temp_db();
        db.with_conn(|c| {
            c.execute_batch("CREATE TABLE t (a INTEGER)")?;
            Ok(())
        })
        .unwrap();

        let _ = db.write(|tx| -> DbResult<()> {
            tx.execute("INSERT INTO t (a) VALUES (1)", [])?;
            Err(DbError::Invalid("boom".into()))
        });

        let n: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn write_commits_on_success() {
        let (_d, db) = temp_db();
        db.with_conn(|c| {
            c.execute_batch("CREATE TABLE t (a INTEGER)")?;
            Ok(())
        })
        .unwrap();
        db.write(|tx| {
            tx.execute("INSERT INTO t (a) VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();
        let n: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn pool_reuses_connections() {
        let (_d, db) = temp_db();
        let before = *db.inner.open.lock().unwrap();
        for _ in 0..50 {
            db.with_conn(|c| Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?))
                .unwrap();
        }
        let after = *db.inner.open.lock().unwrap();
        assert_eq!(before, after, "pool grew instead of reusing");
        assert!(after <= DEFAULT_POOL_SIZE);
    }

    #[test]
    fn shrink_idle_keeps_one_connection_and_never_runs_while_one_is_out() {
        let (_d, db) = temp_db();
        // Force the pool to grow past one connection.
        let held = db.checkout().unwrap();
        let second = db.checkout().unwrap();
        db.checkin(second);
        assert_eq!(db.inner.idle.lock().unwrap().len(), 1);

        // One connection is checked out, so shrink_idle must not touch the pool.
        db.shrink_idle();
        assert_eq!(db.inner.idle.lock().unwrap().len(), 1);

        db.checkin(held);
        assert_eq!(db.inner.idle.lock().unwrap().len(), 2);
        db.shrink_idle();
        assert_eq!(db.inner.idle.lock().unwrap().len(), 1);
        assert_eq!(*db.inner.open.lock().unwrap(), 1);

        // The survivor is still usable.
        let n: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn a_panicking_closure_returns_its_connection() {
        let dir = tempfile::TempDir::new().unwrap();
        let db = Db::open(dir.path().join("t.sqlite"), 2).unwrap();
        for _ in 0..4 {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = db.with_conn(|_| -> DbResult<()> { panic!("boom") });
            }));
            assert!(caught.is_err());
        }
        // Four panics against a two-connection pool: without the guard the
        // pool would be exhausted and this call would block on the timeout.
        let n: i64 = db
            .with_conn(|c| Ok(c.query_row("SELECT 1", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }
}
