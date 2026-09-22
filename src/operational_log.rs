//! Versioned JSON formatting for the optional operational file sink.

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_events_escape_newlines_and_include_span_context() {
        let capture = Capture(Arc::new(Mutex::new(Vec::new())));
        let output = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .event_format(JsonOperationalFormat)
            .with_ansi(false)
            .with_writer(move || output.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("dispatch", orb_id = "orb-test");
            let _entered = span.enter();
            tracing::info!(attempt = 2_u64, "first\nsecond");
        });
        let bytes = capture.0.lock().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        let event: Value = serde_json::from_str(text).unwrap();
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["fields"]["attempt"], 2);
        assert_eq!(event["fields"]["message"], "first\nsecond");
        assert_eq!(event["spans"][0]["name"], "dispatch");
        assert!(event["spans"][0]["fields"]
            .as_str()
            .unwrap()
            .contains("orb-test"));
    }
}

struct EventFields(Map<String, Value>);

impl Visit for EventFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().into(), Value::String(format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0
            .insert(field.name().into(), Value::String(value.into()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().into(), value.into());
    }
}

pub struct JsonOperationalFormat;

impl<S, N> FormatEvent<S, N> for JsonOperationalFormat
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
        let mut fields = EventFields(Map::new());
        event.record(&mut fields);
        let mut spans = Vec::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let extensions = span.extensions();
                let fields = extensions
                    .get::<FormattedFields<N>>()
                    .map_or_else(String::new, ToString::to_string);
                spans.push(serde_json::json!({ "name": span.name(), "fields": fields }));
            }
        }
        let record = serde_json::json!({
            "schema_version": 1,
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "level": event.metadata().level().as_str(),
            "target": event.metadata().target(),
            "fields": fields.0,
            "spans": spans,
        });
        writeln!(writer, "{record}")
    }
}
