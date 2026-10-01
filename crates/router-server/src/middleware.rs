//! The auth middleware: builds `RequestFacts` from the socket and headers, asks
//! `auth::guard` for a verdict, and turns it into a response.
//!
//! There is no peer token minted into the frontend: axum can read the socket
//! directly, so a forwarding header is only believed when the TCP peer is
//! loopback.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::net::SocketAddr;

use crate::auth::guard::{GuardKind, GuardState, RequestFacts, evaluate, extract_api_key};
use crate::state::AppState;

/// Read a cookie by name from a `Cookie` header, without a parser dependency.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get("cookie")?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name)
            && let Some(value) = rest.strip_prefix('=')
        {
            return Some(value.to_string());
        }
    }
    None
}

/// Build `RequestFacts` from the socket and the headers.
pub fn facts_from(headers: &HeaderMap, peer: SocketAddr) -> RequestFacts {
    let h = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    RequestFacts {
        peer: Some(peer.ip()),
        x_forwarded_for: h("x-forwarded-for").map(str::to_string),
        x_real_ip: h("x-real-ip").map(str::to_string),
        origin: h("origin").map(str::to_string),
        host: h("host").map(str::to_string),
        auth_token: cookie(headers, "auth_token"),
        cli_token: h("x-9r-cli-token").map(str::to_string),
    }
}

/// `GuardState` over the live app state.
///
/// `require_login` is a synchronous settings read, so it is resolved lazily and
/// at most once per request: the LLM API path (`/v1/*`) never asks for it, and
/// making it eager would put a SQLite read on every routed request. The other
/// three checks are cheap (a signature verify, a `OnceLock` read, one indexed
/// `SELECT`) and are evaluated only when the path table asks.
struct LiveGuard<'a> {
    state: &'a AppState,
    require_login: std::sync::OnceLock<bool>,
}

impl GuardState for LiveGuard<'_> {
    fn require_login(&self) -> bool {
        *self
            .require_login
            .get_or_init(|| self.state.require_login())
    }

    fn has_valid_cli_token(&self, token: Option<&str>) -> bool {
        self.state.has_valid_cli_token(token)
    }

    fn is_authenticated(&self, auth_token: Option<&str>) -> bool {
        if self.state.session.verify_token(auth_token) {
            return true;
        }
        !self.require_login()
    }

    fn has_valid_session(&self, auth_token: Option<&str>) -> bool {
        self.state.session.verify_token(auth_token)
    }

    fn has_valid_api_key(&self, key: &str) -> bool {
        self.state
            .db
            .with_conn(|conn| router_db::repos::api_keys::validate_api_key(conn, key))
            .unwrap_or(false)
    }
}

/// The middleware.
pub async fn guard(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers().clone();
    let pathname = request.uri().path().to_string();
    let facts = facts_from(&headers, peer);

    // The query string is only read for the LLM API's `?key=` fallback.
    let query_key = request.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "key")
            .map(|(_, v)| v.into_owned())
    });

    let api_key = extract_api_key(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        headers.get("x-api-key").and_then(|v| v.to_str().ok()),
        headers.get("x-goog-api-key").and_then(|v| v.to_str().ok()),
        query_key.as_deref(),
    )
    .map(str::to_string);

    let guard = LiveGuard {
        state: &state,
        require_login: std::sync::OnceLock::new(),
    };

    match evaluate(&pathname, &facts, &guard, api_key.as_deref()) {
        GuardKind::Allow => next.run(request).await,
        GuardKind::Deny { status, error } => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::UNAUTHORIZED);
            (code, axum::Json(json!({ "error": error }))).into_response()
        }
        GuardKind::Redirect { location } => {
            // A 307, so the method and body survive the redirect.
            let mut response = StatusCode::TEMPORARY_REDIRECT.into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(location) {
                response.headers_mut().insert("location", v);
            }
            response
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn cookie_lookup_handles_the_usual_shapes() {
        let h = headers(&[("cookie", "locale=en; auth_token=abc.def.ghi; theme=dark")]);
        assert_eq!(cookie(&h, "auth_token").as_deref(), Some("abc.def.ghi"));
        assert_eq!(cookie(&h, "locale").as_deref(), Some("en"));
        assert_eq!(cookie(&h, "theme").as_deref(), Some("dark"));
        assert_eq!(cookie(&h, "missing"), None);
    }

    #[test]
    fn cookie_lookup_does_not_match_a_prefix() {
        let h = headers(&[("cookie", "auth_token_extra=x")]);
        assert_eq!(cookie(&h, "auth_token"), None);
    }

    #[test]
    fn cookie_value_may_contain_equals_signs() {
        let h = headers(&[("cookie", "auth_token=a=b=c")]);
        assert_eq!(cookie(&h, "auth_token").as_deref(), Some("a=b=c"));
    }

    #[test]
    fn facts_carry_the_socket_and_the_headers() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let h = headers(&[
            ("cookie", "auth_token=jwt"),
            ("x-9r-cli-token", "cli"),
            ("origin", "http://localhost:5173"),
            ("host", "localhost:20129"),
        ]);
        let facts = facts_from(&h, peer);
        assert_eq!(facts.peer, Some("127.0.0.1".parse().unwrap()));
        assert_eq!(facts.auth_token.as_deref(), Some("jwt"));
        assert_eq!(facts.cli_token.as_deref(), Some("cli"));
        assert_eq!(facts.origin.as_deref(), Some("http://localhost:5173"));
        assert!(facts.is_local());
    }

    #[test]
    fn absent_forwarding_headers_are_none_not_empty() {
        let peer: SocketAddr = "203.0.113.9:5000".parse().unwrap();
        let facts = facts_from(&HeaderMap::new(), peer);
        assert!(facts.x_forwarded_for.is_none());
        assert!(facts.x_real_ip.is_none());
        assert!(!facts.is_local());
    }
}
