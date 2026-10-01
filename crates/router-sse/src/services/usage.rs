//! Per-provider usage lookups: one handler per provider in the registry, plus
//! the helpers they share.
//!
//! Three behaviours are load-bearing:
//!
//! * **Missing and zero are different.** A numeric `0` reads as missing, but
//!   the numeric string `"0"` is the epoch — `parse_reset_time` returns `None`
//!   only for falsy values, so the two zeroes diverge on purpose.
//! * **Quota rows must not carry an absolute `remaining`** where the dashboard
//!   reads `remainingPercentage`: a `remaining` that is a percentage is read as
//!   a percentage, and a credit balance is read as a bar.
//! * **Usage failures are messages, not errors**, except Codex. Every handler
//!   except `get_codex_usage` catches and returns `{message}`, which the route
//!   returns as HTTP 200. Only Codex propagates, so the route can 500.

mod codebuddy;
mod codex;
mod grok_cli;

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::executors::http::{ProxyOptions, prepare_send};
use crate::executors::oauth::to_iso;
use crate::providers::registry::registry;
use crate::translator::concerns::primitives::js_truthy;

pub use codex::{consume_codex_rate_limit_reset_credit, get_codex_rate_limit_reset_credits};

/// Default request timeout: 10s.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

// ─── the dispatch ─────────────────────────────────────────────────────────

/// The fields every handler reads off the connection row.
pub struct UsageConnection<'a> {
    pub provider: &'a str,
    pub access_token: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub provider_specific_data: Option<&'a Map<String, Value>>,
}

impl<'a> UsageConnection<'a> {
    /// Destructure the fields every handler reads off a connection row.
    pub fn from_row(connection: &'a Value) -> Self {
        let string = |key: &str| {
            connection
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        };
        Self {
            provider: connection
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or(""),
            access_token: string("accessToken"),
            api_key: string("apiKey"),
            provider_specific_data: connection
                .get("providerSpecificData")
                .and_then(Value::as_object),
        }
    }

    /// `providerSpecificData` as a value, for the handlers that read it raw.
    fn psd(&self) -> Value {
        Value::Object(self.provider_specific_data.cloned().unwrap_or_default())
    }
}

/// Dispatch to the handler for the connection's provider.
///
/// `Err` is the Codex handler's throw; every other failure is a `{message}`
/// payload the caller returns as 200.
pub async fn get_usage_for_provider(
    connection: &Value,
    proxy_options: &ProxyOptions,
    _force: bool,
) -> Result<Value, String> {
    let c = UsageConnection::from_row(connection);
    match c.provider {
        "codex" => codex::get_codex_usage(c.access_token.unwrap_or(""), proxy_options).await,
        "commandcode" => Ok(get_commandcode_usage(c.api_key, proxy_options).await),
        "deepseek" => Ok(get_deepseek_usage(c.api_key, proxy_options).await),
        "grok-cli" => Ok(grok_cli::get_grok_cli_usage(
            c.access_token.unwrap_or(""),
            Some(&c.psd()),
            proxy_options,
        )
        .await),
        "opencode-go" => Ok(get_opencode_usage(c.api_key, proxy_options).await),
        "codebuddy-intl" => {
            Ok(codebuddy::get_codebuddy_intl_usage(c.access_token, c.api_key, proxy_options).await)
        }
        _ => Ok(json!({ "message": format!("Usage API not implemented for {}", c.provider) })),
    }
}

// ─── shared helpers ───────────────────────────────────────────────────────

/// The provider's registry `transport.usage` block, or an empty object.
pub(crate) fn u(id: &str) -> Value {
    registry()
        .transport(id)
        .and_then(|t| t.usage.clone())
        .unwrap_or_else(|| json!({}))
}

/// `U(id)[key]` as a string.
pub(crate) fn u_str(id: &str, key: &str) -> Option<String> {
    u(id)
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `None` for anything unparseable, ISO-8601 with milliseconds otherwise.
///
/// A falsy value returns `None` first, so a numeric `0` is *not* the epoch — it
/// is a missing value. A numeric **string** `"0"`
/// survives that guard (`"0"` is truthy) and does become the epoch, so the two
/// zeroes diverge and the distinction is deliberate.
///
/// Numbers below 1e12 are seconds; a numeric string takes the same path,
/// because provider APIs are inconsistent about which they send. A non-numeric
/// string is parsed as RFC-3339, then as a bare `YYYY-MM-DD`.
pub(crate) fn parse_reset_time(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if !js_truthy(value) {
        return None;
    }
    if let Some(n) = value.as_f64() {
        return Some(to_iso(if n < 1e12 {
            (n * 1000.0) as i64
        } else {
            n as i64
        }));
    }
    let s = value.as_str()?;
    if s.bytes().all(|b| b.is_ascii_digit()) {
        let n: f64 = s.parse().ok()?;
        return Some(to_iso(if n < 1e12 {
            (n * 1000.0) as i64
        } else {
            n as i64
        }));
    }
    // RFC-3339 first, then the date-only form.
    if let Some(ms) = crate::executors::oauth::parse_time_ms(Some(value)) {
        return Some(to_iso(ms));
    }
    let date = chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    Some(to_iso(
        date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis(),
    ))
}

/// A number, or the fallback when the value is missing, non-numeric, or not
/// finite.
pub(crate) fn to_finite_number(value: Option<&Value>, fallback: f64) -> f64 {
    match value {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()).unwrap_or(fallback),
        Some(Value::String(s)) if !s.trim().is_empty() => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .unwrap_or(fallback),
        _ => fallback,
    }
}

/// `None` stands in for a missing or non-finite value — every caller guards
/// with a finiteness check before using the value.
pub(crate) fn unwrap_val(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    if let Some(obj) = value.as_object()
        && let Some(inner) = obj.get("val")
    {
        return finite(inner);
    }
    finite(value)
}

/// The unwrapped value, or a fallback when there is none.
pub(crate) fn unwrap_val_or(value: Option<&Value>, fallback: f64) -> f64 {
    unwrap_val(value).unwrap_or(fallback)
}

fn finite(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) if !s.trim().is_empty() => {
            s.trim().parse::<f64>().ok().filter(|f| f.is_finite())
        }
        _ => None,
    }
}

// ─── the HTTP layer ───────────────────────────────────────────────────────

/// The response every handler inspects: status, the `ok` flag, and the body
/// text it can parse or quote in an error message.
pub(crate) struct UsageResponse {
    pub status: u16,
    pub ok: bool,
    body: String,
    bytes: Vec<u8>,
}

impl UsageResponse {
    /// The body parsed as JSON, or `None` when it does not parse.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }

    /// The raw body as text.
    pub fn text(&self) -> &str {
        &self.body
    }

    /// The raw bytes — the gRPC-web handler needs them intact, which a lossy
    /// `String` round-trip would not survive.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A method-and-body pair, kept explicit because the three shapes the handlers
/// need (GET, POST JSON, POST a raw gRPC-web frame) do not share a body type.
pub(crate) enum Send<'a> {
    Get,
    Json(&'a Value),
    Raw {
        bytes: &'a [u8],
        content_type: &'a str,
    },
}

/// Fetch with the default timeout; returns `Err` on a transport
/// failure so the handler can quote the message.
pub(crate) async fn fetch(
    url: &str,
    send: Send<'_>,
    headers: &[(&str, String)],
    proxy_options: &ProxyOptions,
) -> Result<UsageResponse, String> {
    fetch_with_timeout(url, send, headers, proxy_options, DEFAULT_TIMEOUT).await
}

pub(crate) async fn fetch_with_timeout(
    url: &str,
    send: Send<'_>,
    headers: &[(&str, String)],
    proxy_options: &ProxyOptions,
    timeout: Duration,
) -> Result<UsageResponse, String> {
    let target = prepare_send(url, proxy_options)
        .await
        .map_err(|e| e.to_string())?;
    let mut request = match send {
        Send::Get => target.client.get(&target.url),
        Send::Json(body) => target.client.post(&target.url).json(body),
        Send::Raw {
            bytes,
            content_type,
        } => target
            .client
            .post(&target.url)
            .header("Content-Type", content_type)
            .body(bytes.to_vec()),
    };
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    for (name, value) in &target.extra_headers {
        request = request.header(name.as_str(), value.as_str());
    }

    let response = match tokio::time::timeout(timeout, request.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("request timed out".to_string()),
    };
    let status = response.status();
    // `bytes()` rather than `text()`: the gRPC-web response is binary and a
    // lossy UTF-8 decode would corrupt the protobuf payload.
    let bytes = response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .unwrap_or_default();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok(UsageResponse {
        status: status.as_u16(),
        ok: status.is_success(),
        body,
        bytes,
    })
}

/// An `Authorization: Bearer …` header pair.
fn bearer(token: &str) -> Vec<(&'static str, String)> {
    vec![("Authorization", format!("Bearer {token}"))]
}

// ─── the small handlers ───────────────────────────────────────────────────

/// DeepSeek usage — GET `https://api.deepseek.com/user/balance`.
async fn get_deepseek_usage(api_key: Option<&str>, proxy_options: &ProxyOptions) -> Value {
    let Some(api_key) = api_key.map(str::trim).filter(|k| !k.is_empty()) else {
        return json!({ "message": "DeepSeek API key not available. Add a key to view usage." });
    };

    let mut headers = bearer(api_key);
    headers.push(("Content-Type", "application/json".to_string()));
    headers.push(("Accept", "application/json".to_string()));

    let response = match fetch(
        "https://api.deepseek.com/user/balance",
        Send::Get,
        &headers,
        proxy_options,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => return json!({ "message": format!("DeepSeek error: {e}") }),
    };

    if response.status == 401 || response.status == 403 {
        return json!({ "plan": "DeepSeek", "message": "DeepSeek authentication failed. Check the API key." });
    }
    if !response.ok {
        let err_text = response.text();
        let trimmed: String = err_text.chars().take(120).collect();
        let suffix = if trimmed.is_empty() {
            String::new()
        } else {
            format!(": {trimmed}")
        };
        return json!({
            "plan": "DeepSeek",
            "message": format!("DeepSeek balance API error ({}){suffix}", response.status),
        });
    }

    let Some(data) = response.json() else {
        return json!({ "message": "DeepSeek balance response was not JSON." });
    };
    if !data.is_object() {
        return json!({ "message": "DeepSeek balance response was not JSON." });
    }

    let balances = parse_balance_infos(&data);
    if balances.is_empty() {
        return json!({ "plan": "DeepSeek", "message": "DeepSeek connected. No balance data returned." });
    }

    let is_available = data.get("is_available").and_then(Value::as_bool) == Some(true)
        || data.get("isAvailable").and_then(Value::as_bool) == Some(true);

    let mut quotas = Map::new();
    for b in &balances {
        let total = b.total_balance.max(0.0);
        // A credit balance is not a usage bar: `isCreditBalance` tells the
        // dashboard to render it as a currency figure instead.
        quotas.insert(
            format!("Balance ({})", b.currency),
            json!({
                "used": 0,
                "total": total,
                "remainingPercentage": if total > 0.0 { 100 } else { 0 },
                "resetAt": null,
                "unlimited": false,
                "isCreditBalance": true,
                "currency": b.currency,
            }),
        );
    }

    json!({
        "plan": if is_available { "DeepSeek" } else { "DeepSeek (Insufficient Balance)" },
        "quotas": Value::Object(quotas),
    })
}

struct BalanceInfo {
    currency: String,
    total_balance: f64,
}

/// Read the `balance_infos` array.
fn parse_balance_infos(data: &Value) -> Vec<BalanceInfo> {
    let Some(list) = data.get("balance_infos").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let currency = obj.get("currency").and_then(Value::as_str)?.to_uppercase();
            if currency.is_empty() {
                return None;
            }
            Some(BalanceInfo {
                currency,
                total_balance: to_finite_number(
                    obj.get("total_balance").or_else(|| obj.get("totalBalance")),
                    0.0,
                ),
            })
        })
        .collect()
}

/// OpenCode Go usage lookup: one request to the provider's usage endpoint,
/// rendered into the same `{ message }` shape every usage handler returns.
async fn get_opencode_usage(api_key: Option<&str>, proxy_options: &ProxyOptions) -> Value {
    let label = "OpenCode Go";
    let Some(api_key) = api_key.map(str::trim).filter(|k| !k.is_empty()) else {
        return json!({ "message": format!("{label} API key not available. Add a key to view usage.") });
    };
    let Some(url) = u_str("opencode-go", "url") else {
        return json!({ "message": format!("{label} usage endpoint is not configured.") });
    };

    let headers = bearer(api_key);
    let response = match fetch(&url, Send::Get, &headers, proxy_options).await {
        Ok(response) => response,
        Err(e) => return json!({ "message": format!("{label} error: {e}") }),
    };

    if response.status == 401 {
        return json!({ "plan": label, "message": format!("{label} authentication failed. Check the API key.") });
    }
    if response.status == 403 {
        // A 403 is either "subscribe" or "this key cannot use the product";
        // only the first has an `EntitlementError` body.
        let subscription_required = response
            .json()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some("EntitlementError");
        return json!({
            "plan": label,
            "message": if subscription_required {
                format!("{label} subscription required for this API key.")
            } else {
                format!("{label} access forbidden for this API key.")
            },
        });
    }
    if !response.ok {
        return json!({ "plan": label, "message": format!("{label} usage API error ({}).", response.status) });
    }

    let Some(data) = response.json() else {
        return json!({ "plan": label, "message": format!("{label} usage response did not contain quota data.") });
    };
    let Some(usage) = data.get("usage").and_then(Value::as_object) else {
        return json!({ "plan": label, "message": format!("{label} usage response did not contain quota data.") });
    };

    let mut quotas = Map::new();
    for (period, name) in [
        ("rolling", "Rolling"),
        ("weekly", "Weekly"),
        ("monthly", "Monthly"),
    ] {
        let Some(quota) = usage.get(period).and_then(Value::as_object) else {
            continue;
        };
        let Some(percent) = quota.get("percent").and_then(finite) else {
            continue;
        };
        let used = to_finite_number(Some(&json!(percent)), 0.0).clamp(0.0, 100.0);
        quotas.insert(
            name.to_string(),
            json!({
                "used": used,
                "total": 100,
                "remaining": 100.0 - used,
                "remainingPercentage": 100.0 - used,
                "resetAt": parse_reset_time(quota.get("resetsAt")),
                "unlimited": false,
            }),
        );
    }

    if quotas.is_empty() {
        return json!({ "plan": label, "message": format!("{label} usage response did not contain valid quota data.") });
    }
    json!({ "plan": label, "quotas": Value::Object(quotas) })
}

/// Command Code usage — whoami, then credits and subscriptions.
async fn get_commandcode_usage(api_key: Option<&str>, proxy_options: &ProxyOptions) -> Value {
    let Some(api_key) = api_key.map(str::trim).filter(|k| !k.is_empty()) else {
        return json!({ "message": "Command Code API key not available. Add a key to view usage." });
    };
    let base = std::env::var("COMMAND_CODE_API_BASE_URL")
        .unwrap_or_else(|_| "https://api.commandcode.ai".to_string());
    let base = base.trim_end_matches('/').to_string();

    let headers = vec![
        ("Authorization", format!("Bearer {api_key}")),
        ("Accept", "application/json".to_string()),
    ];

    let whoami = match fetch(
        &format!("{base}/alpha/whoami?limits=1"),
        Send::Get,
        &headers,
        proxy_options,
    )
    .await
    {
        Ok(response) => response,
        Err(e) => return json!({ "message": format!("Command Code error: {e}") }),
    };
    if whoami.status == 401 || whoami.status == 403 {
        return json!({ "plan": "Command Code", "message": "Command Code authentication failed. Check the API key." });
    }
    if !whoami.ok {
        return json!({
            "plan": "Command Code",
            "message": format!("Command Code usage API error ({})", whoami.status),
        });
    }
    let org_id = whoami.json().and_then(|v| {
        v.get("org")
            .and_then(|o| o.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    let credits_url = commandcode_qs(&format!("{base}/alpha/billing/credits"), org_id.as_deref());
    let subs_url = commandcode_qs(
        &format!("{base}/alpha/billing/subscriptions"),
        org_id.as_deref(),
    );
    let (credits_res, subs_res) = tokio::join!(
        fetch(&credits_url, Send::Get, &headers, proxy_options),
        fetch(&subs_url, Send::Get, &headers, proxy_options),
    );

    let (credits_res, subs_res) = match (credits_res, subs_res) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            return json!({ "message": format!("Command Code error: {e}") });
        }
    };

    let unauthorized = |r: &UsageResponse| r.status == 401 || r.status == 403;
    if unauthorized(&credits_res) || unauthorized(&subs_res) {
        return json!({ "plan": "Command Code", "message": "Command Code authentication failed. Check the API key." });
    }
    if !credits_res.ok {
        return json!({
            "plan": "Command Code",
            "message": format!("Command Code credits API error ({})", credits_res.status),
        });
    }
    if !subs_res.ok {
        return json!({
            "plan": "Command Code",
            "message": format!("Command Code subscriptions API error ({})", subs_res.status),
        });
    }

    let credits_body = credits_res.json().unwrap_or_else(|| json!({}));
    let subs_body = subs_res.json().unwrap_or_else(|| json!({}));

    let plan_id = subs_body
        .get("data")
        .and_then(|d| d.get("planId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let plan = plan_id
        .as_deref()
        .and_then(commandcode_plan_name)
        .map(str::to_string)
        .or_else(|| plan_id.clone())
        .unwrap_or_else(|| "Command Code".to_string());
    let cap = plan_id
        .as_deref()
        .and_then(commandcode_plan_cap)
        .unwrap_or(0.0);

    let c = credits_body
        .get("credits")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let remaining = to_finite_number(c.get("monthlyCredits"), 0.0)
        + to_finite_number(c.get("purchasedCredits"), 0.0)
        + to_finite_number(c.get("freeCredits"), 0.0);
    let used = if cap > 0.0 {
        (cap - remaining).max(0.0)
    } else {
        0.0
    };
    let total = if cap > 0.0 { cap } else { remaining };

    let mut quotas = Map::new();
    quotas.insert(
        "Credits".to_string(),
        json!({
            "used": used,
            "total": total,
            "remaining": remaining,
            "unlimited": cap <= 0.0,
            "resetAt": parse_reset_time(subs_body.get("data").and_then(|d| d.get("currentPeriodEnd"))),
        }),
    );
    if let Some(five_hour) = window_quota(
        credits_body
            .get("windowLimits")
            .and_then(|w| w.get("fiveHour")),
    ) {
        quotas.insert("Session (5h)".to_string(), five_hour);
    }
    if let Some(weekly) = window_quota(
        credits_body
            .get("windowLimits")
            .and_then(|w| w.get("weekly")),
    ) {
        quotas.insert("Weekly".to_string(), weekly);
    }

    json!({ "plan": plan, "quotas": Value::Object(quotas) })
}

/// Display names for Command Code plan ids.
fn commandcode_plan_name(id: &str) -> Option<&'static str> {
    Some(match id {
        "individual-go" => "Go",
        "individual-goat" => "GOAT",
        "individual-pro" | "individual-pro-v1" => "Pro",
        "individual-provider" => "Provider",
        "individual-max" => "Max",
        "individual-ultra" => "Ultra",
        "teams-pro" => "Teams Pro",
        _ => return None,
    })
}

/// Credit caps for Command Code plan ids.
fn commandcode_plan_cap(id: &str) -> Option<f64> {
    Some(match id {
        "individual-go" => 10.0,
        "individual-goat" => 70.0,
        "individual-pro" => 30.0,
        "individual-pro-v1" => 80.0,
        "individual-provider" => 15.0,
        "individual-max" => 150.0,
        "individual-ultra" => 300.0,
        "teams-pro" => 40.0,
        _ => return None,
    })
}

/// Append `orgId` to the route when one is present.
fn commandcode_qs(route: &str, org_id: Option<&str>) -> String {
    match org_id {
        Some(id) => {
            let query: String = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("orgId", id)
                .finish();
            format!("{route}?{query}")
        }
        None => route.to_string(),
    }
}

/// A window with neither a cap nor usage is not a row.
fn window_quota(win: Option<&Value>) -> Option<Value> {
    let win = win?.as_object()?;
    let used = to_finite_number(win.get("used"), 0.0);
    let total = to_finite_number(win.get("cap"), 0.0);
    if total <= 0.0 && used <= 0.0 {
        return None;
    }
    Some(json!({
        "used": used,
        "total": total,
        "remaining": (total - used).max(0.0),
        "unlimited": false,
        "resetAt": parse_reset_time(win.get("resetAt")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_time_reads_seconds_milliseconds_and_dates() {
        // Seconds below 1e12 are multiplied; milliseconds are not.
        assert_eq!(
            parse_reset_time(Some(&json!(1_700_000_000))).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            parse_reset_time(Some(&json!(1_700_000_000_000u64))).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        // The same rule for a numeric string.
        assert_eq!(
            parse_reset_time(Some(&json!("1700000000"))).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            parse_reset_time(Some(&json!("2023-11-14T22:13:20Z"))).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        // Unparseable and falsy values are null.
        assert!(parse_reset_time(None).is_none());
        assert!(parse_reset_time(Some(&Value::Null)).is_none());
        assert!(parse_reset_time(Some(&json!(""))).is_none());
        assert!(parse_reset_time(Some(&json!("not a date"))).is_none());
        // `!resetValue` rejects a numeric 0 (a missing value), but the truthy
        // string "0" survives and becomes the epoch.
        assert!(parse_reset_time(Some(&json!(0))).is_none());
        assert_eq!(
            parse_reset_time(Some(&json!("0"))).unwrap(),
            "1970-01-01T00:00:00.000Z"
        );
        // `new Date(str)` also accepts the bare date form.
        assert_eq!(
            parse_reset_time(Some(&json!("2023-11-14"))).unwrap(),
            "2023-11-14T00:00:00.000Z"
        );
    }

    #[test]
    fn finite_number_accepts_numeric_strings_and_rejects_the_rest() {
        assert_eq!(to_finite_number(Some(&json!(2.5)), 0.0), 2.5);
        assert_eq!(to_finite_number(Some(&json!(" 3 ")), 0.0), 3.0);
        assert_eq!(to_finite_number(Some(&json!("")), 7.0), 7.0);
        assert_eq!(to_finite_number(Some(&json!("abc")), 7.0), 7.0);
        assert_eq!(to_finite_number(Some(&json!(true)), 7.0), 7.0);
        assert_eq!(to_finite_number(None, 7.0), 7.0);
    }

    #[test]
    fn unwrap_val_reads_the_protobuf_val_envelope() {
        assert_eq!(unwrap_val(Some(&json!({"val": 12}))), Some(12.0));
        assert_eq!(unwrap_val(Some(&json!(12))), Some(12.0));
        assert_eq!(unwrap_val(Some(&json!("12"))), Some(12.0));
        // A zero envelope is a real zero, not a miss.
        assert_eq!(unwrap_val(Some(&json!({"val": 0}))), Some(0.0));
        assert_eq!(unwrap_val(Some(&Value::Null)), None);
        assert_eq!(unwrap_val(Some(&json!({"other": 1}))), None);
        assert_eq!(unwrap_val_or(Some(&json!({})), 5.0), 5.0);
    }

    #[test]
    fn commandcode_query_drops_a_missing_org() {
        assert_eq!(commandcode_qs("/r", None), "/r");
        assert_eq!(commandcode_qs("/r", Some("o1")), "/r?orgId=o1");
    }

    #[test]
    fn commandcode_plan_tables_agree() {
        // Every id with a cap must also have a display name.
        for id in [
            "individual-go",
            "individual-goat",
            "individual-pro",
            "individual-pro-v1",
            "individual-provider",
            "individual-max",
            "individual-ultra",
            "teams-pro",
        ] {
            assert!(commandcode_plan_name(id).is_some(), "{id} has no name");
            assert!(commandcode_plan_cap(id).is_some(), "{id} has no cap");
        }
        assert_eq!(commandcode_plan_name("individual-pro-v1"), Some("Pro"));
        assert_eq!(commandcode_plan_cap("individual-pro-v1"), Some(80.0));
    }

    #[test]
    fn window_quota_needs_a_cap_or_usage() {
        assert!(window_quota(None).is_none());
        assert!(window_quota(Some(&json!({"used": 0, "cap": 0}))).is_none());
        let q = window_quota(Some(
            &json!({"used": 3, "cap": 10, "resetAt": 1_700_000_000}),
        ))
        .unwrap();
        assert_eq!(q["used"], json!(3.0));
        assert_eq!(q["remaining"], json!(7.0));
        assert_eq!(q["resetAt"], json!("2023-11-14T22:13:20.000Z"));
    }

    #[test]
    fn the_dispatch_table_matches_the_registry_usage_flags() {
        // Every provider the registry advertises usage for must reach a handler
        // rather than the "not implemented" fallback.
        let supported = registry().usage_supported();
        assert!(!supported.is_empty());
        for id in supported {
            let handled = matches!(
                id,
                "codex"
                    | "commandcode"
                    | "deepseek"
                    | "grok-cli"
                    | "opencode-go"
                    | "codebuddy-intl"
            );
            assert!(handled, "{id} advertises usage but has no handler");
        }
    }

    #[test]
    fn an_unknown_provider_gets_the_default_fallback_message() {
        let connection = json!({ "provider": "nope", "accessToken": "t" });
        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(get_usage_for_provider(
                &connection,
                &ProxyOptions::default(),
                false,
            ))
            .unwrap();
        assert_eq!(out["message"], json!("Usage API not implemented for nope"));
    }
}
