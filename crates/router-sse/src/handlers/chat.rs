//! The `/v1` chat entry point.
//!
//! Resolves the client model to a provider/model pair, runs the combo and
//! capacity-adapter paths, then loops over the provider's accounts calling
//! [`handle_chat_core`]. The account loop is the part that must not be
//! skipped: a provider with three connections retries the next one after a
//! rate limit, and the cooldown bookkeeping that makes that safe lives in
//! `services::auth`.
//!
//! `pxpipe` is dropped, so the settings read here does not carry its fields.
//! Request bodies are not persisted.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use router_db::Db;

use crate::executors::http::ProxyOptions;
use crate::handlers::chat_core::streaming::SaveUsageFn;
use crate::handlers::chat_core::{
    ChatBody, ChatCoreRequest, ChatLog, ChatResult, CredentialsRefreshedFn, RequestSuccessFn,
    handle_chat_core,
};
use crate::runtime_config::http_status;
use crate::services::auth::{
    AccountSelection, SelectedAccount, clear_account_error, get_provider_credentials,
    is_valid_api_key, mark_account_unavailable,
};
use crate::services::capacity_adapter::{
    augment_models_with_capacity_adapter, get_active_adapter_strategy, strip_for_adapter_model,
};
use crate::services::combo::{
    ComboAttempt, ComboOutcome, FusionOutcome, FusionTuning, detect_required_capabilities,
    handle_combo_chat, handle_fusion_chat,
};
use crate::services::model::{ModelInfo, get_combo_models, get_model_info};
use crate::services::token_refresh::{check_and_refresh_token, update_provider_credentials};
use crate::translator::concerns::primitives::js_truthy;
use crate::translator::formats::detect_format_by_endpoint;
use crate::utils::bypass_handler::handle_bypass_request;
use crate::utils::chat_log::{TracingChatLog, mask_key};
use crate::utils::error::unavailable_response;
use crate::utils::model_markers::strip_model_context_marker;
use crate::utils::stream::StreamHooks;

/// `trackPendingRequest` / `appendRequestLog` from the chat pipeline.
///
/// `appendRequestLog` is a no-op, so only the pending counter is real.
struct DbHooks;

impl StreamHooks for DbHooks {
    fn track_pending_request(
        &self,
        model: Option<&str>,
        provider: Option<&str>,
        connection_id: Option<&str>,
        pending: bool,
        error: bool,
    ) {
        if let Some(model) = model {
            router_db::stats::track_pending_request(
                model,
                provider,
                connection_id,
                pending,
                error,
                router_db::time::now_ms(),
            );
            // `trackPendingRequest` ends on `scheduleStatsEvent("pending")`.
            crate::services::stats_emitter::emit_pending();
        }
    }

    fn append_request_log(&self, _entry: Value) {}
}

/// The transport-owned part of the request: the body, the client headers and
/// the endpoint the client posted to.
pub struct ChatRequest<'a> {
    pub body: Value,
    /// Lower-cased client headers.
    pub headers: &'a HashMap<String, String>,
    /// The path the client posted to, for endpoint-based format detection.
    pub endpoint: Option<&'a str>,
    pub api_key: Option<String>,
}

/// The chat result a successful combo attempt produced. The combo loop only
/// carries an index, so the body rides out through this slot.
type ResultSlot = Arc<Mutex<Option<ChatResult>>>;

/// `handleChat(request, clientRawRequest)`.
pub async fn handle_chat(db: &Db, request: ChatRequest<'_>) -> ChatResult {
    let mut body = request.body;
    let headers = request.headers;
    let endpoint = request.endpoint;
    let api_key = request.api_key;
    let log: Arc<dyn ChatLog> = Arc::new(TracingChatLog);

    let stripped =
        strip_model_context_marker(body.get("model").and_then(Value::as_str).unwrap_or(""));
    let model_str = stripped.model.to_string();
    if stripped.context_marker.is_some()
        && let Some(obj) = body.as_object_mut()
    {
        obj.insert("model".into(), json!(model_str));
    }

    if let Some(key) = api_key.as_deref().filter(|k| !k.is_empty()) {
        log.debug("AUTH", &format!("API Key: {}", mask_key(key)));
    } else {
        log.debug("AUTH", "No API key provided (local mode)");
    }

    // A failed settings read must not read as `requireApiKey: false`; that would
    // open the gate whenever the database is unhealthy.
    let settings = match db.with_conn(router_db::repos::settings::get_settings) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::error!(%error, "chat: settings read failed");
            return ChatResult::error(http_status::SERVICE_UNAVAILABLE, "Settings unavailable");
        }
    };

    if js_truthy(settings.get("requireApiKey").unwrap_or(&Value::Null)) {
        let Some(key) = api_key.as_deref().filter(|k| !k.is_empty()) else {
            return ChatResult::error(http_status::UNAUTHORIZED, "Missing API key");
        };
        if !is_valid_api_key(db, key) {
            return ChatResult::error(http_status::UNAUTHORIZED, "Invalid API key");
        }
    }

    if model_str.is_empty() {
        return ChatResult::error(http_status::BAD_REQUEST, "Missing model");
    }

    let user_agent = headers.get("user-agent").map(String::as_str).unwrap_or("");
    let cc_filter_naming = settings
        .get("ccFilterNaming")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(bypass) = handle_bypass_request(&body, &model_str, user_agent, cc_filter_naming) {
        return ChatResult {
            status: 200,
            headers: vec![
                ("Content-Type".to_string(), bypass.content_type.to_string()),
                ("Access-Control-Allow-Origin".to_string(), "*".to_string()),
            ],
            body: ChatBody::Bytes(bypass.bytes()),
            log: None,
            usage_stats: None,
            error: None,
            resets_at_ms: None,
        };
    }

    let required = detect_required_capabilities(&body);
    let combos = load_combos(db);
    let combo_lookup = |name: &str| -> Option<Value> { lookup_combo(&combos, name) };

    if let Some(combo_models) = get_combo_models(&model_str, combo_lookup) {
        let strategy = combo_strategy(&settings, &model_str);
        let augmented = augment_models_with_capacity_adapter(&combo_models, &required, &settings);
        let adapter_added: HashSet<String> = augmented
            .iter()
            .filter(|m| !combo_models.contains(m))
            .cloned()
            .collect();

        if strategy == "fusion" {
            return run_fusion(
                db,
                body,
                combo_models,
                &settings,
                &model_str,
                headers,
                endpoint,
                api_key,
            )
            .await;
        }

        let sticky = settings
            .get("comboStickyRoundRobinLimit")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let slot: ResultSlot = Arc::new(Mutex::new(None));
        let outcome = handle_combo_chat(
            &body,
            &augmented,
            Some(&model_str),
            strategy,
            sticky,
            true,
            combo_closure(
                db,
                &body,
                &adapter_added,
                headers,
                endpoint,
                api_key.clone(),
                Arc::clone(&slot),
            ),
        )
        .await;
        return combo_result(outcome, &slot);
    }

    let solo = augment_models_with_capacity_adapter(
        std::slice::from_ref(&model_str),
        &required,
        &settings,
    );
    if solo.len() > 1 {
        let adapter_added: HashSet<String> =
            solo.iter().filter(|m| **m != model_str).cloned().collect();
        let strategy = get_active_adapter_strategy(&required, &settings);
        let slot: ResultSlot = Arc::new(Mutex::new(None));
        let outcome = handle_combo_chat(
            &body,
            &solo,
            Some(&model_str),
            strategy,
            1,
            true,
            combo_closure(
                db,
                &body,
                &adapter_added,
                headers,
                endpoint,
                api_key.clone(),
                Arc::clone(&slot),
            ),
        )
        .await;
        return combo_result(outcome, &slot);
    }

    handle_single_model_chat(db, body, &model_str, headers, endpoint, api_key).await
}

/// The `withCapacityAdapterStripping((b, m) => handleSingleModelChat(...))`
/// callback, storing the winning result so the combo loop can hand it back.
fn combo_closure<'a>(
    db: &'a Db,
    body: &'a Value,
    adapter_added: &'a HashSet<String>,
    headers: &'a HashMap<String, String>,
    endpoint: Option<&'a str>,
    api_key: Option<String>,
    slot: ResultSlot,
) -> impl FnMut(
    usize,
    &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ComboAttempt> + Send + 'a>> {
    move |_index, model| {
        let model = model.to_string();
        let mut attempt_body = body.clone();
        if let Some(trimmed) = strip_for_adapter_model(&attempt_body, &model, adapter_added) {
            attempt_body = trimmed;
        }
        let api_key = api_key.clone();
        let slot = Arc::clone(&slot);
        Box::pin(async move {
            let result =
                handle_single_model_chat(db, attempt_body, &model, headers, endpoint, api_key)
                    .await;
            if result.success() {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                ComboAttempt::ok()
            } else {
                ComboAttempt::failed(
                    result.status,
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| result.status.to_string()),
                )
            }
        })
    }
}

/// Map a combo outcome onto the response.
fn combo_result(outcome: ComboOutcome, slot: &ResultSlot) -> ChatResult {
    match outcome {
        ComboOutcome::Success { .. } => slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| ChatResult::error(500, "combo produced no response")),
        ComboOutcome::AllFailed {
            status,
            message,
            retry_after,
            retry_after_human,
        } => match retry_after {
            Some(retry_after) => {
                let (code, body, retry_sec) =
                    unavailable_response(status, &message, &retry_after, &retry_after_human);
                let mut result = ChatResult::json(code, &body);
                result.error = Some(message);
                result.headers.push(("retry-after".to_string(), retry_sec));
                result
            }
            None => ChatResult::error(status, &message),
        },
    }
}

/// `handleSingleModelChat(body, modelStr, clientRawRequest, request, apiKey)`.
///
/// Boxed because it and [`run_fusion`] call each other: an unboxed cycle makes
/// the compiler's type computation diverge (`E0391`), so the box erases one
/// edge of the cycle.
pub fn handle_single_model_chat<'a>(
    db: &'a Db,
    body: Value,
    model_str: &'a str,
    headers: &'a HashMap<String, String>,
    endpoint: Option<&'a str>,
    api_key: Option<String>,
) -> Pin<Box<dyn Future<Output = ChatResult> + Send + 'a>> {
    Box::pin(handle_single_model_chat_inner(
        db, body, model_str, headers, endpoint, api_key,
    ))
}

async fn handle_single_model_chat_inner(
    db: &Db,
    body: Value,
    model_str: &str,
    headers: &HashMap<String, String>,
    endpoint: Option<&str>,
    api_key: Option<String>,
) -> ChatResult {
    let log: Arc<dyn ChatLog> = Arc::new(TracingChatLog);
    let aliases = db
        .with_conn(router_db::repos::aliases::get_model_aliases)
        .ok()
        .and_then(|v| v.as_object().cloned());
    let nodes = db
        .with_conn(|conn| router_db::repos::nodes::get_provider_nodes(conn, None))
        .unwrap_or_default();
    let combos = load_combos(db);
    let combo_lookup = |name: &str| -> Option<Value> { lookup_combo(&combos, name) };

    let info: ModelInfo = get_model_info(model_str, aliases.as_ref(), combo_lookup, &nodes);

    if info.provider.is_empty() {
        // A combo name that reached the single-model path: re-check, since the
        // caller may have routed here directly.
        if let Some(combo_models) = get_combo_models(model_str, combo_lookup) {
            let settings = db
                .with_conn(router_db::repos::settings::get_settings)
                .unwrap_or(Value::Null);
            let strategy = combo_strategy(&settings, model_str);
            if strategy == "fusion" {
                return run_fusion(
                    db,
                    body,
                    combo_models,
                    &settings,
                    model_str,
                    headers,
                    endpoint,
                    api_key,
                )
                .await;
            }
            let required = detect_required_capabilities(&body);
            let augmented =
                augment_models_with_capacity_adapter(&combo_models, &required, &settings);
            let sticky = settings
                .get("comboStickyRoundRobinLimit")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let adapter_added: HashSet<String> = augmented
                .iter()
                .filter(|m| !combo_models.contains(m))
                .cloned()
                .collect();
            let slot: ResultSlot = Arc::new(Mutex::new(None));
            let outcome = handle_combo_chat(
                &body,
                &augmented,
                Some(model_str),
                strategy,
                sticky,
                true,
                combo_closure(
                    db,
                    &body,
                    &adapter_added,
                    headers,
                    endpoint,
                    api_key,
                    Arc::clone(&slot),
                ),
            )
            .await;
            return combo_result(outcome, &slot);
        }
        return ChatResult::error(http_status::BAD_REQUEST, "Invalid model format");
    }

    let provider = info.provider.clone();
    let model = info.model.clone();
    let user_agent = headers.get("user-agent").map(String::as_str).unwrap_or("");

    let mut exclude: HashSet<String> = HashSet::new();
    let mut last_error: Option<String> = None;
    let mut last_status: Option<u16> = None;
    let mut last_headers: Vec<(String, String)> = Vec::new();

    loop {
        let selection =
            get_provider_credentials(db, &provider, Some(&exclude), Some(&model), None).await;

        let selected: SelectedAccount = match selection {
            AccountSelection::Selected(account) => *account,
            AccountSelection::AllRateLimited(limited) => {
                let message = last_error
                    .clone()
                    .or_else(|| limited.last_error.clone())
                    .unwrap_or_else(|| "Unavailable".to_string());
                let (code, body, retry_sec) = unavailable_response(
                    http_status::SERVICE_UNAVAILABLE,
                    &format!("[{provider}/{model}] {message}"),
                    &limited.retry_after,
                    &limited.retry_after_human,
                );
                let mut result = ChatResult::json(code, &body);
                result.error = Some(message);
                result.headers.extend(last_headers);
                result.headers.push(("retry-after".to_string(), retry_sec));
                return result;
            }
            AccountSelection::None => {
                if exclude.is_empty() {
                    return ChatResult::error(
                        http_status::NOT_FOUND,
                        &format!("No active credentials for provider: {provider}"),
                    );
                }
                let mut result = ChatResult::error(
                    last_status.unwrap_or(http_status::SERVICE_UNAVAILABLE),
                    &last_error.unwrap_or_else(|| "All accounts unavailable".to_string()),
                );
                result.headers.extend(last_headers);
                return result;
            }
        };

        let connection_id = selected
            .credentials
            .connection_id
            .clone()
            .unwrap_or_default();
        let proxy_options = ProxyOptions::from_value(Some(&Value::Object(
            selected.credentials.provider_specific_data.clone(),
        )));

        let refreshed =
            check_and_refresh_token(&provider, &selected.credentials, &proxy_options, false).await;
        if let Some(patch) = &refreshed.patch {
            update_provider_credentials(db, &connection_id, patch);
        }

        let settings = db
            .with_conn(router_db::repos::settings::get_settings)
            .unwrap_or(Value::Null);
        let provider_thinking = settings
            .get("providerThinking")
            .and_then(|p| p.get(&provider))
            .cloned();

        let source_format_override =
            endpoint.and_then(|e| detect_format_by_endpoint(e, Some(&body)));

        let mut core_body = body.clone();
        if let Some(obj) = core_body.as_object_mut() {
            obj.insert("model".into(), json!(format!("{provider}/{model}")));
        }

        let psd = selected.credentials.provider_specific_data.clone();
        let refreshed_connection_id = connection_id.clone();
        // The callbacks outlive this loop iteration, so they own their `Db`.
        let db_for_refresh = db.clone();
        let on_credentials_refreshed: CredentialsRefreshedFn = Arc::new(move |mut patch: Value| {
            if let Some(obj) = patch.as_object_mut() {
                obj.insert(
                    "existingProviderSpecificData".into(),
                    Value::Object(psd.clone()),
                );
                obj.insert("testStatus".into(), json!("active"));
            }
            update_provider_credentials(&db_for_refresh, &refreshed_connection_id, &patch);
        });

        let connection_row = selected.connection.clone();
        let success_connection_id = connection_id.clone();
        let success_model = model.clone();
        let db_for_success = db.clone();
        let on_request_success: RequestSuccessFn = Arc::new(move || {
            clear_account_error(
                &db_for_success,
                &success_connection_id,
                &connection_row,
                Some(success_model.as_str()),
            );
        });

        let mut credentials = refreshed.credentials;
        credentials.connection_name = Some(selected.connection_name.clone());

        let save_usage = build_save_usage(db);

        let result = handle_chat_core(ChatCoreRequest {
            body: core_body,
            provider: &provider,
            model: &model,
            credentials: &mut credentials,
            log: Some(Arc::clone(&log)),
            hooks: Some(Arc::new(DbHooks)),
            client_headers: headers,
            client_body: Some(&body),
            client_endpoint: endpoint,
            connection_id: Some(&connection_id),
            user_agent: Some(user_agent),
            api_key: api_key.as_deref(),
            cc_filter_naming: settings
                .get("ccFilterNaming")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            rtk_enabled: settings
                .get("rtkEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            caveman_enabled: settings
                .get("cavemanEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            caveman_level: Some(
                settings
                    .get("cavemanLevel")
                    .and_then(Value::as_str)
                    .unwrap_or("full"),
            ),
            ponytail_enabled: settings
                .get("ponytailEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            ponytail_level: Some(
                settings
                    .get("ponytailLevel")
                    .and_then(Value::as_str)
                    .unwrap_or("full"),
            ),
            source_format_override,
            provider_thinking: provider_thinking.as_ref(),
            proxy_options: proxy_options.clone(),
            cancel: None,
            on_credentials_refreshed: Some(on_credentials_refreshed),
            on_request_success: Some(on_request_success),
            save_usage,
        })
        .await;

        if result.success() {
            return result;
        }

        let resets_at_ms = result.resets_at_ms;

        let relay_edge_error = is_relay_edge_error(&proxy_options, result.status);

        let should_fallback = if relay_edge_error {
            false
        } else {
            mark_account_unavailable(
                db,
                &connection_id,
                result.status,
                result.error.as_deref().unwrap_or(""),
                Some(provider.as_str()),
                Some(model.as_str()),
                resets_at_ms.map(|ms| ms as i64),
            )
            .should_fallback
        };

        if should_fallback {
            exclude.insert(connection_id);
            last_error = result.error;
            last_status = Some(result.status);
            last_headers = result.headers;
            continue;
        }

        return result;
    }
}

/// `handleFusionChat(...)`: run the panel non-streaming, then the judge.
#[allow(clippy::too_many_arguments)]
async fn run_fusion(
    db: &Db,
    body: Value,
    combo_models: Vec<String>,
    settings: &Value,
    combo_name: &str,
    headers: &HashMap<String, String>,
    endpoint: Option<&str>,
    api_key: Option<String>,
) -> ChatResult {
    let combo_strategies = settings.get("comboStrategies");
    let judge_model = combo_strategies
        .and_then(|c| c.get(combo_name))
        .and_then(|c| c.get("judgeModel"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let tuning = combo_strategies
        .and_then(|c| c.get(combo_name))
        .and_then(|c| c.get("fusionTuning"))
        .and_then(parse_fusion_tuning);

    let panel_body = body.clone();
    let panel_headers = headers.clone();
    let panel_api_key = api_key.clone();
    // The fusion callback is spawned per panel model, so it must own its
    // captures: `db` is `Arc`-backed and cheap to clone, the endpoint becomes a
    // `String`.
    let panel_db = db.clone();
    let panel_endpoint = endpoint.map(str::to_string);
    let outcome = handle_fusion_chat(
        &body,
        &combo_models,
        judge_model.as_deref(),
        tuning,
        move |panel_body: Value, model: String, _is_panel: bool| {
            let headers = panel_headers.clone();
            let api_key = panel_api_key.clone();
            let db = panel_db.clone();
            let endpoint = panel_endpoint.clone();
            async move {
                let result = handle_single_model_chat(
                    &db,
                    panel_body,
                    &model,
                    &headers,
                    endpoint.as_deref(),
                    api_key,
                )
                .await;
                if !result.success() {
                    return Err(ComboAttempt::failed(
                        result.status,
                        result.error.clone().unwrap_or_default(),
                    ));
                }
                match result.body {
                    ChatBody::Json(text) => serde_json::from_str::<Value>(&text)
                        .map_err(|_| ComboAttempt::failed(result.status, "unparseable panel body")),
                    _ => Err(ComboAttempt::failed(result.status, "panel body not JSON")),
                }
            }
        },
    )
    .await;

    match outcome {
        FusionOutcome::Judge { body, model } => {
            handle_single_model_chat(db, body, &model, headers, endpoint, api_key).await
        }
        FusionOutcome::Direct { model } => {
            handle_single_model_chat(db, panel_body, &model, headers, endpoint, api_key).await
        }
        FusionOutcome::Failed { status, message } => ChatResult::error(status, &message),
    }
}

fn parse_fusion_tuning(value: &Value) -> Option<FusionTuning> {
    let obj = value.as_object()?;
    let mut tuning = FusionTuning::default();
    if let Some(v) = obj.get("minPanel").and_then(Value::as_u64) {
        tuning.min_panel = v as usize;
    }
    if let Some(v) = obj.get("stragglerGraceMs").and_then(Value::as_u64) {
        tuning.straggler_grace_ms = v;
    }
    if let Some(v) = obj.get("panelHardTimeoutMs").and_then(Value::as_u64) {
        tuning.panel_hard_timeout_ms = v;
    }
    Some(tuning)
}

/// `settings.comboStrategies[name].fallbackStrategy || settings.comboStrategy
/// || "fallback"`.
fn combo_strategy<'a>(settings: &'a Value, name: &str) -> &'a str {
    settings
        .get("comboStrategies")
        .and_then(|c| c.get(name))
        .and_then(|c| c.get("fallbackStrategy"))
        .and_then(Value::as_str)
        .or_else(|| settings.get("comboStrategy").and_then(Value::as_str))
        .unwrap_or("fallback")
}

fn lookup_combo(combos: &[Value], name: &str) -> Option<Value> {
    combos
        .iter()
        .find(|c| c.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
}

fn load_combos(db: &Db) -> Vec<Value> {
    db.with_conn(router_db::repos::combos::get_combos)
        .unwrap_or_default()
}

/// `saveRequestUsage(entry)`. The callback is synchronous, so the write blocks
/// the caller; it is one short `BEGIN IMMEDIATE` on an idle connection.
/// ponytail: sync write inside the stream-complete callback; move to
/// `spawn_blocking` if a slow disk ever shows up in a profile.
fn build_save_usage(db: &Db) -> SaveUsageFn {
    let db = db.clone();
    Arc::new(move |row: Value| {
        let cost = |provider: Option<&str>, model: Option<&str>, tokens: &Value| -> f64 {
            crate::catalog::get_pricing_for_model(provider, model.unwrap_or(""))
                .map(|pricing| crate::catalog::calculate_cost_from_tokens(tokens, pricing))
                .unwrap_or(0.0)
        };
        // `Db::write` retries, so its closure is `Fn`: clone the row per attempt
        // rather than mutating captured state.
        match db.write(|tx| {
            let mut row = row.clone();
            router_db::repos::usage::save_request_usage(tx, &mut row, &cost)
        }) {
            // `saveRequestUsage` fires `update` only when a new row landed.
            Ok(true) => crate::services::stats_emitter::emit_update(),
            Ok(false) => {}
            Err(error) => tracing::warn!("usage write failed: {error}"),
        }
    })
}

/// Whether a status came from the relay hop rather than the provider.
///
/// Cloudflare generates 520-527 at its edge for the Worker, so the credential
/// is not at fault and must not be cooled for it.
fn is_relay_edge_error(proxy_options: &ProxyOptions, status: u16) -> bool {
    proxy_options
        .vercel_relay_url
        .as_deref()
        .map(str::trim)
        .is_some_and(|u| !u.is_empty())
        && (520..=527).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_relay_requests_treat_edge_statuses_as_transport() {
        let relay = ProxyOptions {
            vercel_relay_url: Some("https://x.workers.dev".into()),
            ..ProxyOptions::default()
        };
        assert!(is_relay_edge_error(&relay, 520));
        assert!(is_relay_edge_error(&relay, 527));
        // A provider 4xx/5xx still falls back and cools as before.
        assert!(!is_relay_edge_error(&relay, 429));
        assert!(!is_relay_edge_error(&relay, 500));
        // No relay configured: the status came from the provider directly.
        assert!(!is_relay_edge_error(&ProxyOptions::default(), 520));
    }
}
