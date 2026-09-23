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

use crate::bandcamp::{
    BandcampClient, CartError, DEFAULT_CART_ORIGIN, Price, SessionStatus, bandcamp_origin,
};
use crate::server::BandcampServer;
use crate::tools::json_result;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToCartParams {
    /// Bandcamp item id to add (an album id or track id, as returned by the search tools).
    pub item_id: u64,
    /// Item type: "album", "track", or "package". Defaults to "album".
    pub item_type: Option<String>,
    /// Price to pay, in the item's currency. Defaults to the item's minimum,
    /// read from its page at `item_url`; a lower price is refused. Many
    /// Bandcamp items are "name your price", so paying more is allowed.
    pub unit_price: Option<f64>,
    /// Seller's band id (the `band_id` from a search result). Recommended: Bandcamp
    /// associates the cart line with the selling band.
    pub band_id: Option<u64>,
    /// Quantity to add. Defaults to 1.
    pub quantity: Option<u32>,
    /// The item's Bandcamp URL (e.g. from a search result). Its page is where
    /// the minimum price is read, so it is required unless `unit_price` is
    /// given. Also targets the correct site origin for the request; defaults
    /// to https://bandcamp.com.
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

/// Derive the `https://host` origin to post to from an optional item URL.
///
/// `item_url` is model-supplied, so anything that is not a Bandcamp origin falls
/// back to the default rather than being used as a target.
fn origin_from_url(item_url: Option<&str>) -> String {
    let Some(url) = item_url else {
        return DEFAULT_CART_ORIGIN.to_string();
    };
    bandcamp_origin(url).unwrap_or_else(|| {
        tracing::warn!(%url, "item_url is not a bandcamp URL; posting to the default origin");
        DEFAULT_CART_ORIGIN.to_string()
    })
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
                          request that would be sent. Pass the item's `item_url`: the price is read \
                          from its page, `unit_price` defaults to the item's minimum, and a \
                          unit_price below the minimum is refused. The result shows the price and \
                          its currency."
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

        // The minimum is read from the item's page. When it cannot be (the `Err`
        // says why), a caller-given price is sent unchecked with a warning; with
        // no price either, there is nothing safe to send.
        let minimum: Result<Price, String> = match params.item_url.as_deref() {
            None => Err("no item_url was given, so the price was not checked".to_string()),
            Some(url) => match self
                .client()
                .item_price(url, item_type, params.item_id)
                .await
            {
                Ok(Some(price)) => Ok(price),
                // The page was read and does not list this item: most often a
                // track id sent with the default item_type "album", or an item
                // only sold as part of a release. Neither should reach the cart.
                Ok(None) => {
                    return Err(McpError::invalid_params(
                        format!(
                            "the page at {url} does not sell item {} as item_type {:?}. Check \
                             that item_type matches item_id (search results are tracks), and \
                             that the item is sold on its own.",
                            params.item_id,
                            params.item_type.as_deref().unwrap_or("album"),
                        ),
                        None,
                    ));
                }
                Err(e) => Err(format!("the price could not be checked: {e:#}")),
            },
        };
        let unit_price = match (params.unit_price, &minimum) {
            (Some(price), Ok(min)) if price < min.amount => {
                return Err(McpError::invalid_params(
                    format!(
                        "unit_price {price} is below this item's minimum of {} {}",
                        min.amount, min.currency
                    ),
                    None,
                ));
            }
            (Some(price), _) => price,
            (None, Ok(min)) => min.amount,
            (None, Err(why)) => {
                return Err(McpError::invalid_params(
                    format!("no unit_price was given and {why}; pass unit_price"),
                    None,
                ));
            }
        };
        // Shown in every result, so the price and currency are visible before
        // (and after) anything is sent.
        let price_view = match &minimum {
            Ok(min) => json!({
                "unit_price": unit_price,
                "currency": min.currency,
                "minimum": min.amount,
            }),
            Err(why) => json!({
                "unit_price": unit_price,
                "currency": null,
                "minimum": null,
                "warning": why,
            }),
        };

        // Fields for the /cart/cb "add" operation, per the reverse-engineered protocol.
        let mut form: Vec<(&str, String)> = vec![
            ("req", "add".to_string()),
            ("req_id", rand_id()),
            ("sync_num", "1".to_string()),
            ("local_id", rand_id()),
            ("item_type", item_type.to_string()),
            ("item_id", params.item_id.to_string()),
            ("unit_price", format!("{unit_price}")),
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
                "price": price_view,
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
                    "price": price_view,
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

#[cfg(test)]
mod tests {
    use super::origin_from_url;

    #[test]
    fn keeps_bandcamp_origins() {
        assert_eq!(
            origin_from_url(Some("https://bandcamp.com/album/x")),
            "https://bandcamp.com"
        );
        // Artist subdomains are where real item URLs live.
        assert_eq!(
            origin_from_url(Some("https://naibu.bandcamp.com/track/x")),
            "https://naibu.bandcamp.com"
        );
    }

    #[test]
    fn falls_back_for_anything_else() {
        // A poisoned search result or an injected instruction must not be able to
        // aim the cookie-bearing cart POST somewhere else.
        for hostile in [
            "https://attacker.example/x",
            "https://bandcamp.com.evil.example/x",
            "not a url",
        ] {
            assert_eq!(origin_from_url(Some(hostile)), "https://bandcamp.com");
        }
        assert_eq!(origin_from_url(None), "https://bandcamp.com");
    }

    #[test]
    fn upgrades_plaintext_bandcamp_urls() {
        assert_eq!(
            origin_from_url(Some("http://bandcamp.com/album/x")),
            "https://bandcamp.com"
        );
    }
}
