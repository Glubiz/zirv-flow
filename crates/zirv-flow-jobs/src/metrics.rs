//! Job metrics, through the `metrics` facade; the binary installs the recorder.

use zirv_flow_store::QueueCounts;

/// How a job attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Completed,
    Retried,
    Dead,
    LeaseLost,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Retried => "retried",
            Self::Dead => "dead",
            Self::LeaseLost => "lease_lost",
        }
    }
}

pub(crate) fn record_attempt(kind: &str, outcome: Outcome, seconds: f64) {
    metrics::counter!(
        "zirv_flow_jobs_total",
        "kind" => kind.to_owned(),
        "outcome" => outcome.as_str(),
    )
    .increment(1);
    metrics::histogram!("zirv_flow_job_duration_seconds", "kind" => kind.to_owned())
        .record(seconds);
}

pub(crate) fn set_queue_gauges(counts: QueueCounts) {
    metrics::gauge!("zirv_flow_jobs_ready").set(counts.ready as f64);
    metrics::gauge!("zirv_flow_jobs_running").set(counts.running as f64);
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    #[test]
    fn an_attempt_is_counted_by_kind_and_outcome_and_timed() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            record_attempt("maintenance.noop", Outcome::Completed, 0.25);
            record_attempt("maintenance.noop", Outcome::Completed, 0.5);
            record_attempt("maintenance.noop", Outcome::LeaseLost, 0.1);
        });
        let output = handle.render();

        assert!(
            output.contains(
                "zirv_flow_jobs_total{kind=\"maintenance.noop\",outcome=\"completed\"} 2"
            ),
            "{output}"
        );
        assert!(output.contains("outcome=\"lease_lost\"} 1"), "{output}");
        assert!(
            output.contains("zirv_flow_job_duration_seconds"),
            "{output}"
        );
    }

    #[test]
    fn every_outcome_has_its_own_label() {
        let labels = [
            Outcome::Completed,
            Outcome::Retried,
            Outcome::Dead,
            Outcome::LeaseLost,
        ]
        .map(Outcome::as_str);

        assert_eq!(labels, ["completed", "retried", "dead", "lease_lost"]);
    }

    #[test]
    fn the_queue_gauges_follow_the_counts() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            set_queue_gauges(QueueCounts {
                ready: 7,
                running: 3,
                dead: 1,
            });
        });
        let output = handle.render();

        assert!(output.contains("zirv_flow_jobs_ready 7"), "{output}");
        assert!(output.contains("zirv_flow_jobs_running 3"), "{output}");
    }
}
