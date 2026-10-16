//! The `DefaultExecutor` subclasses whose whole deviation is a URL or a body
//! rewrite.
//!
//! Each keeps the inherited send/retry loop and auth descriptor; only the hook
//! the provider actually needs is overridden.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::{ExecError, Executor, insert_header};
use crate::providers::model::Transport;

// ─── codebuddy ───────────────────────────────────────────────────────────

/// The shared `stream: true` + `reasoning_summary` mirror the CodeBuddy
/// gateway needs. It rejects a non-stream chat request outright.
fn codebuddy_reasoning(transformed: &mut Value) {
    let effort = transformed
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string);
    match effort.as_deref() {
        Some("none") | Some("off") => {
            if let Some(obj) = transformed.as_object_mut() {
                obj.shift_remove("reasoning_effort");
            }
        }
        Some(_) => {
            if let Some(obj) = transformed.as_object_mut() {
                obj.insert("reasoning_summary".into(), json!("auto"));
            }
        }
        None => {}
    }
}

/// Stable conversation id derived from the session key via UUID v5
/// (RFC 4122 name-with-namespace DNS). Stable across retries and failovers so
/// the upstream keeps conversation state pinned to the same session.
fn codebuddy_conversation_id(credentials: &Credentials, body: Option<&Value>) -> String {
    let seed = credentials
        .psd_str("session_id")
        .or_else(|| credentials.psd_str("prompt_cache_key"))
        .or_else(|| credentials.psd_str("conversation_id"))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            credentials
                .header("x-session-id")
                .or_else(|| credentials.header("x-client-request-id"))
        })
        .filter(|s| !s.is_empty())
        .or_else(|| {
            body.and_then(|b| {
                b.get("prompt_cache_key")
                    .or_else(|| b.get("session_id"))
                    .or_else(|| b.get("conversation_id"))
                    .and_then(Value::as_str)
            })
            .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            // Fall back to the first user message text as the seed.
            body.and_then(|b| {
                b.get("messages")
                    .and_then(Value::as_array)
                    .and_then(|msgs| {
                        msgs.iter().find(|m| {
                            m.get("role").and_then(Value::as_str) == Some("user")
                                && m.get("content").and_then(Value::as_str).is_some()
                        })
                    })
                    .and_then(|m| m.get("content").and_then(Value::as_str))
            })
        })
        .filter(|s| !s.is_empty());

    let seed = seed.unwrap_or_else(|| uuid::Uuid::new_v4().to_string().leak());
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, seed.as_bytes()).to_string()
}

/// `CodeBuddyIntlExecutor`: the international gateway, which wants user content
/// as typed blocks and answers 11101 without a leading system prompt.
pub struct CodeBuddyIntlExecutor {
    inner: DefaultExecutor,
}

impl CodeBuddyIntlExecutor {
    pub fn new() -> Self {
        Self {
            inner: DefaultExecutor::new("codebuddy-intl"),
        }
    }
}

impl Default for CodeBuddyIntlExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for CodeBuddyIntlExecutor {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn config(&self) -> &Transport {
        self.inner.config()
    }

    fn transform_request(
        &self,
        model: &str,
        body: Value,
        stream: bool,
        credentials: &Credentials,
    ) -> Value {
        let mut transformed = self
            .inner
            .transform_request(model, body, stream, credentials);
        if let Some(obj) = transformed.as_object_mut() {
            obj.insert("stream".into(), json!(true));
        }
        codebuddy_reasoning(&mut transformed);

        let source = transformed
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut rebuilt = vec![json!({"role": "system", "content": "You are CodeBuddy Code."})];
        for message in source {
            let Some(obj) = message.as_object() else {
                continue;
            };
            let role = obj.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "system" || role == "developer" {
                continue;
            }
            let mut message = obj.clone();
            if role == "user"
                && let Some(Value::String(text)) = message.get("content").cloned()
            {
                message.insert("content".into(), json!([{ "type": "text", "text": text }]));
            }
            rebuilt.push(Value::Object(message));
        }
        if let Some(obj) = transformed.as_object_mut() {
            obj.insert("messages".into(), Value::Array(rebuilt));
        }
        transformed
    }

    /// Inject the `X-Conversation-*` header family that the CodeBuddy
    /// international gateway requires for channel verification. Without them
    /// the gateway returns HTTP 400 code 11128 ("conversation id missing").
    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        model: &str,
        body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        let mut headers = self
            .inner
            .build_headers(credentials, stream, url, model, body)?;

        let conv_id = codebuddy_conversation_id(credentials, body);
        insert_header(&mut headers, "X-Conversation-ID", &conv_id)?;
        insert_header(
            &mut headers,
            "X-Conversation-Request-ID",
            &uuid::Uuid::new_v4().to_string(),
        )?;
        insert_header(
            &mut headers,
            "X-Conversation-Message-ID",
            &uuid::Uuid::new_v4().to_string(),
        )?;
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codebuddy_forces_stream_and_keeps_reasoning() {
        let executor = CodeBuddyIntlExecutor::new();
        let body = json!({"messages": [], "stream": false, "reasoning_effort": "high"});
        let out = executor.transform_request("m", body, false, &Credentials::default());
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["reasoning_summary"], json!("auto"));

        let body = json!({"messages": [], "reasoning_effort": "none"});
        let out = executor.transform_request("m", body, false, &Credentials::default());
        assert!(out.get("reasoning_effort").is_none());
        assert!(out.get("reasoning_summary").is_none());
    }

    #[test]
    fn codebuddy_intl_rebuilds_the_message_list() {
        let executor = CodeBuddyIntlExecutor::new();
        let body = json!({"messages": [
            {"role": "system", "content": "drop me"},
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": [{"type": "text", "text": "hi"}]},
        ]});
        let out = executor.transform_request("m", body, false, &Credentials::default());
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"], json!("You are CodeBuddy Code."));
        assert_eq!(
            messages[1]["content"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(
            messages[2]["content"],
            json!([{"type": "text", "text": "hi"}])
        );
    }

    #[test]
    fn codebuddy_intl_injects_conversation_headers() {
        let executor = CodeBuddyIntlExecutor::new();
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let mut credentials = Credentials::default();
        credentials
            .provider_specific_data
            .insert("prompt_cache_key".into(), json!("conv-12345"));
        let headers = executor
            .build_headers(
                &credentials,
                true,
                "https://www.codebuddy.ai/v1/chat/completions",
                "m",
                Some(&body),
            )
            .unwrap();
        assert!(headers.contains_key("x-conversation-id"));
        assert!(headers.contains_key("x-conversation-request-id"));
        assert!(headers.contains_key("x-conversation-message-id"));
    }

    #[test]
    fn codebuddy_conversation_id_is_stable_across_calls() {
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let mut credentials = Credentials::default();
        credentials
            .provider_specific_data
            .insert("prompt_cache_key".into(), json!("conv-12345"));
        let id1 = codebuddy_conversation_id(&credentials, Some(&body));
        let id2 = codebuddy_conversation_id(&credentials, Some(&body));
        assert_eq!(id1, id2);
        // psd seed takes priority over body, so None body still yields the same id.
        assert_eq!(id1, codebuddy_conversation_id(&credentials, None));
    }

    #[test]
    fn codebuddy_conversation_id_falls_back_to_user_message_text() {
        let body = json!({"messages": [
            {"role": "user", "content": "what is the weather today?"},
        ]});
        let credentials = Credentials::default();
        let id = codebuddy_conversation_id(&credentials, Some(&body));
        assert!(!id.is_empty());
        // Deterministic from the message text.
        let id2 = codebuddy_conversation_id(&credentials, Some(&body));
        assert_eq!(id, id2);
    }

    #[test]
    fn the_simple_executor_reads_its_transport_from_the_registry() {
        assert_eq!(
            CodeBuddyIntlExecutor::new().config().format_or_default(),
            "openai"
        );
        assert_eq!(CodeBuddyIntlExecutor::new().provider(), "codebuddy-intl");
    }
}
