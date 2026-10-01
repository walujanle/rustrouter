//! SQLite persistence: schema, migrations and repositories.
//!
//! This crate is the byte-parity boundary with the shared 9router database.
//! See `docs/DB-PARITY.md` before changing anything here.
//!
//! Two rules drive the whole crate:
//!
//! - **JSON columns are byte-compared.** `serde_json` runs with
//!   `preserve_order`, values are built as `Value` (never a struct with
//!   `skip_serializing_if`), and `undefined` is written as `null` with the key
//!   kept — `json_col` exists to keep that honest.
//! - **Timestamps are `Date.toISOString()`** for `usageHistory.timestamp` and
//!   local time for `usageDaily.dateKey`; `time` owns both.
//!
//! The crate holds no registry knowledge. Pricing cost calculation and the
//! built-in pricing catalog are injected by the caller, so `router-sse` can
//! depend on this crate without a cycle.

pub mod backup;
pub mod driver;
pub mod error;
pub mod export;
pub mod identity;
pub mod json_col;
pub mod kv_store;
pub mod meta_store;
pub mod migrations;
pub mod paths;
pub mod repos;
pub mod schema;
pub mod stats;
pub mod time;

pub use driver::Db;
pub use error::{DbError, DbResult};
pub use paths::Paths;

// Re-exported so a downstream crate can name `Connection` and `Transaction` in
// its own signatures without pinning a second, possibly different rusqlite.
pub use rusqlite;
