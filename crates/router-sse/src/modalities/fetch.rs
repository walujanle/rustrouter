//! Web-fetch core.
//!
//! Providers kept (per `docs/MODALITIES.md`): **firecrawl**, **tavily**,
//! **exa**. Three provider branches collapse to one `match`, as the doc says.
//!
//! The SSRF guard is not here — the app layer validates the target before it
//! dispatches (`assertPublicUrlResolved` runs first), and the upstream API hosts
//! are admin-configured, not client-supplied. `sanitizeHeaders`'s non-ASCII
//! strip is kept because a header value built from user input can carry
//! characters HTTP cannot encode.

use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::http::ProxyOptions;
use crate::modalities::{
    ModalityBody, ModalityError, ModalityHttp, ModalityResponse, SendFailure, sanitize_headers,
};

/// `DEFAULT_TIMEOUT_MS`.
const DEFAULT_TIMEOUT_MS: u64 = 15_000;
/// `DEFAULT_FORMAT`.
const DEFAULT_FORMAT: &str = "markdown";

/// `handleFetchCore({url, format, maxCharacters, provider, providerConfig, credentials, log})`.
pub async fn fetch_core(
    body: &Value,
    provider_id: &str,
    credentials: Option<&Credentials>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let url = body.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return Err(ModalityError::openai(400, "url is required"));
    }

    let format = body
        .get("format")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_FORMAT);
    let max_characters = body.get("max_characters").and_then(Value::as_i64);

    let Some(config) = crate::modalities::provider_config(provider_id, "fetchConfig") else {
        return Err(ModalityError::openai(
            400,
            format!("Unsupported provider: {provider_id}"),
        ));
    };

    let timeout_ms = config
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    // `credentials?.apiKey || credentials?.key || credentials?.token || ""`. The
    // credential shape `getProviderCredentials` builds carries `apiKey` but no
    // `key`/`token`, so those two fall through to a top-level column read here
    // (the Rust `Credentials.extra`).
    let api_key = credentials
        .and_then(|c| {
            [
                c.api_key.as_deref(),
                c.extra.get("key").and_then(Value::as_str),
                c.extra.get("token").and_then(Value::as_str),
            ]
            .into_iter()
            .flatten()
            .find(|s| !s.is_empty())
        })
        .unwrap_or("");
    let cost_per_query = config.get("costPerQuery").cloned().unwrap_or(Value::Null);
    let started_at = Instant::now();

    let ctx = RunCtx {
        url,
        format,
        timeout_ms,
        api_key,
        max_characters,
        cost_per_query,
        started_at,
    };

    match provider_id {
        "firecrawl" => run_firecrawl(&ctx, proxy_options).await,
        "tavily" => run_tavily(&ctx, proxy_options).await,
        "exa" => run_exa(&ctx, proxy_options).await,
        _ => Err(ModalityError::openai(
            400,
            format!("Unsupported provider: {provider_id}"),
        )),
    }
}

/// The shared inputs each provider runner reads.
struct RunCtx<'a> {
    url: &'a str,
    format: &'a str,
    timeout_ms: u64,
    api_key: &'a str,
    max_characters: Option<i64>,
    cost_per_query: Value,
    started_at: Instant,
}

/// `tryFetch(url, init, timeoutMs)`: the transport half. A transport failure
/// becomes `{ ok: false, timeout, error }` — 504 on abort, else 502.
async fn try_fetch(
    ctx: &RunCtx<'_>,
    provider: &str,
    endpoint: &str,
    auth: &[(&str, String)],
    body: &Value,
    proxy_options: &ProxyOptions,
) -> Result<(Value, String, i64), ModalityError> {
    let upstream_start = Instant::now();
    let mut headers: Vec<(String, String)> =
        vec![("content-type".into(), "application/json".into())];
    headers.extend(auth.iter().map(|(k, v)| ((*k).to_string(), v.clone())));
    let headers = sanitize_headers(&headers);

    let response = ModalityHttp::send(
        "POST",
        endpoint,
        &headers,
        ModalityBody::Json(body),
        proxy_options,
        ctx.timeout_ms,
    )
    .await
    .map_err(|e| match e {
        SendFailure::Timeout => ModalityError::openai(504, e.message()),
        SendFailure::Error(message) => ModalityError::openai(502, message),
    })?;
    let upstream_ms = upstream_start.elapsed().as_millis() as i64;

    // `readJsonOrText(res)`: JSON when the content type says so, text otherwise.
    let (json, text) = if response.content_type.contains("application/json") {
        (response.json().ok(), String::new())
    } else {
        (None, response.text.clone())
    };

    if !response.ok() {
        let error = json
            .as_ref()
            .and_then(|j| j.get("error"))
            .filter(|e| crate::translator::concerns::primitives::js_truthy(e))
            .map(crate::translator::concerns::primitives::js_string)
            .unwrap_or_else(|| format!("{provider} error: {}", response.status));
        return Err(ModalityError::openai(response.status, error));
    }

    Ok((json.unwrap_or(Value::Null), text, upstream_ms))
}

/// `runFirecrawl`.
async fn run_firecrawl(
    ctx: &RunCtx<'_>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let auth: Vec<(&str, String)> = if ctx.api_key.is_empty() {
        vec![]
    } else {
        vec![("authorization", format!("Bearer {}", ctx.api_key))]
    };
    let body = json!({ "url": ctx.url, "formats": [ctx.format] });
    let (json, _, upstream_ms) = try_fetch(
        ctx,
        "Firecrawl",
        "https://api.firecrawl.dev/v1/scrape",
        &auth,
        &body,
        proxy_options,
    )
    .await?;

    let d = json.get("data").cloned().unwrap_or(Value::Null);
    let text = truncate(
        str_or(&d, &["markdown", "html", "text"]).unwrap_or(""),
        ctx.max_characters,
    );
    let title = d.pointer("/metadata/title").cloned().unwrap_or(Value::Null);
    Ok(ModalityResponse::json(build_data(
        "firecrawl",
        ctx,
        title,
        text,
        None,
        upstream_ms,
    )))
}

/// `runTavily`.
async fn run_tavily(
    ctx: &RunCtx<'_>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let auth: Vec<(&str, String)> = if ctx.api_key.is_empty() {
        vec![]
    } else {
        vec![("authorization", format!("Bearer {}", ctx.api_key))]
    };
    let body = json!({ "urls": [ctx.url], "extract_depth": "basic" });
    let (json, _, upstream_ms) = try_fetch(
        ctx,
        "Tavily",
        "https://api.tavily.com/extract",
        &auth,
        &body,
        proxy_options,
    )
    .await?;

    let first = json.pointer("/results/0").cloned().unwrap_or(Value::Null);
    let text = truncate(
        first
            .get("raw_content")
            .and_then(Value::as_str)
            .unwrap_or(""),
        ctx.max_characters,
    );
    Ok(ModalityResponse::json(build_data(
        "tavily",
        ctx,
        Value::Null,
        text,
        None,
        upstream_ms,
    )))
}

/// `runExa`.
async fn run_exa(
    ctx: &RunCtx<'_>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let auth: Vec<(&str, String)> = if ctx.api_key.is_empty() {
        vec![]
    } else {
        vec![("x-api-key", ctx.api_key.to_string())]
    };
    let body = json!({ "ids": [ctx.url], "text": true });
    let (json, _, upstream_ms) = try_fetch(
        ctx,
        "Exa",
        "https://api.exa.ai/contents",
        &auth,
        &body,
        proxy_options,
    )
    .await?;

    let first = json.pointer("/results/0").cloned().unwrap_or(Value::Null);
    let text = truncate(
        first.get("text").and_then(Value::as_str).unwrap_or(""),
        ctx.max_characters,
    );
    let title = first
        .get("title")
        .filter(|t| crate::translator::concerns::primitives::js_truthy(t))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(ModalityResponse::json(build_data(
        "exa",
        ctx,
        title,
        text,
        None,
        upstream_ms,
    )))
}

/// The first non-empty string among `keys` — JS `a || b || c` over strings.
fn str_or<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        item.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
}

/// `truncate(text, max)`: JS `text.slice(0, max)`, skipped when `max` is falsy.
fn truncate(text: &str, max: Option<i64>) -> String {
    match max {
        Some(max) if max > 0 => text.chars().take(max as usize).collect(),
        _ => text.to_string(),
    }
}

/// `buildData({...})`: the unified fetch envelope.
fn build_data(
    provider: &str,
    ctx: &RunCtx<'_>,
    title: Value,
    text: String,
    links: Option<Vec<Value>>,
    upstream_ms: i64,
) -> Value {
    let mut data = Map::new();
    data.insert("provider".into(), json!(provider));
    data.insert("url".into(), json!(ctx.url));
    data.insert(
        "title".into(),
        if crate::translator::concerns::primitives::js_truthy(&title) {
            title
        } else {
            Value::Null
        },
    );
    data.insert(
        "content".into(),
        json!({ "format": ctx.format, "text": text, "length": text.encode_utf16().count() }),
    );
    data.insert(
        "metadata".into(),
        json!({ "author": null, "published_at": null, "language": null }),
    );
    data.insert(
        "usage".into(),
        json!({ "fetch_cost_usd": ctx.cost_per_query }),
    );
    data.insert(
        "metrics".into(),
        json!({
            "response_time_ms": ctx.started_at.elapsed().as_millis() as i64,
            "upstream_latency_ms": upstream_ms,
        }),
    );
    if let Some(links) = links {
        data.insert("links".into(), Value::Array(links));
    }
    Value::Object(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RunCtx<'static> {
        RunCtx {
            url: "https://example.com",
            format: "markdown",
            timeout_ms: 15_000,
            api_key: "k",
            max_characters: None,
            cost_per_query: json!(0.002),
            started_at: Instant::now(),
        }
    }

    #[test]
    fn build_data_has_the_envelope_shape() {
        let data = build_data("firecrawl", &ctx(), json!("Title"), "body".into(), None, 12);
        assert_eq!(data["provider"], json!("firecrawl"));
        assert_eq!(data["title"], json!("Title"));
        assert_eq!(data["content"]["format"], json!("markdown"));
        assert_eq!(data["content"]["length"], json!(4));
        assert_eq!(data["usage"]["fetch_cost_usd"], json!(0.002));
        assert_eq!(data["metrics"]["upstream_latency_ms"], json!(12));
        assert!(data.get("links").is_none(), "links only when provided");
    }

    #[test]
    fn a_falsy_title_becomes_null_and_links_are_kept() {
        let data = build_data(
            "exa",
            &ctx(),
            json!(""),
            "x".into(),
            Some(vec![json!({"url": "https://a.com"})]),
            0,
        );
        assert_eq!(data["title"], Value::Null);
        assert_eq!(data["links"][0]["url"], json!("https://a.com"));
    }

    #[test]
    fn truncate_skips_when_max_is_falsy() {
        assert_eq!(truncate("abcdef", None), "abcdef");
        assert_eq!(truncate("abcdef", Some(0)), "abcdef");
        assert_eq!(truncate("abcdef", Some(3)), "abc");
        assert_eq!(truncate("abcdef", Some(-1)), "abcdef");
    }

    #[test]
    fn str_or_takes_the_first_non_empty() {
        let v = json!({"markdown": "", "html": "<p>", "text": "t"});
        assert_eq!(str_or(&v, &["markdown", "html", "text"]), Some("<p>"));
        assert_eq!(str_or(&v, &["missing"]), None);
    }

    #[tokio::test]
    async fn a_missing_url_is_a_400() {
        let err = fetch_core(&json!({}), "exa", None, &ProxyOptions::default())
            .await
            .unwrap_err();
        assert_eq!(err.status, 400);
        assert_eq!(err.message, "url is required");
    }

    #[tokio::test]
    async fn a_provider_without_fetch_config_is_a_400() {
        let err = fetch_core(
            &json!({"url": "https://example.com"}),
            "mistral",
            None,
            &ProxyOptions::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.status, 400);
        assert!(err.message.contains("Unsupported provider"));
    }
}
