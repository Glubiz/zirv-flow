//! Every job kind this build ships: one module per kind and one line in [`ALL`].

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::{ErasedKind, JobError};

mod ai_transcripts;
mod audit_retention;
mod decisions_retention;
mod delivery_send;
mod delivery_sync;
mod destination_erase;
mod event_forward;
mod events_retention;
mod flow_enroll;
mod flow_lifecycle;
mod flow_settle;
mod flow_step;
mod import_dry_run;
mod import_rows;
mod import_run;
mod ingest_drain;
mod ingest_replay;
mod jobs_retention;
mod kafka_consume;
mod messages_retention;
mod noop;
mod person_merge_rewrite;
mod privacy_erase;
mod privacy_export;
mod segment_reevaluate;
pub(crate) mod segment_refresh;
mod sending_domain_verify;
mod sources_purge;
mod summary_rollup;
mod sync_run;
mod sync_tokens;
mod template_test_send;
mod traits_recompute;
mod traits_slot;
mod webhook_call;

#[cfg(test)]
mod simulation;

pub(crate) use flow_step::{WakeBatch, plan_wakes};

/// Stamps the run of retention `window` unless it failed having deleted nothing: a purge that
/// never got going must not look fresh to the status the API reads from `retention_runs`. A
/// failure after some batches committed still stamps, as those rows are gone.
pub(crate) async fn record_run(
    pool: &zirv_flow_store::PgPool,
    window: &str,
    now: time::OffsetDateTime,
    deleted: u64,
    failed: bool,
) -> Result<(), zirv_flow_store::StoreError> {
    if failed && deleted == 0 {
        return Ok(());
    }
    zirv_flow_store::record_retention_run(pool, window, now, deleted).await
}

/// The rows removed by the last recorded run of `window`; none before its first run.
#[cfg(test)]
pub(crate) async fn recorded_run(pool: &zirv_flow_store::PgPool, window: &str) -> Option<i64> {
    zirv_flow_store::retention_runs(pool)
        .await
        .unwrap()
        .into_iter()
        .find(|run| run.window == window)
        .map(|run| run.deleted)
}

pub(crate) static ALL: &[&dyn ErasedKind] = &[
    &noop::Noop,
    &person_merge_rewrite::PersonMergeRewrite,
    &traits_recompute::TraitsRecompute,
    &traits_slot::TraitsSlotBackfill,
    &traits_slot::TraitsSlotClear,
    &events_retention::EventsRetention,
    &messages_retention::MessagesRetention,
    &decisions_retention::DecisionsRetention,
    &jobs_retention::JobsRetention,
    &audit_retention::AuditRetention,
    &ai_transcripts::AiTranscripts,
    &ingest_drain::IngestDrain,
    &ingest_replay::IngestReplay,
    &privacy_erase::PrivacyErase,
    &privacy_export::PrivacyExport,
    &import_dry_run::ImportDryRun,
    &import_run::ImportRun,
    &sync_run::SyncRunKind,
    &sources_purge::SourcesPurge,
    &kafka_consume::KafkaConsume,
    &segment_refresh::SegmentRefresh,
    &segment_reevaluate::SegmentReevaluate,
    &flow_step::FlowStep,
    &flow_enroll::FlowEnroll,
    &flow_settle::FlowSettle,
    &flow_lifecycle::FlowLifecycle,
    &webhook_call::WebhookCall,
    &delivery_send::DeliverySend,
    &delivery_sync::DeliverySync,
    &destination_erase::DestinationErase,
    &event_forward::EventForward,
    &sending_domain_verify::SendingDomainVerify,
    &template_test_send::TemplateTestSend,
    &summary_rollup::SummaryRollup,
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

/// Helpers of the tests that guard the lock order of the segment jobs: the slot lock comes
/// before every segment lock.
#[cfg(test)]
pub(crate) mod slot_order {
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;
    use zirv_flow_store::{REFRESH_LOCK_CLASS, SLOT_LOCK_CLASS, Tx, begin};

    /// A transaction holding the exclusive slot lock of the workspace, as a retire does, so a job
    /// that asks for the shared lock waits. Dropping or committing it releases the lock.
    pub(crate) async fn hold_slot_lock_exclusively(pool: &PgPool, ws: WorkspaceId) -> Tx {
        let mut tx = begin(pool).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::text))")
            .bind(SLOT_LOCK_CLASS)
            .bind(ws.as_uuid().to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }

    /// Whether nobody holds the refresh lock of `segment`: a job still waiting for the slot lock
    /// must not hold it.
    pub(crate) async fn segment_lock_is_free(pool: &PgPool, segment: Uuid) -> bool {
        sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1 || '/' || $2::text, 0))",
        )
        .bind(REFRESH_LOCK_CLASS)
        .bind(segment)
        .fetch_one(pool)
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod run_stamp_tests {
    use sqlx::PgPool;
    use time::OffsetDateTime;

    use super::{record_run, recorded_run};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_run_stamps_only_when_it_deleted_something(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let now = OffsetDateTime::now_utc();

        record_run(&pool, "w", now, 0, true).await.unwrap();
        assert_eq!(recorded_run(&pool, "w").await, None);

        record_run(&pool, "w", now, 5, true).await.unwrap();
        assert_eq!(recorded_run(&pool, "w").await, Some(5));

        record_run(&pool, "x", now, 0, false).await.unwrap();
        assert_eq!(recorded_run(&pool, "x").await, Some(0));
    }
}
