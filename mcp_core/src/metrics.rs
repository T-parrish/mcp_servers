//! The OpenTelemetry instruments shared by every server.
//!
//! These record unconditionally. When no meter provider is installed (the
//! default — see [`crate::telemetry`]), the global API hands back no-op
//! instruments, so the recording calls cost nothing.
//!
//! Attribute names follow the OpenTelemetry semantic conventions: HTTP client
//! attributes on the request histogram, `mcp.tool.name` on the tool counter.
//! Instrument names are namespaced per server (`spotify.request.duration`,
//! `bandcamp.request.duration`), so one telemetry backend can hold several.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram, Meter};

use crate::ServiceInfo;

/// Bucket boundaries in seconds. Sized for this workload: these APIs answer in
/// tens-to-hundreds of milliseconds, and rate-limiter waits are multiples of the
/// configured pacing interval.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 10.0,
];

/// Set by [`crate::telemetry::init`] before any tool can run. The fallbacks keep
/// tests and other non-server callers working without an explicit setup step.
static SCOPE: OnceLock<&'static str> = OnceLock::new();
static PREFIX: OnceLock<&'static str> = OnceLock::new();

pub(crate) fn configure(service: &ServiceInfo) {
    let _ = SCOPE.set(service.crate_name);
    let _ = PREFIX.set(service.metric_prefix);
}

/// The meter for this server's instrumentation scope. Servers use it for their
/// own instruments, beyond the shared ones below.
pub fn meter() -> Meter {
    opentelemetry::global::meter(SCOPE.get_or_init(|| "mcp_core"))
}

struct Instruments {
    tool_calls: Counter<u64>,
    request_duration: Histogram<f64>,
    rate_limiter_wait: Histogram<f64>,
}

fn instruments() -> &'static Instruments {
    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let prefix = *PREFIX.get_or_init(|| "mcp");
        let meter = meter();
        Instruments {
            tool_calls: meter
                .u64_counter("mcp.tool.calls")
                .with_description("MCP tool invocations, by tool and outcome.")
                .with_unit("{call}")
                .build(),
            request_duration: meter
                .f64_histogram(format!("{prefix}.request.duration"))
                .with_description("Duration of outbound requests to the upstream service.")
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            rate_limiter_wait: meter
                .f64_histogram(format!("{prefix}.rate_limiter.wait.duration"))
                .with_description(
                    "Time an outbound request spent blocked on the rate limiter before starting.",
                )
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
        }
    })
}

/// Count one tool invocation. `outcome` is `"ok"` or `"error"`.
pub fn record_tool_call(tool: &'static str, outcome: &'static str) {
    instruments().tool_calls.add(
        1,
        &[
            KeyValue::new("mcp.tool.name", tool),
            KeyValue::new("outcome", outcome),
        ],
    );
}

/// Record the duration of one outbound request.
///
/// `route` is the low-cardinality URL template (e.g. `/v1/playlists/{id}/tracks`)
/// for servers that have one; pass `None` to leave the attribute off. `status` is
/// the HTTP response status, or `None` when the request never produced one.
pub fn record_request(
    method: &'static str,
    host: &str,
    route: Option<&'static str>,
    status: Option<u16>,
    elapsed: Duration,
) {
    let mut attrs = vec![
        KeyValue::new("http.request.method", method),
        KeyValue::new("server.address", host.to_string()),
    ];
    if let Some(route) = route {
        attrs.push(KeyValue::new("url.template", route));
    }
    match status {
        Some(code) => attrs.push(KeyValue::new("http.response.status_code", i64::from(code))),
        None => attrs.push(KeyValue::new("error.type", "request_failed")),
    }
    instruments()
        .request_duration
        .record(elapsed.as_secs_f64(), &attrs);
}

/// Record how long a request waited for a rate-limiter slot.
pub fn record_rate_limiter_wait(elapsed: Duration) {
    instruments()
        .rate_limiter_wait
        .record(elapsed.as_secs_f64(), &[]);
}
