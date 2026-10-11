//! The governed event store (architecture sections 12.1 and 12.2): the schema registry and
//! policy, the event path of apply-batch, ingest rejects, partitions and retention.

mod apply;
mod partitions;
mod rejects;
mod retention;
mod schemas;

pub use apply::{EventOutcome, EventRecord, apply_event_batch};
pub use partitions::ensure_event_partitions;
pub use rejects::{IngestReject, RejectCursor, RejectFilter, list_ingest_rejects};
pub use retention::{EventRetention, RETENTION_BATCH, RetentionReport, apply_event_retention};
pub use schemas::{
    EventPolicy, EventSchema, EventSchemaUpdate, NewEventSchema, PublishOutcome, SchemaVersion,
    create_event_schema, find_event_schema, get_event_policy, list_event_schemas,
    list_schema_versions, publish_schema_draft, save_schema_draft, set_event_policy,
    update_event_schema,
};
