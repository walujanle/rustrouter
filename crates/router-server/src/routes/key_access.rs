//! Per-key access control for the `/v1/*` inference surface.
//!
//! Two modes per key: unrestricted (the default) and restricted, where the key
//! may call only the combos and models named in its allow list. Matching is
//! exact and case-insensitive on **resolved** identities, so every spelling that
//! routes to the same target is treated the same, and a string that merely looks
//! like a listed entry but routes elsewhere is not.
//!
//! An allowed combo grants its members when they are reached *through* that
//! combo. The check runs on the requested target before combo expansion, so a
//! member reached directly still needs its own entry.
//!
//! The gate runs before any credential lookup: a denied request never touches a
//! provider account.

use axum::http::HeaderMap;
use serde_json::Value;

use crate::state::AppState;

/// A restricted key's resolved allow sets.
pub struct KeyAccess {
    pub key_name: String,
    combo_names: std::collections::HashSet<String>,
    models: std::collections::HashSet<String>,
    providers: std::collections::HashSet<String>,
}

/// The client key, read from the same four places the dashboard guard reads:
/// `Bearer`, `x-api-key`, `x-goog-api-key`, `?key=`.
///
/// Handlers' own two-source extraction is not enough here: a restricted key sent
/// as `x-goog-api-key` would look like "no key" and be treated as unrestricted.
pub fn extract_client_api_key(headers: &HeaderMap, query_key: Option<&str>) -> Option<String> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if let Some(auth) = header("authorization")
        && let Some(rest) = auth.strip_prefix("Bearer ")
        && !rest.is_empty()
    {
        return Some(rest.to_string());
    }
    if let Some(value) = header("x-api-key").filter(|s| !s.is_empty()) {
        return Some(value);
    }
    if let Some(value) = header("x-goog-api-key").filter(|s| !s.is_empty()) {
        return Some(value);
    }
    query_key.filter(|s| !s.is_empty()).map(str::to_string)
}

/// Load the access context for a request, or `None` when the request is
/// unrestricted (no key, unknown key, or an unrestricted key).
pub async fn key_access_for(state: &AppState, api_key: Option<&str>) -> Option<KeyAccess> {
    let api_key = api_key.filter(|k| !k.is_empty())?;
    let key = api_key.to_string();
    let record = state
        .read(move |conn| router_db::repos::api_keys::get_key_access_by_key(conn, &key))
        .await
        .ok()
        .flatten()?;
    let (id, name, access) = record;
    if access.get("restricted") != Some(&Value::Bool(true)) {
        return None;
    }
    let allow: Vec<String> = access
        .get("allow")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let combos = state
        .read(router_db::repos::combos::get_combos)
        .await
        .unwrap_or_default();
    let combo_names: std::collections::HashSet<String> = combos
        .iter()
        .filter_map(|c| c.get("name").and_then(Value::as_str))
        .map(|n| n.to_lowercase())
        .collect();

    let mut resolved_models = std::collections::HashSet::new();
    let mut resolved_providers = std::collections::HashSet::new();
    for entry in &allow {
        let lowered = entry.to_lowercase();
        if combo_names.contains(&lowered) {
            continue;
        }
        let info = super::v1::resolve_model_info(state, entry).await;
        if !info.provider.is_empty() && !info.model.is_empty() {
            resolved_models.insert(format!(
                "{}/{}",
                info.provider.to_lowercase(),
                info.model.to_lowercase()
            ));
        }
        // A bare name is a provider-as-model target (search/fetch).
        if !entry.contains('/') {
            resolved_providers
                .insert(router_sse::services::auth::resolve_provider_id(entry).to_lowercase());
        }
    }

    Some(KeyAccess {
        key_name: if name.is_empty() { id } else { name },
        combo_names,
        models: resolved_models,
        providers: resolved_providers,
    })
}

/// The 403 the client sees. It names only what the client itself sent.
fn denied(requested: &str) -> axum::response::Response {
    crate::routes::v1::json_err(
        403,
        &format!("This API key is not allowed to use model \"{requested}\""),
    )
}

impl KeyAccess {
    fn allows_combo(&self, requested: &str) -> bool {
        self.combo_names.contains(&requested.to_lowercase())
    }

    fn allows_resolved(&self, provider: &str, model: &str) -> bool {
        self.models.contains(&format!(
            "{}/{}",
            provider.to_lowercase(),
            model.to_lowercase()
        ))
    }

    fn allows_provider(&self, provider: &str) -> bool {
        self.providers.contains(&provider.to_lowercase())
    }
}

/// Gate for a target that may be a combo or a model (chat, systemone).
///
/// `combo_models` is routing's own combo resolution for `requested`, so a combo
/// name that exists but has no members routes nowhere and is not a free pass.
pub fn enforce(
    access: Option<&KeyAccess>,
    requested: &str,
    resolved: &router_sse::services::model::ModelInfo,
    is_combo: bool,
) -> Option<axum::response::Response> {
    let access = access?;
    if requested.is_empty() {
        return Some(denied(""));
    }
    let allowed = if is_combo {
        access.allows_combo(requested)
    } else {
        !resolved.provider.is_empty() && access.allows_resolved(&resolved.provider, &resolved.model)
    };
    if allowed {
        None
    } else {
        Some(denied(requested))
    }
}

/// Gate for a target the handler resolved itself (embeddings).
///
/// A missing provider or model is denied: an unverifiable target is not an
/// allowed one.
pub fn enforce_resolved(
    access: Option<&KeyAccess>,
    requested: &str,
    provider: &str,
    model: &str,
) -> Option<axum::response::Response> {
    let access = access?;
    let label = if requested.is_empty() {
        format!("{provider}/{model}")
    } else {
        requested.to_string()
    };
    if provider.is_empty() || model.is_empty() {
        return Some(denied(&label));
    }
    if access.allows_resolved(provider, model) {
        None
    } else {
        Some(denied(&label))
    }
}

/// Gate for search and fetch, where the provider IS the model.
pub fn enforce_provider(
    access: Option<&KeyAccess>,
    requested: &str,
    is_combo: bool,
) -> Option<axum::response::Response> {
    let access = access?;
    if requested.is_empty() {
        return Some(denied(""));
    }
    let allowed = if is_combo {
        access.allows_combo(requested)
    } else {
        access.allows_provider(&router_sse::services::auth::resolve_provider_id(requested))
    };
    if allowed {
        None
    } else {
        Some(denied(requested))
    }
}

/// `/v1/models`: a restricted key sees only the combos and models it may call.
pub fn filter_models_list(access: Option<&KeyAccess>, list: Vec<Value>) -> Vec<Value> {
    let Some(access) = access else {
        return list;
    };
    list.into_iter()
        .filter(|entry| {
            let Some(id) = entry.get("id").and_then(Value::as_str) else {
                return false;
            };
            if entry.get("owned_by").and_then(Value::as_str) == Some("combo") {
                return access.allows_combo(id);
            }
            if let Some(kind) = entry.get("kind").and_then(Value::as_str)
                && matches!(kind, "webSearch" | "webFetch")
            {
                return entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .is_some_and(|p| access.allows_provider(p));
            }
            // A listed model entry is `alias/model`; compare on the resolved
            // pair so an alias spelling matches its provider spelling.
            let Some((provider, model)) = id.split_once('/') else {
                return false;
            };
            access.allows_resolved(provider, model)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn key_extraction_follows_the_guard_order() {
        assert_eq!(
            extract_client_api_key(&headers(&[("authorization", "Bearer a")]), Some("q"))
                .as_deref(),
            Some("a")
        );
        assert_eq!(
            extract_client_api_key(&headers(&[("x-api-key", "b")]), Some("q")).as_deref(),
            Some("b")
        );
        assert_eq!(
            extract_client_api_key(&headers(&[("x-goog-api-key", "c")]), Some("q")).as_deref(),
            Some("c")
        );
        assert_eq!(
            extract_client_api_key(&HeaderMap::new(), Some("q")).as_deref(),
            Some("q")
        );
        assert_eq!(extract_client_api_key(&HeaderMap::new(), None), None);
        // A non-Bearer authorization header does not count.
        assert_eq!(
            extract_client_api_key(&headers(&[("authorization", "Basic x")]), None),
            None
        );
    }

    fn access(combos: &[&str], models: &[&str], providers: &[&str]) -> KeyAccess {
        KeyAccess {
            key_name: "k".into(),
            combo_names: combos.iter().map(|s| s.to_lowercase()).collect(),
            models: models.iter().map(|s| s.to_lowercase()).collect(),
            providers: providers.iter().map(|s| s.to_lowercase()).collect(),
        }
    }

    #[test]
    fn combo_and_model_matching_is_case_insensitive_and_exact() {
        let a = access(&["Main"], &["cx/gpt-5.6-terra"], &[]);
        assert!(a.allows_combo("MAIN"));
        assert!(!a.allows_combo("main2"));
        assert!(a.allows_resolved("CX", "GPT-5.6-TERRA"));
        assert!(!a.allows_resolved("cx", "gpt-5.6-sol"));
        // A raw string that only looks like a listed entry does not match a
        // different resolved target.
        assert!(!a.allows_resolved("openai", "MAIN"));
    }

    #[test]
    fn an_unrestricted_key_bypasses_every_gate() {
        let info = router_sse::services::model::ModelInfo {
            provider: "cx".into(),
            model: "gpt-5.6-terra".into(),
        };
        assert!(enforce(None, "anything", &info, false).is_none());
        assert!(enforce_resolved(None, "x", "p", "m").is_none());
        assert!(enforce_provider(None, "brave", false).is_none());
        let list = vec![serde_json::json!({"id": "cx/x"})];
        assert_eq!(filter_models_list(None, list.clone()), list);
    }

    #[test]
    fn a_missing_target_is_denied_for_a_restricted_key() {
        let a = access(&[], &[], &[]);
        let empty = router_sse::services::model::ModelInfo {
            provider: String::new(),
            model: String::new(),
        };
        assert!(enforce(Some(&a), "", &empty, false).is_some());
        assert!(enforce_resolved(Some(&a), "", "", "").is_some());
        assert!(enforce_provider(Some(&a), "", false).is_some());
    }

    #[test]
    fn models_list_is_narrowed_to_the_allow_list() {
        let a = access(&["Main"], &["cx/gpt-5.6-terra"], &["brave"]);
        let list = vec![
            serde_json::json!({"id": "Main", "owned_by": "combo"}),
            serde_json::json!({"id": "Other", "owned_by": "combo"}),
            serde_json::json!({"id": "cx/gpt-5.6-terra", "owned_by": "cx"}),
            serde_json::json!({"id": "cx/gpt-5.6-sol", "owned_by": "cx"}),
            serde_json::json!({"id": "brave", "owned_by": "brave", "kind": "webSearch"}),
            serde_json::json!({"id": "exa", "owned_by": "exa", "kind": "webSearch"}),
        ];
        let ids: Vec<String> = filter_models_list(Some(&a), list)
            .into_iter()
            .filter_map(|v| v.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        assert_eq!(ids, vec!["Main", "cx/gpt-5.6-terra", "brave"]);
    }
}
