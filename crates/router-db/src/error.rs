//! Error type for the persistence layer.

use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("no database connection available after {0:?}")]
    PoolClosed(Duration),

    #[error("database is locked after {attempts} attempts")]
    Busy { attempts: u32 },

    /// `PROVIDER_NAME_CONFLICT`: an apikey connection with this name already
    /// exists and the caller passed `allowOverwrite: false`. Carries the row
    /// that would have been replaced so the route can answer `409`.
    #[error("{message}")]
    ProviderNameConflict {
        message: String,
        existing_id: String,
        existing_name: String,
    },

    #[error("{0}")]
    Invalid(String),
}

impl DbError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        DbError::Io {
            path: path.into(),
            source,
        }
    }

    /// True when the error is a transient lock conflict worth retrying.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            DbError::Sqlite(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    }
}

pub type DbResult<T> = Result<T, DbError>;
