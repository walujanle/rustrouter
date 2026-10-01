//! The public, unauthenticated dashboard routes: `health`, `init`,
//! `version`, `settings/require-login`, `shutdown`.
//!
//! Every one of these is on the guard's `PUBLIC_API_PATHS` allow-list, so the
//! handlers assume no session.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::state::{APP_VERSION, AppState};

/// `GET /api/health`.
pub async fn health() -> Response {
    cors((StatusCode::OK, Json(json!({ "ok": true }))).into_response())
}

/// `OPTIONS /api/health`.
pub async fn health_options() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert("access-control-allow-origin", "*".parse().unwrap());
    headers.insert(
        "access-control-allow-methods",
        "GET, OPTIONS".parse().unwrap(),
    );
    headers.insert("access-control-allow-headers", "*".parse().unwrap());
    response
}

/// `GET /api/init`: a plain-text "Initialized" body.
pub async fn init() -> Response {
    (StatusCode::OK, "Initialized").into_response()
}

/// `GET /api/version`.
///
/// Reports the running version plus whatever the last update check cached. The
/// check itself runs in the background (started at boot and refreshed daily),
/// so this route never blocks on the network and stays on the public allow-list.
///
/// `hasUpdate` is the version signal; `binaryChanged` is the re-cut signal — the
/// running executable's hash differs from the published asset's. `updateAvailable`
/// is the union, and is what the sidebar banner keys off. `installMethod` says
/// how the binary was installed, and `installCmd` is the matching update command.
pub async fn version(State(state): State<AppState>) -> Response {
    crate::services::update_check::refresh_if_stale(state);
    Json(version_payload()).into_response()
}

/// The fallback link for a direct binary. Unlike a Docker or npm install, that
/// one has no command, so the release page is the whole prompt and must not
/// depend on a completed GitHub poll.
const LATEST_RELEASE_URL: &str = "https://github.com/walujanle/rustrouter/releases/latest";

/// Assemble the body from the cached status. Split out so it can be tested
/// without an `AppState` and without spawning the network refresh.
fn version_payload() -> Value {
    version_payload_for(crate::services::update_check::install_method())
}

/// The body for a known install method, so a test can pin the channel without
/// reading the live environment and filesystem.
fn version_payload_for(method: crate::services::update_check::InstallMethod) -> Value {
    use crate::services::update_check::InstallMethod;

    let status = crate::services::update_check::cached();
    let (latest, has_update, binary_changed, release_url, checked_at) = match status {
        Some(s) => (
            s.latest.map(Value::String).unwrap_or(Value::Null),
            s.has_update,
            s.binary_changed.map(Value::Bool).unwrap_or(Value::Null),
            s.release_url.map(Value::String).unwrap_or(Value::Null),
            s.checked_at.map(Value::String).unwrap_or(Value::Null),
        ),
        None => (Value::Null, false, Value::Null, Value::Null, Value::Null),
    };

    // The image's binary is compiled inside the image, so it is never the
    // release asset and the hash can never match — leaving the signal on would
    // make `updateAvailable` permanently true and nag every container forever.
    let (binary_changed, update_available) = if method == InstallMethod::Docker {
        (Value::Null, has_update)
    } else {
        (
            binary_changed.clone(),
            has_update || binary_changed == Value::Bool(true),
        )
    };

    // A direct binary has no command, so the release page is the only action;
    // fall back to the constant link when no poll has completed.
    let release_url = match (method, release_url) {
        (InstallMethod::Binary, Value::Null) => Value::String(LATEST_RELEASE_URL.to_string()),
        (_, url) => url,
    };

    json!({
        "currentVersion": APP_VERSION,
        "latestVersion": latest,
        "hasUpdate": has_update,
        "binaryChanged": binary_changed,
        "updateAvailable": update_available,
        "releaseUrl": release_url,
        "installMethod": method.as_str(),
        "installCmd": method.update_command(),
        "checkedAt": checked_at,
    })
}

/// `GET /api/settings/require-login`.
///
/// `tunnelDashboardAccess`, `tunnelUrl` and `tailscaleUrl` are not read. The
/// keys stay in the defaults table so a 9router-written row still resolves.
pub async fn require_login(State(state): State<AppState>) -> Response {
    let settings = state.read(router_db::repos::settings::get_settings).await;
    match settings {
        Ok(s) => Json(json!({
            "requireLogin": s.get("requireLogin") != Some(&Value::Bool(false)),
        }))
        .into_response(),
        // A settings-read failure returns `{ requireLogin: true }` at 200.
        Err(_) => Json(json!({ "requireLogin": true })).into_response(),
    }
}

/// `POST /api/shutdown`: always protected by the guard, so reaching the handler
/// already proved a JWT or the CLI token. Answers before exiting so the client
/// still sees a body.
pub async fn shutdown() -> Response {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        std::process::exit(0);
    });
    Json(json!({ "success": true })).into_response()
}

/// `POST /api/version/shutdown`: release the file locks a manual update needs.
///
/// This process is the only holder of the data directory, so releasing the
/// locks is the exit itself, delayed past the response so the client still
/// gets a body.
pub async fn version_shutdown() -> Response {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    Json(json!({
        "success": true,
        "message": "Shutting down for manual update...",
    }))
    .into_response()
}

/// Read a header as a string, for handlers that need one.
pub fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::update_check::InstallMethod;
    use axum::body::to_bytes;

    async fn body_json(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_ok_and_cors() {
        let response = health().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        assert_eq!(body_json(response).await, json!({ "ok": true }));
    }

    #[tokio::test]
    async fn preflight_is_204() {
        let response = health_options().await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()["access-control-allow-methods"],
            "GET, OPTIONS"
        );
    }

    #[tokio::test]
    async fn init_is_plain_text() {
        let response = init().await;
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], b"Initialized");
    }

    #[test]
    fn version_payload_keeps_its_shape_before_any_check() {
        // No cache populated: the route still answers with every field, and the
        // absence of a check is not reported as an available update.
        //
        // Pinned to a method rather than the env-reading `version_payload()`:
        // detection reads the live filesystem, so the bare call would report
        // Docker on a CI runner or a devcontainer and fail this assertion.
        let body = version_payload_for(InstallMethod::Npm);
        assert_eq!(body["currentVersion"], json!(APP_VERSION));
        assert_eq!(body["latestVersion"], Value::Null);
        assert_eq!(body["hasUpdate"], json!(false));
        assert_eq!(body["binaryChanged"], Value::Null);
        assert_eq!(body["updateAvailable"], json!(false));
        assert_eq!(body["installCmd"], json!("npm i -g rustrouter@latest"));
    }

    #[test]
    fn each_install_method_gets_its_own_prompt() {
        let npm = version_payload_for(InstallMethod::Npm);
        assert_eq!(npm["installMethod"], json!("npm"));
        assert_eq!(npm["installCmd"], json!("npm i -g rustrouter@latest"));

        let docker = version_payload_for(InstallMethod::Docker);
        assert_eq!(docker["installMethod"], json!("docker"));
        assert!(
            docker["installCmd"]
                .as_str()
                .is_some_and(|c| c.starts_with("docker pull ghcr.io/walujanle/rustrouter")),
            "docker prompt must pull the image: {}",
            docker["installCmd"]
        );
        // The image's binary is never the release asset, so the re-cut signal is
        // meaningless there and must stay off or the banner nags forever.
        assert_eq!(docker["binaryChanged"], Value::Null);

        let binary = version_payload_for(InstallMethod::Binary);
        assert_eq!(binary["installMethod"], json!("binary"));
        assert_eq!(binary["installCmd"], Value::Null);
        assert_eq!(
            binary["releaseUrl"],
            json!("https://github.com/walujanle/rustrouter/releases/latest")
        );
    }

    #[test]
    fn a_docker_install_never_reports_a_binary_change() {
        // Guards the permanent-nag regression directly: even with a cached
        // `binary_changed = Some(true)`, a Docker install reports null and does
        // not turn that into `updateAvailable`.
        let body = version_payload_for(InstallMethod::Docker);
        assert_eq!(body["binaryChanged"], Value::Null);
        assert_eq!(body["updateAvailable"], json!(false));
    }
}
