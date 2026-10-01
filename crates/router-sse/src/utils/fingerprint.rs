//! OpenCode free-tier client fingerprint.
//!
//! The upstream gate requires the lowercase file-search quartet
//! (`bash/glob/grep/read`). Agent clients declare the same tools in other
//! casings, so those variants are renamed to the canonical spelling and
//! duplicates removed; the response side restores the caller's original
//! spelling.
//!
//! The rename map is keyed on the request body object. A Rust `Value` has no
//! identity to key on, so `apply_fingerprint_tools` returns the map and the
//! caller threads it to `translate_response` through the stream state.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::translator::schema::{claude_block, openai_block};

/// `OPENCODE_FINGERPRINT_TOOLS`.
pub const OPENCODE_FINGERPRINT_TOOLS: [&str; 4] = ["bash", "glob", "grep", "read"];

/// The rename map: sent (canonical) name → caller's original name.
pub type ToolNameMap = HashMap<String, String>;

/// `fingerprintToolKey(name)`: the canonical lowercase name, or `""`.
pub fn fingerprint_tool_key(name: Option<&str>) -> &'static str {
    let lower = name.unwrap_or("").trim().to_ascii_lowercase();
    OPENCODE_FINGERPRINT_TOOLS
        .iter()
        .find(|t| **t == lower)
        .copied()
        .unwrap_or("")
}

/// `toolNameOf(tool)`: `name` from the flat shape, else `function.name`.
fn tool_name_of(tool: &Value) -> String {
    let Some(obj) = tool.as_object() else {
        return String::new();
    };
    if let Some(name) = obj.get("name").and_then(Value::as_str) {
        let t = name.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(name) = obj
        .get("function")
        .and_then(Value::as_object)
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
    {
        return name.trim().to_string();
    }
    String::new()
}

/// `concealFingerprintToolNames(tools)`.
pub fn conceal_fingerprint_tool_names(tools: Option<&Value>) -> (Vec<Value>, ToolNameMap) {
    let mut map = ToolNameMap::new();
    let Some(arr) = tools.and_then(Value::as_array) else {
        return (Vec::new(), map);
    };
    if arr.is_empty() {
        return (Vec::new(), map);
    }

    let mut seen_quartet: HashSet<&'static str> = HashSet::new();
    let mut out: Vec<Value> = Vec::with_capacity(arr.len());
    for tool in arr {
        let Some(obj) = tool.as_object() else {
            out.push(tool.clone());
            continue;
        };
        let current = tool_name_of(tool);
        let key = fingerprint_tool_key(Some(&current));
        if key.is_empty() {
            out.push(tool.clone());
            continue;
        }
        // `Bash` + `bash` is rejected upstream as a duplicate.
        if !seen_quartet.insert(key) {
            continue;
        }
        if current != key {
            map.insert(key.to_string(), current.clone());
            let mut renamed = obj.clone();
            if let Some(function) = obj.get("function").and_then(Value::as_object) {
                let mut f = function.clone();
                f.insert("name".into(), json!(key));
                renamed.insert("function".into(), Value::Object(f));
            } else {
                renamed.insert("name".into(), json!(key));
            }
            out.push(Value::Object(renamed));
        } else {
            out.push(tool.clone());
        }
    }
    (out, map)
}

/// `appendMissingFingerprintTools(tools, flat)`.
pub fn append_missing_fingerprint_tools(tools: &mut Vec<Value>, flat: bool) {
    for name in OPENCODE_FINGERPRINT_TOOLS {
        let present = tools
            .iter()
            .any(|t| fingerprint_tool_key(Some(&tool_name_of(t))) == name);
        if present {
            continue;
        }
        tools.push(if flat {
            json!({
                "type": "function",
                "name": name,
                "description": "This tool is currently unavailable and must not be used.",
                "parameters": {"type": "object", "properties": {}},
            })
        } else {
            json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": "This tool is currently unavailable and must not be used.",
                    "parameters": {"type": "object", "properties": {}},
                }
            })
        });
    }
}

/// `retargetToolChoice(body, map)`.
pub fn retarget_tool_choice(body: &mut Value, map: &ToolNameMap) {
    if map.is_empty() {
        return;
    }
    let Some(choice) = body.get("tool_choice").and_then(Value::as_object).cloned() else {
        return;
    };

    if let Some(name) = choice.get("name").and_then(Value::as_str) {
        let key = fingerprint_tool_key(Some(name));
        if !key.is_empty() && map.contains_key(key) {
            let mut c = choice;
            c.insert("name".into(), json!(key));
            body["tool_choice"] = Value::Object(c);
        }
        return;
    }

    if let Some(function) = choice.get("function").and_then(Value::as_object)
        && let Some(name) = function.get("name").and_then(Value::as_str)
    {
        let key = fingerprint_tool_key(Some(name));
        if !key.is_empty() && map.contains_key(key) {
            let mut f = function.clone();
            f.insert("name".into(), json!(key));
            let mut c = choice;
            c.insert("function".into(), Value::Object(f));
            body["tool_choice"] = Value::Object(c);
        }
    }
}

/// `applyFingerprintTools(body, flat)`: the full request-side pass. Returns the
/// rename map.
pub fn apply_fingerprint_tools(body: &mut Value, flat: bool) -> ToolNameMap {
    if !body.is_object() {
        return ToolNameMap::new();
    }
    let had_client_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());

    let (mut tools, map) = conceal_fingerprint_tool_names(body.get("tools"));
    append_missing_fingerprint_tools(&mut tools, flat);
    body["tools"] = Value::Array(tools);
    retarget_tool_choice(body, &map);

    let has_choice = body.get("tool_choice").is_some_and(|v| !v.is_null());
    if !has_choice {
        if flat {
            body["tool_choice"] = json!("auto");
        } else if !had_client_tools {
            body["tool_choice"] = json!("none");
        }
    }

    record_renamed_tool_names(body, &map);
    map
}

/// The body key the rename map rides on between the executor's
/// `applyFingerprintTools` pass and the orchestrator's `takeRenamedToolNames`
/// call.
///
/// The map is keyed on the body object; a Rust `Value` has no identity, so the
/// map travels in the body under a key no upstream accepts.
/// [`crate::executors::executor`] strips it before serializing the wire body.
pub const RENAMED_TOOL_NAMES_FIELD: &str = "_renamedToolNames";

/// `recordRenamedToolNames(body, map)`: stash the map for a later
/// [`take_renamed_tool_names`].
pub fn record_renamed_tool_names(body: &mut Value, map: &ToolNameMap) {
    if map.is_empty() {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            RENAMED_TOOL_NAMES_FIELD.into(),
            Value::Object(map.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
        );
    }
}

/// `takeRenamedToolNames(body)`: read and clear the stashed map.
pub fn take_renamed_tool_names(body: &mut Value) -> ToolNameMap {
    let Some(obj) = body.as_object_mut() else {
        return ToolNameMap::new();
    };
    let Some(stashed) = obj.shift_remove(RENAMED_TOOL_NAMES_FIELD) else {
        return ToolNameMap::new();
    };
    stashed
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// `restoreToolNames(payload, map)` — restore caller spellings in every
/// supported response/event shape.
pub fn restore_tool_names(payload: &mut Value, map: Option<&ToolNameMap>) {
    let Some(map) = map.filter(|m| !m.is_empty()) else {
        return;
    };
    restore_inner(payload, map);
}

fn restore_inner(payload: &mut Value, map: &ToolNameMap) {
    if let Value::Array(items) = payload {
        for item in items {
            restore_inner(item, map);
        }
        return;
    }
    if !payload.is_object() {
        return;
    }

    // Claude streaming content_block_start event.
    if payload.get("type").and_then(Value::as_str) == Some("content_block_start") {
        let rename = payload
            .get("content_block")
            .and_then(Value::as_object)
            .filter(|b| b.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE))
            .and_then(|b| b.get("name").and_then(Value::as_str))
            .and_then(|n| map.get(n).cloned());
        if let Some(orig) = rename {
            let mut block = payload["content_block"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            block.insert("name".into(), Value::String(orig));
            payload["content_block"] = Value::Object(block);
        }
    }

    // Claude non-streaming message body.
    if let Some(content) = payload.get("content").and_then(Value::as_array).cloned() {
        let mapped: Vec<Value> = content
            .into_iter()
            .map(|mut block| {
                if block.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE)
                    && let Some(name) = block.get("name").and_then(Value::as_str)
                    && let Some(orig) = map.get(name)
                {
                    block["name"] = Value::String(orig.clone());
                }
                block
            })
            .collect();
        payload["content"] = Value::Array(mapped);
    }

    // OpenAI Chat Completions, streaming delta and JSON message shapes.
    if let Some(choices) = payload.get("choices").and_then(Value::as_array).cloned() {
        let mapped: Vec<Value> = choices
            .into_iter()
            .map(|choice| {
                let mut next = choice.clone();
                for holder in ["delta", "message"] {
                    let Some(value) = choice.get(holder).and_then(Value::as_object) else {
                        continue;
                    };
                    let Some(calls) = value.get("tool_calls").and_then(Value::as_array) else {
                        continue;
                    };
                    if calls.is_empty() {
                        continue;
                    }
                    let calls: Vec<Value> = calls
                        .iter()
                        .map(|call| {
                            let Some(name) = call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                            else {
                                return call.clone();
                            };
                            let Some(orig) = map.get(name) else {
                                return call.clone();
                            };
                            let mut c = call.clone();
                            let mut f = c["function"].as_object().cloned().unwrap_or_default();
                            f.insert("name".into(), Value::String(orig.clone()));
                            c["function"] = Value::Object(f);
                            c
                        })
                        .collect();
                    let mut v = value.clone();
                    v.insert("tool_calls".into(), Value::Array(calls));
                    next[holder] = Value::Object(v);
                }
                next
            })
            .collect();
        payload["choices"] = Value::Array(mapped);
    }

    // OpenAI Responses final JSON body.
    if let Some(output) = payload.get("output").and_then(Value::as_array).cloned() {
        let mapped: Vec<Value> = output
            .into_iter()
            .map(|mut item| {
                if item.get("type").and_then(Value::as_str) == Some("function_call")
                    && let Some(name) = item.get("name").and_then(Value::as_str)
                    && let Some(orig) = map.get(name)
                {
                    item["name"] = Value::String(orig.clone());
                }
                item
            })
            .collect();
        payload["output"] = Value::Array(mapped);
    }

    // OpenAI Responses SSE events such as response.output_item.added/done.
    let rename = payload
        .get("item")
        .and_then(Value::as_object)
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("function_call"))
        .and_then(|i| i.get("name").and_then(Value::as_str))
        .and_then(|n| map.get(n).cloned());
    if let Some(orig) = rename {
        let mut item = payload["item"].as_object().cloned().unwrap_or_default();
        item.insert("name".into(), Value::String(orig));
        payload["item"] = Value::Object(item);
    }
}

/// `OPENAI_BLOCK` re-export so callers reading the tool shape do not need the
/// schema import.
pub use openai_block::FUNCTION as OPENAI_FUNCTION_TYPE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quartet_is_canonicalised_and_deduped() {
        let tools = json!([
            {"type": "function", "function": {"name": "Bash"}},
            {"type": "function", "function": {"name": "bash"}},
            {"type": "function", "function": {"name": "MyTool"}},
        ]);
        let (out, map) = conceal_fingerprint_tool_names(Some(&tools));
        // One "Bash" renamed to "bash"; the second is dropped as a duplicate.
        let names: Vec<&str> = out
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["bash", "MyTool"]);
        assert_eq!(map.get("bash"), Some(&"Bash".to_string()));
    }

    #[test]
    fn flat_and_chat_tool_shapes_both_rename() {
        let flat = json!([{"type": "function", "name": "Glob"}]);
        let (out, map) = conceal_fingerprint_tool_names(Some(&flat));
        assert_eq!(out[0]["name"], json!("glob"));
        assert_eq!(map.get("glob"), Some(&"Glob".to_string()));

        let chat = json!([{"type": "function", "function": {"name": "Grep"}}]);
        let (out, _) = conceal_fingerprint_tool_names(Some(&chat));
        assert_eq!(out[0]["function"]["name"], json!("grep"));
    }

    #[test]
    fn missing_members_are_appended_in_the_right_shape() {
        let mut tools = vec![json!({"type": "function", "function": {"name": "bash"}})];
        append_missing_fingerprint_tools(&mut tools, false);
        assert_eq!(tools.len(), 4);
        assert_eq!(tools[1]["function"]["name"], json!("glob"));
        assert_eq!(tools[1]["type"], json!("function"));

        let mut flat = vec![json!({"type": "function", "name": "read"})];
        append_missing_fingerprint_tools(&mut flat, true);
        assert_eq!(flat.len(), 4);
        assert!(
            flat[1].get("function").is_none(),
            "flat shape has no function wrapper"
        );
        assert_eq!(flat[1]["name"], json!("bash"));
    }

    #[test]
    fn apply_sets_legacy_tool_choice_defaults() {
        let mut flat = json!({});
        apply_fingerprint_tools(&mut flat, true);
        assert_eq!(flat["tool_choice"], json!("auto"));

        // A chat request with no caller tools defaults to "none".
        let mut chat = json!({});
        apply_fingerprint_tools(&mut chat, false);
        assert_eq!(chat["tool_choice"], json!("none"));

        // A chat request that had caller tools keeps no forced choice.
        let mut chat = json!({"tools": [{"type": "function", "function": {"name": "MyTool"}}]});
        apply_fingerprint_tools(&mut chat, false);
        assert!(chat.get("tool_choice").is_none());
    }

    #[test]
    fn retarget_tool_choice_points_at_the_canonical_name() {
        let mut map = ToolNameMap::new();
        map.insert("bash".into(), "Bash".into());

        let mut body = json!({"tool_choice": {"type": "tool", "name": "Bash"}});
        retarget_tool_choice(&mut body, &map);
        assert_eq!(body["tool_choice"]["name"], json!("bash"));

        let mut body = json!({"tool_choice": {"type": "function", "function": {"name": "Bash"}}});
        retarget_tool_choice(&mut body, &map);
        assert_eq!(body["tool_choice"]["function"]["name"], json!("bash"));

        // A name outside the quartet is left alone.
        let mut body = json!({"tool_choice": {"type": "tool", "name": "MyTool"}});
        retarget_tool_choice(&mut body, &map);
        assert_eq!(body["tool_choice"]["name"], json!("MyTool"));
    }

    #[test]
    fn restore_covers_claude_and_openai_shapes() {
        let mut map = ToolNameMap::new();
        map.insert("bash".into(), "Bash".into());

        let mut chunk = json!({"type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "bash"}});
        restore_tool_names(&mut chunk, Some(&map));
        assert_eq!(chunk["content_block"]["name"], json!("Bash"));

        let mut chat = json!({"choices": [{"delta": {"tool_calls": [
            {"function": {"name": "bash", "arguments": "{}"}}]}}]});
        restore_tool_names(&mut chat, Some(&map));
        assert_eq!(
            chat["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            json!("Bash")
        );

        let mut responses = json!({"output": [{"type": "function_call", "name": "bash"}]});
        restore_tool_names(&mut responses, Some(&map));
        assert_eq!(responses["output"][0]["name"], json!("Bash"));

        let mut event = json!({"type": "response.output_item.added",
            "item": {"type": "function_call", "name": "bash"}});
        restore_tool_names(&mut event, Some(&map));
        assert_eq!(event["item"]["name"], json!("Bash"));

        let mut body = json!({"content": [{"type": "tool_use", "name": "bash"}]});
        restore_tool_names(&mut body, Some(&map));
        assert_eq!(body["content"][0]["name"], json!("Bash"));
    }

    #[test]
    fn restore_is_a_noop_without_a_map() {
        let mut payload = json!({"content": [{"type": "tool_use", "name": "bash"}]});
        let before = payload.clone();
        restore_tool_names(&mut payload, None);
        assert_eq!(payload, before);
        restore_tool_names(&mut payload, Some(&ToolNameMap::new()));
        assert_eq!(payload, before);
    }
}
