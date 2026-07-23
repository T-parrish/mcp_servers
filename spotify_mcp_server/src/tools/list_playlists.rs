//! Action: list the logged-in user's playlists.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::server::SpotifyServer;
use crate::tools::{api_error_result, json_result};

const ROUTE: &str = "/v1/me/playlists";
const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListPlaylistsParams {
    /// Maximum number of playlists to return (default 50). Pages are followed
    /// automatically to reach this many.
    pub limit: Option<usize>,
    /// If true (the default), return only playlists you own. Set false to also
    /// include playlists you merely follow.
    pub only_mine: Option<bool>,
}

#[derive(Debug, Serialize)]
struct Playlist {
    id: String,
    name: String,
    description: Option<String>,
    owner: Option<String>,
    owner_id: String,
    public: Option<bool>,
    collaborative: bool,
    track_count: u32,
    url: Option<String>,
    /// Version identifier of the playlist's contents; changes on every edit.
    snapshot_id: Option<String>,
}

impl From<RawPlaylist> for Playlist {
    fn from(p: RawPlaylist) -> Self {
        Playlist {
            id: p.id,
            name: p.name,
            description: p.description.filter(|d| !d.is_empty()),
            owner: p.owner.display_name,
            owner_id: p.owner.id,
            public: p.public,
            collaborative: p.collaborative,
            track_count: p.tracks.total,
            url: p.external_urls.spotify,
            snapshot_id: p.snapshot_id,
        }
    }
}

#[tool_router(router = list_playlists_router, vis = "pub")]
impl SpotifyServer {
    #[tool(
        description = "List the Spotify playlists that belong to the logged-in user (including \
                          private and collaborative ones). Pass `only_mine:false` to also include \
                          playlists you follow but do not own."
    )]
    #[tracing::instrument(
        name = "tools/call list_playlists",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "list_playlists",
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn list_playlists(
        &self,
        Parameters(params): Parameters<ListPlaylistsParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.list_playlists_inner(params).await;
        crate::metrics::record_tool_call("list_playlists", crate::tools::outcome(&result));
        result
    }

    async fn list_playlists_inner(
        &self,
        params: ListPlaylistsParams,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        let only_mine = params.only_mine.unwrap_or(true);

        // Ownership is decided by comparing against the account's own user id,
        // so that lookup is needed before the listing can be filtered.
        let me = if only_mine {
            match self.client().current_user().await {
                Ok(user) => Some(user.id),
                Err(e) => return api_error_result(e, "could not identify the logged-in user"),
            }
        } else {
            None
        };

        // `only_mine` filters client-side (the endpoint has no owner filter), so
        // fetch without a cap and truncate afterwards.
        let fetch_limit = if only_mine { usize::MAX } else { limit };
        let (raw, total) = match self
            .client()
            .get_paged::<RawPlaylist>(ROUTE, ROUTE, &[], fetch_limit)
            .await
        {
            Ok(page) => page,
            Err(e) => return api_error_result(e, "listing playlists failed"),
        };

        let playlists: Vec<Playlist> = raw
            .into_iter()
            .filter(|p| me.as_ref().is_none_or(|id| &p.owner.id == id))
            .take(limit)
            .map(Playlist::from)
            .collect();

        tracing::Span::current().record("result.count", playlists.len());
        tracing::info!(
            count = playlists.len(),
            total,
            only_mine,
            "playlist listing complete"
        );
        json_result(&playlists)
    }
}

// --- Raw wire types (Spotify simplified playlist object) ---

#[derive(Debug, Deserialize)]
struct RawPlaylist {
    id: String,
    name: String,
    description: Option<String>,
    owner: RawOwner,
    /// Absent when the playlist's visibility is not exposed to this user.
    public: Option<bool>,
    #[serde(default)]
    collaborative: bool,
    tracks: RawTracksRef,
    snapshot_id: Option<String>,
    #[serde(default)]
    external_urls: RawExternalUrls,
}

#[derive(Debug, Deserialize)]
struct RawOwner {
    id: String,
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawTracksRef {
    total: u32,
}

#[derive(Debug, Default, Deserialize)]
struct RawExternalUrls {
    spotify: Option<String>,
}
