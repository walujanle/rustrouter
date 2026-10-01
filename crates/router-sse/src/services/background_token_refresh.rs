//! Background proactive OAuth token refresh selection.
//!
//! The scheduler tick (`runBackgroundTokenRefreshTick`) and the interval
//! start/stop pair are deliberately absent: they are process lifecycle, and the
//! server layer owns them. What lives here is the pure predicate the tick runs
//! first, so the "is this connection due?" rule can be tested without a
//! scheduler or a clock.
//!
//! The tick fails open. The predicate itself never fails: a connection missing
//! an expiry or a refresh token is simply not selected.

use serde_json::Value;

use crate::executors::oauth::{parse_time_ms, refresh_lead_ms};
use crate::providers::registry::registry;
use crate::translator::concerns::primitives::js_truthy_opt;

/// `BACKGROUND_REFRESH_LEAD_MS` — refresh when expiry is within 30 minutes, or
/// the provider's own on-request lead, whichever is larger.
pub const BACKGROUND_REFRESH_LEAD_MS: i64 = 30 * 60 * 1000;

/// `getCredentialExpiryMs(conn)`: `conn.expiresAt ?? conn.tokenExpiresAt`,
/// parsed. `??` falls through on `null`/`undefined` only, so an explicit empty
/// string stays and parses to `None` rather than falling to the second key.
fn credential_expiry_ms(conn: &Value) -> Option<i64> {
    let value = conn
        .get("expiresAt")
        .filter(|v| !v.is_null())
        .or_else(|| conn.get("tokenExpiresAt"));
    parse_time_ms(value)
}

/// `selectConnectionsNeedingRefresh(connections, nowMs)`.
///
/// Returns the ids of OAuth connections whose access token expires inside
/// `max(provider lead, BACKGROUND_REFRESH_LEAD_MS)`. The caller re-resolves each
/// id to a connection to refresh; the predicate only decides *which* are due.
///
/// A connection whose provider has no registry `providerOauth` entry is skipped:
/// there is no refresher for it, so it would be re-selected every tick, fail to
/// refresh, and log forever. That is the state a connection is left in when its
/// provider is dropped from the registry but its row survives in the shared
/// database — the row is inert data, and the tick must not chase it.
pub fn select_connections_needing_refresh(connections: &[Value], now_ms: i64) -> Vec<String> {
    if connections.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for conn in connections {
        if conn.is_null() {
            continue;
        }

        let auth_type = conn
            .get("authType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
            .replace('_', "");
        if auth_type != "oauth" {
            continue;
        }
        if !js_truthy_opt(conn.get("refreshToken")) {
            continue;
        }

        let provider = conn.get("provider").and_then(Value::as_str).unwrap_or("");
        if registry().oauth(provider).is_none() {
            continue;
        }

        let Some(expires_at_ms) = credential_expiry_ms(conn) else {
            continue;
        };

        let lead_ms = refresh_lead_ms(provider).max(BACKGROUND_REFRESH_LEAD_MS);

        if expires_at_ms - now_ms < lead_ms {
            out.push(
                conn.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connection(id: &str, expires_in_ms: i64, now: i64) -> Value {
        // `kilocode` declares no lead, so the effective lead is exactly the
        // 30-minute default this test exercises.
        json!({
            "id": id,
            "provider": "kilocode",
            "authType": "oauth",
            "refreshToken": "r",
            "expiresAt": crate::executors::oauth::to_iso(now + expires_in_ms),
        })
    }

    #[test]
    fn the_lead_window_boundary_is_strictly_inside() {
        let now = 1_700_000_000_000i64;
        // Exactly at the lead is not yet due; one ms inside is.
        let at = vec![connection("a", BACKGROUND_REFRESH_LEAD_MS, now)];
        assert!(select_connections_needing_refresh(&at, now).is_empty());

        let inside = vec![connection("b", BACKGROUND_REFRESH_LEAD_MS - 1, now)];
        assert_eq!(select_connections_needing_refresh(&inside, now), vec!["b"]);
    }

    #[test]
    fn a_provider_lead_larger_than_the_default_wins() {
        let now = 1_700_000_000_000i64;
        // codex's lead is 5 days, well past the 30m default.
        let mut conn = connection("c", BACKGROUND_REFRESH_LEAD_MS + 60_000, now);
        conn["provider"] = json!("codex");
        assert_eq!(select_connections_needing_refresh(&[conn], now), vec!["c"]);
    }

    #[test]
    fn non_oauth_missing_token_and_missing_expiry_are_skipped() {
        let now = 1_700_000_000_000i64;
        let mut api_key = connection("a", 1000, now);
        api_key["authType"] = json!("api_key");
        let mut no_token = connection("b", 1000, now);
        no_token["refreshToken"] = json!("");
        let mut no_expiry = connection("c", 1000, now);
        no_expiry.as_object_mut().unwrap().remove("expiresAt");

        assert!(
            select_connections_needing_refresh(&[api_key, no_token, no_expiry], now).is_empty()
        );
    }

    #[test]
    fn a_connection_without_an_oauth_entry_is_never_selected() {
        // A provider with no `providerOauth` entry has no refresher, so
        // selecting its connection would log a failed refresh every tick.
        let now = 1_700_000_000_000i64;
        let mut unknown = connection("a", -60_000, now);
        unknown["provider"] = json!("deepseek");
        assert!(select_connections_needing_refresh(&[unknown], now).is_empty());
    }

    #[test]
    fn an_underscored_auth_type_still_counts_as_oauth() {
        let now = 1_700_000_000_000i64;
        let mut conn = connection("a", 1000, now);
        conn["authType"] = json!("OAuth_2");
        // "oauth2" after underscore removal — not equal to "oauth".
        assert!(select_connections_needing_refresh(&[conn], now).is_empty());

        let mut conn = connection("b", 1000, now);
        conn["authType"] = json!("O_Auth");
        assert_eq!(select_connections_needing_refresh(&[conn], now), vec!["b"]);
    }
}
