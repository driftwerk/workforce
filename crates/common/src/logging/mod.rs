use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};
use eyre::Result;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::Subscriber;
use tracing_subscriber::layer::Context;
use tracing_subscriber::{fmt, layer::SubscriberExt, reload, util::SubscriberInitExt, EnvFilter};

#[cfg(feature = "prometheus")]
use metrics_exporter_prometheus::PrometheusHandle;

mod datadog;

/// Handle for dynamically changing the log filter at runtime.
///
/// Used by the TUI to suppress log output while the alternate screen is active.
pub static LOG_RELOAD_HANDLE: OnceLock<reload::Handle<EnvFilter, tracing_subscriber::Registry>> =
    OnceLock::new();

/// Global log buffer accessible by the `system.logs` tool.
pub static LOG_BUFFER: OnceLock<Arc<SharedLogBuffer>> = OnceLock::new();

/// Global Prometheus handle. Populated by [`setup_logging`] when the
/// `prometheus` feature is enabled and a recorder install succeeds. The
/// hub's `/metrics` HTTP route renders this on demand.
#[cfg(feature = "prometheus")]
pub static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Sentry client guard. Held for the process lifetime so events flush
/// on Drop. Inert when SENTRY_DSN is unset.
#[cfg(feature = "sentry")]
static SENTRY_GUARD: OnceLock<sentry::ClientInitGuard> = OnceLock::new();

/// OTel providers held for the process lifetime so background batch
/// exporters keep flushing. Only set when OTEL_EXPORTER_OTLP_ENDPOINT was
/// configured at init time.
#[cfg(feature = "otel")]
static OTEL_PROVIDERS: OnceLock<OtelProviders> = OnceLock::new();

// ─── Metric label policy ─────────────────────────────────────────────────────
//
// Each workforce install is single-tenant, so there is no `tenant_id` to
// label by. Allowed (bounded) labels for `metrics::*` calls:
//
//   - `agent`         — AgentId slug (e.g. "developer"). Bounded by the
//                       agent registry per install (typically <50).
//   - `user_identity` — Identity.display_name slug. Bounded by team size.
//   - `user_role`     — IdentityRole as_str() ("system_admin"|"manager"|"member").
//   - `model`         — LLM model slug (e.g. "claude-sonnet-4-6"). Bounded.
//   - `outcome`       — "ok" | "err" | "timeout" | "rate_limited" etc.
//   - `endpoint`      — HTTP route (must be the matched pattern, NOT the raw URL).
//   - `class`, `kind` — bounded enums chosen at the call site.
//
// Forbidden labels (unbounded — cause Prometheus/Datadog series explosions
// and per-series billing on Datadog):
//
//   - raw UUIDs, session_id, task_id, run_id, trace_id, request_id
//
// If you need per-session/per-request visibility, emit a `tracing::info!`
// log line — logs are designed for high-cardinality search, metrics aren't.

// ─── Log buffer ──────────────────────────────────────────────────────────────

/// A single captured log entry.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Thread-safe ring buffer of recent log entries.
pub struct SharedLogBuffer {
    entries: RwLock<VecDeque<LogEntry>>,
    capacity: usize,
}

impl SharedLogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if at capacity.
    pub fn push(&self, entry: LogEntry) {
        let mut buf = self.entries.write();
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Return the most recent `n` entries (newest last).
    pub fn read_last(&self, n: usize) -> Vec<LogEntry> {
        let buf = self.entries.read();
        let start = buf.len().saturating_sub(n);
        buf.range(start..).cloned().collect()
    }

    /// Filtered search across the buffer.
    pub fn search(
        &self,
        level: Option<&str>,
        target: Option<&str>,
        keyword: Option<&str>,
        since: Option<DateTime<Utc>>,
        limit: usize,
    ) -> Vec<LogEntry> {
        let buf = self.entries.read();
        buf.iter()
            .rev()
            .filter(|e| {
                if let Some(l) = level {
                    if !e.level.eq_ignore_ascii_case(l) {
                        return false;
                    }
                }
                if let Some(t) = target {
                    if !e.target.contains(t) {
                        return false;
                    }
                }
                if let Some(kw) = keyword {
                    if !e.message.to_lowercase().contains(&kw.to_lowercase()) {
                        return false;
                    }
                }
                if let Some(ts) = since {
                    if e.timestamp < ts {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

// ─── Tracing layer ──────────────────────────────────────────────────────────

/// Tracing layer that captures events into a `SharedLogBuffer`.
struct BufferLayer {
    buffer: Arc<SharedLogBuffer>,
}

/// Visitor that extracts the `message` field from a tracing event.
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else if self.message.is_empty() {
            // Capture first field as message fallback
        } else {
            // Append extra fields to message for context
            self.message
                .push_str(&format!(" {}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.message
                .push_str(&format!(" {}={}", field.name(), value));
        }
    }
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for BufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();

        let mut visitor = MessageVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);

        self.buffer.push(LogEntry {
            timestamp: Utc::now(),
            level: metadata.level().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message,
        });
    }
}

// ─── Setup ───────────────────────────────────────────────────────────────────

const DEFAULT_FILTER: &str = "workforce_core=info,workforce_common=info,workforce_nats=info,workforce_policy=info,workforce_agent=info,workforce::docs=info,warn";

/// Targets pinned below their natural level regardless of the global filter.
/// These are high-volume diagnostic streams whose data lives elsewhere (e.g.
/// `memory_metrics` is mirrored to Prometheus gauges and scraped), so a blunt
/// `RUST_LOG=info` must not resurface them. An explicit per-target directive in
/// `RUST_LOG` still wins — the suppression is only applied when absent.
const BASELINE_SUPPRESSIONS: &[(&str, &str)] = &[("memory_metrics", "warn")];

/// Resolve the filter string from the environment with a clear precedence:
/// `RUST_LOG` (full filter) → `WORKFORCE_LOG_LEVEL` (global level prepended to
/// the curated per-target defaults) → [`DEFAULT_FILTER`].
fn resolve_filter_string(rust_log: Option<&str>, log_level: Option<&str>) -> String {
    match (rust_log, log_level) {
        (Some(filter), _) if !filter.is_empty() => filter.to_owned(),
        (_, Some(level)) if !level.is_empty() => format!("{level},{DEFAULT_FILTER}"),
        _ => DEFAULT_FILTER.to_owned(),
    }
}

/// Append the baseline suppressions that the resolved filter does not already
/// name, so they survive a coarse global level without overriding an explicit
/// operator choice.
fn with_baseline_suppressions(raw: &str, filter: EnvFilter) -> EnvFilter {
    BASELINE_SUPPRESSIONS
        .iter()
        .filter(|(target, _)| !raw.contains(target))
        .fold(filter, |filter, (target, level)| {
            filter.add_directive(
                format!("{target}={level}")
                    .parse()
                    .expect("baseline suppression directive is a valid constant"),
            )
        })
}

fn default_env_filter() -> EnvFilter {
    let raw = resolve_filter_string(
        std::env::var("RUST_LOG").ok().as_deref(),
        std::env::var("WORKFORCE_LOG_LEVEL").ok().as_deref(),
    );
    with_baseline_suppressions(&raw, EnvFilter::new(&raw))
}

/// Initialise tracing with sensible defaults plus optional production-grade
/// telemetry export.
///
/// Always-on:
///   - [`EnvFilter`] resolved as `RUST_LOG` → `WORKFORCE_LOG_LEVEL` →
///     [`DEFAULT_FILTER`], with [`BASELINE_SUPPRESSIONS`] pinned on top so a
///     coarse global level cannot resurface high-volume diagnostic targets.
///   - In-memory [`SharedLogBuffer`] for the `system.logs` tool
///   - [`reload::Handle`] so the TUI can suppress/restore output
///
/// Conditionally enabled at runtime via env vars (only when the matching
/// Cargo feature is compiled in):
///
///   - `WORKFORCE_LOG_FORMAT=json` — emit Datadog-native JSON (see
///     [`datadog`]) instead of pretty fmt. Required for structured-field
///     parsing by the Datadog Agent / Cloud Logging.
///   - `OTEL_EXPORTER_OTLP_ENDPOINT` — feature `otel`. Exports traces and
///     logs over OTLP gRPC (Tempo/Cloud Trace/Datadog Agent OTLP receiver).
///     `OTEL_TRACES_SAMPLER_ARG` (default 1.0) controls trace sampling.
///     Resource attrs come from `WORKFORCE_SERVICE_NAME` (default
///     "workforce-hub"), `WORKFORCE_DEPLOYMENT_ENV`, `CARGO_PKG_VERSION`,
///     and `WORKFORCE_INSTANCE_ID` (defaults to hostname).
///   - `SENTRY_DSN` — feature `sentry`. Initialises the Sentry client and
///     adds the sentry-tracing capture layer.
///   - feature `prometheus` — installs the global `metrics` recorder so
///     calls like `metrics::counter!(...)` from anywhere in the workspace
///     accumulate into a renderable [`PrometheusHandle`] stored in
///     [`PROMETHEUS_HANDLE`]. The hub serves it on `/metrics`.
pub fn setup_logging() -> Result<()> {
    // Sentry must initialise before the capture layer is registered so
    // the global client exists when sentry-tracing tries to dispatch.
    #[cfg(feature = "sentry")]
    init_sentry();

    #[cfg(feature = "prometheus")]
    install_prometheus_recorder();

    let (filter_layer, reload_handle) = reload::Layer::new(default_env_filter());
    let buffer = Arc::new(SharedLogBuffer::new(5000));
    let _ = LOG_BUFFER.set(Arc::clone(&buffer));

    let registry = tracing_subscriber::registry()
        .with(filter_layer)
        .with(BufferLayer { buffer })
        .with(fmt_layer());

    #[cfg(feature = "sentry")]
    let registry = registry.with(sentry_tracing::layer());

    // OTLP layers are `Option<L>`: the blanket `impl Layer<S> for Option<L>`
    // no-ops when None, which keeps the registry's compile-time type stable
    // whether or not OTEL_EXPORTER_OTLP_ENDPOINT is set.
    #[cfg(feature = "otel")]
    let otel_providers: Option<OtelProviders> = init_otel().unwrap_or_else(|e| {
        eprintln!("warning: OTLP telemetry init failed, continuing without export: {e}");
        None
    });
    #[cfg(feature = "otel")]
    let registry = registry
        .with(
            otel_providers
                .as_ref()
                .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer.clone())),
        )
        .with(otel_providers.as_ref().map(|p| {
            opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&p.log_provider)
        }));
    #[cfg(feature = "otel")]
    if let Some(p) = otel_providers {
        let _ = OTEL_PROVIDERS.set(p);
    }

    registry
        .try_init()
        .map_err(|e| eyre::eyre!("Failed to initialise tracing: {}", e))?;

    let _ = LOG_RELOAD_HANDLE.set(reload_handle);
    Ok(())
}

fn fmt_layer<S>() -> Box<dyn tracing_subscriber::Layer<S> + Send + Sync>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    if json_mode_from_env(std::env::var("WORKFORCE_LOG_FORMAT").ok().as_deref()) {
        Box::new(
            fmt::layer()
                .fmt_fields(datadog::json_fields())
                .event_format(datadog::DatadogJson),
        )
    } else {
        Box::new(fmt::layer().with_target(true).with_thread_ids(false))
    }
}

fn json_mode_from_env(raw: Option<&str>) -> bool {
    matches!(raw, Some("json") | Some("JSON"))
}

#[cfg(feature = "sentry")]
fn init_sentry() {
    let dsn = match std::env::var("SENTRY_DSN") {
        Ok(v) if !v.is_empty() => v,
        _ => return,
    };
    let guard = sentry::init((
        dsn,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            environment: std::env::var("WORKFORCE_DEPLOYMENT_ENV")
                .ok()
                .filter(|v| !v.is_empty())
                .map(Into::into),
            ..Default::default()
        },
    ));
    let _ = SENTRY_GUARD.set(guard);
}

#[cfg(feature = "prometheus")]
fn install_prometheus_recorder() {
    match metrics_exporter_prometheus::PrometheusBuilder::new().install_recorder() {
        Ok(handle) => {
            let _ = PROMETHEUS_HANDLE.set(handle);
        }
        Err(e) => eprintln!("warning: failed to install Prometheus recorder: {e}"),
    }
}

#[cfg(feature = "otel")]
struct OtelProviders {
    tracer: opentelemetry_sdk::trace::Tracer,
    log_provider: opentelemetry_sdk::logs::SdkLoggerProvider,
    // Keep providers alive for graceful shutdown on Drop.
    _trace_provider: opentelemetry_sdk::trace::SdkTracerProvider,
}

#[cfg(feature = "otel")]
fn init_otel() -> Result<Option<OtelProviders>> {
    use opentelemetry::trace::TracerProvider as _;

    if std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        return Ok(None);
    }

    let resource = build_resource();
    let sampler = parse_traces_sampler();

    let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()
        .map_err(|e| eyre::eyre!("OTLP span exporter build failed: {e}"))?;

    let trace_provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(trace_exporter)
        .with_sampler(sampler)
        .with_resource(resource.clone())
        .build();
    let tracer = trace_provider.tracer("workforce-hub");

    let log_exporter = opentelemetry_otlp::LogExporter::builder()
        .with_tonic()
        .build()
        .map_err(|e| eyre::eyre!("OTLP log exporter build failed: {e}"))?;

    let log_provider = opentelemetry_sdk::logs::SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource)
        .build();

    opentelemetry::global::set_tracer_provider(trace_provider.clone());

    Ok(Some(OtelProviders {
        tracer,
        log_provider,
        _trace_provider: trace_provider,
    }))
}

#[cfg(feature = "otel")]
fn build_resource() -> opentelemetry_sdk::Resource {
    let service_name =
        std::env::var("WORKFORCE_SERVICE_NAME").unwrap_or_else(|_| "workforce-hub".to_string());
    let instance_id = std::env::var("WORKFORCE_INSTANCE_ID")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());

    let mut builder = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name)
        .with_attribute(opentelemetry::KeyValue::new(
            "service.version",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_attribute(opentelemetry::KeyValue::new(
            "service.instance.id",
            instance_id,
        ));
    if let Ok(env) = std::env::var("WORKFORCE_DEPLOYMENT_ENV") {
        if !env.is_empty() {
            builder =
                builder.with_attribute(opentelemetry::KeyValue::new("deployment.environment", env));
        }
    }
    builder.build()
}

/// Parse a sampler ratio from a raw env value, clamped to `[0.0, 1.0]`.
///
/// Out-of-range or unparseable input falls back to `1.0` (sample all).
/// Kept pure so tests can pin the parsing without env-var contamination.
#[cfg(any(test, feature = "otel"))]
fn parse_sampler_ratio(raw: Option<&str>) -> f64 {
    raw.and_then(|v| v.parse::<f64>().ok())
        .map(|r| r.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}

/// Build a parent-based trace-id-ratio sampler from `OTEL_TRACES_SAMPLER_ARG`.
///
/// We deliberately don't honour the full `OTEL_TRACES_SAMPLER` matrix —
/// the only ratio that matters in practice is `parentbased_traceidratio`,
/// and exposing the rest invites foot-gun configurations.
#[cfg(feature = "otel")]
fn parse_traces_sampler() -> opentelemetry_sdk::trace::Sampler {
    let ratio = parse_sampler_ratio(std::env::var("OTEL_TRACES_SAMPLER_ARG").ok().as_deref());
    opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
        opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(ratio),
    ))
}

/// Suppress all log output (used while TUI alternate screen is active).
pub fn suppress_logging() {
    if let Some(handle) = LOG_RELOAD_HANDLE.get() {
        let _ = handle.modify(|filter| *filter = EnvFilter::new("off"));
    }
}

/// Restore normal log output.
pub fn restore_logging() {
    if let Some(handle) = LOG_RELOAD_HANDLE.get() {
        let _ = handle.modify(|f| *f = default_env_filter());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_filter_precedence() {
        assert_eq!(
            resolve_filter_string(Some("debug,hyper=warn"), Some("info")),
            "debug,hyper=warn"
        );
        assert_eq!(
            resolve_filter_string(None, Some("debug")),
            format!("debug,{DEFAULT_FILTER}")
        );
        assert_eq!(resolve_filter_string(Some(""), Some("")), DEFAULT_FILTER);
        assert_eq!(resolve_filter_string(None, None), DEFAULT_FILTER);
    }

    #[test]
    fn baseline_suppression_added_when_absent() {
        let rendered = with_baseline_suppressions("info", EnvFilter::new("info")).to_string();
        assert!(rendered.contains("memory_metrics=warn"), "{rendered}");
    }

    #[test]
    fn baseline_suppression_yields_to_explicit_directive() {
        let raw = "info,memory_metrics=debug";
        let rendered = with_baseline_suppressions(raw, EnvFilter::new(raw)).to_string();
        assert!(rendered.contains("memory_metrics=debug"), "{rendered}");
        assert!(!rendered.contains("memory_metrics=warn"), "{rendered}");
    }

    #[test]
    fn buffer_push_and_read() {
        let buf = SharedLogBuffer::new(3);
        for i in 0..5 {
            buf.push(LogEntry {
                timestamp: Utc::now(),
                level: "INFO".into(),
                target: "test".into(),
                message: format!("msg {}", i),
            });
        }
        let entries = buf.read_last(10);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].message, "msg 2");
        assert_eq!(entries[2].message, "msg 4");
    }

    #[test]
    fn buffer_search_by_level() {
        let buf = SharedLogBuffer::new(100);
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "test".into(),
            message: "info msg".into(),
        });
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "ERROR".into(),
            target: "test".into(),
            message: "error msg".into(),
        });
        let results = buf.search(Some("error"), None, None, None, 100);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message, "error msg");
    }

    #[test]
    fn buffer_search_by_keyword() {
        let buf = SharedLogBuffer::new(100);
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "scheduler".into(),
            message: "Job fired successfully".into(),
        });
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "serve".into(),
            message: "Request handled".into(),
        });
        let results = buf.search(None, None, Some("job"), None, 100);
        assert_eq!(results.len(), 1);
        assert!(results[0].message.contains("Job"));
    }

    #[test]
    fn buffer_search_by_target() {
        let buf = SharedLogBuffer::new(100);
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "workforce_hub::scheduler".into(),
            message: "tick".into(),
        });
        buf.push(LogEntry {
            timestamp: Utc::now(),
            level: "INFO".into(),
            target: "workforce_hub::serve".into(),
            message: "request".into(),
        });
        let results = buf.search(None, Some("scheduler"), None, None, 100);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].message, "tick");
    }

    #[test]
    fn json_mode_recognises_only_json_token() {
        assert!(json_mode_from_env(Some("json")));
        assert!(json_mode_from_env(Some("JSON")));
        assert!(!json_mode_from_env(Some("Json")));
        assert!(!json_mode_from_env(Some("pretty")));
        assert!(!json_mode_from_env(Some("")));
        assert!(!json_mode_from_env(None));
    }

    #[test]
    fn sampler_ratio_defaults_to_one() {
        assert_eq!(parse_sampler_ratio(None), 1.0);
        assert_eq!(parse_sampler_ratio(Some("")), 1.0);
        assert_eq!(parse_sampler_ratio(Some("not-a-number")), 1.0);
    }

    #[test]
    fn sampler_ratio_parses_in_range() {
        assert_eq!(parse_sampler_ratio(Some("0.0")), 0.0);
        assert_eq!(parse_sampler_ratio(Some("0.25")), 0.25);
        assert_eq!(parse_sampler_ratio(Some("1.0")), 1.0);
    }

    #[test]
    fn sampler_ratio_clamps_out_of_range() {
        assert_eq!(parse_sampler_ratio(Some("-0.5")), 0.0);
        assert_eq!(parse_sampler_ratio(Some("1.5")), 1.0);
        assert_eq!(parse_sampler_ratio(Some("100")), 1.0);
    }
}
