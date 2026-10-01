//! Grok CLI usage: REST billing plus the gRPC-web quota frame decoder.
//!
//! Two data sources that disagree with each other, which is why both are here:
//!
//! * REST billing (`/v1/billing?format=credits`) returns protobuf-json
//!   `{ val: n }` envelopes. Exhausted free/promo accounts come back as
//!   `cap=0/used=0/prepaid=0`, and the dashboard reads `total === 0` as
//!   "unlimited" — so that state is reported as a synthetic 1/1 depleted row
//!   rather than a zero-total bar.
//! * Paid SuperGrok often returns `cap=0` over REST while still exposing the
//!   shared weekly pool through `GetGrokCreditsConfig`, a gRPC-web endpoint with
//!   a binary response. Hence the hand-rolled protobuf reader at the bottom:
//!   there is no .proto for it and the schema is two fields deep.
//!
//! No quota row here carries an absolute `remaining`: the dashboard's
//! `getRemainingPercentage` treats that key as a 0–100 percentage.

use serde_json::{Map, Value, json};

use crate::executors::http::ProxyOptions;
use crate::executors::oauth::to_iso;
use crate::services::usage::{
    Send, fetch, parse_reset_time, to_finite_number, u_str, unwrap_val_or,
};
use crate::translator::concerns::primitives::{js_json_number, js_truthy_opt};

/// `GRPC_CREDITS_URL` — not in the registry: it belongs to grok.com rather than
/// the CLI proxy the registry describes.
const GRPC_CREDITS_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";

/// The empty gRPC-web request frame (flag 0 + length 0). Without it upstream
/// answers `grpc-status 13 "Missing request message."` with a 0-byte body.
const GRPC_WEB_EMPTY_REQUEST_FRAME: [u8; 5] = [0, 0, 0, 0, 0];

/// `GROK_CLI_USER_AGENT`.
fn user_agent() -> String {
    format!(
        "grok-shell/{} (linux; x86_64)",
        crate::executors::grok_cli::GROK_CLI_VERSION
    )
}

/// `buildGrokCliHeaders(accessToken, providerSpecificData)`.
fn build_headers(
    access_token: &str,
    provider_specific_data: Option<&Value>,
) -> Vec<(&'static str, String)> {
    let psd = provider_specific_data.and_then(Value::as_object);
    let psd_str = |key: &str| {
        psd.and_then(|m| m.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };

    let mut headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Accept", "application/json".to_string()),
        ("User-Agent", user_agent()),
        ("x-xai-token-auth", "xai-grok-cli".to_string()),
        (
            "x-grok-client-identifier",
            crate::executors::grok_cli::GROK_CLI_CLIENT_IDENTIFIER.to_string(),
        ),
        (
            "x-grok-client-version",
            crate::executors::grok_cli::GROK_CLI_VERSION.to_string(),
        ),
        ("x-grok-client-mode", "headless".to_string()),
    ];
    if let Some(email) = psd_str("email") {
        headers.push(("x-email", email));
    }
    if let Some(user_id) = psd_str("userId").or_else(|| psd_str("principalId")) {
        headers.push(("x-userid", user_id));
    }
    headers
}

/// `subscriptionTier(user, config)`.
///
/// The key is resolved with nullish coalescing first (`subscriptionTier ??
/// subscription_tier ?? subscription?.tier ?? config?.…`) and only then coerced
/// with `typeof rawTier === "string" ? rawTier.trim() : ""`. So a
/// present-but-non-string value *stops* the chain and yields `""`; it does not
/// fall through to the next key. The resolution and the coercion are separate
/// steps here for the same reason.
fn subscription_tier(user: Option<&Value>, config: &Value) -> String {
    let candidates: [Option<&Value>; 5] = [
        user.and_then(|u| u.get("subscriptionTier")),
        user.and_then(|u| u.get("subscription_tier")),
        user.and_then(|u| u.get("subscription"))
            .and_then(|s| s.get("tier")),
        config.get("subscriptionTier"),
        config.get("subscription_tier"),
    ];
    let raw = candidates
        .into_iter()
        .find(|v| !matches!(v, None | Some(Value::Null)));
    match raw.flatten() {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// `resolvePlan(user, config)`: the tier with `_`/`-` folded to spaces and each
/// word capitalised, because the raw values are `super_grok_heavy`.
fn resolve_plan(user: Option<&Value>, config: &Value) -> String {
    let tier = subscription_tier(user, config);
    if !tier.is_empty() {
        let spaced = tier.replace(['_', '-'], " ");
        let spaced = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
        return spaced
            .split(' ')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }
    if user
        .and_then(|u| u.get("hasGrokCodeAccess"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return "Grok Code".to_string();
    }
    "Grok Build".to_string()
}

/// `planFromAccessToken(accessToken)`: display-only tier read out of the JWT.
///
/// The upstream remains authoritative for access and quota; this only fills in
/// a plan name when the billing payload has nothing better.
fn plan_from_access_token(access_token: &str) -> String {
    let Some(payload) = access_token.split('.').nth(1) else {
        return String::new();
    };
    let Some(bytes) = decode_base64url(payload) else {
        return String::new();
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&bytes) else {
        return String::new();
    };
    match claims.get("tier").and_then(Value::as_f64) {
        Some(0.0) => "Free",
        Some(1.0) => "SuperGrok",
        Some(2.0) => "X Basic",
        Some(3.0) => "X Premium",
        Some(4.0) => "X Premium Plus",
        Some(5.0) => "SuperGrok Heavy",
        Some(6.0) => "SuperGrok Lite",
        _ => "",
    }
    .to_string()
}

/// Node's `Buffer.from(s, "base64url")` is lenient about padding.
fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let trimmed = input.trim_end_matches('=');
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(trimmed)
        .ok()
        .or_else(|| {
            base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(trimmed)
                .ok()
        })
}

/// `makeQuota({used, total, resetAt, unlimited})`.
fn make_quota(used: f64, total: f64, reset_at: Option<String>, unlimited: bool) -> Value {
    let safe_total = to_finite_number(Some(&json!(total)), 0.0).max(0.0);
    let safe_used = used.max(0.0);
    let reset = reset_at.map_or(Value::Null, Value::String);

    if unlimited || safe_total == 0.0 {
        // `total: 0` even for an unlimited row with a real total — the dashboard
        // reads a zero total as "no bar", which is what unlimited means.
        return json!({
            "used": js_json_number(safe_used),
            "total": 0,
            "remainingPercentage": if unlimited { 100 } else { 0 },
            "resetAt": reset,
            "unlimited": true,
        });
    }
    let remaining = (safe_total - safe_used).max(0.0);
    json!({
        "used": js_json_number(safe_used),
        "total": js_json_number(safe_total),
        "remainingPercentage": js_json_number((remaining / safe_total) * 100.0),
        "resetAt": reset,
        "unlimited": false,
    })
}

/// `parseGrokCliBilling(billing, user)`.
pub fn parse_grok_cli_billing(billing: &Value, user: Option<&Value>) -> Value {
    let root = billing
        .as_object()
        .map_or_else(|| json!({}), |_| billing.clone());
    let config = match root.get("config") {
        Some(c) if c.is_object() => c.clone(),
        _ => root.clone(),
    };

    let first_reset =
        |candidates: &[Option<&Value>]| candidates.iter().find_map(|v| parse_reset_time(*v));
    // `config.resetAt || config.resetsAt || config.periodEnd` — `||` falls
    // through on null *and* empty/0, so the three are grouped before the parse
    // rather than tried as separate candidates.
    let config_reset = pick_truthy_raw(&config, &["resetAt", "resetsAt", "periodEnd"]);
    let root_reset = pick_truthy_raw(&root, &["resetAt", "resetsAt", "periodEnd"]);
    let period_end = first_reset(&[
        config.get("billingPeriodEnd"),
        config.get("billing_period_end"),
        config.get("currentPeriod").and_then(|p| p.get("end")),
        config_reset.as_ref(),
        root.get("billingPeriodEnd"),
        root.get("billing_period_end"),
        root_reset.as_ref(),
    ]);

    let mut quotas = Map::new();
    let tier = subscription_tier(user, &config);
    let subscription_access =
        !tier.is_empty() && !matches!(tier.to_lowercase().as_str(), "free" | "none" | "null");

    // `config.monthlyLimit ?? config.monthly_limit ?? root.monthlyLimit ??
    // root.monthly_limit` — every *config* key is tried before any *root* key,
    // and `??` only falls through on null/absent, so a literal 0 survives and is
    // then rejected by the `> 0` guard below.
    let pick_num = |keys: &[&str]| -> Option<f64> {
        let config_value = pick_from_raw(&config, keys);
        let root_value = pick_from_raw(&root, keys);
        config_value
            .or(root_value)
            .map(|v| unwrap_val_or(Some(&v), f64::NAN))
            .filter(|n| n.is_finite())
    };

    let monthly_limit = pick_num(&["monthlyLimit", "monthly_limit"]);
    let included_used = pick_num(&["includedUsed", "included_used"]);
    let total_used = pick_num(&["totalUsed", "total_used"]);
    if let Some(limit) = monthly_limit.filter(|v| *v > 0.0) {
        let used = included_used.or(total_used).unwrap_or(0.0);
        quotas.insert(
            "Monthly included".to_string(),
            make_quota(used, limit, period_end.clone(), false),
        );
    }

    let on_demand_cap = pick_num(&["onDemandCap"]);
    let on_demand_used = pick_num(&["onDemandUsed"]);
    match on_demand_cap {
        Some(cap) if cap > 0.0 => {
            let used = on_demand_used
                .filter(|v| v.is_finite())
                .map_or(0.0, |v| v.max(0.0));
            quotas.insert(
                "On-demand".to_string(),
                make_quota(used, cap, period_end.clone(), false),
            );
        }
        // Cap 0 on a non-subscription account is the exhausted free/promo state;
        // chat answers 402 `personal-team-blocked:spending-limit`. A zero-total
        // row would read as "unlimited", so report 1/1 at 0%.
        Some(0.0) if !subscription_access && on_demand_used.is_some() => {
            quotas.insert(
                "On-demand".to_string(),
                json!({
                    "used": 1,
                    "total": 1,
                    "remainingPercentage": 0,
                    "resetAt": period_end.clone().map_or(Value::Null, Value::String),
                    "unlimited": false,
                }),
            );
        }
        _ => {}
    }

    if let Some(prepaid) = pick_num(&["prepaidBalance"]).filter(|v| *v > 0.0) {
        quotas.insert(
            "Prepaid".to_string(),
            json!({
                "used": 0,
                "total": js_json_number(prepaid),
                "remainingPercentage": 100,
                "resetAt": Value::Null,
                "unlimited": false,
            }),
        );
    }

    // SuperGrok's shared weekly pool. `creditUsagePercent` is the whole used
    // percentage; `productUsage` is a legend for it, not separate quotas.
    if let Some(used_pct) = pick_num(&["creditUsagePercent", "credit_usage_percent"])
        && used_pct >= 0.0
    {
        quotas.insert(
            "Weekly SuperGrok".to_string(),
            make_quota(used_pct.clamp(0.0, 100.0), 100.0, period_end.clone(), false),
        );
    }

    // Richer credit envelopes, parsed opportunistically for account types whose
    // billing shape is not the one captured above.
    let bags = [
        root.get("credits"),
        root.get("creditBalance"),
        root.get("usage"),
        config.get("credits"),
        config.get("includedCredits"),
        config.get("subscriptionCredits"),
    ];
    for bag in bags.into_iter().flatten().filter(|b| b.is_object()) {
        let total = pick_from(bag, &["total", "limit", "cap", "allocation", "amount"]);
        let used = pick_from(bag, &["used", "spent", "consumed"]);
        let remaining = pick_from(bag, &["remaining", "balance", "left"]);
        if quotas.contains_key("Credits") {
            continue;
        }
        if let Some(total) = total.filter(|v| *v > 0.0) {
            let resolved_used = used
                .or_else(|| remaining.map(|r| (total - r).max(0.0)))
                .unwrap_or(0.0);
            // `parseResetTime(bag.resetAt || bag.resetsAt || bag.end) || periodEnd`
            // — the `||` group picks the first truthy key before parsing.
            let reset = pick_truthy_raw(bag, &["resetAt", "resetsAt", "end"])
                .and_then(|v| parse_reset_time(Some(&v)))
                .or_else(|| period_end.clone());
            quotas.insert(
                "Credits".to_string(),
                make_quota(resolved_used, total, reset, false),
            );
        } else if let Some(remaining) = remaining.filter(|v| *v >= 0.0) {
            quotas.insert(
                "Credits".to_string(),
                json!({
                    "used": 0,
                    "total": js_json_number(if remaining > 0.0 { remaining } else { 1.0 }),
                    "remainingPercentage": if remaining > 0.0 { 100 } else { 0 },
                    "resetAt": period_end.clone().map_or(Value::Null, Value::String),
                    "unlimited": false,
                }),
            );
        }
    }

    // Exhausted when every finite bar sits at 0% remaining.
    let exhausted = !quotas.is_empty()
        && quotas.values().all(|q| {
            q.get("unlimited").and_then(Value::as_bool) != Some(true)
                && q.get("remainingPercentage")
                    .and_then(Value::as_f64)
                    .unwrap_or(100.0)
                    <= 0.0
        });

    json!({
        "plan": resolve_plan(user, &config),
        "quotas": Value::Object(quotas),
        "periodEnd": period_end.map_or(Value::Null, Value::String),
        "exhausted": exhausted,
        "subscriptionAccess": subscription_access,
    })
}

/// `unwrapVal(bag.a ?? bag.b …, NaN)`.
fn pick_from(bag: &Value, keys: &[&str]) -> Option<f64> {
    let value = pick_from_raw(bag, keys)?;
    let n = unwrap_val_or(Some(&value), f64::NAN);
    n.is_finite().then_some(n)
}

/// `bag.a ?? bag.b ?? bag.c` — `??` falls through only on null/absent, so a
/// present `0` wins and is later rejected by the caller's `> 0` guard.
fn pick_from_raw(bag: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter()
        .map(|k| bag.get(*k))
        .find(|v| !matches!(v, None | Some(Value::Null)))
        .flatten()
        .cloned()
}

/// `bag.a || bag.b || bag.c` — `||` also falls through on `0`/`""`, so the first
/// *truthy* key wins. The reset-time pick uses this, not `??`.
fn pick_truthy_raw(bag: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter()
        .find(|k| js_truthy_opt(bag.get(**k)))
        .and_then(|k| bag.get(*k))
        .cloned()
}

/// `quotasFromGrpcCredits(decoded)`.
fn quotas_from_grpc_credits(decoded: Option<GrokCredits>) -> Option<Value> {
    let decoded = decoded?;
    if !decoded.percent_used.is_finite() {
        return None;
    }
    // fixed32 ratio * 100 lands on 34.999… for 0.35.
    let used = decoded.percent_used.clamp(0.0, 100.0).round();
    let mut quotas = Map::new();
    quotas.insert(
        "Weekly SuperGrok".to_string(),
        make_quota(used, 100.0, decoded.reset_at, false),
    );
    Some(Value::Object(quotas))
}

/// `fetchGrokCliCreditsConfig(accessToken, proxyOptions)`. Fail-open: any
/// network, auth or parse failure is `None`.
async fn fetch_grok_cli_credits_config(
    access_token: &str,
    proxy_options: &ProxyOptions,
) -> Option<GrokCredits> {
    if access_token.is_empty() {
        return None;
    }
    let headers = vec![
        ("Authorization", format!("Bearer {access_token}")),
        ("Content-Type", "application/grpc-web+proto".to_string()),
        ("X-Grpc-Web", "1".to_string()),
        ("Accept", "application/grpc-web+proto".to_string()),
    ];
    let response = fetch(
        GRPC_CREDITS_URL,
        Send::Raw {
            bytes: &GRPC_WEB_EMPTY_REQUEST_FRAME,
            content_type: "application/grpc-web+proto",
        },
        &headers,
        proxy_options,
    )
    .await
    .ok()?;
    if !response.ok {
        return None;
    }
    decode_grok_credits_frame(response.bytes())
}

/// `getGrokCliUsage(accessToken, providerSpecificData, proxyOptions)`.
pub async fn get_grok_cli_usage(
    access_token: &str,
    provider_specific_data: Option<&Value>,
    proxy_options: &ProxyOptions,
) -> Value {
    if access_token.is_empty() {
        return json!({ "message": "Grok CLI access token not available." });
    }

    let Some(billing_url) = u_str("grok-cli", "url") else {
        return json!({ "message": "Grok CLI usage error: billing endpoint is not configured." });
    };
    let user_url = u_str("grok-cli", "userUrl");
    let headers = build_headers(access_token, provider_specific_data);

    // The profile fetch is best-effort: the official CLI fires both at startup
    // and tolerates a failing user lookup.
    let (billing_response, user_response) = tokio::join!(
        fetch(&billing_url, Send::Get, &headers, proxy_options),
        async {
            match &user_url {
                Some(url) => fetch(url, Send::Get, &headers, proxy_options).await.ok(),
                None => None,
            }
        }
    );

    let billing_response = match billing_response {
        Ok(response) => response,
        Err(e) => return json!({ "message": format!("Grok CLI usage error: {e}") }),
    };

    if billing_response.status == 401 || billing_response.status == 403 {
        return json!({ "message": "Grok CLI authentication expired. Please re-authorize." });
    }
    if !billing_response.ok {
        let text = billing_response.text();
        let trimmed = if text.is_empty() {
            String::new()
        } else {
            format!(": {}", text.chars().take(200).collect::<String>())
        };
        return json!({ "message": format!("Grok CLI billing API error ({}){trimmed}", billing_response.status) });
    }

    let Some(billing) = billing_response.json() else {
        return json!({ "message": "Grok CLI billing response was not JSON." });
    };
    if !billing.is_object() {
        return json!({ "message": "Grok CLI billing response was not JSON." });
    }

    let user = user_response.filter(|r| r.ok).and_then(|r| r.json());

    let mut parsed = parse_grok_cli_billing(&billing, user.as_ref());
    let plan = plan_from_access_token(access_token);
    if !plan.is_empty() {
        parsed["plan"] = Value::String(plan);
    }

    let empty_quotas = parsed["quotas"].as_object().is_none_or(Map::is_empty);
    if !empty_quotas {
        // The dashboard hides the quota table whenever `message` is set, so a
        // depleted account keeps its 0% bar without a blocking message.
        return json!({ "plan": parsed["plan"], "quotas": parsed["quotas"] });
    }

    // Paid SuperGrok often answers `cap=0` over REST while exposing the shared
    // weekly pool here — try it before declaring the account exhausted.
    let grpc = fetch_grok_cli_credits_config(access_token, proxy_options).await;
    if let Some(grpc_quotas) = quotas_from_grpc_credits(grpc) {
        return json!({ "plan": parsed["plan"], "quotas": grpc_quotas });
    }

    let message = if parsed["subscriptionAccess"].as_bool() == Some(true) {
        "Subscription access is active; Grok does not expose a numeric included quota."
    } else {
        "Grok Build connected, but no credit allotment was returned. Free promo may be exhausted."
    };
    json!({ "plan": parsed["plan"], "message": message, "quotas": {} })
}

// ─── gRPC-web frame decoder ───────────────────────────────────────────────

/// A decoded `GetGrokCreditsConfig` response.
#[derive(Debug, Clone, PartialEq)]
pub struct GrokCredits {
    pub percent_used: f64,
    pub reset_at: Option<String>,
}

const FIELD_CREDITS_INFO: u32 = 1;
const CREDITS_FIELD_USAGE_RATIO: u32 = 1;
const CREDITS_FIELD_RESET_TIMESTAMP: u32 = 5;
const TIMESTAMP_FIELD_SECONDS: u32 = 1;
const TIMESTAMP_FIELD_NANOS: u32 = 2;

const WIRE_TYPE_VARINT: u32 = 0;
const WIRE_TYPE_FIXED64: u32 = 1;
const WIRE_TYPE_LENGTH_DELIMITED: u32 = 2;
const WIRE_TYPE_FIXED32: u32 = 5;

/// A protobuf field, as far as this decoder needs one.
#[derive(Debug, Clone)]
enum Field {
    Varint(f64),
    Fixed64([u8; 8]),
    Fixed32([u8; 4]),
    Bytes(Vec<u8>),
}

/// `probeFrameHeader(buffer, offset)`.
fn probe_frame_header(buffer: &[u8], offset: usize) -> Option<(u8, usize, usize)> {
    if offset > buffer.len() || buffer.len() - offset < 5 {
        return None;
    }
    let flag = buffer[offset];
    if !matches!(flag, 0x00 | 0x01 | 0x80 | 0x81) {
        return None;
    }
    let payload_start = offset + 5;
    let payload_length = u32::from_be_bytes([
        buffer[offset + 1],
        buffer[offset + 2],
        buffer[offset + 3],
        buffer[offset + 4],
    ]) as usize;
    if payload_length > buffer.len() - payload_start {
        return None;
    }
    Some((flag, payload_start, payload_length))
}

/// `readVarint(buffer, offset)`: value and the offset just past it.
///
/// The value is carried as `f64` for the precision behaviour on the fields that
/// matter (a tag and a timestamp's seconds).
fn read_varint(buffer: &[u8], offset: usize) -> Option<(f64, usize)> {
    let mut result: u128 = 0;
    let mut shift: u32 = 0;
    let mut pos = offset;
    loop {
        if pos >= buffer.len() {
            return None;
        }
        let byte = buffer[pos];
        result |= ((byte & 0x7f) as u128) << shift;
        pos += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        // `MAX_VARINT_SHIFT_BITS = 70n`.
        if shift > 70 {
            return None;
        }
    }
    Some((result as f64, pos))
}

/// `readField(buffer, offset)`.
fn read_field(buffer: &[u8], offset: usize) -> Option<(u32, Field, usize)> {
    let (tag, after_tag) = read_varint(buffer, offset)?;
    // `value >>> 3` / `value & 7` are 32-bit operations in JS.
    let tag = tag as u64 as u32;
    let field_number = tag >> 3;
    let wire_type = tag & 0x7;
    if field_number == 0 {
        return None;
    }

    match wire_type {
        WIRE_TYPE_VARINT => {
            let (value, next) = read_varint(buffer, after_tag)?;
            Some((field_number, Field::Varint(value), next))
        }
        WIRE_TYPE_LENGTH_DELIMITED => {
            let (length, body_start) = read_varint(buffer, after_tag)?;
            let length = length as usize;
            if body_start + length > buffer.len() {
                return None;
            }
            Some((
                field_number,
                Field::Bytes(buffer[body_start..body_start + length].to_vec()),
                body_start + length,
            ))
        }
        WIRE_TYPE_FIXED64 => {
            if after_tag + 8 > buffer.len() {
                return None;
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&buffer[after_tag..after_tag + 8]);
            Some((field_number, Field::Fixed64(bytes), after_tag + 8))
        }
        WIRE_TYPE_FIXED32 => {
            if after_tag + 4 > buffer.len() {
                return None;
            }
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&buffer[after_tag..after_tag + 4]);
            Some((field_number, Field::Fixed32(bytes), after_tag + 4))
        }
        _ => None,
    }
}

/// `decodeFields(buffer)`: field number to field, last one wins.
fn decode_fields(buffer: &[u8]) -> Option<Map<String, Value>> {
    let mut fields: Map<String, Value> = Map::new();
    let mut offset = 0;
    while offset < buffer.len() {
        let (number, field, next) = read_field(buffer, offset)?;
        fields.insert(number.to_string(), encode_field(&field));
        offset = next;
    }
    Some(fields)
}

/// The decoded fields are carried as `Value` so the map type stays `serde_json`'s
/// `Map`; these two tags are the only shapes the walk below reads.
const TAG_VARINT: &str = "__varint";
const TAG_FIXED32: &str = "__fixed32";
const TAG_FIXED64: &str = "__fixed64";
const TAG_BYTES: &str = "__bytes";

fn encode_field(field: &Field) -> Value {
    match field {
        Field::Varint(v) => json!({ TAG_VARINT: v }),
        Field::Fixed32(b) => json!({ TAG_FIXED32: b.to_vec() }),
        Field::Fixed64(b) => json!({ TAG_FIXED64: b.to_vec() }),
        Field::Bytes(b) => json!({ TAG_BYTES: b }),
    }
}

fn field_wire_type(value: &Value) -> Option<&str> {
    let obj = value.as_object()?;
    obj.keys().next().map(String::as_str)
}

/// `findDataFramePayload(buffer)`: the first non-trailer frame's payload.
fn find_data_frame_payload(buffer: &[u8]) -> Option<&[u8]> {
    let mut offset = 0;
    while offset < buffer.len() {
        let (flag, payload_start, payload_length) = probe_frame_header(buffer, offset)?;
        let frame_end = payload_start + payload_length;
        let is_trailer = flag & 0x80 != 0;
        if !is_trailer {
            return Some(&buffer[payload_start..frame_end]);
        }
        offset = frame_end;
    }
    None
}

/// `extractNestedMessage(field)`.
fn extract_nested_message(fields: &Map<String, Value>, number: u32) -> Option<Map<String, Value>> {
    let field = fields.get(&number.to_string())?;
    if field_wire_type(field)? != TAG_BYTES {
        return None;
    }
    let bytes = field[TAG_BYTES].as_array()?;
    let bytes: Vec<u8> = bytes
        .iter()
        .filter_map(|b| b.as_u64().map(|n| n as u8))
        .collect();
    decode_fields(&bytes)
}

/// `extractUsageRatio(field)`: absent means 0% used (proto3 omission).
fn extract_usage_ratio(fields: &Map<String, Value>, number: u32) -> Option<f64> {
    let Some(field) = fields.get(&number.to_string()) else {
        return Some(0.0);
    };
    let bytes: Vec<u8> = field[field_wire_type(field)?]
        .as_array()?
        .iter()
        .filter_map(|b| b.as_u64().map(|n| n as u8))
        .collect();
    match field_wire_type(field)? {
        TAG_FIXED32 if bytes.len() == 4 => {
            Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        }
        TAG_FIXED64 if bytes.len() == 8 => Some(f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])),
        _ => None,
    }
}

/// `extractResetAt(field)`: a `Timestamp{seconds, nanos}`.
fn extract_reset_at(fields: &Map<String, Value>, number: u32) -> Option<String> {
    let nested = extract_nested_message(fields, number)?;
    let seconds = nested
        .get(&TIMESTAMP_FIELD_SECONDS.to_string())
        .and_then(|f| f[TAG_VARINT].as_f64())
        .unwrap_or(0.0);
    let nanos = nested
        .get(&TIMESTAMP_FIELD_NANOS.to_string())
        .and_then(|f| f[TAG_VARINT].as_f64())
        .unwrap_or(0.0);
    let millis = seconds * 1000.0 + (nanos / 1_000_000.0).round();
    // `new Date(millis)` is Invalid for |ms| beyond the ECMAScript range.
    if !millis.is_finite() || millis.abs() > 8.64e15 {
        return None;
    }
    Some(to_iso(millis as i64))
}

/// `decodeGrokCreditsFrame(buffer)`.
pub fn decode_grok_credits_frame(buffer: &[u8]) -> Option<GrokCredits> {
    if buffer.is_empty() {
        return None;
    }

    let framed = probe_frame_header(buffer, 0).is_some();
    let payload = if framed {
        find_data_frame_payload(buffer)?
    } else {
        buffer
    };

    let top_level = decode_fields(payload)?;
    let credits_info = extract_nested_message(&top_level, FIELD_CREDITS_INFO)?;

    let usage_ratio = extract_usage_ratio(&credits_info, CREDITS_FIELD_USAGE_RATIO)?;
    if !usage_ratio.is_finite() || usage_ratio < 0.0 {
        return None;
    }

    Some(GrokCredits {
        percent_used: (usage_ratio * 100.0).min(100.0),
        reset_at: extract_reset_at(&credits_info, CREDITS_FIELD_RESET_TIMESTAMP),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── fixtures: a minimal protobuf encoder for the decoder tests ──────────

    fn encode_varint(value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut v = value;
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if v == 0 {
                break;
            }
        }
        bytes
    }

    fn encode_tag(field_number: u32, wire_type: u32) -> Vec<u8> {
        encode_varint(((field_number << 3) | wire_type) as u64)
    }

    fn encode_fixed32_field(field_number: u32, value: f32) -> Vec<u8> {
        let mut out = encode_tag(field_number, WIRE_TYPE_FIXED32);
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    fn encode_length_delimited(field_number: u32, body: &[u8]) -> Vec<u8> {
        let mut out = encode_tag(field_number, WIRE_TYPE_LENGTH_DELIMITED);
        out.extend_from_slice(&encode_varint(body.len() as u64));
        out.extend_from_slice(body);
        out
    }

    fn encode_varint_field(field_number: u32, value: u64) -> Vec<u8> {
        let mut out = encode_tag(field_number, WIRE_TYPE_VARINT);
        out.extend_from_slice(&encode_varint(value));
        out
    }

    fn encode_timestamp_field(field_number: u32, seconds: u64, nanos: u64) -> Vec<u8> {
        let mut body = Vec::new();
        if seconds != 0 {
            body.extend_from_slice(&encode_varint_field(1, seconds));
        }
        if nanos != 0 {
            body.extend_from_slice(&encode_varint_field(2, nanos));
        }
        encode_length_delimited(field_number, &body)
    }

    struct CreditsShape {
        usage_ratio: Option<f32>,
        reset: Option<(u64, u64)>,
    }

    fn encode_credits_info(shape: &CreditsShape) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(ratio) = shape.usage_ratio {
            out.extend_from_slice(&encode_fixed32_field(1, ratio));
        }
        if let Some((seconds, nanos)) = shape.reset {
            out.extend_from_slice(&encode_timestamp_field(5, seconds, nanos));
        }
        out
    }

    fn frame_data(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x00];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn frame_trailer() -> Vec<u8> {
        let body = b"grpc-status:0\r\n";
        let mut out = vec![0x80];
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    const RESET_SECONDS: u64 = 1_784_825_940;
    const RESET_NANOS: u64 = 867_850_000;

    fn expected_iso() -> String {
        to_iso((RESET_SECONDS * 1000 + RESET_NANOS.div_ceil(1_000_000)) as i64)
    }

    #[test]
    fn the_real_frame_shape_decodes() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: Some(1.0),
            reset: Some((RESET_SECONDS, RESET_NANOS)),
        });
        let buffer = [
            frame_data(&encode_length_delimited(1, &info)),
            frame_trailer(),
        ]
        .concat();
        let decoded = decode_grok_credits_frame(&buffer).expect("decodes");
        assert_eq!(decoded.percent_used, 100.0);
        assert_eq!(decoded.reset_at.as_deref(), Some(expected_iso().as_str()));
    }

    #[test]
    fn a_trailing_trailer_frame_does_not_change_the_result() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: Some(0.5),
            reset: Some((RESET_SECONDS, 0)),
        });
        let top = encode_length_delimited(1, &info);
        let without = frame_data(&top);
        let with = [frame_data(&top), frame_trailer()].concat();
        assert_eq!(
            decode_grok_credits_frame(&without),
            decode_grok_credits_frame(&with)
        );
        assert_eq!(decode_grok_credits_frame(&with).unwrap().percent_used, 50.0);
    }

    #[test]
    fn a_raw_unframed_payload_also_decodes() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: Some(0.75),
            reset: Some((RESET_SECONDS, RESET_NANOS)),
        });
        let payload = encode_length_delimited(1, &info);
        assert!(probe_frame_header(&payload, 0).is_none());
        let decoded = decode_grok_credits_frame(&payload).expect("decodes");
        assert!((decoded.percent_used - 75.0).abs() < 1e-4);
        assert_eq!(decoded.reset_at.as_deref(), Some(expected_iso().as_str()));
    }

    #[test]
    fn an_omitted_ratio_is_zero_percent() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: None,
            reset: Some((RESET_SECONDS, RESET_NANOS)),
        });
        let decoded = decode_grok_credits_frame(&frame_data(&encode_length_delimited(1, &info)))
            .expect("decodes");
        assert_eq!(decoded.percent_used, 0.0);
        assert_eq!(decoded.reset_at.as_deref(), Some(expected_iso().as_str()));
    }

    #[test]
    fn a_ratio_above_one_clamps_to_a_hundred() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: Some(1.5),
            reset: None,
        });
        let decoded = decode_grok_credits_frame(&frame_data(&encode_length_delimited(1, &info)))
            .expect("decodes");
        assert_eq!(decoded.percent_used, 100.0);
        assert!(decoded.reset_at.is_none());
    }

    #[test]
    fn a_negative_ratio_is_rejected() {
        let info = encode_credits_info(&CreditsShape {
            usage_ratio: Some(-0.1),
            reset: None,
        });
        assert!(
            decode_grok_credits_frame(&frame_data(&encode_length_delimited(1, &info))).is_none()
        );
    }

    #[test]
    fn malformed_payloads_return_none_rather_than_panicking() {
        // Top-level field 1 as a varint instead of a message.
        assert!(decode_grok_credits_frame(&frame_data(&encode_varint_field(1, 42))).is_none());
        // No field 1 at all.
        assert!(decode_grok_credits_frame(&frame_data(&encode_varint_field(9, 1))).is_none());
        // Nested ratio with the wrong wire type.
        let info = encode_length_delimited(1, b"not-a-float");
        assert!(
            decode_grok_credits_frame(&frame_data(&encode_length_delimited(1, &info))).is_none()
        );
        // Empty, truncated frame header, and a length past the end.
        assert!(decode_grok_credits_frame(&[]).is_none());
        assert!(decode_grok_credits_frame(&[0x00, 0x00]).is_none());
        assert!(decode_grok_credits_frame(&[0x00, 0xff, 0xff, 0xff, 0xff, 0x01]).is_none());
        // An unsupported flag.
        assert!(decode_grok_credits_frame(&[0x02, 0, 0, 0, 0]).is_none());
    }

    // ─── billing ─────────────────────────────────────────────────────────────

    #[test]
    fn on_demand_and_prepaid_become_bars() {
        let billing = json!({"config": {
            "onDemandCap": {"val": 20.0},
            "onDemandUsed": {"val": 5.0},
            "prepaidBalance": {"val": 12.5},
        }});
        let parsed = parse_grok_cli_billing(&billing, None);
        let quotas = parsed["quotas"].as_object().unwrap();
        assert_eq!(quotas["On-demand"]["used"], json!(5));
        assert_eq!(quotas["On-demand"]["total"], json!(20));
        assert_eq!(quotas["On-demand"]["remainingPercentage"], json!(75));
        assert_eq!(quotas["On-demand"]["unlimited"], json!(false));
        assert!(
            !quotas["On-demand"]
                .as_object()
                .unwrap()
                .contains_key("remaining")
        );
        assert_eq!(quotas["Prepaid"]["used"], json!(0));
        assert_eq!(quotas["Prepaid"]["total"], json!(12.5));
        assert_eq!(quotas["Prepaid"]["remainingPercentage"], json!(100));
    }

    #[test]
    fn an_exhausted_promo_account_gets_a_depleted_row_not_a_zero_total() {
        let billing = json!({"config": {"onDemandCap": {"val": 0}, "onDemandUsed": {"val": 0}}});
        let parsed = parse_grok_cli_billing(&billing, None);
        let row = &parsed["quotas"]["On-demand"];
        // A zero total reads as "unlimited" in the dashboard, hence the 1/1.
        assert_eq!(row["used"], json!(1));
        assert_eq!(row["total"], json!(1));
        assert_eq!(row["remainingPercentage"], json!(0));
        assert_eq!(parsed["exhausted"], json!(true));
        assert_eq!(parsed["subscriptionAccess"], json!(false));
    }

    #[test]
    fn a_zero_cap_on_a_subscription_is_not_reported_as_exhausted() {
        let billing = json!({"config": {"onDemandCap": {"val": 0}, "onDemandUsed": {"val": 0}}});
        let user = json!({"subscriptionTier": "super_grok"});
        let parsed = parse_grok_cli_billing(&billing, Some(&user));
        assert!(parsed["quotas"].as_object().unwrap().is_empty());
        assert_eq!(parsed["subscriptionAccess"], json!(true));
        assert_eq!(parsed["plan"], json!("Super Grok"));
    }

    #[test]
    fn a_free_tier_is_not_subscription_access() {
        for tier in ["free", "NONE", "null"] {
            let user = json!({"subscriptionTier": tier});
            let parsed = parse_grok_cli_billing(&json!({}), Some(&user));
            assert_eq!(parsed["subscriptionAccess"], json!(false), "tier {tier}");
        }
    }

    #[test]
    fn the_weekly_pool_percent_is_a_single_bar() {
        let billing = json!({"config": {"creditUsagePercent": {"val": 34.5}}});
        let parsed = parse_grok_cli_billing(&billing, None);
        let row = &parsed["quotas"]["Weekly SuperGrok"];
        assert_eq!(row["used"], json!(34.5));
        assert_eq!(row["total"], json!(100));
        assert_eq!(row["remainingPercentage"], json!(65.5));
    }

    #[test]
    fn monthly_included_prefers_included_used_over_total_used() {
        let billing = json!({"config": {"monthlyLimit": 100, "includedUsed": 10, "totalUsed": 90}});
        assert_eq!(
            parse_grok_cli_billing(&billing, None)["quotas"]["Monthly included"]["used"],
            json!(10)
        );

        let billing = json!({"config": {"monthlyLimit": 100, "totalUsed": 90}});
        assert_eq!(
            parse_grok_cli_billing(&billing, None)["quotas"]["Monthly included"]["used"],
            json!(90)
        );
    }

    #[test]
    fn a_credit_bag_fills_in_the_gap_the_other_rows_left() {
        let billing = json!({"config": {}, "credits": {"total": 50, "remaining": 20}});
        let parsed = parse_grok_cli_billing(&billing, None);
        let row = &parsed["quotas"]["Credits"];
        assert_eq!(row["used"], json!(30));
        assert_eq!(row["total"], json!(50));
        assert_eq!(row["remainingPercentage"], json!(40));

        // A balance with no known allotment becomes a full bar of that balance.
        let billing = json!({"credits": {"remaining": 7}});
        let parsed = parse_grok_cli_billing(&billing, None);
        assert_eq!(parsed["quotas"]["Credits"]["total"], json!(7));
        assert_eq!(
            parsed["quotas"]["Credits"]["remainingPercentage"],
            json!(100)
        );
    }

    #[test]
    fn the_config_block_falls_back_to_the_root() {
        // No `config` key: the billing object is its own config.
        let billing = json!({"onDemandCap": {"val": 10}, "onDemandUsed": {"val": 10}});
        let parsed = parse_grok_cli_billing(&billing, None);
        assert_eq!(
            parsed["quotas"]["On-demand"]["remainingPercentage"],
            json!(0)
        );
        assert_eq!(parsed["exhausted"], json!(true));
    }

    #[test]
    fn a_period_end_is_read_from_every_spelling() {
        let cases = [
            json!({"config": {"billingPeriodEnd": 1_700_000_000}}),
            json!({"config": {"billing_period_end": 1_700_000_000}}),
            json!({"config": {"currentPeriod": {"end": 1_700_000_000}}}),
            json!({"config": {"resetAt": 1_700_000_000}}),
            json!({"billingPeriodEnd": 1_700_000_000}),
            json!({"resetAt": 1_700_000_000}),
        ];
        for billing in cases {
            let parsed = parse_grok_cli_billing(&billing, None);
            assert_eq!(
                parsed["periodEnd"],
                json!("2023-11-14T22:13:20.000Z"),
                "{billing}"
            );
        }
    }

    #[test]
    fn a_plan_comes_from_the_tier_then_the_flags() {
        assert_eq!(
            parse_grok_cli_billing(&json!({}), None)["plan"],
            json!("Grok Build")
        );
        assert_eq!(
            parse_grok_cli_billing(&json!({}), Some(&json!({"hasGrokCodeAccess": true})))["plan"],
            json!("Grok Code")
        );
        assert_eq!(
            parse_grok_cli_billing(
                &json!({"config": {"subscription_tier": "super-grok-heavy"}}),
                None
            )["plan"],
            json!("Super Grok Heavy")
        );
        // The user's tier wins over the config's.
        let user = json!({"subscription": {"tier": "x_premium"}});
        let billing = json!({"config": {"subscriptionTier": "ignored"}});
        assert_eq!(
            parse_grok_cli_billing(&billing, Some(&user))["plan"],
            json!("X Premium")
        );
    }

    #[test]
    fn the_plan_comes_from_the_access_token_when_the_payload_has_none() {
        // `{"tier":1}` as base64url, unpadded.
        let token = "x.eyJ0aWVyIjoxfQ.y";
        assert_eq!(plan_from_access_token(token), "SuperGrok");
        assert_eq!(plan_from_access_token("x.eyJ0aWVyIjo5fQ.y"), "");
        assert_eq!(plan_from_access_token("no-dots"), "");
        assert_eq!(plan_from_access_token("x.!!!.y"), "");
    }

    #[test]
    fn grpc_credits_round_to_whole_percent() {
        let quotas = quotas_from_grpc_credits(Some(GrokCredits {
            percent_used: 34.999,
            reset_at: None,
        }))
        .unwrap();
        assert_eq!(quotas["Weekly SuperGrok"]["used"], json!(35));
        assert_eq!(quotas["Weekly SuperGrok"]["total"], json!(100));
        assert_eq!(quotas["Weekly SuperGrok"]["resetAt"], Value::Null);
        assert!(quotas_from_grpc_credits(None).is_none());
    }

    #[test]
    fn a_reset_chain_skips_a_present_but_falsy_value() {
        // `config.resetAt || config.resetsAt || config.periodEnd`: an empty
        // string at `resetAt` falls through to `resetsAt`.
        let billing = json!({"config": {"resetAt": "", "resetsAt": 1_700_000_000}});
        assert_eq!(
            parse_grok_cli_billing(&billing, None)["periodEnd"],
            json!("2023-11-14T22:13:20.000Z")
        );
        // The same for a credit bag's own reset key.
        let billing = json!({"credits": {"total": 10, "resetAt": "", "end": 1_700_000_000}});
        assert_eq!(
            parse_grok_cli_billing(&billing, None)["quotas"]["Credits"]["resetAt"],
            json!("2023-11-14T22:13:20.000Z")
        );
    }

    #[test]
    fn a_nullish_number_chain_keeps_a_present_zero() {
        // `??` (not `||`): a config `monthlyLimit: 0` wins over a root fallback,
        // and the `> 0` guard then drops the row entirely.
        let billing = json!({"config": {"monthlyLimit": 0}, "monthlyLimit": 500});
        assert!(
            parse_grok_cli_billing(&billing, None)["quotas"]
                .get("Monthly included")
                .is_none()
        );
        // Every config key is tried before any root key.
        let billing = json!({"config": {"monthly_limit": 300}, "monthlyLimit": 999});
        let parsed = parse_grok_cli_billing(&billing, None);
        assert_eq!(parsed["quotas"]["Monthly included"]["total"], json!(300));
    }

    #[test]
    fn a_non_string_tier_stops_the_chain_instead_of_falling_through() {
        // `??` resolves the key first, then `typeof rawTier === "string"` — a
        // number at `subscriptionTier` yields "" rather than the config value.
        let user = json!({"subscriptionTier": 5});
        let billing = json!({"config": {"subscriptionTier": "super_grok"}});
        assert_eq!(
            parse_grok_cli_billing(&billing, Some(&user))["plan"],
            json!("Grok Build")
        );
    }

    #[test]
    fn the_header_set_carries_the_client_identity_and_the_optional_user() {
        let psd = json!({"email": "a@b.c", "principalId": "p-1"});
        let headers = build_headers("tok", Some(&psd));
        let find = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(find("Authorization"), Some("Bearer tok"));
        assert_eq!(find("x-xai-token-auth"), Some("xai-grok-cli"));
        assert_eq!(find("x-grok-client-mode"), Some("headless"));
        assert_eq!(find("x-email"), Some("a@b.c"));
        assert_eq!(find("x-userid"), Some("p-1"));

        let headers = build_headers("tok", None);
        assert!(
            headers
                .iter()
                .all(|(k, _)| *k != "x-email" && *k != "x-userid")
        );
    }
}
