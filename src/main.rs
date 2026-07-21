mod bandcamp;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use crate::server::BandcampServer;

#[tokio::main]
async fn main() -> Result<()> {
    // MCP over stdio uses stdout for the protocol stream, so all logs/traces
    // must go to stderr. Level is controlled by RUST_LOG (default: info).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!("starting bandcamp MCP server (stdio transport)");

    let service = BandcampServer::new()
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start server"))?;

    service.waiting().await?;
    tracing::info!("bandcamp MCP server shut down");
    Ok(())
}
