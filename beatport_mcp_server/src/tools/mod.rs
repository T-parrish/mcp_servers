//! MCP tool actions.
//!
//! Each submodule defines one action — its parameter struct, its output type,
//! and its `#[tool]` handler — as a named tool router on [`BeatportServer`].
//!
//! # Adding a new action
//! 1. Create `tools/<action>.rs` with a
//!    `#[tool_router(router = <action>_router, vis = "pub")]` impl block on
//!    `BeatportServer` containing the `#[tool]` handler.
//! 2. Register it in the two marked spots below: add `mod <action>;` and add
//!    `+ BeatportServer::<action>_router()` in [`router`].

mod authenticate;
mod find_purchase_options;

use rmcp::handler::server::router::tool::ToolRouter;

pub(crate) use mcp_core::tools::{json_result, outcome};

use crate::server::BeatportServer;

/// The combined router of every action's tools.
pub(crate) fn router() -> ToolRouter<BeatportServer> {
    BeatportServer::authenticate_router() + BeatportServer::find_purchase_options_router()
}
