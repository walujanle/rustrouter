//! Request translators.
//!
//! Each module exposes one `pub fn …_request(ctx, body) -> Value` and, where a
//! second pair is registered from the same file, a second function.

pub mod claude_to_openai;
pub mod openai_responses;
pub mod openai_to_claude;
pub mod openai_to_commandcode;
