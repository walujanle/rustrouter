//! Probe one model through the local gateway.
//!
//! The probe goes back through rustrouter's own `/api/v1/*` surface, so a
//! success proves the whole path (route, guard, translation, upstream). The
//! caller builds the internal headers — the first active API key plus the CLI
//! token — and passes them in, which keeps this module free of DB access.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::handlers::chat_core::non_streaming::unwrap_cline_envelope;
use crate::services::auth::resolve_provider_id;
use crate::translator::concerns::primitives::js_truthy;

const PROBE_TIMEOUT_MS: u64 = 15_000;

/// A response body parsed as JSON when possible, raw text otherwise.
struct ProbeBody {
    parsed: Option<Value>,
    raw: String,
}

async fn read_body(res: reqwest::Response) -> (u16, ProbeBody) {
    let status = res.status().as_u16();
    let raw = res.text().await.unwrap_or_default();
    let parsed = if raw.is_empty() {
        None
    } else {
        serde_json::from_str(&raw).ok()
    };
    (status, ProbeBody { parsed, raw })
}

/// The `detail` chain: `error.message`, then `msg`, then `message`, then
/// `error`, then the raw body.
fn detail(body: &ProbeBody, fields: &[&str]) -> Option<String> {
    if let Some(parsed) = body.parsed.as_ref() {
        for field in fields {
            match parsed.get(*field) {
                Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
                Some(Value::Object(o)) => {
                    if let Some(Value::String(m)) = o.get("message")
                        && !m.is_empty()
                    {
                        return Some(m.clone());
                    }
                }
                Some(other) if !other.is_null() => return Some(other.to_string()),
                _ => {}
            }
        }
    }
    // `parsed?.error?.message || parsed?.error || rawText`: an unparseable body
    // still contributes its raw text.
    if body.raw.is_empty() {
        None
    } else {
        Some(body.raw.clone())
    }
}

/// `HTTP {status}: {detail}` with the detail capped, or just `HTTP {status}`.
fn http_error(status: u16, detail: Option<String>, cap: usize) -> String {
    match detail.filter(|d| !d.is_empty()) {
        Some(d) => {
            let sliced: String = d.chars().take(cap).collect();
            format!("HTTP {status}: {sliced}")
        }
        None => format!("HTTP {status}"),
    }
}

/// `pingModelByKind(model, kind, baseUrl)`.
///
/// `headers` carries `Authorization` and `x-9r-cli-token`; the JSON
/// `Content-Type` is added per branch.
pub async fn ping_model_by_kind(
    model: &str,
    kind: &str,
    base_url: &str,
    headers: &[(String, String)],
) -> Value {
    let started = Instant::now();
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_millis(PROBE_TIMEOUT_MS))
        .build()
    else {
        return json!({ "ok": false, "latencyMs": 0, "error": "Failed to build probe client" });
    };
    let base = base_url.trim_end_matches('/');

    let request = match kind {
        "embedding" => client
            .post(format!("{base}/api/v1/embeddings"))
            .headers(json_headers(headers))
            .json(&json!({ "model": model, "input": "test" })),
        "image" => client
            .post(format!("{base}/api/v1/images/generations"))
            .headers(json_headers(headers))
            .json(&json!({ "model": model, "prompt": "test" })),
        "systemone" => client
            .post(format!("{base}/api/v1/systemone"))
            .headers(json_headers(headers))
            .json(&json!({
                "model": model,
                "state": "Customer: I was charged twice for my order this morning.",
                "questions": {
                    "probe": {
                        "type": "noul",
                        "instructions": "Is the customer reporting a billing problem?",
                    },
                },
            })),
        _ => client
            .post(format!("{base}/api/v1/chat/completions"))
            .headers(json_headers(headers))
            .json(&json!({
                "model": model,
                // 1024 tokens: reasoning models spend their budget on
                // chain-of-thought before answering; a tiny probe starves them
                // and yields a false "no choices" failure.
                "max_tokens": 1024,
                "stream": false,
                "messages": [{ "role": "user", "content": "hi" }],
            })),
    };

    let res = match request.send().await {
        Ok(res) => res,
        Err(e) => {
            let message = if e.is_timeout() {
                "Probe timed out".to_string()
            } else {
                e.to_string()
            };
            return json!({
                "ok": false,
                "latencyMs": started.elapsed().as_millis() as u64,
                "error": message,
            });
        }
    };
    let (status, body) = read_body(res).await;
    let latency_ms = started.elapsed().as_millis() as u64;

    match kind {
        "embedding" => {
            if !(200..300).contains(&status) {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "error": http_error(status, detail(&body, &["error"]), 240),
                    "status": status,
                });
            }
            let has_embedding = body
                .parsed
                .as_ref()
                .and_then(|p| p.get("data"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|d| d.get("embedding"))
                .is_some_and(Value::is_array);
            if !has_embedding {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": "Provider returned no embedding data",
                });
            }
            json!({ "ok": true, "latencyMs": latency_ms, "error": Value::Null, "status": status })
        }
        "image" => {
            if !(200..300).contains(&status) {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "error": http_error(status, detail(&body, &["error", "msg", "message"]), 240),
                    "status": status,
                });
            }
            let has_images = body
                .parsed
                .as_ref()
                .and_then(|p| p.get("data"))
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            if !has_images {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": "Provider returned no image data for this model",
                });
            }
            json!({ "ok": true, "latencyMs": latency_ms, "error": Value::Null, "status": status })
        }
        "systemone" => {
            if !(200..300).contains(&status) {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "error": http_error(status, detail(&body, &["error", "msg", "message"]), 240),
                    "status": status,
                });
            }
            let has_answers = body
                .parsed
                .as_ref()
                .and_then(|p| p.get("answers"))
                .and_then(Value::as_object)
                .is_some_and(|a| !a.is_empty());
            if !has_answers {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": "Provider returned no answers for this model",
                });
            }
            json!({ "ok": true, "latencyMs": latency_ms, "error": Value::Null, "status": status })
        }
        _ => {
            // Unwrap before the choices checks. No-op for providers that do not
            // opt in via transport.quirks.clineEnvelope.
            let provider_id = resolve_provider_id(model.split('/').next().unwrap_or(""));
            let parsed = body
                .parsed
                .clone()
                .map(|p| unwrap_cline_envelope(p, &provider_id));
            let body = ProbeBody {
                parsed,
                raw: body.raw.clone(),
            };

            if !(200..300).contains(&status) {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "error": http_error(status, detail(&body, &["error", "msg", "message"]), 500),
                    "status": status,
                });
            }

            let provider_status = body.parsed.as_ref().and_then(|p| p.get("status"));
            let provider_msg = body
                .parsed
                .as_ref()
                .and_then(|p| p.get("msg").or_else(|| p.get("message")));
            let has_provider_error_status =
                provider_status.is_some_and(|s| !s.is_null() && s != "200" && s != "0");
            if has_provider_error_status && let Some(msg) = provider_msg.filter(|m| !m.is_null()) {
                let text = msg
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| msg.to_string());
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": format!(
                        "Provider status {}: {}",
                        provider_status.map(Value::to_string).unwrap_or_default(),
                        text.chars().take(240).collect::<String>()
                    ),
                });
            }

            if let Some(error) = body.parsed.as_ref().and_then(|p| p.get("error"))
                && !error.is_null()
            {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        error
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| error.to_string())
                    });
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": message.chars().take(240).collect::<String>(),
                });
            }

            let first_choice = body
                .parsed
                .as_ref()
                .and_then(|p| p.get("choices"))
                .and_then(Value::as_array)
                .and_then(|a| a.first());
            let has_choices = first_choice.is_some();
            let message = first_choice.and_then(|c| c.get("message"));
            let has_reasoning = message.is_some_and(|m| {
                [
                    "reasoning",
                    "reasoning_content",
                    "thinking",
                    "thinking_content",
                ]
                .iter()
                .any(|k| m.get(*k).is_some_and(js_truthy))
            });
            let content_empty = message
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .is_empty();
            let finish_length = first_choice
                .and_then(|c| c.get("finish_reason"))
                .and_then(Value::as_str)
                == Some("length");

            // Soft-pass: a reasoning model may burn its whole budget on
            // chain-of-thought and return `finish_reason:"length"` with empty
            // content but non-empty reasoning. That is a live connection.
            if has_choices && finish_length && content_empty && has_reasoning {
                return json!({
                    "ok": true,
                    "latencyMs": latency_ms,
                    "error": Value::Null,
                    "status": status,
                    "note": "reasoning-only response (length-limited)",
                });
            }

            if !has_choices {
                return json!({
                    "ok": false,
                    "latencyMs": latency_ms,
                    "status": status,
                    "error": "Provider returned no completion choices for this model",
                });
            }

            json!({ "ok": true, "latencyMs": latency_ms, "error": Value::Null, "status": status })
        }
    }
}

/// `Content-Type: application/json` plus the caller's headers.
fn json_headers(headers: &[(String, String)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    map.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    for (k, v) in headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            map.insert(name, value);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_caps_the_detail_and_omits_it_when_absent() {
        assert_eq!(http_error(500, None, 240), "HTTP 500");
        assert_eq!(http_error(500, Some(String::new()), 240), "HTTP 500");
        assert_eq!(
            http_error(404, Some("not found".into()), 240),
            "HTTP 404: not found"
        );
        let long = "x".repeat(300);
        assert_eq!(
            http_error(500, Some(long), 240).len(),
            "HTTP 500: ".len() + 240
        );
    }

    #[test]
    fn detail_prefers_error_message_then_raw_body() {
        let body = ProbeBody {
            parsed: Some(json!({ "error": { "message": "boom" } })),
            raw: "{\"error\":{\"message\":\"boom\"}}".into(),
        };
        assert_eq!(detail(&body, &["error"]), Some("boom".to_string()));
        let body = ProbeBody {
            parsed: None,
            raw: "plain".into(),
        };
        assert_eq!(detail(&body, &["error"]), Some("plain".to_string()));
    }
}
