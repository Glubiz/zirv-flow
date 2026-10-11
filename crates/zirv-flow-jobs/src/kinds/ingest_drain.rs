//! `ingest.drain`: applies a workspace's ingestion inbox (architecture section 7.1). Each
//! batch is one transaction of its own that applies the oldest rows in id order and deletes
//! them, so the job's transaction only holds the lease.
//!
//! A batch that fails with a retryable error is retried with jittered backoff. A batch that
//! fails otherwise holds a poison row: its rows are applied one at a time, and the row that
//! still fails becomes an `apply_failed` ingest reject, so one bad row never blocks a
//! workspace's inbox. The job never re-enqueues itself, since it still holds its unique key;
//! the scheduler leader's safety net picks up what is left.
//!
//! Nothing here logs an identifier, a value or an event name.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    DRAIN_JOB_KIND, DrainOutcome, HmacKey, PgPool, StoreError, drain_batch, drain_batch_singly,
};

use crate::metrics::record_drained;
use crate::worker::random_unit;
use crate::{JobContext, JobError, JobKind};

/// How long one run drains before it ends, below the job lease.
const TIME_BUDGET: Duration = Duration::from_secs(20);

/// Retries of a batch after a retryable error.
const RETRIES: u32 = 3;

/// The first retry delay; it doubles per retry and gets up to as much jitter again.
const BACKOFF_BASE: Duration = Duration::from_millis(50);

pub(crate) struct IngestDrain;

impl JobKind for IngestDrain {
    const KIND: &'static str = DRAIN_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let workspace = payload
            .get("workspace_id")
            .and_then(Value::as_str)
            .and_then(|text| Uuid::parse_str(text).ok())
            .map(WorkspaceId::from_uuid)
            .ok_or(JobError::new("the payload has no workspace_id"))?;
        let settings = ctx
            .ingest()
            .ok_or(JobError::new("the worker has no pool for the ingest drain"))?;
        let key = settings
            .redaction_key
            .as_deref()
            .ok_or(JobError::new("the worker has no master key"))?;
        let limit = i64::from(settings.batch_size.max(1));

        let started = Instant::now();
        loop {
            let outcome = drain_once(&settings.pool, workspace, key, limit).await?;
            record_drained(&outcome);
            let more = i64::try_from(outcome.rows).is_ok_and(|rows| rows >= limit);
            if !more || started.elapsed() >= TIME_BUDGET {
                return Ok(());
            }
        }
    }
}

/// One batch: whole when it can be, row by row when a row is poison.
async fn drain_once(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    limit: i64,
) -> Result<DrainOutcome, JobError> {
    match retrying(|| drain_batch(pool, workspace, key, limit)).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.is_retryable() => Err(CONFLICTED),
        Err(_) => retrying(|| drain_batch_singly(pool, workspace, key, limit))
            .await
            .map_err(|error| {
                if error.is_retryable() {
                    CONFLICTED
                } else {
                    JobError::new("the inbox batch could not be applied")
                }
            }),
    }
}

const CONFLICTED: JobError = JobError::new("the inbox batch kept conflicting with another");

/// Runs `operation`, and again up to [`RETRIES`] times while it fails with a retryable error.
async fn retrying<T, F, Fut>(mut operation: F) -> Result<T, StoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, StoreError>>,
{
    let mut retries = 0;
    loop {
        match operation().await {
            Err(error) if error.is_retryable() && retries < RETRIES => {
                let delay = BACKOFF_BASE * 2_u32.pow(retries);
                tokio::time::sleep(delay.mul_f64(1.0 + random_unit())).await;
                retries += 1;
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Cause, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{IngestItem, IngestPayload, NewJob, append_inbox};

    use super::*;
    use crate::context::IngestSettings;

    #[test]
    fn the_kind_name_is_ingest_drain() {
        assert_eq!(IngestDrain::KIND, "ingest.drain");
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn append(pool: &PgPool, ws: WorkspaceId, count: usize) -> TraceContext {
        let trace = TraceContext::root(Cause::api("req"));
        let items: Vec<IngestItem> = (0..count)
            .map(|n| IngestItem {
                identifiers: vec![("external:user_id".into(), format!("u{n}"))],
                timestamp: None,
                message_id: Some(format!("m-{n}")),
                payload: IngestPayload::Track {
                    event: "seen".into(),
                    properties: serde_json::Map::new(),
                },
            })
            .collect();
        append_inbox(pool, ws, Uuid::now_v7(), &trace, &items)
            .await
            .unwrap();
        trace
    }

    /// Claims the drain job the append enqueued and runs the handler on it.
    async fn run_job(
        pool: &PgPool,
        key: Option<std::sync::Arc<HmacKey>>,
        batch_size: u32,
    ) -> Result<(), JobError> {
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[IngestDrain::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(IngestSettings {
            pool: pool.clone(),
            redaction_key: key,
            batch_size,
        });
        IngestDrain.run(&mut ctx, claimed.payload.clone()).await
    }

    fn key() -> Option<std::sync::Arc<HmacKey>> {
        Some(std::sync::Arc::new(
            MasterKey::generate().hmac_key(HmacPurpose::Redaction),
        ))
    }

    async fn inbox(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM ingest_inbox")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn an_error_that_is_not_retryable_is_not_retried() {
        let mut calls = 0;

        let result: Result<(), StoreError> = retrying(|| {
            calls += 1;
            async {
                Err(zirv_flow_store::connect("not a database url", 1)
                    .await
                    .unwrap_err())
            }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn a_success_is_returned_after_one_call() {
        let mut calls = 0;

        let result = retrying(|| {
            calls += 1;
            async { Ok::<_, StoreError>(7) }
        })
        .await;

        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_drains_in_batches_until_a_short_batch(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 5).await;

        run_job(&pool, key(), 2).await.unwrap();

        assert_eq!(inbox(&pool).await, 0);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 5);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_does_not_enqueue_itself_again(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 3).await;

        run_job(&pool, key(), 1).await.unwrap();

        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jobs, 1, "only the running job itself");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retryable_error_is_retried_and_the_batch_then_applies(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 3).await;
        for statement in [
            "CREATE SEQUENCE flaky_calls",
            "CREATE FUNCTION flake() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               IF nextval('flaky_calls') <= 2 THEN \
                 RAISE EXCEPTION 'flaky' USING ERRCODE = '40001'; \
               END IF; RETURN OLD; END $$",
            "CREATE TRIGGER flaky BEFORE DELETE ON ingest_inbox \
             FOR EACH ROW EXECUTE FUNCTION flake()",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        run_job(&pool, key(), 500).await.unwrap();

        assert_eq!(inbox(&pool).await, 0);
        let calls: i64 = sqlx::query_scalar("SELECT last_value FROM flaky_calls")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(calls >= 3, "the delete ran again after each failure");
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 3, "nothing was applied twice");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_keeps_conflicting_fails_the_job_and_keeps_the_rows(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 2).await;
        for statement in [
            "CREATE FUNCTION flake() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               RAISE EXCEPTION 'flaky' USING ERRCODE = '40001'; END $$",
            "CREATE TRIGGER flaky BEFORE DELETE ON ingest_inbox \
             FOR EACH ROW EXECUTE FUNCTION flake()",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }

        let error = run_job(&pool, key(), 500).await.unwrap_err();

        assert_eq!(error, CONFLICTED);
        assert_eq!(inbox(&pool).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_row_is_rejected_and_the_rest_of_the_inbox_applies(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 4).await;
        sqlx::query("UPDATE ingest_inbox SET item = '{}' WHERE message_id = 'm-1'")
            .execute(&pool)
            .await
            .unwrap();

        run_job(&pool, key(), 500).await.unwrap();

        assert_eq!(inbox(&pool).await, 0);
        let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 3);
        let reasons: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT reason, message_id FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(reasons, [("apply_failed".into(), Some("m-1".into()))]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_without_a_master_key_fails_the_job_and_touches_nothing(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 2).await;

        let error = run_job(&pool, None, 500).await.unwrap_err();

        assert_eq!(error.message(), "the worker has no master key");
        assert_eq!(inbox(&pool).await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_without_a_workspace_fails_the_job(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let mut new = NewJob::new(IngestDrain::KIND, TraceContext::root(Cause::api("req")));
        new.payload = json!({});
        zirv_flow_store::enqueue(&pool, &new).await.unwrap();

        let error = run_job(&pool, key(), 500).await.unwrap_err();

        assert_eq!(error.message(), "the payload has no workspace_id");
    }
}
