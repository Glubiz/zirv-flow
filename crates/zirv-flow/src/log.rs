//! Structured logging for the long-running commands.

use std::io;

use tracing::{Dispatch, warn};
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::config::{Config, LogFormat};
use crate::otlp::{Otlp, ZirvTracer};

/// Installs the process-wide subscriber, writing to standard error. A second call is a
/// no-op, which only tests make. Returns the trace exporter when `[log.otlp]` is set; the
/// caller flushes it on the way out.
pub(crate) fn install(config: &Config) -> Option<Otlp> {
    let otlp = match Otlp::build(config.log_otlp_endpoint()) {
        Ok(otlp) => otlp,
        Err(error) => {
            let dispatch = dispatch(
                config.log_filter(),
                config.log_format(),
                io::stderr as fn() -> io::Stderr,
            );
            let _ = tracing::dispatcher::set_global_default(dispatch);
            warn!("trace export is off: {error}");
            return None;
        }
    };
    let dispatch = dispatch_with(
        config.log_filter(),
        config.log_format(),
        io::stderr as fn() -> io::Stderr,
        otlp.as_ref().map(Otlp::layer),
    );
    let _ = tracing::dispatcher::set_global_default(dispatch);
    otlp
}

/// A subscriber for `filter` and `format` that writes to `writer`. The filter was checked
/// when the configuration loaded; an unusable one falls back to `info`.
pub(crate) fn dispatch<W>(filter: &str, format: LogFormat, writer: W) -> Dispatch
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    dispatch_with(filter, format, writer, None)
}

/// [`dispatch`] with the trace-export layer next to the log layer, when there is one.
pub(crate) fn dispatch_with<W>(
    filter: &str,
    format: LogFormat,
    writer: W,
    export: Option<OpenTelemetryLayer<Registry, ZirvTracer>>,
) -> Dispatch
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);
    let layer = match format {
        LogFormat::Json => layer.json().with_span_list(false).boxed(),
        LogFormat::Text => layer.boxed(),
    };
    Dispatch::new(
        tracing_subscriber::registry()
            .with(export)
            .with(layer)
            .with(filter),
    )
}

/// Whether `filter` is a usable filter directive.
pub(crate) fn is_valid_filter(filter: &str) -> bool {
    EnvFilter::try_new(filter).is_ok()
}

/// A writer that keeps what it is given, for tests that read the log.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Captured {
    pub(crate) fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// The captured lines parsed as JSON.
    pub(crate) fn json_lines(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[cfg(test)]
impl io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_is_one_object_per_line_with_the_event_fields() {
        let captured = Captured::default();
        let dispatch = dispatch("info", LogFormat::Json, captured.clone());

        tracing::dispatcher::with_default(&dispatch, || {
            tracing::info!(role = "worker", "started");
            tracing::debug!("filtered out");
        });

        let lines = captured.json_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["level"], "INFO");
        assert_eq!(lines[0]["fields"]["message"], "started");
        assert_eq!(lines[0]["fields"]["role"], "worker");
        assert!(lines[0]["timestamp"].is_string());
    }

    #[test]
    fn text_output_is_plain_and_the_filter_applies_per_target() {
        let captured = Captured::default();
        let dispatch = dispatch("warn,zirv_flow=debug", LogFormat::Text, captured.clone());

        tracing::dispatcher::with_default(&dispatch, || {
            tracing::debug!(target: "zirv_flow::x", "kept");
            tracing::info!(target: "other", "dropped");
        });

        let text = captured.text();
        assert!(text.contains("kept"), "{text}");
        assert!(!text.contains("dropped"), "{text}");
        assert!(!text.starts_with('{'), "{text}");
    }

    #[test]
    fn the_export_layer_leaves_the_log_lines_as_they_were_and_gets_the_spans() {
        let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
        let provider = crate::otlp::provider()
            .with_simple_exporter(exporter.clone())
            .build();
        let otlp = Otlp::with_provider(provider);
        let plain = Captured::default();
        let exporting = Captured::default();
        let run = |dispatch: &Dispatch| {
            tracing::dispatcher::with_default(dispatch, || {
                tracing::info_span!("job", trace_id = "0123456789abcdef0123456789abcdef")
                    .in_scope(|| tracing::info!(role = "worker", "started"));
            });
        };

        run(&dispatch("info", LogFormat::Json, plain.clone()));
        run(&dispatch_with(
            "info",
            LogFormat::Json,
            exporting.clone(),
            Some(otlp.layer()),
        ));

        let strip = |lines: Vec<serde_json::Value>| {
            lines
                .into_iter()
                .map(|mut line| {
                    line.as_object_mut().unwrap().remove("timestamp");
                    line
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(strip(plain.json_lines()), strip(exporting.json_lines()));
        assert_eq!(exporter.get_finished_spans().unwrap().len(), 1);
    }

    #[test]
    fn filter_directives_are_checked() {
        assert!(is_valid_filter("info"));
        assert!(is_valid_filter("warn,zirv_flow_jobs=debug"));
        assert!(!is_valid_filter("zirv=notalevel"));
    }
}
