//! Shared translator logic.
//!
//! Everything here is format-agnostic. A concern either maps between two
//! representations (`finish_reason`, `usage`) or mutates a body in place
//! (`modality`, `param_support`, `tool_call`, `prefetch`). None of them know
//! which direction the translation is going; the translators pass that in.

pub mod finish_reason;
pub mod image;
pub mod modality;
pub mod param_support;
pub mod prefetch;
pub mod primitives;
pub mod tool_call;
pub mod usage;
