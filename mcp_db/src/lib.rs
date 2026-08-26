//! Postgres persistence shared by the MCP servers in this workspace.
//!
//! Owns the connection pool and the writes. It deliberately does **not** own
//! the migrations: those are plain files under `mcp_db/migrations`, applied by
//! `sqlx-cli`, which `run.sh` invokes before starting a server. What this crate
//! does instead is refuse to hand back a pool for a database that has not been
//! migrated — see [`connect`] and [`ensure_migrated`].
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
use sqlx_core::migrate::Migrator;
use sqlx_core::query_as::query_as;
use sqlx_core::query_scalar::query_scalar;
use sqlx_postgres::PgPoolOptions;

pub use purchase_options::{NewPurchaseOption, find_song_id, insert_purchase_options};
pub use songs::{NewSong, insert_songs};
pub use sqlx_postgres::PgPool;

/// Every migration under `mcp_db/migrations`, embedded at compile time.
///
/// This is `sqlx::migrate!` — reached through `sqlx-macros` and the local
/// `sqlx_shim` crate, because the `sqlx` facade cannot be depended on here (see
/// the root `Cargo.toml`). Adding a migration is still just adding a file:
/// nothing below names one.
///
/// Public so the tests can apply exactly what [`ensure_migrated`] checks
/// for, rather than a second reading of the same directory.
pub static MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations");

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

    ensure_migrated(&pool).await?;

    tracing::info!("database ready");
    Ok(Some(pool))
}

/// Fail unless every migration under `mcp_db/migrations` has been applied.
///
/// Both halves of the comparison are read rather than written down: what
/// *should* have run is [`MIGRATOR`], resolved from the migration directory by
/// `sqlx::migrate!`; what *has* run comes from `_sqlx_migrations`, which is
/// `sqlx-cli`'s record. So a migration that only adds an index or alters a
/// column is caught here rather than by the first query that needs it.
///
/// Because the migrator carries the SQL, this also catches a migration that was
/// edited after it ran — a database whose schema no longer matches the files it
/// was built from, which the version numbers alone cannot show.
pub async fn ensure_migrated(pool: &PgPool) -> anyhow::Result<()> {
    // `to_regclass` rather than an error code: an absent table is the ordinary
    // state of a database sqlx-cli has never touched, not an exception.
    let recorded: bool = query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await
        .context("could not inspect the database schema")?;
    let applied: Vec<(i64, Vec<u8>, bool)> = if recorded {
        query_as("SELECT version, checksum, success FROM _sqlx_migrations")
            .fetch_all(pool)
            .await
            .context("could not read the applied migrations")?
    } else {
        Vec::new()
    };

    let mut missing = Vec::new();
    for expected in MIGRATOR.iter() {
        let version = expected.version;
        let description = &expected.description;
        match applied.iter().find(|(v, _, _)| *v == version) {
            // Applied but not finished. `sqlx migrate run` refuses to move past
            // one of these, so pointing at it would send the operator round a
            // loop that cannot end.
            Some((_, _, false)) => anyhow::bail!(
                "migration {version} ({description}) is recorded as failed — the database is \
                 dirty and has to be repaired by hand before migrations can continue"
            ),
            Some((_, checksum, true)) if checksum[..] != expected.checksum[..] => {
                anyhow::bail!(
                    "migration {version} ({description}) does not match the one this database \
                     ran — it was edited after being applied, so the schema and the file have \
                     drifted and only one of them can be right"
                )
            }
            Some(_) => {}
            None => missing.push(format!("{version} ({description})")),
        }
    }
    anyhow::ensure!(
        missing.is_empty(),
        "the database is missing {} of {} migration(s) — {} — {MIGRATE_HINT}",
        missing.len(),
        MIGRATOR.iter().count(),
        missing.join(", "),
    );

    Ok(())
}
