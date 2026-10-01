//! The ChatGPT Codex backend, which speaks the Responses API and gates on a
//! handful of body invariants.
//!
//! Three behaviours here are load-bearing and easy to lose:
//!
//! * **The tool-schema compatibility strip.** `codex/responses` validates every
//!   function tool's `parameters` with a regex engine that has no Unicode
//!   property escapes, so a `pattern` containing `\p{...}` 400s the whole
//!   request on *every* account and the combo pays a full failover before
//!   landing somewhere that accepts it (#3922). The strip is deliberately not a
//!   general schema sanitizer (#3667): only `pattern` strings that actually
//!   carry a property escape are touched.
//! * **The proactive refresh window.** A Codex refresh token can go stale while
//!   still nominally valid, so a refresh also fires once the last one is 8 days
//!   old, from `oauth.codex.maxRefreshAgeMs`.
//! * **The SSE-level retry.** Overload arrives as a 200-OK body carrying
//!   `event: error`, which the status-based retry never sees. The first 256 KB
//!   are peeked and the body re-assembled; capacity errors instead rotate the
//!   account, because retrying the same account cannot help.
//!
//! `execute` is overridden for the image prefetch alone, which has to happen
//! before the body is sent and cannot be done from the synchronous
//! `transform_request`. `BaseLoop` exists so that override still runs against
//! *this* executor's hooks: delegating to `inner.execute` instead would bind
//! every hook to the inner `DefaultExecutor` and silently skip the `/compact`
//! URL, the session and account headers, and the whole body transform.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::future::join_all;
use regex::Regex;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::catalog::get_thinking_levels;
use crate::config::codex_instructions::CODEX_DEFAULT_INSTRUCTIONS;
use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::{
    ByteStream, ExecError, ExecuteRequest, Executor, ExecutorLog, UpstreamBody, UpstreamResponse,
    insert_header,
};
use crate::executors::http::ProxyOptions;
use crate::executors::oauth::{RefreshedCredentials, should_refresh_credentials};
use crate::executors::retry::RetryConfig;
use crate::providers::lookup::model_upstream_id;
use crate::providers::model::Transport;
use crate::runtime_config::http_status;
use crate::session_manager::{SessionIdentityInput, now_ms, resolve_session_id};
use crate::translator::concerns::image::fetch_image_as_base64;
use crate::translator::concerns::primitives::js_truthy;
use crate::translator::formats::responses_api::normalize_responses_input;

/// SSE error patterns inside 200-OK bodies. Some retry same account first;
/// capacity rotates accounts.
const CODEX_SSE_RETRY_PATTERNS: [&str; 2] = ["server_is_overloaded", "service_unavailable_error"];
const CODEX_SSE_ACCOUNT_FALLBACK_PATTERNS: [&str; 2] =
    ["selected model is at capacity", "model_at_capacity"];
const CODEX_SSE_USER_OUTPUT_PATTERNS: [&str; 4] = [
    "event: response.output_text.delta",
    "event: response.function_call_arguments.delta",
    "\"type\":\"response.output_text.delta\"",
    "\"type\":\"response.function_call_arguments.delta\"",
];
const CODEX_SSE_PEEK_BYTES: usize = 256 * 1024;
const CODEX_MODEL_CAPACITY_MESSAGE: &str =
    "Selected model is at capacity. Please try a different model.";

/// Server-generated item id prefixes that Codex `/responses` cannot resolve
/// when `store=false`.
static SERVER_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(rs|fc|resp|msg)_").expect("static pattern"));

/// Hosted tool types that Codex/OpenAI Responses executes server-side.
const CODEX_HOSTED_TOOL_TYPES: [&str; 10] = [
    "image_generation",
    "web_search",
    "web_search_preview",
    "file_search",
    "computer",
    "computer_use_preview",
    "code_interpreter",
    "mcp",
    "local_shell",
    "tool_search",
];

/// Responses-native freeform tools carry a name plus format payload and must
/// pass through intact.
const CODEX_PASSTHROUGH_TOOL_TYPES: [&str; 1] = ["custom"];

/// Allowlist of fields accepted by Codex Responses API — anything else is
/// stripped, because an unknown field surfaces upstream as `routing_unsupported`.
const RESPONSES_API_ALLOWLIST: [&str; 13] = [
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "service_tier",
    "include",
    "prompt_cache_key",
    "client_metadata",
    "text",
];

/// The effort levels a model-name suffix may carry, in match order.
const EFFORT_LEVELS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

// ─── tool-schema compatibility ───────────────────────────────────────────

/// `\p{...}` / `\P{...}` with an odd number of preceding backslashes — an even
/// count means the backslash itself is escaped, so `\\p{Cc}` is a literal "p".
static UNICODE_PROPERTY_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^\\])(\\\\)*\\[pP]\{").expect("static pattern"));

pub fn has_unicode_property_escape(pattern: &str) -> bool {
    UNICODE_PROPERTY_ESCAPE.is_match(pattern)
}

/// Returns the cleaned schema and the number of `pattern` keys removed. The walk
/// is copy-on-write so an untouched schema keeps object identity and a caller
/// can cheaply detect a no-op; Rust has no identity to preserve, so the count is
/// the signal.
///
/// Everything except a `pattern` carrying a property escape passes through
/// untouched, and `properties` is special-cased because its keys are arbitrary
/// property *names* (which may themselves be `pattern` or `properties`) and must
/// never be read as schema keywords.
pub fn strip_codex_unsupported_patterns(schema: Value) -> (Value, u64) {
    fn walk(node: Value, removed: &mut u64) -> Value {
        match node {
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|i| walk(i, removed)).collect())
            }
            Value::Object(map) => {
                let mut next = Map::new();
                for (key, value) in map {
                    if key == "pattern"
                        && let Value::String(pattern) = &value
                        && has_unicode_property_escape(pattern)
                    {
                        *removed += 1;
                        continue;
                    }
                    if key == "properties"
                        && let Value::Object(props) = value
                    {
                        let mut cleaned = Map::new();
                        for (name, prop_schema) in props {
                            cleaned.insert(name, walk(prop_schema, removed));
                        }
                        next.insert(key, Value::Object(cleaned));
                        continue;
                    }
                    next.insert(key, walk(value, removed));
                }
                Value::Object(next)
            }
            other => other,
        }
    }

    let mut removed = 0u64;
    let cleaned = walk(schema, &mut removed);
    (cleaned, removed)
}

// ─── body transforms ─────────────────────────────────────────────────────

/// Keeps system prompts in the cacheable prefix, which `role: "system"` inside
/// `input[]` does not.
fn convert_system_to_developer_role(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items.iter_mut() {
        let Some(obj) = item.as_object_mut() else {
            continue;
        };
        // A falsy `type` counts as absent, so `""` and `null` convert too.
        let type_is_message = match obj.get("type") {
            None => true,
            Some(t) => !js_truthy(t) || t.as_str() == Some("message"),
        };
        let is_system =
            obj.get("role").and_then(Value::as_str) == Some("system") && type_is_message;
        if is_system {
            obj.insert("role".into(), json!("developer"));
        }
    }
}

/// Server-generated ids cannot be resolved with `store=false`, so referencing
/// one is a 404.
fn strip_stored_item_references(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    items.retain_mut(|item| {
        if let Value::String(s) = item {
            return !SERVER_ID_PATTERN.is_match(s);
        }
        if let Some(obj) = item.as_object_mut() {
            if obj.get("type").and_then(Value::as_str) == Some("item_reference") {
                return false;
            }
            if let Some(id) = obj.get("id").and_then(Value::as_str)
                && SERVER_ID_PATTERN.is_match(id)
            {
                obj.shift_remove("id");
            }
        }
        true
    });
}

/// The `{ type: "object", properties: {} }` a tool with no declared parameters
/// gets.
fn empty_object_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Flatten the Chat-Completions tool shape into the Responses flat format and
/// drop the types Codex cannot execute.
fn normalize_codex_tools(body: &mut Value) {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    let mut valid_names: HashSet<String> = HashSet::new();
    let mut removed_patterns = 0u64;

    let mut kept: Vec<Value> = Vec::with_capacity(tools.len());
    for tool in tools.drain(..) {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let tool_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if tool_type == "namespace" {
            let mut namespace = obj.clone();
            if let Some(children) = namespace.get_mut("tools").and_then(Value::as_array_mut) {
                for child in children.iter_mut() {
                    let Some(child_obj) = child.as_object_mut() else {
                        continue;
                    };
                    // This branch registers the name as-is, unlike the function
                    // branch below.
                    let name = child_obj
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| n.trim().chars().take(128).collect::<String>())
                        .unwrap_or_default();
                    if !name.is_empty() {
                        valid_names.insert(name);
                    }
                    if let Some(parameters) = child_obj.get("parameters").cloned()
                        && parameters.is_object()
                    {
                        let (cleaned, removed) = strip_codex_unsupported_patterns(parameters);
                        removed_patterns += removed;
                        child_obj.insert("parameters".into(), cleaned);
                    }
                }
            }
            kept.push(Value::Object(namespace));
            continue;
        }

        if tool_type != "function" {
            if CODEX_PASSTHROUGH_TOOL_TYPES.contains(&tool_type) {
                kept.push(tool);
                continue;
            }
            // Codex requires an explicit `type: "function"`: a nested
            // `function` object or a bare `name` is not flattened here, it is
            // dropped. What survives this branch is a hosted tool.
            let untyped_or_nested = tool_type.is_empty()
                || obj.get("function").is_some_and(js_truthy)
                || obj.get("name").is_some_and(Value::is_string);
            if untyped_or_nested {
                continue;
            }
            if CODEX_HOSTED_TOOL_TYPES.contains(&tool_type) {
                kept.push(tool);
            }
            continue;
        }

        let function = obj.get("function").and_then(Value::as_object);
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| function.and_then(|f| f.get("name")).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            continue;
        }

        let description = obj
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| {
                function
                    .and_then(|f| f.get("description"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let parameters = obj
            .get("parameters")
            .filter(|p| p.is_object())
            .or_else(|| {
                function
                    .and_then(|f| f.get("parameters"))
                    .filter(|p| p.is_object())
            })
            .cloned()
            .unwrap_or_else(empty_object_schema);
        let (parameters, removed) = strip_codex_unsupported_patterns(parameters);
        removed_patterns += removed;

        // The tool is rebuilt from scratch rather than patched, so fields like
        // `strict` and `function` never reach the upstream.
        let mut flattened = Map::new();
        flattened.insert("type".into(), json!("function"));
        flattened.insert(
            "name".into(),
            json!(name.chars().take(128).collect::<String>()),
        );
        if !description.is_empty() {
            flattened.insert("description".into(), json!(description));
        }
        flattened.insert("parameters".into(), parameters);
        // The untruncated trimmed name is what `tool_choice` is validated
        // against.
        valid_names.insert(name);
        kept.push(Value::Object(flattened));
    }

    if removed_patterns > 0 {
        tracing::debug!(
            target: "router_sse::executor",
            "CODEX stripped {removed_patterns} unsupported tool schema pattern(s)"
        );
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("tools".into(), Value::Array(kept));
    }

    // Drop tool_choice if it references an unknown function name.
    let unknown_choice = body
        .get("tool_choice")
        .and_then(Value::as_object)
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("function"))
        .is_some_and(|c| {
            let name = c.get("name").and_then(Value::as_str).unwrap_or("").trim();
            name.is_empty() || !valid_names.contains(name)
        });
    if unknown_choice && let Some(obj) = body.as_object_mut() {
        obj.shift_remove("tool_choice");
    }
}

/// Resolve the conversation-stable session id: client session, then
/// assistant-text hash, then workspace, then connection.
fn resolve_cache_session_id(body: &Value, credentials: &Credentials) -> String {
    resolve_session_id(&SessionIdentityInput {
        headers: &credentials.raw_headers,
        body,
        connection_id: credentials.connection_id.as_deref(),
        workspace_id: credentials.psd_str("workspaceId"),
        scope: "codex",
    })
}

/// Clamp the requested level to the one the model actually supports, folding the
/// virtual `ultra`/`max` levels onto what Codex accepts.
fn normalize_reasoning_effort(model: &str, value: &str) -> String {
    let supported = get_thinking_levels(Some("codex"), model);
    let supports = |level: &str| {
        supported
            .as_ref()
            .is_some_and(|levels| levels.contains(&level))
    };
    if supports(value) {
        return value.to_string();
    }
    if value == "ultra" && supports("max") {
        return "max".to_string();
    }
    if value == "max" || value == "ultra" {
        return "xhigh".to_string();
    }
    value.to_string()
}

// ─── SSE peek ────────────────────────────────────────────────────────────

/// The first `message` the payload carries, however deeply the upstream wrapped
/// it.
fn find_nested_message(value: &Value, depth: usize) -> Option<String> {
    if depth > 6 {
        return None;
    }
    match value {
        Value::Array(items) => items
            .iter()
            .find_map(|item| find_nested_message(item, depth + 1)),
        Value::Object(map) => {
            let non_empty = |v: Option<&Value>| {
                v.and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            };
            if let Some(message) = non_empty(map.get("message")) {
                return Some(message);
            }
            if let Some(message) = non_empty(map.get("error").and_then(|e| e.get("message"))) {
                return Some(message);
            }
            if let Some(message) = non_empty(
                map.get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message")),
            ) {
                return Some(message);
            }
            map.values()
                .find_map(|child| find_nested_message(child, depth + 1))
        }
        _ => None,
    }
}

fn extract_sse_error_message(text: &str, fallback: &str) -> String {
    static CAPACITY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Selected model is at capacity\. Please try a different model\.")
            .expect("static pattern")
    });
    if let Some(m) = CAPACITY.find(text) {
        return m.as_str().to_string();
    }

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(data)
            && let Some(message) = find_nested_message(&parsed, 0)
        {
            return message;
        }
    }

    if fallback.is_empty() {
        CODEX_MODEL_CAPACITY_MESSAGE.to_string()
    } else {
        fallback.to_string()
    }
}

/// What the peek found, plus the body the caller must use either way.
struct Peek {
    matched: Option<String>,
    message: Option<String>,
    account_fallback: bool,
    body: UpstreamBody,
}

/// Read at most `CODEX_SSE_PEEK_BYTES` of a 200-OK body looking for an overload
/// the status never showed.
///
/// When nothing matches, the bytes already read are re-assembled in front of
/// the remaining upstream stream — the original body has been consumed by then,
/// so the caller has to take `body` back.
async fn peek_sse_transient_error(body: UpstreamBody) -> Peek {
    let unchanged = |body: UpstreamBody| Peek {
        matched: None,
        message: None,
        account_fallback: false,
        body,
    };

    let response = match body {
        UpstreamBody::Stream(response) => response,
        other => return unchanged(other),
    };
    if !response.status().is_success() {
        return unchanged(UpstreamBody::Stream(response));
    }

    let mut stream = response.bytes_stream();
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut text = String::new();
    let mut matched: Option<String> = None;
    let mut account_fallback = false;

    while text.len() < CODEX_SSE_PEEK_BYTES {
        match stream.next().await {
            Some(Ok(chunk)) => {
                text.push_str(&String::from_utf8_lossy(&chunk));
                chunks.push(chunk);
            }
            // A read error: keep what was read.
            _ => break,
        }

        let lower = text.to_lowercase();
        if let Some(hit) = CODEX_SSE_ACCOUNT_FALLBACK_PATTERNS
            .iter()
            .find(|p| lower.contains(**p))
        {
            matched = Some((*hit).to_string());
            account_fallback = true;
            break;
        }
        if let Some(hit) = CODEX_SSE_RETRY_PATTERNS
            .iter()
            .find(|p| lower.contains(**p))
        {
            matched = Some((*hit).to_string());
            break;
        }
        if CODEX_SSE_USER_OUTPUT_PATTERNS
            .iter()
            .any(|p| lower.contains(p))
        {
            break;
        }
    }

    if let Some(matched) = matched {
        // Decode the whole prefix as one buffer: a multi-byte character split
        // across chunks would mangle under a per-chunk decode.
        let raw: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
        let full = String::from_utf8_lossy(&raw);
        // Dropping the stream cancels the upstream read.
        return Peek {
            message: Some(extract_sse_error_message(&full, &matched)),
            matched: Some(matched),
            account_fallback,
            body: UpstreamBody::Buffered(Bytes::new()),
        };
    }

    let rest: ByteStream = Box::pin(stream.map(|r| r.map_err(std::io::Error::other)));
    let combined: ByteStream = Box::pin(
        futures::stream::iter(chunks.into_iter().map(Ok::<Bytes, std::io::Error>)).chain(rest),
    );
    unchanged(UpstreamBody::Synthesized(combined))
}

/// The error shape the account-fallback layer reads to decide whether to rotate
/// the credential.
fn codex_sse_error_response(
    mut result: UpstreamResponse,
    status: u16,
    message: &str,
) -> UpstreamResponse {
    let body = json!({
        "error": {
            "message": message,
            "type": if status >= 500 { "server_error" } else { "invalid_request_error" },
            "code": if status == http_status::SERVICE_UNAVAILABLE { "service_unavailable" } else { "upstream_error" },
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    result.status = status;
    result.headers = headers;
    result.body = UpstreamBody::Buffered(Bytes::from(body.to_string()));
    result
}

// ─── image prefetch ──────────────────────────────────────────────────────

/// One `content[]` entry, with a remote `image_url` replaced by an inlined
/// `input_image`. The Codex backend cannot fetch remote images itself.
async fn prefetch_image_part(part: Value) -> Value {
    if part.get("type").and_then(Value::as_str) != Some("image_url") {
        return part;
    }
    let url = match part.get("image_url") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    let detail = part
        .get("image_url")
        .and_then(|i| i.get("detail"))
        .and_then(Value::as_str)
        .filter(|d| !d.is_empty())
        .unwrap_or("auto")
        .to_string();
    if url.is_empty() {
        return part;
    }
    if url.starts_with("data:") {
        return json!({"type": "input_image", "image_url": url, "detail": detail});
    }
    let inlined = match fetch_image_as_base64(&url).await {
        Some(fetched) => fetched.url,
        None => url,
    };
    json!({"type": "input_image", "image_url": inlined, "detail": detail})
}

/// Mutates `body.input` in place, so the retry loop never refetches what it
/// already inlined.
async fn prefetch_images(body: &mut Value) {
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in items.iter_mut() {
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let resolved = join_all(content.iter().cloned().map(prefetch_image_part)).await;
        *content = resolved;
    }
}

/// `body.input[*].content[*]` entries typed `image_url`.
fn count_image_parts(body: &Value) -> usize {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.get("content")
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts
                                .iter()
                                .filter(|c| {
                                    c.get("type").and_then(Value::as_str) == Some("image_url")
                                })
                                .count()
                        })
                        .unwrap_or(0)
                })
                .sum::<usize>()
        })
        .unwrap_or(0)
}

// ─── executor ────────────────────────────────────────────────────────────

/// The Responses-shaped Codex backend.
pub struct CodexExecutor {
    /// Used for the transport, the URL and the OAuth refresh. The body and
    /// header hooks are this executor's own.
    inner: DefaultExecutor,
    /// Set by `transform_request`, read by `build_url`.
    is_compact: AtomicBool,
    /// Set by `transform_request`, read by `build_headers`. The base loop calls
    /// the two hooks in that order.
    session_id: Mutex<Option<String>>,
}

impl CodexExecutor {
    pub fn new() -> Self {
        Self {
            inner: DefaultExecutor::new("codex"),
            is_compact: AtomicBool::new(false),
            session_id: Mutex::new(None),
        }
    }

    fn current_session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .filter(|s| !s.is_empty())
    }
}

impl Default for CodexExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for CodexExecutor {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn config(&self) -> &Transport {
        self.inner.config()
    }

    /// `/compact` is a separate endpoint rather than a body flag.
    fn build_url(
        &self,
        model: &str,
        stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        let base = self
            .inner
            .build_url(model, stream, url_index, credentials)?;
        Ok(if self.is_compact.load(Ordering::Relaxed) {
            format!("{base}/compact")
        } else {
            base
        })
    }

    /// Adds the Codex identity headers. `transform_request` runs BEFORE
    /// `build_headers`, which is what makes the session id available here.
    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        model: &str,
        body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        let mut headers = self
            .inner
            .build_headers(credentials, stream, url, model, body)?;

        let session_id = self
            .current_session_id()
            .or_else(|| credentials.connection_id.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "default".to_string());
        insert_header(&mut headers, "session_id", &session_id)?;

        // Identify the client type to the Codex backend (matches the official
        // codex CLI).
        if !headers.contains_key("originator") {
            insert_header(&mut headers, "originator", "codex_cli_rs")?;
        }

        // Account/workspace binding — required when multiple Codex accounts are
        // configured. OAuth import stores the ChatGPT account id as
        // `chatgptAccountId`; older or custom rows may use `workspaceId` /
        // `accountId`. Prefer the explicit workspace but fall back, so requests
        // don't cross-bind to the wrong OpenAI account and surface as
        // `token_invalid` after adding another account.
        let account_id = credentials
            .psd_str("workspaceId")
            .or_else(|| credentials.psd_str("chatgptAccountId"))
            .or_else(|| credentials.psd_str("accountId"))
            .filter(|s| !s.is_empty());
        if let Some(account_id) = account_id
            && !headers.contains_key("ChatGPT-Account-ID")
        {
            insert_header(&mut headers, "ChatGPT-Account-ID", account_id)?;
        }

        Ok(headers)
    }

    /// No refresh token, no refresh.
    async fn refresh_credentials(
        &self,
        credentials: &Credentials,
        log: Option<&dyn ExecutorLog>,
        proxy_options: &ProxyOptions,
    ) -> Option<RefreshedCredentials> {
        credentials.refresh_token.as_ref()?;
        self.inner
            .refresh_credentials(credentials, log, proxy_options)
            .await
    }

    /// An explicit expiry inside the lead window, or the proactive stale-token
    /// window. The 8-day age comes from `codex.oauth.maxRefreshAgeMs` in the
    /// registry rather than a second constant that could drift.
    fn needs_refresh(&self, credentials: &Credentials) -> bool {
        should_refresh_credentials("codex", credentials, now_ms() as i64)
    }

    /// `usage_limit_reached` carries the precise reset time, which the fallback
    /// layer uses instead of a default cooldown.
    fn parse_error(&self, status: u16, body_text: &str) -> Value {
        if status == http_status::RATE_LIMITED
            && !body_text.is_empty()
            && let Ok(parsed) = serde_json::from_str::<Value>(body_text)
            && let Some(error) = parsed.get("error")
            && error.get("type").and_then(Value::as_str) == Some("usage_limit_reached")
        {
            let now = now_ms() as f64;
            let mut resets_at_ms: Option<f64> = None;
            if let Some(seconds) = error.get("resets_at").and_then(Value::as_f64)
                && seconds > 0.0
            {
                let ms = seconds * 1000.0;
                if ms > now {
                    resets_at_ms = Some(ms);
                }
            }
            if resets_at_ms.is_none()
                && let Some(seconds) = error.get("resets_in_seconds").and_then(Value::as_f64)
                && seconds > 0.0
            {
                resets_at_ms = Some(now + seconds * 1000.0);
            }
            if let Some(resets_at_ms) = resets_at_ms {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(body_text);
                return json!({"status": status, "message": message, "resetsAtMs": resets_at_ms});
            }
        }
        self.inner.parse_error(status, body_text)
    }

    /// Image fetching is handled separately in `prefetch_images` so this stays
    /// sync.
    fn transform_request(
        &self,
        model: &str,
        mut body: Value,
        _stream: bool,
        credentials: &Credentials,
    ) -> Value {
        let compact = body.get("_compact").is_some_and(js_truthy);
        self.is_compact.store(compact, Ordering::Relaxed);
        if let Some(obj) = body.as_object_mut() {
            obj.shift_remove("_compact");
        }
        if !body.is_object() {
            return body;
        }

        let session_id = resolve_cache_session_id(&body, credentials);
        if let Ok(mut guard) = self.session_id.lock() {
            *guard = Some(session_id.clone());
        }

        if let Some(normalized) = normalize_responses_input(body.get("input")) {
            body["input"] = Value::Array(normalized);
        }

        // The Codex API rejects an empty input.
        let input_empty = match body.get("input") {
            Some(Value::Array(items)) => items.is_empty(),
            Some(other) => !js_truthy(other),
            None => true,
        };
        if input_empty {
            body["input"] = json!([{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "..."}],
            }]);
        }

        convert_system_to_developer_role(&mut body);
        strip_stored_item_references(&mut body);
        normalize_codex_tools(&mut body);

        body["stream"] = json!(true);

        let needs_instructions = match body.get("instructions") {
            Some(Value::String(s)) => s.trim().is_empty(),
            Some(other) => !js_truthy(other),
            None => true,
        };
        if needs_instructions {
            body["instructions"] = json!(CODEX_DEFAULT_INSTRUCTIONS);
        }

        body["store"] = json!(false);

        let has_cache_key = body.get("prompt_cache_key").is_some_and(js_truthy);
        if !has_cache_key && !session_id.is_empty() {
            body["prompt_cache_key"] = json!(session_id);
        }

        // Map the virtual Codex review models onto the upstream model before
        // the suffix parsing below.
        let requested = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(model)
            .to_string();
        body["model"] = json!(model_upstream_id("cx", &requested));

        // Extract the thinking level from the model-name suffix, e.g.
        // `gpt-5.3-codex-high` → high, `gpt-5.3-codex` → default.
        let mut model_effort: Option<String> = None;
        if let Some(current) = body.get("model").and_then(Value::as_str) {
            for level in EFFORT_LEVELS {
                let suffix = format!("-{level}");
                if current.ends_with(&suffix) {
                    model_effort = Some(level.to_string());
                    let stripped = current.replacen(&suffix, "", 1);
                    body["model"] = json!(stripped);
                    break;
                }
            }
        }
        let resolved_model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        // Priority: explicit reasoning.effort > reasoning_effort param > model
        // suffix > default.
        let requested_effort = body
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(model_effort);
        let reasoning_is_truthy = body.get("reasoning").is_some_and(js_truthy);
        if !reasoning_is_truthy {
            let effort = normalize_reasoning_effort(
                &resolved_model,
                requested_effort.as_deref().unwrap_or("low"),
            );
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
        } else if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
            if let Some(Value::String(existing)) = reasoning.get("effort").cloned() {
                let effort = normalize_reasoning_effort(&resolved_model, &existing);
                reasoning.insert("effort".into(), json!(effort));
            }
            if !reasoning.get("summary").is_some_and(js_truthy) {
                reasoning.insert("summary".into(), json!("auto"));
            }
        }
        if let Some(obj) = body.as_object_mut() {
            obj.shift_remove("reasoning_effort");
        }

        // Encrypted reasoning content is required by the Codex backend for
        // reasoning models.
        let wants_encrypted = body
            .get("reasoning")
            .and_then(|r| r.get("effort"))
            .is_some_and(|e| js_truthy(e) && e.as_str() != Some("none"));
        if wants_encrypted {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }

        if let Some(obj) = body.as_object_mut() {
            // Parameters Codex rejects outright. Several arrive from clients
            // that speak the full Responses API (Cursor, Droid) or from the
            // chat-completions shape.
            for key in [
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
                "logprobs",
                "top_logprobs",
                "n",
                "seed",
                "max_tokens",
                "max_completion_tokens",
                "max_output_tokens",
                "user",
                "prompt_cache_retention",
                "metadata",
                "stream_options",
                "safety_identifier",
                // `store=false` → the backend cannot resolve a previous
                // response, so referencing one is a 404.
                "previous_response_id",
            ] {
                obj.shift_remove(key);
            }

            if obj.get("service_tier").and_then(Value::as_str) == Some("fast") {
                obj.insert("service_tier".into(), json!("priority"));
            }
            // A truthy non-string tier is deleted too, not just a wrong string.
            let wrong_tier = obj
                .get("service_tier")
                .is_some_and(|v| js_truthy(v) && v.as_str() != Some("priority"));
            if wrong_tier {
                obj.shift_remove("service_tier");
            }

            // Final allowlist filter — an unknown field surfaces upstream as
            // `routing_unsupported`.
            let unknown: Vec<String> = obj
                .keys()
                .filter(|k| !RESPONSES_API_ALLOWLIST.contains(&k.as_str()))
                .cloned()
                .collect();
            for key in unknown {
                obj.shift_remove(&key);
            }
        }

        body
    }

    /// Prefetch images once, then run the base loop wrapped in the SSE-level
    /// retry.
    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        // The request is rebuilt per SSE retry rather than consumed once: the
        // base loop takes the body and the transformed copy, and only the
        // prefetched bytes must survive into the next attempt.
        let ExecuteRequest {
            model,
            mut body,
            stream,
            credentials,
            cancel,
            log,
            proxy_options,
            provider_session_id,
            client_tool,
        } = req;

        let image_count = count_image_parts(&body);
        let input_len = body
            .get("input")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let session = self
            .current_session_id()
            .unwrap_or_else(|| "pending".to_string());
        tracing::debug!(
            target: "router_sse::executor",
            "CODEX execute start | inputItems={input_len} | images={image_count} | sessionId={session}"
        );

        let started = Instant::now();
        prefetch_images(&mut body).await;
        if image_count > 0 {
            tracing::debug!(
                target: "router_sse::executor",
                "CODEX prefetchImages done | {}ms",
                started.elapsed().as_millis()
            );
        }

        // Retry loop for SSE-level overloaded errors (a 200-OK body carrying
        // `event: error`). Reuses the 503 retry config — same semantic: the
        // upstream is temporarily unavailable.
        let retry_config = RetryConfig::merged(self.config().retry.as_ref());
        let entry = retry_config.entry(http_status::SERVICE_UNAVAILABLE);
        let (attempts, delay_ms) = (entry.attempts, entry.delay_ms);
        let mut attempt = 0u32;

        loop {
            let attempt_req = ExecuteRequest {
                model,
                body: body.clone(),
                stream,
                credentials,
                cancel: cancel.clone(),
                log,
                proxy_options: proxy_options.clone(),
                provider_session_id,
                client_tool,
            };
            let mut result = BaseLoop { outer: self }.execute(attempt_req).await?;

            let peek = peek_sse_transient_error(result.take_body()).await;
            let Some(matched) = peek.matched.clone() else {
                result.body = peek.body;
                return Ok(result);
            };

            if peek.account_fallback {
                let message = peek
                    .message
                    .unwrap_or_else(|| CODEX_MODEL_CAPACITY_MESSAGE.to_string());
                tracing::warn!(
                    target: "router_sse::executor",
                    "RETRY CODEX | SSE account fallback \"{message}\""
                );
                return Ok(codex_sse_error_response(
                    result,
                    http_status::SERVICE_UNAVAILABLE,
                    &message,
                ));
            }

            if attempt >= attempts {
                tracing::warn!(
                    target: "router_sse::executor",
                    "RETRY CODEX | SSE overloaded \"{matched}\" — retries exhausted ({attempt}/{attempts})"
                );
                let message = peek.message.unwrap_or(matched);
                return Ok(codex_sse_error_response(
                    result,
                    http_status::SERVICE_UNAVAILABLE,
                    &message,
                ));
            }

            attempt += 1;
            if let Some(log) = log {
                log.debug(
                    "RETRY",
                    &format!(
                        "CODEX | SSE \"{matched}\" retry {attempt}/{attempts} after {}s",
                        delay_ms / 1000
                    ),
                );
            }
            tracing::debug!(
                target: "router_sse::executor",
                "CODEX SSE overloaded \"{matched}\" → retry {attempt}/{attempts} in {delay_ms}ms"
            );
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
}

/// A view of a `CodexExecutor` that inherits the trait's default `execute`.
///
/// Delegating to the inner `DefaultExecutor`
/// instead would run the base loop against the *inner* hooks, so `/compact`,
/// the Codex headers and the whole body transform would never be applied. The
/// adapter keeps the loop single-sourced in `executor.rs` while resolving every
/// hook on the outer executor.
///
/// No `#[async_trait]` here: the impl overrides only the synchronous hooks and
/// inherits the trait's default `execute`.
struct BaseLoop<'a> {
    outer: &'a CodexExecutor,
}

impl Executor for BaseLoop<'_> {
    fn provider(&self) -> &str {
        self.outer.provider()
    }

    fn config(&self) -> &Transport {
        self.outer.config()
    }

    fn build_url(
        &self,
        model: &str,
        stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        self.outer.build_url(model, stream, url_index, credentials)
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        model: &str,
        body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        self.outer
            .build_headers(credentials, stream, url, model, body)
    }

    fn transform_request(
        &self,
        model: &str,
        body: Value,
        stream: bool,
        credentials: &Credentials,
    ) -> Value {
        self.outer
            .transform_request(model, body, stream, credentials)
    }

    fn should_retry(&self, status: u16, url_index: usize) -> bool {
        self.outer.should_retry(status, url_index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_escapes_are_detected_only_with_an_odd_backslash_run() {
        assert!(has_unicode_property_escape(r"^\p{Cc}$"));
        assert!(has_unicode_property_escape(r"[^\p{Cc}\p{Cf}]"));
        assert!(has_unicode_property_escape(r"\P{L}"));
        // An even run means the backslash itself is escaped: a literal "p".
        assert!(!has_unicode_property_escape(r"\\p{Cc}"));
        assert!(!has_unicode_property_escape(r"^\w+$"));
        assert!(!has_unicode_property_escape("plain"));
    }

    #[test]
    fn the_strip_removes_only_unsupported_patterns() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "pattern": r"^\p{Cc}$"},
                "name": {"type": "string", "pattern": "^[a-z]+$"},
            },
        });
        let (cleaned, removed) = strip_codex_unsupported_patterns(schema);
        assert_eq!(removed, 1);
        assert!(cleaned["properties"]["path"].get("pattern").is_none());
        assert_eq!(cleaned["properties"]["name"]["pattern"], json!("^[a-z]+$"));
    }

    #[test]
    fn a_property_named_pattern_is_not_read_as_a_schema_keyword() {
        // `properties` keys are arbitrary property names, not schema keywords.
        let schema = json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "pattern": r"\p{L}"},
            },
        });
        let (cleaned, removed) = strip_codex_unsupported_patterns(schema);
        assert_eq!(removed, 1);
        assert_eq!(cleaned["properties"]["pattern"]["type"], json!("string"));
    }

    #[test]
    fn system_messages_become_developer_messages() {
        let mut body = json!({"input": [
            {"role": "system", "content": "keep me cacheable"},
            {"type": "message", "role": "system", "content": "me too"},
            {"type": "function_call", "role": "system", "content": "not a message"},
            {"role": "user", "content": "hello"},
        ]});
        convert_system_to_developer_role(&mut body);
        assert_eq!(body["input"][0]["role"], json!("developer"));
        assert_eq!(body["input"][1]["role"], json!("developer"));
        assert_eq!(
            body["input"][2]["role"],
            json!("system"),
            "only message items convert"
        );
        assert_eq!(body["input"][3]["role"], json!("user"));
    }

    #[test]
    fn stored_item_references_are_dropped_and_local_ids_kept() {
        let mut body = json!({"input": [
            "rs_abc",
            {"type": "item_reference", "id": "rs_1"},
            {"type": "message", "id": "resp_1", "role": "user"},
            {"type": "message", "id": "local-1", "role": "user"},
        ]});
        strip_stored_item_references(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(
            items.len(),
            2,
            "a bare id string and an item_reference are dropped"
        );
        assert!(
            items[0].get("id").is_none(),
            "a server-generated id is stripped"
        );
        assert_eq!(items[1]["id"], json!("local-1"));
    }

    #[test]
    fn tools_flatten_to_the_flat_function_shape() {
        let mut body = json!({"tools": [
            {"type": "function", "function": {"name": "  read_file ", "description": "d", "parameters": {"type": "object"}}},
            {"type": "web_search"},
            {"type": "custom", "name": "freeform"},
            {"type": "nonsense"},
        ]});
        normalize_codex_tools(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 3, "an unknown hosted type is dropped");
        assert_eq!(tools[0]["name"], json!("read_file"), "the name is trimmed");
        assert_eq!(tools[0]["type"], json!("function"));
        assert!(tools[0].get("function").is_none());
        assert_eq!(
            tools[1]["type"],
            json!("web_search"),
            "hosted tools pass through"
        );
        assert_eq!(tools[2]["name"], json!("freeform"));
    }

    #[test]
    fn an_untyped_or_nested_tool_is_dropped_not_flattened() {
        // Codex requires an explicit `type: "function"`; only the Grok executor
        // flattens the nested shape.
        let mut body = json!({"tools": [
            {"name": "bare"},
            {"function": {"name": "nested"}},
            {"type": "function", "name": "proper"},
        ]});
        normalize_codex_tools(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("proper"));
    }

    #[test]
    fn a_tool_choice_naming_an_unknown_function_is_dropped() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "known"}],
            "tool_choice": {"type": "function", "name": "missing"},
        });
        normalize_codex_tools(&mut body);
        assert!(body.get("tool_choice").is_none());

        let mut body = json!({
            "tools": [{"type": "function", "name": "known"}],
            "tool_choice": {"type": "function", "name": "known"},
        });
        normalize_codex_tools(&mut body);
        assert_eq!(body["tool_choice"]["name"], json!("known"));
    }

    #[test]
    fn sse_error_messages_prefer_the_capacity_text_then_the_nested_message() {
        assert_eq!(
            extract_sse_error_message(
                "blah Selected model is at capacity. Please try a different model. blah",
                "fb"
            ),
            "Selected model is at capacity. Please try a different model."
        );
        assert_eq!(
            extract_sse_error_message("data: {\"error\":{\"message\":\"deep\"}}\n", "fb"),
            "deep"
        );
        assert_eq!(extract_sse_error_message("data: [DONE]\n", "fb"), "fb");
        assert_eq!(
            extract_sse_error_message("", ""),
            CODEX_MODEL_CAPACITY_MESSAGE
        );
    }

    #[test]
    fn nested_message_search_is_bounded_and_skips_non_objects() {
        assert_eq!(
            find_nested_message(&json!({"a": {"b": {"message": " found "}}}), 0),
            Some(" found ".into())
        );
        assert_eq!(find_nested_message(&json!({"message": "   "}), 0), None);
        assert_eq!(find_nested_message(&json!("message"), 0), None);
        assert_eq!(
            find_nested_message(
                &json!({"a": {"b": {"c": {"d": {"e": {"f": {"g": {"message": "x"}}}}}}}}),
                0
            ),
            None
        );
    }

    /// The load-bearing value: `oauth.codex.maxRefreshAgeMs` is 8 days
    /// (`691200000`), read from the registry rather than duplicated.
    #[test]
    fn codex_refreshes_proactively_after_eight_days() {
        let now = 1_700_000_000_000i64;
        let mut credentials = Credentials {
            refresh_token: Some("r".into()),
            ..Default::default()
        };

        let stamp = |age_ms: i64| crate::executors::oauth::to_iso(now - age_ms);
        credentials
            .provider_specific_data
            .insert("lastRefreshAt".into(), json!(stamp(7 * 86_400_000)));
        assert!(
            !should_refresh_credentials("codex", &credentials, now),
            "7 days is inside the window"
        );
        credentials
            .provider_specific_data
            .insert("lastRefreshAt".into(), json!(stamp(8 * 86_400_000)));
        assert!(
            should_refresh_credentials("codex", &credentials, now),
            "8 days is the boundary"
        );
    }

    #[test]
    fn reasoning_effort_folds_the_virtual_levels() {
        // sol/terra list `ultra` themselves, so it survives unchanged.
        assert_eq!(normalize_reasoning_effort("gpt-5.6-sol", "ultra"), "ultra");
        // gpt-6* stops at `max`, so `ultra` folds onto it.
        assert_eq!(normalize_reasoning_effort("gpt-6-astra", "ultra"), "max");
        assert_eq!(normalize_reasoning_effort("gpt-5.3-codex", "max"), "xhigh");
        assert_eq!(normalize_reasoning_effort("gpt-5.3-codex", "high"), "high");
        assert_eq!(
            normalize_reasoning_effort("gpt-5.3-codex", "nonsense"),
            "nonsense"
        );
    }

    #[test]
    fn parse_error_reads_the_precise_reset_time() {
        let executor = CodexExecutor::new();
        let now = now_ms() as f64;
        let out = executor.parse_error(
            http_status::RATE_LIMITED,
            &json!({"error": {"type": "usage_limit_reached", "message": "limit", "resets_in_seconds": 60}}).to_string(),
        );
        assert_eq!(out["status"], json!(429));
        assert_eq!(out["message"], json!("limit"));
        let resets = out["resetsAtMs"].as_f64().unwrap();
        assert!(resets > now && resets <= now + 61_000.0);

        // A past `resets_at` falls through to `resets_in_seconds`.
        let out = executor.parse_error(
            http_status::RATE_LIMITED,
            &json!({"error": {"type": "usage_limit_reached", "resets_at": 1, "resets_in_seconds": 60}}).to_string(),
        );
        assert!(out["resetsAtMs"].as_f64().unwrap() > now);

        // Anything else keeps the default shape.
        let out = executor.parse_error(http_status::RATE_LIMITED, "nope");
        assert!(out.get("resetsAtMs").is_none());
    }
}
