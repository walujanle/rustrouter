//! `GET /api/registry` — the client-side constants contract.
//!
//! The dashboard needs the provider list, the model table, capabilities and
//! icons as a single synchronous source of truth. The Vue app cannot read the
//! Rust registry directly, so this route serves that projection once at boot;
//! the SPA installs it into a store and every downstream module reads the
//! shapes from there.
//!
//! Every value already has a Rust source — `providers::ui` for the category
//! maps and the media kinds, `registry()` for the model table and the alias
//! index — so this is a projection, not a second implementation, and there is
//! no static JSON copy that can drift.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use router_sse::providers::registry::{raw_models_for, registry};
use router_sse::providers::ui::{media_provider_kinds, provider_categories};

use crate::error::ApiError;
use crate::state::AppState;

/// `GET /api/registry`.
///
/// Body:
/// - `providers`: the five category maps (`free`, `freeTier`, `oauth`,
///   `apikey`, `webCookie`), each `id → buildProviderEntry`.
/// - `mediaProviderKinds`: `MEDIA_PROVIDER_KINDS`.
/// - `usageSupportedProviders` / `usageApikeyProviders`: `features.usage` and
///   `features.usageApikey` ids in registry order.
/// - `providerModels`: `PROVIDER_MODELS`, keyed `alias || id`.
/// - `providerIdToAlias`: `PROVIDER_ID_TO_ALIAS`.
pub async fn get(State(_state): State<AppState>) -> Result<Response, ApiError> {
    let mut providers = Map::new();
    for (category, value) in provider_categories() {
        providers.insert(category, value);
    }

    // `PROVIDER_MODELS`: keys come from `models_iter` in file order; the values
    // come from the raw JSON so each model keeps its original key order (the
    // typed `Model` cannot restore it through `#[serde(flatten)]`).
    let mut provider_models = Map::new();
    for (key, _) in registry().models_iter() {
        let models = raw_models_for(key)
            .map(|raw| Value::Array(raw.clone()))
            .unwrap_or_else(|| Value::Array(Vec::new()));
        provider_models.insert(key.to_string(), models);
    }

    // `PROVIDER_ID_TO_ALIAS` over the registry entries, so the order is the
    // registry's rather than a `HashMap`'s.
    let mut provider_id_to_alias = Map::new();
    for entry in registry().entries() {
        provider_id_to_alias.insert(entry.id.clone(), json!(registry().alias_for(&entry.id)));
    }

    let body = json!({
        "providers": providers,
        "mediaProviderKinds": media_provider_kinds(),
        "usageSupportedProviders": registry().usage_supported(),
        "usageApikeyProviders": registry().usage_apikey(),
        "providerModels": provider_models,
        "providerIdToAlias": provider_id_to_alias,
    });

    Ok(Json(body).into_response())
}
