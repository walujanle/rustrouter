//! Auto quota tracker: disable a connection when its quota is exhausted and
//! re-enable it once usage is available again.
//!
//! This is an addition to the shared schema's behaviour: 9router offers manual
//! bulk "Turn off Empty" / "Turn on Available" buttons and a `quotaAutoPing`
//! that *warms* a 5-hour window; neither disables a connection on its own. The
//! tracker is off unless `settings.quotaAutoTrackerEnabled` is true.
//!
//! Two rules keep it from fighting the user:
//!
//! 1. Only a connection the tracker itself disabled is re-enabled. A
//!    user-disabled connection carries no `quotaAutoDisabled` marker, so the
//!    tracker leaves it alone even when its quota is available.
//! 2. A connection with no quota reading is never disabled. "No data" is not
//!    "exhausted", so a missing quota is treated as available.

use serde_json::{Value, json};

use crate::error::DbResult;
use crate::json_col::is_falsy;

/// Marker written when the tracker disables a connection. Read back on the
/// next pass so the tracker can tell its own disables from the user's.
pub const AUTO_DISABLED_FIELD: &str = "quotaAutoDisabled";
/// When the tracker disabled the connection.
pub const AUTO_DISABLED_AT_FIELD: &str = "quotaAutoDisabledAt";

/// `remaining` wins when it is a finite number; otherwise `used >= total` with a
/// positive `total`. An `unlimited` quota is never exhausted.
pub fn is_quota_exhausted(quota: Option<&Value>) -> bool {
    let Some(quota) = quota else { return false };
    if quota.get("unlimited").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    if let Some(remaining) = finite_number(quota.get("remaining")) {
        return remaining <= 0.0;
    }
    match (
        finite_number(quota.get("used")),
        finite_number(quota.get("total")),
    ) {
        (Some(used), Some(total)) if total > 0.0 => used >= total,
        _ => false,
    }
}

/// A finite JSON number, or a numeric string; anything else is `None`. An
/// empty string is not numeric here, so the value is trimmed before parsing.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        Some(Value::String(s)) if !s.trim().is_empty() => {
            s.trim().parse::<f64>().ok().filter(|f| f.is_finite())
        }
        _ => None,
    }
}

/// Any quota in the snapshot is exhausted. The caller passes the provider's
/// whole `quotas` object; a connection is disabled when any of its quota
/// windows is spent.
pub fn any_quota_exhausted(quotas: Option<&Value>) -> bool {
    let Some(Value::Object(map)) = quotas else {
        return false;
    };
    map.values().any(|q| is_quota_exhausted(Some(q)))
}

/// What the tracker decided for one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaAction {
    /// Disable: quota exhausted and the connection was active.
    Disable,
    /// Re-enable: quota available and the tracker had disabled it.
    Enable,
    /// Nothing to do.
    NoChange,
}

/// Decide what the tracker should do for one connection.
///
/// Pure, so the decision is unit-testable without a database.
pub fn plan_quota_action(
    is_active: bool,
    quota_exhausted: bool,
    was_auto_disabled: bool,
) -> QuotaAction {
    if quota_exhausted {
        // Only an active connection can be disabled, and a connection the
        // tracker already disabled stays disabled.
        if is_active && !was_auto_disabled {
            QuotaAction::Disable
        } else {
            QuotaAction::NoChange
        }
    } else if was_auto_disabled {
        // Quota is available again: re-enable, even if something else flipped
        // `isActive` back on — clearing the marker is the point.
        QuotaAction::Enable
    } else {
        QuotaAction::NoChange
    }
}

/// Whether the tracker is switched on in settings.
pub fn is_enabled(settings: &Value) -> bool {
    settings
        .get("quotaAutoTrackerEnabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Apply the tracker to one connection value, in place, given its quota
/// snapshot. Returns the action taken. The caller persists the connection when
/// the action is not `NoChange`.
pub fn apply_to_connection(
    connection: &mut Value,
    quotas: Option<&Value>,
    now_iso: &str,
) -> QuotaAction {
    let Some(obj) = connection.as_object_mut() else {
        return QuotaAction::NoChange;
    };
    let is_active = obj.get("isActive").and_then(Value::as_bool).unwrap_or(true);
    let was_auto_disabled = obj.get(AUTO_DISABLED_FIELD).is_some_and(|v| !is_falsy(v));
    let action = plan_quota_action(is_active, any_quota_exhausted(quotas), was_auto_disabled);

    match action {
        QuotaAction::Disable => {
            obj.insert("isActive".into(), json!(false));
            obj.insert(AUTO_DISABLED_FIELD.into(), json!(true));
            obj.insert(AUTO_DISABLED_AT_FIELD.into(), json!(now_iso));
        }
        QuotaAction::Enable => {
            obj.insert("isActive".into(), json!(true));
            obj.shift_remove(AUTO_DISABLED_FIELD);
            obj.shift_remove(AUTO_DISABLED_AT_FIELD);
        }
        QuotaAction::NoChange => {}
    }
    action
}

/// Whether the tracker itself disabled this connection.
pub fn was_auto_disabled(connection: &Value) -> bool {
    connection
        .get(AUTO_DISABLED_FIELD)
        .map(|v| !is_falsy(v))
        .unwrap_or(false)
}

/// Persist a connection the tracker changed. `upsert` needs the whole row; the
/// connection value already carries every fixed column, so it is written back
/// verbatim with only `updatedAt` refreshed.
pub fn persist(conn: &rusqlite::Connection, connection: &Value, now_iso: &str) -> DbResult<()> {
    let mut updated = connection.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("updatedAt".into(), json!(now_iso));
    }
    crate::repos::connections::upsert(conn, &updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_reads_remaining_then_used_over_total() {
        assert!(is_quota_exhausted(Some(&json!({ "remaining": 0 }))));
        assert!(is_quota_exhausted(Some(&json!({ "remaining": -1 }))));
        assert!(!is_quota_exhausted(Some(&json!({ "remaining": 5 }))));
        // remaining wins over used/total when present.
        assert!(!is_quota_exhausted(Some(
            &json!({ "remaining": 1, "used": 10, "total": 10 })
        )));
        // No remaining: fall back to used >= total.
        assert!(is_quota_exhausted(Some(
            &json!({ "used": 10, "total": 10 })
        )));
        assert!(is_quota_exhausted(Some(
            &json!({ "used": 11, "total": 10 })
        )));
        assert!(!is_quota_exhausted(Some(
            &json!({ "used": 9, "total": 10 })
        )));
        // total of zero is not exhaustion.
        assert!(!is_quota_exhausted(Some(&json!({ "used": 5, "total": 0 }))));
    }

    #[test]
    fn unlimited_and_missing_are_never_exhausted() {
        assert!(!is_quota_exhausted(Some(
            &json!({ "unlimited": true, "remaining": 0 })
        )));
        assert!(!is_quota_exhausted(None));
        assert!(!is_quota_exhausted(Some(&json!({}))));
    }

    #[test]
    fn numeric_strings_are_read_like_numbers() {
        assert!(is_quota_exhausted(Some(&json!({ "remaining": "0" }))));
        assert!(!is_quota_exhausted(Some(&json!({ "remaining": "" }))));
        assert!(is_quota_exhausted(Some(
            &json!({ "used": "10", "total": "10" })
        )));
    }

    #[test]
    fn any_window_exhausted_disables() {
        let quotas = json!({ "five_hour": { "remaining": 0 }, "weekly": { "remaining": 90 } });
        assert!(any_quota_exhausted(Some(&quotas)));
        let ok = json!({ "five_hour": { "remaining": 10 }, "weekly": { "remaining": 90 } });
        assert!(!any_quota_exhausted(Some(&ok)));
        assert!(!any_quota_exhausted(None));
    }

    #[test]
    fn plan_disables_only_active_non_marked_connections() {
        assert_eq!(plan_quota_action(true, true, false), QuotaAction::Disable);
        // Already auto-disabled: no repeat work.
        assert_eq!(plan_quota_action(true, true, true), QuotaAction::NoChange);
        // User-disabled connection is left alone.
        assert_eq!(plan_quota_action(false, true, false), QuotaAction::NoChange);
        assert_eq!(plan_quota_action(false, true, true), QuotaAction::NoChange);
    }

    #[test]
    fn plan_re_enables_only_the_trackers_own_disables() {
        assert_eq!(plan_quota_action(false, false, true), QuotaAction::Enable);
        // A user-disabled connection with available quota is not re-enabled.
        assert_eq!(
            plan_quota_action(false, false, false),
            QuotaAction::NoChange
        );
        assert_eq!(plan_quota_action(true, false, false), QuotaAction::NoChange);
    }

    #[test]
    fn apply_marks_and_unmarks_the_connection() {
        let mut conn = json!({ "id": "c1", "isActive": true });
        let spent = json!({ "five_hour": { "remaining": 0 } });
        assert_eq!(
            apply_to_connection(&mut conn, Some(&spent), "2026-09-27T00:00:00.000Z"),
            QuotaAction::Disable
        );
        assert_eq!(conn["isActive"], json!(false));
        assert_eq!(conn[AUTO_DISABLED_FIELD], json!(true));
        assert_eq!(
            conn[AUTO_DISABLED_AT_FIELD],
            json!("2026-09-27T00:00:00.000Z")
        );

        let fresh = json!({ "five_hour": { "remaining": 100 } });
        assert_eq!(
            apply_to_connection(&mut conn, Some(&fresh), "2026-09-27T01:00:00.000Z"),
            QuotaAction::Enable
        );
        assert_eq!(conn["isActive"], json!(true));
        assert!(conn.get(AUTO_DISABLED_FIELD).is_none());
        assert!(conn.get(AUTO_DISABLED_AT_FIELD).is_none());
    }

    #[test]
    fn apply_never_touches_a_user_disabled_connection() {
        let mut conn = json!({ "id": "c1", "isActive": false });
        let spent = json!({ "five_hour": { "remaining": 0 } });
        assert_eq!(
            apply_to_connection(&mut conn, Some(&spent), "t"),
            QuotaAction::NoChange
        );
        assert_eq!(conn["isActive"], json!(false));
        let fresh = json!({ "five_hour": { "remaining": 100 } });
        assert_eq!(
            apply_to_connection(&mut conn, Some(&fresh), "t"),
            QuotaAction::NoChange
        );
        assert_eq!(conn["isActive"], json!(false));
    }

    #[test]
    fn the_gate_defaults_off() {
        assert!(!is_enabled(&json!({})));
        assert!(!is_enabled(&json!({ "quotaAutoTrackerEnabled": false })));
        assert!(is_enabled(&json!({ "quotaAutoTrackerEnabled": true })));
    }
}
