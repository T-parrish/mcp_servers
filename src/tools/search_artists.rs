//! Action: search for artists / bands by name.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::bandcamp::RawResult;
use crate::server::BandcampServer;
use crate::tools::json_result;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArtistsParams {
    /// Artist / band name to search for.
    pub query: String,
    /// Maximum number of results to return (default 10).
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Artist {
    name: String,
    artist_id: Option<i64>,
    location: Option<String>,
    url: Option<String>,
}

impl From<RawResult> for Artist {
    fn from(r: RawResult) -> Self {
        Artist {
            name: r.name.unwrap_or_default(),
            artist_id: r.id,
            location: r.location,
            // Band results carry their URL in item_url_root, not item_url_path.
            url: r.item_url_root.or(r.item_url_path),
        }
    }
}

#[tool_router(router = search_artists_router, vis = "pub")]
impl BandcampServer {
    #[tool(description = "Search Bandcamp for artists / bands by name.")]
    #[tracing::instrument(
        name = "tools/call search_artists",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "search_artists",
            result.count = tracing::field::Empty,
        ),
        err,
    )]
    async fn search_artists(
        &self,
        Parameters(params): Parameters<SearchArtistsParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.search_artists_inner(params).await;
        crate::metrics::record_tool_call("search_artists", crate::tools::outcome(&result));
        result
    }

    async fn search_artists_inner(
        &self,
        params: SearchArtistsParams,
    ) -> Result<CallToolResult, McpError> {
        let limit = params.limit.unwrap_or(10);
        // "b" = band/artist entity ("a" is albums, "t" is tracks).
        let results = self
            .client()
            .autocomplete(&params.query, "b")
            .await
            .map_err(|e| McpError::internal_error(format!("artist search failed: {e}"), None))?;
        let artists: Vec<Artist> = results
            .into_iter()
            .filter(|r| r.result_type.as_deref() == Some("b"))
            .take(limit)
            .map(Artist::from)
            .collect();
        tracing::Span::current().record("result.count", artists.len());
        tracing::info!(count = artists.len(), "artist search complete");
        json_result(&artists)
    }
}
