mod beatport;
mod server;
mod tools;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

use crate::server::BeatportServer;

/// `service.name` when the operator has not set `OTEL_SERVICE_NAME`. Defaulting
/// it here rather than in `.env` lets one workspace-wide `.env` serve every
/// server without them all reporting the same name.
const SERVICE_NAME: &str = "beatport-mcp";

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (from the working directory, searching parents — so the
    // workspace root's file is found) before anything reads the environment.
    // Real environment variables take precedence over it.
    let dotenv_path = dotenv::dotenv().ok();

    // Dropped at the end of `main`, which flushes any buffered spans.
    let _telemetry = mcp_core::telemetry::init(mcp_core::service_info!(SERVICE_NAME, "beatport"))?;

    match dotenv_path {
        Some(path) => tracing::info!(path = %path.display(), "loaded .env"),
        None => tracing::debug!("no .env file found"),
    }
    tracing::info!("starting beatport MCP server (stdio transport)");

    // `None` when DATABASE_URL is unset: only `find_purchase_options` needs it.
    // A database that *is* configured but unreachable or unmigrated still
    // stops startup.
    let db = mcp_db::connect()
        .await
        .inspect_err(|e| tracing::error!(error = %e, "database unavailable"))?;
    if db.is_none() {
        tracing::warn!("DATABASE_URL is not set; `find_purchase_options` will refuse to run");
    }

    let service = BeatportServer::new(db)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start server"))?;

    service.waiting().await?;
    tracing::info!("beatport MCP server shut down");
    Ok(())
}
