//! Nous Research: credential probing and catalogue normalisation.
//!
//! Nous exposes `/v1/models` publicly, so a `/models` probe proves nothing
//! about a Bearer value. The only honest check is a one-token inference call on
//! a free model, which also keeps validation from consuming paid credits.

use serde_json::{Value, json};

pub const NOUS_INFERENCE_BASE_URL: &str = "https://inference-api.nousresearch.com/v1";
pub const NOUS_CHAT_COMPLETIONS_URL: &str =
    "https://inference-api.nousresearch.com/v1/chat/completions";
pub const NOUS_MODELS_URL: &str = "https://inference-api.nousresearch.com/v1/models";

/// A current free-tier text model, so a probe never costs anything.
pub const NOUS_VALIDATION_MODEL: &str = "poolside/laguna-xs-2.1:free";

/// The probe body shared by initial validation and the saved-connection test.
pub fn validation_body() -> Value {
    json!({
        "model": NOUS_VALIDATION_MODEL,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
        "stream": false,
    })
}

/// Only a 2xx proves the credential was accepted. Nous can answer 4xx before
/// authentication runs (402 when `Authorization` is absent), so a "not 401"
/// rule would accept anything.
pub fn is_api_key_accepted(status: u16) -> bool {
    (200..300).contains(&status)
}

pub fn api_key_validation_error(status: u16) -> &'static str {
    if status == 401 || status == 403 {
        "Invalid or inactive API key"
    } else {
        "Unable to verify API key"
    }
}

fn as_positive_integer(value: Option<&Value>) -> Option<i64> {
    let n = value?.as_i64()?;
    (n > 0).then_some(n)
}

fn is_text_capable(model: &Value) -> bool {
    match model
        .get("architecture")
        .and_then(|a| a.get("output_modalities"))
        .and_then(Value::as_array)
    {
        None => true,
        Some(modalities) => {
            modalities.is_empty() || modalities.iter().any(|m| m.as_str() == Some("text"))
        }
    }
}

fn raw_models(payload: &Value) -> Vec<Value> {
    if let Some(list) = payload.as_array() {
        return list.clone();
    }
    for key in ["data", "models", "results"] {
        if let Some(list) = payload.get(key).and_then(Value::as_array) {
            return list.clone();
        }
    }
    Vec::new()
}

/// `normalizeNousModels`: text-capable entries only, with the context and
/// output limits pulled off `top_provider` when the top level omits them.
pub fn normalize_models(payload: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = raw_models(payload)
        .into_iter()
        .filter(is_text_capable)
        .filter_map(|model| {
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let mut entry = serde_json::Map::new();
            entry.insert("id".into(), json!(id));
            entry.insert(
                "name".into(),
                model
                    .get("name")
                    .and_then(Value::as_str)
                    .map_or_else(|| json!(id), |n| json!(n)),
            );
            let context = as_positive_integer(model.get("context_length"))
                .or_else(|| as_positive_integer(model.pointer("/top_provider/context_length")));
            if let Some(context) = context {
                entry.insert("contextLength".into(), json!(context));
            }
            if let Some(max_output) =
                as_positive_integer(model.pointer("/top_provider/max_completion_tokens"))
            {
                entry.insert("maxOutputTokens".into(), json!(max_output));
            }
            if let Some(description) = model.get("description").and_then(Value::as_str) {
                entry.insert("description".into(), json!(description));
            }
            Some(Value::Object(entry))
        })
        .collect();
    // First-party models first, preserving upstream order inside each group.
    out.sort_by_key(|m| {
        let first_party = m
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("nousresearch/"));
        !first_party
    });
    out
}

fn is_zero_price(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(v) => v.as_f64() == Some(0.0),
    }
}

/// `normalizeNousFreeModels`: the zero-cost text catalogue the provider page
/// offers, largest context first.
pub fn normalize_free_models(payload: &Value) -> Vec<Value> {
    let free: Vec<Value> = raw_models(payload)
        .into_iter()
        .filter(|model| {
            let context = as_positive_integer(model.get("context_length"))
                .or_else(|| as_positive_integer(model.pointer("/top_provider/context_length")));
            is_text_capable(model)
                && is_zero_price(model.pointer("/pricing/prompt"))
                && is_zero_price(model.pointer("/pricing/completion"))
                && context.is_some_and(|c| c >= 200_000)
        })
        .collect();
    let mut out = normalize_models(&Value::Array(free));
    out.sort_by_key(|m| {
        std::cmp::Reverse(m.get("contextLength").and_then(Value::as_i64).unwrap_or(0))
    });
    out
}

/// The live list is emitted as `nous/<upstream id>` because `parse_model` splits
/// at the first `/`: a bare `poolside/laguna-s-2.1:free` would resolve to the
/// existing `poolside` provider instead of Nous.
pub fn prefix_model_ids(models: Vec<Value>) -> Vec<Value> {
    models
        .into_iter()
        .map(|model| {
            let Some(id) = model.get("id").and_then(Value::as_str).map(str::to_string) else {
                return model;
            };
            let mut out = model.as_object().cloned().unwrap_or_default();
            out.insert("id".into(), json!(format!("nous/{id}")));
            out.insert("upstreamModelId".into(), json!(id));
            Value::Object(out)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_2xx_counts_as_accepted() {
        for status in [200, 201, 204] {
            assert!(is_api_key_accepted(status), "{status} should be accepted");
        }
        for status in [400, 401, 402, 403, 404, 429, 500] {
            assert!(!is_api_key_accepted(status), "{status} should be rejected");
        }
    }

    #[test]
    fn validation_error_text_splits_auth_from_other_failures() {
        assert_eq!(api_key_validation_error(401), "Invalid or inactive API key");
        assert_eq!(api_key_validation_error(403), "Invalid or inactive API key");
        assert_eq!(api_key_validation_error(500), "Unable to verify API key");
    }

    #[test]
    fn normalization_reads_both_context_shapes_and_drops_non_text() {
        let payload = json!({
            "data": [
                { "id": "a/text", "name": "A", "context_length": 262144,
                  "top_provider": { "max_completion_tokens": 32768 } },
                { "id": "b/embed", "architecture": { "output_modalities": ["embedding"] } },
                { "id": "c/image", "architecture": { "output_modalities": ["image"] } },
                { "id": "d/ctx", "top_provider": { "context_length": 1000 } },
            ]
        });
        let models = normalize_models(&payload);
        let ids: Vec<&str> = models
            .iter()
            .filter_map(|m| m.get("id").and_then(Value::as_str))
            .collect();
        assert_eq!(ids, vec!["a/text", "d/ctx"]);
        assert_eq!(models[0]["contextLength"], json!(262144));
        assert_eq!(models[0]["maxOutputTokens"], json!(32768));
        assert_eq!(models[1]["contextLength"], json!(1000));
    }

    #[test]
    fn free_filter_needs_zero_price_and_a_large_context() {
        let payload = json!({
            "data": [
                { "id": "free/big", "context_length": 262144,
                  "pricing": { "prompt": 0, "completion": 0 } },
                { "id": "paid/big", "context_length": 262144,
                  "pricing": { "prompt": 0.1, "completion": 0.2 } },
                { "id": "free/small", "context_length": 8000,
                  "pricing": { "prompt": 0, "completion": 0 } },
            ]
        });
        let models = normalize_free_models(&payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], json!("free/big"));
    }

    #[test]
    fn live_ids_are_provider_prefixed() {
        let models = prefix_model_ids(vec![json!({ "id": "poolside/laguna-xs-2.1:free" })]);
        assert_eq!(models[0]["id"], json!("nous/poolside/laguna-xs-2.1:free"));
        assert_eq!(
            models[0]["upstreamModelId"],
            json!("poolside/laguna-xs-2.1:free")
        );
    }
}
