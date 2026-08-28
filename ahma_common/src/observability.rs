//! Shared OpenTelemetry observability configuration and initialization.
//!
//! Provides a single source of truth for tracing/metrics setup used by
//! both `ahma_mcp` and `ahma_http_bridge`.
//!
//! ## Design
//!
//! Tracing is opt-in: pass `--opentelemetry <endpoint>` (or set
//! `OTEL_EXPORTER_OTLP_ENDPOINT`) to activate export.  When neither is
//! provided the pipeline uses a no-op tracer — zero runtime overhead.
//!
//! The returned [`TelemetryGuard`] **must** be kept alive until the process is
//! ready to shut down. Dropping it triggers a synchronous flush of any buffered
//! spans/metrics before the exporter is torn down.
//!
//! ## Environment Variables
//!
//! | Variable | Description |
//! |---|---|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP endpoint URL; its presence enables tracing |
//! | `OTEL_SERVICE_NAME` | Service name attached to all exported telemetry |
//! | `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout (handled by SDK) |

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Runtime configuration for distributed tracing export.
///
/// Build via [`ObservabilityConfig::from_env`] early in startup (before the
/// full CLI is parsed), then optionally apply CLI override with
/// [`ObservabilityConfig::with_endpoint`].
#[derive(Debug, Clone, PartialEq)]
pub struct ObservabilityConfig {
    /// OTLP exporter base endpoint (e.g. `http://localhost:4318`).
    /// `None` means tracing is disabled — a no-op tracer is used.
    pub endpoint: Option<String>,
    /// Service name attached to all exported telemetry.
    pub service_name: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            service_name: "ahma".to_string(),
        }
    }
}

impl ObservabilityConfig {
    /// Build configuration from standard environment variables.
    ///
    /// Tracing is enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
    /// The service name falls back to the `service_name` argument unless
    /// `OTEL_SERVICE_NAME` is present.
    pub fn from_env(service_name: &str) -> Self {
        let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        let resolved_service =
            std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| service_name.to_string());
        Self {
            endpoint,
            service_name: resolved_service,
        }
    }

    /// Override the OTLP endpoint.  Providing `Some(url)` enables tracing;
    /// `None` leaves the current setting unchanged.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: Option<&str>) -> Self {
        if let Some(ep) = endpoint {
            self.endpoint = Some(ep.to_string());
        }
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TelemetryGuard — always-defined lifecycle wrapper
// ─────────────────────────────────────────────────────────────────────────────

/// Opaque handle that keeps the OTEL provider pipeline alive until dropped.
///
/// On drop this flushes buffered spans/metrics and shuts down the exporter.
/// The caller must keep this value alive for the full duration of the
/// application (typically stored in `main()` or the top-level `run()` fn).
///
/// When no endpoint is configured this is a zero-cost no-op.
pub struct TelemetryGuard {
    /// Type-erased inner guard; holds `ObservabilityGuard` when OTEL is active.
    _inner: Option<Box<dyn std::any::Any + Send>>,
}

impl TelemetryGuard {
    /// Create a no-op guard (returned when observability is disabled).
    pub fn none() -> Self {
        Self { _inner: None }
    }

    fn with_guard(guard: ObservabilityGuard) -> Self {
        Self {
            _inner: Some(Box::new(guard)),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// OTEL initialization
// ─────────────────────────────────────────────────────────────────────────────

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    Resource,
    trace::{self as sdktrace, SdkTracerProvider},
};

/// Internal guard that holds the tracer provider and shuts it down on drop.
struct ObservabilityGuard {
    tracer_provider: SdkTracerProvider,
}

impl Drop for ObservabilityGuard {
    fn drop(&mut self) {
        if let Err(e) = self.tracer_provider.shutdown() {
            // tracing subscriber may already be torn down at this point;
            // write directly to stderr to avoid losing the message.
            eprintln!("[ahma] OTEL tracer provider shutdown error: {e}");
        }
    }
}

/// Build an OpenTelemetry tracing layer and start exporting spans.
///
/// Returns `(None, no-op guard)` when `config.endpoint` is `None`, or when
/// the OTLP exporter cannot be built (error is printed to stderr rather than
/// propagating so that the server always starts, just without telemetry).
///
/// The caller **must** keep the returned [`TelemetryGuard`] alive until
/// shutdown to ensure all buffered spans are flushed.
///
/// The layer is generic over the subscriber type `S` to compose cleanly with
/// [`tracing_subscriber::registry()`] chains.
pub fn create_otel_layer<S>(
    config: &ObservabilityConfig,
) -> (
    Option<tracing_opentelemetry::OpenTelemetryLayer<S, sdktrace::Tracer>>,
    TelemetryGuard,
)
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let endpoint = match &config.endpoint {
        Some(ep) => ep.clone(),
        None => return (None, TelemetryGuard::none()),
    };

    let traces_endpoint = format!("{}/v1/traces", endpoint.trim_end_matches('/'));

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(&traces_endpoint)
        .build()
    {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "[ahma] OTEL: failed to build span exporter (endpoint={traces_endpoint:?}): {err}"
            );
            return (None, TelemetryGuard::none());
        }
    };

    let resource = Resource::builder()
        .with_service_name(config.service_name.clone())
        .build();

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();

    // Register as global so `opentelemetry::global::tracer()` works everywhere.
    opentelemetry::global::set_tracer_provider(provider.clone());

    let tracer = provider.tracer(config.service_name.clone());
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    let guard = TelemetryGuard::with_guard(ObservabilityGuard {
        tracer_provider: provider,
    });

    (Some(layer), guard)
}

// ─────────────────────────────────────────────────────────────────────────────
// Cross-process context propagation helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Extract the W3C `traceparent` string from the current tracing span.
///
/// Returns `None` when no active span exists or the span does not carry
/// a valid OTEL `SpanContext`.
///
/// The returned string is ready to be injected as the `TRACEPARENT` environment
/// variable into a child process so that it can resume the trace.
pub fn current_traceparent() -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span = tracing::Span::current();
    let ctx = span.context();
    let span_ref = ctx.span();
    let span_ctx = span_ref.span_context();
    if span_ctx.is_valid() {
        Some(format!(
            "00-{}-{}-{:02x}",
            span_ctx.trace_id(),
            span_ctx.span_id(),
            span_ctx.trace_flags().to_u8()
        ))
    } else {
        None
    }
}

/// Read `TRACEPARENT` from the process environment (set by the HTTP bridge when
/// spawning this subprocess).  Returns the raw W3C traceparent string, if present.
///
/// The caller is responsible for using it to create a linked/child span, e.g. by
/// passing it to `OpenTelemetry` propagation APIs.
pub fn env_traceparent() -> Option<String> {
    std::env::var("TRACEPARENT")
        .ok()
        .filter(|s| s.starts_with("00-") && s.len() >= 55)
}

// ─────────────────────────────────────────────────────────────────────────────
// Metrics helpers — available whenever opentelemetry base is compiled
// ─────────────────────────────────────────────────────────────────────────────

/// Record a tool call outcome in the global OTEL meter.
///
/// When no meter provider is registered (observability disabled) this is a
/// no-op; no overhead is incurred on the hot path.
pub fn record_tool_call(tool_name: &str, outcome: ToolCallOutcome, duration_ms: u64) {
    use opentelemetry::{KeyValue, global};

    let meter = global::meter("ahma");

    meter.u64_counter("ahma.tool.calls").build().add(
        1,
        &[
            KeyValue::new("tool_name", tool_name.to_string()),
            KeyValue::new("outcome", outcome.as_str()),
        ],
    );

    meter.u64_histogram("ahma.tool.duration_ms").build().record(
        duration_ms,
        &[KeyValue::new("tool_name", tool_name.to_string())],
    );
}

/// Record a sandbox gating failure (tool call rejected before sandbox is ready).
pub fn record_sandbox_gating_failure() {
    use opentelemetry::global;
    global::meter("ahma")
        .u64_counter("ahma.sandbox.gating_failures")
        .build()
        .add(1, &[]);
}

/// Possible outcomes for a tool call, used as a metric attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallOutcome {
    /// Tool completed without error.
    Success,
    /// Tool returned an application-level error.
    Error,
    /// Tool was cancelled before completion.
    Cancelled,
    /// Tool timed out.
    Timeout,
}

impl ToolCallOutcome {
    /// Stable string representation for use as a metric/span attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_disabled() {
        let cfg = ObservabilityConfig::default();
        assert!(cfg.endpoint.is_none());
        assert_eq!(cfg.service_name, "ahma");
    }

    #[test]
    fn from_env_disabled_when_no_vars_set() {
        // Only run when OTEL_EXPORTER_OTLP_ENDPOINT is absent in the test environment.
        if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_err() {
            let cfg = ObservabilityConfig::from_env("test-service");
            assert!(cfg.endpoint.is_none());
            assert_eq!(cfg.service_name, "test-service");
        }
    }

    #[test]
    fn with_endpoint_enables_tracing() {
        let cfg = ObservabilityConfig::default().with_endpoint(Some("http://custom:4317"));
        assert_eq!(cfg.endpoint.as_deref(), Some("http://custom:4317"));
    }

    #[test]
    fn with_endpoint_none_leaves_disabled() {
        let cfg = ObservabilityConfig::default().with_endpoint(None);
        assert!(cfg.endpoint.is_none());
    }

    #[test]
    fn tool_call_outcome_as_str() {
        assert_eq!(ToolCallOutcome::Success.as_str(), "success");
        assert_eq!(ToolCallOutcome::Error.as_str(), "error");
        assert_eq!(ToolCallOutcome::Cancelled.as_str(), "cancelled");
        assert_eq!(ToolCallOutcome::Timeout.as_str(), "timeout");
    }

    #[test]
    fn telemetry_guard_none_is_cheap() {
        let _guard = TelemetryGuard::none(); // should be a no-op, trivially droppable
    }

    use parking_lot::Mutex;
    use std::sync::LazyLock;

    /// Serializes every test that reads or writes process environment variables,
    /// since env is process-global and these tests run in the same binary.
    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// Save the current value of an env var, returning a closure-free snapshot.
    fn snapshot(key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    /// Restore a previously snapshotted env var (set or remove as appropriate).
    fn restore(key: &str, prior: Option<String>) {
        match prior {
            // SAFETY: env mutation is serialized by ENV_MUTEX in every test that mutates env.
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn from_env_service_name_overridden_by_env_var() {
        let _g = ENV_MUTEX.lock();
        let prior_name = snapshot("OTEL_SERVICE_NAME");
        let prior_endpoint = snapshot("OTEL_EXPORTER_OTLP_ENDPOINT");

        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var("OTEL_SERVICE_NAME", "from-env-service");
            std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        }

        let cfg = ObservabilityConfig::from_env("arg-service");
        // OTEL_SERVICE_NAME wins over the argument.
        assert_eq!(cfg.service_name, "from-env-service");
        // No endpoint set -> tracing disabled.
        assert!(cfg.endpoint.is_none());

        restore("OTEL_SERVICE_NAME", prior_name);
        restore("OTEL_EXPORTER_OTLP_ENDPOINT", prior_endpoint);
    }

    #[test]
    fn from_env_endpoint_enables_tracing() {
        let _g = ENV_MUTEX.lock();
        let prior_name = snapshot("OTEL_SERVICE_NAME");
        let prior_endpoint = snapshot("OTEL_EXPORTER_OTLP_ENDPOINT");

        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::remove_var("OTEL_SERVICE_NAME");
            std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://localhost:4318");
        }

        let cfg = ObservabilityConfig::from_env("fallback-service");
        assert_eq!(cfg.endpoint.as_deref(), Some("http://localhost:4318"));
        // No OTEL_SERVICE_NAME -> falls back to the argument.
        assert_eq!(cfg.service_name, "fallback-service");

        restore("OTEL_SERVICE_NAME", prior_name);
        restore("OTEL_EXPORTER_OTLP_ENDPOINT", prior_endpoint);
    }

    #[test]
    fn create_otel_layer_none_endpoint_returns_no_layer() {
        let cfg = ObservabilityConfig::default(); // endpoint: None
        let (layer, guard) = create_otel_layer::<tracing_subscriber::Registry>(&cfg);
        assert!(layer.is_none(), "no endpoint should yield no layer");
        // Dropping the none-guard must be a trivial no-op.
        drop(guard);
    }

    #[test]
    fn create_otel_layer_with_endpoint_builds_layer_and_guard() {
        // The batch HTTP exporter is constructed lazily and does NOT connect on
        // build, so this succeeds even without a live collector. This also
        // exercises ObservabilityGuard::drop() on teardown.
        let cfg = ObservabilityConfig {
            endpoint: Some("http://localhost:4318".to_string()),
            service_name: "otel-layer-test".to_string(),
        };
        let (layer, guard) = create_otel_layer::<tracing_subscriber::Registry>(&cfg);
        assert!(
            layer.is_some(),
            "a configured endpoint should produce an OTEL layer"
        );
        // Drop exercises ObservabilityGuard::drop -> tracer_provider.shutdown().
        drop(guard);
    }

    #[test]
    fn create_otel_layer_trims_trailing_slash_endpoint() {
        // Endpoint with a trailing slash must still build successfully.
        let cfg = ObservabilityConfig {
            endpoint: Some("http://localhost:4318/".to_string()),
            service_name: "trim-test".to_string(),
        };
        let (layer, guard) = create_otel_layer::<tracing_subscriber::Registry>(&cfg);
        assert!(layer.is_some());
        drop(guard);
    }

    #[test]
    fn env_traceparent_valid_value_is_returned() {
        let _g = ENV_MUTEX.lock();
        let prior = snapshot("TRACEPARENT");

        // Valid W3C traceparent: "00-" + 32 hex trace id + "-" + 16 hex span id
        // + "-" + 2 hex flags = exactly 55 chars.
        let valid = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
        assert_eq!(valid.len(), 55);
        // SAFETY: serialized by ENV_MUTEX.
        unsafe { std::env::set_var("TRACEPARENT", valid) };

        assert_eq!(env_traceparent().as_deref(), Some(valid));

        restore("TRACEPARENT", prior);
    }

    #[test]
    fn env_traceparent_too_short_is_none() {
        let _g = ENV_MUTEX.lock();
        let prior = snapshot("TRACEPARENT");

        // Starts with "00-" but well under 55 chars -> rejected.
        // SAFETY: serialized by ENV_MUTEX.
        unsafe { std::env::set_var("TRACEPARENT", "00-tooshort") };
        assert!(env_traceparent().is_none());

        // Long enough but wrong prefix -> also rejected.
        // SAFETY: serialized by ENV_MUTEX.
        unsafe {
            std::env::set_var(
                "TRACEPARENT",
                "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01",
            )
        };
        assert!(env_traceparent().is_none());

        restore("TRACEPARENT", prior);
    }

    #[test]
    fn env_traceparent_unset_is_none() {
        let _g = ENV_MUTEX.lock();
        let prior = snapshot("TRACEPARENT");

        // SAFETY: serialized by ENV_MUTEX.
        unsafe { std::env::remove_var("TRACEPARENT") };
        assert!(env_traceparent().is_none());

        restore("TRACEPARENT", prior);
    }

    #[test]
    fn current_traceparent_without_active_span_is_none() {
        // Outside any OTEL-instrumented span there is no valid span context.
        assert!(current_traceparent().is_none());
    }

    #[test]
    fn record_tool_call_is_noop_without_provider() {
        // No meter provider registered -> these must not panic for any outcome.
        for outcome in [
            ToolCallOutcome::Success,
            ToolCallOutcome::Error,
            ToolCallOutcome::Cancelled,
            ToolCallOutcome::Timeout,
        ] {
            record_tool_call("test_tool", outcome, 42);
        }
    }

    #[test]
    fn record_sandbox_gating_failure_is_noop_without_provider() {
        // No meter provider registered -> must not panic.
        record_sandbox_gating_failure();
    }
}
