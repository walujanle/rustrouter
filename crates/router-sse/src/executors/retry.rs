//! Per-status retry policy: the default table merged with a transport's own
//! `retry` block.
//!
//! Two shapes exist in the registry and both are load-bearing: a number is the
//! attempt count with the default delay (`{"429": 0}` disables the retry
//! entirely), and an object carries `{attempts, delayMs}` where a missing
//! `delayMs` falls back to `RETRY_DELAY_MS`.

use std::collections::HashMap;

use serde_json::Value;

use crate::runtime_config::RETRY_DELAY_MS;

/// One resolved entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryEntry {
    pub attempts: u32,
    pub delay_ms: u64,
}

impl RetryEntry {
    const DISABLED: Self = Self {
        attempts: 0,
        delay_ms: RETRY_DELAY_MS,
    };
}

/// Resolve one status code's registry entry into an attempt count and delay.
pub fn resolve_retry_entry(entry: Option<&Value>) -> RetryEntry {
    let Some(entry) = entry else {
        return RetryEntry::DISABLED;
    };
    if let Some(n) = entry.as_u64() {
        return RetryEntry {
            attempts: n as u32,
            delay_ms: RETRY_DELAY_MS,
        };
    }
    let attempts = entry.get("attempts").and_then(Value::as_u64).unwrap_or(0) as u32;
    let delay_ms = entry
        .get("delayMs")
        .and_then(Value::as_u64)
        .unwrap_or(RETRY_DELAY_MS);
    RetryEntry { attempts, delay_ms }
}

/// The resolved retry table: defaults overlaid with a transport's overrides.
#[derive(Debug, Clone, Default)]
pub struct RetryConfig {
    entries: HashMap<u16, RetryEntry>,
}

impl RetryConfig {
    /// The default table plus a transport's overrides.
    pub fn merged(transport_retry: Option<&Value>) -> Self {
        let mut entries = HashMap::new();
        for (status, attempts, delay) in [
            (429u16, 0u32, 0u64),
            (502, 3, 3000),
            (503, 3, 2000),
            (504, 2, 3000),
        ] {
            entries.insert(
                status,
                RetryEntry {
                    attempts,
                    delay_ms: delay,
                },
            );
        }
        if let Some(map) = transport_retry.and_then(Value::as_object) {
            for (key, value) in map {
                if let Ok(status) = key.parse::<u16>() {
                    entries.insert(status, resolve_retry_entry(Some(value)));
                }
            }
        }
        Self { entries }
    }

    /// The resolved entry for a status code, disabled when absent.
    pub fn entry(&self, status: u16) -> RetryEntry {
        self.entries
            .get(&status)
            .copied()
            .unwrap_or(RetryEntry::DISABLED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_the_expected_table() {
        let c = RetryConfig::merged(None);
        assert_eq!(
            c.entry(429),
            RetryEntry {
                attempts: 0,
                delay_ms: 0
            }
        );
        assert_eq!(
            c.entry(502),
            RetryEntry {
                attempts: 3,
                delay_ms: 3000
            }
        );
        assert_eq!(
            c.entry(503),
            RetryEntry {
                attempts: 3,
                delay_ms: 2000
            }
        );
        assert_eq!(
            c.entry(504),
            RetryEntry {
                attempts: 2,
                delay_ms: 3000
            }
        );
        assert_eq!(c.entry(500), RetryEntry::DISABLED);
    }

    #[test]
    fn a_number_override_keeps_the_legacy_delay() {
        let c = RetryConfig::merged(Some(&json!({"429": 0})));
        assert_eq!(
            c.entry(429),
            RetryEntry {
                attempts: 0,
                delay_ms: RETRY_DELAY_MS
            }
        );
    }

    #[test]
    fn an_object_override_may_set_attempts_only() {
        let c = RetryConfig::merged(Some(&json!({"429": {"attempts": 3}})));
        assert_eq!(
            c.entry(429),
            RetryEntry {
                attempts: 3,
                delay_ms: RETRY_DELAY_MS
            }
        );
    }

    #[test]
    fn an_explicit_delay_wins() {
        let c = RetryConfig::merged(Some(&json!({"503": {"attempts": 1, "delayMs": 500}})));
        assert_eq!(
            c.entry(503),
            RetryEntry {
                attempts: 1,
                delay_ms: 500
            }
        );
    }
}
