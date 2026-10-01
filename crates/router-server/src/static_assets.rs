//! The embedded dashboard: `web/dist` folded into the binary.
//!
//! A single binary has no build directory at runtime, so the Vite output is
//! embedded. Unmatched client routes fall back to `index.html`, which is what
//! keeps a hard reload on `/dashboard/keys` working.

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

/// `web/dist` relative to this crate. `AGENTS.md` states the release build
/// embeds it; `cargo build` for a dev run needs `cd web && npm run build` first.
///
/// `web/dist` is gitignored, so a fresh clone has no folder. `allow_missing`
/// keeps `cargo build`/`cargo test` working there (the release build always
/// builds the frontend first); `serve` then answers with a placeholder instead
/// of failing to compile.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Assets;

/// Serve one embedded file, or the SPA shell, or a 404.
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    if let Some(response) = file(path) {
        return response;
    }
    // An unmatched API path is a 404, not the SPA shell; only client routes
    // get the shell.
    if path.starts_with("api/") || path.starts_with("v1") {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    // A client-side route (`/dashboard`, `/login`, …) has no file of its own.
    match file("index.html") {
        Some(response) => response,
        None => missing_shell(),
    }
}

/// The dashboard is not embedded in this build: `web/dist` was absent when the
/// binary was compiled (`cargo build` without `cd web && npm run build`). The
/// routing API is unaffected, so say that rather than returning a bare 404.
fn missing_shell() -> Response {
    const PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>RustRouter</title></head><body style=\"font-family:system-ui,sans-serif;\
max-width:40rem;margin:4rem auto;padding:0 1.5rem;line-height:1.6;\">\
<h1>The dashboard is not built</h1>\
<p>This binary was compiled without <code>web/dist</code>. Build the frontend \
(<code>cd web &amp;&amp; npm run build</code>) and rebuild, or run <code>vite dev</code>.</p>\
<p>The routing API (<code>/v1/*</code>) is unaffected and still works.</p>\
</body></html>";
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PAGE,
    )
        .into_response()
}

fn file(path: &str) -> Option<Response> {
    let asset = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    Response::builder()
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control(path))
        .body(Body::from(asset.data.into_owned()))
        .ok()
}

/// The `Cache-Control` for an embedded asset.
///
/// Vite fingerprints everything it emits into `assets/` (the hash is in the
/// filename), so those are safe to cache forever: a change produces a new name
/// and a new request. The unhashed files (`favicon.svg`, `providers/*.png`,
/// `icons/`, `fonts/`) can be replaced in place across builds, so they get a
/// day rather than a year. `index.html` must never be cached — it names the
/// current asset hashes, and a stale copy pins the app to a deleted bundle.
fn cache_control(path: &str) -> &'static str {
    if path == "index.html" {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=86400"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_shell_is_embedded_when_dist_is_present() {
        // `web/dist` is gitignored, so a fresh clone builds without it. Only
        // assert the shell when the folder was actually there at build time.
        if Path::new("../../web/dist").exists() {
            assert!(Assets::get("index.html").is_some());
        } else {
            assert!(Assets::get("index.html").is_none());
        }
    }

    #[test]
    fn cache_control_separates_hashed_from_plain_assets() {
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(
            cache_control("assets/index-43rek8oV.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("favicon.svg"), "public, max-age=86400");
        assert_eq!(
            cache_control("providers/openai.png"),
            "public, max-age=86400"
        );
    }
}
