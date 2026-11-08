//! `ingest.drain`: applies one shard of a workspace's ingestion inbox (architecture section 7.1).
//! The payload names the shard, `index` of `count`, and the job's unique key is per workspace and
//! shard, so a workspace's drain is several jobs that spread over the workers' drain slots. Each
//! batch is one transaction of its own that applies the oldest rows of the shard's buckets in id
//! order and deletes them, so the job's transaction only confirms the lease, with no row lock,
//! and completes it.
//!
//! A run first buckets the workspace's rows that have no bucket yet (appended before the column
//! existed, or by a pre-0.29 serve), so every row joins its key's queue by id. Its batches lock
//! the rows they bucket, so jobs that start together share the work, and a run that finds fewer
//! rows than a batch has seen them all bucketed: no shard starts while an older row has no
//! bucket. Then the run loops batches
//! over the shard's buckets until they are empty or the time budget ends. The rows of one record
//! key share a bucket, so each key applies in id order even when two jobs cover the same bucket:
//! a batch locks the rows it takes, skips locked ones and leaves out a bucket's rows after a
//! missing one.
//!
//! The run drains the buckets its payload names, whatever shard count the worker is configured
//! with: a count that differs, in a config change or a rolling deploy, still drains the rows it
//! was enqueued for, and the worker's own requeue and the scheduler's safety net enqueue by the
//! configured count, so any bucket left over gets a job of the new count and the two converge.
//!
//! A payload without a shard, queued before the drain was split, enqueues the configured
//! count's jobs for the shards that hold rows and completes.
//!
//! A batch that fails with a retryable error is retried with jittered backoff. A batch that
//! fails otherwise holds a poison row: its rows are applied one at a time, and the row that
//! still fails becomes an `apply_failed` ingest reject, so one bad row never blocks a
//! workspace's inbox. The job never re-enqueues itself, since it still holds its unique key; the
//! worker's requeue after completion and the scheduler leader's safety net pick up what is left.
//!
//! Nothing here logs an identifier, a value or an event name.

use std::future::Future;
use std::time::{Duration, Instant};

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    DRAIN_JOB_KIND, DrainOutcome, HmacKey, PgPool, Shard, StoreError, backfill_affinity,
    drain_batch, drain_batch_singly, enqueue_pending_drains, payload_shard,
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

    /// A run writes nothing in its job transaction, which would otherwise hold a transaction id,
    /// and so the xmin horizon, for the whole time budget (#397). A late run that overlaps a new
    /// one is safe without the row lock: each batch locks the inbox rows it takes, applies and
    /// deletes them in one transaction, and leaves out a bucket's rows after one another batch
    /// holds, so no row applies twice and no key out of order, as between this run's own shards.
    /// No erasure marks a drain job, and the completion stays fenced on the lease token.
    const LOCKS_LEASE: bool = false;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let workspace = payload
            .get("workspace_id")
            .and_then(Value::as_str)
            .and_then(|text| Uuid::parse_str(text).ok())
            .map(WorkspaceId::from_uuid)
            .ok_or(JobError::new("the payload has no workspace_id"))?;
        let shard = payload_shard(&payload).map_err(JobError::new)?;
        let settings = ctx
            .ingest()
            .ok_or(JobError::new("the worker has no pool for the ingest drain"))?;
        let pool = &settings.pool;
        let Some(shard) = shard else {
            enqueue_pending_drains(pool, workspace, settings.shards, ctx.trace())
                .await
                .map_err(|_| JobError::new("the shard jobs could not be enqueued"))?;
            return Ok(());
        };
        let key = settings
            .redaction_key
            .clone()
            .ok_or(JobError::new("the worker has no master key"))?;
        let limit = i64::from(settings.batch_size.max(1));

        let started = Instant::now();
        if !backfill(pool, workspace, limit, started, TIME_BUDGET).await? {
            return Ok(());
        }
        drain_shard(pool, workspace, &key, shard, limit, started, TIME_BUDGET)
            .await
            .map(drop)
    }
}

/// Buckets the workspace's unbucketed rows until none are left. Returns whether it finished
/// before `budget` ran out; the shards must not start while older rows have no bucket.
async fn backfill(
    pool: &PgPool,
    workspace: WorkspaceId,
    limit: i64,
    started: Instant,
    budget: Duration,
) -> Result<bool, JobError> {
    loop {
        let done = retrying(|| backfill_affinity(pool, workspace, limit))
            .await
            .map_err(|error| {
                if error.is_retryable() {
                    CONFLICTED
                } else {
                    JobError::new("the inbox rows could not be bucketed")
                }
            })?;
        if i64::try_from(done).is_ok_and(|done| done < limit) {
            return Ok(true);
        }
        if started.elapsed() >= budget {
            return Ok(false);
        }
    }
}

/// Drains one shard until its buckets are empty or `budget` runs out. Returns whether the
/// shard was left empty.
async fn drain_shard(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    shard: Shard,
    limit: i64,
    started: Instant,
    budget: Duration,
) -> Result<bool, JobError> {
    loop {
        let outcome = drain_once(pool, workspace, key, shard, limit).await?;
        record_drained(&outcome);
        if !outcome.more {
            return Ok(true);
        }
        if started.elapsed() >= budget {
            return Ok(false);
        }
    }
}

/// One batch: whole when it can be, row by row when a row is poison.
async fn drain_once(
    pool: &PgPool,
    workspace: WorkspaceId,
    key: &HmacKey,
    shard: Shard,
    limit: i64,
) -> Result<DrainOutcome, JobError> {
    match retrying(|| drain_batch(pool, workspace, shard, key, limit)).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.is_retryable() => Err(CONFLICTED),
        Err(_) => retrying(|| drain_batch_singly(pool, workspace, shard, key, limit))
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
pub(super) async fn retrying<T, F, Fut>(mut operation: F) -> Result<T, StoreError>
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

/// Runs a batch commit whole, and again with `singly` set when it fails with an error that
/// retrying cannot cure. `conflicted` is the failure for a conflict that outlasts the retries;
/// `unapplied` is the one for a batch that cannot be applied even record by record.
pub(super) async fn whole_then_singly<T, F, Fut>(
    conflicted: JobError,
    unapplied: &'static str,
    mut commit: F,
) -> Result<T, JobError>
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Result<T, StoreError>>,
{
    match retrying(|| commit(false)).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error.is_retryable() => Err(conflicted),
        Err(_) => retrying(|| commit(true)).await.map_err(|error| {
            if error.is_retryable() {
                conflicted
            } else {
                JobError::new(unapplied)
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_store::decision_testing::PERSONAL_DATA;

    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
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
                    properties: PERSONAL_DATA.properties(),
                },
            })
            .collect();
        append_inbox(pool, ws, Uuid::now_v7(), &trace, &items)
            .await
            .unwrap();
        trace
    }

    /// Claims every ready drain job the appends enqueued, one per shard the rows fall in, and
    /// runs the handler on each in turn. Returns the first error, after running them all.
    async fn run_job(
        pool: &PgPool,
        key: Option<std::sync::Arc<HmacKey>>,
        batch_size: u32,
    ) -> Result<(), JobError> {
        run_job_with(pool, key, batch_size, 4).await
    }

    async fn run_job_with(
        pool: &PgPool,
        key: Option<std::sync::Arc<HmacKey>>,
        batch_size: u32,
        shards: u32,
    ) -> Result<(), JobError> {
        let mut first = Ok(());
        for claimed in claim_all(pool).await {
            let result = run_claimed(pool, &claimed, key.clone(), batch_size, shards).await;
            if first.is_ok() {
                first = result;
            }
        }
        first
    }

    /// Claims every ready drain job. A claim takes at most one, so it claims until none is left.
    async fn claim_all(pool: &PgPool) -> Vec<zirv_flow_store::ClaimedJob> {
        let mut claimed = Vec::new();
        loop {
            let batch = zirv_flow_store::claim_jobs(
                pool,
                "w",
                &[IngestDrain::KIND],
                Duration::from_secs(30),
                None,
                100,
            )
            .await
            .unwrap();
            if batch.is_empty() {
                return claimed;
            }
            claimed.extend(batch);
        }
    }

    /// Runs the handler on `claimed`; `shards` is the count the worker is configured with.
    async fn run_claimed(
        pool: &PgPool,
        claimed: &zirv_flow_store::ClaimedJob,
        key: Option<std::sync::Arc<HmacKey>>,
        batch_size: u32,
        shards: u32,
    ) -> Result<(), JobError> {
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, claimed).with_ingest(IngestSettings {
            pool: pool.clone(),
            redaction_key: key,
            batch_size,
            shards,
        });
        IngestDrain.run(&mut ctx, claimed.payload.clone()).await
    }

    fn key() -> Option<std::sync::Arc<HmacKey>> {
        Some(std::sync::Arc::new(
            HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction),
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
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 5).await;

            run_job(&pool, key(), 2).await.unwrap();

            assert_eq!(inbox(&pool).await, 0);
            let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(events, 5);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_does_not_enqueue_itself_again(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 3).await;
            let count = || async {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM jobs WHERE kind = 'ingest.drain'",
                )
                .fetch_one(&pool)
                .await
                .unwrap()
            };
            let enqueued = count().await;
            assert!(enqueued >= 1);

            run_job(&pool, key(), 1).await.unwrap();

            assert_eq!(count().await, enqueued, "only the running jobs themselves");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retryable_error_is_retried_and_the_batch_then_applies(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_keeps_conflicting_fails_the_job_and_keeps_the_rows(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_row_is_rejected_and_the_rest_of_the_inbox_applies(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
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
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_without_a_master_key_fails_the_job_and_touches_nothing(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 2).await;

            let error = run_job(&pool, None, 500).await.unwrap_err();

            assert_eq!(error.message(), "the worker has no master key");
            assert_eq!(inbox(&pool).await, 2);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_without_a_workspace_fails_the_job(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let mut new = NewJob::new(IngestDrain::KIND, TraceContext::root(Cause::api("req")));
            new.payload = json!({});
            zirv_flow_store::enqueue(&pool, &new).await.unwrap();

            let error = run_job(&pool, key(), 500).await.unwrap_err();

            assert_eq!(error.message(), "the payload has no workspace_id");
        })
        .await;
    }

    fn identify(user: &str, message_id: &str, plan: &str) -> IngestItem {
        IngestItem {
            identifiers: vec![("external:user_id".into(), user.into())],
            timestamp: None,
            message_id: Some(message_id.into()),
            payload: IngestPayload::Identify {
                traits: json!({ "plan": plan }).as_object().unwrap().clone(),
            },
        }
    }

    async fn append_items(pool: &PgPool, ws: WorkspaceId, items: &[IngestItem]) {
        let trace = TraceContext::root(Cause::api("req"));
        append_inbox(pool, ws, Uuid::now_v7(), &trace, items)
            .await
            .unwrap();
    }

    async fn plan_of(pool: &PgPool, user: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT p.traits->>'plan' FROM people p JOIN identities i ON i.person_id = p.id \
             WHERE i.value = $1",
        )
        .bind(user)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn four_shards_drain_a_backlog_completely_to_the_last_value(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            let mut items = Vec::new();
            for round in ["free", "pro"] {
                for n in 0..200 {
                    items.push(identify(&format!("u{n}"), &format!("{round}-{n}"), round));
                }
            }
            append_items(&pool, ws, &items).await;

            run_job_with(&pool, key(), 7, 4).await.unwrap();

            assert_eq!(inbox(&pool).await, 0);
            for n in [0, 17, 199] {
                assert_eq!(
                    plan_of(&pool, &format!("u{n}")).await.as_deref(),
                    Some("pro")
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unbucketed_rows_apply_before_newer_bucketed_rows_of_the_same_key(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append_items(&pool, ws, &[identify("k", "old", "older")]).await;
            sqlx::query("UPDATE ingest_inbox SET affinity = NULL")
                .execute(&pool)
                .await
                .unwrap();
            append_items(&pool, ws, &[identify("k", "new", "newer")]).await;

            run_job_with(&pool, key(), 500, 4).await.unwrap();

            assert_eq!(inbox(&pool).await, 0);
            assert_eq!(plan_of(&pool, "k").await.as_deref(), Some("newer"));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shard_whose_rows_are_locked_does_not_stop_the_other_shards(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            let items: Vec<IngestItem> = (0..80)
                .map(|n| identify(&format!("u{n}"), &format!("m-{n}"), "free"))
                .collect();
            append_items(&pool, ws, &items).await;
            let mut holder = zirv_flow_store::begin(&pool).await.unwrap();
            let locked: Vec<i64> = sqlx::query_scalar(
                "SELECT id FROM ingest_inbox WHERE affinity % 4 = 0 ORDER BY id FOR UPDATE",
            )
            .fetch_all(&mut *holder)
            .await
            .unwrap();
            assert!(!locked.is_empty());

            run_job_with(&pool, key(), 5, 4).await.unwrap();

            let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM ingest_inbox ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
            assert_eq!(left, locked, "only the locked shard's rows remain");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_an_old_serve_appended_after_a_bucketed_one_of_its_key_applies_after_it(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append_items(&pool, ws, &[identify("k", "first", "old")]).await;
            append_items(&pool, ws, &[identify("k", "second", "new")]).await;
            sqlx::query("UPDATE ingest_inbox SET affinity = NULL WHERE message_id = 'second'")
                .execute(&pool)
                .await
                .unwrap();

            run_job_with(&pool, key(), 500, 4).await.unwrap();

            assert_eq!(inbox(&pool).await, 0);
            assert_eq!(plan_of(&pool, "k").await.as_deref(), Some("new"));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_spent_budget_ends_the_backfill_and_the_shard_loop_after_one_batch(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            let items: Vec<IngestItem> = (0..30)
                .map(|n| identify(&format!("u{n}"), &format!("m-{n}"), "free"))
                .collect();
            append_items(&pool, ws, &items).await;
            sqlx::query("UPDATE ingest_inbox SET affinity = NULL")
                .execute(&pool)
                .await
                .unwrap();
            let spent = Duration::ZERO;

            let finished = backfill(&pool, ws, 10, Instant::now(), spent)
                .await
                .unwrap();

            assert!(!finished, "ten of thirty rows are bucketed");
            let unbucketed: i64 =
                sqlx::query_scalar("SELECT count(*) FROM ingest_inbox WHERE affinity IS NULL")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(unbucketed, 20);
            assert!(
                backfill(&pool, ws, 10, Instant::now(), Duration::from_secs(60))
                    .await
                    .unwrap()
            );

            // One bucket with more rows than a batch holds.
            sqlx::query("UPDATE ingest_inbox SET affinity = 0")
                .execute(&pool)
                .await
                .unwrap();
            let hmac = key().unwrap();
            let finished = drain_shard(
                &pool,
                ws,
                &hmac,
                Shard { index: 0, count: 1 },
                10,
                Instant::now(),
                spent,
            )
            .await
            .unwrap();

            assert!(!finished);
            assert!(inbox(&pool).await > 0, "one batch is not the backlog");
        })
        .await;
    }

    /// Queries that are not idle on other connections of the test's database.
    async fn busy_connections(pool: &PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
             AND pid <> pg_backend_pid() AND state <> 'idle' AND backend_type = 'client backend'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dropping_the_handler_aborts_the_drain(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 4000).await;
            let total = inbox(&pool).await;
            let handler = tokio::spawn({
                let pool = pool.clone();
                async move { run_job_with(&pool, key(), 1, 4).await }
            });
            while inbox(&pool).await == total {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }

            handler.abort();
            assert!(handler.await.unwrap_err().is_cancelled());

            // Every connection must come back: a shard that outlived the handler would hold one.
            let deadline = Instant::now() + Duration::from_secs(10);
            while pool.num_idle() != pool.size() as usize {
                assert!(
                    Instant::now() < deadline,
                    "the drain still holds a connection"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The client side lets go at once, but PostgreSQL only notices a dropped client when
            // the statement it is running ends, so give the server a bounded time to finish
            // with the abandoned backends before the baseline is read.
            let deadline = Instant::now() + Duration::from_secs(30);
            while busy_connections(&pool).await != 0 {
                assert!(
                    Instant::now() < deadline,
                    "no query is still running: a backend stayed busy after the drop"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let left = inbox(&pool).await;
            assert!(left > 0, "the run was cut off with rows remaining");
            // A drain that outlived the handler, even one asleep in its longest retry backoff
            // (400 ms), would commit another one-row batch within this window.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                inbox(&pool).await,
                left,
                "no batch committed after the drop"
            );
            assert_eq!(
                busy_connections(&pool).await,
                0,
                "no query was started after the drop"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_shard_fails_the_job_after_the_others_drained_their_rows(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            let items: Vec<IngestItem> = (0..80)
                .map(|n| identify(&format!("u{n}"), &format!("m-{n}"), "free"))
                .collect();
            append_items(&pool, ws, &items).await;
            for statement in [
                "CREATE FUNCTION flake() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               IF OLD.affinity % 4 = 0 THEN \
                 RAISE EXCEPTION 'flaky' USING ERRCODE = '40001'; \
               END IF; RETURN OLD; END $$",
                "CREATE TRIGGER flaky BEFORE DELETE ON ingest_inbox \
             FOR EACH ROW EXECUTE FUNCTION flake()",
            ] {
                sqlx::query(statement).execute(&pool).await.unwrap();
            }
            let failing: i64 =
                sqlx::query_scalar("SELECT count(*) FROM ingest_inbox WHERE affinity % 4 = 0")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert!(failing > 0 && failing < 80);

            let error = run_job_with(&pool, key(), 5, 4).await.unwrap_err();

            assert_eq!(error, CONFLICTED);
            assert_eq!(
                inbox(&pool).await,
                failing,
                "the other shards drained theirs"
            );
        })
        .await;
    }

    /// The shard indexes (of `count`) the inbox's rows fall in.
    async fn shards_with_rows(pool: &PgPool, count: i32) -> Vec<i32> {
        sqlx::query_scalar("SELECT DISTINCT affinity::int % $1 FROM ingest_inbox ORDER BY 1")
            .bind(count)
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_without_a_shard_enqueues_the_shard_jobs_that_hold_rows_and_completes(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 12).await;
            sqlx::query("DELETE FROM jobs")
                .execute(&pool)
                .await
                .unwrap();
            let mut old = NewJob::new(IngestDrain::KIND, TraceContext::root(Cause::api("req")));
            old.workspace_id = Some(ws);
            old.payload = json!({ "workspace_id": ws.as_uuid() });
            old.unique_key = Some(format!("ingest.drain:{}", ws.as_uuid()));
            zirv_flow_store::enqueue(&pool, &old).await.unwrap();
            let claimed = claim_all(&pool).await;
            assert_eq!(claimed.len(), 1);

            run_claimed(&pool, &claimed[0], key(), 50, 4).await.unwrap();

            assert_eq!(inbox(&pool).await, 12, "the old job drains nothing itself");
            let mut keys: Vec<String> = sqlx::query_scalar(
                "SELECT unique_key FROM jobs WHERE state = 'ready' ORDER BY unique_key",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            let mut expected: Vec<String> = shards_with_rows(&pool, 4)
                .await
                .iter()
                .map(|index| format!("ingest.drain:{}:{index}/4", ws.as_uuid()))
                .collect();
            keys.sort();
            expected.sort();
            assert_eq!(keys, expected);

            run_job(&pool, key(), 50).await.unwrap();
            assert_eq!(inbox(&pool).await, 0, "the shard jobs drain what is left");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_drains_the_buckets_its_payload_names_whatever_count_the_worker_has(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            append(&pool, ws, 60).await;
            let claimed = claim_all(&pool).await;
            let first = claimed
                .iter()
                .find(|job| job.payload["shard"]["index"] == 0)
                .expect("a job for shard 0 of 8");
            assert_eq!(first.payload["shard"]["count"], 8);
            let before = inbox(&pool).await;

            run_claimed(&pool, first, key(), 500, 3).await.unwrap();

            let left: Vec<i16> =
                sqlx::query_scalar("SELECT affinity FROM ingest_inbox ORDER BY id")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert!(i64::try_from(left.len()).unwrap() < before);
            assert!(
                left.iter().all(|bucket| bucket % 8 != 0),
                "every bucket of shard 0 of 8 is empty"
            );
            assert!(
                left.iter().any(|bucket| bucket % 3 == 0),
                "the worker's own three shards were not drained"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_payload_naming_a_shard_that_owns_no_buckets_fails_the_job(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            for (index, count) in [(0, 0), (4, 4), (0, 65)] {
                sqlx::query("DELETE FROM jobs")
                    .execute(&pool)
                    .await
                    .unwrap();
                let mut new = NewJob::new(IngestDrain::KIND, TraceContext::root(Cause::api("r")));
                new.payload = json!({
                    "workspace_id": ws.as_uuid(),
                    "shard": { "index": index, "count": count },
                });
                zirv_flow_store::enqueue(&pool, &new).await.unwrap();

                let error = run_job(&pool, key(), 50).await.unwrap_err();

                assert_eq!(
                    error.message(),
                    "the payload names a shard that owns no buckets"
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn jobs_of_different_counts_over_the_same_buckets_keep_every_key_in_order(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            let mut items = Vec::new();
            for round in ["free", "basic", "pro"] {
                for n in 0..100 {
                    items.push(identify(&format!("u{n}"), &format!("{round}-{n}"), round));
                }
            }
            append_items(&pool, ws, &items).await;
            let trace = TraceContext::root(Cause::api("req"));
            zirv_flow_store::enqueue_pending_drains(&pool, ws, 4, &trace)
                .await
                .unwrap();
            let claimed = claim_all(&pool).await;
            let counts: std::collections::BTreeSet<i64> = claimed
                .iter()
                .filter_map(|job| job.payload["shard"]["count"].as_i64())
                .collect();
            assert_eq!(
                counts,
                [4, 8].into(),
                "jobs of both counts cover the buckets"
            );

            // Neighbours in this order share buckets; the permits keep the test pool from running
            // dry (a job holds its transaction and a batch).
            let mut claimed = claimed;
            claimed.sort_by_key(|job| {
                (
                    job.payload["shard"]["index"].as_i64(),
                    job.payload["shard"]["count"].as_i64(),
                )
            });
            let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(3));
            let mut runs = tokio::task::JoinSet::new();
            for job in claimed {
                let (pool, permits) = (pool.clone(), std::sync::Arc::clone(&permits));
                runs.spawn(async move {
                    let _permit = permits.acquire().await.unwrap();
                    run_claimed(&pool, &job, key(), 7, 8).await
                });
            }
            while let Some(done) = runs.join_next().await {
                done.unwrap().unwrap();
            }
            // Batches that found their rows locked by the other count's job took nothing and
            // left them: finish what is left the way the requeue would.
            while inbox(&pool).await > 0 {
                sqlx::query("DELETE FROM jobs")
                    .execute(&pool)
                    .await
                    .unwrap();
                zirv_flow_store::enqueue_pending_drains(&pool, ws, 8, &trace)
                    .await
                    .unwrap();
                run_job(&pool, key(), 7).await.unwrap();
            }

            for n in 0..100 {
                assert_eq!(
                    plan_of(&pool, &format!("u{n}")).await.as_deref(),
                    Some("pro"),
                    "u{n} ended on its last value"
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_behind_a_jobs_last_empty_check_leaves_that_shard_a_runnable_job(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool).await;
            // Draining a first row makes k a known person, so the next two rows share its lane.
            append_items(&pool, ws, &[identify("k", "zeroth", "oldest")]).await;
            for job in claim_all(&pool).await {
                run_claimed(&pool, &job, key(), 50, 8).await.unwrap();
                assert!(
                    zirv_flow_store::complete_job(&pool, job.id, job.lease_token)
                        .await
                        .unwrap()
                );
            }
            append_items(&pool, ws, &[identify("k", "first", "old")]).await;
            let claimed = claim_all(&pool).await;
            assert_eq!(claimed.len(), 1);
            let job = &claimed[0];
            let shard = zirv_flow_store::payload_shard(&job.payload)
                .unwrap()
                .unwrap();

            run_claimed(&pool, job, key(), 50, 8).await.unwrap();
            assert_eq!(inbox(&pool).await, 0, "the last batch emptied the shard");
            // The row lands after the final empty check, while the job still holds its key.
            append_items(&pool, ws, &[identify("k", "second", "new")]).await;
            let ready: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'ready'")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                ready, 0,
                "the running job's key dropped the append's enqueue"
            );

            assert!(
                zirv_flow_store::complete_job(&pool, job.id, job.lease_token)
                    .await
                    .unwrap()
            );
            let trace = TraceContext::root(Cause::api("req"));
            assert!(
                zirv_flow_store::enqueue_drain_if_pending(&pool, ws, shard, &trace)
                    .await
                    .unwrap(),
                "the requeue after completion finds the row and enqueues the shard"
            );

            run_job(&pool, key(), 50).await.unwrap();
            assert_eq!(plan_of(&pool, "k").await.as_deref(), Some("new"));
        })
        .await;
    }

    /// A mapped, enabled hook of the workspace, built through the store's public surface.
    async fn hook(pool: &PgPool, ws: WorkspaceId) -> zirv_flow_store::Webhook {
        use zirv_flow_core::{Keyring, SecretId, SecretPurpose, seal, secret_aad};
        use zirv_flow_store::{
            DiscoveredStream, NewSource, NewWebhook, StreamSettings, configure_stream,
            create_source, create_webhook, set_stream_mapping, upsert_discovered_streams,
        };

        let keys = Keyring::new(MasterKey::generate(), None).unwrap();
        let secret = SecretId::new();
        let sealed = seal(
            &keys,
            b"zfwh_test",
            &secret_aad(ws, secret, SecretPurpose::WebhookSecret),
        );
        zirv_flow_store::secrets::insert_unchecked(
            pool,
            ws,
            SecretPurpose::WebhookSecret,
            secret,
            &sealed,
        )
        .await
        .unwrap();
        let config = json!({});
        let source = create_source(
            pool,
            ws,
            &NewSource {
                id: Uuid::now_v7(),
                kind: "webhook",
                name: "orders",
                config: &config,
                credential_id: None,
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let descriptor = json!({});
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let streams = upsert_discovered_streams(
            &mut tx,
            ws,
            source.id,
            &[DiscoveredStream {
                name: "payload",
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                descriptor: &descriptor,
                suggested_mapping: None,
            }],
        )
        .await
        .unwrap()
        .unwrap();
        let created = create_webhook(
            &mut tx,
            ws,
            &NewWebhook {
                id: Uuid::new_v4(),
                source_id: source.id,
                stream_id: streams[0].id,
                secret_id: secret,
                signature_header: "X-Zirv-Signature",
                signature_prefix: "sha256=",
                timestamp_header: None,
                tolerance_seconds: 300,
                dedupe_field: "id",
                created_by: None,
            },
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        configure_stream(
            &mut connection,
            ws,
            source.id,
            "payload",
            &StreamSettings {
                enabled: true,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: None,
                reconcile_schedule: None,
                sweep_limit: 0.5,
            },
        )
        .await
        .unwrap();
        let mapping = json!({
            "columns": [{"column": "email", "target": {"type": "identifier", "kind": "email"}}],
            "event": {"name": "order_placed", "properties": []}
        });
        set_stream_mapping(pool, ws, source.id, "payload", &mapping)
            .await
            .unwrap();
        created
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_row_drains_beside_track_rows_and_is_counted_under_its_kind(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            use metrics_exporter_prometheus::PrometheusBuilder;

            let ws = workspace(&pool).await;
            append(&pool, ws, 2).await;
            let created = hook(&pool, ws).await;
            let receive = zirv_flow_store::find_for_receive(&pool, created.id)
                .await
                .unwrap()
                .unwrap();
            let trace = TraceContext::root(Cause::api("hook"));
            let payload = json!({"id": "o-1", "email": PERSONAL_DATA.email});
            zirv_flow_store::append_webhook(
                &pool,
                &receive,
                &trace,
                &payload,
                None,
                &format!("wh:{}:o-1", created.id),
                true,
            )
            .await
            .unwrap();
            let recorder = PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let guard = metrics::set_default_local_recorder(&recorder);

            run_job(&pool, key(), 500).await.unwrap();
            drop(guard);

            assert_eq!(inbox(&pool).await, 0);
            let events: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(events, 3);
            let output = handle.render();
            assert!(
                output.contains(
                    "zirv_flow_ingest_applied_total{kind=\"webhook\",outcome=\"applied\"} 1"
                ),
                "{output}"
            );
            assert!(
                output.contains(
                    "zirv_flow_ingest_applied_total{kind=\"webhook\",outcome=\"accepted\"} 1"
                ),
                "{output}"
            );
            assert!(!output.contains(PERSONAL_DATA.email), "{output}");
            assert!(!output.contains(&created.id.to_string()), "{output}");
        })
        .await;
    }
}
