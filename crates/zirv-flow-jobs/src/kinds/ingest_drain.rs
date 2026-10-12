//! `ingest.drain`: applies a workspace's ingestion inbox (architecture section 7.1). Each
//! batch is one transaction of its own that applies the oldest rows in id order and deletes
//! them, so the job's transaction only holds the lease.
//!
//! A run first buckets the workspace's rows that have no bucket yet (appended before the
//! column existed, or by a pre-0.29 serve), so every row joins its key's queue by id. Then
//! `shards` loops drain the buckets concurrently, one shard each; the rows of one record key
//! share a bucket, so each key still applies in id order. Each loop is a tokio task of its own,
//! owned by the handler in a `JoinSet`, so the shards spread over the runtime threads instead of
//! polling in one task. Losing the lease drops the handler, which aborts every shard task and
//! rolls back their transactions.
//!
//! A batch that fails with a retryable error is retried with jittered backoff. A batch that
//! fails otherwise holds a poison row: its rows are applied one at a time, and the row that
//! still fails becomes an `apply_failed` ingest reject, so one bad row never blocks a
//! workspace's inbox. The job never re-enqueues itself, since it still holds its unique key;
//! the scheduler leader's safety net picks up what is left.
//!
//! Nothing here logs an identifier, a value or an event name.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::task::JoinSet;
use tracing::{Instrument, Span};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{
    DRAIN_JOB_KIND, DrainOutcome, HmacKey, PgPool, Shard, StoreError, backfill_affinity,
    drain_batch, drain_batch_singly,
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
            .clone()
            .ok_or(JobError::new("the worker has no master key"))?;
        let limit = i64::from(settings.batch_size.max(1));

        let started = Instant::now();
        let pool = &settings.pool;
        if !backfill(pool, workspace, limit, started, TIME_BUDGET).await? {
            return Ok(());
        }
        let shards = settings.shards.max(1);
        // The set owns the tasks: dropping it, as a lost lease drops this future, aborts them all.
        let mut tasks = JoinSet::new();
        for index in 0..shards {
            let shard = Shard {
                index,
                count: shards,
            };
            let (pool, key) = (pool.clone(), Arc::clone(&key));
            tasks.spawn(
                async move {
                    drain_shard(&pool, workspace, &key, shard, limit, started, TIME_BUDGET).await
                }
                .instrument(Span::current()),
            );
        }
        join_shards(tasks).await
    }
}

/// Waits for every shard task to end; the batches the others committed stand. Returns the
/// first error, and a task that panicked counts as one.
async fn join_shards(mut tasks: JoinSet<Result<bool, JobError>>) -> Result<(), JobError> {
    let mut first = Ok(());
    while let Some(joined) = tasks.join_next().await {
        let result = match joined {
            Ok(result) => result.map(drop),
            Err(_) => Err(JobError::new("a drain shard panicked")),
        };
        if first.is_ok() {
            first = result;
        }
    }
    first
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

#[cfg(test)]
mod tests {
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
        run_job_with(pool, key, batch_size, 4).await
    }

    async fn run_job_with(
        pool: &PgPool,
        key: Option<std::sync::Arc<HmacKey>>,
        batch_size: u32,
        shards: u32,
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
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn unbucketed_rows_apply_before_newer_bucketed_rows_of_the_same_key(pool: PgPool) {
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
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_shard_whose_rows_are_locked_does_not_stop_the_other_shards(pool: PgPool) {
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
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_shard_drains_everything_too(pool: PgPool) {
        let ws = workspace(&pool).await;
        append(&pool, ws, 30).await;

        run_job_with(&pool, key(), 4, 1).await.unwrap();

        assert_eq!(inbox(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_an_old_serve_appended_after_a_bucketed_one_of_its_key_applies_after_it(
        pool: PgPool,
    ) {
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
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_spent_budget_ends_the_backfill_and_the_shard_loop_after_one_batch(pool: PgPool) {
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
    }

    #[tokio::test]
    async fn a_shard_task_that_panics_fails_the_job_after_the_others_ended() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        tasks.spawn(async { panic!("a shard blew up") });
        tasks.spawn({
            let finished = Arc::clone(&finished);
            async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(true)
            }
        });

        let error = join_shards(tasks).await.unwrap_err();

        assert_eq!(error.message(), "a drain shard panicked");
        assert!(
            finished.load(std::sync::atomic::Ordering::SeqCst),
            "the other task ended first"
        );
    }

    #[tokio::test]
    async fn the_first_error_is_returned_and_a_later_one_does_not_replace_it() {
        let mut tasks = JoinSet::new();
        tasks.spawn(async { Err(CONFLICTED) });
        tasks.spawn(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(JobError::new("later"))
        });

        assert_eq!(join_shards(tasks).await.unwrap_err(), CONFLICTED);
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
    async fn dropping_the_handler_aborts_every_shard_task(pool: PgPool) {
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
                "a shard still holds a connection"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let left = inbox(&pool).await;
        assert!(left > 0, "the run was cut off with rows remaining");
        // A shard that outlived the handler, even one asleep in its longest retry backoff
        // (400 ms), would commit another one-row batch within this window.
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            inbox(&pool).await,
            left,
            "no shard committed after the drop"
        );
        assert_eq!(
            busy_connections(&pool).await,
            0,
            "no query is still running"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_shard_fails_the_job_after_the_others_drained_their_rows(pool: PgPool) {
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
    }
}
