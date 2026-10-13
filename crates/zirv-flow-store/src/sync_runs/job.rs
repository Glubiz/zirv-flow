//! The job that runs a sync.

use zirv_flow_core::{Cause, TraceContext, TraceId};

use super::model::SyncRun;

/// The job kind that runs a sync, and fans a scheduled source out into runs.
pub const RUN_JOB_KIND: &str = "sync.run";

/// The job kind that consumes a stream-mode stream (a Kafka topic) for one time slice and
/// enqueues its successor.
pub const CONSUME_JOB_KIND: &str = "kafka.consume";

/// The unique key of the consumer job for `generation` of a run. The job that is running holds
/// its own key (the unique index covers ready and running rows), so the successor needs the
/// next generation's key.
pub fn consume_unique_key(run_id: uuid::Uuid, generation: u32) -> String {
    format!("kafka:consume:{run_id}:{generation}")
}

/// The trace and cause every job and decision of a run carries: the run's own trace, caused by
/// the run.
pub fn run_trace(run: &SyncRun) -> TraceContext {
    TraceContext {
        trace_id: TraceId::from_uuid(run.trace_id),
        cause: Cause::sync_run(run.id),
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::CauseKind;

    use super::*;
    use crate::sync_runs::model::tests::sample_run;

    #[test]
    fn the_job_kind_is_sync_run() {
        assert_eq!(RUN_JOB_KIND, "sync.run");
    }

    #[test]
    fn the_consumer_kind_and_its_keys_are_stable_and_differ_by_generation() {
        let run = uuid::Uuid::nil();

        assert_eq!(CONSUME_JOB_KIND, "kafka.consume");
        assert_eq!(
            consume_unique_key(run, 3),
            "kafka:consume:00000000-0000-0000-0000-000000000000:3"
        );
        assert_ne!(consume_unique_key(run, 0), consume_unique_key(run, 1));
    }

    #[test]
    fn a_run_trace_is_the_runs_trace_caused_by_the_run() {
        let run = sample_run();

        let trace = run_trace(&run);

        assert_eq!(trace.trace_id.as_uuid(), run.trace_id);
        assert_eq!(trace.cause.kind, CauseKind::SyncRun);
        assert_eq!(trace.cause.id, run.id.to_string());
    }
}
