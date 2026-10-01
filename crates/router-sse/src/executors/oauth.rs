//! Credential freshness and the refreshed-credential merge, including the
//! lead-time helpers.
//!
//! `should_refresh_credentials` has two triggers and both matter: an explicit
//! `expiresAt` inside the provider's lead window, and the proactive
//! `maxRefreshAgeMs` window (codex: 8 days) which exists because a Codex
//! refresh token can go stale while still nominally valid.

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::providers::registry::registry;

/// The default refresh lead when a provider declares none.
pub const TOKEN_EXPIRY_BUFFER_MS: i64 = 5 * 60 * 1000;

/// Parse a timestamp: numbers below 1e12 are seconds, strings are parsed as
/// dates. Anything unparseable is `None`.
pub fn parse_time_ms(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    if let Some(n) = value.as_f64() {
        return Some(if n < 1e12 {
            (n * 1000.0) as i64
        } else {
            n as i64
        });
    }
    let s = value.as_str()?;
    if s.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc).timestamp_millis())
}

/// The credential's expiry, from `expiresAt` or the `tokenExpiresAt` extra.
pub fn credential_expiry_ms(credentials: &Credentials) -> Option<i64> {
    let from_expires_at = credentials
        .expires_at
        .as_deref()
        .map(|s| Value::String(s.to_string()));
    let from_extra = credentials.extra.get("tokenExpiresAt").cloned();
    parse_time_ms(from_expires_at.as_ref().or(from_extra.as_ref()))
}

/// The credential's last refresh time, from the extra fields or provider data.
pub fn credential_last_refresh_ms(credentials: &Credentials) -> Option<i64> {
    parse_time_ms(credentials.extra.get("lastRefreshAt"))
        .or_else(|| parse_time_ms(credentials.extra.get("lastRefresh")))
        .or_else(|| parse_time_ms(credentials.provider_specific_data.get("lastRefreshAt")))
}

/// Whether the last refresh is older than `max_age_ms`. A credential with no
/// recorded refresh counts as stale.
pub fn is_refresh_stale(credentials: &Credentials, now_ms: i64, max_age_ms: i64) -> bool {
    match credential_last_refresh_ms(credentials) {
        Some(last) => now_ms - last >= max_age_ms,
        None => true,
    }
}

/// How far before expiry a provider's credential is refreshed, from the
/// registry or the default buffer.
pub fn refresh_lead_ms(provider: &str) -> i64 {
    let declared = registry()
        .oauth(provider)
        .and_then(|o| o.get("refreshLeadMs"))
        .and_then(Value::as_i64);
    if let Some(lead) = declared {
        return lead;
    }
    TOKEN_EXPIRY_BUFFER_MS
}

/// Whether a credential should be refreshed now: either it is inside the
/// provider's lead window, or the proactive `maxRefreshAgeMs` window has
/// elapsed since the last refresh.
pub fn should_refresh_credentials(provider: &str, credentials: &Credentials, now_ms: i64) -> bool {
    if let Some(expires_at) = credential_expiry_ms(credentials)
        && expires_at - now_ms < refresh_lead_ms(provider)
    {
        return true;
    }
    let max_age = registry()
        .oauth(provider)
        .and_then(|o| o.get("maxRefreshAgeMs"))
        .and_then(Value::as_i64);
    if let Some(max_age) = max_age
        && credentials.refresh_token.is_some()
        && is_refresh_stale(credentials, now_ms, max_age)
    {
        return true;
    }
    false
}

/// Whether a refresh result carries an error code that no retry can recover
/// from.
pub fn is_unrecoverable_refresh_error(result: &Value) -> bool {
    let code = result.get("error").and_then(Value::as_str).unwrap_or("");
    matches!(
        code,
        "unrecoverable_refresh_error"
            | "refresh_token_reused"
            | "invalid_request"
            | "invalid_grant"
    )
}

/// Classify a failed refresh into `{status, code, description, permanent}`.
pub fn classify_oauth_refresh_error(error_text: &str, status: u16) -> Value {
    let parsed: Option<Value> = if error_text.is_empty() {
        None
    } else {
        serde_json::from_str(error_text).ok()
    };
    let code = parsed
        .as_ref()
        .and_then(|p| p.get("error"))
        .and_then(|e| {
            e.get("code")
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .or_else(|| c.as_i64().map(|n| n.to_string()))
                })
                .or_else(|| e.as_str().map(str::to_string))
        })
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|p| p.get("error_code"))
                .and_then(|c| {
                    c.as_str()
                        .map(str::to_string)
                        .or_else(|| c.as_i64().map(|n| n.to_string()))
                })
        })
        .unwrap_or_default();
    let description = parsed
        .as_ref()
        .and_then(|p| p.get("error_description"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| error_text.to_string());
    let combined = format!("{code} {description}").to_ascii_lowercase();
    let permanent = [
        "refresh_token_expired",
        "refresh_token_reused",
        "refresh_token_invalidated",
        "invalid_grant",
    ]
    .iter()
    .any(|m| combined.contains(m));

    json!({ "status": status, "code": code, "description": description, "permanent": permanent })
}

/// Merge `next` over `existing` provider-specific data, key by key.
fn merge_psd(
    existing: Option<&Map<String, Value>>,
    next: &Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = existing.cloned().unwrap_or_default();
    for (k, v) in next {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

/// The result of a refresh: only the fields the merge reads back.
#[derive(Debug, Clone, Default)]
pub struct RefreshedCredentials {
    pub access_token: Option<String>,
    pub api_key: Option<String>,
    pub token: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_in: Option<i64>,
    pub expires_at: Option<String>,
    pub project_id: Option<String>,
    pub provider_specific_data: Option<Map<String, Value>>,
    pub copilot_token: Option<String>,
    pub copilot_token_expires_at: Option<String>,
    pub last_refresh_at: Option<String>,
    /// `{ error: "…" }` from an unrecoverable refresh.
    pub error: Option<String>,
}

impl RefreshedCredentials {
    /// The `{ error: "…" }` shape the unrecoverable-error check reads.
    pub fn to_error_value(&self) -> Value {
        json!({ "error": self.error.clone().unwrap_or_default() })
    }
}

/// Merge a refresh result into the stored credential.
///
/// Returns the sparse patch written back to storage, or `None` when the refresh
/// produced nothing.
pub fn merge_refreshed_credentials(
    provider: &str,
    current: &Credentials,
    refreshed: &RefreshedCredentials,
    now_ms: i64,
) -> Option<Value> {
    if refreshed.error.is_some() {
        return Some(refreshed.to_error_value());
    }
    let mut next = Map::new();
    let mut any = false;

    let set = |map: &mut Map<String, Value>, key: &str, value: Value| {
        map.insert(key.to_string(), value);
    };
    if let Some(v) = &refreshed.access_token {
        set(&mut next, "accessToken", json!(v));
        any = true;
    }
    if let Some(v) = &refreshed.api_key {
        set(&mut next, "apiKey", json!(v));
        any = true;
    }
    if let Some(v) = &refreshed.token {
        set(&mut next, "token", json!(v));
        any = true;
    }
    let refresh_token = refreshed
        .refresh_token
        .clone()
        .or_else(|| current.refresh_token.clone());
    if let Some(v) = refresh_token {
        set(&mut next, "refreshToken", json!(v));
        any = true;
    }
    let id_token = refreshed
        .id_token
        .clone()
        .or_else(|| current.id_token.clone());
    if let Some(v) = id_token {
        set(&mut next, "idToken", json!(v));
        any = true;
    }
    if let Some(expires_in) = refreshed.expires_in {
        set(&mut next, "expiresIn", json!(expires_in));
        set(
            &mut next,
            "expiresAt",
            json!(to_expires_at(expires_in, now_ms)),
        );
        any = true;
    } else if let Some(expires_at) = &refreshed.expires_at {
        set(&mut next, "expiresAt", json!(expires_at));
        any = true;
    }
    if let Some(project_id) = &refreshed.project_id {
        set(&mut next, "projectId", json!(project_id));
        any = true;
    }
    if let Some(psd) = &refreshed.provider_specific_data {
        let merged = merge_psd(Some(&current.provider_specific_data), psd);
        set(&mut next, "providerSpecificData", Value::Object(merged));
        any = true;
    }
    if let Some(v) = &refreshed.copilot_token {
        set(&mut next, "copilotToken", json!(v));
        any = true;
    }
    if let Some(v) = &refreshed.copilot_token_expires_at {
        set(&mut next, "copilotTokenExpiresAt", json!(v));
        any = true;
    }

    let track = registry()
        .oauth(provider)
        .and_then(|o| o.get("trackRefreshAt"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if track || any {
        let stamp = refreshed
            .last_refresh_at
            .clone()
            .unwrap_or_else(|| to_iso(now_ms));
        set(&mut next, "lastRefreshAt", json!(stamp));
    }

    if next.is_empty() {
        None
    } else {
        Some(Value::Object(next))
    }
}

/// An absolute expiry from a relative `expires_in`.
pub fn to_expires_at(expires_in: i64, now_ms: i64) -> String {
    to_iso(now_ms + expires_in * 1000)
}

/// Format a millisecond timestamp as UTC RFC 3339 with milliseconds.
pub fn to_iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_parsing_handles_seconds_ms_and_iso() {
        assert_eq!(
            parse_time_ms(Some(&json!(1_700_000_000))),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_time_ms(Some(&json!(1_700_000_000_000i64))),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_time_ms(Some(&json!("2023-11-14T22:13:20.000Z"))),
            Some(1_700_000_000_000)
        );
        assert_eq!(parse_time_ms(Some(&json!(""))), None);
        assert_eq!(parse_time_ms(None), None);
    }

    #[test]
    fn expiry_inside_the_lead_window_triggers_a_refresh() {
        let mut c = Credentials::default();
        let now = 1_700_000_000_000i64;
        // grok-cli's lead is 5m; 3m out is inside it.
        c.expires_at = Some(to_iso(now + 3 * 60 * 1000));
        assert!(should_refresh_credentials("grok-cli", &c, now));
        c.expires_at = Some(to_iso(now + 10 * 60 * 1000));
        assert!(!should_refresh_credentials("grok-cli", &c, now));
    }

    #[test]
    fn codex_refreshes_proactively_when_the_stamp_is_old() {
        let mut c = Credentials {
            refresh_token: Some("r".into()),
            ..Default::default()
        };
        let now = 1_700_000_000_000i64;
        c.provider_specific_data.insert(
            "lastRefreshAt".into(),
            json!(to_iso(now - 9 * 24 * 3600 * 1000)),
        );
        assert!(
            should_refresh_credentials("codex", &c, now),
            "9 days > 8-day window"
        );
        c.provider_specific_data
            .insert("lastRefreshAt".into(), json!(to_iso(now - 1000)));
        assert!(!should_refresh_credentials("codex", &c, now));
    }

    #[test]
    fn merge_stamps_last_refresh_at_for_tracking_providers() {
        let current = Credentials::default();
        let refreshed = RefreshedCredentials {
            access_token: Some("a".into()),
            expires_in: Some(3600),
            ..Default::default()
        };
        let merged =
            merge_refreshed_credentials("codex", &current, &refreshed, 1_700_000_000_000).unwrap();
        assert_eq!(merged["accessToken"], json!("a"));
        assert_eq!(merged["expiresAt"], json!("2023-11-14T23:13:20.000Z"));
        assert!(merged.get("lastRefreshAt").is_some());
    }

    #[test]
    fn merge_keeps_the_current_refresh_token_when_the_response_omits_it() {
        let current = Credentials {
            refresh_token: Some("keep".into()),
            ..Default::default()
        };
        let refreshed = RefreshedCredentials {
            access_token: Some("a".into()),
            ..Default::default()
        };
        let merged = merge_refreshed_credentials("codex", &current, &refreshed, 0).unwrap();
        assert_eq!(merged["refreshToken"], json!("keep"));
    }

    #[test]
    fn unrecoverable_errors_are_returned_as_the_error_shape() {
        let refreshed = RefreshedCredentials {
            error: Some("invalid_grant".into()),
            ..Default::default()
        };
        let merged =
            merge_refreshed_credentials("codex", &Credentials::default(), &refreshed, 0).unwrap();
        assert!(is_unrecoverable_refresh_error(&merged));
    }

    #[test]
    fn refresh_lead_reads_the_registry_then_defaults() {
        assert_eq!(refresh_lead_ms("codex"), 432_000_000);
        assert_eq!(refresh_lead_ms("deepseek"), TOKEN_EXPIRY_BUFFER_MS);
    }

    #[test]
    fn error_classification_flags_permanent_codes() {
        let v = classify_oauth_refresh_error(r#"{"error":"invalid_grant"}"#, 400);
        assert_eq!(v["permanent"], json!(true));
        let v = classify_oauth_refresh_error("boom", 500);
        assert_eq!(v["permanent"], json!(false));
    }
}
