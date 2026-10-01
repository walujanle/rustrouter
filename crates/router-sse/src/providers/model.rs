//! The registry entry and model shapes, and the per-model defaults the rest of
//! the router reads through.
//!
//! Fields the router reads in typed form are declared; anything else falls into
//! `extra` with `preserve_order`, so a registry addition never needs a Rust
//! change just to round-trip. The accessors apply the per-model defaults
//! (`kind || type || "llm"` and friends) because callers depend on the
//! fallback, not on the raw `Option`.

use serde::Deserialize;
use serde_json::Value;

/// One model row. The same id may appear twice; that is load-bearing — the
/// upstream-id lookup relies on it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: Option<String>,
    pub kind: Option<String>,
    #[serde(rename = "type")]
    pub model_type: Option<String>,
    pub upstream_model_id: Option<String>,
    pub quota_family: Option<String>,
    pub target_format: Option<String>,
    pub supported_formats: Option<Vec<String>>,
    pub strip: Option<Vec<String>>,
    pub params: Option<Value>,
    pub capabilities: Option<Value>,
    pub dimensions: Option<Value>,
    pub context_length: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub rate_multiplier: Option<f64>,
    pub label: Option<String>,
    pub description: Option<String>,
    pub thinking: Option<Value>,
    pub image_gen: Option<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// `MODEL_DEFAULTS.kind`.
pub const DEFAULT_KIND: &str = "llm";
/// `MODEL_DEFAULTS.quotaFamily`.
pub const DEFAULT_QUOTA_FAMILY: &str = "normal";

impl Model {
    /// `modelKind(model)`: `kind || type || "llm"`.
    pub fn kind(&self) -> &str {
        self.kind
            .as_deref()
            .or(self.model_type.as_deref())
            .unwrap_or(DEFAULT_KIND)
    }

    /// `modelQuotaFamily(model)`: `quotaFamily || "normal"`.
    pub fn quota_family(&self) -> &str {
        self.quota_family.as_deref().unwrap_or(DEFAULT_QUOTA_FAMILY)
    }

    /// `modelStrip(model)`: `strip || []`.
    pub fn strip(&self) -> &[String] {
        self.strip.as_deref().unwrap_or(&[])
    }

    /// `modelTargetFormat(model)`: `targetFormat || null`.
    pub fn target_format(&self) -> Option<&str> {
        self.target_format.as_deref()
    }

    /// `modelSupportedFormats(model)`: `supportedFormats || null`.
    pub fn supported_formats(&self) -> Option<&[String]> {
        self.supported_formats.as_deref()
    }

    /// The id sent upstream: `upstreamModelId || id`.
    pub fn upstream_id(&self) -> &str {
        self.upstream_model_id.as_deref().unwrap_or(&self.id)
    }
}

/// One transport block, from a registry entry's `transport` or a `transports[]`
/// element, after the dump applied the `format` default and the OAuth
/// injection.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transport {
    pub base_url: Option<String>,
    pub base_urls: Option<Value>,
    pub format: Option<String>,
    pub headers: Option<serde_json::Map<String, Value>>,
    /// Three shapes exist: a single `{header, scheme, source}`, a per-mode
    /// `{apiKey: …, oauth: …}` map, and a `combined: true` variant. Kept as
    /// `Value` so the executors can read whichever shape the entry uses.
    pub auth: Option<Value>,
    pub auth_type: Option<String>,
    pub no_auth: Option<bool>,
    pub force_stream: Option<bool>,
    pub url_suffix: Option<String>,
    pub quirks: Option<Value>,
    pub retry: Option<Value>,
    pub usage: Option<Value>,
    pub models_fetcher: Option<Value>,
    pub regions: Option<Value>,
    pub default_region: Option<String>,
    pub timeout_ms: Option<i64>,
    pub stall_timeout_ms: Option<i64>,
    pub executor: Option<String>,
    pub validate_url: Option<String>,
    pub responses_url: Option<String>,
    pub chat_path: Option<String>,
    pub messages_url: Option<String>,
    pub models_url: Option<String>,
    pub user_url: Option<String>,
    pub billing_url: Option<String>,
    pub auth_url: Option<String>,
    pub refresh_url: Option<String>,
    pub token_url: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub api_client: Option<String>,
    pub cli_version: Option<String>,
    pub client_version: Option<String>,
    pub client_identifier: Option<String>,
    pub reasoning_inject: Option<Value>,
    pub thinking_format: Option<String>,
    pub token_auth: Option<Value>,
    pub copilot: Option<Value>,
    #[serde(default)]
    pub transports: Vec<Transport>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Transport {
    /// The wire format. The dump already filled `format` when the entry omitted
    /// it, but a `transports[]` element may not carry one, so the same
    /// `"openai"` default is re-applied here.
    pub fn format_or_default(&self) -> &str {
        self.format.as_deref().unwrap_or("openai")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(json: Value) -> Model {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn kind_falls_back_to_type_then_llm() {
        assert_eq!(model(serde_json::json!({"id": "x"})).kind(), "llm");
        assert_eq!(
            model(serde_json::json!({"id": "x", "type": "tts"})).kind(),
            "tts"
        );
        assert_eq!(
            model(serde_json::json!({"id": "x", "kind": "image"})).kind(),
            "image"
        );
        // `kind` wins over the legacy `type`.
        assert_eq!(
            model(serde_json::json!({"id": "x", "kind": "image", "type": "tts"})).kind(),
            "image"
        );
    }

    #[test]
    fn defaults_match_model_defaults() {
        let m = model(serde_json::json!({"id": "x"}));
        assert_eq!(m.quota_family(), "normal");
        assert!(m.strip().is_empty());
        assert_eq!(m.target_format(), None);
        assert_eq!(m.supported_formats(), None);
        assert_eq!(m.upstream_id(), "x");
    }

    #[test]
    fn upstream_id_prefers_the_override() {
        let m = model(serde_json::json!({"id": "a", "upstreamModelId": "b"}));
        assert_eq!(m.upstream_id(), "b");
    }

    #[test]
    fn unknown_fields_round_trip_through_extra() {
        let m = model(serde_json::json!({"id": "x", "brandNewField": {"a": 1}}));
        assert_eq!(m.extra.get("brandNewField").unwrap()["a"], 1);
    }

    #[test]
    fn transport_format_defaults_to_openai() {
        let t: Transport = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(t.format_or_default(), "openai");
        let t: Transport = serde_json::from_value(serde_json::json!({"format": "claude"})).unwrap();
        assert_eq!(t.format_or_default(), "claude");
    }
}
