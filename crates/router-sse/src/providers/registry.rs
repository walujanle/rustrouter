//! The provider registry: the committed `registry.json` and the lookups the
//! router resolves on every request.
//!
//! `registry.json` is generated data that bakes in the six derived behaviours —
//! the `format: "openai"` default, the OAuth field injection, the `transports[]`
//! copy, `PROVIDER_MODELS` keyed by `alias || id`, the `MEDIA_KEYS` hoist, and
//! the TTS tables — so nothing here re-derives them. Re-deriving would be a
//! second source of truth that can drift.
//!
//! Everything parses once into `REGISTRY` and the hash maps are built at the
//! same time, so a lookup is a hash probe with no allocation and no re-parse.

use std::collections::HashMap;
use std::sync::LazyLock;

use indexmap::IndexMap;

use serde::Deserialize;
use serde_json::Value;

use crate::providers::model::{Model, Transport};

/// The committed dump.
static REGISTRY_JSON: &str = include_str!("registry.json");

/// Parsed registry and its indexes, built once per process.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    Registry::parse(REGISTRY_JSON).expect("registry.json is generated and must parse")
});

/// Short names for media-only providers whose id, `alias` and `aliases` did not
/// already cover them. Every one of those providers (`elevenlabs`, `jina-ai`,
/// `aws-polly`) is not shipped, so the table is empty and the aliases come from
/// the entries alone.
const MEDIA_ONLY_ALIASES: [(&str, &str); 0] = [];

/// The raw shape of `registry.json`.
#[derive(Debug, Deserialize)]
struct RegistryFile {
    registry: Vec<Provider>,
    providers: HashMap<String, Transport>,
    /// `IndexMap`, not `HashMap`: `/v1/models`' connections-empty branch and
    /// `/v1beta/models` iterate `PROVIDER_MODELS` in file order.
    #[serde(rename = "providerModels")]
    provider_models: IndexMap<String, Vec<Model>>,
    #[serde(rename = "providerOauth")]
    provider_oauth: HashMap<String, Value>,
    #[serde(rename = "providerMedia")]
    provider_media: HashMap<String, Value>,
}

/// One registry entry.
///
/// Fields the router reads in typed form are declared; the rest land in
/// `extra` so a registry change does not need a Rust change to be visible. The
/// media-config fields (`serviceKinds`, `ttsConfig`, …) are deliberately in
/// `extra` — they are passed through to the UI projection as a group.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub alias: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub ui_alias: Option<String>,
    pub category: Option<String>,
    pub auth_type: Option<Value>,
    /// `Option`, not a defaulted `Vec`: the UI projection emits `authModes`
    /// only when the entry declares it, and an empty array is still emitted
    /// because it is truthy.
    pub auth_modes: Option<Vec<String>>,
    pub auth_hint: Option<String>,
    pub has_oauth: Option<bool>,
    pub no_auth: Option<bool>,
    pub hidden: Option<bool>,
    pub priority: Option<i64>,
    pub has_free: Option<bool>,
    pub has_provider_specific_data: Option<bool>,
    pub passthrough_models: Option<bool>,
    pub display: Option<Value>,
    pub transport: Option<Transport>,
    #[serde(default)]
    pub transports: Vec<Transport>,
    pub oauth: Option<Value>,
    pub thinking_config: Option<Value>,
    pub regions: Option<Value>,
    pub default_region: Option<String>,
    pub models: Option<Vec<Model>>,
    /// Every field not named above, in insertion order (`preserve_order`).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl Provider {
    /// `entry.hidden === true`.
    pub fn is_hidden(&self) -> bool {
        self.hidden == Some(true)
    }

    /// The token the UI shows: `uiAlias || alias`. The `||` falls through on an
    /// empty string, so an empty `uiAlias` yields the `alias`.
    pub fn ui_alias_or_alias(&self) -> Option<&str> {
        self.ui_alias
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.alias.as_deref().filter(|s| !s.is_empty()))
    }
}

/// The parsed registry plus the indexes built from it.
pub struct Registry {
    entries: Vec<Provider>,
    transports: HashMap<String, Transport>,
    models: IndexMap<String, Vec<Model>>,
    oauth: HashMap<String, Value>,
    media: HashMap<String, Value>,
    by_id: HashMap<String, usize>,
    /// `ALIAS_TO_PROVIDER_ID`: id, `alias`, every `aliases[]` token, and the
    /// media-only aliases — each mapping to the id.
    alias_to_id: HashMap<String, String>,
    /// `OAUTH_ALIASES`: id → `alias` for entries whose alias differs from the id.
    id_to_alias: HashMap<String, String>,
}

impl Registry {
    fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let file: RegistryFile = serde_json::from_str(json)?;

        let mut by_id = HashMap::with_capacity(file.registry.len());
        let mut alias_to_id: HashMap<String, String> = MEDIA_ONLY_ALIASES
            .iter()
            .map(|(a, id)| ((*a).to_string(), (*id).to_string()))
            .collect();
        let mut id_to_alias = HashMap::new();

        for (i, entry) in file.registry.iter().enumerate() {
            by_id.insert(entry.id.clone(), i);
            alias_to_id.insert(entry.id.clone(), entry.id.clone());
            if let Some(alias) = &entry.alias {
                alias_to_id.insert(alias.clone(), entry.id.clone());
                if alias != &entry.id {
                    id_to_alias.insert(entry.id.clone(), alias.clone());
                }
            }
            for a in &entry.aliases {
                alias_to_id.insert(a.clone(), entry.id.clone());
            }
        }

        Ok(Self {
            entries: file.registry,
            transports: file.providers,
            models: file.provider_models,
            oauth: file.provider_oauth,
            media: file.provider_media,
            by_id,
            alias_to_id,
            id_to_alias,
        })
    }

    /// Every registry entry, in file order.
    pub fn entries(&self) -> &[Provider] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Provider> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    /// `PROVIDERS[id]` — the built transport, with the OAuth injection and the
    /// `format` default already applied by the dump.
    pub fn transport(&self, id: &str) -> Option<&Transport> {
        self.transports.get(id)
    }

    /// `PROVIDER_MODELS[key]`, keyed by `alias || id` as the dump built it.
    pub fn models_for(&self, alias_or_id: &str) -> &[Model] {
        self.models
            .get(alias_or_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// True when a `PROVIDER_MODELS` key exists at all — the `!!models` check,
    /// which differs from an empty list.
    pub fn has_models(&self, alias_or_id: &str) -> bool {
        self.models.contains_key(alias_or_id)
    }

    pub fn oauth(&self, id: &str) -> Option<&Value> {
        self.oauth.get(id)
    }

    pub fn media(&self, id: &str) -> Option<&Value> {
        self.media.get(id)
    }

    /// `resolveProviderAlias`: alias or id in, id out.
    pub fn resolve_alias<'a>(&'a self, alias_or_id: &'a str) -> &'a str {
        self.alias_to_id
            .get(alias_or_id)
            .map(String::as_str)
            .unwrap_or(alias_or_id)
    }

    /// `PROVIDER_ID_TO_ALIAS[id]`: the OAuth alias, else the id.
    pub fn alias_for<'a>(&'a self, id: &'a str) -> &'a str {
        self.id_to_alias.get(id).map(String::as_str).unwrap_or(id)
    }

    /// `PROVIDER_MODELS` in file order — the key and its models.
    /// Both consumers that need the whole table (the connections-empty
    /// `/v1/models` branch, `/v1beta/models`) iterate it.
    pub fn models_iter(&self) -> impl Iterator<Item = (&str, &[Model])> {
        self.models.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    /// The built transport ids, the domain `PROVIDER_ID_TO_ALIAS` is derived
    /// over.
    pub fn transport_ids(&self) -> impl Iterator<Item = &str> {
        self.transports.keys().map(String::as_str)
    }

    /// Ids with `features.usage` set, in registry order.
    pub fn usage_supported(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|p| {
                p.extra
                    .get("features")
                    .and_then(|f| f.get("usage"))
                    .and_then(Value::as_bool)
                    == Some(true)
            })
            .map(|p| p.id.as_str())
            .collect()
    }

    /// Ids with `features.usageApikey` set, in registry order.
    pub fn usage_apikey(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|p| {
                p.extra
                    .get("features")
                    .and_then(|f| f.get("usageApikey"))
                    .and_then(Value::as_bool)
                    == Some(true)
            })
            .map(|p| p.id.as_str())
            .collect()
    }
}

/// Convenience: the process-wide registry.
pub fn registry() -> &'static Registry {
    &REGISTRY
}

/// `PROVIDER_MODELS[key]` as the raw JSON objects `registry.json` stores.
///
/// `Model` is deserialize-only and its `#[serde(flatten)] extra` cannot restore
/// the original key order, so a payload that has to be byte-identical to the
/// `{ ...model, id, name }` projection reads from here instead. The second parse
/// is lazy — only the static-model fallback pays for it.
pub fn raw_models_for(key: &str) -> Option<&'static Vec<Value>> {
    static RAW: LazyLock<IndexMap<String, Vec<Value>>> = LazyLock::new(|| {
        #[derive(Deserialize)]
        struct RawModels {
            #[serde(rename = "providerModels")]
            provider_models: IndexMap<String, Vec<Value>>,
        }
        serde_json::from_str::<RawModels>(REGISTRY_JSON)
            .map(|r| r.provider_models)
            .unwrap_or_default()
    });
    RAW.get(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_generated_file() {
        let r = registry();
        assert_eq!(r.entries().len(), 24);
        assert!(r.get("deepseek").is_some());
        assert!(r.get("openrouter").is_some());
    }

    #[test]
    fn alias_resolution_covers_id_alias_and_aliases() {
        let r = registry();
        assert_eq!(r.resolve_alias("deepseek"), "deepseek");
        assert_eq!(r.resolve_alias("ds"), "deepseek", "alias resolves");
        assert_eq!(r.resolve_alias("brave"), "brave-search", "media-only alias");
        // Unknown tokens pass through untouched.
        assert_eq!(r.resolve_alias("nope"), "nope");
    }

    #[test]
    fn transports_carry_the_applied_defaults() {
        let r = registry();
        // The dump re-applies the shared `format: "openai"` default.
        assert_eq!(
            r.transport("deepseek").unwrap().format_or_default(),
            "openai"
        );
        // A provider that also speaks claude keeps that declared transport.
        let deepseek = r.get("deepseek").expect("deepseek is registered");
        assert!(
            deepseek
                .transports
                .iter()
                .any(|t| t.format_or_default() == "claude"),
            "deepseek carries a claude-format transport"
        );
    }

    #[test]
    fn models_are_keyed_by_alias_or_id_and_are_normalized() {
        let r = registry();
        let models = r.models_for("deepseek");
        assert!(!models.is_empty());
        // The dump fills `name` when the entry omits it.
        assert!(models.iter().all(|m| m.name.is_some()));
        // Duplicate ids are load-bearing (upstreamModelId mapping) and survive.
        let dup_ids = models
            .iter()
            .filter(|m| m.id == "deepseek-v4-pro-max")
            .count();
        assert_eq!(dup_ids, 1);
        let upstream = models
            .iter()
            .find(|m| m.id == "deepseek-v4-pro-max")
            .unwrap();
        assert_eq!(
            upstream.upstream_model_id.as_deref(),
            Some("deepseek-v4-pro")
        );
    }

    #[test]
    fn media_only_providers_have_no_transport_but_do_have_media() {
        let r = registry();
        assert!(r.transport("brave-search").is_none());
        assert!(r.media("brave-search").is_some());
    }

    #[test]
    fn provider_id_to_alias_uses_the_oauth_alias() {
        let r = registry();
        // Entries whose alias equals their id keep the id.
        assert_eq!(r.alias_for("vertex"), "vertex");
        // An entry with a distinct alias resolves through it.
        let entry = r
            .entries()
            .iter()
            .find(|p| p.alias.as_deref().is_some_and(|a| a != p.id));
        let entry = entry.expect("at least one entry has a distinct alias");
        assert_eq!(r.alias_for(&entry.id), entry.alias.as_deref().unwrap());
    }
}
