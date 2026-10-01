//! `providers/*`: the nine kept dashboard connection routes.
//!
//! Two shapes drive the code:
//!
//! * **The connection row is the payload.** `row_to_conn` hands back the stored
//!   JSON blob, so these handlers build their responses by cloning that map and
//!   deleting keys — removing `apiKey` drops the key, it does not null it, and
//!   the frontend tells the two apart.
//! * **Probes go out over the connection's own proxy.** Every probe routes
//!   through the connection proxy, which is the same relay → proxy → direct
//!   resolution `prepare_send` performs.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use regex::Regex;
use serde_json::{Map, Value, json};

use router_sse::executors::http::{ProxyOptions, prepare_send, tls_builder};
use router_sse::executors::oauth::{
    merge_refreshed_credentials, should_refresh_credentials, to_expires_at, to_iso,
};
use router_sse::modalities::ssrf;
use router_sse::providers::normalize::{normalize_provider_id, normalize_provider_specific_data};
use router_sse::providers::registry::{raw_models_for, registry};
use router_sse::providers::ui::{
    is_anthropic_compatible_provider, is_custom_embedding_provider, is_openai_compatible_provider,
};
use router_sse::services::auth::credentials_from_connection;
use router_sse::services::connection_proxy::{
    ResolvedProxyConfig, resolve_connection_proxy_config,
};
use router_sse::services::model_catalog::resolve_live_models;
use router_sse::services::oauth_flow::extract_codex_account_info;
use router_sse::services::proxy_test::test_proxy_url;
use router_sse::services::token_refresh::{refresh_token_by_provider, update_provider_credentials};
use router_sse::translator::concerns::primitives::{js_string, js_truthy};

use crate::error::ApiError;
use crate::routes::models_test::internal_headers;
use crate::routes::provider_nodes::probe_client;
use crate::state::AppState;

/// The order `USAGE_SUPPORTED_PROVIDERS.indexOf(provider)` sorts by.
fn usage_supported() -> Vec<&'static str> {
    registry().usage_supported()
}

fn usage_apikey() -> Vec<&'static str> {
    registry().usage_apikey()
}

fn category_of(id: &str) -> Option<&'static str> {
    registry().get(id).and_then(|p| p.category.as_deref())
}

/// `AI_PROVIDERS[provider]?.name`.
fn display_name(id: &str) -> Option<&'static str> {
    registry()
        .get(id)
        .and_then(|p| p.display.as_ref())
        .and_then(|d| d.get("name"))
        .and_then(Value::as_str)
}

/// `AI_PROVIDERS[provider]?.noAuth === true`.
fn provider_no_auth(id: &str) -> bool {
    registry().get(id).and_then(|p| p.no_auth) == Some(true)
}

/// `getDefaultModel(aliasOrId)`: the first static model id, or `None`.
fn default_model(alias_or_id: &str) -> Option<String> {
    registry()
        .models_for(alias_or_id)
        .first()
        .map(|m| m.id.clone())
}

fn conn_str<'a>(c: &'a Value, key: &str) -> Option<&'a str> {
    c.get(key).and_then(Value::as_str)
}

/// `delete result.apiKey` — remove, never null.
///
/// `idToken` (and the other credentials) can also live inside
/// `providerSpecificData` — that is where the grok-cli bulk import stores it —
/// so the nested object is stripped too. `hide_secrets` is what the get,
/// create and update routes return, and a top-level-only strip would leak the
/// nested token there.
fn strip_secrets(map: &mut Map<String, Value>) {
    for key in SECRET_FIELDS {
        map.shift_remove(key);
    }
    if let Some(psd) = map
        .get_mut("providerSpecificData")
        .and_then(Value::as_object_mut)
    {
        for key in SECRET_FIELDS {
            psd.shift_remove(key);
        }
    }
}

/// The credential keys removed from every response.
const SECRET_FIELDS: [&str; 4] = ["apiKey", "accessToken", "refreshToken", "idToken"];

/// `{...c, apiKey: undefined, accessToken: undefined, refreshToken: undefined,
/// idToken: undefined}`.
fn hide_secrets(connection: &Value) -> Map<String, Value> {
    let mut map = connection.as_object().cloned().unwrap_or_default();
    strip_secrets(&mut map);
    map
}

// ─── proxy config normalization ───────────────────────────────────────────

struct ProxyConfigValues {
    enabled: bool,
    url: String,
    no_proxy: String,
}

/// `normalizeProxyConfig(body)` for create: always returns the three fields,
/// erroring only when enabled without a URL.
fn normalize_proxy_config(body: &Map<String, Value>) -> Result<ProxyConfigValues, String> {
    let enabled = body.get("connectionProxyEnabled") == Some(&Value::Bool(true));
    let url = body
        .get("connectionProxyUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let no_proxy = body
        .get("connectionNoProxy")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if enabled && url.is_empty() {
        return Err("Connection proxy URL is required when connection proxy is enabled".into());
    }
    Ok(ProxyConfigValues {
        enabled,
        url,
        no_proxy,
    })
}

/// `normalizeProxyConfig(body)` for update: `hasAnyProxyField` gates the
/// merge, so a body without any of the three keys leaves the PSD alone.
fn normalize_proxy_config_update(
    body: &Map<String, Value>,
) -> Result<(bool, ProxyConfigValues), String> {
    let has_any = [
        "connectionProxyEnabled",
        "connectionProxyUrl",
        "connectionNoProxy",
    ]
    .iter()
    .any(|k| body.contains_key(*k));
    if !has_any {
        return Ok((
            false,
            ProxyConfigValues {
                enabled: false,
                url: String::new(),
                no_proxy: String::new(),
            },
        ));
    }
    let values = normalize_proxy_config(body)?;
    Ok((true, values))
}

/// `normalizeProxyPoolId(proxyPoolId)` for POST.
fn normalize_proxy_pool_id(
    state: &AppState,
    raw: Option<&Value>,
) -> Result<Option<String>, String> {
    let normalized = match raw {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) if s.is_empty() || s == "__none__" => return Ok(None),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(other) => {
            let s = js_string(other);
            let s = s.trim().to_string();
            if s.is_empty() || s == "__none__" {
                return Ok(None);
            }
            s
        }
    };
    if normalized.is_empty() {
        return Ok(None);
    }
    if lookup_proxy_pool(state, &normalized) {
        Ok(Some(normalized))
    } else {
        Err("Proxy pool not found".into())
    }
}

/// `normalizeProxyPoolUpdate(proxyPoolIdInput)` for PUT: `undefined` means "no
/// field", the empty forms mean "clear it".
fn normalize_proxy_pool_update(
    state: &AppState,
    raw: Option<&Value>,
) -> Result<(bool, Option<String>), String> {
    let Some(value) = raw else {
        return Ok((false, None));
    };
    match value {
        Value::Null => Ok((true, None)),
        Value::String(s) if s.is_empty() || s == "__none__" => Ok((true, None)),
        _ => {
            let normalized = js_string(value).trim().to_string();
            if normalized.is_empty() {
                return Ok((true, None));
            }
            if lookup_proxy_pool(state, &normalized) {
                Ok((true, Some(normalized)))
            } else {
                Err("Proxy pool not found".into())
            }
        }
    }
}

fn lookup_proxy_pool(state: &AppState, id: &str) -> bool {
    let id = id.to_string();
    state
        .db
        .with_conn(move |conn| router_db::repos::proxy_pools::get_proxy_pool_by_id(conn, &id))
        .ok()
        .flatten()
        .is_some()
}

// ─── GET /api/providers ───────────────────────────────────────────────────

/// `GET /api/providers`.
pub async fn list(State(state): State<AppState>) -> Response {
    let result = state
        .read(|conn| {
            let connections =
                router_db::repos::connections::get_provider_connections(conn, None, None)?;
            // A node-read failure leaves the map empty rather than failing the
            // list.
            let nodes = router_db::repos::nodes::get_provider_nodes(conn, None).unwrap_or_default();

            let mut node_name_map: HashMap<String, String> = HashMap::new();
            for node in &nodes {
                if let (Some(id), Some(name)) = (conn_str(node, "id"), conn_str(node, "name")) {
                    node_name_map.insert(id.to_string(), name.to_string());
                }
            }

            let safe: Vec<Value> = connections
                .iter()
                .map(|c| {
                    let provider = conn_str(c, "provider").unwrap_or("");
                    let is_compatible = is_openai_compatible_provider(provider)
                        || is_anthropic_compatible_provider(provider);
                    let mut out = c.as_object().cloned().unwrap_or_default();
                    // Compatible providers get an enriched name; everyone else
                    // keeps the row's own `name` (dropped, not nulled, when the
                    // row has none).
                    if is_compatible {
                        let name = conn_str(c, "name")
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .or_else(|| node_name_map.get(provider).cloned())
                            .or_else(|| {
                                c.get("providerSpecificData")
                                    .and_then(|p| p.get("nodeName"))
                                    .and_then(Value::as_str)
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_string)
                            })
                            .unwrap_or_else(|| provider.to_string());
                        out.insert("name".into(), Value::String(name));
                    }
                    strip_secrets(&mut out);
                    Value::Object(out)
                })
                .collect();

            Ok(json!({ "connections": safe }))
        })
        .await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(_) => ApiError::internal("Failed to fetch providers").into_response(),
    }
}

// ─── POST /api/providers ──────────────────────────────────────────────────

/// `POST /api/providers`.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to create provider").into_response();
    };
    let Some(body) = payload.as_object() else {
        return ApiError::internal("Failed to create provider").into_response();
    };

    let provider_value = normalize_provider_id(body.get("provider").unwrap_or(&Value::Null));
    let Some(provider) = provider_value.as_str().filter(|s| !s.is_empty()) else {
        return ApiError::bad_request("Invalid provider").into_response();
    };

    let Ok(proxy_config) = normalize_proxy_config(body) else {
        return ApiError::bad_request(
            "Connection proxy URL is required when connection proxy is enabled",
        )
        .into_response();
    };
    let proxy_pool_id = match normalize_proxy_pool_id(&state, body.get("proxyPoolId")) {
        Ok(id) => id,
        Err(error) => return ApiError::bad_request(error).into_response(),
    };

    let is_web_cookie_provider = category_of(provider) == Some("webCookie");
    let supports_api_key_mode = registry()
        .get(provider)
        .and_then(|p| p.auth_modes.as_ref())
        .is_some_and(|modes| modes.iter().any(|m| m == "apikey"));
    let is_valid_provider = category_of(provider) == Some("apikey")
        || category_of(provider) == Some("freeTier")
        || supports_api_key_mode
        || is_web_cookie_provider
        || is_openai_compatible_provider(provider)
        || is_anthropic_compatible_provider(provider)
        || is_custom_embedding_provider(provider);
    if !is_valid_provider {
        return ApiError::bad_request("Invalid provider").into_response();
    }

    let api_key = conn_str(&payload, "apiKey").unwrap_or("");
    if api_key.is_empty() {
        let message = if is_web_cookie_provider {
            "Cookie value is required"
        } else {
            "API Key is required"
        };
        return ApiError::bad_request(message).into_response();
    }

    let connection_name = conn_str(&payload, "name")
        .filter(|s| !s.is_empty())
        .or_else(|| conn_str(&payload, "displayName").filter(|s| !s.is_empty()))
        .or_else(|| display_name(provider))
        .map(str::to_string);
    let Some(connection_name) = connection_name else {
        return ApiError::bad_request("Name is required").into_response();
    };

    // `normalizeProviderSpecificData(provider, body, body.providerSpecificData)`
    // — only `ollama-local` had a special case, and it is dropped.
    let mut provider_specific_data =
        match normalize_provider_specific_data(body.get("providerSpecificData")) {
            Value::Object(m) => m,
            _ => Map::new(),
        };

    if is_openai_compatible_provider(provider)
        || is_anthropic_compatible_provider(provider)
        || is_custom_embedding_provider(provider)
    {
        let node = state
            .read({
                let provider = provider.to_string();
                move |conn| router_db::repos::nodes::get_provider_node_by_id(conn, &provider)
            })
            .await
            .ok()
            .flatten();
        let Some(node) = node else {
            let message = if is_openai_compatible_provider(provider) {
                "OpenAI Compatible node not found"
            } else if is_anthropic_compatible_provider(provider) {
                "Anthropic Compatible node not found"
            } else {
                "Custom Embedding node not found"
            };
            return ApiError::not_found(message).into_response();
        };
        let mut node_psd = Map::new();
        node_psd.insert(
            "prefix".into(),
            node.get("prefix").cloned().unwrap_or(Value::Null),
        );
        if is_openai_compatible_provider(provider) {
            node_psd.insert(
                "apiType".into(),
                node.get("apiType").cloned().unwrap_or(Value::Null),
            );
        }
        node_psd.insert(
            "baseUrl".into(),
            node.get("baseUrl").cloned().unwrap_or(Value::Null),
        );
        node_psd.insert(
            "nodeName".into(),
            node.get("name").cloned().unwrap_or(Value::Null),
        );
        provider_specific_data = node_psd;
    }

    provider_specific_data.insert(
        "connectionProxyEnabled".into(),
        Value::Bool(proxy_config.enabled),
    );
    provider_specific_data.insert("connectionProxyUrl".into(), Value::String(proxy_config.url));
    provider_specific_data.insert(
        "connectionNoProxy".into(),
        Value::String(proxy_config.no_proxy),
    );
    if let Some(pool_id) = proxy_pool_id {
        provider_specific_data.insert("proxyPoolId".into(), Value::String(pool_id));
    }

    let allow_overwrite = if body.get("id").is_some_and(js_truthy) {
        true
    } else {
        body.get("allowOverwrite") == Some(&Value::Bool(true))
            || body.get("overwrite") == Some(&Value::Bool(true))
    };

    let mut data = Map::new();
    data.insert("provider".into(), json!(provider));
    data.insert(
        "authType".into(),
        json!(if is_web_cookie_provider {
            "cookie"
        } else {
            "apikey"
        }),
    );
    data.insert("name".into(), json!(connection_name));
    data.insert("apiKey".into(), json!(api_key));
    data.insert(
        "priority".into(),
        json!(
            body.get("priority")
                .and_then(Value::as_i64)
                .filter(|p| *p != 0)
                .unwrap_or(1)
        ),
    );
    data.insert(
        "globalPriority".into(),
        body.get("globalPriority").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "defaultModel".into(),
        body.get("defaultModel").cloned().unwrap_or(Value::Null),
    );
    data.insert(
        "providerSpecificData".into(),
        Value::Object(provider_specific_data),
    );
    data.insert("isActive".into(), Value::Bool(true));
    data.insert(
        "testStatus".into(),
        conn_str(&payload, "testStatus")
            .filter(|s| !s.is_empty())
            .map(|s| json!(s))
            .unwrap_or_else(|| json!("unknown")),
    );
    data.insert("allowOverwrite".into(), Value::Bool(allow_overwrite));
    let data = Value::Object(data);

    match state
        .write(move |tx| router_db::repos::connections::create_provider_connection(tx, &data))
        .await
    {
        Ok(connection) => {
            let result = hide_secrets(&connection);
            (StatusCode::CREATED, Json(json!({ "connection": result }))).into_response()
        }
        Err(router_db::DbError::ProviderNameConflict {
            message,
            existing_id,
            existing_name,
        }) => ApiError::new(StatusCode::CONFLICT, message)
            .with_extra(json!({
                "code": "PROVIDER_NAME_CONFLICT",
                "existingId": existing_id,
                "existingName": existing_name,
            }))
            .into_response(),
        Err(_) => ApiError::internal("Failed to create provider").into_response(),
    }
}

// ─── GET /api/providers/client ────────────────────────────────────────────

/// Run-once guard for the codex email backfill. Reset on failure.
static CODEX_BACKFILL_DONE: AtomicBool = AtomicBool::new(false);

/// `backfillCodexEmails()`.
async fn backfill_codex_emails(state: &AppState) {
    if CODEX_BACKFILL_DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let result = state
        .write(|tx| {
            let connections =
                router_db::repos::connections::get_provider_connections(tx, None, None)?;
            for conn in &connections {
                if conn_str(conn, "provider") != Some("codex")
                    || conn_str(conn, "authType") != Some("oauth")
                {
                    continue;
                }
                let Some(id_token) = conn_str(conn, "idToken").filter(|s| !s.is_empty()) else {
                    continue;
                };
                let has_email = conn_str(conn, "email").is_some_and(|s| !s.is_empty());
                let has_account_info = conn
                    .get("providerSpecificData")
                    .and_then(|p| p.get("chatgptAccountId"))
                    .is_some_and(js_truthy);
                if has_email && has_account_info {
                    continue;
                }

                let info = extract_codex_account_info(id_token);
                let email = info.get("email").cloned().unwrap_or(Value::Null);
                let account_id = info.get("chatgptAccountId").cloned().unwrap_or(Value::Null);
                let plan_type = info.get("chatgptPlanType").cloned().unwrap_or(Value::Null);
                if !js_truthy(&email) && !js_truthy(&account_id) {
                    continue;
                }

                let mut patch = Map::new();
                if !has_email && js_truthy(&email) {
                    patch.insert("email".into(), email);
                }
                if js_truthy(&account_id) || js_truthy(&plan_type) {
                    let mut psd = conn
                        .get("providerSpecificData")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    psd.insert("chatgptAccountId".into(), account_id);
                    psd.insert("chatgptPlanType".into(), plan_type);
                    patch.insert("providerSpecificData".into(), Value::Object(psd));
                }
                if patch.is_empty() {
                    continue;
                }
                let id = conn_str(conn, "id").unwrap_or("");
                router_db::repos::connections::update_provider_connection(
                    tx,
                    id,
                    &Value::Object(patch),
                )?;
            }
            Ok(())
        })
        .await;
    if result.is_err() {
        CODEX_BACKFILL_DONE.store(false, Ordering::SeqCst);
    }
}

/// The connection fields safe to expose on the client route.
const SAFE_FIELDS: [&str; 19] = [
    "id",
    "provider",
    "authType",
    "name",
    "email",
    "displayName",
    "priority",
    "globalPriority",
    "isActive",
    "defaultModel",
    "testStatus",
    "lastError",
    "lastErrorAt",
    "errorCode",
    "expiresAt",
    "lastUsedAt",
    "consecutiveUseCount",
    "createdAt",
    "updatedAt",
];

/// The `providerSpecificData` fields safe to expose on the client route.
const SAFE_PSD_FIELDS: [&str; 22] = [
    "baseUrl",
    "azureEndpoint",
    "deployment",
    "apiVersion",
    "accountId",
    "region",
    "projectId",
    "resourceUrl",
    "proxyPoolId",
    "connectionProxyEnabled",
    "connectionProxyUrl",
    "connectionNoProxy",
    "githubLogin",
    "githubName",
    "githubEmail",
    "githubUserId",
    "username",
    "firstName",
    "lastName",
    "authMethod",
    "authKind",
    "profileArn",
];

/// `maskName(name)`: a long opaque token in the name is truncated to a prefix.
fn mask_name(name: &Value) -> Value {
    let Some(text) = name.as_str() else {
        return name.clone();
    };
    if text.chars().count() <= 16 {
        return name.clone();
    }
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[a-zA-Z0-9_-]{32,}").expect("static pattern"));
    if TOKEN.is_match(text) {
        let prefix: String = text.chars().take(8).collect();
        return json!(format!("{prefix}***"));
    }
    name.clone()
}

/// `sanitize(c)`: only the safe fields survive, and the PSD keeps a fixed
/// subset.
fn sanitize_connection(c: &Value) -> Value {
    let mut safe = Map::new();
    for field in SAFE_FIELDS {
        if let Some(value) = c.get(field) {
            safe.insert(field.to_string(), value.clone());
        }
    }
    if safe.get("name").is_some_and(js_truthy) {
        let masked = mask_name(safe.get("name").unwrap_or(&Value::Null));
        safe.insert("name".into(), masked);
    }
    if c.get("providerSpecificData").is_some_and(js_truthy) {
        let mut psd = Map::new();
        for field in SAFE_PSD_FIELDS {
            if let Some(value) = c.get("providerSpecificData").and_then(|p| p.get(field)) {
                psd.insert(field.to_string(), value.clone());
            }
        }
        safe.insert("providerSpecificData".into(), Value::Object(psd));
    }
    Value::Object(safe)
}

/// `isUsageEligible(connection)`.
fn is_usage_eligible(connection: &Value) -> bool {
    let provider = conn_str(connection, "provider").unwrap_or("");
    usage_supported().contains(&provider)
        && (conn_str(connection, "authType") == Some("oauth") || usage_apikey().contains(&provider))
}

/// `parsePositiveInt(value, fallback)`.
fn parse_positive_int(value: Option<&str>, fallback: i64) -> i64 {
    let Some(raw) = value else {
        return fallback;
    };
    let trimmed = raw.trim_start();
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return fallback;
    }
    match digits.parse::<i64>() {
        Ok(n) if !negative && n > 0 => n,
        _ => fallback,
    }
}

/// `sortConnections(connections, sort)`.
fn sort_connections(mut list: Vec<Value>, sort: &str) -> Vec<Value> {
    let supported = usage_supported();
    if sort == "provider" {
        list.sort_by(|a, b| {
            let pa = conn_str(a, "provider").unwrap_or("");
            let pb = conn_str(b, "provider").unwrap_or("");
            let oa = supported
                .iter()
                .position(|p| *p == pa)
                .map_or(-1i64, |i| i as i64);
            let ob = supported
                .iter()
                .position(|p| *p == pb)
                .map_or(-1i64, |i| i as i64);
            if oa != ob {
                return oa.cmp(&ob);
            }
            pa.cmp(pb)
        });
        return list;
    }
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    list.sort_by(|a, b| {
        let pa = a
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(MAX_SAFE_INTEGER);
        let pb = b
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(MAX_SAFE_INTEGER);
        if pa != pb {
            return pa.cmp(&pb);
        }
        conn_str(a, "provider")
            .unwrap_or("")
            .cmp(conn_str(b, "provider").unwrap_or(""))
    });
    list
}

/// `GET /api/providers/client`.
pub async fn client(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    backfill_codex_emails(&state).await;

    let provider = params.get("provider").map(String::as_str).unwrap_or("all");
    let account_status = params
        .get("accountStatus")
        .map(String::as_str)
        .unwrap_or("all");
    let sort = params.get("sort").map(String::as_str).unwrap_or("priority");
    let page = parse_positive_int(params.get("page").map(String::as_str), 1);
    let page_size = parse_positive_int(params.get("pageSize").map(String::as_str), 20).min(500);

    let result = state
        .read(|conn| router_db::repos::connections::get_provider_connections(conn, None, None))
        .await;
    let Ok(all_connections) = result else {
        return ApiError::internal("Failed to fetch providers").into_response();
    };

    let eligible: Vec<Value> = all_connections
        .iter()
        .filter(|c| is_usage_eligible(c))
        .cloned()
        .collect();

    let mut provider_options: Vec<String> = eligible
        .iter()
        .filter_map(|c| conn_str(c, "provider").map(str::to_string))
        .collect();
    provider_options.sort();
    provider_options.dedup();

    let provider_filtered: Vec<Value> = eligible
        .iter()
        .filter(|c| provider == "all" || conn_str(c, "provider") == Some(provider))
        .cloned()
        .collect();

    let account_filtered: Vec<Value> = provider_filtered
        .iter()
        .filter(|c| {
            let active = c.get("isActive").and_then(Value::as_bool).unwrap_or(true);
            match account_status {
                "active" => active,
                "inactive" => !active,
                _ => true,
            }
        })
        .cloned()
        .collect();

    let sorted = sort_connections(account_filtered, sort);
    let total = sorted.len() as i64;
    let total_pages = ((total + page_size - 1) / page_size).max(1);
    let current_page = page.min(total_pages);
    let offset = ((current_page - 1) * page_size).max(0) as usize;
    let page_connections: Vec<Value> = sorted
        .iter()
        .skip(offset)
        .take(page_size as usize)
        .map(sanitize_connection)
        .collect();

    Json(json!({
        "connections": page_connections,
        "providerOptions": provider_options,
        "pagination": {
            "page": current_page,
            "pageSize": page_size,
            "total": total,
            "totalPages": total_pages,
        },
        "totals": {
            "eligibleConnections": eligible.len(),
            "providerFilteredConnections": provider_filtered.len(),
        },
    }))
    .into_response()
}

// ─── GET|PUT|DELETE /api/providers/{id} ───────────────────────────────────

/// `GET /api/providers/{id}`.
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let connection = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();
    let Some(connection) = connection else {
        return ApiError::not_found("Connection not found").into_response();
    };
    Json(json!({ "connection": Value::Object(hide_secrets(&connection)) })).into_response()
}

/// `PUT /api/providers/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update connection").into_response();
    };
    let Some(body) = payload.as_object() else {
        return ApiError::internal("Failed to update connection").into_response();
    };

    let existing = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();
    let Some(existing) = existing else {
        return ApiError::not_found("Connection not found").into_response();
    };

    let Ok((has_any_proxy_field, proxy_config)) = normalize_proxy_config_update(body) else {
        return ApiError::bad_request(
            "Connection proxy URL is required when connection proxy is enabled",
        )
        .into_response();
    };
    let (has_proxy_pool_field, proxy_pool_id) =
        match normalize_proxy_pool_update(&state, body.get("proxyPoolId")) {
            Ok(value) => value,
            Err(error) => return ApiError::bad_request(error).into_response(),
        };

    let mut update_data = Map::new();
    for key in [
        "name",
        "priority",
        "globalPriority",
        "defaultModel",
        "isActive",
    ] {
        if let Some(value) = body.get(key) {
            update_data.insert(key.to_string(), value.clone());
        }
    }
    if let Some(api_key) = body.get("apiKey").and_then(Value::as_str)
        && !api_key.is_empty()
        && conn_str(&existing, "authType") == Some("apikey")
    {
        update_data.insert("apiKey".into(), json!(api_key));
    }
    for key in ["testStatus", "lastError", "lastErrorAt"] {
        if let Some(value) = body.get(key) {
            update_data.insert(key.to_string(), value.clone());
        }
    }

    let existing_psd = existing.get("providerSpecificData");
    let incoming_psd = body.get("providerSpecificData");
    let should_merge = existing_psd.is_some()
        || incoming_psd.is_some()
        || has_any_proxy_field
        || has_proxy_pool_field;
    if should_merge {
        let mut psd = existing_psd
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(incoming) = incoming_psd.and_then(Value::as_object) {
            for (k, v) in incoming {
                psd.insert(k.clone(), v.clone());
            }
        }
        if has_any_proxy_field {
            psd.insert(
                "connectionProxyEnabled".into(),
                Value::Bool(proxy_config.enabled),
            );
            psd.insert("connectionProxyUrl".into(), Value::String(proxy_config.url));
            psd.insert(
                "connectionNoProxy".into(),
                Value::String(proxy_config.no_proxy),
            );
        }
        if has_proxy_pool_field {
            match proxy_pool_id {
                None => {
                    psd.shift_remove("proxyPoolId");
                }
                Some(pool_id) => {
                    psd.insert("proxyPoolId".into(), Value::String(pool_id));
                }
            }
        }
        update_data.insert("providerSpecificData".into(), Value::Object(psd));
    }

    let update_data = Value::Object(update_data);
    let updated = state
        .write({
            let id = id.clone();
            move |tx| {
                router_db::repos::connections::update_provider_connection(tx, &id, &update_data)
            }
        })
        .await;
    match updated {
        Ok(Some(connection)) => {
            Json(json!({ "connection": Value::Object(hide_secrets(&connection)) })).into_response()
        }
        Ok(None) => Json(json!({ "connection": {} })).into_response(),
        Err(_) => ApiError::internal("Failed to update connection").into_response(),
    }
}

/// `DELETE /api/providers/{id}`.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let deleted = state
        .write({
            let id = id.clone();
            move |tx| router_db::repos::connections::delete_provider_connection(tx, &id)
        })
        .await;
    match deleted {
        Ok(true) => Json(json!({ "message": "Connection deleted successfully" })).into_response(),
        Ok(false) => ApiError::not_found("Connection not found").into_response(),
        Err(_) => ApiError::internal("Failed to delete connection").into_response(),
    }
}

// ─── POST /api/providers/{id}/test-models ─────────────────────────────────

/// One `{id, name}` pair the model ping walks.
struct TestModel {
    id: String,
    name: String,
    kind: String,
}

/// `POST /api/providers/{id}/test-models`.
pub async fn test_models(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let connection = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();
    let Some(connection) = connection else {
        return ApiError::not_found("Connection not found").into_response();
    };

    let provider_id = conn_str(&connection, "provider").unwrap_or("").to_string();
    let is_compatible = is_openai_compatible_provider(&provider_id)
        || is_anthropic_compatible_provider(&provider_id);
    let alias = registry().alias_for(&provider_id).to_string();

    let mut models: Vec<TestModel> = registry()
        .models_for(&alias)
        .iter()
        .map(|m| TestModel {
            id: m.id.clone(),
            name: m.name.clone().unwrap_or_else(|| m.id.clone()),
            kind: m.kind().to_string(),
        })
        .collect();

    let base_url = format!("http://127.0.0.1:{}", crate::state::resolve_port());

    if is_compatible && models.is_empty() {
        let ids =
            router_sse::services::model_catalog::fetch_compatible_model_ids(&connection).await;
        models = ids
            .into_iter()
            .map(|model_id| TestModel {
                name: model_id.clone(),
                id: model_id,
                kind: "llm".to_string(),
            })
            .collect();
    }

    if models.is_empty() {
        return ApiError::bad_request("No models configured for this provider").into_response();
    }

    let headers = internal_headers(&state).await;
    let first = &models[0];
    // Warm up with the first model so a token refresh lands before the rest.
    let first_result = router_sse::services::ping::ping_model_by_kind(
        &format!("{alias}/{}", first.id),
        &first.kind,
        &base_url,
        &headers,
    )
    .await;
    let mut results = vec![model_result(first, first_result)];

    for model in &models[1..] {
        let result = router_sse::services::ping::ping_model_by_kind(
            &format!("{alias}/{}", model.id),
            &model.kind,
            &base_url,
            &headers,
        )
        .await;
        results.push(model_result(model, result));
    }

    Json(json!({ "provider": provider_id, "connectionId": id, "results": results })).into_response()
}

fn model_result(model: &TestModel, result: Value) -> Value {
    let mut out = Map::new();
    out.insert("modelId".into(), json!(model.id));
    out.insert("name".into(), json!(model.name));
    if let Value::Object(result) = result {
        for (k, v) in result {
            out.insert(k, v);
        }
    }
    Value::Object(out)
}

// ─── GET /api/providers/{id}/models ───────────────────────────────────────

/// `getStaticProviderModels(providerId)`: `{...model, id, name: name || id}`
/// with the raw key order preserved.
fn static_provider_models(provider_id: &str) -> Vec<Value> {
    let alias = registry().alias_for(provider_id);
    let Some(raw) = raw_models_for(alias) else {
        return Vec::new();
    };
    raw.iter()
        .map(|model| {
            let mut out = model.as_object().cloned().unwrap_or_default();
            let id = out.get("id").cloned().unwrap_or(Value::Null);
            let name = out
                .get("name")
                .filter(|n| !n.is_null())
                .cloned()
                .unwrap_or_else(|| id.clone());
            out.insert("id".into(), id);
            out.insert("name".into(), name);
            Value::Object(out)
        })
        .collect()
}

/// `parseOpenAIStyleModels(data)`: an array, or `data.data || data.models ||
/// data.results || []`.
fn parse_openai_style_models(data: &Value) -> Vec<Value> {
    if let Some(list) = data.as_array() {
        return list.clone();
    }
    for key in ["data", "models", "results"] {
        if let Some(list) = data.get(key).and_then(Value::as_array) {
            return list.clone();
        }
    }
    Vec::new()
}

/// `appendCodexReviewModels(models)`.
fn append_codex_review_models(models: Vec<Value>) -> Vec<Value> {
    let mut out = Vec::with_capacity(models.len());
    for model in models {
        let id = ["id", "slug", "model", "name"]
            .iter()
            .find_map(|k| model.get(k).and_then(Value::as_str))
            .map(str::to_string);
        let Some(id) = id else {
            continue;
        };
        let name = ["display_name", "displayName", "name"]
            .iter()
            .find_map(|k| model.get(k).and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| id.clone());
        let mut normalized = model.as_object().cloned().unwrap_or_default();
        normalized.insert("id".into(), json!(id));
        normalized.insert("name".into(), json!(name));

        let model_type = model.get("type").and_then(Value::as_str).unwrap_or("llm");
        let is_chat_model = model_type != "image" && !id.to_lowercase().contains("embed");
        if !is_chat_model || id.ends_with("-review") {
            out.push(Value::Object(normalized));
            continue;
        }
        out.push(Value::Object(normalized.clone()));
        normalized.insert("id".into(), json!(format!("{id}-review")));
        normalized.insert("name".into(), json!(format!("{name} Review")));
        normalized.insert("upstreamModelId".into(), json!(id));
        normalized.insert("quotaFamily".into(), json!("review"));
        out.push(Value::Object(normalized));
    }
    out
}

/// The static GET-config table, restricted to the kept provider ids.
struct ModelsConfig {
    url: &'static str,
    method: Method,
    headers: &'static [(&'static str, &'static str)],
    auth_header: Option<&'static str>,
    auth_prefix: &'static str,
    auth_query: Option<&'static str>,
    body: bool,
    parse: fn(&Value) -> Vec<Value>,
}

fn models_config(provider_id: &str) -> Option<ModelsConfig> {
    let openai = |url: &'static str| ModelsConfig {
        url,
        method: Method::GET,
        headers: &[("Content-Type", "application/json")],
        auth_header: Some("Authorization"),
        auth_prefix: "Bearer ",
        auth_query: None,
        body: false,
        parse: parse_openai_style_models,
    };
    match provider_id {
        "deepseek" => Some(openai("https://api.deepseek.com/models")),
        "mistral" => Some(openai("https://api.mistral.ai/v1/models")),
        "nvidia" => Some(openai("https://integrate.api.nvidia.com/v1/models")),
        "openrouter" => Some(openai("https://openrouter.ai/api/v1/models")),
        "tokenharbor" => Some(openai("https://tokenharbor.ai/v1/models")),
        "dahl" => Some(openai("https://inference.dahl.global/v1/models")),
        "atria" => Some(openai("https://api.atria-asi.ai/v1/models")),
        "agnes" => Some(openai("https://apihub.agnes-ai.com/v1/models")),
        "bai" => Some(openai("https://api.b.ai/v1/models")),
        _ => None,
    }
}

/// The result of a custom resolver: models, an optional warning, or an error.
enum ResolverOutcome {
    Models(Vec<Value>, Option<String>),
    Error(String, StatusCode),
}

/// `resolveGrokCliModels`, with the static-catalog fallback and warning text.
async fn custom_resolver(
    state: &AppState,
    provider_id: &str,
    connection: &Value,
) -> Option<ResolverOutcome> {
    match provider_id {
        "grok-cli" => {
            let live = resolve_live_models(&state.db, "grok-cli", connection).await;
            Some(match live {
                Some(models) if !models.is_empty() => {
                    ResolverOutcome::Models(models.into_iter().map(|m| m.raw).collect(), None)
                }
                _ => ResolverOutcome::Models(
                    static_provider_models("grok-cli"),
                    Some("Grok CLI returned no live models; using static catalog.".into()),
                ),
            })
        }
        // `buildOAuthResolver` has no static fallback: an empty live list comes
        // back as `models: []` plus a warning, never a silent zero list.
        "codex" => Some(codex_live_models(state, connection).await),
        _ => None,
    }
}

/// `buildOAuthResolver`'s fetch step, for the codex live-models endpoint: a
/// 401/403 triggers a refresh, the credentials are persisted, and the request
/// retries once. A missing access token is the resolver's 401; otherwise the
/// outcome carries the models, or an empty list plus a warning.
async fn codex_live_models(state: &AppState, connection: &Value) -> ResolverOutcome {
    let Some(access_token) = conn_str(connection, "accessToken").filter(|s| !s.is_empty()) else {
        return ResolverOutcome::Error("No valid token found".into(), StatusCode::UNAUTHORIZED);
    };
    let Some(client) = probe_client() else {
        return ResolverOutcome::Models(
            Vec::new(),
            Some("Failed to fetch Codex models: Network error".into()),
        );
    };

    let client_version = registry()
        .transport("codex")
        .and_then(|t| t.client_version.clone())
        .unwrap_or_else(|| "0.144.6".to_string());
    let url =
        format!("https://chatgpt.com/backend-api/codex/models?client_version={client_version}");
    let fetch = |token: &str| {
        client
            .get(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("Authorization", format!("Bearer {token}"))
            .header("originator", "codex_cli_rs")
            .send()
    };

    let refresh_token = conn_str(connection, "refreshToken").unwrap_or("");
    let mut response = match fetch(access_token).await {
        Ok(response) => response,
        Err(err) => {
            return ResolverOutcome::Models(
                Vec::new(),
                Some(format!("Failed to fetch Codex models: {err}")),
            );
        }
    };

    if (response.status().as_u16() == 401 || response.status().as_u16() == 403)
        && !refresh_token.is_empty()
        && let Some(patch) = refresh_oauth_token(state, "codex", connection).await
    {
        let id = conn_str(connection, "id").unwrap_or("");
        update_provider_credentials(&state.db, id, &patch);
        if let Some(token) = patch.get("accessToken").and_then(Value::as_str)
            && let Ok(retry) = fetch(token).await
        {
            response = retry;
        }
    }

    if response.status().is_success() {
        let data: Value = response.json().await.unwrap_or(Value::Null);
        let models = append_codex_review_models(parse_openai_style_models(&data));
        if !models.is_empty() {
            return ResolverOutcome::Models(models, None);
        }
        return ResolverOutcome::Models(Vec::new(), None);
    }
    let status = response.status().as_u16();
    let error_text = response.text().await.unwrap_or_default();
    ResolverOutcome::Models(
        Vec::new(),
        Some(format!(
            "Failed to fetch Codex models: {status} {error_text}"
        )),
    )
}

/// `GET /api/providers/{id}/models`.
pub async fn models(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let connection = state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();
    let Some(connection) = connection else {
        return ApiError::not_found("Connection not found").into_response();
    };

    let provider_id = conn_str(&connection, "provider").unwrap_or("").to_string();
    let connection_id = conn_str(&connection, "id").unwrap_or("").to_string();
    let api_key = conn_str(&connection, "apiKey").unwrap_or("");
    let base = connection
        .get("providerSpecificData")
        .and_then(|p| p.get("baseUrl"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if is_openai_compatible_provider(&provider_id) {
        let Some(base) = base.filter(|s| !s.is_empty()) else {
            return ApiError::bad_request("No base URL configured for OpenAI compatible provider")
                .into_response();
        };
        let url = format!("{}/models", base.trim_end_matches('/'));
        let Some(client) = probe_client() else {
            return ApiError::internal("Failed to fetch models").into_response();
        };
        return match client
            .get(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                let data: Value = res.json().await.unwrap_or(Value::Null);
                let models = data
                    .get("data")
                    .and_then(Value::as_array)
                    .or_else(|| data.get("models").and_then(Value::as_array))
                    .cloned()
                    .unwrap_or_default();
                Json(json!({
                    "provider": provider_id,
                    "connectionId": connection_id,
                    "models": models,
                }))
                .into_response()
            }
            Ok(res) => status_error("Failed to fetch models", res.status()),
            Err(_) => ApiError::internal("Failed to fetch models").into_response(),
        };
    }

    if is_anthropic_compatible_provider(&provider_id) {
        let Some(base) = base.filter(|s| !s.is_empty()) else {
            return ApiError::bad_request(
                "No base URL configured for Anthropic compatible provider",
            )
            .into_response();
        };
        let normalized = crate::routes::provider_nodes::sanitize_anthropic_base(&base);
        let url = format!("{normalized}/models");
        let Some(client) = probe_client() else {
            return ApiError::internal("Failed to fetch models").into_response();
        };
        return match client
            .get(&url)
            .header("Content-Type", "application/json")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("Authorization", format!("Bearer {api_key}"))
            .send()
            .await
        {
            Ok(res) if res.status().is_success() => {
                let data: Value = res.json().await.unwrap_or(Value::Null);
                let models = data
                    .get("data")
                    .and_then(Value::as_array)
                    .or_else(|| data.get("models").and_then(Value::as_array))
                    .cloned()
                    .unwrap_or_default();
                Json(json!({
                    "provider": provider_id,
                    "connectionId": connection_id,
                    "models": models,
                }))
                .into_response()
            }
            Ok(res) => status_error("Failed to fetch models", res.status()),
            Err(_) => ApiError::internal("Failed to fetch models").into_response(),
        };
    }

    if let Some(outcome) = custom_resolver(&state, &provider_id, &connection).await {
        return match outcome {
            ResolverOutcome::Models(models, warning) => match warning {
                Some(warning) => Json(json!({
                    "provider": provider_id,
                    "connectionId": connection_id,
                    "models": models,
                    "warning": warning,
                }))
                .into_response(),
                None => Json(json!({
                    "provider": provider_id,
                    "connectionId": connection_id,
                    "models": models,
                }))
                .into_response(),
            },
            ResolverOutcome::Error(error, status) => ApiError::new(status, error).into_response(),
        };
    }

    let Some(config) = models_config(&provider_id) else {
        return ApiError::bad_request(format!(
            "Provider {provider_id} does not support models listing"
        ))
        .into_response();
    };

    let token = connection
        .get("providerSpecificData")
        .and_then(|p| p.get("copilotToken"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| conn_str(&connection, "accessToken").filter(|s| !s.is_empty()))
        .or_else(|| Some(api_key).filter(|s| !s.is_empty()));
    let Some(token) = token else {
        return ApiError::unauthorized("No valid token found").into_response();
    };

    let mut url = config.url.to_string();
    if let Some(query) = config.auth_query {
        url = format!("{url}?{query}={token}");
    }

    let Some(client) = probe_client() else {
        return ApiError::internal("Failed to fetch models").into_response();
    };
    let mut request = client.request(config.method.clone(), &url);
    for (name, value) in config.headers {
        request = request.header(*name, *value);
    }
    if let Some(auth_header) = config.auth_header
        && config.auth_query.is_none()
    {
        request = request.header(auth_header, format!("{}{token}", config.auth_prefix));
    }
    if config.body && config.method == Method::POST {
        request = request.json(&json!({}));
    }

    match request.send().await {
        Ok(res) if res.status().is_success() => {
            let data: Value = res.json().await.unwrap_or(Value::Null);
            let models = (config.parse)(&data);
            Json(json!({
                "provider": provider_id,
                "connectionId": connection_id,
                "models": models,
            }))
            .into_response()
        }
        Ok(res) => status_error("Failed to fetch models", res.status()),
        Err(_) => ApiError::internal("Failed to fetch models").into_response(),
    }
}

/// A `{ error: "..." }` body carrying the upstream's own status.
fn status_error(prefix: &str, status: StatusCode) -> Response {
    ApiError::new(status, format!("{prefix}: {}", status.as_u16())).into_response()
}

// ─── GET /api/providers/suggested-models ──────────────────────────────────

/// The suggested-model filters, keyed by source kind.
fn filter_suggested(kind: &str, models: &[Value]) -> Option<Vec<Value>> {
    match kind {
        "openrouter-free" => {
            let mut out: Vec<Value> = models
                .iter()
                .filter(|m| {
                    m.pointer("/pricing/prompt").and_then(Value::as_str) == Some("0")
                        && m.pointer("/pricing/completion").and_then(Value::as_str) == Some("0")
                        && m.get("context_length")
                            .and_then(Value::as_i64)
                            .is_some_and(|n| n >= 200_000)
                })
                .map(|m| {
                    json!({
                        "id": m.get("id").cloned().unwrap_or(Value::Null),
                        "name": m.get("name").cloned().unwrap_or(Value::Null),
                        "contextLength": m.get("context_length").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            out.sort_by_key(|m| {
                std::cmp::Reverse(m.get("contextLength").and_then(Value::as_i64).unwrap_or(0))
            });
            Some(out)
        }
        "opencode-free" => Some(
            models
                .iter()
                .filter(|m| {
                    let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                    (id.ends_with("-free") || id == "big-pickle") && id != "deepseek-v4-flash-free"
                })
                .map(|m| {
                    let id = m.get("id").cloned().unwrap_or(Value::Null);
                    json!({ "id": id.clone(), "name": id })
                })
                .collect(),
        ),
        "opencode-go" => Some(
            models
                .iter()
                .filter(|m| m.get("id").is_some_and(Value::is_string))
                .map(|m| {
                    let id = m.get("id").cloned().unwrap_or(Value::Null);
                    json!({ "id": id.clone(), "name": id })
                })
                .collect(),
        ),
        "mimo-free" => Some(
            models
                .iter()
                .filter(|m| {
                    m.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with("mimo"))
                        || m.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| name.to_lowercase().contains("mimo"))
                })
                .map(|m| {
                    let id = m.get("id").cloned().unwrap_or(Value::Null);
                    let name = m.get("name").cloned().unwrap_or_else(|| id.clone());
                    json!({ "id": id, "name": name })
                })
                .collect(),
        ),
        "airforce-free" => {
            let mut out: Vec<Value> = models
                .iter()
                .filter(|m| {
                    let id = m.get("id").and_then(Value::as_str).unwrap_or("");
                    (m.get("tier").and_then(Value::as_str) == Some("free") || id.ends_with(":free"))
                        && m.get("supports_chat") == Some(&Value::Bool(true))
                        && m.get("media_type")
                            .and_then(Value::as_str)
                            .is_none_or(|t| t == "chat" || t == "text")
                })
                .map(|m| {
                    let id = m.get("id").cloned().unwrap_or(Value::Null);
                    let name = m.get("name").cloned().unwrap_or_else(|| id.clone());
                    json!({
                        "id": id,
                        "name": name,
                        "contextLength": m.get("context_length").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            out.sort_by(|a, b| {
                a.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .cmp(b.get("id").and_then(Value::as_str).unwrap_or(""))
            });
            Some(out)
        }
        _ => None,
    }
}

/// `GET /api/providers/suggested-models`.
pub async fn suggested_models(Query(params): Query<HashMap<String, String>>) -> Response {
    let url = params.get("url").map(String::as_str).unwrap_or("");
    let kind = params.get("type").map(String::as_str).unwrap_or("");
    if url.is_empty() || kind.is_empty() {
        return ApiError::bad_request("Missing url or type").into_response();
    }
    if filter_suggested(kind, &[]).is_none() {
        return ApiError::bad_request("Unknown filter type").into_response();
    }

    let Ok(parsed) = url::Url::parse(url) else {
        return Json(json!({ "data": [] })).into_response();
    };
    // The URL is caller-supplied, so it goes through the SSRF guard before the
    // probe connects. A refusal returns the same empty list as any other miss.
    if ssrf::assert_public_url_resolved(url).await.is_err() {
        return Json(json!({ "data": [] })).into_response();
    }
    let Some(client) = probe_client() else {
        return Json(json!({ "data": [] })).into_response();
    };
    let response = match client.get(parsed).send().await {
        Ok(res) if res.status().is_success() => res,
        _ => return Json(json!({ "data": [] })).into_response(),
    };
    let Ok(json_body) = response.json::<Value>().await else {
        return Json(json!({ "data": [] })).into_response();
    };

    // `json.data ?? json.models ?? json` — `??`, so an explicit null falls
    // through.
    let raw = json_body
        .get("data")
        .filter(|v| !v.is_null())
        .or_else(|| json_body.get("models").filter(|v| !v.is_null()))
        .cloned()
        .unwrap_or_else(|| json_body.clone());
    let list = raw.as_array().cloned().unwrap_or_default();
    let data = filter_suggested(kind, &list).unwrap_or_default();
    Json(json!({ "data": data })).into_response()
}

// ─── POST /api/providers/test-batch ───────────────────────────────────────

/// `getAuthGroup(providerId, connection)`.
fn auth_group(provider_id: &str, connection: Option<&Value>) -> String {
    if let Some(auth_type) = connection.and_then(|c| conn_str(c, "authType")) {
        if auth_type == "oauth" {
            if category_of(provider_id) == Some("free") {
                return "free".into();
            }
            return "oauth".into();
        }
        return auth_type.to_string();
    }
    if category_of(provider_id) == Some("free") {
        return "free".into();
    }
    if category_of(provider_id) == Some("oauth") {
        return "oauth".into();
    }
    if category_of(provider_id) == Some("apikey") {
        return "apikey".into();
    }
    if is_openai_compatible_provider(provider_id) || is_anthropic_compatible_provider(provider_id) {
        return "compatible".into();
    }
    "apikey".into()
}

/// `isCompatibleProvider(providerId)`.
fn is_compatible_provider(provider_id: &str) -> bool {
    is_openai_compatible_provider(provider_id) || is_anthropic_compatible_provider(provider_id)
}

/// `POST /api/providers/test-batch`.
pub async fn test_batch(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Batch test failed").into_response();
    };
    let Some(mode) = payload
        .get("mode")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ApiError::bad_request("mode is required").into_response();
    };
    let provider_id = payload.get("providerId").and_then(Value::as_str);

    let all = state
        .read(|conn| {
            router_db::repos::connections::get_provider_connections(conn, None, Some(true))
        })
        .await
        .unwrap_or_default();

    let to_test: Vec<Value> = match mode {
        "provider" => match provider_id {
            Some(provider) => all
                .iter()
                .filter(|c| conn_str(c, "provider") == Some(provider))
                .cloned()
                .collect(),
            None => Vec::new(),
        },
        "oauth" => all
            .iter()
            .filter(|c| auth_group(conn_str(c, "provider").unwrap_or(""), Some(c)) == "oauth")
            .cloned()
            .collect(),
        "free" => all
            .iter()
            .filter(|c| auth_group(conn_str(c, "provider").unwrap_or(""), Some(c)) == "free")
            .cloned()
            .collect(),
        "apikey" => all
            .iter()
            .filter(|c| auth_group(conn_str(c, "provider").unwrap_or(""), Some(c)) == "apikey")
            .cloned()
            .collect(),
        "compatible" => all
            .iter()
            .filter(|c| is_compatible_provider(conn_str(c, "provider").unwrap_or("")))
            .cloned()
            .collect(),
        "all" => all.clone(),
        _ => {
            return ApiError::bad_request(
                "Invalid mode. Use: provider, oauth, free, apikey, compatible, all",
            )
            .into_response();
        }
    };

    if to_test.is_empty() {
        return Json(json!({
            "mode": mode,
            "providerId": provider_id.map(|s| json!(s)).unwrap_or(Value::Null),
            "results": [],
            "summary": { "total": 0, "passed": 0, "failed": 0 },
            "testedAt": now_iso(),
        }))
        .into_response();
    }

    let mut results = Vec::with_capacity(to_test.len());
    for conn in &to_test {
        let conn_id = conn_str(conn, "id").unwrap_or("").to_string();
        let provider = conn_str(conn, "provider").unwrap_or("").to_string();
        let connection_name = conn_str(conn, "name")
            .filter(|s| !s.is_empty())
            .or_else(|| conn_str(conn, "email").filter(|s| !s.is_empty()))
            .unwrap_or(&provider)
            .to_string();
        let auth_type = conn_str(conn, "authType")
            .map(str::to_string)
            .unwrap_or_else(|| auth_group(&provider, Some(conn)));

        let tested = test_single_connection(&state, &conn_id).await;
        results.push(json!({
            "provider": provider,
            "connectionId": conn_id,
            "connectionName": connection_name,
            "authType": auth_type,
            "valid": tested.valid,
            "latencyMs": tested.latency_ms,
            "error": tested.error,
            "diagnosis": Value::Null,
            "statusCode": Value::Null,
            "testedAt": tested.tested_at,
        }));
    }

    let passed = results
        .iter()
        .filter(|r| r.get("valid") == Some(&Value::Bool(true)))
        .count();
    let total = results.len();
    Json(json!({
        "mode": mode,
        "providerId": provider_id.map(|s| json!(s)).unwrap_or(Value::Null),
        "results": results,
        "testedAt": now_iso(),
        "summary": { "total": total, "passed": passed, "failed": total - passed },
    }))
    .into_response()
}

// ─── connection testing ───────────────────────────────────────────────────

/// `proxyOptions` for a connection probe, built from the resolved
/// per-connection proxy config.
fn proxy_options(resolved: &ResolvedProxyConfig) -> ProxyOptions {
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    ProxyOptions {
        enabled: resolved.connection_proxy_enabled,
        url: non_empty(&resolved.connection_proxy_url),
        no_proxy: non_empty(&resolved.connection_no_proxy),
        strict_proxy: resolved.strict_proxy,
        vercel_relay_url: non_empty(&resolved.vercel_relay_url),
    }
}

/// Send a probe through the connection's own proxy, or the relay, or direct.
/// Every probe is capped at 15 seconds.
async fn send_probe(
    proxy: &ProxyOptions,
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<&Value>,
) -> Option<reqwest::Response> {
    let target = prepare_send(url, proxy).await.ok()?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut builder = target
        .client
        .request(method, &target.url)
        .timeout(Duration::from_secs(15));
    for (name, value) in headers.iter().chain(target.extra_headers.iter()) {
        builder = builder.header(name, value);
    }
    if let Some(body) = body {
        builder = builder.json(body);
    }
    builder.send().await.ok()
}

/// A probe result: `{valid, error, warning, refreshed, newTokens}`.
#[derive(Default)]
struct ProbeResult {
    valid: bool,
    error: Value,
    warning: Option<String>,
    refreshed: bool,
    new_tokens: Option<Value>,
}

impl ProbeResult {
    fn invalid(error: impl Into<String>) -> Self {
        Self {
            valid: false,
            error: json!(error.into()),
            ..Self::default()
        }
    }

    fn ok() -> Self {
        Self {
            valid: true,
            ..Self::default()
        }
    }
}

/// `testSingleConnection(id)`'s return shape.
struct TestOutcome {
    valid: bool,
    error: Value,
    refreshed: bool,
    latency_ms: i64,
    tested_at: String,
}

/// `refreshOAuthToken(connection)`: the sparse patch a refresh produced, or
/// `None` when it failed or there is no refresh token.
async fn refresh_oauth_token(
    state: &AppState,
    provider: &str,
    connection: &Value,
) -> Option<Value> {
    let credentials = credentials_from_connection(connection);
    if credentials
        .refresh_token
        .as_deref()
        .unwrap_or("")
        .is_empty()
    {
        return None;
    }
    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned();
    let resolved = resolve_connection_proxy_config(&state.db, psd.as_ref());
    let refreshed =
        refresh_token_by_provider(provider, &credentials, &proxy_options(&resolved)).await?;
    let patch = merge_refreshed_credentials(
        provider,
        &credentials,
        &refreshed,
        router_db::time::now_ms(),
    )?;
    if patch.get("error").is_some() {
        return None;
    }
    Some(patch)
}

/// The static half of `OAUTH_TEST_CONFIG`, restricted to the kept providers.
struct OauthTestConfig {
    url: Option<String>,
    method: &'static str,
    auth_header: Option<&'static str>,
    auth_prefix: &'static str,
    extra_headers: Vec<(String, String)>,
    body: Option<Value>,
    accept_statuses: Vec<u16>,
    soft_fail: Vec<(u16, &'static str)>,
    refreshable: bool,
    check_expiry: bool,
    no_auth: bool,
}

fn oauth_test_config(provider: &str) -> Option<OauthTestConfig> {
    let codex_version = registry()
        .transport("codex")
        .and_then(|t| t.cli_version.clone())
        .unwrap_or_else(|| "0.155.0".to_string());
    let grok_user_url = registry()
        .transport("grok-cli")
        .and_then(|t| t.user_url.clone())
        .unwrap_or_else(|| "https://cli-chat-proxy.grok.com/v1/user".to_string());
    let grok_headers: Vec<(String, String)> = registry()
        .transport("grok-cli")
        .and_then(|t| t.headers.as_ref())
        .map(|h| {
            h.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                (
                    "User-Agent".into(),
                    "grok-pager/0.2.93 grok-shell/0.2.93 (linux; x86_64)".into(),
                ),
                ("x-xai-token-auth".into(), "xai-grok-cli".into()),
                ("x-grok-client-identifier".into(), "grok-pager".into()),
                ("x-grok-client-version".into(), "0.2.93".into()),
            ]
        });
    let kilocode_base = registry()
        .oauth("kilocode")
        .and_then(|o| o.get("apiBaseUrl"))
        .and_then(Value::as_str)
        .unwrap_or("https://api.kilo.ai")
        .to_string();

    let plain = |url: Option<String>, method: &'static str, refreshable: bool| OauthTestConfig {
        url,
        method,
        auth_header: Some("Authorization"),
        auth_prefix: "Bearer ",
        extra_headers: Vec::new(),
        body: None,
        accept_statuses: Vec::new(),
        soft_fail: Vec::new(),
        refreshable,
        check_expiry: false,
        no_auth: false,
    };

    Some(match provider {
        "codex" => OauthTestConfig {
            extra_headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("originator".into(), "codex_cli_rs".into()),
                ("User-Agent".into(), format!("codex_cli_rs/{codex_version}")),
            ],
            body: Some(
                json!({ "model": "gpt-5.3-codex", "input": [], "stream": false, "store": false }),
            ),
            accept_statuses: vec![400],
            ..plain(
                Some("https://chatgpt.com/backend-api/codex/responses".into()),
                "POST",
                true,
            )
        },
        "kilocode" => plain(Some(format!("{kilocode_base}/api/profile")), "GET", false),
        "grok-cli" => {
            let mut extra = vec![("Accept".to_string(), "application/json".to_string())];
            extra.extend(grok_headers);
            OauthTestConfig {
                extra_headers: extra,
                accept_statuses: vec![402],
                soft_fail: vec![(
                    402,
                    "Connected, but Grok Build credits are exhausted (spending limit). Add credits or upgrade SuperGrok.",
                )],
                ..plain(Some(grok_user_url), "GET", true)
            }
        }
        _ => return None,
    })
}

/// `classifyOAuthProbeResult(res, config)` → `(valid, error, soft)`.
fn classify_oauth_probe(status: u16, config: &OauthTestConfig) -> (bool, Value, bool) {
    let ok = (200..300).contains(&status);
    if !ok && !config.accept_statuses.contains(&status) {
        let error = match status {
            401 => "Token invalid or revoked".to_string(),
            403 => "Access denied".to_string(),
            s => format!("API returned {s}"),
        };
        return (false, json!(error), false);
    }
    if !ok && let Some((_, message)) = config.soft_fail.iter().find(|(s, _)| *s == status) {
        return (true, json!(message), true);
    }
    (true, Value::Null, false)
}

/// `encodeURIComponent`.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `testOAuthConnection(connection, effectiveProxy)`.
async fn test_oauth_connection(
    state: &AppState,
    connection: &Value,
    proxy: &ProxyOptions,
) -> ProbeResult {
    let provider = conn_str(connection, "provider").unwrap_or("");
    let Some(config) = oauth_test_config(provider) else {
        return ProbeResult::invalid("Provider test not supported");
    };
    let Some(mut access_token) = conn_str(connection, "accessToken")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    else {
        return ProbeResult::invalid("No access token");
    };
    let refresh_token = conn_str(connection, "refreshToken").unwrap_or("");

    let mut refreshed = false;
    let mut new_tokens: Option<Value> = None;

    let credentials = credentials_from_connection(connection);
    let token_expired =
        should_refresh_credentials(provider, &credentials, router_db::time::now_ms());
    if config.refreshable && token_expired && !refresh_token.is_empty() {
        match refresh_oauth_token(state, provider, connection).await {
            Some(tokens) => {
                if let Some(token) = tokens.get("accessToken").and_then(Value::as_str) {
                    access_token = token.to_string();
                }
                refreshed = true;
                new_tokens = Some(tokens);
            }
            None => return ProbeResult::invalid("Token expired and refresh failed"),
        }
    }

    if config.check_expiry {
        if refreshed {
            return ProbeResult {
                valid: true,
                refreshed,
                new_tokens,
                ..ProbeResult::default()
            };
        }
        if token_expired {
            return ProbeResult::invalid("Token expired");
        }
        return ProbeResult::ok();
    }

    let headers = |token: &str| -> Vec<(String, String)> {
        let mut headers: Vec<(String, String)> = Vec::new();
        if !config.no_auth
            && let Some(header) = config.auth_header
        {
            headers.push((header.to_string(), format!("{}{token}", config.auth_prefix)));
        }
        for (name, value) in &config.extra_headers {
            headers.push((name.clone(), value.clone()));
        }
        headers
    };
    let send = |token: &str| {
        let url = config.url.clone().unwrap_or_default();
        let headers = headers(token);
        let body = config.body.clone();
        let method = config.method;
        async move {
            send_probe(proxy, &url, method, &headers, body.as_ref())
                .await
                .map(|r| r.status().as_u16())
        }
    };

    let Some(status) = send(&access_token).await else {
        return ProbeResult::invalid("Network error");
    };
    let (valid, error, soft) = classify_oauth_probe(status, &config);
    if valid {
        return ProbeResult {
            valid: true,
            error: if soft { error.clone() } else { Value::Null },
            warning: soft.then(|| error.as_str().unwrap_or("").to_string()),
            refreshed,
            new_tokens,
        };
    }
    if status == 401
        && config.refreshable
        && !refreshed
        && !refresh_token.is_empty()
        && let Some(tokens) = refresh_oauth_token(state, provider, connection).await
        && let Some(token) = tokens.get("accessToken").and_then(Value::as_str)
        && let Some(retry_status) = send(token).await
    {
        let (retry_valid, retry_error, retry_soft) = classify_oauth_probe(retry_status, &config);
        if retry_valid {
            return ProbeResult {
                valid: true,
                error: if retry_soft {
                    retry_error.clone()
                } else {
                    Value::Null
                },
                warning: retry_soft.then(|| retry_error.as_str().unwrap_or("").to_string()),
                refreshed: true,
                new_tokens: Some(tokens),
            };
        }
    }
    if status == 401 && config.refreshable && !refreshed && !refresh_token.is_empty() {
        return ProbeResult::invalid("Token invalid or revoked");
    }
    ProbeResult {
        valid: false,
        error,
        refreshed,
        ..ProbeResult::default()
    }
}

/// `testApiKeyConnection(connection, effectiveProxy)`.
async fn test_api_key_connection(connection: &Value, proxy: &ProxyOptions) -> ProbeResult {
    let provider = conn_str(connection, "provider").unwrap_or("");
    let api_key = conn_str(connection, "apiKey").unwrap_or("");
    let base = connection
        .get("providerSpecificData")
        .and_then(|p| p.get("baseUrl"))
        .and_then(Value::as_str)
        .unwrap_or("");

    let bearer = || vec![("Authorization".to_string(), format!("Bearer {api_key}"))];
    let status_of = |res: Option<reqwest::Response>| res.map(|r| r.status().as_u16());
    let get = |url: String, headers: Vec<(String, String)>| async move {
        status_of(send_probe(proxy, &url, "GET", &headers, None).await)
    };
    let post = |url: String, headers: Vec<(String, String)>, body: Value| async move {
        status_of(send_probe(proxy, &url, "POST", &headers, Some(&body)).await)
    };
    let ok_status = |status: Option<u16>| match status {
        Some(s) if (200..300).contains(&s) => ProbeResult::ok(),
        Some(_) => ProbeResult::invalid("Invalid API key"),
        None => ProbeResult::invalid("Network error"),
    };

    if is_openai_compatible_provider(provider) {
        if base.is_empty() {
            return ProbeResult::invalid("Missing base URL");
        }
        let url = format!("{}/models", base.trim_end_matches('/'));
        return match get(url, bearer()).await {
            Some(s) if (200..300).contains(&s) => ProbeResult::ok(),
            Some(_) => ProbeResult::invalid("Invalid API key or base URL"),
            None => ProbeResult::invalid("Network error"),
        };
    }

    if is_anthropic_compatible_provider(provider) {
        if base.is_empty() {
            return ProbeResult::invalid("Missing base URL");
        }
        let normalized = crate::routes::provider_nodes::sanitize_anthropic_base(base);
        let url = format!("{normalized}/v1/messages");
        let model = conn_str(connection, "defaultModel")
            .filter(|s| !s.is_empty())
            .unwrap_or("claude-3-haiku-20240307");
        let headers = vec![
            ("x-api-key".into(), api_key.to_string()),
            ("anthropic-version".into(), "2023-06-01".into()),
            ("content-type".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {api_key}")),
        ];
        let body = json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        });
        return match post(url, headers, body).await {
            Some(s) => {
                let valid = s != 401 && s != 403;
                ProbeResult {
                    valid,
                    error: if valid {
                        Value::Null
                    } else {
                        json!("Invalid API key or base URL")
                    },
                    ..ProbeResult::default()
                }
            }
            None => ProbeResult::invalid("Network error"),
        };
    }

    match provider {
        "deepseek" | "mistral" | "nvidia" | "tokenharbor" | "dahl" | "atria" | "agnes" | "bai" => {
            let url = registry()
                .transport(provider)
                .and_then(|t| t.validate_url.clone())
                .unwrap_or_default();
            ok_status(get(url, bearer()).await)
        }
        "openrouter" => {
            ok_status(get("https://openrouter.ai/api/v1/auth/key".into(), bearer()).await)
        }
        "opencode" => {
            let headers = vec![
                ("Authorization".into(), "Bearer public".into()),
                ("User-Agent".into(), "opencode/1.18.31".into()),
            ];
            match get("https://opencode.ai/zen/v1/models".into(), headers).await {
                Some(s) if (200..300).contains(&s) => ProbeResult::ok(),
                Some(_) => ProbeResult::invalid("OpenCode free tier unavailable"),
                None => ProbeResult::invalid("Network error"),
            }
        }
        "opencode-go" => {
            let headers = vec![
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), format!("Bearer {api_key}")),
            ];
            let body = json!({
                "model": default_model("opencode-go").unwrap_or_default(),
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
                "stream": false,
            });
            match post(
                "https://opencode.ai/zen/go/v1/chat/completions".into(),
                headers,
                body,
            )
            .await
            {
                Some(s) if s != 401 && s != 403 => ProbeResult::ok(),
                Some(_) => ProbeResult::invalid("Invalid API key"),
                None => ProbeResult::invalid("Network error"),
            }
        }
        _ => ProbeResult::invalid("Provider test not supported"),
    }
}

/// `testSingleConnection(id)`, including the DB writes.
async fn test_single_connection(state: &AppState, id: &str) -> TestOutcome {
    let connection = state
        .read({
            let id = id.to_string();
            move |conn| router_db::repos::connections::get_provider_connection_by_id(conn, &id)
        })
        .await
        .ok()
        .flatten();
    let Some(connection) = connection else {
        return TestOutcome {
            valid: false,
            error: json!("Connection not found"),
            refreshed: false,
            latency_ms: 0,
            tested_at: now_iso(),
        };
    };

    let psd = connection
        .get("providerSpecificData")
        .and_then(Value::as_object)
        .cloned();
    let resolved = resolve_connection_proxy_config(&state.db, psd.as_ref());
    let proxy = proxy_options(&resolved);

    if resolved.connection_proxy_enabled
        && !resolved.connection_proxy_url.is_empty()
        && resolved.vercel_relay_url.is_empty()
    {
        let proxy_result = test_proxy_url(Some(&resolved.connection_proxy_url), None, None).await;
        if proxy_result.get("ok") != Some(&Value::Bool(true)) {
            let proxy_error = proxy_result
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    format!(
                        "Proxy test failed with status {}",
                        proxy_result
                            .get("status")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                    )
                });
            let mut patch = Map::new();
            patch.insert("testStatus".into(), json!("error"));
            patch.insert("lastError".into(), json!(proxy_error));
            patch.insert("lastErrorAt".into(), json!(now_iso()));
            write_connection(state, id, Value::Object(patch)).await;
            return TestOutcome {
                valid: false,
                error: json!(proxy_error),
                refreshed: false,
                latency_ms: 0,
                tested_at: now_iso(),
            };
        }
    }

    let started = Instant::now();
    let auth_type = conn_str(&connection, "authType").unwrap_or("");
    let result = if auth_type == "apikey" || auth_type == "cookie" {
        test_api_key_connection(&connection, &proxy).await
    } else {
        test_oauth_connection(state, &connection, &proxy).await
    };
    let latency_ms = started.elapsed().as_millis() as i64;

    let soft_warning: Option<String> = if result.valid {
        result
            .warning
            .clone()
            .or_else(|| result.error.as_str().map(str::to_string))
    } else {
        None
    };
    let mut update = Map::new();
    update.insert(
        "testStatus".into(),
        json!(if result.valid { "active" } else { "error" }),
    );
    update.insert(
        "lastError".into(),
        if result.valid {
            soft_warning
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null)
        } else {
            result.error.clone()
        },
    );
    update.insert(
        "lastErrorAt".into(),
        if result.valid && soft_warning.is_none() {
            Value::Null
        } else {
            json!(now_iso())
        },
    );

    if result.refreshed
        && let Some(tokens) = &result.new_tokens
    {
        for key in ["accessToken", "refreshToken", "idToken", "lastRefreshAt"] {
            if let Some(value) = tokens.get(key).filter(|v| js_truthy(v)) {
                update.insert(key.to_string(), value.clone());
            }
        }
        if let Some(expires_in) = tokens
            .get("expiresIn")
            .filter(|v| js_truthy(v))
            .and_then(Value::as_i64)
        {
            update.insert("expiresIn".into(), json!(expires_in));
            update.insert(
                "expiresAt".into(),
                json!(to_expires_at(expires_in, router_db::time::now_ms())),
            );
        } else if let Some(expires_at) = tokens.get("expiresAt").filter(|v| js_truthy(v)) {
            update.insert("expiresAt".into(), expires_at.clone());
        }
        if let Some(tokens_psd) = tokens
            .get("providerSpecificData")
            .and_then(Value::as_object)
        {
            let mut merged = connection
                .get("providerSpecificData")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (k, v) in tokens_psd {
                merged.insert(k.clone(), v.clone());
            }
            update.insert("providerSpecificData".into(), Value::Object(merged));
        }
    }

    write_connection(state, id, Value::Object(update)).await;

    TestOutcome {
        valid: result.valid,
        error: result.error,
        refreshed: result.refreshed,
        latency_ms,
        tested_at: now_iso(),
    }
}

/// `updateProviderConnection(id, patch)`.
async fn write_connection(state: &AppState, id: &str, patch: Value) {
    let id = id.to_string();
    let _ = state
        .write(move |tx| router_db::repos::connections::update_provider_connection(tx, &id, &patch))
        .await;
}

/// `POST /api/providers/{id}/test`.
pub async fn test(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let result = test_single_connection(&state, &id).await;
    if result.error == json!("Connection not found") {
        return ApiError::not_found("Connection not found").into_response();
    }
    Json(json!({
        "valid": result.valid,
        "error": result.error,
        "refreshed": result.refreshed,
    }))
    .into_response()
}

// ─── POST /api/providers/validate ─────────────────────────────────────────

/// An 8-second probe client.
fn probe_client_8s() -> Option<reqwest::Client> {
    tls_builder().timeout(Duration::from_secs(8)).build().ok()
}

/// One plain probe (no connection proxy) → the response status.
async fn plain_probe(
    url: &str,
    method: &str,
    headers: &[(String, String)],
    body: Option<Value>,
) -> Option<u16> {
    let client = probe_client_8s()?;
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut builder = client.request(method, url);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    if let Some(body) = body {
        builder = builder.json(&body);
    }
    builder.send().await.ok().map(|r| r.status().as_u16())
}

/// `probeWebProvider(provider, apiKey)` → `Some(valid)`, or `None` to skip.
async fn probe_web_provider(provider: &str, api_key: &str) -> Option<bool> {
    let entry = registry().get(provider)?;
    let kinds: Vec<&str> = entry
        .extra
        .get("serviceKinds")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_else(|| vec!["llm"]);
    if !kinds.iter().all(|k| *k == "webSearch" || *k == "webFetch") {
        return None;
    }
    let cfg = entry
        .extra
        .get("searchConfig")
        .or_else(|| entry.extra.get("fetchConfig"))?;
    if cfg.get("authType").and_then(Value::as_str) == Some("none") {
        return Some(true);
    }

    let mut url = cfg
        .get("validateUrl")
        .or_else(|| cfg.get("baseUrl"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut headers: Vec<(String, String)> =
        vec![("Content-Type".into(), "application/json".into())];
    match cfg.get("authHeader").and_then(Value::as_str) {
        Some("bearer") => headers.push(("Authorization".into(), format!("Bearer {api_key}"))),
        Some("x-api-key") => headers.push(("x-api-key".into(), api_key.to_string())),
        Some("x-subscription-token") => {
            headers.push(("x-subscription-token".into(), api_key.to_string()))
        }
        Some("key") => url.push_str(&format!("?key={}&q=ping&cx=test", urlencode(api_key))),
        Some("api_key") => url.push_str(&format!(
            "?api_key={}&q=ping&engine=google",
            urlencode(api_key)
        )),
        _ => {}
    }

    let method = cfg.get("method").and_then(Value::as_str).unwrap_or("GET");
    let body = (method == "POST")
        .then(|| json!({ "query": "ping", "q": "ping", "url": "https://example.com" }));
    Some(match plain_probe(&url, method, &headers, body).await {
        Some(status) => status != 401 && status != 403,
        None => false,
    })
}

/// `probeMediaProvider(provider, apiKey)` → `Some(valid)`, or `None` to skip.
async fn probe_media_provider(provider: &str, api_key: &str) -> Option<bool> {
    let entry = registry().get(provider)?;
    const MEDIA_KINDS: [&str; 1] = ["embedding"];
    let kinds: Vec<&str> = entry
        .extra
        .get("serviceKinds")
        .and_then(Value::as_array)
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_else(|| vec!["llm"]);
    if !kinds.iter().all(|k| MEDIA_KINDS.contains(k)) {
        return None;
    }
    let cfg = ["embeddingConfig"].iter().find_map(|k| entry.extra.get(*k));
    let Some(cfg) = cfg else {
        return Some(true);
    };
    if entry.no_auth == Some(true) || cfg.get("authType").and_then(Value::as_str) == Some("none") {
        return Some(true);
    }
    if matches!(
        cfg.get("authHeader").and_then(Value::as_str),
        Some("playht") | Some("aws-sigv4")
    ) {
        return Some(true);
    }

    let mut headers: Vec<(String, String)> =
        vec![("Content-Type".into(), "application/json".into())];
    if let Some(extra) = cfg.get("extraHeaders").and_then(Value::as_object) {
        for (k, v) in extra {
            if let Some(value) = v.as_str() {
                headers.push((k.clone(), value.to_string()));
            }
        }
    }
    match cfg.get("authHeader").and_then(Value::as_str) {
        Some("bearer") => headers.push(("Authorization".into(), format!("Bearer {api_key}"))),
        Some("key") => headers.push(("Authorization".into(), format!("Key {api_key}"))),
        Some("x-api-key") => headers.push(("x-api-key".into(), api_key.to_string())),
        Some("x-key") => headers.push(("x-key".into(), api_key.to_string())),
        Some("xi-api-key") => headers.push(("xi-api-key".into(), api_key.to_string())),
        Some("token") => headers.push(("Authorization".into(), format!("Token {api_key}"))),
        Some("basic") => headers.push(("Authorization".into(), format!("Basic {api_key}"))),
        _ => return None,
    }

    let method = cfg.get("method").and_then(Value::as_str).unwrap_or("POST");
    let base_url = cfg.get("baseUrl").and_then(Value::as_str).unwrap_or("");
    let body = (method != "GET").then(|| {
        json!({
            "input": "ping",
            "text": "ping",
            "prompt": "ping",
            "model": default_model(provider).unwrap_or_else(|| "test".into()),
        })
    });
    Some(match plain_probe(base_url, method, &headers, body).await {
        Some(status) => status != 401 && status != 403,
        None => false,
    })
}

/// `getProviderNodeById(provider)` for the compatible branches.
async fn get_node(state: &AppState, id: &str) -> Option<Value> {
    let id = id.to_string();
    state
        .read(move |conn| router_db::repos::nodes::get_provider_node_by_id(conn, &id))
        .await
        .ok()
        .flatten()
}

/// `POST /api/providers/validate`.
pub async fn validate(
    State(state): State<AppState>,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Validation failed").into_response();
    };
    let Some(body) = payload.as_object() else {
        return ApiError::internal("Validation failed").into_response();
    };

    let provider_value = normalize_provider_id(body.get("provider").unwrap_or(&Value::Null));
    let provider = provider_value.as_str().unwrap_or("");
    let api_key = conn_str(&payload, "apiKey").unwrap_or("");
    let is_no_auth = provider_no_auth(provider);
    if provider.is_empty() || (api_key.is_empty() && !is_no_auth) {
        return ApiError::bad_request("Provider and API key required").into_response();
    }

    let valid_body = |valid: bool| json!({ "valid": valid, "error": if valid { Value::Null } else { json!("Invalid API key") } });
    let bearer = vec![("Authorization".to_string(), format!("Bearer {api_key}"))];

    if is_openai_compatible_provider(provider) {
        let Some(node) = get_node(&state, provider).await else {
            return ApiError::not_found("OpenAI Compatible node not found").into_response();
        };
        let base = node.get("baseUrl").and_then(Value::as_str).unwrap_or("");
        let url = format!("{}/models", base.trim_end_matches('/'));
        let valid = matches!(plain_probe(&url, "GET", &bearer, None).await, Some(s) if (200..300).contains(&s));
        return Json(valid_body(valid)).into_response();
    }

    if is_custom_embedding_provider(provider) {
        let Some(node) = get_node(&state, provider).await else {
            return ApiError::not_found("Custom Embedding node not found").into_response();
        };
        let base = node
            .get("baseUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches('/')
            .to_string();
        if let Some(status) = plain_probe(&format!("{base}/models"), "GET", &bearer, None).await {
            if (200..300).contains(&status) {
                return Json(json!({ "valid": true })).into_response();
            }
            if status == 401 || status == 403 {
                return Json(json!({ "valid": false, "error": "Invalid API key" })).into_response();
            }
        }
        let embed_headers = vec![
            ("Authorization".to_string(), format!("Bearer {api_key}")),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        let embed_body = json!({ "model": "test", "input": "ping" });
        let valid = matches!(
            plain_probe(&format!("{base}/embeddings"), "POST", &embed_headers, Some(embed_body)).await,
            Some(status) if status != 401 && status != 403
        );
        return Json(valid_body(valid)).into_response();
    }

    if is_anthropic_compatible_provider(provider) {
        let Some(node) = get_node(&state, provider).await else {
            return ApiError::not_found("Anthropic Compatible node not found").into_response();
        };
        let raw = node.get("baseUrl").and_then(Value::as_str).unwrap_or("");
        let normalized = crate::routes::provider_nodes::sanitize_anthropic_base(raw);
        let url = format!("{normalized}/v1/messages");
        let model = node
            .get("defaultModel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("claude-3-haiku-20240307");
        let headers = vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("Authorization".to_string(), format!("Bearer {api_key}")),
        ];
        let body = json!({
            "model": model,
            "max_tokens": 1,
            "messages": [{ "role": "user", "content": "test" }],
        });
        let valid = matches!(
            plain_probe(&url, "POST", &headers, Some(body)).await,
            Some(status) if status != 401 && status != 403
        );
        return Json(valid_body(valid)).into_response();
    }

    if let Some(web_result) = probe_web_provider(provider, api_key).await {
        return Json(valid_body(web_result)).into_response();
    }

    if let Some(media_result) = probe_media_provider(provider, api_key).await {
        return Json(valid_body(media_result)).into_response();
    }

    match provider {
        "openrouter" => {
            let valid = matches!(
                plain_probe("https://openrouter.ai/api/v1/models", "GET", &bearer, None).await,
                Some(s) if (200..300).contains(&s)
            );
            Json(valid_body(valid)).into_response()
        }
        "deepseek" | "mistral" | "nvidia" => {
            let url = registry()
                .transport(provider)
                .and_then(|t| t.validate_url.clone())
                .unwrap_or_default();
            let headers = if api_key.is_empty() {
                Vec::new()
            } else {
                bearer.clone()
            };
            let valid = matches!(
                plain_probe(&url, "GET", &headers, None).await,
                Some(s) if (200..300).contains(&s)
            );
            Json(valid_body(valid)).into_response()
        }
        "opencode-go" => {
            let headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), format!("Bearer {api_key}")),
            ];
            let body = json!({
                "model": default_model("opencode-go").unwrap_or_default(),
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
                "stream": false,
            });
            let valid = matches!(
                plain_probe("https://opencode.ai/zen/go/v1/chat/completions", "POST", &headers, Some(body)).await,
                Some(s) if s != 401 && s != 403
            );
            Json(valid_body(valid)).into_response()
        }
        "commandcode" => {
            let Some(transport) = registry().transport("commandcode") else {
                return ApiError::bad_request("Provider validation not supported").into_response();
            };
            let base_url = transport.base_url.clone().unwrap_or_default();
            let credentials = router_sse::credentials::Credentials::default();
            let mut meta = router_sse::translator::RequestMeta::default();
            let model = default_model("commandcode").unwrap_or_default();
            let payload = router_sse::translator::request::openai_to_commandcode::openai_to_commandcode_request(
                &mut router_sse::translator::RequestContext {
                    model: &model,
                    stream: false,
                    credentials: &credentials,
                    meta: &mut meta,
                },
                json!({
                    "messages": [{ "role": "user", "content": "ping" }],
                    "max_tokens": 1,
                    "stream": false,
                }),
            );
            let mut headers = vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("x-session-id".to_string(), uuid::Uuid::new_v4().to_string()),
                ("Authorization".to_string(), format!("Bearer {api_key}")),
            ];
            if let Some(extra) = transport.headers.as_ref() {
                for (k, v) in extra {
                    if let Some(value) = v.as_str() {
                        headers.push((k.clone(), value.to_string()));
                    }
                }
            }
            let valid = matches!(
                plain_probe(&base_url, "POST", &headers, Some(payload)).await,
                Some(s) if s != 401 && s != 403
            );
            Json(valid_body(valid)).into_response()
        }
        _ => {
            // Generic probe for OpenAI-format providers (config-driven from the
            // registry), the default branch.
            let Some(transport) = registry().transport(provider) else {
                return ApiError::bad_request("Provider validation not supported").into_response();
            };
            let Some(base_url) = transport.base_url.clone() else {
                return ApiError::bad_request("Provider validation not supported").into_response();
            };
            if transport.format_or_default() != "openai" {
                return ApiError::bad_request("Provider validation not supported").into_response();
            }
            if transport.no_auth == Some(true) {
                return Json(json!({ "valid": true, "error": Value::Null })).into_response();
            }
            let mut headers: Vec<(String, String)> =
                vec![("Content-Type".to_string(), "application/json".to_string())];
            if let Some(extra) = transport.headers.as_ref() {
                for (k, v) in extra {
                    if let Some(value) = v.as_str() {
                        headers.push((k.clone(), value.to_string()));
                    }
                }
            }
            let auth_header = transport
                .auth
                .as_ref()
                .and_then(|a| a.get("header"))
                .and_then(Value::as_str);
            if auth_header == Some("x-api-key") {
                headers.push(("X-API-Key".to_string(), api_key.to_string()));
            } else {
                headers.push(("Authorization".to_string(), format!("Bearer {api_key}")));
            }
            let models_base = base_url
                .trim_end_matches("/chat/completions")
                .trim_end_matches("/chatbot")
                .to_string();
            if let Some(status) =
                plain_probe(&format!("{models_base}/models"), "GET", &headers, None).await
            {
                if status == 401 || status == 403 {
                    return Json(valid_body(false)).into_response();
                }
                if (200..300).contains(&status) {
                    return Json(json!({ "valid": true, "error": Value::Null })).into_response();
                }
            }
            let model = default_model(provider).unwrap_or_else(|| "test".into());
            let body = json!({ "model": model, "messages": [{ "role": "user", "content": "ping" }], "max_tokens": 1 });
            let valid = matches!(
                plain_probe(&base_url, "POST", &headers, Some(body)).await,
                Some(s) if s != 401 && s != 403
            );
            Json(valid_body(valid)).into_response()
        }
    }
}

/// `new Date().toISOString()`.
fn now_iso() -> String {
    to_iso(router_db::time::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(provider: &str) -> OauthTestConfig {
        oauth_test_config(provider).expect("kept provider has a test config")
    }

    #[test]
    fn classify_matches_the_reference_branches() {
        // Hard failure: 401 and 403 map to their own strings.
        assert_eq!(
            classify_oauth_probe(401, &config("kilocode")),
            (false, json!("Token invalid or revoked"), false)
        );
        assert_eq!(
            classify_oauth_probe(403, &config("kilocode")),
            (false, json!("Access denied"), false)
        );
        assert_eq!(
            classify_oauth_probe(500, &config("kilocode")),
            (false, json!("API returned 500"), false)
        );

        // Codex 400 is a silent success: auth proved, no warning.
        assert_eq!(
            classify_oauth_probe(400, &config("codex")),
            (true, Value::Null, false)
        );
        assert_eq!(
            classify_oauth_probe(200, &config("codex")),
            (true, Value::Null, false)
        );

        // Grok CLI 402 is a soft success carrying the spending-limit warning.
        let (valid, error, soft) = classify_oauth_probe(402, &config("grok-cli"));
        assert!(valid && soft);
        assert!(
            error
                .as_str()
                .unwrap_or("")
                .contains("credits are exhausted")
        );
    }

    #[test]
    fn unknown_providers_have_no_test_config() {
        for provider in ["not-a-provider", "example-unknown", "no-such-oauth"] {
            assert!(
                oauth_test_config(provider).is_none(),
                "{provider} has no oauth test config"
            );
        }
        for provider in ["codex", "grok-cli", "kilocode"] {
            assert!(
                oauth_test_config(provider).is_some(),
                "{provider} must be kept"
            );
        }
    }

    #[test]
    fn urlencode_matches_encode_uri_component() {
        assert_eq!(urlencode("abc-_.!~*'()"), "abc-_.!~*'()");
        assert_eq!(urlencode("a b/c?d=e"), "a%20b%2Fc%3Fd%3De");
    }
}
