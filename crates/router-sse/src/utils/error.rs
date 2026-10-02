//! Client-facing error shapes: the status-to-type table and the default
//! per-status messages.
//!
//! These are the *client* error shapes, not the dashboard's. The dashboard
//! returns `{ error: "..." }` (see `router-server::error`); these return the
//! OpenAI-compatible `{ error: { message, type, code } }` that SDK clients
//! parse, and the 429 path adds a `Retry-After` header.
//!
//! `build_error_body` is public because several call sites outside this module
//! need it: the stream-error writers and the chat core.

use serde_json::{Value, json};

/// The `{ type, code }` pair per status.
fn error_type(status: u16) -> (&'static str, &'static str) {
    match status {
        400 => ("invalid_request_error", "bad_request"),
        401 => ("authentication_error", "invalid_api_key"),
        402 => ("billing_error", "payment_required"),
        403 => ("permission_error", "insufficient_quota"),
        404 => ("invalid_request_error", "model_not_found"),
        406 => ("invalid_request_error", "model_not_supported"),
        429 => ("rate_limit_error", "rate_limit_exceeded"),
        500 => ("server_error", "internal_server_error"),
        502 => ("server_error", "bad_gateway"),
        503 => ("server_error", "service_unavailable"),
        504 => ("server_error", "gateway_timeout"),
        // Unlisted statuses: 5xx is a server error, everything else a bad
        // request with no code.
        s if s >= 500 => ("server_error", "internal_server_error"),
        _ => ("invalid_request_error", ""),
    }
}

/// The default message for a status, when one is known.
pub fn default_error_message(status: u16) -> Option<&'static str> {
    match status {
        400 => Some("Bad request"),
        401 => Some("Invalid API key provided"),
        402 => Some("Payment required"),
        403 => Some("You exceeded your current quota"),
        404 => Some("Model not found"),
        406 => Some("Model not supported"),
        429 => Some("Rate limit exceeded"),
        500 => Some("Internal server error"),
        502 => Some("Bad gateway - upstream provider error"),
        503 => Some("Service temporarily unavailable"),
        504 => Some("Gateway timeout"),
        _ => None,
    }
}

/// An explicit message wins, then the status default, then a generic string.
/// An empty string falls through, which is why the guard is `is_empty` and not
/// `None`.
pub fn build_error_body(status_code: u16, message: &str) -> Value {
    let (kind, code) = error_type(status_code);
    let message = if message.is_empty() {
        default_error_message(status_code).unwrap_or("An error occurred")
    } else {
        message
    };
    json!({
        "error": { "message": message, "type": kind, "code": code }
    })
}

/// A status, message and optional reset time parsed out of an upstream error.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamError {
    pub status_code: u16,
    pub message: String,
    /// `undefined` when the executor reported no precise expiry; a provider that
    /// knows its own reset time (codex) sets it.
    pub resets_at_ms: Option<f64>,
}

/// `executor_parsed` is the result of the executor's own error-parsing hook,
/// already called by the caller because the hook needs the executor object.
/// When it returns an object its `message` is trusted (falling back to the
/// default for the status), and its `status` is preferred over the HTTP one.
///
/// The default path reads `error.message`, then `message`, then `error`, then
/// the raw body text, and stringifies a non-string result — a provider that
/// returns `{"error": {...}}` yields the serialized object, not
/// `"[object Object]"`.
pub fn parse_upstream_error(
    status: u16,
    body_text: &str,
    executor_parsed: Option<&Value>,
) -> UpstreamError {
    if let Some(parsed) = executor_parsed.filter(|p| p.is_object()) {
        let status_code = parsed
            .get("status")
            .and_then(Value::as_u64)
            .map(|s| s as u16)
            .unwrap_or(status);
        let message = parsed
            .get("message")
            .filter(|m| crate::translator::concerns::primitives::js_truthy(m))
            .map(crate::translator::concerns::primitives::js_string)
            .unwrap_or_else(|| {
                default_error_message(status)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Upstream error: {status}"))
            });
        return UpstreamError {
            status_code,
            message,
            resets_at_ms: parsed.get("resetsAtMs").and_then(Value::as_f64),
        };
    }

    let json: Option<Value> = serde_json::from_str(body_text).ok();
    let message = json
        .as_ref()
        .and_then(|j| {
            j.get("error")
                .and_then(|e| e.get("message"))
                .filter(|m| crate::translator::concerns::primitives::js_truthy(m))
                .or_else(|| {
                    j.get("message")
                        .filter(|m| crate::translator::concerns::primitives::js_truthy(m))
                })
                .or_else(|| {
                    j.get("error")
                        .filter(|e| crate::translator::concerns::primitives::js_truthy(e))
                })
        })
        .map(crate::translator::concerns::primitives::js_string)
        .unwrap_or_else(|| body_text.to_string());

    let final_message = if message.is_empty() {
        default_error_message(status)
            .map(str::to_string)
            .unwrap_or_else(|| format!("Upstream error: {status}"))
    } else {
        message
    };

    UpstreamError {
        status_code: status,
        message: final_message,
        resets_at_ms: None,
    }
}

/// The `{ success, status, error, resetsAtMs }` result the chat core builds
/// from a failed request.
#[derive(Debug, Clone)]
pub struct ErrorResult {
    pub status: u16,
    pub error: String,
    pub resets_at_ms: Option<f64>,
}

impl ErrorResult {
    pub fn new(status: u16, message: impl Into<String>, resets_at_ms: Option<f64>) -> Self {
        Self {
            status,
            error: message.into(),
            resets_at_ms,
        }
    }

    /// The response body for this error.
    pub fn body(&self) -> Value {
        build_error_body(self.status, &self.error)
    }
}

/// An unavailability response: the JSON body and status, plus the
/// `Retry-After` value in whole seconds.
///
/// Note the body is *not* the standard error shape: it is a bare
/// `{ error: { message } }` with the human retry hint appended, plus a
/// `Retry-After` clamped to at least 1.
pub fn unavailable_response(
    status_code: u16,
    message: &str,
    retry_after_iso: &str,
    retry_after_human: &str,
) -> (u16, Value, String) {
    let retry_after_sec = retry_after_secs(retry_after_iso);
    let msg = format!("{message} ({retry_after_human})");
    (
        status_code,
        json!({ "error": { "message": msg } }),
        retry_after_sec.to_string(),
    )
}

/// Whole seconds until `iso`, floored at 1. A parse failure also yields 1.
fn retry_after_secs(iso: &str) -> i64 {
    let target = router_db::time::parse_iso(iso);
    let Some(target) = target else {
        return 1;
    };
    let diff_ms = (target - chrono::Utc::now()).num_milliseconds();
    let secs = (diff_ms as f64 / 1000.0).ceil() as i64;
    secs.max(1)
}

/// Render a provider error as `[code]: message`, with an optional cause chain.
pub fn format_provider_error(
    code: Option<&str>,
    message: &str,
    cause_code: Option<&str>,
    cause_message: Option<&str>,
) -> String {
    let code = code.filter(|c| !c.is_empty()).unwrap_or("FETCH_FAILED");
    let message = if message.is_empty() {
        "Unknown error"
    } else {
        message
    };
    let cause = match (
        cause_code.filter(|c| !c.is_empty()),
        cause_message.filter(|m| !m.is_empty()),
    ) {
        (Some(c), Some(m)) => format!(" (cause: {c}: {m})"),
        (Some(c), None) => format!(" (cause: {c})"),
        (None, Some(m)) => format!(" (cause: {m})"),
        (None, None) => String::new(),
    };
    format!("[{code}]: {message}{cause}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_error_body_uses_the_table_then_the_fallback() {
        assert_eq!(
            build_error_body(429, "slow down"),
            json!({"error": {"message": "slow down", "type": "rate_limit_error", "code": "rate_limit_exceeded"}})
        );
        // Unknown 4xx: `invalid_request_error` with an empty code.
        assert_eq!(
            build_error_body(418, "teapot"),
            json!({"error": {"message": "teapot", "type": "invalid_request_error", "code": ""}})
        );
        // Unknown 5xx.
        assert_eq!(
            build_error_body(599, "upstream died"),
            json!({"error": {"message": "upstream died", "type": "server_error", "code": "internal_server_error"}})
        );
    }

    #[test]
    fn empty_message_falls_back_to_the_default() {
        assert_eq!(
            build_error_body(404, "")["error"]["message"],
            json!("Model not found")
        );
        assert_eq!(
            build_error_body(418, "")["error"]["message"],
            json!("An error occurred")
        );
    }

    #[test]
    fn parse_upstream_error_prefers_the_error_message() {
        let parsed = parse_upstream_error(429, r#"{"error":{"message":"quota"}}"#, None);
        assert_eq!(parsed.message, "quota");
        assert_eq!(parsed.status_code, 429);
    }

    #[test]
    fn parse_upstream_error_stringifies_a_non_string_error() {
        let parsed = parse_upstream_error(400, r#"{"error":{"code":"x"}}"#, None);
        assert_eq!(parsed.message, r#"{"code":"x"}"#);
    }

    #[test]
    fn parse_upstream_error_falls_back_to_the_body_text() {
        let parsed = parse_upstream_error(502, "bad gateway", None);
        assert_eq!(parsed.message, "bad gateway");
        let parsed = parse_upstream_error(502, "", None);
        assert_eq!(parsed.message, "Bad gateway - upstream provider error");
        let parsed = parse_upstream_error(599, "", None);
        assert_eq!(parsed.message, "Upstream error: 599");
    }

    #[test]
    fn executor_parse_error_wins_including_its_status_and_reset() {
        let hook = json!({"status": 429, "message": "reset", "resetsAtMs": 1_700_000_000_000.0});
        let parsed = parse_upstream_error(502, "ignored", Some(&hook));
        assert_eq!(parsed.status_code, 429);
        assert_eq!(parsed.message, "reset");
        assert_eq!(parsed.resets_at_ms, Some(1_700_000_000_000.0));
    }

    #[test]
    fn executor_parse_error_without_a_message_uses_the_http_status_default() {
        let hook = json!({"resetsAtMs": 1.0});
        let parsed = parse_upstream_error(429, "ignored", Some(&hook));
        assert_eq!(parsed.status_code, 429);
        assert_eq!(parsed.message, "Rate limit exceeded");
    }

    #[test]
    fn format_provider_error_appends_the_cause_chain() {
        assert_eq!(
            format_provider_error(None, "fetch failed", None, None),
            "[FETCH_FAILED]: fetch failed"
        );
        assert_eq!(
            format_provider_error(Some("429"), "slow", Some("UND_ERR_SOCKET"), None),
            "[429]: slow (cause: UND_ERR_SOCKET)"
        );
        assert_eq!(
            format_provider_error(
                Some("429"),
                "slow",
                Some("ECONNRESET"),
                Some("read ECONNRESET")
            ),
            "[429]: slow (cause: ECONNRESET: read ECONNRESET)"
        );
    }

    #[test]
    fn unavailable_response_clamps_retry_after_to_at_least_one() {
        let (status, body, retry) =
            unavailable_response(429, "locked", "1970-01-01T00:00:00.000Z", "reset after 0s");
        assert_eq!(status, 429);
        assert_eq!(
            body,
            json!({"error": {"message": "locked (reset after 0s)"}})
        );
        assert_eq!(retry, "1");
    }
}
