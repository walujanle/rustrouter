//! The Responses API on `cli-chat-proxy.grok.com`, authenticated with the
//! `xai-grok-cli` OAuth device-code token.
//!
//! Four behaviours here are load-bearing and easy to lose:
//!
//! * **The turn index is monotonic per session.** The official CLI sends
//!   `x-grok-turn-idx` as the 1-based conversation turn, so the header must
//!   never go backwards within the process even when a client sends a
//!   delta-style payload with a single user message.
//! * **A retry must reuse the turn it already sent.** A retry is the same
//!   request; only a *new* request advances the index.
//! * **Reasoning effort is model-gated.** `grok-build` and Composer reject the
//!   `effort` key outright but still accept `summary` and the encrypted
//!   continuity blob, so the effort key is deleted rather than the whole
//!   `reasoning` object.
//! * **`role: "system"` stays as-is.** The official grok-pager HAR sends
//!   `system`, not `developer` (Codex converts; Grok CLI does not).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use regex::Regex;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::default::DefaultExecutor;
use crate::executors::executor::{
    ExecError, ExecuteRequest, Executor, ExecutorLog, UpstreamResponse, insert_header,
};
use crate::executors::http::ProxyOptions;
use crate::executors::oauth::{RefreshedCredentials, should_refresh_credentials};
use crate::providers::lookup::model_upstream_id;
use crate::providers::model::Transport;
use crate::runtime_config::{http_status, memory_config};
use crate::session_manager::{SessionIdentityInput, now_ms, resolve_session_id};
use crate::translator::concerns::primitives::{js_string, js_truthy};
use crate::translator::formats::responses_api::normalize_responses_input;

pub const GROK_CLI_VERSION: &str = "0.2.99";
pub const GROK_CLI_CLIENT_IDENTIFIER: &str = "grok-shell";

/// Server-generated item id prefixes that `/responses` cannot resolve when
/// `store=false`.
static SERVER_ID_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(rs|fc|resp|msg)_").expect("static pattern"));

/// Hosted tool types executed server-side by the Grok CLI backend.
const HOSTED_TOOL_TYPES: [&str; 8] = [
    "web_search",
    "x_search",
    "web_search_preview",
    "file_search",
    "image_generation",
    "code_interpreter",
    "mcp",
    "local_shell",
];

/// Fields the cli-chat-proxy Responses API accepts; anything else is stripped.
const RESPONSES_API_ALLOWLIST: [&str; 16] = [
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "include",
    "temperature",
    "top_p",
    "max_output_tokens",
    "parallel_tool_calls",
    "text",
    "metadata",
    "prompt_cache_key",
];

const EFFORT_LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const GROK_CLI_TURN_STORE_MAX: usize = 5000;

/// A native Grok CLI item id: `rs_`/`msg_`/`fc_` followed by a UUID. Anything
/// else that matches `SERVER_ID_PATTERN` is a foreign id the backend cannot
/// resolve.
static GROK_CLI_NATIVE_ITEM_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:rs|msg|fc)_[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
        .expect("static pattern")
});

/// Unknown models omit the effort until live metadata reaches dispatch.
static GROK_45_MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^grok-4\.5(?:$|-)").expect("static pattern"));

/// The parameter shape a freeform (`custom`) tool is flattened onto.
fn freeform_tool_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {"input": {"type": "string"}},
        "required": ["input"],
    })
}

pub fn supports_grok_cli_reasoning_effort(model: &str) -> bool {
    GROK_45_MODEL.is_match(model)
}

// ─── turn tracking ───────────────────────────────────────────────────────

/// One session's last turn index and when it was used.
#[derive(Clone, Copy)]
struct TurnEntry {
    turn: u64,
    last_used: i64,
}

/// Per-session last turn index so multi-turn headers never go backwards within
/// this process.
static SESSION_TURN_STORE: LazyLock<Mutex<HashMap<String, TurnEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The 1-based conversation turn (≈ user messages); the first chat turn is 1.
pub fn count_grok_cli_user_turns(input: Option<&Value>) -> u64 {
    let Some(items) = input.and_then(Value::as_array) else {
        return 1;
    };
    let mut count = 0u64;
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        if obj.get("role").and_then(Value::as_str) == Some("user")
            && (item_type.is_empty() || item_type == "message")
        {
            count += 1;
        }
    }
    count.max(1)
}

/// Prefer the user-message count from the payload (full-history clients), but
/// never decrease against the last index observed for the same session in this
/// process.
///
/// A retry must reuse the turn it already sent. Rust has no stable object
/// identity, and the base loop hands `transform_request` a fresh clone per
/// attempt, so the caller passes `advance` instead: `false` for a retry of the
/// same request, `true` for a new one.
pub fn resolve_grok_cli_turn_idx(session_id: &str, input: Option<&Value>, advance: bool) -> u64 {
    let from_input = count_grok_cli_user_turns(input);
    if session_id.is_empty() {
        return from_input;
    }

    let now = now_ms() as i64;
    let mut store = SESSION_TURN_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let previous = store
        .get(session_id)
        .filter(|entry| now - entry.last_used <= memory_config::SESSION_TTL_MS)
        .map(|entry| entry.turn)
        .unwrap_or(0);

    // A new delta-style request advances the turn; a retry reuses it.
    let turn = if previous > 0 {
        from_input.max(previous + u64::from(advance))
    } else {
        from_input
    };

    // HashMap iteration order is not insertion order, and the exact victim is
    // not observable — any over-cap session simply restarts from its payload
    // count.
    while store.len() >= GROK_CLI_TURN_STORE_MAX {
        let Some(victim) = store.keys().next().cloned() else {
            break;
        };
        store.remove(&victim);
    }
    store.insert(
        session_id.to_string(),
        TurnEntry {
            turn,
            last_used: now,
        },
    );
    turn
}

/// Clears the turn store, for tests.
#[cfg(test)]
fn reset_grok_cli_turn_store() {
    SESSION_TURN_STORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// The number of sessions the turn store holds, for tests.
#[cfg(test)]
fn grok_cli_turn_store_size() -> usize {
    SESSION_TURN_STORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .len()
}

/// An unknown or absent level is `high`, which is what the CLI sends by default.
pub fn normalize_grok_cli_effort(value: Option<&str>) -> String {
    let effort = value.unwrap_or("").trim().to_ascii_lowercase();
    if effort == "max" {
        return "xhigh".to_string();
    }
    if EFFORT_LEVELS.contains(&effort.as_str()) {
        return effort;
    }
    "high".to_string()
}

/// Only the four explicit thread keys are consulted, never the whole body — a
/// client that sends a fresh body per turn must not be treated as a new
/// conversation.
///
/// The connection id falls back to the `id` in `extra`, not to the email other
/// executors use; the connection row id arrives in `extra`.
pub fn resolve_grok_cli_session_id(credentials: &Credentials, body: &Value) -> String {
    // ponytail: clients without stable thread metadata share one connection
    // session; split further when their wire format exposes a durable
    // conversation id.
    let mut explicit = Map::new();
    for key in [
        "prompt_cache_key",
        "session_id",
        "conversation_id",
        "metadata",
    ] {
        if let Some(value) = body.get(key) {
            explicit.insert(key.to_string(), value.clone());
        }
    }
    let explicit = Value::Object(explicit);
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
    resolve_session_id(&SessionIdentityInput {
        headers: &credentials.raw_headers,
        body: &explicit,
        connection_id,
        workspace_id: credentials.psd_str("workspaceId"),
        scope: "grok-cli",
    })
}

// ─── input normalization ─────────────────────────────────────────────────

fn stringify_grok_cli_tool_output(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn is_native_grok_cli_item_id(id: &str) -> bool {
    GROK_CLI_NATIVE_ITEM_ID.is_match(id)
}

/// `None` drops the item.
fn normalize_grok_cli_input_item(item: Value) -> Option<Value> {
    let Some(obj) = item.as_object() else {
        return Some(item);
    };
    let mut clean = obj.clone();
    clean.shift_remove("internal_chat_message_metadata_passthrough");

    let item_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

    if item_type == "reasoning" {
        let id = obj.get("id").and_then(Value::as_str).unwrap_or("");
        let encrypted = obj.get("encrypted_content").and_then(Value::as_str);
        if !is_native_grok_cli_item_id(id) || encrypted.is_none() {
            return None;
        }
        return Some(Value::Object(clean));
    }

    if item_type == "custom_tool_call" {
        let call_id = obj
            .get("call_id")
            .and_then(Value::as_str)
            .or_else(|| obj.get("id").and_then(Value::as_str));
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let call_id = call_id.filter(|s| !s.is_empty())?;
        if name.is_empty() {
            return None;
        }
        let payload = obj.get("input").or_else(|| obj.get("arguments"));
        return Some(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": json!({"input": stringify_grok_cli_tool_output(payload)}).to_string(),
        }));
    }

    if item_type == "custom_tool_call_output" || item_type == "function_call_output" {
        let call_id = obj
            .get("call_id")
            .and_then(Value::as_str)
            .or_else(|| obj.get("id").and_then(Value::as_str));
        let call_id = call_id.filter(|s| !s.is_empty())?;
        return Some(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": stringify_grok_cli_tool_output(obj.get("output")),
        }));
    }

    if item_type == "function_call" {
        let call_id = obj
            .get("call_id")
            .and_then(Value::as_str)
            .or_else(|| obj.get("id").and_then(Value::as_str));
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let call_id = call_id.filter(|s| !s.is_empty())?;
        if name.is_empty() {
            return None;
        }
        let mut normalized = Map::new();
        normalized.insert("type".into(), json!("function_call"));
        if let Some(id) = obj.get("id").and_then(Value::as_str)
            && is_native_grok_cli_item_id(id)
        {
            normalized.insert("id".into(), json!(id));
        }
        normalized.insert("call_id".into(), json!(call_id));
        normalized.insert("name".into(), json!(name));
        normalized.insert(
            "arguments".into(),
            json!(match obj.get("arguments") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => "{}".to_string(),
            }),
        );
        if let Some(status) = obj.get("status").and_then(Value::as_str) {
            normalized.insert("status".into(), json!(status));
        }
        return Some(Value::Object(normalized));
    }

    Some(Value::Object(clean))
}

/// Normalize each item, then drop a `function_call_output` whose
/// `function_call` was itself dropped — the backend rejects an orphaned result.
pub fn normalize_grok_cli_input(body: &mut Value) {
    let Some(items) = body.get("input").and_then(Value::as_array) else {
        return;
    };
    let normalized: Vec<Value> = items
        .iter()
        .cloned()
        .filter_map(normalize_grok_cli_input_item)
        .collect();

    let call_ids: HashSet<String> = normalized
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
        .filter_map(|item| {
            item.get("call_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();

    let kept: Vec<Value> = normalized
        .into_iter()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) != Some("function_call_output")
                || item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| call_ids.contains(id))
        })
        .collect();

    body["input"] = Value::Array(kept);
}

/// A native Grok id survives, a foreign server id is dropped.
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
            let is_foreign_server_id = obj
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| SERVER_ID_PATTERN.is_match(id) && !is_native_grok_cli_item_id(id))
                .is_some();
            if is_foreign_server_id {
                obj.shift_remove("id");
            }
        }
        true
    });
}

/// Flatten the Chat Completions tool shape into the Responses flat format,
/// keeping the hosted tools as they are.
fn normalize_grok_cli_tools(body: &mut Value) {
    let tools_present = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|t| !t.is_empty());
    if !tools_present {
        if let Some(obj) = body.as_object_mut() {
            obj.shift_remove("tools");
            obj.shift_remove("tool_choice");
        }
        return;
    }

    let tools = body
        .get_mut("tools")
        .and_then(Value::as_array_mut)
        .cloned()
        .unwrap_or_default();

    let mut valid_names: HashSet<String> = HashSet::new();
    let mut hosted_types: HashSet<String> = HashSet::new();
    let mut kept: Vec<Value> = Vec::with_capacity(tools.len());

    for tool in tools {
        let Some(obj) = tool.as_object() else {
            continue;
        };
        let tool_type = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if tool_type != "function" {
            if HOSTED_TOOL_TYPES.contains(&tool_type) {
                hosted_types.insert(tool_type.to_string());
                kept.push(tool);
                continue;
            }
            // A nested function shape, or a bare tool carrying a name, falls
            // through to the flattening below; anything else is dropped.
            let looks_like_function =
                obj.contains_key("function") || obj.get("name").and_then(Value::as_str).is_some();
            if !tool_type.is_empty() && !looks_like_function {
                continue;
            }
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
        let parameters = if tool_type == "custom" {
            freeform_tool_parameters()
        } else {
            obj.get("parameters")
                .filter(|p| p.is_object())
                .or_else(|| {
                    function
                        .and_then(|f| f.get("parameters"))
                        .filter(|p| p.is_object())
                })
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}))
        };

        let mut flattened = Map::new();
        flattened.insert("type".into(), json!("function"));
        let truncated: String = name.chars().take(128).collect();
        flattened.insert("name".into(), json!(truncated.clone()));
        if !description.is_empty() {
            flattened.insert("description".into(), json!(description));
        }
        flattened.insert("parameters".into(), parameters);
        // The *truncated* name is what `tool_choice` is validated against.
        valid_names.insert(truncated);
        kept.push(Value::Object(flattened));
    }

    if kept.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.shift_remove("tools");
            obj.shift_remove("tool_choice");
        }
        return;
    }

    if let Some(obj) = body.as_object_mut() {
        obj.insert("tools".into(), Value::Array(kept));
    }

    let choice = body.get("tool_choice").and_then(Value::as_object).cloned();
    let Some(choice) = choice else {
        return;
    };
    let choice_type = choice.get("type").and_then(Value::as_str).unwrap_or("");
    let drop_choice = if choice_type == "function" || choice_type == "custom" {
        let raw = choice
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| {
                choice
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        let name: String = raw.trim().chars().take(128).collect();
        if name.is_empty() || !valid_names.contains(&name) {
            true
        } else {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "name": name}),
                );
            }
            false
        }
    } else {
        !hosted_types.contains(choice_type)
    };
    if drop_choice && let Some(obj) = body.as_object_mut() {
        obj.shift_remove("tool_choice");
    }
}

/// The first matching suffix wins, in `EFFORT_LEVELS` order.
fn resolve_effort_from_model(model_id: &str) -> Option<String> {
    if model_id.is_empty() {
        return None;
    }
    EFFORT_LEVELS
        .iter()
        .find(|level| model_id.ends_with(&format!("-{level}")))
        .map(|level| (*level).to_string())
}

// ─── executor ────────────────────────────────────────────────────────────

/// Also serves the `gcli` and `gb` aliases, which map to the same executor.
pub struct GrokCliExecutor {
    inner: DefaultExecutor,
    /// Set by `transform_request`, read by `build_headers`.
    current_session_id: Mutex<Option<String>>,
    /// The current request id.
    current_req_id: Mutex<Option<String>>,
    /// The current turn index.
    current_turn_idx: Mutex<u64>,
    /// The stable agent id, resolved once per process.
    agent_id: Mutex<Option<String>>,
    /// The current model, surfaced as `x-grok-model-override`.
    current_model: Mutex<Option<String>>,
    /// A retry must reuse the turn it already sent. The base loop clones the body
    /// per attempt, so the identity cannot come from the value — this flag
    /// carries the same signal instead. It is armed once per `execute` (the start
    /// of a logical request) and spent by the first `transform_request` of that
    /// request; later attempts in the same call reuse the stored turn.
    turn_advanced: AtomicBool,
}

impl GrokCliExecutor {
    pub fn new() -> Self {
        Self {
            inner: DefaultExecutor::new("grok-cli"),
            current_session_id: Mutex::new(None),
            current_req_id: Mutex::new(None),
            current_turn_idx: Mutex::new(1),
            agent_id: Mutex::new(None),
            current_model: Mutex::new(None),
            turn_advanced: AtomicBool::new(false),
        }
    }

    fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        cell.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn session_id(&self) -> Option<String> {
        Self::lock(&self.current_session_id)
            .clone()
            .filter(|s| !s.is_empty())
    }

    fn agent_id(&self) -> Option<String> {
        Self::lock(&self.agent_id).clone().filter(|s| !s.is_empty())
    }

    /// A stable machine-derived id, formatted UUID-ish for header aesthetics, so
    /// a connection with no `deviceId` still fingerprints like the CLI.
    ///
    /// `consistent_machine_id` returns a 16-character digest, so the later
    /// windows clamp and the `"a"` group comes out empty — that is the intended
    /// output for this length, not a truncation to fix.
    async fn resolve_agent_id(&self) -> String {
        let machine_id = router_db::identity::consistent_machine_id(
            &router_db::Paths::from_env(),
            Some("grok-cli-agent"),
        )
        .ok();

        match machine_id {
            Some(mid) => {
                let tail = format!("{:0<12}", js_slice(&mid, 0, 12));
                format!(
                    "{}-{}-5{}-a{}-{}",
                    js_slice(&mid, 0, 8),
                    js_slice(&mid, 8, 12),
                    js_slice(&mid, 13, 16),
                    js_slice(&mid, 17, 20),
                    tail,
                )
            }
            None => uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Out-of-range windows clamp to the string length rather than panicking.
fn js_slice(value: &str, start: usize, end: usize) -> String {
    value
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

impl Default for GrokCliExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for GrokCliExecutor {
    fn provider(&self) -> &str {
        self.inner.provider()
    }

    fn config(&self) -> &Transport {
        self.inner.config()
    }

    /// The base URL is the endpoint, suffix and all.
    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        _url_index: usize,
        _credentials: &Credentials,
    ) -> Result<String, ExecError> {
        self.config()
            .base_url
            .clone()
            .ok_or_else(|| ExecError::Build("grok-cli has no base URL".into()))
    }

    /// Delegate to the default executor once a refresh token exists.
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

    /// The registry's `refreshLeadMs` (300000) and no `maxRefreshAgeMs`, so only
    /// the expiry window triggers a refresh.
    fn needs_refresh(&self, credentials: &Credentials) -> bool {
        should_refresh_credentials("grok-cli", credentials, now_ms() as i64)
    }

    /// The CLI fingerprint. Every header here is part of what the proxy checks,
    /// not decoration.
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

        // Static fingerprint from the registry.
        if let Some(configured) = self.config().headers.as_ref() {
            for (key, value) in configured {
                if value.is_null() {
                    continue;
                }
                let name = key.to_ascii_lowercase();
                if headers.contains_key(name.as_str()) {
                    continue;
                }
                insert_header(&mut headers, key, &js_string(value))?;
            }
        }

        let client_identifier = self
            .config()
            .client_identifier
            .clone()
            .or_else(|| {
                headers
                    .get("x-grok-client-identifier")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| GROK_CLI_CLIENT_IDENTIFIER.to_string());
        insert_header(&mut headers, "x-grok-client-identifier", &client_identifier)?;

        let client_version = self
            .config()
            .client_version
            .clone()
            .or_else(|| {
                headers
                    .get("x-grok-client-version")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| GROK_CLI_VERSION.to_string());
        insert_header(&mut headers, "x-grok-client-version", &client_version)?;

        let session_id = self
            .session_id()
            .or_else(|| credentials.connection_id.clone().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let req_id = Self::lock(&self.current_req_id)
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        insert_header(&mut headers, "x-grok-session-id", &session_id)?;
        // The CLI uses the same id for conv + session on chat turns.
        insert_header(&mut headers, "x-grok-conv-id", &session_id)?;
        insert_header(&mut headers, "x-grok-req-id", &req_id)?;
        let turn_idx = *Self::lock(&self.current_turn_idx);
        insert_header(
            &mut headers,
            "x-grok-turn-idx",
            &turn_idx.max(1).to_string(),
        )?;

        if let Some(agent_id) = self.agent_id() {
            insert_header(&mut headers, "x-grok-agent-id", &agent_id)?;
        }

        // The CLI always sets the model override.
        if let Some(model) = Self::lock(&self.current_model)
            .clone()
            .filter(|m| !m.is_empty())
        {
            insert_header(&mut headers, "x-grok-model-override", &model)?;
        }

        // The email may live in the top-level field or the provider-specific
        // data, so fall back either way — an OAuth connection must always
        // fingerprint like the CLI.
        let email = credentials
            .psd_str("email")
            .or(credentials.email.as_deref())
            .filter(|s| !s.is_empty());
        if let Some(email) = email {
            insert_header(&mut headers, "x-email", email)?;
        }
        let user_id = credentials
            .psd_str("userId")
            .or_else(|| credentials.extra.get("userId").and_then(Value::as_str))
            .or_else(|| {
                credentials
                    .extra
                    .get("providerUserId")
                    .and_then(Value::as_str)
            })
            .filter(|s| !s.is_empty());
        if let Some(user_id) = user_id {
            insert_header(&mut headers, "x-userid", user_id)?;
        }

        Ok(headers)
    }

    /// A 402 `personal-team-blocked:spending-limit` is surfaced as payment/quota
    /// so the fallback layer can rotate the connection.
    fn parse_error(&self, status: u16, body_text: &str) -> Value {
        if status == http_status::PAYMENT_REQUIRED
            && !body_text.is_empty()
            && let Ok(parsed) = serde_json::from_str::<Value>(body_text)
        {
            // Take the first truthy `error` or `message`, then keep it only if
            // it is a string: a truthy non-string `error` yields `bodyText`, it
            // does not fall through to `message`.
            let raw = parsed
                .get("error")
                .filter(|v| js_truthy(v))
                .or_else(|| parsed.get("message").filter(|v| js_truthy(v)))
                .and_then(Value::as_str);
            let message = match raw {
                Some(text) => text.to_string(),
                None => body_text.to_string(),
            };
            // An absent code becomes the empty string and is kept; a truthy
            // non-string one is dropped.
            let mut out = json!({"status": status, "message": message});
            match parsed.get("code") {
                Some(code) if js_truthy(code) => {
                    if let Some(code) = code.as_str() {
                        out["code"] = json!(code);
                    }
                }
                _ => out["code"] = json!(""),
            }
            return out;
        }
        self.inner.parse_error(status, body_text)
    }

    /// Normalize the input, tools and reasoning, then resolve the session, the
    /// turn index and the upstream model id.
    fn transform_request(
        &self,
        model: &str,
        mut body: Value,
        _stream: bool,
        credentials: &Credentials,
    ) -> Value {
        let session_id = resolve_grok_cli_session_id(credentials, &body);
        *Self::lock(&self.current_session_id) = Some(session_id.clone());
        *Self::lock(&self.current_req_id) = Some(uuid::Uuid::new_v4().to_string());
        let agent_id = credentials
            .psd_str("deviceId")
            .or_else(|| credentials.psd_str("agentId"))
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        *Self::lock(&self.agent_id) = agent_id;

        if let Some(normalized) = normalize_responses_input(body.get("input")) {
            body["input"] = Value::Array(normalized);
        }

        // Chat Completions clients arrive with `messages[]` — the translator
        // should have converted already, but guard the empty input.
        let input_empty = match body.get("input") {
            Some(Value::Array(items)) => items.is_empty(),
            Some(other) => !js_truthy(other),
            None => true,
        };
        if input_empty {
            let messages = body
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .filter(|m| !m.is_empty());
            match messages {
                // Soft fallback: map messages → input messages (string content
                // only).
                Some(messages) => {
                    let mapped: Vec<Value> = messages
                        .iter()
                        .map(|message| {
                            // A string content passes through; an absent or null
                            // one becomes the two-character `""`, because the
                            // value is JSON-stringified.
                            let content = match message.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(Value::Null) | None => "\"\"".to_string(),
                                Some(other) => other.to_string(),
                            };
                            json!({
                                "type": "message",
                                "role": message.get("role").and_then(Value::as_str).unwrap_or("user"),
                                "content": content,
                            })
                        })
                        .collect();
                    body["input"] = Value::Array(mapped);
                    if let Some(obj) = body.as_object_mut() {
                        obj.shift_remove("messages");
                    }
                }
                None => {
                    body["input"] = json!([{"type": "message", "role": "user", "content": "..."}]);
                }
            }
        }

        // `role: "system"` stays as-is: the official grok-pager HAR sends
        // `system`, not `developer` (Codex converts; Grok CLI does not).
        normalize_grok_cli_input(&mut body);
        strip_stored_item_references(&mut body);
        normalize_grok_cli_tools(&mut body);

        // Turn index after the input is finalized. The first attempt of a
        // request advances; a retry of the same request reuses what it sent.
        let advance = self.turn_advanced.swap(true, Ordering::SeqCst);
        let turn = resolve_grok_cli_turn_idx(&session_id, body.get("input"), advance);
        *Self::lock(&self.current_turn_idx) = turn;

        body["stream"] = json!(true);
        body["store"] = json!(false);

        // Resolve the upstream model id (strip the effort-suffix virtual models).
        let requested = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(model)
            .to_string();
        let model_effort = resolve_effort_from_model(&requested);
        let mut resolved_model = requested.clone();
        if let Some(effort) = &model_effort {
            resolved_model = resolved_model
                .strip_suffix(&format!("-{effort}"))
                .unwrap_or(&resolved_model)
                .to_string();
        }
        resolved_model = model_upstream_id("gcli", &resolved_model);
        // Also try the provider id key.
        if resolved_model == requested {
            resolved_model = model_upstream_id("grok-cli", &resolved_model);
        }
        body["model"] = json!(resolved_model.clone());
        *Self::lock(&self.current_model) = Some(resolved_model.clone());

        // Reasoning effort priority: explicit > `reasoning_effort` > model
        // suffix > default. `grok-build` and Composer reject the effort but
        // still accept `summary` and the encrypted continuity blob.
        let supports_effort = supports_grok_cli_reasoning_effort(&resolved_model);
        let requested_effort = body
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(model_effort);

        let reasoning_is_object = body.get("reasoning").is_some_and(|r| r.is_object());
        if !reasoning_is_object {
            let mut reasoning = Map::new();
            reasoning.insert("summary".into(), json!("concise"));
            if supports_effort {
                reasoning.insert(
                    "effort".into(),
                    json!(normalize_grok_cli_effort(requested_effort.as_deref())),
                );
            }
            body["reasoning"] = Value::Object(reasoning);
        } else if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
            if supports_effort {
                // A falsy effort falls through to the next source, and a truthy
                // non-string one normalizes to `high` rather than to the
                // fallback.
                let existing = reasoning
                    .get("effort")
                    .filter(|v| js_truthy(v))
                    .map(|v| v.as_str().unwrap_or("").to_string());
                let effort = existing.or(requested_effort);
                reasoning.insert(
                    "effort".into(),
                    json!(normalize_grok_cli_effort(effort.as_deref())),
                );
            } else {
                reasoning.shift_remove("effort");
            }
            if !reasoning.get("summary").is_some_and(js_truthy) {
                reasoning.insert("summary".into(), json!("concise"));
            }
        }
        if let Some(obj) = body.as_object_mut() {
            obj.shift_remove("reasoning_effort");
        }

        // Encrypted reasoning for multi-turn continuity (the CLI always asks
        // for it). `effort !== "none"` means an absent or non-string effort
        // still requests the blob.
        let wants_encrypted = body.get("reasoning").is_some_and(js_truthy)
            && body
                .get("reasoning")
                .and_then(|r| r.get("effort"))
                .and_then(Value::as_str)
                != Some("none");
        if wants_encrypted {
            let mut include = body
                .get("include")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !include
                .iter()
                .any(|v| v.as_str() == Some("reasoning.encrypted_content"))
            {
                include.push(json!("reasoning.encrypted_content"));
            }
            body["include"] = Value::Array(include);
        }

        if let Some(obj) = body.as_object_mut() {
            // Chat Completions leftovers the Responses API rejects.
            for key in [
                "messages",
                "max_tokens",
                "max_completion_tokens",
                "n",
                "seed",
                "logprobs",
                "top_logprobs",
                "frequency_penalty",
                "presence_penalty",
                "logit_bias",
                "user",
                "stream_options",
                "prompt_cache_retention",
                "safety_identifier",
                // `store=false` → the backend cannot resolve a previous
                // response.
                "previous_response_id",
            ] {
                obj.shift_remove(key);
            }

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

    /// Lazy-resolve the stable agent id once per process, then run the base
    /// loop.
    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        let has_device_id = req
            .credentials
            .psd_str("deviceId")
            .is_some_and(|s| !s.is_empty());
        if has_device_id {
            let device_id = req
                .credentials
                .psd_str("deviceId")
                .unwrap_or("")
                .to_string();
            *Self::lock(&self.agent_id) = Some(device_id);
        } else if self.agent_id().is_none() {
            let agent_id = self.resolve_agent_id().await;
            *Self::lock(&self.agent_id) = Some(agent_id);
        }

        // One `execute` is one logical request: arm the advance, and let the
        // first `transform_request` spend it. The base loop's per-URL attempts
        // then reuse the index already sent, which is what keeps a retry from
        // advancing `x-grok-turn-idx`.
        self.turn_advanced.store(false, Ordering::SeqCst);

        BaseLoop { outer: self }.execute(req).await
    }
}

/// A view of a `GrokCliExecutor` that inherits the trait's default `execute`.
///
/// Delegating to the inner `DefaultExecutor`
/// instead would run the base loop against the *inner* hooks, so the CLI
/// fingerprint headers and the whole body transform would never be applied.
/// The adapter keeps the loop single-sourced in `executor.rs` while resolving
/// every hook on the outer executor.
///
/// No `#[async_trait]` here: the impl overrides only the synchronous hooks and
/// inherits the trait's default `execute`.
struct BaseLoop<'a> {
    outer: &'a GrokCliExecutor,
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

    /// `SESSION_TURN_STORE` is process-global and two tests clear it. Without
    /// this guard a concurrent `reset` wipes the other's `"sess-1"` entry
    /// mid-assertion. A poisoned lock is recovered so one failure does not
    /// cascade.
    fn turn_store_guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn user_turn_count_ignores_non_message_items() {
        assert_eq!(count_grok_cli_user_turns(None), 1);
        assert_eq!(
            count_grok_cli_user_turns(Some(&json!([]))),
            1,
            "the floor is one"
        );
        let input = json!([
            {"role": "user", "content": "a"},
            {"type": "message", "role": "user", "content": "b"},
            {"type": "function_call", "role": "user"},
            {"role": "assistant", "content": "c"},
            "a string item",
        ]);
        assert_eq!(count_grok_cli_user_turns(Some(&input)), 2);
    }

    #[test]
    fn the_turn_index_never_goes_backwards_and_advances_on_a_new_request() {
        let _serial = turn_store_guard();
        reset_grok_cli_turn_store();
        let session = "sess-1";
        let one_turn = json!([{"role": "user", "content": "delta"}]);

        // First request: the payload count is the floor.
        assert_eq!(
            resolve_grok_cli_turn_idx(session, Some(&one_turn), false),
            1
        );
        // A retry of that request reuses the index it already sent.
        assert_eq!(
            resolve_grok_cli_turn_idx(session, Some(&one_turn), false),
            1
        );
        // A new delta-style request advances even though the payload still
        // carries a single user message.
        assert_eq!(resolve_grok_cli_turn_idx(session, Some(&one_turn), true), 2);
        // A full-history client jumps forward to its own count.
        let many = json!([
            {"role": "user", "content": "1"},
            {"role": "user", "content": "2"},
            {"role": "user", "content": "3"},
            {"role": "user", "content": "4"},
        ]);
        assert_eq!(resolve_grok_cli_turn_idx(session, Some(&many), true), 4);
        // An empty session id resolves straight from the payload.
        assert_eq!(resolve_grok_cli_turn_idx("", Some(&many), true), 4);
        reset_grok_cli_turn_store();
    }

    #[test]
    fn the_turn_store_is_bounded() {
        let _serial = turn_store_guard();
        reset_grok_cli_turn_store();
        for i in 0..(GROK_CLI_TURN_STORE_MAX + 10) {
            resolve_grok_cli_turn_idx(&format!("s{i}"), None, false);
        }
        assert!(grok_cli_turn_store_size() <= GROK_CLI_TURN_STORE_MAX);
        reset_grok_cli_turn_store();
    }

    #[test]
    fn effort_normalization_folds_max_and_defaults_to_high() {
        assert_eq!(normalize_grok_cli_effort(Some("max")), "xhigh");
        assert_eq!(normalize_grok_cli_effort(Some("LOW")), "low");
        assert_eq!(normalize_grok_cli_effort(Some("nonsense")), "high");
        assert_eq!(normalize_grok_cli_effort(None), "high");
    }

    #[test]
    fn reasoning_effort_is_gated_on_the_grok_45_family() {
        assert!(supports_grok_cli_reasoning_effort("grok-4.5"));
        assert!(supports_grok_cli_reasoning_effort("grok-4.5-high"));
        assert!(!supports_grok_cli_reasoning_effort("grok-build"));
        assert!(!supports_grok_cli_reasoning_effort("grok-4.6"));
    }

    #[test]
    fn effort_suffixes_resolve_in_level_order() {
        assert_eq!(
            resolve_effort_from_model("grok-4.5-high"),
            Some("high".into())
        );
        assert_eq!(resolve_effort_from_model("grok-4.5"), None);
        assert_eq!(resolve_effort_from_model(""), None);
    }

    #[test]
    fn native_item_ids_are_recognized_and_foreign_ones_are_not() {
        assert!(is_native_grok_cli_item_id(
            "rs_123e4567-e89b-12d3-a456-426614174000"
        ));
        assert!(is_native_grok_cli_item_id(
            "msg_123E4567-E89B-12D3-A456-426614174000"
        ));
        assert!(!is_native_grok_cli_item_id("rs_abc"));
        assert!(!is_native_grok_cli_item_id("call_1"));
    }

    #[test]
    fn reasoning_items_without_a_native_id_or_encrypted_content_are_dropped() {
        let mut body = json!({"input": [
            {"type": "reasoning", "id": "rs_123e4567-e89b-12d3-a456-426614174000", "encrypted_content": "blob", "internal_chat_message_metadata_passthrough": {"x": 1}},
            {"type": "reasoning", "id": "rs_abc", "encrypted_content": "blob"},
            {"type": "reasoning", "id": "rs_123e4567-e89b-12d3-a456-426614174000"},
        ]});
        normalize_grok_cli_input(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["encrypted_content"], json!("blob"));
        assert!(
            items[0]
                .get("internal_chat_message_metadata_passthrough")
                .is_none()
        );
    }

    #[test]
    fn a_custom_tool_call_flattens_onto_the_freeform_parameter_shape() {
        let mut body = json!({"input": [
            {"type": "custom_tool_call", "call_id": "c1", "name": " shell ", "input": {"cmd": "ls"}},
        ]});
        normalize_grok_cli_input(&mut body);
        let item = &body["input"][0];
        assert_eq!(item["type"], json!("function_call"));
        assert_eq!(item["name"], json!("shell"));
        assert_eq!(item["arguments"], json!(r#"{"input":"{\"cmd\":\"ls\"}"}"#));
    }

    #[test]
    fn an_orphaned_function_call_output_is_dropped() {
        let mut body = json!({"input": [
            {"type": "function_call", "call_id": "kept", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "kept", "output": "ok"},
            {"type": "function_call_output", "call_id": "orphan", "output": "lost"},
        ]});
        normalize_grok_cli_input(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["output"], json!("ok"));
    }

    #[test]
    fn a_native_id_survives_the_stored_reference_strip() {
        let mut body = json!({"input": [
            {"type": "function_call", "id": "rs_123e4567-e89b-12d3-a456-426614174000", "call_id": "c"},
            {"type": "message", "id": "resp_abc", "role": "user"},
            {"type": "item_reference", "id": "rs_1"},
        ]});
        strip_stored_item_references(&mut body);
        let items = body["input"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0]["id"],
            json!("rs_123e4567-e89b-12d3-a456-426614174000")
        );
        assert!(items[1].get("id").is_none());
    }

    #[test]
    fn grok_tools_keep_hosted_types_and_flatten_functions() {
        let mut body = json!({"tools": [
            {"type": "web_search"},
            {"type": "x_search"},
            {"type": "function", "function": {"name": " read ", "parameters": {"type": "object"}}},
            {"type": "custom", "name": "freeform"},
            {"type": "nonsense"},
        ]});
        normalize_grok_cli_tools(&mut body);
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4, "the unknown type is dropped");
        assert_eq!(tools[0]["type"], json!("web_search"));
        assert_eq!(tools[1]["type"], json!("x_search"));
        assert_eq!(tools[2]["name"], json!("read"));
        assert!(tools[2].get("function").is_none());
        assert_eq!(
            tools[3]["parameters"],
            freeform_tool_parameters(),
            "a custom tool gets the freeform input schema"
        );
    }

    #[test]
    fn an_empty_tool_list_drops_the_choice_too() {
        let mut body = json!({"tools": [], "tool_choice": "auto"});
        normalize_grok_cli_tools(&mut body);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn a_hosted_tool_choice_is_kept_and_an_unknown_one_dropped() {
        let mut body =
            json!({"tools": [{"type": "web_search"}], "tool_choice": {"type": "web_search"}});
        normalize_grok_cli_tools(&mut body);
        assert!(body.get("tool_choice").is_some());

        let mut body =
            json!({"tools": [{"type": "web_search"}], "tool_choice": {"type": "mystery"}});
        normalize_grok_cli_tools(&mut body);
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn a_function_tool_choice_is_rewritten_to_the_flat_shape() {
        let mut body = json!({
            "tools": [{"type": "function", "name": "known"}],
            "tool_choice": {"type": "function", "function": {"name": "known"}},
        });
        normalize_grok_cli_tools(&mut body);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "known"})
        );
    }

    #[test]
    fn the_session_id_prefers_the_client_body_fields() {
        let credentials = Credentials::default();
        let body = json!({"prompt_cache_key": "thread-1", "metadata": {"user_id": "u"}});
        assert_eq!(resolve_grok_cli_session_id(&credentials, &body), "thread-1");
    }

    #[test]
    fn parse_error_surfaces_the_402_code() {
        let executor = GrokCliExecutor::new();
        let out = executor.parse_error(
            http_status::PAYMENT_REQUIRED,
            &json!({"code": "personal-team-blocked:spending-limit", "error": "no credit"})
                .to_string(),
        );
        assert_eq!(out["status"], json!(402));
        assert_eq!(out["message"], json!("no credit"));
        assert_eq!(out["code"], json!("personal-team-blocked:spending-limit"));

        // An absent code is the empty string, not a missing key.
        let out = executor.parse_error(
            http_status::PAYMENT_REQUIRED,
            &json!({"error": "x"}).to_string(),
        );
        assert_eq!(out["code"], json!(""));

        // A truthy non-string code is dropped.
        let out = executor.parse_error(
            http_status::PAYMENT_REQUIRED,
            &json!({"code": 7, "error": "x"}).to_string(),
        );
        assert!(out.get("code").is_none());

        // A non-string error falls back to the raw body.
        let body = json!({"error": {"nested": true}}).to_string();
        let out = executor.parse_error(http_status::PAYMENT_REQUIRED, &body);
        assert_eq!(out["message"], json!(body));

        // A non-JSON 402 falls through to the default shape.
        let out = executor.parse_error(http_status::PAYMENT_REQUIRED, "plain text");
        assert_eq!(out["status"], json!(402));
        assert!(out.get("code").is_none());
    }

    #[test]
    fn the_agent_id_windows_clamp_like_javascript_slice() {
        // `consistent_machine_id` yields 16 hex characters, so the last two
        // windows run past the end and the `a` group is empty.
        assert_eq!(js_slice("0123456789abcdef", 0, 8), "01234567");
        assert_eq!(js_slice("0123456789abcdef", 8, 12), "89ab");
        assert_eq!(js_slice("0123456789abcdef", 13, 16), "def");
        assert_eq!(js_slice("0123456789abcdef", 17, 20), "");
        assert_eq!(
            format!("{:0<12}", js_slice("0123456789abcdef", 0, 12)),
            "0123456789ab"
        );
        assert_eq!(js_slice("short", 0, 8), "short");
    }
}
