//! Gemini thought-signature cache.
//!
//! Each Gemini backend only accepts signatures it produced: replaying a Claude
//! signature to Gemini is a 400 "Corrupted thought signature". So every entry
//! records the model family that produced it, and a lookup for a different
//! family misses.
//!
//! Entries are not mirrored into the `kv` table under the
//! `gemini_thought_signatures` scope: nothing would read them back, so this
//! keeps the RAM cache only. `ponytail:` add the kv mirror if a future caller
//! needs signatures to survive a restart.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::session_manager::now_ms;

const MAX_SIGNATURES: usize = 2000;
const MEMORY_TTL_MS: u64 = 1000 * 60 * 60;

#[derive(Clone)]
struct Entry {
    signature: String,
    family: Option<String>,
    expires_at: u64,
}

static STORE: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `signatureFamily(model)`: `"claude"`, `"gemini"`, the lower-cased model, or
/// `None` for an empty/non-string model.
pub fn signature_family(model: Option<&str>) -> Option<String> {
    let m = model.unwrap_or("").to_lowercase();
    if m.is_empty() {
        return None;
    }
    if m.contains("claude") {
        return Some("claude".to_string());
    }
    if m.contains("gemini") {
        return Some("gemini".to_string());
    }
    Some(m)
}

/// `isCompatible(entry, family)`: entries stored before families were recorded
/// (no `family`) stay usable for any model.
fn is_compatible(entry: &Entry, family: Option<&str>) -> bool {
    entry.family.is_none() || family.is_none() || entry.family.as_deref() == family
}

/// `pruneMemoryExpired()`: drop expired entries, then evict oldest-inserted
/// down to the cap.
fn prune_memory_expired(store: &mut HashMap<String, Entry>) {
    let now = now_ms();
    store.retain(|_, v| v.expires_at > now);
    while store.len() > MAX_SIGNATURES {
        let Some(victim) = store.keys().next().cloned() else {
            break;
        };
        store.remove(&victim);
    }
}

/// `storeGeminiThoughtSignature(toolCallId, signature, sessionId, model)`.
pub fn store_gemini_thought_signature(
    tool_call_id: Option<&str>,
    signature: Option<&str>,
    session_id: Option<&str>,
    model: Option<&str>,
) {
    let Some(tool_call_id) = tool_call_id.filter(|s| !s.is_empty()) else {
        return;
    };
    let Some(signature) = signature.filter(|s| !s.is_empty()) else {
        return;
    };

    let now = now_ms();
    let family = signature_family(model);
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    prune_memory_expired(&mut store);

    let mut keys: Vec<String> = Vec::new();
    if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
        keys.push(format!("{session_id}:{tool_call_id}"));
    }
    keys.push(tool_call_id.to_string());

    for k in keys {
        store.insert(
            k,
            Entry {
                signature: signature.to_string(),
                family: family.clone(),
                expires_at: now + MEMORY_TTL_MS,
            },
        );
    }
}

/// `getGeminiThoughtSignatureSync(toolCallId, sessionId, model)`: RAM only.
pub fn get_gemini_thought_signature_sync(
    tool_call_id: Option<&str>,
    session_id: Option<&str>,
    model: Option<&str>,
) -> Option<String> {
    let tool_call_id = tool_call_id.filter(|s| !s.is_empty())?;
    let family = signature_family(model);
    let now = now_ms();

    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    prune_memory_expired(&mut store);

    if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
        let session_key = format!("{session_id}:{tool_call_id}");
        if let Some(entry) = store.get(&session_key)
            && entry.expires_at > now
            && is_compatible(entry, family.as_deref())
        {
            return Some(entry.signature.clone());
        }
    }

    let entry = store.get(tool_call_id)?;
    if entry.expires_at > now && is_compatible(entry, family.as_deref()) {
        return Some(entry.signature.clone());
    }
    None
}

/// Test/`clear` hook.
pub fn clear_thought_signatures() {
    STORE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The store is process-global and every test that clears it must not
/// interleave with another, in this module or in a translator's tests. A
/// poisoned lock is recovered: one failing test must not turn the rest into
/// false failures.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        test_guard()
    }

    #[test]
    fn family_detection_lowercases_and_buckets_by_backend() {
        assert_eq!(
            signature_family(Some("claude-sonnet-4")),
            Some("claude".into())
        );
        assert_eq!(
            signature_family(Some("Gemini-2.5-Pro")),
            Some("gemini".into())
        );
        assert_eq!(signature_family(Some("gpt-5")), Some("gpt-5".into()));
        assert_eq!(signature_family(None), None);
        assert_eq!(signature_family(Some("")), None);
    }

    #[test]
    fn a_signature_round_trips_by_tool_call_id() {
        let _serial = guard();
        clear_thought_signatures();
        store_gemini_thought_signature(Some("call_1"), Some("sig-a"), None, Some("gemini-2.5-pro"));
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), None, Some("gemini-2.5-pro")),
            Some("sig-a".into())
        );
    }

    #[test]
    fn a_session_scoped_key_is_preferred_then_falls_back_to_the_bare_id() {
        let _serial = guard();
        clear_thought_signatures();
        store_gemini_thought_signature(Some("call_1"), Some("bare"), None, Some("gemini"));
        // The caller's session has no entry of its own, so the bare key answers.
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), Some("other"), Some("gemini")),
            Some("bare".into())
        );

        // A scoped store writes the bare key as well, so it becomes the
        // fallback for every other session.
        store_gemini_thought_signature(
            Some("call_1"),
            Some("scoped"),
            Some("sess"),
            Some("gemini"),
        );
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), Some("sess"), Some("gemini")),
            Some("scoped".into())
        );
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), Some("other"), Some("gemini")),
            Some("scoped".into())
        );
    }

    #[test]
    fn a_cross_family_lookup_misses() {
        let _serial = guard();
        clear_thought_signatures();
        store_gemini_thought_signature(
            Some("call_1"),
            Some("gemini-sig"),
            None,
            Some("gemini-2.5-pro"),
        );
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), None, Some("claude-sonnet-4")),
            None,
            "a Gemini signature must not be replayed to a Claude backend"
        );
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), None, Some("gemini-2.5-flash")),
            Some("gemini-sig".into())
        );
    }

    #[test]
    fn empty_inputs_are_rejected_and_a_familyless_entry_matches_any_model() {
        let _serial = guard();
        clear_thought_signatures();
        store_gemini_thought_signature(Some(""), Some("x"), None, None);
        store_gemini_thought_signature(Some("call_1"), Some(""), None, None);
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_1"), None, Some("m")),
            None
        );

        store_gemini_thought_signature(Some("call_2"), Some("legacy"), None, None);
        assert_eq!(
            get_gemini_thought_signature_sync(Some("call_2"), None, Some("claude")),
            Some("legacy".into()),
            "a pre-family entry stays usable"
        );
        assert_eq!(
            get_gemini_thought_signature_sync(Some(""), None, None),
            None
        );
        assert_eq!(get_gemini_thought_signature_sync(None, None, None), None);
    }
}
