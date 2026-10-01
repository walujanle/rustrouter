//! Codex usage lookup.
//!
//! This is the one usage handler that **throws** instead of returning a
//! `{message}` payload, and the route depends on that: a Codex failure becomes
//! a 500, everything else a 200 with an explanatory message.
//!
//! The response shape is unusual in two ways. Rate limits arrive under three
//! different keys depending on the account (`rate_limit`, `rate_limits`, or
//! `rate_limits_by_limit_id.codex`), and the review and Spark pools are
//! separate buckets that must not be merged into the primary one — a review
//! window at 100% does not mean chat is exhausted.

use serde_json::{Map, Value, json};

use crate::executors::http::ProxyOptions;
use crate::executors::oauth::to_iso;
use crate::services::usage::{Send, fetch, parse_reset_time, to_finite_number, u_str};

/// `getCodexUsage(accessToken, proxyOptions)`: `Err` propagates as a 500.
pub async fn get_codex_usage(
    access_token: &str,
    proxy_options: &ProxyOptions,
) -> Result<Value, String> {
    let Some(usage_url) = u_str("codex", "url") else {
        return Err("Failed to fetch Codex usage: usage endpoint is not configured".to_string());
    };

    let headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
    ];
    let response = fetch(&usage_url, Send::Get, &headers, proxy_options)
        .await
        .map_err(|e| format!("Failed to fetch Codex usage: {e}"))?;

    if !response.ok {
        return Ok(json!({
            "message": format!("Codex connected. Usage API temporarily unavailable ({}).", response.status),
        }));
    }

    let data = response
        .json()
        .ok_or_else(|| "Failed to fetch Codex usage: response was not JSON".to_string())?;

    let normal_rate_limit = data
        .get("rate_limit")
        .or_else(|| data.get("rate_limits"))
        .or_else(|| {
            data.get("rate_limits_by_limit_id")
                .and_then(|m| m.get("codex"))
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    let review_rate_limit = get_codex_review_rate_limit(&data);
    let spark_rate_limit = get_codex_spark_rate_limit(&data);

    let available_reset_credits = to_finite_number(
        data.get("rate_limit_reset_credits")
            .and_then(|c| c.get("available_count")),
        0.0,
    )
    .max(0.0);

    let mut quotas = Map::new();
    append_codex_quota_windows(&mut quotas, "", &normal_rate_limit);
    append_codex_quota_windows(
        &mut quotas,
        "review",
        review_rate_limit.as_ref().unwrap_or(&Value::Null),
    );
    append_codex_quota_windows(
        &mut quotas,
        "spark",
        spark_rate_limit.as_ref().unwrap_or(&Value::Null),
    );

    let limit_reached = |snapshot: Option<&Value>| {
        get_codex_rate_limit_body(snapshot)
            .and_then(|b| b.get("limit_reached").and_then(Value::as_bool))
            .unwrap_or(false)
    };

    Ok(json!({
        "plan": data
            .get("plan_type")
            .or_else(|| data.get("summary").and_then(|s| s.get("plan")))
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        "limitReached": limit_reached(Some(&normal_rate_limit)),
        "reviewLimitReached": limit_reached(review_rate_limit.as_ref()),
        "sparkLimitReached": limit_reached(spark_rate_limit.as_ref()),
        "resetCredits": { "availableCount": available_reset_credits },
        "quotas": Value::Object(quotas),
    }))
}

/// `getCodexRateLimitBody(snapshot)`: a snapshot may nest its limits under
/// `rate_limit`, and a bare array is not a snapshot.
fn get_codex_rate_limit_body(snapshot: Option<&Value>) -> Option<&Value> {
    let snapshot = snapshot?;
    if !snapshot.is_object() {
        return None;
    }
    match snapshot.get("rate_limit") {
        Some(inner) if inner.is_object() => Some(inner),
        _ => Some(snapshot),
    }
}

/// `formatCodexWindow(window)`.
fn format_codex_window(window: &Value) -> Value {
    let used = to_finite_number(
        window
            .get("used_percent")
            .or_else(|| window.get("percent_used")),
        0.0,
    )
    .clamp(0.0, 100.0);
    json!({
        "used": used,
        "total": 100,
        "remaining": (100.0 - used).max(0.0),
        "resetAt": parse_reset_time(
            window.get("reset_at").or_else(|| window.get("resets_at")).or_else(|| window.get("resetAt"))
        ),
        "unlimited": false,
    })
}

/// `appendCodexQuotaWindows(quotas, prefix, snapshot)`: `session`/`weekly`, or
/// `<prefix>_session`/`<prefix>_weekly` for the review and Spark pools.
fn append_codex_quota_windows(
    quotas: &mut Map<String, Value>,
    prefix: &str,
    snapshot: &Value,
) -> bool {
    let Some(rate_limit) = get_codex_rate_limit_body(Some(snapshot)) else {
        return false;
    };

    let pick = |a: Option<&Value>,
                b: Option<&Value>,
                c: Option<&Value>,
                d: Option<&Value>|
     -> Option<Value> { a.or(b).or(c).or(d).cloned() };
    let primary = pick(
        rate_limit.get("primary_window"),
        rate_limit.get("primary"),
        snapshot.get("primary_window"),
        snapshot.get("primary"),
    );
    let secondary = pick(
        rate_limit.get("secondary_window"),
        rate_limit.get("secondary"),
        snapshot.get("secondary_window"),
        snapshot.get("secondary"),
    );

    let mut added = false;
    let key = |name: &str| {
        if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}_{name}")
        }
    };
    if let Some(primary) = primary {
        quotas.insert(key("session"), format_codex_window(&primary));
        added = true;
    }
    if let Some(secondary) = secondary {
        quotas.insert(key("weekly"), format_codex_window(&secondary));
        added = true;
    }
    added
}

/// `getCodexReviewRateLimit(data)`.
fn get_codex_review_rate_limit(data: &Value) -> Option<Value> {
    if let Some(limit) = data
        .get("code_review_rate_limit")
        .or_else(|| data.get("review_rate_limit"))
    {
        return Some(limit.clone());
    }
    if let Some(by_id) = data
        .get("rate_limits_by_limit_id")
        .and_then(Value::as_object)
    {
        return by_id
            .get("code_review")
            .or_else(|| by_id.get("codex_review"))
            .or_else(|| by_id.get("review"))
            .cloned();
    }
    additional_limit_matching(data, |id| {
        id == "code_review" || id == "codex_review" || id == "review" || id.contains("review")
    })
}

/// `getCodexSparkRateLimit(data)`.
fn get_codex_spark_rate_limit(data: &Value) -> Option<Value> {
    if let Some(limit) = data
        .get("spark_rate_limit")
        .or_else(|| data.get("gpt_5_3_codex_spark_rate_limit"))
    {
        return Some(limit.clone());
    }
    if let Some(by_id) = data
        .get("rate_limits_by_limit_id")
        .and_then(Value::as_object)
    {
        return by_id
            .get("gpt-5.3-codex-spark")
            .or_else(|| by_id.get("gpt_5_3_codex_spark"))
            .or_else(|| by_id.get("spark"))
            .cloned();
    }
    additional_limit_matching(data, |id| {
        id.contains("spark") || id.contains("5.3-codex-spark")
    })
}

/// The `additional_rate_limits` scan both lookups share: the first entry whose
/// `limit_name`/`metered_feature`/`id` matches.
fn additional_limit_matching(data: &Value, matches: impl Fn(&str) -> bool) -> Option<Value> {
    data.get("additional_rate_limits")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            let id = entry
                .get("limit_name")
                .or_else(|| entry.get("metered_feature"))
                .or_else(|| entry.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase();
            matches(&id)
        })
        .cloned()
}

// ─── reset credits ────────────────────────────────────────────────────────

/// `toIsoDate(value)`: a bare number below 1e12 is seconds.
fn to_iso_date(value: Option<&Value>) -> Option<String> {
    let ms = crate::executors::oauth::parse_time_ms(value)?;
    Some(to_iso(ms))
}

/// `errorMessage(value, fallback)`.
fn error_message(value: Option<&Value>, fallback: &str) -> String {
    match value {
        None | Some(Value::Null) => fallback.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => match o.get("message").and_then(Value::as_str) {
            Some(message) => message.to_string(),
            None => serde_json::to_string(value.unwrap()).unwrap_or_else(|_| fallback.to_string()),
        },
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| fallback.to_string()),
    }
}

/// `getCodexAccountId(providerSpecificData)`.
fn get_codex_account_id(provider_specific_data: Option<&Value>) -> Option<String> {
    let psd = provider_specific_data?.as_object()?;
    for key in ["workspaceId", "accountId", "chatgptAccountId"] {
        if let Some(value) = psd
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

/// `getCodexRateLimitResetCredits(accessToken, proxyOptions, providerSpecificData)`.
pub async fn get_codex_rate_limit_reset_credits(
    access_token: &str,
    proxy_options: &ProxyOptions,
    provider_specific_data: Option<&Value>,
) -> Result<Value, String> {
    if access_token.is_empty() {
        return Err(
            "No Codex access token available. Please re-authorize the connection.".to_string(),
        );
    }
    let Some(url) = u_str("codex", "resetCreditsUrl") else {
        return Err("Codex reset credits endpoint is not configured.".to_string());
    };

    let mut headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
        ("OpenAI-Beta", "codex-1".to_string()),
        ("originator", "codex_cli_rs".to_string()),
    ];
    if let Some(account_id) = get_codex_account_id(provider_specific_data) {
        headers.push(("ChatGPT-Account-ID", account_id));
    }

    let response = fetch(&url, Send::Get, &headers, proxy_options)
        .await
        .map_err(|e| format!("Codex reset credits API unavailable: {e}"))?;

    // A non-JSON body is not itself an error: the status decides.
    let data = response.json();

    if !response.ok {
        let detail = data.as_ref().and_then(|d| {
            d.get("message")
                .or_else(|| d.get("error"))
                .or_else(|| d.get("detail"))
        });
        return Err(error_message(
            detail,
            &format!("Codex reset credits API unavailable ({}).", response.status),
        ));
    }

    let credits = data
        .as_ref()
        .and_then(|d| d.get("credits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    Ok(json!({
        "availableCount": to_finite_number(
            data.as_ref().and_then(|d| d.get("available_count").or_else(|| d.get("availableCount"))),
            0.0,
        )
        .max(0.0),
        "credits": credits
            .iter()
            .map(|credit| json!({
                "status": credit.get("status").and_then(Value::as_str).unwrap_or("unknown"),
                "grantedAt": to_iso_date(credit.get("granted_at").or_else(|| credit.get("grantedAt"))),
                "expiresAt": to_iso_date(credit.get("expires_at").or_else(|| credit.get("expiresAt"))),
            }))
            .collect::<Vec<_>>(),
    }))
}

/// `consumeCodexRateLimitResetCredit(accessToken, redeemRequestId, proxyOptions)`.
///
/// Spending a credit is irreversible, so the result is reported rather than
/// interpreted: `ok` is true only when the server confirms the reset.
pub async fn consume_codex_rate_limit_reset_credit(
    access_token: &str,
    redeem_request_id: &str,
    proxy_options: &ProxyOptions,
) -> Result<Value, String> {
    if access_token.is_empty() {
        return Err(
            "No Codex access token available. Please re-authorize the connection.".to_string(),
        );
    }
    if redeem_request_id.is_empty() {
        return Err("A redeem request id is required to consume a Codex reset credit.".to_string());
    }
    let Some(url) = u_str("codex", "resetCreditsConsumeUrl") else {
        return Err("Codex reset credits endpoint is not configured.".to_string());
    };

    let headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
    ];
    let body = json!({ "redeem_request_id": redeem_request_id });
    let response = fetch(&url, Send::Json(&body), &headers, proxy_options)
        .await
        .map_err(|e| format!("Failed to consume Codex reset credit: {e}"))?;

    let data = response.json();
    let code = data
        .as_ref()
        .and_then(|d| d.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let windows_reset = to_finite_number(data.as_ref().and_then(|d| d.get("windows_reset")), 0.0);
    let success = response.ok && (code.as_deref() == Some("reset") || windows_reset > 0.0);

    Ok(json!({
        "ok": success,
        "noCredit": response.ok && code.as_deref() == Some("no_credit"),
        "status": response.status,
        "code": code,
        "windowsReset": windows_reset,
        "message": data.as_ref().and_then(|d| d.get("message")).cloned().unwrap_or(Value::Null),
        "raw": data.unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_read_either_percent_key_and_both_reset_spellings() {
        let a = format_codex_window(&json!({"used_percent": 30, "reset_at": 1_700_000_000}));
        assert_eq!(a["used"], json!(30.0));
        assert_eq!(a["remaining"], json!(70.0));
        assert_eq!(a["resetAt"], json!("2023-11-14T22:13:20.000Z"));

        let b =
            format_codex_window(&json!({"percent_used": 30, "resets_at": 1_700_000_000_000u64}));
        assert_eq!(b["used"], json!(30.0));
        assert_eq!(b["resetAt"], json!("2023-11-14T22:13:20.000Z"));
    }

    #[test]
    fn an_over_range_percent_is_clamped() {
        assert_eq!(
            format_codex_window(&json!({"used_percent": 130}))["used"],
            json!(100.0)
        );
        assert_eq!(
            format_codex_window(&json!({"used_percent": 130}))["remaining"],
            json!(0.0)
        );
        assert_eq!(
            format_codex_window(&json!({"used_percent": -5}))["used"],
            json!(0.0)
        );
    }

    #[test]
    fn quota_windows_are_prefixed_for_the_secondary_pools() {
        let mut quotas = Map::new();
        let snapshot = json!({"primary_window": {"used_percent": 10}, "secondary_window": {"used_percent": 20}});
        assert!(append_codex_quota_windows(&mut quotas, "", &snapshot));
        assert_eq!(quotas["session"]["used"], json!(10.0));
        assert_eq!(quotas["weekly"]["used"], json!(20.0));

        let mut quotas = Map::new();
        assert!(append_codex_quota_windows(&mut quotas, "review", &snapshot));
        assert_eq!(quotas["review_session"]["used"], json!(10.0));
        assert_eq!(quotas["review_weekly"]["used"], json!(20.0));
        assert!(!quotas.contains_key("session"));
    }

    #[test]
    fn a_nested_rate_limit_body_is_unwrapped() {
        let mut quotas = Map::new();
        let snapshot = json!({"rate_limit": {"primary": {"used_percent": 5}}});
        assert!(append_codex_quota_windows(&mut quotas, "", &snapshot));
        assert_eq!(quotas["session"]["used"], json!(5.0));
    }

    #[test]
    fn a_snapshot_with_no_windows_adds_nothing() {
        let mut quotas = Map::new();
        assert!(!append_codex_quota_windows(
            &mut quotas,
            "",
            &json!({"limit_reached": true})
        ));
        assert!(quotas.is_empty());
        // A null snapshot, and one that is not an object at all.
        assert!(!append_codex_quota_windows(&mut quotas, "", &Value::Null));
        assert!(!append_codex_quota_windows(&mut quotas, "", &json!([1, 2])));
        assert!(quotas.is_empty());
    }

    #[test]
    fn the_review_limit_is_found_by_key_then_by_id_then_by_scan() {
        assert_eq!(
            get_codex_review_rate_limit(&json!({"code_review_rate_limit": {"a": 1}})).unwrap()["a"],
            json!(1)
        );
        assert_eq!(
            get_codex_review_rate_limit(
                &json!({"rate_limits_by_limit_id": {"codex_review": {"b": 2}}})
            )
            .unwrap()["b"],
            json!(2)
        );
        // The scan lower-cases and matches on a substring.
        let scanned = get_codex_review_rate_limit(&json!({
            "additional_rate_limits": [{"limit_name": "Code_Review_Pool", "primary_window": {}}]
        }));
        assert!(scanned.is_some());
        assert!(
            get_codex_review_rate_limit(
                &json!({"additional_rate_limits": [{"limit_name": "other"}]})
            )
            .is_none()
        );
    }

    #[test]
    fn the_spark_limit_has_its_own_keys() {
        assert!(get_codex_spark_rate_limit(&json!({"spark_rate_limit": {}})).is_some());
        assert!(
            get_codex_spark_rate_limit(&json!({"gpt_5_3_codex_spark_rate_limit": {}})).is_some()
        );
        assert!(
            get_codex_spark_rate_limit(&json!({"rate_limits_by_limit_id": {"spark": {}}}))
                .is_some()
        );
        assert!(
            get_codex_spark_rate_limit(&json!({
                "additional_rate_limits": [{"metered_feature": "5.3-codex-spark"}]
            }))
            .is_some()
        );
        // A review pool must not be mistaken for the Spark pool.
        assert!(get_codex_spark_rate_limit(&json!({"code_review_rate_limit": {}})).is_none());
    }

    #[test]
    fn the_account_id_prefers_workspace_then_account_then_chatgpt() {
        let id = |v: Value| get_codex_account_id(Some(&v));
        assert_eq!(
            id(json!({"workspaceId": "w", "accountId": "a"})).as_deref(),
            Some("w")
        );
        assert_eq!(
            id(json!({"accountId": "a", "chatgptAccountId": "c"})).as_deref(),
            Some("a")
        );
        assert_eq!(id(json!({"chatgptAccountId": "c"})).as_deref(), Some("c"));
        assert_eq!(id(json!({"workspaceId": ""})), None);
        assert_eq!(get_codex_account_id(None), None);
    }

    #[test]
    fn error_messages_read_a_string_a_message_field_or_the_whole_value() {
        assert_eq!(error_message(Some(&json!("plain")), "fb"), "plain");
        assert_eq!(
            error_message(Some(&json!({"message": "inner"})), "fb"),
            "inner"
        );
        assert_eq!(
            error_message(Some(&json!({"other": 1})), "fb"),
            "{\"other\":1}"
        );
        assert_eq!(error_message(Some(&json!(42)), "fb"), "42");
        assert_eq!(error_message(None, "fb"), "fb");
        assert_eq!(error_message(Some(&Value::Null), "fb"), "fb");
    }

    #[test]
    fn credit_dates_accept_seconds_milliseconds_and_iso() {
        assert_eq!(
            to_iso_date(Some(&json!(1_700_000_000))).as_deref(),
            Some("2023-11-14T22:13:20.000Z")
        );
        assert_eq!(
            to_iso_date(Some(&json!(1_700_000_000_000u64))).as_deref(),
            Some("2023-11-14T22:13:20.000Z")
        );
        assert_eq!(
            to_iso_date(Some(&json!("2023-11-14T22:13:20Z"))).as_deref(),
            Some("2023-11-14T22:13:20.000Z")
        );
        assert!(to_iso_date(None).is_none());
        assert!(to_iso_date(Some(&json!("nope"))).is_none());
    }
}
