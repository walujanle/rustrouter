//! Model capabilities and pricing, resolved from the committed `catalog.json`.
//!
//! `catalog.json` is generated data and never hand-edited, because the
//! runtime mutation
//! (`PROVIDER_CAPABILITIES["qoder-cn"] = PROVIDER_CAPABILITIES["qoder"]`) and
//! the hand-maintained rows are both easy to get subtly wrong.

#[allow(clippy::module_inception)] // `catalog::catalog` matches the generated file name
pub mod catalog;

pub use catalog::{
    Capabilities, CatalogSource, Pricing, aggregate_combo_capabilities, calculate_cost_from_tokens,
    capabilities_from_service_kind, default_pricing, get_capabilities_for_model,
    get_pricing_for_model, get_thinking_levels, invalidate_catalog, looks_like_vision_model,
    match_pattern, set_catalog_source, supports_reasoning,
};
