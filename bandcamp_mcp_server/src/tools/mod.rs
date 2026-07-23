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

use rmcp::handler::server::router::tool::ToolRouter;

pub(crate) use mcp_core::tools::{json_result, outcome};

use crate::server::BandcampServer;

/// The combined router of every action's tools.
pub(crate) fn router() -> ToolRouter<BandcampServer> {
    BandcampServer::search_artists_router()
        + BandcampServer::search_songs_router()
        + BandcampServer::add_to_cart_router()
        + BandcampServer::authenticate_router()
}
