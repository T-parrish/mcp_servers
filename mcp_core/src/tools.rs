//! Helpers shared by every server's tool handlers.

use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, Content},
};

/// The `outcome` metric attribute for a handler's return value. Note that a
/// tool result carrying an in-band failure (e.g. `auth_required`) is still `ok`
/// here — this tracks protocol-level errors.
pub fn outcome(result: &Result<CallToolResult, McpError>) -> &'static str {
    if result.is_ok() { "ok" } else { "error" }
}

/// Serialize a value to a pretty-JSON tool result.
pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|e| McpError::internal_error(format!("failed to serialize result: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(json)]))
}
