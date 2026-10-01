//! The three process-level schedulers, started once from `serve_once`.
//! See `docs/RUNTIME.md`.
//!
//! 1. **Proactive OAuth token refresh.** The due-connection predicate is the
//!    pure, tested `background_token_refresh::select_connections_needing_refresh`;
//!    this module owns the tick, the interval and the sequential inter-account
//!    delays.
//! 2. **Quota auto-ping.** Warms the Codex 5-hour window. The auto
//!    quota tracker (a rustrouter addition, `router_db::repos::quota_tracker`)
//!    rides the same usage read.
//! 3. **Model catalog sync**, a self-contained task in
//!    `router_sse::services::model_catalog_sync`.
//!
//! Fail-open everywhere: a tick error or a per-connection failure never kills
//! the interval. The three loops run on the runtime the server is served from,
//! so they stop when the process stops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use serde_json::{Value, json};

use router_sse::credentials::Credentials;
use router_sse::executors::executor::ExecuteRequest;
use router_sse::executors::get_executor;
use router_sse::executors::http::ProxyOptions;
use router_sse::executors::oauth::{parse_time_ms, to_iso};
use router_sse::services::auth::credentials_from_connection;
use router_sse::services::background_token_refresh::select_connections_needing_refresh;
use router_sse::services::connection_proxy::resolve_connection_proxy_config;
use router_sse::services::model_catalog_sync;
use router_sse::services::token_refresh::{check_and_refresh_token, update_provider_credentials};
use router_sse::services::usage::get_usage_for_provider;
use router_sse::utils::in_flight::InFlightGuard;

use crate::state::AppState;

// ─── start ────────────────────────────────────────────────────────────────

/// Start every scheduler. Called once, after the DB is migrated, before the
/// listener binds: the schedulers read connections the moment they come up.
pub async fn start_all(state: AppState) {
    // Catalog sync, then the reader install.
    model_catalog_sync::start_model_catalog_sync();
    model_catalog_sync::install_catalog_source().await;

    start_background_token_refresh(state.clone());

    let settings = state
        .read(router_db::repos::settings::get_settings)
        .await
        .unwrap_or(Value::Null);
    configure_quota_auto_ping(state.clone(), &settings);
    crate::services::update_check::configure(state, &settings);
}

// ─── 1. proactive OAuth token refresh ─────────────────────────────────────

/// Time between refresh sweeps.
const REFRESH_INTERVAL_MS: Duration = Duration::from_secs(5 * 60);
/// Delay before the first sweep, after startup.
const REFRESH_INITIAL_DELAY_MS: Duration = Duration::from_secs(10);
/// Delay between two connection refreshes in one sweep.
const NORMAL_DELAY_MS: u64 = 1_500;

/// Reads an environment flag: `1`, `true`, `yes` or `on` is true.
fn truthy_env(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            let v = v.trim().to_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn env_ms(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

fn start_background_token_refresh(state: AppState) {
    if truthy_env("DISABLE_BACKGROUND_TOKEN_REFRESH") {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(REFRESH_INITIAL_DELAY_MS).await;
        loop {
            token_refresh_tick(&state).await;
            tokio::time::sleep(REFRESH_INTERVAL_MS).await;
        }
    });
}

async fn token_refresh_tick(state: &AppState) {
    let connections = state
        .read(|conn| {
            router_db::repos::connections::get_provider_connections(conn, None, Some(true))
        })
        .await
        .unwrap_or_default();

    let due = select_connections_needing_refresh(&connections, router_db::time::now_ms());
    if due.is_empty() {
        return;
    }

    let normal_delay = env_ms("BG_REFRESH_DELAY_MS", NORMAL_DELAY_MS);

    for (index, id) in due.iter().enumerate() {
        let id = id.clone();
        let Some(connection) = state
            .read(move |conn| {
                router_db::repos::connections::get_provider_connection_by_id(conn, &id)
            })
            .await
            .ok()
            .flatten()
        else {
            continue;
        };

        let provider = connection
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let connection_id = connection.get("id").and_then(Value::as_str).unwrap_or("");
        let proxy = proxy_options_for(state, &connection);
        match refresh_connection(state, &connection, &proxy, true).await {
            Ok(_) => tracing::info!("[BG_TOKEN_REFRESH] refreshed {provider} {connection_id}"),
            Err(error) => tracing::warn!("[BG_TOKEN_REFRESH] {provider} refresh failed: {error}"),
        }

        if index + 1 < due.len() {
            // The 200ms jitter keeps a burst of refreshes off the same instant.
            tokio::time::sleep(Duration::from_millis(normal_delay + 200)).await;
        }
    }
}

// ─── 2. quota auto-ping (+ the auto quota tracker) ────────────────────────

/// Time between quota-ping sweeps.
const QUOTA_TICK_MS: Duration = Duration::from_secs(60);
/// Fire once the reset passes, within this tolerance.
const PING_LEAD_MS: i64 = 5_000;
/// Refetch usage when within this window of the reset.
const REFRESH_AHEAD_MS: i64 = 300_000;
/// Do not hammer a provider that just failed.
const FAILURE_COOLDOWN_MS: i64 = 900_000;

/// One provider's ping settings.
struct PingConfig {
    provider: &'static str,
    settings_key: &'static str,
    quota_key: &'static str,
    ping_when_reset_at_slides: bool,
    reset_at_drift_ms: i64,
    min_ping_interval_ms: i64,
    skip_when_blocking_quota_exhausted: bool,
    ping_model: &'static str,
    ping_text: &'static str,
    ping_instructions: Option<&'static str>,
    ping_reasoning_effort: Option<&'static str>,
}

const PING_CONFIGS: [PingConfig; 1] = [PingConfig {
    provider: "codex",
    settings_key: "codexAutoPing",
    quota_key: "session",
    ping_when_reset_at_slides: true,
    reset_at_drift_ms: 30_000,
    min_ping_interval_ms: 600_000,
    skip_when_blocking_quota_exhausted: true,
    ping_model: "gpt-5.5",
    ping_text: "hi",
    ping_instructions: Some("Reply with OK."),
    ping_reasoning_effort: Some("none"),
}];

static QUOTA_RUNNING: AtomicBool = AtomicBool::new(false);
static QUOTA_TICK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// The loop task, so `stop` aborts it instead of leaving it parked in a sleep
/// that a later `start` would race with a second loop.
static QUOTA_TASK: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
    std::sync::Mutex::new(None);
static RESET_CACHE: std::sync::LazyLock<DashMap<String, String>> =
    std::sync::LazyLock::new(DashMap::new);
static FAILURE_CACHE: std::sync::LazyLock<DashMap<String, i64>> =
    std::sync::LazyLock::new(DashMap::new);

fn cache_key(provider: &str, connection_id: &str) -> String {
    format!("{provider}:{connection_id}")
}

/// Starts the loop while at least one connection is opted in for pinging, or
/// the auto quota tracker is on — the tracker rides the same usage read and
/// needs the loop to run.
pub fn configure_quota_auto_ping(state: AppState, settings: &Value) {
    if has_quota_auto_ping_enabled(settings)
        || router_db::repos::quota_tracker::is_enabled(settings)
    {
        start_quota_auto_ping(state);
    } else {
        stop_quota_auto_ping();
    }
}

/// Whether any provider has at least one connection opted in for pinging.
fn has_quota_auto_ping_enabled(settings: &Value) -> bool {
    PING_CONFIGS.iter().any(|config| {
        settings
            .get(config.settings_key)
            .and_then(|v| v.get("connections"))
            .and_then(Value::as_object)
            .is_some_and(|map| map.values().any(|v| v == &Value::Bool(true)))
    })
}

fn start_quota_auto_ping(state: AppState) {
    if QUOTA_RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = tokio::spawn(async move {
        // One pass immediately, then one per interval.
        run_quota_tick(&state).await;
        loop {
            tokio::time::sleep(QUOTA_TICK_MS).await;
            if !QUOTA_RUNNING.load(Ordering::SeqCst) {
                break;
            }
            run_quota_tick(&state).await;
        }
    });
    if let Ok(mut slot) = QUOTA_TASK.lock() {
        *slot = Some(handle);
    }
}

fn stop_quota_auto_ping() {
    QUOTA_RUNNING.store(false, Ordering::SeqCst);
    if let Ok(mut slot) = QUOTA_TASK.lock()
        && let Some(handle) = slot.take()
    {
        handle.abort();
    }
}

/// One quota-ping sweep over every configured provider.
async fn run_quota_tick(state: &AppState) {
    // Clears `QUOTA_TICK_ACTIVE` on drop, so a panic cannot disable the tick.
    let Some(_guard) = InFlightGuard::acquire(&QUOTA_TICK_ACTIVE) else {
        return;
    };
    let settings = state
        .read(router_db::repos::settings::get_settings)
        .await
        .unwrap_or(Value::Null);
    let tracker_enabled = router_db::repos::quota_tracker::is_enabled(&settings);

    for config in &PING_CONFIGS {
        let enabled_map = settings
            .get(config.settings_key)
            .and_then(|v| v.get("connections"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if enabled_map.is_empty() && !tracker_enabled {
            continue;
        }

        let provider = config.provider;
        let connections = state
            .read(move |conn| {
                router_db::repos::connections::get_provider_connections(
                    conn,
                    Some(provider),
                    Some(true),
                )
            })
            .await
            .unwrap_or_default();

        for connection in connections {
            if connection.get("authType").and_then(Value::as_str) != Some("oauth") {
                continue;
            }
            let id = connection.get("id").and_then(Value::as_str).unwrap_or("");
            let ping_enabled = enabled_map.get(id) == Some(&Value::Bool(true));
            if !ping_enabled && !tracker_enabled {
                continue;
            }
            process_connection(state, config, connection, ping_enabled, tracker_enabled).await;
        }
    }
}

/// Refreshes one connection, then runs the quota tracker (when enabled) and
/// the ping off the same usage read.
async fn process_connection(
    state: &AppState,
    config: &PingConfig,
    connection: Value,
    ping_enabled: bool,
    tracker_enabled: bool,
) {
    let id = connection
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let key = cache_key(config.provider, &id);
    let now = router_db::time::now_ms();

    if ping_enabled {
        // A cached reset still ahead of the refresh-ahead window means there is
        // nothing to look at yet.
        let cached_reset = RESET_CACHE.get(&key).map(|v| v.clone());
        if !config.ping_when_reset_at_slides
            && let Some(reset) = cached_reset.as_deref()
            && let Some(ms) = parse_time_ms(Some(&Value::String(reset.to_string())))
            && now < ms - REFRESH_AHEAD_MS
        {
            return;
        }
        if should_skip_after_failure(&key, now) {
            return;
        }
    }

    let proxy = proxy_options_for(state, &connection);
    let connection = match refresh_connection(state, &connection, &proxy, false).await {
        Ok(updated) => updated,
        Err(error) => {
            if ping_enabled {
                FAILURE_CACHE.insert(key, now);
            }
            tracing::warn!(
                "[AutoPing] {}:{id}: refresh failed: {error}",
                config.provider
            );
            return;
        }
    };

    let usage = match get_usage_for_provider(&connection, &proxy, false).await {
        Ok(usage) => usage,
        Err(error) => {
            if ping_enabled {
                FAILURE_CACHE.insert(key, now);
            }
            tracing::warn!("[AutoPing] {}:{id}: usage failed: {error}", config.provider);
            return;
        }
    };
    let quotas = usage.get("quotas").cloned().unwrap_or(Value::Null);

    if tracker_enabled {
        run_quota_tracker(state, &connection, &quotas).await;
    }

    if !ping_enabled {
        return;
    }

    let quota = quotas.get(config.quota_key).cloned().unwrap_or(Value::Null);
    let Some(reset_at) = quota
        .get("resetAt")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return;
    };
    let cached_reset = RESET_CACHE.insert(key.clone(), reset_at.clone());

    if config.skip_when_blocking_quota_exhausted
        && has_exhausted_blocking_quota(&quotas, config.quota_key)
    {
        return;
    }
    if router_db::repos::quota_tracker::is_quota_exhausted(Some(&quota)) {
        return;
    }

    if !should_ping_for_reset(
        config,
        cached_reset.as_deref(),
        &reset_at,
        router_db::time::now_ms(),
    ) {
        return;
    }
    if was_pinged_recently(
        &connection,
        config.min_ping_interval_ms,
        router_db::time::now_ms(),
    ) {
        return;
    }
    let reset_key = normalize_reset_key(&reset_at);
    let last_pinged_reset_key = connection
        .get("lastPingedResetKey")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            connection
                .get("lastPingedResetAt")
                .and_then(Value::as_str)
                .map(normalize_reset_key)
        });
    if last_pinged_reset_key.as_deref() == Some(reset_key.as_str()) {
        return;
    }

    let ok = send_codex_ping(&connection, config, proxy.clone()).await;
    if !ok {
        FAILURE_CACHE.insert(key, router_db::time::now_ms());
        tracing::warn!(
            "[AutoPing] {}:{id}: ping failed (reset {reset_at})",
            config.provider
        );
        return;
    }

    FAILURE_CACHE.remove(&key);
    let now_iso = router_db::time::now_iso();
    let update = json!({
        "lastPingedResetAt": reset_at,
        "lastPingedResetKey": reset_key,
        "lastPingAt": now_iso,
        "updatedAt": now_iso,
    });
    let provider = config.provider;
    let write_id = id.clone();
    let _ = state
        .write(move |tx| {
            router_db::repos::connections::update_provider_connection(tx, &write_id, &update)
        })
        .await;
    tracing::info!("[AutoPing] {provider}:{id}: ping sent (reset {reset_at})");
}

/// Apply the auto quota tracker to one connection, persisting only on a change.
async fn run_quota_tracker(state: &AppState, connection: &Value, quotas: &Value) {
    let mut candidate = connection.clone();
    let now_iso = router_db::time::now_iso();
    let action = router_db::repos::quota_tracker::apply_to_connection(
        &mut candidate,
        Some(quotas),
        &now_iso,
    );
    if action == router_db::repos::quota_tracker::QuotaAction::NoChange {
        return;
    }
    let _ = state
        .write(move |tx| router_db::repos::quota_tracker::persist(tx, &candidate, &now_iso))
        .await;
}

fn should_skip_after_failure(key: &str, now_ms: i64) -> bool {
    FAILURE_CACHE
        .get(key)
        .is_some_and(|failed_at| now_ms - *failed_at < FAILURE_COOLDOWN_MS)
}

/// Whether a quota other than the session one blocks pinging.
fn is_blocking_quota_name(name: &str, session_key: &str) -> bool {
    name != session_key && !name.to_lowercase().contains("session")
}

fn has_exhausted_blocking_quota(quotas: &Value, session_key: &str) -> bool {
    let Some(map) = quotas.as_object() else {
        return false;
    };
    map.iter().any(|(name, quota)| {
        is_blocking_quota_name(name, session_key)
            && router_db::repos::quota_tracker::is_quota_exhausted(Some(quota))
    })
}

/// Floors a reset time to the minute as canonical ISO. An unparseable value is
/// returned unchanged.
fn normalize_reset_key(reset_at: &str) -> String {
    match parse_time_ms(Some(&Value::String(reset_at.to_string()))) {
        Some(ms) => to_iso(ms.div_euclid(60_000) * 60_000),
        None => reset_at.to_string(),
    }
}

fn get_reset_drift_ms(previous_reset_at: &str, next_reset_at: &str) -> i64 {
    match (
        parse_time_ms(Some(&Value::String(previous_reset_at.to_string()))),
        parse_time_ms(Some(&Value::String(next_reset_at.to_string()))),
    ) {
        (Some(previous), Some(next)) => next - previous,
        _ => 0,
    }
}

fn should_ping_for_reset(
    config: &PingConfig,
    cached_reset: Option<&str>,
    reset_at: &str,
    now: i64,
) -> bool {
    if config.ping_when_reset_at_slides {
        return cached_reset.is_some_and(|previous| {
            get_reset_drift_ms(previous, reset_at) >= config.reset_at_drift_ms
        });
    }
    parse_time_ms(Some(&Value::String(reset_at.to_string())))
        .is_some_and(|ms| now >= ms - PING_LEAD_MS)
}

fn was_pinged_recently(connection: &Value, interval_ms: i64, now_ms: i64) -> bool {
    if interval_ms == 0 {
        return false;
    }
    connection
        .get("lastPingAt")
        .and_then(Value::as_str)
        .and_then(|s| parse_time_ms(Some(&Value::String(s.to_string()))))
        .is_some_and(|last| now_ms - last < interval_ms)
}

/// A minimal streaming Codex request, used only to warm the 5-hour window.
/// Codex starts the window once the streaming response completes, so the body
/// has to be drained.
async fn send_codex_ping(connection: &Value, config: &PingConfig, proxy: ProxyOptions) -> bool {
    let credentials = Credentials {
        access_token: connection
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_string),
        connection_id: connection
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        provider_specific_data: connection
            .get("providerSpecificData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        ..Credentials::default()
    };

    let mut body = json!({
        "model": config.ping_model,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": config.ping_text }],
        }],
        "store": false,
        "stream": true,
    });
    if let Some(instructions) = config.ping_instructions {
        body["instructions"] = json!(instructions);
    }
    if let Some(effort) = config.ping_reasoning_effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }

    let executor = get_executor("codex");
    let request = ExecuteRequest::new(config.ping_model, body, true, &credentials, proxy);
    match executor.execute(request).await {
        Ok(response) if (200..300).contains(&response.status) => {
            let _ = response.text().await;
            true
        }
        Ok(mut response) => {
            // Drop the body without reading it.
            let _ = response.take_body();
            false
        }
        Err(_) => false,
    }
}

// ─── shared helpers ───────────────────────────────────────────────────────

/// Resolves the connection's proxy settings into `ProxyOptions`.
fn proxy_options_for(state: &AppState, connection: &Value) -> ProxyOptions {
    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object);
    let resolved = resolve_connection_proxy_config(&state.db, psd);
    ProxyOptions::from_value(Some(&json!({
        "connectionProxyEnabled": resolved.connection_proxy_enabled,
        "connectionProxyUrl": resolved.connection_proxy_url,
        "connectionNoProxy": resolved.connection_no_proxy,
        "vercelRelayUrl": resolved.vercel_relay_url,
        "strictProxy": false,
    })))
}

/// Refreshes a connection and returns the one a caller should use next. The
/// connection is re-read after the write so the caller sees the new token.
async fn refresh_connection(
    state: &AppState,
    connection: &Value,
    proxy: &ProxyOptions,
    force: bool,
) -> Result<Value, String> {
    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let credentials = credentials_from_connection(connection);
    let outcome = check_and_refresh_token(&provider, &credentials, proxy, force).await;

    if let Some(patch) = outcome.patch {
        let connection_id = credentials.connection_id.clone().unwrap_or_default();
        if !connection_id.is_empty() {
            let db = state.db.clone();
            let patch_for_write = patch.clone();
            let _ = tokio::task::spawn_blocking(move || {
                update_provider_credentials(&db, &connection_id, &patch_for_write)
            })
            .await;
            let id = connection
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Ok(Some(updated)) = state
                .read(move |conn| {
                    router_db::repos::connections::get_provider_connection_by_id(conn, &id)
                })
                .await
            {
                return Ok(updated);
            }
        }
        return Ok(connection.clone());
    }

    // A refresh that produced nothing, on a connection with no access token to
    // fall back on, is the "please re-authorize" case.
    let has_access_token = connection
        .get("accessToken")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !has_access_token {
        return Err(
            "Failed to refresh credentials. Please re-authorize the connection.".to_string(),
        );
    }
    Ok(connection.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_key_floors_to_the_minute_and_keeps_unparseable_values() {
        assert_eq!(
            normalize_reset_key("2026-01-02T03:04:37.123Z"),
            "2026-01-02T03:04:00.000Z"
        );
        assert_eq!(normalize_reset_key("not a date"), "not a date");
    }

    #[test]
    fn drift_is_the_next_minus_previous() {
        assert_eq!(
            get_reset_drift_ms("2026-01-02T03:00:00.000Z", "2026-01-02T03:00:45.000Z"),
            45_000
        );
        assert_eq!(get_reset_drift_ms("bad", "2026-01-02T03:00:45.000Z"), 0);
    }

    #[test]
    fn sliding_reset_pings_only_past_the_drift_threshold() {
        let config = &PING_CONFIGS[0];
        assert!(config.ping_when_reset_at_slides);
        assert!(should_ping_for_reset(
            config,
            Some("2026-01-02T03:00:00.000Z"),
            "2026-01-02T03:00:31.000Z",
            0
        ));
        assert!(!should_ping_for_reset(
            config,
            Some("2026-01-02T03:00:00.000Z"),
            "2026-01-02T03:00:29.000Z",
            0
        ));
        assert!(!should_ping_for_reset(
            config,
            None,
            "2026-01-02T03:00:31.000Z",
            0
        ));
    }

    #[test]
    fn blocking_quota_names_exclude_the_session_window() {
        assert!(!is_blocking_quota_name("session", "session"));
        assert!(!is_blocking_quota_name("session (5h)", "session"));
        assert!(is_blocking_quota_name("weekly", "session"));
    }

    #[test]
    fn ping_enabled_needs_a_truthy_connection_flag() {
        let settings = json!({ "codexAutoPing": { "connections": { "a": true, "b": false } } });
        assert!(has_quota_auto_ping_enabled(&settings));
        let none = json!({ "codexAutoPing": { "connections": { "b": false } } });
        assert!(!has_quota_auto_ping_enabled(&none));
    }
}
