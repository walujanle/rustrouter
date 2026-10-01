//! Model-string parsing and per-model lookups.
//!
//! These run on every chat request, so the alias maps they need live in the
//! registry and are built once. The `(level)` thinking-suffix handling in
//! `get_model_upstream_id` is the subtle one: the suffix is stripped before the
//! lookup so it hits the base id, then re-appended, because `applyThinking`
//! downstream still needs to see it.

use std::sync::LazyLock;

use regex::Regex;

use crate::providers::model::Model;
use crate::providers::registry::registry;

/// `CODEX_REVIEW_SUFFIX`.
pub const CODEX_REVIEW_SUFFIX: &str = "-review";

/// `FORMATS.OPENAI_RESPONSES`.
pub const FORMAT_OPENAI_RESPONSES: &str = "openai-responses";

/// `BUILTIN_MODEL_ALIASES`.
const BUILTIN_MODEL_ALIASES: [(&str, &str); 1] = [("grok-build", "gcli/grok-build")];

/// `MODEL_PREFIX_PROVIDERS`: prefix → provider, first match wins.
static MODEL_PREFIX_PROVIDERS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"^codex-auto-review$", "codex"),
        (r"^claude-", "anthropic"),
        (r"^gpt-", "openai"),
        (r"^o[134]", "openai"),
        (r"^deepseek-", "openrouter"),
    ]
    .into_iter()
    .map(|(p, prov)| (Regex::new(p).expect("static pattern"), prov))
    .collect()
});

/// A trailing `(...)` group, e.g. the `(high)` in `claude-sonnet-4.5(high)`.
static TRAILING_PAREN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([^()]+\)\s*$").expect("static pattern"));

/// `/^muse[-_]?spark(?:$|[-_:.\s])/i`.
static MUSE_SPARK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^muse[-_]?spark(?:$|[-_:.\s])").expect("static pattern"));

/// The result of `parseModel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub is_alias: bool,
    pub provider_alias: Option<String>,
}

/// `parseModel(modelStr)`: split `provider/model`, else treat the whole string
/// as a model alias.
pub fn parse_model(model_str: Option<&str>) -> ParsedModel {
    let Some(model_str) = model_str.filter(|s| !s.is_empty()) else {
        return ParsedModel {
            provider: None,
            model: None,
            is_alias: false,
            provider_alias: None,
        };
    };

    if let Some(slash) = model_str.find('/') {
        let provider_or_alias = &model_str[..slash];
        return ParsedModel {
            provider: Some(registry().resolve_alias(provider_or_alias).to_string()),
            model: Some(model_str[slash + 1..].to_string()),
            is_alias: false,
            provider_alias: Some(provider_or_alias.to_string()),
        };
    }

    ParsedModel {
        provider: None,
        model: Some(model_str.to_string()),
        is_alias: true,
        provider_alias: None,
    }
}

/// `resolveModelAliasFromMap(alias, aliases)`.
///
/// The map values are either `"provider/model"` or `{provider, model}`; the
/// caller supplies whichever it has (a user alias table or the builtin one).
pub fn resolve_model_alias_from_map(
    alias: &str,
    aliases: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<(String, String)> {
    let resolved = aliases?.get(alias)?;
    if let Some(s) = resolved.as_str() {
        let slash = s.find('/')?;
        return Some((
            registry().resolve_alias(&s[..slash]).to_string(),
            s[slash + 1..].to_string(),
        ));
    }
    let provider = resolved.get("provider")?.as_str()?;
    let model = resolved.get("model")?.as_str()?;
    Some((
        registry().resolve_alias(provider).to_string(),
        model.to_string(),
    ))
}

/// `BUILTIN_MODEL_ALIASES` as a map, so `get_model_info_core` can consult it
/// the same way it consults the caller's table.
pub fn builtin_model_aliases() -> &'static serde_json::Map<String, serde_json::Value> {
    static MAP: LazyLock<serde_json::Map<String, serde_json::Value>> = LazyLock::new(|| {
        BUILTIN_MODEL_ALIASES
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                )
            })
            .collect()
    });
    &MAP
}

/// `inferProviderFromModelName`: prefix match, else `"openai"`.
pub fn infer_provider_from_model_name(model_name: Option<&str>) -> &'static str {
    let Some(name) = model_name.filter(|s| !s.is_empty()) else {
        return "openai";
    };
    let lower = name.to_lowercase();
    MODEL_PREFIX_PROVIDERS
        .iter()
        .find(|(re, _)| re.is_match(&lower))
        .map(|(_, p)| *p)
        .unwrap_or("openai")
}

/// `findModel(models, modelId, aliasOrId)`: exact id, then the id with a
/// trailing `(...)` stripped.
fn find_model<'a>(models: &'a [Model], model_id: &str, _alias_or_id: &str) -> Option<&'a Model> {
    let base = strip_trailing_paren(model_id);
    models.iter().find(|m| m.id == model_id || m.id == base)
}

/// `normalizeModelId`: digit-hyphen-digit → digit-dot-digit.
pub fn normalize_model_id(model_id: &str) -> String {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d)-(\d)").expect("static pattern"));
    RE.replace_all(model_id, "$1.$2").into_owned()
}

/// Strip one trailing `(...)` group and trim, matching
/// `replace(/\([^()]+\)\s*$/, "").trim()`: the `trim` runs even when there is
/// no group, because the unchanged string is still trimmed.
fn strip_trailing_paren(model_id: &str) -> &str {
    match TRAILING_PAREN.find(model_id) {
        Some(m) => model_id[..m.start()].trim(),
        None => model_id.trim(),
    }
}

/// `isMuseSparkModel(modelId)`.
pub fn is_muse_spark_model(model_id: &str) -> bool {
    let clean = strip_trailing_paren(model_id);
    let base = clean.rsplit('/').next().unwrap_or(clean);
    MUSE_SPARK.is_match(base)
}

/// `isValidModel(aliasOrId, modelId, passthroughProviders)`.
pub fn is_valid_model(alias_or_id: &str, model_id: &str, passthrough: bool) -> bool {
    if passthrough {
        return true;
    }
    let models = registry().models_for(alias_or_id);
    if !registry().has_models(alias_or_id) {
        return false;
    }
    find_model(models, model_id, alias_or_id).is_some()
}

/// `getModelType(aliasOrId, modelId)`: `kind || type || null`.
///
/// Unlike `Model::kind`, this does NOT fall back to `"llm"` — this returns
/// `null` for a found model that declares neither.
pub fn model_type(alias_or_id: &str, model_id: &str) -> Option<String> {
    if !registry().has_models(alias_or_id) {
        return None;
    }
    find_model(registry().models_for(alias_or_id), model_id, alias_or_id)
        .and_then(|m| m.kind.as_deref().or(m.model_type.as_deref()))
        .map(str::to_string)
}

/// `getModelTargetFormat(aliasOrId, modelId)`.
pub fn model_target_format(alias_or_id: &str, model_id: &str) -> Option<String> {
    const MUSE_SPARK_PROVIDERS: [&str; 5] = ["", "oc", "opencode", "ocg", "opencode-go"];
    if MUSE_SPARK_PROVIDERS.contains(&alias_or_id) && is_muse_spark_model(model_id) {
        return Some(FORMAT_OPENAI_RESPONSES.to_string());
    }
    if !registry().has_models(alias_or_id) {
        return None;
    }
    find_model(registry().models_for(alias_or_id), model_id, alias_or_id)
        .and_then(|m| m.target_format().map(str::to_string))
}

/// `getModelSupportedFormats(aliasOrId, modelId)`.
pub fn model_supported_formats(alias_or_id: &str, model_id: &str) -> Option<Vec<String>> {
    if !registry().has_models(alias_or_id) {
        return None;
    }
    find_model(registry().models_for(alias_or_id), model_id, alias_or_id)
        .and_then(|m| m.supported_formats().map(<[String]>::to_vec))
}

/// `getModelQuotaFamily(aliasOrId, modelId)`.
pub fn model_quota_family(alias_or_id: &str, model_id: &str) -> String {
    find_model(registry().models_for(alias_or_id), model_id, alias_or_id)
        .map(|m| m.quota_family().to_string())
        .unwrap_or_else(|| crate::providers::model::DEFAULT_QUOTA_FAMILY.to_string())
}

/// `getModelStrip(alias, modelId)`.
pub fn model_strip(alias_or_id: &str, model_id: &str) -> Vec<String> {
    find_model(registry().models_for(alias_or_id), model_id, alias_or_id)
        .map(|m| m.strip().to_vec())
        .unwrap_or_default()
}

/// `getModelUpstreamId(aliasOrId, modelId)`.
///
/// A trailing `(level)` on the input is split off so the lookup hits the base
/// id, then re-appended to the result. A `(preset)` on the *resolved* id is
/// treated the same way, with the input suffix winning when both exist.
///
/// The `suffix` is the whole regex match, trailing whitespace included, and
/// `base_id` is only trimmed when a suffix was found — both are deliberate,
/// not an oversight to tidy up.
pub fn model_upstream_id(alias_or_id: &str, model_id: &str) -> String {
    let suffix = TRAILING_PAREN
        .find(model_id)
        .map(|m| m.as_str().to_string());
    let base_id = match &suffix {
        Some(_) => strip_trailing_paren(model_id).to_string(),
        None => model_id.to_string(),
    };
    let suffix = suffix.unwrap_or_default();

    let found = find_model(registry().models_for(alias_or_id), &base_id, alias_or_id);
    if let Some(resolved) = found.map(|m| m.upstream_id().to_string()) {
        let preset = TRAILING_PAREN
            .find(&resolved)
            .map(|m| m.as_str().to_string());
        let resolved_base = match &preset {
            Some(_) => strip_trailing_paren(&resolved).to_string(),
            None => resolved,
        };
        let tail = if suffix.is_empty() {
            preset.unwrap_or_default()
        } else {
            suffix
        };
        return format!("{resolved_base}{tail}");
    }

    // Codex review variants carry no upstreamModelId; strip the suffix.
    if alias_or_id == "cx" && base_id.ends_with(CODEX_REVIEW_SUFFIX) {
        return format!(
            "{}{suffix}",
            &base_id[..base_id.len() - CODEX_REVIEW_SUFFIX.len()]
        );
    }
    format!("{base_id}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_model_splits_on_the_first_slash_only() {
        let p = parse_model(Some("deepseek/deepseek-v4-pro"));
        assert_eq!(p.provider.as_deref(), Some("deepseek"));
        assert_eq!(p.model.as_deref(), Some("deepseek-v4-pro"));
        assert!(!p.is_alias);
        // The provider half goes through alias resolution; `kc` is Kilocode.
        let p = parse_model(Some("kc/anthropic/claude-sonnet-4-20250514"));
        assert_eq!(p.provider.as_deref(), Some("kilocode"));
        assert_eq!(
            p.model.as_deref(),
            Some("anthropic/claude-sonnet-4-20250514")
        );
    }

    #[test]
    fn parse_model_without_a_slash_is_a_model_alias() {
        let p = parse_model(Some("grok-build"));
        assert_eq!(p.provider, None);
        assert_eq!(p.model.as_deref(), Some("grok-build"));
        assert!(p.is_alias);
        assert_eq!(parse_model(None).model, None);
        assert_eq!(parse_model(Some("")).model, None);
    }

    #[test]
    fn alias_resolution_resolves_the_provider_half() {
        let p = parse_model(Some("ds/deepseek-v4-pro"));
        assert_eq!(
            p.provider.as_deref(),
            Some("deepseek"),
            "ds is deepseek's alias"
        );
        assert_eq!(p.provider_alias.as_deref(), Some("ds"));
    }

    #[test]
    fn prefix_inference_matches_the_reference_table() {
        assert_eq!(
            infer_provider_from_model_name(Some("claude-sonnet-4.5")),
            "anthropic"
        );
        assert_eq!(infer_provider_from_model_name(Some("gpt-5")), "openai");
        assert_eq!(infer_provider_from_model_name(Some("o3-mini")), "openai");
        assert_eq!(
            infer_provider_from_model_name(Some("codex-auto-review")),
            "codex"
        );
        assert_eq!(
            infer_provider_from_model_name(Some("deepseek-chat")),
            "openrouter"
        );
        assert_eq!(
            infer_provider_from_model_name(Some("unknown-thing")),
            "openai"
        );
        assert_eq!(infer_provider_from_model_name(None), "openai");
    }

    #[test]
    fn normalize_model_id_only_touches_digit_hyphen_digit() {
        assert_eq!(normalize_model_id("claude-sonnet-4-5"), "claude-sonnet-4.5");
        assert_eq!(normalize_model_id("qwen3-coder-next"), "qwen3-coder-next");
        assert_eq!(normalize_model_id("glm-4.6"), "glm-4.6");
    }

    #[test]
    fn upstream_id_follows_the_declared_override() {
        assert_eq!(
            model_upstream_id("deepseek", "deepseek-v4-pro-max"),
            "deepseek-v4-pro"
        );
        assert_eq!(
            model_upstream_id("deepseek", "deepseek-v4-pro"),
            "deepseek-v4-pro"
        );
    }

    #[test]
    fn upstream_id_preserves_the_thinking_suffix() {
        assert_eq!(
            model_upstream_id("deepseek", "deepseek-v4-pro-max(high)"),
            "deepseek-v4-pro(high)"
        );
    }

    #[test]
    fn upstream_id_falls_back_to_the_base_id() {
        assert_eq!(
            model_upstream_id("deepseek", "unknown-model"),
            "unknown-model"
        );
    }

    #[test]
    fn muse_spark_detection() {
        assert!(is_muse_spark_model("muse-spark"));
        assert!(is_muse_spark_model("musespark-1"));
        assert!(is_muse_spark_model("vendor/muse-spark:free"));
        assert!(is_muse_spark_model("muse-spark (high)"));
        assert!(!is_muse_spark_model("muse-sparkling"));
        assert!(!is_muse_spark_model("gpt-5"));
    }

    #[test]
    fn muse_spark_models_route_to_openai_responses() {
        assert_eq!(
            model_target_format("oc", "muse-spark"),
            Some("openai-responses".into())
        );
        assert_eq!(model_target_format("deepseek", "muse-spark"), None);
    }

    #[test]
    fn builtin_alias_map_resolves() {
        // `gcli` is grok-cli's alias, so the provider half resolves through it.
        assert_eq!(
            resolve_model_alias_from_map("grok-build", Some(builtin_model_aliases())),
            Some(("grok-cli".to_string(), "grok-build".to_string()))
        );
    }

    #[test]
    fn valid_model_requires_a_known_provider_unless_passthrough() {
        assert!(is_valid_model("deepseek", "deepseek-v4-pro", false));
        assert!(!is_valid_model("deepseek", "nope", false));
        assert!(!is_valid_model("no-such-provider", "nope", false));
        assert!(is_valid_model("no-such-provider", "nope", true));
    }

    #[test]
    fn strip_and_quota_family_have_defaults() {
        assert_eq!(model_quota_family("deepseek", "deepseek-v4-pro"), "normal");
        assert!(model_strip("deepseek", "deepseek-v4-pro").is_empty());
    }

    #[test]
    fn review_variant_suffix_is_stripped_for_cx() {
        // Codex review variants carry no upstreamModelId, so the suffix is
        // stripped and the thinking suffix preserved.
        assert_eq!(model_upstream_id("cx", "gpt-5.5-review"), "gpt-5.5");
        assert_eq!(
            model_upstream_id("cx", "gpt-5.5-review(high)"),
            "gpt-5.5(high)"
        );
    }

    #[test]
    fn resolve_model_alias_accepts_the_object_form() {
        let mut map = serde_json::Map::new();
        map.insert(
            "mine".into(),
            json!({"provider": "ds", "model": "deepseek-v4-pro"}),
        );
        assert_eq!(
            resolve_model_alias_from_map("mine", Some(&map)),
            Some(("deepseek".to_string(), "deepseek-v4-pro".to_string()))
        );
        assert_eq!(resolve_model_alias_from_map("missing", Some(&map)), None);
        assert_eq!(resolve_model_alias_from_map("mine", None), None);
    }
}
