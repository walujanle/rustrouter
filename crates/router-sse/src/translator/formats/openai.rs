//! `filterToOpenAIFormat`.
//!
//! The last pass before an OpenAI-format request leaves the process. It
//! normalizes hybrid bodies — a client may send Claude content blocks with
//! OpenAI tools — into something every OpenAI-compatible upstream accepts.
//!
//! Two behaviours look like bugs but are load-bearing and kept: a `developer`
//! role is folded to `system` (many providers reject `developer`), and a
//! `tool_result` block is kept even though it is a Claude type, because
//! `VALID_OPENAI_MESSAGE_TYPES` lists it and the downstream providers accept it.

use serde_json::{Map, Value, json};

use crate::translator::schema::{VALID_OPENAI_CONTENT_TYPES, claude_block, openai_block, role};

/// Options for `filter_to_openai_format`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenAiFilterOptions {
    /// Keep `cache_control` on content blocks (DashScope / alicode).
    pub preserve_cache_control: bool,
}

/// `filterToOpenAIFormat(body, opts)`, in place.
pub fn filter_to_openai_format(body: &mut Value, opts: OpenAiFilterOptions) {
    let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };

    let mapped: Vec<Value> = messages
        .iter()
        .map(|msg| filter_message(msg, opts.preserve_cache_control))
        .collect();
    let filtered: Vec<Value> = mapped.into_iter().filter(keep_message).collect();
    body["messages"] = Value::Array(filtered);

    // Some providers (Qwen) reject an empty tools array.
    if body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| t.is_empty())
        && let Some(obj) = body.as_object_mut()
    {
        obj.remove("tools");
    }

    normalize_tools(body);
    normalize_tool_choice(body);
}

/// One message through the block filter. `tool` and assistant-with-`tool_calls`
/// messages pass through untouched.
fn filter_message(msg: &Value, keep_cache: bool) -> Value {
    let msg_role = msg.get("role").and_then(Value::as_str).unwrap_or_default();

    let msg = if msg_role == role::DEVELOPER {
        let mut m = msg.clone();
        m["role"] = json!(role::SYSTEM);
        m
    } else {
        msg.clone()
    };

    if msg_role == role::TOOL {
        return msg;
    }
    if msg_role == role::ASSISTANT && msg.get("tool_calls").is_some() {
        return msg;
    }

    let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
        return msg; // a string content or a non-array passes through
    };

    let mut out: Vec<Value> = Vec::with_capacity(content.len());
    for block in &content {
        let block_type = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if block_type == claude_block::THINKING || block_type == claude_block::REDACTED_THINKING {
            continue;
        }
        if VALID_OPENAI_CONTENT_TYPES.contains(&block_type) {
            out.push(strip_block(block, keep_cache));
        } else if block_type == claude_block::TOOL_USE {
            // Converted to tool_calls elsewhere.
            continue;
        } else if block_type == claude_block::TOOL_RESULT {
            out.push(strip_block(block, keep_cache));
        }
    }
    if out.is_empty() {
        out.push(json!({"type": openai_block::TEXT, "text": ""}));
    }
    let mut msg = msg;
    msg["content"] = Value::Array(out);
    msg
}

/// `stripBlock(block)`: drop `signature`, and `cache_control` unless preserved.
fn strip_block(block: &Value, keep_cache: bool) -> Value {
    let Some(obj) = block.as_object() else {
        return block.clone();
    };
    let mut out = Map::new();
    for (k, v) in obj {
        if k == "signature" {
            continue;
        }
        if k == "cache_control" && !keep_cache {
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    Value::Object(out)
}

/// `filter(...)`: drop a message with only empty text, but never a tool message
/// or an assistant message carrying tool_calls.
fn keep_message(msg: &Value) -> bool {
    let msg_role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    if msg_role == role::TOOL {
        return true;
    }
    if msg_role == role::ASSISTANT && msg.get("tool_calls").is_some() {
        return true;
    }
    match msg.get("content") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(blocks)) => blocks.iter().any(|b| {
            let is_text = b.get("type").and_then(Value::as_str) == Some(openai_block::TEXT);
            if is_text {
                b.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| !t.trim().is_empty())
            } else {
                true
            }
        }),
        _ => true,
    }
}

/// `normalize_tools`: Claude and Gemini tool shapes → OpenAI function tools.
fn normalize_tools(body: &mut Value) {
    let Some(tools) = body.get("tools").and_then(Value::as_array).cloned() else {
        return;
    };
    if tools.is_empty() {
        return;
    }
    let mut out: Vec<Value> = Vec::with_capacity(tools.len());
    for tool in &tools {
        if tool.get("type").and_then(Value::as_str) == Some(openai_block::FUNCTION)
            && tool.get("function").is_some()
        {
            out.push(tool.clone());
            continue;
        }
        // Claude: {name, description, input_schema}
        if tool.get("name").is_some()
            && (tool.get("input_schema").is_some() || tool.get("description").is_some())
        {
            out.push(json!({
                "type": openai_block::FUNCTION,
                "function": {
                    "name": tool.get("name"),
                    "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                }
            }));
            continue;
        }
        // Gemini: {functionDeclarations: [{name, description, parameters}]}
        if let Some(decls) = tool.get("functionDeclarations").and_then(Value::as_array) {
            for f in decls {
                out.push(json!({
                    "type": openai_block::FUNCTION,
                    "function": {
                        "name": f.get("name"),
                        "description": f.get("description").and_then(Value::as_str).unwrap_or(""),
                        "parameters": f.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    }
                }));
            }
            continue;
        }
        out.push(tool.clone());
    }
    body["tools"] = Value::Array(out);
}

/// `normalize_tool_choice`: Claude `{type: auto|any|tool}` → OpenAI.
fn normalize_tool_choice(body: &mut Value) {
    let Some(choice) = body.get("tool_choice").cloned() else {
        return;
    };
    if !choice.is_object() {
        return;
    }
    let kind = choice
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let replacement = match kind {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": openai_block::FUNCTION, "function": {"name": name}})),
        _ => None,
    };
    if let Some(replacement) = replacement {
        body["tool_choice"] = replacement;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn thinking_blocks_are_removed_and_the_empty_message_is_dropped() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "thinking", "thinking": "x"},
                {"type": "redacted_thinking", "data": "y"},
            ]},
        ]});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        // The block filter leaves an empty text placeholder, then the message
        // filter drops the message for having nothing but empty text.
        assert_eq!(body["messages"], json!([]));
    }

    #[test]
    fn the_developer_role_is_folded_to_system() {
        let mut body = json!({"messages": [{"role": "developer", "content": "be brief"}]});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        assert_eq!(body["messages"][0]["role"], json!("system"));
    }

    #[test]
    fn tool_messages_and_assistant_tool_calls_are_never_dropped() {
        let mut body = json!({"messages": [
            {"role": "tool", "tool_call_id": "a", "content": ""},
            {"role": "assistant", "tool_calls": [{"id": "a"}], "content": ""},
        ]});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_empty_string_message_is_dropped() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "   "},
            {"role": "user", "content": "keep"},
        ]});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0]["content"], json!("keep"));
    }

    #[test]
    fn cache_control_survives_only_when_asked() {
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}, "signature": "s"},
        ]}]});

        let mut stripped = body.clone();
        filter_to_openai_format(&mut stripped, OpenAiFilterOptions::default());
        assert!(
            stripped["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert!(
            stripped["messages"][0]["content"][0]
                .get("signature")
                .is_none()
        );

        let mut kept = body.clone();
        filter_to_openai_format(
            &mut kept,
            OpenAiFilterOptions {
                preserve_cache_control: true,
            },
        );
        assert!(
            kept["messages"][0]["content"][0]
                .get("cache_control")
                .is_some()
        );
        assert!(
            kept["messages"][0]["content"][0].get("signature").is_none(),
            "signature always drops"
        );
    }

    #[test]
    fn an_empty_tools_array_is_removed() {
        let mut body = json!({"messages": [], "tools": []});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn claude_and_gemini_tools_are_normalized() {
        let mut body = json!({"messages": [], "tools": [
            {"name": "claude_tool", "description": "d", "input_schema": {"type": "object"}},
            {"functionDeclarations": [{"name": "gem_tool", "description": "g", "parameters": {"type": "object"}}]},
        ]});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            2,
            "the gemini entry flattens into one function tool"
        );
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["function"]["name"], json!("claude_tool"));
        assert_eq!(tools[1]["function"]["name"], json!("gem_tool"));
    }

    #[test]
    fn tool_choice_is_translated_from_the_claude_shape() {
        for (input, expected) in [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (
                json!({"type": "tool", "name": "t"}),
                json!({"type": "function", "function": {"name": "t"}}),
            ),
        ] {
            let mut body = json!({"messages": [], "tool_choice": input});
            filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
            assert_eq!(body["tool_choice"], expected);
        }
        // A string tool_choice passes through.
        let mut body = json!({"messages": [], "tool_choice": "none"});
        filter_to_openai_format(&mut body, OpenAiFilterOptions::default());
        assert_eq!(body["tool_choice"], json!("none"));
    }
}
