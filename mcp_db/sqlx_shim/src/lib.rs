//! The `sqlx` crate, as far as `sqlx_macros::migrate!` is concerned.
//!
//! The macro expands to a `Migrator` literal written in absolute paths —
//! `::sqlx::migrate::{Migrator, Migration, MigrationType}` and
//! `::sqlx::SqlStr` — because it is meant to be re-exported by the `sqlx`
//! facade crate. This workspace cannot depend on that crate: it declares an
//! optional SQLite backend, and Cargo resolves optional dependencies whether or
//! not their feature is enabled, so `sqlx-sqlite`'s `libsqlite3-sys` collides
//! with the one `rookie` pulls in for the bandcamp server over `links =
//! "sqlite3"`. The macro crate itself has no such problem.
//!
//! So this crate supplies those four paths, out of `sqlx-core`, and `mcp_db`
//! depends on it under the name the macro expects:
//!
//! ```toml
//! sqlx = { package = "mcp_sqlx_shim", path = "sqlx_shim" }
//! ```
//!
//! Nothing else should depend on it. If a future sqlx release expands to a path
//! that is not re-exported here, the failure is a "cannot find X in sqlx" at
//! the `migrate!` call site, and the fix is to add the path below.

pub use sqlx_core::migrate;
pub use sqlx_core::sql_str::SqlStr;
