//! Integration tests for the `songs` table. What is being tested — the
//! normalized keys and the conflict behaviour — is the schema's job rather than
//! the caller's, so these need a real Postgres:
//!
//! ```sh
//! docker compose up -d postgres-test
//! cargo test -p mcp_db
//! ```
//!
//! Each test gets its own throwaway database and drops it afterwards; see
//! [`common`]. Without `TEST_DATABASE_URL` they skip rather than fail.

mod common;

use mcp_db::NewSong;
use sqlx_core::query_scalar::query_scalar;

const ARTIST: &str = "Test Artist";

fn song(artist: &str, title: &str, album: &str) -> NewSong {
    NewSong {
        artist: artist.to_string(),
        title: title.to_string(),
        artists: vec![artist.to_string(), "Someone Else".to_string()],
        album: Some(album.to_string()),
        album_release_date: Some("2019-03".to_string()),
        duration_ms: Some(214_000),
        spotify_id: Some("abc123".to_string()),
        isrc: Some("GBAAA1900001".to_string()),
    }
}

#[tokio::test]
async fn a_colliding_song_leaves_the_original_untouched() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    let first = mcp_db::insert_songs(&db.pool, &[song(ARTIST, "Some Song", "First Album")])
        .await
        .unwrap();
    assert_eq!(first, 1, "the first insert should be new");

    // Same song as far as the normalized key is concerned — different case,
    // padding and internal spacing — but with every other value changed.
    let again = mcp_db::insert_songs(
        &db.pool,
        &[song("  TEST   artist ", "some   song", "Second Album")],
    )
    .await
    .unwrap();
    assert_eq!(again, 0, "a collision should insert nothing");

    let albums: Vec<String> = query_scalar("SELECT album FROM songs")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        albums,
        vec!["First Album".to_string()],
        "the original row should survive unmodified, and be the only one"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn duplicates_within_one_batch_collapse() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    // The caller does not pre-deduplicate, so `DO NOTHING` has to cope with a
    // batch that repeats a key.
    let inserted = mcp_db::insert_songs(
        &db.pool,
        &[
            song(ARTIST, "Repeated", "First Album"),
            song(ARTIST, "REPEATED", "Second Album"),
            song(ARTIST, "Distinct", "Third Album"),
        ],
    )
    .await
    .unwrap();
    assert_eq!(inserted, 2, "the repeat should collapse into the first");

    db.cleanup().await;
}

#[tokio::test]
async fn the_schema_the_servers_require_is_present() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    // `mcp_db::connect` checks that every migration has *run*, not what it
    // created, so a migration that stopped creating one of these should fail
    // here rather than at the first query that needs it.
    for table in ["songs", "purchase_options"] {
        let exists: bool = query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(exists, "the migrations should create `{table}`");
    }

    db.cleanup().await;
}
