//! Sync runs (architecture sections 7.1, 7.2, 12.4 and 16.3): one run reads one stream of a
//! source from a checkpoint and applies the records to people. This module holds the run's
//! state machine, the one transaction that commits a batch, and the job that starts a run.
//!
//! Nothing here holds or logs a record value, an identifier or a trait value.

mod batch;
mod job;
mod model;
mod sweep;
#[cfg(test)]
pub(crate) mod testing;

pub use batch::{
    BatchOutcome, BatchProgress, Counts, DELETE_BY_SIGNAL, DELETE_BY_SOURCE,
    SYNC_REJECT_CHECK_MIN_RECORDS, SyncBatch, SyncReject, SyncUpsert, commit_sync_batch,
};
pub use job::{RUN_JOB_KIND, run_trace};
pub use model::{
    CancelOutcome, FanOut, Lease, ReconcileQueued, RunCursor, RunEnd, RunKind, RunReject,
    RunTrigger, SyncRun, cancel_run, fail_orphaned_runs, fail_run, find_run, finish_run,
    list_run_rejects, list_runs, queue_reconcile_run, queue_run, queue_runs_for_source, start_run,
};
pub use sweep::{SYNC_SWEEP_BATCH, SweepOutcome, SweepUnit, sweep_exceeds_limit, sweep_unreached};
