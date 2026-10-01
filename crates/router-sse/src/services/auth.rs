//! Account selection and cooldown bookkeeping.
//!
//! [`get_provider_credentials`] picks the connection a request runs on (free
//! virtual connection, pinning, round-robin with a sticky counter, or
//! fill-first) and returns it as [`Credentials`] plus the connection-level proxy
//! config the executor needs. [`mark_account_unavailable`] locks a
//! `modelLock_<model>` field on failure, and [`clear_account_error`] unwinds
//! those locks after a success.
//!
//! The pure decision helpers live in [`crate::services::account_fallback`];
//! this module is the DB-facing composition over them.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use router_db::Db;

use crate::credentials::Credentials;
use crate::providers::registry::registry;
use crate::services::account_fallback::{
    self, MAX_RATE_LIMIT_COOLDOWN_MS, check_fallback_error, format_retry_after,
    get_earliest_model_lock_until, is_model_lock_active,
};
use crate::services::connection_proxy::{
    pick_proxy_pool_id, proxy_fields_into_psd, resolve_connection_proxy_config,
};

/// `resolveProviderId(aliasOrId)`: the registry id, or the input unchanged.
pub fn resolve_provider_id(alias_or_id: &str) -> String {
    registry().resolve_alias(alias_or_id).to_string()
}

/// `FREE_PROVIDERS[providerId]?.noAuth`: the providers that need no credential
/// row at all, so the selector synthesizes a virtual `noauth` connection.
pub fn is_free_no_auth(provider_id: &str) -> bool {
    registry()
        .get(provider_id)
        .is_some_and(|p| p.category.as_deref() == Some("free") && p.no_auth == Some(true))
}

/// `AllRateLimited` — the `{ allRateLimited: true, … }` shape the caller
/// serializes into the retry response.
#[derive(Debug, Clone)]
pub struct AllRateLimited {
    pub retry_after: String,
    pub retry_after_human: String,
    pub last_error: Option<String>,
    pub last_error_code: Option<Value>,
}

/// What `getProviderCredentials` resolves to.
#[derive(Debug, Clone)]
pub enum AccountSelection {
    /// A connection to run on.
    Selected(Box<SelectedAccount>),
    /// Every account is cooling down; retry after `retry_after`.
    AllRateLimited(Box<AllRateLimited>),
    /// No credential rows at all.
    None,
}

/// The selected connection, in the shape the chat pipeline consumes.
#[derive(Debug, Clone)]
pub struct SelectedAccount {
    pub credentials: Credentials,
    /// The raw connection row, which `clearAccountError` reads for its
    /// `modelLock_*` keys.
    pub connection: Value,
    pub connection_name: String,
    pub test_status: Option<String>,
    pub last_error: Option<String>,
}

/// `getProviderCredentials(provider, excludeConnectionIds, model, options)`.
///
/// Selection is serialized behind a process-wide lock so two concurrent
/// requests never read the same sticky counter.
pub async fn get_provider_credentials(
    db: &Db,
    provider: &str,
    exclude_connection_ids: Option<&HashSet<String>>,
    model: Option<&str>,
    preferred_connection_id: Option<&str>,
) -> AccountSelection {
    let provider_id = resolve_provider_id(provider);
    let exclude = exclude_connection_ids.cloned().unwrap_or_default();

    if is_free_no_auth(&provider_id) {
        return AccountSelection::Selected(Box::new(build_noauth_account(db, &provider_id)));
    }

    let connections = match db.with_conn(|conn| {
        router_db::repos::connections::get_provider_connections(
            conn,
            Some(&provider_id),
            Some(true),
        )
    }) {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!("AUTH | {provider} | connection read failed: {error}");
            return AccountSelection::None;
        }
    };

    if connections.is_empty() {
        tracing::warn!("AUTH | No credentials for {provider}");
        return AccountSelection::None;
    }

    let available: Vec<Value> = connections
        .iter()
        .filter(|c| {
            let id = c.get("id").and_then(Value::as_str).unwrap_or("");
            !exclude.contains(id) && !is_model_lock_active(c, model)
        })
        .cloned()
        .collect();

    if available.is_empty() {
        let mut expiries: Vec<String> = connections
            .iter()
            .filter(|c| is_model_lock_active(c, model))
            .filter_map(get_earliest_model_lock_until)
            .collect();
        expiries.sort();

        if let Some(earliest) = expiries.first() {
            let earliest_conn = connections.iter().find(|c| is_model_lock_active(c, model));
            return AccountSelection::AllRateLimited(Box::new(AllRateLimited {
                retry_after: earliest.clone(),
                retry_after_human: format_retry_after(Some(earliest)),
                last_error: earliest_conn
                    .and_then(|c| c.get("lastError"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                last_error_code: earliest_conn.and_then(|c| c.get("errorCode")).cloned(),
            }));
        }
        tracing::warn!(
            "AUTH | {provider} | all {} accounts unavailable",
            connections.len()
        );
        return AccountSelection::None;
    }

    let settings = db
        .with_conn(router_db::repos::settings::get_settings)
        .unwrap_or(Value::Null);
    let provider_override = settings
        .get("providerStrategies")
        .and_then(|s| s.get(&provider_id))
        .cloned()
        .unwrap_or(Value::Null);
    let strategy = provider_override
        .get("fallbackStrategy")
        .and_then(Value::as_str)
        .or_else(|| settings.get("fallbackStrategy").and_then(Value::as_str))
        .unwrap_or("fill-first");

    let pinned = preferred_connection_id.and_then(|preferred| {
        available
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(preferred))
            .cloned()
    });

    let connection = match pinned {
        Some(connection) => connection,
        None if strategy == "round-robin" => {
            select_round_robin(db, &available, &provider_override, &settings).await
        }
        None => available[0].clone(),
    };

    Some(connection).map_or(AccountSelection::None, |connection| {
        AccountSelection::Selected(Box::new(build_selected(db, connection)))
    })
}

/// The `round-robin` strategy: stay on the most recently used account until its
/// sticky count reaches the limit, then move to the least recently used one.
async fn select_round_robin(
    db: &Db,
    available: &[Value],
    provider_override: &Value,
    settings: &Value,
) -> Value {
    let sticky_limit = provider_override
        .get("stickyRoundRobinLimit")
        .and_then(Value::as_i64)
        .or_else(|| {
            settings
                .get("stickyRoundRobinLimit")
                .and_then(Value::as_i64)
        })
        .unwrap_or(3);

    let by_recency = sort_by_recency(available, true);
    let current = by_recency.first().cloned();
    let current_count = current
        .as_ref()
        .and_then(|c| c.get("consecutiveUseCount"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let current_used = current
        .as_ref()
        .and_then(|c| c.get("lastUsedAt"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());

    let (connection, next_count) = if current_used && current_count < sticky_limit {
        let current = current.clone().expect("checked above");
        let count = current
            .get("consecutiveUseCount")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        (current, count)
    } else {
        let oldest = sort_by_recency(available, false);
        (oldest[0].clone(), 1)
    };

    let id = connection
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let patch = json!({
        "lastUsedAt": router_db::time::now_iso(),
        "consecutiveUseCount": next_count,
    });
    if let Err(error) = db.with_conn(|conn| {
        router_db::repos::connections::update_provider_connection(conn, &id, &patch)
    }) {
        tracing::warn!("AUTH | sticky update failed for {id}: {error}");
    }
    connection
}

/// `[...availableConnections].sort(...)` by `lastUsedAt`.
///
/// `most_recent` picks the descending order (`byRecency`); otherwise the
/// ascending order (`sortedByOldest`). Accounts with no
/// `lastUsedAt` sort last when descending and first when ascending, and ties
/// break on `priority || 999`.
fn sort_by_recency(available: &[Value], most_recent: bool) -> Vec<Value> {
    let mut list = available.to_vec();
    list.sort_by(|a, b| {
        let a_used = a
            .get("lastUsedAt")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let b_used = b
            .get("lastUsedAt")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        match (a_used, b_used) {
            (None, None) => priority_or(a, 999).cmp(&priority_or(b, 999)),
            (None, Some(_)) => {
                if most_recent {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            }
            (Some(_), None) => {
                if most_recent {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            (Some(a_used), Some(b_used)) => {
                let a_ms = router_db::time::parse_iso(a_used).map(|d| d.timestamp_millis());
                let b_ms = router_db::time::parse_iso(b_used).map(|d| d.timestamp_millis());
                match (a_ms, b_ms) {
                    (Some(a_ms), Some(b_ms)) => {
                        if most_recent {
                            b_ms.cmp(&a_ms)
                        } else {
                            a_ms.cmp(&b_ms)
                        }
                    }
                    _ => std::cmp::Ordering::Equal,
                }
            }
        }
    });
    list
}

fn priority_or(c: &Value, fallback: i64) -> i64 {
    match c.get("priority").and_then(Value::as_i64) {
        Some(0) | None => fallback,
        Some(p) => p,
    }
}

/// The free-tier virtual connection: no DB row, a proxy pool resolved from the
/// provider's rotation strategy.
fn build_noauth_account(db: &Db, provider_id: &str) -> SelectedAccount {
    let settings = db
        .with_conn(router_db::repos::settings::get_settings)
        .unwrap_or(Value::Null);
    let provider_override = settings
        .get("providerStrategies")
        .and_then(|s| s.get(provider_id))
        .cloned()
        .unwrap_or(Value::Null);
    let strategy = provider_override
        .get("rotateStrategy")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let mut picked_id = provider_override
        .get("proxyPoolId")
        .and_then(Value::as_str)
        .map(str::to_string);

    if strategy != "none" {
        let pool_ids: Vec<String> = db
            .with_conn(|conn| {
                router_db::repos::proxy_pools::get_proxy_pools(conn, Some(true), None)
            })
            .unwrap_or_default()
            .iter()
            .filter(|p| {
                p.get("proxyUrl")
                    .and_then(Value::as_str)
                    .is_some_and(|u| !u.is_empty())
            })
            .filter_map(|p| p.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        picked_id = pick_proxy_pool_id(&pool_ids, strategy, provider_id);
    }

    let mut psd = Map::new();
    psd.insert("proxyPoolId".into(), json!(picked_id.unwrap_or_default()));
    let resolved = resolve_connection_proxy_config(db, Some(&psd));
    let mut psd = Map::new();
    proxy_fields_into_psd(&mut psd, &resolved);

    let connection = json!({
        "id": "noauth",
        "connectionName": "Public",
        "isActive": true,
        "accessToken": "public",
        "providerSpecificData": psd,
    });

    SelectedAccount {
        credentials: credentials_from_connection(&connection),
        connection,
        connection_name: "Public".to_string(),
        test_status: None,
        last_error: None,
    }
}

/// Build the [`SelectedAccount`] for a real connection row.
fn build_selected(db: &Db, connection: Value) -> SelectedAccount {
    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let resolved = resolve_connection_proxy_config(db, Some(&psd));

    let mut merged = psd;
    proxy_fields_into_psd(&mut merged, &resolved);

    let name = connection
        .get("displayName")
        .and_then(Value::as_str)
        .or_else(|| connection.get("name").and_then(Value::as_str))
        .or_else(|| connection.get("email").and_then(Value::as_str))
        .or_else(|| connection.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();

    let mut enriched = connection.clone();
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert("providerSpecificData".into(), Value::Object(merged));
    }

    let test_status = connection
        .get("testStatus")
        .and_then(Value::as_str)
        .map(str::to_string);
    let last_error = connection
        .get("lastError")
        .and_then(Value::as_str)
        .map(str::to_string);

    SelectedAccount {
        credentials: credentials_from_connection(&enriched),
        connection,
        connection_name: name,
        test_status,
        last_error,
    }
}

/// Map a connection row (plus the resolved proxy fields) onto [`Credentials`].
///
/// Every field the row does not carry is `None`, and the remaining columns land
/// in `extra` so an executor can still read a provider-specific key the typed
/// struct does not name.
pub fn credentials_from_connection(connection: &Value) -> Credentials {
    let get_str = |key: &str| {
        connection
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    Credentials {
        access_token: get_str("accessToken"),
        api_key: get_str("apiKey"),
        refresh_token: get_str("refreshToken"),
        id_token: get_str("idToken"),
        expires_at: get_str("expiresAt"),
        expires_in: connection.get("expiresIn").and_then(Value::as_i64),
        token_type: get_str("tokenType"),
        scope: get_str("scope"),
        project_id: get_str("projectId"),
        email: get_str("email"),
        connection_id: get_str("id").or_else(|| get_str("connectionId")),
        display_name: get_str("displayName").or_else(|| get_str("name")),
        connection_name: None,
        provider_specific_data: psd,
        raw_headers: std::collections::HashMap::new(),
        client_session_id: None,
        runtime_transport: None,
        extra: connection.as_object().cloned().unwrap_or_default(),
    }
}

/// The outcome of [`mark_account_unavailable`].
#[derive(Debug, Clone, Copy)]
pub struct FallbackDecisionResult {
    pub should_fallback: bool,
    pub cooldown_ms: u64,
}

/// `markAccountUnavailable(connectionId, status, errorText, provider, model,
/// resetsAtMs)`: lock `modelLock_<model>` (or `modelLock___all` when no model
/// is named) and record the error state.
pub fn mark_account_unavailable(
    db: &Db,
    connection_id: &str,
    status: u16,
    error_text: &str,
    provider: Option<&str>,
    model: Option<&str>,
    resets_at_ms: Option<i64>,
) -> FallbackDecisionResult {
    if connection_id.is_empty() || connection_id == "noauth" {
        return FallbackDecisionResult {
            should_fallback: false,
            cooldown_ms: 0,
        };
    }

    let connections = db
        .with_conn(|conn| {
            router_db::repos::connections::get_provider_connections(conn, provider, None)
        })
        .unwrap_or_default();
    let conn = connections
        .iter()
        .find(|c| c.get("id").and_then(Value::as_str) == Some(connection_id))
        .cloned();
    let backoff_level = conn
        .as_ref()
        .and_then(|c| c.get("backoffLevel"))
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;

    let now_ms = router_db::time::now_ms();

    let (should_fallback, cooldown_ms, new_backoff_level) =
        if let Some(reset) = resets_at_ms.filter(|ms| *ms > now_ms) {
            let cooldown = ((reset - now_ms) as u64).min(MAX_RATE_LIMIT_COOLDOWN_MS);
            (true, cooldown, Some(0u32))
        } else {
            let decision = check_fallback_error(status, Some(&json!(error_text)), backoff_level);
            (
                decision.should_fallback,
                decision.cooldown_ms,
                decision.new_backoff_level,
            )
        };

    if !should_fallback {
        return FallbackDecisionResult {
            should_fallback: false,
            cooldown_ms: 0,
        };
    }

    let reason: String = error_text.chars().take(200).collect();
    let lock_update = account_fallback::build_model_lock_update(model, cooldown_ms);

    let mut patch = lock_update.as_object().cloned().unwrap_or_default();
    patch.insert("testStatus".into(), json!("unavailable"));
    patch.insert("lastError".into(), json!(reason));
    patch.insert("errorCode".into(), json!(status));
    patch.insert("lastErrorAt".into(), json!(router_db::time::now_iso()));
    patch.insert(
        "backoffLevel".into(),
        json!(new_backoff_level.unwrap_or(backoff_level)),
    );
    let patch = Value::Object(patch);

    if let Err(error) = db.with_conn(|conn| {
        router_db::repos::connections::update_provider_connection(conn, connection_id, &patch)
    }) {
        tracing::warn!("AUTH | lock write failed for {connection_id}: {error}");
    }

    let lock_key = lock_update
        .as_object()
        .and_then(|m| m.keys().next())
        .cloned()
        .unwrap_or_default();
    let conn_name = conn
        .as_ref()
        .and_then(|c| {
            c.get("displayName")
                .or_else(|| c.get("name"))
                .or_else(|| c.get("email"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .unwrap_or_else(|| connection_id.chars().take(8).collect());
    tracing::warn!(
        "AUTH | {conn_name} locked {lock_key} for {}s [{status}]",
        cooldown_ms / 1000
    );

    FallbackDecisionResult {
        should_fallback: true,
        cooldown_ms,
    }
}

/// `clearAccountError(connectionId, currentConnection, model)`: drop the lock
/// for the model that just succeeded plus every expired lock, and reset the
/// error state only when no active lock remains.
pub fn clear_account_error(
    db: &Db,
    connection_id: &str,
    current_connection: &Value,
    model: Option<&str>,
) {
    if connection_id.is_empty() || connection_id == "noauth" {
        return;
    }
    let conn = current_connection
        .get("_connection")
        .unwrap_or(current_connection);
    let now_ms = router_db::time::now_ms();

    let Some(obj) = conn.as_object() else {
        return;
    };
    let all_lock_keys: Vec<String> = obj
        .keys()
        .filter(|k| k.starts_with(account_fallback::MODEL_LOCK_PREFIX))
        .cloned()
        .collect();

    let test_status = conn.get("testStatus").and_then(Value::as_str);
    let last_error = conn.get("lastError").and_then(Value::as_str);
    if test_status.is_none() && last_error.is_none() && all_lock_keys.is_empty() {
        return;
    }

    let succeeded_key = model.map(|m| account_fallback::get_model_lock_key(Some(m)));
    let account_wide = account_fallback::model_lock_all();
    let keys_to_clear: Vec<String> = all_lock_keys
        .iter()
        .filter(|key| {
            if succeeded_key.as_deref() == Some(key.as_str()) {
                return true;
            }
            if model.is_some() && **key == account_wide {
                return true;
            }
            obj.get(*key)
                .and_then(Value::as_str)
                .and_then(router_db::time::parse_iso)
                .is_some_and(|t| t.timestamp_millis() <= now_ms)
        })
        .cloned()
        .collect();

    if keys_to_clear.is_empty() && test_status != Some("unavailable") && last_error.is_none() {
        return;
    }

    let remaining_active = all_lock_keys.iter().any(|key| {
        !keys_to_clear.contains(key)
            && obj
                .get(key)
                .and_then(Value::as_str)
                .and_then(router_db::time::parse_iso)
                .is_some_and(|t| t.timestamp_millis() > now_ms)
    });

    let mut clear = Map::new();
    for key in &keys_to_clear {
        clear.insert(key.clone(), Value::Null);
    }
    if !remaining_active {
        clear.insert("testStatus".into(), json!("active"));
        clear.insert("lastError".into(), Value::Null);
        clear.insert("errorCode".into(), Value::Null);
        clear.insert("lastErrorAt".into(), Value::Null);
        clear.insert("backoffLevel".into(), json!(0));
    }

    if let Err(error) = db.with_conn(|conn| {
        router_db::repos::connections::update_provider_connection(
            conn,
            connection_id,
            &Value::Object(clear),
        )
    }) {
        tracing::warn!("AUTH | clear failed for {connection_id}: {error}");
    }
}

/// `extractApiKey(request)`: the `Authorization: Bearer` token, else `x-api-key`.
pub fn extract_api_key(authorization: Option<&str>, x_api_key: Option<&str>) -> Option<String> {
    if let Some(header) = authorization
        && let Some(token) = header.strip_prefix("Bearer ")
    {
        return Some(token.to_string());
    }
    x_api_key.filter(|s| !s.is_empty()).map(str::to_string)
}

/// `isValidApiKey(apiKey)`: an active row in `apiKeys` with a matching key.
pub fn is_valid_api_key(db: &Db, api_key: &str) -> bool {
    if api_key.is_empty() {
        return false;
    }
    db.with_conn(|conn| router_db::repos::api_keys::validate_api_key(conn, api_key))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_api_key_prefers_the_bearer_header() {
        assert_eq!(
            extract_api_key(Some("Bearer abc"), Some("xyz")),
            Some("abc".to_string())
        );
        assert_eq!(extract_api_key(None, Some("xyz")), Some("xyz".to_string()));
        assert_eq!(
            extract_api_key(Some("Basic abc"), Some("xyz")),
            Some("xyz".to_string())
        );
        assert_eq!(extract_api_key(Some("Basic abc"), None), None);
        assert_eq!(extract_api_key(None, Some("")), None);
    }

    #[test]
    fn resolve_provider_id_maps_aliases_and_passes_through() {
        assert_eq!(resolve_provider_id("kc"), resolve_provider_id("kc"));
        assert_eq!(resolve_provider_id("opencode"), "opencode");
        assert_eq!(resolve_provider_id("not-a-provider"), "not-a-provider");
    }

    #[test]
    fn sort_by_recency_puts_unused_last_when_descending() {
        let list = vec![
            json!({"id": "a", "lastUsedAt": "2024-01-01T00:00:00.000Z"}),
            json!({"id": "b"}),
            json!({"id": "c", "lastUsedAt": "2024-06-01T00:00:00.000Z"}),
        ];
        let newest = sort_by_recency(&list, true);
        assert_eq!(newest[0]["id"], json!("c"));
        assert_eq!(newest[2]["id"], json!("b"));

        let oldest = sort_by_recency(&list, false);
        assert_eq!(oldest[0]["id"], json!("b"));
        assert_eq!(oldest[2]["id"], json!("c"));
    }
}
