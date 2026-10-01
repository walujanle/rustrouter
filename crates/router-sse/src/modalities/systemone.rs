//! System One (Jev) core.
//!
//! A native decision-payload pass-through: URL and headers come from the
//! registry's `systemoneConfig`, and the body and JSON response are forwarded
//! untouched — decision models have no chat translation layer.
//!
//! Providers kept: **opencode** and **openrouter** (both declare
//! `systemoneConfig`). The `x-opencode-session` header is generated for every
//! request, not only the OpenCode lanes.

use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::http::ProxyOptions;
use crate::modalities::{
    ModalityBody, ModalityError, ModalityHttp, ModalityResponse, SendFailure, headers_from_value,
    provider_config,
};
use crate::runtime_config::{FETCH_CONNECT_TIMEOUT_MS, http_status};

/// `handleSystemoneCore({body, modelInfo, credentials, log, onRequestSuccess})`.
///
/// The 401/403 refresh-and-retry half lives in the route layer, as it does for
/// embeddings: the refresh hook is an executor concern.
pub async fn systemone_core(
    body: &Value,
    provider: &str,
    model: &str,
    credentials: Option<&Credentials>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let Some(cfg) = provider_config(provider, "systemoneConfig") else {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            format!("Provider '{provider}' does not support System One."),
        ));
    };
    let Some(base_url) = cfg.get("baseUrl").and_then(Value::as_str) else {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            format!("Provider '{provider}' does not support System One."),
        ));
    };

    // Validate input at the trust boundary; question-level shape is upstream's
    // job.
    if body.get("state").is_none_or(|s| s.is_null()) {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            "Missing required field: state",
        ));
    }
    match body.get("questions") {
        Some(Value::Object(_)) => {}
        _ => {
            return Err(ModalityError::openai(
                http_status::BAD_REQUEST,
                "Missing required field: questions",
            ));
        }
    }

    // No-auth free lanes carry accessToken "public" from the credential stub.
    let token = super::credential_token(credentials);
    let mut headers: Vec<(String, String)> =
        vec![("Content-Type".into(), "application/json".into())];
    if let Some(token) = token {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    headers.extend(headers_from_value(cfg.get("headers")));
    // OpenCode lanes expect the official client session header on every request.
    headers.push((
        "x-opencode-session".into(),
        crate::executors::opencode::generate_session_id(router_db::time::now_ms() as u64),
    ));

    let mut request_body = body.as_object().cloned().unwrap_or_default();
    request_body.insert("model".into(), json!(model));

    let response = ModalityHttp::send(
        "POST",
        base_url,
        &headers,
        ModalityBody::Json(&Value::Object(request_body)),
        proxy_options,
        FETCH_CONNECT_TIMEOUT_MS,
    )
    .await
    .map_err(|e| {
        let message = match e {
            SendFailure::Timeout => crate::utils::error::format_provider_error(
                Some(&http_status::BAD_GATEWAY.to_string()),
                e.message(),
                None,
                None,
            ),
            SendFailure::Error(message) => crate::utils::error::format_provider_error(
                Some(&http_status::BAD_GATEWAY.to_string()),
                &message,
                None,
                None,
            ),
        };
        ModalityError::openai(http_status::BAD_GATEWAY, message)
    })?;

    if !response.ok() {
        let message = super::upstream_error_message(&response);
        let formatted = crate::utils::error::format_provider_error(
            Some(&response.status.to_string()),
            &message,
            None,
            None,
        );
        return Err(ModalityError::openai(response.status, formatted));
    }

    let response_body: Value = response.json().map_err(|_| {
        ModalityError::openai(
            http_status::BAD_GATEWAY,
            format!("Invalid JSON response from {provider}"),
        )
    })?;

    // `usage.input_tokens || 0` / `usage.output_tokens || 0`, only when `usage`
    // is present.
    let usage = response_body
        .get("usage")
        .filter(|u| crate::translator::concerns::primitives::js_truthy(u))
        .map(|usage| {
            let mut out = Map::new();
            out.insert(
                "prompt_tokens".into(),
                json!(
                    usage
                        .get("input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                ),
            );
            out.insert(
                "completion_tokens".into(),
                json!(
                    usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                ),
            );
            Value::Object(out)
        });

    Ok(ModalityResponse::json_with_usage(response_body, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_provider_without_systemone_config_is_a_400() {
        let err = systemone_core(
            &json!({"state": {}, "questions": {}}),
            "mistral",
            "m",
            None,
            &ProxyOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("does not support System One"));
    }

    #[tokio::test]
    async fn missing_state_is_a_400() {
        let err = systemone_core(
            &json!({"questions": {}}),
            "opencode",
            "m",
            None,
            &ProxyOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "Missing required field: state");
    }

    #[tokio::test]
    async fn questions_must_be_an_object_not_an_array() {
        for questions in [json!(null), json!(["a"]), json!("x"), json!(5)] {
            let err = systemone_core(
                &json!({"state": {"a": 1}, "questions": questions}),
                "opencode",
                "m",
                None,
                &ProxyOptions::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(err.status, 400, "{questions}");
            assert_eq!(err.message, "Missing required field: questions");
        }
    }
}
