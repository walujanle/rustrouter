//! The translator registry and the three entry points.
//!
//! Translation pivots through OpenAI. A translator registered on the exact
//! `source:target` pair runs as a **direct route** instead, which is lossless
//! for pairs the OpenAI bridge mangles (thinking blocks, tool ids, non-base64
//! images, `is_error`).
//!
//! Registration is one explicit `register_all` — the same set of pairs, listed
//! in one place, with no import-order hazard.
//!
//! `ResponseState` is the base every translator starts from. Anything a
//! translator invents beyond it lives in `ResponseState::extra`, because those
//! scratch fields are not part of the contract and typing them here would mean
//! editing this file for every translator.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::providers::registry::registry as provider_registry;
use crate::session_manager::{SessionIdentityInput, resolve_session_id};
use crate::translator::concerns::primitives::safe_parse_json;
use crate::translator::concerns::tool_call::{ensure_tool_call_ids, fix_missing_tool_responses};
use crate::translator::formats::claude::{PrepareClaudeArgs, prepare_claude_request};
use crate::translator::formats::openai::{OpenAiFilterOptions, filter_to_openai_format};
use crate::translator::schema::{openai_block, role};
use crate::utils::claude_cloaking::{cloak_claude_tools, decloak_stream_chunk};
use crate::utils::fingerprint::restore_tool_names;

pub mod concerns;
pub mod formats;
pub mod request;
pub mod response;
pub mod schema;

/// `requestFn(model, body, stream, credentials)` plus the side channel a
/// translator uses for metadata it stashes on the body
/// (`_toolNameMap`, `_customToolNames`).
pub struct RequestContext<'a> {
    pub model: &'a str,
    pub stream: bool,
    pub credentials: &'a Credentials,
    pub meta: &'a mut RequestMeta,
}

/// Metadata a request translator produces for the response side. These would
/// otherwise hang off the returned body as underscore-prefixed keys that the
/// caller strips; a typed struct keeps them out of the wire payload.
#[derive(Debug, Clone, Default)]
pub struct RequestMeta {
    /// `_toolNameMap`: sent name → caller's original name.
    pub tool_name_map: Option<HashMap<String, String>>,
    /// `_customToolNames` (OpenAI Responses only).
    pub custom_tool_names: Vec<String>,
}

/// A request translator.
pub type RequestFn = fn(&mut RequestContext<'_>, Value) -> Value;
/// A response translator. An empty return means "emit nothing", the
/// `null`/`[]` case.
pub type ResponseFn = fn(&Value, &mut ResponseState) -> Vec<Value>;

/// `register(from, to, requestFn, responseFn)`.
#[derive(Default)]
pub struct Registry {
    request: HashMap<String, RequestFn>,
    response: HashMap<String, ResponseFn>,
}

impl Registry {
    /// Either function may be absent.
    pub fn register(
        &mut self,
        from: &str,
        to: &str,
        request_fn: Option<RequestFn>,
        response_fn: Option<ResponseFn>,
    ) {
        let key = format!("{from}:{to}");
        if let Some(f) = request_fn {
            self.request.insert(key.clone(), f);
        }
        if let Some(f) = response_fn {
            self.response.insert(key, f);
        }
    }

    pub fn request_fn(&self, from: &str, to: &str) -> Option<RequestFn> {
        self.request.get(&format!("{from}:{to}")).copied()
    }

    pub fn response_fn(&self, from: &str, to: &str) -> Option<ResponseFn> {
        self.response.get(&format!("{from}:{to}")).copied()
    }
}

/// Every registered pair, in one place.
///
/// The key order is `(from, to)` — `request_fn(from, to)` for a request, and
/// `response_fn(provider_format, client_format)` for a response. Where the
/// same function is registered under two keys it is registered twice here,
/// because the registry is keyed by the exact pair.
fn register_all(registry: &mut Registry) {
    use crate::translator::request as req;
    use crate::translator::response as res;

    // ── request translators ───────────────────────────────────────────────
    registry.register(
        formats::CLAUDE,
        formats::OPENAI,
        Some(req::claude_to_openai::claude_to_openai_request),
        None,
    );
    registry.register(
        formats::OPENAI,
        formats::CLAUDE,
        Some(req::openai_to_claude::openai_to_claude_request),
        None,
    );
    registry.register(
        formats::OPENAI_RESPONSES,
        formats::OPENAI,
        Some(req::openai_responses::openai_responses_to_openai_request),
        None,
    );
    registry.register(
        formats::OPENAI,
        formats::OPENAI_RESPONSES,
        Some(req::openai_responses::openai_to_openai_responses_request),
        None,
    );
    registry.register(
        formats::OPENAI,
        formats::COMMANDCODE,
        Some(req::openai_to_commandcode::openai_to_commandcode_request),
        None,
    );

    // ── response translators ──────────────────────────────────────────────
    registry.register(
        formats::CLAUDE,
        formats::OPENAI,
        None,
        Some(res::claude_to_openai::claude_to_openai_response),
    );
    registry.register(
        formats::OPENAI,
        formats::CLAUDE,
        None,
        Some(res::openai_to_claude::openai_to_claude_response),
    );
    registry.register(
        formats::OPENAI,
        formats::OPENAI_RESPONSES,
        None,
        Some(res::openai_responses::openai_to_openai_responses_response),
    );
    registry.register(
        formats::OPENAI_RESPONSES,
        formats::OPENAI,
        None,
        Some(res::openai_responses::openai_responses_to_openai_response),
    );
    registry.register(
        formats::COMMANDCODE,
        formats::OPENAI,
        None,
        Some(res::commandcode_to_openai::commandcode_to_openai_response),
    );
}

/// The process-wide registry, built once.
pub fn registry() -> &'static Registry {
    static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
        let mut r = Registry::default();
        register_all(&mut r);
        r
    });
    &REGISTRY
}

/// The arguments `translateRequest` takes beyond the body.
pub struct TranslateRequestArgs<'a> {
    pub source_format: &'a str,
    pub target_format: &'a str,
    pub model: &'a str,
    pub stream: bool,
    pub provider: Option<&'a str>,
    /// `stripList` from the `PROVIDER_MODELS` entry.
    pub strip_list: &'a [String],
    pub connection_id: Option<&'a str>,
}

/// The result of `translateRequest`.
#[derive(Debug)]
pub struct TranslatedRequest {
    pub body: Value,
    /// `_toolNameMap`, surfaced rather than smuggled through the body.
    pub tool_name_map: Option<HashMap<String, String>>,
    /// `_customToolNames`, likewise.
    pub custom_tool_names: Vec<String>,
}

/// `translateRequest(sourceFormat, targetFormat, model, body, stream,
/// credentials, provider, stripList, connectionId)`.
///
/// `credentials` is taken mutably: the session id captured from the original
/// body is stashed on it for the Gemini/Kiro envelope builders that run later
/// in this same call.
pub fn translate_request(
    args: &TranslateRequestArgs<'_>,
    mut body: Value,
    credentials: &mut Credentials,
) -> TranslatedRequest {
    strip_content_types(&mut body, args.strip_list);

    // Thinking config is dropped when the last turn is not the user's; the
    // intent itself is captured before any format conversion renames fields.
    normalize_thinking_config(&mut body);

    // Some providers require an id on every tool call.
    ensure_tool_call_ids(&mut body);

    fix_missing_tool_responses(&mut body);

    let thinking_intent = crate::thinking::capture_thinking(&body);

    let client_session_id = resolve_session_id(&SessionIdentityInput {
        headers: &credentials.raw_headers,
        body: &body,
        connection_id: args.connection_id,
        workspace_id: None,
        scope: args.target_format,
    });
    credentials.client_session_id = Some(client_session_id.clone());

    let mut meta = RequestMeta::default();

    if args.source_format != args.target_format {
        let direct = registry().request_fn(args.source_format, args.target_format);
        if let Some(direct) = direct {
            body = direct(
                &mut RequestContext {
                    model: args.model,
                    stream: args.stream,
                    credentials,
                    meta: &mut meta,
                },
                body,
            );
        } else {
            if args.source_format != formats::OPENAI
                && let Some(to_openai) = registry().request_fn(args.source_format, formats::OPENAI)
            {
                body = to_openai(
                    &mut RequestContext {
                        model: args.model,
                        stream: args.stream,
                        credentials,
                        meta: &mut meta,
                    },
                    body,
                );
            }
            if args.target_format != formats::OPENAI
                && let Some(from_openai) =
                    registry().request_fn(formats::OPENAI, args.target_format)
            {
                body = from_openai(
                    &mut RequestContext {
                        model: args.model,
                        stream: args.stream,
                        credentials,
                        meta: &mut meta,
                    },
                    body,
                );
            }
        }
    }

    crate::thinking::apply_thinking(
        args.target_format,
        args.model,
        &mut body,
        args.provider,
        thinking_intent.as_ref(),
    );

    // A hybrid request (OpenAI messages + Claude tools) still needs the OpenAI
    // shape cleaned up.
    if args.target_format == formats::OPENAI {
        let preserve_cache_control = quirk_truthy(args.provider, "preserveCacheControl");
        filter_to_openai_format(
            &mut body,
            OpenAiFilterOptions {
                preserve_cache_control,
            },
        );
    }

    if args.target_format == formats::CLAUDE {
        prepare_claude_request(
            &mut body,
            &PrepareClaudeArgs {
                provider: args.provider,
                api_key: credentials.bearer(),
                connection_id: args.connection_id,
                raw_headers: Some(&credentials.raw_headers),
                session_id: Some(&client_session_id),
            },
        );
    }

    // Anti-ban cloaking: only providers flagged `cloakToolsOnOAuth`, and only
    // with an OAuth token.
    if quirk_truthy(args.provider, "cloakToolsOnOAuth")
        && credentials
            .bearer()
            .is_some_and(|k| k.contains("sk-ant-oat"))
    {
        let cloaked = cloak_claude_tools(&body);
        body = cloaked.body;
        if let Some(map) = cloaked.tool_name_map {
            meta.tool_name_map = Some(map);
        }
    }

    TranslatedRequest {
        body,
        tool_name_map: meta.tool_name_map,
        custom_tool_names: meta.custom_tool_names,
    }
}

/// The result of `translateResponse`: the client-format chunks, plus the OpenAI
/// intermediate attached to the array for logging.
#[derive(Debug, Default)]
pub struct TranslatedResponse {
    pub chunks: Vec<Value>,
    pub openai_intermediate: Option<Vec<Value>>,
}

/// `translateResponse(targetFormat, sourceFormat, chunk, state)`.
pub fn translate_response(
    target_format: &str,
    source_format: &str,
    chunk: &Value,
    state: &mut ResponseState,
) -> TranslatedResponse {
    // Cloned: the response translators take `state` mutably, so a borrow of
    // `state.tool_name_map` cannot span the calls below.
    let map = state.tool_name_map.clone();

    // Same format still needs decloaking: the request side suffixes client
    // tools even when no format conversion happens, so a streamed tool_use
    // would otherwise reach the client with an unknown name.
    if source_format == target_format {
        let mut c = chunk.clone();
        decloak_stream_chunk(&mut c, map.as_ref());
        restore_tool_names(&mut c, map.as_ref());
        return TranslatedResponse {
            chunks: vec![c],
            openai_intermediate: None,
        };
    }

    if let Some(direct) = registry().response_fn(target_format, source_format) {
        let mut results = direct(chunk, state);
        for r in results.iter_mut() {
            restore_tool_names(r, map.as_ref());
        }
        return TranslatedResponse {
            chunks: results,
            openai_intermediate: None,
        };
    }

    let mut results = vec![chunk.clone()];
    let mut openai_intermediate: Option<Vec<Value>> = None;

    if target_format != formats::OPENAI
        && let Some(to_openai) = registry().response_fn(target_format, formats::OPENAI)
    {
        results = to_openai(chunk, state);
        openai_intermediate = Some(results.clone());
    }

    if source_format != formats::OPENAI
        && let Some(from_openai) = registry().response_fn(formats::OPENAI, source_format)
    {
        let mut final_results: Vec<Value> = Vec::new();
        for r in &results {
            final_results.extend(from_openai(r, state));
        }
        results = final_results;
    }

    for r in results.iter_mut() {
        restore_tool_names(r, map.as_ref());
    }

    // The intermediate is only meaningful when both ends are non-OpenAI.
    let openai_intermediate = match openai_intermediate {
        Some(intermediate)
            if source_format != formats::OPENAI && target_format != formats::OPENAI =>
        {
            Some(intermediate)
        }
        _ => None,
    };

    TranslatedResponse {
        chunks: results,
        openai_intermediate,
    }
}

/// `needsTranslation(sourceFormat, targetFormat)`.
pub fn needs_translation(source_format: &str, target_format: &str) -> bool {
    source_format != target_format
}

/// `stripContentTypes(body, stripList)`.
pub fn strip_content_types(body: &mut Value, strip_list: &[String]) {
    if strip_list.is_empty() {
        return;
    }
    let strip_image = strip_list.iter().any(|s| s == "image");
    let strip_audio = strip_list.iter().any(|s| s == "audio");
    if !strip_image && !strip_audio {
        return;
    }

    let should_strip = |type_: Option<&str>| -> bool {
        match type_ {
            Some(openai_block::IMAGE_URL) | Some(openai_block::IMAGE) => strip_image,
            Some(openai_block::AUDIO_URL) | Some(openai_block::INPUT_AUDIO) => strip_audio,
            _ => false,
        }
    };

    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        content.retain(|part| !should_strip(part.get("type").and_then(Value::as_str)));
        if content.is_empty() {
            msg["content"] = json!("");
        }
    }
}

/// `isLastMessageFromUser(body)`: an absent or empty message list counts as
/// the user's turn.
pub fn is_last_message_from_user(body: &Value) -> bool {
    let messages = body.get("messages").or_else(|| body.get("contents"));
    let Some(Value::Array(items)) = messages else {
        return true;
    };
    let Some(last) = items.last() else {
        return true;
    };
    last.get("role").and_then(Value::as_str) == Some(role::USER)
}

/// `normalizeThinkingConfig(body)`: OpenAI's `reasoning_effort` is
/// request-level and must survive tool-result turns, so only the Claude-shaped
/// `thinking` field is dropped.
pub fn normalize_thinking_config(body: &mut Value) {
    if is_last_message_from_user(body) {
        return;
    }
    if let Some(obj) = body.as_object_mut() {
        obj.shift_remove("thinking");
    }
}

/// Build the initial state for a source format; `openai-responses` seeds the
/// id/created triple.
pub fn init_state(source_format: &str) -> ResponseState {
    let mut state = ResponseState::default();
    if source_format == formats::OPENAI_RESPONSES {
        let now = crate::session_manager::now_ms();
        state.seq = 0;
        state.response_id = format!("resp_{now}");
        state.created = (now / 1000) as i64;
    }
    state
}

/// The streaming state bag: the base fields every translator shares, plus the
/// `openai-responses` extras. Fields a translator invents live in `extra`.
#[derive(Debug, Clone)]
pub struct ResponseState {
    // ── base, for every source format ──────────────────────────────────────
    pub message_id: Option<String>,
    pub model: Option<String>,
    pub text_block_started: bool,
    pub thinking_block_started: bool,
    pub in_thinking_block: bool,
    pub current_block_index: Option<i64>,
    pub tool_calls: Map<String, Value>,
    pub finish_reason: Option<String>,
    pub finish_reason_sent: bool,
    pub usage: Option<Value>,
    pub content_block_index: i64,

    // ── openai-responses extras ────────────────────────────────────────────
    pub seq: i64,
    pub response_id: String,
    pub created: i64,
    pub started: bool,
    pub msg_text_buf: Map<String, Value>,
    pub msg_item_added: Map<String, Value>,
    pub msg_content_added: Map<String, Value>,
    pub msg_item_done: Map<String, Value>,
    pub reasoning_id: String,
    pub reasoning_index: i64,
    pub reasoning_buf: String,
    pub reasoning_part_added: bool,
    pub reasoning_done: bool,
    pub in_thinking: bool,
    pub func_args_buf: Map<String, Value>,
    pub func_names: Map<String, Value>,
    pub func_call_ids: Map<String, Value>,
    pub func_item_added: Map<String, Value>,
    pub func_args_done: Map<String, Value>,
    pub func_item_done: Map<String, Value>,
    pub custom_tool_names: HashSet<String>,
    pub completed_sent: bool,

    // ── wired by the stream handler, read by translators ───────────────────
    pub provider: Option<String>,
    pub target_format: Option<String>,
    pub session_id: Option<String>,
    /// `_toolNameMap` / `toolNameMap`.
    pub tool_name_map: Option<HashMap<String, String>>,

    /// Translator-local scratch (`_toolCallAccum`, `openTools`, …).
    pub extra: Map<String, Value>,
}

impl Default for ResponseState {
    fn default() -> Self {
        Self {
            message_id: None,
            model: None,
            text_block_started: false,
            thinking_block_started: false,
            in_thinking_block: false,
            current_block_index: None,
            tool_calls: Map::new(),
            finish_reason: None,
            finish_reason_sent: false,
            usage: None,
            content_block_index: -1,
            seq: 0,
            response_id: String::new(),
            created: 0,
            started: false,
            msg_text_buf: Map::new(),
            msg_item_added: Map::new(),
            msg_content_added: Map::new(),
            msg_item_done: Map::new(),
            reasoning_id: String::new(),
            reasoning_index: -1,
            reasoning_buf: String::new(),
            reasoning_part_added: false,
            reasoning_done: false,
            in_thinking: false,
            func_args_buf: Map::new(),
            func_names: Map::new(),
            func_call_ids: Map::new(),
            func_item_added: Map::new(),
            func_args_done: Map::new(),
            func_item_done: Map::new(),
            custom_tool_names: HashSet::new(),
            completed_sent: false,
            provider: None,
            target_format: None,
            session_id: None,
            tool_name_map: None,
            extra: Map::new(),
        }
    }
}

impl ResponseState {
    /// `++state.seq`: the next Responses event sequence number.
    pub fn next_seq(&mut self) -> i64 {
        self.seq += 1;
        self.seq
    }

    /// `isCustomTool(state, name)`.
    pub fn is_custom_tool(&self, name: &str) -> bool {
        !name.is_empty() && self.custom_tool_names.contains(name)
    }

    /// Read a scratch value, `null` when absent.
    pub fn scratch(&self, key: &str) -> Value {
        self.extra.get(key).cloned().unwrap_or(Value::Null)
    }

    /// Read a scratch buffer as text.
    pub fn scratch_str(&self, key: &str) -> String {
        self.extra
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// Append to a scratch text buffer, creating it when absent.
    pub fn scratch_push(&mut self, key: &str, text: &str) {
        let existing = self.scratch_str(key);
        self.extra
            .insert(key.into(), json!(format!("{existing}{text}")));
    }

    /// `safeParseJSON(state.<key>, fallback)`.
    pub fn scratch_parse(&self, key: &str, fallback: Value) -> Value {
        safe_parse_json(self.scratch(key), fallback)
    }
}

/// Read a boolean/array quirk off the built transport with JS truthiness.
fn quirk_truthy(provider: Option<&str>, key: &str) -> bool {
    provider
        .and_then(|p| provider_registry().transport(p))
        .and_then(|t| t.quirks.as_ref())
        .and_then(|q| q.get(key))
        .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_content_types_removes_only_opted_in_media() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "keep"},
                {"type": "image_url", "image_url": {"url": "data:x"}},
                {"type": "input_audio", "input_audio": {"data": "y"}},
            ]},
        ]});
        strip_content_types(&mut body, &["image".to_string()]);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "image gone, audio kept");
        assert_eq!(content[0]["type"], json!("text"));
        assert_eq!(content[1]["type"], json!("input_audio"));
    }

    #[test]
    fn strip_content_types_empties_a_message_that_loses_every_part() {
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "image", "source": {}}]},
        ]});
        strip_content_types(&mut body, &["image".to_string()]);
        assert_eq!(body["messages"][0]["content"], json!(""));
    }

    #[test]
    fn strip_content_types_is_a_noop_for_an_empty_or_unrelated_list() {
        let mut body = json!({"messages": [{"role": "user", "content": [{"type": "image"}]}]});
        let before = body.clone();
        strip_content_types(&mut body, &[]);
        assert_eq!(body, before);
        strip_content_types(&mut body, &["video".to_string()]);
        assert_eq!(body, before);
    }

    #[test]
    fn last_message_from_user_handles_missing_and_empty_lists() {
        assert!(is_last_message_from_user(&json!({})));
        assert!(is_last_message_from_user(&json!({"messages": []})));
        assert!(is_last_message_from_user(
            &json!({"messages": [{"role": "user"}]})
        ));
        assert!(!is_last_message_from_user(
            &json!({"messages": [{"role": "assistant"}]})
        ));
        // `contents` is the Gemini spelling.
        assert!(is_last_message_from_user(
            &json!({"contents": [{"role": "user"}]})
        ));
    }

    #[test]
    fn thinking_is_dropped_only_when_the_last_turn_is_not_the_users() {
        let mut body = json!({
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high",
            "messages": [{"role": "assistant"}],
        });
        normalize_thinking_config(&mut body);
        assert!(
            body.get("thinking").is_none(),
            "Claude-shaped thinking dropped"
        );
        assert_eq!(
            body["reasoning_effort"],
            json!("high"),
            "request-level effort survives"
        );

        let mut body = json!({
            "thinking": {"type": "enabled"},
            "messages": [{"role": "user"}],
        });
        normalize_thinking_config(&mut body);
        assert!(body.get("thinking").is_some(), "user's turn keeps it");
    }

    #[test]
    fn init_state_has_the_base_fields_and_responses_extras_only_for_responses() {
        let base = init_state(formats::OPENAI);
        assert_eq!(base.content_block_index, -1);
        assert_eq!(base.reasoning_index, -1);
        assert!(
            base.response_id.is_empty(),
            "no responses id for a chat source"
        );
        assert!(base.tool_calls.is_empty());

        let mut responses = init_state(formats::OPENAI_RESPONSES);
        assert!(responses.response_id.starts_with("resp_"));
        assert!(responses.created > 0);
        assert_eq!(responses.reasoning_index, -1);
        assert!(!responses.started);
        assert_eq!(responses.next_seq(), 1);
        assert_eq!(responses.next_seq(), 2);
    }

    #[test]
    fn needs_translation_is_identity() {
        assert!(!needs_translation(formats::CLAUDE, formats::CLAUDE));
        assert!(needs_translation(formats::CLAUDE, formats::OPENAI));
    }

    #[test]
    fn scratch_helpers_read_write_and_parse() {
        let mut state = ResponseState::default();
        assert_eq!(state.scratch_str("buf"), "");
        state.scratch_push("buf", "a");
        state.scratch_push("buf", "b");
        assert_eq!(state.scratch_str("buf"), "ab");
        state.extra.insert("json".into(), json!("{\"a\":1}"));
        assert_eq!(state.scratch_parse("json", json!(null)), json!({"a": 1}));
        // `safeParseJSON` passes a non-string through untouched, so a missing
        // scratch key yields `null` rather than the caller's fallback.
        assert_eq!(state.scratch_parse("missing", json!("fb")), Value::Null);
    }

    #[test]
    fn custom_tool_names_lookup_rejects_empty() {
        let mut state = ResponseState::default();
        state.custom_tool_names.insert("apply_patch".into());
        assert!(state.is_custom_tool("apply_patch"));
        assert!(!state.is_custom_tool(""));
        assert!(!state.is_custom_tool("other"));
    }

    #[test]
    fn translate_response_same_format_decloaks_tool_names() {
        let mut state = ResponseState::default();
        let mut map = HashMap::new();
        map.insert("MyTool_ide".to_string(), "MyTool".to_string());
        state.tool_name_map = Some(map);

        let chunk = json!({
            "type": "content_block_start",
            "content_block": {"type": "tool_use", "name": "MyTool_ide"},
        });
        let out = translate_response(formats::CLAUDE, formats::CLAUDE, &chunk, &mut state);
        assert_eq!(out.chunks.len(), 1);
        assert_eq!(out.chunks[0]["content_block"]["name"], json!("MyTool"));
        assert!(out.openai_intermediate.is_none());
    }

    #[test]
    fn translate_response_pivots_through_openai() {
        // A Claude upstream to an OpenAI-Responses client: no direct route, so
        // it goes Claude → OpenAI → Responses and the OpenAI intermediate is
        // kept for logging.
        let mut state = ResponseState::default();
        let chunk = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"},
        });
        let out = translate_response(
            formats::CLAUDE,
            formats::OPENAI_RESPONSES,
            &chunk,
            &mut state,
        );
        assert!(!out.chunks.is_empty());
        assert!(
            out.openai_intermediate.is_some(),
            "both ends non-OpenAI keeps it"
        );
    }

    #[test]
    fn translate_response_direct_route_skips_the_pivot() {
        let mut state = ResponseState::default();
        let chunk = json!({"choices": [{"delta": {"content": "x"}}]});
        // `openai:claude` is registered, so this converts in one hop.
        let out = translate_response(formats::OPENAI, formats::CLAUDE, &chunk, &mut state);
        assert_ne!(out.chunks, vec![chunk]);
        assert!(
            out.openai_intermediate.is_none(),
            "no pivot for a direct route"
        );
    }

    #[test]
    fn every_kept_pair_is_registered() {
        let r = registry();
        // The request pairs the registry must hold.
        let request_pairs = [
            (formats::CLAUDE, formats::OPENAI),
            (formats::OPENAI, formats::CLAUDE),
            (formats::OPENAI_RESPONSES, formats::OPENAI),
            (formats::OPENAI, formats::OPENAI_RESPONSES),
            (formats::OPENAI, formats::COMMANDCODE),
        ];
        for (from, to) in request_pairs {
            assert!(
                r.request_fn(from, to).is_some(),
                "missing request {from}:{to}"
            );
        }

        // Response pairs are keyed (provider, client).
        let response_pairs = [
            (formats::CLAUDE, formats::OPENAI),
            (formats::OPENAI, formats::CLAUDE),
            (formats::OPENAI, formats::OPENAI_RESPONSES),
            (formats::OPENAI_RESPONSES, formats::OPENAI),
            (formats::COMMANDCODE, formats::OPENAI),
        ];
        for (from, to) in response_pairs {
            assert!(
                r.response_fn(from, to).is_some(),
                "missing response {from}:{to}"
            );
        }
    }

    #[test]
    fn unregistered_provider_pairs_return_none() {
        let r = registry();
        assert!(r.request_fn(formats::OPENAI, formats::KIRO).is_none());
        assert!(r.request_fn(formats::OPENAI, formats::GEMINI).is_none());
        assert!(r.response_fn(formats::KIRO, formats::OPENAI).is_none());
        assert!(r.response_fn(formats::CURSOR, formats::OPENAI).is_none());
        assert!(r.response_fn(formats::VERTEX, formats::OPENAI).is_none());
    }

    #[test]
    fn registry_lookup_respects_pair_order() {
        let r = registry();
        assert!(r.request_fn(formats::CLAUDE, formats::OPENAI).is_some());
        // The reverse pair is a different key: `claude → openai` exists,
        // `openai → claude` is a separate registration.
        assert!(r.response_fn(formats::OPENAI, formats::CLAUDE).is_some());
        assert!(
            r.request_fn(formats::COMMANDCODE, formats::OPENAI)
                .is_none(),
            "commandcode is a response-only route"
        );
    }
}
