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
    BatchOutcome, BatchProgress, Counts, DELETE_BY_SIGNAL, DELETE_BY_SOURCE, EventCounts,
    SYNC_REJECT_CHECK_MIN_RECORDS, SyncBatch, SyncEvent, SyncReject, SyncUpsert, commit_sync_batch,
};
pub(crate) use batch::{Unfenced, fence_run, write_offsets};
pub use job::{CONSUME_JOB_KIND, RUN_JOB_KIND, consume_unique_key, run_trace};
pub use model::{
    CancelOutcome, FanOut, Lease, ReconcileQueued, Rotated, RunCursor, RunEnd, RunKind, RunReject,
    RunTrigger, SyncRun, cancel_consumer_runs, cancel_run, fail_orphaned_runs, fail_run, find_run,
    finish_run, list_run_rejects, list_runs, queue_reconcile_run, queue_run, queue_runs_for_source,
    rotate_stream_run, start_run, start_stream_run,
};
pub use sweep::{SYNC_SWEEP_BATCH, SweepOutcome, SweepUnit, sweep_exceeds_limit, sweep_unreached};
