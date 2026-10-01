//! Dashboard auth: session cookie, login lockout, and the route guard.
//!
//! SAML and OIDC are not supported, so `authMode`/`ssoType` stay as inert
//! settings fields and the login route's SSO branch is gone.

pub mod guard;
pub mod jwt;
pub mod login_limiter;
pub mod session;
