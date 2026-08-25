//! The MCP server type. Owns the shared Spotify client and the combined tool
//! router; individual actions live in `crate::tools`.

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{ServerCapabilities, ServerInfo},
    tool_handler,
};

use crate::spotify::SpotifyClient;

#[derive(Clone)]
pub struct SpotifyServer {
    client: Arc<SpotifyClient>,
    /// Where songs are recorded. `None` when no database is configured, which
    /// only `save_playlist_songs` cares about — every other tool reads.
    db: Option<mcp_db::PgPool>,
    tool_router: ToolRouter<Self>,
}

impl SpotifyServer {
    pub fn new(db: Option<mcp_db::PgPool>) -> Self {
        Self {
            client: Arc::new(SpotifyClient::new()),
            db,
            tool_router: crate::tools::router(),
        }
    }

    /// Shared Spotify client, used by the tool actions.
    pub(crate) fn client(&self) -> &SpotifyClient {
        &self.client
    }

    /// The database pool, if persistence is configured.
    pub(crate) fn db(&self) -> Option<&mcp_db::PgPool> {
        self.db.as_ref()
    }
}

#[tool_handler]
impl ServerHandler for SpotifyServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for interacting with Spotify: log in via OAuth, list the playlists that \
                 belong to the logged-in user, list the tracks (with metadata) of a specific \
                 playlist, and save a playlist's songs to the local library for looking up on \
                 other stores later. Backed by the official Spotify Web API. Call `authenticate` \
                 first — the other tools return an `auth_required` result until a login is stored."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
