//! OpenAI → Claude request.
//!
//! Three decisions here are not obvious:
//!
//! - `max_tokens` is clamped against the model's real `maxOutput`, not the
//!   conservative 64000 default, so a high-output model (Opus 4.8 = 128000) is
//!   not pre-clamped before `prepareClaudeRequest`'s model-aware step runs.
//! - Turn assembly splits tool results out. Anthropic requires a `tool_result`
//!   in its own user turn immediately after the `tool_use` it answers, and a
//!   turn that carries a `tool_use` is closed as soon as that block is seen.
//! - Tool names are sent unchanged. A `toolNameMap` (sent name → original name)
//!   would otherwise be filled and carried on the result for the response side,
//!   but `RequestContext` is shared and there is nothing to rewrite: the sent
//!   name *is* the original name.

use serde_json::{Map, Value, json};

use crate::catalog::get_capabilities_for_model;
use crate::constants::CLAUDE_SYSTEM_PROMPT;
use crate::runtime_config::DEFAULT_MAX_TOKENS;
use crate::translator::RequestContext;
use crate::translator::concerns::image::parse_data_uri;
use crate::translator::concerns::primitives::{
    js_string, js_truthy, js_truthy_opt, safe_parse_json,
};
use crate::translator::formats::gemini::extract_text_content;
use crate::translator::formats::max_tokens::adjust_max_tokens;
use crate::translator::schema::{claude_block, openai_block, role};

/// `CLAUDE_TOOL_CHOICE_TYPES`: the only `tool_choice.type` values Anthropic
/// accepts. Anything else is replaced with `auto` rather than forwarded, where
/// it would draw a 400.
const CLAUDE_TOOL_CHOICE_TYPES: [&str; 4] = ["auto", "any", "tool", "none"];

/// `openaiToClaudeRequest(model, body, stream)`.
pub fn openai_to_claude_request(ctx: &mut RequestContext<'_>, body: Value) -> Value {
    let mut result = Map::new();

    // `maxOutput || undefined`: a zero ceiling falls back to the default bound.
    let model_ceiling = get_capabilities_for_model(None, ctx.model).max_output;
    let ceiling = if model_ceiling != 0 {
        model_ceiling
    } else {
        DEFAULT_MAX_TOKENS
    };

    result.insert("model".into(), json!(ctx.model));
    result.insert(
        "max_tokens".into(),
        json!(adjust_max_tokens(&body, ceiling)),
    );
    result.insert("stream".into(), json!(ctx.stream));

    // `!== undefined`: a key present as `null` is still copied through.
    if let Some(temperature) = body.get("temperature") {
        result.insert("temperature".into(), temperature.clone());
    }

    result.insert("messages".into(), json!([]));
    let mut system_parts: Vec<String> = Vec::new();

    if let Some(Value::Array(messages)) = body.get("messages") {
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) != Some(role::SYSTEM) {
                continue;
            }
            match msg.get("content") {
                Some(Value::String(s)) => system_parts.push(s.clone()),
                other => {
                    system_parts.push(extract_text_content(other.unwrap_or(&Value::Null), "\n"))
                }
            }
        }

        let mut current_role: Option<String> = None;
        let mut current_parts: Vec<Value> = Vec::new();

        for msg in messages {
            if msg.get("role").and_then(Value::as_str) == Some(role::SYSTEM) {
                continue;
            }
            let new_role = match msg.get("role").and_then(Value::as_str) {
                Some(role::USER) | Some(role::TOOL) => role::USER,
                _ => role::ASSISTANT,
            };
            let blocks = get_content_blocks_from_message(msg);
            let has_tool_use = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE));
            let has_tool_result = blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_RESULT));

            // A tool_result must stand alone in the user turn that follows its
            // tool_use; anything else the message carried is deferred.
            if has_tool_result {
                let tool_result_blocks: Vec<Value> = blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_RESULT)
                    })
                    .cloned()
                    .collect();
                let other_blocks: Vec<Value> = blocks
                    .iter()
                    .filter(|b| {
                        b.get("type").and_then(Value::as_str) != Some(claude_block::TOOL_RESULT)
                    })
                    .cloned()
                    .collect();

                flush_current_message(&mut result, &current_role, &mut current_parts);

                if !tool_result_blocks.is_empty() {
                    push_message(
                        &mut result,
                        json!({"role": role::USER, "content": tool_result_blocks}),
                    );
                }
                if !other_blocks.is_empty() {
                    current_role = Some(new_role.to_string());
                    current_parts.extend(other_blocks);
                }
                continue;
            }

            if current_role.as_deref() != Some(new_role) {
                flush_current_message(&mut result, &current_role, &mut current_parts);
                current_role = Some(new_role.to_string());
            }

            current_parts.extend(blocks);

            if has_tool_use {
                flush_current_message(&mut result, &current_role, &mut current_parts);
            }
        }

        flush_current_message(&mut result, &current_role, &mut current_parts);

        // Cache the tail of the last assistant turn that has real content; a
        // thinking block cannot carry cache_control, so it is skipped.
        const VALID_BLOCK_TYPES: [&str; 4] = [
            claude_block::TEXT,
            claude_block::TOOL_USE,
            claude_block::TOOL_RESULT,
            claude_block::IMAGE,
        ];
        if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages.iter_mut().rev() {
                let is_assistant =
                    message.get("role").and_then(Value::as_str) == Some(role::ASSISTANT);
                let content_len = message
                    .get("content")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if !is_assistant || content_len == 0 {
                    continue;
                }
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in content.iter_mut().rev() {
                        let is_valid = block
                            .get("type")
                            .and_then(Value::as_str)
                            .is_some_and(|t| VALID_BLOCK_TYPES.contains(&t));
                        if is_valid {
                            if let Some(obj) = block.as_object_mut() {
                                obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                            }
                            break;
                        }
                    }
                }
                break;
            }
        }
    }

    if let Some(response_format) = body.get("response_format").filter(|v| js_truthy(v)) {
        let format_type = response_format.get("type").and_then(Value::as_str);
        let schema = response_format
            .get("json_schema")
            .and_then(|j| j.get("schema"));
        if format_type == Some("json_schema") && js_truthy_opt(schema) {
            let schema_json =
                serde_json::to_string_pretty(schema.unwrap_or(&Value::Null)).unwrap_or_default();
            system_parts.push(format!(
                "You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{schema_json}\n```\nRespond ONLY with the JSON object, no other text."
            ));
        } else if format_type == Some("json_object") {
            system_parts.push(
                "You must respond with valid JSON. Respond ONLY with a JSON object, no other text."
                    .to_string(),
            );
        }
    }

    let claude_code_prompt = json!({"type": claude_block::TEXT, "text": CLAUDE_SYSTEM_PROMPT});

    if !system_parts.is_empty() {
        let system_text = system_parts.join("\n");
        result.insert(
            "system".into(),
            json!([
                claude_code_prompt,
                {"type": claude_block::TEXT, "text": system_text, "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ]),
        );
    } else {
        result.insert("system".into(), json!([claude_code_prompt]));
    }

    if let Some(Value::Array(tools)) = body.get("tools") {
        let mut converted: Vec<Value> = Vec::new();
        for tool in tools {
            // A built-in tool (e.g. web_search_20250305) passes through as-is.
            // `toolType && toolType !== "function"` is truthy for a non-string
            // type too, so this reads the raw value rather than only strings.
            let is_builtin = tool
                .get("type")
                .is_some_and(|t| js_truthy(t) && t.as_str() != Some(openai_block::FUNCTION));
            if is_builtin {
                converted.push(tool.clone());
                continue;
            }
            // Function-shaped tools arrive with or without the parent `type`:
            // `{type:"function", function:{…}}` and the legacy `{function:{…}}`
            // must both yield `toolData.name`.
            let tool_data = tool
                .get("function")
                .filter(|v| !v.is_null())
                .unwrap_or(tool);
            let tool_name = js_string_undefined(tool_data.get("name"));

            let input_schema = tool_data
                .get("parameters")
                .filter(|v| js_truthy(v))
                .or_else(|| tool_data.get("input_schema").filter(|v| js_truthy(v)))
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}, "required": []}));

            converted.push(json!({
                "name": tool_name,
                // `toolData.description || ""`: a falsy description becomes the
                // empty string, a truthy one passes through unchanged.
                "description": tool_data
                    .get("description")
                    .filter(|v| js_truthy(v))
                    .cloned()
                    .unwrap_or_else(|| json!("")),
                "input_schema": input_schema,
            }));
        }

        if let Some(last) = converted.last_mut().and_then(Value::as_object_mut) {
            last.insert(
                "cache_control".into(),
                json!({"type": "ephemeral", "ttl": "1h"}),
            );
        }

        result.insert("tools".into(), Value::Array(converted));
    }

    if let Some(tool_choice) = body.get("tool_choice").filter(|v| js_truthy(v)) {
        result.insert(
            "tool_choice".into(),
            convert_openai_tool_choice(tool_choice),
        );
    }

    Value::Object(result)
}

/// `flushCurrentMessage()`: close the pending turn, keeping `current_role` set.
fn flush_current_message(
    result: &mut Map<String, Value>,
    current_role: &Option<String>,
    current_parts: &mut Vec<Value>,
) {
    let Some(role_) = current_role else {
        return;
    };
    if current_parts.is_empty() {
        return;
    }
    let content = std::mem::take(current_parts);
    let mut message = Map::new();
    message.insert("role".into(), json!(role_));
    message.insert("content".into(), Value::Array(content));
    push_message(result, Value::Object(message));
}

fn push_message(result: &mut Map<String, Value>, msg: Value) {
    if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(msg);
    }
}

/// `getContentBlocksFromMessage(msg, toolNameMap)`.
///
/// The second parameter is never read — the prefix comes from the constant —
/// so it is not modelled here.
fn get_content_blocks_from_message(msg: &Value) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let msg_role = msg.get("role").and_then(Value::as_str);

    if msg_role == Some(role::TOOL) {
        // `tool_use_id: msg.tool_call_id` and `content: msg.content` are
        // undefined when the key is absent, and an undefined value omits the
        // key from the wire object rather than writing null.
        let mut block = Map::new();
        block.insert("type".into(), json!(claude_block::TOOL_RESULT));
        if let Some(v) = msg.get("tool_call_id") {
            block.insert("tool_use_id".into(), v.clone());
        }
        if let Some(v) = msg.get("content") {
            block.insert("content".into(), v.clone());
        }
        blocks.push(Value::Object(block));
    } else if msg_role == Some(role::USER) {
        match msg.get("content") {
            Some(Value::String(content)) => {
                if !content.is_empty() {
                    blocks.push(json!({"type": claude_block::TEXT, "text": content}));
                }
            }
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some(openai_block::TEXT) => {
                            if js_truthy_opt(part.get("text")) {
                                blocks.push(
                                    json!({"type": claude_block::TEXT, "text": part.get("text")}),
                                );
                            }
                        }
                        Some(claude_block::TOOL_RESULT) => {
                            let mut block = Map::new();
                            block.insert("type".into(), json!(claude_block::TOOL_RESULT));
                            if let Some(v) = part.get("tool_use_id") {
                                block.insert("tool_use_id".into(), v.clone());
                            }
                            if let Some(v) = part.get("content") {
                                block.insert("content".into(), v.clone());
                            }
                            if let Some(is_error) = part.get("is_error").filter(|v| js_truthy(v)) {
                                block.insert("is_error".into(), is_error.clone());
                            }
                            blocks.push(Value::Object(block));
                        }
                        Some(openai_block::IMAGE_URL) => {
                            let url = part.get("image_url").and_then(|i| i.get("url"));
                            if let Some(parsed) =
                                url.and_then(Value::as_str).and_then(parse_data_uri)
                            {
                                blocks.push(json!({
                                    "type": claude_block::IMAGE,
                                    "source": {"type": "base64", "media_type": parsed.mime_type, "data": parsed.base64},
                                }));
                            } else if let Some(url) = url.and_then(Value::as_str)
                                && (url.starts_with("http://") || url.starts_with("https://"))
                            {
                                blocks.push(json!({
                                    "type": claude_block::IMAGE,
                                    "source": {"type": "url", "url": url},
                                }));
                            }
                        }
                        Some(openai_block::IMAGE) => {
                            if let Some(source) = part.get("source").filter(|v| js_truthy(v)) {
                                blocks.push(json!({"type": claude_block::IMAGE, "source": source}));
                            }
                        }
                        Some(openai_block::FILE) => {
                            // OpenAI file block -> Claude document. Claude rejects
                            // every mime but PDF here, so anything else is dropped.
                            if let Some(file) = part.get("file").filter(|v| js_truthy(v))
                                && let Some(parsed) = file
                                    .get("file_data")
                                    .and_then(Value::as_str)
                                    .and_then(parse_data_uri)
                                && parsed.mime_type == "application/pdf"
                            {
                                blocks.push(json!({
                                    "type": claude_block::DOCUMENT,
                                    "source": {"type": "base64", "media_type": parsed.mime_type, "data": parsed.base64},
                                }));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    } else if msg_role == Some(role::ASSISTANT) {
        match msg.get("content") {
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some(openai_block::TEXT) => {
                            if js_truthy_opt(part.get("text")) {
                                blocks.push(
                                    json!({"type": claude_block::TEXT, "text": part.get("text")}),
                                );
                            }
                        }
                        Some(claude_block::TOOL_USE) => {
                            // The name already carries the declaration's prefix.
                            // id/name/input are undefined when absent, so the key
                            // is dropped rather than written as null.
                            let mut block = Map::new();
                            block.insert("type".into(), json!(claude_block::TOOL_USE));
                            for key in ["id", "name", "input"] {
                                if let Some(v) = part.get(key) {
                                    block.insert(key.into(), v.clone());
                                }
                            }
                            blocks.push(Value::Object(block));
                        }
                        Some(claude_block::THINKING) => {
                            // cache_control is not allowed on a thinking block.
                            let mut block = part.clone();
                            if let Some(obj) = block.as_object_mut() {
                                obj.shift_remove("cache_control");
                            }
                            blocks.push(block);
                        }
                        _ => {}
                    }
                }
            }
            Some(content) if js_truthy(content) => {
                let text = match content {
                    Value::String(s) => s.clone(),
                    other => extract_text_content(other, "\n"),
                };
                if !text.is_empty() {
                    blocks.push(json!({"type": claude_block::TEXT, "text": text}));
                }
            }
            _ => {}
        }

        if let Some(Value::Array(tool_calls)) = msg.get("tool_calls") {
            for tc in tool_calls {
                if tc.get("type").and_then(Value::as_str) != Some(openai_block::FUNCTION) {
                    continue;
                }
                let function = tc.get("function");
                let name = js_string_undefined(function.and_then(|f| f.get("name")));
                let mut block = Map::new();
                block.insert("type".into(), json!(claude_block::TOOL_USE));
                if let Some(v) = tc.get("id") {
                    block.insert("id".into(), v.clone());
                }
                block.insert("name".into(), json!(name));
                // `safeParseJSON(args, args)`: a non-string passes through, and
                // an absent `arguments` omits the key (JS `undefined`).
                if let Some(arguments) = function.and_then(|f| f.get("arguments")) {
                    block.insert(
                        "input".into(),
                        safe_parse_json(arguments.clone(), arguments.clone()),
                    );
                }
                blocks.push(Value::Object(block));
            }
        }
    }

    blocks
}

/// `convertOpenAIToolChoice(choice)`.
fn convert_openai_tool_choice(choice: &Value) -> Value {
    if !js_truthy(choice) {
        return json!({"type": "auto"});
    }

    if choice.is_string() {
        if choice.as_str() == Some("required") {
            return json!({"type": "any"});
        }
        return json!({"type": "auto"});
    }

    if choice.is_object() {
        // The OpenAI forced-tool shape also carries `.type: "function"`, which
        // Claude rejects, so it is checked before the native pass-through.
        if js_truthy_opt(choice.get("function").and_then(|f| f.get("name"))) {
            return json!({"type": "tool", "name": choice.get("function").and_then(|f| f.get("name"))});
        }
        if let Some(type_) = choice.get("type").and_then(Value::as_str)
            && CLAUDE_TOOL_CHOICE_TYPES.contains(&type_)
        {
            return choice.clone();
        }
    }

    json!({"type": "auto"})
}

/// `String(value)`: an absent value is the literal `"undefined"`, reached by
/// string concatenation.
fn js_string_undefined(value: Option<&Value>) -> String {
    value
        .map(js_string)
        .unwrap_or_else(|| "undefined".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Credentials;
    use crate::translator::RequestMeta;

    fn run(body: Value) -> Value {
        let credentials = Credentials::default();
        let mut meta = RequestMeta::default();
        let mut ctx = RequestContext {
            model: "claude-3",
            stream: true,
            credentials: &credentials,
            meta: &mut meta,
        };
        openai_to_claude_request(&mut ctx, body)
    }

    #[test]
    fn the_claude_code_prompt_leads_and_user_system_text_follows_with_a_ttl() {
        let out = run(json!({
            "messages": [{"role": "system", "content": "be terse"}],
        }));
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], json!(CLAUDE_SYSTEM_PROMPT));
        assert_eq!(system[1]["text"], json!("be terse"));
        assert_eq!(
            system[1]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
    }

    #[test]
    fn without_a_system_message_only_the_claude_code_prompt_is_sent() {
        let out = run(json!({"messages": []}));
        assert_eq!(out["system"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn consecutive_same_role_messages_merge_into_one_turn() {
        let out = run(json!({
            "messages": [
                {"role": "user", "content": "a"},
                {"role": "user", "content": "b"},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], json!("user"));
        assert_eq!(
            msgs[0]["content"],
            json!([
                {"type": "text", "text": "a"},
                {"type": "text", "text": "b"},
            ])
        );
    }

    #[test]
    fn a_tool_use_closes_its_turn_and_a_tool_result_gets_its_own_user_turn() {
        let out = run(json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}},
                ]},
                {"role": "tool", "tool_call_id": "t1", "content": "42"},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("assistant"));
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
        // The assistant turn's tail block carries the ephemeral cache marker.
        assert_eq!(
            msgs[0]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(
            msgs[1],
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "42"}],
            })
        );
    }

    #[test]
    fn assistant_tool_calls_become_tool_use_blocks_with_parsed_arguments() {
        let out = run(json!({
            "messages": [{
                "role": "assistant",
                "content": "thinking out loud",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{\"a\":1}"},
                }],
            }],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["content"][0],
            json!({"type": "text", "text": "thinking out loud"})
        );
        // The `tool_use` is the last cache-eligible block of the last assistant
        // message, so it carries the ephemeral marker.
        assert_eq!(
            msgs[0]["content"][1],
            json!({
                "type": "tool_use",
                "id": "c1",
                "name": "f",
                "input": {"a": 1},
                "cache_control": {"type": "ephemeral"},
            })
        );
    }

    #[test]
    fn an_unparseable_tool_arguments_string_is_kept_verbatim() {
        let out = run(json!({
            "messages": [{
                "role": "assistant",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "not json"}}],
            }],
        }));
        assert_eq!(out["messages"][0]["content"][0]["input"], json!("not json"));
    }

    #[test]
    fn images_become_base64_or_url_sources_and_non_pdf_files_are_dropped() {
        let out = run(json!({
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}},
                {"type": "file", "file": {"file_data": "data:application/pdf;base64,BBBB"}},
                {"type": "file", "file": {"file_data": "data:text/plain;base64,CCCC"}},
            ]}],
        }));
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "the non-PDF file is dropped");
        assert_eq!(
            content[0]["source"],
            json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})
        );
        assert_eq!(
            content[1]["source"],
            json!({"type": "url", "url": "https://example.com/a.png"})
        );
        assert_eq!(content[2]["type"], json!("document"));
    }

    #[test]
    fn a_thinking_block_loses_its_cache_control() {
        let out = run(json!({
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "h", "cache_control": {"type": "ephemeral"}},
            ]}],
        }));
        let block = &out["messages"][0]["content"][0];
        assert_eq!(block["type"], json!("thinking"));
        assert!(
            block.get("cache_control").is_none(),
            "stripped on the way in"
        );
    }

    #[test]
    fn tools_are_converted_and_the_last_one_is_cached() {
        let out = run(json!({
            "tools": [
                {"type": "function", "function": {"name": "echo", "description": "d", "parameters": {"type": "object"}}},
                {"function": {"name": "legacy"}},
                {"type": "web_search_20250305", "name": "ws"},
            ],
            "messages": [],
        }));
        let tools = out["tools"].as_array().unwrap();
        assert_eq!(tools[0]["name"], json!("echo"));
        assert_eq!(tools[0]["description"], json!("d"));
        assert_eq!(tools[0]["input_schema"], json!({"type": "object"}));
        // The bare-function shape must still yield a name.
        assert_eq!(tools[1]["name"], json!("legacy"));
        assert_eq!(tools[1]["description"], json!(""));
        assert_eq!(
            tools[1]["input_schema"],
            json!({"type": "object", "properties": {}, "required": []})
        );
        // Built-in tools pass through untouched.
        assert_eq!(tools[2]["type"], json!("web_search_20250305"));
        assert_eq!(tools[2]["name"], json!("ws"));
        assert_eq!(
            tools[2]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        assert!(tools[0].get("cache_control").is_none());
    }

    #[test]
    fn tool_choice_shapes_are_normalized_to_what_claude_accepts() {
        assert_eq!(
            run(json!({"tool_choice": "required", "messages": []}))["tool_choice"],
            json!({"type": "any"})
        );
        assert_eq!(
            run(json!({"tool_choice": "auto", "messages": []}))["tool_choice"],
            json!({"type": "auto"})
        );
        assert_eq!(
            run(
                json!({"tool_choice": {"type": "function", "function": {"name": "f"}}, "messages": []})
            )["tool_choice"],
            json!({"type": "tool", "name": "f"})
        );
        assert_eq!(
            run(json!({"tool_choice": {"type": "any"}, "messages": []}))["tool_choice"],
            json!({"type": "any"})
        );
        assert_eq!(
            run(json!({"tool_choice": {"type": "bogus"}, "messages": []}))["tool_choice"],
            json!({"type": "auto"})
        );
        assert!(run(json!({"messages": []})).get("tool_choice").is_none());
    }

    #[test]
    fn a_json_object_response_format_appends_a_system_instruction() {
        let out = run(json!({"response_format": {"type": "json_object"}, "messages": []}));
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert!(system[1]["text"].as_str().unwrap().contains("valid JSON"));

        let out = run(json!({
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object"}}},
            "messages": [],
        }));
        assert!(
            out["system"][1]["text"]
                .as_str()
                .unwrap()
                .contains("\"type\": \"object\"")
        );
    }

    #[test]
    fn temperature_and_stream_are_carried_over() {
        let out = run(json!({"temperature": 0.5, "messages": []}));
        assert_eq!(out["temperature"], json!(0.5));
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["model"], json!("claude-3"));
    }
}
