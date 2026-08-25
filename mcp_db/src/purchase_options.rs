//! Looking up songs, and recording where they can be bought.

use sqlx_core::query_builder::QueryBuilder;
use sqlx_core::query_scalar::query_scalar;
use sqlx_postgres::{PgPool, Postgres};

/// Find a song by the normalized keys the schema generates.
///
/// The `lower(regexp_replace(btrim(...)))` here has to reproduce what
/// `mcp_db/migrations/0001_songs.sql` generates `artist_key` and `title_key`
/// with, or nothing will ever match. That duplication is the price of letting
/// the database own the normalization; `a_song_is_found_however_it_is_spelled`
/// in `tests/purchase_options.rs` fails if the two ever drift apart.
const FIND_SONG: &str = "\
    SELECT id FROM songs \
     WHERE artist_key = lower(regexp_replace(btrim($1), '\\s+', ' ', 'g')) \
       AND title_key  = lower(regexp_replace(btrim($2), '\\s+', ' ', 'g'))";

/// A place a song can be bought, as one search found it.
///
/// `searched_at` and `purchased` are left to the column defaults (now, and
/// false). `price`/`currency` are not here yet: the searches that produce these
/// rows do not report a price, and a column that is always NULL is better than
/// a field that is always `None`.
#[derive(Debug)]
pub struct NewPurchaseOption {
    pub song_id: i64,
    /// Which store, e.g. `"bandcamp"`.
    pub platform: String,
    /// Where the song was found — `None` records a search that found nothing,
    /// which is worth keeping so the same song is not looked up forever.
    pub url: Option<String>,
}

/// The `id` of the song with this artist and title, if the library has it.
///
/// Matches on the normalized keys, so the caller can pass the text however it
/// has it — Bandcamp's spacing and capitalization need not agree with Spotify's.
pub async fn find_song_id(pool: &PgPool, artist: &str, title: &str) -> anyhow::Result<Option<i64>> {
    let id = query_scalar(FIND_SONG)
        .bind(artist)
        .bind(title)
        .fetch_optional(pool)
        .await?;
    Ok(id)
}

/// Append purchase options. Every call adds rows: the table is a log of
/// searches, so nothing here updates or replaces what an earlier search found.
#[tracing::instrument(
    name = "INSERT purchase_options",
    skip_all,
    fields(
        otel.kind = "client",
        db.system.name = "postgresql",
        db.operation.name = "INSERT",
        db.collection.name = "purchase_options",
        db.response.returned_rows = tracing::field::Empty,
    ),
    err,
)]
pub async fn insert_purchase_options(
    pool: &PgPool,
    options: &[NewPurchaseOption],
) -> anyhow::Result<u64> {
    if options.is_empty() {
        return Ok(0);
    }

    let mut query: QueryBuilder<Postgres> =
        QueryBuilder::new("INSERT INTO purchase_options (song_id, platform, url) ");
    query.push_values(options, |mut row, option| {
        row.push_bind(option.song_id)
            .push_bind(&option.platform)
            .push_bind(&option.url);
    });

    let inserted = query.build().execute(pool).await?.rows_affected();
    tracing::Span::current().record("db.response.returned_rows", inserted);
    Ok(inserted)
}
