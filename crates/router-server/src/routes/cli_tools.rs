//! The three kept CLI-tool writers: `cli-tools/{claude,codex,hermes}-settings`.
//!
//! Each mutates files in the user's real home directory, so the guard routes
//! them through `LOCAL_ONLY_PATHS` and each path gets its own write lock. The
//! other 22 writers the dashboard offered left with their tools.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::error::ApiError;

/// The exa MCP server entry the Claude writer installs. Cowork is dropped, so
/// the one entry is inlined rather than carrying the whole plugin catalogue.
const EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";

/// One lock per resolved path, so two concurrent writes to the same dotfile
/// serialise while writes to different files still overlap.
static LOCKS: LazyLock<DashMap<PathBuf, std::sync::Arc<Mutex<()>>>> = LazyLock::new(DashMap::new);

async fn lock_for(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    let entry = LOCKS
        .entry(path.to_path_buf())
        .or_insert_with(|| std::sync::Arc::new(Mutex::new(())))
        .clone();
    entry.lock_owned().await
}

/// The user's home directory. `HOME` wins so tests can point at a temp dir;
/// `dirs::home_dir` is the real path.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(dirs::home_dir)
}

/// `which`/`where` probe, falling back to a file-exists check.
fn installed(binary: &str, fallback: &Path) -> bool {
    if which::which(binary).is_ok() {
        return true;
    }
    fallback.exists()
}

/// Parse JSON, tolerating a trailing comma before a closing brace or bracket.
fn parse_jsonc(text: &str) -> Option<Value> {
    serde_json::from_str(text).ok().or_else(|| {
        let stripped = strip_trailing_commas(text);
        serde_json::from_str(&stripped).ok()
    })
}

/// Drop a comma that is followed only by whitespace and a closing brace/bracket.
fn strip_trailing_commas(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                i += 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// `baseUrl.endsWith("/v1") ? baseUrl : baseUrl + "/v1"`.
fn normalize_base_url(base: &str) -> String {
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

fn write_text(path: &Path, contents: &str) -> Result<(), std::io::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, contents)
}

/// Pretty-print a value as 2-space-indented JSON.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

async fn parse_body(
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Map<String, Value>, Response> {
    match body {
        Ok(Json(Value::Object(map))) => Ok(map),
        Ok(_) => Err(ApiError::bad_request("Invalid request body").into_response()),
        Err(_) => Err(ApiError::bad_request("Invalid JSON body").into_response()),
    }
}

// ── Claude Code ───────────────────────────────────────────────────────────

/// `RESET_ENV_KEYS`.
const CLAUDE_RESET_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "API_TIMEOUT_MS",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
];

fn claude_settings_path() -> Option<PathBuf> {
    home().map(|h| h.join(".claude").join("settings.json"))
}

fn claude_json_path() -> Option<PathBuf> {
    home().map(|h| h.join(".claude.json"))
}

/// `writeClaudeJsonMcp(mcpServers)`: merge or drop the `exa` entry, keeping the
/// rest of `~/.claude.json`.
fn write_claude_json_mcp(mcp_servers: Option<Value>) -> std::io::Result<()> {
    let Some(path) = claude_json_path() else {
        return Ok(());
    };
    let mut data = read_text(&path)
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    match mcp_servers.and_then(|v| v.as_object().cloned()) {
        Some(servers) if !servers.is_empty() => {
            let mut existing = data
                .get("mcpServers")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (k, v) in servers {
                existing.insert(k, v);
            }
            data.insert("mcpServers".into(), Value::Object(existing));
        }
        _ => {
            if let Some(servers) = data.get_mut("mcpServers").and_then(Value::as_object_mut) {
                servers.remove("exa");
                if servers.is_empty() {
                    data.remove("mcpServers");
                }
            }
        }
    }
    write_text(&path, &pretty(&Value::Object(data)))
}

/// `GET /api/cli-tools/claude-settings`.
pub async fn claude_get() -> Response {
    let Some(settings_path) = claude_settings_path() else {
        return Json(json!({ "installed": false, "settings": null, "message": "Claude CLI is not installed" }))
            .into_response();
    };
    if !installed("claude", &settings_path) {
        return Json(json!({
            "installed": false,
            "settings": null,
            "message": "Claude CLI is not installed",
        }))
        .into_response();
    }

    let settings = read_text(&settings_path).and_then(|t| parse_jsonc(&t));
    let has_9router = settings
        .as_ref()
        .and_then(|s| s.get("env"))
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .is_some();
    let exa_enabled = claude_json_path()
        .and_then(|p| read_text(&p))
        .and_then(|t| parse_jsonc(&t))
        .and_then(|v| v.get("mcpServers").cloned())
        .and_then(|m| m.get("exa").cloned())
        .is_some();

    Json(json!({
        "installed": true,
        "settings": settings.unwrap_or(Value::Null),
        "has9Router": has_9router,
        "exaMcpEnabled": exa_enabled,
        "settingsPath": settings_path.to_string_lossy(),
    }))
    .into_response()
}

/// `POST /api/cli-tools/claude-settings`.
pub async fn claude_post(body: Result<Json<Value>, JsonRejection>) -> Response {
    let body = match parse_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(env) = body.get("env").and_then(Value::as_object).cloned() else {
        return ApiError::bad_request("Invalid env object").into_response();
    };
    let Some(settings_path) = claude_settings_path() else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&settings_path).await;

    let mut env = env;
    if let Some(base) = env.get("ANTHROPIC_BASE_URL").and_then(Value::as_str) {
        env.insert(
            "ANTHROPIC_BASE_URL".into(),
            Value::String(normalize_base_url(base)),
        );
    }

    // A stored token wins: an incoming one is dropped so Reset is the only way
    // to clear it.
    let mut current = read_text(&settings_path)
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if current
        .get("env")
        .and_then(|e| e.get("ANTHROPIC_AUTH_TOKEN"))
        .is_some()
    {
        env.remove("ANTHROPIC_AUTH_TOKEN");
    }

    let mut merged_env = current
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (k, v) in env {
        merged_env.insert(k, v);
    }
    match body.get("autoCompactWindow").filter(|v| is_truthy(v)) {
        Some(w) => {
            merged_env.insert(
                "CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(),
                Value::String(match w {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }),
            );
        }
        None => {
            merged_env.remove("CLAUDE_CODE_AUTO_COMPACT_WINDOW");
        }
    }

    current.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    current.insert("env".into(), Value::Object(merged_env));

    if let Err(e) = write_text(&settings_path, &pretty(&Value::Object(current))) {
        return ApiError::internal(format!("Failed to update claude settings: {e}"))
            .into_response();
    }
    let mcp = if is_truthy(body.get("exaMcpEnabled").unwrap_or(&Value::Null)) {
        Some(json!({ "exa": { "type": "http", "url": EXA_MCP_URL } }))
    } else {
        None
    };
    if let Err(e) = write_claude_json_mcp(mcp) {
        return ApiError::internal(format!("Failed to update claude settings: {e}"))
            .into_response();
    }
    Json(json!({ "success": true, "message": "Settings updated successfully" })).into_response()
}

/// `DELETE /api/cli-tools/claude-settings`.
pub async fn claude_delete() -> Response {
    let Some(settings_path) = claude_settings_path() else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&settings_path).await;

    let Some(text) = read_text(&settings_path) else {
        return Json(json!({ "success": true, "message": "No settings file to reset" }))
            .into_response();
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return ApiError::internal("Failed to reset claude settings").into_response();
    };
    let mut current = parsed.as_object().cloned().unwrap_or_default();
    if let Some(env) = current.get_mut("env").and_then(Value::as_object_mut) {
        for key in CLAUDE_RESET_ENV_KEYS {
            env.remove(*key);
        }
        if env.is_empty() {
            current.remove("env");
        }
    }
    if let Err(e) = write_text(&settings_path, &pretty(&Value::Object(current))) {
        return ApiError::internal(format!("Failed to reset claude settings: {e}")).into_response();
    }
    if let Err(e) = write_claude_json_mcp(None) {
        return ApiError::internal(format!("Failed to reset claude settings: {e}")).into_response();
    }
    Json(json!({ "success": true, "message": "Settings reset successfully" })).into_response()
}

// ── Codex ─────────────────────────────────────────────────────────────────

fn codex_dir() -> Option<PathBuf> {
    home().map(|h| h.join(".codex"))
}
fn codex_config_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("config.toml"))
}
fn codex_auth_path() -> Option<PathBuf> {
    codex_dir().map(|d| d.join("auth.json"))
}

/// True when the config declares a `9router` model provider.
fn codex_has_9router(config: &str) -> bool {
    config.contains("model_provider = \"9router\"") || config.contains("[model_providers.9router]")
}

/// Set `a.b.c = value` in a `toml_edit` document, creating tables as needed.
fn set_dotted(doc: &mut toml_edit::DocumentMut, dotted: &str, value: toml_edit::Item) {
    let parts: Vec<&str> = dotted.split('.').collect();
    let mut table = doc.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        if !table.contains_key(part) || !table[part].is_table() {
            table[part] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        table = table[part].as_table_mut().expect("just ensured a table");
    }
    table[parts[parts.len() - 1]] = value;
}

fn remove_dotted(doc: &mut toml_edit::DocumentMut, dotted: &str) {
    let parts: Vec<&str> = dotted.split('.').collect();
    let mut table = doc.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        match table.get_mut(part).and_then(|i| i.as_table_mut()) {
            Some(t) => table = t,
            None => return,
        }
    }
    table.remove(parts[parts.len() - 1]);
}

/// `GET /api/cli-tools/codex-settings`.
pub async fn codex_get() -> Response {
    let Some(config_path) = codex_config_path() else {
        return Json(
            json!({ "installed": false, "config": null, "message": "Codex CLI is not installed" }),
        )
        .into_response();
    };
    if !installed("codex", &config_path) {
        return Json(json!({
            "installed": false,
            "config": null,
            "message": "Codex CLI is not installed",
        }))
        .into_response();
    }
    let config = read_text(&config_path);
    let has = config.as_deref().is_some_and(codex_has_9router);
    Json(json!({
        "installed": true,
        "config": config.map_or(Value::Null, Value::String),
        "has9Router": has,
        "configPath": config_path.to_string_lossy(),
    }))
    .into_response()
}

/// `POST /api/cli-tools/codex-settings`.
pub async fn codex_post(body: Result<Json<Value>, JsonRejection>) -> Response {
    let body = match parse_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let str_of = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let (Some(base_url), Some(api_key), Some(model)) =
        (str_of("baseUrl"), str_of("apiKey"), str_of("model"))
    else {
        return ApiError::bad_request("baseUrl, apiKey and model are required").into_response();
    };
    let subagent = str_of("subagentModel").unwrap_or(model);
    let Some(config_path) = codex_config_path() else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&config_path).await;

    let mut doc = read_text(&config_path)
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
        .unwrap_or_default();

    doc["model"] = toml_edit::value(model);
    doc["model_provider"] = toml_edit::value("9router");

    let mut provider = toml_edit::Table::new();
    provider["name"] = toml_edit::value("RustRouter");
    provider["base_url"] = toml_edit::value(normalize_base_url(base_url));
    provider["wire_api"] = toml_edit::value("responses");
    let mut headers = toml_edit::Table::new();
    headers["Authorization"] = toml_edit::value(format!("Bearer {api_key}"));
    provider["http_headers"] = toml_edit::Item::Table(headers);
    set_dotted(
        &mut doc,
        "model_providers.9router",
        toml_edit::Item::Table(provider),
    );

    remove_dotted(&mut doc, "agents.subagent");
    set_dotted(
        &mut doc,
        "agents.default_subagent_model",
        toml_edit::value(subagent),
    );

    if let Err(e) = write_text(&config_path, &doc.to_string()) {
        return ApiError::internal(format!("Failed to update codex settings: {e}")).into_response();
    }
    Json(json!({
        "success": true,
        "message": "Codex settings applied successfully!",
        "configPath": config_path.to_string_lossy(),
    }))
    .into_response()
}

/// `DELETE /api/cli-tools/codex-settings`.
pub async fn codex_delete() -> Response {
    let Some(config_path) = codex_config_path() else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&config_path).await;

    let Some(text) = read_text(&config_path) else {
        return Json(json!({ "success": true, "message": "No config file to reset" }))
            .into_response();
    };
    let mut doc = match text.parse::<toml_edit::DocumentMut>() {
        Ok(d) => d,
        Err(e) => {
            return ApiError::internal(format!("Failed to reset codex settings: {e}"))
                .into_response();
        }
    };

    if doc.get("model_provider").and_then(|i| i.as_str()) == Some("9router") {
        doc.remove("model");
        doc.remove("model_provider");
    }
    remove_dotted(&mut doc, "model_providers.9router");
    remove_dotted(&mut doc, "agents.default_subagent_model");
    remove_dotted(&mut doc, "agents.subagent");

    if let Err(e) = write_text(&config_path, &doc.to_string()) {
        return ApiError::internal(format!("Failed to reset codex settings: {e}")).into_response();
    }

    if let Some(auth_path) = codex_auth_path()
        && let Some(auth_text) = read_text(&auth_path)
        && let Some(mut auth) = serde_json::from_str::<Value>(&auth_text)
            .ok()
            .and_then(|v| v.as_object().cloned())
    {
        auth.remove("OPENAI_API_KEY");
        auth.remove("auth_mode");
        let _ = if auth.is_empty() {
            std::fs::remove_file(&auth_path)
        } else {
            write_text(&auth_path, &pretty(&Value::Object(auth)))
        };
    }

    Json(json!({ "success": true, "message": "RustRouter settings removed successfully" }))
        .into_response()
}

// ── Hermes Agent ──────────────────────────────────────────────────────────

fn hermes_dir() -> Option<PathBuf> {
    home().map(|h| h.join(".hermes"))
}
fn hermes_config_path() -> Option<PathBuf> {
    hermes_dir().map(|d| d.join("config.yaml"))
}
fn hermes_env_path() -> Option<PathBuf> {
    hermes_dir().map(|d| d.join(".env"))
}

/// The `model:` / `delegation:` / `auxiliary:` block regexes and helpers.
/// Editing stays regex-based on purpose: routing the user's hand-edited YAML
/// through a serializer would reorder keys and drop comments.
mod yaml {
    use regex::Regex;
    use std::sync::LazyLock;

    pub static MODEL_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^model:[ \t]*\r?\n((?:[ \t]+.*\r?\n?|[ \t]*\r?\n)*)").unwrap()
    });
    pub static DELEGATION_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^delegation:[ \t]*\r?\n((?:[ \t]+.*\r?\n?|[ \t]*\r?\n)*)").unwrap()
    });
    pub static AUX_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^auxiliary:[ \t]*\r?\n((?:(?:[ \t]+.*\r?\n?)|(?:[ \t]*\r?\n))*)").unwrap()
    });
    pub fn aux_role(role: &str) -> Regex {
        Regex::new(&format!(
            r"(?m)^  {role}:[ \t]*\r?\n(?:(?:[ \t]{{4,}}.*\r?\n?)|(?:[ \t]*\r?\n))*"
        ))
        .unwrap()
    }

    pub fn model_block(model: &str, base: &str) -> String {
        format!(
            "model:\n  default: \"{model}\"\n  provider: \"custom\"\n  base_url: \"{base}\"\n  api_key: ${{OPENAI_API_KEY}}\n"
        )
    }
    pub fn delegation_block(model: &str, base: &str) -> String {
        format!(
            "delegation:\n  model: \"{model}\"\n  provider: \"custom\"\n  base_url: \"{base}\"\n  api_key: ${{OPENAI_API_KEY}}\n"
        )
    }
    pub fn aux_role_block(role: &str, model: &str, base: &str) -> String {
        format!(
            "  {role}:\n    provider: \"custom\"\n    model: \"{model}\"\n    base_url: \"{base}\"\n    api_key: ${{OPENAI_API_KEY}}\n"
        )
    }

    /// Best-effort scalar reader inside a block body.
    fn field(body: &str, key: &str) -> Option<String> {
        let re = Regex::new(&format!(r#"(?m)^[ \t]+{key}:[ \t]*["']?([^"'\r\n]+)["']?"#)).ok()?;
        re.captures(body)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().trim().to_string())
    }

    pub fn parse_model(yaml: &str) -> Option<serde_json::Value> {
        let caps = MODEL_BLOCK.captures(yaml)?;
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        Some(serde_json::json!({
            "default": field(body, "default"),
            "provider": field(body, "provider"),
            "base_url": field(body, "base_url"),
            "api_key": field(body, "api_key"),
        }))
    }

    pub fn parse_delegation(yaml: &str) -> Option<serde_json::Value> {
        let caps = DELEGATION_BLOCK.captures(yaml)?;
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        Some(serde_json::json!({
            "model": field(body, "model"),
            "provider": field(body, "provider"),
            "base_url": field(body, "base_url"),
        }))
    }

    pub fn parse_aux_roles(yaml: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut roles = serde_json::Map::new();
        let Some(caps) = AUX_BLOCK.captures(yaml) else {
            return roles;
        };
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let sub = Regex::new(
            r"(?m)^  ([A-Za-z0-9_]+):[ \t]*\r?\n((?:(?:[ \t]{4,}.*\r?\n?)|(?:[ \t]*\r?\n))*)",
        )
        .unwrap();
        for caps in sub.captures_iter(body) {
            let role = caps.get(1).unwrap().as_str();
            let block = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            roles.insert(
                role.to_string(),
                serde_json::json!({
                    "model": field(block, "model"),
                    "provider": field(block, "provider"),
                    "base_url": field(block, "base_url"),
                }),
            );
        }
        roles
    }

    pub fn upsert_model(yaml: &str, block: &str) -> String {
        if MODEL_BLOCK.is_match(yaml) {
            MODEL_BLOCK.replace(yaml, block).into_owned()
        } else if yaml.is_empty() {
            block.to_string()
        } else {
            format!("{block}\n{yaml}")
        }
    }

    pub fn upsert_delegation(yaml: &str, block: &str) -> String {
        if DELEGATION_BLOCK.is_match(yaml) {
            DELEGATION_BLOCK.replace(yaml, block).into_owned()
        } else if yaml.is_empty() || yaml.ends_with('\n') {
            format!("{yaml}{block}")
        } else {
            format!("{yaml}\n{block}")
        }
    }

    pub fn remove_delegation(yaml: &str) -> String {
        DELEGATION_BLOCK.replace(yaml, "").into_owned()
    }

    pub fn remove_model(yaml: &str) -> String {
        let stripped = MODEL_BLOCK.replace(yaml, "");
        stripped.trim_start_matches('\n').to_string()
    }

    pub fn upsert_aux(yaml: &str, role: &str, block: &str) -> String {
        let role_re = aux_role(role);
        match AUX_BLOCK.captures(yaml) {
            None => {
                let combined = format!("auxiliary:\n{block}");
                if yaml.is_empty() || yaml.ends_with('\n') {
                    format!("{yaml}{combined}")
                } else {
                    format!("{yaml}\n{combined}")
                }
            }
            Some(caps) => {
                let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
                let new_body = if role_re.is_match(body) {
                    role_re.replace(body, block).into_owned()
                } else {
                    format!("{body}{block}")
                };
                AUX_BLOCK
                    .replace(yaml, format!("auxiliary:\n{new_body}"))
                    .into_owned()
            }
        }
    }

    pub fn remove_aux(yaml: &str, role: &str) -> String {
        let Some(caps) = AUX_BLOCK.captures(yaml) else {
            return yaml.to_string();
        };
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let new_body = aux_role(role).replace(body, "").into_owned();
        if new_body.trim().is_empty() {
            AUX_BLOCK.replace(yaml, "").into_owned()
        } else {
            AUX_BLOCK
                .replace(yaml, format!("auxiliary:\n{new_body}"))
                .into_owned()
        }
    }
}

fn hermes_has_9router(cfg: &Value) -> bool {
    let Some(base) = cfg.get("base_url").and_then(Value::as_str) else {
        return false;
    };
    cfg.get("provider").and_then(Value::as_str) == Some("custom")
        && (base.contains("localhost") || base.contains("127.0.0.1") || base.contains("0.0.0.0"))
}

/// `GET /api/cli-tools/hermes-settings`.
pub async fn hermes_get() -> Response {
    let Some(config_path) = hermes_config_path() else {
        return Json(json!({ "installed": false, "settings": null, "message": "Hermes Agent is not installed" }))
            .into_response();
    };
    if !installed("hermes", &config_path) {
        return Json(json!({
            "installed": false,
            "settings": null,
            "message": "Hermes Agent is not installed",
        }))
        .into_response();
    }
    let yaml = read_text(&config_path).unwrap_or_default();
    let model = yaml::parse_model(&yaml);
    let delegation = yaml::parse_delegation(&yaml);
    let auxiliary = Value::Object(yaml::parse_aux_roles(&yaml));
    let has = model.as_ref().is_some_and(hermes_has_9router)
        || delegation.as_ref().is_some_and(hermes_has_9router)
        || auxiliary
            .as_object()
            .is_some_and(|m| m.values().any(hermes_has_9router));
    Json(json!({
        "installed": true,
        "settings": {
            "model": model,
            "delegation": delegation,
            "auxiliary": auxiliary,
        },
        "has9Router": has,
        "configPath": config_path.to_string_lossy(),
    }))
    .into_response()
}

/// `POST /api/cli-tools/hermes-settings`.
pub async fn hermes_post(body: Result<Json<Value>, JsonRejection>) -> Response {
    let body = match parse_body(body).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let Some(base_url) = body
        .get("baseUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return ApiError::bad_request("baseUrl and model are required").into_response();
    };

    // `selections` is the current shape; a bare `model` is the legacy CLI caller.
    let selections: Vec<(String, String)> = match body.get("selections").and_then(Value::as_array) {
        Some(arr)
            if arr
                .iter()
                .any(|s| s.get("role").is_some() && s.get("model").is_some()) =>
        {
            arr.iter()
                .filter_map(|s| {
                    Some((
                        s.get("role")?.as_str()?.to_string(),
                        s.get("model")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        }
        _ => match body.get("model").and_then(Value::as_str) {
            Some(m) => vec![("default".to_string(), m.to_string())],
            None => Vec::new(),
        },
    };
    if !selections.iter().any(|(role, _)| role == "default") {
        return ApiError::bad_request("baseUrl and model are required").into_response();
    }

    let (Some(config_path), Some(env_path)) = (hermes_config_path(), hermes_env_path()) else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&config_path).await;
    let normalized = normalize_base_url(base_url);

    let mut new_yaml = read_text(&config_path).unwrap_or_default();
    for (role, model) in &selections {
        new_yaml = match role.as_str() {
            "default" => yaml::upsert_model(&new_yaml, &yaml::model_block(model, &normalized)),
            "delegation" => {
                yaml::upsert_delegation(&new_yaml, &yaml::delegation_block(model, &normalized))
            }
            other => yaml::upsert_aux(
                &new_yaml,
                other,
                &yaml::aux_role_block(other, model, &normalized),
            ),
        };
    }
    if let Err(e) = write_text(&config_path, &new_yaml) {
        return ApiError::internal(format!("Failed to update hermes settings: {e}"))
            .into_response();
    }

    if let Some(api_key) = body.get("apiKey").and_then(Value::as_str)
        && !api_key.is_empty()
    {
        let existing = read_text(&env_path).unwrap_or_default();
        let re = regex::Regex::new(r"(?m)^OPENAI_API_KEY=.*$").unwrap();
        let line = format!("OPENAI_API_KEY={api_key}");
        let updated = if re.is_match(&existing) {
            re.replace(&existing, line.as_str()).into_owned()
        } else if !existing.is_empty() && !existing.ends_with('\n') {
            format!("{existing}\n{line}\n")
        } else {
            format!("{existing}{line}\n")
        };
        if let Err(e) = write_text(&env_path, &updated) {
            return ApiError::internal(format!("Failed to update hermes settings: {e}"))
                .into_response();
        }
    }

    Json(json!({
        "success": true,
        "message": "Hermes settings applied successfully!",
        "configPath": config_path.to_string_lossy(),
    }))
    .into_response()
}

/// `DELETE /api/cli-tools/hermes-settings`.
pub async fn hermes_delete() -> Response {
    let Some(config_path) = hermes_config_path() else {
        return ApiError::internal("No home directory").into_response();
    };
    let _guard = lock_for(&config_path).await;

    let Some(yaml) = read_text(&config_path) else {
        return Json(json!({ "success": true, "message": "No config file to reset" }))
            .into_response();
    };

    let mut new_yaml = yaml::remove_model(&yaml);
    new_yaml = yaml::remove_delegation(&new_yaml);
    for (role, cfg) in yaml::parse_aux_roles(&yaml) {
        if cfg.get("provider").and_then(Value::as_str) == Some("custom") {
            new_yaml = yaml::remove_aux(&new_yaml, &role);
        }
    }
    new_yaml = new_yaml.trim_start_matches('\n').to_string();

    if let Err(e) = write_text(&config_path, &new_yaml) {
        return ApiError::internal(format!("Failed to reset hermes settings: {e}")).into_response();
    }
    Json(json!({ "success": true, "message": "RustRouter model blocks removed" })).into_response()
}

/// Truthiness as the two body flags the Claude writer reads expect it.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_commas_are_stripped() {
        let cleaned = strip_trailing_commas(r#"{"a":1,"b":[1,2,],}"#);
        assert_eq!(cleaned, r#"{"a":1,"b":[1,2]}"#);
        let parsed: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed["b"][1], json!(2));
        // The regex does not parse strings, so a comma before a brace inside
        // one is stripped too. Only reachable when the strict parse already
        // failed, and kept for parity.
        assert_eq!(strip_trailing_commas(r#"{"a":"x,}"}"#), r#"{"a":"x}"}"#);
    }

    #[test]
    fn base_url_gets_v1_once() {
        assert_eq!(
            normalize_base_url("http://localhost:20129"),
            "http://localhost:20129/v1"
        );
        assert_eq!(
            normalize_base_url("http://localhost:20129/v1"),
            "http://localhost:20129/v1"
        );
    }

    #[test]
    fn codex_toml_round_trips_unknown_keys() {
        let mut doc: toml_edit::DocumentMut = "[other]\nkeep = 1\n".parse().unwrap();
        set_dotted(
            &mut doc,
            "model_providers.9router",
            toml_edit::Item::Table(toml_edit::Table::new()),
        );
        doc["model_provider"] = toml_edit::value("9router");
        let text = doc.to_string();
        assert!(text.contains("keep = 1"));
        assert!(codex_has_9router(&text));
        // `codex_delete` drops the root pointer only when it names 9router.
        if doc.get("model_provider").and_then(|i| i.as_str()) == Some("9router") {
            doc.remove("model_provider");
        }
        remove_dotted(&mut doc, "model_providers.9router");
        assert!(!codex_has_9router(&doc.to_string()));
        assert!(doc.to_string().contains("keep = 1"));
    }

    #[test]
    fn hermes_model_block_upserts_and_removes() {
        let yaml = "other: 1\n";
        let with_model = yaml::upsert_model(yaml, &yaml::model_block("m", "http://x/v1"));
        assert!(with_model.contains("model:"));
        let replaced = yaml::upsert_model(&with_model, &yaml::model_block("m2", "http://x/v1"));
        assert!(replaced.contains("m2"));
        assert_eq!(replaced.matches("model:").count(), 1);
        let removed = yaml::remove_model(&replaced);
        assert!(!removed.contains("default: \"m2\""));
        assert!(removed.contains("other: 1"));
    }

    #[test]
    fn hermes_aux_roles_parse() {
        let yaml = "auxiliary:\n  fast:\n    provider: \"custom\"\n    model: \"x\"\n    base_url: \"http://localhost:20129/v1\"\n";
        let roles = yaml::parse_aux_roles(yaml);
        assert_eq!(roles["fast"]["model"], json!("x"));
        assert!(hermes_has_9router(&roles["fast"]));
    }

    #[test]
    fn js_truthiness_matches() {
        assert!(is_truthy(&json!(true)));
        assert!(is_truthy(&json!("x")));
        assert!(!is_truthy(&json!("")));
        assert!(!is_truthy(&json!(0)));
        assert!(!is_truthy(&Value::Null));
    }
}
