//! The non-chat modality cores: embeddings, search, fetch and systemone.
//!
//! HTTP-free like the rest of the engine. A core takes a parsed request body (or
//! an already-extracted multipart form) plus the resolved credentials and returns
//! a [`ModalityResponse`] — status, content type, body — or a [`ModalityError`].
//! Auth, credential selection and the multi-account fallback loop stay in the
//! route layer.
//!
//! ## Contract for the route layer
//!
//! ```text
//! embeddings_core(body, provider, model, credentials, proxy) -> Result<ModalityResponse>
//! search_core(body, provider_id, credentials, proxy) -> Result<ModalityResponse>
//! fetch_core(body, provider_id, credentials, proxy) -> Result<ModalityResponse>
//! systemone_core(body, provider, model, credentials, proxy) -> Result<ModalityResponse>
//! ```
//!
//! * `body` is the parsed JSON request; `provider`/`model` are the already-split
//!   pair; `provider_id` is alias-resolved (`registry().resolve_alias`) because
//!   the provider's `searchConfig` / `searchViaChat` / `fetchConfig` /
//!   `embeddingConfig` / `systemoneConfig` is read here, from the
//!   ported registry, not passed in.
//! * `credentials` is `None` only on the no-auth path.
//! * A core returns `Err(ModalityError)` for any error result.
//!   `ModalityError.body` already carries the exact JSON envelope
//!   (`buildErrorBody` for four of them, search's own
//!   `{ error: { message, code } }` for the fifth); `status` and `message` feed the
//!   app layer's `markAccountUnavailable` call.
//! * On success the app layer must pass [`websearch_lock_key`] / [`webfetch_lock_key`]
//!   as the failure lock scope. A failing search must not write an account-wide
//!   `__all` lock that takes a shared chat key offline.
//!
//! `ModalityResponse.usage` is `Some` for embeddings and systemone (the two
//! that feed `saveRequestUsage`); search and fetch record nothing.

use std::time::Duration;

use serde_json::Value;

use crate::credentials::Credentials;
use crate::executors::http::{ProxyOptions, prepare_send};
use crate::utils::error::build_error_body;

pub mod embeddings;
pub mod fetch;
pub mod search;
pub mod ssrf;
pub mod systemone;

pub use embeddings::embeddings_core;
pub use fetch::fetch_core;
pub use search::search_core;
pub use systemone::systemone_core;

/// The result of a core, before the route layer turns it into an HTTP response.
#[derive(Debug, Clone)]
pub struct ModalityResponse {
    pub status: u16,
    /// `application/json` on every JSON path; the upstream's own type on the STT
    /// OpenAI-compatible passthrough (which forwards the body verbatim).
    pub content_type: String,
    /// The JSON body, or a `Value::String` for a verbatim text passthrough.
    pub body: Value,
    /// `usage` for the billing hook (embeddings, systemone). `None` otherwise.
    pub usage: Option<Value>,
}

impl ModalityResponse {
    /// A 200 JSON response with no usage.
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json".to_string(),
            body,
            usage: None,
        }
    }

    /// A 200 JSON response carrying `usage`.
    pub fn json_with_usage(body: Value, usage: Option<Value>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".to_string(),
            body,
            usage,
        }
    }

    /// A 200 body forwarded verbatim with its upstream content type.
    pub fn text(body: String, content_type: String) -> Self {
        Self {
            status: 200,
            content_type,
            body: Value::String(body),
            usage: None,
        }
    }

    /// The bytes the route layer writes. A non-JSON content type means the body
    /// is a verbatim string.
    pub fn body_bytes(&self) -> Vec<u8> {
        if self.content_type.starts_with("application/json") {
            serde_json::to_vec(&self.body).unwrap_or_default()
        } else {
            self.body.as_str().unwrap_or_default().as_bytes().to_vec()
        }
    }
}

/// A core failure, already shaped as an error result.
#[derive(Debug, Clone)]
pub struct ModalityError {
    pub status: u16,
    pub message: String,
    /// The exact JSON body returned to the client.
    pub body: Value,
}

impl ModalityError {
    /// `createErrorResult(status, message)`: the OpenAI-compatible envelope.
    pub fn openai(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            status,
            body: build_error_body(status, &message),
            message,
        }
    }

    /// `errorResult(status, error)`: a bare
    /// `{ error: { message, code } }`, deliberately unlike `buildErrorBody`.
    pub fn search(status: u16, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            status,
            body: serde_json::json!({ "error": { "message": message, "code": status } }),
            message,
        }
    }

    /// The status and body the route layer renders.
    pub fn into_response(self) -> (u16, Value) {
        (self.status, self.body)
    }
}

impl std::fmt::Display for ModalityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ModalityError {}

/// The per-connection failure lock the app layer must pass to
/// `markAccountUnavailable` / `getProviderCredentials`. Without it a failed
/// search writes an account-wide `__all` lock that takes a shared chat key
/// offline.
pub fn websearch_lock_key(provider_id: &str) -> String {
    format!("websearch:{provider_id}")
}

/// The web-fetch counterpart of [`websearch_lock_key`].
pub fn webfetch_lock_key(provider_id: &str) -> String {
    format!("webfetch:{provider_id}")
}

/// A provider's modality config (`embeddingConfig`, `searchConfig`,
/// `fetchConfig`, `systemoneConfig`, `searchViaChat`) out of the registry dump.
/// The registry is generated and static, so the borrow is `'static`.
pub fn provider_config(provider_id: &str, key: &str) -> Option<&'static Value> {
    crate::providers::registry()
        .get(provider_id)?
        .extra
        .get(key)
}

/// `sanitizeHeaders`: HTTP header values must be a ByteString, so every
/// character above U+00FF is dropped, then the value is trimmed. Non-string
/// values pass through untouched; here headers are already strings.
pub fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .filter(|c| (*c as u32) <= 0xFF)
        .collect::<String>()
        .trim()
        .to_string()
}

/// [`sanitize_header_value`] over a header list.
pub fn sanitize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.clone(), sanitize_header_value(v)))
        .collect()
}

/// `Number.isFinite(n)`.
pub(crate) fn finite_number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// `credentials?.apiKey || credentials?.accessToken || undefined`.
pub(crate) fn credential_token(credentials: Option<&Credentials>) -> Option<&str> {
    credentials.and_then(|c| {
        c.api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| c.access_token.as_deref().filter(|s| !s.is_empty()))
    })
}

/// The response shape every non-SSRF send produces.
#[derive(Debug, Clone)]
pub struct ModalityHttpResponse {
    pub status: u16,
    pub content_type: String,
    pub text: String,
}

impl ModalityHttpResponse {
    /// `res.json()`: `Err` when the body is not JSON.
    pub fn json(&self) -> Result<Value, serde_json::Error> {
        serde_json::from_str(&self.text)
    }

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// A transport failure, kept distinct from a provider status so each core can
/// pick the status to use (502, or 504 on abort).
#[derive(Debug)]
pub enum SendFailure {
    Timeout,
    Error(String),
}

impl SendFailure {
    pub fn message(&self) -> &str {
        match self {
            SendFailure::Timeout => "request timed out",
            SendFailure::Error(m) => m,
        }
    }
}

/// The body of a [`ModalityHttp::send`].
pub enum ModalityBody<'a> {
    Json(&'a Value),
    Bytes(Vec<u8>),
    Multipart(reqwest::multipart::Form),
    Empty,
}

/// The outbound HTTP helper for the modality cores. It reuses
/// `executors::http::prepare_send` so the outbound proxy and the MITM DNS
/// bypass apply the same way they do for chat, and bounds the send with a
/// time-to-headers deadline (the deadline ends when the response head arrives,
/// so the body read is unbounded here too).
pub struct ModalityHttp;

impl ModalityHttp {
    pub async fn send(
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: ModalityBody<'_>,
        proxy_options: &ProxyOptions,
        timeout_ms: u64,
    ) -> Result<ModalityHttpResponse, SendFailure> {
        let target = prepare_send(url, proxy_options)
            .await
            .map_err(|e| SendFailure::Error(e.to_string()))?;

        let mut request = match method {
            "GET" => target.client.get(&target.url),
            _ => target.client.post(&target.url),
        };

        let mut header_map = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            crate::executors::executor::insert_header(&mut header_map, name, value)
                .map_err(|e| SendFailure::Error(e.to_string()))?;
        }
        for (name, value) in &target.extra_headers {
            crate::executors::executor::insert_header(&mut header_map, name, value)
                .map_err(|e| SendFailure::Error(e.to_string()))?;
        }
        for (name, value) in header_map.iter() {
            request = request.header(name, value);
        }

        request = match body {
            ModalityBody::Json(value) => request.json(value),
            ModalityBody::Bytes(bytes) => request.body(bytes),
            ModalityBody::Multipart(form) => request.multipart(form),
            ModalityBody::Empty => request,
        };

        let sent = tokio::time::timeout(Duration::from_millis(timeout_ms), request.send()).await;
        let response = match sent {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => return Err(SendFailure::Error(e.to_string())),
            Err(_) => return Err(SendFailure::Timeout),
        };

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let text = response
            .text()
            .await
            .map_err(|e| SendFailure::Error(e.to_string()))?;
        Ok(ModalityHttpResponse {
            status,
            content_type,
            text,
        })
    }
}

/// `upstreamError(res)`: the message out of a non-ok upstream body.
pub(crate) fn upstream_error_message(response: &ModalityHttpResponse) -> String {
    let fallback = if response.text.is_empty() {
        format!("Upstream error ({})", response.status)
    } else {
        response.text.clone()
    };
    let Ok(json) = response.json() else {
        return fallback;
    };
    let message = json
        .get("error")
        .and_then(|e| e.get("message"))
        .filter(|m| crate::translator::concerns::primitives::js_truthy(m))
        .or_else(|| {
            json.get("error")
                .filter(|e| crate::translator::concerns::primitives::js_truthy(e))
        })
        .or_else(|| {
            json.get("message")
                .filter(|m| crate::translator::concerns::primitives::js_truthy(m))
        });
    match message {
        Some(Value::String(s)) => s.clone(),
        Some(v) => crate::translator::concerns::primitives::js_string(v),
        None => fallback,
    }
}

/// `Object.entries(headers)` over a JSON object, as a header list.
pub(crate) fn headers_from_value(headers: Option<&Value>) -> Vec<(String, String)> {
    let Some(Value::Object(map)) = headers else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| {
            v.as_str().map(|s| (k.clone(), s.to_string())).or_else(|| {
                if v.is_null() {
                    None
                } else {
                    Some((
                        k.clone(),
                        crate::translator::concerns::primitives::js_string(v),
                    ))
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_sanitizer_strips_above_ff_and_trims() {
        // 'é' (U+00E9) survives, '€' (U+20AC) and a CJK char do not.
        assert_eq!(sanitize_header_value("  Béar € 漢 "), "Béar");
        assert_eq!(sanitize_header_value("plain"), "plain");
        assert_eq!(sanitize_header_value(""), "");
    }

    #[test]
    fn lock_keys_are_capability_scoped() {
        assert_eq!(websearch_lock_key("tavily"), "websearch:tavily");
        assert_eq!(webfetch_lock_key("exa"), "webfetch:exa");
    }

    #[test]
    fn modality_error_shapes_match_the_two_envelopes() {
        let openai = ModalityError::openai(400, "bad");
        assert_eq!(
            openai.body,
            json!({"error": {"message": "bad", "type": "invalid_request_error", "code": "bad_request"}})
        );
        let search = ModalityError::search(502, "nope");
        assert_eq!(
            search.body,
            json!({"error": {"message": "nope", "code": 502}})
        );
    }

    #[test]
    fn json_body_bytes_round_trip_and_text_is_verbatim() {
        let json = ModalityResponse::json(json!({"a": 1}));
        assert_eq!(json.body_bytes(), br#"{"a":1}"#.to_vec());
        let text = ModalityResponse::text("hello".into(), "text/plain".into());
        assert_eq!(text.body_bytes(), b"hello".to_vec());
    }

    #[test]
    fn upstream_error_reads_the_nested_message() {
        let response = ModalityHttpResponse {
            status: 400,
            content_type: "application/json".into(),
            text: r#"{"error":{"message":"quota"}}"#.into(),
        };
        assert_eq!(upstream_error_message(&response), "quota");
        let response = ModalityHttpResponse {
            status: 500,
            content_type: "text/plain".into(),
            text: String::new(),
        };
        assert_eq!(upstream_error_message(&response), "Upstream error (500)");
    }
}
