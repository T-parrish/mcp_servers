//! Action: view auth status and save a Beatport bearer token.
//!
//! The token is persisted to a file (see [`BeatportClient::token_file`]) so it
//! survives restarts, and loaded into memory at startup.
//!
//! [`BeatportClient::token_file`]: crate::beatport::BeatportClient::token_file

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::beatport::{BeatportClient, SessionStatus};
use crate::server::BeatportServer;
use crate::tools::json_result;

const HOW_TO_GET_TOKEN: &str = "Log in on https://api.beatport.com/v4/docs/ , open your browser \
    dev tools → Network tab → click any request to api.beatport.com → copy the value of its \
    `Authorization` request header, and pass it as the `token` argument.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AuthenticateParams {
    /// A Beatport bearer token to save (with or without the `Bearer ` prefix).
    /// Omit to just check the current authentication status.
    pub token: Option<String>,
}

#[tool_router(router = authenticate_router, vis = "pub")]
impl BeatportServer {
    #[tool(
        description = "Check Beatport authentication status, or save a bearer token copied from \
                          a logged-in session on Beatport's API docs page. Omit `token` to see \
                          status."
    )]
    // `skip_all`: the params carry a bearer token, which must never reach a span.
    #[tracing::instrument(
        name = "tools/call authenticate",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "authenticate",
        ),
        err,
    )]
    async fn authenticate(
        &self,
        Parameters(params): Parameters<AuthenticateParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.authenticate_inner(params).await;
        mcp_core::metrics::record_tool_call("authenticate", crate::tools::outcome(&result));
        result
    }

    async fn authenticate_inner(
        &self,
        params: AuthenticateParams,
    ) -> Result<CallToolResult, McpError> {
        let client = self.client();
        if let Some(token) = params.token {
            client.save_token(&token).map_err(|e| {
                McpError::invalid_params(format!("could not save token: {e}"), None)
            })?;
        }
        // Verify against Beatport so "authenticated" reflects a real session.
        let status = client.verify_session().await;
        json_result(&status_report(client, &status))
    }
}

/// Build the JSON status report shared by the save and status-check paths.
fn status_report(client: &BeatportClient, status: &SessionStatus) -> Value {
    let (state, message, username) = match status {
        SessionStatus::Valid { username } => (
            "authenticated",
            "Logged in to Beatport.".to_string(),
            Some(username.as_str()),
        ),
        SessionStatus::Invalid => (
            "invalid_token",
            format!("The saved token is expired or invalid. {HOW_TO_GET_TOKEN}"),
            None,
        ),
        SessionStatus::NoToken => (
            "unauthenticated",
            format!("No token is loaded. {HOW_TO_GET_TOKEN}"),
            None,
        ),
        SessionStatus::Unknown(e) => (
            "unknown",
            format!("Could not verify the token with Beatport: {e}"),
            None,
        ),
    };
    json!({
        "status": state,
        "message": message,
        "username": username,
        "token_file": client.token_file().display().to_string(),
    })
}
