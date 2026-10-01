//! Tool-call id hygiene and tool-result repair.
//!
//! Two separate concerns live here. `ensure_tool_call_ids` sanitizes ids to the
//! Anthropic pattern because some providers 400 on anything else.
//! `fix_missing_tool_responses` inserts empty `role: tool` messages after an
//! assistant turn whose tool calls the next message does not answer — the
//! generic helper, which Kiro deliberately skips (it cannot consume OpenAI
//! `role: tool` messages).

use serde_json::{Map, Value, json};

use crate::translator::formats;
use crate::translator::schema::{claude_block, role};

/// Anthropic's tool_use.id pattern: `^[a-zA-Z0-9_-]+$`.
fn is_valid_tool_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `sanitizeToolId(id)`: strip every character outside the pattern; `None` when
/// nothing is left or the input is not a string.
fn sanitize_tool_id(id: Option<&Value>) -> Option<String> {
    let id = id?.as_str()?;
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `generateToolCallId(msgIndex, tcIndex, toolName)`.
pub fn generate_tool_call_id(msg_index: usize, tc_index: usize, tool_name: Option<&str>) -> String {
    let name = match tool_name.filter(|n| !n.is_empty()) {
        Some(n) => {
            let cleaned: String = n
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect();
            format!("_{cleaned}")
        }
        None => String::new(),
    };
    format!("call_msg{msg_index}_tc{tc_index}{name}")
}

/// `fallbackToolCallId(index)`. The wall-clock half is supplied by the caller
/// so this stays deterministic and testable.
pub fn fallback_tool_call_id(index: Option<usize>, now_ms: i64) -> String {
    match index {
        Some(i) => format!("call_{i}_{now_ms}"),
        None => format!("call_{now_ms}"),
    }
}

/// `ensureToolCallIds(body)`, in place.
pub fn ensure_tool_call_ids(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    for (i, msg) in messages.iter_mut().enumerate() {
        let msg_role = msg
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if msg_role == role::ASSISTANT
            && let Some(tool_calls) = msg.get_mut("tool_calls").and_then(Value::as_array_mut)
        {
            for (j, tc) in tool_calls.iter_mut().enumerate() {
                let valid = tc
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(is_valid_tool_id);
                if !valid {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let id = sanitize_tool_id(tc.get("id"))
                        .unwrap_or_else(|| generate_tool_call_id(i, j, name.as_deref()));
                    tc["id"] = json!(id);
                }
                if tc.get("type").is_none() {
                    tc["type"] = json!("function");
                }
                // Some providers require arguments to be a JSON string.
                if let Some(args) = tc.get_mut("function").and_then(|f| f.get_mut("arguments"))
                    && !args.is_string()
                    && !args.is_null()
                {
                    *args = Value::String(args.to_string());
                }
            }
        }

        if msg_role == role::TOOL
            && let Some(id) = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_string)
            && !is_valid_tool_id(&id)
        {
            let fixed = sanitize_tool_id(msg.get("tool_call_id"))
                .unwrap_or_else(|| generate_tool_call_id(i, 0, None));
            msg["tool_call_id"] = json!(fixed);
        }

        if let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) {
            for (k, block) in content.iter_mut().enumerate() {
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if block_type == claude_block::TOOL_USE {
                    let valid = block
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(is_valid_tool_id);
                    if !valid {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let id = sanitize_tool_id(block.get("id"))
                            .unwrap_or_else(|| generate_tool_call_id(i, k, name.as_deref()));
                        block["id"] = json!(id);
                    }
                }
                if block_type == claude_block::TOOL_RESULT {
                    let valid = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .is_some_and(is_valid_tool_id);
                    if !valid {
                        let id = sanitize_tool_id(block.get("tool_use_id"))
                            .unwrap_or_else(|| generate_tool_call_id(i, k, None));
                        block["tool_use_id"] = json!(id);
                    }
                }
            }
        }
    }
}

/// `getToolCallIds(msg)`: OpenAI `tool_calls[].id` and Claude `tool_use.id`.
pub fn get_tool_call_ids(msg: &Value) -> Vec<String> {
    if msg.get("role").and_then(Value::as_str) != Some(role::ASSISTANT) {
        return Vec::new();
    }
    let mut ids = Vec::new();
    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
        for tc in tool_calls {
            if let Some(id) = tc.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        }
    }
    if let Some(content) = msg.get("content").and_then(Value::as_array) {
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE)
                && let Some(id) = block.get("id").and_then(Value::as_str)
            {
                ids.push(id.to_string());
            }
        }
    }
    ids
}

/// `hasToolResults(msg, ids)`.
pub fn has_tool_results(msg: &Value, tool_call_ids: &[String]) -> bool {
    if tool_call_ids.is_empty() {
        return false;
    }
    let msg_role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
    if msg_role == role::TOOL
        && let Some(id) = msg.get("tool_call_id").and_then(Value::as_str)
    {
        return tool_call_ids.iter().any(|c| c == id);
    }
    if msg_role == role::USER
        && let Some(content) = msg.get("content").and_then(Value::as_array)
    {
        return content.iter().any(|block| {
            block.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_RESULT)
                && block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| tool_call_ids.iter().any(|c| c == id))
        });
    }
    false
}

/// `fixMissingToolResponses(body)`, in place.
pub fn fix_missing_tool_responses(body: &mut Value) {
    let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };

    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        out.push(msg.clone());
        let ids = get_tool_call_ids(msg);
        if ids.is_empty() {
            continue;
        }
        let answered = messages
            .get(i + 1)
            .is_some_and(|next| has_tool_results(next, &ids));
        if !answered {
            for id in ids {
                let mut stub = Map::new();
                stub.insert("role".into(), json!(role::TOOL));
                stub.insert("tool_call_id".into(), json!(id));
                stub.insert("content".into(), json!(""));
                out.push(Value::Object(stub));
            }
        }
    }
    body["messages"] = Value::Array(out);
}

/// `defaultClaudeToolType(tools)`: default `type: "custom"` on tools that carry
/// no truthy `type`. The spread order matters — the default is applied last so
/// a falsy `type: null` cannot overwrite it.
pub fn default_claude_tool_type(tools: &Value) -> Value {
    let Some(arr) = tools.as_array() else {
        return tools.clone();
    };
    Value::Array(
        arr.iter()
            .map(|tool| {
                let truthy = tool.get("type").is_some_and(|t| {
                    !(t.is_null() || t.as_str() == Some("") || t.as_bool() == Some(false))
                });
                if truthy {
                    tool.clone()
                } else {
                    let mut t = tool.as_object().cloned().unwrap_or_default();
                    t.insert("type".into(), json!("custom"));
                    Value::Object(t)
                }
            })
            .collect(),
    )
}

/// `shouldDefaultClaudeToolType(provider, finalFormat, tools, PROVIDERS)`: the
/// quirk is read off the built transport, which is where the dump hoists it.
pub fn should_default_claude_tool_type(
    provider: Option<&str>,
    final_format: &str,
    tools: &Value,
) -> bool {
    if final_format != formats::CLAUDE || !tools.is_array() {
        return false;
    }
    provider
        .and_then(|p| crate::providers::registry::registry().transport(p))
        .and_then(|t| t.quirks.as_ref())
        .and_then(|q| q.get("requireClaudeToolType"))
        .and_then(Value::as_bool)
        == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_sanitized_or_generated() {
        let mut body = json!({"messages": [
            {"role": "assistant", "tool_calls": [
                {"id": "bad id!", "function": {"name": "do_thing"}},
                {"id": "ok-id_1"},
            ]},
            {"role": "tool", "tool_call_id": "no/good", "content": "x"},
            {"role": "user", "content": [
                {"type": "tool_use", "id": "a b", "name": "t"},
                {"type": "tool_result", "tool_use_id": "c d"},
            ]},
        ]});
        ensure_tool_call_ids(&mut body);
        let m = &body["messages"];
        assert_eq!(m[0]["tool_calls"][0]["id"], json!("badid"), "sanitized");
        assert_eq!(
            m[0]["tool_calls"][0]["type"],
            json!("function"),
            "type defaulted"
        );
        assert_eq!(
            m[0]["tool_calls"][1]["id"],
            json!("ok-id_1"),
            "valid id kept"
        );
        assert_eq!(m[1]["tool_call_id"], json!("nogood"));
        assert_eq!(m[2]["content"][0]["id"], json!("ab"));
        assert_eq!(m[2]["content"][1]["tool_use_id"], json!("cd"));
    }

    #[test]
    fn arguments_are_stringified_only_when_not_already_a_string() {
        let mut body = json!({"messages": [
            {"role": "assistant", "tool_calls": [{"id": "x", "function": {"arguments": {"a": 1}}}]},
        ]});
        ensure_tool_call_ids(&mut body);
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            json!("{\"a\":1}")
        );
    }

    #[test]
    fn missing_tool_responses_are_inserted_per_id() {
        let mut body = json!({"messages": [
            {"role": "assistant", "tool_calls": [{"id": "a"}, {"id": "b"}]},
            {"role": "user", "content": "hi"},
        ]});
        fix_missing_tool_responses(&mut body);
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[1]["role"], json!("tool"));
        assert_eq!(m[1]["tool_call_id"], json!("a"));
        assert_eq!(m[2]["tool_call_id"], json!("b"));
        assert_eq!(m[2]["content"], json!(""));
    }

    #[test]
    fn an_answered_tool_call_is_left_alone() {
        let mut body = json!({"messages": [
            {"role": "assistant", "tool_calls": [{"id": "a"}]},
            {"role": "tool", "tool_call_id": "a", "content": "r"},
        ]});
        fix_missing_tool_responses(&mut body);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn claude_tool_result_in_user_content_counts_as_answered() {
        let mut body = json!({"messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a"}]},
        ]});
        fix_missing_tool_responses(&mut body);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn default_claude_tool_type_only_fills_falsy_types() {
        let tools = json!([
            {"name": "a"},
            {"name": "b", "type": "computer_use"},
            {"name": "c", "type": null},
            {"name": "d", "type": ""},
        ]);
        let out = default_claude_tool_type(&tools);
        assert_eq!(out[0]["type"], json!("custom"));
        assert_eq!(
            out[1]["type"],
            json!("computer_use"),
            "truthy type untouched"
        );
        assert_eq!(
            out[2]["type"],
            json!("custom"),
            "null cannot survive the default"
        );
        assert_eq!(out[3]["type"], json!("custom"));
    }

    #[test]
    fn should_default_requires_the_quirk_provider_and_claude_format() {
        // No kept provider declares `requireClaudeToolType` — MiniMax, the only
        // one that did, left with the provider set — so the quirk never fires.
        for provider in ["deepseek", "claude", "mistral", "opencode"] {
            assert!(!should_default_claude_tool_type(
                Some(provider),
                formats::CLAUDE,
                &json!([])
            ));
        }
        assert!(!should_default_claude_tool_type(
            None,
            formats::CLAUDE,
            &json!([])
        ));
        assert!(!should_default_claude_tool_type(
            Some("deepseek"),
            formats::OPENAI,
            &json!([])
        ));
        assert!(!should_default_claude_tool_type(
            Some("deepseek"),
            formats::CLAUDE,
            &json!("x")
        ));
    }
}
