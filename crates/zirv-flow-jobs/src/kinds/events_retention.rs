//! `events.retention`: keeps the event store within bounds (architecture section 16.3). An
//! instance-level job that a daily schedule enqueues. It deletes events past their name's
//! retention, old ingest rejects and dedupe records, and drops whole old partitions; it
//! needs no workspace and no master key.

use serde_json::Value;
use tracing::{info, warn};
use zirv_flow_store::{
    RetentionReport, apply_event_retention_progress, database_now, record_retention_run,
};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct EventsRetention;

impl JobKind for EventsRetention {
    const KIND: &'static str = "events.retention";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool() else {
            return Err(JobError::new("the worker has no pool to retain events on"));
        };
        let settings = *ctx.event_retention();
        let now = database_now(pool).await?;
        let windows = *ctx.retention_settings();
        // What committed before a failure still counts, so the runs are recorded either way.
        let mut report = RetentionReport::default();
        let outcome =
            apply_event_retention_progress(pool, &settings, &windows, now, &mut report).await;
        record_retention_run(pool, "events", now, report.events_deleted).await?;
        record_retention_run(pool, "ingest_rejects", now, report.rejects_deleted).await?;
        for partition in &report.skipped_partitions {
            warn!(partition = %partition, "event partition drop skipped: the table is locked");
        }
        outcome?;
        info!(
            events_deleted = report.events_deleted,
            partitions_dropped = report.partitions_dropped,
            partitions_skipped = report.skipped_partitions.len(),
            rejects_deleted = report.rejects_deleted,
            dedupe_deleted = report.dedupe_deleted,
            "event retention ran"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{EventRecord, EventRetention, NewJob, apply_event_batch};

    use super::*;

    #[test]
    fn the_kind_name_is_events_retention() {
        assert_eq!(EventsRetention::KIND, "events.retention");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_deletes_what_is_past_the_settings_and_keeps_the_rest(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let now = OffsetDateTime::now_utc();
        for days in [1_i64, 400] {
            let record = EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), "a@example.com".into())],
                name: "seen".into(),
                properties: json!({}).as_object().cloned().unwrap(),
                timestamp: None,
                received_at: now - time::Duration::days(days),
                message_id: None,
                trace: TraceContext::root(Cause::api("t")),
            };
            apply_event_batch(&pool, ws, &key, &[record]).await.unwrap();
        }
        let mut new = NewJob::new(EventsRetention::KIND, TraceContext::root(Cause::api("req")));
        new.payload = json!({});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[EventsRetention::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();

        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_ingest(crate::context::IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 1,
                shards: 1,
            })
            .with_event_retention(EventRetention {
                default_days: 365,
                ..EventRetention::default()
            });
        EventsRetention
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        tx.rollback().await.unwrap();

        let ages: Vec<i64> = sqlx::query_scalar(
            "SELECT (extract(epoch FROM now() - occurred_at) / 86400)::bigint FROM events",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(ages, [1]);
        assert_eq!(super::super::recorded_run(&pool, "events").await, Some(1));
        assert_eq!(
            super::super::recorded_run(&pool, "ingest_rejects").await,
            Some(0)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_a_committed_batch_still_records_that_batch(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &WorkspaceName::parse("ours").unwrap(),
            &Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        // One full batch of old rejects, then a second delete statement that a trigger refuses.
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, reason, \
                trace_id) \
             SELECT gen_random_uuid(), $1, now() - make_interval(days => 400), 'web', 'event', \
                'bad', gen_random_uuid() \
             FROM generate_series(1, 5001)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(
            "CREATE SEQUENCE reject_deletes; \
             CREATE FUNCTION refuse_second() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF nextval('reject_deletes') > 1 THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN NULL; END $$; \
             CREATE TRIGGER refuse_second BEFORE DELETE ON ingest_rejects \
             FOR EACH STATEMENT EXECUTE FUNCTION refuse_second();",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut new = NewJob::new(EventsRetention::KIND, TraceContext::root(Cause::api("req")));
        new.payload = json!({});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[EventsRetention::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_ingest(crate::context::IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 1,
                shards: 1,
            })
            .with_event_retention(EventRetention::default());

        let result = EventsRetention.run(&mut ctx, claimed.payload.clone()).await;
        tx.rollback().await.unwrap();

        assert!(result.is_err());
        assert_eq!(
            super::super::recorded_run(&pool, "ingest_rejects").await,
            Some(5000)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_daily_at_0317_utc(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'events.retention'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "17 3 * * *", true)
        );
    }
}
