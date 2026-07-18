//! OpenTelemetry trace export (OTLP/gRPC).
//!
//! Askr owns the whole request boundary, so it can export a span per request that
//! splits the time Octane/FPM are blind to: a root `http.request` span with a
//! child `php.execute` span. Point `ASKR_OTEL_ENDPOINT` at an OTLP collector
//! (Jaeger, Tempo, the OTel Collector) and see, per request, exactly how much was
//! PHP vs everything else.
//!
//! Env:
//!   ASKR_OTEL_ENDPOINT   otlp gRPC endpoint, e.g. http://127.0.0.1:4317 (enables)
//!   ASKR_OTEL_SERVICE    service.name resource attribute (default "askr")
//!
//! Off by default and behind `--features otel`.

use std::time::{Duration, SystemTime};

use opentelemetry::global;
use opentelemetry::trace::{Span, SpanKind, TraceContextExt, Tracer, TracerProvider as _};
use opentelemetry::{Context, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::TracerProvider as SdkTracerProvider;
use opentelemetry_sdk::Resource;

/// A child span within a request: a named phase with a start offset (from request
/// start) and a duration.
pub struct Phase {
    pub name: &'static str,
    pub offset: Duration,
    pub dur: Duration,
}

/// A finished request, reconstructed into a root span + child phase spans.
pub struct RequestSpan {
    pub method: String,
    pub path: String,
    pub status: u16,
    /// Wall-clock time the request started.
    pub start_wall: SystemTime,
    /// Total request duration.
    pub total: Duration,
    /// Cache disposition: "HIT" | "MISS" | "STALE" | "".
    pub cache: &'static str,
    /// Bytes written to the client.
    pub bytes: u64,
    /// Negotiated protocol: "1.1" | "2" | "3".
    pub proto: &'static str,
    /// URL query string (without the leading `?`); empty if none.
    pub query: String,
    /// Child spans (php.execute, request.read, response.build, …), in any order.
    pub phases: Vec<Phase>,
}

pub struct Otel {
    tracer: opentelemetry_sdk::trace::Tracer,
    _provider: SdkTracerProvider,
}

impl Otel {
    /// Build the exporter/provider from the environment, or `None` if
    /// `ASKR_OTEL_ENDPOINT` is unset. Call inside the Tokio runtime.
    pub fn from_env() -> Option<Otel> {
        let endpoint = std::env::var("ASKR_OTEL_ENDPOINT")
            .ok()
            .filter(|s| !s.is_empty())?;
        let service = std::env::var("ASKR_OTEL_SERVICE").unwrap_or_else(|_| "askr".into());

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .map_err(|e| tracing::warn!(error = %e, "otel: exporter build failed"))
            .ok()?;

        let resource = Resource::new([KeyValue::new("service.name", service)]);
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter, opentelemetry_sdk::runtime::Tokio)
            .with_resource(resource)
            .build();
        let tracer = provider.tracer("askr");
        global::set_tracer_provider(provider.clone());
        tracing::info!("otel: OTLP trace export enabled");
        Some(Otel {
            tracer,
            _provider: provider,
        })
    }

    /// Export one request as a root `http.request` span plus a child span per
    /// measured phase (php.execute, cache.lookup, response.build, …), timed from
    /// the reconstructed wall-clock windows.
    pub fn record(&self, r: RequestSpan) {
        let end = r.start_wall + r.total;
        let mut attrs = vec![
            KeyValue::new("http.request.method", r.method),
            KeyValue::new("url.path", r.path),
            KeyValue::new("http.response.status_code", r.status as i64),
            KeyValue::new("http.response.body.size", r.bytes as i64),
            KeyValue::new("network.protocol.version", r.proto),
            KeyValue::new("askr.cache", r.cache),
        ];
        if !r.query.is_empty() {
            attrs.push(KeyValue::new("url.query", r.query));
        }
        let root = self
            .tracer
            .span_builder("http.request")
            .with_kind(SpanKind::Server)
            .with_start_time(r.start_wall)
            .with_attributes(attrs)
            .start(&self.tracer);

        let cx = Context::current_with_span(root);

        for p in r.phases {
            if p.dur.is_zero() {
                continue;
            }
            let start = r.start_wall + p.offset;
            let mut span = self
                .tracer
                .span_builder(p.name)
                .with_start_time(start)
                .start_with_context(&self.tracer, &cx);
            span.end_with_timestamp(start + p.dur);
        }

        cx.span().end_with_timestamp(end);
    }
}
