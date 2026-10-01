//! Static configuration blobs the pipeline reads.
//!
//! Only the blobs that still have a consumer are here. The fallback decision
//! table lives in `services::account_fallback`; runtime constants are the
//! crate-level `runtime_config`; the default thinking signatures are in
//! `constants.rs`.

pub mod codex_instructions;
