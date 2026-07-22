//! MCP tool actions.
//!
//! Each submodule defines one action — its parameter struct, its output type,
//! and its `#[tool]` handler — as a named tool router on [`BandcampServer`].
//!
//! # Adding a new action
//! 1. Create `tools/<action>.rs` with a
//!    `#[tool_router(router = <action>_router, vis = "pub")]` impl block on
//!    `BandcampServer` containing the `#[tool]` handler.
//! 2. Register it in the two marked spots below: add `mod <action>;` and add
//!    `+ BandcampServer::<action>_router()` in [`router`].

mod add_to_cart;
mod authenticate;
mod search_artists;
mod search_songs;

use rmcp::{
    ErrorData as McpError,
    handler::server::router::tool::ToolRouter,
    model::{CallToolResult, Content},
};

use crate::server::BandcampServer;

/// The combined router of every action's tools.
pub(crate) fn router() -> ToolRouter<BandcampServer> {
    BandcampServer::search_artists_router()
        + BandcampServer::search_songs_router()
        + BandcampServer::add_to_cart_router()
        + BandcampServer::authenticate_router()
}

/// The `outcome` metric attribute for a handler's return value. Note that a
/// tool result carrying an in-band failure (e.g. `auth_required`) is still `ok`
/// here — this tracks protocol-level errors.
pub(crate) fn outcome(result: &Result<CallToolResult, McpError>) -> &'static str {
    if result.is_ok() { "ok" } else { "error" }
}

/// Serialize a value to a pretty-JSON tool result. Shared by all actions.
pub(crate) fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("failed to serialize result: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
