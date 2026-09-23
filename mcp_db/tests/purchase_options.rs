//! Integration tests for `purchase_options` and the song lookup that feeds it.
//! Like the `songs` tests, each gets its own throwaway database:
//!
//! ```sh
//! docker compose up -d postgres-test
//! cargo test -p mcp_db
//! ```

mod common;

use mcp_db::{NewPurchaseOption, NewSong, SongDetails};
use sqlx_core::query_scalar::query_scalar;
use sqlx_postgres::PgPool;

fn song(artist: &str, title: &str) -> NewSong {
    NewSong {
        artist: artist.to_string(),
        title: title.to_string(),
        artists: vec![artist.to_string()],
        album: None,
        album_release_date: None,
        duration_ms: None,
        spotify_id: None,
        isrc: None,
    }
}

async fn seed(pool: &PgPool, artist: &str, title: &str) -> i64 {
    mcp_db::insert_songs(pool, &[song(artist, title)])
        .await
        .unwrap();
    mcp_db::find_song_id(pool, artist, title)
        .await
        .unwrap()
        .expect("the song just inserted should be findable")
}

/// The lookup builds its own copy of the normalization the schema *generates*
/// the key columns with. If those two ever disagree, every lookup silently
/// returns `None` and nothing can be recorded — so pin it with text that only
/// matches after normalizing.
#[tokio::test]
async fn a_song_is_found_however_it_is_spelled() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let id = seed(&db.pool, "Nia Archives", "There Goes Ma Head").await;

    for (artist, title) in [
        ("Nia Archives", "There Goes Ma Head"),
        ("  nia   ARCHIVES ", "there  goes ma   head"),
        ("NIA ARCHIVES", "There Goes Ma Head "),
    ] {
        let found = mcp_db::find_song_id(&db.pool, artist, title).await.unwrap();
        assert_eq!(
            found,
            Some(id),
            "`{artist} — {title}` should find the same song"
        );
    }

    let missing = mcp_db::find_song_id(&db.pool, "Nia Archives", "A Different Song")
        .await
        .unwrap();
    assert_eq!(missing, None, "a song not in the library should not match");

    db.cleanup().await;
}

#[tokio::test]
async fn searches_accumulate_rather_than_replace() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let id = seed(&db.pool, "Naibu", "Caught Me Falling").await;

    let first = mcp_db::insert_purchase_options(
        &db.pool,
        &[
            NewPurchaseOption {
                song_id: id,
                platform: "bandcamp".into(),
                url: Some("https://naibu.bandcamp.com/track/caught-me-falling".into()),
                price: None,
                currency: None,
            },
            NewPurchaseOption {
                song_id: id,
                platform: "bandcamp".into(),
                url: Some("https://label.bandcamp.com/album/compilation".into()),
                price: None,
                currency: None,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(first, 2, "both options should be recorded");

    // Searching again appends; the table is a log, not a current-state view.
    mcp_db::insert_purchase_options(
        &db.pool,
        &[NewPurchaseOption {
            song_id: id,
            platform: "bandcamp".into(),
            url: Some("https://naibu.bandcamp.com/track/caught-me-falling".into()),
            price: None,
            currency: None,
        }],
    )
    .await
    .unwrap();

    let total: i64 = query_scalar("SELECT count(*) FROM purchase_options WHERE song_id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        total, 3,
        "a repeat search should add a row, not replace one"
    );

    db.cleanup().await;
}

/// A search that found nothing is recorded as a row with no URL — that is what
/// distinguishes "looked, found nothing" from "never looked".
#[tokio::test]
async fn a_fruitless_search_is_still_recorded() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let id = seed(&db.pool, "Some Artist", "Obscure Track").await;

    mcp_db::insert_purchase_options(
        &db.pool,
        &[NewPurchaseOption {
            song_id: id,
            platform: "bandcamp".into(),
            url: None,
            price: None,
            currency: None,
        }],
    )
    .await
    .unwrap();

    let (url, purchased, searched): (Option<String>, bool, bool) = sqlx_core::query_as::query_as(
        "SELECT url, purchased, searched_at IS NOT NULL FROM purchase_options WHERE song_id = $1",
    )
    .bind(id)
    .fetch_one(&db.pool)
    .await
    .unwrap();

    assert_eq!(url, None, "nothing was found, so there is no URL");
    assert!(!purchased, "a search does not buy anything");
    assert!(searched, "the row still records when the search happened");

    db.cleanup().await;
}

/// The foreign key is what keeps an option attached to a real song.
#[tokio::test]
async fn an_option_cannot_dangle() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };

    let orphan = mcp_db::insert_purchase_options(
        &db.pool,
        &[NewPurchaseOption {
            song_id: 999_999,
            platform: "bandcamp".into(),
            url: None,
            price: None,
            currency: None,
        }],
    )
    .await;
    assert!(orphan.is_err(), "an unknown song_id should be rejected");

    // And deleting a song takes its options with it.
    let id = seed(&db.pool, "Doomed Artist", "Doomed Song").await;
    mcp_db::insert_purchase_options(
        &db.pool,
        &[NewPurchaseOption {
            song_id: id,
            platform: "bandcamp".into(),
            url: Some("https://example.bandcamp.com/track/x".into()),
            price: None,
            currency: None,
        }],
    )
    .await
    .unwrap();

    sqlx_core::query::query("DELETE FROM songs WHERE id = $1")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();

    let left: i64 = query_scalar("SELECT count(*) FROM purchase_options WHERE song_id = $1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "ON DELETE CASCADE should remove the options too");

    db.cleanup().await;
}

/// The price is stored exactly as the page gave it, with its currency, and a
/// row without one stays NULL rather than becoming 0 — which would claim the
/// song is free.
#[tokio::test]
async fn prices_are_recorded_with_their_currency() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let id = seed(&db.pool, "Aphex Twin", "Windowlicker").await;

    mcp_db::insert_purchase_options(
        &db.pool,
        &[
            NewPurchaseOption {
                song_id: id,
                platform: "bandcamp".into(),
                url: Some("https://aphextwin.bandcamp.com/track/windowlicker".into()),
                price: Some(0.99),
                currency: Some("GBP".into()),
            },
            NewPurchaseOption {
                song_id: id,
                platform: "bandcamp".into(),
                url: Some("https://label.bandcamp.com/track/windowlicker".into()),
                price: None,
                currency: None,
            },
        ],
    )
    .await
    .unwrap();

    let rows: Vec<(Option<String>, Option<String>)> = sqlx_core::query_as::query_as(
        "SELECT price::text, currency::text FROM purchase_options WHERE song_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&db.pool)
    .await
    .unwrap();

    assert_eq!(
        rows,
        vec![
            (Some("0.99".to_string()), Some("GBP".to_string())),
            (None, None),
        ]
    );

    db.cleanup().await;
}

#[tokio::test]
async fn a_songs_details_can_be_looked_up() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let mut known = song("Trex", "Architect");
    known.isrc = Some("GBQSE2600008".into());
    known.duration_ms = Some(274_000);
    mcp_db::insert_songs(&db.pool, &[known]).await.unwrap();
    let id = mcp_db::find_song_id(&db.pool, "Trex", "Architect")
        .await
        .unwrap()
        .unwrap();
    let bare = seed(&db.pool, "Some Artist", "No Details").await;

    assert_eq!(
        mcp_db::song_details(&db.pool, id).await.unwrap(),
        SongDetails {
            isrc: Some("GBQSE2600008".into()),
            duration_ms: Some(274_000),
        }
    );
    assert_eq!(
        mcp_db::song_details(&db.pool, bare).await.unwrap(),
        SongDetails::default()
    );

    db.cleanup().await;
}

/// Only the most recent search on a platform decides whether a song was found
/// there, and a search elsewhere does not count.
#[tokio::test]
async fn found_on_reflects_the_latest_search_on_that_platform() {
    let Some(db) = common::TestDb::new().await else {
        return;
    };
    let id = seed(&db.pool, "Naibu", "Caught Me Falling").await;
    let search = |platform: &str, url: Option<&str>| NewPurchaseOption {
        song_id: id,
        platform: platform.into(),
        url: url.map(Into::into),
        price: None,
        currency: None,
    };

    assert!(
        !mcp_db::found_on(&db.pool, id, "bandcamp").await.unwrap(),
        "never searched counts as not found"
    );

    mcp_db::insert_purchase_options(
        &db.pool,
        &[search(
            "bandcamp",
            Some("https://naibu.bandcamp.com/track/x"),
        )],
    )
    .await
    .unwrap();
    assert!(mcp_db::found_on(&db.pool, id, "bandcamp").await.unwrap());
    assert!(
        !mcp_db::found_on(&db.pool, id, "beatport").await.unwrap(),
        "a find on one platform says nothing about another"
    );

    // A later search that found nothing supersedes the earlier find.
    mcp_db::insert_purchase_options(&db.pool, &[search("bandcamp", None)])
        .await
        .unwrap();
    assert!(!mcp_db::found_on(&db.pool, id, "bandcamp").await.unwrap());

    db.cleanup().await;
}
