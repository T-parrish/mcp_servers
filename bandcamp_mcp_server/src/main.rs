mod bandcamp;
mod server;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

use crate::server::BandcampServer;

/// `service.name` when the operator has not set `OTEL_SERVICE_NAME`. Defaulting
/// it here rather than in `.env` lets one workspace-wide `.env` serve every
/// server without them all reporting the same name.
const SERVICE_NAME: &str = "bandcamp-mcp";

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (from the working directory, searching parents — so the
    // workspace root's file is found) before anything reads the environment.
    // Real environment variables take precedence over it.
    let dotenv_path = dotenv::dotenv().ok();

    // Dropped at the end of `main`, which flushes any buffered spans.
    let _telemetry = mcp_core::telemetry::init(mcp_core::service_info!(SERVICE_NAME, "bandcamp"))?;

    match dotenv_path {
        Some(path) => tracing::info!(path = %path.display(), "loaded .env"),
        None => tracing::debug!("no .env file found"),
    }
    tracing::info!("starting bandcamp MCP server (stdio transport)");

    let service = BandcampServer::new()
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start server"))?;

    service.waiting().await?;
    tracing::info!("bandcamp MCP server shut down");
    Ok(())
}
