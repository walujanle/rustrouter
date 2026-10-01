//! The `settings` singleton.
//!
//! The SQLite file is shared with 9router, so `DEFAULT_SETTINGS` declares every
//! key a stored row may hold: an existing row must resolve every key it reads,
//! and a row rustrouter writes must not silently drop keys 9router expects.

use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value, json};

use crate::error::DbResult;
use crate::json_col::{parse_json, stringify_json};

pub const DEFAULT_MITM_ROUTER_BASE: &str = "http://localhost:20128";

/// `HEADROOM_URL`, defaulting to `http://localhost:8787`.
pub fn default_headroom_url() -> String {
    std::env::var("HEADROOM_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "http://localhost:8787".to_string())
}

/// The default settings, in declaration order. Order is the key order of the
/// stored JSON, so it is part of the parity contract.
pub fn default_settings() -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("cloudEnabled".into(), json!(false));
    m.insert("tunnelEnabled".into(), json!(false));
    m.insert("tunnelUrl".into(), json!(""));
    m.insert("tunnelProvider".into(), json!("cloudflare"));
    m.insert("tailscaleEnabled".into(), json!(false));
    m.insert("tailscaleUrl".into(), json!(""));
    m.insert("stickyRoundRobinLimit".into(), json!(3));
    m.insert("providerStrategies".into(), json!({}));
    m.insert("quotaVisibility".into(), json!({}));
    m.insert("comboStrategy".into(), json!("fallback"));
    m.insert("comboStickyRoundRobinLimit".into(), json!(1));
    m.insert("comboStrategies".into(), json!({}));
    m.insert(
        "capacityAdapter".into(),
        json!({
            "vision": { "enabled": true, "roundRobin": false, "models": [] },
            "pdf": { "enabled": false, "roundRobin": false, "models": [] },
            "audioInput": { "enabled": true, "roundRobin": false, "models": [] },
            "videoInput": { "enabled": false, "roundRobin": false, "models": [] },
        }),
    );
    m.insert("requireLogin".into(), json!(true));
    m.insert("requireApiKey".into(), json!(true));
    m.insert("tunnelDashboardAccess".into(), json!(true));
    m.insert("authMode".into(), json!("password"));
    m.insert("ssoType".into(), json!("oidc"));
    m.insert("oidcIssuerUrl".into(), json!(""));
    m.insert("oidcClientId".into(), json!(""));
    m.insert("oidcClientSecret".into(), json!(""));
    m.insert("oidcScopes".into(), json!("openid profile email"));
    m.insert("oidcLoginLabel".into(), json!("Sign in with OIDC"));
    m.insert("samlEntryPoint".into(), json!(""));
    m.insert("samlIssuer".into(), json!("urn:9router:sp"));
    m.insert("samlCert".into(), json!(""));
    m.insert("samlLoginLabel".into(), json!("Sign in with SAML SSO"));
    m.insert("samlAttributeEmail".into(), json!("email"));
    m.insert("samlAttributeName".into(), json!("name"));
    m.insert("enableObservability".into(), json!(false));
    m.insert("observabilityMaxRecords".into(), json!(1000));
    m.insert("observabilityBatchSize".into(), json!(20));
    m.insert("observabilityFlushIntervalMs".into(), json!(5000));
    m.insert("observabilityMaxJsonSize".into(), json!(5));
    m.insert("outboundProxyEnabled".into(), json!(false));
    m.insert("outboundProxyUrl".into(), json!(""));
    m.insert("outboundNoProxy".into(), json!(""));
    m.insert("mitmRouterBaseUrl".into(), json!(DEFAULT_MITM_ROUTER_BASE));
    m.insert("dnsToolEnabled".into(), json!({}));
    m.insert("rtkEnabled".into(), json!(true));
    m.insert("headroomEnabled".into(), json!(false));
    m.insert("headroomUrl".into(), json!(default_headroom_url()));
    m.insert("headroomCompressUserMessages".into(), json!(false));
    m.insert("headroomTimeoutMs".into(), json!(3000));
    m.insert("cavemanEnabled".into(), json!(false));
    m.insert("cavemanLevel".into(), json!("full"));
    m.insert("ponytailEnabled".into(), json!(false));
    m.insert("ponytailLevel".into(), json!("full"));
    m.insert("pxpipeEnabled".into(), json!(false));
    m.insert("pxpipeAutoInstall".into(), json!(true));
    m.insert("pxpipeMinChars".into(), json!(25000));
    m.insert("pxpipeTimeoutMs".into(), json!(15000));
    // rustrouter-only. 9router has no auto quota tracker, only manual bulk
    // enable/disable buttons, so this key has no counterpart there and is
    // appended after the whole 9router set. Off by default.
    m.insert("quotaAutoTrackerEnabled".into(), json!(false));
    // rustrouter-only. Daily update check against the GitHub Releases; 9router
    // polls the npm registry instead. Appended after the 9router set, like the
    // quota-tracker key. On by default.
    m.insert("autoUpdateCheck".into(), json!(true));
    m
}

/// Merge a stored settings object over the defaults.
///
/// `{...DEFAULT_SETTINGS, ...raw}` — defaults first, raw overwrites in place and
/// appends its unknown keys, preserving each side's order.
///
/// A second pass over the default keys looking for `undefined` is deliberately
/// omitted: it is unreachable for JSON-sourced data, because every default key
/// is already defined by the spread and JSON has no `undefined`.
pub fn merge_with_defaults(raw: Option<Value>) -> Value {
    let mut merged = default_settings();
    if let Value::Object(raw) = raw.unwrap_or(Value::Null) {
        for (k, v) in raw {
            merged.insert(k, v);
        }
    }
    migrate_capacity_adapter(&mut merged);
    Value::Object(merged)
}

/// `oc/mimo-v2.5-free` was renamed; rewrite it wherever a capacity adapter
/// lists models.
fn migrate_capacity_adapter(merged: &mut Map<String, Value>) {
    let Some(Value::Object(caps)) = merged.get_mut("capacityAdapter") else {
        return;
    };
    for entry in caps.values_mut() {
        let Value::Object(obj) = entry else { continue };
        let Some(Value::Array(models)) = obj.get_mut("models") else {
            continue;
        };
        for m in models.iter_mut() {
            if m.as_str() == Some("oc/mimo-v2.5-free") {
                *m = Value::String("oc/mimo-v2.6-flash-free".into());
            }
        }
    }
}

/// The raw stored settings object, with no defaults merged.
pub fn read_raw(conn: &Connection) -> DbResult<Value> {
    let row: Option<String> = conn
        .query_row("SELECT data FROM settings WHERE id = 1", [], |r| r.get(0))
        .optional()?;
    Ok(row
        .map(|s| parse_json(&s, Value::Object(Map::new())))
        .unwrap_or_else(|| Value::Object(Map::new())))
}

/// Stored settings with the defaults merged in.
pub fn get_settings(conn: &Connection) -> DbResult<Value> {
    Ok(merge_with_defaults(Some(read_raw(conn)?)))
}

/// Read-merge-write the settings row inside one transaction.
///
/// The caller must run this inside `Db::write` so the read and the write share
/// one `BEGIN IMMEDIATE` transaction; a bare call is still correct for a single
/// process but loses the protection against concurrent updaters.
pub fn update_settings(conn: &Connection, updates: Value) -> DbResult<Value> {
    let current = read_raw(conn)?;
    let mut next = match current {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Value::Object(u) = updates {
        for (k, v) in u {
            next.insert(k, v);
        }
    }
    let next = Value::Object(next);
    conn.execute(
        "INSERT INTO settings(id, data) VALUES(1, ?) ON CONFLICT(id) DO UPDATE SET data = excluded.data",
        [stringify_json(&next)],
    )?;
    Ok(merge_with_defaults(Some(next)))
}

/// The raw stored object, no defaults merged.
pub fn export_settings(conn: &Connection) -> DbResult<Value> {
    read_raw(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&crate::schema::create_table_sql("settings").unwrap())
            .unwrap();
        conn
    }

    #[test]
    fn defaults_keep_expected_key_order() {
        let defaults = default_settings();
        let keys: Vec<&str> = defaults.keys().map(String::as_str).collect();
        assert_eq!(keys[0], "cloudEnabled");
        assert_eq!(keys[1], "tunnelEnabled");
        assert_eq!(keys[6], "stickyRoundRobinLimit");
        // The shared keys end at `pxpipeTimeoutMs`; the rustrouter-only keys
        // follow, so every shared key keeps its position.
        assert_eq!(keys[keys.len() - 3], "pxpipeTimeoutMs");
        assert_eq!(keys[keys.len() - 2], "quotaAutoTrackerEnabled");
        assert_eq!(keys[keys.len() - 1], "autoUpdateCheck");
        assert!(keys.contains(&"headroomEnabled"));
        assert!(keys.contains(&"pxpipeEnabled"));
        assert!(keys.contains(&"samlCert"));
    }

    #[test]
    fn missing_row_yields_defaults() {
        let conn = db();
        let s = get_settings(&conn).unwrap();
        assert_eq!(s["stickyRoundRobinLimit"], json!(3));
        assert_eq!(s["authMode"], json!("password"));
        assert_eq!(s["rtkEnabled"], json!(true));
    }

    #[test]
    fn update_merges_rather_than_replaces() {
        let conn = db();
        update_settings(&conn, json!({ "comboStrategy": "sticky" })).unwrap();
        let s = get_settings(&conn).unwrap();
        assert_eq!(s["comboStrategy"], json!("sticky"));
        assert_eq!(s["stickyRoundRobinLimit"], json!(3), "untouched key lost");
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let conn = db();
        update_settings(&conn, json!({ "futureKey": { "a": 1 } })).unwrap();
        let s = get_settings(&conn).unwrap();
        assert_eq!(s["futureKey"], json!({ "a": 1 }));
    }

    #[test]
    fn raw_stored_key_order_puts_unknown_keys_last() {
        let conn = db();
        // A first-ever update stores the patch alone (raw merge, no defaults),
        // so seed the row the way an install does before asserting order.
        update_settings(&conn, Value::Object(default_settings())).unwrap();
        update_settings(&conn, json!({ "zzzNew": 1 })).unwrap();
        let raw = read_raw(&conn).unwrap();
        let keys: Vec<&String> = raw.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "cloudEnabled");
        assert_eq!(keys[keys.len() - 1], "zzzNew");
    }

    #[test]
    fn capacity_adapter_model_rename_applies() {
        let merged = merge_with_defaults(Some(json!({
            "capacityAdapter": {
                "vision": { "enabled": true, "models": ["oc/mimo-v2.5-free", "keep-me"] }
            }
        })));
        let models = &merged["capacityAdapter"]["vision"]["models"];
        assert_eq!(models[0], json!("oc/mimo-v2.6-flash-free"));
        assert_eq!(models[1], json!("keep-me"));
    }

    #[test]
    fn null_is_not_replaced_by_a_default() {
        // `merged[key] === undefined` is false for null, so null survives.
        let merged = merge_with_defaults(Some(json!({ "comboStrategy": null })));
        assert_eq!(merged["comboStrategy"], Value::Null);
    }

    #[test]
    fn export_returns_raw_without_defaults() {
        let conn = db();
        assert_eq!(export_settings(&conn).unwrap(), json!({}));
        update_settings(&conn, json!({ "a": 1 })).unwrap();
        assert_eq!(export_settings(&conn).unwrap(), json!({ "a": 1 }));
    }
}
