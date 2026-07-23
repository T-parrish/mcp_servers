//! Telemetry setup: a stderr log layer plus, when configured, OpenTelemetry
//! OTLP export of spans and metrics.
//!
//! Export is enabled only when an OTLP endpoint is configured — either the
//! shared `OTEL_EXPORTER_OTLP_ENDPOINT` or a signal-specific
//! `OTEL_EXPORTER_OTLP_{TRACES,METRICS}_ENDPOINT` — so the default local
//! experience is unchanged. Endpoint, headers, timeout and the metric export
//! interval are read from the standard `OTEL_*` variables by the SDK itself.
//!
//! Note: MCP over stdio owns stdout for the protocol stream, so nothing here may
//! ever write to stdout — logs go to stderr, telemetry goes over the network.

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use tracing_subscriber::{
    EnvFilter, Layer, filter::LevelFilter, filter::Targets, layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::ServiceInfo;

/// Held for the lifetime of the process. Dropping it flushes buffered telemetry;
/// without that the batch processor and periodic reader discard whatever they
/// are still holding.
pub struct Telemetry {
    tracer_provider: Option<SdkTracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = self.meter_provider.take()
            && let Err(e) = provider.shutdown()
        {
            tracing::warn!(error = %e, "failed to shut down the OTLP metric exporter cleanly");
        }
        if let Some(provider) = self.tracer_provider.take()
            && let Err(e) = provider.shutdown()
        {
            tracing::warn!(error = %e, "failed to shut down the OTLP span exporter cleanly");
        }
    }
}

/// Whether an OTLP endpoint is configured for `signal`, either signal-specific
/// or shared.
fn endpoint_configured(signal: &str) -> bool {
    [
        format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT"),
        "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
    ]
    .iter()
    .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

/// Whether the operator has already supplied a `service.name`.
fn service_name_configured() -> bool {
    std::env::var("OTEL_SERVICE_NAME").is_ok_and(|v| !v.trim().is_empty())
        || std::env::var("OTEL_RESOURCE_ATTRIBUTES").is_ok_and(|v| v.contains("service.name="))
}

/// The resource describing this service, shared by both signals.
///
/// `Resource::builder` already picks up OTEL_SERVICE_NAME / OTEL_RESOURCE_ATTRIBUTES,
/// and setting the name explicitly would override them — so only fall back to the
/// server's own name when the operator has configured neither. That fallback is
/// what lets one shared `.env` serve several servers: each names itself.
fn resource(service: &ServiceInfo) -> Resource {
    let mut builder = Resource::builder().with_attribute(opentelemetry::KeyValue::new(
        opentelemetry_semantic_conventions::attribute::SERVICE_VERSION,
        service.crate_version,
    ));
    if !service_name_configured() {
        builder = builder.with_service_name(service.default_service_name);
    }
    builder.build()
}

/// Install the global subscriber and meter provider. Always logs to stderr;
/// additionally exports spans and metrics over OTLP when configured.
pub fn init(service: ServiceInfo) -> Result<Telemetry> {
    crate::metrics::configure(&service);

    // Level is controlled by RUST_LOG (default: info).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);

    let tracer_provider = if endpoint_configured("TRACES") {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .build()
            .context("building the OTLP span exporter")?;
        Some(
            SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .with_resource(resource(&service))
                .build(),
        )
    } else {
        None
    };

    // The OTel layer is only added when there is a tracer to feed it, so the
    // two arms produce different subscriber types; `Option<Layer>` unifies them.
    //
    // The target filter exports only the server crate's spans. Without it, rmcp's
    // `serve_inner` span — which lives for the whole stdio session — becomes the
    // root of every trace, so nothing is queryable until the client disconnects.
    // Excluding it makes each tool call its own complete trace, and also keeps
    // the SDK's own internal-log events from feeding back into the exporter.
    let otel_layer = tracer_provider.as_ref().map(|p| {
        tracing_opentelemetry::layer()
            .with_tracer(p.tracer(service.crate_name))
            .with_filter(Targets::new().with_target(service.crate_name, LevelFilter::TRACE))
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(otel_layer)
        .init();

    // Logging is live from here on, so these are the first messages that can be seen.
    match tracer_provider {
        Some(_) => tracing::info!("OTLP span export enabled"),
        None => tracing::debug!("no OTLP traces endpoint configured; span export disabled"),
    }

    let meter_provider = if endpoint_configured("METRICS") {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .build()
            .context("building the OTLP metric exporter")?;
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .with_resource(resource(&service))
            .build();
        // The instruments in `crate::metrics` resolve through the global provider.
        opentelemetry::global::set_meter_provider(provider.clone());
        tracing::info!("OTLP metric export enabled");
        Some(provider)
    } else {
        tracing::debug!("no OTLP metrics endpoint configured; metric export disabled");
        None
    };

    Ok(Telemetry {
        tracer_provider,
        meter_provider,
    })
}
