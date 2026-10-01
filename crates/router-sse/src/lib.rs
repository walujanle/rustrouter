//! The routing engine: provider registry data, translators, executors, the chat
//! pipeline and the non-chat modality cores.
//!
//! Deliberately HTTP-free so it can be unit-tested in isolation. See
//! `docs/CHAT-PIPELINE.md`.

pub mod catalog;
pub mod config;
pub mod constants;
pub mod credentials;
pub mod executors;
pub mod handlers;
pub mod modalities;
pub mod providers;
pub mod rtk;
pub mod runtime_config;
pub mod services;
pub mod session_manager;
pub mod thinking;
pub mod transformer;
pub mod translator;
pub mod utils;
