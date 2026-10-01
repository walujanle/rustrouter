//! Service layer.
//!
//! The pipeline's decision-making that is not a translator and not an executor:
//! model resolution, combo strategies, account fallback, token refresh and the
//! capacity adapter.
//!
//! There is no history-compaction service here on purpose: the router has none,
//! and a module for it would only duplicate the first half of `combo`.

pub mod account_fallback;
pub mod auth;
pub mod background_token_refresh;
pub mod capacity_adapter;
pub mod combo;
pub mod combo_presets;
pub mod connection_proxy;
pub mod model;
pub mod model_catalog;
pub mod model_catalog_sync;
pub mod oauth_flow;
pub mod ping;
pub mod proxy_test;
pub mod single_flight;
pub mod stats_emitter;
pub mod token_refresh;
pub mod usage;
