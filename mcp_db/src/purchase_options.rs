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
/// false).
#[derive(Debug)]
pub struct NewPurchaseOption {
    pub song_id: i64,
    /// Which store, e.g. `"bandcamp"`.
    pub platform: String,
    /// Where the song was found — `None` records a search that found nothing,
    /// which is worth keeping so the same song is not looked up forever.
    pub url: Option<String>,
    /// What the song costs there, in `currency`. `None` when the price could
    /// not be read, or the song is not sold on its own.
    pub price: Option<f64>,
    /// ISO 4217 code for `price`; set exactly when `price` is.
    pub currency: Option<String>,
}

/// Whether a store's result title is plausibly the library song's title.
///
/// Containment either way rather than equality, because the same recording is
/// titled loosely across stores: the library's "Caught Me Falling" is Bandcamp's
/// "Caught Me Falling (ft. Joe Mandeno)", and a library title carrying a remix
/// suffix can be the longer of the two instead.
///
/// But a version the candidate names — "(Boro2g Remix)", "- VIP" — must be in
/// the library's title too: containment alone would take someone's remix of a
/// song for the song itself.
pub fn titles_match(wanted: &str, candidate: &str) -> bool {
    let wanted = normalize(wanted);
    let candidate = normalize(candidate);
    !wanted.is_empty()
        && !candidate.is_empty()
        && (candidate.contains(&wanted) || wanted.contains(&candidate))
        && version_markers(&candidate)
            .iter()
            .all(|marker| wanted.contains(marker.as_str()))
}

/// Words that make a title part name a version of a song rather than the song.
const VERSION_WORDS: &[&str] = &[
    "remix",
    "mix",
    "edit",
    "vip",
    "bootleg",
    "flip",
    "rework",
    "refix",
    "dub",
    "instrumental",
    "live",
    "version",
    "remaster",
    "remastered",
];

/// The parts of a normalized title that name a version: bracketed parts, and
/// a trailing " - …" part, that contain a version word. "Original Mix" names
/// the song itself, so it is not one.
fn version_markers(title: &str) -> Vec<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = title;
    while let Some(open) = rest.find(['(', '[']) {
        let close = if rest[open..].starts_with('(') {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[open + 1..].find(close) else {
            break;
        };
        parts.push(&rest[open + 1..open + 1 + len]);
        rest = &rest[open + 1 + len + 1..];
    }
    if let Some((_, suffix)) = title.rsplit_once(" - ") {
        parts.push(suffix);
    }
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| *p != "original mix" && *p != "original")
        .filter(|p| {
            p.split(|c: char| !c.is_alphanumeric())
                .any(|word| VERSION_WORDS.contains(&word))
        })
        .map(str::to_string)
        .collect()
}

/// Lower-case, trimmed, internal whitespace collapsed — the same shape the
/// `songs` keys use, so comparisons here behave like lookups there.
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

    let mut query: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO purchase_options (song_id, platform, url, price, currency) ",
    );
    query.push_values(options, |mut row, option| {
        row.push_bind(option.song_id)
            .push_bind(&option.platform)
            .push_bind(&option.url)
            .push_bind(option.price)
            .push_bind(&option.currency);
    });

    let inserted = query.build().execute(pool).await?.rows_affected();
    tracing::Span::current().record("db.response.returned_rows", inserted);
    Ok(inserted)
}

/// What the library knows that identifies a recording, for confirming that a
/// store's result is the same one.
#[derive(Debug, Default, PartialEq)]
pub struct SongDetails {
    pub isrc: Option<String>,
    pub duration_ms: Option<i64>,
}

/// The song's identifying details; both empty for an unknown `song_id`.
pub async fn song_details(pool: &PgPool, song_id: i64) -> anyhow::Result<SongDetails> {
    let row: Option<(Option<String>, Option<i64>)> =
        sqlx_core::query_as::query_as("SELECT isrc, duration_ms FROM songs WHERE id = $1")
            .bind(song_id)
            .fetch_optional(pool)
            .await?;
    let (isrc, duration_ms) = row.unwrap_or_default();
    Ok(SongDetails { isrc, duration_ms })
}

/// Whether the most recent search for this song on `platform` found it.
///
/// One search's rows are inserted together and so share a `searched_at`; the
/// latest search is the rows with the greatest one. A song never searched there
/// counts as not found.
pub async fn found_on(pool: &PgPool, song_id: i64, platform: &str) -> anyhow::Result<bool> {
    let found: Option<bool> = query_scalar(
        "SELECT bool_or(url IS NOT NULL) FROM purchase_options \
          WHERE song_id = $1 AND platform = $2 \
            AND searched_at = (SELECT max(searched_at) FROM purchase_options \
                                WHERE song_id = $1 AND platform = $2)",
    )
    .bind(song_id)
    .bind(platform)
    .fetch_one(pool)
    .await?;
    Ok(found.unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::titles_match;

    #[test]
    fn accepts_the_same_song_titled_differently() {
        assert!(titles_match(
            "Caught Me Falling",
            "Caught Me Falling (ft. Joe Mandeno)"
        ));
        assert!(titles_match("caught  me falling", "  Caught Me Falling  "));
        // The library's title is the longer one when it carries a remix suffix.
        assert!(titles_match("Platoon - SpectraSoul Remix", "Platoon"));
    }

    #[test]
    fn rejects_other_tracks_from_the_same_release() {
        // These all came back from a real search for "Naibu Caught Me Falling",
        // matched on the album name rather than the track.
        for other in [
            "Descente",
            "Les Profondeurs",
            "The Way You Turn (Lower Effort Mix)",
        ] {
            assert!(
                !titles_match("Caught Me Falling", other),
                "`{other}` is not the song"
            );
        }
    }

    #[test]
    fn rejects_someone_elses_version_of_the_song() {
        // Real: a Bandcamp search for Dismantle's "Ghost" found this remix.
        assert!(!titles_match("Ghost", "Dismantle - Ghost (Boro2g Remix)"));
        assert!(!titles_match("Ghost", "Ghost - VIP"));
        assert!(!titles_match("Ghost", "Ghost [Extended Mix]"));
    }

    #[test]
    fn accepts_the_version_the_library_has() {
        assert!(titles_match("Licorice (Remix)", "Licorice (Remix)"));
        // Beatport keeps the version out of the name, in `mix_name`.
        assert!(titles_match(
            "Run A Red Light - Logistics Remix",
            "Run A Red Light"
        ));
        // "Original Mix" is the song itself.
        assert!(titles_match("Architect", "Architect (Original Mix)"));
    }

    #[test]
    fn rejects_empty_titles() {
        assert!(!titles_match("", "Caught Me Falling"));
        assert!(!titles_match("Caught Me Falling", ""));
    }
}
