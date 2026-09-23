//! The MCP server type. Owns the shared Beatport client and the combined tool
//! router; individual actions live in `crate::tools`.

use std::sync::Arc;

use rmcp::{
    ServerHandler,
    handler::server::router::tool::ToolRouter,
    model::{ServerCapabilities, ServerInfo},
    tool_handler,
};

use crate::beatport::BeatportClient;

#[derive(Clone)]
pub struct BeatportServer {
    client: Arc<BeatportClient>,
    /// The song library, when one is configured. Only `find_purchase_options`
    /// needs it.
    db: Option<mcp_db::PgPool>,
    tool_router: ToolRouter<Self>,
}

impl BeatportServer {
    pub fn new(db: Option<mcp_db::PgPool>) -> Self {
        Self {
            client: Arc::new(BeatportClient::new()),
            db,
            tool_router: crate::tools::router(),
        }
    }

    /// Shared Beatport client, used by the tool actions.
    pub(crate) fn client(&self) -> &BeatportClient {
        &self.client
    }

    /// The database pool, if persistence is configured.
    pub(crate) fn db(&self) -> Option<&mcp_db::PgPool> {
        self.db.as_ref()
    }
}

#[tool_handler]
impl ServerHandler for BeatportServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Tools for Beatport: authenticate with a bearer token copied from a logged-in \
                 session, and record where a song from the local library can be bought and for \
                 how much. Backed by Beatport's v4 API."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
