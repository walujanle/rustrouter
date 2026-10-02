//! Claude request preparation.
//!
//! Everything that turns a translated body into something the Anthropic
//! Messages API (or a Claude-compatible upstream) accepts: cache-control
//! anchoring, empty-message filtering, tool_use/tool_result ordering, thinking
//! placeholders, image hoisting, and OAuth cloaking.
//!
//! Several steps look redundant and are not. `capCacheControlBlocks` exists
//! because the 4-marker budget must be spent on the *head* anchors first; a
//! naive "keep the last 4" drops exactly the anchors re-anchoring is for.
//! `normalizeClaudePassthrough` folds mid-conversation `system` messages in
//! place rather than hoisting them, because hoisting invalidates the cached
//! prefix on every request.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::catalog::get_capabilities_for_model;
use crate::runtime_config::DEFAULT_MAX_TOKENS;
use crate::session_manager::{SessionIdentityInput, resolve_session_id};
use crate::translator::schema::{claude_block, role};
use crate::utils::claude_cloaking::apply_cloaking;
use crate::utils::claude_signature::is_valid_claude_signature;

/// `DEFAULT_THINKING_CLAUDE_SIGNATURE`.
pub use crate::constants::DEFAULT_THINKING_CLAUDE_SIGNATURE;

/// `ADAPTIVE_THINKING_UNSUPPORTED = /haiku/i`.
static ADAPTIVE_THINKING_UNSUPPORTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("(?i)haiku").unwrap());
/// `CLAUDE_SERVER_TOOL_USE_ID = /^srvtoolu_[a-zA-Z0-9_]+$/`.
static CLAUDE_SERVER_TOOL_USE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^srvtoolu_[a-zA-Z0-9_]+$").unwrap());

/// `handlesThinkingBlocks(provider)`.
fn handles_thinking_blocks(provider: Option<&str>) -> bool {
    provider.is_some_and(|p| p.starts_with("anthropic-compatible")) || provider == Some("deepseek")
}

/// `lastCacheableToolIndex(tools)`: the index of the last tool that is not
/// `defer_loading`, or `None` (the `-1` case).
pub fn last_cacheable_tool_index(tools: Option<&Value>) -> Option<usize> {
    last_cacheable_tool_index_slice(tools?.as_array()?)
}

fn last_cacheable_tool_index_slice(tools: &[Value]) -> Option<usize> {
    tools
        .iter()
        .rposition(|t| t.get("defer_loading").and_then(Value::as_bool) != Some(true))
}

/// `hasValidContent(msg)`.
pub fn has_valid_content(msg: &Value) -> bool {
    match msg.get("content") {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Object(block)) => valid_block(block),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .any(|b| b.as_object().is_some_and(valid_block)),
        _ => false,
    }
}

fn valid_block(block: &Map<String, Value>) -> bool {
    match block.get("type").and_then(Value::as_str) {
        Some(claude_block::TEXT) => block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|t| !t.trim().is_empty()),
        Some(claude_block::TOOL_USE)
        | Some(claude_block::TOOL_RESULT)
        | Some(claude_block::IMAGE)
        | Some(claude_block::DOCUMENT) => true,
        _ => false,
    }
}

/// `normalizeMessageContent(msg)`: a bare content-block object becomes a
/// one-block array and loses any client-placed `cache_control`.
fn normalize_message_content(msg: &mut Value) {
    let bare = msg.get("content").is_some_and(|c| c.is_object());
    if !bare {
        return;
    }
    let mut block = msg["content"].as_object().cloned().unwrap_or_default();
    block.shift_remove("cache_control");
    msg["content"] = Value::Array(vec![Value::Object(block)]);
}

/// `countCacheControlBlocks(body)`.
fn count_cache_control_blocks(body: &Value) -> usize {
    let mut n = 0;
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        n += system.iter().filter(|b| has_cache_control(b)).count();
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        n += tools.iter().filter(|t| has_cache_control(t)).count();
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for m in messages {
            match m.get("content") {
                Some(Value::Array(blocks)) => {
                    n += blocks.iter().filter(|b| has_cache_control(b)).count();
                }
                Some(Value::Object(c)) if c.contains_key("cache_control") => n += 1,
                _ => {}
            }
        }
    }
    n
}

fn has_cache_control(block: &Value) -> bool {
    block.get("cache_control").is_some_and(|c| !c.is_null())
}

/// Where a cache-control marker lives, so the head anchors can be identified
/// without object identity.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    System(usize),
    Tool(usize),
    Message(usize, usize),
}

fn remove_cache_control_at(body: &mut Value, marker: Marker) {
    let slot = match marker {
        Marker::System(i) => body.get_mut("system").and_then(|s| s.get_mut(i)),
        Marker::Tool(i) => body.get_mut("tools").and_then(|t| t.get_mut(i)),
        Marker::Message(m, b) => body
            .get_mut("messages")
            .and_then(|msgs| msgs.get_mut(m))
            .and_then(|msg| msg.get_mut("content"))
            .and_then(|c| c.get_mut(b)),
    };
    if let Some(block) = slot.and_then(Value::as_object_mut) {
        block.shift_remove("cache_control");
    }
}

/// `capCacheControlBlocks(body)`: trim markers past the 4-marker budget,
/// holding the head anchors and keeping the tail-most of the rest.
fn cap_cache_control_blocks(body: &mut Value) {
    let last_system = body
        .get("system")
        .and_then(Value::as_array)
        .and_then(|s| s.len().checked_sub(1));
    let last_tool = last_cacheable_tool_index(body.get("tools"));

    let mut marked: Vec<Marker> = Vec::new();
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        for (i, b) in system.iter().enumerate() {
            if has_cache_control(b) {
                marked.push(Marker::System(i));
            }
        }
    }
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for (i, t) in tools.iter().enumerate() {
            if has_cache_control(t) {
                marked.push(Marker::Tool(i));
            }
        }
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for (m, msg) in messages.iter().enumerate() {
            if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                for (b, block) in blocks.iter().enumerate() {
                    if has_cache_control(block) {
                        marked.push(Marker::Message(m, b));
                    }
                }
            }
        }
    }

    let is_head = |m: &Marker| match m {
        Marker::System(i) => last_system == Some(*i),
        Marker::Tool(i) => last_tool == Some(*i),
        Marker::Message(..) => false,
    };

    let head_count = marked.iter().filter(|m| is_head(m)).count();
    let rest: Vec<Marker> = marked.iter().copied().filter(|m| !is_head(m)).collect();
    let keep = 4usize.saturating_sub(head_count);
    let drop_count = rest.len().saturating_sub(keep);
    for marker in rest.into_iter().take(drop_count) {
        remove_cache_control_at(body, marker);
    }
}

/// `fixToolUseOrdering(messages)`.
pub fn fix_tool_use_ordering(messages: &[Value]) -> Vec<Value> {
    if messages.len() <= 1 {
        return messages.to_vec();
    }

    // Pass 1: drop text blocks that follow a tool_use in an assistant message.
    let mut pass1: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some(role::ASSISTANT) {
            pass1.push(msg.clone());
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            pass1.push(msg.clone());
            continue;
        };
        let has_tool_use = content
            .iter()
            .any(|b| b.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_USE));
        if !has_tool_use {
            pass1.push(msg.clone());
            continue;
        }
        let mut new_content: Vec<Value> = Vec::with_capacity(content.len());
        let mut found_tool_use = false;
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some(claude_block::TOOL_USE) => {
                    found_tool_use = true;
                    new_content.push(block.clone());
                }
                Some(claude_block::THINKING) | Some(claude_block::REDACTED_THINKING) => {
                    new_content.push(block.clone());
                }
                _ if !found_tool_use => new_content.push(block.clone()),
                _ => {}
            }
        }
        let mut m = msg.clone();
        m["content"] = Value::Array(new_content);
        pass1.push(m);
    }

    // Pass 2: merge consecutive same-role messages, tool_result first.
    let mut merged: Vec<Value> = Vec::with_capacity(pass1.len());
    for msg in pass1 {
        let msg_role = msg.get("role").and_then(Value::as_str).unwrap_or_default();
        let last_role = merged
            .last()
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let msg_content = as_block_array(msg.get("content"));
        if !merged.is_empty() && last_role == msg_role {
            let last = merged.last_mut().unwrap();
            let last_content = as_block_array(last.get("content"));
            let mut tool_results: Vec<Value> = Vec::new();
            let mut other: Vec<Value> = Vec::new();
            for block in last_content.iter().chain(msg_content.iter()) {
                if block.get("type").and_then(Value::as_str) == Some(claude_block::TOOL_RESULT) {
                    tool_results.push(block.clone());
                } else {
                    other.push(block.clone());
                }
            }
            tool_results.extend(other);
            last["content"] = Value::Array(tool_results);
        } else {
            let mut m = Map::new();
            m.insert("role".into(), json!(msg_role));
            m.insert("content".into(), Value::Array(msg_content.to_vec()));
            merged.push(Value::Object(m));
        }
    }
    merged
}

/// `Array.isArray(c) ? c : [{type: "text", text: c}]`.
fn as_block_array(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => vec![json!({"type": claude_block::TEXT, "text": s})],
        _ => Vec::new(),
    }
}

/// `buildThinkingPlaceholder(provider)`.
fn build_thinking_placeholder(provider: Option<&str>) -> Value {
    let mut block = json!({"type": claude_block::THINKING, "thinking": "."});
    // DeepSeek's Anthropic-compatible endpoint needs the block but not the
    // signed-thinking fallback.
    if provider != Some("deepseek") {
        block["signature"] = json!(DEFAULT_THINKING_CLAUDE_SIGNATURE);
    }
    block
}

fn has_foreign_server_tool_use_id(block: &Value) -> bool {
    if block.get("type").and_then(Value::as_str) != Some(claude_block::SERVER_TOOL_USE) {
        return false;
    }
    let id = block.get("id").map(value_to_string).unwrap_or_default();
    !CLAUDE_SERVER_TOOL_USE_ID.is_match(&id)
}

/// JS `String(v)` for the id check — `null`/missing become `""`.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `normalizeClaudePassthrough(body, model)`, in place.
pub fn normalize_claude_passthrough(body: &mut Value, model: &str) {
    if !body.is_object() {
        return;
    }
    let is_haiku = ADAPTIVE_THINKING_UNSUPPORTED.is_match(model);

    // 1. Downgrade adaptive thinking on models that reject it.
    if body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("adaptive")
        && is_haiku
    {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": 10000});
    }

    // 2. Strip `output_config.effort` on models that reject it.
    if is_haiku
        && body
            .get("output_config")
            .and_then(|o| o.get("effort"))
            .is_some_and(|e| !e.is_null())
    {
        if let Some(obj) = body.get_mut("output_config").and_then(Value::as_object_mut) {
            obj.shift_remove("effort");
        }
        let empty = body
            .get("output_config")
            .and_then(Value::as_object)
            .is_some_and(Map::is_empty);
        if empty {
            body.as_object_mut().unwrap().shift_remove("output_config");
        }
    }

    // 3. Wrap bare content-block objects before the fold below assumes arrays.
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages.iter_mut() {
            normalize_message_content(msg);
        }
    }

    // 4. Fold mid-conversation system messages into the neighbouring turn.
    if let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() {
        let mut out: Vec<Value> = Vec::with_capacity(messages.len());
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) != Some(role::SYSTEM) {
                out.push(msg);
                continue;
            }
            let text = match msg.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(blocks)) => blocks
                    .iter()
                    .map(|b| match b {
                        Value::String(s) => s.clone(),
                        other => other
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join("\n"),
                _ => String::new(),
            };
            if text.trim().is_empty() {
                continue;
            }

            let block = json!({"type": claude_block::TEXT, "text": text});
            let prev_is_user = out
                .last()
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some(role::USER);
            if prev_is_user {
                let prev = out.last_mut().unwrap();
                let mut content: Vec<Value> = match prev.get("content") {
                    Some(Value::String(s)) => {
                        vec![json!({"type": claude_block::TEXT, "text": s})]
                    }
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                content.push(block);
                prev["content"] = Value::Array(content);
                continue;
            }
            out.push(json!({"role": role::USER, "content": [block]}));
        }
        body["messages"] = Value::Array(out);
    }

    // 5. Drop foreign thinking signatures and foreign server_tool_use ids.
    let thinking_enabled = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("enabled");
    let mut dropped_server_tool_use_ids: HashSet<String> = HashSet::new();
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages.iter_mut() {
            if msg.get("role").and_then(Value::as_str) != Some(role::ASSISTANT) {
                continue;
            }
            let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
                continue;
            };
            let mut has_tool_use = false;
            let mut has_kept_thinking = false;
            let mut kept: Vec<Value> = Vec::with_capacity(content.len());
            for block in content {
                let block_type = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if block_type == claude_block::THINKING
                    || block_type == claude_block::REDACTED_THINKING
                {
                    if is_valid_claude_signature(block.get("signature").and_then(Value::as_str)) {
                        has_kept_thinking = true;
                        kept.push(block);
                    }
                    continue;
                }
                if has_foreign_server_tool_use_id(&block) {
                    if let Some(id) = block.get("id")
                        && !id.is_null()
                    {
                        dropped_server_tool_use_ids.insert(value_to_string(id));
                    }
                    continue;
                }
                if block_type == claude_block::TOOL_USE {
                    has_tool_use = true;
                }
                kept.push(block);
            }
            if thinking_enabled && !has_kept_thinking && has_tool_use {
                kept.insert(0, build_thinking_placeholder(Some("claude")));
            }
            msg["content"] = Value::Array(kept);
        }
    }

    // A dropped server_tool_use must take its result with it.
    if !dropped_server_tool_use_ids.is_empty()
        && let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut)
    {
        for msg in messages.iter_mut() {
            let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
                continue;
            };
            let kept: Vec<Value> = content
                .into_iter()
                .filter(|block| {
                    let t = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let is_result =
                        t == claude_block::TOOL_RESULT || t == claude_block::WEB_SEARCH_TOOL_RESULT;
                    let id = block
                        .get("tool_use_id")
                        .map(value_to_string)
                        .unwrap_or_default();
                    !(is_result && dropped_server_tool_use_ids.contains(&id))
                })
                .collect();
            msg["content"] = Value::Array(kept);
        }
    }

    // 6. Drop empty text blocks and any message left with no content.
    if let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() {
        let mut out: Vec<Value> = Vec::with_capacity(messages.len());
        for mut msg in messages {
            match msg.get("content") {
                Some(Value::String(s)) => {
                    if s.trim().is_empty() {
                        continue;
                    }
                    out.push(msg);
                }
                Some(Value::Array(blocks)) => {
                    let filtered: Vec<Value> = blocks
                        .iter()
                        .filter(|b| {
                            !(b.get("type").and_then(Value::as_str) == Some(claude_block::TEXT)
                                && b.get("text")
                                    .map(value_to_string)
                                    .unwrap_or_default()
                                    .trim()
                                    .is_empty())
                        })
                        .cloned()
                        .collect();
                    if filtered.is_empty() {
                        continue;
                    }
                    msg["content"] = Value::Array(filtered);
                    out.push(msg);
                }
                _ => out.push(msg),
            }
        }
        body["messages"] = Value::Array(out);
    }
}

/// `markLastCacheableBlock(msg)`: a 5m breakpoint on the last non-thinking
/// block.
fn mark_last_cacheable_block(msg: &mut Value) -> bool {
    let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    for block in content.iter_mut().rev() {
        let Some(obj) = block.as_object_mut() else {
            continue;
        };
        let t = obj.get("type").and_then(Value::as_str).unwrap_or_default();
        if t == claude_block::THINKING || t == claude_block::REDACTED_THINKING {
            continue;
        }
        obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
        return true;
    }
    false
}

/// `anchorClaudeCache(body)`, in place.
pub fn anchor_claude_cache(body: &mut Value) {
    if !body.is_object() {
        return;
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages.iter_mut() {
            normalize_message_content(msg);
        }
    }

    // Invalid markers first, whatever the budget: a tool carrying both
    // defer_loading and cache_control is rejected outright.
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if tool.get("defer_loading").and_then(Value::as_bool) == Some(true)
                && let Some(obj) = tool.as_object_mut()
            {
                obj.shift_remove("cache_control");
            }
        }
    }

    // Head anchors before any budget guard.
    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
        let last = system.len().checked_sub(1);
        for (i, block) in system.iter_mut().enumerate() {
            let Some(obj) = block.as_object_mut() else {
                continue;
            };
            if Some(i) == last {
                obj.insert(
                    "cache_control".into(),
                    json!({"type": "ephemeral", "ttl": "1h"}),
                );
            } else {
                obj.shift_remove("cache_control");
            }
        }
    }

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let last = last_cacheable_tool_index_slice(tools);
        for (i, tool) in tools.iter_mut().enumerate() {
            let Some(obj) = tool.as_object_mut() else {
                continue;
            };
            if Some(i) == last {
                obj.insert(
                    "cache_control".into(),
                    json!({"type": "ephemeral", "ttl": "1h"}),
                );
            } else {
                obj.shift_remove("cache_control");
            }
        }
    }

    if count_cache_control_blocks(body) >= 4 {
        cap_cache_control_blocks(body);
        return;
    }

    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        let mut anchored = false;
        for msg in messages.iter_mut().rev() {
            let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
                continue;
            };
            for block in content.iter_mut() {
                if let Some(obj) = block.as_object_mut() {
                    obj.shift_remove("cache_control");
                }
            }
            if anchored || msg.get("role").and_then(Value::as_str) != Some(role::ASSISTANT) {
                continue;
            }
            anchored = mark_last_cacheable_block(msg);
        }

        // First turn: no assistant yet, so anchor the final message instead.
        if !anchored {
            for msg in messages.iter_mut().rev() {
                if mark_last_cacheable_block(msg) {
                    break;
                }
            }
        }
    }
}

/// `hoistToolResultImages(body)`: move images out of a `tool_result` into the
/// same user turn, since most Claude-compatible endpoints drop them inside a
/// result.
pub fn hoist_tool_result_images(body: &mut Value) {
    let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut touched = false;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.get("role").and_then(Value::as_str) != Some(role::USER) {
            out.push(msg);
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
            out.push(msg);
            continue;
        };
        let mut hoisted: Vec<Value> = Vec::new();
        let mut new_content: Vec<Value> = Vec::with_capacity(content.len());
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some(claude_block::TOOL_RESULT) {
                new_content.push(block);
                continue;
            }
            let Some(inner) = block.get("content").and_then(Value::as_array).cloned() else {
                new_content.push(block);
                continue;
            };
            let images: Vec<Value> = inner
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some(claude_block::IMAGE))
                .cloned()
                .collect();
            if images.is_empty() {
                new_content.push(block);
                continue;
            }
            let rest: Vec<Value> = inner
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) != Some(claude_block::IMAGE))
                .cloned()
                .collect();
            let id = block
                .get("tool_use_id")
                .map(value_to_string)
                .unwrap_or_default();
            hoisted.push(json!({
                "type": claude_block::TEXT,
                "text": format!("[Image from tool result {id}]"),
            }));
            hoisted.extend(images);
            let mut b = block.clone();
            b["content"] = if rest.is_empty() {
                json!([{"type": claude_block::TEXT, "text": "(image attached below)"}])
            } else {
                Value::Array(rest)
            };
            new_content.push(b);
        }
        if hoisted.is_empty() {
            out.push(msg);
            continue;
        }
        touched = true;
        // tool_result blocks must lead the user message; the images follow.
        new_content.extend(hoisted);
        let mut m = msg.clone();
        m["content"] = Value::Array(new_content);
        out.push(m);
    }
    if touched {
        body["messages"] = Value::Array(out);
    }
}

/// The inputs `prepareClaudeRequest` needs beyond the body.
#[derive(Debug, Clone, Default)]
pub struct PrepareClaudeArgs<'a> {
    pub provider: Option<&'a str>,
    pub api_key: Option<&'a str>,
    pub connection_id: Option<&'a str>,
    /// Lower-cased client headers. `None` is the same as an empty map — both
    /// reach the same lookups.
    pub raw_headers: Option<&'a std::collections::HashMap<String, String>>,
    pub session_id: Option<&'a str>,
}

/// `prepareClaudeRequest(body, provider, apiKey, connectionId, rawHeaders,
/// sessionId)`, in place.
pub fn prepare_claude_request(body: &mut Value, args: &PrepareClaudeArgs<'_>) {
    let provider = args.provider;

    // quirk: MiniMax's Claude-compatible endpoint rejects output_config.
    if transport_quirk_truthy(provider, "dropOutputConfig") {
        body.as_object_mut().unwrap().shift_remove("output_config");
    }

    // Clamp max_tokens to the model's real output ceiling.
    let max_tokens = body.get("max_tokens").and_then(Value::as_i64);
    if let Some(mut mt) = max_tokens.filter(|v| *v != 0) {
        let model = body.get("model").and_then(Value::as_str).unwrap_or("");
        let ceiling = {
            let c = get_capabilities_for_model(provider, model).max_output;
            if c > 0 { c } else { DEFAULT_MAX_TOKENS }
        };
        if mt > ceiling {
            mt = ceiling;
        }

        let budget = body
            .get("thinking")
            .filter(|t| t.get("type").and_then(Value::as_str) == Some("enabled"))
            .and_then(|t| t.get("budget_tokens"))
            .and_then(Value::as_i64);
        if let Some(budget) = budget
            && budget >= mt
        {
            mt = (budget + 1024).min(ceiling);
            if budget >= mt {
                body["thinking"]["budget_tokens"] = json!(1024.max(mt - 1024));
            }
        }
        body["max_tokens"] = json!(mt);
    }

    // 1. System: drop every cache_control, re-anchor the last block at 1h.
    if let Some(system) = body.get("system").and_then(Value::as_array).cloned() {
        let last = system.len().checked_sub(1);
        let mapped: Vec<Value> = system
            .into_iter()
            .enumerate()
            .map(|(i, block)| {
                let mut b = block;
                if let Some(obj) = b.as_object_mut() {
                    obj.shift_remove("cache_control");
                    if Some(i) == last {
                        obj.insert(
                            "cache_control".into(),
                            json!({"type": "ephemeral", "ttl": "1h"}),
                        );
                    }
                }
                b
            })
            .collect();
        body["system"] = Value::Array(mapped);
    }

    // 2. Messages.
    if let Some(messages) = body.get("messages").and_then(Value::as_array).cloned() {
        let len = messages.len();
        let mut filtered: Vec<Value> = Vec::with_capacity(len);
        for (i, msg) in messages.into_iter().enumerate() {
            let mut msg = msg;
            normalize_message_content(&mut msg);
            if let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) {
                for block in content.iter_mut() {
                    if let Some(obj) = block.as_object_mut() {
                        obj.shift_remove("cache_control");
                    }
                }
            }
            let is_final_assistant =
                i == len - 1 && msg.get("role").and_then(Value::as_str) == Some(role::ASSISTANT);
            if is_final_assistant || has_valid_content(&msg) {
                filtered.push(msg);
            }
        }

        filtered = fix_tool_use_ordering(&filtered);
        body["messages"] = Value::Array(filtered.clone());

        let last_is_user = filtered
            .last()
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some(role::USER);
        let thinking_enabled = body
            .get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(Value::as_str)
            == Some("enabled")
            && last_is_user;

        let mut last_assistant_processed = false;
        let mut filtered = filtered;
        for msg in filtered.iter_mut().rev() {
            if msg.get("role").and_then(Value::as_str) != Some(role::ASSISTANT) {
                continue;
            }
            let Some(content) = msg.get("content").and_then(Value::as_array).cloned() else {
                continue;
            };
            if !content.is_empty()
                && !last_assistant_processed
                && let Some(content_mut) = msg.get_mut("content").and_then(Value::as_array_mut)
            {
                for block in content_mut.iter_mut().rev() {
                    let t = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if t != claude_block::THINKING && t != claude_block::REDACTED_THINKING {
                        if let Some(obj) = block.as_object_mut() {
                            obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                        }
                        break;
                    }
                }
                last_assistant_processed = true;
            }

            if handles_thinking_blocks(provider) {
                let is_deepseek = provider == Some("deepseek");
                let mut has_tool_use = false;
                let mut has_kept_thinking = false;
                let mut kept: Vec<Value> = Vec::with_capacity(content.len());
                for block in content {
                    let t = block
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let is_thinking =
                        t == claude_block::THINKING || t == claude_block::REDACTED_THINKING;
                    if is_thinking {
                        let mut block = block;
                        if is_deepseek {
                            has_kept_thinking = true;
                            kept.push(block);
                        } else {
                            block["signature"] = json!(DEFAULT_THINKING_CLAUDE_SIGNATURE);
                            has_kept_thinking = true;
                            kept.push(block);
                        }
                        continue;
                    }
                    if t == claude_block::TOOL_USE {
                        has_tool_use = true;
                    }
                    kept.push(block);
                }
                if thinking_enabled && !has_kept_thinking && has_tool_use {
                    kept.insert(0, build_thinking_placeholder(provider));
                }
                msg["content"] = Value::Array(kept);
            }
        }
        body["messages"] = Value::Array(filtered);
    }

    // 3. Tools.
    if let Some(tools) = body.get("tools").and_then(Value::as_array).cloned() {
        let mut tools = tools;
        let supported_types = transport_quirk(provider, "claudeSupportedToolTypes")
            .and_then(Value::as_array)
            .cloned();
        let has_whitelist = supported_types.is_some();
        let supported_types = supported_types.unwrap_or_default();
        tools = tools
            .into_iter()
            .filter(|tool| {
                let t = tool.get("type").and_then(Value::as_str);
                match t {
                    None | Some("function") => true,
                    Some(t) => {
                        if has_whitelist {
                            supported_types.iter().any(|s| s.as_str() == Some(t))
                        } else {
                            false
                        }
                    }
                }
            })
            .map(|tool| {
                if let Some(function) = tool.get("function") {
                    return json!({
                        "name": function.get("name"),
                        "description": function.get("description"),
                        "input_schema": function.get("parameters"),
                    });
                }
                if has_whitelist {
                    return tool;
                }
                let mut t = tool;
                if let Some(obj) = t.as_object_mut() {
                    obj.shift_remove("type");
                }
                t
            })
            .collect();

        let last_cacheable = last_cacheable_tool_index_slice(&tools);
        tools = tools
            .into_iter()
            .enumerate()
            .map(|(i, tool)| {
                let mut t = tool;
                if let Some(obj) = t.as_object_mut() {
                    obj.shift_remove("cache_control");
                    if Some(i) == last_cacheable {
                        obj.insert(
                            "cache_control".into(),
                            json!({"type": "ephemeral", "ttl": "1h"}),
                        );
                    }
                }
                t
            })
            .collect();

        if tools.is_empty() {
            let obj = body.as_object_mut().unwrap();
            obj.shift_remove("tools");
            obj.shift_remove("tool_choice");
        } else {
            body["tools"] = Value::Array(tools);
        }
    }

    // Anthropic reads images inside tool_result; other endpoints do not.
    if !provider.is_some_and(|p| p.starts_with("anthropic-compatible")) {
        hoist_tool_result_images(body);
    }

    // Cloaking for OAuth tokens.
    let is_claude_like = provider.is_some_and(|p| p.starts_with("anthropic-compatible"));
    if is_claude_like && let Some(api_key) = args.api_key {
        let sid = match args.session_id {
            Some(sid) => sid.to_string(),
            None => {
                let empty = std::collections::HashMap::new();
                let headers = args.raw_headers.unwrap_or(&empty);
                resolve_session_id(&SessionIdentityInput {
                    headers,
                    body,
                    connection_id: args.connection_id,
                    workspace_id: None,
                    scope: "claude",
                })
            }
        };
        apply_cloaking(body, Some(api_key), Some(&sid));
    }
}

/// Read a quirk off the built transport as raw JSON.
fn transport_quirk<'a>(provider: Option<&str>, key: &str) -> Option<&'a Value> {
    let quirks = provider
        .and_then(|p| crate::providers::registry::registry().transport(p))
        .and_then(|t| t.quirks.as_ref())?;
    quirks.get(key)
}

/// JS truthiness for a quirk flag: present and neither `false` nor `null`.
fn transport_quirk_truthy(provider: Option<&str>, key: &str) -> bool {
    transport_quirk(provider, key).is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn args<'a>(headers: &'a HashMap<String, String>) -> PrepareClaudeArgs<'a> {
        PrepareClaudeArgs {
            raw_headers: Some(headers),
            ..Default::default()
        }
    }

    #[test]
    fn last_cacheable_tool_index_skips_deferred_tools() {
        let tools = json!([
            {"name": "a"},
            {"name": "b", "defer_loading": true},
            {"name": "c"},
        ]);
        assert_eq!(last_cacheable_tool_index(Some(&tools)), Some(2));
        let all_deferred = json!([{"name": "a", "defer_loading": true}]);
        assert_eq!(last_cacheable_tool_index(Some(&all_deferred)), None);
        assert_eq!(last_cacheable_tool_index(None), None);
    }

    #[test]
    fn has_valid_content_matches_the_expected_cases() {
        assert!(has_valid_content(&json!({"content": " hi "})));
        assert!(!has_valid_content(&json!({"content": "   "})));
        assert!(has_valid_content(
            &json!({"content": [{"type": "tool_use", "id": "x"}]})
        ));
        assert!(!has_valid_content(
            &json!({"content": [{"type": "text", "text": "  "}]})
        ));
        assert!(has_valid_content(&json!({"content": {"type": "image"}})));
        assert!(!has_valid_content(&json!({})));
    }

    #[test]
    fn fix_tool_use_ordering_drops_text_after_tool_use_and_merges_roles() {
        let messages = json!([
            {"role": "assistant", "content": [
                {"type": "text", "text": "before"},
                {"type": "tool_use", "id": "t1"},
                {"type": "text", "text": "after"},
            ]},
            {"role": "user", "content": [{"type": "text", "text": "u1"}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]},
        ]);
        let out = fix_tool_use_ordering(messages.as_array().unwrap());
        assert_eq!(out.len(), 2, "two user messages merge");
        let assistant_content = out[0]["content"].as_array().unwrap();
        assert_eq!(assistant_content.len(), 2);
        assert_eq!(assistant_content[0]["text"], json!("before"));
        assert_eq!(assistant_content[1]["type"], json!("tool_use"));
        // tool_result leads the merged user turn.
        let user_content = out[1]["content"].as_array().unwrap();
        assert_eq!(user_content[0]["type"], json!("tool_result"));
        assert_eq!(user_content[1]["type"], json!("text"));
    }

    #[test]
    fn normalize_passthrough_downgrades_adaptive_thinking_for_haiku() {
        let mut body = json!({
            "model": "claude-haiku-4.5",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high", "other": 1},
        });
        normalize_claude_passthrough(&mut body, "claude-haiku-4.5");
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 10000})
        );
        assert!(body["output_config"].get("effort").is_none());
        assert_eq!(
            body["output_config"]["other"],
            json!(1),
            "other fields survive"
        );

        // An emptied output_config is removed entirely.
        let mut body = json!({"output_config": {"effort": "high"}});
        normalize_claude_passthrough(&mut body, "haiku");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn normalize_passthrough_folds_mid_conversation_system_messages() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "hello"},
            {"role": "system", "content": "reminder"},
            {"role": "assistant", "content": "hi"},
            {"role": "system", "content": "  "},
        ]});
        normalize_claude_passthrough(&mut body, "");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "the blank system message is dropped");
        let user_content = messages[0]["content"].as_array().unwrap();
        assert_eq!(user_content.len(), 2);
        assert_eq!(user_content[1]["text"], json!("reminder"));
    }

    #[test]
    fn normalize_passthrough_drops_foreign_thinking_and_server_tool_use() {
        let mut body = json!({
            "thinking": {"type": "enabled"},
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "x", "signature": "not-a-signature"},
                {"type": "server_tool_use", "id": "call_abc", "name": "analyze_image"},
                {"type": "tool_use", "id": "t1", "name": "x"},
            ]}, {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_abc", "content": "r"},
            ]}],
        });
        normalize_claude_passthrough(&mut body, "");
        let assistant = body["messages"][0]["content"].as_array().unwrap();
        // Foreign thinking dropped; a placeholder is prepended because thinking
        // is enabled and a tool_use survives.
        assert_eq!(assistant[0]["type"], json!("thinking"));
        assert_eq!(
            assistant[0]["signature"],
            json!(DEFAULT_THINKING_CLAUDE_SIGNATURE)
        );
        assert_eq!(assistant[1]["type"], json!("tool_use"));
        // The server_tool_use and its orphaned result are gone. The user turn
        // held nothing else, so the whole message is dropped rather than left
        // with an empty content array.
        assert!(
            !assistant
                .iter()
                .any(|b| b["type"] == json!("server_tool_use"))
        );
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn normalize_passthrough_drops_empty_text_and_empty_messages() {
        let mut body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "user", "content": [{"type": "text", "text": "   "}]},
        ]});
        normalize_claude_passthrough(&mut body, "");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cap_cache_control_keeps_the_head_anchors() {
        let mut body = json!({
            "system": [{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}],
            "tools": [
                {"name": "a", "cache_control": {"type": "ephemeral"}},
                {"name": "b", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "1", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "2", "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        });
        // 5 markers total: head = last system + last tool; keep = 2; drop the
        // first 3 of the remaining 3.
        cap_cache_control_blocks(&mut body);
        assert!(
            body["system"][0].get("cache_control").is_some(),
            "head system anchor held"
        );
        assert!(
            body["tools"][1].get("cache_control").is_some(),
            "head tool anchor held"
        );
        // Two head anchors leave a budget of two for the three remaining
        // markers, so only the earliest is dropped.
        assert!(body["tools"][0].get("cache_control").is_none());
        let blocks = body["messages"][0]["content"].as_array().unwrap();
        assert!(blocks[0].get("cache_control").is_some());
        assert!(blocks[1].get("cache_control").is_some());
    }

    #[test]
    fn anchor_cache_pins_the_head_and_the_last_assistant() {
        let mut body = json!({
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "tools": [{"name": "t"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "u"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "x"},
                    {"type": "text", "text": "a"},
                ]},
                {"role": "user", "content": [{"type": "text", "text": "u2"}]},
            ],
        });
        anchor_claude_cache(&mut body);
        assert!(
            body["system"][1].get("cache_control").is_some(),
            "last system block"
        );
        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][0]["cache_control"],
            json!({"type": "ephemeral", "ttl": "1h"})
        );
        // The last assistant's last non-thinking block is anchored at 5m.
        assert_eq!(
            body["messages"][1]["content"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(
            body["messages"][1]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }

    #[test]
    fn anchor_cache_falls_back_to_the_final_message_on_the_first_turn() {
        let mut body =
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "u"}]}]});
        anchor_claude_cache(&mut body);
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
    }

    #[test]
    fn hoist_moves_tool_result_images_into_the_user_turn() {
        let mut body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "result"},
                {"type": "image", "source": {"type": "base64"}},
            ]},
        ]}]});
        hoist_tool_result_images(&mut body);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "tool_result + hoisted text + image");
        assert_eq!(
            content[0]["content"],
            json!([{"type": "text", "text": "result"}])
        );
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .starts_with("[Image from tool result t1]")
        );
        assert_eq!(content[2]["type"], json!("image"));
    }

    #[test]
    fn hoist_leaves_a_result_without_images_untouched() {
        let original = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "r"}]},
        ]}]});
        let mut body = original.clone();
        hoist_tool_result_images(&mut body);
        assert_eq!(body, original);
    }

    #[test]
    fn prepare_clamps_max_tokens_and_keeps_output_config() {
        let headers = HashMap::new();
        let mut body = json!({
            "model": "claude-sonnet-4.5",
            "max_tokens": 999_999,
            "output_config": {"effort": "high"},
        });
        prepare_claude_request(
            &mut body,
            &PrepareClaudeArgs {
                provider: Some("deepseek"),
                ..args(&headers)
            },
        );
        // `dropOutputConfig` belonged to MiniMax, which left with the provider
        // set, so no kept provider drops it.
        assert!(body.get("output_config").is_some());
        assert!(body["max_tokens"].as_i64().unwrap() <= 64_000);
    }

    #[test]
    fn prepare_reconciles_a_thinking_budget_above_max_tokens() {
        let headers = HashMap::new();
        let mut body = json!({
            "model": "claude-opus-4.5",
            "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 5000},
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        prepare_claude_request(&mut body, &args(&headers));
        let mt = body["max_tokens"].as_i64().unwrap();
        assert_eq!(mt, 6024, "budget + 1024 buffer");
    }

    #[test]
    fn prepare_normalizes_openai_style_tools_for_a_non_claude_provider() {
        let headers = HashMap::new();
        let mut body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [
                {"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}},
                {"name": "builtin", "type": "web_search_20250305"},
            ],
        });
        prepare_claude_request(
            &mut body,
            &PrepareClaudeArgs {
                provider: Some("deepseek"),
                ..args(&headers)
            },
        );
        let tools = body["tools"].as_array().unwrap();
        // DeepSeek whitelists web_search_*, so the built-in survives with its type.
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], json!("f"));
        assert_eq!(tools[0]["input_schema"], json!({"type": "object"}));
        assert!(
            tools[0].get("type").is_none(),
            "function tool loses its type wrapper"
        );
        assert_eq!(tools[1]["type"], json!("web_search_20250305"));
    }

    #[test]
    fn prepare_drops_a_tools_array_left_empty() {
        let headers = HashMap::new();
        let mut body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            "tools": [{"name": "builtin", "type": "web_search_20250305"}],
            "tool_choice": {"type": "auto"},
        });
        prepare_claude_request(
            &mut body,
            &PrepareClaudeArgs {
                provider: Some("openrouter"),
                ..args(&headers)
            },
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }
}
