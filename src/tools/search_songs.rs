//! Action: search for songs (tracks) by a given artist.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::bandcamp::RawResult;
use crate::server::BandcampServer;
use crate::tools::json_result;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchSongsParams {
    /// Artist / band name whose songs to search for.
    pub artist: String,
    /// Optional extra text (e.g. a partial track title) to narrow the search.
    pub query: Option<String>,
    /// Maximum number of results to return (default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Song {
    title: String,
    artist: Option<String>,
    /// Seller band id, needed to add the item to a cart.
    band_id: Option<i64>,
    album: Option<String>,
    track_id: Option<i64>,
    url: Option<String>,
}

impl From<RawResult> for Song {
    fn from(r: RawResult) -> Self {
        Song {
            title: r.name.unwrap_or_default(),
            artist: r.band_name,
            band_id: r.band_id,
            album: r.album_name,
            track_id: r.id,
            url: r.item_url_path,
        }
    }
}

#[tool_router(router = search_songs_router, vis = "pub")]
impl BandcampServer {
    #[tool(description = "Search Bandcamp for songs (tracks) by a given artist. \
                          Optionally narrow with a partial track title.")]
    async fn search_songs(
        &self,
        Parameters(params): Parameters<SearchSongsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(10);
        let search_text = match params.query.as_deref() {
            Some(q) => format!("{} {}", params.artist, q),
            None => params.artist.clone(),
        };
        // "t" = tracks.
        let results = self
            .client()
            .autocomplete(&search_text, "t")
            .await
            .map_err(|e| McpError::internal_error(format!("song search failed: {e}"), None))?;
        let artist_lc = params.artist.to_lowercase();
        let songs: Vec<Song> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("t"))
            // Keep only tracks whose band name matches the requested artist.
            .filter(|r| {
                r.band_name
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&artist_lc))
            })
            .take(limit)
            .map(Song::from)
            .collect();
        tracing::info!(count = songs.len(), "song search complete");
        json_result(&songs)
    }
}
