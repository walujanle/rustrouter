//! The paid OpenCode Go subscription, whose models are split across three
//! endpoints (`/chat/completions`, `/messages`, `/responses`) and picked per
//! model by the registry's `targetFormat`.
//!
//! The session digest differs from the free tier's: `opencode-go` is prefixed
//! into the hash, so a client moving between the two tiers does not carry an id
//! the other side has already burned.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::{
    ExecError, ExecuteRequest, Executor, UpstreamResponse, insert_header,
};
use crate::executors::opencode::{
    LoopRunner, SESSION_HEADER, base_model_id, normalize_responses_tools, normalize_session,
    raw_session_header, sanitize_responses_items,
};
use crate::providers::lookup::FORMAT_OPENAI_RESPONSES;
use crate::providers::model::Transport;
use crate::providers::registry::registry;
use crate::session_manager::{SessionIdentityInput, resolve_session_id};
use crate::translator::concerns::primitives::js_truthy;
use crate::translator::formats::responses_api::normalize_responses_input;

const SESSION_FIELD: &str = "_opencodeGoSession";
/// The endpoint Muse Spark and the other Responses models live on.
const RESPONSES_BASE_URL: &str = "https://opencode.ai/zen/go/v1/responses";

/// Derive a session id from the client tool and the resolved session, prefixed
/// so it cannot collide with the free tier's digest of the same input.
fn translated_session(session_id: &str, client_tool: &str) -> String {
    let tool = if client_tool.is_empty() {
        "generic"
    } else {
        client_tool
    };
    let digest = Sha256::digest(format!("opencode-go\0{tool}\0{session_id}").as_bytes());
    format!("ses_{}", &hex::encode(digest)[..32])
}

/// The registry entry's `targetFormat` is the whole answer here: a model the
/// registry does not list is not a responses model.
fn is_responses_model(model: &str) -> bool {
    let base = base_model_id(model);
    registry()
        .models_for("opencode-go")
        .iter()
        .find(|m| m.id == base)
        .and_then(|m| m.target_format())
        == Some(FORMAT_OPENAI_RESPONSES)
}

pub struct OpenCodeGoExecutor {
    inner: DefaultExecutor,
}

impl OpenCodeGoExecutor {
    pub fn new() -> Self {
        Self {
            inner: DefaultExecutor::new("opencode-go"),
        }
    }

    /// Prefer the client's raw session header, otherwise hash the provider
    /// session (or the manager's derived id) and stash it on the credentials.
    pub fn prepare_request_credentials(
        &self,
        body: Option<&Value>,
        credentials: &Credentials,
        provider_session_id: Option<&str>,
        client_tool: Option<&str>,
    ) -> Credentials {
        let empty = Value::Null;
        let body = body.unwrap_or(&empty);
        let native = raw_session_header(&credentials.raw_headers);
        let resolved = provider_session_id
            .and_then(normalize_session)
            .unwrap_or_else(|| {
                resolve_session_id(&SessionIdentityInput {
                    headers: &credentials.raw_headers,
                    body,
                    connection_id: credentials.connection_id.as_deref(),
                    workspace_id: None,
                    scope: "opencode-go",
                })
            });

        let mut prepared = credentials.clone();
        prepared.extra.insert(
            SESSION_FIELD.into(),
            json!(
                native.unwrap_or_else(|| translated_session(&resolved, client_tool.unwrap_or("")))
            ),
        );
        prepared
    }
}

impl Default for OpenCodeGoExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for OpenCodeGoExecutor {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn config(&self) -> &Transport {
        self.inner.config()
    }

    fn build_url(
        &self,
        model: &str,
        stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        // Muse Spark lives on /responses even when a stale runtimeTransport
        // leaks in.
        if is_responses_model(model) {
            return Ok(RESPONSES_BASE_URL.to_string());
        }
        self.inner.build_url(model, stream, url_index, credentials)
    }

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
        let prepared = credentials
            .extra
            .get(SESSION_FIELD)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let session = match prepared {
            Some(session) => session.to_string(),
            None => {
                let fallback = self.prepare_request_credentials(None, credentials, None, None);
                fallback
                    .extra
                    .get(SESSION_FIELD)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
        };
        insert_header(&mut headers, SESSION_HEADER, &session)?;
        Ok(headers)
    }

    fn transform_request(
        &self,
        model: &str,
        body: Value,
        stream: bool,
        credentials: &Credentials,
    ) -> Value {
        let mut out = self
            .inner
            .transform_request(model, body, stream, credentials);
        let effective_model = if model.is_empty() {
            out.get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            model.to_string()
        };
        if !is_responses_model(&effective_model) || !out.is_object() {
            return out;
        }

        if let Some(normalized) = normalize_responses_input(out.get("input")) {
            out["input"] = Value::Array(normalized);
        }
        if out
            .get("input")
            .and_then(Value::as_array)
            .is_none_or(|i| i.is_empty())
        {
            out["input"] = json!([{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "..."}],
            }]);
        }
        // Responses names the output cap `max_output_tokens`, not `max_tokens`.
        if out.get("max_output_tokens").is_none() {
            if let Some(value) = out.get("max_completion_tokens").cloned() {
                out["max_output_tokens"] = value;
            } else if let Some(value) = out.get("max_tokens").cloned() {
                out["max_output_tokens"] = value;
            }
        }
        if let Some(obj) = out.as_object_mut() {
            obj.shift_remove("max_tokens");
            obj.shift_remove("max_completion_tokens");
        }
        if let Some(effort) = out.get("reasoning_effort").cloned()
            && out.get("reasoning").is_none()
        {
            out["reasoning"] = json!({"effort": effort, "summary": "auto"});
        }
        if out.get("reasoning").is_some_and(Value::is_object)
            && !out
                .get("reasoning")
                .and_then(|r| r.get("summary"))
                .is_some_and(js_truthy)
        {
            out["reasoning"]["summary"] = json!("auto");
        }
        if let Some(obj) = out.as_object_mut() {
            obj.shift_remove("reasoning_effort");
        }
        out["stream"] = json!(true);
        out["store"] = json!(false);
        normalize_responses_tools(&mut out);
        sanitize_responses_items(&mut out);
        out
    }

    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        let credentials = self.prepare_request_credentials(
            Some(&req.body),
            req.credentials,
            req.provider_session_id,
            req.client_tool,
        );
        LoopRunner(self)
            .execute(req.with_credentials(&credentials))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials_with(connection_id: &str) -> Credentials {
        Credentials {
            connection_id: Some(connection_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn only_registry_declared_responses_models_take_the_responses_path() {
        assert!(is_responses_model("grok-4.6"));
        assert!(is_responses_model("muse-spark-1.3-contributor"));
        assert!(!is_responses_model("glm-5.3"), "openai-only model");
        assert!(
            !is_responses_model("deepseek-v4-pro"),
            "declared openai, not responses"
        );
        // An id the registry does not list is not responses here.
        assert!(!is_responses_model("muse-spark-9.9-contributor-free"));
        // The thinking suffix is stripped before the lookup.
        assert!(is_responses_model("grok-4.6(high)"));
    }

    #[test]
    fn the_session_digest_is_stable_and_client_scoped() {
        let a = translated_session("conv-1", "claude-code");
        assert_eq!(a, translated_session("conv-1", "claude-code"));
        assert_ne!(a, translated_session("conv-1", "cursor"));
        assert_ne!(a, translated_session("conv-2", "claude-code"));
        assert!(a.starts_with("ses_"));
        assert_eq!(a.len(), 4 + 32);
        assert_eq!(
            translated_session("conv-1", ""),
            translated_session("conv-1", "generic"),
            "an absent client tool is `generic`"
        );
        // Distinct from the free tier's digest of the same input.
        assert_ne!(
            a,
            crate::executors::opencode::translate_session_id(Some("conv-1"), "claude-code")
        );
    }

    #[test]
    fn a_native_session_header_is_passed_through_unhashed() {
        let executor = OpenCodeGoExecutor::new();
        let mut credentials = credentials_with("conn-a");
        credentials
            .raw_headers
            .insert(SESSION_HEADER.into(), "  ses_custom  ".into());
        let prepared = executor.prepare_request_credentials(None, &credentials, None, None);
        assert_eq!(
            prepared.extra.get(SESSION_FIELD).and_then(Value::as_str),
            Some("ses_custom"),
            "any normalized header value is accepted here, not just the canonical shape"
        );
    }

    #[test]
    fn a_provider_session_is_hashed_and_a_connection_is_derived() {
        let executor = OpenCodeGoExecutor::new();
        let prepared = executor.prepare_request_credentials(
            Some(&json!({})),
            &credentials_with("conn-a"),
            Some("app-session"),
            Some("cursor"),
        );
        let expected = translated_session("app-session", "cursor");
        assert_eq!(
            prepared.extra.get(SESSION_FIELD).and_then(Value::as_str),
            Some(expected.as_str())
        );

        // Without a provider session the manager's derived id is hashed instead.
        let derived = executor.prepare_request_credentials(
            Some(&json!({})),
            &credentials_with("conn-a"),
            None,
            None,
        );
        let session = derived
            .extra
            .get(SESSION_FIELD)
            .and_then(Value::as_str)
            .unwrap();
        assert!(session.starts_with("ses_"));
        assert_eq!(session.len(), 4 + 32, "a digest, not a raw derived id");
    }

    #[test]
    fn headers_keep_the_inherited_auth_and_add_the_session() {
        let executor = OpenCodeGoExecutor::new();
        let mut credentials = credentials_with("conn-a");
        credentials.api_key = Some("k".into());
        let prepared = executor.prepare_request_credentials(None, &credentials, None, None);
        let headers = executor
            .build_headers(
                &prepared,
                true,
                "https://opencode.ai/zen/go/v1/chat/completions",
                "glm-5.3",
                None,
            )
            .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer k");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(
            headers.get(SESSION_HEADER).unwrap(),
            prepared
                .extra
                .get(SESSION_FIELD)
                .and_then(Value::as_str)
                .unwrap()
        );
    }

    #[test]
    fn headers_still_resolve_a_session_without_prepared_credentials() {
        let executor = OpenCodeGoExecutor::new();
        let mut credentials = credentials_with("conn-a");
        credentials.api_key = Some("k".into());
        let headers = executor
            .build_headers(
                &credentials,
                true,
                "https://opencode.ai/zen/go/v1/chat/completions",
                "glm-5.3",
                None,
            )
            .unwrap();
        let session = headers.get(SESSION_HEADER).unwrap().to_str().unwrap();
        assert!(session.starts_with("ses_"));
        assert_eq!(session.len(), 4 + 32);
    }

    #[test]
    fn urls_prefer_the_responses_endpoint_for_its_models() {
        let executor = OpenCodeGoExecutor::new();
        let credentials = Credentials::default();
        assert_eq!(
            executor
                .build_url("grok-4.6", true, 0, &credentials)
                .unwrap(),
            RESPONSES_BASE_URL
        );
        assert_eq!(
            executor
                .build_url("glm-5.3", true, 0, &credentials)
                .unwrap(),
            "https://opencode.ai/zen/go/v1/chat/completions"
        );
    }

    #[test]
    fn a_responses_request_is_reshaped_and_not_fingerprinted() {
        let executor = OpenCodeGoExecutor::new();
        let body =
            json!({"input": "hello", "max_completion_tokens": 42, "reasoning_effort": "low"});
        let out = executor.transform_request("grok-4.6", body, true, &Credentials::default());
        assert_eq!(out["max_output_tokens"], json!(42));
        assert!(out.get("max_completion_tokens").is_none());
        assert_eq!(out["reasoning"]["effort"], json!("low"));
        assert_eq!(out["reasoning"]["summary"], json!("auto"));
        assert!(out.get("reasoning_effort").is_none());
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["store"], json!(false));
        assert_eq!(out["input"][0]["type"], json!("message"));
        assert!(
            out.get("tools").is_none() || out["tools"].as_array().is_some_and(|t| t.is_empty()),
            "opencode-go does not inject the free-tier fingerprint tools"
        );
    }

    #[test]
    fn a_chat_request_is_left_to_the_default_transform() {
        let executor = OpenCodeGoExecutor::new();
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "max_tokens": 5});
        let out = executor.transform_request("glm-5.3", body, true, &Credentials::default());
        assert_eq!(
            out["max_tokens"],
            json!(5),
            "the chat path keeps the chat cap"
        );
        assert!(out.get("store").is_none());
        assert!(out.get("stream").is_none() || out["stream"] == json!(true));
    }
}
