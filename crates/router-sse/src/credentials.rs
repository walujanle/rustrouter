//! The credential view a request carries through translation and into every
//! executor.
//!
//! Deliberately a plain owned struct rather than a borrowed trait object: the
//! translators read a fixed handful of fields, and the one write — the session
//! id `translate_request` stashes for the Gemini/Kiro envelope builders to read
//! later in the same call — has to be visible to a later translator, so the
//! struct is passed mutably and then reborrowed immutably.
//!
//! `raw_headers` is stored lower-cased. The header lookup accepts either case,
//! which collapses to a single lower-cased map here.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::providers::model::Transport;

/// One connection's credential material, resolved from the DB row.
#[derive(Debug, Clone, Default)]
pub struct Credentials {
    pub access_token: Option<String>,
    pub api_key: Option<String>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub expires_at: Option<String>,
    pub expires_in: Option<i64>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub project_id: Option<String>,
    pub email: Option<String>,
    pub connection_id: Option<String>,
    pub display_name: Option<String>,
    /// The account label the `▶` request line prints. Set by the
    /// account-selection loop, not read from the DB row.
    pub connection_name: Option<String>,
    /// The provider-defined extras, kept as a map: the keys are provider-defined
    /// (`authMethod`, `profileArn`, `chatgptAccountId`, …) and a struct would
    /// need a Rust change for every provider.
    pub provider_specific_data: Map<String, Value>,
    /// Client request headers, lower-cased keys.
    pub raw_headers: HashMap<String, String>,
    /// The per-request session id: set by `translate_request`, read by the
    /// Gemini and Kiro envelope builders.
    pub client_session_id: Option<String>,
    /// The source-format-matched endpoint for a multi-endpoint provider, set by
    /// the chat pipeline so the executor can skip translation. It overrides the
    /// provider's default URL and headers.
    pub runtime_transport: Option<Transport>,
    /// Any other connection column, in insertion order.
    pub extra: Map<String, Value>,
}

impl Credentials {
    /// The access token, else the API key. An empty string counts as absent.
    pub fn bearer(&self) -> Option<&str> {
        self.access_token
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.api_key.as_deref().filter(|s| !s.is_empty()))
    }

    /// Look up a request header, lower-cased on both sides.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.raw_headers
            .get(&key.to_ascii_lowercase())
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// A `provider_specific_data` entry, when it is a string.
    pub fn psd_str(&self, key: &str) -> Option<&str> {
        self.provider_specific_data.get(key).and_then(Value::as_str)
    }

    /// The id sessions are scoped to: connection id, else email.
    pub fn session_scope_id(&self) -> Option<&str> {
        self.connection_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| self.email.as_deref().filter(|s| !s.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_prefers_the_access_token_then_falls_through() {
        let mut c = Credentials::default();
        assert_eq!(c.bearer(), None);
        c.api_key = Some("k".into());
        assert_eq!(c.bearer(), Some("k"));
        c.access_token = Some("t".into());
        assert_eq!(c.bearer(), Some("t"));
        // An empty access token falls through to the api key.
        c.access_token = Some(String::new());
        assert_eq!(c.bearer(), Some("k"));
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_rejects_empty() {
        let mut c = Credentials::default();
        c.raw_headers
            .insert("x-claude-code-session-id".into(), "s1".into());
        c.raw_headers.insert("x-empty".into(), String::new());
        assert_eq!(c.header("X-Claude-Code-Session-Id"), Some("s1"));
        assert_eq!(c.header("x-empty"), None);
        assert_eq!(c.header("missing"), None);
    }

    #[test]
    fn session_scope_falls_back_to_email() {
        let mut c = Credentials::default();
        assert_eq!(c.session_scope_id(), None);
        c.email = Some("a@b.c".into());
        assert_eq!(c.session_scope_id(), Some("a@b.c"));
        c.connection_id = Some("conn-1".into());
        assert_eq!(c.session_scope_id(), Some("conn-1"));
    }
}
