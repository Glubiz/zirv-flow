//! Sync runs (architecture sections 7.1, 7.2, 12.4 and 16.3): one run reads one stream of a
//! source from a checkpoint and applies the records to people. This module holds the run's
//! state machine, the one transaction that commits a batch, and the job that starts a run.
//!
//! Nothing here holds or logs a record value, an identifier or a trait value.

mod batch;
mod job;
mod model;
#[cfg(test)]
pub(crate) mod testing;

pub use batch::{
    BatchOutcome, BatchProgress, Counts, EventCounts, SYNC_REJECT_CHECK_MIN_RECORDS, SyncBatch,
    SyncEvent, SyncReject, SyncUpsert, commit_sync_batch,
};
pub use job::{RUN_JOB_KIND, run_trace};
pub use model::{
    CancelOutcome, FanOut, Lease, RunCursor, RunEnd, RunReject, RunTrigger, SyncRun, cancel_run,
    fail_orphaned_runs, fail_run, find_run, finish_run, list_run_rejects, list_runs, queue_run,
    queue_runs_for_source, start_run,
};
