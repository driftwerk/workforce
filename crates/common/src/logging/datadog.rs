//! Datadog-native JSON event formatter.
//!
//! `tracing_subscriber`'s built-in JSON formatter nests the human message under
//! `fields.message` and emits `level`/`timestamp`, none of which Datadog's
//! default remappers recognise — so logs arrive as an unparsed blob. This
//! formatter emits the Datadog *reserved* attribute names directly
//! (`message`, `status`, `@timestamp`, `logger.name`), so a `source:base`
//! stream renders correctly with no log pipeline. Every other field — event
//! and active-span — is flattened to a top-level, searchable attribute.

use chrono::Utc;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::{JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

/// Event formatter that writes one Datadog-native JSON object per line.
pub(super) struct DatadogJson;

impl<S, N> FormatEvent<S, N> for DatadogJson
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let span_fields = ctx
            .event_scope()
            .into_iter()
            .flat_map(|scope| scope.from_root())
            .fold(Map::new(), |mut acc, span| {
                if let Some(formatted) = span.extensions().get::<FormattedFields<N>>() {
                    if let Ok(Value::Object(map)) = serde_json::from_str(&formatted.fields) {
                        acc.extend(map);
                    }
                }
                acc
            });

        let mut collector = FieldCollector::default();
        event.record(&mut collector);

        let meta = event.metadata();
        let thread = std::thread::current();
        let record = build_record(
            now_rfc3339_millis(),
            meta.level(),
            meta.target(),
            thread.name(),
            span_fields,
            collector.0,
        );

        let line = serde_json::to_string(&Value::Object(record)).map_err(|_| std::fmt::Error)?;
        writeln!(writer, "{line}")
    }
}

/// Assemble the final record. Pure (timestamp injected) so it is unit-testable
/// without a subscriber. Reserved keys are inserted last so a user field of the
/// same name can never clobber the Datadog semantics; the event's own message
/// already occupies `message` via the field collector.
fn build_record(
    timestamp: String,
    level: &Level,
    target: &str,
    thread_name: Option<&str>,
    span_fields: Map<String, Value>,
    event_fields: Map<String, Value>,
) -> Map<String, Value> {
    let mut record = span_fields;
    record.extend(event_fields);
    record
        .entry("message")
        .or_insert_with(|| Value::String(target.to_owned()));
    record.insert("@timestamp".into(), Value::String(timestamp));
    record.insert(
        "status".into(),
        Value::String(level.as_str().to_ascii_lowercase()),
    );
    record.insert("logger.name".into(), Value::String(target.to_owned()));
    if let Some(name) = thread_name {
        record.insert("thread.name".into(), Value::String(name.to_owned()));
    }
    record
}

fn now_rfc3339_millis() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Collects event fields into a JSON object, preserving native types so numeric
/// and boolean attributes stay queryable as such in Datadog.
#[derive(Default)]
struct FieldCollector(Map<String, Value>);

impl FieldCollector {
    fn put(&mut self, field: &Field, value: Value) {
        self.0.insert(field.name().to_owned(), value);
    }
}

impl Visit for FieldCollector {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.put(field, value.to_string().into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}").into());
    }
}

/// The JSON field formatter our span-field storage relies on. Exposed so the
/// layer wiring uses one canonical constructor.
pub(super) fn json_fields() -> JsonFields {
    JsonFields::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(fields: Map<String, Value>) -> Map<String, Value> {
        build_record(
            "2026-05-31T07:21:33.752Z".to_owned(),
            &Level::INFO,
            "workforce_hub::job_runner",
            Some("tokio-runtime-worker"),
            Map::new(),
            fields,
        )
    }

    #[test]
    fn formats_reserved_keys() {
        let mut fields = Map::new();
        fields.insert("message".into(), "hello".into());
        let r = record(fields);
        assert_eq!(r["@timestamp"], Value::from("2026-05-31T07:21:33.752Z"));
        assert!(r["@timestamp"].as_str().unwrap().ends_with('Z'));
        assert_eq!(r["status"], Value::from("info"));
        assert_eq!(r["message"], Value::from("hello"));
        assert_eq!(r["logger.name"], Value::from("workforce_hub::job_runner"));
        assert_eq!(r["thread.name"], Value::from("tokio-runtime-worker"));
    }

    #[test]
    fn flattens_event_fields_preserving_types() {
        let mut fields = Map::new();
        fields.insert("event".into(), "memory_metrics".into());
        fields.insert("sessions".into(), 3i64.into());
        fields.insert("ok".into(), true.into());
        let r = record(fields);
        assert_eq!(r["event"], Value::from("memory_metrics"));
        assert_eq!(r["sessions"], Value::from(3));
        assert!(r["sessions"].is_number());
        assert_eq!(r["ok"], Value::Bool(true));
    }

    #[test]
    fn reserved_keys_win_over_user_fields() {
        let mut fields = Map::new();
        fields.insert("status".into(), "totally-bogus".into());
        fields.insert("logger.name".into(), "spoofed".into());
        let r = record(fields);
        assert_eq!(r["status"], Value::from("info"));
        assert_eq!(r["logger.name"], Value::from("workforce_hub::job_runner"));
    }

    #[test]
    fn message_less_event_falls_back_to_target() {
        let r = build_record(
            "t".to_owned(),
            &Level::INFO,
            "memory.access_audit",
            None,
            Map::new(),
            Map::new(),
        );
        assert_eq!(r["message"], Value::from("memory.access_audit"));
    }

    #[test]
    fn explicit_message_is_not_overwritten_by_fallback() {
        let mut fields = Map::new();
        fields.insert("message".into(), "real message".into());
        let r = record(fields);
        assert_eq!(r["message"], Value::from("real message"));
    }

    #[test]
    fn level_is_lowercased() {
        let r = build_record(
            "t".to_owned(),
            &Level::WARN,
            "t",
            None,
            Map::new(),
            Map::new(),
        );
        assert_eq!(r["status"], Value::from("warn"));
        assert!(!r.contains_key("thread.name"));
    }

    #[test]
    fn event_fields_override_span_fields() {
        let mut span = Map::new();
        span.insert("agent".into(), "atlas".into());
        span.insert("turn".into(), 1i64.into());
        let mut event = Map::new();
        event.insert("agent".into(), "bernard".into());
        let r = build_record("t".to_owned(), &Level::INFO, "t", None, span, event);
        assert_eq!(r["agent"], Value::from("bernard"));
        assert_eq!(r["turn"], Value::from(1));
    }

    #[test]
    fn end_to_end_emits_datadog_native_line() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct VecWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for VecWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for VecWriter {
            type Writer = VecWriter;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let sink = VecWriter::default();
        let layer = tracing_subscriber::fmt::layer()
            .fmt_fields(json_fields())
            .event_format(DatadogJson)
            .with_writer(sink.clone());
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("turn", agent = "atlas");
            let _g = span.enter();
            tracing::info!(target: "test_logger", region = "NORTH", count = 3, "run log for {}", "NORTH");
        });

        let raw = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        let v: Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(v["message"], Value::from("run log for NORTH"));
        assert_eq!(v["status"], Value::from("info"));
        assert_eq!(v["logger.name"], Value::from("test_logger"));
        assert_eq!(v["region"], Value::from("NORTH"));
        assert_eq!(v["count"], Value::from(3));
        assert_eq!(v["agent"], Value::from("atlas"));
        assert!(v["@timestamp"].as_str().unwrap().ends_with('Z'));
    }
}
