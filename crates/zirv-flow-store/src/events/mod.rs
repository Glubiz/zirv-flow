//! The governed event store (architecture sections 12.1 and 12.2): the schema registry and
//! policy, the event path of apply-batch, ingest rejects, partitions and retention.

mod apply;
mod debugger;
mod explore;
mod partitions;
mod rejects;
mod replay;
mod retention;
mod schemas;

pub(crate) use apply::{EventBatch, FailedItem, apply_event_record, reject_apply_failed};
pub use apply::{EventOutcome, EventRecord, apply_event_batch};
#[cfg(test)]
pub(crate) use apply::{NewEvent, insert_event};
pub use debugger::{
    DEBUGGER_CHANNEL, DebuggerListener, FEED_RETENTION_MINUTES, FeedFilter, FeedItem,
    WATCH_SECONDS, debugger_epoch, debugger_feed_head, debugger_feed_position, debugger_feed_seqs,
    debugger_feed_start, listen_for_debugger, prune_debugger_feed, read_debugger_feed,
    touch_debugger_watch,
};
pub use explore::{
    BREAKDOWN_VALUES, Breakdown, Bucket, CountResult, ExploreError, ExploreFilter, ExploreLimits,
    ExploreRow, ExploreScope, FilterOp, MAX_FILTERS, MAX_RANGE_DAYS, RowCursor, RowsPage,
    explore_breakdown, explore_count, explore_rows, explore_total,
};
pub use partitions::ensure_event_partitions;
pub use rejects::{IngestReject, RejectCursor, RejectFilter, list_ingest_rejects};
pub(crate) use rejects::{Refusal, insert_record_rejects};
pub use replay::{
    MAX_REPLAY, REPLAY_JOB_KIND, ReplayPrediction, ReplaySelection, ReplayTotals, enqueue_replay,
    preview_replay, replay_rejects, select_rejects,
};
pub use retention::{EventRetention, RETENTION_BATCH, RetentionReport, apply_event_retention};
pub use schemas::{
    EventPolicy, EventSchema, EventSchemaUpdate, NewEventSchema, PublishOutcome, SchemaVersion,
    create_event_schema, find_event_schema, get_event_policy, list_event_schemas,
    list_schema_versions, publish_schema_draft, save_schema_draft, set_event_policy,
    update_event_schema,
};
