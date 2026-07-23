//! Machinery shared by the MCP servers in this workspace.
//!
//! Everything here was duplicated between the servers before the workspace
//! existed: telemetry wiring, the OpenTelemetry instruments, the outbound rate
//! limiter, and the two tool-result helpers. What differs per server — its HTTP
//! client, its auth model, its tools — deliberately stays in that server's crate.
//!
//! A server wires this up in `main` with [`service_info!`] and [`telemetry::init`]:
//!
//! ```ignore
//! let _telemetry = mcp_core::telemetry::init(mcp_core::service_info!("spotify-mcp", "spotify"))?;
//! ```

pub mod metrics;
pub mod ratelimit;
pub mod telemetry;
pub mod tools;

/// Describes the calling server to [`telemetry::init`].
///
/// Built by [`service_info!`] rather than by hand: the `CARGO_PKG_*` values have
/// to be expanded in the *server's* crate, since expanding them here would
/// describe `mcp_core` instead.
pub struct ServiceInfo {
    /// `service.name` when the operator has not set `OTEL_SERVICE_NAME`.
    pub default_service_name: &'static str,
    /// The server crate's name. Used as the instrumentation scope, and as the
    /// span target that is allowed through to the OTLP exporter.
    pub crate_name: &'static str,
    /// The server crate's version, reported as `service.version`.
    pub crate_version: &'static str,
    /// Prefix for this server's own metric names, e.g. `spotify` for
    /// `spotify.request.duration`.
    pub metric_prefix: &'static str,
}

/// Build a [`ServiceInfo`] for the calling crate.
///
/// `default_service_name` is the `service.name` to fall back to (e.g.
/// `"spotify-mcp"`), and `metric_prefix` namespaces this server's instruments
/// (e.g. `"spotify"`).
#[macro_export]
macro_rules! service_info {
    ($default_service_name:expr, $metric_prefix:expr) => {
        $crate::ServiceInfo {
            default_service_name: $default_service_name,
            crate_name: env!("CARGO_PKG_NAME"),
            crate_version: env!("CARGO_PKG_VERSION"),
            metric_prefix: $metric_prefix,
        }
    };
}
