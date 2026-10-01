//! Probe a proxy URL with one HEAD request.
//!
//! Used by `settings/proxy-test` and by `proxy-pools/[id]/test` for `http`
//! pools. A request goes out through the proxy against a test URL, returning a
//! `{ok,status,statusText,url,elapsedMs}` shape; the status is always `200`-ish
//! when the transport works, never the upstream's.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::executors::http::tls_builder;

const DEFAULT_TEST_URL: &str = "https://google.com/";
const DEFAULT_TIMEOUT_MS: u64 = 8000;
const MAX_TIMEOUT_MS: u64 = 30_000;

/// `testProxyUrl({proxyUrl, testUrl, timeoutMs})`.
///
/// Never returns `Err`: a failure is a `{ok:false, status, error}` body, because
/// the route surfaces the object rather than an HTTP error.
pub async fn test_proxy_url(
    proxy_url: Option<&str>,
    test_url: Option<&str>,
    timeout_ms: Option<u64>,
) -> Value {
    let normalized_proxy_url = proxy_url.unwrap_or("").trim();
    if normalized_proxy_url.is_empty() {
        return json!({ "ok": false, "status": 400, "error": "proxyUrl is required" });
    }

    let normalized_test_url = test_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_TEST_URL);
    let normalized_timeout_ms = timeout_ms
        .filter(|n| *n > 0)
        .map(|n| n.min(MAX_TIMEOUT_MS))
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    let proxy = match reqwest::Proxy::all(normalized_proxy_url) {
        Ok(p) => p,
        Err(e) => {
            return json!({
                "ok": false,
                "status": 400,
                "error": format!("Invalid proxy URL: {e}"),
            });
        }
    };

    let client = match tls_builder().proxy(proxy).build() {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "ok": false,
                "status": 400,
                "error": format!("Invalid proxy URL: {e}"),
            });
        }
    };

    let started = Instant::now();
    let result = client
        .head(normalized_test_url)
        .header("User-Agent", "9Router")
        .timeout(Duration::from_millis(normalized_timeout_ms))
        .send()
        .await;

    match result {
        Ok(res) => json!({
            "ok": res.status().is_success(),
            "status": res.status().as_u16(),
            "statusText": res.status().canonical_reason().unwrap_or(""),
            "url": normalized_test_url,
            "elapsedMs": started.elapsed().as_millis() as u64,
        }),
        Err(e) => {
            let message = if e.is_timeout() {
                "Proxy test timed out".to_string()
            } else {
                get_error_message(&e)
            };
            json!({ "ok": false, "status": 500, "error": message })
        }
    }
}

/// `getErrorMessage(err)`: prefer the innermost cause when the outer message
/// does not already carry it.
fn get_error_message(err: &reqwest::Error) -> String {
    let base = err.to_string();
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(err);
    let mut deepest = base.clone();
    while let Some(s) = source {
        deepest = s.to_string();
        source = std::error::Error::source(s);
    }
    if deepest != base && !base.contains(&deepest) {
        format!("{base}: {deepest}")
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_proxy_url_is_a_400_body() {
        let out = test_proxy_url(Some("  "), None, None).await;
        assert_eq!(out["ok"], json!(false));
        assert_eq!(out["status"], json!(400));
        assert_eq!(out["error"], json!("proxyUrl is required"));
    }

    #[tokio::test]
    async fn an_unparseable_proxy_url_is_a_400_body() {
        let out = test_proxy_url(Some("http://"), None, None).await;
        assert_eq!(out["status"], json!(400));
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .starts_with("Invalid proxy URL")
        );
    }
}
