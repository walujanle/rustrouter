//! The HTTP surface: the OpenAI-compatible `/v1*` routes, the dashboard `/api/*`
//! routes, auth middleware, OAuth loopback listeners, CLI-tool config writers and
//! the embedded frontend.
//!
//! See `docs/PLAN.md` for the route keep/drop decision and `docs/RUNTIME.md` for
//! the process-level behaviour this crate has to reproduce.
//!
//! The `/v1*` handlers are not here yet. What is here is the whole auth path:
//! session cookie, login lockout, the route guard, and the public dashboard
//! routes. Everything else mounts onto the router in `app` as it lands.

pub mod app;
pub mod auth;
pub mod error;
pub mod middleware;
pub mod reclaim;
pub mod routes;
pub mod services;
pub mod state;
pub mod static_assets;

pub use app::{router, serve};
pub use error::ApiError;
pub use state::{APP_VERSION, AppState, resolve_host, resolve_port};
