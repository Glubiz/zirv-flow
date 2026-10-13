//! The metrics of the database sources. Labels are the source type and a fixed reason, never a
//! source, stream or workspace.

use std::future::Future;

use super::error::reason_of;
use crate::sources::SourceError;

pub(crate) async fn metered<T>(
    source_type: &'static str,
    call: impl Future<Output = Result<T, SourceError>>,
) -> Result<T, SourceError> {
    let result = call.await;
    if let Err(failure) = &result {
        error(source_type, failure);
    }
    result
}

pub(crate) fn batch(source_type: &'static str) {
    metrics::counter!("zirv_flow_source_db_batches_total", "source_type" => source_type)
        .increment(1);
}

/// Counts a failed call by its reason. Egress denials are also counted by class in
/// `zirv_flow_egress_denied_total` where they happen.
pub(crate) fn error(source_type: &'static str, error: &SourceError) {
    metrics::counter!(
        "zirv_flow_source_db_errors_total",
        "source_type" => source_type,
        "reason" => reason_of(error)
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_and_errors_are_counted_with_only_the_type_and_reason() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            batch("postgres");
            batch("postgres");
            error("mysql", &SourceError::Auth);
        });

        let rendered = handle.render();

        assert!(
            rendered.contains("zirv_flow_source_db_batches_total{source_type=\"postgres\"} 2"),
            "{rendered}"
        );
        assert!(
            rendered.contains(
                "zirv_flow_source_db_errors_total{source_type=\"mysql\",reason=\"auth\"} 1"
            ),
            "{rendered}"
        );
    }
}
