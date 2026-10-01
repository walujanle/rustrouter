//! Combo strategies.
//!
//! A combo is a named list of `provider/model` strings the router tries in
//! order. Two strategies exist: `fallback` (plain in-order retry) and
//! `round-robin` (rotate the list, sticky for N requests). Both are wrapped by
//! an auto-switch pass that floats models satisfying the request's required
//! modalities to the front, because a combo whose first model cannot see images
//! burns a failed attempt on every image request.
//!
//! The transport half — calling the models — is injected as an async closure
//! with the `handleSingleModel(body, modelStr)` contract, which keeps this
//! module testable without an HTTP layer.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::catalog::catalog::get_capabilities_for_model;
use crate::services::account_fallback::{check_fallback_error, format_retry_after};
use crate::translator::concerns::primitives::{js_string, js_truthy};
use crate::translator::formats::gemini::extract_text_content;

/// `HARD_CAPS`: input modalities. Missing one drops request data (an image gets
/// stripped), so they are prioritised over soft capabilities like `search`,
/// which only degrade a feature.
const HARD_CAPS: [&str; 4] = ["vision", "pdf", "audioInput", "videoInput"];

/// `TOOL_CALL_PREFIX` / `TOOL_RESULT_PREFIX`: the markers that replace structured
/// tool turns when a panel model must not see tools.
const TOOL_CALL_PREFIX: &str = "[Called tools: ";
const TOOL_RESULT_PREFIX: &str = "[Tool result: ";

/// One panel answer.
#[derive(Debug, Clone)]
pub struct PanelAnswer {
    pub model: String,
    pub text: String,
}

/// `flattenToolHistory(messages)`: turn tool turns into prose so a panel model
/// keeps the context but cannot loop on tools.
fn flatten_tool_history(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .filter(|m| js_truthy(m))
        .map(|msg| {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("");
            if role == "tool" || role == "function" {
                let content = msg.get("content").cloned().unwrap_or(Value::Null);
                let text = extract_text_content(&content, "");
                let text = if text.is_empty() { js_string(&content) } else { text };
                return json!({"role": "assistant", "content": format!("{TOOL_RESULT_PREFIX}{text}]")});
            }

            if role == "assistant"
                && let Some(calls) = msg.get("tool_calls").and_then(Value::as_array)
            {
                let mut rest = msg.as_object().cloned().unwrap_or_default();
                rest.shift_remove("tool_calls");
                let names: Vec<String> = calls
                    .iter()
                    .map(|c| {
                        c.get("function")
                            .and_then(|f| f.get("name"))
                            .or_else(|| c.get("name"))
                            .filter(|n| js_truthy(n))
                            .map(js_string)
                            .unwrap_or_else(|| "tool".to_string())
                    })
                    .collect();
                let content = rest.get("content").cloned().unwrap_or(Value::Null);
                let mut base = extract_text_content(&content, "");
                if base.is_empty()
                    && let Some(s) = content.as_str()
                {
                    base = s.to_string();
                }
                let sep = if base.is_empty() { "" } else { "\n" };
                rest.insert(
                    "content".into(),
                    json!(format!("{base}{sep}{TOOL_CALL_PREFIX}{}]", names.join(", "))),
                );
                return Value::Object(rest);
            }

            if let Some(blocks) = msg.get("content").and_then(Value::as_array) {
                let has_tool_use = blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));
                let has_tool_result = blocks.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
                if has_tool_use || has_tool_result {
                    let mut text_parts = Vec::new();
                    let mut tool_names = Vec::new();
                    let mut tool_results = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(t) = block.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                                    text_parts.push(t.to_string());
                                }
                            }
                            Some("tool_use") => {
                                tool_names.push(
                                    block
                                        .get("name")
                                        .filter(|n| js_truthy(n))
                                        .map(js_string)
                                        .unwrap_or_else(|| "tool".to_string()),
                                );
                            }
                            Some("tool_result") => {
                                let content = block.get("content").cloned().unwrap_or(Value::Null);
                                let text = extract_text_content(&content, "");
                                tool_results.push(if text.is_empty() { js_string(&content) } else { text });
                            }
                            _ => {}
                        }
                    }
                    let mut new_content = text_parts.join("\n");
                    if !tool_names.is_empty() {
                        let sep = if new_content.is_empty() { "" } else { "\n" };
                        new_content = format!("{new_content}{sep}{TOOL_CALL_PREFIX}{}]", tool_names.join(", "));
                    }
                    if !tool_results.is_empty() {
                        let sep = if new_content.is_empty() { "" } else { "\n" };
                        new_content = format!("{new_content}{sep}{TOOL_RESULT_PREFIX}{}]", tool_results.join("\n"));
                    }
                    let mut rest = msg.as_object().cloned().unwrap_or_default();
                    rest.insert("content".into(), json!(new_content));
                    return Value::Object(rest);
                }
            }

            msg.clone()
        })
        .collect()
}

/// `reorderByCapabilities(models, required)`: stable sort into
/// tier 0 (all hard + all soft), tier 1 (all hard), tier 2 (rest).
///
/// Never drops a model, so the fallback chain stays intact.
pub fn reorder_by_capabilities(models: &[String], required: &HashSet<String>) -> Vec<String> {
    if required.is_empty() || models.len() <= 1 {
        return models.to_vec();
    }
    let hard: Vec<&String> = required
        .iter()
        .filter(|c| HARD_CAPS.contains(&c.as_str()))
        .collect();
    let soft: Vec<&String> = required
        .iter()
        .filter(|c| !HARD_CAPS.contains(&c.as_str()))
        .collect();

    let tier_of = |m: &str| {
        let slash = m.find('/');
        let (provider, model) = match slash {
            // `slash > 0`: a leading slash means no provider prefix.
            Some(0) | None => ("", m),
            Some(i) => (&m[..i], &m[i + 1..]),
        };
        let caps = get_capabilities_for_model((!provider.is_empty()).then_some(provider), model);
        let has = |c: &str| match c {
            "vision" => caps.vision,
            "pdf" => caps.pdf,
            "audioInput" => caps.audio_input,
            "videoInput" => caps.video_input,
            "search" => caps.search,
            _ => false,
        };
        if !hard.iter().all(|c| has(c)) {
            return 2;
        }
        if soft.iter().all(|c| has(c)) { 0 } else { 1 }
    };

    let mut indexed: Vec<(usize, String, u8)> = models
        .iter()
        .enumerate()
        .map(|(i, m)| (i, m.clone(), tier_of(m)))
        .collect();
    indexed.sort_by_key(|(i, _, t)| (*t, *i));
    indexed.into_iter().map(|(_, m, _)| m).collect()
}

/// `comboRotationState`: combo name → `{index, consecutiveUseCount}`.
static COMBO_ROTATION_STATE: LazyLock<Mutex<std::collections::HashMap<String, RotationState>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

#[derive(Debug, Clone, Copy)]
struct RotationState {
    index: usize,
    consecutive_use_count: u64,
}

/// `trailingUserItems(arr)`: the run after the last assistant/model turn, which
/// is the current user turn. History media must not pin the combo to a vision
/// model — those get stripped downstream instead.
fn trailing_user_items(arr: Option<&Vec<Value>>) -> &[Value] {
    let Some(arr) = arr else {
        return &[];
    };
    let is_assistant = |r: Option<&str>| r == Some("assistant") || r == Some("model");
    let mut i = arr.len();
    while i > 0 {
        let role = arr[i - 1].get("role").and_then(Value::as_str);
        if is_assistant(role) {
            break;
        }
        i -= 1;
    }
    &arr[i..]
}

/// `detectRequiredCapabilities(body)`: the modalities the *current* user turn
/// needs, plus request-wide `search` (currently unwired, kept for parity).
///
/// Scans three request shapes: OpenAI/Claude `messages`, Responses `input`, and
/// Gemini `contents`.
pub fn detect_required_capabilities(body: &Value) -> HashSet<String> {
    let mut required = HashSet::new();
    if !body.is_object() {
        return required;
    }

    let add_by_mime = |mime: Option<&str>, required: &mut HashSet<String>| {
        let Some(mime) = mime else { return };
        if mime.starts_with("image/") {
            required.insert("vision".to_string());
        } else if mime == "application/pdf" {
            required.insert("pdf".to_string());
        } else if mime.starts_with("audio/") {
            required.insert("audioInput".to_string());
        } else if mime.starts_with("video/") {
            required.insert("videoInput".to_string());
        }
    };

    // `data:<mime>;...`: the mime runs up to the first `;` or `,`.
    fn mime_of_data_uri(s: &str) -> Option<&str> {
        let rest = s.strip_prefix("data:")?;
        let end = rest.find([';', ',']).unwrap_or(rest.len());
        Some(&rest[..end])
    }

    let scan_block = |b: &Value, required: &mut HashSet<String>| {
        if !b.is_object() {
            return;
        }
        match b.get("type").and_then(Value::as_str) {
            Some("image_url" | "image" | "input_image") => {
                required.insert("vision".to_string());
            }
            Some("input_audio" | "audio_url" | "audio") => {
                required.insert("audioInput".to_string());
            }
            Some("input_video" | "video_url" | "video") => {
                required.insert("videoInput".to_string());
            }
            Some("file" | "document" | "input_file") => {
                // Infer the modality from an embedded mime, else assume pdf.
                let mime = b
                    .get("input_audio")
                    .and_then(|a| a.get("format"))
                    .and_then(Value::as_str)
                    .map(|f| format!("audio/{f}"))
                    .or_else(|| {
                        b.get("file")
                            .and_then(|f| f.get("file_data"))
                            .and_then(Value::as_str)
                            .and_then(mime_of_data_uri)
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        b.get("source")
                            .and_then(|s| s.get("media_type"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .or_else(|| {
                        b.get("source")
                            .and_then(|s| s.get("data"))
                            .and_then(Value::as_str)
                            .and_then(mime_of_data_uri)
                            .map(str::to_string)
                    });
                match mime {
                    Some(m) => add_by_mime(Some(&m), required),
                    None => {
                        required.insert("pdf".to_string());
                    }
                }
            }
            _ => {}
        }
        // Gemini parts carry the mime on inlineData/fileData.
        add_by_mime(
            b.get("inlineData")
                .or_else(|| b.get("fileData"))
                .and_then(|d| d.get("mimeType"))
                .and_then(Value::as_str),
            required,
        );
    };

    let scan_content = |content: Option<&Value>, required: &mut HashSet<String>| {
        if let Some(Value::Array(blocks)) = content {
            for b in blocks {
                scan_block(b, required);
            }
        }
    };

    let scan_message = |m: &Value, required: &mut HashSet<String>| {
        if !m.is_object() {
            return;
        }

        // Ollama / Hermes images array.
        if m.get("images")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty())
        {
            required.insert("vision".to_string());
        }

        // Vercel AI SDK / Hermes attachments.
        if let Some(attachments) = m
            .get("experimental_attachments")
            .or_else(|| m.get("attachments"))
            .and_then(Value::as_array)
        {
            for att in attachments {
                if !js_truthy(att) {
                    continue;
                }
                let mime = att
                    .get("contentType")
                    .or_else(|| att.get("mediaType"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        att.get("url")
                            .and_then(Value::as_str)
                            .and_then(mime_of_data_uri)
                            .map(str::to_string)
                    });
                match mime {
                    Some(m) => add_by_mime(Some(&m), required),
                    None => {
                        if js_truthy(att.get("url").unwrap_or(&Value::Null))
                            || js_truthy(att.get("data").unwrap_or(&Value::Null))
                        {
                            required.insert("vision".to_string());
                        }
                    }
                }
            }
        }

        if js_truthy(m.get("image_url").unwrap_or(&Value::Null))
            || js_truthy(m.get("image").unwrap_or(&Value::Null))
        {
            required.insert("vision".to_string());
        }
        if js_truthy(m.get("audio_url").unwrap_or(&Value::Null))
            || js_truthy(m.get("audio").unwrap_or(&Value::Null))
        {
            required.insert("audioInput".to_string());
        }

        scan_content(m.get("content"), required);

        if let Some(content) = m.get("content").and_then(Value::as_str) {
            if content.contains("data:image/") {
                required.insert("vision".to_string());
            } else if content.contains("data:audio/") {
                required.insert("audioInput".to_string());
            } else if content.contains("data:application/pdf") {
                required.insert("pdf".to_string());
            }
        }
    };

    let messages = body.get("messages").and_then(Value::as_array);
    for m in trailing_user_items(messages) {
        scan_message(m, &mut required);
    }
    let input = body.get("input").and_then(Value::as_array);
    for item in trailing_user_items(input) {
        scan_content(item.get("content"), &mut required);
    }
    let contents = body
        .get("contents")
        .or_else(|| body.get("request").and_then(|r| r.get("contents")))
        .and_then(Value::as_array);
    for c in trailing_user_items(contents) {
        scan_content(c.get("parts"), &mut required);
    }

    required
}

/// `normalizeStickyLimit(stickyLimit)`.
fn normalize_sticky_limit(sticky_limit: u64) -> u64 {
    if sticky_limit > 0 { sticky_limit } else { 1 }
}

/// `rotateModelsFromIndex(models, currentIndex)`.
fn rotate_models_from_index(models: &[String], current_index: usize) -> Vec<String> {
    let mut rotated = models.to_vec();
    for _ in 0..current_index.min(rotated.len()) {
        let moved = rotated.remove(0);
        rotated.push(moved);
    }
    rotated
}

/// `getRotatedModels(models, comboName, strategy, stickyLimit = 1)`.
///
/// Only `round-robin` rotates; every other strategy returns the list untouched.
pub fn get_rotated_models(
    models: &[String],
    combo_name: Option<&str>,
    strategy: &str,
    sticky_limit: u64,
) -> Vec<String> {
    if models.len() <= 1 || strategy != "round-robin" {
        return models.to_vec();
    }
    let rotation_key = combo_name
        .filter(|n| !n.is_empty())
        .unwrap_or("__default__")
        .to_string();
    let sticky = normalize_sticky_limit(sticky_limit);

    let mut state_map = COMBO_ROTATION_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let state = state_map
        .get(&rotation_key)
        .copied()
        .unwrap_or(RotationState {
            index: 0,
            consecutive_use_count: 0,
        });

    let current_index = state.index % models.len();
    let rotated = rotate_models_from_index(models, current_index);
    let next_use_count = state.consecutive_use_count + 1;

    let next = if next_use_count >= sticky {
        RotationState {
            index: (current_index + 1) % models.len(),
            consecutive_use_count: 0,
        }
    } else {
        RotationState {
            index: current_index,
            consecutive_use_count: next_use_count,
        }
    };
    state_map.insert(rotation_key, next);
    rotated
}

/// `resetComboRotation(comboName)`: one combo, or all of them when `None`.
pub fn reset_combo_rotation(combo_name: Option<&str>) {
    let mut state_map = COMBO_ROTATION_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match combo_name {
        Some(name) => {
            state_map.remove(name);
        }
        None => state_map.clear(),
    }
}

/// `getComboModelsFromData(modelStr, combosData)`: a bare name that matches a
/// combo yields its model list; anything with a `/` is a `provider/model` pair
/// and never a combo.
///
/// Accepts either a bare array or an object with a `combos` array, since the
/// stored value takes both shapes.
pub fn get_combo_models_from_data(model_str: &str, combos_data: &Value) -> Option<Vec<String>> {
    if model_str.contains('/') {
        return None;
    }
    let combos = match combos_data {
        Value::Array(a) => a,
        Value::Object(o) => o.get("combos")?.as_array()?,
        _ => return None,
    };
    let combo = combos
        .iter()
        .find(|c| c.get("name").and_then(Value::as_str) == Some(model_str))?;
    let models = combo.get("models")?.as_array()?;
    if models.is_empty() {
        return None;
    }
    Some(
        models
            .iter()
            .filter_map(|m| m.as_str().map(str::to_string))
            .collect(),
    )
}

/// What the caller's `handleSingleModel` returned, reduced to what the combo
/// loop reads: a success, or a failure with a status and message.
#[derive(Debug)]
pub struct ComboAttempt {
    pub ok: bool,
    pub status: u16,
    pub error_text: String,
    /// `errorBody.retryAfter`, when the failure carried one.
    pub retry_after: Option<String>,
}

impl ComboAttempt {
    pub fn ok() -> Self {
        Self {
            ok: true,
            status: 200,
            error_text: String::new(),
            retry_after: None,
        }
    }

    pub fn failed(status: u16, error_text: impl Into<String>) -> Self {
        Self {
            ok: false,
            status,
            error_text: error_text.into(),
            retry_after: None,
        }
    }
}

/// The outcome of a whole combo run: either the winning model's index, or the
/// aggregate failure the caller must turn into a response.
#[derive(Debug)]
pub enum ComboOutcome {
    /// `index` is into the rotated list, which is what the caller passed to
    /// `handleSingleModel` — not into the original list.
    Success { index: usize },
    /// Every model failed. `retry_after` is the earliest pending reset across
    /// the attempts, when any reported one.
    AllFailed {
        status: u16,
        message: String,
        retry_after: Option<String>,
        retry_after_human: String,
    },
}

/// `handleComboChat({...})`.
///
/// `handle_single_model` is called once per model with the request body and the
/// model string; it is the caller's job to route, execute and translate. Its
/// success/failure detail collapses into the returned [`ComboAttempt`].
///
/// The transient-wait rule is preserved: a 502/503/504 with a cooldown of 5s or
/// less sleeps before moving on, so a briefly overloaded provider gets a chance
/// to recover instead of being skipped.
pub async fn handle_combo_chat<F, Fut>(
    body: &Value,
    models: &[String],
    combo_name: Option<&str>,
    combo_strategy: &str,
    combo_sticky_limit: u64,
    auto_switch: bool,
    mut handle_single_model: F,
) -> ComboOutcome
where
    F: FnMut(usize, &str) -> Fut,
    Fut: std::future::Future<Output = ComboAttempt>,
{
    let mut rotated = get_rotated_models(models, combo_name, combo_strategy, combo_sticky_limit);

    if auto_switch {
        let required = detect_required_capabilities(body);
        if !required.is_empty() {
            let reordered = reorder_by_capabilities(&rotated, &required);
            if reordered.first() != rotated.first() {
                tracing::info!(
                    "COMBO auto-switch for [{}] -> {}",
                    required.iter().cloned().collect::<Vec<_>>().join(","),
                    reordered.first().cloned().unwrap_or_default()
                );
            }
            rotated = reordered;
        }
    }

    let mut last_error: Option<String> = None;
    let mut earliest_retry_after: Option<String> = None;
    let mut last_status: Option<u16> = None;

    for (i, model_str) in rotated.iter().enumerate() {
        tracing::info!(
            "COMBO trying model {}/{}: {}",
            i + 1,
            rotated.len(),
            model_str
        );

        let result = handle_single_model(i, model_str).await;

        if result.ok {
            tracing::info!("COMBO model {model_str} succeeded");
            return ComboOutcome::Success { index: i };
        }

        let mut error_text = result.error_text;
        if error_text.is_empty() {
            error_text = result.status.to_string();
        }

        if let Some(retry_after) = &result.retry_after {
            let earlier = earliest_retry_after
                .as_deref()
                .is_none_or(|current| retry_after.as_str() < current);
            if earlier {
                earliest_retry_after = Some(retry_after.clone());
            }
        }

        let decision = check_fallback_error(result.status, Some(&json!(error_text)), 0);

        if !decision.should_fallback {
            tracing::warn!(
                "COMBO model {model_str} failed (no fallback) status={}",
                result.status
            );
            return ComboOutcome::AllFailed {
                status: result.status,
                message: error_text,
                retry_after: earliest_retry_after,
                retry_after_human: String::new(),
            };
        }

        // Transient upstream: wait out a short cooldown before moving on.
        if decision.cooldown_ms > 0
            && decision.cooldown_ms <= 5000
            && matches!(result.status, 502..=504)
        {
            tracing::info!(
                "COMBO model {model_str} transient {} waiting {}ms before next",
                result.status,
                decision.cooldown_ms
            );
            tokio::time::sleep(Duration::from_millis(decision.cooldown_ms)).await;
        }

        last_error = Some(error_text);
        last_status.get_or_insert(result.status);
        tracing::warn!(
            "COMBO model {model_str} failed, trying next status={}",
            result.status
        );
    }

    // All models failed. 503 rather than 406: the providers are unavailable or
    // uncredentialed, which is retryable — not a bad request.
    let last_error = last_error.unwrap_or_else(|| "All combo models unavailable".to_string());
    let all_disabled = last_error.to_lowercase().contains("no credentials");
    let status = if all_disabled {
        503
    } else {
        last_status.unwrap_or(503)
    };
    let retry_after_human = earliest_retry_after
        .as_deref()
        .map(|r| format_retry_after(Some(r)))
        .unwrap_or_default();

    ComboOutcome::AllFailed {
        status,
        message: last_error,
        retry_after: earliest_retry_after,
        retry_after_human,
    }
}

// ─── fusion ──────────────────────────────────────────────────────────────

/// `FUSION_DEFAULTS`.
#[derive(Debug, Clone, Copy)]
pub struct FusionTuning {
    pub min_panel: usize,
    pub straggler_grace_ms: u64,
    pub panel_hard_timeout_ms: u64,
}

impl Default for FusionTuning {
    fn default() -> Self {
        Self {
            min_panel: 2,
            straggler_grace_ms: 8000,
            panel_hard_timeout_ms: 90_000,
        }
    }
}

/// `extractPanelText(json)`: assistant text from a non-stream completion in any
/// of the four client formats.
fn extract_panel_text(json: &Value) -> String {
    if !json.is_object() {
        return String::new();
    }

    if let Some(choice) = json.get("choices").and_then(|c| c.get(0)) {
        let msg = choice.get("message").or_else(|| choice.get("delta"));
        let t = extract_text_content(
            msg.and_then(|m| m.get("content")).unwrap_or(&Value::Null),
            "",
        );
        if !t.trim().is_empty() {
            return t;
        }
        if let Some(text) = choice
            .get("text")
            .and_then(Value::as_str)
            .filter(|t| !t.trim().is_empty())
        {
            return text.to_string();
        }
    }

    let claude_text = extract_text_content(json.get("content").unwrap_or(&Value::Null), "");
    if !claude_text.trim().is_empty() {
        return claude_text;
    }

    if let Some(parts) = json
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        let t: String = parts
            .iter()
            .map(|p| p.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect();
        if !t.trim().is_empty() {
            return t;
        }
    }

    if let Some(output) = json.get("output").and_then(Value::as_array) {
        let t: String = output
            .iter()
            .flat_map(|o| {
                o.get("content")
                    .and_then(Value::as_array)
                    .map(|c| {
                        c.iter()
                            .map(|c| c.get("text").and_then(Value::as_str).unwrap_or(""))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
        if !t.trim().is_empty() {
            return t;
        }
    }

    String::new()
}

/// `appendUserTurn(body, text)`: add a synthesized user turn to whichever array
/// the request shape uses, preserving the original conversation.
pub fn append_user_turn(body: &Value, text: &str) -> Value {
    let mut next = body.as_object().cloned().unwrap_or_default();
    if next.get("messages").and_then(Value::as_array).is_some() {
        let mut messages = next["messages"].as_array().cloned().unwrap_or_default();
        messages.push(json!({"role": "user", "content": text}));
        next.insert("messages".into(), Value::Array(messages));
    } else if next.get("input").and_then(Value::as_array).is_some() {
        let mut input = next["input"].as_array().cloned().unwrap_or_default();
        input.push(json!({"role": "user", "content": text}));
        next.insert("input".into(), Value::Array(input));
    } else if next.get("contents").and_then(Value::as_array).is_some() {
        let mut contents = next["contents"].as_array().cloned().unwrap_or_default();
        contents.push(json!({"role": "user", "parts": [{"text": text}]}));
        next.insert("contents".into(), Value::Array(contents));
    } else {
        next.insert(
            "messages".into(),
            json!([{"role": "user", "content": text}]),
        );
    }
    Value::Object(next)
}

/// `buildJudgePrompt(answers)`: anonymize the panel as `[Source N]` so the judge
/// weighs substance, not model brand, then ask for one synthesized answer.
pub fn build_judge_prompt(answers: &[PanelAnswer]) -> String {
    let panel = answers
        .iter()
        .enumerate()
        .map(|(i, a)| format!("[Source {}]\n{}", i + 1, a.text))
        .collect::<Vec<_>>()
        .join("\n\n");

    [
        format!(
            "You are the JUDGE in a model-fusion panel. {} expert models independently answered the user's most recent request. Their responses are below, anonymized by source.",
            answers.len()
        ),
        String::new(),
        "Do NOT mention that multiple models were used, and do NOT refer to the sources. Produce ONE authoritative final answer addressed directly to the user.".to_string(),
        String::new(),
        "First, internally analyze the panel along these dimensions: consensus (points most sources agree on — treat as higher-confidence), contradictions (where they disagree — resolve with your own judgment), partial coverage, unique insights only one source surfaced, and blind spots every source missed. Then write the best possible final answer grounded in that analysis — more complete and correct than any single response, with no filler.".to_string(),
        String::new(),
        "=== PANEL RESPONSES ===".to_string(),
        panel,
        "=== END PANEL RESPONSES ===".to_string(),
        String::new(),
        "Now write the final answer to the user's original request.".to_string(),
    ]
    .join("\n")
}

/// `collectPanel(calls, {minPanel, stragglerGraceMs, panelHardTimeoutMs})`:
/// once `min_panel` calls have succeeded, start a grace timer for the rest, so
/// the slowest model does not dominate wall time. Bounded by a hard timeout.
///
/// Returns a sparse vector aligned to `calls`: `None` where a call was dropped.
/// A dropped call keeps running in the runtime; its result is ignored.
pub async fn collect_panel<T, E, Fut>(
    calls: Vec<Fut>,
    min_panel: usize,
    straggler_grace_ms: u64,
    panel_hard_timeout_ms: u64,
) -> Vec<Option<Result<T, E>>>
where
    T: Send + 'static,
    E: Send + 'static,
    Fut: std::future::Future<Output = Result<T, E>> + Send + 'static,
{
    let len = calls.len();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(usize, Result<T, E>)>();

    for (i, call) in calls.into_iter().enumerate() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = call.await;
            let _ = tx.send((i, result));
        });
    }
    drop(tx);

    let mut out: Vec<Option<Result<T, E>>> = (0..len).map(|_| None).collect();
    let mut settled = 0usize;
    let mut ok = 0usize;
    let mut grace_deadline: Option<tokio::time::Instant> = None;
    let hard_deadline = tokio::time::Instant::now() + Duration::from_millis(panel_hard_timeout_ms);

    while settled < len {
        let deadline = match grace_deadline {
            Some(g) => g.min(hard_deadline),
            None => hard_deadline,
        };
        let received = tokio::time::timeout_at(deadline, rx.recv()).await;
        match received {
            Ok(Some((i, result))) => {
                settled += 1;
                if result.is_ok() {
                    ok += 1;
                }
                out[i] = Some(result);
                if ok >= min_panel && grace_deadline.is_none() {
                    grace_deadline = Some(
                        tokio::time::Instant::now() + Duration::from_millis(straggler_grace_ms),
                    );
                }
            }
            // Every sender dropped: nothing more can arrive.
            Ok(None) => break,
            // Grace window or hard timeout expired.
            Err(_) => break,
        }
    }

    out
}

/// The result of a fusion run: the judge's model string plus the body to send
/// it, or the reason there is nothing to judge.
#[derive(Debug)]
pub enum FusionOutcome {
    /// Hand the judge this body and this model. The caller streams it back.
    Judge { body: Value, model: String },
    /// The panel produced one answer; return it directly with no fusion.
    Direct { model: String },
    /// Nothing usable; the caller builds an error response.
    Failed { status: u16, message: String },
}

/// `handleFusionChat({...})`.
///
/// `handle_single_model(body, model, is_panel)` is the caller's transport: it
/// returns the parsed non-streaming body on success, or the failure. It is
/// `Fn` rather than `FnMut` because every panel call runs concurrently, and it
/// takes owned arguments so the spawned futures are `'static`.
///
/// The caller is responsible for what `is_panel` implies at the transport level
/// (non-streaming, tools stripped), which is why the flag travels to the caller
/// rather than being absorbed here.
pub async fn handle_fusion_chat<F, Fut>(
    body: &Value,
    models: &[String],
    judge_model: Option<&str>,
    tuning: Option<FusionTuning>,
    handle_single_model: F,
) -> FusionOutcome
where
    F: Fn(Value, String, bool) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Value, ComboAttempt>> + Send + 'static,
{
    let panel: Vec<String> = models.iter().filter(|m| !m.is_empty()).cloned().collect();
    if panel.is_empty() {
        return FusionOutcome::Failed {
            status: 400,
            message: "Fusion combo has no models".to_string(),
        };
    }
    // A single-model fusion has nothing to fuse.
    if panel.len() == 1 {
        return FusionOutcome::Direct {
            model: panel[0].clone(),
        };
    }

    let cfg = tuning.unwrap_or_default();
    let min_panel = cfg.min_panel.clamp(2, panel.len());
    let judge = judge_model
        .map(str::trim)
        .filter(|j| !j.is_empty())
        .unwrap_or(&panel[0])
        .to_string();

    // Panel calls are non-streaming with tools stripped: the judge needs prose.
    let mut panel_body = body.as_object().cloned().unwrap_or_default();
    panel_body.shift_remove("tools");
    panel_body.shift_remove("tool_choice");
    // `stream_options` only travels with `stream: true`; providers reject it
    // otherwise ("stream_options should be set along with stream = true").
    panel_body.shift_remove("stream_options");
    panel_body.insert("stream".into(), Value::Bool(false));
    if let Some(messages) = panel_body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
    {
        panel_body.insert(
            "messages".into(),
            Value::Array(flatten_tool_history(&messages)),
        );
    } else if let Some(input) = panel_body.get("input").and_then(Value::as_array).cloned() {
        panel_body.insert("input".into(), Value::Array(flatten_tool_history(&input)));
    }
    let panel_body = Value::Object(panel_body);

    tracing::info!(
        "FUSION combo panel={} [{}] judge={judge} quorum={min_panel}",
        panel.len(),
        panel.join(", ")
    );

    let started = std::time::Instant::now();
    // `Arc` so each panel future owns a handle: the futures must be `'static`
    // to be spawned, and they all call the same transport.
    let handle = std::sync::Arc::new(handle_single_model);
    let calls: Vec<_> = panel
        .iter()
        .map(|model| {
            let model = model.clone();
            let body = panel_body.clone();
            let handle = std::sync::Arc::clone(&handle);
            async move { handle(body, model, true).await }
        })
        .collect();
    let settled = collect_panel(
        calls,
        min_panel,
        cfg.straggler_grace_ms,
        cfg.panel_hard_timeout_ms,
    )
    .await;
    tracing::info!(
        "FUSION fan-out collected in {}ms",
        started.elapsed().as_millis()
    );

    // Keep only the answers that arrived, parsed and non-empty.
    let mut answers: Vec<PanelAnswer> = Vec::new();
    for (i, result) in settled.into_iter().enumerate() {
        let model = &panel[i];
        let Some(result) = result else {
            tracing::warn!("FUSION panel {model} dropped (straggler/timeout)");
            continue;
        };
        match result {
            Err(failure) => {
                tracing::warn!("FUSION panel {model} failed status={}", failure.status);
            }
            Ok(json) => {
                let text = extract_panel_text(&json);
                if text.is_empty() {
                    tracing::warn!("FUSION panel {model} returned empty content");
                } else {
                    tracing::info!("FUSION panel {model} ok ({} chars)", text.len());
                    answers.push(PanelAnswer {
                        model: model.clone(),
                        text,
                    });
                }
            }
        }
    }

    // Degrade gracefully rather than failing the whole request.
    match answers.len() {
        0 => {
            tracing::warn!("FUSION all panel models failed");
            FusionOutcome::Failed {
                status: 503,
                message: "All fusion panel models failed".to_string(),
            }
        }
        1 => {
            let model = answers[0].model.clone();
            tracing::info!("FUSION only {model} succeeded — answering directly (no fusion)");
            FusionOutcome::Direct { model }
        }
        _ => {
            let judge_body = append_user_turn(body, &build_judge_prompt(&answers));
            tracing::info!("FUSION judging {} answers with {judge}", answers.len());
            FusionOutcome::Judge {
                body: judge_body,
                model: judge,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `COMBO_ROTATION_STATE` is process-global and `reset_combo_rotation(None)`
    /// clears every combo, so a concurrent reset wipes a sibling test's cursor
    /// mid-assertion. A poisoned lock is recovered so one failure does not
    /// cascade.
    fn rotation_guard() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn combo_lookup_rejects_provider_prefixed_names() {
        let combos = json!([{"name": "fast", "models": ["a/b", "c/d"]}]);
        assert_eq!(
            get_combo_models_from_data("fast", &combos).unwrap(),
            vec!["a/b", "c/d"]
        );
        assert!(get_combo_models_from_data("a/b", &combos).is_none());
        assert!(get_combo_models_from_data("missing", &combos).is_none());
    }

    #[test]
    fn combo_lookup_accepts_the_object_shape_and_rejects_empties() {
        let wrapped = json!({"combos": [{"name": "fast", "models": ["a/b"]}]});
        assert_eq!(
            get_combo_models_from_data("fast", &wrapped).unwrap(),
            vec!["a/b"]
        );
        let empty = json!([{"name": "fast", "models": []}]);
        assert!(get_combo_models_from_data("fast", &empty).is_none());
    }

    #[test]
    fn rotation_is_a_no_op_for_one_model_or_a_plain_strategy() {
        let _serial = rotation_guard();
        let models = vec!["a/b".to_string(), "c/d".to_string()];
        assert_eq!(
            get_rotated_models(&models, Some("x"), "fallback", 1),
            models
        );
        let single = vec!["a/b".to_string()];
        assert_eq!(
            get_rotated_models(&single, Some("x"), "round-robin", 1),
            single
        );
    }

    #[test]
    fn round_robin_advances_after_the_sticky_limit() {
        let _serial = rotation_guard();
        reset_combo_rotation(None);
        let models = vec!["a/1".to_string(), "b/2".to_string(), "c/3".to_string()];

        // sticky = 1: every call advances.
        assert_eq!(
            get_rotated_models(&models, Some("rr"), "round-robin", 1),
            models
        );
        assert_eq!(
            get_rotated_models(&models, Some("rr"), "round-robin", 1),
            vec!["b/2", "c/3", "a/1"]
        );
        reset_combo_rotation(None);

        // sticky = 2: the first model stays for two calls.
        assert_eq!(
            get_rotated_models(&models, Some("rr"), "round-robin", 2),
            models
        );
        assert_eq!(
            get_rotated_models(&models, Some("rr"), "round-robin", 2),
            models
        );
        assert_eq!(
            get_rotated_models(&models, Some("rr"), "round-robin", 2),
            vec!["b/2", "c/3", "a/1"]
        );
        reset_combo_rotation(None);
    }

    #[test]
    fn sticky_limit_zero_normalizes_to_one() {
        let _serial = rotation_guard();
        reset_combo_rotation(None);
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        assert_eq!(
            get_rotated_models(&models, Some("z"), "round-robin", 0),
            models
        );
        assert_eq!(
            get_rotated_models(&models, Some("z"), "round-robin", 0),
            vec!["b/2", "a/1"]
        );
        reset_combo_rotation(None);
    }

    #[test]
    fn rotation_state_is_per_combo_name() {
        let _serial = rotation_guard();
        reset_combo_rotation(None);
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        get_rotated_models(&models, Some("one"), "round-robin", 1);
        // A different combo has its own cursor and starts fresh.
        assert_eq!(
            get_rotated_models(&models, Some("two"), "round-robin", 1),
            models
        );
        reset_combo_rotation(None);
    }

    #[test]
    fn detect_capabilities_reads_only_the_current_user_turn() {
        // History image, current turn text: no vision required.
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]},
            {"role": "assistant", "content": "ok"},
            {"role": "user", "content": "what did I show you?"},
        ]});
        assert!(detect_required_capabilities(&body).is_empty());

        // Current turn image: vision required.
        let body = json!({"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]},
        ]});
        let required = detect_required_capabilities(&body);
        assert!(required.contains("vision"));
        assert_eq!(required.len(), 1);
    }

    #[test]
    fn detect_capabilities_sees_data_uris_and_attachments() {
        let body =
            json!({"messages": [{"role": "user", "content": "see data:image/png;base64,AAA"}]});
        assert!(detect_required_capabilities(&body).contains("vision"));

        let body = json!({"messages": [{"role": "user", "content": "x", "attachments": [{"contentType": "application/pdf"}]}]});
        assert!(detect_required_capabilities(&body).contains("pdf"));

        let body = json!({"messages": [{"role": "user", "images": ["data:image/png;base64,AAA"]}]});
        assert!(detect_required_capabilities(&body).contains("vision"));
    }

    #[test]
    fn detect_capabilities_reads_responses_and_gemini_shapes() {
        let responses = json!({"input": [{"role": "user", "content": [{"type": "input_image", "image_url": "x"}]}]});
        assert!(detect_required_capabilities(&responses).contains("vision"));

        let gemini = json!({"contents": [{"role": "user", "parts": [{"inlineData": {"mimeType": "audio/wav", "data": "x"}}]}]});
        assert!(detect_required_capabilities(&gemini).contains("audioInput"));

        let nested = json!({"request": {"contents": [{"role": "user", "parts": [{"fileData": {"mimeType": "video/mp4"}}]}]}});
        assert!(detect_required_capabilities(&nested).contains("videoInput"));
    }

    #[test]
    fn reorder_floats_capable_models_but_never_drops_one() {
        // A model the capability table marks vision-capable, and one it does not.
        let capable = "openrouter/google/gemini-2.5-flash".to_string();
        let incapable = "openrouter/deepseek/deepseek-chat".to_string();
        let models = vec![incapable.clone(), capable.clone()];
        let required: HashSet<String> = ["vision".to_string()].into_iter().collect();
        let reordered = reorder_by_capabilities(&models, &required);
        assert_eq!(reordered.len(), 2);
        assert!(reordered.contains(&incapable) && reordered.contains(&capable));

        // With nothing required the order is untouched.
        assert_eq!(reorder_by_capabilities(&models, &HashSet::new()), models);
    }

    #[test]
    fn append_user_turn_uses_the_requests_own_array() {
        let body = json!({"messages": [{"role": "user", "content": "a"}]});
        let out = append_user_turn(&body, "judge");
        assert_eq!(out["messages"].as_array().unwrap().len(), 2);
        assert_eq!(out["messages"][1]["content"], json!("judge"));

        let body = json!({"input": [{"role": "user", "content": "a"}]});
        let out = append_user_turn(&body, "judge");
        assert_eq!(out["input"].as_array().unwrap().len(), 2);

        let body = json!({"contents": [{"role": "user", "parts": []}]});
        let out = append_user_turn(&body, "judge");
        assert_eq!(out["contents"][1]["parts"][0]["text"], json!("judge"));

        // Nothing to append to: a fresh messages array is created.
        let out = append_user_turn(&json!({}), "judge");
        assert_eq!(out["messages"][0]["content"], json!("judge"));
    }

    #[test]
    fn judge_prompt_anonymizes_sources() {
        let answers = vec![
            PanelAnswer {
                model: "a/1".into(),
                text: "first".into(),
            },
            PanelAnswer {
                model: "b/2".into(),
                text: "second".into(),
            },
        ];
        let prompt = build_judge_prompt(&answers);
        assert!(prompt.contains("[Source 1]\nfirst"));
        assert!(prompt.contains("[Source 2]\nsecond"));
        assert!(prompt.contains("2 expert models"));
        assert!(!prompt.contains("a/1"));
    }

    #[test]
    fn panel_text_reads_all_four_formats() {
        assert_eq!(
            extract_panel_text(&json!({"choices": [{"message": {"content": "hi"}}]})),
            "hi"
        );
        assert_eq!(
            extract_panel_text(&json!({"choices": [{"text": "legacy"}]})),
            "legacy"
        );
        assert_eq!(
            extract_panel_text(&json!({"content": [{"type": "text", "text": "claude"}]})),
            "claude"
        );
        assert_eq!(
            extract_panel_text(&json!({"candidates": [{"content": {"parts": [{"text": "gem"}]}}]})),
            "gem"
        );
        assert_eq!(
            extract_panel_text(&json!({"output": [{"content": [{"text": "resp"}]}]})),
            "resp"
        );
        assert_eq!(extract_panel_text(&json!({})), "");
    }

    #[test]
    fn flatten_tool_history_turns_results_into_prose() {
        let messages = json!([
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "read"}}, {"name": "write"}]},
            {"role": "tool", "content": "file body"},
        ]);
        let out = flatten_tool_history(messages.as_array().unwrap());
        assert_eq!(out.len(), 3);
        assert_eq!(out[1]["content"], json!("[Called tools: read, write]"));
        assert!(out[1].get("tool_calls").is_none());
        assert_eq!(out[2]["content"], json!("[Tool result: file body]"));
        assert_eq!(out[2]["role"], json!("assistant"));
    }

    #[test]
    fn flatten_tool_history_handles_claude_blocks() {
        let messages = json!([{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "tool_use", "name": "search"},
            {"type": "tool_result", "content": "found it"},
        ]}]);
        let out = flatten_tool_history(messages.as_array().unwrap());
        let content = out[0]["content"].as_str().unwrap();
        assert!(content.contains("look"));
        assert!(content.contains("[Called tools: search]"));
        assert!(content.contains("[Tool result: found it]"));
    }

    #[test]
    fn flatten_tool_history_leaves_plain_turns_alone() {
        let messages = json!([{"role": "user", "content": "plain"}]);
        let out = flatten_tool_history(messages.as_array().unwrap());
        assert_eq!(out[0], json!({"role": "user", "content": "plain"}));
    }

    #[tokio::test]
    async fn combo_returns_the_first_success() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let outcome = handle_combo_chat(&body, &models, None, "fallback", 1, false, |_i, model| {
            let model = model.to_string();
            async move {
                if model == "a/1" {
                    ComboAttempt::ok()
                } else {
                    ComboAttempt::failed(500, "nope")
                }
            }
        })
        .await;
        assert!(matches!(outcome, ComboOutcome::Success { index: 0 }));
    }

    #[tokio::test]
    async fn combo_falls_through_to_the_next_model() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let outcome = handle_combo_chat(&body, &models, None, "fallback", 1, false, |_i, model| {
            let model = model.to_string();
            async move {
                if model == "a/1" {
                    ComboAttempt::failed(500, "overloaded")
                } else {
                    ComboAttempt::ok()
                }
            }
        })
        .await;
        assert!(matches!(outcome, ComboOutcome::Success { index: 1 }));
    }

    #[tokio::test]
    async fn combo_reports_no_credentials_as_503() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string()];
        let outcome =
            handle_combo_chat(&body, &models, None, "fallback", 1, false, |_i, _m| async {
                ComboAttempt::failed(500, "No credentials for provider")
            })
            .await;
        match outcome {
            ComboOutcome::AllFailed {
                status, message, ..
            } => {
                assert_eq!(status, 503);
                assert!(message.to_lowercase().contains("no credentials"));
            }
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn combo_stops_on_a_non_fallback_error() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let mut calls = 0;
        let outcome = handle_combo_chat(&body, &models, None, "fallback", 1, false, |_i, _m| {
            calls += 1;
            async { ComboAttempt::failed(400, "malformed body") }
        })
        .await;
        match outcome {
            ComboOutcome::AllFailed { status, .. } => assert_eq!(status, 400),
            other => panic!("expected AllFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn panel_collection_returns_early_after_quorum() {
        let calls: Vec<_> = (0..3)
            .map(|i| async move {
                if i == 0 {
                    Ok(i)
                } else {
                    // Never completes: the grace window must cut it off.
                    std::future::pending::<()>().await;
                    Err(())
                }
            })
            .collect();
        let start = std::time::Instant::now();
        let out = collect_panel(calls, 1, 50, 10_000).await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], Some(Ok(0)));
        assert!(out[1].is_none() && out[2].is_none());
        // Cut off by the grace window, not the hard timeout.
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn panel_collection_honours_the_hard_timeout() {
        let calls: Vec<_> = (0..2)
            .map(|_| async {
                std::future::pending::<()>().await;
                Err(())
            })
            .collect();
        let start = std::time::Instant::now();
        let out = collect_panel::<i32, (), _>(calls, 2, 5000, 60).await;
        assert!(out.iter().all(Option::is_none));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    #[tokio::test]
    async fn fusion_answers_directly_when_only_one_panel_model_succeeds() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let outcome = handle_fusion_chat(
            &body,
            &models,
            None,
            None,
            |_b, model, is_panel| async move {
                assert!(is_panel, "panel calls must be flagged");
                if model == "a/1" {
                    Ok(json!({"choices": [{"message": {"content": "only"}}]}))
                } else {
                    Err(ComboAttempt::failed(500, "dead"))
                }
            },
        )
        .await;
        match outcome {
            FusionOutcome::Direct { model } => assert_eq!(model, "a/1"),
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fusion_judges_when_the_panel_is_thick_enough() {
        let body = json!({"messages": [{"role": "user", "content": "x"}], "tools": [{"x": 1}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let outcome = handle_fusion_chat(&body, &models, Some("judge/1"), None, |b, model, is_panel| async move {
            if is_panel {
                // Tools must be stripped and streaming forced off for the panel.
                assert!(b.get("tools").is_none());
                assert_eq!(b["stream"], json!(false));
                Ok(json!({"choices": [{"message": {"content": format!("answer from {model}")}}]}))
            } else {
                Ok(json!({}))
            }
        })
        .await;
        match outcome {
            FusionOutcome::Judge { body, model } => {
                assert_eq!(model, "judge/1");
                // The judge body keeps the client's tools and gets the prompt appended.
                assert!(body.get("tools").is_some());
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 2);
                let prompt = messages[1]["content"].as_str().unwrap();
                assert!(prompt.contains("[Source 1]"));
                assert!(prompt.contains("answer from a/1"));
            }
            other => panic!("expected Judge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fusion_errors_when_no_panel_model_answers() {
        let body = json!({"messages": [{"role": "user", "content": "x"}]});
        let models = vec!["a/1".to_string(), "b/2".to_string()];
        let outcome = handle_fusion_chat(&body, &models, None, None, |_b, _m, _p| async {
            Err(ComboAttempt::failed(500, "dead"))
        })
        .await;
        match outcome {
            FusionOutcome::Failed { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fusion_with_one_model_skips_the_fan_out_entirely() {
        let body = json!({"messages": []});
        let models = vec!["solo/1".to_string()];
        let outcome = handle_fusion_chat(&body, &models, None, None, |_b, _m, _p| async {
            panic!("a one-model fusion must not call the transport")
        })
        .await;
        match outcome {
            FusionOutcome::Direct { model } => assert_eq!(model, "solo/1"),
            other => panic!("expected Direct, got {other:?}"),
        }
    }
}
