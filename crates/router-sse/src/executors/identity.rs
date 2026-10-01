//! Cross-provider auth identity headers.
//!
//! Both build a fingerprint the upstream gates on.

use crate::providers::registry::registry;

/// The `kilocodeOrg` hook: only when `providerSpecificData.orgId` is set.
pub fn kilocode_org_header(org_id: Option<&str>) -> Option<(&'static str, String)> {
    let org_id = org_id.filter(|s| !s.is_empty())?;
    Some(("X-Kilocode-OrganizationID", org_id.to_string()))
}

/// The registry's `oauth.<id>.clientId`, used by the generic refresh grants.
pub fn oauth_client_id(provider: &str) -> Option<String> {
    registry()
        .oauth(provider)
        .and_then(|o| o.get("clientId"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kilocode_org_header_is_omitted_without_an_org() {
        assert!(kilocode_org_header(None).is_none());
        assert!(kilocode_org_header(Some("")).is_none());
        assert_eq!(
            kilocode_org_header(Some("o1")),
            Some(("X-Kilocode-OrganizationID", "o1".to_string()))
        );
    }

    #[test]
    fn oauth_client_ids_come_from_the_registry() {
        assert_eq!(
            oauth_client_id("codex").as_deref(),
            Some("app_EMoamEEZ73f0CkXaXp7hrann")
        );
        assert!(oauth_client_id("deepseek").is_none());
    }
}
