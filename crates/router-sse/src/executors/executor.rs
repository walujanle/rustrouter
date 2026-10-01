//! The executor contract and the shared send/retry loop.
//!
//! `execute` is the one place the URL-fallback loop, the per-status retry and
//! the connect timeout live. A provider executor overrides hooks
//! (`build_url`, `build_headers`, `transform_request`, `should_retry`,
//! `refresh_credentials`, `parse_error`) and inherits the loop.
//!
//! The connect timeout is a **time-to-headers** deadline, not a whole-request
//! one. `reqwest::Client::send` resolves when the response head arrives and
//! leaves the body as a stream, so the deadline wraps `send` and is released
//! the moment headers land. A client-level `timeout` would instead kill a
//! long SSE stream mid-flight.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::credentials::Credentials;
use crate::executors::http::{ProxyOptions, SendError, prepare_send};
use crate::executors::oauth::should_refresh_credentials;
use crate::executors::retry::RetryConfig;
use crate::providers::model::Transport;
use crate::runtime_config::{FETCH_CONNECT_TIMEOUT_MS, http_status};

/// Logging hooks an executor can report through.
pub trait ExecutorLog: Send + Sync {
    fn debug(&self, tag: &str, message: &str);
    fn info(&self, tag: &str, message: &str);
    fn error(&self, tag: &str, message: &str);
}

/// `tracing`'s own level filter decides whether this shows.
fn dbg_fetch(message: &str) {
    tracing::debug!(target: "router_sse::executor", "{message}");
}

/// Everything `execute` needs. `body` is owned so each URL attempt transforms a
/// fresh copy, since `transform_request` runs once per iteration.
pub struct ExecuteRequest<'a> {
    pub model: &'a str,
    pub body: Value,
    pub stream: bool,
    pub credentials: &'a Credentials,
    pub cancel: Option<CancellationToken>,
    pub log: Option<&'a dyn ExecutorLog>,
    pub proxy_options: ProxyOptions,
    /// The app layer's resolved session, read by the opencode executors when
    /// they prepare credentials.
    pub provider_session_id: Option<&'a str>,
    /// The detected downstream client, part of the opencode session digest.
    pub client_tool: Option<&'a str>,
}

impl<'a> ExecuteRequest<'a> {
    /// The common case: a request with no session context.
    pub fn new(
        model: &'a str,
        body: Value,
        stream: bool,
        credentials: &'a Credentials,
        proxy_options: ProxyOptions,
    ) -> Self {
        Self {
            model,
            body,
            stream,
            credentials,
            cancel: None,
            log: None,
            proxy_options,
            provider_session_id: None,
            client_tool: None,
        }
    }

    /// Re-point at a credential set the executor built itself (Vertex mints an
    /// access token, the opencode executors add a session field), keeping every
    /// other field.
    pub fn with_credentials<'b>(self, credentials: &'b Credentials) -> ExecuteRequest<'b>
    where
        'a: 'b,
    {
        ExecuteRequest {
            model: self.model,
            body: self.body,
            stream: self.stream,
            credentials,
            cancel: self.cancel,
            log: self.log,
            proxy_options: self.proxy_options,
            provider_session_id: self.provider_session_id,
            client_tool: self.client_tool,
        }
    }
}

/// A synthesized upstream byte stream, already framed as the client expects.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// The upstream body, either untouched (the success path streams it) or
/// buffered (a retry consumed it).
pub enum UpstreamBody {
    Stream(reqwest::Response),
    /// A body the executor built itself — an SSE stream re-framed from a
    /// non-SSE upstream (trae), or a protobuf/NDJSON decoder's output.
    Synthesized(ByteStream),
    Buffered(Bytes),
}

/// A resolved upstream call.
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: UpstreamBody,
    pub url: String,
    pub request_headers: HeaderMap,
}

impl UpstreamResponse {
    /// `response.text()`: consumes the body either way.
    pub async fn text(self) -> Result<String, ExecError> {
        match self.body {
            UpstreamBody::Buffered(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
            UpstreamBody::Stream(response) => response.text().await.map_err(ExecError::Request),
            UpstreamBody::Synthesized(mut stream) => {
                let mut out = Vec::new();
                while let Some(chunk) = stream.next().await {
                    out.extend_from_slice(&chunk.map_err(ExecError::Io)?);
                }
                Ok(String::from_utf8_lossy(&out).into_owned())
            }
        }
    }

    /// Take the body, leaving an empty buffer behind.
    pub fn take_body(&mut self) -> UpstreamBody {
        std::mem::replace(&mut self.body, UpstreamBody::Buffered(Bytes::new()))
    }

    /// The raw upstream byte stream, when the body was not consumed. A buffered
    /// body is handed back as a one-chunk stream so a caller that already
    /// decided not to retry can still read it.
    pub fn into_stream(self) -> Result<reqwest::Response, ExecError> {
        match self.body {
            UpstreamBody::Stream(response) => Ok(response),
            UpstreamBody::Buffered(bytes) => Err(ExecError::BufferedBody(bytes.len())),
            UpstreamBody::Synthesized(_) => Err(ExecError::SynthesizedBody),
        }
    }

    /// The body as a uniform byte stream, whatever variant it is. The streaming
    /// handler uses this so it never has to care which executor produced it.
    pub fn into_byte_stream(self) -> ByteStream {
        match self.body {
            UpstreamBody::Stream(response) => Box::pin(
                response
                    .bytes_stream()
                    .map(|r| r.map_err(std::io::Error::other)),
            ),
            UpstreamBody::Synthesized(stream) => stream,
            UpstreamBody::Buffered(bytes) => {
                let items: Vec<Result<Bytes, std::io::Error>> = if bytes.is_empty() {
                    vec![]
                } else {
                    vec![Ok(bytes)]
                };
                Box::pin(futures::stream::iter(items))
            }
        }
    }
}

/// An executor failure.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("upstream request failed: {0}")]
    Request(reqwest::Error),
    #[error("request cancelled")]
    Cancelled,
    #[error("upstream did not return headers within the connect timeout")]
    ConnectTimeout,
    #[error("could not build the upstream request: {0}")]
    Build(String),
    #[error("the upstream body was already buffered ({0} bytes)")]
    BufferedBody(usize),
    #[error("the upstream body is a synthesized stream, not a live response")]
    SynthesizedBody,
    #[error("reading the upstream body failed: {0}")]
    Io(std::io::Error),
    #[error("proxy required but failed (strictProxy=true): {0}")]
    StrictProxy(String),
    #[error("{0} requires accountId in providerSpecificData")]
    MissingAccountId(String),
    #[error("all {count} URLs failed with status {status}")]
    AllUrlsFailed { count: usize, status: u16 },
}

impl From<SendError> for ExecError {
    fn from(value: SendError) -> Self {
        match value {
            SendError::Build(e) => ExecError::Request(e),
            SendError::StrictProxy(m) => ExecError::StrictProxy(m),
        }
    }
}

/// The shared executor contract: every hook a provider executor may override.
#[async_trait]
pub trait Executor: Send + Sync {
    fn provider(&self) -> &str;

    /// The built registry transport the executor reads its config from.
    fn config(&self) -> &Transport;

    fn no_auth(&self) -> bool {
        self.config().no_auth.unwrap_or(false)
    }

    fn get_base_urls(&self) -> Vec<String> {
        if let Some(list) = self.config().base_urls.as_ref().and_then(Value::as_array) {
            let urls: Vec<String> = list
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !urls.is_empty() {
                return urls;
            }
        }
        self.config()
            .base_url
            .clone()
            .map(|u| vec![u])
            .unwrap_or_default()
    }

    fn get_fallback_count(&self) -> usize {
        self.get_base_urls().len().max(1)
    }

    /// The `openai-compatible-`/`anthropic-compatible-` branches come first
    /// because a synthetic node has no registry transport to read a base URL
    /// from; the user-supplied `baseUrl` lives on the credential.
    fn build_url(
        &self,
        _model: &str,
        _stream: bool,
        url_index: usize,
        credentials: &Credentials,
    ) -> Result<String, ExecError> {
        if let Some(url) = crate::executors::default::compat_build_url(self.provider(), credentials)
        {
            return Ok(url);
        }
        let urls = self.get_base_urls();
        urls.get(url_index)
            .or_else(|| urls.first())
            .cloned()
            .or_else(|| self.config().base_url.clone())
            .ok_or_else(|| ExecError::Build(format!("{} has no base URL", self.provider())))
    }

    fn build_headers(
        &self,
        credentials: &Credentials,
        stream: bool,
        _url: &str,
        _model: &str,
        _body: Option<&Value>,
    ) -> Result<HeaderMap, ExecError> {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        for (k, v) in self.config().headers.iter().flatten() {
            insert_header(
                &mut headers,
                k,
                &crate::translator::concerns::primitives::js_string(v),
            )?;
        }

        if self.provider().starts_with("anthropic-compatible-") {
            if let Some(api_key) = credentials.api_key.as_deref().filter(|s| !s.is_empty()) {
                insert_header(&mut headers, "x-api-key", api_key)?;
            } else if let Some(token) = credentials
                .access_token
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
            }
            if !headers.contains_key("anthropic-version") {
                insert_header(
                    &mut headers,
                    "anthropic-version",
                    crate::providers::shared::ANTHROPIC_API_VERSION,
                )?;
            }
        } else if let Some(token) = credentials.bearer() {
            insert_header(&mut headers, "authorization", &format!("Bearer {token}"))?;
        }

        if stream {
            insert_header(&mut headers, "accept", "text/event-stream")?;
        }
        Ok(headers)
    }

    fn transform_request(
        &self,
        _model: &str,
        body: Value,
        _stream: bool,
        _credentials: &Credentials,
    ) -> Value {
        body
    }

    /// Whether a status should advance to the next fallback URL.
    fn should_retry(&self, status: u16, url_index: usize) -> bool {
        status == http_status::RATE_LIMITED && url_index + 1 < self.get_fallback_count()
    }

    async fn refresh_credentials(
        &self,
        _credentials: &Credentials,
        _log: Option<&dyn ExecutorLog>,
        _proxy_options: &ProxyOptions,
    ) -> Option<crate::executors::oauth::RefreshedCredentials> {
        None
    }

    fn needs_refresh(&self, credentials: &Credentials) -> bool {
        should_refresh_credentials(
            self.provider(),
            credentials,
            crate::session_manager::now_ms() as i64,
        )
    }

    /// Turn a failed response into the error object the pipeline reports.
    fn parse_error(&self, status: u16, body_text: &str) -> Value {
        let message = if body_text.is_empty() {
            format!("HTTP {status}")
        } else {
            body_text.to_string()
        };
        serde_json::json!({ "status": status, "message": message })
    }

    /// `Some(wait)` means the caller should retry the same URL after sleeping;
    /// the attempt counter has already been incremented.
    async fn try_retry(
        &self,
        retry_config: &RetryConfig,
        attempts_by_url: &mut HashMap<usize, u32>,
        url_index: usize,
        status_key: u16,
        log: Option<&dyn ExecutorLog>,
    ) -> Option<Duration> {
        let entry = retry_config.entry(status_key);
        let attempts = attempts_by_url.get(&url_index).copied().unwrap_or(0);
        if entry.attempts == 0 || attempts >= entry.attempts {
            return None;
        }
        let wait_ms = entry.delay_ms;
        let next = attempts + 1;
        attempts_by_url.insert(url_index, next);
        if let Some(log) = log {
            log.debug(
                "RETRY",
                &format!(
                    "{status_key} retry {next}/{} after {}s",
                    entry.attempts,
                    wait_ms / 1000
                ),
            );
        }
        Some(Duration::from_millis(wait_ms))
    }

    /// Run the send/retry loop for one request.
    async fn execute(&self, req: ExecuteRequest<'_>) -> Result<UpstreamResponse, ExecError> {
        let fallback_count = self.get_fallback_count();
        let retry_config = RetryConfig::merged(self.config().retry.as_ref());
        let timeout_ms = self
            .config()
            .timeout_ms
            .filter(|v| *v > 0)
            .map(|v| v as u64)
            .unwrap_or(FETCH_CONNECT_TIMEOUT_MS);

        let mut attempts_by_url: HashMap<usize, u32> = HashMap::new();
        let mut last_error: Option<ExecError> = None;
        let mut last_status: u16 = 0;
        let mut url_index: usize = 0;

        while url_index < fallback_count {
            let url = self.build_url(req.model, req.stream, url_index, req.credentials)?;
            let mut transformed =
                self.transform_request(req.model, req.body.clone(), req.stream, req.credentials);
            // The fingerprint pass parks the rename map on the body for the
            // orchestrator to pick up; the wire body must not carry it.
            if let Some(obj) = transformed.as_object_mut() {
                obj.remove(crate::utils::fingerprint::RENAMED_TOOL_NAMES_FIELD);
            }
            let headers = self.build_headers(
                req.credentials,
                req.stream,
                &url,
                req.model,
                Some(&transformed),
            )?;
            let body_bytes = Bytes::from(
                serde_json::to_vec(&transformed).map_err(|e| ExecError::Build(e.to_string()))?,
            );

            let target = prepare_send(&url, &req.proxy_options).await?;
            let mut builder = target.client.post(&target.url);
            for (name, value) in headers.iter() {
                builder = builder.header(name, value);
            }
            for (name, value) in &target.extra_headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            let request = builder
                .body(body_bytes.clone())
                .build()
                .map_err(ExecError::Request)?;

            dbg_fetch(&format!(
                "{} -> {url} | body={}B | connectTimeout={timeout_ms}ms",
                self.provider().to_uppercase(),
                body_bytes.len()
            ));

            let send = target.client.execute(request);
            let outcome = match req.cancel.as_ref() {
                Some(token) => tokio::select! {
                    biased;
                    _ = token.cancelled() => return Err(ExecError::Cancelled),
                    result = tokio::time::timeout(Duration::from_millis(timeout_ms), send) => result,
                },
                None => tokio::time::timeout(Duration::from_millis(timeout_ms), send).await,
            };

            match outcome {
                Ok(Ok(response)) => {
                    let status = response.status().as_u16();
                    let response_headers = response.headers().clone();
                    dbg_fetch(&format!("{} <- {status}", self.provider().to_uppercase()));

                    let attempts = attempts_by_url.get(&url_index).copied().unwrap_or(0);
                    let entry = retry_config.entry(status);
                    if entry.attempts > 0 && attempts < entry.attempts {
                        let retry = self
                            .try_retry(
                                &retry_config,
                                &mut attempts_by_url,
                                url_index,
                                status,
                                req.log,
                            )
                            .await;
                        if let Some(wait) = retry {
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                    }

                    if self.should_retry(status, url_index) {
                        last_status = status;
                        url_index += 1;
                        continue;
                    }
                    return Ok(UpstreamResponse {
                        status,
                        headers: response_headers,
                        body: UpstreamBody::Stream(response),
                        url,
                        request_headers: headers,
                    });
                }
                Ok(Err(error)) => {
                    dbg_fetch(&format!("{} x {error}", self.provider().to_uppercase()));
                    last_error = Some(ExecError::Request(error));
                }
                Err(_elapsed) => {
                    dbg_fetch(&format!(
                        "{} x connect timeout",
                        self.provider().to_uppercase()
                    ));
                    last_error = Some(ExecError::ConnectTimeout);
                }
            }

            // Network and connect-timeout failures map to the 502 retry config.
            let retry = self
                .try_retry(
                    &retry_config,
                    &mut attempts_by_url,
                    url_index,
                    http_status::BAD_GATEWAY,
                    req.log,
                )
                .await;
            if let Some(wait) = retry {
                tokio::time::sleep(wait).await;
                continue;
            }
            if url_index + 1 < fallback_count {
                url_index += 1;
                continue;
            }
            return Err(last_error.unwrap_or(ExecError::ConnectTimeout));
        }

        Err(last_error.unwrap_or(ExecError::AllUrlsFailed {
            count: fallback_count,
            status: last_status,
        }))
    }
}

/// Insert a header, lower-casing the name the way `HeaderMap` does and failing
/// on a name or value HTTP cannot carry.
pub fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), ExecError> {
    let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| ExecError::Build(format!("invalid header name {name:?}: {e}")))?;
    let value = reqwest::header::HeaderValue::from_str(value)
        .map_err(|e| ExecError::Build(format!("invalid header value for {name}: {e}")))?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_insertion_rejects_a_bad_name_and_value() {
        let mut headers = HeaderMap::new();
        assert!(insert_header(&mut headers, "x-ok", "fine").is_ok());
        assert!(insert_header(&mut headers, "bad header", "v").is_err());
        assert!(insert_header(&mut headers, "x-bad", "line\nbreak").is_err());
    }

    #[test]
    fn a_buffered_body_refuses_to_yield_a_stream() {
        let response = UpstreamResponse {
            status: 429,
            headers: HeaderMap::new(),
            body: UpstreamBody::Buffered(Bytes::from_static(b"{}")),
            url: "https://x".into(),
            request_headers: HeaderMap::new(),
        };
        assert!(matches!(
            response.into_stream(),
            Err(ExecError::BufferedBody(2))
        ));
    }
}
