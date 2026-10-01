//! The HTTP client the TUI drives the server with.
//!
//! Same contract: every call returns `{ success, data | error, status_code }`,
//! the `x-9r-cli-token` header carries the CLI token, and a body is only sent on
//! POST/PUT/PATCH. The token is the machine id under the CLI salt — the same
//! derivation the server's guard checks against — so no login is involved.
//!
//! The TUI owns the main thread and the tokio runtime is on the server thread,
//! so this is the blocking `reqwest` client with a 30-second timeout.

use std::time::Duration;

use serde_json::{Value, json};

/// The header the server's guard reads.
const CLI_TOKEN_HEADER: &str = "x-9r-cli-token";

/// One API result. `data` is present only on success, `error` only on failure.
pub struct ApiResult {
    pub success: bool,
    pub data: Value,
    pub error: String,
    /// The HTTP status. Kept on the result shape; no menu reads it today.
    #[allow(dead_code)]
    pub status_code: u16,
}

impl ApiResult {
    fn ok(data: Value, status_code: u16) -> Self {
        Self {
            success: true,
            data,
            error: String::new(),
            status_code,
        }
    }

    fn err(error: impl Into<String>, status_code: u16) -> Self {
        Self {
            success: false,
            data: Value::Null,
            error: error.into(),
            status_code,
        }
    }
}

pub struct Api {
    client: reqwest::blocking::Client,
    base: String,
    token: String,
}

impl Api {
    /// Build a client for `http://localhost:{port}`, sending `token`.
    pub fn new(port: u16, token: String) -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            base: format!("http://localhost:{port}"),
            token,
        })
    }

    /// Never returns `Err`: a transport failure is a `success: false` result
    /// rather than a rejection.
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> ApiResult {
        let url = format!("{}{path}", self.base);
        let mut req = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "PATCH" => self.client.patch(&url),
            "DELETE" => self.client.delete(&url),
            _ => return ApiResult::err(format!("Unsupported method {method}"), 0),
        }
        .header("content-type", "application/json")
        .header(CLI_TOKEN_HEADER, &self.token);
        if let Some(value) = body {
            req = req.json(value);
        }

        let response = match req.send() {
            Ok(r) => r,
            Err(e) => return ApiResult::err(format!("Network error: {e}"), 0),
        };
        let status = response.status().as_u16();
        let text = response.text().unwrap_or_default();
        let parsed: Value = if text.is_empty() {
            json!({})
        } else {
            match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => return ApiResult::err(format!("Failed to parse response: {e}"), status),
            }
        };

        // A 4xx/5xx status or a non-empty `error` field is the failure branch.
        let has_error = parsed
            .get("error")
            .is_some_and(|e| !e.is_null() && e.as_str() != Some(""));
        if status >= 400 || has_error {
            let message = parsed
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            return ApiResult::err(message, status);
        }
        ApiResult::ok(parsed, status)
    }

    fn get(&self, path: &str) -> ApiResult {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: &Value) -> ApiResult {
        self.request("POST", path, Some(body))
    }

    fn put(&self, path: &str, body: &Value) -> ApiResult {
        self.request("PUT", path, Some(body))
    }

    fn patch(&self, path: &str, body: &Value) -> ApiResult {
        self.request("PATCH", path, Some(body))
    }

    fn delete(&self, path: &str) -> ApiResult {
        self.request("DELETE", path, None)
    }

    // ─── Providers ────────────────────────────────────────────────────────

    pub fn get_providers(&self) -> ApiResult {
        self.get("/api/providers")
    }

    pub fn test_provider(&self, id: &str) -> ApiResult {
        self.post(&format!("/api/providers/{id}/test"), &json!({}))
    }

    pub fn delete_provider(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/providers/{id}"))
    }

    pub fn update_connection(&self, id: &str, body: &Value) -> ApiResult {
        self.put(&format!("/api/providers/{id}"), body)
    }

    pub fn create_api_key_provider(&self, body: &Value) -> ApiResult {
        self.post("/api/providers", body)
    }

    // ─── OAuth ────────────────────────────────────────────────────────────

    /// `getOAuthAuthUrl`: codex pins port 1455 and `/auth/callback`; everything
    /// else uses the server's own port and `/callback`.
    pub fn get_oauth_auth_url(&self, provider: &str, port: u16) -> ApiResult {
        let redirect = if provider == "codex" {
            "http://localhost:1455/auth/callback".to_string()
        } else {
            format!("http://localhost:{port}/callback")
        };
        self.get(&format!(
            "/api/oauth/{provider}/authorize?redirect_uri={}",
            urlencode(&redirect)
        ))
    }

    pub fn exchange_oauth_code(&self, provider: &str, body: &Value) -> ApiResult {
        self.post(&format!("/api/oauth/{provider}/exchange"), body)
    }

    pub fn get_oauth_device_code(&self, provider: &str) -> ApiResult {
        self.get(&format!("/api/oauth/{provider}/device-code"))
    }

    pub fn poll_oauth_token(&self, provider: &str, body: &Value) -> ApiResult {
        self.post(&format!("/api/oauth/{provider}/poll"), body)
    }

    // ─── API keys ─────────────────────────────────────────────────────────

    pub fn get_api_keys(&self) -> ApiResult {
        self.get("/api/keys")
    }

    pub fn create_api_key(&self, name: &str) -> ApiResult {
        self.post("/api/keys", &json!({ "name": name }))
    }

    pub fn delete_api_key(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/keys/{id}"))
    }

    // ─── Combos ───────────────────────────────────────────────────────────

    pub fn get_combos(&self) -> ApiResult {
        self.get("/api/combos")
    }

    pub fn create_combo(&self, body: &Value) -> ApiResult {
        self.post("/api/combos", body)
    }

    pub fn update_combo(&self, id: &str, body: &Value) -> ApiResult {
        self.put(&format!("/api/combos/{id}"), body)
    }

    pub fn delete_combo(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/combos/{id}"))
    }

    // ─── CLI tools ────────────────────────────────────────────────────────

    pub fn get_cli_tool_settings(&self, tool: &str) -> ApiResult {
        self.get(&format!("/api/cli-tools/{tool}-settings"))
    }

    pub fn apply_cli_tool_settings(&self, tool: &str, body: &Value) -> ApiResult {
        self.post(&format!("/api/cli-tools/{tool}-settings"), body)
    }

    pub fn reset_cli_tool_settings(&self, tool: &str) -> ApiResult {
        self.delete(&format!("/api/cli-tools/{tool}-settings"))
    }

    // ─── Settings ─────────────────────────────────────────────────────────

    pub fn get_settings(&self) -> ApiResult {
        self.get("/api/settings")
    }

    pub fn update_settings(&self, body: &Value) -> ApiResult {
        self.patch("/api/settings", body)
    }

    pub fn reset_password(&self) -> ApiResult {
        self.post("/api/auth/reset-password", &json!({}))
    }

    // ─── Models ───────────────────────────────────────────────────────────

    /// `getModels`: the internal model table (`provider/model`, with aliases).
    pub fn get_models(&self) -> ApiResult {
        self.get("/api/models")
    }

    /// `getAvailableModels`: the OpenAI-shaped list, `owned_by` = alias or
    /// `combo`.
    pub fn get_available_models(&self) -> ApiResult {
        self.get("/v1/models")
    }

    // ─── Provider nodes (custom providers) ────────────────────────────────

    pub fn get_provider_nodes(&self) -> ApiResult {
        self.get("/api/provider-nodes")
    }

    pub fn create_provider_node(&self, body: &Value) -> ApiResult {
        self.post("/api/provider-nodes", body)
    }

    pub fn update_provider_node(&self, id: &str, body: &Value) -> ApiResult {
        self.put(&format!("/api/provider-nodes/{id}"), body)
    }

    pub fn delete_provider_node(&self, id: &str) -> ApiResult {
        self.delete(&format!("/api/provider-nodes/{id}"))
    }

    // ─── Registry ─────────────────────────────────────────────────────────

    /// `GET /api/registry`: the provider projection (names, aliases,
    /// categories, the model table) the menus render from.
    pub fn get_registry(&self) -> ApiResult {
        self.get("/api/registry")
    }
}

/// Percent-encode the one query value we build.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_escapes_reserved_characters() {
        assert_eq!(
            urlencode("http://localhost:20129/callback"),
            "http%3A%2F%2Flocalhost%3A20129%2Fcallback"
        );
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
    }
}
