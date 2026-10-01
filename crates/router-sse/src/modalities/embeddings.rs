//! Embeddings core plus the embedding-provider adapters.
//!
//! Providers kept (per `docs/MODALITIES.md`): **mistral**, **nvidia**,
//! **openrouter**. All three use the OpenAI-compatible adapter; there is no
//! dynamic-node branch. The whole adapter map collapses to one
//! `embeddingConfig` read out of the registry, which is why there is no trait
//! here: one implementation is not an abstraction.

use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::http::ProxyOptions;
use crate::modalities::{
    ModalityBody, ModalityError, ModalityHttp, ModalityResponse, headers_from_value,
    provider_config, upstream_error_message,
};
use crate::runtime_config::{FETCH_CONNECT_TIMEOUT_MS, http_status};

/// The providers whose `embeddingConfig` this core serves.
const SUPPORTED: [&str; 3] = ["mistral", "nvidia", "openrouter"];

/// `handleEmbeddingsCore({body, modelInfo, credentials, ...})`.
///
/// The 401/403 token-refresh-and-retry half lives in the route layer, because
/// the refresh hook (`getExecutor(provider).refreshCredentials`) is an executor
/// concern and the cores stay free of the executor map. The route layer retries
/// by calling this again with the refreshed credentials.
pub async fn embeddings_core(
    body: &Value,
    provider: &str,
    model: &str,
    credentials: Option<&Credentials>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    // Validate input.
    let input = body.get("input");
    match input {
        None | Some(Value::Null) => {
            return Err(ModalityError::openai(
                http_status::BAD_REQUEST,
                "Missing required field: input",
            ));
        }
        Some(Value::String(_)) | Some(Value::Array(_)) => {}
        Some(_) => {
            return Err(ModalityError::openai(
                http_status::BAD_REQUEST,
                "input must be a string or array of strings",
            ));
        }
    }
    let input = input.expect("checked above");

    let Some(cfg) = provider_config(provider, "embeddingConfig") else {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            format!("Provider '{provider}' does not support embeddings."),
        ));
    };
    if !SUPPORTED.contains(&provider) {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            format!("Provider '{provider}' does not support embeddings."),
        ));
    }

    // `buildUrl` / `buildHeaders` / `buildBody`, then a misconfiguration is a
    // 400 with the reason in it, not an uncaught throw.
    let Some(url) = cfg.get("baseUrl").and_then(Value::as_str) else {
        return Err(ModalityError::openai(
            http_status::BAD_REQUEST,
            format!("[{provider}/{model}] missing embeddingConfig.baseUrl"),
        ));
    };

    let mut headers: Vec<(String, String)> =
        vec![("Content-Type".into(), "application/json".into())];
    if let Some(token) = super::credential_token(credentials) {
        headers.push(("Authorization".into(), format!("Bearer {token}")));
    }
    headers.extend(headers_from_value(cfg.get("headers")));

    let encoding_format = body
        .get("encoding_format")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("float");
    let request_body = build_body(model, input, encoding_format, body.get("dimensions"));

    let response = ModalityHttp::send(
        "POST",
        url,
        &headers,
        ModalityBody::Json(&request_body),
        proxy_options,
        FETCH_CONNECT_TIMEOUT_MS,
    )
    .await
    .map_err(|e| {
        ModalityError::openai(
            http_status::BAD_GATEWAY,
            crate::utils::error::format_provider_error(None, e.message(), None, None),
        )
    })?;

    if !response.ok() {
        let message = upstream_error_message(&response);
        return Err(ModalityError::openai(
            response.status,
            crate::utils::error::format_provider_error(None, &message, None, None),
        ));
    }

    // The OpenAI-compatible adapter's `normalize` is the identity function, but
    // the response is echoed back and `usage` rides along for the billing hook.
    let normalized: Value = response.json().map_err(|_| {
        ModalityError::openai(
            http_status::BAD_GATEWAY,
            format!("Invalid JSON response from {provider}"),
        )
    })?;
    let usage = normalized
        .get("usage")
        .filter(|u| crate::translator::concerns::primitives::js_truthy(u))
        .cloned();
    Ok(ModalityResponse::json_with_usage(normalized, usage))
}

/// The OpenAI-compatible `buildBody`.
///
/// `encoding_format` is always present (`body.encoding_format || "float"`), and
/// `dimensions` is only added when it parses as a finite number above zero —
/// `dimensions: ""` and `dimensions: 0` are both dropped.
fn build_body(
    model: &str,
    input: &Value,
    encoding_format: &str,
    dimensions: Option<&Value>,
) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert("input".into(), input.clone());
    body.insert("encoding_format".into(), json!(encoding_format));
    if let Some(dim) = dimensions
        .and_then(Value::as_f64)
        .filter(|d| d.is_finite() && *d > 0.0)
    {
        // `Number(dimensions)` then `JSON.stringify`: a whole value has no
        // decimal point, and payloads here are byte-compared.
        body.insert(
            "dimensions".into(),
            crate::translator::concerns::primitives::js_json_number(dim),
        );
    }
    Value::Object(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_carries_encoding_format_and_conditional_dimensions() {
        let body = build_body("m", &json!("hello"), "float", None);
        assert_eq!(
            body,
            json!({"model": "m", "input": "hello", "encoding_format": "float"})
        );

        let body = build_body("m", &json!(["a", "b"]), "base64", Some(&json!(256)));
        assert_eq!(
            body,
            json!({"model": "m", "input": ["a", "b"], "encoding_format": "base64", "dimensions": 256})
        );

        // Empty string, zero and negative are all dropped.
        for dim in [json!(""), json!(0), json!(-4), json!(null)] {
            let body = build_body("m", &json!("x"), "float", Some(&dim));
            assert!(body.get("dimensions").is_none(), "{dim} must be dropped");
        }
    }

    #[tokio::test]
    async fn missing_input_is_a_400() {
        let err = embeddings_core(&json!({}), "mistral", "m", None, &ProxyOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "Missing required field: input");
    }

    #[tokio::test]
    async fn an_unsupported_provider_is_a_400() {
        let err = embeddings_core(
            &json!({"input": "hi"}),
            "deepseek",
            "m",
            None,
            &ProxyOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("does not support embeddings"));
    }
}
