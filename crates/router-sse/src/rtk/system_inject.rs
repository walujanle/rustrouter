//! Append an instruction into the system message of the final request body,
//! dispatching by format so it works for translated and native-passthrough
//! flows.
//!
//! Every branch bails silently: only the exact node found is touched, so a
//! shape that does not match is a no-op.

use serde_json::{Map, Value};

use crate::translator::formats as f;
use crate::translator::schema::{claude_block, openai_block, responses_item, role};

const SEP: &str = "\n\n";

/// Append `prompt` into the system slot the `format` shape uses.
pub fn inject_system_prompt(body: &mut Value, format: &str, prompt: &str) {
    if prompt.is_empty() || !body.is_object() {
        return;
    }

    if is_kiro_body(body) || format == f::KIRO {
        inject_kiro_system(body, prompt);
        return;
    }
    if format == f::CLAUDE {
        inject_claude_system(body, prompt);
        return;
    }
    if matches!(format, f::GEMINI | f::GEMINI_CLI | f::VERTEX) {
        inject_gemini_system(body, prompt);
        return;
    }

    // OpenAI-shaped formats: dispatch by wire shape. `instructions` string takes
    // precedence; `messages[]` means Chat; `input[]` means Responses.
    if body.get("instructions").is_some_and(Value::is_string) {
        inject_instructions_system(body, prompt);
        return;
    }
    if body.get("messages").is_some_and(Value::is_array) {
        inject_chat_system(body, prompt);
        return;
    }
    if body.get("input").is_some_and(Value::is_array) {
        inject_responses_input_system(body, prompt);
    }
    // A string `input` must stay untouched.
}

fn is_kiro_body(body: &Value) -> bool {
    let Some(cs) = body.get("conversationState").and_then(Value::as_object) else {
        return false;
    };
    let history_turn = cs
        .get("history")
        .and_then(Value::as_array)
        .is_some_and(|h| {
            h.iter().any(|it| {
                it.get("userInputMessage").is_some() || it.get("assistantResponseMessage").is_some()
            })
        });
    history_turn
        || cs
            .get("currentMessage")
            .and_then(|c| c.get("userInputMessage"))
            .is_some()
}

/// Exact idempotency: prompt present as its own SEP-delimited segment (or the
/// whole string), not as a substring of unrelated text.
fn has_prompt(haystack: &str, prompt: &str) -> bool {
    haystack == prompt || haystack.split(SEP).any(|s| s == prompt)
}

fn dedup_string_append(curr: &str, prompt: &str) -> String {
    if curr.is_empty() {
        return prompt.to_string();
    }
    if has_prompt(curr, prompt) {
        return curr.to_string();
    }
    format!("{curr}{SEP}{prompt}")
}

// ---- OpenAI instructions string ----
fn inject_instructions_system(body: &mut Value, prompt: &str) {
    let Some(curr) = body.get("instructions").and_then(Value::as_str) else {
        return;
    };
    if has_prompt(curr, prompt) {
        return;
    }
    let next = if curr.is_empty() {
        prompt.to_string()
    } else {
        format!("{curr}{SEP}{prompt}")
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert("instructions".to_string(), Value::String(next));
    }
}

// ---- Chat messages[] ----
fn inject_chat_system(body: &mut Value, prompt: &str) {
    let Some(arr) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    if contains_prompt_in_messages(arr, prompt) {
        return;
    }
    let idx = arr.iter().position(|m| {
        m.get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == role::SYSTEM || r == role::DEVELOPER)
    });
    match idx {
        Some(i) => append_to_chat_message(&mut arr[i], prompt),
        None => {
            let mut msg = Map::new();
            msg.insert("role".into(), Value::String(role::SYSTEM.into()));
            msg.insert("content".into(), Value::String(prompt.to_string()));
            arr.insert(0, Value::Object(msg));
        }
    }
}

fn contains_prompt_in_messages(arr: &[Value], prompt: &str) -> bool {
    for m in arr {
        if !m
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == role::SYSTEM || r == role::DEVELOPER)
        {
            continue;
        }
        let c = m.get("content");
        if let Some(s) = c.and_then(Value::as_str)
            && has_prompt(s, prompt)
        {
            return true;
        }
        if let Some(parts) = c.and_then(Value::as_array)
            && parts.iter().any(|p| {
                p.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| has_prompt(t, prompt))
            })
        {
            return true;
        }
    }
    false
}

fn append_to_chat_message(msg: &mut Value, prompt: &str) {
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    match obj.get("content") {
        Some(Value::String(c)) => {
            let next = dedup_string_append(c, prompt);
            if next != *c {
                obj.insert("content".into(), Value::String(next));
            }
        }
        Some(Value::Array(_)) => {
            if let Some(Value::Array(c)) = obj.get_mut("content") {
                if c.iter()
                    .any(|b| b.get("text").and_then(Value::as_str) == Some(prompt))
                {
                    return;
                }
                let mut block = Map::new();
                block.insert("type".into(), Value::String(openai_block::TEXT.into()));
                block.insert("text".into(), Value::String(prompt.to_string()));
                c.push(Value::Object(block));
            }
        }
        _ => {
            obj.insert("content".into(), Value::String(prompt.to_string()));
        }
    }
}

// ---- Responses input[] ----
fn inject_responses_input_system(body: &mut Value, prompt: &str) {
    let Some(arr) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    if contains_prompt_in_responses_input(arr, prompt) {
        return;
    }
    let idx = arr.iter().position(|m| {
        m.get("type").and_then(Value::as_str) == Some(responses_item::MESSAGE)
            && m.get("role")
                .and_then(Value::as_str)
                .is_some_and(|r| r == role::SYSTEM || r == role::DEVELOPER)
    });
    match idx {
        Some(i) => append_to_responses_message(&mut arr[i], prompt),
        None => {
            let mut msg = Map::new();
            msg.insert("type".into(), Value::String(responses_item::MESSAGE.into()));
            msg.insert("role".into(), Value::String(role::SYSTEM.into()));
            let mut block = Map::new();
            block.insert(
                "type".into(),
                Value::String(responses_item::INPUT_TEXT.into()),
            );
            block.insert("text".into(), Value::String(prompt.to_string()));
            msg.insert("content".into(), Value::Array(vec![Value::Object(block)]));
            arr.insert(0, Value::Object(msg));
        }
    }
}

fn contains_prompt_in_responses_input(arr: &[Value], prompt: &str) -> bool {
    for item in arr {
        if item.get("type").and_then(Value::as_str) != Some(responses_item::MESSAGE) {
            continue;
        }
        if !item
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|r| r == role::SYSTEM || r == role::DEVELOPER)
        {
            continue;
        }
        let c = item.get("content");
        if let Some(s) = c.and_then(Value::as_str)
            && has_prompt(s, prompt)
        {
            return true;
        }
        if let Some(parts) = c.and_then(Value::as_array)
            && parts.iter().any(|p| {
                p.get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|t| has_prompt(t, prompt))
            })
        {
            return true;
        }
    }
    false
}

fn append_to_responses_message(msg: &mut Value, prompt: &str) {
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    match obj.get("content") {
        Some(Value::String(c)) => {
            let next = dedup_string_append(c, prompt);
            if next != *c {
                obj.insert("content".into(), Value::String(next));
            }
        }
        Some(Value::Array(_)) => {
            if let Some(Value::Array(c)) = obj.get_mut("content") {
                if c.iter()
                    .any(|b| b.get("text").and_then(Value::as_str) == Some(prompt))
                {
                    return;
                }
                let mut block = Map::new();
                block.insert(
                    "type".into(),
                    Value::String(responses_item::INPUT_TEXT.into()),
                );
                block.insert("text".into(), Value::String(prompt.to_string()));
                c.push(Value::Object(block));
            }
        }
        _ => {
            let mut block = Map::new();
            block.insert(
                "type".into(),
                Value::String(responses_item::INPUT_TEXT.into()),
            );
            block.insert("text".into(), Value::String(prompt.to_string()));
            obj.insert("content".into(), Value::Array(vec![Value::Object(block)]));
        }
    }
}

// ---- Claude ----
fn inject_claude_system(body: &mut Value, prompt: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match obj.get("system") {
        Some(Value::String(sys)) => {
            if has_prompt(sys, prompt) {
                return;
            }
            let next = if sys.is_empty() {
                prompt.to_string()
            } else {
                format!("{sys}{SEP}{prompt}")
            };
            obj.insert("system".into(), Value::String(next));
        }
        Some(Value::Array(_)) => {
            if let Some(Value::Array(sys)) = obj.get_mut("system") {
                if sys
                    .iter()
                    .any(|b| b.get("text").and_then(Value::as_str) == Some(prompt))
                {
                    return;
                }
                let mut block = Map::new();
                block.insert("type".into(), Value::String(claude_block::TEXT.into()));
                block.insert("text".into(), Value::String(prompt.to_string()));
                // Insert before the last cache_control block so the cache prefix
                // is not broken; otherwise append.
                let last_cache = sys.iter().rposition(|b| b.get("cache_control").is_some());
                match last_cache {
                    Some(i) => sys.insert(i, Value::Object(block)),
                    None => sys.push(Value::Object(block)),
                }
            }
        }
        _ => {
            obj.insert("system".into(), Value::String(prompt.to_string()));
        }
    }
}

// ---- Gemini ----
fn inject_gemini_system(body: &mut Value, prompt: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let key = if obj.contains_key("system_instruction") {
        "system_instruction"
    } else {
        "systemInstruction"
    };

    if let Some(parts) = obj
        .get_mut(key)
        .and_then(|sys| sys.get_mut("parts"))
        .and_then(Value::as_array_mut)
    {
        if parts
            .iter()
            .any(|p| p.get("text").and_then(Value::as_str) == Some(prompt))
        {
            return;
        }
        let mut part = Map::new();
        part.insert("text".into(), Value::String(prompt.to_string()));
        parts.push(Value::Object(part));
        return;
    }
    let mut part = Map::new();
    part.insert("text".into(), Value::String(prompt.to_string()));
    let mut sys = Map::new();
    sys.insert("parts".into(), Value::Array(vec![Value::Object(part)]));
    obj.insert(key.to_string(), Value::Object(sys));
}

// ---- Kiro ----
// Appended to the first user turn's content — the same place the Kiro
// translator mirrors system text. A top-level `systemPrompt` is deliberately
// NOT written: kiro.dev rejects any body carrying that field with
// `400 REQUEST_BODY_INVALID`, and every kr/ model failed while an RTK prompt
// was active because this injector kept adding it back.
fn inject_kiro_system(body: &mut Value, prompt: &str) {
    let Some(cs) = body
        .get_mut("conversationState")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let history_idx = cs
        .get("history")
        .and_then(Value::as_array)
        .and_then(|h| h.iter().position(|it| it.get("userInputMessage").is_some()));
    let msg = match history_idx {
        Some(i) => cs
            .get_mut("history")
            .and_then(Value::as_array_mut)
            .and_then(|h| h.get_mut(i))
            .and_then(|it| it.get_mut("userInputMessage")),
        None => cs
            .get_mut("currentMessage")
            .and_then(|c| c.get_mut("userInputMessage")),
    };
    let Some(msg) = msg else {
        return;
    };
    let Some(obj) = msg.as_object_mut() else {
        return;
    };
    let content = obj
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let next = dedup_string_append(&content, prompt);
    if next != content {
        obj.insert("content".into(), Value::String(next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const P: &str = "be terse";

    #[test]
    fn claude_string_system_is_appended_once() {
        let mut body = json!({"system": "hi", "messages": []});
        inject_system_prompt(&mut body, f::CLAUDE, P);
        assert_eq!(body["system"], "hi\n\nbe terse");
        inject_system_prompt(&mut body, f::CLAUDE, P);
        assert_eq!(body["system"], "hi\n\nbe terse", "idempotent");
    }

    #[test]
    fn claude_absent_system_is_created() {
        let mut body = json!({"messages": []});
        inject_system_prompt(&mut body, f::CLAUDE, P);
        assert_eq!(body["system"], P);
    }

    #[test]
    fn claude_array_system_inserts_before_last_cache_control() {
        let mut body = json!({"system": [
            {"type": "text", "text": "a"},
            {"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}}
        ]});
        inject_system_prompt(&mut body, f::CLAUDE, P);
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 3);
        assert_eq!(sys[1]["text"], P);
        assert!(sys[2].get("cache_control").is_some());
    }

    #[test]
    fn openai_chat_appends_to_existing_system() {
        let mut body = json!({"messages": [{"role": "system", "content": "base"}]});
        inject_system_prompt(&mut body, f::OPENAI, P);
        assert_eq!(body["messages"][0]["content"], "base\n\nbe terse");
    }

    #[test]
    fn openai_chat_creates_system_at_front() {
        let mut body = json!({"messages": [{"role": "user", "content": "hi"}]});
        inject_system_prompt(&mut body, f::OPENAI, P);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], P);
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn instructions_string_is_used_for_responses() {
        let mut body = json!({"instructions": "sys", "input": []});
        inject_system_prompt(&mut body, f::OPENAI_RESPONSES, P);
        assert_eq!(body["instructions"], "sys\n\nbe terse");
    }

    #[test]
    fn responses_input_creates_a_system_message() {
        let mut body = json!({"input": [{"type": "message", "role": "user", "content": "hi"}]});
        inject_system_prompt(&mut body, f::OPENAI_RESPONSES, P);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "system");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], P);
    }

    #[test]
    fn gemini_system_instruction_is_created() {
        let mut body = json!({"contents": []});
        inject_system_prompt(&mut body, f::GEMINI, P);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], P);
    }

    #[test]
    fn kiro_appends_to_first_user_turn() {
        let mut body = json!({"conversationState": {
            "history": [{"userInputMessage": {"content": "hello"}}],
            "currentMessage": {"userInputMessage": {"content": "again"}}
        }});
        inject_system_prompt(&mut body, f::KIRO, P);
        assert_eq!(
            body["conversationState"]["history"][0]["userInputMessage"]["content"],
            "hello\n\nbe terse"
        );
        assert_eq!(
            body["conversationState"]["currentMessage"]["userInputMessage"]["content"],
            "again"
        );
        assert!(body.get("systemPrompt").is_none());
    }

    #[test]
    fn string_input_stays_untouched() {
        let mut body = json!({"input": "raw"});
        inject_system_prompt(&mut body, f::OPENAI_RESPONSES, P);
        assert_eq!(body["input"], "raw");
    }
}
