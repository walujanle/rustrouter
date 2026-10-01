//! The outbound HTTP client, including the client construction every executor
//! shares.
//!
//! The proxy setting must take effect without a restart. Rust has no global
//! fetch to patch, so the proxy config lives behind an `RwLock` here and each
//! request's client is built from the current value — a settings change lands
//! on the next request.
//!
//! Two behaviours are deliberately preserved:
//!
//! * `NO_PROXY` matching, including the leading-dot form.
//! * The MITM DNS bypass for a fixed host list. A previous install may have left
//!   an `/etc/hosts` entry (or a local resolver override) for those hosts; the
//!   bypass resolves the real IP and pins it with `ClientBuilder::resolve`, which
//!   keeps SNI and certificate validation on the hostname. `ponytail:` this uses
//!   DoH (`dns.google`) rather than raw DNS, because a Rust std DNS client does
//!   not exist and one dependency for this is not worth it.
//!
//! No client here carries a request timeout. The abort on timeout only covers
//! time-to-headers — it is cleared the moment the response head arrives — so a
//! client-level `timeout` would kill a long SSE stream mid-flight. The connect
//! deadline is enforced by the caller around `send`; a stalled body is the
//! stream watchdog's job.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde_json::Value;

use crate::runtime_config::memory_config;

/// Hosts whose DNS is resolved through DoH to bypass a stale local override.
/// A previous install may have left an `/etc/hosts` entry or resolver override
/// for these hosts.
const MITM_BYPASS_HOSTS: [&str; 6] = [
    "cloudcode-pa.googleapis.com",
    "daily-cloudcode-pa.googleapis.com",
    "api.individual.githubcopilot.com",
    "q.us-east-1.amazonaws.com",
    "codewhisperer.us-east-1.amazonaws.com",
    "api2.cursor.sh",
];

/// The per-request proxy options: the per-connection proxy plus the Vercel
/// relay.
#[derive(Debug, Clone, Default)]
pub struct ProxyOptions {
    pub enabled: bool,
    pub url: Option<String>,
    pub no_proxy: Option<String>,
    pub strict_proxy: bool,
    pub vercel_relay_url: Option<String>,
}

impl ProxyOptions {
    /// Build from the camelCase settings JSON.
    pub fn from_value(v: Option<&Value>) -> Self {
        let Some(v) = v else {
            return Self::default();
        };
        let get = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Self {
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(false)
                || v.get("connectionProxyEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            url: get("url").or_else(|| get("connectionProxyUrl")),
            no_proxy: get("noProxy").or_else(|| get("connectionNoProxy")),
            strict_proxy: v
                .get("strictProxy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            vercel_relay_url: get("vercelRelayUrl"),
        }
    }
}

/// The process-wide outbound proxy setting, applied on `set`.
#[derive(Debug, Clone, Default)]
pub struct OutboundProxy {
    pub enabled: bool,
    pub url: Option<String>,
    pub no_proxy: Option<String>,
}

static OUTBOUND: LazyLock<RwLock<OutboundProxy>> =
    LazyLock::new(|| RwLock::new(OutboundProxy::default()));

/// Store the process-wide outbound proxy setting. The server calls this on
/// settings load and on every settings change.
pub fn set_outbound_proxy(proxy: OutboundProxy) {
    if let Ok(mut guard) = OUTBOUND.write() {
        *guard = proxy;
    }
    // The proxy is baked into each cached client, so the cache is stale the
    // moment the setting changes.
    if let Ok(mut map) = CLIENTS.write() {
        map.clear();
    }
}

/// The current outbound proxy setting.
pub fn outbound_proxy() -> OutboundProxy {
    OUTBOUND.read().map(|g| g.clone()).unwrap_or_default()
}

/// Read the three proxy settings keys and store them. Called at boot and on
/// every settings write, so a change takes effect without a restart.
///
/// Rust has no shared env contract to honor, so the setting is stored directly
/// and read by `resolve_proxy_url`; an externally-set `HTTP_PROXY` is still
/// consulted through `env_proxy_url` when this setting is off.
pub fn apply_outbound_proxy_settings(settings: &Value) {
    set_outbound_proxy(OutboundProxy {
        enabled: settings
            .get("outboundProxyEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        url: settings
            .get("outboundProxyUrl")
            .and_then(Value::as_str)
            .map(str::to_string),
        no_proxy: settings
            .get("outboundNoProxy")
            .and_then(Value::as_str)
            .map(str::to_string),
    });
}

/// Normalize a proxy URL: a bare `host:port` becomes `http://host:port`.
fn normalize_proxy_url(raw: Option<&str>) -> Option<String> {
    let s = raw.unwrap_or("").trim();
    if s.is_empty() {
        return None;
    }
    if url::Url::parse(s).is_ok() {
        Some(s.to_string())
    } else {
        Some(format!("http://{s}"))
    }
}

/// Whether `no_proxy` excludes the target, including the leading-dot form.
fn should_bypass_by_no_proxy(target_url: &str, no_proxy: Option<&str>) -> bool {
    let no_proxy = no_proxy.unwrap_or("").trim();
    if no_proxy.is_empty() {
        return false;
    }
    let Ok(parsed) = url::Url::parse(target_url) else {
        return false;
    };
    let Some(hostname) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return false;
    };
    no_proxy
        .split(',')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .any(|pattern| {
            if pattern == "*" {
                return true;
            }
            if let Some(suffix) = pattern.strip_prefix('.') {
                return hostname.ends_with(&pattern) || hostname == suffix;
            }
            hostname == pattern || hostname.ends_with(&format!(".{pattern}"))
        })
}

/// The env-var proxy for the target's scheme.
fn env_proxy_url(target_url: &str, no_proxy: Option<&str>) -> Option<String> {
    if should_bypass_by_no_proxy(target_url, no_proxy) {
        return None;
    }
    let protocol = url::Url::parse(target_url).ok()?.scheme().to_string();
    let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    if protocol == "https" {
        get("HTTPS_PROXY")
            .or_else(|| get("https_proxy"))
            .or_else(|| get("ALL_PROXY"))
            .or_else(|| get("all_proxy"))
    } else {
        get("HTTP_PROXY")
            .or_else(|| get("http_proxy"))
            .or_else(|| get("ALL_PROXY"))
            .or_else(|| get("all_proxy"))
    }
}

/// A `scheme://host:port` label for the console log. Never the full URL: a
/// provider URL can carry an API key in its query string and a proxy URL can
/// carry `user:pass@` credentials, and neither belongs in a log line.
fn log_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => match (u.host_str(), u.port_or_known_default()) {
            (Some(h), Some(p)) => format!("{}://{h}:{p}", u.scheme()),
            (Some(h), None) => format!("{}://{h}", u.scheme()),
            _ => format!("{}://<no-host>", u.scheme()),
        },
        Err(_) => "<invalid-url>".into(),
    }
}

/// Whether the target's host is on the MITM DNS bypass list.
fn should_bypass_mitm_dns(target_url: &str) -> bool {
    url::Url::parse(target_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| MITM_BYPASS_HOSTS.iter().any(|host| h.contains(host)))
}

/// Resolve the real IP for a hostname over DoH, cached for the configured DNS
/// TTL.
async fn resolve_real_ip(hostname: &str) -> Option<std::net::IpAddr> {
    static CACHE: LazyLock<std::sync::Mutex<HashMap<String, (std::net::IpAddr, i64)>>> =
        LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
    let now = crate::session_manager::now_ms() as i64;
    if let Ok(cache) = CACHE.lock()
        && let Some((ip, expiry)) = cache.get(hostname)
        && now < *expiry
    {
        return Some(*ip);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let resp = client
        .get("https://dns.google/resolve")
        .query(&[("name", hostname), ("type", "A")])
        .send()
        .await
        .ok()?;
    let json: Value = resp.json().await.ok()?;
    let ip = json
        .get("Answer")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("type").and_then(Value::as_u64) == Some(1))
        })
        .and_then(|r| r.get("data"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())?;

    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(
            hostname.to_string(),
            (ip, now + memory_config::DNS_CACHE_TTL_MS),
        );
    }
    Some(ip)
}

/// The resolved proxy for a target URL: the connection proxy wins over the
/// environment. The second element names the source for the console log.
fn resolve_proxy_url(
    target_url: &str,
    proxy_options: &ProxyOptions,
) -> (Option<String>, &'static str) {
    let connection = if proxy_options.enabled {
        let raw = normalize_proxy_url(proxy_options.url.as_deref());
        match raw {
            Some(url)
                if !should_bypass_by_no_proxy(target_url, proxy_options.no_proxy.as_deref()) =>
            {
                Some(url)
            }
            Some(_) => {
                // A configured proxy that `no_proxy` excludes: the request goes
                // direct by design, and saying so stops a false "proxy broken".
                tracing::info!(
                    target: "router_sse::proxy",
                    "[ProxyFetch] connection proxy bypassed by no_proxy -> {}",
                    log_host(target_url)
                );
                None
            }
            None => None,
        }
    } else {
        None
    };
    if connection.is_some() {
        return (connection, "connection");
    }
    let outbound = outbound_proxy();
    let no_proxy = proxy_options
        .no_proxy
        .clone()
        .or_else(|| outbound.no_proxy.clone());
    if outbound.enabled {
        if let Some(url) = normalize_proxy_url(outbound.url.as_deref())
            && !should_bypass_by_no_proxy(target_url, no_proxy.as_deref())
        {
            return (Some(url), "outbound");
        }
        return (None, "none");
    }
    match normalize_proxy_url(env_proxy_url(target_url, no_proxy.as_deref()).as_deref()) {
        Some(url) => (Some(url), "env"),
        None => (None, "none"),
    }
}

/// What a `reqwest::Client` is configured with: the proxy URL and the pinned
/// DNS override. Both are baked in at build time, so two requests sharing a key
/// share one client and one connection pool.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    proxy: Option<String>,
    bypass_host: Option<String>,
    bypass_ip: Option<std::net::IpAddr>,
    bypass_port: Option<u16>,
    /// The SSRF guard's manual hop loop needs a client that does not follow
    /// redirects internally; every other caller keeps reqwest's default policy.
    no_redirect: bool,
}

impl ClientKey {
    fn direct() -> Self {
        Self {
            proxy: None,
            bypass_host: None,
            bypass_ip: None,
            bypass_port: None,
            no_redirect: false,
        }
    }

    fn proxied(proxy: &str) -> Self {
        Self {
            proxy: Some(proxy.to_string()),
            ..Self::direct()
        }
    }

    fn pinned(host: &str, ip: std::net::IpAddr, port: u16) -> Self {
        Self {
            bypass_host: Some(host.to_string()),
            bypass_ip: Some(ip),
            bypass_port: Some(port),
            ..Self::direct()
        }
    }
}

/// One client per key, built lazily. A fresh client per request rebuilds the
/// TLS config, the resolver and the connection pool every time, which is the
/// churn the allocator then holds onto. The key set is small — one direct
/// client, one per proxy URL, one per pinned MITM host — and the whole map is
/// cleared when the proxy setting changes.
static CLIENTS: LazyLock<RwLock<HashMap<ClientKey, reqwest::Client>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Cap on cached clients. A pinned host whose DoH answer changes mints a new
/// key, so without a bound the map would grow for the process lifetime. Each
/// client is cheap to rebuild, so dropping the whole map on overflow costs one
/// TLS handshake per live key.
const MAX_CACHED_CLIENTS: usize = 32;

/// The shared client for `key`, building and caching it on first use.
///
/// No client-level timeout: it would abort a long SSE stream mid-flight. The
/// connect deadline is the caller's, and a stalled body is the watchdog's.
fn client_for(key: ClientKey) -> Result<reqwest::Client, reqwest::Error> {
    if let Ok(map) = CLIENTS.read()
        && let Some(client) = map.get(&key)
    {
        return Ok(client.clone());
    }
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(30));
    if key.no_redirect {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if let Some(proxy) = &key.proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    if let (Some(host), Some(ip), Some(port)) = (&key.bypass_host, key.bypass_ip, key.bypass_port) {
        builder = builder.resolve(host, SocketAddr::new(ip, port));
    }
    let client = builder.build()?;
    if let Ok(mut map) = CLIENTS.write() {
        if map.len() >= MAX_CACHED_CLIENTS {
            map.clear();
        }
        map.insert(key, client.clone());
    }
    Ok(client)
}

/// The client for one request, honoring the proxy and the MITM bypass.
///
/// A strict-proxy failure is surfaced rather than silently falling back to a
/// direct connection.
fn build_client(
    target_url: &str,
    proxy_url: Option<&str>,
    bypass_ip: Option<std::net::IpAddr>,
    no_redirect: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut key = match (proxy_url, bypass_ip) {
        (Some(proxy), _) => ClientKey::proxied(proxy),
        (None, Some(ip)) => {
            let parsed = url::Url::parse(target_url).ok();
            let host = parsed
                .as_ref()
                .and_then(|u| u.host_str())
                .unwrap_or("")
                .to_string();
            let port = parsed
                .as_ref()
                .and_then(|u| u.port_or_known_default())
                .unwrap_or(443);
            ClientKey::pinned(&host, ip, port)
        }
        (None, None) => ClientKey::direct(),
    };
    key.no_redirect = no_redirect;
    client_for(key)
}

/// The client and URL a send should use. The relay rewrites both.
pub struct SendTarget {
    pub client: reqwest::Client,
    pub url: String,
    pub extra_headers: Vec<(String, String)>,
}

/// Target resolution: relay, then proxy, then direct.
pub async fn prepare_send(
    url: &str,
    proxy_options: &ProxyOptions,
) -> Result<SendTarget, SendError> {
    prepare_send_with_redirect_policy(url, proxy_options, true).await
}

/// [`prepare_send`] with control over reqwest's redirect policy.
///
/// `follow_redirects: false` gives the SSRF guard's manual hop loop a client
/// that will not silently follow a redirect it has not re-validated.
pub async fn prepare_send_with_redirect_policy(
    url: &str,
    proxy_options: &ProxyOptions,
    follow_redirects: bool,
) -> Result<SendTarget, SendError> {
    let no_redirect = !follow_redirects;
    if let Some(relay) = proxy_options
        .vercel_relay_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        && let Ok(parsed) = url::Url::parse(url)
    {
        let client = build_client(url, None, None, no_redirect).map_err(SendError::Build)?;
        tracing::info!(
            target: "router_sse::proxy",
            "[ProxyFetch] relay -> {} via {}",
            log_host(url),
            log_host(relay)
        );
        return Ok(SendTarget {
            client,
            url: relay.to_string(),
            extra_headers: vec![
                (
                    "x-relay-target".into(),
                    format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or("")),
                ),
                (
                    "x-relay-path".into(),
                    format!(
                        "{}{}",
                        parsed.path(),
                        parsed.query().map(|q| format!("?{q}")).unwrap_or_default()
                    ),
                ),
            ],
        });
    }

    let (proxy_url, proxy_source) = resolve_proxy_url(url, proxy_options);

    if should_bypass_mitm_dns(url)
        && proxy_url.is_none()
        && let Ok(parsed) = url::Url::parse(url)
        && let Some(host) = parsed.host_str()
        && let Some(ip) = resolve_real_ip(host).await
    {
        let client = build_client(url, None, Some(ip), no_redirect).map_err(SendError::Build)?;
        tracing::info!(
            target: "router_sse::proxy",
            "[ProxyFetch] direct -> {} (mitm dns bypass, pinned {ip})",
            log_host(url)
        );
        return Ok(SendTarget {
            client,
            url: url.to_string(),
            extra_headers: Vec::new(),
        });
    }

    if let Some(proxy) = &proxy_url {
        match build_client(url, Some(proxy), None, no_redirect) {
            Ok(client) => {
                tracing::info!(
                    target: "router_sse::proxy",
                    "[ProxyFetch] proxy -> {} via {} ({proxy_source})",
                    log_host(url),
                    log_host(proxy)
                );
                return Ok(SendTarget {
                    client,
                    url: url.to_string(),
                    extra_headers: Vec::new(),
                });
            }
            Err(e) => {
                if proxy_options.strict_proxy {
                    tracing::error!(
                        target: "router_sse::proxy",
                        "[ProxyFetch] strictProxy, refusing direct fallback -> {}: {e}",
                        log_host(url)
                    );
                    return Err(SendError::StrictProxy(e.to_string()));
                }
                tracing::warn!(
                    target: "router_sse::proxy",
                    "[ProxyFetch] proxy failed, falling back to direct -> {}: {e}",
                    log_host(url)
                );
            }
        }
    }

    tracing::info!(
        target: "router_sse::proxy",
        "[ProxyFetch] direct -> {}",
        log_host(url)
    );
    let client = build_client(url, None, None, no_redirect).map_err(SendError::Build)?;
    Ok(SendTarget {
        client,
        url: url.to_string(),
        extra_headers: Vec::new(),
    })
}

/// A send failure.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("client build failed: {0}")]
    Build(reqwest::Error),
    #[error("proxy required but failed (strictProxy=true): {0}")]
    StrictProxy(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_proxy_matches_exact_suffix_and_wildcard() {
        assert!(should_bypass_by_no_proxy(
            "https://api.x.com/v1",
            Some("api.x.com")
        ));
        assert!(should_bypass_by_no_proxy(
            "https://api.x.com/v1",
            Some("x.com")
        ));
        assert!(should_bypass_by_no_proxy(
            "https://api.x.com/v1",
            Some(".x.com")
        ));
        assert!(should_bypass_by_no_proxy("https://api.x.com/v1", Some("*")));
        assert!(!should_bypass_by_no_proxy(
            "https://api.x.com/v1",
            Some("y.com")
        ));
        // The leading-dot form also matches the bare domain.
        assert!(should_bypass_by_no_proxy(
            "https://x.com/v1",
            Some(".x.com")
        ));
        assert!(!should_bypass_by_no_proxy(
            "https://notx.com/v1",
            Some("x.com")
        ));
    }

    #[test]
    fn proxy_url_normalizes_a_bare_host_port() {
        assert_eq!(
            normalize_proxy_url(Some("127.0.0.1:7890")).as_deref(),
            Some("http://127.0.0.1:7890")
        );
        assert_eq!(
            normalize_proxy_url(Some("http://p:1")).as_deref(),
            Some("http://p:1")
        );
        assert_eq!(normalize_proxy_url(Some("  ")).as_deref(), None);
    }

    #[test]
    fn mitm_hosts_are_matched_by_substring() {
        assert!(should_bypass_mitm_dns(
            "https://daily-cloudcode-pa.googleapis.com/v1internal:x"
        ));
        assert!(should_bypass_mitm_dns("https://api2.cursor.sh/agent"));
        assert!(!should_bypass_mitm_dns("https://api.openai.com/v1"));
    }

    /// The console log names where a proxy came from; the connection proxy
    /// wins, and a `no_proxy` exclusion resolves to no proxy at all.
    #[test]
    fn resolve_proxy_url_labels_the_connection_source() {
        let opts = ProxyOptions {
            enabled: true,
            url: Some("http://127.0.0.1:7890".into()),
            no_proxy: None,
            strict_proxy: false,
            vercel_relay_url: None,
        };
        let (url, source) = resolve_proxy_url("https://api.openai.com/v1", &opts);
        assert_eq!(source, "connection");
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:7890"));

        let excluded = ProxyOptions {
            no_proxy: Some("openai.com".into()),
            ..opts
        };
        let (url, source) = resolve_proxy_url("https://api.openai.com/v1", &excluded);
        assert_eq!(source, "none");
        assert!(url.is_none());
    }

    #[test]
    fn the_same_key_reuses_one_cached_client_and_distinct_keys_do_not() {
        CLIENTS.write().unwrap().clear();
        // `reqwest::Client` is `Arc`-internal, so a second call for the same key
        // must hand back a clone of the cached one rather than building another.
        // The map size is the observable proof that no rebuild happened.
        let _ = client_for(ClientKey::direct()).unwrap();
        let _ = client_for(ClientKey::direct()).unwrap();
        assert_eq!(CLIENTS.read().unwrap().len(), 1);

        let pinned = ClientKey::pinned("api.x.com", "127.0.0.1".parse().unwrap(), 443);
        let _ = client_for(pinned).unwrap();
        assert_eq!(CLIENTS.read().unwrap().len(), 2);

        // Overflow drops the map instead of growing it without bound.
        for port in 0..MAX_CACHED_CLIENTS as u16 + 1 {
            let _ = client_for(ClientKey::pinned("h", "127.0.0.1".parse().unwrap(), port)).unwrap();
        }
        assert!(CLIENTS.read().unwrap().len() <= MAX_CACHED_CLIENTS);
        CLIENTS.write().unwrap().clear();
    }

    #[test]
    fn proxy_options_read_the_connection_shape() {
        let v = serde_json::json!({
            "connectionProxyEnabled": true,
            "connectionProxyUrl": "http://p:1",
            "connectionNoProxy": "a.com"
        });
        let o = ProxyOptions::from_value(Some(&v));
        assert!(o.enabled);
        assert_eq!(o.url.as_deref(), Some("http://p:1"));
        assert_eq!(o.no_proxy.as_deref(), Some("a.com"));
    }
}
