//! The request handlers.
//!
//! The chat pipeline lives under [`chat_core`]. The non-chat modality cores
//! (embeddings, search, fetch) arrive in later phases; this module grows with
//! them.

pub mod chat;
pub mod chat_core;
pub mod responses_handler;
