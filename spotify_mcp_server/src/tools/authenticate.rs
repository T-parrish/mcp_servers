//! Action: log in to Spotify, or report the current login.
//!
//! Runs the Authorization Code flow with PKCE: a one-shot loopback listener is
//! bound to the configured redirect URI, the browser is pointed at Spotify's
//! consent page, and the redirect that comes back is redeemed for a token set
//! (persisted by [`SpotifyClient`], so later runs skip all of this).
//!
//! The handler blocks while the user completes the consent screen, so it is
//! bounded by `timeout_seconds`.
//!
//! [`SpotifyClient`]: crate::spotify::SpotifyClient

use std::time::Duration;

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::oauth;
use crate::server::SpotifyServer;
use crate::spotify::{ApiError, CurrentUser, SCOPES, SpotifyClient, TokenStatus};
use crate::tools::json_result;

const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// What to tell the user when no application credentials are configured.
const HOW_TO_CONFIGURE: &str = "Create an app at https://developer.spotify.com/dashboard, add the \
    redirect URI below to its settings, then set SPOTIFY_CLIENT_ID (in the environment or the \
    server's .env) and restart the server.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AuthenticateParams {
    /// Start a new browser login even if a stored login is still valid.
    pub force: Option<bool>,
    /// How long to wait for the browser redirect, in seconds (default 120).
    pub timeout_seconds: Option<u64>,
}

#[tool_router(router = authenticate_router, vis = "pub")]
impl SpotifyServer {
    #[tool(
        description = "Log in to Spotify, or report the current login. Opens the Spotify \
                          consent page in your browser and captures the redirect automatically; \
                          the login is saved and refreshed, so this normally only has to be done \
                          once. Pass `force:true` to re-authorize."
    )]
    // `skip_all`: the flow handles authorization codes and tokens, none of
    // which may ever reach a span.
    #[tracing::instrument(
        name = "tools/call authenticate",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "authenticate",
            auth.status = tracing::field::Empty,
        ),
        err,
    )]
    async fn authenticate(
        &self,
        Parameters(params): Parameters<AuthenticateParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.authenticate_inner(params).await;
        crate::metrics::record_tool_call("authenticate", crate::tools::outcome(&result));
        result
    }

    async fn authenticate_inner(
        &self,
        params: AuthenticateParams,
    ) -> Result<CallToolResult, McpError> {
        let client = self.client();

        // Reuse a stored login unless asked not to. Verifying against /v1/me is
        // what makes "authenticated" mean a session Spotify actually accepts.
        if params.force != Some(true) && matches!(client.token_status(), TokenStatus::Valid { .. })
        {
            match client.current_user().await {
                Ok(user) => {
                    tracing::Span::current().record("auth.status", "authenticated");
                    return json_result(&report(client, "authenticated", Some(user)));
                }
                // Anything the client can't recover from means a fresh login.
                Err(ApiError::AuthRequired(reason)) => {
                    tracing::info!(%reason, "stored login is unusable; starting a new one");
                }
                Err(ApiError::Other(e)) => {
                    return Err(McpError::internal_error(
                        format!("could not verify the stored login: {e:#}"),
                        None,
                    ));
                }
            }
        }

        let Some(client_id) = client.client_id() else {
            tracing::Span::current().record("auth.status", "not_configured");
            return json_result(&report(client, "not_configured", None));
        };

        // Bind before advertising the URL so the redirect can never outrace the
        // listener.
        let listener = match oauth::bind_callback(client.redirect_uri()).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::Span::current().record("auth.status", "listener_failed");
                let mut value = report(client, "listener_failed", None);
                value["message"] = json!(format!(
                    "Could not listen for the OAuth redirect: {e:#}. Free the port, or point \
                     SPOTIFY_REDIRECT_URI at another loopback port registered on your Spotify app."
                ));
                return json_result(&value);
            }
        };

        let pkce = oauth::pkce();
        let state = oauth::state();
        let url = oauth::authorize_url(client_id, client.redirect_uri(), SCOPES, &pkce, &state)
            .map_err(|e| {
                McpError::internal_error(format!("could not build the authorize URL: {e:#}"), None)
            })?;
        if let Err(e) = oauth::open_in_browser(&url) {
            tracing::warn!(error = %e, "could not open the browser; the user must open the URL");
        }

        let timeout = Duration::from_secs(params.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let code =
            match oauth::wait_for_code(listener, client.redirect_uri(), &state, timeout).await {
                Ok(code) => code,
                Err(e) => {
                    tracing::Span::current().record("auth.status", "authorization_failed");
                    let mut value = report(client, "authorization_failed", None);
                    value["message"] = json!(format!(
                        "Authorization did not complete: {e:#}. Open this URL in a browser and \
                         approve access, then call `authenticate` again: {url}"
                    ));
                    return json_result(&value);
                }
            };

        client
            .exchange_code(&code, &pkce.verifier)
            .await
            .map_err(|e| {
                McpError::internal_error(
                    format!("could not redeem the authorization code: {e:#}"),
                    None,
                )
            })?;

        let user = client.current_user().await.map_err(|e| {
            McpError::internal_error(format!("logged in, but /v1/me failed: {e}"), None)
        })?;
        tracing::Span::current().record("auth.status", "authenticated");
        tracing::info!(user_id = %user.id, "spotify authorization complete");
        json_result(&report(client, "authenticated", Some(user)))
    }
}

/// Build the JSON status report shared by every path through the handler.
fn report(client: &SpotifyClient, status: &str, user: Option<CurrentUser>) -> Value {
    let message = match status {
        "authenticated" => "Logged in to Spotify. `list_playlists` and `list_playlist_tracks` are \
                            ready to use."
            .to_string(),
        "not_configured" => format!("No SPOTIFY_CLIENT_ID is configured. {HOW_TO_CONFIGURE}"),
        _ => String::new(),
    };
    let (expires_in_seconds, scopes) = match client.token_status() {
        TokenStatus::Valid { expires_at, scope } => (
            Some(
                expires_at.saturating_sub(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                ),
            ),
            Some(scope),
        ),
        _ => (None, None),
    };
    json!({
        "status": status,
        "message": message,
        "user": user,
        "scopes": scopes,
        "access_token_expires_in_seconds": expires_in_seconds,
        "redirect_uri": client.redirect_uri(),
        "token_file": client.token_file().display().to_string(),
    })
}
