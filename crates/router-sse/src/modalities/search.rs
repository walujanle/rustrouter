//! Search core.
//!
//! Providers kept (per `docs/MODALITIES.md`): **brave-search**, **exa**,
//! **linkup**, **tavily**, **youcom**. The five builders and five normalizers
//! become two `match` statements — a trait with five impls would be more code
//! than the matches, for a fixed set.
//!
//! The **15 s global deadline** is load-bearing: it spans the dedicated
//! attempt, not each request.
//!
//! There is no chat-search lane. No shipped provider declares `searchViaChat`,
//! so a provider without a `searchConfig` has no search support at all.

use std::time::Instant;

use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::http::ProxyOptions;
use crate::modalities::ssrf::{PublicResponse, fetch_public};
use crate::modalities::{
    ModalityError, ModalityResponse, finite_number, provider_config, sanitize_headers,
};
use crate::translator::concerns::primitives::js_truthy;

/// `GLOBAL_TIMEOUT_MS`.
const GLOBAL_TIMEOUT_MS: u64 = 15_000;

/// `handleSearchCore({body, provider, providerConfig, credentials, log})`.
///
/// `provider_id` must already be alias-resolved. `body` is the sanitized body
/// the route layer forwards (`query` already trimmed by the app layer).
pub async fn search_core(
    body: &Value,
    provider_id: &str,
    credentials: Option<&Credentials>,
    proxy_options: &ProxyOptions,
) -> Result<ModalityResponse, ModalityError> {
    let global_start = Instant::now();

    // 1. Sanitize query.
    let raw_query = body.get("query").and_then(Value::as_str).unwrap_or("");
    let clean = sanitize_query(raw_query).map_err(|e| ModalityError::search(400, e))?;
    let mut normalized_body = body.clone();
    if let Some(obj) = normalized_body.as_object_mut() {
        obj.insert("query".into(), json!(clean));
    }

    let config = provider_config(provider_id, "searchConfig");

    // 2. Route: the dedicated search API. There is no chat-search lane — no
    // shipped provider declares `searchViaChat` — so a provider without a
    // `searchConfig` has no lane at all.
    let Some(config) = config else {
        return Err(ModalityError::search(
            400,
            format!("Provider {provider_id} does not support web search"),
        ));
    };
    try_dedicated_provider(
        provider_id,
        config,
        &normalized_body,
        credentials,
        proxy_options,
        global_start,
    )
    .await
}

/// `sanitizeQuery(query)`: reject control characters, NFKC-normalize, collapse
/// whitespace.
///
/// `String::normalize` is not in std, and pulling in `unicode-normalization` for
/// one call is not worth a dependency: NFKC only changes compatibility forms
/// (full-width digits, ligatures), which a search query rarely carries. The
/// control-character and whitespace behaviour is preserved. `ponytail:` add
/// `unicode-normalization` if a caller ever needs compatibility folding.
pub fn sanitize_query(query: &str) -> Result<String, String> {
    let has_control = query.chars().any(|c| {
        let n = c as u32;
        matches!(n, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F)
    });
    if has_control {
        return Err("Query contains invalid control characters".to_string());
    }
    let clean = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.is_empty() {
        return Err("Query is empty after normalization".to_string());
    }
    Ok(clean)
}

/// `parseDomainFilter(domainFilter)`.
fn parse_domain_filter(value: Option<&Value>) -> (Vec<String>, Vec<String>) {
    let Some(items) = value.and_then(Value::as_array) else {
        return (Vec::new(), Vec::new());
    };
    let strings: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
    let includes = strings
        .iter()
        .filter(|d| !d.starts_with('-'))
        .map(|d| (*d).to_string())
        .collect();
    let excludes = strings
        .iter()
        .filter(|d| d.starts_with('-'))
        .map(|d| d[1..].to_string())
        .collect();
    (includes, excludes)
}

/// The first non-empty string under any of `keys` — JS `a || b` over strings.
fn str_or<'a>(item: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| {
        item.get(*k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    })
}

/// `getProviderSetting(params, key)`: `providerOptions` first, then
/// `providerSpecificData`, trimmed and non-empty.
fn get_provider_setting(
    body: &Value,
    credentials: Option<&Credentials>,
    key: &str,
) -> Option<String> {
    let from_options = body
        .get("provider_options")
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(v) = from_options {
        return Some(v.to_string());
    }
    credentials
        .and_then(|c| c.psd_str(key))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `resolveBaseUrl(config, params)`: a client-supplied `baseUrl` override is
/// SSRF-hardened through layer 1; the provider's own configured baseUrl is
/// trusted as-is (admin-controlled).
fn resolve_base_url(
    config: &Value,
    body: &Value,
    credentials: Option<&Credentials>,
) -> Result<String, ModalityError> {
    let override_url = get_provider_setting(body, credentials, "baseUrl");
    if let Some(url) = &override_url {
        let parsed = url::Url::parse(url)
            .map_err(|_| ModalityError::search(400, format!("Invalid baseUrl: {url}")))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(ModalityError::search(
                400,
                format!("Invalid baseUrl protocol: {}", parsed.scheme()),
            ));
        }
        crate::modalities::ssrf::assert_public_url(url)
            .map_err(|e| ModalityError::search(400, e.to_string()))?;
    }
    let base = override_url
        .or_else(|| {
            config
                .get("baseUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    Ok(base.trim_end_matches('/').to_string())
}

/// `Math.min(body.max_results || config.defaultMaxResults || 5,
/// config.maxMaxResults || 100)`. JS `||` drops a zero, so `max_results: 0`
/// still takes the provider default.
fn resolve_max_results(body: &Value, config: &Value) -> i64 {
    let default = config
        .get("defaultMaxResults")
        .and_then(Value::as_i64)
        .unwrap_or(5);
    let ceiling = config
        .get("maxMaxResults")
        .and_then(Value::as_i64)
        .unwrap_or(100);
    body.get("max_results")
        .and_then(Value::as_i64)
        .filter(|n| *n != 0)
        .unwrap_or(default)
        .min(ceiling)
}

/// The `{url, method, headers, json_body}` a builder produces.
struct BuiltRequest {
    url: String,
    method: &'static str,
    headers: Vec<(String, String)>,
    json_body: Option<Value>,
}

/// `buildSearchRequest(provider, params)`: the five kept builders.
fn build_request(
    provider_id: &str,
    config: &Value,
    body: &Value,
    credentials: Option<&Credentials>,
) -> Result<BuiltRequest, ModalityError> {
    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    let search_type = body
        .get("search_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            config
                .get("searchTypes")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
        })
        .unwrap_or("web");
    let max_results = resolve_max_results(body, config);
    let token = super::credential_token(credentials);
    let country = body.get("country").and_then(Value::as_str);
    let language = body.get("language").and_then(Value::as_str);
    let time_range = body.get("time_range").and_then(Value::as_str);
    let offset = body.get("offset").and_then(Value::as_f64);
    let domain_filter = body.get("domain_filter");
    let content_options = body.get("content_options");

    let base = resolve_base_url(config, body, credentials)?;

    match provider_id {
        "brave-search" => {
            let endpoint = if search_type == "news" {
                "/news/search"
            } else {
                "/web/search"
            };
            let mut qp = url::form_urlencoded::Serializer::new(String::new());
            qp.append_pair("q", query);
            qp.append_pair("count", &max_results.to_string());
            if let Some(country) = country {
                qp.append_pair("country", country);
            }
            if let Some(language) = language {
                qp.append_pair("search_lang", language);
            }
            let mut headers = vec![("Accept".into(), "application/json".into())];
            if let Some(token) = token {
                headers.push(("X-Subscription-Token".into(), token.to_string()));
            }
            Ok(BuiltRequest {
                url: format!("{base}{endpoint}?{}", qp.finish()),
                method: "GET",
                headers,
                json_body: None,
            })
        }
        "exa" => {
            let (includes, excludes) = parse_domain_filter(domain_filter);
            let mut req = Map::new();
            req.insert("query".into(), json!(query));
            req.insert("numResults".into(), json!(max_results));
            req.insert("type".into(), json!("auto"));
            req.insert("text".into(), json!(true));
            req.insert("highlights".into(), json!(true));
            if !includes.is_empty() {
                req.insert("includeDomains".into(), json!(includes));
            }
            if !excludes.is_empty() {
                req.insert("excludeDomains".into(), json!(excludes));
            }
            if search_type == "news" {
                req.insert("category".into(), json!("news"));
            }
            let mut headers = vec![("Content-Type".into(), "application/json".into())];
            if let Some(token) = token {
                headers.push(("x-api-key".into(), token.to_string()));
            }
            Ok(BuiltRequest {
                url: base,
                method: "POST",
                headers,
                json_body: Some(Value::Object(req)),
            })
        }
        "linkup" => {
            let Some(token) = token else {
                return Err(ModalityError::search(
                    400,
                    "Linkup Search requires an API key",
                ));
            };
            let (includes, excludes) = parse_domain_filter(domain_filter);
            let depth = get_provider_setting(body, credentials, "depth")
                .filter(|d| ["fast", "standard", "deep"].contains(&d.as_str()))
                .unwrap_or_else(|| "standard".to_string());
            let mut req = Map::new();
            req.insert("q".into(), json!(query));
            req.insert("depth".into(), json!(depth));
            req.insert("outputType".into(), json!("searchResults"));
            req.insert("maxResults".into(), json!(max_results));
            if !includes.is_empty() {
                req.insert("includeDomains".into(), json!(includes));
            }
            if !excludes.is_empty() {
                req.insert("excludeDomains".into(), json!(excludes));
            }
            if let Some(range) = time_range.filter(|r| *r != "any") {
                let (from, to) = linkup_date_range(range);
                req.insert("fromDate".into(), json!(from));
                req.insert("toDate".into(), json!(to));
            }
            Ok(BuiltRequest {
                url: base,
                method: "POST",
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    ("Authorization".into(), format!("Bearer {token}")),
                ],
                json_body: Some(Value::Object(req)),
            })
        }
        "tavily" => {
            let (includes, excludes) = parse_domain_filter(domain_filter);
            let mut req = Map::new();
            req.insert("query".into(), json!(query));
            req.insert("max_results".into(), json!(max_results));
            req.insert(
                "topic".into(),
                json!(if search_type == "news" {
                    "news"
                } else {
                    "general"
                }),
            );
            if !includes.is_empty() {
                req.insert("include_domains".into(), json!(includes));
            }
            if !excludes.is_empty() {
                req.insert("exclude_domains".into(), json!(excludes));
            }
            if let Some(country) = country {
                req.insert("country".into(), json!(country));
            }
            let mut headers = vec![("Content-Type".into(), "application/json".into())];
            if let Some(token) = token {
                headers.push(("Authorization".into(), format!("Bearer {token}")));
            }
            Ok(BuiltRequest {
                url: base,
                method: "POST",
                headers,
                json_body: Some(Value::Object(req)),
            })
        }
        "youcom" => {
            let Some(token) = token else {
                return Err(ModalityError::search(
                    400,
                    "You.com Search requires an API key",
                ));
            };
            let (includes, excludes) = parse_domain_filter(domain_filter);
            let mut qp = url::form_urlencoded::Serializer::new(String::new());
            qp.append_pair("query", query);
            qp.append_pair("count", &max_results.min(100).to_string());
            if let Some(range) = time_range.filter(|r| *r != "any") {
                qp.append_pair("freshness", range);
            }
            if let Some(offset) = offset.filter(|o| *o > 0.0)
                && max_results > 0
            {
                let page = (offset / max_results as f64).floor().min(9.0) as i64;
                qp.append_pair("offset", &page.to_string());
            }
            if let Some(country) = country {
                qp.append_pair("country", country);
            }
            if let Some(language) = language {
                qp.append_pair("language", language);
            }
            if !includes.is_empty() {
                qp.append_pair("include_domains", &includes.join(","));
            }
            if !excludes.is_empty() {
                qp.append_pair("exclude_domains", &excludes.join(","));
            }
            if content_options
                .and_then(|c| c.get("full_page"))
                .is_some_and(js_truthy)
            {
                qp.append_pair(
                    "livecrawl",
                    if search_type == "news" { "news" } else { "web" },
                );
                let markdown = content_options
                    .and_then(|c| c.get("format"))
                    .and_then(Value::as_str)
                    == Some("markdown");
                qp.append_pair(
                    "livecrawl_formats",
                    if markdown { "markdown" } else { "html" },
                );
            }
            Ok(BuiltRequest {
                url: format!("{base}?{}", qp.finish()),
                method: "GET",
                headers: vec![
                    ("Accept".into(), "application/json".into()),
                    ("X-API-Key".into(), token.to_string()),
                ],
                json_body: None,
            })
        }
        _ => Err(ModalityError::search(
            400,
            format!("Unsupported search provider: {provider_id}"),
        )),
    }
}

/// `buildLinkupRequest`'s `fromDate`/`toDate`: today and today minus the range,
/// as `YYYY-MM-DD`. Month/year steps are calendar steps (`setUTCMonth` /
/// `setUTCFullYear`), not fixed day counts.
fn linkup_date_range(range: &str) -> (String, String) {
    let today = chrono::Utc::now();
    let to = today.format("%Y-%m-%d").to_string();
    let from = match range {
        "day" => today - chrono::Days::new(1),
        "week" => today - chrono::Days::new(7),
        "month" => today - chrono::Months::new(1),
        "year" => today - chrono::Months::new(12),
        _ => today,
    };
    (from.format("%Y-%m-%d").to_string(), to)
}

/// `tryDedicatedProvider({...})`.
async fn try_dedicated_provider(
    provider_id: &str,
    config: &Value,
    body: &Value,
    credentials: Option<&Credentials>,
    proxy_options: &ProxyOptions,
    global_start: Instant,
) -> Result<ModalityResponse, ModalityError> {
    let start_time = Instant::now();
    let token = super::credential_token(credentials);

    if config.get("authType").and_then(Value::as_str) != Some("none") && token.is_none() {
        return Err(ModalityError::search(
            401,
            format!("No credentials for provider: {provider_id}"),
        ));
    }

    let built = build_request(provider_id, config, body, credentials)?;

    // Timeout = min(provider timeout, remaining global), floored at 1 s.
    let remaining = GLOBAL_TIMEOUT_MS as i64 - global_start.elapsed().as_millis() as i64;
    let provider_timeout = config
        .get("timeoutMs")
        .and_then(Value::as_i64)
        .unwrap_or(10_000);
    let timeout = provider_timeout.min(remaining.max(1000)).max(1) as u64;

    let headers = sanitize_headers(&built.headers);
    let sent = fetch_public(
        built.method,
        &built.url,
        &headers,
        built.json_body.as_ref(),
        proxy_options,
        timeout,
    )
    .await;

    let response = match sent {
        Ok(response) => response,
        Err(e) => {
            // The fetch try/catch: abort is 504, anything else 502. A blocked
            // *override* baseUrl already failed earlier as a 400.
            let message = e.to_string();
            if message.contains("timed out") {
                return Err(ModalityError::search(
                    504,
                    format!("{provider_id} timeout: {message}"),
                ));
            }
            return Err(ModalityError::search(
                502,
                format!("{provider_id} error: {message}"),
            ));
        }
    };

    if !response.ok() {
        let err_text: String = response.body.chars().take(200).collect();
        return Err(ModalityError::search(
            response.status,
            format!("{provider_id} returned {}: {err_text}", response.status),
        ));
    }

    let data = parse_json(&response).map_err(|_| {
        ModalityError::search(502, format!("{provider_id} error: invalid JSON response"))
    })?;

    let query = body.get("query").and_then(Value::as_str).unwrap_or("");
    let search_type = body
        .get("search_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            config
                .get("searchTypes")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .unwrap_or("web")
        });
    let normalized = normalize_search_response(provider_id, &data, query, search_type);
    let max_results = resolve_max_results(body, config);
    let results: Vec<Value> = normalized
        .results
        .iter()
        .take(max_results.max(0) as usize)
        .cloned()
        .collect();
    let duration = start_time.elapsed().as_millis() as i64;

    let mut usage = Map::new();
    usage.insert("queries_used".into(), json!(1));
    usage.insert(
        "search_cost_usd".into(),
        config.get("costPerQuery").cloned().unwrap_or(Value::Null),
    );
    if let Some(credits) = finite_number(config.get("creditsPerResult")) {
        usage.insert(
            "provider_credits_used".into(),
            json!(results.len() as f64 * credits),
        );
    }

    let mut data_out = Map::new();
    data_out.insert("provider".into(), json!(provider_id));
    data_out.insert("query".into(), json!(query));
    data_out.insert("results".into(), Value::Array(results));
    data_out.insert("answer".into(), Value::Null);
    data_out.insert("usage".into(), Value::Object(usage));
    data_out.insert(
        "metrics".into(),
        json!({
            "response_time_ms": duration,
            "upstream_latency_ms": duration,
            "total_results_available": normalized.total_results,
        }),
    );
    data_out.insert("errors".into(), json!([]));

    Ok(ModalityResponse::json(Value::Object(data_out)))
}

/// `await res.json()` — a JSON parse failure is a thrown error.
fn parse_json(response: &PublicResponse) -> Result<Value, serde_json::Error> {
    response.json()
}

/// The normalizer output.
struct NormalizedSearch {
    results: Vec<Value>,
    total_results: Value,
}

/// `normalizeSearchResponse(providerId, data, query, searchType)`: the five kept
/// normalizers.
fn normalize_search_response(
    provider_id: &str,
    data: &Value,
    _query: &str,
    search_type: &str,
) -> NormalizedSearch {
    let now = router_db::time::now_iso();
    match provider_id {
        "brave-search" => {
            let container = if search_type == "news" {
                data.get("news").unwrap_or(data)
            } else {
                data.get("web").unwrap_or(&Value::Null)
            };
            let Some(items) = container.get("results").and_then(Value::as_array) else {
                return empty();
            };
            let results = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    make_result(
                        "brave-search",
                        &json!({
                            "title": item.get("title"),
                            "url": item.get("url"),
                            "snippet": item.get("description"),
                            "published_at": str_or(item, &["page_age", "age"]),
                            "favicon_url": item.pointer("/meta_url/favicon").or_else(|| item.get("favicon")),
                        }),
                        idx,
                        &now,
                    )
                })
                .collect();
            NormalizedSearch {
                results,
                total_results: container.get("totalCount").cloned().unwrap_or(Value::Null),
            }
        }
        "exa" => {
            let Some(items) = data.get("results").and_then(Value::as_array) else {
                return empty();
            };
            let results = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let snippet = item
                        .get("highlights")
                        .and_then(Value::as_array)
                        .and_then(|h| h.first())
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            item.get("text")
                                .and_then(Value::as_str)
                                .map(|t| slice_utf16(t, 300))
                        });
                    make_result(
                        "exa",
                        &json!({
                            "title": item.get("title"),
                            "url": item.get("url"),
                            "snippet": snippet,
                            "score": item.get("score"),
                            "published_at": item.get("publishedDate"),
                            "favicon_url": item.get("favicon"),
                            "author": item.get("author"),
                            "image_url": item.get("image"),
                            "full_text": item.get("text"),
                            "text_format": "text",
                        }),
                        idx,
                        &now,
                    )
                })
                .collect::<Vec<_>>();
            let total = json!(results.len());
            NormalizedSearch {
                results,
                total_results: total,
            }
        }
        "linkup" => {
            let items = data
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let results = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    make_result(
                        "linkup",
                        &json!({
                            "title": str_or(item, &["name", "title"]),
                            "url": item.get("url"),
                            "snippet": str_or(item, &["content", "snippet"]),
                            "source_type": str_or(item, &["type"]).unwrap_or("web"),
                            "image_url": str_or(item, &["image_url", "imageUrl"]),
                            "full_text": item.get("content"),
                            "text_format": "text",
                        }),
                        idx,
                        &now,
                    )
                })
                .collect::<Vec<_>>();
            let total = json!(results.len());
            NormalizedSearch {
                results,
                total_results: total,
            }
        }
        "tavily" => {
            let Some(items) = data.get("results").and_then(Value::as_array) else {
                return empty();
            };
            let results = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    make_result(
                        "tavily",
                        &json!({
                            "title": item.get("title"),
                            "url": item.get("url"),
                            "snippet": item.get("content").and_then(Value::as_str).unwrap_or(""),
                            "score": item.get("score"),
                            "published_at": item.get("published_date"),
                            "full_text": item.get("raw_content"),
                            "text_format": "text",
                        }),
                        idx,
                        &now,
                    )
                })
                .collect::<Vec<_>>();
            let total = json!(results.len());
            NormalizedSearch {
                results,
                total_results: total,
            }
        }
        "youcom" => {
            let container = data
                .get("results")
                .filter(|r| r.is_object())
                .unwrap_or(&Value::Null);
            let section = if search_type == "news" {
                container.get("news").and_then(Value::as_array)
            } else {
                container.get("web").and_then(Value::as_array)
            };
            let items = section.cloned().unwrap_or_default();
            let results = items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    let first_snippet = item
                        .get("snippets")
                        .and_then(Value::as_array)
                        .and_then(|s| s.iter().find_map(Value::as_str));
                    let markdown = item.get("markdown").and_then(Value::as_str);
                    let livecrawl_text = markdown
                        .map(str::to_string)
                        .or_else(|| item.get("html").and_then(Value::as_str).map(str::to_string));
                    let livecrawl_format = if markdown.is_some() {
                        "markdown"
                    } else {
                        "html"
                    };
                    make_result(
                        "youcom",
                        &json!({
                            "title": item.get("title"),
                            "url": item.get("url"),
                            "snippet": first_snippet
                                .or_else(|| item.get("description").and_then(Value::as_str))
                                .unwrap_or(""),
                            "published_at": item.get("page_age"),
                            "favicon_url": item.get("favicon_url"),
                            "image_url": item.get("thumbnail_url"),
                            "source_type": search_type,
                            "full_text": livecrawl_text,
                            "text_format": livecrawl_format,
                        }),
                        idx,
                        &now,
                    )
                })
                .collect::<Vec<_>>();
            let total = json!(results.len());
            NormalizedSearch {
                results,
                total_results: total,
            }
        }
        _ => empty(),
    }
}

fn empty() -> NormalizedSearch {
    NormalizedSearch {
        results: Vec::new(),
        total_results: Value::Null,
    }
}

/// `makeResult(providerId, item, idx, now)`.
///
/// `item.x || null` (and `|| ""`) is JS falsiness, and `display_url` is
/// `undefined` when the url is empty — `JSON.stringify` drops an `undefined`
/// key, so the key must be absent here, not null.
fn make_result(provider_id: &str, item: &Value, idx: usize, now: &str) -> Value {
    let url = item.get("url").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    out.insert(
        "title".into(),
        json!(item.get("title").and_then(Value::as_str).unwrap_or("")),
    );
    out.insert("url".into(), json!(url));
    if !url.is_empty() {
        let stripped = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);
        let stripped = stripped.strip_prefix("www.").unwrap_or(stripped);
        out.insert(
            "display_url".into(),
            json!(stripped.split('?').next().unwrap_or(stripped)),
        );
    }
    out.insert(
        "snippet".into(),
        json!(item.get("snippet").and_then(Value::as_str).unwrap_or("")),
    );
    out.insert("position".into(), json!(idx + 1));
    out.insert(
        "score".into(),
        match item.get("score").and_then(Value::as_f64) {
            Some(s) => json!(s.clamp(0.0, 1.0)),
            None => Value::Null,
        },
    );
    out.insert(
        "published_at".into(),
        null_if_falsy(item.get("published_at")),
    );
    out.insert("favicon_url".into(), null_if_falsy(item.get("favicon_url")));
    out.insert(
        "content".into(),
        match item.get("full_text") {
            Some(Value::String(text)) if !text.is_empty() => {
                let format = item
                    .get("text_format")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("text");
                json!({ "format": format, "text": text, "length": text.encode_utf16().count() })
            }
            _ => Value::Null,
        },
    );
    out.insert(
        "metadata".into(),
        json!({
            "author": null_if_falsy(item.get("author")),
            "language": Value::Null,
            "source_type": null_if_falsy(item.get("source_type")),
            "image_url": null_if_falsy(item.get("image_url")),
        }),
    );
    out.insert(
        "citation".into(),
        json!({ "provider": provider_id, "retrieved_at": now, "rank": idx + 1 }),
    );
    out.insert("provider_raw".into(), Value::Null);
    Value::Object(out)
}

/// JS `value || null`.
fn null_if_falsy(value: Option<&Value>) -> Value {
    match value {
        Some(v) if js_truthy(v) => v.clone(),
        _ => Value::Null,
    }
}

/// `s.slice(0, max)` over UTF-16 code units, matching JS string indexing.
fn slice_utf16(s: &str, max: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(max).collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_query_rejects_control_chars_and_collapses_whitespace() {
        assert_eq!(sanitize_query("  hello   world  ").unwrap(), "hello world");
        assert_eq!(sanitize_query("a\tb\nc").unwrap(), "a b c");
        assert_eq!(
            sanitize_query("bad\u{0007}query").unwrap_err(),
            "Query contains invalid control characters"
        );
        assert_eq!(
            sanitize_query("   ").unwrap_err(),
            "Query is empty after normalization"
        );
    }

    #[test]
    fn domain_filter_splits_excludes() {
        let (inc, exc) = parse_domain_filter(Some(&json!(["a.com", "-b.com", "c.org"])));
        assert_eq!(inc, vec!["a.com", "c.org"]);
        assert_eq!(exc, vec!["b.com"]);
        let (inc, exc) = parse_domain_filter(None);
        assert!(inc.is_empty() && exc.is_empty());
    }

    #[test]
    fn max_results_zero_falls_through_to_the_provider_default() {
        let config = json!({ "defaultMaxResults": 5, "maxMaxResults": 20 });
        assert_eq!(resolve_max_results(&json!({"max_results": 0}), &config), 5);
        assert_eq!(resolve_max_results(&json!({}), &config), 5);
        assert_eq!(
            resolve_max_results(&json!({"max_results": 50}), &config),
            20
        );
        assert_eq!(resolve_max_results(&json!({"max_results": 3}), &config), 3);
    }

    #[test]
    fn brave_normalizer_maps_the_web_container() {
        let data = json!({
            "web": {
                "totalCount": 42,
                "results": [{"title": "T", "url": "https://www.x.com/a?q=1", "description": "S"}]
            }
        });
        let out = normalize_search_response("brave-search", &data, "q", "web");
        assert_eq!(out.total_results, json!(42));
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0]["display_url"], json!("x.com/a"));
        assert_eq!(out.results[0]["position"], json!(1));
        assert_eq!(
            out.results[0]["citation"]["provider"],
            json!("brave-search")
        );
    }

    #[test]
    fn an_empty_url_drops_display_url_entirely() {
        let data = json!({"web": {"results": [{"title": "T"}]}});
        let out = normalize_search_response("brave-search", &data, "q", "web");
        assert!(
            out.results[0].get("display_url").is_none(),
            "undefined is dropped by JSON.stringify, so the key must be absent"
        );
    }

    #[test]
    fn brave_news_falls_back_to_the_root() {
        let data = json!({"news": {"results": [{"title": "N", "url": "https://n.com"}]}});
        let out = normalize_search_response("brave-search", &data, "q", "news");
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.total_results, Value::Null);
    }

    #[test]
    fn exa_normalizer_reads_highlights_and_clamps_score() {
        let data = json!({
            "results": [{
                "title": "T", "url": "https://e.com", "highlights": ["hl"],
                "score": 1.5, "author": "A", "text": "body"
            }]
        });
        let out = normalize_search_response("exa", &data, "q", "web");
        assert_eq!(out.results[0]["snippet"], json!("hl"));
        assert_eq!(out.results[0]["score"], json!(1.0));
        assert_eq!(out.results[0]["metadata"]["author"], json!("A"));
        assert_eq!(out.results[0]["content"]["format"], json!("text"));
        assert_eq!(out.total_results, json!(1));
    }

    #[test]
    fn tavily_and_linkup_normalizers_map_their_shapes() {
        let tavily = json!({"results": [{"title": "T", "url": "https://t.com", "content": "c", "raw_content": "raw"}]});
        let out = normalize_search_response("tavily", &tavily, "q", "web");
        assert_eq!(out.results[0]["snippet"], json!("c"));
        assert_eq!(out.results[0]["content"]["text"], json!("raw"));

        let linkup = json!({"results": [{"name": "N", "url": "https://l.com", "content": "c"}]});
        let out = normalize_search_response("linkup", &linkup, "q", "web");
        assert_eq!(out.results[0]["title"], json!("N"));
        assert_eq!(out.results[0]["metadata"]["source_type"], json!("web"));
    }

    #[test]
    fn youcom_normalizer_reads_the_section_and_livecrawl() {
        let data = json!({
            "results": {
                "web": [{
                    "title": "T", "url": "https://y.com",
                    "snippets": ["first", "second"], "markdown": "# md",
                    "thumbnail_url": "img"
                }]
            }
        });
        let out = normalize_search_response("youcom", &data, "q", "web");
        assert_eq!(out.results[0]["snippet"], json!("first"));
        assert_eq!(out.results[0]["content"]["format"], json!("markdown"));
        assert_eq!(out.results[0]["metadata"]["image_url"], json!("img"));
    }

    #[test]
    fn an_unknown_normalizer_returns_empty() {
        let out = normalize_search_response("nope", &json!({}), "q", "web");
        assert!(out.results.is_empty());
        assert_eq!(out.total_results, Value::Null);
    }
}
