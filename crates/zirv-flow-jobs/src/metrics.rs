//! Job metrics, through the `metrics` facade; the binary installs the recorder.

use std::collections::BTreeSet;

use zirv_flow_store::{DrainOutcome, QueueDepth};

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

/// Counts what a drained batch did: `zirv_flow_ingest_applied_total{kind,outcome}` and
/// `zirv_flow_ingest_late_total`.
pub(crate) fn record_drained(outcome: &DrainOutcome) {
    for ((kind, result), count) in &outcome.applied {
        metrics::counter!(
            "zirv_flow_ingest_applied_total",
            "kind" => *kind,
            "outcome" => *result,
        )
        .increment(*count);
    }
    if outcome.late > 0 {
        metrics::counter!("zirv_flow_ingest_late_total").increment(outcome.late);
    }
}

/// How a sync run ended, the `outcome` label of `zirv_flow_sync_runs_total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl RunOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Counts an ended sync run and times it: `zirv_flow_sync_runs_total{outcome}` and
/// `zirv_flow_sync_run_duration_seconds`. No source, stream or workspace label.
pub(crate) fn record_sync_run(outcome: RunOutcome, seconds: f64) {
    metrics::counter!("zirv_flow_sync_runs_total", "outcome" => outcome.as_str()).increment(1);
    metrics::histogram!("zirv_flow_sync_run_duration_seconds").record(seconds);
}

/// Counts what a committed sync batch did: `zirv_flow_sync_records_total{outcome}`.
pub(crate) fn record_sync_records(counts: &zirv_flow_store::SyncCounts) {
    for (outcome, count) in [
        ("applied", counts.applied),
        ("unchanged", counts.unchanged),
        ("rejected", counts.rejected),
        ("deleted", counts.deleted),
    ] {
        if let Ok(count) = u64::try_from(count)
            && count > 0
        {
            metrics::counter!("zirv_flow_sync_records_total", "outcome" => outcome)
                .increment(count);
        }
    }
}

/// The `zirv_flow_ingest_inbox_age_seconds` gauge: the oldest unprocessed inbox row.
pub(crate) fn set_inbox_age(seconds: f64) {
    metrics::gauge!("zirv_flow_ingest_inbox_age_seconds").set(seconds);
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
        set_inbox_age(0.0);
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

    #[test]
    fn a_drained_batch_counts_by_kind_and_outcome_and_the_age_gauge_follows() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut outcome = DrainOutcome {
            rows: 3,
            late: 2,
            ..DrainOutcome::default()
        };
        outcome.applied.insert(("track", "valid"), 2);
        outcome.applied.insert(("identify", "apply_failed"), 1);

        metrics::with_local_recorder(&recorder, || {
            record_drained(&outcome);
            set_inbox_age(12.5);
        });

        let output = handle.render();
        assert!(
            output.contains("zirv_flow_ingest_applied_total{kind=\"track\",outcome=\"valid\"} 2"),
            "{output}"
        );
        assert!(output.contains("outcome=\"apply_failed\"} 1"), "{output}");
        assert!(output.contains("zirv_flow_ingest_late_total 2"), "{output}");
        assert!(
            output.contains("zirv_flow_ingest_inbox_age_seconds 12.5"),
            "{output}"
        );
    }

    #[test]
    fn stepping_down_zeroes_the_inbox_age() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            set_inbox_age(9.0);
            QueueGauges::default().zero_all();
        });

        assert!(
            handle
                .render()
                .contains("zirv_flow_ingest_inbox_age_seconds 0"),
        );
    }

    #[test]
    fn sync_metrics_carry_only_the_outcome_label() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            record_sync_run(RunOutcome::Succeeded, 1.5);
            record_sync_run(RunOutcome::Failed, 0.5);
            record_sync_records(&zirv_flow_store::SyncCounts {
                read: 9,
                applied: 4,
                unchanged: 3,
                rejected: 1,
                deleted: 1,
            });
        });
        let output = handle.render();

        assert!(
            output.contains("zirv_flow_sync_runs_total{outcome=\"succeeded\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_sync_runs_total{outcome=\"failed\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_sync_run_duration_seconds_count 2"),
            "{output}"
        );
        for (outcome, count) in [
            ("applied", 4),
            ("unchanged", 3),
            ("rejected", 1),
            ("deleted", 1),
        ] {
            let line = format!("zirv_flow_sync_records_total{{outcome=\"{outcome}\"}} {count}");
            assert!(output.contains(&line), "{output}");
        }
        assert!(
            !output.contains("source") && !output.contains("workspace"),
            "{output}"
        );
    }
}
