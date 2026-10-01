//! OpenAI → CommandCode request.
//!
//! The upstream `/alpha/generate` body is Anthropic-shaped in two places that
//! bite:
//!
//! - `params.system` is a single top-level string. A `system` role inside
//!   `messages[]` is rejected, so those turns are folded out and joined with a
//!   blank line.
//! - Every `messages[*].content` is an array of blocks, never a bare string.
//!   Text, native images, `tool-call` and `tool-result` are the only block
//!   shapes the endpoint accepts.
//!
//! `threadId` is minted per request and `config` is a stub: `structure` and the
//! git fields describe a local checkout the gateway does not have, so they are
//! sent empty. `workingDir`/`environment` are still filled so the shape
//! matches, and `environment` uses Node's `process.platform` spelling
//! (`win32`, `darwin`) rather than `std::env::consts::OS`.

use serde_json::{Map, Value, json};

use crate::runtime_config::DEFAULT_MAX_TOKENS;
use crate::translator::RequestContext;
use crate::translator::concerns::image::{encode_data_uri, parse_data_uri};
use crate::translator::concerns::primitives::{
    js_nullish, js_string, js_string_or_empty, js_truthy as truthy, js_truthy_opt as truthy_opt,
    safe_parse_json,
};
use crate::translator::schema::{claude_block, openai_block, role};

/// `openaiToCommandCodeRequest(model, body, stream)`.
pub fn openai_to_commandcode_request(ctx: &mut RequestContext<'_>, body: Value) -> Value {
    let (messages, system) = convert_messages(body.get("messages"));

    let mut params = Map::new();
    params.insert("model".into(), json!(ctx.model));
    params.insert("messages".into(), Value::Array(messages));
    params.insert("stream".into(), json!(ctx.stream));

    // `body.max_tokens ?? body.max_output_tokens ?? DEFAULT_MAX_TOKENS` —
    // `??`, so an explicit 0 is kept and only null/absent falls through.
    let max_tokens_default = json!(DEFAULT_MAX_TOKENS);
    let max_tokens = js_nullish(body.get("max_output_tokens"), Some(&max_tokens_default));
    let max_tokens = js_nullish(body.get("max_tokens"), max_tokens)
        .cloned()
        .unwrap_or(Value::Null);
    params.insert("max_tokens".into(), max_tokens);

    let temperature_default = json!(0.3);
    let temperature = js_nullish(body.get("temperature"), Some(&temperature_default))
        .cloned()
        .unwrap_or_else(|| json!(0.3));
    params.insert("temperature".into(), temperature);

    if !system.is_empty() {
        params.insert("system".into(), json!(system));
    }

    if let Some(tools) = convert_tools(body.get("tools")) {
        params.insert("tools".into(), tools);
    }

    // `!= null`: an explicit 0 is kept, a null is dropped.
    if let Some(top_p) = body.get("top_p").filter(|v| !v.is_null()) {
        params.insert("top_p".into(), top_p.clone());
    }

    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    json!({
        "threadId": uuid::Uuid::new_v4().to_string(),
        "memory": "",
        "config": {
            "workingDir": working_dir(),
            "date": today,
            "environment": node_platform(),
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "",
            "gitStatus": "",
            "recentCommits": [],
        },
        "params": Value::Object(params),
    })
}

/// `process.cwd()`.
fn working_dir() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default()
}

/// `process.platform`. Node's spellings, not Rust's.
fn node_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        "darwin"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "freebsd") {
        "freebsd"
    } else if cfg!(target_os = "openbsd") {
        "openbsd"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        std::env::consts::OS
    }
}

/// `flattenText(content)`: strings joined with a newline, non-array objects read
/// through `.text` when it is a string.
fn flatten_text(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut parts: Vec<String> = Vec::new();
            for p in items {
                if let Value::String(s) = p {
                    parts.push(s.clone());
                } else if truthy(p)
                    && let Some(t) = p.get("text").and_then(Value::as_str)
                {
                    parts.push(t.to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => js_string(other),
    }
}

/// `toNativeImageBlock(part)`: an OpenAI image block or a Claude base64 source
/// becomes CommandCode's `{type:"image", image, mimeType, mediaType}`.
fn to_native_image_block(part: &Value) -> Option<Value> {
    if !matches!(part, Value::Object(_) | Value::Array(_)) {
        return None;
    }
    let part_type = part.get("type").and_then(Value::as_str);

    if part_type == Some(openai_block::IMAGE_URL) {
        let url = match part.get("image_url") {
            Some(Value::String(s)) => Some(s.as_str()),
            other => other.and_then(|v| v.get("url")).and_then(Value::as_str),
        };
        let parsed = parse_data_uri(url?)?;
        return Some(json!({
            "type": openai_block::IMAGE,
            "image": encode_data_uri(parsed.mime_type, parsed.base64),
            "mimeType": parsed.mime_type,
            "mediaType": parsed.mime_type,
        }));
    }

    // OPENAI_BLOCK.IMAGE and CLAUDE_BLOCK.IMAGE are the same string.
    if part_type == Some(openai_block::IMAGE) || part_type == Some(claude_block::IMAGE) {
        if let Some(image) = part.get("image").and_then(Value::as_str)
            && image.starts_with("data:")
        {
            let parsed = parse_data_uri(image);
            let mime = part
                .get("mimeType")
                .filter(|v| truthy(v))
                .cloned()
                .or_else(|| parsed.as_ref().map(|p| json!(p.mime_type)))
                .unwrap_or_else(|| json!("image/png"));
            return Some(json!({
                "type": openai_block::IMAGE,
                "image": image,
                "mimeType": mime.clone(),
                "mediaType": mime,
            }));
        }

        let source = part.get("source");
        let source_data = source.and_then(|s| s.get("data"));
        if source.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("base64")
            && source_data.is_some_and(Value::is_string)
        {
            let mime = source
                .and_then(|s| s.get("media_type"))
                .filter(|v| truthy(v))
                .cloned()
                .unwrap_or_else(|| json!("image/png"));
            let data = source_data.and_then(Value::as_str).unwrap_or("");
            return Some(json!({
                "type": openai_block::IMAGE,
                "image": encode_data_uri(&js_string(&mime), data),
                "mimeType": mime.clone(),
                "mediaType": mime,
            }));
        }
    }

    None
}

/// `toContentBlocks(content)`: always an array, never a bare string.
fn to_content_blocks(content: Option<&Value>) -> Value {
    match content {
        None | Some(Value::Null) => json!([{"type": openai_block::TEXT, "text": ""}]),
        Some(Value::String(s)) => json!([{"type": openai_block::TEXT, "text": s}]),
        Some(Value::Array(items)) => {
            let mut blocks: Vec<Value> = Vec::new();
            for part in items {
                if let Value::String(s) = part {
                    blocks.push(json!({"type": openai_block::TEXT, "text": s}));
                    continue;
                }
                // `part && typeof part === "object"` — numbers and booleans fall through.
                if !matches!(part, Value::Object(_) | Value::Array(_)) {
                    continue;
                }
                let is_text = part.get("type").and_then(Value::as_str) == Some(openai_block::TEXT)
                    && part.get("text").is_some_and(Value::is_string);
                if is_text {
                    blocks.push(json!({
                        "type": openai_block::TEXT,
                        "text": part.get("text").cloned().unwrap_or(Value::Null),
                    }));
                } else if let Some(image) = to_native_image_block(part) {
                    blocks.push(image);
                } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                    blocks.push(json!({"type": openai_block::TEXT, "text": text}));
                }
            }
            if blocks.is_empty() {
                json!([{"type": openai_block::TEXT, "text": ""}])
            } else {
                Value::Array(blocks)
            }
        }
        Some(other) => json!([{"type": openai_block::TEXT, "text": js_string(other)}]),
    }
}

/// `convertMessages(messages)`: the provider-shaped messages plus the folded
/// system string.
fn convert_messages(messages: Option<&Value>) -> (Vec<Value>, String) {
    let mut out: Vec<Value> = Vec::new();
    let mut system_texts: Vec<String> = Vec::new();

    let none: Vec<Value> = Vec::new();
    let list: &[Value] = match messages {
        Some(Value::Array(items)) => items,
        _ => &none,
    };

    for m in list {
        if !truthy(m) {
            continue;
        }
        let role_ = m.get("role").and_then(Value::as_str);

        if role_ == Some(role::SYSTEM) {
            let t = flatten_text(m.get("content"));
            if !t.is_empty() {
                system_texts.push(t);
            }
            continue;
        }

        if role_ == Some(role::TOOL) {
            out.push(json!({
                "role": role::TOOL,
                "content": [{
                    "type": "tool-result",
                    "toolCallId": js_string_or_empty(m.get("tool_call_id").unwrap_or(&Value::Null)),
                    "toolName": js_string_or_empty(m.get("name").unwrap_or(&Value::Null)),
                    "output": {"type": "text", "value": flatten_text(m.get("content"))},
                }],
            }));
            continue;
        }

        if role_ == Some(role::ASSISTANT) {
            let mut blocks: Vec<Value> = Vec::new();
            let rc = ["reasoning_content", "thought", "reasoning"]
                .iter()
                .find_map(|k| m.get(*k).filter(|v| truthy(v)).cloned());
            let tool_calls = m.get("tool_calls").and_then(Value::as_array);
            let has_tool_calls = tool_calls.is_some_and(|a| !a.is_empty());
            if rc.is_some() || has_tool_calls {
                // `text: rc || " "` — the raw reasoning value, not a stringified one.
                blocks.push(json!({
                    "type": "reasoning",
                    "text": rc.unwrap_or_else(|| json!(" ")),
                }));
            }
            let text = flatten_text(m.get("content"));
            if !text.is_empty() {
                blocks.push(json!({"type": openai_block::TEXT, "text": text}));
            }
            if let Some(tool_calls) = tool_calls {
                for tc in tool_calls {
                    let function = tc.get("function");
                    blocks.push(json!({
                        "type": "tool-call",
                        "toolCallId": js_string_or_empty(tc.get("id").unwrap_or(&Value::Null)),
                        "toolName": js_string_or_empty(
                            function.and_then(|f| f.get("name")).unwrap_or(&Value::Null),
                        ),
                        "input": safe_parse_json_arg(function.and_then(|f| f.get("arguments"))),
                    }));
                }
            }
            out.push(json!({
                "role": role::ASSISTANT,
                "content": if blocks.is_empty() {
                    json!([{"type": openai_block::TEXT, "text": ""}])
                } else {
                    Value::Array(blocks)
                },
            }));
            continue;
        }

        out.push(json!({"role": role::USER, "content": to_content_blocks(m.get("content"))}));
    }

    (out, system_texts.join("\n\n"))
}

/// The file-local `safeParseJson`: unlike `concerns`' helper, a `null`/absent
/// input is `{}` rather than a passthrough.
fn safe_parse_json_arg(value: Option<&Value>) -> Value {
    match value {
        None | Some(Value::Null) => json!({}),
        Some(v) => safe_parse_json(v.clone(), json!({})),
    }
}

/// `convertTools(tools)`: Anthropic plain `{name, description, input_schema}`,
/// or `undefined` when there is nothing to send.
fn convert_tools(tools: Option<&Value>) -> Option<Value> {
    let Some(Value::Array(tools)) = tools else {
        return None;
    };
    if tools.is_empty() {
        return None;
    }

    let mut result: Vec<Value> = Vec::new();
    for t in tools {
        if !truthy(t) {
            continue;
        }
        let function = t.get("function");
        if t.get("type").and_then(Value::as_str) == Some(openai_block::FUNCTION)
            && truthy_opt(function)
            && let Some(function) = function
        {
            let mut spec = Map::new();
            // `name`/`description` come from a JS object literal: an `undefined`
            // value drops the key, an explicit `null` keeps it.
            if let Some(name) = function.get("name") {
                spec.insert("name".into(), name.clone());
            }
            if let Some(description) = function.get("description") {
                spec.insert("description".into(), description.clone());
            }
            spec.insert(
                "input_schema".into(),
                function
                    .get("parameters")
                    .filter(|v| truthy(v))
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
            );
            result.push(Value::Object(spec));
        } else if truthy_opt(t.get("name"))
            && (truthy_opt(t.get("input_schema")) || truthy_opt(t.get("parameters")))
        {
            let mut spec = Map::new();
            spec.insert("name".into(), t.get("name").cloned().unwrap_or(Value::Null));
            if let Some(description) = t.get("description") {
                spec.insert("description".into(), description.clone());
            }
            spec.insert(
                "input_schema".into(),
                t.get("input_schema")
                    .filter(|v| truthy(v))
                    .cloned()
                    .or_else(|| t.get("parameters").cloned())
                    .unwrap_or(Value::Null),
            );
            result.push(Value::Object(spec));
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(Value::Array(result))
    }
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
            model: "cmc-1",
            stream: true,
            credentials: &credentials,
            meta: &mut meta,
        };
        openai_to_commandcode_request(&mut ctx, body)
    }

    #[test]
    fn the_envelope_keeps_the_expected_key_order() {
        let out = run(json!({"messages": []}));
        let keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["threadId", "memory", "config", "params"]);

        let config: Vec<&str> = out["config"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            config,
            [
                "workingDir",
                "date",
                "environment",
                "structure",
                "isGitRepo",
                "currentBranch",
                "mainBranch",
                "gitStatus",
                "recentCommits"
            ]
        );

        let params: Vec<&str> = out["params"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            params,
            ["model", "messages", "stream", "max_tokens", "temperature"]
        );
        assert_eq!(out["params"]["model"], json!("cmc-1"));
        assert_eq!(out["params"]["stream"], json!(true));
        assert_eq!(out["config"]["structure"], json!([]));
        assert_eq!(out["config"]["isGitRepo"], json!(false));
        assert_eq!(out["config"]["recentCommits"], json!([]));
        assert_eq!(out["memory"], json!(""));
    }

    #[test]
    fn system_messages_fold_into_the_top_level_string() {
        let out = run(json!({"messages": [
            {"role": "system", "content": "a"},
            {"role": "system", "content": [{"type": "text", "text": "b"}]},
            {"role": "user", "content": "hi"},
        ]}));
        assert_eq!(out["params"]["system"], json!("a\n\nb"));
        assert_eq!(out["params"]["messages"].as_array().unwrap().len(), 1);

        // No system turns means no key at all.
        let out = run(json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert!(out["params"].get("system").is_none());
    }

    #[test]
    fn a_tool_message_becomes_a_tool_result_block() {
        let out = run(json!({"messages": [
            {"role": "tool", "tool_call_id": "t1", "name": "f", "content": "42"},
        ]}));
        assert_eq!(
            out["params"]["messages"][0],
            json!({
                "role": "tool",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": "t1",
                    "toolName": "f",
                    "output": {"type": "text", "value": "42"},
                }],
            })
        );
    }

    #[test]
    fn assistant_reasoning_text_and_tool_calls_become_blocks() {
        let out = run(json!({"messages": [{
            "role": "assistant",
            "reasoning_content": "think",
            "content": "ok",
            "tool_calls": [{"id": "t1", "function": {"name": "f", "arguments": "{\"a\":1}"}}],
        }]}));
        let content = &out["params"]["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "reasoning", "text": "think"}));
        assert_eq!(content[1], json!({"type": "text", "text": "ok"}));
        assert_eq!(
            content[2],
            json!({"type": "tool-call", "toolCallId": "t1", "toolName": "f", "input": {"a": 1}})
        );
    }

    #[test]
    fn tool_calls_without_reasoning_get_a_placeholder_reasoning_block() {
        let out = run(json!({"messages": [{
            "role": "assistant",
            "tool_calls": [{"id": "t1", "function": {"name": "f", "arguments": "{}"}}],
        }]}));
        let content = &out["params"]["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "reasoning", "text": " "}));
        assert_eq!(content[1]["input"], json!({}));
    }

    #[test]
    fn an_empty_assistant_turn_still_carries_one_text_block() {
        let out = run(json!({"messages": [{"role": "assistant", "content": ""}]}));
        assert_eq!(
            out["params"]["messages"][0]["content"],
            json!([{"type": "text", "text": ""}])
        );
    }

    #[test]
    fn user_content_is_always_an_array_of_blocks() {
        let out = run(json!({"messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(
            out["params"]["messages"][0]["content"],
            json!([{"type": "text", "text": "hi"}])
        );

        let out = run(json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ]}]}));
        let content = &out["params"]["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "hi"}));
        assert_eq!(
            content[1],
            json!({"type": "image", "image": "data:image/png;base64,AAAA", "mimeType": "image/png", "mediaType": "image/png"})
        );

        // A bare `image_url` string is read the same way.
        let out = run(json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": "data:image/png;base64,AAAA"},
        ]}]}));
        assert_eq!(
            out["params"]["messages"][0]["content"][0]["type"],
            json!("image")
        );

        // A non-data URL cannot be a native image and is dropped.
        let out = run(json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}},
        ]}]}));
        assert_eq!(
            out["params"]["messages"][0]["content"],
            json!([{"type": "text", "text": ""}])
        );
    }

    #[test]
    fn a_claude_base64_source_becomes_a_native_image_block() {
        let out = run(json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
        ]}]}));
        assert_eq!(
            out["params"]["messages"][0]["content"][0],
            json!({
                "type": "image",
                "image": "data:image/jpeg;base64,BBBB",
                "mimeType": "image/jpeg",
                "mediaType": "image/jpeg",
            })
        );
    }

    #[test]
    fn tools_convert_and_empty_tools_are_omitted() {
        let out = run(json!({
            "messages": [],
            "tools": [
                {"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}},
                {"name": "g", "input_schema": {"type": "object"}},
            ],
        }));
        assert_eq!(
            out["params"]["tools"][0],
            json!({"name": "f", "description": "d", "input_schema": {"type": "object"}})
        );
        // `g` has no description: the key is dropped, not null.
        assert_eq!(
            out["params"]["tools"][1],
            json!({"name": "g", "input_schema": {"type": "object"}})
        );

        // A function tool without `parameters` defaults the schema.
        let out = run(json!({
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "h"}}],
        }));
        assert_eq!(
            out["params"]["tools"][0],
            json!({"name": "h", "input_schema": {"type": "object"}})
        );

        for tools in [json!([]), json!(null), json!("nope")] {
            let out = run(json!({"messages": [], "tools": tools}));
            assert!(
                out["params"].get("tools").is_none(),
                "{tools} must not produce tools"
            );
        }
    }

    #[test]
    fn max_tokens_temperature_and_top_p_follow_nullish_rules() {
        fn params(body: Value) -> Value {
            run(body)["params"].clone()
        }

        assert_eq!(params(json!({"messages": []}))["max_tokens"], json!(64000));
        assert_eq!(
            params(json!({"messages": [], "max_output_tokens": 200}))["max_tokens"],
            json!(200)
        );
        assert_eq!(
            params(json!({"messages": [], "max_tokens": null, "max_output_tokens": 50}))["max_tokens"],
            json!(50)
        );
        // `??` keeps an explicit 0.
        assert_eq!(
            params(json!({"messages": [], "max_tokens": 0}))["max_tokens"],
            json!(0)
        );

        assert_eq!(params(json!({"messages": []}))["temperature"], json!(0.3));
        assert_eq!(
            params(json!({"messages": [], "temperature": 0}))["temperature"],
            json!(0)
        );

        assert!(params(json!({"messages": []})).get("top_p").is_none());
        assert!(
            params(json!({"messages": [], "top_p": null}))
                .get("top_p")
                .is_none()
        );
        assert_eq!(
            params(json!({"messages": [], "top_p": 0}))["top_p"],
            json!(0)
        );
    }
}
