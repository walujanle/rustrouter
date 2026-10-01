//! Update checking against the GitHub Releases of `walujanle/rustrouter`.
//!
//! Two signals, because a version alone misses a re-cut release:
//!
//! 1. the newest release tag compared to the running version, and
//! 2. the sha256 of the running executable compared to the Release asset's
//!    `digest`, which catches a release rebuilt under the same version.
//!
//! `GET /api/version` reads the cache and never blocks on the network; the
//! scheduler fills it. See `docs/RUNTIME.md`.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use router_sse::executors::http::tls_builder;

use crate::state::{APP_VERSION, AppState};

const RELEASES_URL: &str = "https://api.github.com/repos/walujanle/rustrouter/releases/latest";
const USER_AGENT: &str = "rustrouter-update-check";
/// One check a day is the ask; a manual `GET /api/version` refreshes sooner when
/// the cache is this old.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const STALE_AFTER_MS: i64 = 6 * 60 * 60 * 1000;
const STARTUP_DELAY: Duration = Duration::from_secs(60);

/// What the version route reports. Absent fields are `null` in the JSON.
#[derive(Clone, Debug, Default)]
pub struct UpdateStatus {
    pub latest: Option<String>,
    pub has_update: bool,
    /// `Some(false)` means the running binary's hash differs from the published
    /// one; `None` means it could not be determined (no asset, no exe).
    pub binary_changed: Option<bool>,
    pub release_url: Option<String>,
    pub checked_at: Option<String>,
}

static CACHE: LazyLock<RwLock<Option<UpdateStatus>>> = LazyLock::new(|| RwLock::new(None));
static RUNNING: AtomicBool = AtomicBool::new(false);
static LAST_ATTEMPT_MS: AtomicI64 = AtomicI64::new(0);
/// The loop task, so `configure(false)` aborts it rather than leaving it parked
/// in a sleep that a later `start` would race with a second loop.
static TASK: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> = std::sync::Mutex::new(None);

/// The cached status, or `None` before the first check completes.
pub fn cached() -> Option<UpdateStatus> {
    CACHE.read().ok().and_then(|c| c.clone())
}

/// Kick off a check in the background when the cache is stale. Never blocks the
/// caller, so the public version route stays cheap.
///
/// The attempt timestamp is claimed here, before the spawn, so a burst of
/// concurrent `/api/version` requests fires one check rather than one per
/// request: the second caller reads the timestamp this call just stored.
pub fn refresh_if_stale(state: AppState) {
    let now = router_db::time::now_ms();
    let last = LAST_ATTEMPT_MS.load(Ordering::SeqCst);
    if last != 0 && now - last < STALE_AFTER_MS {
        return;
    }
    if LAST_ATTEMPT_MS
        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    tokio::spawn(async move {
        check_once(&state).await;
    });
}

/// Start the daily loop while `autoUpdateCheck` is on, stop it otherwise.
pub fn configure(state: AppState, settings: &Value) {
    let enabled = settings
        .get("autoUpdateCheck")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if enabled {
        start(state);
    } else {
        RUNNING.store(false, Ordering::SeqCst);
        if let Ok(mut slot) = TASK.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }
    }
}

fn start(state: AppState) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = tokio::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;
        loop {
            if !RUNNING.load(Ordering::SeqCst) {
                break;
            }
            check_once(&state).await;
            tokio::time::sleep(CHECK_INTERVAL).await;
        }
    });
    if let Ok(mut slot) = TASK.lock() {
        *slot = Some(handle);
    }
}

/// One poll. Failures keep the previous cache and only log, so a rate limit or
/// an offline machine never clears a known-good status.
async fn check_once(_state: &AppState) {
    LAST_ATTEMPT_MS.store(router_db::time::now_ms(), Ordering::SeqCst);
    match fetch_latest().await {
        Ok(status) => {
            if let Ok(mut cache) = CACHE.write() {
                *cache = Some(status);
            }
        }
        Err(error) => tracing::warn!("[UpdateCheck] {error}"),
    }
}

async fn fetch_latest() -> Result<UpdateStatus, String> {
    let client = tls_builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(RELEASES_URL)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("release lookup failed ({})", response.status()));
    }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;

    let latest = body
        .get("tag_name")
        .and_then(Value::as_str)
        .map(|tag| tag.trim_start_matches('v').to_string())
        .filter(|v| !v.is_empty());
    let release_url = body
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string);

    let has_update = latest.as_deref().is_some_and(|l| is_newer(l, APP_VERSION));

    Ok(UpdateStatus {
        binary_changed: binary_changed(&body),
        latest,
        has_update,
        release_url,
        checked_at: Some(router_db::time::now_iso()),
    })
}

/// Compare the running executable's sha256 to the digest of the asset for this
/// platform. `None` when the platform asset, the digest, or the executable is
/// unavailable — an absent signal is not a "changed" signal.
fn binary_changed(body: &Value) -> Option<bool> {
    let base = asset_prefix()?;
    let assets = body.get("assets")?.as_array()?;
    let asset = assets.iter().find(|a| {
        a.get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.starts_with(&format!("{base}-")))
    })?;
    let expected = asset
        .get("digest")
        .and_then(Value::as_str)
        .and_then(|d| d.strip_prefix("sha256:"))?;
    let actual = running_sha256()?;
    Some(!actual.eq_ignore_ascii_case(expected))
}

/// The asset name prefix for the platform this binary is running on, matching
/// the CI matrix. `None` on a platform with no published asset.
fn asset_prefix() -> Option<&'static str> {
    // Termux reports `linux`/`aarch64` to the process, but the binary it runs is
    // the android build, so the android asset is the one to compare against.
    if is_android() {
        return Some("rustrouter-linux-android-aarch64");
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("rustrouter-linux-amd64"),
        ("linux", "aarch64") => Some("rustrouter-linux-arm64"),
        ("macos", "aarch64") => Some("rustrouter-macos-arm64"),
        ("windows", "x86_64") => Some("rustrouter-windows-x64"),
        ("windows", "aarch64") => Some("rustrouter-windows-arm64"),
        _ => None,
    }
}

fn is_android() -> bool {
    std::env::var("PREFIX")
        .map(|p| p.contains("com.termux"))
        .unwrap_or(false)
        || std::path::Path::new("/system/bin/linker64").exists()
}

/// Where the running binary came from, so the update prompt can hand the user
/// the command that actually updates *this* install instead of always assuming
/// npm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallMethod {
    Docker,
    Npm,
    Binary,
}

impl InstallMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            InstallMethod::Docker => "docker",
            InstallMethod::Npm => "npm",
            InstallMethod::Binary => "binary",
        }
    }

    /// The one shell command that moves this channel to the newest release, or
    /// `None` for a direct binary — that one is replaced from the release page,
    /// whose URL the payload carries as `releaseUrl`.
    ///
    /// The Docker command is DOCKER.md's Update section: pull, then remove the
    /// container. Recreating it is left to the user because the run line (or the
    /// compose file) is theirs, and the image tag cannot be read from inside the
    /// container — `HOSTNAME` is pinned to `0.0.0.0`, so the usual container-id
    /// trick is unavailable.
    pub fn update_command(self) -> Option<&'static str> {
        match self {
            InstallMethod::Docker => {
                Some("docker pull ghcr.io/walujanle/rustrouter:latest && docker rm -f rustrouter")
            }
            InstallMethod::Npm => Some("npm i -g rustrouter@latest"),
            InstallMethod::Binary => None,
        }
    }

    /// Whether the update can be applied by stopping this process and replacing
    /// the binary. False inside a container: it cannot pull its own replacement,
    /// and a `restart: always` policy would bring the old container straight
    /// back, so offering a shutdown button there is wrong.
    pub fn can_shutdown(self) -> bool {
        !matches!(self, InstallMethod::Docker)
    }
}

/// The marker our own npm shim and image set, so the common cases need no
/// heuristic. See `npm/bin/rustrouter.js` and the Dockerfile runtime stage.
const METHOD_ENV: &str = "RUSTROUTER_INSTALL_METHOD";

/// Cached: the environment and the filesystem do not change under a running
/// process, and `/api/version` is public and polled.
static INSTALL_METHOD: LazyLock<InstallMethod> = LazyLock::new(detect_install_method);

pub fn install_method() -> InstallMethod {
    *INSTALL_METHOD
}

/// First match wins:
///
/// 1. the `RUSTROUTER_INSTALL_METHOD` marker, set by our own shim and image;
/// 2. a `node_modules` component in the executable path, which is positive
///    evidence the binary arrived through a package manager;
/// 3. container markers — this only says *where* it runs, so it sits below the
///    install-channel signals;
/// 4. otherwise a direct binary.
fn detect_install_method() -> InstallMethod {
    if let Ok(v) = std::env::var(METHOD_ENV)
        && let Some(m) = parse_method(&v)
    {
        return m;
    }
    if path_is_in_node_modules() {
        return InstallMethod::Npm;
    }
    if is_container() {
        return InstallMethod::Docker;
    }
    InstallMethod::Binary
}

fn parse_method(raw: &str) -> Option<InstallMethod> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "docker" => Some(InstallMethod::Docker),
        "npm" | "node" | "nodejs" => Some(InstallMethod::Npm),
        "binary" | "direct" => Some(InstallMethod::Binary),
        _ => None,
    }
}

/// The executable sits under a `node_modules` directory. A path-component match,
/// not a substring, so `/srv/node_modules_backup/rustrouter` does not count. The
/// shim spawns the resolved real path, so this sees the package tree, not npm's
/// own symlink.
fn path_is_in_node_modules() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    exe.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("node_modules"))
}

/// PID 1 is the strongest signal on Linux: the image's entrypoint `exec`s
/// `gosu rustrouter`, so the server really is PID 1, and it covers
/// Kubernetes/containerd where no marker file exists. A host always has init
/// there, and a devcontainer does not run rustrouter as PID 1, so this does not
/// misfire on a source build.
///
/// The marker files need the image's own binary path as well, because
/// `/.dockerenv` exists in every Docker container — without the guard, a cargo
/// build inside a devcontainer would be told to `docker pull`.
fn is_container() -> bool {
    if std::process::id() == 1 {
        return true;
    }
    let image_binary = std::env::current_exe()
        .ok()
        .is_some_and(|p| p == std::path::Path::new("/usr/local/bin/rustrouter"));
    image_binary
        && (std::path::Path::new("/.dockerenv").exists()
            || std::path::Path::new("/run/.containerenv").exists())
}

fn running_sha256() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let bytes = std::fs::read(exe).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// `x.y.z` tuple comparison. A non-numeric component sorts as 0.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    parse_version(candidate) > parse_version(current)
}

fn parse_version(v: &str) -> (u64, u64, u64) {
    let mut parts = v.trim().split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_compare_is_numeric_not_lexical() {
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
    }

    #[test]
    fn a_missing_component_sorts_as_zero() {
        assert!(is_newer("0.3", "0.2.9"));
        assert_eq!(parse_version("1"), (1, 0, 0));
        assert_eq!(parse_version("junk"), (0, 0, 0));
    }

    #[test]
    fn method_marker_parses_case_insensitively() {
        assert_eq!(parse_method("Docker"), Some(InstallMethod::Docker));
        assert_eq!(parse_method("  NPM  "), Some(InstallMethod::Npm));
        assert_eq!(parse_method("node"), Some(InstallMethod::Npm));
        assert_eq!(parse_method("Binary"), Some(InstallMethod::Binary));
        assert_eq!(parse_method(""), None);
        assert_eq!(parse_method("apt"), None);
    }

    #[test]
    fn only_docker_refuses_the_shutdown_flow() {
        // A container cannot pull its own replacement, and `restart: always`
        // would bring the old one back, so it must not offer a shutdown.
        assert!(!InstallMethod::Docker.can_shutdown());
        assert!(InstallMethod::Npm.can_shutdown());
        assert!(InstallMethod::Binary.can_shutdown());
    }

    #[test]
    fn the_binary_method_has_no_command() {
        assert_eq!(InstallMethod::Binary.update_command(), None);
        assert_eq!(
            InstallMethod::Npm.update_command(),
            Some("npm i -g rustrouter@latest")
        );
        assert!(
            InstallMethod::Docker
                .update_command()
                .is_some_and(|c| c.starts_with("docker pull ghcr.io/walujanle/rustrouter"))
        );
    }

    #[test]
    fn node_modules_is_a_component_match_not_a_substring() {
        // `/srv/node_modules_backup/rustrouter` must not read as an npm install.
        let real = std::env::current_exe().expect("the test binary has a path");
        let dir = real.parent().expect("has a parent");
        let decoy = dir.join("node_modules_backup");
        assert!(
            !decoy
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case("node_modules")),
            "a `node_modules_*` sibling must not match"
        );
        assert!(
            decoy
                .join("node_modules")
                .join("x")
                .components()
                .any(|c| c.as_os_str().eq_ignore_ascii_case("node_modules")),
            "a real `node_modules` component must match"
        );
    }
}
