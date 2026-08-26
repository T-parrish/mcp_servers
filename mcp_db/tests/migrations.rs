//! Integration tests for the startup schema check.
//!
//! What is being tested is `mcp_db::ensure_migrated`'s reading of
//! `_sqlx_migrations`, so these need a real, really-migrated Postgres:
//!
//! ```sh
//! docker compose up -d postgres-test
//! cargo test -p mcp_db
//! ```
//!
//! Each test gets its own throwaway database and drops it afterwards; see
//! [`common`]. Without `TEST_DATABASE_URL` they skip rather than fail.

mod common;

use sqlx_core::raw_sql::raw_sql;
use sqlx_core::sql_str::AssertSqlSafe;

#[tokio::test]
async fn a_fully_migrated_database_is_accepted() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    mcp_db::ensure_migrated(&db.pool)
        .await
        .expect("a freshly migrated database should be accepted");

    db.cleanup().await;
}

/// The case the table-name check could not see: the newest migration never ran,
/// but everything the older ones created is still there. Written against
/// whichever migration is newest rather than against `0001`, so it keeps
/// testing the same thing once there is an `0002` — including an `0002` that
/// only adds an index and so leaves no missing table to notice.
#[tokio::test]
async fn an_unapplied_migration_is_rejected() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    raw_sql(AssertSqlSafe(
        "DELETE FROM _sqlx_migrations WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    ))
    .execute(&db.pool)
    .await
    .expect("could not un-apply the newest migration");

    let err = mcp_db::ensure_migrated(&db.pool)
        .await
        .expect_err("a database missing a migration should be rejected")
        .to_string();
    assert!(
        err.contains("sqlx migrate run"),
        "the error should say how to fix it, got: {err}"
    );

    db.cleanup().await;
}

/// A database sqlx-cli has never touched: no `_sqlx_migrations` at all, which
/// is a different code path from a table that exists and is short a row.
#[tokio::test]
async fn a_database_that_was_never_migrated_is_rejected() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    raw_sql(AssertSqlSafe("DROP TABLE _sqlx_migrations"))
        .execute(&db.pool)
        .await
        .expect("could not drop the migrations table");

    let err = mcp_db::ensure_migrated(&db.pool)
        .await
        .expect_err("an unmigrated database should be rejected")
        .to_string();
    assert!(
        err.contains("sqlx migrate run"),
        "the error should say how to fix it, got: {err}"
    );

    db.cleanup().await;
}

/// A migration that started and did not finish. `sqlx migrate run` will not
/// move past one of these, so the check has to say something other than "run
/// the migrations".
#[tokio::test]
async fn a_dirty_migration_is_rejected_with_its_own_message() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    raw_sql(AssertSqlSafe(
        "UPDATE _sqlx_migrations SET success = false \
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    ))
    .execute(&db.pool)
    .await
    .expect("could not mark the newest migration dirty");

    let err = mcp_db::ensure_migrated(&db.pool)
        .await
        .expect_err("a dirty database should be rejected")
        .to_string();
    assert!(
        err.contains("dirty"),
        "the error should name the dirty state, got: {err}"
    );

    db.cleanup().await;
}

/// A migration edited after it ran: the versions still line up, but the schema
/// in the database is no longer the one the files describe. Only visible
/// because the migrator carries the SQL it was built from.
#[tokio::test]
async fn a_migration_edited_after_it_ran_is_rejected() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    raw_sql(AssertSqlSafe(
        "UPDATE _sqlx_migrations SET checksum = '\\x00'::bytea \
         WHERE version = (SELECT max(version) FROM _sqlx_migrations)",
    ))
    .execute(&db.pool)
    .await
    .expect("could not rewrite the recorded checksum");

    let err = mcp_db::ensure_migrated(&db.pool)
        .await
        .expect_err("a drifted migration should be rejected")
        .to_string();
    assert!(
        err.contains("edited after being applied"),
        "the error should name the drift, got: {err}"
    );

    db.cleanup().await;
}
