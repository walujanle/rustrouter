//! Response translators.
//!
//! Each module exposes one `pub fn …_response(chunk, state) -> Vec<Value>`.
//! An empty vector means "emit nothing", the `null`/`[]` case.

pub mod claude_to_openai;
pub mod commandcode_to_openai;
pub mod openai_responses;
pub mod openai_to_claude;
