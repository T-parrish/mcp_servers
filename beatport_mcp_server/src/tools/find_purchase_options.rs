//! Action: find where a song from the local library can be bought on Beatport,
//! and record what the search found.
//!
//! Matches by ISRC first — the library has it from Spotify, and Beatport
//! indexes it, so a hit is the exact recording. Only when that finds nothing
//! (no ISRC, or Beatport sells a different version under another one) does it
//! fall back to a text search on artist and title.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::beatport::{ApiError, RawPrice, RawTrack};
use crate::server::BeatportServer;
use crate::tools::json_result;

const PLATFORM: &str = "beatport";
/// The format prices are recorded for. Beatport has one page per track, not
/// per format: the format comes from the account's download preference.
const FORMAT: &str = "aiff";
const DEFAULT_LIMIT: usize = 10;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindPurchaseOptionsParams {
    /// The song's artist, as the library has it. Matched loosely enough that
    /// case and spacing need not agree exactly.
    pub artist: String,
    /// The song's title, as the library has it.
    pub title: String,
    /// Maximum number of Beatport results to record (default 10).
    pub limit: Option<usize>,
    /// Skip the search if the most recent search on this other platform (e.g.
    /// "bandcamp") found the song. Makes Beatport a fallback for it.
    pub only_if_not_on: Option<String>,
}

/// How a result was matched to the library song.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
enum MatchedBy {
    /// Same ISRC: the exact recording.
    Isrc,
    /// Artist and title text: the same song, possibly another version of it.
    Text,
}

/// One place the song was found, as recorded.
#[derive(Debug, Serialize)]
struct Found {
    title: String,
    mix_name: Option<String>,
    artists: Vec<String>,
    url: String,
    /// The AIFF price: `base_price` plus the account's AIFF surcharge. `None`
    /// when the track is listed but not for sale.
    price: Option<f64>,
    /// The listed (MP3) price.
    base_price: Option<f64>,
    currency: Option<String>,
    format: &'static str,
    isrc: Option<String>,
    release_date: Option<String>,
    matched_by: MatchedBy,
    track_id: u64,
}

impl Found {
    fn new(track: RawTrack, matched_by: MatchedBy, aiff_fee: &RawPrice) -> Self {
        let base = track.purchase_price();
        Self {
            price: base.and_then(|b| aiff_price(b, aiff_fee)),
            base_price: base.map(|b| b.value),
            currency: base.map(|b| b.code.clone()),
            format: FORMAT,
            url: track.web_url(),
            title: track.name,
            mix_name: track.mix_name,
            artists: track.artists.into_iter().map(|a| a.name).collect(),
            isrc: track.isrc,
            release_date: track.new_release_date,
            matched_by,
            track_id: track.id,
        }
    }
}

/// The AIFF price of a track listed at `base`, rounded to the cent. `None` if
/// the fee is in another currency: adding them would be meaningless.
fn aiff_price(base: &RawPrice, fee: &RawPrice) -> Option<f64> {
    (base.code == fee.code).then(|| ((base.value + fee.value) * 100.0).round() / 100.0)
}

/// Keep only the most recent release of each recording.
///
/// The same recording is often sold on several releases — the original, then
/// compilations — each a separate listing with its own page. A recording is
/// the ISRC *and* the mix name: labels do not always give each version its own
/// ISRC (Chris Lake's "Long Jacket" Extended Mix and Radio Edit share one), so
/// the ISRC alone would merge versions. A listing without an ISRC is kept as it
/// is, since nothing says what it duplicates. Order is otherwise preserved.
fn latest_release_per_recording(tracks: Vec<RawTrack>) -> Vec<RawTrack> {
    let mut latest: std::collections::HashMap<(String, String), &RawTrack> =
        std::collections::HashMap::new();
    for track in &tracks {
        if let Some(isrc) = &track.isrc {
            let mix = track.mix_name.as_deref().unwrap_or_default().to_lowercase();
            let entry = latest.entry((isrc.clone(), mix)).or_insert(track);
            // ISO dates compare correctly as text; a missing one sorts first.
            if track.new_release_date > entry.new_release_date {
                *entry = track;
            }
        }
    }
    let keep: std::collections::HashSet<u64> = latest.values().map(|t| t.id).collect();
    tracks
        .into_iter()
        .filter(|t| t.isrc.is_none() || keep.contains(&t.id))
        .collect()
}

/// Whether a text-search result is plausibly the song: one of its artists is
/// the library's artist — the whole name, not a substring of another — and its
/// title matches.
fn text_match(artist: &str, title: &str, track: &RawTrack) -> bool {
    let artist = artist.trim().to_lowercase();
    track
        .artists
        .iter()
        .any(|a| a.name.trim().to_lowercase() == artist)
        && mcp_db::titles_match(title, &track.name)
}

/// An `auth_required` tool result telling the assistant to (re-)authenticate.
fn auth_required(reason: &str) -> Result<CallToolResult, McpError> {
    json_result(&json!({
        "mode": "auth_required",
        "message": format!(
            "Not authenticated with Beatport ({reason}). Call the `authenticate` tool with a \
             fresh token, then retry. Nothing was recorded."
        ),
    }))
}

#[tool_router(router = find_purchase_options_router, vis = "pub")]
impl BeatportServer {
    #[tool(
        description = "Search Beatport for a song that is already in the local library and record \
                          where it can be bought and for how much. Takes the song's artist and \
                          title — the song must have been saved first (see the Spotify server's \
                          `save_playlist_songs`). Matches by the song's ISRC first (`matched_by: \
                          isrc`, the exact recording), falling back to artist and title (`text`, \
                          possibly another version — check `mix_name`). Prices are for AIFF: the \
                          listed price plus the account's AIFF surcharge (`base_price` is the \
                          listed one); the link is the track's page, where the format follows the \
                          account's download preference. Every result is recorded, \
                          and a search that finds nothing is recorded too. Pass \
                          `only_if_not_on: \"bandcamp\"` to search only songs Bandcamp's latest \
                          search did not find."
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
        let db_error =
            |e: anyhow::Error| McpError::internal_error(format!("database error: {e:#}"), None);

        let song_id = mcp_db::find_song_id(db, &params.artist, &params.title)
            .await
            .map_err(db_error)?
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

        if let Some(other) = params.only_if_not_on.as_deref()
            && mcp_db::found_on(db, song_id, other)
                .await
                .map_err(db_error)?
        {
            return json_result(&json!({
                "song_id": song_id,
                "artist": params.artist,
                "title": params.title,
                "skipped": true,
                "message": format!(
                    "Not searched: the latest search on {other} found this song. Nothing was \
                     recorded."
                ),
            }));
        }

        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let isrc = mcp_db::song_details(db, song_id)
            .await
            .map_err(db_error)?
            .isrc;

        let matches = async {
            if let Some(isrc) = isrc.as_deref() {
                let tracks = self.client().tracks_by_isrc(isrc).await?;
                if !tracks.is_empty() {
                    return Ok::<_, ApiError>((tracks, MatchedBy::Isrc));
                }
            }
            let query = format!("{} {}", params.artist, params.title);
            let tracks = self
                .client()
                .search_tracks(&query)
                .await?
                .into_iter()
                // Search ranks loosely: other artists, and other tracks from a
                // release whose name matched. Each kept result becomes a row
                // claiming "this is where you buy that song".
                .filter(|t| text_match(&params.artist, &params.title, t))
                .collect();
            Ok((tracks, MatchedBy::Text))
        };
        let priced = async {
            let (tracks, matched_by) = matches.await?;
            if tracks.is_empty() {
                return Ok(Vec::new());
            }
            // Only needed when there is something to price.
            let fee = self.client().aiff_fee().await?;
            Ok::<_, ApiError>(
                latest_release_per_recording(tracks)
                    .into_iter()
                    .take(limit)
                    .map(|t| Found::new(t, matched_by, &fee))
                    .collect(),
            )
        };
        let found: Vec<Found> = match priced.await {
            Ok(found) => found,
            // Nothing is recorded: a search that could not run is not a
            // search that found nothing.
            Err(ApiError::AuthRequired(reason)) => return auth_required(&reason),
            Err(ApiError::Other(e)) => {
                return Err(McpError::internal_error(
                    format!("beatport search failed: {e:#}"),
                    None,
                ));
            }
        };

        // As on Bandcamp, a search that found nothing is still a row.
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
            "isrc": isrc,
            "found": found.len(),
            "rows_recorded": recorded,
            "options": found,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{aiff_price, latest_release_per_recording, text_match};
    use crate::beatport::{RawPrice, RawTrack};

    fn usd(value: f64) -> RawPrice {
        RawPrice {
            value,
            code: "USD".into(),
        }
    }

    #[test]
    fn aiff_costs_the_base_price_plus_the_fee() {
        // Real pairs from the account's purchases: the fee is flat.
        assert_eq!(aiff_price(&usd(1.49), &usd(0.75)), Some(2.24));
        assert_eq!(aiff_price(&usd(2.49), &usd(0.75)), Some(3.24));
    }

    #[test]
    fn a_fee_in_another_currency_gives_no_price() {
        let eur = RawPrice {
            value: 0.75,
            code: "EUR".into(),
        };
        assert_eq!(aiff_price(&usd(1.49), &eur), None);
    }

    fn track(name: &str, artists: &[&str]) -> RawTrack {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": name,
            "mix_name": "Original Mix",
            "slug": "x",
            "isrc": null,
            "artists": artists.iter().map(|a| serde_json::json!({"name": a})).collect::<Vec<_>>(),
            "price": {"value": 1.69, "code": "USD"},
            "sale_type": {"name": "purchase"},
        }))
        .unwrap()
    }

    #[test]
    fn matches_a_featured_lead_artist() {
        // Spotify's lead artist is one of several on Beatport.
        assert!(text_match(
            "Halogenix",
            "Nakal (ft. Eka)",
            &track("Nakal (ft. Eka)", &["Halogenix", "JD. REID"])
        ));
    }

    #[test]
    fn matches_a_remix_titled_apart_from_its_mix_name() {
        // Spotify folds the mix into the title; Beatport keeps it in `mix_name`.
        assert!(text_match(
            "Everything But The Girl",
            "Run A Red Light - Logistics Remix",
            &track("Run A Red Light", &["Everything But The Girl"])
        ));
    }

    #[test]
    fn rejects_an_artist_whose_name_merely_contains_it() {
        assert!(!text_match(
            "Dismantle",
            "Ghost",
            &track("Ghost", &["We Must Dismantle All this"])
        ));
    }

    #[test]
    fn rejects_another_artists_song_of_the_same_name() {
        assert!(!text_match(
            "Trex",
            "Architect",
            &track("Architect", &["Someone Else"])
        ));
    }

    fn listing(id: u64, isrc: Option<&str>, released: &str) -> RawTrack {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "name": "Us",
            "mix_name": "Pola & Bryson Remix",
            "slug": "us",
            "isrc": isrc,
            "new_release_date": released,
            "artists": [{"name": "Alcemist"}],
            "price": {"value": 1.49, "code": "USD"},
            "sale_type": {"name": "purchase"},
        }))
        .unwrap()
    }

    fn ids(tracks: &[RawTrack]) -> Vec<u64> {
        tracks.iter().map(|t| t.id).collect()
    }

    #[test]
    fn keeps_only_the_latest_release_of_a_recording() {
        // One recording on several releases, in the order search returns them.
        let tracks = vec![
            listing(1, Some("GB1"), "2021-03-05"),
            listing(2, Some("GB1"), "2024-11-01"),
            listing(3, Some("GB1"), "2019-06-14"),
        ];
        assert_eq!(ids(&latest_release_per_recording(tracks)), [2]);
    }

    #[test]
    fn keeps_versions_that_share_an_isrc_apart() {
        // Real: Beatport lists both of these under CBEFB2605337, same day.
        let mut extended = listing(1, Some("CBEFB2605337"), "2026-08-28");
        extended.mix_name = Some("Extended Mix".into());
        let mut radio = listing(2, Some("CBEFB2605337"), "2026-08-28");
        radio.mix_name = Some("Radio Edit".into());
        assert_eq!(
            ids(&latest_release_per_recording(vec![extended, radio])),
            [1, 2]
        );
    }

    #[test]
    fn keeps_each_version_of_a_song() {
        // An Extended Mix and a Radio Edit are different recordings.
        let tracks = vec![
            listing(1, Some("EXTENDED"), "2026-08-28"),
            listing(2, Some("RADIO"), "2026-09-10"),
            listing(3, Some("EXTENDED"), "2026-01-01"),
        ];
        assert_eq!(ids(&latest_release_per_recording(tracks)), [1, 2]);
    }

    #[test]
    fn keeps_listings_without_an_isrc() {
        let tracks = vec![
            listing(1, None, "2020-01-01"),
            listing(2, None, "2021-01-01"),
        ];
        assert_eq!(ids(&latest_release_per_recording(tracks)), [1, 2]);
    }
}
