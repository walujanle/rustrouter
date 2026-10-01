//! The provider registry: generated data, the shapes it parses into, the
//! per-request lookups, and the UI projection.
//!
//! `registry.json` is generated data and never hand-edited, because the derived
//! behaviours it bakes in (format default, OAuth injection, TTS tables) are easy
//! to get subtly wrong.

pub mod lookup;
pub mod model;
pub mod normalize;
pub mod registry;
pub mod service;
pub mod shared;
pub mod ui;

pub use lookup::{
    ParsedModel, is_valid_model, model_quota_family, model_strip, model_target_format,
    model_upstream_id, parse_model, resolve_model_alias_from_map,
};
pub use model::{Model, Transport};
pub use normalize::{normalize_provider_id, normalize_provider_specific_data};
pub use registry::{Provider, Registry, registry};
pub use service::{detect_format, get_target_format, resolve_transport};
pub use ui::{build_provider_entry, media_provider_kinds, provider_categories, providers_by_kind};
