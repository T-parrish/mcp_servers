//! Action: find where a song from the local library can be bought on Bandcamp,
//! and record what the search found.
//!
//! Takes the song's artist and title rather than a free query, because every
//! row written has to hang off a song the library already knows: the search is
//! for a *particular* song, not for whatever Bandcamp feels like returning.
//!
//! Search matches text, and text is ambiguous — namesake artists, other songs
//! whose title contains this one, someone's remix of it. So each candidate is
//! confirmed against its own page before it is recorded: by ISRC when both
//! sides have one, else by duration.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::bandcamp::Recording;
use crate::server::BandcampServer;
use crate::tools::json_result;

const PLATFORM: &str = "bandcamp";
const DEFAULT_LIMIT: usize = 10;
/// How far a candidate's duration may be from the library's and still be the
/// same recording. The same recording differs by at most a second across
/// stores; another version or song is usually off by far more.
const DURATION_TOLERANCE_SECS: i64 = 3;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindPurchaseOptionsParams {
    /// The song's artist, as the library has it. Matched loosely enough that
    /// case and spacing need not agree exactly.
    pub artist: String,
    /// The song's title, as the library has it.
    pub title: String,
    /// Maximum number of Bandcamp results to record (default 10).
    pub limit: Option<usize>,
}

/// One place the song was found, as recorded.
#[derive(Debug, Serialize)]
struct Found {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    url: String,
    /// The track's minimum price on its page, in `currency`. `None` when the
    /// track is not sold on its own.
    price: Option<f64>,
    currency: Option<String>,
    /// How it was confirmed to be the library's recording.
    confirmed_by: Confirmation,
    /// With `url`, what `add_to_cart` needs to buy this track.
    track_id: u64,
}

/// A candidate that looked right in search but was not the song.
#[derive(Debug, Serialize)]
struct Rejected {
    title: String,
    artist: Option<String>,
    url: String,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Confirmation {
    Isrc,
    Duration,
}

/// Whether a candidate's page shows it is the library's recording.
///
/// An ISRC on both sides decides it outright. Otherwise the durations must
/// agree. With neither to go on, the candidate is refused: recording it would
/// be a claim nothing supports.
fn confirm(song: &mcp_db::SongDetails, page: &Recording) -> Result<Confirmation, String> {
    if let (Some(want), Some(have)) = (&song.isrc, &page.isrc) {
        return match want.eq_ignore_ascii_case(have) {
            true => Ok(Confirmation::Isrc),
            false => Err(format!("a different recording (ISRC {have}, not {want})")),
        };
    }
    match (song.duration_ms, page.duration_secs) {
        (Some(want_ms), Some(have)) => {
            let off = i64::from(have) - (want_ms + 500) / 1000;
            match off.abs() <= DURATION_TOLERANCE_SECS {
                true => Ok(Confirmation::Duration),
                false => Err(format!("a different length ({off:+} s)")),
            }
        }
        _ => Err("nothing on its page to confirm it is the same recording".to_string()),
    }
}

/// Whether a Bandcamp artist credit ("Halogenix & JD. REID", "Sam Binga,
/// Machinedrum & Cesco") names the library's artist as one of its artists.
/// Whole names, not substrings: "We Must Dismantle All this" is not Dismantle.
/// Compared as lists, so a duo credited with "&" still matches itself.
fn credits_artist(credit: &str, artist: &str) -> bool {
    let credit = credit_names(credit);
    let artist = credit_names(artist);
    !artist.is_empty() && credit.windows(artist.len()).any(|w| w == artist.as_slice())
}

/// The names in an artist credit, lower-cased, in order.
fn credit_names(credit: &str) -> Vec<String> {
    let credit = format!(" {} ", credit.to_lowercase());
    let mut names = vec![credit.as_str()];
    for separator in [",", " & ", " x ", " ft. ", " feat. ", " featuring "] {
        names = names.iter().flat_map(|n| n.split(separator)).collect();
    }
    names
        .into_iter()
        .map(|n| n.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|n| !n.is_empty())
        .collect()
}

#[tool_router(router = find_purchase_options_router, vis = "pub")]
impl BandcampServer {
    #[tool(
        description = "Search Bandcamp for a song that is already in the local library and record \
                          where it can be bought. Takes the song's artist and title — the song \
                          must have been saved first (see the Spotify server's \
                          `save_playlist_songs`). Every result is recorded as a purchase option, \
                          and a search that finds nothing is recorded too, so the same song is \
                          not looked up over and over. Each result is confirmed against its page \
                          to be the same recording — by ISRC when both have one, else by \
                          duration — and search hits that are not (a namesake artist, another \
                          song, someone's remix) are left out and listed in `rejected` with the \
                          reason. Each result's price (the track's minimum, in the artist's \
                          currency) is read from the same page; `price` is null when the track is \
                          not sold on its own."
    )]
    #[tracing::instrument(
        name = "tools/call find_purchase_options",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "find_purchase_options",
            db.song.id = tracing::field::Empty,
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn find_purchase_options(
        &self,
        Parameters(params): Parameters<FindPurchaseOptionsParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.find_purchase_options_inner(params).await;
        mcp_core::metrics::record_tool_call(
            "find_purchase_options",
            crate::tools::outcome(&result),
        );
        result
    }

    async fn find_purchase_options_inner(
        &self,
        params: FindPurchaseOptionsParams,
    ) -> Result<CallToolResult, McpError> {
        let db = self.db().ok_or_else(|| {
            McpError::internal_error(
                "this server has no database configured, so it cannot record purchase options. \
                 Set DATABASE_URL in .env (`docker compose up -d postgres` starts a local one) \
                 and restart the server."
                    .to_string(),
                None,
            )
        })?;

        // The song has to exist first: a purchase option is always *for* a song,
        // and the library is what says which songs are worth shopping for.
        let song_id = mcp_db::find_song_id(db, &params.artist, &params.title)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("looking up the song failed: {e:#}"), None)
            })?
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "`{} — {}` is not in the library, so there is nothing to record options \
                         against. Save the playlist it comes from first with the Spotify \
                         server's `save_playlist_songs`.",
                        params.artist, params.title
                    ),
                    None,
                )
            })?;
        tracing::Span::current().record("db.song.id", song_id);

        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let search_text = format!("{} {}", params.artist, params.title);
        // "t" = tracks.
        let results = self
            .client()
            .autocomplete(&search_text, "t")
            .await
            .map_err(|e| McpError::internal_error(format!("song search failed: {e}"), None))?;

        let candidates: Vec<_> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("t"))
            // The autocomplete endpoint happily returns other artists' tracks
            // for a loose query.
            .filter(|r| {
                r.band_name
                    .as_deref()
                    .is_some_and(|b| credits_artist(b, &params.artist))
            })
            // ...and, worse for this tool, every *other* track on a release
            // whose album name matched. `search_songs` can leave those in for a
            // human to sift; here each result becomes a row asserting "this is
            // where you buy that song", so a wrong title is a wrong claim.
            .filter(|r| {
                r.name
                    .as_deref()
                    .is_some_and(|n| mcp_db::titles_match(&params.title, n))
            })
            .collect();

        // Each candidate's page gives its price and what identifies its
        // recording. A page that cannot be read fails the call and records
        // nothing: the candidate could be neither confirmed nor ruled out, and
        // leaving it out could turn "found" into a recorded "found nothing".
        let song = mcp_db::song_details(db, song_id).await.map_err(|e| {
            McpError::internal_error(format!("looking up the song failed: {e:#}"), None)
        })?;
        let mut found: Vec<Found> = Vec::new();
        let mut rejected: Vec<Rejected> = Vec::new();
        for r in candidates {
            if found.len() == limit {
                break;
            }
            let (Some(url), Some(track_id)) =
                (r.item_url_path, r.id.and_then(|id| u64::try_from(id).ok()))
            else {
                continue;
            };
            let page = self
                .client()
                .track_page(&url, track_id)
                .await
                .map_err(|e| {
                    McpError::internal_error(format!("reading {url} failed: {e:#}"), None)
                })?;
            match confirm(&song, &page.recording) {
                Ok(confirmed_by) => found.push(Found {
                    title: r.name.unwrap_or_default(),
                    artist: r.band_name,
                    album: r.album_name,
                    url,
                    price: page.price.as_ref().map(|p| p.amount),
                    currency: page.price.map(|p| p.currency),
                    confirmed_by,
                    track_id,
                }),
                Err(reason) => rejected.push(Rejected {
                    title: r.name.unwrap_or_default(),
                    artist: r.band_name,
                    url,
                    reason,
                }),
            }
        }

        // A search that found nothing is still worth a row: it records that we
        // looked and when, which is what stops the same song being searched
        // forever. Hence one row with no URL rather than no row at all.
        let options: Vec<mcp_db::NewPurchaseOption> = if found.is_empty() {
            vec![mcp_db::NewPurchaseOption {
                song_id,
                platform: PLATFORM.to_string(),
                url: None,
                price: None,
                currency: None,
            }]
        } else {
            found
                .iter()
                .map(|f| mcp_db::NewPurchaseOption {
                    song_id,
                    platform: PLATFORM.to_string(),
                    url: Some(f.url.clone()),
                    price: f.price,
                    currency: f.currency.clone(),
                })
                .collect()
        };

        let recorded = mcp_db::insert_purchase_options(db, &options)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    format!("recording the purchase options failed: {e:#}"),
                    None,
                )
            })?;

        tracing::Span::current().record("result.count", found.len());
        tracing::info!(
            song_id,
            found = found.len(),
            recorded,
            "purchase options recorded"
        );

        json_result(&json!({
            "song_id": song_id,
            "artist": params.artist,
            "title": params.title,
            "found": found.len(),
            "rows_recorded": recorded,
            "options": found,
            "rejected": rejected,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{Confirmation, confirm, credits_artist};
    use crate::bandcamp::Recording;
    use mcp_db::SongDetails;

    fn song(isrc: Option<&str>, duration_ms: Option<i64>) -> SongDetails {
        SongDetails {
            isrc: isrc.map(Into::into),
            duration_ms,
        }
    }

    fn page(isrc: Option<&str>, duration_secs: Option<u32>) -> Recording {
        Recording {
            isrc: isrc.map(Into::into),
            duration_secs,
        }
    }

    #[test]
    fn an_isrc_on_both_sides_decides() {
        let monty = song(Some("GBKQU2688497"), Some(273_810));
        assert_eq!(
            confirm(&monty, &page(Some("GBKQU2688497"), Some(273))),
            Ok(Confirmation::Isrc)
        );
        // Even at the right length, another ISRC is another recording.
        assert!(confirm(&monty, &page(Some("GBXXX0000001"), Some(274))).is_err());
    }

    #[test]
    fn otherwise_the_duration_must_agree() {
        // Real: Spotify has Monty's "Questions" at 273.8 s.
        let monty = song(Some("GBKQU2688497"), Some(273_810));
        assert_eq!(
            confirm(&monty, &page(None, Some(273))),
            Ok(Confirmation::Duration)
        );
        // "21 Questions" by a namesake Monty, and a compilation's "Questions".
        assert!(confirm(&monty, &page(None, Some(99))).is_err());
        assert!(confirm(&monty, &page(None, Some(323))).is_err());
    }

    #[test]
    fn with_nothing_to_compare_it_is_refused() {
        assert!(confirm(&song(Some("GB1"), Some(262_000)), &page(None, None)).is_err());
        assert!(confirm(&song(None, None), &page(Some("GB1"), Some(262))).is_err());
    }

    #[test]
    fn credits_match_whole_artist_names() {
        assert!(credits_artist("Halogenix & JD. REID", "Halogenix"));
        assert!(credits_artist(
            "Sam Binga, Machinedrum & Cesco",
            "Sam Binga"
        ));
        assert!(credits_artist("CASPA", "Caspa"));
        assert!(credits_artist("Napes ft. Scratchy", "Napes"));
        // A duo credited with "&" is still itself.
        assert!(credits_artist("Chase & Status, Bou", "Chase & Status"));
        // Real: a Bandcamp search for Dismantle found this band.
        assert!(!credits_artist("We Must Dismantle All this", "Dismantle"));
        assert!(!credits_artist("Forget Montyy", "Monty"));
    }
}
