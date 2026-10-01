//! Session-id resolution.
//!
//! Three module-level stores, `LazyLock<Mutex<HashMap>>` with lazy eviction on
//! read: a background timer thread buys nothing for a store that is only ever
//! touched from request handlers, and lazy eviction has no drift.
//!
//! The id format is load-bearing: it mirrors the client binary's
//! `randomUUID() + Date.now()` so a cached prefix stays stable for the process
//! lifetime, scoped per connection.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::runtime_config::memory_config;

/// Session time-to-live.
const SESSION_TTL_MS: u64 = memory_config::SESSION_TTL_MS as u64;
/// Per-store caps: runtime sessions, assistant-text sessions, continuation
/// sessions.
const MAX_RUNTIME_SESSIONS: usize = 1000;
const MAX_ASSISTANT_SESSIONS: usize = 5000;
const MAX_CONTINUATION_SESSIONS: usize = 5000;
/// Minimum and maximum assistant text used to derive a session id.
const ASSISTANT_MIN_LEN: usize = 50;
const ASSISTANT_CAP_LEN: usize = 50;

/// Headers a session id may arrive in.
const SESSION_HEADER_KEYS: [&str; 4] = [
    "x-session-id",
    "session-id",
    "session_id",
    "x-amp-thread-id",
];
const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// `Date.now()`.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `crypto.randomUUID()`.
fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `generateBinaryStyleId()`: `randomUUID() + Date.now()`.
pub fn generate_binary_style_id() -> String {
    format!("{}{}", random_uuid(), now_ms())
}

/// One stored id plus its last-use stamp.
#[derive(Clone, Copy)]
struct Entry {
    last_used: u64,
}

/// A keyed id store with lazy TTL eviction and a hard size cap.
struct Store {
    entries: HashMap<String, String>,
    meta: HashMap<String, Entry>,
    cap: usize,
}

impl Store {
    fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            meta: HashMap::new(),
            cap,
        }
    }

    fn get(&mut self, key: &str) -> Option<String> {
        let now = now_ms();
        if let Some(entry) = self.meta.get_mut(key) {
            if now.saturating_sub(entry.last_used) > SESSION_TTL_MS {
                self.entries.remove(key);
                self.meta.remove(key);
                return None;
            }
            entry.last_used = now;
        }
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: String, value: String) {
        // Evict expired entries opportunistically, then honour the cap: drop
        // the oldest inserted key.
        let now = now_ms();
        let expired: Vec<String> = self
            .meta
            .iter()
            .filter(|(_, e)| now.saturating_sub(e.last_used) > SESSION_TTL_MS)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired {
            self.entries.remove(&k);
            self.meta.remove(&k);
        }
        while self.entries.len() >= self.cap {
            // HashMap iteration order is not insertion order, but the exact
            // victim is not observable — any over-cap key is a valid eviction.
            let Some(victim) = self.entries.keys().next().cloned() else {
                break;
            };
            self.entries.remove(&victim);
            self.meta.remove(&victim);
        }
        self.meta.insert(key.clone(), Entry { last_used: now });
        self.entries.insert(key, value);
    }
}

static RUNTIME_STORE: LazyLock<Mutex<Store>> =
    LazyLock::new(|| Mutex::new(Store::new(MAX_RUNTIME_SESSIONS)));
static ASSISTANT_STORE: LazyLock<Mutex<Store>> =
    LazyLock::new(|| Mutex::new(Store::new(MAX_ASSISTANT_SESSIONS)));
static CONTINUATION_STORE: LazyLock<Mutex<Store>> =
    LazyLock::new(|| Mutex::new(Store::new(MAX_CONTINUATION_SESSIONS)));

/// `deriveSessionId(connectionId)`.
pub fn derive_session_id(connection_id: Option<&str>) -> String {
    let Some(connection_id) = connection_id.filter(|s| !s.is_empty()) else {
        return generate_binary_style_id();
    };
    let mut store = RUNTIME_STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = store.get(connection_id) {
        return existing;
    }
    let session_id = generate_binary_style_id();
    store.insert(connection_id.to_string(), session_id.clone());
    session_id
}

/// `sha16(text)`: first 16 hex chars of the sha256 digest.
fn sha16(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// `normalizeSessionId(value)`.
fn normalize_session_id(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() || v.len() > 256 {
        return None;
    }
    Some(v.to_string())
}

/// `extractClaudeCodeSession(userId)`: `_session_{uuid}` suffix, else a JSON
/// `{session_id}` object.
fn extract_claude_code_session(user_id: Option<&str>) -> Option<String> {
    let user_id = user_id.filter(|s| !s.is_empty())?;
    if let Some(idx) = user_id.rfind("_session_") {
        let tail = &user_id[idx + "_session_".len()..];
        // The regex is `_session_([a-f0-9-]+)$` — anchored at the end.
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return normalize_session_id(tail);
        }
    }
    if user_id.starts_with('{') {
        return serde_json::from_str::<Value>(user_id)
            .ok()
            .and_then(|v| {
                v.get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .and_then(|s| normalize_session_id(&s));
    }
    None
}

/// `headerValue(headers, key)`. `headers` is lower-cased already.
fn header_value(headers: &HashMap<String, String>, key: &str) -> Option<String> {
    headers.get(key).and_then(|v| normalize_session_id(v))
}

/// `extractClientSessionId(headers, body, scope)`.
fn extract_client_session_id(
    headers: &HashMap<String, String>,
    body: &Value,
    scope: &str,
) -> Option<String> {
    let claude = extract_claude_code_session(
        body.get("metadata")
            .and_then(|m| m.get("user_id"))
            .and_then(Value::as_str),
    )
    .or_else(|| header_value(headers, CLAUDE_CODE_SESSION_HEADER));
    if let Some(claude) = claude {
        return Some(format!("claude:{claude}"));
    }
    for key in SESSION_HEADER_KEYS {
        if let Some(v) = header_value(headers, key) {
            return Some(v);
        }
    }
    if scope != "kiro"
        && let Some(v) = header_value(headers, "x-client-request-id")
    {
        return Some(v);
    }

    ["prompt_cache_key", "session_id", "conversation_id"]
        .iter()
        .find_map(|k| {
            body.get(*k)
                .and_then(Value::as_str)
                .and_then(normalize_session_id)
        })
        .or_else(|| {
            if scope == "kiro" {
                None
            } else {
                body.get("metadata")
                    .and_then(|m| m.get("user_id"))
                    .and_then(Value::as_str)
                    .and_then(normalize_session_id)
            }
        })
}

/// `requestMessages(body)`.
fn request_messages(body: &Value) -> &[Value] {
    if let Some(m) = body.get("messages").and_then(Value::as_array) {
        return m;
    }
    if let Some(i) = body.get("input").and_then(Value::as_array) {
        return i;
    }
    &[]
}

/// `accumulateAssistantText(body)` — capped at 50 chars.
fn accumulate_assistant_text(body: &Value) -> String {
    let mut text = String::new();
    for item in request_messages(body) {
        if item.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        match item.get("content") {
            Some(Value::String(s)) => text.push_str(s),
            Some(Value::Array(parts)) => {
                for c in parts {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    } else if let Some(o) = c.get("output").and_then(Value::as_str) {
                        text.push_str(o);
                    }
                }
            }
            _ => {}
        }
        if text.len() >= ASSISTANT_CAP_LEN {
            break;
        }
    }
    text
}

/// `assistantTextSessionId(scope, body)`.
fn assistant_text_session_id(scope: &str, body: &Value) -> Option<String> {
    let text = accumulate_assistant_text(body);
    if text.len() < ASSISTANT_MIN_LEN {
        return None;
    }
    // Slicing a Rust `String` by byte would panic mid-codepoint, so cut on a
    // char boundary.
    let capped: String = text.chars().take(ASSISTANT_CAP_LEN).collect();
    let hash = sha16(&format!("{scope}:{capped}"));
    let mut store = ASSISTANT_STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = store.get(&hash) {
        return Some(existing);
    }
    let session_id = generate_binary_style_id();
    store.insert(hash, session_id.clone());
    Some(session_id)
}

/// The input `resolveSessionIdentity` takes. `headers` is lower-cased.
#[derive(Debug, Clone)]
pub struct SessionIdentityInput<'a> {
    pub headers: &'a HashMap<String, String>,
    pub body: &'a Value,
    pub connection_id: Option<&'a str>,
    pub workspace_id: Option<&'a str>,
    pub scope: &'a str,
}

impl Default for SessionIdentityInput<'_> {
    fn default() -> Self {
        static NO_HEADERS: LazyLock<HashMap<String, String>> = LazyLock::new(HashMap::new);
        static NO_BODY: LazyLock<Value> = LazyLock::new(|| Value::Null);
        Self {
            headers: &NO_HEADERS,
            body: &NO_BODY,
            connection_id: None,
            workspace_id: None,
            scope: "",
        }
    }
}

/// `resolveSessionIdentity({headers, body, connectionId, workspaceId, scope})`.
/// Returns `(session_id, ephemeral)`.
pub fn resolve_session_identity(input: &SessionIdentityInput<'_>) -> (String, bool) {
    if let Some(client) = extract_client_session_id(input.headers, input.body, input.scope) {
        return (client, false);
    }
    if input.scope != "kiro" {
        let scope_key = format!("{}:{}", input.scope, input.connection_id.unwrap_or(""));
        if let Some(from_assistant) = assistant_text_session_id(&scope_key, input.body) {
            return (from_assistant, false);
        }
    }
    if let Some(ws) = input.workspace_id.and_then(normalize_session_id) {
        return (ws, false);
    }
    if input.scope == "kiro" {
        return (generate_binary_style_id(), true);
    }
    (derive_session_id(input.connection_id), false)
}

/// `resolveSessionId(opts)`.
pub fn resolve_session_id(input: &SessionIdentityInput<'_>) -> String {
    resolve_session_identity(input).0
}

/// `resolveContinuationId({sessionId, connectionId, scope, ephemeral})`.
pub fn resolve_continuation_id(
    session_id: Option<&str>,
    connection_id: Option<&str>,
    scope: &str,
    ephemeral: bool,
) -> String {
    if ephemeral {
        return random_uuid();
    }
    let key = format!(
        "{scope}:{}:{}",
        connection_id.unwrap_or(""),
        session_id.unwrap_or("")
    );
    let mut store = CONTINUATION_STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = store.get(&key) {
        return existing;
    }
    let continuation_id = random_uuid();
    store.insert(key, continuation_id.clone());
    continuation_id
}

/// `toNumericSessionId(sessionId)`: an already-numeric id passes through,
/// anything else becomes `-<int64>` from the first 8 sha256 bytes with the top
/// bit cleared.
pub fn to_numeric_session_id(session_id: Option<&str>) -> Option<String> {
    let v = session_id.and_then(normalize_session_id)?;
    if v.parse::<i64>().is_ok() {
        return Some(v);
    }
    let digest = Sha256::digest(v.as_bytes());
    let n = u64::from_be_bytes(digest[..8].try_into().unwrap()) & 0x7fff_ffff_ffff_ffff;
    Some(format!("-{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn no_headers() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn derived_ids_are_stable_per_connection_and_fresh_without_one() {
        let a = derive_session_id(Some("conn-a"));
        let b = derive_session_id(Some("conn-a"));
        assert_eq!(a, b, "same connection reuses the id");
        assert_ne!(derive_session_id(None), derive_session_id(None));
        assert_ne!(
            derive_session_id(Some("conn-a")),
            derive_session_id(Some("conn-b"))
        );
    }

    #[test]
    fn claude_code_session_wins_and_is_prefixed() {
        let mut headers = HashMap::new();
        headers.insert("x-claude-code-session-id".into(), "from-header".into());
        let body = json!({"metadata": {"user_id": "_session_abc-123"}});
        let input = SessionIdentityInput {
            headers: &headers,
            body: &body,
            scope: "claude",
            ..Default::default()
        };
        assert_eq!(resolve_session_id(&input), "claude:abc-123");
    }

    #[test]
    fn claude_code_session_from_json_user_id() {
        let headers = no_headers();
        let body = json!({"metadata": {"user_id": "{\"session_id\":\"xyz\"}"}});
        let input = SessionIdentityInput {
            headers: &headers,
            body: &body,
            scope: "",
            ..Default::default()
        };
        assert_eq!(resolve_session_id(&input), "claude:xyz");
    }

    #[test]
    fn generic_session_headers_are_tried_in_order() {
        let mut headers = HashMap::new();
        headers.insert("x-session-id".into(), "sess-1".into());
        let body = json!({});
        let input = SessionIdentityInput {
            headers: &headers,
            body: &body,
            scope: "",
            ..Default::default()
        };
        assert_eq!(resolve_session_id(&input), "sess-1");
    }

    #[test]
    fn x_client_request_id_is_ignored_for_kiro_scope() {
        let mut headers = HashMap::new();
        headers.insert("x-client-request-id".into(), "req-1".into());
        let body = json!({});
        let kiro = SessionIdentityInput {
            headers: &headers,
            body: &body,
            scope: "kiro",
            ..Default::default()
        };
        let (id, ephemeral) = resolve_session_identity(&kiro);
        assert!(
            ephemeral,
            "kiro falls to an ephemeral id when nothing else matches"
        );
        assert!(!id.is_empty());

        let other = SessionIdentityInput {
            headers: &headers,
            body: &body,
            scope: "claude",
            ..Default::default()
        };
        assert_eq!(resolve_session_id(&other), "req-1");
    }

    #[test]
    fn assistant_text_hash_needs_50_chars_and_is_stable() {
        let headers = no_headers();
        let short = json!({"messages": [{"role": "assistant", "content": "too short"}]});
        let input = SessionIdentityInput {
            headers: &headers,
            body: &short,
            connection_id: Some("c"),
            scope: "claude",
            ..Default::default()
        };
        let (id, _) = resolve_session_identity(&input);
        // Falls through to deriveSessionId, which is stable per connection.
        assert_eq!(id, derive_session_id(Some("c")));

        let long = json!({"messages": [{"role": "assistant", "content": "x".repeat(60)}]});
        let input = SessionIdentityInput {
            headers: &headers,
            body: &long,
            connection_id: Some("c"),
            scope: "claude",
            ..Default::default()
        };
        let first = resolve_session_id(&input);
        let second = resolve_session_id(&input);
        assert_eq!(first, second, "same assistant text reuses the id");
        assert_ne!(first, derive_session_id(Some("c")));
    }

    #[test]
    fn numeric_session_ids_pass_through_others_are_hashed() {
        assert_eq!(to_numeric_session_id(Some("12345")), Some("12345".into()));
        assert_eq!(to_numeric_session_id(Some("-99")), Some("-99".into()));
        let hashed = to_numeric_session_id(Some("uuid-here")).unwrap();
        assert!(hashed.starts_with('-'));
        assert_eq!(
            to_numeric_session_id(Some("uuid-here")),
            Some(hashed.clone()),
            "deterministic"
        );
        assert_eq!(to_numeric_session_id(Some("  ")), None);
    }

    #[test]
    fn continuation_ids_are_stable_per_key_unless_ephemeral() {
        let a = resolve_continuation_id(Some("s1"), Some("c1"), "claude", false);
        let b = resolve_continuation_id(Some("s1"), Some("c1"), "claude", false);
        assert_eq!(a, b);
        assert_ne!(
            resolve_continuation_id(Some("s2"), Some("c1"), "claude", false),
            a
        );
        assert_ne!(
            resolve_continuation_id(Some("s1"), Some("c1"), "claude", true),
            resolve_continuation_id(Some("s1"), Some("c1"), "claude", true)
        );
    }
}
