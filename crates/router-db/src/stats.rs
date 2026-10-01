//! Process-global live-request state.
//!
//! A single `OnceLock` holds the state for the process, so it survives module
//! re-evaluation without the escape hatch a `global` would need.

use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value, json};

/// `PENDING_TIMEOUT_MS`.
pub const PENDING_TIMEOUT_MS: i64 = 60 * 1000;
/// `CONN_CACHE_TTL_MS`.
pub const CONN_CACHE_TTL_MS: i64 = 30 * 1000;
/// How long an error provider stays "recent" in the stats payload.
pub const ERROR_PROVIDER_WINDOW_MS: i64 = 10_000;

/// The window length in milliseconds for a named period.
pub fn period_ms(period: &str) -> Option<i64> {
    match period {
        "24h" => Some(86_400_000),
        "7d" => Some(604_800_000),
        "30d" => Some(2_592_000_000),
        "60d" => Some(5_184_000_000),
        _ => None,
    }
}

/// Mask an API key for display.
///
/// Keys sharing a machine-id prefix (team keys) must not collide, so the tail
/// is kept once the key is long enough to have a distinct one.
pub fn mask_api_key(key: Option<&str>) -> Option<String> {
    let key = key?;
    if key.is_empty() {
        return None;
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 12 {
        return Some(format!("{}***", chars[0]));
    }
    Some(format!(
        "{}***{}",
        chars[..8].iter().collect::<String>(),
        chars[chars.len() - 4..].iter().collect::<String>()
    ))
}

/// Live pending requests, the error marker and the connection cache.
#[derive(Default)]
pub struct UsageState {
    pub pending_by_model: indexmap::IndexMap<String, i64>,
    pub pending_by_account: indexmap::IndexMap<String, indexmap::IndexMap<String, i64>>,
    pub last_error_provider: (String, i64),
    pub connection_map: indexmap::IndexMap<String, String>,
    pub connection_map_ts: i64,
    /// `(deadline_ms, model_key)` pairs, swept lazily instead of held as
    /// per-request timers.
    pub stale_deadlines: Vec<(i64, String)>,
}

static STATE: OnceLock<Mutex<UsageState>> = OnceLock::new();

/// The single process-wide instance.
pub fn state() -> &'static Mutex<UsageState> {
    STATE.get_or_init(|| Mutex::new(UsageState::default()))
}

/// Record the start or end of a pending request.
///
/// Each start also records a 60 s deadline; [`sweep_stale_pending`] zeroes a
/// counter whose deadline has passed, so no timer handles are kept alive per
/// request.
pub fn track_pending_request(
    model: &str,
    provider: Option<&str>,
    connection_id: Option<&str>,
    started: bool,
    error: bool,
    now_ms: i64,
) {
    let model_key = match provider {
        Some(p) if !p.is_empty() => format!("{model} ({p})"),
        _ => model.to_string(),
    };
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());

    let delta = if started { 1 } else { -1 };
    let entry = st.pending_by_model.entry(model_key.clone()).or_insert(0);
    *entry = (*entry + delta).max(0);
    if *entry == 0 {
        st.pending_by_model.shift_remove(&model_key);
    }

    if let Some(conn_id) = connection_id.filter(|s| !s.is_empty()) {
        let account = st
            .pending_by_account
            .entry(conn_id.to_string())
            .or_default();
        let cell = account.entry(model_key.clone()).or_insert(0);
        *cell = (*cell + delta).max(0);
        if *cell == 0 {
            account.shift_remove(&model_key);
        }
        if account.is_empty() {
            st.pending_by_account.shift_remove(conn_id);
        }
    }

    if !started
        && error
        && let Some(p) = provider.filter(|s| !s.is_empty())
    {
        st.last_error_provider = (p.to_lowercase(), now_ms);
    }
    st.stale_deadlines
        .retain(|(deadline, _)| *deadline > now_ms);
    if started {
        st.stale_deadlines
            .push((now_ms + PENDING_TIMEOUT_MS, model_key));
    }
}

/// Zero any counter whose 60 s deadline has passed. Call before reading stats.
pub fn sweep_stale_pending(now_ms: i64) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    let expired: Vec<String> = st
        .stale_deadlines
        .iter()
        .filter(|(deadline, _)| *deadline <= now_ms)
        .map(|(_, key)| key.clone())
        .collect();
    st.stale_deadlines
        .retain(|(deadline, _)| *deadline > now_ms);

    for model_key in expired {
        if let Some(v) = st.pending_by_model.get_mut(&model_key)
            && *v > 0
        {
            *v = 0;
        }
        st.pending_by_model.shift_remove(&model_key);
        for account in st.pending_by_account.values_mut() {
            account.shift_remove(&model_key);
        }
    }
    st.pending_by_account.retain(|_, m| !m.is_empty());
}

/// The cached connection map when it is still inside `CONN_CACHE_TTL_MS`, with
/// no database read. The SSE stream calls this on every stats event, so the
/// common case must not touch `providerConnections` at all.
pub fn fresh_connection_map(now_ms: i64) -> Option<indexmap::IndexMap<String, String>> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    if !st.connection_map.is_empty() && now_ms - st.connection_map_ts < CONN_CACHE_TTL_MS {
        Some(st.connection_map.clone())
    } else {
        None
    }
}

/// Cache the connection-id to display-name map for `CONN_CACHE_TTL_MS`.
pub fn cached_connection_map(
    now_ms: i64,
    load: impl FnOnce() -> Vec<(String, String)>,
) -> indexmap::IndexMap<String, String> {
    if let Some(map) = fresh_connection_map(now_ms) {
        return map;
    }
    let map: indexmap::IndexMap<String, String> = load().into_iter().collect();
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.connection_map = map.clone();
    st.connection_map_ts = now_ms;
    map
}

/// One entry of `activeRequests`.
pub fn active_requests(connection_map: &indexmap::IndexMap<String, String>) -> Vec<Value> {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = Vec::new();
    for (connection_id, models) in &st.pending_by_account {
        for (model_key, count) in models {
            if *count <= 0 {
                continue;
            }
            let account_name = connection_map
                .get(connection_id)
                .cloned()
                .unwrap_or_else(|| {
                    let head: String = connection_id.chars().take(8).collect();
                    format!("Account {head}...")
                });
            let (model, provider) = split_model_key(model_key);
            out.push(json!({
                "model": model,
                "provider": provider,
                "account": account_name,
                "count": count,
            }));
        }
    }
    out
}

/// The error provider when it is still within the 10 s window.
pub fn recent_error_provider(now_ms: i64) -> String {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    if now_ms - st.last_error_provider.1 < ERROR_PROVIDER_WINDOW_MS {
        st.last_error_provider.0.clone()
    } else {
        String::new()
    }
}

/// A snapshot of the pending maps for the stats payload.
pub fn pending_snapshot() -> Value {
    let st = state().lock().unwrap_or_else(|e| e.into_inner());
    let mut by_model = Map::new();
    for (k, v) in &st.pending_by_model {
        by_model.insert(k.clone(), json!(v));
    }
    let mut by_account = Map::new();
    for (conn, models) in &st.pending_by_account {
        let mut inner = Map::new();
        for (k, v) in models {
            inner.insert(k.clone(), json!(v));
        }
        by_account.insert(conn.clone(), Value::Object(inner));
    }
    json!({ "byModel": by_model, "byAccount": by_account })
}

/// Split `model (provider)` on the last ` (`.
pub fn split_model_key(model_key: &str) -> (String, String) {
    if model_key.ends_with(')')
        && let Some(pos) = model_key.rfind(" (")
    {
        let model = &model_key[..pos];
        let provider = &model_key[pos + 2..model_key.len() - 1];
        return (model.to_string(), provider.to_string());
    }
    (model_key.to_string(), "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_matches_the_expected() {
        assert_eq!(mask_api_key(None), None);
        assert_eq!(mask_api_key(Some("")), None);
        assert_eq!(mask_api_key(Some("short")), Some("s***".into()));
        // 12 chars is still short-form; 13 is the first that keeps the tail.
        assert_eq!(mask_api_key(Some("123456789012")), Some("1***".into()));
        assert_eq!(
            mask_api_key(Some("sk-abcdefghijkl")),
            Some("sk-abcde***ijkl".into())
        );
    }

    #[test]
    fn model_key_split_uses_the_last_paren() {
        assert_eq!(
            split_model_key("gpt-4o (openai)"),
            ("gpt-4o".into(), "openai".into())
        );
        assert_eq!(split_model_key("a (b) (c)"), ("a (b)".into(), "c".into()));
        assert_eq!(split_model_key("bare"), ("bare".into(), "unknown".into()));
        assert_eq!(
            split_model_key("x (unclosed"),
            ("x (unclosed".into(), "unknown".into())
        );
    }

    #[test]
    fn pending_counters_go_up_and_down() {
        // Use a fresh state by exercising the public API; the global is shared
        // across tests in one binary, so keys are namespaced per test.
        track_pending_request("t1", Some("p"), Some("c1"), true, false, 1000);
        track_pending_request("t1", Some("p"), Some("c1"), true, false, 1000);
        let snap = pending_snapshot();
        assert_eq!(snap["byModel"]["t1 (p)"], json!(2));

        track_pending_request("t1", Some("p"), Some("c1"), false, false, 1000);
        let snap = pending_snapshot();
        assert_eq!(snap["byModel"]["t1 (p)"], json!(1));

        track_pending_request("t1", Some("p"), Some("c1"), false, false, 1000);
        let snap = pending_snapshot();
        assert!(
            snap["byModel"].get("t1 (p)").is_none(),
            "zero keys are dropped"
        );
    }

    #[test]
    fn counters_never_go_negative() {
        track_pending_request("t2", None, None, false, false, 1000);
        let snap = pending_snapshot();
        assert!(snap["byModel"].get("t2").is_none());
    }

    #[test]
    fn error_provider_is_recorded_lowercased() {
        track_pending_request("t3", Some("OpenAI"), Some("c3"), false, true, 5000);
        assert_eq!(recent_error_provider(6000), "openai");
        assert_eq!(recent_error_provider(5000 + 10_001), "");
    }

    #[test]
    fn period_ms_matches_the_reference_table() {
        assert_eq!(period_ms("24h"), Some(86_400_000));
        assert_eq!(period_ms("7d"), Some(604_800_000));
        assert_eq!(period_ms("30d"), Some(2_592_000_000));
        assert_eq!(period_ms("60d"), Some(5_184_000_000));
        assert_eq!(period_ms("all"), None);
    }
}
