//! Action: add an item to the cart.
//!
//! STUB: a real Bandcamp cart requires an authenticated user session and the
//! site's non-public purchase flow, which is intentionally not implemented
//! here. This records intent and returns a simulated result.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, schemars,
    tool, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::server::BandcampServer;
use crate::tools::json_result;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToCartParams {
    /// Bandcamp item id (a track id or album id) to add to the cart.
    pub item_id: u64,
    /// Item type: "track" or "album". Defaults to "track".
    pub item_type: Option<String>,
    /// Human-readable item name, used only in the confirmation message.
    pub item_name: Option<String>,
    /// Unit price to record on the cart line.
    pub price: Option<f64>,
}

#[derive(Debug, Serialize)]
struct CartResult {
    status: &'static str,
    message: String,
    item_id: u64,
    item_type: String,
    item_name: Option<String>,
    price: Option<f64>,
}

#[tool_router(router = add_to_cart_router, vis = "pub")]
impl BandcampServer {
    #[tool(description = "Add a Bandcamp item to the cart by its item id. \
                          NOTE: currently a stub — returns a simulated result and does \
                          not modify a real Bandcamp cart (that needs an authenticated session).")]
    async fn add_to_cart(
        &self,
        Parameters(params): Parameters<AddToCartParams>,
    ) -> Result<CallToolResult, McpError> {
        let item_type = params.item_type.as_deref().unwrap_or("track");
        tracing::warn!(
            item_id = params.item_id,
            item_type,
            "add_to_cart is stubbed: no real Bandcamp cart mutation is performed"
        );
        let result = CartResult {
            status: "simulated",
            message: format!(
                "Simulated adding {item_type} {} to cart. Real Bandcamp cart integration is not \
                 implemented (it requires an authenticated session).",
                params.item_id
            ),
            item_id: params.item_id,
            item_type: item_type.to_string(),
            item_name: params.item_name,
            price: params.price,
        };
        json_result(&result)
    }
}
