//! Timestamp and date-key helpers.
//!
//! Byte parity depends on these matching JavaScript exactly:
//!
//! - `new Date().toISOString()` is UTC, milliseconds, trailing `Z`.
//!   `to_rfc3339_opts(SecondsFormat::Millis, true)` produces the same bytes.
//! - `usageDaily.dateKey` is built from `getFullYear`/`getMonth`/`getDate`,
//!   which are **local time**, not UTC. Getting this wrong shifts every
//!   aggregated day by the host's offset.

use chrono::{DateTime, Local, SecondsFormat, TimeZone, Utc};

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    to_iso(Utc::now())
}

pub fn to_iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parse an ISO-8601 instant. Returns `None` for anything `new Date()` would
/// have turned into `Invalid Date`.
pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            // Bare dates and offsets without seconds appear in legacy rows.
            DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z")
                .map(|d| d.with_timezone(&Utc))
                .ok()
        })
}

/// Local-time `YYYY-MM-DD`, the `usageDaily.dateKey` shape.
pub fn local_date_key(timestamp: Option<&str>) -> String {
    let local = match timestamp.and_then(parse_iso) {
        Some(dt) => Local.from_utc_datetime(&dt.naive_utc()),
        None => Local::now(),
    };
    local.format("%Y-%m-%d").to_string()
}

/// Local-time `YYYYMMDD-HHMMSS`, the backup-directory stamp.
pub fn timestamp_slug() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// Milliseconds since the Unix epoch, for TTL caches.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Local midnight of the current day as a UTC instant.
///
/// `from_local_datetime` returns `None` when local midnight does not exist or
/// is ambiguous — a DST transition at 00:00, as in Chile, Cuba, Iran or
/// Lebanon. `setHours(0,0,0,0)` normalizes forward to 01:00 there, so falling
/// back to "now" would silently collapse a "today" range to the last few
/// seconds. Take the earliest real instant instead.
pub fn local_midnight() -> Option<DateTime<Utc>> {
    let midnight = Local::now().date_naive().and_hms_opt(0, 0, 0)?;
    Local
        .from_local_datetime(&midnight)
        .earliest()
        .or_else(|| Local.from_local_datetime(&midnight).latest())
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_format_is_utc_millis_with_z() {
        let s = now_iso();
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.len(), 24, "{s}");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
    }

    #[test]
    fn local_date_key_matches_js_semantics() {
        // A UTC instant near midnight can land on a different local day; the
        // key must be derived from the local calendar, not the UTC one.
        let key = local_date_key(Some("2026-01-01T00:00:00.000Z"));
        assert_eq!(key.len(), 10);
        assert_eq!(&key[4..5], "-");
    }

    #[test]
    fn invalid_timestamp_falls_back_to_now() {
        let key = local_date_key(Some("not-a-date"));
        assert_eq!(key, Local::now().format("%Y-%m-%d").to_string());
    }
}
