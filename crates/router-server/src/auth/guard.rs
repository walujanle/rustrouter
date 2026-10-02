//! The auth guard, path tables included.
//!
//! The tables are data, not policy invented here: an existing deployment's
//! client depends on exactly these classifications. `PROTECTED_API_PATHS` is
//! kept as a documented constant even though nothing reads it, because the
//! published contract includes it.
//!
//! Three jobs are covered here: derive the client IP from the TCP socket,
//! strip client-supplied forwarding headers, and trust them only from a
//! loopback peer. `RequestFacts` is built from `ConnectInfo`, and
//! `is_loopback_addr` below does the loopback test.

use std::net::IpAddr;

use crate::auth::login_limiter::{is_loopback_addr, is_loopback_hostname};

/// The header the CLI sends to prove it is the local tool.
pub const CLI_TOKEN_HEADER: &str = "x-9r-cli-token";
/// `CLI_TOKEN_SALT`.
pub const CLI_TOKEN_SALT: &str = "9r-cli-auth";

/// No auth required; the LLM API has its own key check inside the handler.
pub const PUBLIC_API_PATHS: &[&str] = &[
    "/api/health",
    "/api/init",
    "/api/auth/login",
    "/api/auth/logout",
    "/api/auth/status",
    "/api/version",
    "/api/settings/require-login",
    // Public documentation: the skills page copies this URL for the user to
    // paste into an external AI, which holds no session cookie. See
    // `routes/skills.rs`.
    "/api/skills",
];

/// LLM endpoints that carry their own API-key auth. Kept as root-level
/// rewrites too, because the guard runs before the rewrites.
///
/// `/systemone` is a deliberate hardening divergence: it is not in the root
/// rewrite list, so without this entry its handler's key gate would be the
/// only check and a remote keyless caller would get through whenever that
/// setting is off. Listing it here gives it the same guard-level key
/// requirement as `/v1`.
pub const PUBLIC_PREFIXES: &[&str] = &[
    "/v1",
    "/v1beta",
    "/api/v1",
    "/codex",
    "/responses",
    "/systemone",
];

/// Always require a JWT or the local CLI token, regardless of `requireLogin`.
pub const ALWAYS_PROTECTED: &[&str] = &[
    "/api/shutdown",
    "/api/settings/database",
    "/api/version/shutdown",
    "/api/version/update",
];

/// Declared but never read. It is part of the published contract, so it is
/// preserved verbatim; the deny-by-default branch on `/api/*` makes it
/// redundant. The `/api/cloud` and `/api/tunnel` entries left with those
/// subsystems.
pub const PROTECTED_API_PATHS: &[&str] = &[
    "/api/settings",
    "/api/keys",
    "/api/providers",
    "/api/provider-nodes",
    "/api/proxy-pools",
    "/api/combos",
    "/api/models",
    "/api/usage",
    "/api/oauth",
    "/api/media-providers",
    "/api/pricing",
    "/api/tags",
    "/api/cli-tools",
    "/api/mcp",
    "/api/translator",
];

/// Routes that spawn child processes or write the user's real dotfiles. The
/// tunnel, headroom, cowork, MCP and cursor/kiro auto-import entries left with
/// those subsystems; the three kept CLI-tool writers stay because they mutate
/// `~/.codex`, `~/.claude` and `~/.hermes`.
pub const LOCAL_ONLY_PATHS: &[&str] = &[
    "/api/auth/reset-password",
    "/api/cli-tools/claude-settings",
    "/api/cli-tools/codex-settings",
    "/api/cli-tools/hermes-settings",
];

/// What the guard decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    Allow,
    /// A JSON body `{ "error": <message> }` at `status`.
    Deny {
        status: u16,
        error: &'static str,
    },
    /// Send the browser to `location`.
    Redirect {
        location: &'static str,
    },
}

/// The request facts the guard needs, already derived from the socket by the
/// caller. Anything the client could have forged has been dropped by then.
#[derive(Debug, Clone, Default)]
pub struct RequestFacts {
    /// TCP peer address, from axum's `ConnectInfo`.
    pub peer: Option<IpAddr>,
    /// `x-forwarded-for`, only meaningful when `peer` is loopback.
    pub x_forwarded_for: Option<String>,
    /// `x-real-ip`, likewise.
    pub x_real_ip: Option<String>,
    /// `origin`, absent on legitimate OAuth redirects.
    pub origin: Option<String>,
    /// `host`, used only by the development fallback.
    pub host: Option<String>,
    /// The `auth_token` cookie.
    pub auth_token: Option<String>,
    /// The `x-9r-cli-token` header.
    pub cli_token: Option<String>,
}

impl RequestFacts {
    /// Whether the request arrived through a reverse proxy.
    fn via_proxy(&self) -> bool {
        self.x_forwarded_for.is_some() || self.x_real_ip.is_some()
    }

    /// The peer address after the proxy-trust rule.
    fn peer_is_loopback(&self) -> bool {
        self.peer.is_some_and(is_loopback_addr)
    }

    pub fn is_local(&self) -> bool {
        if self.via_proxy() {
            return false;
        }
        if !self.peer_is_loopback() {
            return false;
        }
        match self.origin.as_deref() {
            // An absent Origin is allowed: OAuth redirects send none.
            None => true,
            Some(origin) => origin_hostname(origin).is_some_and(|h| is_loopback_hostname(&h)),
        }
    }
}

/// The origin's hostname, or `None` for an unparseable origin.
fn origin_hostname(origin: &str) -> Option<String> {
    url::Url::parse(origin).ok()?.host_str().map(str::to_string)
}

/// The host-side state the guard consults. A trait so the decision can be
/// tested without a database.
pub trait GuardState {
    /// Whether `requireLogin` is not disabled.
    fn require_login(&self) -> bool;
    /// Whether the token matches the local CLI token.
    fn has_valid_cli_token(&self, token: Option<&str>) -> bool;
    /// A valid JWT, or login not required.
    fn is_authenticated(&self, auth_token: Option<&str>) -> bool;
    /// A valid JWT, with no `requireLogin` fallback. The always-protected
    /// routes need a real credential even when login is off.
    fn has_valid_session(&self, auth_token: Option<&str>) -> bool;
    /// Whether the key matches a stored API key.
    fn has_valid_api_key(&self, key: &str) -> bool;
}

pub fn is_public_llm_api(pathname: &str) -> bool {
    matches_prefix(PUBLIC_PREFIXES, pathname)
}

pub fn is_public_api(pathname: &str) -> bool {
    is_public_llm_api(pathname) || matches_prefix(PUBLIC_API_PATHS, pathname)
}

/// `Authorization: Bearer`, then `x-api-key`, then `x-goog-api-key`. The
/// fourth source is the `?key=` query parameter; the caller passes that in as
/// `query_key`.
pub fn extract_api_key<'a>(
    authorization: Option<&'a str>,
    x_api_key: Option<&'a str>,
    x_goog_api_key: Option<&'a str>,
    query_key: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(v) = authorization
        && let Some(rest) = v.strip_prefix("Bearer ")
    {
        return Some(rest);
    }
    if let Some(v) = x_api_key {
        return Some(v);
    }
    if let Some(v) = x_goog_api_key {
        return Some(v);
    }
    query_key
}

pub fn can_access_public_llm_api<S: GuardState>(
    facts: &RequestFacts,
    state: &S,
    api_key: Option<&str>,
) -> bool {
    if facts.is_local() {
        return true;
    }
    if state.has_valid_cli_token(facts.cli_token.as_deref()) {
        return true;
    }
    api_key.is_some_and(|k| state.has_valid_api_key(k))
}

pub fn can_access_local_only_route<S: GuardState>(facts: &RequestFacts, state: &S) -> bool {
    if state.has_valid_cli_token(facts.cli_token.as_deref()) {
        return true;
    }
    facts.is_local() && state.is_authenticated(facts.auth_token.as_deref())
}

/// `api_key` is the value `extract_api_key` returned, if any.
pub fn evaluate<S: GuardState>(
    pathname: &str,
    facts: &RequestFacts,
    state: &S,
    api_key: Option<&str>,
) -> GuardKind {
    if matches_prefix(LOCAL_ONLY_PATHS, pathname) && !can_access_local_only_route(facts, state) {
        return GuardKind::Deny {
            status: 403,
            error: "Local only: CLI token required",
        };
    }

    if matches_prefix(ALWAYS_PROTECTED, pathname) {
        if state.has_valid_cli_token(facts.cli_token.as_deref())
            || state.has_valid_session(facts.auth_token.as_deref())
        {
            return GuardKind::Allow;
        }
        return GuardKind::Deny {
            status: 401,
            error: "Unauthorized",
        };
    }

    if is_public_llm_api(pathname) {
        if can_access_public_llm_api(facts, state, api_key) {
            return GuardKind::Allow;
        }
        return GuardKind::Deny {
            status: 401,
            error: "API key required for remote API access",
        };
    }

    // Deny by default for /api/*: the public allow-list bypasses, everything
    // else needs auth.
    if pathname.starts_with("/api/") {
        if is_public_api(pathname) {
            return GuardKind::Allow;
        }
        if state.has_valid_cli_token(facts.cli_token.as_deref())
            || state.is_authenticated(facts.auth_token.as_deref())
        {
            return GuardKind::Allow;
        }
        return GuardKind::Deny {
            status: 401,
            error: "Unauthorized",
        };
    }

    if pathname.starts_with("/dashboard") {
        if !state.require_login() {
            return GuardKind::Allow;
        }
        if state.is_authenticated(facts.auth_token.as_deref()) {
            return GuardKind::Allow;
        }
        return GuardKind::Redirect { location: "/login" };
    }

    if pathname == "/" {
        return GuardKind::Redirect {
            location: "/dashboard",
        };
    }

    GuardKind::Allow
}

fn matches_prefix(list: &[&str], pathname: &str) -> bool {
    list.iter().any(|p| {
        pathname == *p
            || (pathname.starts_with(p) && pathname.as_bytes().get(p.len()) == Some(&b'/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        require_login: bool,
        cli: bool,
        authenticated: bool,
        api_key: bool,
    }

    impl Default for Fake {
        fn default() -> Self {
            Self {
                require_login: true,
                cli: false,
                authenticated: false,
                api_key: false,
            }
        }
    }

    impl GuardState for Fake {
        fn require_login(&self) -> bool {
            self.require_login
        }
        fn has_valid_cli_token(&self, token: Option<&str>) -> bool {
            self.cli && token.is_some()
        }
        fn is_authenticated(&self, token: Option<&str>) -> bool {
            !self.require_login || (self.authenticated && token.is_some())
        }
        fn has_valid_session(&self, token: Option<&str>) -> bool {
            self.authenticated && token.is_some()
        }
        fn has_valid_api_key(&self, _key: &str) -> bool {
            self.api_key
        }
    }

    fn local() -> RequestFacts {
        RequestFacts {
            peer: Some("127.0.0.1".parse().unwrap()),
            ..Default::default()
        }
    }

    fn remote() -> RequestFacts {
        RequestFacts {
            peer: Some("203.0.113.9".parse().unwrap()),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_matching_needs_a_boundary() {
        assert!(matches_prefix(&["/v1"], "/v1"));
        assert!(matches_prefix(&["/v1"], "/v1/models"));
        assert!(!matches_prefix(&["/v1"], "/v1beta"));
        assert!(!matches_prefix(&["/v1"], "/v1x"));
    }

    #[test]
    fn public_paths_and_prefixes() {
        assert!(is_public_api("/api/health"));
        assert!(is_public_api("/api/auth/login"));
        assert!(is_public_api("/v1/chat/completions"));
        assert!(is_public_api("/api/v1/models"));
        assert!(is_public_api("/codex/anything"));
        assert!(is_public_api("/responses"));
        assert!(is_public_api("/api/skills/9router/SKILL.md"));
        assert!(!is_public_api("/api/keys"));
        // `/systemone` is a root rewrite not in the list above, so without the
        // entry it would fall through every branch and reach the handler
        // unauthenticated. Listing it gives it the same guard-level key
        // requirement as `/v1`.
        assert!(is_public_llm_api("/systemone"));
    }

    #[test]
    fn local_requests_allow_the_llm_api() {
        let state = Fake::default();
        assert!(can_access_public_llm_api(&local(), &state, None));
        assert!(!can_access_public_llm_api(&remote(), &state, None));
        assert_eq!(
            evaluate("/v1/models", &local(), &state, None),
            GuardKind::Allow
        );
        assert_eq!(
            evaluate("/v1/models", &remote(), &state, None),
            GuardKind::Deny {
                status: 401,
                error: "API key required for remote API access"
            }
        );
    }

    #[test]
    fn a_remote_llm_request_needs_a_valid_api_key() {
        let state = Fake {
            api_key: true,
            ..Default::default()
        };
        assert!(can_access_public_llm_api(&remote(), &state, Some("sk-x")));
        assert!(!can_access_public_llm_api(&remote(), &state, None));
        // A key the store rejects is no better than no key.
        assert!(!can_access_public_llm_api(
            &remote(),
            &Fake::default(),
            Some("sk-x")
        ));
        assert_eq!(
            evaluate("/v1/models", &remote(), &state, Some("sk-x")),
            GuardKind::Allow
        );
    }

    #[test]
    fn cli_token_opens_everything_local_only() {
        let state = Fake {
            cli: true,
            ..Default::default()
        };
        let mut facts = remote();
        facts.cli_token = Some("token".into());
        assert!(can_access_local_only_route(&facts, &state));
        assert_eq!(
            evaluate("/api/auth/reset-password", &facts, &state, None),
            GuardKind::Allow
        );
        assert_eq!(
            evaluate("/api/version/update", &facts, &state, None),
            GuardKind::Allow
        );
    }

    #[test]
    fn local_only_route_needs_local_or_cli() {
        let state = Fake::default();
        assert_eq!(
            evaluate("/api/auth/reset-password", &remote(), &state, None),
            GuardKind::Deny {
                status: 403,
                error: "Local only: CLI token required"
            }
        );
        // Local but unauthenticated is still refused: localness and
        // authentication are both required.
        assert_eq!(
            evaluate("/api/auth/reset-password", &local(), &state, None),
            GuardKind::Deny {
                status: 403,
                error: "Local only: CLI token required"
            }
        );
        let state = Fake {
            authenticated: true,
            ..Default::default()
        };
        let mut facts = local();
        facts.auth_token = Some("jwt".into());
        assert_eq!(
            evaluate("/api/auth/reset-password", &facts, &state, None),
            GuardKind::Allow
        );
    }

    #[test]
    fn always_protected_needs_a_real_token_even_with_require_login_off() {
        // `requireLogin: false` makes `is_authenticated` true for everyone, but
        // these routes need a real credential, so the CLI/JWT check is the only
        // gate and a bare remote caller stays out.
        let state = Fake {
            require_login: false,
            ..Default::default()
        };
        assert_eq!(
            evaluate("/api/settings/database", &remote(), &state, None),
            GuardKind::Deny {
                status: 401,
                error: "Unauthorized"
            }
        );

        // A session cookie still opens it.
        let authed = Fake {
            require_login: false,
            authenticated: true,
            ..Default::default()
        };
        let mut facts = remote();
        facts.auth_token = Some("jwt".into());
        assert_eq!(
            evaluate("/api/settings/database", &facts, &authed, None),
            GuardKind::Allow
        );

        let strict = Fake::default();
        assert_eq!(
            evaluate("/api/settings/database", &remote(), &strict, None),
            GuardKind::Deny {
                status: 401,
                error: "Unauthorized"
            }
        );
    }

    #[test]
    fn api_routes_deny_by_default_but_allow_the_list() {
        let state = Fake::default();
        assert_eq!(
            evaluate("/api/health", &remote(), &state, None),
            GuardKind::Allow
        );
        assert_eq!(
            evaluate("/api/keys", &remote(), &state, None),
            GuardKind::Deny {
                status: 401,
                error: "Unauthorized"
            }
        );
    }

    #[test]
    fn dashboard_redirects_to_login_when_require_login() {
        let state = Fake::default();
        assert_eq!(
            evaluate("/dashboard", &remote(), &state, None),
            GuardKind::Redirect { location: "/login" }
        );
        assert_eq!(
            evaluate("/", &remote(), &state, None),
            GuardKind::Redirect {
                location: "/dashboard"
            }
        );
    }

    #[test]
    fn dashboard_is_open_when_require_login_is_off() {
        let state = Fake {
            require_login: false,
            ..Default::default()
        };
        assert_eq!(
            evaluate("/dashboard", &remote(), &state, None),
            GuardKind::Allow
        );
    }

    #[test]
    fn an_authenticated_dashboard_request_passes() {
        let state = Fake {
            authenticated: true,
            ..Default::default()
        };
        let mut facts = remote();
        facts.auth_token = Some("jwt".into());
        assert_eq!(
            evaluate("/dashboard", &facts, &state, None),
            GuardKind::Allow
        );
    }

    #[test]
    fn via_proxy_disables_localness() {
        let facts = RequestFacts {
            peer: Some("127.0.0.1".parse().unwrap()),
            x_forwarded_for: Some("203.0.113.9".into()),
            ..Default::default()
        };
        assert!(!facts.is_local());
    }

    #[test]
    fn a_non_loopback_origin_disables_localness() {
        let mut facts = local();
        facts.origin = Some("https://evil.example".into());
        assert!(!facts.is_local());
        facts.origin = Some("http://localhost:5173".into());
        assert!(facts.is_local());
        facts.origin = Some("http://[::1]:20129".into());
        assert!(facts.is_local());
        // An unparseable origin is a reject, not a pass.
        facts.origin = Some("not a url".into());
        assert!(!facts.is_local());
    }

    #[test]
    fn a_remote_peer_is_never_local() {
        assert!(!remote().is_local());
        let mut facts = remote();
        facts.origin = Some("http://localhost".into());
        assert!(
            !facts.is_local(),
            "loopback origin does not launder the peer"
        );
    }

    #[test]
    fn api_key_extraction_follows_the_reference_order() {
        assert_eq!(
            extract_api_key(
                Some("Bearer sk-a"),
                Some("sk-b"),
                Some("sk-c"),
                Some("sk-d")
            ),
            Some("sk-a")
        );
        assert_eq!(
            extract_api_key(Some("Basic zzz"), Some("sk-b"), Some("sk-c"), Some("sk-d")),
            Some("sk-b"),
            "a non-Bearer Authorization header falls through"
        );
        assert_eq!(
            extract_api_key(None, None, Some("sk-c"), Some("sk-d")),
            Some("sk-c")
        );
        assert_eq!(
            extract_api_key(None, None, None, Some("sk-d")),
            Some("sk-d")
        );
        assert_eq!(extract_api_key(None, None, None, None), None);
    }
}
