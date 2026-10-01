//! The OpenCode free tier, plus the session and fingerprint machinery
//! `opencode-go` shares.
//!
//! The digest, the id shape and the Responses normalizers live once as
//! `pub(crate)` items here, because all three tiers must stay byte-identical: a
//! client that switches executor mid-conversation would otherwise change
//! identity.
//!
//! The upstream free-tier quota is accounted per session, so a fresh
//! `x-opencode-session` on every request burns it and surfaces as
//! `429 FreeUsageLimitError` with growing reset-after delays. The real CLI
//! reuses one long-lived canonical session per conversation; `stable_session_id`
//! does the same, one session per downstream identity, evicted on the same TTL as
//! the other session stores.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use rand::Rng;
use regex::Regex;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::credentials::Credentials;
use crate::executors::executor::{
    ExecError, ExecuteRequest, Executor, UpstreamResponse, insert_header,
};
use crate::providers::lookup::is_muse_spark_model;
use crate::providers::model::Transport;
use crate::providers::registry::registry;
use crate::providers::shared::ANTHROPIC_API_VERSION;
use crate::session_manager::{SessionIdentityInput, now_ms, resolve_session_id};
use crate::translator::concerns::primitives::{js_string, js_truthy};
use crate::translator::formats::responses_api::{
    clamp_responses_call_id, coerce_responses_arguments, coerce_responses_output,
    normalize_responses_input,
};
use crate::utils::fingerprint::apply_fingerprint_tools;
use crate::utils::reasoning_injector::inject_reasoning_content;

/// The user agent sent when the client's own is not trusted.
pub(crate) const OPENCODE_UA: &str = "opencode/1.18.31";
/// Longest session id accepted from a caller.
const MAX_SESSION_LENGTH: usize = 256;
/// Longest tool name the upstream accepts.
const MAX_TOOL_NAME_LEN: usize = 128;
pub(crate) const SESSION_HEADER: &str = "x-opencode-session";
const SESSION_FIELD: &str = "_opencodeSession";
const REQ_FIELD: &str = "_opencodeRequest";
const BASE62_CHARS: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
/// Cap on the stable-session store.
const MAX_STABLE_SESSIONS: usize = 1000;
/// How long a stable session survives without use.
const SESSION_TTL_MS: u64 = crate::runtime_config::memory_config::SESSION_TTL_MS as u64;

/// The canonical session id: `ses_`, 12 hex digits, 14 base62 characters.
static OPENCODE_SESSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ses_[0-9a-f]{12}[0-9A-Za-z]{14}$").expect("static pattern"));
/// The canonical request id: `msg_`, 12 hex digits, 14 base62 characters.
static OPENCODE_REQUEST_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^msg_[0-9a-f]{12}[0-9A-Za-z]{14}$").expect("static pattern"));
/// Matches the `opencode/x.y[.z]` version in a client user agent.
static OPENCODE_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)opencode/(\d+)\.(\d+)(?:\.(\d+))?").expect("static pattern"));

/// `(last timestamp, counter)`; the counter keeps same-millisecond ids apart.
static SESSION_COUNTER: LazyLock<Mutex<(u64, u64)>> = LazyLock::new(|| Mutex::new((0, 0)));
/// Per-process counter for synthetic response call ids.
static CALL_ID_SEQ: AtomicU64 = AtomicU64::new(0);

// ─── ids ─────────────────────────────────────────────────────────────────

/// 14 random bytes mapped onto the base62 alphabet.
fn unstable_random() -> String {
    let mut bytes = [0u8; 14];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| BASE62_CHARS.as_bytes()[(*b % 62) as usize] as char)
        .collect()
}

/// The six hex bytes of a 48-bit value, most significant first.
fn id_time_hex(value: u64) -> String {
    (0..6)
        .map(|i| format!("{:02x}", (value >> (40 - 8 * i)) & 0xff))
        .collect()
}

pub(crate) fn has_valid_opencode_version(ua: &str) -> bool {
    let Some(caps) = OPENCODE_VERSION_RE.captures(ua) else {
        return false;
    };
    let major: u32 = caps[1].parse().unwrap_or(0);
    let minor: u32 = caps[2].parse().unwrap_or(0);
    major > 1 || (major == 1 && minor >= 17)
}

/// The id is derived from the negated counter stamp; only the low 48 bits
/// survive, so the negation is done in `u64`.
pub fn generate_session_id(timestamp: u64) -> String {
    let mut state = SESSION_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    if timestamp != state.0 {
        state.0 = timestamp;
        state.1 = 0;
    }
    state.1 += 1;
    let current = timestamp.wrapping_mul(0x1000).wrapping_add(state.1);
    format!("ses_{}{}", id_time_hex(!current), unstable_random())
}

pub fn generate_request_id(timestamp: u64) -> String {
    let current = timestamp.wrapping_mul(0x1000).wrapping_add(1);
    format!("msg_{}{}", id_time_hex(current), unstable_random())
}

/// The base62 tail both id builders share, from digest bytes 6..20.
fn digest_tail(digest: &[u8]) -> String {
    (6..20)
        .map(|i| BASE62_CHARS.as_bytes()[(digest[i] % 62) as usize] as char)
        .collect()
}

/// Return the id unchanged when it is already canonical, otherwise derive a new
/// one from the client tool and the raw id.
pub(crate) fn translate_session_id(session_id: Option<&str>, client_tool: &str) -> String {
    if let Some(id) = session_id {
        let trimmed = id.trim();
        if OPENCODE_SESSION_RE.is_match(trimmed) {
            return trimmed.to_string();
        }
    }
    let tool = if client_tool.is_empty() {
        "generic"
    } else {
        client_tool
    };
    let digest =
        Sha256::digest(format!("opencode\0{tool}\0{}", session_id.unwrap_or("")).as_bytes());
    format!(
        "ses_{}{}",
        hex::encode(&digest[..6]),
        digest_tail(&digest[..])
    )
}

pub(crate) fn normalize_session(value: &str) -> Option<String> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > MAX_SESSION_LENGTH {
        return None;
    }
    Some(normalized.to_string())
}

/// The caller's own `x-opencode-session`, accepted only when it already has the
/// canonical shape.
pub(crate) fn native_session(headers: &HashMap<String, String>) -> Option<String> {
    raw_session_header(headers).filter(|s| OPENCODE_SESSION_RE.is_match(s))
}

/// The `x-opencode-session` header, trimmed and length-checked but not
/// shape-checked. `opencode-go` accepts any normalized value here.
pub(crate) fn raw_session_header(headers: &HashMap<String, String>) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(SESSION_HEADER))
        .and_then(|(_, v)| normalize_session(v))
}

fn normalize_request_id(value: &str) -> Option<String> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > MAX_SESSION_LENGTH {
        return None;
    }
    OPENCODE_REQUEST_RE
        .is_match(normalized)
        .then(|| normalized.to_string())
}

/// The last `n` characters, or the whole string when it is shorter.
fn tail_chars(text: &str, n: usize) -> String {
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(n)).collect()
}

/// The last user turn's text, capped at 600 characters.
fn last_user_text(body: &Value) -> String {
    let Some(arr) = body
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| body.get("input").and_then(Value::as_array))
    else {
        return match body.get("input").and_then(Value::as_str) {
            Some(s) => tail_chars(s, 600),
            None => String::new(),
        };
    };
    for msg in arr.iter().rev() {
        let Some(obj) = msg.as_object() else {
            continue;
        };
        if let Some(role) = obj.get("role").and_then(Value::as_str)
            && role != "user"
        {
            continue;
        }
        match obj.get("content") {
            Some(Value::String(content)) => {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return tail_chars(trimmed, 600);
                }
            }
            Some(Value::Array(parts)) => {
                let text = parts
                    .iter()
                    .map(|part| {
                        part.as_str()
                            .map(str::to_string)
                            .or_else(|| {
                                part.get("text").and_then(Value::as_str).map(str::to_string)
                            })
                            .or_else(|| {
                                part.get("input_text")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = text.trim();
                if !text.is_empty() {
                    return tail_chars(text, 600);
                }
            }
            _ => {}
        }
    }
    String::new()
}

/// The CLI sends the current user message id as `x-opencode-request`, stable per
/// turn and identical on retries. Deriving it from the session plus the last user
/// message gives retries the same id.
pub fn derive_request_id(session_id: &str, body: &Value) -> String {
    let text = last_user_text(body);
    if text.is_empty() {
        return generate_request_id(now_ms());
    }
    let digest = Sha256::digest(format!("opencode-req\0{session_id}\0{text}").as_bytes());
    let id = format!(
        "msg_{}{}",
        hex::encode(&digest[..6]),
        digest_tail(&digest[..])
    );
    if OPENCODE_REQUEST_RE.is_match(&id) {
        id
    } else {
        generate_request_id(now_ms())
    }
}

// ─── stable sessions ─────────────────────────────────────────────────────

/// `(session id, last used)` per identity key.
static STABLE_SESSIONS: LazyLock<Mutex<HashMap<String, (String, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn identity_key(credentials: &Credentials) -> String {
    let connection_id = credentials
        .connection_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            credentials
                .extra
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    if let Some(id) = connection_id {
        return format!("opencode:conn:{}", id.chars().take(128).collect::<String>());
    }
    // Header keys are stored lower-cased, so the four spellings of the same two
    // headers collapse to two lookups.
    let auth = credentials
        .header("authorization")
        .or_else(|| credentials.header("x-api-key"));
    if let Some(auth) = auth {
        return format!(
            "opencode:auth:{}",
            &hex::encode(Sha256::digest(auth.as_bytes()))[..32]
        );
    }
    "opencode:default".to_string()
}

/// One canonical session per downstream identity, refreshed on use and evicted
/// lazily on the shared TTL.
pub fn stable_session_id(credentials: &Credentials) -> String {
    let key = identity_key(credentials);
    let now = now_ms();
    let mut store = STABLE_SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    // A lazy sweep on access has no drift and matches the other session stores
    // in this crate.
    store.retain(|_, (_, last_used)| now.saturating_sub(*last_used) <= SESSION_TTL_MS);
    if let Some((session, last_used)) = store.get_mut(&key) {
        *last_used = now;
        return session.clone();
    }
    let session = generate_session_id(now);
    while store.len() >= MAX_STABLE_SESSIONS {
        let Some(victim) = store.keys().next().cloned() else {
            break;
        };
        store.remove(&victim);
    }
    store.insert(key, (session.clone(), now));
    session
}

// ─── request shape ───────────────────────────────────────────────────────

/// A conversation already in progress, which is what makes the shared session
/// manager's answer better than a fresh id.
pub(crate) fn body_has_session_hints(body: &Value) -> bool {
    let Some(obj) = body.as_object() else {
        return false;
    };
    for key in ["session_id", "conversation_id", "prompt_cache_key"] {
        if obj
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
        {
            return true;
        }
    }
    if obj
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    if let Some(session_id) = obj.get("request").and_then(|r| r.get("sessionId"))
        && !session_id.is_null()
        && !js_string(session_id).is_empty()
    {
        return true;
    }

    let Some(arr) = obj
        .get("messages")
        .and_then(Value::as_array)
        .or_else(|| obj.get("input").and_then(Value::as_array))
    else {
        return false;
    };
    let mut assistant_text = String::new();
    for msg in arr {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        match msg.get("content") {
            Some(Value::String(content)) => assistant_text.push_str(content),
            Some(Value::Array(parts)) => {
                for part in parts {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        assistant_text.push_str(text);
                    } else if let Some(output) = part.get("output").and_then(Value::as_str) {
                        assistant_text.push_str(output);
                    }
                }
            }
            _ => {}
        }
        if assistant_text.chars().count() >= 50 {
            return true;
        }
    }
    false
}

/// Strip the thinking suffix `model(level)` so registry lookups hit the base id.
pub(crate) fn base_model_id(model: &str) -> String {
    static TRAILING_PAREN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\([^()]+\)\s*$").expect("static pattern"));
    match TRAILING_PAREN.find(model) {
        Some(m) => model[..m.start()].trim().to_string(),
        None => model.trim().to_string(),
    }
}

/// Truncate a tool name to `MAX_TOOL_NAME_LEN` characters.
pub(crate) fn clamp_tool_name(name: &str) -> String {
    match name.char_indices().nth(MAX_TOOL_NAME_LEN) {
        Some((idx, _)) => name[..idx].to_string(),
        None => name.to_string(),
    }
}

/// Flatten Chat Completions tool declarations into the Responses flat shape and
/// drop hosted or nameless tools the `/responses` endpoint rejects.
pub(crate) fn normalize_responses_tools(body: &mut Value) {
    let Some(tools) = body.get("tools").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut valid_names: Vec<String> = Vec::new();
    let mut kept: Vec<Value> = Vec::with_capacity(tools.len());
    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let function = obj.get("function").and_then(Value::as_object);
        let raw_name = obj
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| function.and_then(|f| f.get("name")).and_then(Value::as_str))
            .unwrap_or("");
        let name = raw_name.trim();
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
        let mut parameters = obj
            .get("parameters")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| {
                function
                    .and_then(|f| f.get("parameters"))
                    .and_then(Value::as_object)
                    .cloned()
            })
            .unwrap_or_else(|| {
                let mut empty = Map::new();
                empty.insert("type".into(), json!("object"));
                empty.insert("properties".into(), json!({}));
                empty
            });
        // `{type:"object"}` without properties is rejected by strict Responses
        // backends, so fill in the empty properties map.
        if parameters.get("type").and_then(Value::as_str) == Some("object")
            && !parameters.contains_key("properties")
        {
            parameters.insert("properties".into(), json!({}));
        }

        let name = clamp_tool_name(name);
        let mut rebuilt = Map::new();
        rebuilt.insert("type".into(), json!("function"));
        rebuilt.insert("name".into(), json!(name));
        if !description.is_empty() {
            rebuilt.insert("description".into(), json!(description));
        }
        rebuilt.insert("parameters".into(), Value::Object(parameters));
        valid_names.push(name);
        kept.push(Value::Object(rebuilt));
    }
    body["tools"] = Value::Array(kept);

    if body.get("tool_choice").is_some_and(Value::is_object)
        && let Some(choice) = body.get("tool_choice").and_then(Value::as_object)
        && choice.get("type").and_then(Value::as_str) == Some("function")
    {
        let name = choice
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if (name.is_empty() || !valid_names.iter().any(|n| n == name))
            && let Some(obj) = body.as_object_mut()
        {
            obj.shift_remove("tool_choice");
        }
    }
}

/// The last line of defence for native Responses clients, whose payloads skip
/// translation. Prior-turn reasoning
/// items are dropped because the upstream account pool cannot decrypt their
/// `encrypted_content`, and under `store:false` omitting it makes OpenAI reject
/// the referenced item as "not found or was deleted".
pub(crate) fn sanitize_responses_items(body: &mut Value) {
    let Some(items) = body.get("input").and_then(Value::as_array).cloned() else {
        return;
    };
    let mut kept: Vec<Value> = Vec::with_capacity(items.len());
    for item in items {
        let Some(source) = item.as_object() else {
            kept.push(item);
            continue;
        };
        if source.get("type").and_then(Value::as_str) == Some("reasoning") {
            continue;
        }
        let mut obj = source.clone();
        obj.shift_remove("encrypted_content");
        obj.shift_remove("reasoning_encrypted_content");

        let item_type = obj.get("type").and_then(Value::as_str).map(str::to_string);
        match item_type.as_deref() {
            Some("function_call") => {
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                if name.is_empty() {
                    continue;
                }
                let name = clamp_tool_name(name);
                let call_id = clamp_responses_call_id(
                    obj.get("call_id"),
                    now_ms() as i64,
                    CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
                );
                let arguments = coerce_responses_arguments(obj.get("arguments"));
                obj.insert("name".into(), json!(name));
                obj.insert("call_id".into(), json!(call_id));
                obj.insert("arguments".into(), json!(arguments));
            }
            Some("function_call_output") => {
                let call_id = clamp_responses_call_id(
                    obj.get("call_id"),
                    now_ms() as i64,
                    CALL_ID_SEQ.fetch_add(1, Ordering::Relaxed) + 1,
                );
                let output = coerce_responses_output(obj.get("output"));
                obj.insert("call_id".into(), json!(call_id));
                obj.insert("output".into(), json!(output));
            }
            _ => {}
        }
        kept.push(Value::Object(obj));
    }
    body["input"] = Value::Array(kept);
}

/// The two listed `RESPONSES_MODELS` plus every Muse Spark contributor id.
fn is_responses_model(model: &str) -> bool {
    const RESPONSES_MODELS: [&str; 2] = [
        "muse-spark-1.2-contributor-free",
        "muse-spark-1.3-contributor-free",
    ];
    let base = base_model_id(model);
    RESPONSES_MODELS.contains(&base.as_str()) || is_muse_spark_model(&base)
}

fn is_messages_model(model: &str) -> bool {
    base_model_id(model) == "union-alpha"
}

/// The Responses API takes thinking as `reasoning:{effort,summary}`, so the Chat
/// fields are folded in at this boundary.
fn normalize_opencode_reasoning(model: &str, body: &mut Value) {
    let current = body.get("reasoning").and_then(Value::as_object).cloned();
    let requested = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            current
                .as_ref()
                .and_then(|c| c.get("effort"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let Some(requested) = requested else {
        return;
    };

    // The caller's model argument wins; the body's is the fallback.
    let effective_model = if model.is_empty() {
        body.get("model").and_then(Value::as_str).unwrap_or("")
    } else {
        model
    };
    let clean_model = base_model_id(effective_model);
    let supported =
        crate::catalog::get_thinking_levels(Some("opencode"), &clean_model).unwrap_or_default();
    let is_supported = |level: &str| supported.contains(&level);
    let mut effort = requested.trim().to_lowercase();
    if (effort == "max" || effort == "ultra") && !supported.is_empty() && !is_supported(&effort) {
        if effort == "ultra" && is_supported("max") {
            effort = "max".to_string();
        } else if is_supported("xhigh") {
            effort = "xhigh".to_string();
        }
    }

    let mut reasoning = current.unwrap_or_default();
    reasoning.insert("effort".into(), json!(effort));
    if !reasoning.get("summary").is_some_and(js_truthy) {
        reasoning.insert("summary".into(), json!("auto"));
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    obj.insert("reasoning".into(), Value::Object(reasoning));
    obj.shift_remove("reasoning_effort");
}

// ─── the default loop, reached from an override ──────────────────────────

/// Runs the trait's default send/retry loop against an executor that has
/// already prepared its own credentials.
///
/// The opencode executors must re-point the request at a credential set they
/// built from the body, which is what `ExecuteRequest::with_credentials` is for.
/// The loop itself is the trait's default `execute`, and an override cannot call
/// it (a plain `self.execute(...)` recurses), so this forwards every hook the
/// loop reads back to the real executor and leaves `execute` at the default.
pub(crate) struct LoopRunner<'a, E: Executor + ?Sized>(pub(crate) &'a E);

/// No `#[async_trait]` here: the impl overrides only the synchronous hooks and
/// inherits the trait's default `execute`.
impl<E: Executor + ?Sized> Executor for LoopRunner<'_, E> {
    fn provider(&self) -> &str {
        self.0.provider()
    }

    fn config(&self) -> &Transport {
        self.0.config()
    }

    fn build_url(
        &self,
        model: &str,
        stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        self.0.build_url(model, stream, url_index, credentials)
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        model: &str,
        body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        self.0.build_headers(credentials, stream, url, model, body)
    }

    fn transform_request(
        &self,
        model: &str,
        body: Value,
        stream: bool,
        credentials: &Credentials,
    ) -> Value {
        self.0.transform_request(model, body, stream, credentials)
    }

    fn should_retry(&self, status: u16, url_index: usize) -> bool {
        self.0.should_retry(status, url_index)
    }
}

// ─── executor ────────────────────────────────────────────────────────────

pub struct OpenCodeExecutor {
    config: Transport,
}

impl OpenCodeExecutor {
    pub fn new() -> Self {
        Self {
            config: registry()
                .transport("opencode")
                .cloned()
                .unwrap_or_else(|| serde_json::from_value(json!({})).expect("empty transport")),
        }
    }

    /// Resolve the session and request ids and stash them on a copy of the
    /// credentials.
    pub fn prepare_request_credentials(
        &self,
        body: Option<&Value>,
        credentials: &Credentials,
        provider_session_id: Option<&str>,
        client_tool: Option<&str>,
    ) -> Credentials {
        let empty = Value::Null;
        let body = body.unwrap_or(&empty);
        let session = resolve_opencode_session(body, credentials, provider_session_id, client_tool);
        let request_id = resolve_opencode_request_id(body, credentials, &session);

        let mut prepared = credentials.clone();
        prepared.extra.insert(SESSION_FIELD.into(), json!(session));
        prepared.extra.insert(REQ_FIELD.into(), json!(request_id));
        prepared
    }
}

impl Default for OpenCodeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Prefer a canonical client header, then a translated incoming session, then
/// the shared session manager, and finally the stable store.
fn resolve_opencode_session(
    body: &Value,
    credentials: &Credentials,
    provider_session_id: Option<&str>,
    client_tool: Option<&str>,
) -> String {
    let headers = &credentials.raw_headers;
    if let Some(native) = native_session(headers) {
        return native;
    }

    let incoming = raw_session_header(headers);
    let hinted = incoming.or_else(|| provider_session_id.and_then(normalize_session));
    if let Some(hinted) = hinted {
        return translate_session_id(Some(&hinted), client_tool.unwrap_or(""));
    }

    let has_connection = credentials
        .connection_id
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    if has_connection || body_has_session_hints(body) {
        let via_manager = resolve_session_id(&SessionIdentityInput {
            headers,
            body,
            connection_id: credentials.connection_id.as_deref(),
            workspace_id: None,
            scope: "opencode",
        });
        if !via_manager.is_empty() {
            return translate_session_id(Some(&via_manager), client_tool.unwrap_or(""));
        }
    }

    stable_session_id(credentials)
}

/// Honour a well-formed `x-opencode-request`, otherwise derive one from the
/// session and body.
fn resolve_opencode_request_id(
    body: &Value,
    credentials: &Credentials,
    session_id: &str,
) -> String {
    let header = credentials
        .raw_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request"))
        .map(|(_, v)| v.as_str());
    if let Some(raw) = header
        && let Some(normalized) = normalize_request_id(raw)
    {
        return normalized;
    }
    // The scan stops at the first match even when the value does not normalize,
    // so an invalid header still ends the search.
    derive_request_id(session_id, body)
}

/// The `forceAutoToolChoiceModels` quirk: only the ids confirmed auto-only.
fn force_auto_tool_choice(config: &Transport, model: &str) -> bool {
    config
        .quirks
        .as_ref()
        .and_then(|q| q.get("forceAutoToolChoiceModels"))
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str().is_some_and(|s| s == model)))
}

#[async_trait]
impl Executor for OpenCodeExecutor {
    fn provider(&self) -> &str {
        "opencode"
    }

    fn config(&self) -> &Transport {
        &self.config
    }

    fn build_url(
        &self,
        model: &str,
        _stream: bool,
        _url_index: usize,
        _credentials: &Credentials,
    ) -> Result<String, ExecError> {
        let base = self.config.base_url.clone().unwrap_or_default();
        if is_responses_model(model) {
            return Ok(format!("{base}/zen/v1/responses"));
        }
        if is_messages_model(model) {
            return Ok(format!("{base}/zen/v1/messages"));
        }
        Ok(format!("{base}/zen/v1/chat/completions"))
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        url: &str,
        _model: &str,
        _body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        let downstream_ua = credentials.header("user-agent").unwrap_or("");
        let is_opencode_downstream = has_valid_opencode_version(downstream_ua);

        let session = match credentials
            .extra
            .get(SESSION_FIELD)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(session) => session.to_string(),
            None => {
                let prepared = self.prepare_request_credentials(None, credentials, None, None);
                prepared
                    .extra
                    .get(SESSION_FIELD)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            }
        };
        let request_id = credentials
            .extra
            .get(REQ_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                credentials
                    .header("x-opencode-request")
                    .and_then(normalize_request_id)
            })
            .unwrap_or_else(|| generate_request_id(now_ms()));

        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "content-type", "application/json")?;
        // Free-tier credentials are pooled; the upstream reads the session, not
        // the token.
        insert_header(&mut headers, "authorization", "Bearer public")?;
        insert_header(
            &mut headers,
            "user-agent",
            if is_opencode_downstream {
                downstream_ua
            } else {
                OPENCODE_UA
            },
        )?;
        insert_header(
            &mut headers,
            "x-opencode-client",
            credentials.header("x-opencode-client").unwrap_or("desktop"),
        )?;
        insert_header(&mut headers, SESSION_HEADER, &session)?;
        insert_header(&mut headers, "x-opencode-request", &request_id)?;
        insert_header(
            &mut headers,
            "x-opencode-project",
            credentials.header("x-opencode-project").unwrap_or("global"),
        )?;
        insert_header(
            &mut headers,
            "accept",
            if stream { "text/event-stream" } else { "*/*" },
        )?;
        if url.ends_with("/messages") {
            insert_header(&mut headers, "anthropic-version", ANTHROPIC_API_VERSION)?;
        }
        Ok(headers)
    }

    fn transform_request(
        &self,
        model: &str,
        mut body: Value,
        _stream: bool,
        _credentials: &Credentials,
    ) -> Value {
        if body.is_object() {
            if !model.is_empty() && !body.get("model").is_some_and(js_truthy) {
                body["model"] = json!(model);
            }
            // The free tier rejects non-streaming requests on free models with
            // `403 FreeTierError`; always stream upstream and let the handler
            // aggregate for a non-stream client.
            body["stream"] = json!(true);
        }

        let effective_model = if model.is_empty() {
            body.get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            model.to_string()
        };

        if is_responses_model(&effective_model) && body.is_object() {
            if body.get("tool_choice").is_some_and(|c| c != &json!("auto"))
                && force_auto_tool_choice(&self.config, &base_model_id(model))
                && let Some(obj) = body.as_object_mut()
            {
                obj.insert("tool_choice".into(), json!("auto"));
            }
            if let Some(normalized) = normalize_responses_input(body.get("input")) {
                body["input"] = Value::Array(normalized);
            }
            if body
                .get("input")
                .and_then(Value::as_array)
                .is_none_or(|i| i.is_empty())
            {
                body["input"] = json!([{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "..."}],
                }]);
            }
            // Responses names the output cap `max_output_tokens`.
            if body.get("max_output_tokens").is_none() {
                if let Some(value) = body.get("max_completion_tokens").cloned() {
                    body["max_output_tokens"] = value;
                } else if let Some(value) = body.get("max_tokens").cloned() {
                    body["max_output_tokens"] = value;
                }
            }
            if let Some(obj) = body.as_object_mut() {
                obj.shift_remove("max_tokens");
                obj.shift_remove("max_completion_tokens");
            }
            normalize_opencode_reasoning(model, &mut body);
            body["stream"] = json!(true);
            body["store"] = json!(false);
            normalize_responses_tools(&mut body);
            sanitize_responses_items(&mut body);
            // Free-tier fingerprint tools are required even when an agent client
            // supplied tools: ZCode/Claude Code requests normally carry a
            // non-empty tool array, and skipping the cloak here is a 403.
            apply_fingerprint_tools(&mut body, true);
        } else if body.is_object() {
            apply_fingerprint_tools(&mut body, false);
        }

        inject_reasoning_content(Some("opencode"), model, body)
    }

    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        let credentials = self.prepare_request_credentials(
            Some(&req.body),
            req.credentials,
            req.provider_session_id,
            req.client_tool,
        );
        LoopRunner(self)
            .execute(req.with_credentials(&credentials))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials_with(connection_id: &str) -> Credentials {
        Credentials {
            connection_id: Some(connection_id.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn generated_ids_have_the_shape_the_upstream_matches() {
        let session = generate_session_id(1_700_000_000_000);
        assert!(OPENCODE_SESSION_RE.is_match(&session), "{session}");
        assert_eq!(session.len(), 30);
        let request = generate_request_id(1_700_000_000_000);
        assert!(OPENCODE_REQUEST_RE.is_match(&request), "{request}");
        assert_eq!(request.len(), 30);
    }

    #[test]
    fn the_session_stamp_is_the_complement_of_the_request_stamp() {
        // The session id negates the counter stamp; the request id does not.
        // Both take the same low 48 bits.
        assert_eq!(id_time_hex(1), "000000000001");
        // `~1n` is `-2n`, whose low 48 bits are all ones but for the last.
        assert_eq!(id_time_hex(!1u64), "fffffffffffe");
        assert_eq!(id_time_hex(0x1234_5678_9abc), "123456789abc");
    }

    #[test]
    fn ids_minted_in_the_same_millisecond_stay_distinct() {
        // The counter is what keeps a burst of same-ms ids apart; the random
        // tail alone would collide often enough to matter. Parallel tests share
        // the counter, so the stamp itself is not asserted on here.
        let ids: Vec<String> = (0..8)
            .map(|_| generate_session_id(1_700_000_000_001))
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(ids.iter().all(|id| OPENCODE_SESSION_RE.is_match(id)));
    }

    #[test]
    fn a_valid_session_id_passes_through_translation() {
        let valid = "ses_0123456789abABCDEFGHIJKLMN";
        assert_eq!(translate_session_id(Some(valid), "claude-code"), valid);
        assert_eq!(
            translate_session_id(Some(&format!("  {valid}  ")), ""),
            valid
        );
    }

    #[test]
    fn translation_is_deterministic_and_client_scoped() {
        let a = translate_session_id(Some("conv-1"), "claude-code");
        let b = translate_session_id(Some("conv-1"), "claude-code");
        assert_eq!(a, b);
        assert_ne!(a, translate_session_id(Some("conv-1"), "cursor"));
        assert_ne!(a, translate_session_id(Some("conv-2"), "claude-code"));
        assert!(OPENCODE_SESSION_RE.is_match(&a));
        // An absent client tool falls back to `generic`.
        assert_eq!(
            translate_session_id(Some("conv-1"), ""),
            translate_session_id(Some("conv-1"), "generic")
        );
    }

    #[test]
    fn a_derived_request_id_tracks_the_last_user_turn() {
        let body = json!({"messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "answer"},
            {"role": "user", "content": [{"type": "text", "text": "second"}]},
        ]});
        let a = derive_request_id("ses_x", &body);
        assert_eq!(a, derive_request_id("ses_x", &body), "retries share the id");
        assert_ne!(
            a,
            derive_request_id(
                "ses_x",
                &json!({"messages": [{"role": "user", "content": "other"}]})
            )
        );
        assert!(OPENCODE_REQUEST_RE.is_match(&a));
    }

    #[test]
    fn last_user_text_reads_messages_input_and_plain_input() {
        assert_eq!(
            last_user_text(&json!({"messages": [{"role": "user", "content": " hi "}]})),
            "hi"
        );
        assert_eq!(last_user_text(&json!({"input": "prompt"})), "prompt");
        assert_eq!(
            last_user_text(
                &json!({"input": [{"role": "user", "content": [{"input_text": "from input"}]}]})
            ),
            "from input"
        );
        // An assistant-only tail is not a user turn.
        assert_eq!(
            last_user_text(
                &json!({"messages": [{"role": "user", "content": "u"}, {"role": "assistant", "content": "a"}]})
            ),
            "u"
        );
        assert_eq!(last_user_text(&json!({})), "");
        assert_eq!(tail_chars(&"x".repeat(700), 600).chars().count(), 600);
    }

    #[test]
    fn only_recent_client_versions_are_trusted_for_the_user_agent() {
        assert!(has_valid_opencode_version("opencode/1.18.31"));
        assert!(has_valid_opencode_version("opencode/1.17"));
        assert!(!has_valid_opencode_version("opencode/1.16.9"));
        assert!(!has_valid_opencode_version("curl/8.0"));
        assert!(!has_valid_opencode_version(""));
    }

    #[test]
    fn base_model_ids_strip_the_thinking_suffix() {
        assert_eq!(
            base_model_id("muse-spark-1.3-contributor-free(high)"),
            "muse-spark-1.3-contributor-free"
        );
        assert_eq!(base_model_id("union-alpha"), "union-alpha");
        assert!(is_responses_model("muse-spark-1.3-contributor-free"));
        assert!(is_responses_model("muse-spark-9.9-contributor(high)"));
        assert!(!is_responses_model("union-alpha"));
        assert!(is_messages_model("union-alpha"));
    }

    #[test]
    fn responses_tools_are_flattened_and_nameless_ones_dropped() {
        let mut body = json!({"tools": [
            {"type": "function", "function": {"name": " MyTool ", "description": "d", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"description": "no name"}},
            {"type": "web_search"},
        ]});
        normalize_responses_tools(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], json!("MyTool"));
        assert!(tools[0].get("function").is_none());
        assert_eq!(tools[0]["parameters"]["properties"], json!({}));
    }

    #[test]
    fn a_tool_choice_outside_the_surviving_tools_is_dropped() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "kept"}],
            "tool_choice": {"type": "function", "name": "gone"},
        });
        normalize_responses_tools(&mut body);
        assert!(body.get("tool_choice").is_none());

        let mut body = json!({
            "tools": [{"type": "function", "name": "kept"}],
            "tool_choice": {"type": "function", "name": "kept"},
        });
        normalize_responses_tools(&mut body);
        assert_eq!(body["tool_choice"]["name"], json!("kept"));
    }

    #[test]
    fn sanitizing_drops_reasoning_and_repairs_tool_items() {
        let mut body = json!({"input": [
            {"type": "reasoning", "encrypted_content": "blob"},
            {"type": "message", "role": "user", "encrypted_content": "blob"},
            {"type": "function_call", "name": " bash ", "call_id": "", "arguments": {"a": 1}},
            {"type": "function_call", "name": "  "},
            {"type": "function_call_output", "call_id": "call_1", "output": {"b": 2}},
        ]});
        sanitize_responses_items(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], json!("message"));
        assert!(items[0].get("encrypted_content").is_none());
        assert_eq!(items[1]["name"], json!("bash"));
        assert_eq!(items[1]["arguments"], json!("{\"a\":1}"));
        assert_eq!(items[2]["output"], json!("{\"b\":2}"));
        assert!(items[1]["call_id"].as_str().unwrap().starts_with("call_"));
    }

    #[test]
    fn session_hints_are_detected_from_metadata_and_transcript() {
        assert!(body_has_session_hints(&json!({"prompt_cache_key": "k"})));
        assert!(body_has_session_hints(
            &json!({"metadata": {"user_id": "u"}})
        ));
        assert!(!body_has_session_hints(
            &json!({"metadata": {"user_id": "  "}})
        ));
        let short = json!({"messages": [{"role": "assistant", "content": "short"}]});
        assert!(
            !body_has_session_hints(&short),
            "under the 50-character threshold"
        );
        let long = json!({"messages": [{"role": "assistant", "content": "x".repeat(50)}]});
        assert!(body_has_session_hints(&long));
        assert!(!body_has_session_hints(
            &json!({"messages": [{"role": "user", "content": "x".repeat(80)}]})
        ));
    }

    #[test]
    fn the_stable_session_is_reused_per_identity() {
        let first = stable_session_id(&credentials_with("conn-a"));
        let second = stable_session_id(&credentials_with("conn-a"));
        assert_eq!(first, second);
        assert!(OPENCODE_SESSION_RE.is_match(&first));
        assert_ne!(first, stable_session_id(&credentials_with("conn-b")));

        // The identity is the credential's connection id or its authorization
        // header — an api key alone is not an identity, so it does not fork
        // the session.
        let mut by_key = Credentials {
            api_key: Some("secret".into()),
            ..Default::default()
        };
        assert_eq!(identity_key(&by_key), "opencode:default");
        by_key
            .raw_headers
            .insert("authorization".into(), "Bearer secret".into());
        let a = stable_session_id(&by_key);
        by_key
            .raw_headers
            .insert("authorization".into(), "Bearer other".into());
        assert_ne!(a, stable_session_id(&by_key));
        // The key is the digest, never the token.
        let key = identity_key(&by_key);
        assert!(key.starts_with("opencode:auth:"));
        assert_eq!(key.len(), "opencode:auth:".len() + 32);
        assert!(!key.contains("other"));
    }

    #[test]
    fn an_explicit_session_header_wins_over_the_stable_store() {
        let mut credentials = credentials_with("conn-a");
        let native = "ses_0123456789abABCDEFGHIJKLMN";
        credentials
            .raw_headers
            .insert(SESSION_HEADER.into(), native.into());
        assert_eq!(
            resolve_opencode_session(&json!({}), &credentials, None, None),
            native,
            "a canonical header is passed straight through"
        );
    }

    #[test]
    fn a_provider_session_is_translated_rather_than_reused() {
        let credentials = credentials_with("conn-a");
        let resolved = resolve_opencode_session(
            &json!({}),
            &credentials,
            Some("app-session"),
            Some("cursor"),
        );
        assert_eq!(
            resolved,
            translate_session_id(Some("app-session"), "cursor")
        );
        // Without a hint and without a connection the stable store answers.
        let anonymous = Credentials::default();
        assert!(OPENCODE_SESSION_RE.is_match(&resolve_opencode_session(
            &json!({}),
            &anonymous,
            None,
            None
        )));
    }

    #[test]
    fn the_request_id_header_is_honoured_only_when_well_formed() {
        let mut credentials = credentials_with("conn-a");
        let valid = "msg_0123456789abABCDEFGHIJKLMN";
        credentials
            .raw_headers
            .insert("x-opencode-request".into(), valid.into());
        assert_eq!(
            resolve_opencode_request_id(&json!({}), &credentials, "ses_x"),
            valid
        );

        credentials
            .raw_headers
            .insert("x-opencode-request".into(), "not-an-id".into());
        let derived = resolve_opencode_request_id(&json!({"input": "hi"}), &credentials, "ses_x");
        assert_ne!(derived, "not-an-id");
        assert!(OPENCODE_REQUEST_RE.is_match(&derived));
    }

    #[test]
    fn prepared_credentials_carry_the_session_and_the_request_id() {
        let executor = OpenCodeExecutor::new();
        let credentials = credentials_with("conn-a");
        let body = json!({"messages": [{"role": "user", "content": "hello"}]});
        let prepared = executor.prepare_request_credentials(Some(&body), &credentials, None, None);
        let session = prepared
            .extra
            .get(SESSION_FIELD)
            .and_then(Value::as_str)
            .unwrap();
        assert!(OPENCODE_SESSION_RE.is_match(session));
        let request = prepared
            .extra
            .get(REQ_FIELD)
            .and_then(Value::as_str)
            .unwrap();
        assert!(OPENCODE_REQUEST_RE.is_match(request));
        // The source credentials are not mutated.
        assert!(credentials.extra.is_empty());
    }

    #[test]
    fn headers_carry_the_session_and_the_free_tier_ua() {
        let executor = OpenCodeExecutor::new();
        let credentials = credentials_with("conn-a");
        let prepared = executor.prepare_request_credentials(None, &credentials, None, None);
        let headers = executor
            .build_headers(
                &prepared,
                true,
                "https://opencode.ai/zen/v1/chat/completions",
                "m",
                None,
            )
            .unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer public");
        assert_eq!(headers.get("user-agent").unwrap(), OPENCODE_UA);
        assert_eq!(headers.get("x-opencode-client").unwrap(), "desktop");
        assert_eq!(headers.get("x-opencode-project").unwrap(), "global");
        assert_eq!(headers.get("accept").unwrap(), "text/event-stream");
        assert_eq!(
            headers.get(SESSION_HEADER).unwrap(),
            prepared
                .extra
                .get(SESSION_FIELD)
                .and_then(Value::as_str)
                .unwrap()
        );
        assert!(headers.get("anthropic-version").is_none());

        let messages = executor
            .build_headers(
                &prepared,
                false,
                "https://opencode.ai/zen/v1/messages",
                "union-alpha",
                None,
            )
            .unwrap();
        assert_eq!(messages.get("accept").unwrap(), "*/*");
        assert_eq!(
            messages.get("anthropic-version").unwrap(),
            ANTHROPIC_API_VERSION
        );
    }

    #[test]
    fn a_valid_client_user_agent_is_forwarded() {
        let executor = OpenCodeExecutor::new();
        let mut credentials = credentials_with("conn-a");
        credentials
            .raw_headers
            .insert("user-agent".into(), "opencode/1.18.40".into());
        credentials
            .raw_headers
            .insert("x-opencode-client".into(), "cli".into());
        credentials
            .raw_headers
            .insert("x-opencode-project".into(), "proj".into());
        let headers = executor
            .build_headers(
                &credentials,
                true,
                "https://opencode.ai/zen/v1/chat/completions",
                "m",
                None,
            )
            .unwrap();
        assert_eq!(headers.get("user-agent").unwrap(), "opencode/1.18.40");
        assert_eq!(headers.get("x-opencode-client").unwrap(), "cli");
        assert_eq!(headers.get("x-opencode-project").unwrap(), "proj");
    }

    #[test]
    fn urls_follow_the_model_family() {
        let executor = OpenCodeExecutor::new();
        let credentials = Credentials::default();
        assert_eq!(
            executor
                .build_url("muse-spark-1.3-contributor-free", true, 0, &credentials)
                .unwrap(),
            "https://opencode.ai/zen/v1/responses"
        );
        assert_eq!(
            executor
                .build_url("union-alpha", true, 0, &credentials)
                .unwrap(),
            "https://opencode.ai/zen/v1/messages"
        );
        assert_eq!(
            executor
                .build_url("gpt-5.6-luna", true, 0, &credentials)
                .unwrap(),
            "https://opencode.ai/zen/v1/chat/completions"
        );
    }

    #[test]
    fn a_responses_model_gets_the_fingerprint_tool_set() {
        let executor = OpenCodeExecutor::new();
        let body = json!({
            "model": "muse-spark-1.3-contributor-free",
            "input": "hello",
            "max_tokens": 10,
            "reasoning_effort": "high",
        });
        let out = executor.transform_request(
            "muse-spark-1.3-contributor-free",
            body,
            false,
            &Credentials::default(),
        );
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["store"], json!(false));
        assert_eq!(out["max_output_tokens"], json!(10));
        assert!(out.get("max_tokens").is_none());
        assert_eq!(out["reasoning"]["effort"], json!("high"));
        assert_eq!(out["reasoning"]["summary"], json!("auto"));
        assert!(out.get("reasoning_effort").is_none());
        assert_eq!(out["input"][0]["type"], json!("message"));
        assert_eq!(out["tool_choice"], json!("auto"));
        let names: Vec<&str> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(names, ["bash", "glob", "grep", "read"]);
    }

    #[test]
    fn a_chat_model_gets_the_nested_fingerprint_tools() {
        let executor = OpenCodeExecutor::new();
        let body = json!({"messages": [{"role": "user", "content": "hi"}], "stream": false});
        let out = executor.transform_request("gpt-5.6-luna", body, false, &Credentials::default());
        assert_eq!(out["stream"], json!(true));
        assert_eq!(
            out["tool_choice"],
            json!("none"),
            "no caller tools means no forced choice"
        );
        assert_eq!(out["tools"][0]["function"]["name"], json!("bash"));
        assert!(
            out.get("store").is_none(),
            "the chat path leaves store alone"
        );
    }

    #[test]
    fn the_forced_tool_choice_is_limited_to_the_allowlisted_model() {
        let executor = OpenCodeExecutor::new();
        let body = json!({
            "model": "muse-spark-1.3-contributor-free",
            "input": "hi",
            "tool_choice": "required",
        });
        let out = executor.transform_request(
            "muse-spark-1.3-contributor-free",
            body,
            false,
            &Credentials::default(),
        );
        assert_eq!(out["tool_choice"], json!("auto"));

        let body = json!({"model": "muse-spark-1.2-contributor-free", "input": "hi", "tool_choice": "required"});
        let out = executor.transform_request(
            "muse-spark-1.2-contributor-free",
            body,
            false,
            &Credentials::default(),
        );
        assert_eq!(
            out["tool_choice"],
            json!("required"),
            "not in forceAutoToolChoiceModels"
        );
    }

    #[test]
    fn reasoning_effort_is_clamped_to_the_supported_levels() {
        // The muse-spark models think in the OpenAI level set, which tops out at
        // `xhigh`, so both `ultra` and `max` fall back to it.
        let mut body = json!({"reasoning_effort": "ULTRA "});
        normalize_opencode_reasoning("muse-spark-1.3-contributor-free", &mut body);
        assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
        assert_eq!(body["reasoning"]["summary"], json!("auto"));
        assert!(body.get("reasoning_effort").is_none());

        let mut body = json!({"reasoning": {"effort": "max"}, "input": "hi"});
        normalize_opencode_reasoning("muse-spark-1.3-contributor-free", &mut body);
        assert_eq!(body["reasoning"]["effort"], json!("xhigh"));
        assert_eq!(
            body["reasoning"]["summary"],
            json!("auto"),
            "the summary is added when missing"
        );

        // A level the model already supports is left alone, and an existing
        // summary is not overwritten.
        let mut body = json!({"reasoning": {"effort": "low", "summary": "concise"}});
        normalize_opencode_reasoning("muse-spark-1.3-contributor-free", &mut body);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "concise"})
        );

        // No effort anywhere is a no-op.
        let mut untouched = json!({"input": "hi"});
        normalize_opencode_reasoning("muse-spark-1.3-contributor-free", &mut untouched);
        assert!(untouched.get("reasoning").is_none());
    }

    #[test]
    fn the_executor_reads_its_transport_from_the_registry() {
        let executor = OpenCodeExecutor::new();
        assert_eq!(executor.provider(), "opencode");
        assert_eq!(
            executor.config().base_url.as_deref(),
            Some("https://opencode.ai")
        );
        assert!(executor.no_auth(), "the free tier is unauthenticated");
    }
}
