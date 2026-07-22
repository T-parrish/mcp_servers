mod bandcamp;
mod metrics;
mod server;
mod telemetry;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

use crate::server::BandcampServer;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (from the working directory) before anything reads the
    // environment. Real environment variables take precedence over it.
    let dotenv_path = dotenv::dotenv().ok();

    // Dropped at the end of `main`, which flushes any buffered spans.
    let _telemetry = telemetry::init()?;

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
