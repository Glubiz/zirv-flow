//! The job that runs a sync.

use zirv_flow_core::{Cause, TraceContext, TraceId};

use super::model::SyncRun;

/// The job kind that runs a sync, and fans a scheduled source out into runs.
pub const RUN_JOB_KIND: &str = "sync.run";

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
    fn a_run_trace_is_the_runs_trace_caused_by_the_run() {
        let run = sample_run();

        let trace = run_trace(&run);

        assert_eq!(trace.trace_id.as_uuid(), run.trace_id);
        assert_eq!(trace.cause.kind, CauseKind::SyncRun);
        assert_eq!(trace.cause.id, run.id.to_string());
    }
}
