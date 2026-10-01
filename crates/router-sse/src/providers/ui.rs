//! The UI projection over the registry.
//!
//! The Vue app fetches these shapes from the Rust API, so this is the only place
//! the projection exists.
//!
//! `build_provider_entry` is order-sensitive: `display` is spread first and then
//! conditionally overridden, and the JSON key order is what the frontend sees.

use serde_json::{Map, Value, json};

use crate::providers::model::DEFAULT_KIND;
use crate::providers::registry::{Provider, registry};

/// The registry stores the literal token `"RISK_NOTICE"` to avoid an import
/// cycle; the UI resolves it to this text here.
pub const RISK_NOTICE: &str = "⚠️ Risk Notice: This provider uses a subscription/OAuth session not officially licensed for proxy/router use. Account may be restricted or banned. Use at your own risk.";

/// `MEDIA_ENTRY_KEYS` — a superset of the runtime `MEDIA_KEYS`, adding
/// `credentialFallback` and `systemoneConfig`.
///
/// `sttConfig`, `ttsConfig`, `imageConfig`, `videoConfig`, `musicConfig`,
/// `imageToTextConfig` and `searchViaChat` are absent: no media entry carries
/// them, so a whitelist entry would copy a key that never exists.
const MEDIA_ENTRY_KEYS: [&str; 9] = [
    "serviceKinds",
    "embeddingConfig",
    "searchConfig",
    "fetchConfig",
    "credentialFallback",
    "systemoneConfig",
    "modelsFetcher",
    "mediaPriority",
    "hiddenKinds",
];

/// `buildProviderEntry(r)`: the per-provider object the dashboard renders.
///
/// The key order is the spread order, and the conditional fields use truthiness,
/// not "is set": `regions: []` is emitted (an empty array is truthy) while
/// `regions: null` is not, and `priority: 0` is emitted while
/// `priority: undefined` is not.
pub fn build_provider_entry(r: &Provider) -> Value {
    let mut out = Map::new();

    if let Some(display) = r.display.as_ref().and_then(Value::as_object) {
        for (k, v) in display {
            // The registry stores the token; the UI shows the text.
            if k == "deprecationNotice" && v.as_str() == Some("RISK_NOTICE") {
                out.insert(k.clone(), json!(RISK_NOTICE));
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
    }

    out.insert("id".into(), json!(r.id));
    // `alias: r.uiAlias || r.alias` — absent when neither is set.
    if let Some(alias) = r.ui_alias_or_alias() {
        out.insert("alias".into(), json!(alias));
    }

    // Every remaining field is behind a truthiness check. `false` and `""`
    // are falsy and so are skipped; an object or array is always truthy, so
    // presence is the only test it needs.
    if r.hidden == Some(true) {
        out.insert("hidden".into(), json!(true));
    }
    for (k, v) in media_fields(r) {
        out.insert(k, v);
    }
    if let Some(p) = r.priority {
        out.insert("priority".into(), json!(p));
    }
    for (present, key) in [
        (r.has_free, "hasFree"),
        (r.has_provider_specific_data, "hasProviderSpecificData"),
        (r.no_auth, "noAuth"),
        (r.passthrough_models, "passthroughModels"),
        (r.has_oauth, "hasOAuth"),
    ] {
        if present == Some(true) {
            out.insert(key.into(), json!(true));
        }
    }
    if let Some(t) = &r.thinking_config {
        out.insert("thinkingConfig".into(), t.clone());
    }
    if let Some(regions) = &r.regions {
        out.insert("regions".into(), regions.clone());
        if let Some(dr) = &r.default_region {
            out.insert("defaultRegion".into(), json!(dr));
        }
    }
    if let Some(modes) = &r.auth_modes {
        out.insert("authModes".into(), json!(modes));
    }
    if let Some(a) = r.auth_type.as_ref().filter(|v| !v.is_null()) {
        out.insert("authType".into(), a.clone());
    }
    if let Some(h) = r.auth_hint.as_deref().filter(|s| !s.is_empty()) {
        out.insert("authHint".into(), json!(h));
    }

    Value::Object(out)
}

/// The media fields the entry declares, whether at the top level or under a
/// legacy `media` object. Top-level wins on a collision, matching the
/// `Object.assign` order.
fn media_fields(r: &Provider) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(legacy) = r.extra.get("media").and_then(Value::as_object) {
        for (k, v) in legacy {
            out.insert(k.clone(), v.clone());
        }
    }
    for k in MEDIA_ENTRY_KEYS {
        if let Some(v) = r.extra.get(k) {
            out.insert(k.to_string(), v.clone());
        }
    }
    out
}

/// `AI_PROVIDERS` split by category, preserving registry order within each.
pub fn providers_by_category(category: &str) -> Map<String, Value> {
    let mut out = Map::new();
    for r in registry()
        .entries()
        .iter()
        .filter(|r| r.category.as_deref() == Some(category))
    {
        out.insert(r.id.clone(), build_provider_entry(r));
    }
    out
}

/// The five category maps, keyed by the constant name the frontend uses.
pub fn provider_categories() -> Map<String, Value> {
    let mut out = Map::new();
    for cat in ["free", "freeTier", "oauth", "apikey", "webCookie"] {
        out.insert(cat.to_string(), Value::Object(providers_by_category(cat)));
    }
    out
}

/// `MEDIA_PROVIDER_KINDS`: the media kind table and the endpoint each kind maps
/// to.
///
/// `image`, `tts`, `video`, `music` and `imageToText` are absent: those kinds
/// are dropped outright, so no provider declares them and the table would
/// advertise endpoints with no route behind them. The imageToText row advertised
/// `/v1/images/understanding`, which is not routed.
pub fn media_provider_kinds() -> Value {
    json!([
        { "id": "embedding", "label": "Embedding", "icon": "data_array", "endpoint": { "method": "POST", "path": "/v1/embeddings" } },
        { "id": "webSearch", "label": "Web Search", "icon": "travel_explore", "endpoint": { "method": "POST", "path": "/v1/search" } },
        { "id": "webFetch", "label": "Web Fetch", "icon": "language", "endpoint": { "method": "POST", "path": "/v1/web/fetch" } },
        { "id": "systemone", "label": "System One", "icon": "psychology", "endpoint": { "method": "POST", "path": "/v1/systemone" }, "isNew": true }
    ])
}

/// `getProvidersByKind(kind)`: providers serving `kind`, hidden ones and
/// hidden-kind ones dropped, sorted by `priority ?? mediaPriority ?? 999`.
pub fn providers_by_kind(kind: &str) -> Vec<Value> {
    let mut out: Vec<(i64, Value)> = registry()
        .entries()
        .iter()
        .filter(|r| {
            if r.is_hidden() {
                return false;
            }
            let kinds: Vec<&str> = match r.extra.get("serviceKinds").and_then(Value::as_array) {
                Some(list) => list.iter().filter_map(Value::as_str).collect(),
                None => vec![DEFAULT_KIND],
            };
            if !kinds.contains(&kind) {
                return false;
            }
            if let Some(hk) = r.extra.get("hiddenKinds").and_then(Value::as_array)
                && hk.iter().any(|v| v.as_str() == Some(kind))
            {
                return false;
            }
            true
        })
        .map(|r| {
            let rank = r
                .priority
                .or_else(|| r.extra.get("mediaPriority").and_then(Value::as_i64))
                .unwrap_or(999);
            (rank, build_provider_entry(r))
        })
        .collect();
    // `sort` is stable, so equal ranks keep registry order.
    out.sort_by_key(|(rank, _)| *rank);
    out.into_iter().map(|(_, v)| v).collect()
}

/// `ALIAS_TO_ID` and `ID_TO_ALIAS` over `AI_PROVIDERS`.
///
/// The key is `uiAlias || alias`. A provider declaring neither would write the
/// literal key `"undefined"` in `ALIAS_TO_ID`, with `ID_TO_ALIAS` getting an
/// `undefined` value that serialization drops. No shipped provider does; the
/// branch stays for parity.
pub fn alias_maps() -> (Map<String, Value>, Map<String, Value>) {
    let mut alias_to_id = Map::new();
    let mut id_to_alias = Map::new();
    for r in registry().entries() {
        match r.ui_alias_or_alias() {
            Some(alias) => {
                alias_to_id.insert(alias.to_string(), json!(r.id));
                id_to_alias.insert(r.id.clone(), json!(alias));
            }
            None => {
                alias_to_id.insert("undefined".to_string(), json!(r.id));
            }
        }
    }
    (alias_to_id, id_to_alias)
}

/// `OPENAI_COMPATIBLE_PREFIX` and friends, plus the prefix predicates.
pub const OPENAI_COMPATIBLE_PREFIX: &str = "openai-compatible-";
pub const ANTHROPIC_COMPATIBLE_PREFIX: &str = "anthropic-compatible-";
pub const CUSTOM_EMBEDDING_PREFIX: &str = "custom-embedding-";

pub fn is_openai_compatible_provider(id: &str) -> bool {
    id.starts_with(OPENAI_COMPATIBLE_PREFIX)
}
pub fn is_anthropic_compatible_provider(id: &str) -> bool {
    id.starts_with(ANTHROPIC_COMPATIBLE_PREFIX)
}
pub fn is_custom_embedding_provider(id: &str) -> bool {
    id.starts_with(CUSTOM_EMBEDDING_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::registry::registry;

    #[test]
    fn entry_carries_display_then_identity() {
        let r = registry().get("deepseek").unwrap();
        let e = build_provider_entry(r);
        let obj = e.as_object().unwrap();
        // display first, then id and alias.
        let keys: Vec<&String> = obj.keys().collect();
        assert!(
            keys.iter().position(|k| *k == "id").unwrap()
                < keys.iter().position(|k| *k == "alias").unwrap()
        );
        assert_eq!(obj["id"], json!("deepseek"));
        assert_eq!(obj["alias"], json!("ds"), "uiAlias wins over alias");
        assert_eq!(obj["name"], json!("DeepSeek"));
    }

    #[test]
    fn risk_notice_token_is_resolved_for_the_ui() {
        let r = registry()
            .entries()
            .iter()
            .find(|r| {
                r.display
                    .as_ref()
                    .and_then(|d| d.get("deprecationNotice"))
                    .and_then(Value::as_str)
                    == Some("RISK_NOTICE")
            })
            .expect("some provider carries the RISK_NOTICE token");
        let e = build_provider_entry(r);
        assert_eq!(e["deprecationNotice"], json!(RISK_NOTICE));
    }

    #[test]
    fn hidden_flag_is_omitted_when_unset() {
        let visible = build_provider_entry(registry().get("deepseek").unwrap());
        assert!(visible.get("hidden").is_none(), "false must not be emitted");
    }

    #[test]
    fn categories_partition_the_registry() {
        let cats = provider_categories();
        let total: usize = ["free", "freeTier", "oauth", "apikey", "webCookie"]
            .into_iter()
            .map(|c| cats[c].as_object().unwrap().len())
            .sum();
        assert_eq!(
            total,
            registry().entries().len(),
            "every entry lands in a category"
        );
    }

    #[test]
    fn providers_by_kind_excludes_hidden_and_sorts_by_priority() {
        let llm = providers_by_kind("llm");
        assert!(!llm.is_empty());
        assert!(
            llm.iter().all(|p| p.get("hidden").is_none()),
            "hidden providers are filtered"
        );
        let priorities: Vec<i64> = llm
            .iter()
            .map(|p| p["priority"].as_i64().unwrap_or(999))
            .collect();
        assert!(
            priorities.windows(2).all(|w| w[0] <= w[1]),
            "sorted ascending"
        );
    }

    #[test]
    fn alias_maps_round_trip() {
        let (alias_to_id, id_to_alias) = alias_maps();
        for (id, alias) in &id_to_alias {
            assert_eq!(
                alias_to_id[alias.as_str().unwrap()],
                json!(id),
                "{id} round-trips"
            );
        }
        assert_eq!(alias_to_id["ds"], json!("deepseek"));
    }

    #[test]
    fn every_kept_entry_declares_an_alias() {
        // A provider with neither uiAlias nor alias would land under the literal
        // key "undefined". No shipped provider does.
        let (alias_to_id, _) = alias_maps();
        assert!(
            !alias_to_id.contains_key("undefined"),
            "a kept entry has no alias"
        );
    }

    #[test]
    fn compat_prefixes() {
        assert!(is_openai_compatible_provider("openai-compatible-x"));
        assert!(is_anthropic_compatible_provider("anthropic-compatible-x"));
        assert!(is_custom_embedding_provider("custom-embedding-x"));
        assert!(!is_openai_compatible_provider("openai"));
    }
}
