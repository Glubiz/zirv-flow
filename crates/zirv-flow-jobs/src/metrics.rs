//! Job metrics, through the `metrics` facade; the binary installs the recorder.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;
use zirv_flow_core::Freshness;
use zirv_flow_store::{DrainOutcome, QueueDepth, StreamFreshness};

/// How a job attempt ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Completed,
    Retried,
    Dead,
    Purged,
    LeaseLost,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Retried => "retried",
            Self::Dead => "dead",
            Self::Purged => "purged",
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

/// Counts what a `flow.enroll` did: `zirv_flow_flow_enrollments_total{trigger,outcome}` for the
/// people enrolled and the ones an occurrence had already enrolled, and
/// `zirv_flow_flow_enroll_skips_total{trigger,reason}` for each refusal, by its reason.
pub(crate) fn record_enroll(trigger: &'static str, outcome: &zirv_flow_store::EnrollOutcome) {
    let counts = [
        ("enrolled", outcome.enrolled.len()),
        ("replayed", outcome.replayed),
    ];
    for (label, count) in counts {
        if count > 0 {
            metrics::counter!(
                "zirv_flow_flow_enrollments_total",
                "trigger" => trigger,
                "outcome" => label,
            )
            .increment(count as u64);
        }
    }
    let mut skipped: BTreeMap<&'static str, u64> = BTreeMap::new();
    for (_, reason) in &outcome.skipped {
        *skipped.entry(reason).or_default() += 1;
    }
    for (reason, count) in skipped {
        record_enroll_skips(trigger, reason, count);
    }
}

/// Counts `count` refusals for one reason.
pub(crate) fn record_enroll_skips(trigger: &'static str, reason: &'static str, count: u64) {
    metrics::counter!(
        "zirv_flow_flow_enroll_skips_total",
        "trigger" => trigger,
        "reason" => reason,
    )
    .increment(count);
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

/// Counts what the event path did with the events of a committed sync batch:
/// `zirv_flow_sync_events_total{outcome}` with `accepted`, `duplicate` and `rejected`.
pub(crate) fn record_sync_events(counts: &zirv_flow_store::SyncEventCounts) {
    for (outcome, count) in [
        ("accepted", counts.accepted),
        ("duplicate", counts.duplicate),
        ("rejected", counts.rejected),
    ] {
        if let Ok(count) = u64::try_from(count)
            && count > 0
        {
            metrics::counter!("zirv_flow_sync_events_total", "outcome" => outcome).increment(count);
        }
    }
}

/// The `zirv_flow_ingest_inbox_age_seconds` gauge: the oldest unprocessed inbox row.
pub(crate) fn set_inbox_age(seconds: f64) {
    metrics::gauge!("zirv_flow_ingest_inbox_age_seconds").set(seconds);
}

/// The `zirv_flow_segment_dirty_age_seconds` gauge: the oldest dirty mark.
pub(crate) fn set_dirty_age(seconds: f64) {
    metrics::gauge!("zirv_flow_segment_dirty_age_seconds").set(seconds);
}

/// The `zirv_flow_segment_dirty_backlog` gauge: the people marked dirty, over all workspaces.
pub(crate) fn set_dirty_backlog(people: i64) {
    metrics::gauge!("zirv_flow_segment_dirty_backlog").set(people as f64);
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
        set_dirty_age(0.0);
        set_dirty_backlog(0);
    }
}

fn queue_gauge(kind: &str, state: &str, value: f64) {
    metrics::gauge!("zirv_flow_jobs", "kind" => kind.to_owned(), "state" => state.to_owned())
        .set(value);
}

/// The freshness gauges (architecture 7.5, 15):
/// - `zirv_flow_stream_freshness_seconds{workspace,stream}`, the age of the stream's data;
/// - `zirv_flow_stream_state{workspace,stream}`, 0 fresh, 1 stale, 2 failing;
/// - `zirv_flow_streams{workspace,state}`, how many enabled streams are in each state.
///
/// The labels are ids only: no source name, kind, host, schedule or record value. It remembers
/// what it has set, so a stream or workspace that dropped out of the report (disabled, deleted)
/// is set to 0 rather than left at its last value.
#[derive(Default)]
pub(crate) struct FreshnessGauges {
    streams: BTreeSet<(Uuid, Uuid)>,
    workspaces: BTreeSet<Uuid>,
    /// The streams whose lag gauge is set.
    lagging: BTreeSet<(Uuid, Uuid)>,
}

impl FreshnessGauges {
    pub(crate) fn update(&mut self, report: &[StreamFreshness]) {
        let streams: BTreeSet<(Uuid, Uuid)> = report
            .iter()
            .map(|stream| (stream.workspace_id, stream.stream_id))
            .collect();
        let mut counts: BTreeMap<Uuid, BTreeMap<Freshness, usize>> = BTreeMap::new();
        for stream in report {
            *counts
                .entry(stream.workspace_id)
                .or_default()
                .entry(stream.state)
                .or_default() += 1;
        }
        let workspaces: BTreeSet<Uuid> = counts.keys().copied().collect();
        for (workspace, stream) in self.streams.difference(&streams) {
            stream_gauges(*workspace, *stream, 0.0, 0.0);
        }
        for workspace in self.workspaces.difference(&workspaces) {
            for state in STATES {
                count_gauge(*workspace, state, 0.0);
            }
        }
        for stream in report {
            stream_gauges(
                stream.workspace_id,
                stream.stream_id,
                stream.age_seconds as f64,
                stream.state.gauge_value(),
            );
        }
        for (workspace, by_state) in &counts {
            for state in STATES {
                let count = by_state.get(&state).copied().unwrap_or(0);
                count_gauge(*workspace, state, count as f64);
            }
        }
        // The lag of a consumer, in messages: set while the stream reports one, 0 once it does
        // not (disabled, deleted, not a consumer).
        let lagging: BTreeSet<(Uuid, Uuid)> = report
            .iter()
            .filter(|stream| stream.lag.is_some())
            .map(|stream| (stream.workspace_id, stream.stream_id))
            .collect();
        for (workspace, stream) in self.lagging.difference(&lagging) {
            lag_gauge(*workspace, *stream, 0.0);
        }
        for stream in report {
            if let Some(lag) = stream.lag {
                lag_gauge(stream.workspace_id, stream.stream_id, lag as f64);
            }
        }
        self.lagging = lagging;
        self.streams = streams;
        self.workspaces = workspaces;
    }

    /// Sets everything this instance has published to 0, for an instance that stops leading.
    pub(crate) fn zero_all(&mut self) {
        self.update(&[]);
    }
}

fn lag_gauge(workspace: Uuid, stream: Uuid, lag: f64) {
    metrics::gauge!(
        "zirv_flow_stream_lag_messages",
        "workspace" => workspace.to_string(),
        "stream" => stream.to_string(),
    )
    .set(lag);
}

const STATES: [Freshness; 3] = [Freshness::Fresh, Freshness::Stale, Freshness::Failing];

fn stream_gauges(workspace: Uuid, stream: Uuid, age_seconds: f64, state: f64) {
    metrics::gauge!(
        "zirv_flow_stream_freshness_seconds",
        "workspace" => workspace.to_string(),
        "stream" => stream.to_string(),
    )
    .set(age_seconds);
    metrics::gauge!(
        "zirv_flow_stream_state",
        "workspace" => workspace.to_string(),
        "stream" => stream.to_string(),
    )
    .set(state);
}

fn count_gauge(workspace: Uuid, state: Freshness, count: f64) {
    metrics::gauge!(
        "zirv_flow_streams",
        "workspace" => workspace.to_string(),
        "state" => state.as_str(),
    )
    .set(count);
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
    fn an_enroll_counts_its_people_and_every_refusal_by_reason() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let person = Uuid::nil();
        let outcome = zirv_flow_store::EnrollOutcome {
            enrolled: Vec::new(),
            skipped: vec![
                (person, "flow_paused"),
                (person, "flow_paused"),
                (person, "already_entered"),
            ],
            replayed: 4,
        };

        metrics::with_local_recorder(&recorder, || {
            record_enroll("segment", &outcome);
            record_enroll_skips("schedule", "flow_paused", 1);
        });
        let output = handle.render();

        assert!(
            output.contains(
                "zirv_flow_flow_enroll_skips_total{trigger=\"segment\",reason=\"flow_paused\"} 2"
            ),
            "{output}"
        );
        assert!(output.contains("reason=\"already_entered\"} 1"), "{output}");
        assert!(
            output.contains("trigger=\"schedule\",reason=\"flow_paused\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("trigger=\"segment\",outcome=\"replayed\"} 4"),
            "{output}"
        );
        assert!(!output.contains("outcome=\"enrolled\""), "{output}");
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
    fn stepping_down_zeroes_the_dirty_gauges() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            set_dirty_age(9.0);
            set_dirty_backlog(4);
            QueueGauges::default().zero_all();
        });

        let output = handle.render();
        assert!(
            output.contains("zirv_flow_segment_dirty_age_seconds 0"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_segment_dirty_backlog 0"),
            "{output}"
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

    fn fresh_row(workspace: Uuid, stream: Uuid, state: Freshness, age: i64) -> StreamFreshness {
        StreamFreshness {
            workspace_id: workspace,
            source_id: Uuid::nil(),
            stream_id: stream,
            state,
            last_success_at: None,
            age_seconds: age,
            threshold_seconds: None,
            threshold_is_default: false,
            high_water_mark: None,
            lag: None,
        }
    }

    #[test]
    fn the_freshness_gauges_follow_the_report_and_zero_what_dropped_out() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut gauges = FreshnessGauges::default();
        let (ws, other) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (a, b, c) = (
            Uuid::from_u128(10),
            Uuid::from_u128(11),
            Uuid::from_u128(12),
        );

        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[
                fresh_row(ws, a, Freshness::Fresh, 5),
                fresh_row(ws, b, Freshness::Stale, 700),
                fresh_row(other, c, Freshness::Failing, 9),
            ]);
        });
        let first = handle.render();
        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[fresh_row(ws, a, Freshness::Failing, 6)]);
        });
        let second = handle.render();

        let value = |output: &str, metric: &str, key: &str, id: Uuid| {
            output
                .lines()
                .filter(|line| {
                    line.starts_with(metric) && line.contains(&format!("{key}=\"{id}\""))
                })
                .map(|line| line.rsplit_once(' ').unwrap().1.to_owned())
                .next()
        };
        assert_eq!(
            value(&first, "zirv_flow_stream_state{", "stream", b).as_deref(),
            Some("1")
        );
        assert_eq!(
            value(&first, "zirv_flow_stream_freshness_seconds{", "stream", b).as_deref(),
            Some("700")
        );
        assert_eq!(
            value(&second, "zirv_flow_stream_state{", "stream", a).as_deref(),
            Some("2")
        );
        assert_eq!(
            value(&second, "zirv_flow_stream_state{", "stream", b).as_deref(),
            Some("0")
        );
        assert_eq!(
            value(&second, "zirv_flow_stream_freshness_seconds{", "stream", c).as_deref(),
            Some("0")
        );
        let counts = |output: &str, workspace: Uuid, state: &str| {
            output
                .lines()
                .find(|line| {
                    line.starts_with("zirv_flow_streams{")
                        && line.contains(&format!("workspace=\"{workspace}\""))
                        && line.contains(&format!("state=\"{state}\""))
                })
                .map(|line| line.rsplit_once(' ').unwrap().1.to_owned())
        };
        assert_eq!(counts(&first, ws, "fresh").as_deref(), Some("1"));
        assert_eq!(counts(&first, ws, "stale").as_deref(), Some("1"));
        assert_eq!(counts(&first, ws, "failing").as_deref(), Some("0"));
        assert_eq!(counts(&second, ws, "failing").as_deref(), Some("1"));
        assert_eq!(counts(&second, ws, "stale").as_deref(), Some("0"));
        assert_eq!(counts(&second, other, "failing").as_deref(), Some("0"));
    }

    #[test]
    fn the_lag_gauge_follows_the_report_and_is_zeroed_when_a_stream_drops_out_or_stops_reporting() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut gauges = FreshnessGauges::default();
        let (ws, a, b) = (Uuid::from_u128(1), Uuid::from_u128(10), Uuid::from_u128(11));
        let consumer = |stream, lag| StreamFreshness {
            lag,
            ..fresh_row(ws, stream, Freshness::Fresh, 1)
        };
        let value = |output: &str, id: Uuid| {
            output
                .lines()
                .filter(|line| {
                    line.starts_with("zirv_flow_stream_lag_messages{")
                        && line.contains(&format!("stream=\"{id}\""))
                })
                .map(|line| line.rsplit_once(' ').unwrap().1.to_owned())
                .next()
        };

        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[consumer(a, Some(1204)), consumer(b, None)]);
        });
        let first = handle.render();
        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[consumer(b, None)]);
        });
        let second = handle.render();

        assert_eq!(value(&first, a).as_deref(), Some("1204"));
        assert_eq!(value(&first, b), None, "a pull stream has no lag series");
        assert_eq!(value(&second, a).as_deref(), Some("0"));
    }

    #[test]
    fn stepping_down_zeroes_the_freshness_gauges() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let mut gauges = FreshnessGauges::default();
        let (ws, stream) = (Uuid::from_u128(1), Uuid::from_u128(10));

        metrics::with_local_recorder(&recorder, || {
            gauges.update(&[fresh_row(ws, stream, Freshness::Stale, 700)]);
            gauges.zero_all();
        });

        let output = handle.render();
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("zirv_flow_stream_state{") && line.ends_with(" 0")),
            "{output}"
        );
        assert!(
            !output
                .lines()
                .any(|line| line.starts_with("zirv_flow_stream_state{") && !line.ends_with(" 0")),
            "{output}"
        );
    }

    #[test]
    fn sync_event_metrics_carry_only_the_outcome_label() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            record_sync_events(&zirv_flow_store::SyncEventCounts {
                accepted: 5,
                duplicate: 2,
                rejected: 1,
            });
            record_sync_events(&zirv_flow_store::SyncEventCounts::default());
        });
        let output = handle.render();

        for (outcome, count) in [("accepted", 5), ("duplicate", 2), ("rejected", 1)] {
            let line = format!("zirv_flow_sync_events_total{{outcome=\"{outcome}\"}} {count}");
            assert!(output.contains(&line), "{output}");
        }
        assert!(
            !output.contains("source") && !output.contains("workspace") && !output.contains("name"),
            "{output}"
        );
    }
}
