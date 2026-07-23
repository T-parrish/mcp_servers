//! Action: add an item to the authenticated user's Bandcamp cart.
//!
//! This drives Bandcamp's undocumented `POST /cart/cb` endpoint (reverse-engineered
//! from the site's `tralbum` bundle), so the exact contract may change without notice.
//!
//! Auth & safety (both read from the environment by [`BandcampClient`]):
//! - `BANDCAMP_COOKIE` — the `Cookie` header from a logged-in Bandcamp session.
//! - `BANDCAMP_ALLOW_CART_WRITES` — must be `1`/`true` to actually send. Otherwise
//!   every call is a **dry run** that returns the exact request without sending it.
//!
//! [`BandcampClient`]: crate::bandcamp::BandcampClient

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::Deserialize;
use serde_json::json;

use crate::bandcamp::{BandcampClient, CartError, SessionStatus};
use crate::server::BandcampServer;
use crate::tools::json_result;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToCartParams {
    /// Bandcamp item id to add (an album id or track id, as returned by the search tools).
    pub item_id: u64,
    /// Item type: "album", "track", or "package". Defaults to "album".
    pub item_type: Option<String>,
    /// Price to pay, in the item's currency. Must meet the item's minimum price
    /// (many Bandcamp items are "name your price").
    pub unit_price: f64,
    /// Seller's band id (the `band_id` from a search result). Recommended: Bandcamp
    /// associates the cart line with the selling band.
    pub band_id: Option<u64>,
    /// Quantity to add. Defaults to 1.
    pub quantity: Option<u32>,
    /// The item's Bandcamp URL (e.g. from a search result). Used to target the
    /// correct site origin for the request; defaults to https://bandcamp.com.
    pub item_url: Option<String>,
    /// Human-readable item name, used only in logs and the confirmation message.
    pub item_name: Option<String>,
}

/// Map a friendly item type to Bandcamp's single-letter code.
fn item_type_code(input: &str) -> Result<&'static str, McpError> {
    match input.trim().to_lowercase().as_str() {
        "album" | "a" => Ok("a"),
        "track" | "t" => Ok("t"),
        "package" | "p" => Ok("p"),
        other => Err(McpError::invalid_params(
            format!("invalid item_type {other:?}; expected album, track, or package"),
            None,
        )),
    }
}

/// Derive the `scheme://host` origin to post to from an optional item URL.
fn origin_from_url(item_url: Option<&str>) -> String {
    item_url
        .and_then(|u| reqwest::Url::parse(u).ok())
        .and_then(|u| {
            let scheme = u.scheme().to_string();
            u.host_str().map(|h| format!("{scheme}://{h}"))
        })
        .unwrap_or_else(|| "https://bandcamp.com".to_string())
}

/// An `auth_required` tool result telling the assistant to (re-)authenticate.
fn auth_required(client: &BandcampClient, reason: &str) -> Result<CallToolResult, McpError> {
    json_result(&json!({
        "mode": "auth_required",
        "message": format!(
            "Not authenticated with Bandcamp ({reason}). Call the `authenticate` tool with a \
             fresh session cookie, then retry."
        ),
        "cookie_file": client.cookie_file().display().to_string(),
    }))
}

/// A pseudo-random id string, mimicking the site's `""+Math.random()` ids.
fn rand_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("0.{nanos}")
}

#[tool_router(router = add_to_cart_router, vis = "pub")]
impl BandcampServer {
    #[tool(
        description = "Add a Bandcamp item to your cart by its item id. Requires a logged-in \
                          session via BANDCAMP_COOKIE. Dry-run by default: it only sends the real \
                          request when BANDCAMP_ALLOW_CART_WRITES=1, otherwise it returns the \
                          request that would be sent. unit_price must meet the item's minimum."
    )]
    #[tracing::instrument(
        name = "tools/call add_to_cart",
        skip_all,
        fields(
            otel.kind = "server",
            mcp.method.name = "tools/call",
            mcp.tool.name = "add_to_cart",
            item_id = params.item_id,
            dry_run = !self.client().cart_writes_enabled(),
        ),
        err,
    )]
    async fn add_to_cart(
        &self,
        Parameters(params): Parameters<AddToCartParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self.add_to_cart_inner(params).await;
        mcp_core::metrics::record_tool_call("add_to_cart", crate::tools::outcome(&result));
        result
    }

    async fn add_to_cart_inner(&self, params: AddToCartParams) -> Result<CallToolResult, McpError> {
        let item_type = item_type_code(params.item_type.as_deref().unwrap_or("album"))?;
        let quantity = params.quantity.unwrap_or(1);
        let origin = origin_from_url(params.item_url.as_deref());

        // Fields for the /cart/cb "add" operation, per the reverse-engineered protocol.
        let mut form: Vec<(&str, String)> = vec![
            ("req", "add".to_string()),
            ("req_id", rand_id()),
            ("sync_num", "1".to_string()),
            ("local_id", rand_id()),
            ("item_type", item_type.to_string()),
            ("item_id", params.item_id.to_string()),
            ("unit_price", format!("{}", params.unit_price)),
            ("quantity", quantity.to_string()),
        ];
        if let Some(band_id) = params.band_id {
            form.push(("band_id", band_id.to_string()));
        }

        let target_url = format!("{origin}/cart/cb");
        let request_view: serde_json::Value = form
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();

        // Dry-run: don't mutate the real cart unless explicitly allowed.
        if !self.client().cart_writes_enabled() {
            tracing::info!(
                item_id = params.item_id,
                item_type,
                "add_to_cart dry-run (BANDCAMP_ALLOW_CART_WRITES not set)"
            );
            return json_result(&json!({
                "mode": "dry_run",
                "message": "Dry run: no request was sent. Set BANDCAMP_ALLOW_CART_WRITES=1 to \
                            actually add to the cart.",
                "target_url": target_url,
                "request": request_view,
                "item_name": params.item_name,
            }));
        }

        // Verify a valid session first: Bandcamp accepts anonymous cart adds, so
        // without this the item would land in a throwaway anonymous cart instead
        // of the user's account. This is also how we detect an expired cookie.
        match self.client().verify_session().await {
            SessionStatus::Valid { .. } => {}
            SessionStatus::NoCookie => {
                return auth_required(self.client(), "no session cookie is loaded");
            }
            SessionStatus::Invalid => {
                return auth_required(
                    self.client(),
                    "the saved session cookie is expired or invalid",
                );
            }
            SessionStatus::Unknown(e) => {
                tracing::warn!(error = %e, "could not verify session; attempting the add anyway");
            }
        }

        match self.client().post_cart_cb(&origin, &form).await {
            Ok(response) => {
                tracing::info!(item_id = params.item_id, item_type, "add_to_cart sent");
                json_result(&json!({
                    "mode": "sent",
                    "target_url": target_url,
                    "request": request_view,
                    "cart_response": response,
                    "item_name": params.item_name,
                }))
            }
            Err(CartError::AuthRequired(reason)) => {
                tracing::warn!(%reason, "add_to_cart needs authentication");
                auth_required(self.client(), &reason)
            }
            Err(CartError::Other(e)) => Err(McpError::internal_error(
                format!("add to cart failed: {e}"),
                None,
            )),
        }
    }
}
