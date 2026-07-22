//! The server's OpenTelemetry instruments.
//!
//! These record unconditionally. When no meter provider is installed (the
//! default — see [`crate::telemetry`]), the global API hands back no-op
//! instruments, so the recording calls cost nothing.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Histogram};

/// Bucket boundaries in seconds. Sized for this workload: Bandcamp calls land
/// in the tens-to-hundreds of milliseconds, and rate-limiter waits are multiples
/// of the 750 ms default pacing interval.
const DURATION_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 10.0,
];

struct Instruments {
    tool_calls: Counter<u64>,
    request_duration: Histogram<f64>,
    rate_limiter_wait: Histogram<f64>,
}

fn instruments() -> &'static Instruments {
    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter(env!("CARGO_PKG_NAME"));
        Instruments {
            tool_calls: meter
                .u64_counter("mcp.tool.calls")
                .with_description("MCP tool invocations, by tool and outcome.")
                .with_unit("{call}")
                .build(),
            request_duration: meter
                .f64_histogram("bandcamp.request.duration")
                .with_description("Duration of outbound requests to Bandcamp.")
                .with_unit("s")
                .with_boundaries(DURATION_BUCKETS.to_vec())
                .build(),
            rate_limiter_wait: meter
                .f64_histogram("bandcamp.rate_limiter.wait.duration")
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
pub(crate) fn record_tool_call(tool: &'static str, outcome: &'static str) {
    instruments().tool_calls.add(
        1,
        &[
            KeyValue::new("mcp.tool.name", tool),
            KeyValue::new("outcome", outcome),
        ],
    );
}

/// Record the duration of one outbound Bandcamp request. `status` is the HTTP
/// response status, or `None` when the request never produced one.
pub(crate) fn record_request(
    method: &'static str,
    host: &str,
    status: Option<u16>,
    elapsed: Duration,
) {
    let mut attrs = vec![
        KeyValue::new("http.request.method", method),
        KeyValue::new("server.address", host.to_string()),
    ];
    match status {
        Some(code) => attrs.push(KeyValue::new("http.response.status_code", i64::from(code))),
        None => attrs.push(KeyValue::new("error.type", "request_failed")),
    }
    instruments()
        .request_duration
        .record(elapsed.as_secs_f64(), &attrs);
}

/// Record how long a request waited for a rate-limiter slot.
pub(crate) fn record_rate_limiter_wait(elapsed: Duration) {
    instruments()
        .rate_limiter_wait
        .record(elapsed.as_secs_f64(), &[]);
}
