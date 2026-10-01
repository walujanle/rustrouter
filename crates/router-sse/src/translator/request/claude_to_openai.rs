//! Claude → OpenAI request.
//!
//! The direct route's forward leg. Two things here are not obvious:
//!
//! - A mid-conversation `system` message becomes a **user** turn wrapped in
//!   `<instructions>` tags. Anthropic rejects a system block after the first
//!   turn, and folding it into the previous assistant turn would be read as
//!   prefill.
//! - `fixMissingToolResponsesOpenAI` is the *local* variant, not
//!   `concerns/toolCall`'s. It scans contiguous tool replies after each
//!   assistant turn and back-fills `[No response received]`; the global one
//!   only looks at the immediately-next message. Both run, on different legs.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::translator::RequestContext;
use crate::translator::concerns::image::encode_data_uri;
use crate::translator::concerns::primitives::{
    collapse_text_parts, js_string_or_empty, js_truthy as truthy,
};
use crate::translator::formats::max_tokens::adjust_max_tokens;
use crate::translator::schema::{claude_block, openai_block, role};

static BILLING_HEADER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)^x-anthropic-billing-header:[^\n]*(?:\r?\n)?").unwrap());

/// `stripAnthropicBillingHeader(text)`: a non-string becomes `""`.
fn strip_anthropic_billing_header(text: &Value) -> String {
    let Some(text) = text.as_str() else {
        return String::new();
    };
    BILLING_HEADER.replace(text, "").to_string()
}

/// `claudeToOpenAIRequest(model, body, stream)`.
pub fn claude_to_openai_request(ctx: &mut RequestContext<'_>, body: Value) -> Value {
    let mut result = Map::new();
    result.insert("model".into(), json!(ctx.model));
    result.insert("messages".into(), json!([]));
    result.insert("stream".into(), json!(ctx.stream));

    if body.get("max_tokens").is_some_and(|v| !v.is_null()) {
        result.insert(
            "max_tokens".into(),
            json!(adjust_max_tokens(
                &body,
                crate::runtime_config::DEFAULT_MAX_TOKENS
            )),
        );
    }

    if let Some(temperature) = body.get("temperature") {
        result.insert("temperature".into(), temperature.clone());
    }

    if let Some(system) = body.get("system").filter(|v| truthy(v)) {
        let system_content = match system {
            Value::Array(items) => items
                .iter()
                .map(|s| strip_anthropic_billing_header(s.get("text").unwrap_or(&Value::Null)))
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            other => strip_anthropic_billing_header(other),
        };
        if !system_content.is_empty() {
            push_message(
                &mut result,
                json!({"role": role::SYSTEM, "content": system_content}),
            );
        }
    }

    if let Some(Value::Array(messages)) = body.get("messages") {
        for msg in messages {
            for converted in convert_claude_message(msg) {
                push_message(&mut result, converted);
            }
        }
    }

    // OpenAI requires a response for every tool_call. The local variant scans
    // contiguous tool replies and back-fills.
    fix_missing_tool_responses_openai(&mut result);

    if let Some(Value::Array(tools)) = body.get("tools") {
        let mapped: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": openai_block::FUNCTION,
                    "function": {
                        "name": tool.get("name"),
                        "description": tool.get("description").map(js_string_or_empty).unwrap_or_default(),
                        "parameters": tool.get("input_schema").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                    },
                })
            })
            .collect();
        result.insert("tools".into(), Value::Array(mapped));
    }

    if let Some(tool_choice) = body.get("tool_choice") {
        result.insert("tool_choice".into(), convert_tool_choice(tool_choice));
    }

    if let Some(effort) = body.get("reasoning_effort") {
        result.insert("reasoning_effort".into(), effort.clone());
    } else if let Some(effort) = body.get("reasoning").and_then(|r| r.get("effort")) {
        result.insert("reasoning_effort".into(), effort.clone());
    }

    if let Some(reasoning) = body.get("reasoning") {
        result.insert("reasoning".into(), reasoning.clone());
    }

    Value::Object(result)
}

fn push_message(result: &mut Map<String, Value>, msg: Value) {
    if let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) {
        messages.push(msg);
    }
}

/// `fixMissingToolResponsesOpenAI(messages)`.
fn fix_missing_tool_responses_openai(result: &mut Map<String, Value>) {
    let Some(messages) = result.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    let mut i = 0;
    while i < messages.len() {
        let is_assistant_with_calls = messages[i].get("role").and_then(Value::as_str)
            == Some(role::ASSISTANT)
            && messages[i]
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|c| !c.is_empty());

        if !is_assistant_with_calls {
            i += 1;
            continue;
        }

        let tool_call_ids: Vec<Value> = messages[i]
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|tc| tc.get("id").cloned())
                    .collect()
            })
            .unwrap_or_default();

        let mut responded: Vec<Value> = Vec::new();
        let mut insert_position = i + 1;
        let mut j = i + 1;
        while j < messages.len() {
            let next = &messages[j];
            let is_tool_reply = next.get("role").and_then(Value::as_str) == Some(role::TOOL)
                && truthy(next.get("tool_call_id").unwrap_or(&Value::Null));
            if !is_tool_reply {
                break;
            }
            if let Some(id) = next.get("tool_call_id") {
                responded.push(id.clone());
            }
            insert_position = j + 1;
            j += 1;
        }

        let missing: Vec<Value> = tool_call_ids
            .into_iter()
            .filter(|id| !responded.contains(id))
            .collect();

        if !missing.is_empty() {
            let missing_responses: Vec<Value> = missing
                .iter()
                .map(|id| {
                    json!({
                        "role": role::TOOL,
                        "tool_call_id": id,
                        "content": "[No response received]",
                    })
                })
                .collect();
            let count = missing_responses.len();
            for (offset, response) in missing_responses.into_iter().enumerate() {
                messages.insert(insert_position + offset, response);
            }
            i = insert_position + count - 1;
        }
        i += 1;
    }
}

/// `systemReminderText(content)`.
fn system_reminder_text(content: &Value) -> String {
    let parts: Vec<String> = match content {
        Value::Array(items) => items
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some(claude_block::TEXT))
            .map(|c| c.get("text").map(js_string_or_empty).unwrap_or_default())
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => vec![String::new()],
    };
    let text = parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return String::new();
    }
    format!("<instructions>\n{text}\n</instructions>")
}

/// `convertClaudeMessage(msg)`: zero, one or many OpenAI messages.
fn convert_claude_message(msg: &Value) -> Vec<Value> {
    // Some clients send content as a single block object. Normalizing must run
    // before the role branch: `systemReminderText` only reads arrays and
    // strings, so a bare-object system turn would otherwise be dropped.
    let mut msg = msg.clone();
    if msg.get("content").is_some_and(|c| c.is_object()) {
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("content".into(), Value::Array(vec![content]));
        }
    }

    if msg.get("role").and_then(Value::as_str) == Some(role::SYSTEM) {
        let text = system_reminder_text(msg.get("content").unwrap_or(&Value::Null));
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![json!({"role": role::USER, "content": text})]
        };
    }

    let role_ = match msg.get("role").and_then(Value::as_str) {
        Some(role::USER) | Some(role::TOOL) => role::USER,
        _ => role::ASSISTANT,
    };

    match msg.get("content") {
        Some(Value::String(s)) => return vec![json!({"role": role_, "content": s})],
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<Value> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_results: Vec<Value> = Vec::new();

            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some(claude_block::TEXT) => {
                        parts.push(json!({"type": openai_block::TEXT, "text": block.get("text")}));
                    }
                    Some(claude_block::IMAGE) => {
                        let source = block.get("source");
                        if source.and_then(|s| s.get("type")).and_then(Value::as_str)
                            == Some("base64")
                        {
                            let media_type = source
                                .and_then(|s| s.get("media_type"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let data = source
                                .and_then(|s| s.get("data"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            parts.push(json!({
                                "type": openai_block::IMAGE_URL,
                                "image_url": {"url": encode_data_uri(media_type, data)},
                            }));
                        }
                    }
                    Some(claude_block::TOOL_USE) => {
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        let arguments = if input.is_null() {
                            "{}".to_string()
                        } else {
                            serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                        };
                        tool_calls.push(json!({
                            "id": block.get("id"),
                            "type": openai_block::FUNCTION,
                            "function": {"name": block.get("name"), "arguments": arguments},
                        }));
                    }
                    Some(claude_block::TOOL_RESULT) => {
                        let mut result_content = String::new();
                        let mut result_images: Vec<Value> = Vec::new();
                        match block.get("content") {
                            Some(Value::String(s)) => result_content = s.clone(),
                            Some(Value::Array(parts_in)) => {
                                for c in parts_in {
                                    if c.get("type").and_then(Value::as_str)
                                        == Some(claude_block::IMAGE)
                                        && c.get("source")
                                            .and_then(|s| s.get("type"))
                                            .and_then(Value::as_str)
                                            == Some("base64")
                                    {
                                        let source = c.get("source");
                                        let media_type = source
                                            .and_then(|s| s.get("media_type"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        let data = source
                                            .and_then(|s| s.get("data"))
                                            .and_then(Value::as_str)
                                            .unwrap_or("");
                                        result_images.push(json!({
                                            "type": openai_block::IMAGE_URL,
                                            "image_url": {"url": encode_data_uri(media_type, data)},
                                        }));
                                    }
                                }
                                let text_only: Vec<&Value> = parts_in
                                    .iter()
                                    .filter(|c| {
                                        c.get("type").and_then(Value::as_str)
                                            == Some(claude_block::TEXT)
                                    })
                                    .collect();
                                let joined = text_only
                                    .iter()
                                    .map(|c| {
                                        c.get("text").map(js_string_or_empty).unwrap_or_default()
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                result_content = if !joined.is_empty() {
                                    joined
                                } else if !result_images.is_empty() {
                                    String::new()
                                } else {
                                    serde_json::to_string(parts_in)
                                        .unwrap_or_else(|_| "[]".to_string())
                                };
                            }
                            Some(other) if truthy(other) => {
                                result_content = serde_json::to_string(other).unwrap_or_default();
                            }
                            _ => {}
                        }

                        tool_results.push(json!({
                            "role": role::TOOL,
                            "tool_call_id": block.get("tool_use_id"),
                            "content": result_content,
                        }));
                        // The OpenAI tool role is text-only; an image a tool
                        // returned is handed to the model in the following user
                        // turn, tagged with the call it came from.
                        if !result_images.is_empty() {
                            parts.push(json!({
                                "type": openai_block::TEXT,
                                "text": format!("[Image from tool result {}]", block.get("tool_use_id").and_then(Value::as_str).unwrap_or("")),
                            }));
                            parts.extend(result_images);
                        }
                    }
                    _ => {}
                }
            }

            if !tool_results.is_empty() {
                if !parts.is_empty() {
                    let mut out = tool_results;
                    out.push(json!({"role": role::USER, "content": collapse_text_parts(Value::Array(parts))}));
                    return out;
                }
                return tool_results;
            }

            if !tool_calls.is_empty() {
                let mut out = Map::new();
                out.insert("role".into(), json!(role::ASSISTANT));
                if !parts.is_empty() {
                    out.insert("content".into(), collapse_text_parts(Value::Array(parts)));
                }
                out.insert("tool_calls".into(), Value::Array(tool_calls));
                return vec![Value::Object(out)];
            }

            if !parts.is_empty() {
                return vec![
                    json!({"role": role_, "content": collapse_text_parts(Value::Array(parts))}),
                ];
            }

            if blocks.is_empty() {
                return vec![json!({"role": role_, "content": ""})];
            }
        }
        _ => {}
    }

    Vec::new()
}

/// `convertToolChoice(choice)`.
fn convert_tool_choice(choice: &Value) -> Value {
    if !truthy(choice) {
        return json!("auto");
    }
    if choice.is_string() {
        return choice.clone();
    }
    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("tool") => json!({
            "type": openai_block::FUNCTION,
            "function": {"name": choice.get("name")},
        }),
        _ => json!("auto"),
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
            model: "m",
            stream: true,
            credentials: &credentials,
            meta: &mut meta,
        };
        claude_to_openai_request(&mut ctx, body)
    }

    #[test]
    fn billing_header_is_stripped_from_system() {
        let out = run(json!({
            "system": "x-anthropic-billing-header: acct=1\nreal prompt",
            "messages": [],
        }));
        assert_eq!(out["messages"][0]["content"], json!("real prompt"));
        // A system array joins the stripped parts.
        let out = run(json!({
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "x-anthropic-billing-header: z\nb"}],
            "messages": [],
        }));
        assert_eq!(out["messages"][0]["content"], json!("a\nb"));
    }

    #[test]
    fn a_mid_conversation_system_message_becomes_an_instructions_user_turn() {
        let out = run(json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": "be terse"},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], json!("user"));
        assert_eq!(
            msgs[1]["content"],
            json!("<instructions>\nbe terse\n</instructions>")
        );
    }

    #[test]
    fn a_bare_object_system_content_is_still_folded() {
        let out = run(json!({
            "messages": [{"role": "system", "content": {"type": "text", "text": "note"}}],
        }));
        assert_eq!(
            out["messages"][0]["content"],
            json!("<instructions>\nnote\n</instructions>")
        );
    }

    #[test]
    fn tool_use_and_tool_result_split_into_the_openai_shape() {
        let out = run(json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "42"},
                ]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "ok"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 1}},
                ]},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0],
            json!({"role": "tool", "tool_call_id": "t1", "content": "42"})
        );
        assert_eq!(msgs[1]["role"], json!("assistant"));
        assert_eq!(msgs[1]["content"], json!("ok"));
        assert_eq!(
            msgs[1]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn a_missing_tool_response_is_back_filled() {
        let out = run(json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "g", "input": {}},
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "one"}]},
            ],
        }));
        let msgs = out["messages"].as_array().unwrap();
        // t1's real reply, then the synthesized t2 reply.
        assert_eq!(msgs[1]["tool_call_id"], json!("t1"));
        assert_eq!(msgs[2]["tool_call_id"], json!("t2"));
        assert_eq!(msgs[2]["content"], json!("[No response received]"));
    }

    #[test]
    fn tool_choice_shapes() {
        assert_eq!(convert_tool_choice(&json!("auto")), json!("auto"));
        assert_eq!(
            convert_tool_choice(&json!({"type": "any"})),
            json!("required")
        );
        assert_eq!(
            convert_tool_choice(&json!({"type": "tool", "name": "f"})),
            json!({"type": "function", "function": {"name": "f"}})
        );
        assert_eq!(convert_tool_choice(&Value::Null), json!("auto"));
    }

    #[test]
    fn max_tokens_tools_and_reasoning_are_carried_over() {
        let out = run(json!({
            "max_tokens": 100,
            "temperature": 0.5,
            "tools": [{"name": "f", "description": "d", "input_schema": {"type": "object"}}],
            "reasoning_effort": "high",
            "messages": [],
        }));
        // A tools array raises max_tokens to the minimum.
        assert_eq!(
            out["max_tokens"],
            json!(crate::runtime_config::DEFAULT_MIN_TOKENS)
        );
        assert_eq!(out["temperature"], json!(0.5));
        assert_eq!(out["tools"][0]["type"], json!("function"));
        assert_eq!(out["tools"][0]["function"]["name"], json!("f"));
        assert_eq!(out["reasoning_effort"], json!("high"));
    }

    #[test]
    fn a_non_string_system_is_not_a_message() {
        let out = run(json!({"system": 7, "messages": []}));
        assert_eq!(out["messages"].as_array().unwrap().len(), 0);
    }
}
