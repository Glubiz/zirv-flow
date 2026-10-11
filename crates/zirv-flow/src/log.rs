//! Structured logging for the long-running commands.

use std::io;

use tracing::Dispatch;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::config::{Config, LogFormat};

/// Installs the process-wide subscriber, writing to standard error. A second call is a
/// no-op, which only tests make.
pub(crate) fn install(config: &Config) {
    let dispatch = dispatch(
        config.log_filter(),
        config.log_format(),
        io::stderr as fn() -> io::Stderr,
    );
    let _ = tracing::dispatcher::set_global_default(dispatch);
}

/// A subscriber for `filter` and `format` that writes to `writer`. The filter was checked
/// when the configuration loaded; an unusable one falls back to `info`.
pub(crate) fn dispatch<W>(filter: &str, format: LogFormat, writer: W) -> Dispatch
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false);
    match format {
        LogFormat::Json => Dispatch::new(builder.json().with_span_list(false).finish()),
        LogFormat::Text => Dispatch::new(builder.finish()),
    }
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
    fn filter_directives_are_checked() {
        assert!(is_valid_filter("info"));
        assert!(is_valid_filter("warn,zirv_flow_jobs=debug"));
        assert!(!is_valid_filter("zirv=notalevel"));
    }
}
