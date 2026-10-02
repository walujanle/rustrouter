//! Cross-cutting helpers the translators and executors share.

pub mod bypass_handler;
pub mod chat_log;
pub mod claude_cloaking;
pub mod claude_signature;
pub mod client_detector;
pub mod error;
pub mod fingerprint;
pub mod gemini_bridge;
pub mod in_flight;
pub mod model_markers;
pub mod ollama_transform;
pub mod reasoning_injector;
pub mod responses_stream_helpers;
pub mod sse;
pub mod stream;
pub mod stream_handler;
pub mod stream_helpers;
pub mod tool_deduper;
pub mod usage_tracking;
