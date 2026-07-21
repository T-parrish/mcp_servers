//! MCP server: exposes the Bandcamp client as three tools over the rmcp router.

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::bandcamp::BandcampClient;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArtistsParams {
    /// Artist / band name to search for.
    pub query: String,
    /// Maximum number of results to return (default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchSongsParams {
    /// Artist / band name whose songs to search for.
    pub artist: String,
    /// Optional extra text (e.g. a partial track title) to narrow the search.
    pub query: Option<String>,
    /// Maximum number of results to return (default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToCartParams {
    /// Bandcamp item id (a track id or album id) to add to the cart.
    pub item_id: u64,
    /// Item type: "track" or "album". Defaults to "track".
    pub item_type: Option<String>,
    /// Human-readable item name, used only in the confirmation message.
    pub item_name: Option<String>,
    /// Unit price to record on the cart line.
    pub price: Option<f64>,
}

#[derive(Clone)]
pub struct BandcampServer {
    client: Arc<BandcampClient>,
    tool_router: ToolRouter<Self>,
}

impl BandcampServer {
    pub fn new() -> Self {
        Self {
            client: Arc::new(BandcampClient::new()),
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for BandcampServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Serialize any value to a pretty JSON tool result.
fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("failed to serialize result: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}

#[tool_router]
impl BandcampServer {
    #[tool(description = "Search Bandcamp for artists / bands by name.")]
    async fn search_artists(
        &self,
        Parameters(params): Parameters<SearchArtistsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(10);
        let artists = self
            .client
            .search_artists(&params.query, limit)
            .await
            .map_err(|e| McpError::internal_error(format!("artist search failed: {e}"), None))?;
        json_result(&artists)
    }

    #[tool(description = "Search Bandcamp for songs (tracks) by a given artist. \
                          Optionally narrow with a partial track title.")]
    async fn search_songs(
        &self,
        Parameters(params): Parameters<SearchSongsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(10);
        let songs = self
            .client
            .search_songs(&params.artist, params.query.as_deref(), limit)
            .await
            .map_err(|e| McpError::internal_error(format!("song search failed: {e}"), None))?;
        json_result(&songs)
    }

    #[tool(description = "Add a Bandcamp item to the cart by its item id. \
                          NOTE: currently a stub — returns a simulated result and does \
                          not modify a real Bandcamp cart (that needs an authenticated session).")]
    async fn add_to_cart(
        &self,
        Parameters(params): Parameters<AddToCartParams>,
    ) -> Result<CallToolResult, McpError> {
        let item_type = params.item_type.as_deref().unwrap_or("track");
        let result = self.client.add_to_cart(
            params.item_id,
            item_type,
            params.item_name.as_deref(),
            params.price,
        );
        json_result(&result)
    }
}

#[tool_handler]
impl ServerHandler for BandcampServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for interacting with Bandcamp: search for artists, search for songs by an \
                 artist, and add items to a cart (currently stubbed). Backed by Bandcamp's \
                 undocumented internal search API."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
