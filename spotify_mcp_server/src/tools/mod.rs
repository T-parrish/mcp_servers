//! MCP tool actions.
//!
//! Each submodule defines one action — its parameter struct, its output type,
//! and its `#[tool]` handler — as a named tool router on [`SpotifyServer`].
//! [`playlist`] is the exception: it holds what the two playlist actions share.
//!
//! # Adding a new action
//! 1. Create `tools/<action>.rs` with a
//!    `#[tool_router(router = <action>_router, vis = "pub")]` impl block on
//!    `SpotifyServer` containing the `#[tool]` handler.
//! 2. Register it in the two marked spots below: add `mod <action>;` and add
//!    `+ SpotifyServer::<action>_router()` in [`router`].

mod authenticate;
mod list_playlist_tracks;
mod list_playlists;
mod playlist;
mod save_playlist_songs;

use rmcp::{
    ErrorData as McpError, handler::server::router::tool::ToolRouter, model::CallToolResult,
};
use serde_json::json;

pub(crate) use mcp_core::tools::{json_result, outcome};

use crate::server::SpotifyServer;
use crate::spotify::ApiError;

/// The combined router of every action's tools.
pub(crate) fn router() -> ToolRouter<SpotifyServer> {
    SpotifyServer::authenticate_router()
        + SpotifyServer::list_playlists_router()
        + SpotifyServer::list_playlist_tracks_router()
        + SpotifyServer::save_playlist_songs_router()
}

/// Turn a client error into either an in-band `auth_required` result — which
/// tells the assistant to call `authenticate` — or a protocol error.
pub(crate) fn api_error_result(e: ApiError, context: &str) -> Result<CallToolResult, McpError> {
    match e {
        ApiError::AuthRequired(reason) => json_result(&json!({
            "status": "auth_required",
            "message": format!("{reason}. Call the `authenticate` tool to log in to Spotify."),
        })),
        ApiError::Other(e) => Err(McpError::internal_error(format!("{context}: {e:#}"), None)),
    }
}
