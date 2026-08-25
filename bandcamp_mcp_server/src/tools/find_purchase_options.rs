//! Action: find where a song from the local library can be bought on Bandcamp,
//! and record what the search found.
//!
//! Takes the song's artist and title rather than a free query, because every
//! row written has to hang off a song the library already knows: the search is
//! for a *particular* song, not for whatever Bandcamp feels like returning.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::server::BandcampServer;
use crate::tools::json_result;

const PLATFORM: &str = "bandcamp";
const DEFAULT_LIMIT: usize = 10;

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

/// Whether a Bandcamp result's title is plausibly the song being looked for.
///
/// Containment either way rather than equality, because the same recording is
/// titled loosely across stores: the library's "Caught Me Falling" is Bandcamp's
/// "Caught Me Falling (ft. Joe Mandeno)", and a library title carrying a remix
/// suffix can be the longer of the two instead.
fn titles_match(wanted: &str, candidate: &str) -> bool {
    let wanted = normalize(wanted);
    let candidate = normalize(candidate);
    !wanted.is_empty()
        && !candidate.is_empty()
        && (candidate.contains(&wanted) || wanted.contains(&candidate))
}

/// Lower-case, trimmed, internal whitespace collapsed — the same shape the
/// `songs` keys use, so comparisons here behave like lookups there.
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// One place the song was found, as recorded.
#[derive(Debug, Serialize)]
struct Found {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    url: Option<String>,
}

#[tool_router(router = find_purchase_options_router, vis = "pub")]
impl BandcampServer {
    #[tool(
        description = "Search Bandcamp for a song that is already in the local library and record \
                          where it can be bought. Takes the song's artist and title — the song \
                          must have been saved first (see the Spotify server's \
                          `save_playlist_songs`). Every result is recorded as a purchase option, \
                          and a search that finds nothing is recorded too, so the same song is \
                          not looked up over and over. Prices are not recorded: Bandcamp's search \
                          does not report them."
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

        let artist_lc = params.artist.to_lowercase();
        let found: Vec<Found> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("t"))
            // The autocomplete endpoint happily returns other artists' tracks
            // for a loose query.
            .filter(|r| {
                r.band_name
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&artist_lc))
            })
            // ...and, worse for this tool, every *other* track on a release
            // whose album name matched. `search_songs` can leave those in for a
            // human to sift; here each result becomes a row asserting "this is
            // where you buy that song", so a wrong title is a wrong claim.
            .filter(|r| {
                r.name
                    .as_deref()
                    .is_some_and(|n| titles_match(&params.title, n))
            })
            .take(limit)
            .map(|r| Found {
                title: r.name.unwrap_or_default(),
                artist: r.band_name,
                album: r.album_name,
                url: r.item_url_path,
            })
            .collect();

        // A search that found nothing is still worth a row: it records that we
        // looked and when, which is what stops the same song being searched
        // forever. Hence one row with no URL rather than no row at all.
        let options: Vec<mcp_db::NewPurchaseOption> = if found.is_empty() {
            vec![mcp_db::NewPurchaseOption {
                song_id,
                platform: PLATFORM.to_string(),
                url: None,
            }]
        } else {
            found
                .iter()
                .map(|f| mcp_db::NewPurchaseOption {
                    song_id,
                    platform: PLATFORM.to_string(),
                    url: f.url.clone(),
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
        }))
    }
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
    fn rejects_empty_titles() {
        assert!(!titles_match("", "Caught Me Falling"));
        assert!(!titles_match("Caught Me Falling", ""));
    }
}
