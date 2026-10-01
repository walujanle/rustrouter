//! Usage tracking.
//!
//! `save_request_usage` is the hot path: one transaction does a dedup select, a
//! history insert, a daily aggregate upsert and a lifetime counter bump. The
//! dedup and the aggregate share the transaction because a partial apply would
//! double-count a day.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{parse_json, parse_json_opt, stringify_json};
use crate::stats::{self, mask_api_key};
use crate::time::{local_date_key, now_iso, now_ms, parse_iso};

/// Compute the cost of one request. Injected so this crate does not depend on
/// the provider registry.
pub type CostFn<'a> = dyn Fn(Option<&str>, Option<&str>, &Value) -> f64 + 'a;

/// `(timestamp, provider, model, connectionId, apiKey, endpoint)` — the columns
/// `overlay_last_used` reads back.
type LastUsedRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// `(timestamp, promptTokens, completionTokens, cost)` — the columns the intraday
/// buckets read back.
type TokenRow = (String, Option<i64>, Option<i64>, Option<f64>);

/// `(timestamp, provider, model, connectionId, promptTokens, completionTokens,
/// status, tokens)` — the columns `get_recent_logs` reads back.
type LogRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

/// Save one request's usage.
///
/// Returns `true` when a new history row was inserted. A duplicate (same
/// timestamp, provider, model, connection, key and token counts) updates only a
/// missing `endpoint` and inserts nothing else.
pub fn save_request_usage(
    conn: &Connection,
    entry: &mut Value,
    cost: &CostFn<'_>,
) -> DbResult<bool> {
    let Some(obj) = entry.as_object_mut() else {
        return Ok(false);
    };

    if !obj.contains_key("timestamp") || obj.get("timestamp").is_some_and(Value::is_null) {
        obj.insert("timestamp".into(), json!(now_iso()));
    }

    let tokens = obj.get("tokens").cloned().unwrap_or(json!({}));
    let provider = obj
        .get("provider")
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = obj.get("model").and_then(Value::as_str).map(str::to_string);

    let computed = cost(provider.as_deref(), model.as_deref(), &tokens);
    obj.insert("cost".into(), json!(computed));

    let prompt_tokens = token_field(&tokens, &["prompt_tokens", "input_tokens"]);
    let completion_tokens = token_field(&tokens, &["completion_tokens", "output_tokens"]);

    let timestamp = obj
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let connection_id = obj
        .get("connectionId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let api_key = obj
        .get("apiKey")
        .and_then(Value::as_str)
        .map(str::to_string);
    let endpoint = obj
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::to_string);
    // `entry.status || "ok"` — an empty string is falsy too, not just a
    // missing key.
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("ok")
        .to_string();

    let existing: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT id, endpoint FROM usageHistory
             WHERE timestamp = ?
               AND COALESCE(provider, '') = COALESCE(?, '')
               AND COALESCE(model, '') = COALESCE(?, '')
               AND COALESCE(connectionId, '') = COALESCE(?, '')
               AND COALESCE(apiKey, '') = COALESCE(?, '')
               AND promptTokens = ?
               AND completionTokens = ?
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![
                timestamp,
                provider,
                model,
                connection_id,
                api_key,
                prompt_tokens,
                completion_tokens
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    if let Some((id, existing_endpoint)) = existing {
        if existing_endpoint.is_none()
            && let Some(ep) = endpoint
        {
            conn.execute(
                "UPDATE usageHistory SET endpoint = ? WHERE id = ?",
                rusqlite::params![ep, id],
            )?;
        }
        return Ok(false);
    }

    conn.execute(
        "INSERT INTO usageHistory(timestamp, provider, model, connectionId, apiKey, endpoint, promptTokens, completionTokens, cost, status, tokens, meta) VALUES(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        rusqlite::params![
            timestamp,
            provider,
            model,
            connection_id,
            api_key,
            endpoint,
            prompt_tokens,
            completion_tokens,
            computed,
            status,
            stringify_json(&tokens),
            stringify_json(&json!({})),
        ],
    )?;

    let date_key = local_date_key(Some(&timestamp));
    let day_row: Option<String> = conn
        .query_row(
            "SELECT data FROM usageDaily WHERE dateKey = ?",
            [&date_key],
            |r| r.get(0),
        )
        .optional()?;
    let mut day = match day_row {
        Some(raw) => match parse_json(&raw, json!({})) {
            Value::Object(m) => m,
            _ => default_day(),
        },
        None => default_day(),
    };
    aggregate_entry_to_day(&mut day, obj);
    conn.execute(
        "INSERT INTO usageDaily(dateKey, data) VALUES(?, ?) ON CONFLICT(dateKey) DO UPDATE SET data = excluded.data",
        rusqlite::params![date_key, stringify_json(&Value::Object(day))],
    )?;

    let current: Option<String> = conn
        .query_row(
            "SELECT value FROM _meta WHERE key = 'totalRequestsLifetime'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let next = current.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
    conn.execute(
        "INSERT INTO _meta(key, value) VALUES('totalRequestsLifetime', ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [next.to_string()],
    )?;

    Ok(true)
}

fn default_day() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("requests".into(), json!(0));
    m.insert("promptTokens".into(), json!(0));
    m.insert("completionTokens".into(), json!(0));
    m.insert("cost".into(), json!(0));
    m.insert("byProvider".into(), json!({}));
    m.insert("byModel".into(), json!({}));
    m.insert("byAccount".into(), json!({}));
    m.insert("byApiKey".into(), json!({}));
    m.insert("byEndpoint".into(), json!({}));
    m
}

/// `tokens.prompt_tokens || tokens.input_tokens || 0`.
///
/// The keys chain with `||`, not `??`: a present-but-zero first key falls
/// through to the next. Returning the first key that exists would report 0 for
/// `{ prompt_tokens: 0, input_tokens: 5 }` instead of 5.
fn token_field(tokens: &Value, keys: &[&str]) -> i64 {
    for k in keys {
        if let Some(n) = tokens.get(*k).and_then(Value::as_i64)
            && n != 0
        {
            return n;
        }
    }
    0
}

/// Fold one entry's values into a counter map under `key`.
fn add_to_counter(
    target: &mut Map<String, Value>,
    key: &str,
    values: &Value,
    meta: Option<&Value>,
) {
    let entry = target
        .entry(key.to_string())
        .or_insert_with(|| {
            json!({ "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0, "cost": 0 })
        });
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    bump(
        obj,
        "requests",
        values.get("requests").and_then(Value::as_i64).unwrap_or(1),
    );
    for k in ["promptTokens", "completionTokens", "cachedTokens"] {
        bump(obj, k, values.get(k).and_then(Value::as_i64).unwrap_or(0));
    }
    if let Some(cost) = values.get("cost").and_then(Value::as_f64) {
        let cur = obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        obj.insert("cost".into(), json!(cur + cost));
    }
    if let Some(Value::Object(meta)) = meta {
        for (k, v) in meta {
            obj.insert(k.clone(), v.clone());
        }
    }
}

fn bump(obj: &mut Map<String, Value>, key: &str, delta: i64) {
    let cur = obj.get(key).and_then(Value::as_i64).unwrap_or(0);
    obj.insert(key.into(), json!(cur + delta));
}

/// Fold one request entry into a day's aggregate.
pub fn aggregate_entry_to_day(day: &mut Map<String, Value>, entry: &Map<String, Value>) {
    let tokens = entry.get("tokens").cloned().unwrap_or(json!({}));
    let prompt_tokens = token_field(&tokens, &["prompt_tokens", "input_tokens"]);
    let completion_tokens = token_field(&tokens, &["completion_tokens", "output_tokens"]);
    let cached_tokens = token_field(&tokens, &["cached_tokens", "cache_read_input_tokens"]);
    let cost = entry.get("cost").and_then(Value::as_f64).unwrap_or(0.0);

    bump(day, "requests", 1);
    bump(day, "promptTokens", prompt_tokens);
    bump(day, "completionTokens", completion_tokens);
    bump(day, "cachedTokens", cached_tokens);
    let cur_cost = day.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    day.insert("cost".into(), json!(cur_cost + cost));

    for k in [
        "byProvider",
        "byModel",
        "byAccount",
        "byApiKey",
        "byEndpoint",
    ] {
        if !day.contains_key(k) {
            day.insert(k.into(), json!({}));
        }
    }

    let provider = entry.get("provider").and_then(Value::as_str);
    let model = entry.get("model").and_then(Value::as_str).unwrap_or("");
    let connection_id = entry.get("connectionId").and_then(Value::as_str);
    let api_key = entry.get("apiKey").and_then(Value::as_str);
    let endpoint = entry.get("endpoint").and_then(Value::as_str);

    let vals = json!({
        "requests": 1,
        "promptTokens": prompt_tokens,
        "completionTokens": completion_tokens,
        "cachedTokens": cached_tokens,
        "cost": cost,
    });

    if let Some(provider) = provider
        && let Some(m) = counter_map(day, "byProvider")
    {
        add_to_counter(m, provider, &vals, None)
    }

    // `${entry.model}|${entry.provider}` when a provider is present.
    let model_key = match provider {
        Some(p) => format!("{model}|{p}"),
        None => model.to_string(),
    };
    let meta = json!({ "rawModel": entry.get("model").cloned().unwrap_or(Value::Null), "provider": provider });
    if let Some(m) = counter_map(day, "byModel") {
        add_to_counter(m, &model_key, &vals, Some(&meta));
    }

    if let Some(connection_id) = connection_id
        && let Some(m) = counter_map(day, "byAccount")
    {
        add_to_counter(m, connection_id, &vals, Some(&meta));
    }

    let api_key_val = api_key.unwrap_or("local-no-key");
    let ak_model_key = format!("{api_key_val}|{model}|{}", provider.unwrap_or("unknown"));
    let ak_meta = json!({
        "rawModel": entry.get("model").cloned().unwrap_or(Value::Null),
        "provider": provider,
        "apiKey": entry.get("apiKey").cloned().unwrap_or(Value::Null),
    });
    if let Some(m) = counter_map(day, "byApiKey") {
        add_to_counter(m, &ak_model_key, &vals, Some(&ak_meta));
    }

    let endpoint_val = endpoint.unwrap_or("Unknown");
    let ep_key = format!("{endpoint_val}|{model}|{}", provider.unwrap_or("unknown"));
    let ep_meta = json!({
        "endpoint": endpoint_val,
        "rawModel": entry.get("model").cloned().unwrap_or(Value::Null),
        "provider": provider,
    });
    if let Some(m) = counter_map(day, "byEndpoint") {
        add_to_counter(m, &ep_key, &vals, Some(&ep_meta));
    }
}

fn counter_map<'a>(
    day: &'a mut Map<String, Value>,
    key: &str,
) -> Option<&'a mut Map<String, Value>> {
    match day.get_mut(key) {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// One row of the usage history payload.
pub fn get_usage_history(
    conn: &Connection,
    provider: Option<&str>,
    model: Option<&str>,
    start_date: Option<&str>,
    end_date: Option<&str>,
) -> DbResult<Vec<Value>> {
    let mut sql = String::from(
        "SELECT timestamp, provider, model, connectionId, apiKey, endpoint, cost, status, tokens FROM usageHistory",
    );
    let mut conds = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(p) = provider {
        conds.push("provider = ?");
        params.push(Box::new(p.to_string()));
    }
    if let Some(m) = model {
        conds.push("model = ?");
        params.push(Box::new(m.to_string()));
    }
    if let Some(s) = start_date {
        conds.push("timestamp >= ?");
        params.push(Box::new(normalize_date_bound(s)));
    }
    if let Some(e) = end_date {
        conds.push("timestamp <= ?");
        params.push(Box::new(normalize_date_bound(e)));
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY id ASC");

    let mut stmt = conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let tokens: Option<String> = r.get("tokens")?;
        let api_key: Option<String> = r.get("apiKey")?;
        Ok(json!({
            "timestamp": r.get::<_, String>("timestamp")?,
            "provider": r.get::<_, Option<String>>("provider")?,
            "model": r.get::<_, Option<String>>("model")?,
            "connectionId": r.get::<_, Option<String>>("connectionId")?,
            "apiKeyMasked": mask_api_key(api_key.as_deref()),
            "endpoint": r.get::<_, Option<String>>("endpoint")?,
            "cost": r.get::<_, Option<f64>>("cost")?,
            "status": r.get::<_, Option<String>>("status")?,
            "tokens": parse_json_opt(tokens.as_deref(), json!({})),
        }))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// `new Date(x).toISOString()`: a date bound is re-serialized to UTC millis
/// before the lexicographic compare, so an offset-bearing or non-ISO input
/// still orders correctly against the stored timestamps.
fn normalize_date_bound(s: &str) -> String {
    parse_iso(s)
        .map(crate::time::to_iso)
        .unwrap_or_else(|| s.to_string())
}

/// Load daily aggregates, optionally limited to the last `max_days`.
pub fn load_days_in_range(
    conn: &Connection,
    max_days: Option<i64>,
) -> DbResult<Vec<(String, Value)>> {
    let (sql, params): (&str, Vec<String>) = match max_days {
        None => (
            "SELECT dateKey, data FROM usageDaily ORDER BY dateKey ASC",
            vec![],
        ),
        Some(max_days) => {
            // `new Date(y, m, d - maxDays + 1)` — local calendar arithmetic.
            let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(max_days - 1);
            (
                "SELECT dateKey, data FROM usageDaily WHERE dateKey >= ? ORDER BY dateKey ASC",
                vec![cutoff.format("%Y-%m-%d").to_string()],
            )
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let refs: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        let date_key: String = r.get(0)?;
        let data: String = r.get(1)?;
        Ok((date_key, parse_json(&data, json!({}))))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Context the stats aggregation needs from other repos.
pub struct StatsContext<'a> {
    /// connection id to display name.
    pub connection_map: &'a indexmap::IndexMap<String, String>,
    /// provider id to display name.
    pub provider_node_name_map: &'a indexmap::IndexMap<String, String>,
    /// api key value to `{name, id, createdAt}`.
    pub api_key_map: &'a Map<String, Value>,
    pub now_ms: i64,
}

/// The full usage stats payload for a period.
pub fn get_usage_stats(conn: &Connection, period: &str, ctx: &StatsContext<'_>) -> DbResult<Value> {
    stats::sweep_stale_pending(ctx.now_ms);

    let mut by_provider = Map::new();
    let mut by_model = Map::new();
    let mut by_account = Map::new();
    let mut by_api_key = Map::new();
    let mut by_endpoint = Map::new();
    let mut total_prompt = 0i64;
    let mut total_completion = 0i64;
    let mut total_cached = 0i64;
    let mut total_cost = 0.0f64;

    let recent_requests = recent_requests_from_history(conn, 100, true)?;

    let use_daily_summary = period != "24h" && period != "today";

    if use_daily_summary {
        let max_days = match period {
            "7d" => Some(7),
            "30d" => Some(30),
            "60d" => Some(60),
            _ => None,
        };
        for (date_key, day) in load_days_in_range(conn, max_days)? {
            total_prompt += day.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
            total_completion += day
                .get("completionTokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            total_cached += day.get("cachedTokens").and_then(Value::as_i64).unwrap_or(0);
            total_cost += day.get("cost").and_then(Value::as_f64).unwrap_or(0.0);

            fold_simple(&mut by_provider, day.get("byProvider"));

            if let Some(Value::Object(models)) = day.get("byModel") {
                for (mk, m) in models {
                    // `m.rawModel || mk.split("|")[0]` — an empty string falls
                    // through, so `||` not `??`.
                    let raw_model = m
                        .get("rawModel")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| mk.split('|').next().map(str::to_string))
                        .unwrap_or_default();
                    let provider = m
                        .get("provider")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| mk.split('|').nth(1).map(str::to_string))
                        .unwrap_or_default();
                    let display = display_provider(ctx.provider_node_name_map, &provider);
                    let stats_key = if provider.is_empty() {
                        raw_model.clone()
                    } else {
                        format!("{raw_model} ({provider})")
                    };
                    let entry = by_model.entry(stats_key).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0,
                            "cachedTokens": 0, "cost": 0,
                            "rawModel": raw_model, "provider": display, "lastUsed": date_key,
                        })
                    });
                    accumulate(entry, m);
                    touch_last_used(entry, &date_key);
                }
            }

            if let Some(Value::Object(accounts)) = day.get("byAccount") {
                for (conn_id, a) in accounts {
                    let account_name = ctx
                        .connection_map
                        .get(conn_id)
                        .cloned()
                        .unwrap_or_else(|| short_account(conn_id));
                    let raw_model = a.get("rawModel").and_then(Value::as_str).unwrap_or("");
                    let provider = a.get("provider").and_then(Value::as_str).unwrap_or("");
                    let display = display_provider(ctx.provider_node_name_map, provider);
                    let account_key = format!("{raw_model} ({provider} - {account_name})");
                    let entry = by_account.entry(account_key).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0,
                            "cachedTokens": 0, "cost": 0,
                            "rawModel": raw_model, "provider": display,
                            "connectionId": conn_id, "accountName": account_name,
                            "lastUsed": date_key,
                        })
                    });
                    accumulate(entry, a);
                    touch_last_used(entry, &date_key);
                }
            }

            if let Some(Value::Object(keys)) = day.get("byApiKey") {
                for (ak_key, ak) in keys {
                    let raw_model = ak.get("rawModel").and_then(Value::as_str).unwrap_or("");
                    let provider = ak.get("provider").and_then(Value::as_str).unwrap_or("");
                    let display = display_provider(ctx.provider_node_name_map, provider);
                    let api_key_val = ak.get("apiKey").and_then(Value::as_str);
                    // `keyInfo?.name || (apiKeyVal ? … : "Local (No API Key)")`
                    let key_name = api_key_val
                        .and_then(|v| ctx.api_key_map.get(v))
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            api_key_val
                                .map(|v| format!("{}...", v.chars().take(8).collect::<String>()))
                        })
                        .unwrap_or_else(|| "Local (No API Key)".to_string());
                    let api_key_masked = mask_api_key(api_key_val);
                    let masked_key = api_key_masked
                        .clone()
                        .unwrap_or_else(|| "local-no-key".into());
                    let entry = by_api_key.entry(ak_key.clone()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0,
                            "cachedTokens": 0, "cost": 0,
                            "rawModel": raw_model, "provider": display,
                            "apiKeyMasked": api_key_masked, "keyName": key_name,
                            "apiKeyKey": masked_key, "lastUsed": date_key,
                        })
                    });
                    accumulate(entry, ak);
                    touch_last_used(entry, &date_key);
                }
            }

            if let Some(Value::Object(endpoints)) = day.get("byEndpoint") {
                for (ep_key, ep) in endpoints {
                    // `ep.endpoint || epKey.split("|")[0] || "Unknown"`.
                    let endpoint = ep
                        .get("endpoint")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .or_else(|| {
                            ep_key
                                .split('|')
                                .next()
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "Unknown".to_string());
                    let raw_model = ep.get("rawModel").and_then(Value::as_str).unwrap_or("");
                    let provider = ep.get("provider").and_then(Value::as_str).unwrap_or("");
                    let display = display_provider(ctx.provider_node_name_map, provider);
                    let entry = by_endpoint.entry(ep_key.clone()).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0,
                            "cachedTokens": 0, "cost": 0,
                            "endpoint": endpoint, "rawModel": raw_model,
                            "provider": display, "lastUsed": date_key,
                        })
                    });
                    accumulate(entry, ep);
                    touch_last_used(entry, &date_key);
                }
            }
        }

        overlay_last_used(
            conn,
            ctx,
            max_days,
            &mut by_model,
            &mut by_account,
            &mut by_api_key,
            &mut by_endpoint,
        )?;
    } else {
        let cutoff = if period == "today" {
            crate::time::local_midnight()
                .map(crate::time::to_iso)
                .unwrap_or_else(now_iso)
        } else {
            let dt = chrono::Utc::now() - chrono::Duration::milliseconds(86_400_000);
            crate::time::to_iso(dt)
        };

        let mut stmt = conn.prepare(
            "SELECT timestamp, provider, model, connectionId, apiKey, endpoint, cost, tokens FROM usageHistory WHERE timestamp >= ?",
        )?;
        let rows: Vec<Value> = stmt
            .query_map([&cutoff], |r| {
                let tokens: Option<String> = r.get("tokens")?;
                Ok(json!({
                    "timestamp": r.get::<_, String>("timestamp")?,
                    "provider": r.get::<_, Option<String>>("provider")?,
                    "model": r.get::<_, Option<String>>("model")?,
                    "connectionId": r.get::<_, Option<String>>("connectionId")?,
                    "apiKey": r.get::<_, Option<String>>("apiKey")?,
                    "endpoint": r.get::<_, Option<String>>("endpoint")?,
                    "cost": r.get::<_, Option<f64>>("cost")?,
                    "tokens": parse_json_opt(tokens.as_deref(), json!({})),
                }))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        for r in &rows {
            let tokens = r.get("tokens").cloned().unwrap_or(json!({}));
            // This path reads `tokens.prompt_tokens || 0` with **no**
            // `input_tokens` fallback, unlike the daily-aggregate path. Same for
            // completion. Only `cachedTokens` accepts the cache-read alias.
            let prompt = tokens
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let completion = tokens
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let cached = token_field(&tokens, &["cached_tokens", "cache_read_input_tokens"]);
            let cost = r.get("cost").and_then(Value::as_f64).unwrap_or(0.0);

            // These stay as raw JSON so the `String()` coercions below match a
            // template literal, where a SQL NULL renders as the four characters
            // `null` rather than an empty string.
            let provider = r.get("provider");
            let model = r.get("model");
            let timestamp = r.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let display = display_provider_value(ctx.provider_node_name_map, provider);

            total_prompt += prompt;
            total_completion += completion;
            total_cached += cached;
            total_cost += cost;

            // `stats.byProvider[r.provider]` — a NULL provider keys the object
            // as the literal string "null".
            let p = by_provider
                .entry(js_str(provider))
                .or_insert_with(zero_counters);
            accumulate_values(p, 1, prompt, completion, cached, cost);

            let model_key = if is_truthy(provider) {
                format!("{} ({})", js_str(model), js_str(provider))
            } else {
                js_str(model)
            };
            let m = by_model.entry(model_key).or_insert_with(|| {
                json!({
                    "requests": 0, "promptTokens": 0, "completionTokens": 0,
                    "cachedTokens": 0, "cost": 0,
                    "rawModel": model.cloned().unwrap_or(Value::Null),
                    "provider": display, "lastUsed": timestamp,
                })
            });
            accumulate_values(m, 1, prompt, completion, cached, cost);
            touch_last_used(m, timestamp);

            if let Some(conn_id) = r.get("connectionId").and_then(Value::as_str) {
                let account_name = ctx
                    .connection_map
                    .get(conn_id)
                    .cloned()
                    .unwrap_or_else(|| short_account(conn_id));
                let account_key =
                    format!("{} ({} - {account_name})", js_str(model), js_str(provider));
                let a = by_account.entry(account_key).or_insert_with(|| {
                    json!({
                        "requests": 0, "promptTokens": 0, "completionTokens": 0,
                        "cachedTokens": 0, "cost": 0,
                        "rawModel": model.cloned().unwrap_or(Value::Null),
                        "provider": display,
                        "connectionId": conn_id, "accountName": account_name,
                        "lastUsed": timestamp,
                    })
                });
                accumulate_values(a, 1, prompt, completion, cached, cost);
                touch_last_used(a, timestamp);
            }

            match r.get("apiKey").and_then(Value::as_str) {
                Some(api_key) => {
                    let key_name = ctx
                        .api_key_map
                        .get(api_key)
                        .and_then(|v| v.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!("{}...", api_key.chars().take(8).collect::<String>())
                        });
                    let masked = mask_api_key(Some(api_key));
                    let ak_key = format!(
                        "{}|{}|{}",
                        masked.clone().unwrap_or_default(),
                        js_str(model),
                        js_or_unknown(provider.and_then(Value::as_str))
                    );
                    let entry = by_api_key.entry(ak_key).or_insert_with(|| {
                        json!({
                            "requests": 0, "promptTokens": 0, "completionTokens": 0,
                            "cachedTokens": 0, "cost": 0,
                            "rawModel": model.cloned().unwrap_or(Value::Null),
                            "provider": display,
                            "apiKeyMasked": masked, "keyName": key_name,
                            "apiKeyKey": masked, "lastUsed": timestamp,
                        })
                    });
                    accumulate_values(entry, 1, prompt, completion, cached, cost);
                    touch_last_used(entry, timestamp);
                }
                None => {
                    let entry = by_api_key
                        .entry("local-no-key".to_string())
                        .or_insert_with(|| {
                            json!({
                                "requests": 0, "promptTokens": 0, "completionTokens": 0,
                                "cachedTokens": 0, "cost": 0,
                                "rawModel": model.cloned().unwrap_or(Value::Null),
                                "provider": display,
                                "apiKeyMasked": Value::Null, "keyName": "Local (No API Key)",
                                "apiKeyKey": "local-no-key", "lastUsed": timestamp,
                            })
                        });
                    accumulate_values(entry, 1, prompt, completion, cached, cost);
                    touch_last_used(entry, timestamp);
                }
            }

            let endpoint = r
                .get("endpoint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Unknown");
            let ep_key = format!(
                "{endpoint}|{}|{}",
                js_str(model),
                js_or_unknown(provider.and_then(Value::as_str))
            );
            let ep = by_endpoint.entry(ep_key).or_insert_with(|| {
                json!({
                    "requests": 0, "promptTokens": 0, "completionTokens": 0,
                    "cachedTokens": 0, "cost": 0,
                    "endpoint": endpoint,
                    "rawModel": model.cloned().unwrap_or(Value::Null),
                    "provider": display, "lastUsed": timestamp,
                })
            });
            accumulate_values(ep, 1, prompt, completion, cached, cost);
            touch_last_used(ep, timestamp);
        }
    }

    let last10_minutes = last_10_minutes(conn, ctx.now_ms)?;
    let active = stats::active_requests(ctx.connection_map);

    // `totalRequests` is the sum over byProvider — not the lifetime counter.
    let total_requests: i64 = by_provider
        .values()
        .map(|p| p.get("requests").and_then(Value::as_i64).unwrap_or(0))
        .sum();

    Ok(json!({
        "totalRequests": total_requests,
        "totalPromptTokens": total_prompt,
        "totalCompletionTokens": total_completion,
        "totalCachedTokens": total_cached,
        "totalCost": total_cost,
        "byProvider": by_provider,
        "byModel": by_model,
        "byAccount": by_account,
        "byApiKey": by_api_key,
        "byEndpoint": by_endpoint,
        "last10Minutes": last10_minutes,
        "pending": stats::pending_snapshot(),
        "activeRequests": active,
        "recentRequests": recent_requests,
        "errorProvider": stats::recent_error_provider(ctx.now_ms),
    }))
}

fn zero_counters() -> Value {
    json!({ "requests": 0, "promptTokens": 0, "completionTokens": 0, "cachedTokens": 0, "cost": 0 })
}

fn accumulate(target: &mut Value, source: &Value) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    for k in [
        "requests",
        "promptTokens",
        "completionTokens",
        "cachedTokens",
    ] {
        bump(obj, k, source.get(k).and_then(Value::as_i64).unwrap_or(0));
    }
    let cost = source.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    let cur = obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    obj.insert("cost".into(), json!(cur + cost));
}

fn accumulate_values(
    target: &mut Value,
    requests: i64,
    prompt: i64,
    completion: i64,
    cached: i64,
    cost: f64,
) {
    let Some(obj) = target.as_object_mut() else {
        return;
    };
    bump(obj, "requests", requests);
    bump(obj, "promptTokens", prompt);
    bump(obj, "completionTokens", completion);
    bump(obj, "cachedTokens", cached);
    let cur = obj.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
    obj.insert("cost".into(), json!(cur + cost));
}

/// Fold `byProvider` entries, which have no per-model metadata.
fn fold_simple(target: &mut Map<String, Value>, source: Option<&Value>) {
    let Some(Value::Object(entries)) = source else {
        return;
    };
    for (provider, p) in entries {
        let entry = target.entry(provider.clone()).or_insert_with(zero_counters);
        accumulate(entry, p);
    }
}

/// `if (dateKey > (entry.lastUsed || ""))` — a lexicographic compare.
fn touch_last_used(entry: &mut Value, candidate: &str) {
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let current = obj.get("lastUsed").and_then(Value::as_str).unwrap_or("");
    if candidate > current {
        obj.insert("lastUsed".into(), json!(candidate));
    }
}

/// `String(value)` for a JSON value: `null` becomes `"null"`, and so do
/// objects and arrays. This is what a template literal does to a SQL column.
fn js_str(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// `v ? … : …` for a JSON value: `null`, `false`, `0` and `""` are falsy.
fn is_truthy(v: Option<&Value>) -> bool {
    !v.is_none_or(crate::json_col::is_falsy)
}

/// `v || "unknown"` — a NULL or empty SQL text column falls through.
fn js_or_unknown(v: Option<&str>) -> String {
    match v {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "unknown".to_string(),
    }
}

/// `providerNodeNameMap[provider] || provider` for a raw column value.
///
/// The `||` falls back to the column itself, so a NULL provider stays JSON
/// `null` in the payload rather than becoming the string `"null"`.
fn display_provider_value(
    map: &indexmap::IndexMap<String, String>,
    provider: Option<&Value>,
) -> Value {
    match provider {
        Some(Value::String(s)) => match map.get(s) {
            Some(display) => json!(display),
            None => json!(s),
        },
        other => other.cloned().unwrap_or(Value::Null),
    }
}

/// `providerNodeNameMap[provider] || provider` for a provider already known to
/// be a string (the daily-aggregate path, where the key was built by the writer).
fn display_provider(map: &indexmap::IndexMap<String, String>, provider: &str) -> String {
    map.get(provider)
        .cloned()
        .unwrap_or_else(|| provider.to_string())
}

fn short_account(conn_id: &str) -> String {
    format!("Account {}...", conn_id.chars().take(8).collect::<String>())
}

/// The last 100 history rows deduped to at most 20 recent requests.
///
/// Public because the SSE live frame rebuilds `recentRequests` on its own: the
/// `ORDER BY id DESC LIMIT 100` probe is index-only and cheap, where the full
/// stats aggregation it used to piggyback on scans every history row.
pub fn recent_requests_from_history(
    conn: &Connection,
    limit: i64,
    include_cached: bool,
) -> DbResult<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, provider, model, tokens, status FROM usageHistory ORDER BY id DESC LIMIT ?",
    )?;
    let rows: Vec<Value> = stmt
        .query_map([limit], |r| {
            let tokens: Option<String> = r.get("tokens")?;
            let t = parse_json_opt(tokens.as_deref(), json!({}));
            let mut out = serde_json::Map::new();
            out.insert("timestamp".into(), json!(r.get::<_, String>("timestamp")?));
            out.insert(
                "model".into(),
                r.get::<_, Option<String>>("model")?
                    .map_or(Value::Null, Value::String),
            );
            out.insert(
                "provider".into(),
                json!(r.get::<_, Option<String>>("provider")?.unwrap_or_default()),
            );
            out.insert(
                "promptTokens".into(),
                json!(token_field(&t, &["prompt_tokens", "input_tokens"])),
            );
            out.insert(
                "completionTokens".into(),
                json!(token_field(&t, &["completion_tokens", "output_tokens"])),
            );
            if include_cached {
                out.insert(
                    "cachedTokens".into(),
                    json!(token_field(
                        &t,
                        &["cached_tokens", "cache_read_input_tokens"]
                    )),
                );
            }
            out.insert(
                "status".into(),
                json!(
                    r.get::<_, Option<String>>("status")?
                        .unwrap_or_else(|| "ok".into())
                ),
            );
            Ok(Value::Object(out))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in rows {
        let prompt = e.get("promptTokens").and_then(Value::as_i64).unwrap_or(0);
        let completion = e
            .get("completionTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if prompt == 0 && completion == 0 {
            continue;
        }
        let timestamp = e.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let minute: String = timestamp.chars().take(16).collect();
        let key = format!(
            "{}|{}|{}|{}|{}",
            e.get("model").and_then(Value::as_str).unwrap_or(""),
            e.get("provider").and_then(Value::as_str).unwrap_or(""),
            prompt,
            completion,
            minute
        );
        if seen.insert(key) {
            out.push(e);
            if out.len() == 20 {
                break;
            }
        }
    }
    Ok(out)
}

/// Overlay precise `lastUsed` timestamps from history for the recent window.
fn overlay_last_used(
    conn: &Connection,
    ctx: &StatsContext<'_>,
    max_days: Option<i64>,
    by_model: &mut Map<String, Value>,
    by_account: &mut Map<String, Value>,
    by_api_key: &mut Map<String, Value>,
    by_endpoint: &mut Map<String, Value>,
) -> DbResult<()> {
    const OVERLAY_WINDOW_MS: i64 = 2 * 86_400_000;
    let cutoff_ms = std::cmp::max(
        max_days.map_or(0, |d| ctx.now_ms - d * 86_400_000),
        ctx.now_ms - OVERLAY_WINDOW_MS,
    );
    let cutoff = crate::time::to_iso(
        chrono::DateTime::from_timestamp_millis(cutoff_ms).unwrap_or_else(chrono::Utc::now),
    );

    let mut stmt = conn.prepare(
        "SELECT timestamp, provider, model, connectionId, apiKey, endpoint FROM usageHistory WHERE timestamp >= ?",
    )?;
    let rows: Vec<LastUsedRow> = stmt
        .query_map([&cutoff], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (ts, provider, model, connection_id, api_key, endpoint) in rows {
        // `e.provider ? … : e.model` — a NULL or empty provider omits the
        // parenthetical entirely.
        let provider_truthy = provider.as_deref().is_some_and(|s| !s.is_empty());
        // A NULL provider interpolates into a template literal as the four
        // characters `null`, not an empty string.
        let provider_str = provider.as_deref().unwrap_or("null");
        let model = model.unwrap_or_default();
        let model_key = if provider_truthy {
            format!("{model} ({provider_str})")
        } else {
            model.clone()
        };
        if let Some(m) = by_model.get_mut(&model_key) {
            touch_last_used_iso(m, &ts);
        }

        if let Some(conn_id) = connection_id.as_deref() {
            let account_name = ctx
                .connection_map
                .get(conn_id)
                .cloned()
                .unwrap_or_else(|| short_account(conn_id));
            // Unlike the model key, this one has no truthiness guard, so a NULL
            // provider is interpolated as the literal `null`.
            let account_key = format!("{model} ({provider_str} - {account_name})");
            if let Some(a) = by_account.get_mut(&account_key) {
                touch_last_used_iso(a, &ts);
            }
        }

        let api_key_key = match api_key.as_deref() {
            Some(k) => format!("{k}|{model}|{}", js_or_unknown(provider.as_deref())),
            None => "local-no-key".to_string(),
        };
        if let Some(a) = by_api_key.get_mut(&api_key_key) {
            touch_last_used_iso(a, &ts);
        }

        let endpoint_key = format!(
            "{}|{model}|{}",
            endpoint
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Unknown"),
            js_or_unknown(provider.as_deref())
        );
        if let Some(e) = by_endpoint.get_mut(&endpoint_key) {
            touch_last_used_iso(e, &ts);
        }
    }
    Ok(())
}

/// `new Date(ts) > new Date(lastUsed)` — a real instant compare, unlike the
/// day-key compare in the aggregate fold.
fn touch_last_used_iso(entry: &mut Value, candidate: &str) {
    let Some(obj) = entry.as_object_mut() else {
        return;
    };
    let current = obj.get("lastUsed").and_then(Value::as_str).unwrap_or("");
    let cand = parse_iso(candidate);
    let cur = parse_iso(current);
    if cand > cur {
        obj.insert("lastUsed".into(), json!(candidate));
    }
}

/// `last10Minutes` — ten one-minute buckets ending at the current minute.
fn last_10_minutes(conn: &Connection, now_ms: i64) -> DbResult<Vec<Value>> {
    const BUCKETS: usize = 10;
    const MINUTE_MS: i64 = 60_000;

    let current_minute_start = (now_ms / MINUTE_MS) * MINUTE_MS;
    let first_bucket = current_minute_start - (BUCKETS as i64 - 1) * MINUTE_MS;

    let mut buckets = vec![
        json!({ "requests": 0, "promptTokens": 0, "completionTokens": 0, "cost": 0 });
        BUCKETS
    ];

    let from = crate::time::to_iso(
        chrono::DateTime::from_timestamp_millis(first_bucket).unwrap_or_else(chrono::Utc::now),
    );
    let to = crate::time::to_iso(
        chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_else(chrono::Utc::now),
    );
    let mut stmt = conn.prepare(
        "SELECT timestamp, promptTokens, completionTokens, cost FROM usageHistory WHERE timestamp >= ? AND timestamp <= ?",
    )?;
    let rows: Vec<TokenRow> = stmt
        .query_map([&from, &to], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    for (timestamp, prompt, completion, cost) in rows {
        let Some(ts) = parse_iso(&timestamp) else {
            continue;
        };
        let minute = (ts.timestamp_millis() / MINUTE_MS) * MINUTE_MS;
        let offset = (minute - first_bucket) / MINUTE_MS;
        if !(0..BUCKETS as i64).contains(&offset) {
            continue;
        }
        let Some(b) = buckets[offset as usize].as_object_mut() else {
            continue;
        };
        bump(b, "requests", 1);
        bump(b, "promptTokens", prompt.unwrap_or(0));
        bump(b, "completionTokens", completion.unwrap_or(0));
        let cur = b.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
        b.insert("cost".into(), json!(cur + cost.unwrap_or(0.0)));
    }
    Ok(buckets)
}

/// `toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit", hour12: false })`
/// on the local wall clock — `HH:MM`, zero-padded, midnight as `00:00`.
fn intraday_label(ts_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts_ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// `toLocaleDateString("en-US", { month: "short", day: "numeric" })` — e.g.
/// `Sep 6`, `Sep 26`. The day is not zero-padded.
fn daily_label(d: chrono::NaiveDate) -> String {
    use chrono::Datelike;
    format!("{} {}", d.format("%b"), d.day())
}

/// Chart buckets for a period.
pub fn get_chart_data(conn: &Connection, period: &str, now_ms: i64) -> DbResult<Vec<Value>> {
    const BUCKET_COUNT: i64 = 24;
    const BUCKET_MS: i64 = 3_600_000;

    if period == "today" || period == "24h" {
        let is_today = period == "today";
        let start_time = if is_today {
            local_midnight_ms().unwrap_or(now_ms)
        } else {
            now_ms - BUCKET_COUNT * BUCKET_MS
        };
        let end_time = start_time + BUCKET_COUNT * BUCKET_MS;

        let mut buckets: Vec<Value> = (0..BUCKET_COUNT)
            .map(|i| {
                json!({
                    "label": intraday_label(start_time + i * BUCKET_MS),
                    "tokens": 0,
                    "cost": 0,
                    "requests": 0,
                })
            })
            .collect();

        let from = crate::time::to_iso(
            chrono::DateTime::from_timestamp_millis(start_time).unwrap_or_else(chrono::Utc::now),
        );
        let mut stmt = conn.prepare(
            "SELECT timestamp, promptTokens, completionTokens, cost FROM usageHistory WHERE timestamp >= ?",
        )?;
        let rows: Vec<TokenRow> = stmt
            .query_map([&from], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        for (timestamp, prompt, completion, cost) in rows {
            let Some(ts) = parse_iso(&timestamp) else {
                continue;
            };
            let t = ts.timestamp_millis();
            if t < start_time {
                continue;
            }
            let idx = if is_today {
                // `today` drops anything past the end of day instead of
                // clamping it into the last bucket.
                if t >= end_time {
                    continue;
                }
                ((t - start_time) / BUCKET_MS) as usize
            } else {
                // `24h` clamps the current partial hour into the last bucket.
                if t > now_ms {
                    continue;
                }
                (((t - start_time) / BUCKET_MS).min(BUCKET_COUNT - 1)) as usize
            };
            let Some(b) = buckets[idx].as_object_mut() else {
                continue;
            };
            bump(b, "tokens", prompt.unwrap_or(0) + completion.unwrap_or(0));
            bump(b, "requests", 1);
            let cur = b.get("cost").and_then(Value::as_f64).unwrap_or(0.0);
            b.insert("cost".into(), json!(cur + cost.unwrap_or(0.0)));
        }
        return Ok(buckets);
    }

    if period == "all" {
        let day_rows = load_days_in_range(conn, None)?;
        let Some((earliest, _)) = day_rows.first() else {
            return Ok(Vec::new());
        };
        let today = chrono::Local::now().date_naive();
        let earliest_date =
            chrono::NaiveDate::parse_from_str(earliest, "%Y-%m-%d").unwrap_or(today);
        let diff_days = ((today - earliest_date).num_days() + 1).max(1);
        let day_map: indexmap::IndexMap<String, Value> = day_rows.into_iter().collect();

        let mut out = Vec::with_capacity(diff_days as usize);
        for i in 0..diff_days {
            let d = earliest_date + chrono::Duration::days(i);
            out.push(chart_bucket(
                daily_label(d),
                day_map.get(&d.format("%Y-%m-%d").to_string()),
            ));
        }
        return Ok(out);
    }

    // `7d` / `30d` / `60d`; anything else falls through to 60.
    let bucket_count = match period {
        "7d" => 7,
        "30d" => 30,
        _ => 60,
    };
    let day_map: indexmap::IndexMap<String, Value> = load_days_in_range(conn, Some(bucket_count))?
        .into_iter()
        .collect();
    let today = chrono::Local::now().date_naive();
    let mut out = Vec::with_capacity(bucket_count as usize);
    for i in 0..bucket_count {
        let d = today - chrono::Duration::days(bucket_count - 1 - i);
        out.push(chart_bucket(
            daily_label(d),
            day_map.get(&d.format("%Y-%m-%d").to_string()),
        ));
    }
    Ok(out)
}

/// Local midnight today as epoch milliseconds.
fn local_midnight_ms() -> Option<i64> {
    crate::time::local_midnight().map(|d| d.timestamp_millis())
}

fn chart_bucket(label: String, day: Option<&Value>) -> Value {
    let tokens = day
        .map(|d| {
            d.get("promptTokens").and_then(Value::as_i64).unwrap_or(0)
                + d.get("completionTokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
        })
        .unwrap_or(0);
    json!({
        "label": label,
        "tokens": tokens,
        "cost": day.and_then(|d| d.get("cost")).and_then(Value::as_f64).unwrap_or(0.0),
        "requests": day.and_then(|d| d.get("requests")).and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Recent logs as preformatted lines, not objects.
///
/// Each line is `DD-MM-YYYY HH:MM:SS | model | PROVIDER | account | sent |
/// received | status` in local time, and the dashboard renders the strings
/// as-is.
pub fn get_recent_logs(
    conn: &Connection,
    limit: i64,
    connection_map: &indexmap::IndexMap<String, String>,
) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT timestamp, provider, model, connectionId, promptTokens, completionTokens, status, tokens FROM usageHistory ORDER BY id DESC LIMIT ?",
    )?;
    let rows: Vec<LogRow> = stmt
        .query_map([limit], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);

    Ok(rows
        .into_iter()
        .map(
            |(timestamp, provider, model, connection_id, prompt, completion, status, tokens)| {
                let ts = parse_iso(&timestamp)
                    .map(|d| {
                        chrono::TimeZone::from_utc_datetime(&chrono::Local, &d.naive_utc())
                            .format("%d-%m-%Y %H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_else(|| timestamp.clone());
                let p = provider
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_uppercase())
                    .unwrap_or_else(|| "-".to_string());
                let m = model
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "-".to_string());
                let account = match connection_id.as_deref() {
                    Some(id) => connection_map
                        .get(id)
                        .cloned()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| id.chars().take(8).collect()),
                    None => "-".to_string(),
                };
                let tk = parse_json_opt(tokens.as_deref(), json!({}));
                let dash = json!("-");
                let sent = prompt
                    .map(|v| json!(v))
                    .or_else(|| tk.get("prompt_tokens").cloned())
                    .unwrap_or(dash.clone());
                let received = completion
                    .map(|v| json!(v))
                    .or_else(|| tk.get("completion_tokens").cloned())
                    .unwrap_or(dash);
                format!(
                    "{ts} | {m} | {p} | {account} | {} | {} | {}",
                    render_num(&sent),
                    render_num(&received),
                    status.unwrap_or_else(|| "-".to_string()),
                )
            },
        )
        .collect())
}

/// Render a JSON scalar the way `String(x)` would inside a template literal.
fn render_num(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

/// `now_ms` passthrough so callers do not need the `time` module.
pub fn current_ms() -> i64 {
    now_ms()
}

/// The DISTINCT provider column from `requestDetails`, not the JSON blob.
///
/// Nothing populates `requestDetails`, so this returns an empty list in
/// practice. The query stays because the table is part of the shared schema and
/// `/api/usage/providers` is a kept route; a database written by 9router still
/// yields its provider list.
pub fn get_distinct_providers(conn: &Connection) -> DbResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT provider FROM requestDetails WHERE provider IS NOT NULL ORDER BY provider ASC",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for t in crate::schema::TABLES {
            conn.execute_batch(&t.create_sql()).unwrap();
        }
        conn
    }

    fn no_cost() -> impl Fn(Option<&str>, Option<&str>, &Value) -> f64 {
        |_, _, _| 0.0
    }

    fn insert_usage(
        conn: &Connection,
        timestamp: &str,
        provider: &str,
        model: &str,
        prompt: i64,
        completion: i64,
    ) {
        let tokens = json!({ "prompt_tokens": prompt, "completion_tokens": completion });
        conn.execute(
            "INSERT INTO usageHistory(timestamp, provider, model, promptTokens, completionTokens, cost, status, tokens, meta) VALUES(?, ?, ?, ?, ?, 0, 'ok', ?, '{}')",
            rusqlite::params![timestamp, provider, model, prompt, completion, stringify_json(&tokens)],
        )
        .unwrap();
    }

    #[test]
    fn save_inserts_then_dedups_identical_entries() {
        let conn = db();
        let cost = no_cost();
        let mut entry = json!({
            "timestamp": "2026-09-26T10:00:00.000Z",
            "provider": "openai", "model": "gpt-4o",
            "tokens": { "prompt_tokens": 10, "completion_tokens": 5 }
        });
        assert!(save_request_usage(&conn, &mut entry, &cost).unwrap());
        assert!(
            !save_request_usage(&conn, &mut entry, &cost).unwrap(),
            "duplicate must not insert"
        );

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM usageHistory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn dedup_backfills_a_missing_endpoint() {
        let conn = db();
        let cost = no_cost();
        let mut first = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "p", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut first, &cost).unwrap();

        let mut second = first.clone();
        second["endpoint"] = json!("/v1/chat/completions");
        assert!(!save_request_usage(&conn, &mut second, &cost).unwrap());

        let endpoint: Option<String> = conn
            .query_row("SELECT endpoint FROM usageHistory", [], |r| r.get(0))
            .unwrap();
        assert_eq!(endpoint.as_deref(), Some("/v1/chat/completions"));
    }

    #[test]
    fn save_increments_the_lifetime_counter_once() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "p", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();
        save_request_usage(&conn, &mut e, &cost).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM _meta WHERE key = 'totalRequestsLifetime'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "1");
    }

    #[test]
    fn save_uses_the_injected_cost() {
        let conn = db();
        let cost = |_: Option<&str>, _: Option<&str>, _: &Value| 1.25f64;
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "p", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();
        assert_eq!(e["cost"], json!(1.25));
    }

    #[test]
    fn daily_aggregate_builds_every_counter_map() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z",
            "provider": "openai", "model": "gpt-4o", "connectionId": "c1",
            "apiKey": "sk-abcdefghijkl", "endpoint": "/v1/chat/completions",
            "tokens": { "prompt_tokens": 10, "completion_tokens": 5, "cached_tokens": 2 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();

        let data: String = conn
            .query_row("SELECT data FROM usageDaily", [], |r| r.get(0))
            .unwrap();
        let day: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(day["requests"], json!(1));
        assert_eq!(day["promptTokens"], json!(10));
        assert_eq!(day["cachedTokens"], json!(2));
        assert_eq!(day["byProvider"]["openai"]["requests"], json!(1));
        assert_eq!(day["byModel"]["gpt-4o|openai"]["rawModel"], json!("gpt-4o"));
        assert_eq!(day["byAccount"]["c1"]["requests"], json!(1));
        assert_eq!(
            day["byApiKey"]["sk-abcdefghijkl|gpt-4o|openai"]["apiKey"],
            json!("sk-abcdefghijkl")
        );
        assert_eq!(
            day["byEndpoint"]["/v1/chat/completions|gpt-4o|openai"]["endpoint"],
            json!("/v1/chat/completions")
        );
    }

    #[test]
    fn no_api_key_uses_the_local_sentinel() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "p", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();
        let data: String = conn
            .query_row("SELECT data FROM usageDaily", [], |r| r.get(0))
            .unwrap();
        let day: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(day["byApiKey"]["local-no-key|m|p"]["apiKey"], Value::Null);
    }

    #[test]
    fn missing_endpoint_uses_unknown_in_the_key() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "p", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();
        let data: String = conn
            .query_row("SELECT data FROM usageDaily", [], |r| r.get(0))
            .unwrap();
        let day: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            day["byEndpoint"]["Unknown|m|p"]["endpoint"],
            json!("Unknown")
        );
    }

    #[test]
    fn model_key_omits_the_pipe_without_a_provider() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();
        let data: String = conn
            .query_row("SELECT data FROM usageDaily", [], |r| r.get(0))
            .unwrap();
        let day: Value = serde_json::from_str(&data).unwrap();
        assert!(day["byModel"].get("m").is_some());
        assert!(day["byProvider"].as_object().unwrap().is_empty());
    }

    #[test]
    fn history_masks_the_api_key() {
        let conn = db();
        insert_usage(&conn, "2026-09-26T10:00:00.000Z", "p", "m", 1, 1);
        conn.execute("UPDATE usageHistory SET apiKey = 'sk-abcdefghijkl'", [])
            .unwrap();
        let rows = get_usage_history(&conn, None, None, None, None).unwrap();
        assert_eq!(rows[0]["apiKeyMasked"], json!("sk-abcde***ijkl"));
    }

    #[test]
    fn history_normalizes_date_bounds() {
        let conn = db();
        insert_usage(&conn, "2026-09-14T17:00:00.000Z", "a", "m1", 1, 1);
        insert_usage(&conn, "2026-09-14T20:00:00.000Z", "b", "m2", 1, 1);
        // `2026-09-15T00:00:00+08:00` is really `2026-09-14T16:00:00.000Z`, so
        // both rows qualify. Compared raw (the old behaviour) the literal string
        // sorts after `2026-09-14T…` and nothing would match.
        assert_eq!(
            get_usage_history(&conn, None, None, Some("2026-09-15T00:00:00+08:00"), None)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn history_filters_apply() {
        let conn = db();
        insert_usage(&conn, "2026-09-01T00:00:00.000Z", "a", "m1", 1, 1);
        insert_usage(&conn, "2026-09-20T00:00:00.000Z", "b", "m2", 1, 1);
        assert_eq!(
            get_usage_history(&conn, Some("a"), None, None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            get_usage_history(&conn, None, Some("m2"), None, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            get_usage_history(&conn, None, None, Some("2026-09-10T00:00:00.000Z"), None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            get_usage_history(&conn, None, None, None, Some("2026-09-10T00:00:00.000Z"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn stats_24h_aggregates_from_history() {
        let conn = db();
        let now = chrono::Utc::now();
        let recent = crate::time::to_iso(now - chrono::Duration::minutes(5));
        insert_usage(&conn, &recent, "openai", "gpt-4o", 10, 5);
        insert_usage(&conn, &recent, "openai", "gpt-4o", 3, 2);

        let conn_map = indexmap::IndexMap::new();
        let node_map = indexmap::IndexMap::new();
        let api_map = Map::new();
        let ctx = StatsContext {
            connection_map: &conn_map,
            provider_node_name_map: &node_map,
            api_key_map: &api_map,
            now_ms: now.timestamp_millis(),
        };
        let stats = get_usage_stats(&conn, "24h", &ctx).unwrap();
        assert_eq!(stats["totalRequests"], json!(2));
        assert_eq!(stats["totalPromptTokens"], json!(13));
        assert_eq!(stats["byProvider"]["openai"]["requests"], json!(2));
        assert_eq!(stats["byModel"]["gpt-4o (openai)"]["requests"], json!(2));
        assert_eq!(stats["last10Minutes"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn stats_daily_summary_uses_daily_rows() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "openai", "model": "gpt-4o",
            "tokens": { "prompt_tokens": 7, "completion_tokens": 3 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();

        let conn_map = indexmap::IndexMap::new();
        let node_map = indexmap::IndexMap::new();
        let api_map = Map::new();
        let ctx = StatsContext {
            connection_map: &conn_map,
            provider_node_name_map: &node_map,
            api_key_map: &api_map,
            now_ms: now_ms(),
        };
        let stats = get_usage_stats(&conn, "all", &ctx).unwrap();
        assert_eq!(stats["totalPromptTokens"], json!(7));
        assert_eq!(stats["byProvider"]["openai"]["requests"], json!(1));
    }

    #[test]
    fn provider_display_name_overrides_the_id() {
        let conn = db();
        let cost = no_cost();
        let mut e = json!({
            "timestamp": "2026-09-26T10:00:00.000Z", "provider": "oc", "model": "m",
            "tokens": { "prompt_tokens": 1, "completion_tokens": 1 }
        });
        save_request_usage(&conn, &mut e, &cost).unwrap();

        let conn_map = indexmap::IndexMap::new();
        let mut node_map = indexmap::IndexMap::new();
        node_map.insert("oc".to_string(), "OpenCode".to_string());
        let api_map = Map::new();
        let ctx = StatsContext {
            connection_map: &conn_map,
            provider_node_name_map: &node_map,
            api_key_map: &api_map,
            now_ms: now_ms(),
        };
        let stats = get_usage_stats(&conn, "all", &ctx).unwrap();
        assert_eq!(stats["byModel"]["m (oc)"]["provider"], json!("OpenCode"));
    }

    #[test]
    fn recent_requests_skip_zero_token_rows_and_dedup() {
        let conn = db();
        insert_usage(&conn, "2026-09-26T10:00:00.000Z", "p", "m", 0, 0);
        insert_usage(&conn, "2026-09-26T10:01:00.000Z", "p", "m", 5, 5);
        insert_usage(&conn, "2026-09-26T10:01:30.000Z", "p", "m", 5, 5);
        let recent = recent_requests_from_history(&conn, 100, true).unwrap();
        assert_eq!(recent.len(), 1, "same minute + same counts is one entry");
    }

    #[test]
    fn chart_today_buckets_by_hour() {
        let conn = db();
        let now = chrono::Utc::now();
        let recent = crate::time::to_iso(now);
        insert_usage(&conn, &recent, "p", "m", 10, 5);
        let buckets = get_chart_data(&conn, "24h", now.timestamp_millis()).unwrap();
        assert_eq!(buckets.len(), 24);
        let total: i64 = buckets
            .iter()
            .map(|b| b["tokens"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(total, 15);
    }

    #[test]
    fn chart_buckets_carry_a_locale_label() {
        let conn = db();
        // `24h` labels are `HH:MM` local time, in the en-US shape the dashboard
        // expects.
        let buckets = get_chart_data(&conn, "24h", 1_789_000_000_000).unwrap();
        assert_eq!(buckets[0]["label"].as_str().unwrap().len(), 5);
        assert_eq!(&buckets[0]["label"].as_str().unwrap()[2..3], ":");

        // `7d` labels are `Mon D`, unpadded day.
        let buckets = get_chart_data(&conn, "7d", now_ms()).unwrap();
        let label = buckets[0]["label"].as_str().unwrap();
        let (month, day) = label.split_once(' ').expect("month day");
        assert_eq!(month.len(), 3, "{label}");
        assert!(day.parse::<u32>().is_ok(), "{label}");
    }

    #[test]
    fn chart_7d_returns_seven_buckets() {
        let conn = db();
        let buckets = get_chart_data(&conn, "7d", now_ms()).unwrap();
        assert_eq!(buckets.len(), 7);
    }

    #[test]
    fn chart_all_returns_empty_without_data() {
        let conn = db();
        assert!(get_chart_data(&conn, "all", now_ms()).unwrap().is_empty());
    }
}
