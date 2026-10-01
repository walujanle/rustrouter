//! The transformers that sit between the chat pipeline and the client.
//!
//! [`stream_to_json`] folds a Responses-API SSE stream back into one JSON
//! object, for a client that asked for JSON but landed on a provider that
//! forces streaming.
//!
//! Lifting Chat Completions SSE into Responses-API SSE is deliberately absent:
//! the translator registry already does that through
//! `translator::response::openai_responses::openai_to_openai_responses_response`,
//! which `utils/stream.rs` drives for a Responses client. A second
//! implementation would be a duplicate, not a seam.

pub mod stream_to_json;
