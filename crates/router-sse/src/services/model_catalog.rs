//! The `/v1/models`, `/v1/models/{kind}`, `/v1/models/info` and
//! `/v1beta/models` catalog builders.
//!
//! These live in the engine rather than the route layer because they read the
//! registry, the DB and — for one provider — the network. The axum handlers
//! are thin: parse the query, call one of these, serialize.
//!
//! The only live resolver here is `grok-cli`.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use router_db::Db;

use crate::catalog::{
    aggregate_combo_capabilities, capabilities_from_service_kind, get_capabilities_for_model,
};
use crate::credentials::Credentials;
use crate::executors::http::{ProxyOptions, prepare_send};
use crate::providers::model::Model;
use crate::providers::registry::registry;
use crate::providers::ui::{is_anthropic_compatible_provider, is_openai_compatible_provider};
use crate::services::auth::credentials_from_connection;
use crate::services::connection_proxy::resolve_connection_proxy_config;
use crate::services::token_refresh::{refresh_provider_credentials, update_provider_credentials};

/// LLM kind sentinel — combos and models with no explicit kind default to LLM.
const LLM_KIND: &str = "llm";

/// Sent by `fetch_compatible_model_ids` so a
/// second 9router instance recognises the request as its own and skips its own
/// dynamic fetch, breaking the recursive loop between two instances pointed at
/// each other.
pub const INTERNAL_MODELS_FETCH_HEADER: &str = "x-9r-internal-models-fetch";

/// Map the per-model `type` field onto a service kind.
fn model_kind(model: &Model) -> &'static str {
    match model.kind() {
        "image" => "image",
        "tts" => "tts",
        "embedding" => "embedding",
        "stt" => "stt",
        "imageToText" => "imageToText",
        "video" => "video",
        _ => LLM_KIND,
    }
}

/// `kind || type || fallback`, with no kind mapping. Used for custom models
/// and `models/info`.
pub fn raw_model_kind(value: &Value, fallback: Option<&str>) -> Option<String> {
    value
        .get("kind")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback.map(str::to_string))
}

/// Guess a kind from the model id when the registry has no entry.
fn infer_kind_from_unknown_model_id(model_id: &str) -> &'static str {
    let lower = model_id.to_lowercase();
    if lower.contains("embed") {
        return "embedding";
    }
    if ["tts", "speech", "audio", "voice"]
        .iter()
        .any(|t| lower.contains(t))
    {
        return "tts";
    }
    if [
        "image",
        "imagen",
        "dall-",
        "dalle",
        "flux",
        "sdxl",
        "sd-",
        "stable-diffusion",
    ]
    .iter()
    .any(|t| lower.contains(t))
    {
        return "image";
    }
    LLM_KIND
}

/// Whether the provider's `serviceKinds` intersect the filter.
fn provider_matches_kinds(provider_id: &str, kind_filter: &[&str]) -> bool {
    let kinds: Vec<String> = registry()
        .get(provider_id)
        .and_then(|p| p.extra.get("serviceKinds"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .filter(|k: &Vec<String>| !k.is_empty())
        .unwrap_or_else(|| vec![LLM_KIND.to_string()]);
    kind_filter
        .iter()
        .any(|k| kinds.iter().any(|candidate| candidate == k))
}

/// Whether the combo's kind is in the filter.
fn combo_matches_kinds(combo: &Value, kind_filter: &[&str]) -> bool {
    let kind = combo
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(LLM_KIND);
    kind_filter.contains(&kind)
}

/// The provider's UI alias, else its alias, else the id.
fn get_provider_alias(provider_id: &str) -> String {
    registry()
        .get(provider_id)
        .and_then(|p| p.ui_alias_or_alias())
        .unwrap_or(provider_id)
        .to_string()
}

/// Map each built transport's alias to its provider id.
fn alias_to_provider_id() -> std::collections::HashMap<String, String> {
    registry()
        .transport_ids()
        .map(|id| (registry().alias_for(id).to_string(), id.to_string()))
        .collect()
}

/// The `{data|models|results}` array an OpenAI-style listing returns, or the
/// value itself when it is already an array.
fn parse_openai_style_models(data: &Value) -> Vec<Value> {
    if let Some(array) = data.as_array() {
        return array.clone();
    }
    for key in ["data", "models", "results"] {
        if let Some(array) = data.get(key).and_then(Value::as_array) {
            return array.clone();
        }
    }
    Vec::new()
}

/// Fetch model ids from an OpenAI- or Anthropic-compatible `/models` endpoint.
///
/// Needs `connection.apiKey` and a string `providerSpecificData.baseUrl`; any
/// other provider shape returns empty. Five-second timeout, and a plain fetch:
/// no connection proxy is applied on this path.
pub async fn fetch_compatible_model_ids(connection: &Value) -> Vec<String> {
    let Some(api_key) = connection
        .get("apiKey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let provider = connection
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let base_url = connection
        .get("providerSpecificData")
        .and_then(|p| p.get("baseUrl"))
        .and_then(Value::as_str)
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .unwrap_or_default();
    if base_url.is_empty() {
        return Vec::new();
    }

    let mut url = format!("{base_url}/models");
    let mut headers: Vec<(String, String)> =
        vec![("Content-Type".to_string(), "application/json".to_string())];

    if is_openai_compatible_provider(provider) {
        headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
    } else if is_anthropic_compatible_provider(provider) {
        if url.ends_with("/messages/models") {
            url.truncate(url.len() - 9);
        } else if url.ends_with("/messages") {
            let base = url[..url.len() - 9].to_string();
            url = format!("{base}/models");
        }
        headers.push(("x-api-key".to_string(), api_key.to_string()));
        headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
    } else {
        return Vec::new();
    }

    headers.push((INTERNAL_MODELS_FETCH_HEADER.to_string(), "1".to_string()));

    let Ok(target) = prepare_send(&url, &ProxyOptions::default()).await else {
        return Vec::new();
    };
    let mut request = target.client.get(&target.url);
    for (name, value) in headers.iter().chain(target.extra_headers.iter()) {
        request = request.header(name.as_str(), value.as_str());
    }
    let Ok(sent) =
        tokio::time::timeout(std::time::Duration::from_millis(5000), request.send()).await
    else {
        return Vec::new();
    };
    let Ok(response) = sent else {
        return Vec::new();
    };
    if !response.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = response.json::<Value>().await else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for model in parse_openai_style_models(&data) {
        let id = model
            .get("id")
            .or_else(|| model.get("name"))
            .or_else(|| model.get("model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(id) = id
            && seen.insert(id.to_string())
        {
            out.push(id.to_string());
        }
    }
    out
}

/// One live-catalog model, the `{ id, name? }` shape the resolvers return.
///
/// `raw` is the exact object `GET /api/providers/{id}/models` serializes for
/// this model — grok-cli spreads the upstream entry, so its extra keys
/// (`contextLength`, `upstreamModelId`, …) have to survive verbatim.
pub struct LiveModel {
    pub id: String,
    pub raw: Value,
    pub kind: &'static str,
    pub capabilities: Option<Value>,
}

/// Dedupe by id, taking the id from the first present of
/// `id | model_id | modelId | model | slug | key | name`.
fn parse_grok_cli_models(data: &Value) -> Vec<LiveModel> {
    const GROK_CLI_MODEL: &str = "grok-build";

    let entries: Vec<Value> = if let Some(array) = data.as_array() {
        array.clone()
    } else {
        for key in ["data", "models", "results"] {
            if let Some(array) = data.get(key).and_then(Value::as_array) {
                return array
                    .iter()
                    .filter_map(|raw| grok_cli_entry(None, raw, GROK_CLI_MODEL))
                    .collect();
            }
        }
        if let Some(object) = data.as_object() {
            return object
                .iter()
                .filter_map(|(key, raw)| grok_cli_entry(Some(key.as_str()), raw, GROK_CLI_MODEL))
                .collect();
        }
        return Vec::new();
    };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in &entries {
        if let Some(model) = grok_cli_entry(None, raw, GROK_CLI_MODEL)
            && seen.insert(model.id.clone())
        {
            out.push(model);
        }
    }
    out
}

/// One parsed model entry. `key` is the object key when the payload is a map.
///
/// `raw` keeps the `{ ...item, id, name }` spread — key order included, since
/// `GET /api/providers/{id}/models` serializes it verbatim.
fn grok_cli_entry(key: Option<&str>, raw: &Value, default_model: &str) -> Option<LiveModel> {
    let item = if let Some(s) = raw.as_str() {
        json!({ "id": s })
    } else if raw.is_object() {
        raw.clone()
    } else {
        return None;
    };
    let first_str = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|k| item.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    };
    let id = first_str(&["id", "model_id", "modelId", "model", "slug"])
        .or_else(|| key.map(str::to_string))
        .or_else(|| first_str(&["name"]))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;

    let name = first_str(&["display_name", "displayName", "name"]).unwrap_or_else(|| id.clone());
    let mut model = item.as_object().cloned().unwrap_or_default();
    model.insert("id".into(), json!(id));
    model.insert("name".into(), json!(name));

    // Coerce to a number, then require it to be finite and positive.
    let numeric = |keys: &[&str]| -> Option<f64> {
        keys.iter().find_map(|k| match item.get(*k) {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            _ => None,
        })
    };
    let context_length = numeric(&[
        "context_length",
        "contextLength",
        "context_window",
        "contextWindow",
    ])
    .filter(|n| n.is_finite() && *n > 0.0);
    let max_output_tokens =
        numeric(&["max_output_tokens", "maxOutputTokens"]).filter(|n| n.is_finite() && *n > 0.0);
    if let Some(n) = context_length {
        model.insert("contextLength".into(), json!(n as i64));
    }
    if let Some(n) = max_output_tokens {
        model.insert("maxOutputTokens".into(), json!(n as i64));
    }
    if id == default_model {
        // The default model gets a fallback context window when none is set.
        if model
            .get("contextLength")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
        {
            model.insert("contextLength".into(), json!(500_000));
        }
        if model
            .get("maxOutputTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            == 0
        {
            model.insert("maxOutputTokens".into(), json!(64_000));
        }
    }

    Some(LiveModel {
        kind: LLM_KIND,
        capabilities: None,
        id,
        raw: Value::Object(model),
    })
}

/// Fetch grok-cli's live model list, refreshing the token once on 401/403.
async fn resolve_grok_cli_models(
    db: &Db,
    connection: &Value,
    proxy_options: &ProxyOptions,
) -> Option<Vec<LiveModel>> {
    const MODELS_URL: &str = "https://cli-chat-proxy.grok.com/v1/models";

    let mut credentials = credentials_from_connection(connection);
    let connection_id = connection.get("id").and_then(Value::as_str).unwrap_or("");
    let mut access_token = credentials.access_token.clone()?;

    async fn send(
        token: &str,
        credentials: &Credentials,
        proxy_options: &ProxyOptions,
    ) -> Option<reqwest::Response> {
        let headers = grok_cli_model_headers(token, credentials);
        let target = prepare_send(MODELS_URL, proxy_options).await.ok()?;
        let mut request = target.client.get(&target.url);
        for (name, value) in headers.iter().chain(target.extra_headers.iter()) {
            request = request.header(name.as_str(), value.as_str());
        }
        request.send().await.ok()
    }

    let mut response = send(&access_token, &credentials, proxy_options).await?;
    if matches!(response.status().as_u16(), 401 | 403)
        && credentials.refresh_token.is_some()
        && let Some(refreshed) =
            refresh_provider_credentials("grok-cli", &credentials, proxy_options).await
        && let Some(new_token) = refreshed.get("accessToken").and_then(Value::as_str)
    {
        access_token = new_token.to_string();
        if !connection_id.is_empty() {
            let mut patch = refreshed.clone();
            if let Some(obj) = patch.as_object_mut() {
                obj.insert(
                    "existingProviderSpecificData".into(),
                    Value::Object(credentials.provider_specific_data.clone()),
                );
            }
            update_provider_credentials(db, connection_id, &patch);
        }
        credentials.access_token = Some(access_token.clone());
        response = send(&access_token, &credentials, proxy_options).await?;
    }

    if !response.status().is_success() {
        return None;
    }
    let data = response.json::<Value>().await.ok()?;
    let models = parse_grok_cli_models(&data);
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

/// The headers grok-cli's models endpoint expects.
fn grok_cli_model_headers(token: &str, credentials: &Credentials) -> Vec<(String, String)> {
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("Accept".to_string(), "application/json".to_string()),
        (
            "User-Agent".to_string(),
            "grok-shell/0.2.99 (linux; x86_64)".to_string(),
        ),
        ("x-xai-token-auth".to_string(), "xai-grok-cli".to_string()),
        (
            "x-grok-client-version".to_string(),
            crate::executors::grok_cli::GROK_CLI_VERSION.to_string(),
        ),
        (
            "x-grok-client-identifier".to_string(),
            crate::executors::grok_cli::GROK_CLI_CLIENT_IDENTIFIER.to_string(),
        ),
        ("x-grok-client-mode".to_string(), "headless".to_string()),
    ];
    if let Some(email) = credentials.psd_str("email") {
        headers.push(("x-email".to_string(), email.to_string()));
    }
    if let Some(user_id) = credentials
        .psd_str("userId")
        .or_else(|| credentials.psd_str("principalId"))
    {
        headers.push(("x-userid".to_string(), user_id.to_string()));
    }
    headers
}

/// Resolve a provider's live model list over the network. Only grok-cli is
/// wired up; every other provider returns `None`.
pub async fn resolve_live_models(
    db: &Db,
    provider_id: &str,
    connection: &Value,
) -> Option<Vec<LiveModel>> {
    match provider_id {
        "grok-cli" => {
            let psd = connection
                .get("providerSpecificData")
                .and_then(Value::as_object)
                .cloned();
            let resolved = resolve_connection_proxy_config(db, psd.as_ref());
            let proxy_options = ProxyOptions {
                enabled: resolved.connection_proxy_enabled,
                url: Some(resolved.connection_proxy_url).filter(|s| !s.is_empty()),
                no_proxy: Some(resolved.connection_no_proxy).filter(|s| !s.is_empty()),
                strict_proxy: resolved.strict_proxy,
                vercel_relay_url: Some(resolved.vercel_relay_url).filter(|s| !s.is_empty()),
            };
            resolve_grok_cli_models(db, connection, &proxy_options).await
        }
        _ => None,
    }
}

/// Build the catalog: combos first, then each active connection's models, or
/// the static registry plus custom models when there are no connections.
pub async fn build_models_list(
    db: &Db,
    kind_filter: &[&str],
    skip_dynamic_fetch: bool,
) -> Vec<Value> {
    let mut connections: Vec<Value> = db
        .with_conn(|conn| router_db::repos::connections::get_provider_connections(conn, None, None))
        .unwrap_or_default();
    connections.retain(|c| c.get("isActive") != Some(&Value::Bool(false)));

    let combos: Vec<Value> = db
        .with_conn(router_db::repos::combos::get_combos)
        .unwrap_or_default();
    let custom_models: Vec<Value> = db
        .with_conn(router_db::repos::aliases::get_custom_models)
        .unwrap_or_default();
    let model_aliases: Value = db
        .with_conn(router_db::repos::aliases::get_model_aliases)
        .unwrap_or(Value::Null);
    let disabled_by_alias: Value = db
        .with_conn(router_db::repos::aliases::get_disabled_models)
        .unwrap_or(Value::Null);

    let is_disabled = |alias: &str, model_id: &str| -> bool {
        disabled_by_alias
            .get(alias)
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|v| v.as_str() == Some(model_id)))
    };

    let mut active_by_provider: Vec<(String, Value)> = Vec::new();
    for conn in &connections {
        let Some(provider) = conn.get("provider").and_then(Value::as_str) else {
            continue;
        };
        if !active_by_provider.iter().any(|(p, _)| p == provider) {
            active_by_provider.push((provider.to_string(), conn.clone()));
        }
    }

    let mut models: Vec<Value> = Vec::new();

    // Combos first (filtered by kind). Web combos expose `kind` so the caller can
    // tell search from fetch.
    let combo_by_name: Map<String, Value> = combos
        .iter()
        .filter_map(|c| {
            let name = c.get("name").and_then(Value::as_str)?;
            Some((
                name.to_string(),
                c.get("models").cloned().unwrap_or(json!([])),
            ))
        })
        .collect();
    let combo_lookup = |name: &str| -> Option<Vec<String>> {
        combo_by_name.get(name).and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    };

    for combo in &combos {
        if !combo_matches_kinds(combo, kind_filter) {
            continue;
        }
        let mut entry = Map::new();
        entry.insert(
            "id".into(),
            combo.get("name").cloned().unwrap_or(Value::Null),
        );
        entry.insert("object".into(), json!("model"));
        entry.insert("owned_by".into(), json!("combo"));
        let kind = combo.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind == "webSearch" || kind == "webFetch" {
            entry.insert("kind".into(), json!(kind));
        } else {
            let combo_models: Vec<String> = combo
                .get("models")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(caps) = aggregate_combo_capabilities(&combo_models, combo_lookup) {
                entry.insert("capabilities".into(), caps);
            }
        }
        models.push(Value::Object(entry));
    }

    if connections.is_empty() {
        // DB has no connections: the static catalog, filtered by per-model kind.
        let alias_to_provider = alias_to_provider_id();
        for (alias, provider_models) in registry().models_iter() {
            let provider_id = alias_to_provider
                .get(alias)
                .map(String::as_str)
                .unwrap_or(alias);
            if !provider_matches_kinds(provider_id, kind_filter) {
                continue;
            }
            for model in provider_models {
                if !kind_filter.contains(&model_kind(model)) || is_disabled(alias, &model.id) {
                    continue;
                }
                models.push(json!({
                    "id": format!("{alias}/{}", model.id),
                    "object": "model",
                    "owned_by": alias,
                    "capabilities": get_capabilities_for_model(Some(alias), &model.id),
                }));
            }
        }

        for custom in &custom_models {
            let Some(id) = custom.get("id").and_then(Value::as_str) else {
                continue;
            };
            if custom
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t != "llm")
            {
                continue;
            }
            if !kind_filter.contains(&LLM_KIND) {
                continue;
            }
            let Some(provider_alias) = custom.get("providerAlias").and_then(Value::as_str) else {
                continue;
            };
            let model_id = id.trim();
            if model_id.is_empty() {
                continue;
            }
            models.push(json!({
                "id": format!("{provider_alias}/{model_id}"),
                "object": "model",
                "owned_by": provider_alias,
            }));
        }
    } else {
        for (provider_id, conn) in &active_by_provider {
            if !provider_matches_kinds(provider_id, kind_filter) {
                continue;
            }
            let static_alias = registry().alias_for(provider_id).to_string();
            let output_alias = conn
                .get("providerSpecificData")
                .and_then(|p| p.get("prefix"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| get_provider_alias(provider_id))
                .trim()
                .to_string();
            let provider_models = registry().models_for(&static_alias);
            let enabled_models = conn
                .get("providerSpecificData")
                .and_then(|p| p.get("enabledModels"))
                .and_then(Value::as_array)
                .cloned();
            let has_explicit_enabled_models =
                enabled_models.as_ref().is_some_and(|list| !list.is_empty());
            let is_compatible = is_openai_compatible_provider(provider_id)
                || is_anthropic_compatible_provider(provider_id);

            let static_kind_by_id: std::collections::HashMap<String, &'static str> =
                provider_models
                    .iter()
                    .map(|m| (m.id.clone(), model_kind(m)))
                    .collect();
            let mut live_kind_by_id: std::collections::HashMap<String, &'static str> =
                std::collections::HashMap::new();
            let mut live_caps_by_id: std::collections::HashMap<String, Value> =
                std::collections::HashMap::new();

            let mut raw_model_ids: Vec<String> = if has_explicit_enabled_models {
                let mut seen = HashSet::new();
                enabled_models
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|v| v.as_str().map(str::trim).map(str::to_string))
                    .filter(|s| !s.is_empty())
                    .filter(|s| seen.insert(s.clone()))
                    .collect()
            } else {
                provider_models.iter().map(|m| m.id.clone()).collect()
            };

            if is_compatible && raw_model_ids.is_empty() && !skip_dynamic_fetch {
                raw_model_ids = fetch_compatible_model_ids(conn).await;
            }

            if !has_explicit_enabled_models
                && provider_id == "grok-cli"
                && let Some(live) = resolve_live_models(db, provider_id, conn).await
                && !live.is_empty()
            {
                raw_model_ids = live.iter().map(|m| m.id.clone()).collect();
                for m in &live {
                    live_kind_by_id.insert(m.id.clone(), m.kind);
                    if let Some(caps) = &m.capabilities {
                        live_caps_by_id.insert(m.id.clone(), caps.clone());
                    }
                }
            }

            let strip_prefix = |model_id: &str| -> String {
                for prefix in [&output_alias, &static_alias, provider_id] {
                    let with_slash = format!("{prefix}/");
                    if model_id.starts_with(&with_slash) {
                        return model_id[with_slash.len()..].to_string();
                    }
                }
                model_id.to_string()
            };
            let model_ids: Vec<String> = raw_model_ids
                .iter()
                .map(|id| strip_prefix(id))
                .filter(|id| !id.trim().is_empty())
                .collect();

            let mut custom_kind_by_id: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let custom_model_ids: Vec<String> = custom_models
                .iter()
                .filter(|m| {
                    let Some(_) = m.get("id").and_then(Value::as_str) else {
                        return false;
                    };
                    let kind =
                        raw_model_kind(m, Some(LLM_KIND)).unwrap_or_else(|| LLM_KIND.to_string());
                    let kind_allowed = kind_filter.contains(&kind.as_str())
                        || (kind == "imageToText" && kind_filter.contains(&LLM_KIND));
                    if !kind_allowed {
                        return false;
                    }
                    let alias = m.get("providerAlias").and_then(Value::as_str).unwrap_or("");
                    alias == static_alias || alias == output_alias || alias == provider_id
                })
                .map(|m| {
                    let id = m
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    if !id.is_empty() {
                        let kind = raw_model_kind(m, Some(LLM_KIND))
                            .unwrap_or_else(|| LLM_KIND.to_string());
                        custom_kind_by_id.insert(id.clone(), kind);
                    }
                    id
                })
                .filter(|id| !id.is_empty())
                .collect();

            let alias_model_ids: Vec<String> = model_aliases
                .as_object()
                .map(|m| m.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .filter(|full| {
                    full.contains('/')
                        && (full.starts_with(&format!("{output_alias}/"))
                            || full.starts_with(&format!("{static_alias}/"))
                            || full.starts_with(&format!("{provider_id}/")))
                })
                .map(|full| strip_prefix(&full))
                .filter(|id| !id.trim().is_empty())
                .collect();

            let mut merged: Vec<String> = Vec::new();
            let mut seen_ids = HashSet::new();
            for id in model_ids
                .into_iter()
                .chain(custom_model_ids)
                .chain(alias_model_ids)
            {
                if seen_ids.insert(id.clone()) {
                    merged.push(id);
                }
            }

            for model_id in merged {
                let custom_kind = custom_kind_by_id.get(&model_id).map(String::as_str);
                let live_kind = live_kind_by_id.get(&model_id).copied();
                let static_kind = static_kind_by_id.get(&model_id).copied();
                let kind: &str = custom_kind
                    .or(live_kind)
                    .or(static_kind)
                    .unwrap_or_else(|| infer_kind_from_unknown_model_id(&model_id));
                let allow_as_llm = kind == "imageToText" && kind_filter.contains(&LLM_KIND);
                if !kind_filter.contains(&kind) && !allow_as_llm {
                    continue;
                }
                if is_disabled(&output_alias, &model_id) || is_disabled(&static_alias, &model_id) {
                    continue;
                }

                let mut model = Map::new();
                model.insert("id".into(), json!(format!("{output_alias}/{model_id}")));
                model.insert("object".into(), json!("model"));
                model.insert("owned_by".into(), json!(output_alias));

                let live_caps = live_caps_by_id.get(&model_id).cloned();
                let service_caps =
                    capabilities_from_service_kind(custom_kind.or(live_kind).unwrap_or(""));
                let mut caps = live_caps.or(service_caps).or_else(|| {
                    (kind == LLM_KIND)
                        .then(|| json!(get_capabilities_for_model(Some(provider_id), &model_id)))
                });
                if let Some(caps_value) = caps.take() {
                    model.insert("capabilities".into(), caps_value.clone());
                    if kind == LLM_KIND || allow_as_llm {
                        let mut context_window =
                            caps_value.get("contextWindow").and_then(Value::as_f64);
                        let mut max_output = caps_value.get("maxOutput").and_then(Value::as_f64);
                        if context_window.is_none() || max_output.is_none() {
                            let fallback = get_capabilities_for_model(Some(provider_id), &model_id);
                            if context_window.is_none() {
                                context_window = Some(fallback.context_window as f64);
                            }
                            if max_output.is_none() {
                                max_output = Some(fallback.max_output as f64);
                            }
                        }
                        if let Some(v) = context_window {
                            model.insert("context_length".into(), json!(v));
                        }
                        if let Some(v) = max_output {
                            model.insert("max_completion_tokens".into(), json!(v));
                        }
                    }
                }
                models.push(Value::Object(model));
            }

            // Web search/fetch — the provider IS the model, so expose
            // `{alias}/search` and `{alias}/fetch` as explicit-kind entries.
            if kind_filter.contains(&"webSearch")
                && crate::modalities::provider_config(provider_id, "searchConfig").is_some()
            {
                models.push(json!({
                    "id": format!("{output_alias}/search"),
                    "object": "model",
                    "kind": "webSearch",
                    "owned_by": output_alias,
                }));
            }
            if kind_filter.contains(&"webFetch")
                && crate::modalities::provider_config(provider_id, "fetchConfig").is_some()
            {
                models.push(json!({
                    "id": format!("{output_alias}/fetch"),
                    "object": "model",
                    "kind": "webFetch",
                    "owned_by": output_alias,
                }));
            }
        }
    }

    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for model in models {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        deduped.push(model);
    }
    deduped
}

/// Map a catalog kind slug to the service kinds it covers.
pub fn kind_slug_map(slug: &str) -> Option<&'static [&'static str]> {
    Some(match slug {
        "image" => &["image"],
        "tts" => &["tts"],
        "stt" => &["stt"],
        "embedding" => &["embedding"],
        "image-to-text" => &["imageToText"],
        "web" => &["webSearch", "webFetch"],
        _ => return None,
    })
}

/// The endpoint each model kind is served from.
fn kind_endpoint(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "llm" => "/v1/chat/completions",
        "image" => "/v1/images/generations",
        "tts" => "/v1/audio/speech",
        "stt" => "/v1/audio/transcriptions",
        "embedding" => "/v1/embeddings",
        "imageToText" => "/v1/chat/completions",
        "webSearch" => "/v1/search",
        "webFetch" => "/v1/fetch",
        _ => return None,
    })
}

/// Resolve a `provider/model` id against the registry, optionally pinned to a
/// requested kind.
pub fn model_info_lookup(full_id: &str, requested_kind: Option<&str>) -> Option<Value> {
    let (alias, model_id) = full_id.split_once('/')?;
    let provider_id = crate::providers::ui::alias_maps()
        .0
        .get(alias)
        .and_then(Value::as_str)
        .unwrap_or(alias)
        .to_string();
    let provider_name = registry()
        .get(&provider_id)
        .and_then(|p| p.display.as_ref())
        .and_then(|d| d.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&provider_id)
        .to_string();

    let list = {
        let by_alias = registry().models_for(alias);
        if by_alias.is_empty() && alias != provider_id {
            registry().models_for(&provider_id)
        } else {
            by_alias
        }
    };

    let found = list
        .iter()
        .find(|m| m.id == model_id && requested_kind.is_none_or(|k| m.kind() == k));
    if let Some(m) = found {
        let kind = m.kind();
        let mut out = Map::new();
        out.insert("id".into(), json!(format!("{alias}/{}", m.id)));
        out.insert(
            "name".into(),
            json!(m.name.clone().unwrap_or_else(|| m.id.clone())),
        );
        out.insert("kind".into(), json!(kind));
        out.insert("owned_by".into(), json!(alias));
        out.insert(
            "endpoint".into(),
            kind_endpoint(kind).map_or(Value::Null, |e| json!(e)),
        );
        if let Some(params) = &m.params {
            out.insert("params".into(), params.clone());
        }
        if let Some(caps) = &m.capabilities {
            out.insert("capabilities".into(), caps.clone());
        }
        if let Some(options) = m.extra.get("options") {
            out.insert("options".into(), options.clone());
        }
        if let Some(dimensions) = &m.dimensions {
            out.insert("dimensions".into(), dimensions.clone());
        }
        if let Some(window) = m.extra.get("contextWindow") {
            out.insert("contextWindow".into(), window.clone());
        }
        return Some(Value::Object(out));
    }

    // Web search/fetch — the virtual `search` / `fetch` ids.
    let search_config = crate::modalities::provider_config(&provider_id, "searchConfig");
    if model_id == "search" && search_config.is_some() {
        let mut out = Map::new();
        out.insert("id".into(), json!(format!("{alias}/search")));
        out.insert("name".into(), json!(format!("{provider_name} Search")));
        out.insert("kind".into(), json!("webSearch"));
        out.insert("owned_by".into(), json!(alias));
        out.insert("endpoint".into(), json!("/v1/search"));
        out.insert(
            "params".into(),
            json!([
                "query",
                "max_results",
                "country",
                "language",
                "time_range",
                "domain_filter",
                "search_type"
            ]),
        );
        if let Some(cfg) = search_config {
            if let Some(v) = cfg.get("searchTypes") {
                out.insert("searchTypes".into(), v.clone());
            }
            if let Some(v) = cfg.get("maxMaxResults") {
                out.insert("maxResults".into(), v.clone());
            }
            if let Some(v) = cfg.get("requiredOptions") {
                out.insert("required".into(), v.clone());
            }
        }
        return Some(Value::Object(out));
    }
    if model_id == "fetch"
        && crate::modalities::provider_config(&provider_id, "fetchConfig").is_some()
    {
        let mut out = Map::new();
        out.insert("id".into(), json!(format!("{alias}/fetch")));
        out.insert("name".into(), json!(format!("{provider_name} Fetch")));
        out.insert("kind".into(), json!("webFetch"));
        out.insert("owned_by".into(), json!(alias));
        out.insert("endpoint".into(), json!("/v1/fetch"));
        out.insert("params".into(), json!(["url", "format", "max_characters"]));
        return Some(Value::Object(out));
    }
    None
}

/// `GET /v1beta/models`: the Gemini-format catalog. Bare `models/{id}` rows for
/// a `gemini` provider are not emitted — no `gemini` provider is registered.
pub fn gemini_models_list() -> Value {
    let mut models: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for (provider, provider_models) in registry().models_iter() {
        for model in provider_models {
            let name = format!("models/{provider}/{}", model.id);
            if !seen.insert(name.clone()) {
                continue;
            }
            let display_name = model.name.clone().unwrap_or_else(|| model.id.clone());
            models.push(json!({
                "name": name,
                "displayName": display_name,
                "description": format!("{provider} model: {display_name}"),
                "supportedGenerationMethods": ["generateContent"],
                "inputTokenLimit": 128000,
                "outputTokenLimit": 8192,
            }));
        }
    }
    json!({ "models": models })
}

/// A character-count estimate of the request's input size.
pub fn estimate_anthropic_input_tokens(body: &Value) -> i64 {
    fn count_value_chars(value: &Value) -> i64 {
        match value {
            Value::Null => 0,
            Value::String(s) => s.chars().count() as i64,
            Value::Number(n) => n.to_string().chars().count() as i64,
            Value::Bool(b) => b.to_string().chars().count() as i64,
            Value::Array(items) => items.iter().map(count_value_chars).sum(),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| k.chars().count() as i64 + count_value_chars(v))
                .sum(),
        }
    }

    fn count_content_block_chars(block: &Value) -> i64 {
        match block {
            Value::Null => 0,
            Value::String(s) => s.chars().count() as i64,
            Value::Object(map) => match map.get("type").and_then(Value::as_str) {
                Some("text") => count_value_chars(map.get("text").unwrap_or(&Value::Null)),
                Some("tool_use") => {
                    count_value_chars(map.get("name").unwrap_or(&Value::Null))
                        + count_value_chars(map.get("input").unwrap_or(&Value::Null))
                }
                Some("tool_result") => {
                    count_value_chars(map.get("content").unwrap_or(&Value::Null))
                }
                Some("thinking") => count_value_chars(map.get("thinking").unwrap_or(&Value::Null)),
                _ => count_value_chars(block),
            },
            _ => count_value_chars(block),
        }
    }

    fn count_message_chars(message: &Value) -> i64 {
        let Some(map) = message.as_object() else {
            return 0;
        };
        match map.get("content") {
            Some(Value::String(s)) => s.chars().count() as i64,
            Some(Value::Array(blocks)) => blocks.iter().map(count_content_block_chars).sum(),
            Some(other) => count_value_chars(other),
            None => 0,
        }
    }

    let mut total_chars = count_value_chars(body.get("system").unwrap_or(&Value::Null))
        + count_value_chars(body.get("tools").unwrap_or(&Value::Null));
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for msg in messages {
            total_chars += count_message_chars(msg);
        }
    }
    // Ceiling division by 4, integer-only.
    (total_chars + 3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_map_matches_the_expected_table() {
        let model = |v: Value| -> Model { serde_json::from_value(v).unwrap() };
        assert_eq!(model_kind(&model(json!({"id": "x"}))), "llm");
        assert_eq!(
            model_kind(&model(json!({"id": "x", "type": "embedding"}))),
            "embedding"
        );
        assert_eq!(
            model_kind(&model(json!({"id": "x", "type": "imageToText"}))),
            "imageToText"
        );
        // No kind mapping for `systemone`, so it falls through to llm.
        assert_eq!(
            model_kind(&model(json!({"id": "x", "type": "systemone"}))),
            "llm"
        );
    }

    #[test]
    fn unknown_model_ids_infer_a_kind() {
        assert_eq!(
            infer_kind_from_unknown_model_id("text-embedding-3"),
            "embedding"
        );
        assert_eq!(infer_kind_from_unknown_model_id("gpt-4o-mini-tts"), "tts");
        assert_eq!(infer_kind_from_unknown_model_id("dall-e-3"), "image");
        assert_eq!(infer_kind_from_unknown_model_id("gpt-4o"), "llm");
    }

    #[test]
    fn count_tokens_matches_the_expected_estimator() {
        // system "abcd" (4) + one user message "abcd" (4) = 8 chars / 4 = 2.
        let body = json!({"system": "abcd", "messages": [{"role": "user", "content": "abcd"}]});
        assert_eq!(estimate_anthropic_input_tokens(&body), 2);

        // Object keys count: {"a":"bb"} is 1 + 2 = 3 chars.
        let body = json!({"messages": [{"role": "user", "content": {"a": "bb"}}]});
        assert_eq!(estimate_anthropic_input_tokens(&body), 1);

        // tool_use counts name + input.
        let body = json!({"messages": [{"role": "user", "content": [
            {"type": "tool_use", "name": "abcd", "input": {"x": "abcd"}}
        ]}]});
        // name 4 + input key 1 + value 4 = 9 chars → ceil(9/4) = 3.
        assert_eq!(estimate_anthropic_input_tokens(&body), 3);
    }

    #[test]
    fn kind_slug_map_covers_the_kept_kinds() {
        assert_eq!(kind_slug_map("embedding"), Some(&["embedding"][..]));
        assert_eq!(kind_slug_map("web"), Some(&["webSearch", "webFetch"][..]));
        assert_eq!(kind_slug_map("image"), Some(&["image"][..]));
        assert_eq!(kind_slug_map("nope"), None);
    }
}
