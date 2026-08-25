//! Postgres persistence shared by the MCP servers in this workspace.
//!
//! Owns the connection pool and the writes. It deliberately does **not** own
//! the migrations: those are plain files under `mcp_db/migrations`, applied by
//! `sqlx-cli`, which `run.sh` invokes before starting a server. What this crate
//! does instead is refuse to hand back a pool for a database that has not been
//! migrated — see [`connect`].
//!
//! A server opens the pool in `main` and passes it down:
//!
//! ```ignore
//! let db = mcp_db::connect().await?;
//! let service = SpotifyServer::new(db).serve(stdio()).await?;
//! ```

pub mod purchase_options;
pub mod songs;

use std::time::Duration;

use anyhow::Context;
use sqlx_core::query_scalar::query_scalar;
use sqlx_postgres::PgPoolOptions;

pub use purchase_options::{NewPurchaseOption, find_song_id, insert_purchase_options};
pub use songs::{NewSong, insert_songs};
pub use sqlx_postgres::PgPool;

/// The tables the servers write to. Their presence is what "migrated" means
/// here; the authority on which migrations have run is `_sqlx_migrations`, but
/// that table is `sqlx-cli`'s business, not this crate's.
const REQUIRED_TABLES: [&str; 2] = ["songs", "purchase_options"];

/// How to fix a database that is missing the schema.
const MIGRATE_HINT: &str = "run `sqlx migrate run --source mcp_db/migrations` \
                            (or start the server through ./run.sh, which does it for you)";

/// Open the pool named by `DATABASE_URL` and verify the schema is present.
///
/// `Ok(None)` means `DATABASE_URL` is unset — persistence is switched off, and
/// the caller is expected to run without it. Anything else about a *configured*
/// database being wrong (unreachable, unmigrated) is an error rather than a
/// silent `None`: opting out is a choice, but a broken opt-in is a
/// misconfiguration the operator should hear about at startup, not later.
pub async fn connect() -> anyhow::Result<Option<PgPool>> {
    let Some(url) = std::env::var("DATABASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(None);
    };

    // Eager, not lazy: connecting now is what makes an unreachable database a
    // startup failure instead of a surprise on the first tool call. The default
    // acquire timeout is 30s, which is a long time to look hung at startup for
    // what is nearly always a server that simply is not running.
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .context(
            "could not connect to the database at DATABASE_URL — is it running? \
             `docker compose up -d postgres` starts the local one",
        )?;

    for table in REQUIRED_TABLES {
        let exists: bool = query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(&pool)
            .await
            .context("could not inspect the database schema")?;
        anyhow::ensure!(
            exists,
            "the database is missing the `{table}` table — {MIGRATE_HINT}"
        );
    }

    tracing::info!("database ready");
    Ok(Some(pool))
}
