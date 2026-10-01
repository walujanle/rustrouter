//! Model-string resolution.
//!
//! The registry-derived half (`parseModel`, `resolveProviderAlias`,
//! `inferProviderFromModelName`, the alias tables) already lives in
//! `providers::lookup`; this module is the composition over it, where the
//! caller's alias table, the combo lookup and the provider nodes may come from
//! the database.
//!
//! `get_model_info` returns a `ModelInfo` whose `provider` is empty to signal a
//! combo, and the caller routes that to the combo path. An empty provider is
//! otherwise unreachable for a non-empty model string (`get_model_info_core`
//! falls back to `"openai"`), and *any* empty provider is treated as "handle as
//! combo" regardless of which branch produced it.

use std::collections::HashSet;
use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::providers::lookup::{
    ParsedModel, builtin_model_aliases, infer_provider_from_model_name, parse_model,
    resolve_model_alias_from_map,
};
use crate::providers::registry::registry;

/// `LOCAL_PROVIDER_ALIASES`: HMR-friendly local overrides applied on top of the
/// registry-derived alias map.
pub const LOCAL_PROVIDER_ALIASES: [(&str, &str); 2] = [
    ("xmtp", "xiaomi-tokenplan"),
    ("xiaomi-tokenplan", "xiaomi-tokenplan"),
];

/// The resolved `{ provider, model }` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// Empty signals a combo (`provider: null` in the JSON shape).
    pub provider: String,
    pub model: String,
}

impl ModelInfo {
    /// The combo short-circuit: an empty provider.
    pub fn is_combo(&self) -> bool {
        self.provider.is_empty()
    }
}

/// `RESERVED_PROVIDER_PREFIXES`: the local aliases plus every registry id,
/// `alias` and `aliases[]`. A user-defined provider-node prefix must not
/// override a built-in id/alias.
pub fn reserved_provider_prefixes() -> &'static HashSet<String> {
    static SET: LazyLock<HashSet<String>> = LazyLock::new(|| {
        let mut set: HashSet<String> = LOCAL_PROVIDER_ALIASES
            .iter()
            .map(|(k, _)| (*k).to_string())
            .collect();
        for entry in registry().entries() {
            set.insert(entry.id.clone());
            if let Some(alias) = &entry.alias {
                set.insert(alias.clone());
            }
            for alias in &entry.aliases {
                set.insert(alias.clone());
            }
        }
        set
    });
    &SET
}

/// `parseModel(modelStr)` (app side): `parse_model`, then the local provider
/// aliases override the resolved provider.
pub fn parse_model_app(model_str: &str) -> ParsedModel {
    let mut parsed = parse_model(Some(model_str));
    let provider_alias = parsed.provider_alias.clone().unwrap_or_default();
    if let Some((_, mapped)) = LOCAL_PROVIDER_ALIASES
        .iter()
        .find(|(alias, _)| *alias == provider_alias)
    {
        parsed.provider = Some((*mapped).to_string());
    }
    parsed
}

/// `getModelInfo(modelStr)` (app side).
///
/// `nodes` are the `providerNodes` rows (`{id, type, prefix, …}`). A
/// `provider/model` prefix that is not reserved and matches a node of type
/// `openai-compatible`, `anthropic-compatible` or `custom-embedding` resolves
/// to that node's id. A bare name that matches a combo short-circuits to an
/// empty provider *before* alias resolution, so a combo name is never routed to
/// a provider.
pub fn get_model_info(
    model_str: &str,
    aliases: Option<&Map<String, Value>>,
    combo_lookup: impl Fn(&str) -> Option<Value>,
    nodes: &[Value],
) -> ModelInfo {
    let parsed = parse_model_app(model_str);

    if !parsed.is_alias {
        let provider_alias = parsed.provider_alias.clone().unwrap_or_default();
        let model = parsed.model.clone().unwrap_or_default();
        if !reserved_provider_prefixes().contains(&provider_alias) {
            for node_type in [
                "openai-compatible",
                "anthropic-compatible",
                "custom-embedding",
            ] {
                if let Some(node) = nodes.iter().find(|n| {
                    n.get("type").and_then(Value::as_str) == Some(node_type)
                        && n.get("prefix").and_then(Value::as_str) == Some(provider_alias.as_str())
                }) {
                    return ModelInfo {
                        provider: node
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        model: model.clone(),
                    };
                }
            }
        }
        return ModelInfo {
            provider: parsed.provider.unwrap_or_default(),
            model,
        };
    }

    let model = parsed.model.unwrap_or_default();
    // A combo name must not be resolved as a model alias.
    if combo_lookup(&model).is_some() {
        return ModelInfo {
            provider: String::new(),
            model,
        };
    }

    get_model_info_core(Some(model_str), aliases)
}

/// `getComboModels(modelStr)` (app side): the model list of a bare combo name,
/// or `None` when it is a `provider/model` pair, not a combo, or the combo has
/// no models.
pub fn get_combo_models(
    model_str: &str,
    combo_lookup: impl Fn(&str) -> Option<Value>,
) -> Option<Vec<String>> {
    if model_str.contains('/') {
        return None;
    }
    let combo = combo_lookup(model_str)?;
    let models = combo.get("models")?.as_array()?;
    if models.is_empty() {
        return None;
    }
    Some(
        models
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
    )
}

/// `getModelInfoCore(modelStr, aliasesOrGetter)`.
///
/// `aliases` is the caller's alias table (`null` in JS terms when the caller has
/// none). Resolution order is the caller's table, then `BUILTIN_MODEL_ALIASES`,
/// then prefix inference — and the prefix fallback keeps the original model
/// string, so `claude-3` resolves to `{anthropic, claude-3}`.
pub fn get_model_info_core(
    model_str: Option<&str>,
    aliases: Option<&serde_json::Map<String, Value>>,
) -> ModelInfo {
    let parsed = parse_model(model_str);
    if !parsed.is_alias {
        return ModelInfo {
            provider: parsed.provider.unwrap_or_default(),
            model: parsed.model.unwrap_or_default(),
        };
    }

    let model = parsed.model.unwrap_or_default();
    let resolved = resolve_model_alias_from_map(&model, aliases)
        .or_else(|| resolve_model_alias_from_map(&model, Some(builtin_model_aliases())));
    if let Some((provider, model)) = resolved {
        return ModelInfo { provider, model };
    }

    ModelInfo {
        provider: infer_provider_from_model_name(Some(&model)).to_string(),
        model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_prefixed_models_pass_through_resolution() {
        let info = get_model_info_core(Some("gcli/grok-build"), None);
        // `resolveProviderAlias` runs on the prefix, so the alias lands on the
        // provider id.
        assert_eq!(info.provider, "grok-cli");
        assert_eq!(info.model, "grok-build");
    }

    #[test]
    fn a_known_alias_resolves_through_the_callers_table() {
        let aliases: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"fast": "openrouter/anthropic/claude-3-haiku"})).unwrap();
        let info = get_model_info_core(Some("fast"), Some(&aliases));
        assert_eq!(info.provider, "openrouter");
        // Only the first slash splits: the rest is the upstream model id.
        assert_eq!(info.model, "anthropic/claude-3-haiku");
    }

    #[test]
    fn an_object_valued_alias_resolves_too() {
        let aliases: serde_json::Map<String, Value> =
            serde_json::from_value(json!({"fast": {"provider": "openrouter", "model": "x"}}))
                .unwrap();
        let info = get_model_info_core(Some("fast"), Some(&aliases));
        assert_eq!(info.provider, "openrouter");
        assert_eq!(info.model, "x");
    }

    #[test]
    fn the_builtin_alias_applies_when_the_caller_has_no_entry() {
        let info = get_model_info_core(Some("grok-build"), None);
        assert_eq!(info.provider, "grok-cli");
        assert_eq!(info.model, "grok-build");
    }

    #[test]
    fn an_unknown_bare_name_falls_back_to_prefix_inference() {
        assert_eq!(get_model_info_core(Some("gpt-5"), None).provider, "openai");
        assert_eq!(
            get_model_info_core(Some("claude-opus-5"), None).provider,
            "anthropic"
        );
        assert_eq!(
            get_model_info_core(Some("deepseek-chat"), None).provider,
            "openrouter"
        );
        // Unmatched: the inference default.
        assert_eq!(
            get_model_info_core(Some("mystery-model"), None).provider,
            "openai"
        );
    }

    #[test]
    fn an_empty_model_string_is_not_an_alias() {
        let info = get_model_info_core(None, None);
        assert_eq!(info.provider, "");
        assert_eq!(info.model, "");
    }

    #[test]
    fn the_local_alias_rewrites_xmtp() {
        let parsed = parse_model_app("xmtp/some-model");
        assert_eq!(parsed.provider.as_deref(), Some("xiaomi-tokenplan"));
        assert_eq!(parsed.model.as_deref(), Some("some-model"));
        // The identity mapping is a no-op but still routed through the table.
        assert_eq!(
            parse_model_app("xiaomi-tokenplan/x").provider.as_deref(),
            Some("xiaomi-tokenplan")
        );
        // A registry alias is untouched.
        assert_eq!(
            parse_model_app("ds/x").provider.as_deref(),
            Some("deepseek")
        );
    }

    #[test]
    fn reserved_prefixes_cover_the_registry_and_the_local_aliases() {
        let reserved = reserved_provider_prefixes();
        assert!(reserved.contains("xmtp"));
        assert!(reserved.contains("xiaomi-tokenplan"));
        assert!(reserved.contains("deepseek"));
        assert!(reserved.contains("ds"), "registry aliases are reserved");
        assert!(!reserved.contains("my-custom-node"));
    }

    #[test]
    fn a_provider_node_prefix_resolves_to_the_node_id() {
        let nodes = vec![
            json!({"id": "node-1", "type": "openai-compatible", "prefix": "mine"}),
            json!({"id": "node-2", "type": "anthropic-compatible", "prefix": "mine2"}),
            json!({"id": "node-3", "type": "custom-embedding", "prefix": "mine3"}),
        ];
        let none = |_: &str| None;

        let info = get_model_info("mine/gpt-x", None, none, &nodes);
        assert_eq!(info.provider, "node-1");
        assert_eq!(info.model, "gpt-x");
        assert_eq!(
            get_model_info("mine2/claude-x", None, none, &nodes).provider,
            "node-2"
        );
        assert_eq!(
            get_model_info("mine3/embed-x", None, none, &nodes).provider,
            "node-3"
        );

        // A reserved prefix ignores the node table entirely.
        let info = get_model_info("deepseek/x", None, none, &nodes);
        assert_eq!(info.provider, "deepseek");
    }

    #[test]
    fn a_combo_name_short_circuits_before_alias_resolution() {
        // The name is also a user model alias; the combo lookup must win.
        let aliases: Map<String, Value> =
            serde_json::from_value(json!({"fast": "openrouter/anthropic/claude-3-haiku"})).unwrap();
        let combo =
            |name: &str| (name == "fast").then(|| json!({"name": "fast", "models": ["a", "b"]}));

        let info = get_model_info("fast", Some(&aliases), combo, &[]);
        assert!(info.is_combo());
        assert_eq!(info.provider, "");
        assert_eq!(info.model, "fast");

        // Without the combo it resolves as an alias.
        let info = get_model_info("fast", Some(&aliases), |_| None, &[]);
        assert_eq!(info.provider, "openrouter");
        assert!(!info.is_combo());
    }

    #[test]
    fn combo_models_require_a_bare_name_and_a_non_empty_list() {
        let combo = |name: &str| match name {
            "fast" => Some(json!({"models": ["a", "b"]})),
            "empty" => Some(json!({"models": []})),
            _ => None,
        };
        assert_eq!(
            get_combo_models("fast", combo),
            Some(vec!["a".to_string(), "b".to_string()])
        );
        assert!(get_combo_models("fast/a", combo).is_none());
        assert!(get_combo_models("empty", combo).is_none());
        assert!(get_combo_models("missing", combo).is_none());
    }
}
