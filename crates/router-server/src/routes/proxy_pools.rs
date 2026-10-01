//! `proxy-pools/*`: the proxy-pool CRUD, test and edge-deploy routes.
//!
//! A pool is a shared outbound proxy that connections point at by id through
//! `providerSpecificData.proxyPoolId`. `http` pools are probed with a real
//! request through the proxy; `vercel`/`cloudflare`/`deno` pools are edge relay
//! workers that take the target in headers, so they are probed with a relay
//! request to httpbin instead.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::time::{Duration, Instant};

use router_sse::services::proxy_test::test_proxy_url;

use crate::error::ApiError;
use crate::state::AppState;

const VALID_PROXY_TYPES: [&str; 4] = ["http", "vercel", "cloudflare", "deno"];
/// The update path accepts a narrower set: `deno` is not editable here.
const VALID_UPDATE_TYPES: [&str; 3] = ["http", "vercel", "cloudflare"];

/// `toBoolean`: only the literal strings count; anything else is `undefined`.
fn to_boolean(value: Option<&str>) -> Option<bool> {
    match value {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// `buildUsageMap(connections)`: pool id to bound-connection count.
fn build_usage_map(connections: &[Value]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for connection in connections {
        let pool_id = connection
            .get("providerSpecificData")
            .and_then(|p| p.get("proxyPoolId"))
            .and_then(Value::as_str);
        if let Some(pool_id) = pool_id.filter(|s| !s.is_empty()) {
            *map.entry(pool_id.to_string()).or_insert(0) += 1;
        }
    }
    map
}

/// `GET /api/proxy-pools`.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<Map<String, Value>>,
) -> Response {
    let is_active = to_boolean(query.get("isActive").and_then(Value::as_str));
    let include_usage = query.get("includeUsage").and_then(Value::as_str) == Some("true");

    let result = state
        .read(move |conn| {
            let pools = router_db::repos::proxy_pools::get_proxy_pools(conn, is_active, None)?;
            if !include_usage {
                return Ok(pools);
            }
            let connections =
                router_db::repos::connections::get_provider_connections(conn, None, None)?;
            let usage = build_usage_map(&connections);
            Ok(pools
                .into_iter()
                .map(|mut pool| {
                    let count = pool
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| usage.get(id))
                        .copied()
                        .unwrap_or(0);
                    if let Value::Object(map) = &mut pool {
                        map.insert("boundConnectionCount".into(), json!(count));
                    }
                    pool
                })
                .collect())
        })
        .await;

    match result {
        Ok(proxy_pools) => Json(json!({ "proxyPools": proxy_pools })).into_response(),
        Err(_) => ApiError::internal("Failed to fetch proxy pools").into_response(),
    }
}

/// `POST /api/proxy-pools`.
pub async fn create(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to create proxy pool").into_response();
    };
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() {
        return ApiError::bad_request("Name is required").into_response();
    }
    let proxy_url = payload
        .get("proxyUrl")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if proxy_url.is_empty() {
        return ApiError::bad_request("Proxy URL is required").into_response();
    }
    let no_proxy = payload
        .get("noProxy")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let is_active = match payload.get("isActive") {
        None => true,
        Some(v) => v == &Value::Bool(true),
    };
    let strict_proxy = payload.get("strictProxy") == Some(&Value::Bool(true));
    let pool_type = payload
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| VALID_PROXY_TYPES.contains(t))
        .unwrap_or("http");

    let mut data = Map::new();
    data.insert("name".into(), json!(name));
    data.insert("proxyUrl".into(), json!(proxy_url));
    data.insert("noProxy".into(), json!(no_proxy));
    data.insert("type".into(), json!(pool_type));
    data.insert("isActive".into(), json!(is_active));
    data.insert("strictProxy".into(), json!(strict_proxy));
    let data = Value::Object(data);

    match state
        .write(move |tx| router_db::repos::proxy_pools::create_proxy_pool(tx, &data))
        .await
    {
        Ok(proxy_pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": proxy_pool })),
        )
            .into_response(),
        Err(_) => ApiError::internal("Failed to create proxy pool").into_response(),
    }
}

/// `GET /api/proxy-pools/{id}`.
pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state
        .read(move |conn| router_db::repos::proxy_pools::get_proxy_pool_by_id(conn, &id))
        .await
    {
        Ok(Some(proxy_pool)) => Json(json!({ "proxyPool": proxy_pool })).into_response(),
        Ok(None) => ApiError::not_found("Proxy pool not found").into_response(),
        Err(_) => ApiError::internal("Failed to fetch proxy pool").into_response(),
    }
}

/// `PUT /api/proxy-pools/{id}`.
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Failed to update proxy pool").into_response();
    };
    let Some(payload) = payload.as_object() else {
        return ApiError::internal("Failed to update proxy pool").into_response();
    };

    let mut updates = Map::new();
    if payload.contains_key("name") {
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            return ApiError::bad_request("Name is required").into_response();
        }
        updates.insert("name".into(), json!(name));
    }
    if payload.contains_key("proxyUrl") {
        let url = payload
            .get("proxyUrl")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if url.is_empty() {
            return ApiError::bad_request("Proxy URL is required").into_response();
        }
        updates.insert("proxyUrl".into(), json!(url));
    }
    if payload.contains_key("noProxy") {
        let no_proxy = payload
            .get("noProxy")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        updates.insert("noProxy".into(), json!(no_proxy));
    }
    if payload.contains_key("isActive") {
        updates.insert(
            "isActive".into(),
            json!(payload.get("isActive") == Some(&Value::Bool(true))),
        );
    }
    if payload.contains_key("strictProxy") {
        updates.insert(
            "strictProxy".into(),
            json!(payload.get("strictProxy") == Some(&Value::Bool(true))),
        );
    }
    if payload.contains_key("type") {
        let pool_type = payload
            .get("type")
            .and_then(Value::as_str)
            .filter(|t| VALID_UPDATE_TYPES.contains(t))
            .unwrap_or("http");
        updates.insert("type".into(), json!(pool_type));
    }
    let updates = Value::Object(updates);

    let result = state
        .write(move |tx| {
            if router_db::repos::proxy_pools::get_proxy_pool_by_id(tx, &id)?.is_none() {
                return Ok(None);
            }
            router_db::repos::proxy_pools::update_proxy_pool(tx, &id, &updates)
        })
        .await;
    match result {
        Ok(Some(proxy_pool)) => Json(json!({ "proxyPool": proxy_pool })).into_response(),
        Ok(None) => ApiError::not_found("Proxy pool not found").into_response(),
        Err(_) => ApiError::internal("Failed to update proxy pool").into_response(),
    }
}

/// `DELETE /api/proxy-pools/{id}`: refuses while connections still bind it.
pub async fn delete(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let result = state
        .write(move |tx| {
            if router_db::repos::proxy_pools::get_proxy_pool_by_id(tx, &id)?.is_none() {
                return Ok(None);
            }
            let connections =
                router_db::repos::connections::get_provider_connections(tx, None, None)?;
            let bound = connections
                .iter()
                .filter(|c| {
                    c.get("providerSpecificData")
                        .and_then(|p| p.get("proxyPoolId"))
                        .and_then(Value::as_str)
                        == Some(id.as_str())
                })
                .count() as i64;
            if bound > 0 {
                return Ok(Some(bound));
            }
            router_db::repos::proxy_pools::delete_proxy_pool(tx, &id)?;
            Ok(Some(0))
        })
        .await;
    match result {
        Ok(Some(0)) => Json(json!({ "success": true })).into_response(),
        Ok(None) => ApiError::not_found("Proxy pool not found").into_response(),
        Ok(Some(bound)) => ApiError::new(StatusCode::CONFLICT, "Proxy pool is currently in use")
            .with_extra(json!({ "boundConnectionCount": bound }))
            .into_response(),
        Err(_) => ApiError::internal("Failed to delete proxy pool").into_response(),
    }
}

/// `testVercelRelay(relayUrl)`: a relay worker probe with a 10 s cap.
async fn test_relay(relay_url: &str) -> Value {
    let started = Instant::now();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => return json!({ "ok": false, "status": 500, "error": e.to_string() }),
    };
    match client
        .get(relay_url)
        .header("x-relay-target", "https://httpbin.org")
        .header("x-relay-path", "/get")
        .send()
        .await
    {
        Ok(res) => json!({
            "ok": res.status().is_success(),
            "status": res.status().as_u16(),
            "statusText": res.status().canonical_reason().unwrap_or(""),
            "elapsedMs": started.elapsed().as_millis() as u64,
        }),
        Err(e) => json!({
            "ok": false,
            "status": 500,
            "error": if e.is_timeout() { "Relay test timed out".to_string() } else { e.to_string() },
        }),
    }
}

/// `POST /api/proxy-pools/{id}/test`.
pub async fn test(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let pool = match state
        .read({
            let id = id.clone();
            move |conn| router_db::repos::proxy_pools::get_proxy_pool_by_id(conn, &id)
        })
        .await
    {
        Ok(Some(pool)) => pool,
        Ok(None) => return ApiError::not_found("Proxy pool not found").into_response(),
        Err(_) => return ApiError::internal("Failed to test proxy pool").into_response(),
    };

    let pool_type = pool.get("type").and_then(Value::as_str).unwrap_or("");
    let proxy_url = pool.get("proxyUrl").and_then(Value::as_str).unwrap_or("");
    let result = if matches!(pool_type, "vercel" | "cloudflare" | "deno") {
        test_relay(proxy_url).await
    } else {
        test_proxy_url(Some(proxy_url), None, None).await
    };

    let ok = result.get("ok") == Some(&Value::Bool(true));
    let now = router_db::time::now_iso();
    let last_error = if ok {
        Value::Null
    } else {
        result
            .get("error")
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| {
                json!(format!(
                    "Proxy test failed with status {}",
                    result.get("status").and_then(Value::as_u64).unwrap_or(0)
                ))
            })
    };

    let mut patch = Map::new();
    patch.insert(
        "testStatus".into(),
        json!(if ok { "active" } else { "error" }),
    );
    patch.insert("lastTestedAt".into(), json!(now));
    patch.insert("lastError".into(), last_error);
    patch.insert("isActive".into(), json!(ok));
    let patch = Value::Object(patch);

    let write = state
        .write({
            let id = id.clone();
            move |tx| router_db::repos::proxy_pools::update_proxy_pool(tx, &id, &patch)
        })
        .await;
    if write.is_err() {
        return ApiError::internal("Failed to test proxy pool").into_response();
    }

    Json(json!({
        "ok": ok,
        "status": result.get("status").cloned().unwrap_or(Value::Null),
        "statusText": result.get("statusText").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null),
        "error": result.get("error").cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null),
        "elapsedMs": result.get("elapsedMs").and_then(Value::as_u64).unwrap_or(0),
        "testedAt": now,
    }))
    .into_response()
}

// --- relay deploys -------------------------------------------------------

const VERCEL_API: &str = "https://api.vercel.com";
const DENO_V2_API: &str = "https://api.deno.com/v2";

const VERCEL_RELAY_CODE: &str = r#"
export const config = { runtime: "edge" };

export default async function handler(req) {
  const target = req.headers.get("x-relay-target");
  const relayPath = req.headers.get("x-relay-path") || "/";
  if (!target) {
    return new Response(JSON.stringify({ error: "Missing x-relay-target header" }), {
      status: 400,
      headers: { "content-type": "application/json" },
    });
  }

  const targetUrl = target.replace(/\/$/, "") + relayPath;

  const rawHeaders = {};
  for (const [k, v] of req.headers.entries()) rawHeaders[k] = v;
  delete rawHeaders["x-relay-target"];
  delete rawHeaders["x-relay-path"];
  delete rawHeaders["host"];

  const response = await fetch(targetUrl, {
    method: req.method,
    headers: rawHeaders,
    body: req.method !== "GET" && req.method !== "HEAD" ? req.body : undefined,
    duplex: "half",
  });

  return new Response(response.body, {
    status: response.status,
    headers: response.headers,
  });
}
"#;

const CLOUDFLARE_RELAY_CODE: &str = r#"
export default {
  async fetch(request, env, ctx) {
    const target = request.headers.get("x-relay-target");
    const relayPath = request.headers.get("x-relay-path") || "/";

    if (!target) {
      return new Response(JSON.stringify({ error: "Missing x-relay-target header" }), {
        status: 400,
        headers: { "content-type": "application/json" },
      });
    }

    const targetUrl = target.replace(/\/$/, "") + relayPath;
    const newRequestInit = {
      method: request.method,
      headers: new Headers(request.headers),
    };

    if (request.method !== "GET" && request.method !== "HEAD") {
      newRequestInit.body = request.body;
      newRequestInit.duplex = "half";
    }

    newRequestInit.headers.delete("x-relay-target");
    newRequestInit.headers.delete("x-relay-path");
    newRequestInit.headers.delete("host");
    // The relay only moves bytes, so ask the origin for the body as-is. A
    // compressed origin body is the one case where the copied framing headers
    // can disagree with the body `fetch` hands back, and a response the edge
    // cannot frame is what it reports as a 520.
    newRequestInit.headers.set("accept-encoding", "identity");

    try {
      const response = await fetch(targetUrl, newRequestInit);
      // `fetch` may already have decoded the body while the copied
      // `content-encoding`/`content-length` still describe the encoded one.
      // Handing the edge a body whose framing contradicts it is the 520.
      // `connection` and `transfer-encoding` are hop-by-hop, and a proxy must
      // not forward them at all.
      const headers = new Headers(response.headers);
      headers.delete("content-encoding");
      headers.delete("content-length");
      headers.delete("transfer-encoding");
      headers.delete("connection");
      headers.delete("keep-alive");
      return new Response(response.body, {
        status: response.status,
        headers,
      });
    } catch (error) {
      return new Response(JSON.stringify({ error: error.message }), {
        status: 502,
        headers: { "content-type": "application/json" },
      });
    }
  },
};
"#;

const DENO_RELAY_CODE: &str = r#"Deno.serve(async (request) => {
  const target = request.headers.get("x-relay-target");
  const relayPath = request.headers.get("x-relay-path") || "/";

  if (!target) {
    return new Response(JSON.stringify({ error: "Missing x-relay-target header" }), {
      status: 400,
      headers: { "content-type": "application/json" },
    });
  }

  const targetUrl = target.replace(/\/$/, "") + relayPath;
  const newHeaders = new Headers(request.headers);
  newHeaders.delete("x-relay-target");
  newHeaders.delete("x-relay-path");
  newHeaders.delete("host");

  const init = {
    method: request.method,
    headers: newHeaders,
  };

  if (request.method !== "GET" && request.method !== "HEAD") {
    init.body = request.body;
    init.duplex = "half";
  }

  try {
    const response = await fetch(targetUrl, init);
    return new Response(response.body, {
      status: response.status,
      headers: response.headers,
    });
  } catch (error) {
    return new Response(JSON.stringify({ error: error.message }), {
      status: 502,
      headers: { "content-type": "application/json" },
    });
  }
});"#;

/// `relay-${Date.now().toString(36)}`.
fn default_project_name() -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0) as u64;
    format!("relay-{}", to_base36(millis))
}

fn to_base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Pull `error.message` out of a provider error body, falling back to `fallback`.
fn provider_error(body: &Value, fallback: &str) -> String {
    body.get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

/// Store the deployed relay URL as an active pool and return it with the URL.
async fn save_deploy(
    state: &AppState,
    name: String,
    deploy_url: String,
    pool_type: &'static str,
) -> Response {
    let mut data = Map::new();
    data.insert("name".into(), json!(name));
    data.insert("proxyUrl".into(), json!(deploy_url));
    data.insert("type".into(), json!(pool_type));
    data.insert("noProxy".into(), json!(""));
    data.insert("isActive".into(), json!(true));
    data.insert("strictProxy".into(), json!(false));
    let data = Value::Object(data);
    match state
        .write(move |tx| router_db::repos::proxy_pools::create_proxy_pool(tx, &data))
        .await
    {
        Ok(proxy_pool) => (
            StatusCode::CREATED,
            Json(json!({ "proxyPool": proxy_pool, "deployUrl": deploy_url })),
        )
            .into_response(),
        Err(_) => ApiError::internal("Deploy failed").into_response(),
    }
}

/// `POST /api/proxy-pools/vercel-deploy`.
pub async fn vercel_deploy(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Deploy failed").into_response();
    };
    let Some(token) = payload.get("vercelToken").and_then(Value::as_str) else {
        return ApiError::bad_request("Vercel API token is required").into_response();
    };
    let project_name = payload
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_project_name);

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(150))
        .build()
    else {
        return ApiError::internal("Deploy failed").into_response();
    };

    let deploy_body = json!({
        "name": project_name,
        "files": [
            { "file": "api/relay.js", "data": VERCEL_RELAY_CODE },
            { "file": "package.json", "data": json!({ "name": project_name, "version": "1.0.0" }).to_string() },
            { "file": "vercel.json", "data": json!({ "rewrites": [{ "source": "/(.*)", "destination": "/api/relay" }] }).to_string() },
        ],
        "projectSettings": { "framework": Value::Null },
        "target": "production",
    });
    let deploy_res = client
        .post(format!("{VERCEL_API}/v13/deployments"))
        .bearer_auth(token)
        .json(&deploy_body)
        .send()
        .await;
    let deploy_res = match deploy_res {
        Ok(r) => r,
        Err(e) => return ApiError::internal(e.to_string()).into_response(),
    };
    if !deploy_res.status().is_success() {
        let status = deploy_res.status().as_u16();
        let err: Value = deploy_res.json().await.unwrap_or(json!({}));
        return ApiError::new(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            provider_error(&err, "Failed to create Vercel deployment"),
        )
        .into_response();
    }
    let deployment: Value = deploy_res.json().await.unwrap_or(json!({}));
    let deployment_id = deployment
        .get("id")
        .or_else(|| deployment.get("uid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let project_id = deployment
        .get("projectId")
        .and_then(Value::as_str)
        .unwrap_or(&project_name)
        .to_string();
    let _ = client
        .patch(format!("{VERCEL_API}/v9/projects/{project_id}"))
        .bearer_auth(token)
        .json(&json!({ "ssoProtection": Value::Null }))
        .send()
        .await;

    // Poll to READY, 3 s apart, 120 s cap.
    let started = Instant::now();
    let url = loop {
        if started.elapsed() > Duration::from_secs(120) {
            return ApiError::internal("Deployment timed out").into_response();
        }
        let res = client
            .get(format!("{VERCEL_API}/v13/deployments/{deployment_id}"))
            .bearer_auth(token)
            .send()
            .await;
        let Ok(res) = res else {
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        let data: Value = res.json().await.unwrap_or(json!({}));
        match data.get("readyState").and_then(Value::as_str) {
            Some("READY") => {
                break data
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
            }
            Some(state) if state == "ERROR" || state == "CANCELED" => {
                return ApiError::internal(format!("Deployment failed: {state}")).into_response();
            }
            _ => tokio::time::sleep(Duration::from_secs(3)).await,
        }
    };

    save_deploy(&state, project_name, format!("https://{url}"), "vercel").await
}

/// `POST /api/proxy-pools/cloudflare-deploy`.
pub async fn cloudflare_deploy(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Deploy failed").into_response();
    };
    let account_id = payload
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let api_token = payload
        .get("apiToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if account_id.is_empty() || api_token.is_empty() {
        return ApiError::bad_request("Cloudflare Account ID and API Token are required")
            .into_response();
    }
    let project_name = payload
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_project_name);

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
    else {
        return ApiError::internal("Deploy failed").into_response();
    };
    let script_url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/workers/scripts/{project_name}"
    );

    let script_part = reqwest::multipart::Part::bytes(CLOUDFLARE_RELAY_CODE.as_bytes().to_vec())
        .file_name("index.js")
        .mime_str("application/javascript+module")
        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
    let metadata = json!({
        "main_module": "index.js",
        "compatibility_date": "2024-03-20",
        "observability": { "enabled": true },
    });
    let metadata_part = reqwest::multipart::Part::bytes(metadata.to_string().into_bytes())
        .file_name("metadata.json")
        .mime_str("application/json")
        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
    let form = reqwest::multipart::Form::new()
        .part("index.js", script_part)
        .part("metadata", metadata_part);

    let upload = client
        .put(&script_url)
        .bearer_auth(api_token)
        .multipart(form)
        .send()
        .await;
    let upload = match upload {
        Ok(r) => r,
        Err(e) => return ApiError::internal(e.to_string()).into_response(),
    };
    if !upload.status().is_success() {
        let status = upload.status().as_u16();
        let err: Value = upload.json().await.unwrap_or(json!({}));
        let message = err
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Failed to upload Worker to Cloudflare")
            .to_string();
        return ApiError::new(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            message,
        )
        .into_response();
    }

    let _ = client
        .post(format!("{script_url}/subdomain"))
        .bearer_auth(api_token)
        .json(&json!({ "enabled": true }))
        .send()
        .await;

    let mut deploy_url = String::new();
    if let Ok(res) = client
        .get(format!(
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/workers/subdomain"
        ))
        .bearer_auth(api_token)
        .json(&json!({}))
        .send()
        .await
        && res.status().is_success()
    {
        let data: Value = res.json().await.unwrap_or(json!({}));
        if let Some(subdomain) = data.pointer("/result/subdomain").and_then(Value::as_str) {
            deploy_url = format!("https://{project_name}.{subdomain}.workers.dev");
        }
    }
    if deploy_url.is_empty() {
        return ApiError::bad_request(
            "Worker deployed but failed to retrieve workers.dev subdomain. Make sure you have setup a workers.dev subdomain in Cloudflare Dashboard.",
        )
        .into_response();
    }

    save_deploy(&state, project_name, deploy_url, "cloudflare").await
}

/// `POST /api/proxy-pools/deno-deploy`.
pub async fn deno_deploy(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(payload)) = body else {
        return ApiError::internal("Deploy failed").into_response();
    };
    let org_domain = payload
        .get("orgDomain")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if org_domain.is_empty() {
        return ApiError::bad_request("Organization domain is required").into_response();
    }
    let deno_token = payload
        .get("denoToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if deno_token.is_empty() {
        return ApiError::bad_request("Deno Deploy API token is required").into_response();
    }
    let project_name = payload
        .get("projectName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(default_project_name);

    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
    else {
        return ApiError::internal("Deploy failed").into_response();
    };

    let create = client
        .post(format!("{DENO_V2_API}/apps"))
        .bearer_auth(deno_token)
        .json(&json!({
            "slug": project_name,
            "labels": { "custom.kind": "9router-relay" },
            "config": {
                "install": "deno install",
                "runtime": { "type": "dynamic", "entrypoint": "main.ts" },
            },
        }))
        .send()
        .await;
    let create = match create {
        Ok(r) => r,
        Err(e) => return ApiError::internal(e.to_string()).into_response(),
    };
    if !create.status().is_success() {
        let status = create.status().as_u16();
        let text = create.text().await.unwrap_or_default();
        if status == 409 {
            return ApiError::new(
                StatusCode::CONFLICT,
                format!("App \"{project_name}\" already exists. Choose a different name."),
            )
            .into_response();
        }
        return ApiError::new(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            format!("Failed to create app ({status}): {text}"),
        )
        .into_response();
    }
    let app: Value = create.json().await.unwrap_or(json!({}));
    let app_id = app
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let deploy = client
        .post(format!("{DENO_V2_API}/apps/{app_id}/deploy"))
        .bearer_auth(deno_token)
        .json(&json!({
            "assets": {
                "main.ts": { "kind": "file", "content": DENO_RELAY_CODE, "encoding": "utf-8" },
            },
        }))
        .send()
        .await;
    let deploy = match deploy {
        Ok(r) => r,
        Err(e) => return ApiError::internal(e.to_string()).into_response(),
    };
    if !deploy.status().is_success() {
        let status = deploy.status().as_u16();
        let text = deploy.text().await.unwrap_or_default();
        let _ = client
            .delete(format!("{DENO_V2_API}/apps/{app_id}"))
            .bearer_auth(deno_token)
            .send()
            .await;
        return ApiError::new(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            format!("Deploy failed ({status}): {text}"),
        )
        .into_response();
    }
    let revision: Value = deploy.json().await.unwrap_or(json!({}));
    let revision_id = revision
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut status = revision
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // 30 * 2 s = 60 s cap.
    let mut attempts = 0;
    while status == "queued" || status == "building" {
        if attempts >= 30 {
            return ApiError::internal("Deploy timed out after 60 seconds").into_response();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(res) = client
            .get(format!("{DENO_V2_API}/revisions/{revision_id}"))
            .bearer_auth(deno_token)
            .send()
            .await
        else {
            break;
        };
        if !res.status().is_success() {
            break;
        }
        let data: Value = res.json().await.unwrap_or(json!({}));
        status = data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        attempts += 1;
    }

    if status != "succeeded" {
        let _ = client
            .delete(format!("{DENO_V2_API}/apps/{app_id}"))
            .bearer_auth(deno_token)
            .send()
            .await;
        return ApiError::internal(format!("Deploy failed with status: {status}")).into_response();
    }

    let org_slug = org_domain.split('.').next().unwrap_or("");
    let deploy_url = format!("https://{project_name}.{org_slug}.deno.net");
    save_deploy(&state, project_name, deploy_url, "deno").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_boolean_only_accepts_literal_strings() {
        assert_eq!(to_boolean(Some("true")), Some(true));
        assert_eq!(to_boolean(Some("false")), Some(false));
        assert_eq!(to_boolean(Some("1")), None);
        assert_eq!(to_boolean(None), None);
    }

    #[test]
    fn usage_map_skips_empty_and_missing_pool_ids() {
        let connections = vec![
            json!({ "providerSpecificData": { "proxyPoolId": "a" } }),
            json!({ "providerSpecificData": { "proxyPoolId": "a" } }),
            json!({ "providerSpecificData": { "proxyPoolId": "" } }),
            json!({ "providerSpecificData": {} }),
            json!({}),
        ];
        let map = build_usage_map(&connections);
        assert_eq!(map.get("a"), Some(&2));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn base36_matches_js_number_tostring() {
        assert_eq!(to_base36(0), "0");
        assert_eq!(to_base36(35), "z");
        assert_eq!(to_base36(36), "10");
    }

    /// A relay response whose copied framing contradicts its body is what the
    /// Cloudflare edge reports as a 520, so the worker must strip those headers
    /// and ask the origin for an unencoded body.
    #[test]
    fn cloudflare_relay_strips_body_framing_headers() {
        for header in [
            "content-encoding",
            "content-length",
            "transfer-encoding",
            "connection",
            "keep-alive",
        ] {
            assert!(
                CLOUDFLARE_RELAY_CODE.contains(&format!("headers.delete(\"{header}\")")),
                "worker does not strip {header}"
            );
        }
        assert!(CLOUDFLARE_RELAY_CODE.contains("set(\"accept-encoding\", \"identity\")"));
    }
}
