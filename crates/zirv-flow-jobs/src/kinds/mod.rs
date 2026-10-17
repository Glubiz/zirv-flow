//! Every job kind this build ships: one module per kind and one line in [`ALL`].

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{ErasedKind, JobError};

mod ai_transcripts;
mod events_retention;
mod import_dry_run;
mod import_rows;
mod import_run;
mod ingest_drain;
mod ingest_replay;
mod kafka_consume;
mod noop;
mod person_merge_rewrite;
mod privacy_erase;
mod privacy_export;
pub(crate) mod segment_refresh;
mod sync_run;
mod sync_tokens;
mod traits_recompute;
mod traits_slot;

pub(crate) static ALL: &[&dyn ErasedKind] = &[
    &noop::Noop,
    &person_merge_rewrite::PersonMergeRewrite,
    &traits_recompute::TraitsRecompute,
    &traits_slot::TraitsSlotBackfill,
    &traits_slot::TraitsSlotClear,
    &events_retention::EventsRetention,
    &ai_transcripts::AiTranscripts,
    &ingest_drain::IngestDrain,
    &ingest_replay::IngestReplay,
    &privacy_erase::PrivacyErase,
    &privacy_export::PrivacyExport,
    &import_dry_run::ImportDryRun,
    &import_run::ImportRun,
    &sync_run::SyncRunKind,
    &kafka_consume::KafkaConsume,
    &segment_refresh::SegmentRefresh,
];

/// The `workspace_id` and the uuid under `id_key` of a payload; `expected` is the error text
/// when either is missing or malformed.
fn workspace_and_id(
    payload: &Value,
    id_key: &str,
    expected: &'static str,
) -> Result<(WorkspaceId, Uuid), JobError> {
    let workspace = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<WorkspaceId>().ok());
    let id = payload
        .get(id_key)
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok());
    workspace.zip(id).ok_or_else(|| JobError::new(expected))
}
