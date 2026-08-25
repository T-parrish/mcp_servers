//! The MCP server type. Owns the shared Bandcamp client and the combined tool
//! router; individual actions live in `crate::tools`.

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{ServerCapabilities, ServerInfo},
    tool_handler,
};

use crate::bandcamp::BandcampClient;

#[derive(Clone)]
pub struct BandcampServer {
    client: Arc<BandcampClient>,
    /// The song library, when one is configured. Only the tools that record
    /// where a song can be bought need it; searching does not.
    db: Option<mcp_db::PgPool>,
    tool_router: ToolRouter<Self>,
}

impl BandcampServer {
    pub fn new(db: Option<mcp_db::PgPool>) -> Self {
        Self {
            client: Arc::new(BandcampClient::new()),
            db,
            tool_router: crate::tools::router(),
        }
    }

    /// Shared Bandcamp client, used by the tool actions.
    pub(crate) fn client(&self) -> &BandcampClient {
        &self.client
    }

    /// The database pool, if persistence is configured.
    pub(crate) fn db(&self) -> Option<&mcp_db::PgPool> {
        self.db.as_ref()
    }
}

#[tool_handler]
impl ServerHandler for BandcampServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for interacting with Bandcamp: search for artists, search for songs by an \
                 artist, record where a song from the local library can be bought, and add items \
                 to a cart (currently stubbed). Backed by Bandcamp's undocumented internal \
                 search API."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
