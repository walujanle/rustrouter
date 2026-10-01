//! Multi-account fallback decisions and the rule table they read.
//!
//! The module is the *policy* half only — it decides whether an error cools a
//! connection down and for how long. Reading and writing connections belongs to
//! the caller; accounts here are `serde_json::Value` because that is the shape
//! the connections repository hands back.
//!
//! The one rule worth reading twice is the request-scoped 4xx guard at the end
//! of `checkFallbackError`. A 400 caused by the request itself says nothing
//! about the credential, and cooling the account down on it removes a healthy
//! connection from rotation — with a single connection, every later request in
//! the window then fails with a copy of the original error and hides the cause.

use serde_json::{Value, json};

use crate::translator::concerns::primitives::{js_string, js_truthy};

/// `BACKOFF_CONFIG`.
pub const BACKOFF_BASE_MS: u64 = 2000;
pub const BACKOFF_MAX_MS: u64 = 5 * 60 * 1000;
pub const BACKOFF_MAX_LEVEL: u32 = 15;

/// `TRANSIENT_COOLDOWN_MS`.
pub const TRANSIENT_COOLDOWN_MS: u64 = 30 * 1000;

/// `MAX_RATE_LIMIT_COOLDOWN_MS`: hard cap for a provider-reported rate-limit
/// cooldown (a codex `resets_at` can be 5-6h out).
pub const MAX_RATE_LIMIT_COOLDOWN_MS: u64 = 30 * 60 * 1000;

/// `COOLDOWN`.
const COOLDOWN_LONG_MS: u64 = 2 * 60 * 1000;
const COOLDOWN_SHORT_MS: u64 = 5 * 1000;

/// One `ERROR_RULES` entry. `None` for `cooldown_ms` means the rule is a
/// backoff rule.
struct ErrorRule {
    text: Option<&'static str>,
    status: Option<u16>,
    cooldown_ms: Option<u64>,
    backoff: bool,
}

/// `ERROR_RULES`, in check order: text rules first (by priority), then status
/// rules. Written as a const array rather than a match so the order is visible
/// and a reordering cannot hide inside control flow.
const ERROR_RULES: &[ErrorRule] = &[
    ErrorRule {
        text: Some("no credentials"),
        status: None,
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: Some("request not allowed"),
        status: None,
        cooldown_ms: Some(COOLDOWN_SHORT_MS),
        backoff: false,
    },
    ErrorRule {
        text: Some("improperly formed request"),
        status: None,
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: Some("rate limit"),
        status: None,
        cooldown_ms: None,
        backoff: true,
    },
    ErrorRule {
        text: Some("too many requests"),
        status: None,
        cooldown_ms: None,
        backoff: true,
    },
    ErrorRule {
        text: Some("quota exceeded"),
        status: None,
        cooldown_ms: None,
        backoff: true,
    },
    ErrorRule {
        text: Some("capacity"),
        status: None,
        cooldown_ms: None,
        backoff: true,
    },
    ErrorRule {
        text: Some("overloaded"),
        status: None,
        cooldown_ms: None,
        backoff: true,
    },
    ErrorRule {
        text: None,
        status: Some(401),
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: None,
        status: Some(402),
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: None,
        status: Some(403),
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: None,
        status: Some(404),
        cooldown_ms: Some(COOLDOWN_LONG_MS),
        backoff: false,
    },
    ErrorRule {
        text: None,
        status: Some(429),
        cooldown_ms: None,
        backoff: true,
    },
];

/// `COOLDOWN_MS`: the per-status cooldown constants, kept as a named group.
pub mod cooldown_ms {
    use super::*;
    pub const UNAUTHORIZED: u64 = COOLDOWN_LONG_MS;
    pub const PAYMENT_REQUIRED: u64 = COOLDOWN_LONG_MS;
    pub const NOT_FOUND: u64 = COOLDOWN_LONG_MS;
    pub const TRANSIENT: u64 = TRANSIENT_COOLDOWN_MS;
    pub const REQUEST_NOT_ALLOWED: u64 = COOLDOWN_SHORT_MS;
}

/// The `checkFallbackError` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FallbackDecision {
    pub should_fallback: bool,
    pub cooldown_ms: u64,
    /// `Some` only when a backoff rule fired; the caller stores it back on the
    /// account so the next failure backs off further.
    pub new_backoff_level: Option<u32>,
}

/// `getQuotaCooldown(backoffLevel = 0)`: `base * 2^(level-1)` capped at 4 min.
///
/// Level 0 and level 1 both yield the base: `Math.max(0, level - 1)`.
pub fn get_quota_cooldown(backoff_level: u32) -> u64 {
    let level = backoff_level.saturating_sub(1).min(31);
    let cooldown = BACKOFF_BASE_MS.saturating_mul(1u64 << level);
    cooldown.min(BACKOFF_MAX_MS)
}

/// `checkFallbackError(status, errorText, backoffLevel = 0)`.
///
/// `error_text` is matched case-insensitively as a substring; a non-string
/// value is serialized to its string form first.
pub fn check_fallback_error(
    status: u16,
    error_text: Option<&Value>,
    backoff_level: u32,
) -> FallbackDecision {
    let lower = match error_text {
        Some(Value::String(s)) => s.to_lowercase(),
        Some(v) if js_truthy(v) => js_string(v).to_lowercase(),
        _ => String::new(),
    };

    for rule in ERROR_RULES {
        let text_hit = rule
            .text
            .is_some_and(|t| !lower.is_empty() && lower.contains(t));
        let status_hit = rule.status == Some(status);
        if !text_hit && !status_hit {
            continue;
        }
        if rule.backoff {
            let new_level = (backoff_level + 1).min(BACKOFF_MAX_LEVEL);
            return FallbackDecision {
                should_fallback: true,
                cooldown_ms: get_quota_cooldown(new_level),
                new_backoff_level: Some(new_level),
            };
        }
        return FallbackDecision {
            should_fallback: true,
            cooldown_ms: rule.cooldown_ms.unwrap_or(0),
            new_backoff_level: None,
        };
    }

    // Request-scoped client errors that matched no rule: hand the upstream error
    // back for this request instead of cooling the account down.
    if (400..500).contains(&status) && !matches!(status, 401 | 402 | 403 | 429) {
        return FallbackDecision {
            should_fallback: false,
            cooldown_ms: 0,
            new_backoff_level: None,
        };
    }

    FallbackDecision {
        should_fallback: true,
        cooldown_ms: TRANSIENT_COOLDOWN_MS,
        new_backoff_level: None,
    }
}

/// `isAccountUnavailable(unavailableUntil)`: an unparseable timestamp reads as
/// expired, because `new Date(bad).getTime()` is `NaN` and `NaN > now` is false.
pub fn is_account_unavailable(unavailable_until: Option<&Value>) -> bool {
    let Some(iso) = unavailable_until
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    router_db::time::parse_iso(iso).is_some_and(|t| t > chrono::Utc::now())
}

/// `getUnavailableUntil(cooldownMs)`.
pub fn get_unavailable_until(cooldown_ms: u64) -> String {
    let until = chrono::Utc::now() + chrono::Duration::milliseconds(cooldown_ms as i64);
    router_db::time::to_iso(until)
}

/// `getEarliestRateLimitedUntil(accounts)`: the soonest *future* expiry across
/// the list, or `None` when none is pending.
pub fn get_earliest_rate_limited_until(accounts: &[Value]) -> Option<String> {
    earliest_expiry(accounts.iter().filter_map(|a| a.get("rateLimitedUntil")))
}

/// `getEarliestModelLockUntil(connection)`: same scan over the flat
/// `modelLock_*` fields.
pub fn get_earliest_model_lock_until(connection: &Value) -> Option<String> {
    let obj = connection.as_object()?;
    earliest_expiry(
        obj.iter()
            .filter(|(k, _)| k.starts_with(MODEL_LOCK_PREFIX))
            .map(|(_, v)| v),
    )
}

/// The shared body of both "earliest future expiry" scans.
fn earliest_expiry<'a>(values: impl Iterator<Item = &'a Value>) -> Option<String> {
    let now = chrono::Utc::now();
    let mut earliest = None;
    for value in values {
        let Some(iso) = value.as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let Some(t) = router_db::time::parse_iso(iso) else {
            continue;
        };
        if t <= now {
            continue;
        }
        if earliest.is_none_or(|e: chrono::DateTime<chrono::Utc>| t < e) {
            earliest = Some(t);
        }
    }
    earliest.map(router_db::time::to_iso)
}

/// `formatRetryAfter(rateLimitedUntil)`: `"reset after 2m 30s"`.
///
/// Zero-valued units are dropped, but seconds are kept when nothing else is
/// left — that is what makes an exact minute read `"reset after 1m"` and an
/// expired timestamp read `"reset after 0s"`.
pub fn format_retry_after(rate_limited_until: Option<&str>) -> String {
    let Some(iso) = rate_limited_until.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let Some(target) = router_db::time::parse_iso(iso) else {
        return "reset after 0s".to_string();
    };
    let diff_ms = (target - chrono::Utc::now()).num_milliseconds();
    if diff_ms <= 0 {
        return "reset after 0s".to_string();
    }
    let total_sec = (diff_ms + 999) / 1000;
    let h = total_sec / 3600;
    let m = (total_sec % 3600) / 60;
    let s = total_sec % 60;

    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    format!("reset after {}", parts.join(" "))
}

/// `MODEL_LOCK_PREFIX`. Re-exported from the connections repository so the
/// string has exactly one definition in the workspace.
pub use router_db::repos::connections::MODEL_LOCK_PREFIX;

/// `MODEL_LOCK_ALL`.
pub fn model_lock_all() -> String {
    format!("{MODEL_LOCK_PREFIX}__all")
}

/// `getModelLockKey(model)`.
pub fn get_model_lock_key(model: Option<&str>) -> String {
    match model.filter(|m| !m.is_empty()) {
        Some(m) => format!("{MODEL_LOCK_PREFIX}{m}"),
        None => model_lock_all(),
    }
}

/// `isModelLockActive(connection, model)`: the model-specific lock or the
/// account-wide one, whichever is later still pending.
pub fn is_model_lock_active(connection: &Value, model: Option<&str>) -> bool {
    let key = get_model_lock_key(model);
    let all = model_lock_all();
    let specific = connection.get(&key);
    let account_wide = connection.get(&all);
    specific
        .or(account_wide)
        .is_some_and(|v| is_account_unavailable(Some(v)))
}

/// `buildModelLockUpdate(model, cooldownMs)`: the flat-field patch that locks a
/// model on a connection.
pub fn build_model_lock_update(model: Option<&str>, cooldown_ms: u64) -> Value {
    let key = get_model_lock_key(model);
    json!({ key: get_unavailable_until(cooldown_ms) })
}

/// `buildClearModelLocksUpdate(connection)`: every `modelLock_*` field set to
/// null. Only keys already present are touched; absent ones stay absent.
pub fn build_clear_model_locks_update(connection: &Value) -> Value {
    let Some(obj) = connection.as_object() else {
        return json!({});
    };
    let mut cleared = serde_json::Map::new();
    for key in obj.keys().filter(|k| k.starts_with(MODEL_LOCK_PREFIX)) {
        cleared.insert(key.clone(), Value::Null);
    }
    Value::Object(cleared)
}

/// `filterAvailableAccounts(accounts, excludeId = null)`.
pub fn filter_available_accounts(accounts: &[Value], exclude_id: Option<&str>) -> Vec<Value> {
    accounts
        .iter()
        .filter(|acc| {
            if let Some(id) = exclude_id
                && acc.get("id").and_then(Value::as_str) == Some(id)
            {
                return false;
            }
            !is_account_unavailable(acc.get("rateLimitedUntil"))
        })
        .cloned()
        .collect()
}

/// `resetAccountState(account)`: clear cooldown and backoff after a success.
pub fn reset_account_state(account: &Value) -> Value {
    let Some(obj) = account.as_object() else {
        return account.clone();
    };
    let mut out = obj.clone();
    out.insert("rateLimitedUntil".into(), Value::Null);
    out.insert("backoffLevel".into(), json!(0));
    out.insert("lastError".into(), Value::Null);
    out.insert("status".into(), json!("active"));
    Value::Object(out)
}

/// `applyErrorState(account, status, errorText)`.
///
/// A decision that says "do not fall back" still records the error, and
/// `rateLimitedUntil` is assigned unconditionally: a request-scoped 400
/// therefore clears any pending cooldown on the account rather than leaving it
/// in place. That is deliberate.
pub fn apply_error_state(account: &Value, status: u16, error_text: Option<&Value>) -> Value {
    let Some(obj) = account.as_object() else {
        return account.clone();
    };
    let backoff_level = obj.get("backoffLevel").and_then(Value::as_u64).unwrap_or(0) as u32;
    let decision = check_fallback_error(status, error_text, backoff_level);

    let mut out = obj.clone();
    out.insert(
        "rateLimitedUntil".into(),
        if decision.cooldown_ms > 0 {
            json!(get_unavailable_until(decision.cooldown_ms))
        } else {
            Value::Null
        },
    );
    out.insert(
        "backoffLevel".into(),
        json!(decision.new_backoff_level.unwrap_or(backoff_level)),
    );
    out.insert(
        "lastError".into(),
        json!({
            "status": status,
            "message": error_text.cloned().unwrap_or(Value::Null),
            "timestamp": router_db::time::now_iso(),
        }),
    );
    out.insert("status".into(), json!("error"));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_the_base_and_caps() {
        assert_eq!(get_quota_cooldown(0), 2000);
        assert_eq!(get_quota_cooldown(1), 2000);
        assert_eq!(get_quota_cooldown(2), 4000);
        assert_eq!(get_quota_cooldown(3), 8000);
        assert_eq!(get_quota_cooldown(15), BACKOFF_MAX_MS);
        assert_eq!(get_quota_cooldown(99), BACKOFF_MAX_MS);
    }

    #[test]
    fn text_rules_beat_status_rules() {
        // A 429 whose body says "request not allowed" gets the short cooldown,
        // not the backoff: the text rule is earlier in the table.
        let d = check_fallback_error(429, Some(&json!("Request not allowed")), 0);
        assert_eq!(d.cooldown_ms, COOLDOWN_SHORT_MS);
        assert_eq!(d.new_backoff_level, None);
    }

    #[test]
    fn text_rules_are_case_insensitive_substrings() {
        let d = check_fallback_error(500, Some(&json!("provider CAPACITY exceeded")), 0);
        assert!(d.should_fallback);
        assert_eq!(d.new_backoff_level, Some(1));
    }

    #[test]
    fn status_rules_apply_without_text() {
        assert_eq!(
            check_fallback_error(401, None, 0).cooldown_ms,
            COOLDOWN_LONG_MS
        );
        assert_eq!(
            check_fallback_error(404, None, 0).cooldown_ms,
            COOLDOWN_LONG_MS
        );
        let d = check_fallback_error(429, None, 3);
        assert_eq!(d.new_backoff_level, Some(4));
        // `base * 2^(level - 1)`, so level 4 is 2000 * 8.
        assert_eq!(d.cooldown_ms, 16000);
    }

    #[test]
    fn backoff_level_saturates_at_the_max() {
        let d = check_fallback_error(429, None, BACKOFF_MAX_LEVEL);
        assert_eq!(d.new_backoff_level, Some(BACKOFF_MAX_LEVEL));
        assert_eq!(d.cooldown_ms, BACKOFF_MAX_MS);
    }

    #[test]
    fn request_scoped_4xx_does_not_cool_the_account() {
        for status in [400, 405, 406, 409, 413, 422] {
            let d = check_fallback_error(status, None, 0);
            assert!(!d.should_fallback, "{status} must not fall back");
            assert_eq!(d.cooldown_ms, 0);
        }
    }

    #[test]
    fn account_scoped_4xx_still_falls_back() {
        for status in [401, 402, 403, 429] {
            assert!(
                check_fallback_error(status, None, 0).should_fallback,
                "{status} must fall back"
            );
        }
    }

    #[test]
    fn unmatched_5xx_gets_the_transient_cooldown() {
        let d = check_fallback_error(500, None, 0);
        assert!(d.should_fallback);
        assert_eq!(d.cooldown_ms, TRANSIENT_COOLDOWN_MS);
        assert_eq!(d.new_backoff_level, None);
    }

    #[test]
    fn model_lock_key_falls_back_to_the_account_wide_field() {
        assert_eq!(get_model_lock_key(Some("gpt/x")), "modelLock_gpt/x");
        assert_eq!(get_model_lock_key(None), "modelLock___all");
        assert_eq!(get_model_lock_key(Some("")), "modelLock___all");
    }

    #[test]
    fn model_lock_is_active_only_while_pending() {
        let future = get_unavailable_until(60_000);
        let past = get_unavailable_until(0);
        let conn = json!({ "modelLock_gpt": future });
        assert!(is_model_lock_active(&conn, Some("gpt")));
        assert!(!is_model_lock_active(&conn, Some("other")));

        let expired = json!({ "modelLock_gpt": past });
        assert!(!is_model_lock_active(&expired, Some("gpt")));

        // The account-wide lock covers every model.
        let all = json!({ "modelLock___all": get_unavailable_until(60_000) });
        assert!(is_model_lock_active(&all, Some("anything")));
    }

    #[test]
    fn clear_update_nulls_only_the_lock_fields() {
        let conn = json!({"modelLock_a": 1, "modelLock_b": 2, "keep": 3});
        assert_eq!(
            build_clear_model_locks_update(&conn),
            json!({"modelLock_a": null, "modelLock_b": null})
        );
    }

    #[test]
    fn filter_drops_locked_accounts_and_the_excluded_id() {
        let soon = get_unavailable_until(60_000);
        let accounts = json!([
            {"id": "a", "rateLimitedUntil": soon},
            {"id": "b", "rateLimitedUntil": null},
            {"id": "c"},
        ]);
        let accounts = accounts.as_array().unwrap().clone();
        let kept = filter_available_accounts(&accounts, None);
        assert_eq!(kept.len(), 2);
        let kept = filter_available_accounts(&accounts, Some("b"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["id"], json!("c"));
    }

    #[test]
    fn reset_state_clears_everything_the_error_state_set() {
        let account = json!({"id": "a", "rateLimitedUntil": "2030-01-01T00:00:00.000Z", "backoffLevel": 7, "lastError": {"x": 1}, "status": "error"});
        let out = reset_account_state(&account);
        assert_eq!(out["rateLimitedUntil"], Value::Null);
        assert_eq!(out["backoffLevel"], json!(0));
        assert_eq!(out["lastError"], Value::Null);
        assert_eq!(out["status"], json!("active"));
        assert_eq!(out["id"], json!("a"));
    }

    #[test]
    fn apply_error_state_records_the_backoff_and_the_error() {
        let account = json!({"id": "a", "backoffLevel": 2});
        let out = apply_error_state(&account, 429, Some(&json!("rate limit")));
        assert_eq!(out["backoffLevel"], json!(3));
        assert_eq!(out["status"], json!("error"));
        assert_eq!(out["lastError"]["status"], json!(429));
        assert_eq!(out["lastError"]["message"], json!("rate limit"));
        assert!(out["rateLimitedUntil"].is_string());
    }

    #[test]
    fn apply_error_state_on_a_request_scoped_400_clears_the_cooldown() {
        let account =
            json!({"id": "a", "backoffLevel": 2, "rateLimitedUntil": "2030-01-01T00:00:00.000Z"});
        let out = apply_error_state(&account, 400, Some(&json!("bad body")));
        assert_eq!(out["rateLimitedUntil"], Value::Null);
        assert_eq!(out["backoffLevel"], json!(2));
    }

    #[test]
    fn retry_after_formats_human_units() {
        assert_eq!(format_retry_after(None), "");
        assert_eq!(format_retry_after(Some("")), "");
        let in_90 = get_unavailable_until(90_000);
        let text = format_retry_after(Some(&in_90));
        assert!(
            text == "reset after 1m 30s" || text == "reset after 1m 29s",
            "{text}"
        );
        assert_eq!(
            format_retry_after(Some("1970-01-01T00:00:00.000Z")),
            "reset after 0s"
        );
    }

    #[test]
    fn earliest_expiry_ignores_past_and_malformed_entries() {
        let past = "1970-01-01T00:00:00.000Z";
        let soon = get_unavailable_until(60_000);
        let later = get_unavailable_until(600_000);
        let accounts = json!([
            {"rateLimitedUntil": past},
            {"rateLimitedUntil": "not a date"},
            {"rateLimitedUntil": null},
            {"rateLimitedUntil": later},
            {"rateLimitedUntil": soon},
        ]);
        let earliest = get_earliest_rate_limited_until(accounts.as_array().unwrap()).unwrap();
        assert_eq!(earliest, soon);
    }

    #[test]
    fn unavailable_until_round_trips_through_the_iso_parser() {
        let iso = get_unavailable_until(60_000);
        assert!(is_account_unavailable(Some(&json!(iso))));
        assert!(!is_account_unavailable(Some(&json!("garbage"))));
        assert!(!is_account_unavailable(None));
    }
}
