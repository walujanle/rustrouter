//! The `DefaultExecutor` subclasses whose whole deviation is a URL or a body
//! rewrite.
//!
//! Each keeps the inherited send/retry loop and auth descriptor; only the hook
//! the provider actually needs is overridden.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::Executor;
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
    fn the_simple_executor_reads_its_transport_from_the_registry() {
        assert_eq!(
            CodeBuddyIntlExecutor::new().config().format_or_default(),
            "openai"
        );
        assert_eq!(CodeBuddyIntlExecutor::new().provider(), "codebuddy-intl");
    }
}
