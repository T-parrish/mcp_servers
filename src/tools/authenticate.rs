//! Action: view auth status and save a Bandcamp session cookie.
//!
//! The cookie is persisted to a file (see [`BandcampClient::cookie_file`]) so it
//! survives restarts, and loaded into memory at startup. When the cookie is
//! missing or expired, `add_to_cart` returns an `auth_required` result telling
//! the assistant to call this tool with a fresh cookie.
//!
//! [`BandcampClient::cookie_file`]: crate::bandcamp::BandcampClient::cookie_file

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::bandcamp::SessionStatus;
use crate::server::BandcampServer;
use crate::tools::json_result;

const HOW_TO_GET_COOKIE: &str = "While logged in at bandcamp.com, open your browser dev tools \
    → Network tab → click any request → copy the full value of the `Cookie` request header, and \
    pass it as the `cookie` argument.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AuthenticateParams {
    /// A Bandcamp session `Cookie` header value to save. Omit to just check the
    /// current authentication status.
    pub cookie: Option<String>,
    /// If true, read the session cookie automatically from the local Chrome
    /// browser (you must be logged in at bandcamp.com in Chrome). On macOS this
    /// triggers a one-time Keychain "Allow" prompt. Takes precedence over `cookie`.
    pub from_browser: Option<bool>,
}

#[tool_router(router = authenticate_router, vis = "pub")]
impl BandcampServer {
    #[tool(description = "Check Bandcamp authentication status, or set a session cookie for cart \
                          operations. Pass `from_browser:true` to pull the cookie from Chrome \
                          automatically, or `cookie` to set it manually; omit both to see status.")]
    async fn authenticate(
        &self,
        Parameters(params): Parameters<AuthenticateParams>,
    ) -> Result<CallToolResult, McpError> {
        let client = self.client();

        // Obtain a cookie (browser or manual) and save it. Saving is the only
        // mutation; a bare status check is read-only.
        if params.from_browser == Some(true) {
            match client.fetch_browser_cookie().await {
                Ok(cookie) => client.save_cookie(&cookie).map_err(|e| {
                    McpError::internal_error(format!("could not save cookie: {e}"), None)
                })?,
                Err(e) => {
                    return json_result(&json!({
                        "status": "browser_fetch_failed",
                        "message": format!(
                            "Could not read the cookie from Chrome: {e}. Make sure you are logged \
                             in at bandcamp.com in Chrome and approved the Keychain prompt."
                        ),
                        "cookie_file": client.cookie_file().display().to_string(),
                    }));
                }
            }
        } else if let Some(cookie) = params.cookie {
            client
                .save_cookie(&cookie)
                .map_err(|e| McpError::invalid_params(format!("could not save cookie: {e}"), None))?;
        }

        // Verify against Bandcamp so "authenticated" reflects a real logged-in session.
        let status = client.verify_session().await;
        json_result(&status_report(client, &status))
    }
}

/// Build the JSON status report shared by the save and status-check paths.
fn status_report(client: &crate::bandcamp::BandcampClient, status: &SessionStatus) -> Value {
    let cookie_file = client.cookie_file().display().to_string();
    let (state, message, fan_id) = match status {
        SessionStatus::Valid { fan_id } => (
            "authenticated",
            "Logged in to Bandcamp. add_to_cart will target your account cart \
             (set BANDCAMP_ALLOW_CART_WRITES=1 to send live requests)."
                .to_string(),
            *fan_id,
        ),
        SessionStatus::Invalid => (
            "invalid_cookie",
            format!("The saved cookie is not a valid logged-in session. {HOW_TO_GET_COOKIE}"),
            None,
        ),
        SessionStatus::NoCookie => (
            "unauthenticated",
            format!("No session cookie is loaded. {HOW_TO_GET_COOKIE}"),
            None,
        ),
        SessionStatus::Unknown(e) => (
            "unknown",
            format!("Could not verify the session with Bandcamp: {e}"),
            None,
        ),
    };
    json!({
        "status": state,
        "message": message,
        "fan_id": fan_id,
        "cart_writes_enabled": client.cart_writes_enabled(),
        "cookie_file": cookie_file,
    })
}
