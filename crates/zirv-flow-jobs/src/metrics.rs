//! Job metrics, through the `metrics` facade; the binary installs the recorder.

use std::collections::BTreeSet;

use zirv_flow_store::QueueDepth;

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

pub(crate) fn record_claim_lag(kind: &str, seconds: f64) {
    metrics::histogram!("zirv_flow_job_claim_lag_seconds", "kind" => kind.to_owned())
        .record(seconds);
}

/// The queue-depth gauge `zirv_flow_jobs{kind,state}`. It remembers which pairs it has set,
/// so a pair that drops out of the counts is set to 0 rather than left at its last value.
#[derive(Default)]
pub(crate) struct QueueGauges {
    seen: BTreeSet<(String, String)>,
}

impl QueueGauges {
    pub(crate) fn update(&mut self, depths: &[QueueDepth]) {
        let current: BTreeSet<(String, String)> = depths
            .iter()
            .map(|depth| (depth.kind.clone(), depth.state.clone()))
            .collect();
        for (kind, state) in self.seen.difference(&current) {
            queue_gauge(kind, state, 0.0);
        }
        for depth in depths {
            queue_gauge(&depth.kind, &depth.state, depth.count as f64);
        }
        self.seen = current;
    }

    /// Sets every pair this instance has published to 0, for an instance that stops leading.
    pub(crate) fn zero_all(&mut self) {
        self.update(&[]);
    }
}

fn queue_gauge(kind: &str, state: &str, value: f64) {
    metrics::gauge!("zirv_flow_jobs", "kind" => kind.to_owned(), "state" => state.to_owned())
        .set(value);
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

    fn depth(kind: &str, state: &str, count: i64) -> QueueDepth {
        QueueDepth {
            kind: kind.into(),
            state: state.into(),
            count,
        }
    }

    #[test]
    fn the_queue_gauges_follow_the_counts_and_zero_a_pair_that_vanished() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut gauges = QueueGauges::default();

        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[depth("a.kind", "ready", 7), depth("a.kind", "running", 3)]);
        });
        let first = handle.render();
        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[depth("a.kind", "ready", 2)]);
        });
        let second = handle.render();

        assert!(
            first.contains("zirv_flow_jobs{kind=\"a.kind\",state=\"ready\"} 7"),
            "{first}"
        );
        assert!(
            first.contains("zirv_flow_jobs{kind=\"a.kind\",state=\"running\"} 3"),
            "{first}"
        );
        assert!(
            second.contains("zirv_flow_jobs{kind=\"a.kind\",state=\"ready\"} 2"),
            "{second}"
        );
        assert!(
            second.contains("zirv_flow_jobs{kind=\"a.kind\",state=\"running\"} 0"),
            "{second}"
        );
    }

    #[test]
    fn stepping_down_zeroes_every_published_pair() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut gauges = QueueGauges::default();

        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[depth("a.kind", "ready", 7), depth("b.kind", "dead", 4)]);
            gauges.zero_all();
        });

        let output = handle.render();
        assert!(
            output.contains("zirv_flow_jobs{kind=\"a.kind\",state=\"ready\"} 0"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_jobs{kind=\"b.kind\",state=\"dead\"} 0"),
            "{output}"
        );
    }

    #[test]
    fn claim_lag_is_recorded_per_kind() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || record_claim_lag("a.kind", 1.5));

        let output = handle.render();
        assert!(
            output.contains("zirv_flow_job_claim_lag_seconds"),
            "{output}"
        );
        assert!(output.contains("kind=\"a.kind\""), "{output}");
    }
}
