//! The daily models.dev catalog sync and its reader half.
//!
//! Downloads the catalog, keeps only what differs from the hand-written tables,
//! and writes two files next to the database. Failures are swallowed on purpose:
//! a stale or missing file just means the hand tables keep deciding on their own.
//!
//! The reader installs itself into `catalog::catalog`'s `refine` hook so the hot
//! per-request capability lookup picks up the synced values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::catalog::catalog::{self, Capabilities};
use crate::executors::http::tls_builder;
use crate::providers::registry::registry;
use crate::utils::in_flight::InFlightGuard;
use router_db::time::now_ms;

/// `CATALOG_URL`.
const CATALOG_URL: &str = "https://models.dev/api.json";
/// `FETCH_TIMEOUT_MS`.
const FETCH_TIMEOUT_MS: Duration = Duration::from_secs(60);
/// `SYNC_INTERVAL_MS`.
pub const SYNC_INTERVAL_MS: Duration = Duration::from_secs(24 * 60 * 60);
/// `STARTUP_DELAY_MS`.
const STARTUP_DELAY_MS: Duration = Duration::from_secs(60);
/// `RETRY_DELAY_MS`.
const RETRY_DELAY_MS: Duration = Duration::from_secs(30 * 60);
/// `CATALOG_VERSION` — bumped when the file schema changes.
pub const CATALOG_VERSION: i64 = 2;
/// `LIMIT_TOLERANCE`.
const LIMIT_TOLERANCE: f64 = 0.1;

/// `MODALITY_BY_INPUT`: models.dev input modality onto our capability key.
fn modality_key(input: &str) -> Option<&'static str> {
    Some(match input {
        "image" => "vision",
        "pdf" => "pdf",
        "audio" => "audioInput",
        "video" => "videoInput",
        _ => return None,
    })
}

/// `PROVIDER_ALIASES`: 9router id onto its models.dev id.
fn provider_alias(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "kimi" => "moonshotai",
        "kimi-cn" => "moonshotai-cn",
        "zhipu" => "zhipuai",
        "hunyuan" => "tencent",
        "doubao" => "volcengine",
        "cloudflare-ai" => "cloudflare-workers-ai",
        _ => return None,
    })
}

/// `"zai-org/GLM-4.6V:free" -> "glm-4.6v"`.
fn base_id(model_id: &str) -> String {
    let without_vendor = model_id.rsplit('/').next().unwrap_or(model_id);
    without_vendor
        .to_lowercase()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string()
}

fn write_atomic(file: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, file)
}

/// `slim(catalog)`: the trimmed copy the add-models skill reads.
fn slim(catalog: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    for (provider_id, provider) in catalog {
        let mut models = Map::new();
        if let Some(entries) = provider.get("models").and_then(Value::as_object) {
            for (model_id, model) in entries {
                let mut row = Map::new();
                let inputs = model
                    .get("modalities")
                    .and_then(|m| m.get("input"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter(|x| x.as_str() != Some("text"))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                row.insert("i".into(), Value::Array(inputs));
                if let Some(c) = model.get("limit").and_then(|l| l.get("context")) {
                    row.insert("c".into(), c.clone());
                }
                if let Some(o) = model.get("limit").and_then(|l| l.get("output")) {
                    row.insert("o".into(), o.clone());
                }
                if let Some(r) = model.get("reasoning").filter(|v| **v != Value::Bool(false)) {
                    row.insert("r".into(), r.clone());
                }
                models.insert(model_id.clone(), Value::Object(row));
            }
        }
        out.insert(provider_id.clone(), Value::Object(models));
    }
    Value::Object(out)
}

/// One registered model's current capabilities, the baseline `build` compares
/// upstream against.
pub struct Entry {
    provider: String,
    model: String,
    /// `model.contextLength` — the hand table's own number, if it has one.
    context_length: Option<i64>,
    current: Capabilities,
}

/// `collectEntries()`: snapshot every registered model's table-resolved
/// capabilities with the synced catalog detached, so the deltas are relative to
/// the hand tables, not the last sync.
fn collect_entries() -> Vec<Entry> {
    catalog::set_catalog_source(None);
    let mut entries = Vec::new();
    for provider in registry().entries() {
        let Some(models) = &provider.models else {
            continue;
        };
        for model in models {
            entries.push(Entry {
                provider: provider.id.clone(),
                model: model.id.clone(),
                context_length: model.context_length,
                current: catalog::get_capabilities_for_model(Some(&provider.id), &model.id),
            });
        }
    }
    entries
}

/// `build(catalog, entries)`: the models and providers delta tables.
pub fn build(
    catalog: &Map<String, Value>,
    entries: &[Entry],
) -> (Map<String, Value>, Map<String, Value>) {
    // Upstream id -> the local ids it backs.
    let mut local_ids: HashMap<String, Vec<String>> = HashMap::new();
    for e in entries {
        let upstream = provider_alias(&e.provider)
            .unwrap_or(&e.provider)
            .to_string();
        let locals = local_ids.entry(upstream).or_default();
        if !locals.contains(&e.provider) {
            locals.push(e.provider.clone());
        }
    }

    let mut by_provider: HashMap<String, Map<String, Value>> = HashMap::new();
    let mut models: Map<String, Value> = Map::new();
    for (provider_id, provider) in catalog {
        let fallback = vec![provider_id.clone()];
        let locals = local_ids.get(provider_id).unwrap_or(&fallback);
        let mut models_by_id = Map::new();
        let mut seen = std::collections::HashSet::new();
        if let Some(entries) = provider.get("models").and_then(Value::as_object) {
            for (model_id, model) in entries {
                let id = base_id(model_id);
                models_by_id.insert(id.clone(), model.clone());
                if !seen.insert(id.clone()) {
                    continue;
                }
                let mut declared = Map::new();
                if let Some(inputs) = model
                    .get("modalities")
                    .and_then(|m| m.get("input"))
                    .and_then(Value::as_array)
                {
                    for input in inputs {
                        if let Some(key) = input.as_str().and_then(modality_key) {
                            declared.insert(key.into(), Value::Bool(true));
                        }
                    }
                }
                if !declared.is_empty() {
                    let declared = Value::Object(declared);
                    for local in locals {
                        models.insert(format!("{local}:{id}"), declared.clone());
                    }
                    if !locals.contains(provider_id) {
                        models.insert(format!("{provider_id}:{id}"), declared);
                    }
                }
            }
        }
        by_provider.insert(provider_id.clone(), models_by_id);
    }

    let mut providers: Map<String, Value> = Map::new();
    for e in entries {
        let alias = provider_alias(&e.provider);
        let upstream = if catalog.contains_key(&e.provider) {
            Some(e.provider.as_str())
        } else {
            alias.filter(|a| catalog.contains_key(*a))
        };
        let Some(upstream) = upstream else { continue };
        let Some(entry) = by_provider
            .get(upstream)
            .and_then(|m| m.get(&base_id(&e.model)))
        else {
            continue;
        };

        let mut delta = Map::new();
        let context = entry
            .get("limit")
            .and_then(|l| l.get("context"))
            .and_then(Value::as_f64);
        let output = entry
            .get("limit")
            .and_then(|l| l.get("output"))
            .and_then(Value::as_f64);
        // Absent or 0 context length is the guard: a hand table that already
        // names a context length keeps it.
        let has_context_length = e.context_length.is_some_and(|v| v != 0);
        if let Some(c) = context
            && c > 0.0
            && !has_context_length
            && (c - e.current.context_window as f64).abs() / e.current.context_window as f64
                > LIMIT_TOLERANCE
        {
            delta.insert("contextWindow".into(), json!(c as i64));
        }
        if let Some(o) = output
            && o > 0.0
            && (o - e.current.max_output as f64).abs() / e.current.max_output as f64
                > LIMIT_TOLERANCE
        {
            delta.insert("maxOutput".into(), json!(o as i64));
        }
        if !delta.is_empty() {
            providers
                .entry(e.provider.clone())
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("just inserted an object")
                .insert(e.model.clone(), Value::Object(delta));
        }
    }

    (models, providers)
}

/// `getSyncState()`.
#[derive(Debug, Clone)]
pub struct SyncState {
    pub running: bool,
    pub last_sync: Option<i64>,
    pub last_error: Option<String>,
    pub last_result: Option<Value>,
    pub etag: Option<String>,
    pub file_version: Option<i64>,
    pub file: String,
    pub url: String,
    pub interval_ms: i64,
}

struct Inner {
    running: AtomicBool,
    last_sync: AtomicI64,
    last_error: Mutex<Option<String>>,
    last_result: Mutex<Option<Value>>,
    etag: Mutex<Option<String>>,
    file_version: AtomicI64,
    data_dir: PathBuf,
}

static INNER: LazyLock<Inner> = LazyLock::new(|| Inner {
    running: AtomicBool::new(false),
    last_sync: AtomicI64::new(0),
    last_error: Mutex::new(None),
    last_result: Mutex::new(None),
    etag: Mutex::new(None),
    file_version: AtomicI64::new(0),
    data_dir: router_db::paths::resolve_data_dir(),
});

fn catalog_file() -> PathBuf {
    INNER.data_dir.join("model-catalog.json")
}
fn catalog_raw_file() -> PathBuf {
    INNER.data_dir.join("model-catalog-raw.json")
}

/// `getSyncState()`.
pub async fn get_sync_state() -> SyncState {
    SyncState {
        running: INNER.running.load(Ordering::SeqCst),
        last_sync: Some(INNER.last_sync.load(Ordering::SeqCst)).filter(|v| *v > 0),
        last_error: INNER.last_error.lock().await.clone(),
        last_result: INNER.last_result.lock().await.clone(),
        etag: INNER.etag.lock().await.clone(),
        file_version: Some(INNER.file_version.load(Ordering::SeqCst)).filter(|v| *v > 0),
        file: catalog_file().to_string_lossy().to_string(),
        url: CATALOG_URL.to_string(),
        interval_ms: SYNC_INTERVAL_MS.as_millis() as i64,
    }
}

/// `syncModelCatalog()`: one sync, or `None` when it could not complete.
pub async fn sync_model_catalog() -> Option<Value> {
    // The guard clears `running` on drop, so a panic in `run_sync` cannot leave
    // the flag set and disable every later sync for the life of the process.
    let _guard = InFlightGuard::acquire(&INNER.running)?;
    let result = run_sync().await;
    // collect_entries() detaches the reader; put it back whatever happened.
    install_catalog_source().await;
    result
}

async fn run_sync() -> Option<Value> {
    let client = tls_builder().timeout(FETCH_TIMEOUT_MS).build().ok()?;
    let mut req = client.get(CATALOG_URL).header("accept", "application/json");
    let etag = INNER.etag.lock().await.clone();
    let file_version = INNER.file_version.load(Ordering::SeqCst);
    if let Some(tag) = &etag
        && file_version == CATALOG_VERSION
    {
        req = req.header("if-none-match", tag);
    }

    let response = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return fail(format!("{e}")).await;
        }
    };

    let result = if response.status().as_u16() == 304 {
        json!({ "status": "unchanged" })
    } else if !response.status().is_success() {
        return fail(format!("HTTP {}", response.status().as_u16())).await;
    } else {
        let new_etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let Ok(text) = response.text().await else {
            return fail("failed to read body".to_string()).await;
        };
        let Ok(Value::Object(catalog)) = serde_json::from_str::<Value>(&text) else {
            return fail("invalid catalog JSON".to_string()).await;
        };

        let entries = collect_entries();
        let (models, providers) = build(&catalog, &entries);
        let serialized = json!({
            "v": CATALOG_VERSION,
            "etag": new_etag,
            "syncedAt": now_ms(),
            "models": models,
            "providers": providers,
        });
        let text = serde_json::to_string(&serialized).ok()?;
        let bytes = text.len();

        if write_atomic(&catalog_file(), &text).is_err()
            || write_atomic(
                &catalog_raw_file(),
                &serde_json::to_string(&slim(&catalog)).unwrap_or_default(),
            )
            .is_err()
        {
            return fail("failed to write catalog".to_string()).await;
        }

        *INNER.etag.lock().await = new_etag.clone();
        INNER.file_version.store(CATALOG_VERSION, Ordering::SeqCst);
        // Drop the parse cache so the next lookup re-reads the file just written.
        *CATALOG_CACHE.lock().await = None;
        catalog::invalidate_catalog();
        json!({
            "status": "updated",
            "etag": new_etag,
            "bytes": bytes,
            "models": models.len(),
            "providers": providers.len(),
        })
    };

    INNER.last_sync.store(now_ms(), Ordering::SeqCst);
    *INNER.last_error.lock().await = None;
    *INNER.last_result.lock().await = Some(result.clone());
    Some(result)
}

async fn fail(message: String) -> Option<Value> {
    *INNER.last_error.lock().await = Some(message);
    None
}

/// `restoreEtag()`: resume from the etag the last run wrote into the file.
fn restore_etag() {
    let Ok(text) = std::fs::read_to_string(catalog_file()) else {
        return;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let etag = parsed
        .get("etag")
        .and_then(Value::as_str)
        .map(str::to_string);
    let version = parsed.get("v").and_then(Value::as_i64).unwrap_or(1);
    let last = std::fs::metadata(catalog_file())
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    if let Ok(mut e) = INNER.etag.try_lock() {
        *e = etag;
    }
    INNER.file_version.store(version, Ordering::SeqCst);
    INNER.last_sync.store(last, Ordering::SeqCst);
}

/// `startModelCatalogSync()`: schedule the recurring sync. `MODEL_CATALOG_SYNC=off`
/// disables it.
pub fn start_model_catalog_sync() {
    if std::env::var("MODEL_CATALOG_SYNC")
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return;
    }
    restore_etag();
    tokio::spawn(async move {
        let mut delay = STARTUP_DELAY_MS;
        loop {
            tokio::time::sleep(delay).await;
            delay = if sync_model_catalog().await.is_some() {
                SYNC_INTERVAL_MS
            } else {
                RETRY_DELAY_MS
            };
        }
    });
}

/// The parsed catalog, cached against the file's length and mtime.
///
/// The capability lookup runs on the request hot path and the file is ~263 KB,
/// so re-reading and re-parsing it per call is pure waste. The cache key is
/// `(len, mtime_ms)`: the sync rewrites the file through `write_atomic`, so a
/// changed key re-parses and an unchanged one is served from memory.
static CATALOG_CACHE: LazyLock<Mutex<Option<(u64, i64, Value)>>> =
    LazyLock::new(|| Mutex::new(None));

/// `installCatalogSource()`: hand the reader to `catalog::catalog`.
pub async fn install_catalog_source() {
    catalog::set_catalog_source(Some(catalog::CatalogSource {
        get_modalities: |provider, model| {
            let parsed = read_catalog_cached_blocking()?;
            parsed
                .get("models")?
                .get(format!("{provider}:{}", base_id(model)))
                .cloned()
        },
        get_limits: |provider, model| {
            let parsed = read_catalog_cached_blocking()?;
            let by_provider = parsed.get("providers")?.get(provider)?;
            by_provider
                .get(model)
                .or_else(|| by_provider.get(base_id(model).as_str()))
                .cloned()
        },
    }));
}

/// Sync read for the `fn`-pointer reader, which cannot await.
///
/// Uses `try_lock` so a concurrent sync's brief hold falls back to a direct
/// read rather than blocking a request thread; the cache is an optimisation,
/// not a correctness requirement.
fn read_catalog_cached_blocking() -> Option<Value> {
    let meta = std::fs::metadata(catalog_file()).ok()?;
    let key = (
        meta.len(),
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    );
    if let Ok(cache) = CATALOG_CACHE.try_lock()
        && let Some((len, mtime, parsed)) = cache.as_ref()
        && (*len, *mtime) == key
    {
        return Some(parsed.clone());
    }
    let text = std::fs::read_to_string(catalog_file()).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    if let Ok(mut cache) = CATALOG_CACHE.try_lock() {
        *cache = Some((key.0, key.1, parsed.clone()));
    }
    Some(parsed)
}

/// `catalog-sync` route's GET body: the state plus a summary of the file.
pub async fn sync_status() -> Value {
    let state = get_sync_state().await;
    let catalog = std::fs::read_to_string(catalog_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|parsed| {
            json!({
                "syncedAt": parsed.get("syncedAt").cloned().unwrap_or(Value::Null),
                "models": parsed.get("models").and_then(Value::as_object).map_or(0, |m| m.len()),
                "providers": parsed.get("providers").and_then(Value::as_object).map_or(0, |m| m.len()),
                "bytes": std::fs::metadata(catalog_file()).map(|m| m.len()).unwrap_or(0),
            })
        });
    json!({
        "running": state.running,
        "lastSync": state.last_sync,
        "lastError": state.last_error,
        "lastResult": state.last_result,
        "etag": state.etag,
        "fileVersion": state.file_version,
        "file": state.file,
        "url": state.url,
        "intervalMs": state.interval_ms,
        "catalog": catalog,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_id_strips_vendor_and_tag() {
        assert_eq!(base_id("zai-org/GLM-4.6V:free"), "glm-4.6v");
        assert_eq!(base_id("claude-opus-4"), "claude-opus-4");
    }

    #[test]
    fn build_files_modalities_under_local_and_upstream_ids() {
        let catalog = json!({
            "moonshotai": { "models": {
                "kimi-k3": { "modalities": { "input": ["text", "image"] }, "limit": { "context": 200000, "output": 32000 } }
            }}
        })
        .as_object()
        .cloned()
        .unwrap();
        let entries = vec![Entry {
            provider: "kimi".into(),
            model: "kimi-k3".into(),
            context_length: None,
            current: Capabilities {
                vision: false,
                pdf: false,
                audio_input: false,
                video_input: false,
                image_output: false,
                audio_output: false,
                search: false,
                tools: true,
                reasoning: false,
                thinking_format: None,
                thinking_can_disable: false,
                thinking_range: None,
                thinking_effort_supported: false,
                context_window: 100_000,
                max_output: 32_000,
            },
        }];
        let (models, providers) = build(&catalog, &entries);
        // `kimi` aliases to `moonshotai`, so the modality lands under both.
        assert_eq!(models["kimi:kimi-k3"]["vision"], json!(true));
        assert_eq!(models["moonshotai:kimi-k3"]["vision"], json!(true));
        // 200000 vs 100000 is a 100% delta, well over the 10% tolerance.
        assert_eq!(providers["kimi"]["kimi-k3"]["contextWindow"], json!(200000));
        // Output matches, so it is not emitted.
        assert!(providers["kimi"]["kimi-k3"].get("maxOutput").is_none());
    }

    #[test]
    fn slim_drops_text_and_shortens_keys() {
        let catalog = json!({
            "p": { "models": { "m": { "modalities": { "input": ["text", "image"] }, "limit": { "context": 10, "output": 5 }, "reasoning": true } } }
        })
        .as_object()
        .cloned()
        .unwrap();
        let out = slim(&catalog);
        assert_eq!(out["p"]["m"]["i"], json!(["image"]));
        assert_eq!(out["p"]["m"]["c"], json!(10));
        assert_eq!(out["p"]["m"]["r"], json!(true));
    }
}
