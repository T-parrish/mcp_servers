//! Action: record the songs of one playlist in the local library.
//!
//! Takes a playlist reference rather than a list of tracks, and fetches them
//! itself. Routing the track data back through the assistant would be slower,
//! far more expensive, and — because the songs table identifies a song by its
//! artist and title text — corruptible: a paraphrased title or a truncated list
//! becomes a wrong or missing row that nothing downstream can detect.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::SpotifyServer;
use crate::tools::playlist::{self, playlist_id};
use crate::tools::{api_error_result, json_result};

/// With no limit given, save the whole playlist: "save this playlist" meaning
/// "save its first hundred tracks" is a trap, and pages are cheap.
const DEFAULT_LIMIT: usize = usize::MAX;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SavePlaylistSongsParams {
    /// The playlist to record. Accepts a bare Spotify ID, a `spotify:playlist:…`
    /// URI, or an open.spotify.com playlist URL.
    pub playlist_id: String,
    /// Maximum number of tracks to record. Defaults to every track in the
    /// playlist; set it only to cap a very long one.
    pub limit: Option<usize>,
    /// Index of the first track to record (default 0).
    pub offset: Option<usize>,
}

#[tool_router(router = save_playlist_songs_router, vis = "pub")]
impl SpotifyServer {
    #[tool(
        description = "Record a Spotify playlist's songs in the local library, so they can be \
                          looked for on other stores later. Takes a playlist ID, URI or URL and \
                          fetches the tracks itself — do not pass track data. Saves the whole \
                          playlist by default. Songs already known are left untouched, so \
                          calling this again is safe and only adds what is new."
    )]
    #[tracing::instrument(
        name = "tools/call save_playlist_songs",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "save_playlist_songs",
            spotify.playlist.id = tracing::field::Empty,
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn save_playlist_songs(
        &self,
        Parameters(params): Parameters<SavePlaylistSongsParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.save_playlist_songs_inner(params).await;
        crate::metrics::record_tool_call("save_playlist_songs", crate::tools::outcome(&result));
        result
    }

    async fn save_playlist_songs_inner(
        &self,
        params: SavePlaylistSongsParams,
    ) -> Result<CallToolResult, McpError> {
        // Persistence is optional to configure, but never optional to succeed:
        // a save that quietly did nothing is the failure this tool exists to
        // make visible.
        let db = self.db().ok_or_else(|| {
            McpError::internal_error(
                "this server has no database configured, so it cannot save songs. Set \
                 DATABASE_URL in .env (`docker compose up -d postgres` starts a local one) and \
                 restart the server."
                    .to_string(),
                None,
            )
        })?;

        let id = playlist_id(&params.playlist_id).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "`{}` is not a Spotify playlist ID, URI or URL",
                    params.playlist_id
                ),
                None,
            )
        })?;
        tracing::Span::current().record("spotify.playlist.id", &id);

        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let offset = params.offset.unwrap_or(0);

        let (tracks, total) = match playlist::fetch_tracks(self.client(), &id, limit, offset).await
        {
            Ok(page) => page,
            Err(e) => return api_error_result(e, "reading the playlist to save it failed"),
        };

        // Podcast episodes and tracks missing an artist or title are not songs;
        // `to_new_song` drops them, so `songs` can be shorter than `tracks`.
        let songs: Vec<mcp_db::NewSong> = tracks
            .iter()
            .filter_map(playlist::PlaylistTrack::to_new_song)
            .collect();

        let stored = mcp_db::insert_songs(db, &songs).await.map_err(|e| {
            McpError::internal_error(format!("saving the playlist's songs failed: {e:#}"), None)
        })?;

        tracing::Span::current().record("result.count", stored);
        tracing::info!(
            stored,
            songs = songs.len(),
            tracks = tracks.len(),
            "playlist songs saved"
        );

        json_result(&json!({
            "playlist_id": id,
            "playlist_total": total,
            "tracks_read": tracks.len(),
            "songs_found": songs.len(),
            "newly_stored": stored,
            "already_known": songs.len() as u64 - stored,
        }))
    }
}
