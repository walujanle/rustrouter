//! Model capabilities and pricing, resolved from the committed `catalog.json`.
//!
//! `catalog.json` is generated data, so the module-scope mutation
//! (`PROVIDER_CAPABILITIES["qoder-cn"] = PROVIDER_CAPABILITIES["qoder"]`) and
//! every hand-maintained row are baked in rather than re-derived. Only the
//! resolution *logic* lives here.
//!
//! Both lookups are on the hot path of every chat request, so the tables parse
//! once into `LazyLock` and the glob patterns are compiled once into a cache —
//! `matchPattern` runs up to a hundred times per capability lookup and building
//! a `Regex` each time would be the whole cost of the request.

use std::collections::HashMap;
use std::sync::LazyLock;

use dashmap::DashMap;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The committed dump.
static CATALOG_JSON: &str = include_str!("catalog.json");

/// Parsed tables, built once per process.
static CATALOG: LazyLock<CatalogFile> = LazyLock::new(|| {
    serde_json::from_str(CATALOG_JSON).expect("catalog.json is generated and must parse")
});

/// Compiled `matchPattern` regexes, keyed by the pattern string.
static PATTERN_CACHE: LazyLock<DashMap<String, Regex>> = LazyLock::new(DashMap::new);

/// One resolved capability set. Field order matches `DEFAULT_CAPABILITIES` so the
/// JSON projection the dashboard reads stays stable.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub vision: bool,
    pub pdf: bool,
    pub audio_input: bool,
    pub video_input: bool,
    pub image_output: bool,
    pub audio_output: bool,
    pub search: bool,
    pub tools: bool,
    pub reasoning: bool,
    /// `null` means "derive from the transport format".
    pub thinking_format: Option<String>,
    pub thinking_can_disable: bool,
    pub thinking_range: Option<ThinkingRange>,
    /// zai format only: the model accepts a `reasoning_effort` level.
    pub thinking_effort_supported: bool,
    pub context_window: i64,
    pub max_output: i64,
}

/// `thinkingRange`: `{ min, max }` for budget formats.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ThinkingRange {
    pub min: Option<i64>,
    pub max: Option<i64>,
}

/// A partial capability row. Every table entry is a delta over
/// `DEFAULT_CAPABILITIES`, so absent fields must fall through, not reset.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapsPatch {
    vision: Option<bool>,
    pdf: Option<bool>,
    audio_input: Option<bool>,
    video_input: Option<bool>,
    image_output: Option<bool>,
    audio_output: Option<bool>,
    search: Option<bool>,
    tools: Option<bool>,
    reasoning: Option<bool>,
    thinking_format: Option<String>,
    thinking_can_disable: Option<bool>,
    thinking_range: Option<ThinkingRange>,
    thinking_effort_supported: Option<bool>,
    context_window: Option<i64>,
    max_output: Option<i64>,
}

impl CapsPatch {
    /// `{ ...DEFAULT_CAPABILITIES, ...patch }`.
    fn apply(&self, base: &Capabilities) -> Capabilities {
        Capabilities {
            vision: self.vision.unwrap_or(base.vision),
            pdf: self.pdf.unwrap_or(base.pdf),
            audio_input: self.audio_input.unwrap_or(base.audio_input),
            video_input: self.video_input.unwrap_or(base.video_input),
            image_output: self.image_output.unwrap_or(base.image_output),
            audio_output: self.audio_output.unwrap_or(base.audio_output),
            search: self.search.unwrap_or(base.search),
            tools: self.tools.unwrap_or(base.tools),
            reasoning: self.reasoning.unwrap_or(base.reasoning),
            thinking_format: self
                .thinking_format
                .clone()
                .or_else(|| base.thinking_format.clone()),
            thinking_can_disable: self
                .thinking_can_disable
                .unwrap_or(base.thinking_can_disable),
            thinking_range: self
                .thinking_range
                .clone()
                .or_else(|| base.thinking_range.clone()),
            thinking_effort_supported: self
                .thinking_effort_supported
                .unwrap_or(base.thinking_effort_supported),
            context_window: self.context_window.unwrap_or(base.context_window),
            max_output: self.max_output.unwrap_or(base.max_output),
        }
    }
}

/// Per-1M-token rates. `input`/`output` are always present; the rest are
/// provider-specific and the cost math tests for them.
///
/// `Serialize` is for `/api/pricing`'s defaults projection only: absent fields
/// must vanish, not become `null`, so `skip_serializing_if` is right here even
/// though it is wrong for the DB rows.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Pricing {
    pub input: f64,
    pub output: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<f64>,
}

/// `{ pattern, caps }` from `PATTERN_CAPABILITIES`.
#[derive(Debug, Deserialize)]
struct PatternCap {
    pattern: String,
    caps: CapsPatch,
}

/// `{ pattern, pricing }` from `PATTERN_PRICING`.
#[derive(Debug, Deserialize)]
struct PatternPrice {
    pattern: String,
    pricing: Pricing,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    default_capabilities: Capabilities,
    model_capabilities: HashMap<String, CapsPatch>,
    provider_capabilities: HashMap<String, HashMap<String, CapsPatch>>,
    pattern_capabilities: Vec<PatternCap>,
    model_pricing: HashMap<String, Pricing>,
    provider_pricing: HashMap<String, HashMap<String, Pricing>>,
    pattern_pricing: Vec<PatternPrice>,
}

/// `matchPattern(pattern, model)`: glob where `*` is the only wildcard, anchored
/// and case-insensitive.
///
/// The matcher builds `^` + each literal segment escaped + `.*` between them +
/// `$`. `regex::escape` is stricter than the class escape this needs but escapes
/// the same characters for these patterns, which contain no regex
/// metacharacters of their own.
pub fn match_pattern(pattern: &str, model: &str) -> bool {
    if let Some(re) = PATTERN_CACHE.get(pattern) {
        return re.is_match(model);
    }
    let mut src = String::with_capacity(pattern.len() + 8);
    src.push_str("(?i)^");
    for (i, segment) in pattern.split('*').enumerate() {
        if i > 0 {
            src.push_str(".*");
        }
        src.push_str(&regex::escape(segment));
    }
    src.push('$');
    // A malformed pattern cannot come from the generated table; treat it as a
    // non-match rather than panicking mid-request.
    let Ok(re) = Regex::new(&src) else {
        return false;
    };
    let matched = re.is_match(model);
    PATTERN_CACHE.insert(pattern.to_string(), re);
    matched
}

/// `looksLikeVisionModel(modelId)`: name signal only, never turns vision off.
pub fn looks_like_vision_model(model_id: &str) -> bool {
    if model_id.is_empty() {
        return false;
    }
    let id = model_id.to_lowercase();
    if NOT_VISION.is_match(&id) {
        return false;
    }
    VISION_NAME.is_match(&id)
}

/// `NOT_VISION` — image/video generation and non-chat models carry the same
/// words but take no image input.
static NOT_VISION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|[-_/:.])(image|img)([-_/:.]|$)|stable-image|gen[0-9]_image|nanobanana|imagine|t2v|i2v|flux|dall|sdxl|diffusion|embed|rerank|guard|moderation|tts|stt|whisper|voice|speech|audio",
    )
    .expect("static pattern")
});

/// `VISION_NAME`.
static VISION_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|[-_/:.])(vision|vl|vlm|multimodal|omni|visual)([-_/:.]|$)|[0-9]\.[0-9]+v([-_/:.]|$)|(^|[-_/:.])(llava|pixtral|internvl|cogvlm|minicpm-v|moondream|idefics|fuyu)",
    )
    .expect("static pattern")
});

/// `COMMANDCODE_TEXT_ONLY`.
const COMMANDCODE_TEXT_ONLY: [&str; 23] = [
    "deepseek/deepseek-v4-pro",
    "deepseek/deepseek-v4-flash",
    "deepseek/deepseek-v4-flash-fast",
    "zai-org/glm-5.3",
    "zai-org/glm-5.2",
    "zai-org/glm-5.2-fast",
    "zai-org/glm-5.1",
    "zai-org/glm-5",
    "minimaxai/minimax-m2.7",
    "minimax/minimax-m2.7-free",
    "minimaxai/minimax-m2.5",
    "xiaomi/mimo-v2.5-pro",
    "qwen/qwen3.6-max-preview",
    "qwen/qwen3.7-max",
    "meituan/longcat-2.0:free",
    "stepfun/step-3.5-flash",
    "tencent/hy4-preview",
    "tencent/hy3",
    "tencent/hy3-paid",
    "nvidia/nemotron-3-ultra-550b-a55b",
    "poolside/laguna-s-2.1-free",
    "inclusionai/ling-3.0-flash-free",
    "inclusionai/ling-3.0-flash-sante:free",
];

/// `isCommandCodeTextOnly(model)`.
fn is_commandcode_text_only(model: &str) -> bool {
    let key = model.to_lowercase();
    if COMMANDCODE_TEXT_ONLY.contains(&key.as_str()) {
        return true;
    }
    COMMANDCODE_TEXT_ONLY.iter().any(|id| {
        let base = id.rsplit('/').next().unwrap_or(id);
        key == base || key.ends_with(&format!("/{base}"))
    })
}

/// `getCapabilitiesForModel(provider, model)`: the 4-step fallback chain, each
/// result merged over `DEFAULT_CAPABILITIES`.
pub fn get_capabilities_for_model(provider: Option<&str>, model: &str) -> Capabilities {
    let default = &CATALOG.default_capabilities;
    if model.is_empty() {
        return default.clone();
    }

    let base_model = model.rsplit('/').next().unwrap_or(model);

    // CommandCode's wire is /alpha/generate for every model, so family patterns
    // (deepseek-v4 -> thinkingFormat:deepseek, vision:false) must not win here.
    if provider == Some("commandcode") || provider == Some("cmc") {
        if let Some(provider_caps) = CATALOG.provider_capabilities.get("commandcode") {
            if let Some(p) = provider_caps.get(model) {
                return p.apply(default);
            }
            if let Some(p) = provider_caps.get(base_model) {
                return p.apply(default);
            }
        }
        let mut caps = default.clone();
        caps.reasoning = true;
        caps.thinking_format = Some("commandcode".to_string());
        caps.thinking_effort_supported = true;
        caps.vision = !is_commandcode_text_only(model);
        caps.context_window = 1_000_000;
        caps.max_output = 384_000;
        return caps;
    }

    // 1. Provider-specific override
    if let Some(provider) = provider
        && let Some(provider_caps) = CATALOG.provider_capabilities.get(provider)
    {
        if let Some(p) = provider_caps.get(model) {
            return p.apply(default);
        }
        if let Some(p) = provider_caps.get(base_model) {
            return p.apply(default);
        }
    }

    // 2. Canonical exact id
    if let Some(p) = CATALOG.model_capabilities.get(base_model) {
        return p.apply(default);
    }
    if let Some(p) = CATALOG.model_capabilities.get(model) {
        return p.apply(default);
    }

    // 3. Pattern match, first hit wins
    for entry in &CATALOG.pattern_capabilities {
        if match_pattern(&entry.pattern, base_model) || match_pattern(&entry.pattern, model) {
            return refine(Some(&entry.caps), provider, model);
        }
    }

    // 4. Floor
    refine(None, provider, model)
}

/// The synced models.dev catalog reader, installed by the sync service
/// (`services::model_catalog_sync`). `None` until a sync or restore installs it.
///
/// Function pointers, not closures: the reader is stateless (it reads the file
/// each call) and `fn` is `Send + Sync` for free.
pub struct CatalogSource {
    pub get_modalities: fn(&str, &str) -> Option<Value>,
    pub get_limits: fn(&str, &str) -> Option<Value>,
}

static CATALOG_SOURCE: LazyLock<std::sync::RwLock<Option<CatalogSource>>> =
    LazyLock::new(|| std::sync::RwLock::new(None));

/// `setCatalogSource(source)`. `None` detaches the reader — the sync calls this
/// before snapshotting the hand tables so its deltas are relative to those.
pub fn set_catalog_source(source: Option<CatalogSource>) {
    if let Ok(mut slot) = CATALOG_SOURCE.write() {
        *slot = source;
    }
}

/// `invalidateCatalog()`. The file reader is stateless here, so this is a no-op
/// kept for the sync's call site.
pub fn invalidate_catalog() {}

/// `MODALITY_KEYS`: the keys the synced catalog may positively turn on.
const MODALITY_KEYS: [&str; 4] = ["vision", "pdf", "audioInput", "videoInput"];

/// `refine(base, provider, model)`: the synced catalog then the name heuristic.
/// Strictly additive — a capability already true stays true, and a false one
/// only flips when the outside source positively declares support.
fn refine(base: Option<&CapsPatch>, provider: Option<&str>, model: &str) -> Capabilities {
    let mut result = match base {
        Some(p) => p.apply(&CATALOG.default_capabilities),
        None => CATALOG.default_capabilities.clone(),
    };

    let source = CATALOG_SOURCE
        .read()
        .ok()
        .and_then(|s| s.as_ref().map(|s| (s.get_modalities, s.get_limits)));
    if let Some((get_modalities, get_limits)) = source
        && let Some(provider) = provider
    {
        if let Some(modalities) = get_modalities(provider, model)
            && let Some(map) = modalities.as_object()
        {
            for key in MODALITY_KEYS {
                if map.get(key) == Some(&Value::Bool(true)) {
                    match key {
                        "vision" => result.vision = true,
                        "pdf" => result.pdf = true,
                        "audioInput" => result.audio_input = true,
                        "videoInput" => result.video_input = true,
                        _ => {}
                    }
                }
            }
        }
        if let Some(limits) = get_limits(provider, model) {
            if let Some(c) = limits.get("contextWindow").and_then(Value::as_i64)
                && c > 0
            {
                result.context_window = c;
            }
            if let Some(o) = limits.get("maxOutput").and_then(Value::as_i64)
                && o > 0
            {
                result.max_output = o;
            }
        }
    }

    if !result.vision && looks_like_vision_model(model) {
        result.vision = true;
    }
    result
}

/// `SERVICE_KIND_CAPABILITIES`: the dashboard's typed model kinds mapped onto
/// runtime input/output capabilities. `null` for a kind with no override (e.g.
/// `llm`).
pub fn capabilities_from_service_kind(kind: &str) -> Option<Value> {
    Some(match kind {
        "imageToText" => serde_json::json!({ "vision": true }),
        "image" => serde_json::json!({ "imageOutput": true }),
        "stt" => serde_json::json!({ "audioInput": true }),
        "tts" => serde_json::json!({ "audioOutput": true }),
        "embedding" => serde_json::json!({ "tools": false }),
        _ => return None,
    })
}

/// `aggregateComboCapabilities(comboModels, comboLookup, _depth)`: OR the input
/// capabilities, `every` for `tools`, `first` for the thinking fields, `min`
/// window and `max` output.
///
/// A fresh object is built here rather than merging full capability sets, so
/// `thinkingEffortSupported` is deliberately absent from the result — that shape
/// is the point of this function. A bare name that exists in `combo_lookup` is a
/// nested combo and recurses, with a depth guard.
pub fn aggregate_combo_capabilities(
    combo_models: &[String],
    combo_lookup: impl Fn(&str) -> Option<Vec<String>>,
) -> Option<Value> {
    aggregate_combo_capabilities_at(combo_models, &combo_lookup, 0)
}

fn aggregate_combo_capabilities_at(
    combo_models: &[String],
    combo_lookup: &impl Fn(&str) -> Option<Vec<String>>,
    depth: usize,
) -> Option<Value> {
    if combo_models.is_empty() || depth > 6 {
        return None;
    }

    let mut caps: Vec<Capabilities> = Vec::with_capacity(combo_models.len());
    for full_id in combo_models {
        if !full_id.contains('/')
            && let Some(nested) = combo_lookup(full_id)
        {
            // A nested combo that aggregates to nothing falls back to its own
            // name as a model id.
            let resolved = aggregate_combo_capabilities_at(&nested, combo_lookup, depth + 1)
                .map(capabilities_from_value)
                .unwrap_or_else(|| get_capabilities_for_model(None, full_id));
            caps.push(resolved);
            continue;
        }
        let (provider, model) = match full_id.split_once('/') {
            Some((p, m)) => (Some(p), m),
            None => (None, full_id.as_str()),
        };
        caps.push(get_capabilities_for_model(provider, model));
    }

    let first = caps.first()?;
    Some(serde_json::json!({
        "vision": caps.iter().any(|c| c.vision),
        "pdf": caps.iter().any(|c| c.pdf),
        "audioInput": caps.iter().any(|c| c.audio_input),
        "videoInput": caps.iter().any(|c| c.video_input),
        "imageOutput": caps.iter().any(|c| c.image_output),
        "audioOutput": caps.iter().any(|c| c.audio_output),
        "search": caps.iter().any(|c| c.search),
        "tools": caps.iter().all(|c| c.tools),
        "reasoning": first.reasoning,
        "thinkingFormat": first.thinking_format,
        "thinkingCanDisable": first.thinking_can_disable,
        "thinkingRange": first.thinking_range,
        "contextWindow": caps.iter().map(|c| c.context_window).min(),
        "maxOutput": caps.iter().map(|c| c.max_output).max(),
    }))
}

/// A capability object (as produced by [`aggregate_combo_capabilities`]) back
/// into a typed set, for the nested-combo recursion.
fn capabilities_from_value(value: Value) -> Capabilities {
    let mut base = CATALOG.default_capabilities.clone();
    let get = |key: &str| value.get(key);
    if let Some(v) = get("vision").and_then(Value::as_bool) {
        base.vision = v;
    }
    if let Some(v) = get("pdf").and_then(Value::as_bool) {
        base.pdf = v;
    }
    if let Some(v) = get("audioInput").and_then(Value::as_bool) {
        base.audio_input = v;
    }
    if let Some(v) = get("videoInput").and_then(Value::as_bool) {
        base.video_input = v;
    }
    if let Some(v) = get("imageOutput").and_then(Value::as_bool) {
        base.image_output = v;
    }
    if let Some(v) = get("audioOutput").and_then(Value::as_bool) {
        base.audio_output = v;
    }
    if let Some(v) = get("search").and_then(Value::as_bool) {
        base.search = v;
    }
    if let Some(v) = get("tools").and_then(Value::as_bool) {
        base.tools = v;
    }
    if let Some(v) = get("reasoning").and_then(Value::as_bool) {
        base.reasoning = v;
    }
    base.thinking_format = get("thinkingFormat")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(v) = get("thinkingCanDisable").and_then(Value::as_bool) {
        base.thinking_can_disable = v;
    }
    if let Some(v) = get("contextWindow").and_then(Value::as_i64) {
        base.context_window = v;
    }
    if let Some(v) = get("maxOutput").and_then(Value::as_i64) {
        base.max_output = v;
    }
    base
}

/// `getPricingForModel(provider, model)`: provider override, canonical id, then
/// the pattern table.
pub fn get_pricing_for_model(provider: Option<&str>, model: &str) -> Option<&'static Pricing> {
    if model.is_empty() {
        return None;
    }

    if let Some(provider) = provider
        && let Some(p) = CATALOG
            .provider_pricing
            .get(provider)
            .and_then(|m| m.get(model))
    {
        return Some(p);
    }

    let base_model = model.rsplit('/').next().unwrap_or(model);
    if let Some(p) = CATALOG.model_pricing.get(base_model) {
        return Some(p);
    }
    if let Some(p) = CATALOG.model_pricing.get(model) {
        return Some(p);
    }

    CATALOG
        .pattern_pricing
        .iter()
        .find(|entry| {
            match_pattern(&entry.pattern, base_model) || match_pattern(&entry.pattern, model)
        })
        .map(|entry| &entry.pricing)
}

/// `getDefaultPricing()`: `PROVIDER_PRICING` as the dashboard reads it — a map
/// of provider id to model id to rate table. `provider_pricing` is a private
/// field, so this is the only way the `/api/pricing` GET can reach the built-in
/// table it merges user overrides onto.
pub fn default_pricing() -> Value {
    let mut out = Map::new();
    for (provider, models) in &CATALOG.provider_pricing {
        let mut entry = Map::new();
        for (model, pricing) in models {
            entry.insert(
                model.clone(),
                serde_json::to_value(pricing).unwrap_or(Value::Null),
            );
        }
        out.insert(provider.clone(), Value::Object(entry));
    }
    Value::Object(out)
}

/// `getThinkingLevels`'s capability half: reasoning is off for a model whose
/// capabilities say so.
pub fn supports_reasoning(provider: Option<&str>, model: &str) -> bool {
    get_capabilities_for_model(provider, model).reasoning
}

/// `FORMAT_LEVELS[thinkingFormat] || L.base`.
fn format_levels(thinking_format: Option<&str>) -> &'static [&'static str] {
    const BASE: &[&str] = &["none", "low", "medium", "high"];
    const ON_OFF: &[&str] = &["none", "thinking"];
    const OPENAI: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
    const LEVEL_MAX: &[&str] = &["none", "low", "medium", "high", "max"];
    const BUDGET_X: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];
    const GEMINI: &[&str] = &["minimal", "low", "medium", "high"];
    const HI_MAX: &[&str] = &["none", "high", "max"];
    const COMMANDCODE: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];

    match thinking_format {
        Some("openai") => OPENAI,
        Some("claude-adaptive") => LEVEL_MAX,
        Some("claude-budget") => BUDGET_X,
        Some("gemini-level") => GEMINI,
        Some("gemini-budget") => BASE,
        Some("zai") => ON_OFF,
        Some("qwen") => BASE,
        Some("kimi") => LEVEL_MAX,
        Some("deepseek") => HI_MAX,
        Some("commandcode") => COMMANDCODE,
        Some("minimax") => ON_OFF,
        Some("hunyuan") => BASE,
        Some("step") => BASE,
        _ => BASE,
    }
}

/// `CODEX_GPT_5_6_LEVELS`.
const CODEX_GPT_5_6_LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// One `PATTERN_THINKING` row: provider, model glob, and the levels it yields.
type ThinkingPattern = (Option<&'static str>, &'static str, &'static [&'static str]);

/// `PATTERN_THINKING`: per-model level overrides, first match wins. The `levels`
/// slice is static, so a `&[&str]` (with the `ultra` variants pre-built) works.
static PATTERN_THINKING: LazyLock<Vec<ThinkingPattern>> = LazyLock::new(|| {
    vec![
        (Some("codex"), "*gpt-6*", CODEX_GPT_5_6_LEVELS),
        (
            Some("codex"),
            "*gpt-5.6-sol*",
            &[
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ],
        ),
        (
            Some("codex"),
            "*gpt-5.6-terra*",
            &[
                "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ],
        ),
        (Some("codex"), "*gpt-5.6-luna*", CODEX_GPT_5_6_LEVELS),
        // Codex cannot disable thinking.
        (None, "*codex*", &["low", "medium", "high", "xhigh"]),
        (
            None,
            "*mimo*v2.6*",
            &["none", "low", "medium", "high", "xhigh"],
        ),
        // DeepSeek v4.*: "none" on the anthropic route is a 400 (disable instead).
        (
            None,
            "*deepseek-v4.*",
            &["none", "low", "medium", "high", "xhigh", "max"],
        ),
        (
            Some("codebuddy-intl"),
            "deepseek-v4*",
            &["low", "high", "xhigh"],
        ),
    ]
});

/// `getThinkingLevels(provider, model)`: the valid level set for the UI picker,
/// or `None` when the model cannot reason. A model that cannot disable thinking
/// drops `"none"` from its set.
pub fn get_thinking_levels(provider: Option<&str>, model: &str) -> Option<Vec<&'static str>> {
    let caps = get_capabilities_for_model(provider, model);
    if !caps.reasoning {
        return None;
    }
    let hit = PATTERN_THINKING.iter().find(|(p, pattern, _)| {
        p.is_none_or(|p| Some(p) == provider) && match_pattern(pattern, model)
    });
    let mut levels = hit
        .map(|(_, _, levels)| levels.to_vec())
        .unwrap_or_else(|| format_levels(caps.thinking_format.as_deref()).to_vec());
    if !caps.thinking_can_disable {
        levels.retain(|l| *l != "none");
    }
    Some(levels)
}

/// `calculateCostFromTokens(tokens, pricing)`: `prompt_tokens` is
/// cache-inclusive, so cached and cache-creation counts are subtracted before
/// charging the input rate.
pub fn calculate_cost_from_tokens(tokens: &serde_json::Value, pricing: &Pricing) -> f64 {
    let num = |v: &serde_json::Value, key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0)
    };
    let input_tokens = {
        let v = num(tokens, "prompt_tokens");
        if v != 0.0 {
            v
        } else {
            num(tokens, "input_tokens")
        }
    };
    let cached_tokens = {
        let v = num(tokens, "cached_tokens");
        if v != 0.0 {
            v
        } else {
            num(tokens, "cache_read_input_tokens")
        }
    };
    let cache_creation_tokens = num(tokens, "cache_creation_input_tokens");
    let non_cached_input = (input_tokens - cached_tokens - cache_creation_tokens).max(0.0);

    let mut cost = non_cached_input * (pricing.input / 1_000_000.0);
    if cached_tokens > 0.0 {
        cost += cached_tokens * (pricing.cached.unwrap_or(pricing.input) / 1_000_000.0);
    }
    let output_tokens = {
        let v = num(tokens, "completion_tokens");
        if v != 0.0 {
            v
        } else {
            num(tokens, "output_tokens")
        }
    };
    cost += output_tokens * (pricing.output / 1_000_000.0);
    let reasoning_tokens = num(tokens, "reasoning_tokens");
    if reasoning_tokens > 0.0 {
        cost += reasoning_tokens * (pricing.reasoning.unwrap_or(pricing.output) / 1_000_000.0);
    }
    if cache_creation_tokens > 0.0 {
        cost +=
            cache_creation_tokens * (pricing.cache_creation.unwrap_or(pricing.input) / 1_000_000.0);
    }
    cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn glob_matching_is_anchored_and_case_insensitive() {
        assert!(match_pattern("*claude*opus*", "claude-opus-4.8"));
        assert!(match_pattern("*gpt-5*", "GPT-5.1-codex"));
        assert!(match_pattern("hy3*", "hy3-preview"));
        // Anchored: a prefix-only match must fail.
        assert!(!match_pattern("gpt-5*", "my-gpt-5"));
        assert!(!match_pattern("*codex", "codex-high"));
        // `.` in a pattern is a literal, not "any char".
        assert!(!match_pattern("claude-opus-4.8", "claude-opus-4x8"));
    }

    #[test]
    fn vision_name_heuristic_matches_the_expected() {
        assert!(looks_like_vision_model("qwen3-vl-plus"));
        assert!(looks_like_vision_model("glm-4.6v"));
        assert!(looks_like_vision_model("deepseek-v4-flash-vision-exp"));
        // Generation and non-chat models are excluded first.
        assert!(!looks_like_vision_model("gpt-image-1"));
        assert!(!looks_like_vision_model("flux-1.1-pro"));
        assert!(!looks_like_vision_model("whisper-large-v3"));
        assert!(!looks_like_vision_model("text-embedding-3-large"));
        // The digit-v branch needs a dotted version.
        assert!(!looks_like_vision_model("gpt-4v"));
        assert!(!looks_like_vision_model("gpt-5"));
    }

    #[test]
    fn capabilities_resolve_through_each_fallback_step() {
        // 1. Provider override beats the global exact entry.
        let caps = get_capabilities_for_model(Some("opencode-go"), "glm-5.3-flash");
        assert_eq!(caps.thinking_format.as_deref(), Some("openai"));
        assert!(!caps.thinking_can_disable);

        // 2. Canonical exact entry.
        let caps = get_capabilities_for_model(None, "claude-opus-4.8");
        assert_eq!(caps.thinking_format.as_deref(), Some("claude-adaptive"));
        assert_eq!(caps.context_window, 1_000_000);

        // 3. Pattern, most specific first: haiku is budget, not adaptive.
        let caps = get_capabilities_for_model(None, "claude-haiku-4-5");
        assert_eq!(caps.thinking_format.as_deref(), Some("claude-budget"));
        assert!(caps.vision);

        // 4. Floor for an unknown id.
        let caps = get_capabilities_for_model(None, "totally-unknown-model");
        assert!(!caps.reasoning);
        assert!(caps.tools);
        assert_eq!(caps.context_window, 200_000);
    }

    #[test]
    fn vendor_prefix_is_stripped_before_the_exact_lookup() {
        let caps = get_capabilities_for_model(None, "anthropic/claude-opus-4.7");
        assert_eq!(caps.thinking_format.as_deref(), Some("claude-adaptive"));
    }

    #[test]
    fn commandcode_ignores_the_family_patterns() {
        // A deepseek-v4 id would normally resolve to thinkingFormat "deepseek";
        // on the commandcode wire it must stay "commandcode".
        let caps = get_capabilities_for_model(Some("commandcode"), "deepseek/deepseek-v4-pro");
        assert_eq!(caps.thinking_format.as_deref(), Some("commandcode"));
        assert!(!caps.vision, "deepseek-v4-pro is on the text-only denylist");

        let caps = get_capabilities_for_model(Some("commandcode"), "zai-org/glm-5.2");
        assert!(!caps.vision);

        // An id off the denylist defaults to vision on this wire.
        let caps = get_capabilities_for_model(Some("commandcode"), "some/new-model");
        assert!(caps.vision);
        assert_eq!(caps.context_window, 1_000_000);
        assert_eq!(caps.max_output, 384_000);
    }

    #[test]
    fn pricing_resolves_through_the_chain() {
        // Provider override (tokenrouter reseller rate).
        let p = get_pricing_for_model(Some("tokenrouter"), "anthropic/claude-opus-4.8").unwrap();
        assert_eq!(p.input, 5.0);
        assert_eq!(p.output, 25.0);

        // Canonical exact.
        let p = get_pricing_for_model(None, "gpt-4o").unwrap();
        assert_eq!(p.input, 2.50);
        assert_eq!(p.output, 10.00);

        // Pattern, specific before generic.
        let p = get_pricing_for_model(None, "claude-sonnet-9.9").unwrap();
        assert_eq!(p.input, 3.00);
        let p = get_pricing_for_model(None, "gemini-3.7-flash").unwrap();
        assert_eq!(p.input, 1.5);

        assert!(get_pricing_for_model(None, "").is_none());
    }

    #[test]
    fn cost_math_subtracts_cached_tokens_from_the_input_rate() {
        let pricing = get_pricing_for_model(None, "gpt-4o").unwrap();
        // 1M input of which 400k cached, plus 1M output.
        let tokens = json!({
            "prompt_tokens": 1_000_000,
            "cached_tokens": 400_000,
            "completion_tokens": 1_000_000,
        });
        // non-cached 600k * 2.50 + 400k * 1.25 + 1M * 10.00
        let expected = 1.5 + 0.5 + 10.0;
        assert!((calculate_cost_from_tokens(&tokens, pricing) - expected).abs() < 1e-9);
    }

    #[test]
    fn cache_creation_tokens_are_charged_at_their_own_rate() {
        // `claude-opus-4-6` prices cache-creation above plain input (6.25 vs 5).
        let pricing = get_pricing_for_model(None, "claude-opus-4-6").unwrap();
        let cache_creation = pricing.cache_creation.unwrap();
        assert_ne!(cache_creation, pricing.input);
        // 1M input of which 250k cache-creation, plus 1M output.
        let tokens = json!({
            "prompt_tokens": 1_000_000,
            "cache_creation_input_tokens": 250_000,
            "completion_tokens": 1_000_000,
        });
        let expected = 750_000.0 / 1_000_000.0 * pricing.input
            + 250_000.0 / 1_000_000.0 * cache_creation
            + pricing.output;
        let actual = calculate_cost_from_tokens(&tokens, pricing);
        assert!((actual - expected).abs() < 1e-9);
        // Without the term the 250k would be charged at the input rate, so the
        // two costs differ — that gap is the fix.
        let without_term = 1_000_000.0 / 1_000_000.0 * pricing.input + pricing.output;
        assert!((actual - without_term).abs() > 1e-9);
    }

    #[test]
    fn catalog_parses_with_the_expected_shape() {
        assert_eq!(CATALOG.model_capabilities.len(), 38);
        assert_eq!(CATALOG.pattern_capabilities.len(), 100);
        // `qoder-cn` left with the provider; the dump filters the
        // provider-keyed tables against the kept id set.
        assert!(!CATALOG.provider_capabilities.contains_key("qoder-cn"));
        assert!(!CATALOG.provider_capabilities.contains_key("commandcode"));
    }

    #[test]
    fn thinking_levels_follow_format_then_pattern_overrides() {
        // Format default: claude-budget exposes the xhigh/max set.
        let levels = get_thinking_levels(None, "claude-haiku-4-5").unwrap();
        assert_eq!(levels, ["none", "low", "medium", "high", "xhigh", "max"]);

        // `claude-opus-4.8` declares `claude-adaptive` and leaves
        // `thinkingCanDisable` at its default, so "none" survives.
        let levels = get_thinking_levels(None, "claude-opus-4.8").unwrap();
        assert!(levels.contains(&"none"));

        // A model that declares `thinkingCanDisable: false` drops "none".
        let levels = get_thinking_levels(None, "claude-fable-5-1").unwrap();
        assert!(!levels.contains(&"none"));

        // Pattern override beats the format default (codex cannot disable).
        let levels = get_thinking_levels(Some("codex"), "gpt-5.1-codex").unwrap();
        assert_eq!(levels, ["low", "medium", "high", "xhigh"]);

        // A per-model pattern carries "ultra".
        let levels = get_thinking_levels(Some("codex"), "gpt-5.6-sol").unwrap();
        assert!(levels.contains(&"ultra"));

        // A non-reasoning model has no levels at all.
        assert!(get_thinking_levels(None, "llama-3.1-8b").is_none());
    }
}
