//! Claude anti-ban cloaking.
//!
//! Three transforms, all gated on an OAuth token (`sk-ant-oat…`):
//!
//! 1. `apply_cloaking` injects the Claude Code billing header as `system[0]`
//!    and a deterministic fake `metadata.user_id`.
//! 2. `cloak_claude_tools` suffixes every client tool name with `_ide` and
//!    appends the Claude Code decoy tool set, so the upstream sees a native
//!    Claude Code client. Built-in server tools (those carrying a `type`) are
//!    never suffixed.
//! 3. `decloak_*` reverses the suffix on the way back out — the streamed
//!    `content_block_start` event and the non-streaming `content[]` body.
//!
//! `generateBillingHeader` hashes `JSON.stringify(body)`, so the body must be a
//! `preserve_order` `Value` (it is) and the serialization must match JS's.

use std::collections::HashMap;

use rand::Rng;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::translator::schema::claude_block;

/// `extractClaudeSessionIdFromUserId(userId)`: a JSON `{session_id}` object or
/// a plain string, with a leading `claude:` stripped either way. Anything else
/// (non-string, empty, unparsable JSON, a JSON object without a string
/// `session_id`) yields `None`.
pub fn extract_claude_session_id_from_user_id(user_id: Option<&str>) -> Option<String> {
    let user_id = user_id.filter(|s| !s.is_empty())?;
    if user_id.starts_with('{') {
        let sid = serde_json::from_str::<Value>(user_id).ok().and_then(|v| {
            v.get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })?;
        return strip_claude_prefix(&sid);
    }
    strip_claude_prefix(user_id)
}

/// `/^claude:/i` stripped, trimmed, empty → `None`.
fn strip_claude_prefix(value: &str) -> Option<String> {
    let stripped = match value.get(..7) {
        Some(prefix) if prefix.eq_ignore_ascii_case("claude:") => &value[7..],
        _ => value,
    };
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// `CLAUDE_TOOL_SUFFIX`.
pub const CLAUDE_TOOL_SUFFIX: &str = "_ide";
/// `CLAUDE_CLI_VERSION`.
pub const CLAUDE_CLI_VERSION: &str = "2.1.280";
/// `CC_ENTRYPOINT`.
const CC_ENTRYPOINT: &str = "sdk-cli";

/// `CC_DECOY_TOOLS`: Claude Code's own tool names, marked unavailable.
pub const CC_DECOY_TOOLS: [&str; 20] = [
    "Task",
    "TaskOutput",
    "TaskStop",
    "TaskCreate",
    "TaskGet",
    "TaskUpdate",
    "TaskList",
    "Bash",
    "Glob",
    "Grep",
    "Read",
    "Edit",
    "Write",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "AskUserQuestion",
    "Skill",
    "EnterPlanMode",
    "ExitPlanMode",
];

fn hex_sha256(input: &str) -> String {
    hex::encode(Sha256::digest(input.as_bytes()))
}

/// `randomBytes(2).toString("hex")`.
fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// `generateBillingHeader(payload)`.
fn generate_billing_header(payload: &Value) -> String {
    let content = serde_json::to_string(payload).unwrap_or_default();
    let cch = &hex_sha256(&content)[..5];
    let build_hash = &random_hex(2)[..3];
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CLI_VERSION}.{build_hash}; cc_entrypoint={CC_ENTRYPOINT}; cch={cch};"
    )
}

/// `deriveUuid(seed)` — a deterministic UUID-v4-shaped string.
fn derive_uuid(seed: &str) -> String {
    let h = hex_sha256(seed);
    let variant = (u8::from_str_radix(&h[16..17], 16).unwrap_or(0) & 0x3) | 0x8;
    format!(
        "{}-{}-4{}-{:x}{}-{}",
        &h[0..8],
        &h[8..12],
        &h[13..16],
        variant,
        &h[17..20],
        &h[20..32]
    )
}

/// `generateFakeUserID(sessionId, apiKey)` — a JSON string.
fn generate_fake_user_id(session_id: Option<&str>, api_key: &str) -> String {
    let device_id = hex_sha256(&format!("device:{api_key}"));
    let account_uuid = derive_uuid(&format!("account:{api_key}"));
    let session_uuid = session_id
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    format!(
        "{{\"device_id\":\"{device_id}\",\"account_uuid\":\"{account_uuid}\",\"session_id\":\"{session_uuid}\"}}"
    )
}

/// `applyCloaking(body, apiKey, sessionId)`, in place. A no-op unless the key is
/// an OAuth token.
pub fn apply_cloaking(body: &mut Value, api_key: Option<&str>, session_id: Option<&str>) {
    let Some(api_key) = api_key.filter(|k| k.contains("sk-ant-oat")) else {
        return;
    };

    let billing_text = generate_billing_header(body);
    let billing_block = json!({"type": "text", "text": billing_text});

    match body.get("system") {
        Some(Value::Array(system)) => {
            let already = system
                .first()
                .and_then(|b| b.get("text"))
                .and_then(Value::as_str)
                .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"));
            if !already {
                let mut next = vec![billing_block];
                next.extend(system.iter().cloned());
                body["system"] = Value::Array(next);
            }
        }
        Some(Value::String(existing)) => {
            let existing = existing.clone();
            body["system"] = json!([billing_block, {"type": "text", "text": existing}]);
        }
        _ => {
            body["system"] = Value::Array(vec![billing_block]);
        }
    }

    let has_user_id = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .is_some_and(|v| !v.is_null() && v.as_str() != Some(""));
    if !has_user_id {
        let user_id = generate_fake_user_id(session_id, api_key);
        let mut metadata = body
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        metadata.insert("user_id".into(), Value::String(user_id));
        body["metadata"] = Value::Object(metadata);
    }
}

/// The result of `cloak_claude_tools`: the rewritten body and the
/// suffixed → original name map.
pub struct CloakResult {
    pub body: Value,
    /// `None` when no tool was renamed, mirroring `toolNameMap.size > 0 ? … : null`.
    pub tool_name_map: Option<HashMap<String, String>>,
}

/// `cloakClaudeTools(body)`.
pub fn cloak_claude_tools(body: &Value) -> CloakResult {
    let suffix = |name: &str| format!("{name}{CLAUDE_TOOL_SUFFIX}");

    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return CloakResult {
            body: body.clone(),
            tool_name_map: None,
        };
    };
    if tools.is_empty() {
        return CloakResult {
            body: body.clone(),
            tool_name_map: None,
        };
    }

    let mut tool_name_map: HashMap<String, String> = HashMap::new();
    let mut client_tool_names: Vec<String> = Vec::new();
    let mut client_declarations: Vec<Value> = Vec::with_capacity(tools.len());

    for tool in tools {
        // Built-in server tools carry a `type` and require an exact reserved
        // `name`; suffixing one makes Claude reject the request.
        if tool.get("type").is_some_and(|t| !t.is_null()) {
            client_declarations.push(tool.clone());
            continue;
        }
        let Some(name) = tool.get("name").and_then(Value::as_str) else {
            client_declarations.push(tool.clone());
            continue;
        };
        let suffixed = suffix(name);
        tool_name_map.insert(suffixed.clone(), name.to_string());
        client_tool_names.push(name.to_string());
        let mut renamed = tool.clone();
        renamed["name"] = Value::String(suffixed);
        client_declarations.push(renamed);
    }

    let mut all_tools = client_declarations;
    for name in CC_DECOY_TOOLS {
        all_tools.push(json!({
            "name": name,
            "description": "This tool is currently unavailable.",
            "input_schema": {"type": "object", "properties": {}}
        }));
    }

    let renamed_messages = body
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .map(|msg| {
                    let Some(content) = msg.get("content").and_then(Value::as_array) else {
                        return msg.clone();
                    };
                    let renamed_content: Vec<Value> = content
                        .iter()
                        .map(|block| {
                            if block.get("type").and_then(Value::as_str)
                                == Some(claude_block::TOOL_USE)
                                && let Some(name) = block.get("name").and_then(Value::as_str)
                            {
                                let mut b = block.clone();
                                b["name"] = Value::String(suffix(name));
                                return b;
                            }
                            block.clone()
                        })
                        .collect();
                    let mut m = msg.clone();
                    m["content"] = Value::Array(renamed_content);
                    m
                })
                .collect::<Vec<Value>>()
        });

    let mut cloaked = body.clone();
    cloaked["tools"] = Value::Array(all_tools);
    if let Some(messages) = renamed_messages {
        cloaked["messages"] = Value::Array(messages);
    }

    // A forced tool_choice must point at the suffixed name, but only when it
    // targets a tool we actually renamed — never a decoy or built-in.
    let forced = body.get("tool_choice").and_then(|c| {
        let kind = c.get("type").and_then(Value::as_str)?;
        let name = c.get("name").and_then(Value::as_str)?;
        (kind == "tool" && client_tool_names.iter().any(|n| n == name)).then(|| name.to_string())
    });
    if let Some(name) = forced {
        let mut choice = body["tool_choice"].clone();
        choice["name"] = Value::String(suffix(&name));
        cloaked["tool_choice"] = choice;
    }

    CloakResult {
        body: cloaked,
        tool_name_map: (!tool_name_map.is_empty()).then_some(tool_name_map),
    }
}

/// `decloakToolNames(body, toolNameMap)` — the non-streaming `content[]` body.
pub fn decloak_tool_names(body: &mut Value, tool_name_map: Option<&HashMap<String, String>>) {
    let Some(map) = tool_name_map.filter(|m| !m.is_empty()) else {
        return;
    };
    let Some(content) = body.get("content").and_then(Value::as_array).cloned() else {
        return;
    };
    let content: Vec<Value> = content
        .into_iter()
        .map(|mut block| {
            if block.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE)
                && let Some(name) = block.get("name").and_then(Value::as_str)
                && let Some(original) = map.get(name)
            {
                block["name"] = Value::String(original.clone());
            }
            block
        })
        .collect();
    body["content"] = Value::Array(content);
}

/// `decloakStreamChunk(chunk, toolNameMap)` — one Claude SSE event. In a Claude
/// stream a tool name appears once, on the `content_block_start` of the block.
pub fn decloak_stream_chunk(chunk: &mut Value, tool_name_map: Option<&HashMap<String, String>>) {
    let Some(map) = tool_name_map.filter(|m| !m.is_empty()) else {
        return;
    };
    if chunk.get("type").and_then(Value::as_str) != Some("content_block_start") {
        return;
    }
    let Some(block) = chunk.get("content_block").and_then(Value::as_object) else {
        return;
    };
    if block.get("type").and_then(Value::as_str) != Some(claude_block::TOOL_USE) {
        return;
    }
    let Some(name) = block.get("name").and_then(Value::as_str) else {
        return;
    };
    let Some(original) = map.get(name) else {
        return;
    };
    let mut next = block.clone();
    next.insert("name".into(), Value::String(original.clone()));
    chunk["content_block"] = Value::Object(next);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cloaking_is_a_noop_without_an_oauth_token() {
        let mut body = json!({"system": "be brief", "metadata": {}});
        let before = body.clone();
        apply_cloaking(&mut body, Some("sk-regular-key"), Some("sid"));
        assert_eq!(body, before);
        apply_cloaking(&mut body, None, None);
        assert_eq!(body, before);
    }

    #[test]
    fn cloaking_injects_the_billing_header_first() {
        let mut body = json!({"system": [{"type": "text", "text": "existing"}]});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), Some("sid"));
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        let header = system[0]["text"].as_str().unwrap();
        assert!(header.starts_with("x-anthropic-billing-header: cc_version=2.1.280."));
        assert!(header.contains("cc_entrypoint=sdk-cli;"));
        assert!(header.ends_with(";"));
        assert_eq!(system[1]["text"], json!("existing"));
    }

    #[test]
    fn cloaking_wraps_a_string_system_and_does_not_double_inject() {
        let mut body = json!({"system": "be brief"});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), None);
        assert_eq!(body["system"].as_array().unwrap().len(), 2);
        assert_eq!(body["system"][1]["text"], json!("be brief"));

        apply_cloaking(&mut body, Some("sk-ant-oat-1"), None);
        assert_eq!(
            body["system"].as_array().unwrap().len(),
            2,
            "second call is idempotent"
        );
    }

    #[test]
    fn fake_user_id_is_deterministic_per_key_but_session_varies() {
        let mut body = json!({});
        apply_cloaking(&mut body, Some("sk-ant-oat-1"), Some("sid-1"));
        let uid = body["metadata"]["user_id"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(uid).unwrap();
        assert_eq!(parsed["session_id"], json!("sid-1"));
        assert_eq!(parsed["device_id"].as_str().unwrap().len(), 64);

        let mut body2 = json!({});
        apply_cloaking(&mut body2, Some("sk-ant-oat-1"), Some("sid-2"));
        let uid2: Value =
            serde_json::from_str(body2["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            uid2["device_id"], parsed["device_id"],
            "device id is per key"
        );
        assert_ne!(uid2["session_id"], parsed["session_id"]);
    }

    #[test]
    fn cloak_suffixes_client_tools_and_appends_decoys() {
        let body = json!({
            "tools": [
                {"name": "MyTool", "description": "d", "input_schema": {"type": "object"}},
                {"name": "web_search_20250305", "type": "web_search_20250305"},
            ],
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t1", "name": "MyTool"},
                {"type": "text", "text": "x"},
            ]}],
            "tool_choice": {"type": "tool", "name": "MyTool"},
        });
        let result = cloak_claude_tools(&body);
        let tools = result.body["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            22,
            "one client tool + one built-in + 20 decoys"
        );
        assert_eq!(tools[0]["name"], json!("MyTool_ide"));
        assert_eq!(
            tools[1]["name"],
            json!("web_search_20250305"),
            "built-in is untouched"
        );
        assert_eq!(
            result.body["messages"][0]["content"][0]["name"],
            json!("MyTool_ide")
        );
        assert_eq!(result.body["tool_choice"]["name"], json!("MyTool_ide"));

        let map = result.tool_name_map.unwrap();
        assert_eq!(map.get("MyTool_ide"), Some(&"MyTool".to_string()));
    }

    #[test]
    fn cloak_returns_a_null_map_when_no_tools_are_renamed() {
        let body = json!({"tools": [{"name": "builtin", "type": "builtin"}]});
        let result = cloak_claude_tools(&body);
        assert!(result.tool_name_map.is_none());
        assert_eq!(
            result.body["tools"].as_array().unwrap().len(),
            21,
            "20 decoys + the built-in"
        );

        let body = json!({"tools": []});
        assert!(cloak_claude_tools(&body).tool_name_map.is_none());
        let body = json!({});
        assert!(cloak_claude_tools(&body).tool_name_map.is_none());
    }

    #[test]
    fn decloak_restores_stream_and_body_names() {
        let mut map = HashMap::new();
        map.insert("MyTool_ide".to_string(), "MyTool".to_string());

        let mut chunk = json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "t1", "name": "MyTool_ide"},
        });
        decloak_stream_chunk(&mut chunk, Some(&map));
        assert_eq!(chunk["content_block"]["name"], json!("MyTool"));

        let mut chunk = json!({"type": "content_block_delta", "index": 0});
        decloak_stream_chunk(&mut chunk, Some(&map));
        assert!(
            chunk.get("content_block").is_none(),
            "non-start events pass through"
        );

        let mut body = json!({"content": [{"type": "tool_use", "name": "MyTool_ide"}]});
        decloak_tool_names(&mut body, Some(&map));
        assert_eq!(body["content"][0]["name"], json!("MyTool"));

        let mut unknown = json!({"content": [{"type": "tool_use", "name": "Task"}]});
        decloak_tool_names(&mut unknown, Some(&map));
        assert_eq!(
            unknown["content"][0]["name"],
            json!("Task"),
            "decoys pass through"
        );
    }

    #[test]
    fn session_id_extraction_handles_both_shapes_and_strips_the_prefix() {
        // Plain string with the `claude:` prefix stripped.
        assert_eq!(
            extract_claude_session_id_from_user_id(Some("claude:abc-123")).as_deref(),
            Some("abc-123")
        );
        // JSON object form.
        assert_eq!(
            extract_claude_session_id_from_user_id(Some(r#"{"session_id":"claude:xyz"}"#))
                .as_deref(),
            Some("xyz")
        );
        // Prefix match is case-insensitive.
        assert_eq!(
            extract_claude_session_id_from_user_id(Some("CLAUDE:up")).as_deref(),
            Some("up")
        );
        // Empty, absent, malformed JSON and a JSON object with no session_id all
        // yield nothing rather than a bogus id.
        assert!(extract_claude_session_id_from_user_id(Some("")).is_none());
        assert!(extract_claude_session_id_from_user_id(None).is_none());
        assert!(extract_claude_session_id_from_user_id(Some("{not json")).is_none());
        assert!(extract_claude_session_id_from_user_id(Some("{}")).is_none());
        assert!(extract_claude_session_id_from_user_id(Some("claude:   ")).is_none());
    }

    #[test]
    fn derive_uuid_is_deterministic_and_shaped_like_v4() {
        let a = derive_uuid("account:sk-ant-oat-1");
        let b = derive_uuid("account:sk-ant-oat-1");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        let variant = u8::from_str_radix(&a[19..20], 16).unwrap();
        assert!(variant & 0x8 == 0x8, "variant nibble is 8-b");
    }
}
