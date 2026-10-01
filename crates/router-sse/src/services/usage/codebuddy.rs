//! CodeBuddy usage, scoped to `codebuddy-intl` — the only CodeBuddy provider in
//! the registry, so the shared handler takes no provider id.
//!
//! Quota sits behind a Tencent billing endpoint that wraps its payload twice
//! (`data.Response.Data`) and mixes two credit types that must not be merged:
//!
//! * **Refill / base packs** recur: their cycle ends long before the resource
//!   expires (`CycleEndTime << DeductionEndTime`), the live numbers live in the
//!   `*Cycle*` fields, and `resetAt` is the next monthly refresh.
//! * **Bonus packs** are one-shot: cycle end equals expiry, numbers live in the
//!   plain `*Capacity*` fields, and they never replenish.
//!
//! Hence the `recurring` flag on every row — the dashboard renders "Resets in"
//! for one and "Expires in" for the other, and getting it backwards promises a
//! refill that will not arrive.

use serde_json::{Map, Value, json};

use crate::executors::http::ProxyOptions;
use crate::providers::registry::registry;
use crate::services::usage::{Send, fetch, parse_reset_time, u_str};
use crate::translator::concerns::primitives::js_string;

/// `num(precise, plain)`: prefer the exact string field, fall back to the number.
fn num(precise: Option<&Value>, plain: Option<&Value>) -> f64 {
    let read = |v: Option<&Value>| -> Option<f64> {
        match v? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    };
    read(precise)
        .or_else(|| read(plain))
        .filter(|n| n.is_finite())
        .unwrap_or(0.0)
}

fn string_field<'a>(account: &'a Value, key: &str) -> Option<&'a str> {
    account
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `refillCadence(acc)`: a cycle of 1.5 days or less is daily, 10 days or less
/// weekly, anything longer monthly.
fn refill_cadence(account: &Value) -> &'static str {
    let start = parse_reset_time(account.get("CycleStartTime"));
    let end = parse_reset_time(account.get("CycleEndTime"));
    if let (Some(start), Some(end)) = (start, end) {
        let days = days_between(&start, &end);
        if let Some(days) = days {
            if days <= 1.5 {
                return "Daily";
            }
            if days <= 10.0 {
                return "Weekly";
            }
        }
    }
    "Monthly"
}

/// `(new Date(end) - new Date(start)) / 86400000` for two ISO timestamps.
fn days_between(start: &str, end: &str) -> Option<f64> {
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    let delta = parse(end)?.signed_duration_since(parse(start)?);
    Some(delta.num_milliseconds() as f64 / 86_400_000.0)
}

/// `cycleEndMs(acc)`: an unparseable value sorts last (`f64::INFINITY`).
fn cycle_end_ms(account: &Value) -> f64 {
    parse_reset_time(account.get("CycleEndTime"))
        .and_then(|iso| chrono::DateTime::parse_from_rfc3339(&iso).ok())
        .map(|dt| dt.timestamp_millis() as f64)
        .unwrap_or(f64::INFINITY)
}

/// `isRefill(acc)`: a cycle that ends more than two days before the resource
/// expires is a recurring allowance; the two ends coincide for a bonus pack.
fn is_refill(account: &Value) -> bool {
    const REFILL_GAP_MS: f64 = 2.0 * 24.0 * 60.0 * 60.0 * 1000.0;
    let cycle_end = cycle_end_ms(account);
    let deduction_end = account.get("DeductionEndTime").and_then(Value::as_f64);
    match deduction_end {
        Some(deduction_end) if cycle_end.is_finite() => deduction_end - cycle_end > REFILL_GAP_MS,
        _ => false,
    }
}

/// `PROVIDER_ID`.
const PROVIDER_ID: &str = "codebuddy-intl";

/// `getCodeBuddyUsage(providerId, accessToken, apiKey, providerSpecificData, proxyOptions)`.
async fn get_codebuddy_usage(
    access_token: Option<&str>,
    api_key: Option<&str>,
    proxy_options: &ProxyOptions,
) -> Value {
    let Some(token) = access_token
        .or(api_key)
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return json!({ "message": "CodeBuddy credential not available." });
    };

    let Some(url) = u_str(PROVIDER_ID, "url") else {
        return json!({ "message": "CodeBuddy quota endpoint is not configured." });
    };

    // The provider's own headers carry the IDE identity upstream validates.
    let mut headers: Vec<(&str, String)> = registry()
        .transport(PROVIDER_ID)
        .map(|t| {
            t.headers
                .iter()
                .flatten()
                .map(|(k, v)| (k.as_str(), js_string(v)))
                .collect()
        })
        .unwrap_or_default();
    headers.push(("Authorization", format!("Bearer {token}")));
    headers.push(("Content-Type", "application/json".to_string()));
    headers.push(("Accept", "application/json".to_string()));

    // The billing endpoint is a JSON POST and rejects a zero-byte body with 400,
    // so the literal `"{}"` is sent (`json!({})` serializes to exactly those two
    // bytes), keeping Content-Type: application/json consistent with it.
    let response = match fetch(&url, Send::Json(&json!({})), &headers, proxy_options).await {
        Ok(response) => response,
        Err(e) => return json!({ "message": format!("CodeBuddy error: {e}") }),
    };

    if response.status == 401 || response.status == 403 {
        return json!({ "message": "CodeBuddy credential invalid or expired." });
    }
    if !response.ok {
        return json!({ "message": format!("CodeBuddy quota API error ({}).", response.status) });
    }

    let Some(body) = response.json() else {
        return json!({ "message": "CodeBuddy error: response was not JSON" });
    };
    if body.get("code").and_then(Value::as_f64) != Some(0.0) {
        let msg = body.get("msg").and_then(Value::as_str).unwrap_or("unknown");
        return json!({ "message": format!("CodeBuddy quota error: {msg}") });
    }

    let data = body
        .get("data")
        .and_then(|d| d.get("Response"))
        .and_then(|r| r.get("Data"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let accounts = data
        .get("Accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if accounts.is_empty() {
        return json!({ "message": "CodeBuddy connected. No credit package found." });
    }

    let mut refills: Vec<&Value> = accounts.iter().filter(|a| is_refill(a)).collect();
    let mut bonuses: Vec<&Value> = accounts.iter().filter(|a| !is_refill(a)).collect();
    let by_expiry = |a: &&Value, b: &&Value| {
        cycle_end_ms(a)
            .partial_cmp(&cycle_end_ms(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    };
    refills.sort_by(by_expiry);
    bonuses.sort_by(by_expiry);

    let mut quotas = Map::new();

    // Refill packs first: cadence-labelled, read from the cycle balance, and
    // resetting at the next refresh rather than expiring.
    let mut seen: Map<String, Value> = Map::new();
    for account in &refills {
        let base = refill_cadence(account);
        let count = seen.get(base).and_then(Value::as_u64).unwrap_or(0) + 1;
        seen.insert(base.to_string(), json!(count));
        let name = if count > 1 {
            format!("{base} {count}")
        } else {
            base.to_string()
        };
        quotas.insert(
            name,
            json!({
                "used": num(account.get("CycleCapacityUsedPrecise"), account.get("CycleCapacityUsed")),
                "total": num(account.get("CycleCapacitySizePrecise"), account.get("CycleCapacitySize")),
                "resetAt": parse_reset_time(account.get("CycleEndTime")),
                "unlimited": false,
                "recurring": true,
            }),
        );
    }

    // Bonus packs: the lifetime balance, expiring for good.
    for (index, account) in bonuses.iter().enumerate() {
        quotas.insert(
            format!("Bonus Pack {}", index + 1),
            json!({
                "used": num(account.get("CapacityUsedPrecise"), account.get("CapacityUsed")),
                "total": num(account.get("CapacitySizePrecise"), account.get("CapacitySize")),
                "resetAt": parse_reset_time(account.get("CycleEndTime")),
                "unlimited": false,
                "recurring": false,
            }),
        );
    }

    let base_pkg = refills.first().copied().or_else(|| accounts.first());
    let plan = base_pkg
        .and_then(|p| string_field(p, "PackageName").or_else(|| string_field(p, "SubProductName")))
        .unwrap_or("CodeBuddy");

    json!({ "plan": plan, "quotas": Value::Object(quotas) })
}

/// `getCodeBuddyIntlUsage(accessToken, apiKey, providerSpecificData, proxyOptions)`.
pub async fn get_codebuddy_intl_usage(
    access_token: Option<&str>,
    api_key: Option<&str>,
    proxy_options: &ProxyOptions,
) -> Value {
    get_codebuddy_usage(access_token, api_key, proxy_options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `deduction_end` is epoch milliseconds. The ISO strings this endpoint also
    /// sends would coerce to `NaN`, which would make every pack a bonus pack, so
    /// the test fixture uses the numeric form.
    fn refill_pack(start: &str, end: &str, deduction_end: i64, used: f64, size: f64) -> Value {
        json!({
            "CycleStartTime": start,
            "CycleEndTime": end,
            "DeductionEndTime": deduction_end,
            "CycleCapacityUsedPrecise": used.to_string(),
            "CycleCapacitySizePrecise": size.to_string(),
            "CycleCapacityUsed": used,
            "CycleCapacitySize": size,
        })
    }

    #[test]
    fn a_cycle_that_ends_long_before_expiry_is_a_refill_pack() {
        // One month of cycle, a year of validity: a refill.
        let pack = refill_pack(
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            1_798_761_600_000, // 2027-01-01T00:00:00Z in ms
            6.54,
            500.0,
        );
        assert!(is_refill(&pack));
        assert_eq!(refill_cadence(&pack), "Monthly");

        // Cycle end equals expiry: a bonus pack.
        let bonus = json!({
            "CycleStartTime": "2026-01-01T00:00:00Z",
            "CycleEndTime": "2026-02-01T00:00:00Z",
            "DeductionEndTime": 1_769_904_000_000i64, // 2026-02-01T00:00:00Z in ms
        });
        assert!(!is_refill(&bonus));
    }

    #[test]
    fn a_missing_deduction_end_is_treated_as_a_bonus_pack() {
        // `Number.isFinite(ce) && Number.isFinite(de)` — an absent deduction end
        // fails that guard, so the pack is not a refill.
        let pack = json!({"CycleEndTime": "2026-02-01T00:00:00Z"});
        assert!(!is_refill(&pack));
        assert!(!is_refill(
            &json!({"CycleEndTime": "not-a-date", "DeductionEndTime": 1i64})
        ));
    }

    #[test]
    fn the_cadence_label_follows_the_cycle_length() {
        const YEAR: i64 = 1_798_761_600_000; // 2027-01-01T00:00:00Z in ms
        let daily = refill_pack(
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z",
            YEAR,
            0.0,
            1.0,
        );
        assert_eq!(refill_cadence(&daily), "Daily");
        let weekly = refill_pack(
            "2026-01-01T00:00:00Z",
            "2026-01-08T00:00:00Z",
            YEAR,
            0.0,
            1.0,
        );
        assert_eq!(refill_cadence(&weekly), "Weekly");
        let monthly = refill_pack(
            "2026-01-01T00:00:00Z",
            "2026-02-01T00:00:00Z",
            YEAR,
            0.0,
            1.0,
        );
        assert_eq!(refill_cadence(&monthly), "Monthly");
        // Unparseable dates fall back to Monthly rather than guessing.
        assert_eq!(refill_cadence(&json!({})), "Monthly");
    }

    #[test]
    fn the_precise_string_field_wins_over_the_number() {
        let account = json!({"CapacityUsedPrecise": "6.54", "CapacityUsed": 7});
        assert_eq!(
            num(
                account.get("CapacityUsedPrecise"),
                account.get("CapacityUsed")
            ),
            6.54
        );
        // A missing or unparseable precise field falls back.
        assert_eq!(num(None, account.get("CapacityUsed")), 7.0);
        assert_eq!(num(Some(&json!("nope")), account.get("CapacityUsed")), 7.0);
        assert_eq!(num(None, None), 0.0);
    }

    #[test]
    fn duplicate_cadences_are_numbered() {
        let accounts = [
            refill_pack(
                "2026-01-01T00:00:00Z",
                "2026-02-01T00:00:00Z",
                1_798_761_600_000,
                1.0,
                10.0,
            ),
            refill_pack(
                "2026-02-01T00:00:00Z",
                "2026-03-01T00:00:00Z",
                1_798_761_600_000,
                2.0,
                20.0,
            ),
        ];
        let mut refills: Vec<&Value> = accounts.iter().filter(|a| is_refill(a)).collect();
        refills.sort_by(|a, b| cycle_end_ms(a).partial_cmp(&cycle_end_ms(b)).unwrap());

        let mut quotas = Map::new();
        let mut seen: Map<String, Value> = Map::new();
        for account in &refills {
            let base = refill_cadence(account);
            let count = seen.get(base).and_then(Value::as_u64).unwrap_or(0) + 1;
            seen.insert(base.to_string(), json!(count));
            let name = if count > 1 {
                format!("{base} {count}")
            } else {
                base.to_string()
            };
            quotas.insert(
                name,
                json!({"used": num(account.get("CycleCapacityUsedPrecise"), None)}),
            );
        }
        assert_eq!(quotas.len(), 2);
        assert_eq!(quotas["Monthly"]["used"], json!(1.0));
        assert_eq!(quotas["Monthly 2"]["used"], json!(2.0));
    }

    #[test]
    fn packs_sort_by_expiry_with_unparseable_dates_last() {
        let a = json!({"CycleEndTime": "2026-03-01T00:00:00Z"});
        let b = json!({"CycleEndTime": "2026-01-01T00:00:00Z"});
        let c = json!({});
        let mut packs = [&a, &b, &c];
        packs.sort_by(|x, y| cycle_end_ms(x).partial_cmp(&cycle_end_ms(y)).unwrap());
        assert_eq!(cycle_end_ms(packs[0]), cycle_end_ms(&b));
        assert!(cycle_end_ms(packs[2]).is_infinite());
    }

    /// The billing endpoint rejects a zero-byte POST body with 400, so the
    /// literal `"{}"` is sent. `json!({})` must serialize to exactly those two
    /// bytes.
    #[test]
    fn the_quota_post_body_is_the_literal_empty_json_object() {
        assert_eq!(serde_json::to_string(&json!({})).unwrap(), "{}");
    }
}
