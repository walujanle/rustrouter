//! Shared process state and the env-contract pieces that have to be resolved
//! once at boot.
//!
//! `PROTECTED_API_PATHS` and friends live in `auth::guard`; this module owns
//! what the handlers need: the DB pool, the resolved paths, the JWT signer, the
//! login limiter, and the CLI-token cache.

use std::sync::{Arc, OnceLock};

use router_db::{Db, Paths};

use crate::auth::login_limiter::Limiter;
use crate::auth::session::Session;

/// The app version the `/api/version` route reports.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Cheap to clone; every field is an `Arc` or a handle.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub paths: Arc<Paths>,
    pub session: Arc<Session>,
    pub limiter: Arc<Limiter>,
    /// `getCliToken()`'s module-level cache. `std::sync::OnceLock` rather than
    /// `tokio`'s, because the guard reads it from a sync context.
    cli_token: Arc<OnceLock<String>>,
}

impl AppState {
    pub fn new(db: Db, paths: Paths) -> router_db::DbResult<Self> {
        let session = Session::load(&paths)?;
        Ok(Self {
            db,
            paths: Arc::new(paths),
            session: Arc::new(session),
            limiter: Arc::new(Limiter::new()),
            cli_token: Arc::new(OnceLock::new()),
        })
    }

    /// `getCliToken()`: the machine id under `9r-cli-auth`, computed once.
    ///
    /// Cached for the process lifetime, so a change to the `cli-secret` file
    /// needs a restart.
    pub fn cli_token(&self) -> &str {
        self.cli_token.get_or_init(|| {
            router_db::identity::consistent_machine_id(
                &self.paths,
                Some(crate::auth::guard::CLI_TOKEN_SALT),
            )
            .unwrap_or_default()
        })
    }

    /// `hasValidCliToken(request)`: constant-time compare so the CLI token
    /// cannot be recovered a byte at a time.
    pub fn has_valid_cli_token(&self, token: Option<&str>) -> bool {
        let Some(token) = token else {
            return false;
        };
        let expected = self.cli_token();
        if expected.is_empty() {
            return false;
        }
        use subtle::ConstantTimeEq;
        expected.as_bytes().ct_eq(token.as_bytes()).into()
    }

    /// `settings.requireLogin !== false`, defaulting to `true` when the read
    /// fails.
    pub fn require_login(&self) -> bool {
        self.db
            .with_conn(router_db::repos::settings::get_settings)
            .map(|s| s.get("requireLogin") != Some(&serde_json::Value::Bool(false)))
            .unwrap_or(true)
    }

    /// Run a read on the blocking pool.
    ///
    /// rusqlite is synchronous. Handlers run on the async runtime, so a direct
    /// call parks a worker thread on disk I/O. The pool is what makes the
    /// handoff cheap: the connection is already open, so this is a `SELECT` on
    /// an idle connection, not a connect.
    pub async fn read<T, F>(&self, f: F) -> router_db::DbResult<T>
    where
        F: FnOnce(&router_db::rusqlite::Connection) -> router_db::DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.with_conn(f))
            .await
            .unwrap_or_else(|_| Err(router_db::DbError::Invalid("db task panicked".into())))
    }

    /// Run a `BEGIN IMMEDIATE` write on the blocking pool.
    pub async fn write<T, F>(&self, f: F) -> router_db::DbResult<T>
    where
        F: Fn(&router_db::rusqlite::Transaction<'_>) -> router_db::DbResult<T> + Send + 'static,
        T: Send + 'static,
    {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.write(f))
            .await
            .unwrap_or_else(|_| Err(router_db::DbError::Invalid("db task panicked".into())))
    }
}

/// `process.env.PORT || 20129`.
pub fn resolve_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20129)
}

/// `process.env.HOSTNAME || "0.0.0.0"`.
pub fn resolve_host() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_defaults_to_20129() {
        // The env var is process-global, so this only asserts the default path
        // when the test runner has not set PORT.
        if std::env::var("PORT").is_err() {
            assert_eq!(resolve_port(), 20129);
        }
    }

    #[test]
    fn host_defaults_to_all_interfaces() {
        if std::env::var("HOSTNAME").is_err() {
            assert_eq!(resolve_host(), "0.0.0.0");
        }
    }
}
