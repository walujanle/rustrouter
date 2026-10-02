//! Translator schema enums. Pure data.

/// `ROLE`.
pub mod role {
    pub const USER: &str = "user";
    pub const ASSISTANT: &str = "assistant";
    pub const TOOL: &str = "tool";
    pub const SYSTEM: &str = "system";
    pub const DEVELOPER: &str = "developer";
}

/// `OPENAI_BLOCK`.
pub mod openai_block {
    pub const TEXT: &str = "text";
    pub const IMAGE_URL: &str = "image_url";
    pub const IMAGE: &str = "image";
    pub const INPUT_AUDIO: &str = "input_audio";
    pub const AUDIO_URL: &str = "audio_url";
    pub const FILE: &str = "file";
    pub const FUNCTION: &str = "function";
}

/// `CLAUDE_BLOCK`.
pub mod claude_block {
    pub const TEXT: &str = "text";
    pub const IMAGE: &str = "image";
    pub const DOCUMENT: &str = "document";
    pub const TOOL_USE: &str = "tool_use";
    pub const TOOL_RESULT: &str = "tool_result";
    pub const THINKING: &str = "thinking";
    pub const REDACTED_THINKING: &str = "redacted_thinking";
    pub const SERVER_TOOL_USE: &str = "server_tool_use";
    pub const WEB_SEARCH_TOOL_RESULT: &str = "web_search_tool_result";
}

/// `RESPONSES_ITEM`.
pub mod responses_item {
    pub const MESSAGE: &str = "message";
    pub const FUNCTION_CALL: &str = "function_call";
    pub const FUNCTION_CALL_OUTPUT: &str = "function_call_output";
    pub const CUSTOM_TOOL_CALL: &str = "custom_tool_call";
    pub const CUSTOM_TOOL_CALL_OUTPUT: &str = "custom_tool_call_output";
    pub const ADDITIONAL_TOOLS: &str = "additional_tools";
    pub const REASONING: &str = "reasoning";
    pub const OUTPUT_TEXT: &str = "output_text";
    pub const INPUT_TEXT: &str = "input_text";
    pub const INPUT_IMAGE: &str = "input_image";
    pub const SUMMARY_TEXT: &str = "summary_text";
}

/// `VALID_OPENAI_CONTENT_TYPES`.
pub const VALID_OPENAI_CONTENT_TYPES: [&str; 6] = [
    openai_block::TEXT,
    openai_block::IMAGE_URL,
    openai_block::IMAGE,
    openai_block::INPUT_AUDIO,
    openai_block::AUDIO_URL,
    openai_block::FILE,
];

/// `VALID_OPENAI_MESSAGE_TYPES`. Note the last entry is Claude's `tool_result`,
/// not an OpenAI type — `filterToOpenAIFormat` keeps tool_result blocks.
pub const VALID_OPENAI_MESSAGE_TYPES: [&str; 5] = [
    openai_block::TEXT,
    openai_block::IMAGE_URL,
    openai_block::IMAGE,
    "tool_calls",
    claude_block::TOOL_RESULT,
];

/// `OPENAI_FINISH`.
pub mod openai_finish {
    pub const STOP: &str = "stop";
    pub const LENGTH: &str = "length";
    pub const TOOL_CALLS: &str = "tool_calls";
    pub const CONTENT_FILTER: &str = "content_filter";
}

/// `CLAUDE_STOP`.
pub mod claude_stop {
    pub const END_TURN: &str = "end_turn";
    pub const MAX_TOKENS: &str = "max_tokens";
    pub const TOOL_USE: &str = "tool_use";
    pub const STOP_SEQUENCE: &str = "stop_sequence";
    pub const REFUSAL: &str = "refusal";
}

/// `MODEL_FALLBACK`.
pub const MODEL_FALLBACK: &str = "unknown";
/// `DEFAULT_IMAGE_MIME`.
pub const DEFAULT_IMAGE_MIME: &str = "image/png";
