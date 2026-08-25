//! Action: list the tracks of one playlist, with their metadata.
//!
//! Read-only. Recording what it returns is `save_playlist_songs`'s job, so that
//! a database problem cannot cost the caller a Spotify request it already made.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::server::SpotifyServer;
use crate::tools::playlist::{self, playlist_id};
use crate::tools::{api_error_result, json_result};

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListPlaylistTracksParams {
    /// The playlist to read. Accepts a bare Spotify ID, a `spotify:playlist:…`
    /// URI, or an open.spotify.com playlist URL.
    pub playlist_id: String,
    /// Maximum number of tracks to return (default 100). Pages are followed
    /// automatically to reach this many.
    pub limit: Option<usize>,
    /// Index of the first track to return (default 0), for paging through a
    /// long playlist across several calls.
    pub offset: Option<usize>,
}

#[tool_router(router = list_playlist_tracks_router, vis = "pub")]
impl SpotifyServer {
    #[tool(
        description = "List the tracks of a Spotify playlist with their metadata (artists, \
                          album, duration, ISRC, popularity, when and by whom they were added). \
                          Takes a playlist ID, URI or URL — get one from `list_playlists`. \
                          This only reads; use `save_playlist_songs` to record a playlist's \
                          songs in the local library."
    )]
    #[tracing::instrument(
        name = "tools/call list_playlist_tracks",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "list_playlist_tracks",
            spotify.playlist.id = tracing::field::Empty,
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn list_playlist_tracks(
        &self,
        Parameters(params): Parameters<ListPlaylistTracksParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.list_playlist_tracks_inner(params).await;
        crate::metrics::record_tool_call("list_playlist_tracks", crate::tools::outcome(&result));
        result
    }

    async fn list_playlist_tracks_inner(
        &self,
        params: ListPlaylistTracksParams,
    ) -> Result<CallToolResult, McpError> {
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
            Err(e) => return api_error_result(e, "listing playlist tracks failed"),
        };

        tracing::Span::current().record("result.count", tracks.len());
        tracing::info!(
            count = tracks.len(),
            total,
            "playlist track listing complete"
        );

        json_result(&json!({
            "playlist_id": id,
            "offset": offset,
            "returned": tracks.len(),
            "total": total,
            "tracks": tracks,
        }))
    }
}
