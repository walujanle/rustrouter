//! Turn a connection's `providerSpecificData` into the proxy configuration the
//! chat pipeline hands the executor.
//!
//! Three sources, in priority order: a proxy pool row, the legacy inline
//! `connectionProxy*` fields, then nothing. Vercel/Cloudflare/Deno pools are
//! relay base-URL rewrites rather than `HTTP_PROXY` values, so they come back
//! with `connectionProxyEnabled: false` and a `vercelRelayUrl`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use serde_json::{Map, Value};

use router_db::Db;

use crate::translator::concerns::primitives::js_string;

/// The resolved proxy configuration.
#[derive(Debug, Clone, Default)]
pub struct ResolvedProxyConfig {
    /// `pool` | `legacy` | `none` | `vercel` | `cloudflare` | `deno` | `error`.
    pub source: String,
    pub proxy_pool_id: Option<String>,
    pub proxy_pool: Option<Value>,
    pub connection_proxy_enabled: bool,
    pub connection_proxy_url: String,
    pub connection_no_proxy: String,
    pub strict_proxy: bool,
    pub vercel_relay_url: String,
}

/// `normalizeString(value)`: `undefined`/`null` → `""`, else `String(v).trim()`.
pub fn normalize_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(v) => js_string(v).trim().to_string(),
    }
}

/// `rotateState`: providerId → next index. In-memory only, so it resets on
/// restart.
static ROTATE_STATE: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `pickProxyPoolId(poolIds, strategy, providerId)`.
///
/// `round-robin` cycles sequentially, `random` picks uniformly, anything else
/// takes the first entry.
pub fn pick_proxy_pool_id(
    pool_ids: &[String],
    strategy: &str,
    provider_id: &str,
) -> Option<String> {
    if pool_ids.is_empty() {
        return None;
    }
    if pool_ids.len() == 1 {
        return Some(pool_ids[0].clone());
    }

    if strategy == "round-robin" {
        let mut state = ROTATE_STATE.lock().unwrap_or_else(|e| e.into_inner());
        // `(state.index + 1) % len` starting from -1: the first call yields 0.
        let next = match state.get(provider_id) {
            Some(current) => (current + 1) % pool_ids.len(),
            None => 0,
        };
        state.insert(provider_id.to_string(), next);
        return Some(pool_ids[next].clone());
    }

    if strategy == "random" {
        use rand::RngExt;
        let index = rand::rng().random_range(0..pool_ids.len());
        return Some(pool_ids[index].clone());
    }

    Some(pool_ids[0].clone())
}

/// `normalizeLegacyProxy(providerSpecificData)`.
fn normalize_legacy_proxy(psd: Option<&Map<String, Value>>) -> ResolvedProxyConfig {
    let enabled = psd
        .and_then(|m| m.get("connectionProxyEnabled"))
        .and_then(Value::as_bool)
        == Some(true);
    ResolvedProxyConfig {
        connection_proxy_enabled: enabled,
        connection_proxy_url: normalize_string(psd.and_then(|m| m.get("connectionProxyUrl"))),
        connection_no_proxy: normalize_string(psd.and_then(|m| m.get("connectionNoProxy"))),
        ..ResolvedProxyConfig::default()
    }
}

/// `resolveConnectionProxyConfig(providerSpecificData)`.
///
/// Never fails: a DB error yields the `source: "error"` shape.
pub fn resolve_connection_proxy_config(
    db: &Db,
    psd: Option<&Map<String, Value>>,
) -> ResolvedProxyConfig {
    let raw = normalize_string(psd.and_then(|m| m.get("proxyPoolId")));
    // `"__none__"` explicitly disables pool resolution.
    let proxy_pool_id = if raw == "__none__" {
        String::new()
    } else {
        raw
    };
    let legacy = normalize_legacy_proxy(psd);

    if !proxy_pool_id.is_empty() {
        let pool = db.with_conn(|conn| {
            router_db::repos::proxy_pools::get_proxy_pool_by_id(conn, &proxy_pool_id)
        });
        let pool = match pool {
            Ok(pool) => pool,
            Err(error) => {
                tracing::error!("[resolveConnectionProxyConfig] pool lookup failed: {error}");
                return ResolvedProxyConfig {
                    source: "error".to_string(),
                    ..ResolvedProxyConfig::default()
                };
            }
        };

        let proxy_url = normalize_string(pool.as_ref().and_then(|p| p.get("proxyUrl")));
        let no_proxy = normalize_string(pool.as_ref().and_then(|p| p.get("noProxy")));
        let is_active = pool
            .as_ref()
            .and_then(|p| p.get("isActive"))
            .and_then(Value::as_bool)
            == Some(true);

        if let Some(pool) = pool.filter(|_| is_active && !proxy_url.is_empty()) {
            let pool_type = pool.get("type").and_then(Value::as_str).unwrap_or("");
            let strict_proxy = pool.get("strictProxy").and_then(Value::as_bool) == Some(true);

            if matches!(pool_type, "vercel" | "cloudflare" | "deno") {
                return ResolvedProxyConfig {
                    source: pool_type.to_string(),
                    proxy_pool_id: Some(proxy_pool_id),
                    proxy_pool: Some(pool),
                    connection_proxy_enabled: false,
                    connection_proxy_url: String::new(),
                    connection_no_proxy: no_proxy,
                    strict_proxy,
                    vercel_relay_url: proxy_url,
                };
            }

            return ResolvedProxyConfig {
                source: "pool".to_string(),
                proxy_pool_id: Some(proxy_pool_id),
                proxy_pool: Some(pool),
                connection_proxy_enabled: true,
                connection_proxy_url: proxy_url,
                connection_no_proxy: no_proxy,
                strict_proxy,
                vercel_relay_url: String::new(),
            };
        }
    }

    if legacy.connection_proxy_enabled && !legacy.connection_proxy_url.is_empty() {
        return ResolvedProxyConfig {
            source: "legacy".to_string(),
            proxy_pool_id: Some(proxy_pool_id).filter(|s| !s.is_empty()),
            proxy_pool: None,
            ..legacy
        };
    }

    ResolvedProxyConfig {
        source: "none".to_string(),
        proxy_pool_id: Some(proxy_pool_id).filter(|s| !s.is_empty()),
        proxy_pool: None,
        ..legacy
    }
}

/// The `providerSpecificData` fields merged onto the resolved connection before
/// it is handed to the executor.
pub fn proxy_fields_into_psd(psd: &mut Map<String, Value>, resolved: &ResolvedProxyConfig) {
    psd.insert(
        "connectionProxyEnabled".into(),
        Value::Bool(resolved.connection_proxy_enabled),
    );
    psd.insert(
        "connectionProxyUrl".into(),
        Value::String(resolved.connection_proxy_url.clone()),
    );
    psd.insert(
        "connectionNoProxy".into(),
        Value::String(resolved.connection_no_proxy.clone()),
    );
    psd.insert(
        "connectionProxyPoolId".into(),
        resolved
            .proxy_pool_id
            .clone()
            .map_or(Value::Null, Value::String),
    );
    psd.insert(
        "vercelRelayUrl".into(),
        Value::String(resolved.vercel_relay_url.clone()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn psd(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn no_config_resolves_to_none() {
        let resolved = normalize_legacy_proxy(Some(&psd(json!({}))));
        assert!(!resolved.connection_proxy_enabled);
        assert_eq!(resolved.connection_proxy_url, "");
    }

    #[test]
    fn legacy_proxy_needs_both_the_flag_and_a_url() {
        let with_url = normalize_legacy_proxy(Some(&psd(json!({
            "connectionProxyEnabled": true,
            "connectionProxyUrl": "http://p:8080",
        }))));
        assert!(with_url.connection_proxy_enabled);
        assert_eq!(with_url.connection_proxy_url, "http://p:8080");

        let no_url = normalize_legacy_proxy(Some(&psd(json!({"connectionProxyEnabled": true}))));
        assert_eq!(no_url.connection_proxy_url, "");
    }

    #[test]
    fn pick_proxy_pool_id_cycles_then_wraps() {
        let pools: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            pick_proxy_pool_id(&pools, "round-robin", "p"),
            Some("a".into())
        );
        assert_eq!(
            pick_proxy_pool_id(&pools, "round-robin", "p"),
            Some("b".into())
        );
        assert_eq!(
            pick_proxy_pool_id(&pools, "round-robin", "p"),
            Some("c".into())
        );
        assert_eq!(
            pick_proxy_pool_id(&pools, "round-robin", "p"),
            Some("a".into())
        );
        assert_eq!(pick_proxy_pool_id(&[], "round-robin", "p"), None);
        assert_eq!(pick_proxy_pool_id(&pools, "none", "p"), Some("a".into()));
    }

    #[test]
    fn normalize_string_trims_and_keeps_non_strings() {
        assert_eq!(normalize_string(None), "");
        assert_eq!(normalize_string(Some(&Value::Null)), "");
        assert_eq!(normalize_string(Some(&json!("  x  "))), "x");
        assert_eq!(normalize_string(Some(&json!(12))), "12");
    }
}
