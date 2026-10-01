//! Proactive token refresh: the per-provider refreshers for the providers in
//! the registry, behind a single-flight lock.
//!
//! Two behaviours here are load-bearing rather than incidental:
//!
//! * **Dedup is not an optimisation.** xAI and Codex rotate the refresh token on
//!   every use. Two concurrent requests refreshing the same connection with the
//!   same old token means the loser gets `invalid_grant` and the account gets
//!   marked dead. `single_flight` collapses them into one call.
//! * **The retry loop hands each attempt the credentials the previous one
//!   produced.** Reusing a consumed refresh token on attempt 2 fails with
//!   `invalid_grant`, so the rotating fields are written back before the next
//!   attempt.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use router_db::Db;

use crate::credentials::Credentials;
use crate::executors::default::{post_form, post_json_with_headers};
use crate::executors::http::ProxyOptions;
use crate::executors::oauth::{
    RefreshedCredentials, classify_oauth_refresh_error, merge_refreshed_credentials,
    should_refresh_credentials,
};
use crate::providers::registry::registry;
use crate::services::single_flight::{Slots, single_flight};

/// `REFRESH_RESULT_TTL_MS`.
const REFRESH_RESULT_TTL_MS: Duration = Duration::from_secs(10);

// ─── single-flight ────────────────────────────────────────────────────────

/// `dedupRefresh(provider, oldToken, fn)`.
static DEDUP: LazyLock<Slots<Option<RefreshedCredentials>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn dedup_refresh<F, Fut>(
    provider: &str,
    old_token: &str,
    f: F,
) -> Option<RefreshedCredentials>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Option<RefreshedCredentials>>,
{
    if old_token.is_empty() {
        return f().await;
    }
    single_flight(
        &DEDUP,
        format!("{provider}:{old_token}"),
        |_| Some(REFRESH_RESULT_TTL_MS),
        f,
    )
    .await
}

/// `withCredentialRefreshLock(provider, credentials, refreshFn)`.
///
/// The lock key is the connection, not the token: two requests for the same
/// account must not both refresh even when they arrive with different tokens.
static REFRESH_LOCKS: LazyLock<Slots<Option<Value>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// `getRefreshLockKey(provider, credentials)`.
fn refresh_lock_key(provider: &str, credentials: &Credentials) -> String {
    let stable = credentials
        .connection_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| credentials.extra.get("id").and_then(Value::as_str))
        .or_else(|| credentials.email.as_deref().filter(|s| !s.is_empty()))
        .or_else(|| {
            credentials
                .display_name
                .as_deref()
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
        .or_else(|| {
            credentials
                .refresh_token
                .as_deref()
                .filter(|t| !t.is_empty())
                .map(|t| {
                    t.chars()
                        .rev()
                        .take(16)
                        .collect::<String>()
                        .chars()
                        .rev()
                        .collect()
                })
        })
        .unwrap_or_else(|| "default".to_string());
    format!("{provider}:{stable}")
}

// ─── refresh_with_retry ───────────────────────────────────────────────────

/// `refreshWithRetry(refreshFn, maxRetries, log)`.
///
/// `credentials` is mutated between attempts: a rotating provider issues a new
/// refresh token, and the next attempt has to use it.
pub async fn refresh_with_retry<F, Fut>(
    credentials: &mut Credentials,
    max_retries: u32,
    mut refresh_fn: F,
) -> Option<RefreshedCredentials>
where
    F: FnMut(&Credentials) -> Fut,
    Fut: Future<Output = Option<RefreshedCredentials>>,
{
    for attempt in 0..max_retries {
        if attempt > 0 {
            let delay = u64::from(attempt) * 1000;
            tracing::debug!("[TOKEN_REFRESH] retry {attempt}/{max_retries} after {delay}ms");
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let result = refresh_fn(credentials).await;
        if let Some(result) = result {
            if let Some(new_refresh) = &result.refresh_token
                && Some(new_refresh.as_str()) != credentials.refresh_token.as_deref()
            {
                if let Some(access) = &result.access_token {
                    credentials.access_token = Some(access.clone());
                }
                credentials.refresh_token = Some(new_refresh.clone());
            }
            return Some(result);
        }
    }

    tracing::error!("[TOKEN_REFRESH] all {max_retries} retry attempts failed");
    None
}

// ─── per-provider refreshers ──────────────────────────────────────────────

/// `refreshCodexToken`: JSON body, and a permanent failure is reported as
/// `unrecoverable_refresh_error` so the caller can stop retrying and ask the
/// user to re-authenticate.
async fn refresh_codex(
    refresh_token: &str,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    let oauth = registry().oauth("codex")?;
    let client_id = oauth.get("clientId").and_then(Value::as_str)?;
    let url = oauth.get("tokenUrl").and_then(Value::as_str)?;

    dedup_refresh("codex", refresh_token, || async {
        let body = json!({
            "client_id": client_id,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        });
        let target = crate::executors::http::prepare_send(url, proxy_options)
            .await
            .ok()?;
        let response = target
            .client
            .post(&target.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .ok()?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let failure = classify_oauth_refresh_error(&text, status);
            if failure.get("permanent").and_then(Value::as_bool) == Some(true) {
                tracing::error!(
                    "[TOKEN_REFRESH] codex refresh token already used or invalid; re-auth required"
                );
                return Some(RefreshedCredentials {
                    error: Some("unrecoverable_refresh_error".to_string()),
                    ..Default::default()
                });
            }
            return None;
        }

        let tokens: Value = response.json().await.ok()?;
        Some(RefreshedCredentials {
            access_token: tokens
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            refresh_token: Some(
                tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(refresh_token)
                    .to_string(),
            ),
            id_token: tokens
                .get("id_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            expires_in: tokens.get("expires_in").and_then(Value::as_i64),
            ..Default::default()
        })
    })
    .await
}

/// `refreshXaiToken`, shared by `grok-cli` and its `gcli` alias.
async fn refresh_xai(
    refresh_token: &str,
    provider: &str,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    let oauth = registry().oauth(provider)?;
    let client_id = oauth.get("clientId").and_then(Value::as_str)?.to_string();
    let url = oauth
        .get("refreshUrl")
        .and_then(Value::as_str)
        .or_else(|| oauth.get("tokenUrl").and_then(Value::as_str))?;

    dedup_refresh("xai", refresh_token, || async {
        let params = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("client_id".to_string(), client_id.clone()),
            ("refresh_token".to_string(), refresh_token.to_string()),
        ];
        let tokens = post_form(url, &params, &[], proxy_options).await?;
        Some(RefreshedCredentials {
            access_token: tokens
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            refresh_token: Some(
                tokens
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(refresh_token)
                    .to_string(),
            ),
            id_token: tokens
                .get("id_token")
                .and_then(Value::as_str)
                .map(str::to_string),
            expires_in: tokens.get("expires_in").and_then(Value::as_i64),
            ..Default::default()
        })
    })
    .await
}

/// `refreshCodebuddyIntlToken`: the refresh token rides in a header, and the
/// payload is `{code, data}`.
async fn refresh_codebuddy_intl(
    refresh_token: &str,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    let oauth = registry().oauth("codebuddy-intl")?;
    let url = oauth.get("refreshUrl").and_then(Value::as_str)?;
    let user_agent = oauth
        .get("userAgent")
        .and_then(Value::as_str)
        .unwrap_or("IDE/2.63.2 CodeBuddy/2.63.2");

    dedup_refresh("codebuddy-intl", refresh_token, || async {
        let headers = vec![
            ("Content-Type", "application/json".to_string()),
            ("Accept", "application/json".to_string()),
            ("User-Agent", user_agent.to_string()),
            ("X-Requested-With", "XMLHttpRequest".to_string()),
            ("X-Domain", "www.codebuddy.ai".to_string()),
            ("X-Refresh-Token", refresh_token.to_string()),
            ("X-Auth-Refresh-Source", "plugin".to_string()),
            ("X-Product", "SaaS".to_string()),
        ];
        let payload = post_json_with_headers(url, &json!({}), &headers, proxy_options).await?;
        let data = payload.get("data")?;
        let access_token = data.get("accessToken").and_then(Value::as_str)?.to_string();

        Some(RefreshedCredentials {
            access_token: Some(access_token),
            refresh_token: Some(
                data.get("refreshToken")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(refresh_token)
                    .to_string(),
            ),
            expires_in: data.get("expiresIn").and_then(Value::as_i64),
            ..Default::default()
        })
    })
    .await
}

/// `refreshTokenByProvider(provider, credentials)`.
///
/// Providers with no entry here have no refresh token to rotate: `kilocode` is
/// device-code only.
pub async fn refresh_token_by_provider(
    provider: &str,
    credentials: &Credentials,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    let refresh_token = credentials
        .refresh_token
        .as_deref()
        .filter(|t| !t.is_empty())?;
    match provider {
        "codex" => refresh_codex(refresh_token, proxy_options).await,
        "grok-cli" | "gcli" => refresh_xai(refresh_token, "grok-cli", proxy_options).await,
        "codebuddy-intl" => refresh_codebuddy_intl(refresh_token, proxy_options).await,
        _ => None,
    }
}

/// `refreshProviderCredentials(provider, credentials, log)`.
///
/// Returns the sparse camelCase patch the caller persists, or `None` when the
/// refresh produced nothing. `Some({error})` is the unrecoverable case.
pub async fn refresh_provider_credentials(
    provider: &str,
    credentials: &Credentials,
    proxy_options: &ProxyOptions,
) -> Option<Value> {
    let key = refresh_lock_key(provider, credentials);
    single_flight(
        &REFRESH_LOCKS,
        key,
        |_| None,
        || async {
            let refreshed = refresh_token_by_provider(provider, credentials, proxy_options).await?;
            merge_refreshed_credentials(
                provider,
                credentials,
                &refreshed,
                router_db::time::now_ms(),
            )
        },
    )
    .await
}

// ─── the request-path entry point ─────────────────────────────────────────

/// What `checkAndRefreshToken` hands back: the credentials the request should
/// use, and the patch the caller should persist (already applied to
/// `credentials`).
#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub credentials: Credentials,
    /// `None` when nothing changed, so the caller can skip the write.
    pub patch: Option<Value>,
}

/// Apply a camelCase refresh patch onto a `Credentials`.
pub fn apply_refresh_patch(credentials: &mut Credentials, patch: &Value) {
    let Some(obj) = patch.as_object() else {
        return;
    };
    let string = |key: &str| {
        obj.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    if let Some(v) = string("accessToken") {
        credentials.access_token = Some(v);
    }
    if let Some(v) = string("refreshToken") {
        credentials.refresh_token = Some(v);
    }
    if let Some(v) = string("idToken") {
        credentials.id_token = Some(v);
    }
    if let Some(v) = string("expiresAt") {
        credentials.expires_at = Some(v);
    }
    if let Some(v) = obj.get("expiresIn").and_then(Value::as_i64) {
        credentials.expires_in = Some(v);
    }
    if let Some(v) = string("projectId") {
        credentials.project_id = Some(v);
    }
    if let Some(Value::Object(psd)) = obj.get("providerSpecificData") {
        for (k, v) in psd {
            credentials
                .provider_specific_data
                .insert(k.clone(), v.clone());
        }
    }
    if let Some(v) = obj.get("lastRefreshAt").cloned() {
        credentials.extra.insert("lastRefreshAt".into(), v);
    }
}

/// `checkAndRefreshToken(provider, credentials, {force})`.
///
/// `force` skips the lead-time check, which is what the background scheduler
/// wants: it applies a larger lead than a live request should.
pub async fn check_and_refresh_token(
    provider: &str,
    credentials: &Credentials,
    proxy_options: &ProxyOptions,
    force: bool,
) -> RefreshOutcome {
    let mut creds = credentials.clone();
    if creds.connection_id.is_none() {
        creds.connection_id = creds
            .extra
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    let mut patch = None;
    let now = router_db::time::now_ms();

    if force || should_refresh_credentials(provider, &creds, now) {
        let expires_at = creds
            .expires_at
            .as_deref()
            .map(|s| Value::String(s.to_string()));
        let remaining_ms = expires_at
            .as_ref()
            .and_then(|v| crate::executors::oauth::parse_time_ms(Some(v)))
            .map(|ms| ms - now);
        tracing::info!(
            "[TOKEN_REFRESH] refreshing {provider} proactively (expires in {:?}s)",
            remaining_ms.map(|ms| ms / 1000)
        );

        if let Some(refreshed) = refresh_provider_credentials(provider, &creds, proxy_options).await
        {
            let rotated =
                refreshed.get("accessToken").is_some() || refreshed.get("apiKey").is_some();
            if rotated {
                apply_refresh_patch(&mut creds, &refreshed);
                patch = Some(refreshed);
            } else if refreshed.get("error").is_some() {
                tracing::warn!("[TOKEN_REFRESH] {provider} needs re-authentication");
            }
        }
    }

    RefreshOutcome {
        credentials: creds,
        patch,
    }
}

/// `normalizeExpiresAt(expiresAt)`: canonical ISO, or `None` when unparseable.
pub fn normalize_expires_at(expires_at: &str) -> Option<String> {
    let ms = crate::executors::oauth::parse_time_ms(Some(&Value::String(expires_at.to_string())))?;
    Some(crate::executors::oauth::to_iso(ms))
}

/// `updateProviderCredentials(connectionId, newCredentials)`: persist the sparse
/// camelCase fields a refresh produced, normalizing the expiry pair and merging
/// `providerSpecificData` over `existingProviderSpecificData`.
///
/// Returns whether a row was written.
pub fn update_provider_credentials(db: &Db, connection_id: &str, new_credentials: &Value) -> bool {
    let Some(new) = new_credentials.as_object() else {
        return false;
    };
    let truthy_str = |key: &str| {
        new.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let mut updates = Map::new();
    if let Some(v) = truthy_str("accessToken") {
        updates.insert("accessToken".into(), json!(v));
    }
    if let Some(v) = truthy_str("refreshToken") {
        updates.insert("refreshToken".into(), json!(v));
    }
    if let Some(v) = truthy_str("idToken") {
        updates.insert("idToken".into(), json!(v));
    }
    if let Some(v) = new
        .get("lastRefreshAt")
        .filter(|v| crate::translator::concerns::primitives::js_truthy(v))
    {
        updates.insert("lastRefreshAt".into(), v.clone());
    }
    if let Some(v) = truthy_str("expiresAt") {
        updates.insert("expiresAt".into(), json!(v));
    }
    if let Some(expires_in) = new.get("expiresIn").and_then(Value::as_i64)
        && expires_in != 0
    {
        updates.insert(
            "expiresAt".into(),
            json!(crate::executors::oauth::to_expires_at(
                expires_in,
                router_db::time::now_ms()
            )),
        );
        updates.insert("expiresIn".into(), json!(expires_in));
    } else if let Some(raw) = truthy_str("expiresAt")
        && let Some(normalized) = normalize_expires_at(&raw)
    {
        let remaining_ms =
            crate::executors::oauth::parse_time_ms(Some(&Value::String(normalized.clone())))
                .map(|ms| ms - router_db::time::now_ms())
                .unwrap_or(0);
        updates.insert("expiresAt".into(), json!(normalized));
        updates.insert(
            "expiresIn".into(),
            json!(remaining_ms.div_euclid(1000).max(1)),
        );
    }
    if let Some(psd) = new.get("providerSpecificData").and_then(Value::as_object) {
        let mut merged = new
            .get("existingProviderSpecificData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for (k, v) in psd {
            merged.insert(k.clone(), v.clone());
        }
        updates.insert("providerSpecificData".into(), Value::Object(merged));
    }
    if let Some(copilot) = truthy_str("copilotToken")
        && let Some(obj) = updates
            .entry("providerSpecificData")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
    {
        obj.insert("copilotToken".into(), json!(copilot));
    }
    if let Some(expires) = new
        .get("copilotTokenExpiresAt")
        .filter(|v| crate::translator::concerns::primitives::js_truthy(v))
        && let Some(obj) = updates
            .entry("providerSpecificData")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
    {
        obj.insert("copilotTokenExpiresAt".into(), expires.clone());
    }
    if let Some(v) = truthy_str("projectId") {
        updates.insert("projectId".into(), json!(v));
    }

    if updates.is_empty() {
        return false;
    }

    let updates = Value::Object(updates);
    match db.write(|tx| {
        router_db::repos::connections::update_provider_connection(tx, connection_id, &updates)
    }) {
        Ok(result) => result.is_some(),
        Err(error) => {
            tracing::error!("[TOKEN_REFRESH] failed to persist credentials: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn credentials_with_token(token: &str) -> Credentials {
        Credentials {
            refresh_token: Some(token.to_string()),
            connection_id: Some("conn-1".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn retry_returns_the_first_success() {
        let mut creds = credentials_with_token("old");
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let result = refresh_with_retry(&mut creds, 3, move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Some(RefreshedCredentials {
                    access_token: Some("new".into()),
                    ..Default::default()
                })
            }
        })
        .await;
        assert_eq!(result.unwrap().access_token.as_deref(), Some("new"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_gives_up_after_the_budget_and_mutates_between_attempts() {
        let mut creds = credentials_with_token("old");
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let result = refresh_with_retry(&mut creds, 3, move |creds| {
            let counter = counter.clone();
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    // A rotating provider: the token it saw is now consumed.
                    Some(RefreshedCredentials {
                        refresh_token: Some(format!("rotated-{attempt}")),
                        ..Default::default()
                    })
                } else {
                    let _ = creds;
                    None
                }
            }
        })
        .await;

        // Attempts 0 and 1 returned a result, so the loop stops at the first
        // Some — the give-up path needs every attempt to return None.
        assert!(result.is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let mut creds = credentials_with_token("old");
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let result = refresh_with_retry(&mut creds, 3, move |_| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                None
            }
        })
        .await;
        assert!(result.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_rotated_refresh_token_is_written_back_for_the_next_attempt() {
        let mut creds = credentials_with_token("old");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();

        let result = refresh_with_retry(&mut creds, 3, move |creds| {
            let recorder = recorder.clone();
            let counter = counter.clone();
            let current = creds.refresh_token.clone().unwrap_or_default();
            async move {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(current);
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // Nothing usable yet, but the refresh token rotated.
                    Some(RefreshedCredentials {
                        refresh_token: Some("rotated".into()),
                        ..Default::default()
                    })
                } else {
                    None
                }
            }
        })
        .await;

        assert!(result.is_some());
        // The rotation was recorded even though the call returned nothing usable.
        assert_eq!(creds.refresh_token.as_deref(), Some("rotated"));
        assert_eq!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
            &["old".to_string()]
        );
    }

    #[tokio::test]
    async fn concurrent_dedup_calls_share_one_refresh() {
        let calls = Arc::new(AtomicU32::new(0));
        let first = {
            let calls = calls.clone();
            tokio::spawn(async move {
                dedup_refresh("test-dedup-a", "tok", || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some(RefreshedCredentials {
                        access_token: Some("shared".into()),
                        ..Default::default()
                    })
                })
                .await
            })
        };
        // Let the first caller register the pending slot.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let second = dedup_refresh("test-dedup-a", "tok", || async {
            panic!("the second caller must reuse the in-flight result")
        })
        .await;

        let first = first.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.unwrap().access_token.as_deref(), Some("shared"));
        assert_eq!(second.unwrap().access_token.as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn an_empty_token_skips_the_dedup_cache_entirely() {
        let result = dedup_refresh("test-dedup-b", "", || async {
            Some(RefreshedCredentials {
                access_token: Some("direct".into()),
                ..Default::default()
            })
        })
        .await;
        assert_eq!(result.unwrap().access_token.as_deref(), Some("direct"));
    }

    #[tokio::test]
    async fn the_refresh_lock_is_keyed_by_connection_not_token() {
        let a = credentials_with_token("t1");
        let mut b = credentials_with_token("t2");
        b.connection_id = Some("conn-1".to_string());
        assert_eq!(refresh_lock_key("codex", &a), refresh_lock_key("codex", &b));

        let mut c = credentials_with_token("t1");
        c.connection_id = Some("conn-2".to_string());
        assert_ne!(refresh_lock_key("codex", &a), refresh_lock_key("codex", &c));

        // No connection id: the token tail is the fallback identity.
        let d = Credentials {
            refresh_token: Some("aaaaaaaaaaaaaaaa1234".into()),
            ..Default::default()
        };
        assert!(refresh_lock_key("codex", &d).ends_with("1234"));
    }

    #[test]
    fn applying_a_patch_writes_every_field_it_carries() {
        let mut creds = Credentials::default();
        let patch = json!({
            "accessToken": "a",
            "refreshToken": "r",
            "idToken": "i",
            "expiresAt": "2030-01-01T00:00:00.000Z",
            "expiresIn": 3600,
            "projectId": "p",
            "providerSpecificData": {"profileArn": "arn"},
            "lastRefreshAt": "2026-01-01T00:00:00.000Z",
        });
        apply_refresh_patch(&mut creds, &patch);

        assert_eq!(creds.access_token.as_deref(), Some("a"));
        assert_eq!(creds.refresh_token.as_deref(), Some("r"));
        assert_eq!(creds.id_token.as_deref(), Some("i"));
        assert_eq!(
            creds.expires_at.as_deref(),
            Some("2030-01-01T00:00:00.000Z")
        );
        assert_eq!(creds.expires_in, Some(3600));
        assert_eq!(creds.project_id.as_deref(), Some("p"));
        assert_eq!(
            creds.provider_specific_data.get("profileArn"),
            Some(&json!("arn"))
        );
        assert_eq!(
            creds.extra.get("lastRefreshAt"),
            Some(&json!("2026-01-01T00:00:00.000Z"))
        );
    }

    #[test]
    fn an_empty_string_in_a_patch_does_not_erase_a_field() {
        let mut creds = credentials_with_token("keep");
        apply_refresh_patch(
            &mut creds,
            &json!({"refreshToken": "", "accessToken": null}),
        );
        assert_eq!(creds.refresh_token.as_deref(), Some("keep"));
        assert!(creds.access_token.is_none());
    }

    #[tokio::test]
    async fn a_provider_without_a_refresh_token_refreshes_to_nothing() {
        let creds = Credentials {
            access_token: Some("a".into()),
            ..Default::default()
        };
        let outcome =
            check_and_refresh_token("codex", &creds, &ProxyOptions::default(), false).await;
        assert!(outcome.patch.is_none());
        assert_eq!(outcome.credentials.access_token.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn an_unknown_provider_has_no_refresher() {
        let creds = credentials_with_token("t");
        assert!(
            refresh_token_by_provider("kilocode", &creds, &ProxyOptions::default())
                .await
                .is_none()
        );
    }
}
