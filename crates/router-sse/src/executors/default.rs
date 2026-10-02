//! The generic executor every provider without a special one uses.
//!
//! Three things here are registry-driven rather than hardcoded:
//!
//! * the auth descriptor per provider (`transport.auth`), with the
//!   `openai-compatible-*` / `anthropic-compatible-*` shapes as the fallback;
//! * the header hooks named by `auth.hooks`;
//! * the OAuth refresh grants from `oauth.refresh`.
//!
//! The refresh dispatch is deliberately narrow: a provider is only refreshed
//! when it appears in *both* the grant table and the refresher list.

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::credentials::Credentials;
use crate::executors::executor::{ExecError, Executor, ExecutorLog, insert_header};
use crate::executors::http::{ProxyOptions, prepare_send};
use crate::executors::identity::{kilocode_org_header, oauth_client_id};
use crate::executors::oauth::RefreshedCredentials;
use crate::providers::model::Transport;
use crate::providers::registry::registry;
use crate::providers::shared::{
    ANTHROPIC_API_VERSION, ANTHROPIC_COMPAT_BASE, OPENAI_COMPAT_BASE, merge_anthropic_beta,
    select_anthropic_beta,
};
use crate::providers::ui::{is_anthropic_compatible_provider, is_openai_compatible_provider};
use crate::translator::concerns::param_support::strip_unsupported_params;
use crate::utils::reasoning_injector::inject_reasoning_content;

/// The stored `providerSpecificData.apiType`, when it is one of the two known
/// values. Read before falling back to the id suffix.
fn stored_api_type(credentials: &Credentials) -> Option<&str> {
    credentials
        .psd_str("apiType")
        .filter(|t| *t == "chat" || *t == "responses")
}

/// The `baseUrl` a synthetic `*-compatible-*` node carries on its credential.
fn compat_base_url(credentials: &Credentials) -> Option<&str> {
    credentials.psd_str("baseUrl").filter(|s| !s.is_empty())
}

/// The `openai-compatible-` / `anthropic-compatible-` URL branch.
///
/// Returns `None` for a normal registry provider, which resolves its URL from
/// `baseUrl`/`baseUrls` instead.
pub fn compat_build_url(provider: &str, credentials: &Credentials) -> Option<String> {
    if is_openai_compatible_provider(provider) {
        let path = match stored_api_type(credentials) {
            Some("responses") => "/responses",
            Some(_) => "/chat/completions",
            None if provider.contains("responses") => "/responses",
            None => "/chat/completions",
        };
        let base = compat_base_url(credentials).unwrap_or(OPENAI_COMPAT_BASE);
        return Some(format!("{}{path}", base.trim_end_matches('/')));
    }
    if is_anthropic_compatible_provider(provider) {
        let base = compat_base_url(credentials).unwrap_or(ANTHROPIC_COMPAT_BASE);
        return Some(format!("{}/messages", base.trim_end_matches('/')));
    }
    None
}

/// One resolved auth descriptor.
#[derive(Debug, Clone, Default)]
pub struct AuthDescriptor {
    pub combined: bool,
    pub header: Option<String>,
    pub scheme: Option<String>,
    pub api_key_header: Option<String>,
    pub api_key_scheme: Option<String>,
    pub oauth_header: Option<String>,
    pub oauth_scheme: Option<String>,
    pub anthropic_version: bool,
    pub hooks: Vec<String>,
}

impl AuthDescriptor {
    /// Parse a `transport.auth` value. Both shapes are handled: the
    /// `combined` one and the split `{apiKey, oauth}` one.
    pub fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        if obj.get("combined").and_then(Value::as_bool) == Some(true) {
            return Some(Self {
                combined: true,
                header: obj
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                scheme: obj
                    .get("scheme")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                anthropic_version: obj
                    .get("anthropicVersion")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                hooks: string_list(obj.get("hooks")),
                ..Default::default()
            });
        }
        let branch = |key: &str| obj.get(key).and_then(Value::as_object);
        let api_key = branch("apiKey");
        let oauth = branch("oauth");
        if api_key.is_none() && oauth.is_none() {
            return None;
        }
        let header = |b: Option<&Map<String, Value>>, k: &str| {
            b.and_then(|b| b.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Some(Self {
            combined: false,
            api_key_header: header(api_key, "header"),
            api_key_scheme: header(api_key, "scheme"),
            oauth_header: header(oauth, "header"),
            oauth_scheme: header(oauth, "scheme"),
            anthropic_version: obj
                .get("anthropicVersion")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            hooks: string_list(obj.get("hooks")),
            ..Default::default()
        })
    }

    /// The auth shape a provider with no registry auth entry falls back to.
    fn fallback(provider: &str, config: &Transport) -> Self {
        if is_anthropic_compatible_provider(provider) {
            return Self {
                api_key_header: Some("x-api-key".into()),
                api_key_scheme: Some("raw".into()),
                oauth_header: Some("Authorization".into()),
                oauth_scheme: Some("bearer".into()),
                anthropic_version: true,
                ..Default::default()
            };
        }
        if config.format_or_default() == "claude" {
            return Self {
                combined: true,
                header: Some("x-api-key".into()),
                scheme: Some("raw".into()),
                anthropic_version: true,
                ..Default::default()
            };
        }
        Self {
            combined: true,
            header: Some("Authorization".into()),
            scheme: Some("bearer".into()),
            ..Default::default()
        }
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Set one auth header, applying the `bearer` scheme when asked.
fn set_auth(
    headers: &mut HeaderMap,
    header: Option<&str>,
    scheme: Option<&str>,
    token: &str,
) -> Result<(), ExecError> {
    let Some(header) = header else {
        return Ok(());
    };
    let value = if scheme == Some("bearer") {
        format!("Bearer {token}")
    } else {
        token.to_string()
    };
    insert_header(headers, header, &value)
}

/// Apply the descriptor's auth to the headers.
///
/// `combined` always sets the header, even when both tokens are absent — the
/// legacy shape sends `Bearer undefined`. The split shape sets only the branch
/// whose token is present.
pub fn apply_auth(
    headers: &mut HeaderMap,
    desc: &AuthDescriptor,
    credentials: &Credentials,
) -> Result<(), ExecError> {
    if desc.combined {
        // An empty string is falsy and falls through.
        let token = credentials
            .api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                credentials
                    .access_token
                    .as_deref()
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("undefined");
        set_auth(
            headers,
            desc.header.as_deref(),
            desc.scheme.as_deref(),
            token,
        )?;
        if desc.anthropic_version && !headers.contains_key("anthropic-version") {
            insert_header(headers, "anthropic-version", ANTHROPIC_API_VERSION)?;
        }
        return Ok(());
    }

    if let Some(api_key) = credentials.api_key.as_deref().filter(|s| !s.is_empty()) {
        set_auth(
            headers,
            desc.api_key_header.as_deref(),
            desc.api_key_scheme.as_deref(),
            api_key,
        )?;
    } else if let Some(token) = credentials
        .access_token
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        set_auth(
            headers,
            desc.oauth_header.as_deref(),
            desc.oauth_scheme.as_deref(),
            token,
        )?;
    }
    if desc.anthropic_version && !headers.contains_key("anthropic-version") {
        insert_header(headers, "anthropic-version", ANTHROPIC_API_VERSION)?;
    }
    Ok(())
}

/// Run the descriptor's header hooks before auth, so a hook cannot clobber the
/// token.
pub fn apply_header_hooks(
    headers: &mut HeaderMap,
    desc: &AuthDescriptor,
    credentials: &Credentials,
) -> Result<(), ExecError> {
    for hook in &desc.hooks {
        if hook == "kilocodeOrg"
            && let Some((k, v)) = kilocode_org_header(credentials.psd_str("orgId"))
        {
            insert_header(headers, k, &v)?;
        }
    }
    Ok(())
}

/// The registry auth descriptor for a provider, with the fallback applied.
pub fn auth_descriptor(provider: &str, config: &Transport) -> AuthDescriptor {
    config
        .auth
        .as_ref()
        .and_then(AuthDescriptor::from_value)
        .unwrap_or_else(|| AuthDescriptor::fallback(provider, config))
}

/// Build the headers for the default executor.
///
/// A `runtime_transport` replaces both the configured headers and the auth
/// descriptor — the matched endpoint is a different upstream with its own auth.
pub fn build_default_headers(
    provider: &str,
    config: &Transport,
    credentials: &Credentials,
    stream: bool,
    model: &str,
    body: Option<&Value>,
) -> Result<HeaderMap, ExecError> {
    let effective = credentials.runtime_transport.as_ref().unwrap_or(config);
    let mut headers = HeaderMap::new();
    insert_header(&mut headers, "content-type", "application/json")?;
    if let Some(configured) = effective.headers.as_ref() {
        for (k, v) in configured {
            insert_header(
                &mut headers,
                k,
                &crate::translator::concerns::primitives::js_string(v),
            )?;
        }
    }

    let desc = auth_descriptor(provider, effective);
    apply_header_hooks(&mut headers, &desc, credentials)?;
    apply_auth(&mut headers, &desc, credentials)?;

    // An `anthropic-compatible-*` node fronting a real Claude model needs the
    // same beta flags a first-party Claude request sends; the model id gates it
    // so a node fronting GLM is left untouched. A client that already sent its
    // own `anthropic-beta` header keeps its flags — they are merged after the
    // selected set, not replaced.
    let is_claude_model = model.starts_with("claude-");
    let client_beta = credentials.header("anthropic-beta");
    if !model.is_empty() && is_anthropic_compatible_provider(provider) && is_claude_model {
        insert_header(
            &mut headers,
            "Anthropic-Beta",
            &merge_anthropic_beta(&[Some(&select_anthropic_beta(model, body)), client_beta]),
        )?;
    }

    if is_anthropic_compatible_provider(provider) {
        let base_url = compat_base_url(credentials).unwrap_or("");
        let is_official = base_url.is_empty() || base_url.contains("api.anthropic.com");
        if !is_official {
            // Some third-party gateways require Bearer auth alongside x-api-key.
            if let Some(api_key) = credentials.api_key.as_deref().filter(|s| !s.is_empty())
                && !headers.contains_key("authorization")
            {
                insert_header(&mut headers, "authorization", &format!("Bearer {api_key}"))?;
            }
            for key in ["anthropic-dangerous-direct-browser-access", "x-app"] {
                headers.remove(key);
            }
            // `claude-code-20250219` is a first-party marker; strip it and drop
            // the header entirely when nothing is left.
            for key in ["anthropic-beta"] {
                if let Some(value) = headers
                    .get(key)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
                {
                    let filtered = value
                        .split(',')
                        .map(str::trim)
                        .filter(|f| !f.is_empty() && *f != "claude-code-20250219")
                        .collect::<Vec<_>>()
                        .join(",");
                    if filtered.is_empty() {
                        headers.remove(key);
                    } else {
                        insert_header(&mut headers, key, &filtered)?;
                    }
                }
            }
        }
    }

    if stream {
        insert_header(&mut headers, "accept", "text/event-stream")?;
    }
    Ok(headers)
}

/// Fold the requested JSON schema into a system prompt for
/// `openai-compatible-*` nodes without native Structured Output.
pub fn apply_json_schema_fallback(provider: &str, body: Value) -> Value {
    if !is_openai_compatible_provider(provider) {
        return body;
    }
    let Some(response_format) = body.get("response_format") else {
        return body;
    };
    if response_format.get("type").and_then(Value::as_str) != Some("json_schema") {
        return body;
    }
    let Some(schema) = response_format
        .get("json_schema")
        .and_then(|j| j.get("schema"))
    else {
        return body;
    };
    let schema_json = serde_json::to_string_pretty(schema).unwrap_or_default();
    let prompt = format!(
        "You must respond with valid JSON that strictly follows this JSON schema:\n```json\n{schema_json}\n```\nRespond ONLY with the JSON object, no other text."
    );

    let mut messages: Vec<Value> = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let system_index = messages
        .iter()
        .position(|m| m.get("role").and_then(Value::as_str) == Some("system"));
    match system_index {
        Some(i) => {
            let content = messages[i].get("content").cloned();
            match content {
                Some(Value::String(s)) => {
                    messages[i]["content"] = json!(format!("{s}\n\n{prompt}"));
                }
                Some(Value::Array(mut parts)) => {
                    parts.push(json!({"type": "text", "text": format!("\n\n{prompt}")}));
                    messages[i]["content"] = Value::Array(parts);
                }
                _ => {}
            }
        }
        None => messages.insert(0, json!({"role": "system", "content": prompt})),
    }

    let mut body = body;
    if let Some(obj) = body.as_object_mut() {
        obj.insert("messages".into(), Value::Array(messages));
        obj.insert("response_format".into(), json!({"type": "json_object"}));
    }
    body
}

/// The default executor. One instance per provider id, built once.
pub struct DefaultExecutor {
    provider: String,
    config: Transport,
}

impl DefaultExecutor {
    /// Build the executor from the provider's registry transport.
    ///
    /// An unknown provider gets an empty transport; only real provider ids reach
    /// this constructor.
    pub fn new(provider: &str) -> Self {
        let config = registry()
            .transport(provider)
            .cloned()
            .unwrap_or_else(|| serde_json::from_value(json!({})).expect("empty transport"));
        Self {
            provider: provider.to_string(),
            config,
        }
    }
}

#[async_trait]
impl Executor for DefaultExecutor {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn config(&self) -> &Transport {
        &self.config
    }

    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        // The runtime transport (multi-endpoint provider) wins over everything:
        // it is the endpoint whose format already matches the client's.
        if let Some(rt) = credentials.runtime_transport.as_ref()
            && let Some(base) = rt.base_url.as_deref()
        {
            return Ok(match rt.url_suffix.as_deref() {
                Some(suffix) => format!("{base}{suffix}"),
                None => base.to_string(),
            });
        }
        if let Some(url) = compat_build_url(&self.provider, credentials) {
            return Ok(url);
        }
        if let Some(suffix) = self.config.url_suffix.as_deref()
            && let Some(base) = self.config.base_url.as_deref()
        {
            return Ok(format!("{base}{suffix}"));
        }
        if let Some(base) = self.config.base_url.as_deref()
            && base.contains("{accountId}")
        {
            let account_id = credentials.psd_str("accountId").unwrap_or("");
            if account_id.is_empty() {
                return Err(ExecError::MissingAccountId(self.provider.clone()));
            }
            return Ok(base.replace("{accountId}", account_id));
        }
        let urls = self.get_base_urls();
        urls.get(url_index)
            .or_else(|| urls.first())
            .cloned()
            .or_else(|| self.config.base_url.clone())
            .ok_or_else(|| ExecError::Build(format!("{} has no base URL", self.provider)))
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        _url: &str,
        model: &str,
        body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        build_default_headers(
            &self.provider,
            &self.config,
            credentials,
            stream,
            model,
            body,
        )
    }

    fn transform_request(
        &self,
        model: &str,
        body: Value,
        _stream: bool,
        _credentials: &Credentials,
    ) -> Value {
        let mut transformed = apply_json_schema_fallback(&self.provider, body);
        if quirk_truthy(&self.config, "dropClientMetadata")
            && let Some(obj) = transformed.as_object_mut()
        {
            obj.shift_remove("client_metadata");
        }
        if transformed.is_object() {
            strip_unsupported_params(Some(self.provider.as_str()), model, &mut transformed);
        }
        inject_reasoning_content(Some(self.provider.as_str()), model, transformed)
    }

    async fn refresh_credentials(
        &self,
        credentials: &Credentials,
        log: Option<&dyn ExecutorLog>,
        proxy_options: &ProxyOptions,
    ) -> Option<RefreshedCredentials> {
        let refresh_token = credentials.refresh_token.clone()?;
        let result =
            refresh_for_provider(&self.provider, credentials, &refresh_token, proxy_options).await;
        match result {
            Some(refreshed) => {
                if let Some(log) = log {
                    log.info("TOKEN", &format!("{} refreshed", self.provider));
                }
                Some(refreshed)
            }
            None => None,
        }
    }
}

fn quirk_truthy(config: &Transport, key: &str) -> bool {
    config
        .quirks
        .as_ref()
        .and_then(|q| q.get(key))
        .is_some_and(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

// ─── refresh ─────────────────────────────────────────────────────────────

/// One refresh grant, from `oauth.refresh` plus the token URL and client id.
struct RefreshGrant {
    encoding: String,
    url: String,
    scope: Option<String>,
    client_id: Option<String>,
}

/// The refresh grant for a provider, if its `oauth` block declares `refresh`.
fn refresh_grant(provider: &str) -> Option<RefreshGrant> {
    let oauth = registry().oauth(provider)?;
    let refresh = oauth.get("refresh")?;
    Some(RefreshGrant {
        encoding: refresh
            .get("encoding")
            .and_then(Value::as_str)
            .unwrap_or("form")
            .to_string(),
        url: oauth
            .get("tokenUrl")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        scope: refresh
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_string),
        client_id: oauth
            .get("clientId")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Dispatch a refresh to the provider's refresher. A provider with no
/// refresher is not refreshed at all — see the module note on the
/// grant/refresher intersection.
async fn refresh_for_provider(
    provider: &str,
    credentials: &Credentials,
    refresh_token: &str,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    match provider {
        "codex" => refresh_from_grant(provider, credentials, refresh_token, proxy_options).await,
        // Kilocode uses the device-code flow and has no refresh token support.
        _ => None,
    }
}

/// The generic `{grant_type, refresh_token, client_id[, scope]}` refresh
/// request, form- or JSON-encoded.
async fn refresh_from_grant(
    provider: &str,
    _credentials: &Credentials,
    refresh_token: &str,
    proxy_options: &ProxyOptions,
) -> Option<RefreshedCredentials> {
    let grant = refresh_grant(provider)?;
    let client_id = grant.client_id.or_else(|| oauth_client_id(provider))?;
    let mut params = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.to_string()),
        ("client_id".to_string(), client_id),
    ];
    if let Some(scope) = &grant.scope {
        params.push(("scope".to_string(), scope.clone()));
    }

    let tokens = if grant.encoding == "json" {
        let object: Map<String, Value> = params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        post_json(&grant.url, &Value::Object(object), proxy_options).await?
    } else {
        post_form(&grant.url, &params, &[], proxy_options).await?
    };
    Some(tokens_to_credentials(&tokens, Some(refresh_token)))
}

/// The `{accessToken, refreshToken, expiresIn}` shape the refreshers return.
fn tokens_to_credentials(tokens: &Value, fallback_refresh: Option<&str>) -> RefreshedCredentials {
    RefreshedCredentials {
        access_token: tokens
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| fallback_refresh.map(str::to_string)),
        expires_in: tokens.get("expires_in").and_then(Value::as_i64),
        ..Default::default()
    }
}

/// A token/refresh endpoint answers fast; a request that hangs is a dead
/// endpoint, not a slow one. These are short JSON/form calls, not SSE streams,
/// so a whole-request deadline is safe here where a client-level one is not.
const OAUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn post_json(url: &str, body: &Value, proxy_options: &ProxyOptions) -> Option<Value> {
    post_json_with_headers(url, body, &[], proxy_options).await
}

pub async fn post_json_with_headers(
    url: &str,
    body: &Value,
    extra: &[(&str, String)],
    proxy_options: &ProxyOptions,
) -> Option<Value> {
    let target = prepare_send(url, proxy_options).await.ok()?;
    let mut request = target
        .client
        .post(&target.url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(body);
    for (k, v) in extra {
        request = request.header(*k, v.as_str());
    }
    let response = tokio::time::timeout(OAUTH_REQUEST_TIMEOUT, request.send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    tokio::time::timeout(OAUTH_REQUEST_TIMEOUT, response.json())
        .await
        .ok()?
        .ok()
}

pub async fn post_form(
    url: &str,
    params: &[(String, String)],
    extra: &[(&str, String)],
    proxy_options: &ProxyOptions,
) -> Option<Value> {
    let target = prepare_send(url, proxy_options).await.ok()?;
    let mut request = target
        .client
        .post(&target.url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(params);
    for (k, v) in extra {
        request = request.header(*k, v.as_str());
    }
    let response = tokio::time::timeout(OAUTH_REQUEST_TIMEOUT, request.send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    tokio::time::timeout(OAUTH_REQUEST_TIMEOUT, response.json())
        .await
        .ok()?
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compat_urls_come_from_the_credential_base_url() {
        let mut credentials = Credentials::default();
        credentials
            .provider_specific_data
            .insert("baseUrl".into(), json!("https://x.example/v1/"));
        assert_eq!(
            compat_build_url("openai-compatible-chat-abc", &credentials).as_deref(),
            Some("https://x.example/v1/chat/completions")
        );
        assert_eq!(
            compat_build_url("openai-compatible-responses-abc", &credentials).as_deref(),
            Some("https://x.example/v1/responses")
        );
        assert_eq!(
            compat_build_url("anthropic-compatible-abc", &credentials).as_deref(),
            Some("https://x.example/v1/messages")
        );
        assert!(compat_build_url("deepseek", &credentials).is_none());
    }

    #[test]
    fn compat_urls_fall_back_to_the_vendor_defaults() {
        let credentials = Credentials::default();
        assert_eq!(
            compat_build_url("openai-compatible-chat-x", &credentials).as_deref(),
            Some("https://api.openai.com/v1/chat/completions")
        );
        assert_eq!(
            compat_build_url("anthropic-compatible-x", &credentials).as_deref(),
            Some("https://api.anthropic.com/v1/messages")
        );
    }

    #[test]
    fn a_stored_api_type_beats_the_id_suffix() {
        let mut credentials = Credentials::default();
        credentials
            .provider_specific_data
            .insert("apiType".into(), json!("responses"));
        assert_eq!(
            compat_build_url("openai-compatible-chat-x", &credentials).as_deref(),
            Some("https://api.openai.com/v1/responses")
        );
    }

    #[test]
    fn combined_auth_always_sets_the_header_even_without_a_token() {
        let desc = AuthDescriptor {
            combined: true,
            header: Some("Authorization".into()),
            scheme: Some("bearer".into()),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, &desc, &Credentials::default()).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer undefined");
    }

    #[test]
    fn combined_auth_prefers_the_api_key_over_the_access_token() {
        let desc = AuthDescriptor {
            combined: true,
            header: Some("x-api-key".into()),
            scheme: Some("raw".into()),
            ..Default::default()
        };
        let credentials = Credentials {
            api_key: Some("k".into()),
            access_token: Some("t".into()),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, &desc, &credentials).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "k");
    }

    #[test]
    fn split_auth_sets_only_the_present_branch() {
        let desc = AuthDescriptor::from_value(&json!({
            "apiKey": {"header": "x-api-key", "scheme": "raw"},
            "oauth": {"header": "Authorization", "scheme": "bearer"},
        }))
        .unwrap();
        let credentials = Credentials {
            api_key: Some("k".into()),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, &desc, &credentials).unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "k");
        assert!(
            headers.get("authorization").is_none(),
            "the oauth branch is skipped"
        );

        let credentials = Credentials {
            access_token: Some("t".into()),
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        apply_auth(&mut headers, &desc, &credentials).unwrap();
        assert_eq!(headers.get("authorization").unwrap(), "Bearer t");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn anthropic_compatible_without_a_registry_entry_gets_the_split_shape() {
        let config: Transport = serde_json::from_value(json!({})).unwrap();
        let desc = auth_descriptor("anthropic-compatible-x", &config);
        assert_eq!(desc.api_key_header.as_deref(), Some("x-api-key"));
        assert_eq!(desc.oauth_header.as_deref(), Some("Authorization"));
        assert!(desc.anthropic_version);
    }

    #[test]
    fn claude_format_providers_fall_back_to_x_api_key() {
        let config: Transport = serde_json::from_value(json!({"format": "claude"})).unwrap();
        let desc = auth_descriptor("some-claude-node", &config);
        assert!(desc.combined);
        assert_eq!(desc.header.as_deref(), Some("x-api-key"));
        assert!(desc.anthropic_version);
    }

    #[test]
    fn json_schema_fallback_folds_the_schema_into_a_system_message() {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object"}}},
        });
        let out = apply_json_schema_fallback("openai-compatible-chat-x", body);
        assert_eq!(out["response_format"]["type"], json!("json_object"));
        assert_eq!(out["messages"][0]["role"], json!("system"));
        assert!(
            out["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("strictly follows")
        );
    }

    #[test]
    fn json_schema_fallback_appends_to_an_existing_system_message() {
        let body = json!({
            "messages": [{"role": "system", "content": "base"}],
            "response_format": {"type": "json_schema", "json_schema": {"schema": {"type": "object"}}},
        });
        let out = apply_json_schema_fallback("openai-compatible-chat-x", body);
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert!(
            out["messages"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("base\n\n")
        );
    }

    #[test]
    fn json_schema_fallback_is_a_noop_for_other_providers() {
        let body =
            json!({"response_format": {"type": "json_schema", "json_schema": {"schema": {}}}});
        let out = apply_json_schema_fallback("deepseek", body.clone());
        assert_eq!(out, body);
    }

    #[test]
    fn the_default_executor_reads_its_transport_from_the_registry() {
        let executor = DefaultExecutor::new("deepseek");
        assert_eq!(
            executor.config().base_url.as_deref(),
            Some("https://api.deepseek.com/chat/completions")
        );
        // An unknown id gets an empty transport.
        let unknown = DefaultExecutor::new("not-a-provider");
        assert!(unknown.config().base_url.is_none());
    }

    #[test]
    fn refresh_grants_are_derived_from_the_oauth_block() {
        let codex = refresh_grant("codex").unwrap();
        assert_eq!(codex.encoding, "form");
        assert_eq!(
            codex.scope.as_deref(),
            Some("openid profile email offline_access")
        );
        // A provider with no `oauth` block has no grant.
        assert!(refresh_grant("deepseek").is_none());
    }
}
