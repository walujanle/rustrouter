//! Data-directory resolution.
//!
//! The same SQLite file is shared with 9router, so a path that differs by
//! even one component means two applications writing two different databases.

use std::path::PathBuf;

pub const APP_NAME: &str = "9router";

/// `getDataDir()`.
///
/// - `DATA_DIR` set and usable wins.
/// - On Windows a Unix-style absolute path (`/…`) is rejected with a warning
///   and falls back, because a Linux-targeted `.env` is not valid there.
/// - `EACCES`/`EPERM` on create falls back too.
/// - Otherwise `%APPDATA%/9router` on Windows, `~/.9router` elsewhere.
pub fn resolve_data_dir() -> PathBuf {
    let configured = std::env::var("DATA_DIR").ok().filter(|s| !s.is_empty());
    let Some(configured) = configured else {
        return default_dir();
    };

    #[cfg(windows)]
    if configured.starts_with('/') {
        tracing::warn!("[DATA_DIR] '{configured}' is a Unix path on Windows, fallback to default");
        return default_dir();
    }

    let path = PathBuf::from(&configured);
    match std::fs::create_dir_all(&path) {
        Ok(()) => path,
        Err(e) if is_permission_denied(&e) => {
            tracing::warn!("[DATA_DIR] '{configured}' not writable, fallback to default");
            default_dir()
        }
        Err(_) => path,
    }
}

fn is_permission_denied(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::PermissionDenied)
}

fn default_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA")
            && !appdata.is_empty()
        {
            return std::path::Path::new(&appdata).join(APP_NAME);
        }
        if let Some(home) = dirs::home_dir() {
            return home.join("AppData").join("Roaming").join(APP_NAME);
        }
    }
    match dirs::home_dir() {
        Some(home) => home.join(format!(".{APP_NAME}")),
        // No home directory at all is unrecoverable; the relative fallback at
        // least keeps the process running with a writable cwd.
        None => PathBuf::from(format!(".{APP_NAME}")),
    }
}

/// Resolved filesystem layout. Built once at startup and shared.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub db_dir: PathBuf,
    pub data_file: PathBuf,
    pub backups_dir: PathBuf,
    pub legacy_main: PathBuf,
    pub legacy_usage: PathBuf,
    pub legacy_disabled: PathBuf,
    pub legacy_details: PathBuf,
    /// `DB_DIR/.migrated-from-json`
    pub migrated_marker: PathBuf,
    /// `DATA_DIR/machine-id`
    pub machine_id_file: PathBuf,
    /// `DATA_DIR/auth/cli-secret`
    pub cli_secret_file: PathBuf,
    /// `DATA_DIR/jwt-secret`
    pub jwt_secret_file: PathBuf,
}

impl Paths {
    pub fn new(data_dir: PathBuf) -> Self {
        let db_dir = data_dir.join("db");
        Self {
            data_file: db_dir.join("data.sqlite"),
            backups_dir: db_dir.join("backups"),
            legacy_main: data_dir.join("db.json"),
            legacy_usage: data_dir.join("usage.json"),
            legacy_disabled: data_dir.join("disabledModels.json"),
            legacy_details: data_dir.join("request-details.json"),
            migrated_marker: db_dir.join(".migrated-from-json"),
            machine_id_file: data_dir.join("machine-id"),
            cli_secret_file: data_dir.join("auth").join("cli-secret"),
            jwt_secret_file: data_dir.join("jwt-secret"),
            db_dir,
            data_dir,
        }
    }

    /// Resolve from the environment.
    pub fn from_env() -> Self {
        Self::new(resolve_data_dir())
    }

    /// `ensureDirs()`.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.db_dir)?;
        std::fs::create_dir_all(&self.backups_dir)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_reference_join_order() {
        let p = Paths::new(PathBuf::from("/data"));
        assert_eq!(p.db_dir, PathBuf::from("/data/db"));
        assert_eq!(p.data_file, PathBuf::from("/data/db/data.sqlite"));
        assert_eq!(p.backups_dir, PathBuf::from("/data/db/backups"));
        assert_eq!(p.legacy_main, PathBuf::from("/data/db.json"));
        assert_eq!(p.legacy_usage, PathBuf::from("/data/usage.json"));
        assert_eq!(
            p.legacy_disabled,
            PathBuf::from("/data/disabledModels.json")
        );
        assert_eq!(
            p.legacy_details,
            PathBuf::from("/data/request-details.json")
        );
        assert_eq!(
            p.migrated_marker,
            PathBuf::from("/data/db/.migrated-from-json")
        );
        assert_eq!(p.machine_id_file, PathBuf::from("/data/machine-id"));
        assert_eq!(p.cli_secret_file, PathBuf::from("/data/auth/cli-secret"));
        assert_eq!(p.jwt_secret_file, PathBuf::from("/data/jwt-secret"));
    }
}
