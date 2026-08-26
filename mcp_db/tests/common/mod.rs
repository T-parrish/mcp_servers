//! A throwaway database per test.
//!
//! These tests must never be able to touch a real library, so the rule is
//! structural rather than a convention to remember: this harness reads
//! `TEST_DATABASE_URL` and **never** `DATABASE_URL`, and refuses to run if the
//! two name the same database. There is no flag or CI check to forget.
//!
//! Each [`TestDb`] creates its own `mcp_test_<random>` database on that server,
//! applies the migrations to it, and drops it again in [`TestDb::cleanup`], so
//! tests cannot see each other's rows or leave anything behind.

use sqlx_core::query_scalar::query_scalar;
use sqlx_core::raw_sql::raw_sql;
use sqlx_core::sql_str::AssertSqlSafe;
use sqlx_postgres::{PgPool, PgPoolOptions};
use url::Url;

pub struct TestDb {
    /// Connected to the server's own database — the one that can create and
    /// drop others. Outlives `pool` so it can drop it at the end.
    admin: PgPool,
    /// Connected to this test's ephemeral database. What tests actually use.
    pub pool: PgPool,
    name: String,
}

impl TestDb {
    /// Build a fresh, migrated database, or `None` if none is configured — in
    /// which case the test should return early and pass.
    pub async fn new() -> Option<Self> {
        let _ = dotenv::dotenv();

        let Some(url) = std::env::var("TEST_DATABASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
        else {
            eprintln!(
                "skipping: TEST_DATABASE_URL is not set. \
                 `docker compose up -d postgres-test` starts a throwaway server; \
                 see README > Persistence > Tests."
            );
            return None;
        };
        let url = Url::parse(&url).expect("TEST_DATABASE_URL must be a valid URL");
        refuse_to_share_with_the_real_database(&url);

        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(url.as_str())
            .await
            .expect("TEST_DATABASE_URL must point at a reachable Postgres");

        // Sweep up anything a previous run left behind — a test that panics
        // never reaches `cleanup`. Databases still in use refuse to drop, which
        // is exactly the right behaviour when another run is in flight, so
        // failures here are ignored.
        let stale: Vec<String> =
            query_scalar("SELECT datname FROM pg_database WHERE datname LIKE 'mcp_test_%'")
                .fetch_all(&admin)
                .await
                .unwrap_or_default();
        for db in stale.iter().filter(|d| is_safe_identifier(d)) {
            let _ = raw_sql(AssertSqlSafe(format!(r#"DROP DATABASE IF EXISTS "{db}""#)))
                .execute(&admin)
                .await;
        }

        // Unique per run so concurrent runs cannot collide.
        let name = format!("mcp_test_{:016x}", rand_suffix());
        raw_sql(AssertSqlSafe(format!(r#"CREATE DATABASE "{name}""#)))
            .execute(&admin)
            .await
            .expect("could not create the test database");

        let mut db_url = url.clone();
        db_url.set_path(&name);
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(db_url.as_str())
            .await
            .expect("could not connect to the test database");

        migrate(&pool).await;
        Some(TestDb { admin, pool, name })
    }

    /// Drop the ephemeral database. Explicit because `Drop` cannot be async.
    pub async fn cleanup(self) {
        self.pool.close().await;
        raw_sql(AssertSqlSafe(format!(
            r#"DROP DATABASE IF EXISTS "{}""#,
            self.name
        )))
        .execute(&self.admin)
        .await
        .expect("could not drop the test database");
    }
}

/// Apply every migration, exactly the way `sqlx migrate run` does.
///
/// The crate's own embedded migrator rather than a second reading of the
/// directory: it is the same list `mcp_db::ensure_migrated` checks against, and
/// running it records `_sqlx_migrations` the way sqlx-cli would, so a test
/// database is indistinguishable from one `run.sh` migrated.
async fn migrate(pool: &PgPool) {
    mcp_db::MIGRATOR
        .run(pool)
        .await
        .unwrap_or_else(|e| panic!("migrations failed: {e}"));
}

/// Refuse to run against whatever `DATABASE_URL` points at.
///
/// The tests delete rows and drop databases. Pointing them at the server's real
/// database — by copying the wrong line into `.env`, say — would destroy a real
/// library, so this is a hard failure rather than a warning.
fn refuse_to_share_with_the_real_database(test_url: &Url) {
    let Ok(real) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(real) = Url::parse(&real) else { return };
    let same_server = real.host_str() == test_url.host_str() && real.port() == test_url.port();
    if same_server && real.path() == test_url.path() {
        panic!(
            "TEST_DATABASE_URL and DATABASE_URL name the same database ({}{}). \
             These tests drop databases and delete rows; point TEST_DATABASE_URL at the \
             throwaway `postgres-test` service instead.",
            test_url.host_str().unwrap_or("?"),
            test_url.path(),
        );
    }
}

/// Whether a database name read back from the server is safe to interpolate.
/// These are names this harness created, but they arrive as data, so check.
fn is_safe_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A random suffix, without pulling in a random-number crate for it.
fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    RandomState::new().build_hasher().finish()
}
