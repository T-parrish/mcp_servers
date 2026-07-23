//! Spotify-specific instruments. The shared ones (tool calls, request duration,
//! rate-limiter wait) live in [`mcp_core::metrics`], which this re-exports so
//! call sites have a single `crate::metrics` to reach for.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

pub(crate) use mcp_core::metrics::{record_request, record_tool_call};

fn token_refreshes() -> &'static Counter<u64> {
    static INSTRUMENT: OnceLock<Counter<u64>> = OnceLock::new();
    INSTRUMENT.get_or_init(|| {
        mcp_core::metrics::meter()
            .u64_counter("spotify.token.refreshes")
            .with_description("OAuth access-token refreshes, by outcome.")
            .with_unit("{refresh}")
            .build()
    })
}

/// Count one access-token refresh attempt. `outcome` is `"ok"` or `"error"`.
pub(crate) fn record_token_refresh(outcome: &'static str) {
    token_refreshes().add(1, &[KeyValue::new("outcome", outcome)]);
}
