//! The durable job queue and recurring schedules (architecture section 14).
//!
//! Every write a worker makes to a claimed job is fenced on its lease token, so a worker
//! whose lease expired can change nothing.

use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::PgListener;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use crate::{PgPool, StoreError};

/// The channel enqueue notifies; the payload is the job kind.
pub const NOTIFY_CHANNEL: &str = "zirv_flow_jobs";
/// Attempts a job gets unless it says otherwise.
pub const DEFAULT_MAX_ATTEMPTS: i32 = 5;
/// The longest `last_error` kept, in bytes.
const MAX_ERROR_BYTES: usize = 2048;
/// The advisory-lock key of the scheduler leader.
const LEADER_LOCK_KEY: i64 = 0x7a69_7276_6a6f_6273;
/// Serialises claims while a per-workspace cap is on, so the cap cannot be overshot.
const CLAIM_LOCK_KEY: i64 = 0x7a69_7276_636c_6d73;

/// A job to enqueue.
#[derive(Clone, Debug)]
pub struct NewJob {
    pub workspace_id: Option<WorkspaceId>,
    pub kind: String,
    pub payload: Value,
    /// Lower numbers are claimed first.
    pub priority: i16,
    /// When the job becomes runnable; the database's `now()` when absent.
    pub run_at: Option<OffsetDateTime>,
    pub max_attempts: i32,
    /// At most one ready or running job holds a key; a second enqueue is a no-op.
    pub unique_key: Option<String>,
    /// The trace the job belongs to and what caused it.
    pub trace: TraceContext,
}

impl NewJob {
    pub fn new(kind: impl Into<String>, trace: TraceContext) -> Self {
        Self {
            workspace_id: None,
            kind: kind.into(),
            payload: Value::Object(serde_json::Map::new()),
            priority: 0,
            run_at: None,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            unique_key: None,
            trace,
        }
    }
}

/// A job as stored.
#[derive(Clone, Debug, FromRow)]
pub struct Job {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub payload: Value,
    pub priority: i16,
    pub run_at: OffsetDateTime,
    pub state: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_token: Option<Uuid>,
    pub lease_expires_at: Option<OffsetDateTime>,
    pub locked_by: Option<String>,
    pub unique_key: Option<String>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
    pub trace_id: Uuid,
    pub cause_kind: String,
    pub cause_id: String,
    pub dead_at: Option<OffsetDateTime>,
}

/// A job a worker has claimed. `attempts` already counts this attempt.
#[derive(Clone, Debug)]
pub struct ClaimedJob {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub payload: Value,
    pub attempts: i32,
    pub max_attempts: i32,
    pub lease_token: Uuid,
    pub unique_key: Option<String>,
    pub trace: TraceContext,
    /// How long the job waited past its `run_at`, by the database's clock, when claimed.
    pub claim_lag_seconds: f64,
}

#[derive(FromRow)]
struct ClaimedRow {
    id: Uuid,
    workspace_id: Option<Uuid>,
    kind: String,
    payload: Value,
    attempts: i32,
    max_attempts: i32,
    lease_token: Uuid,
    unique_key: Option<String>,
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
    claim_lag_seconds: f64,
}

impl ClaimedRow {
    fn into_claimed(self) -> Result<ClaimedJob, StoreError> {
        let kind: CauseKind = self
            .cause_kind
            .parse()
            .map_err(|error| StoreError::query(sqlx::Error::Decode(Box::new(error))))?;
        Ok(ClaimedJob {
            id: self.id,
            workspace_id: self.workspace_id,
            kind: self.kind,
            payload: self.payload,
            attempts: self.attempts,
            max_attempts: self.max_attempts,
            lease_token: self.lease_token,
            unique_key: self.unique_key,
            trace: TraceContext {
                trace_id: TraceId::from_uuid(self.trace_id),
                cause: Cause::from_stored(kind, self.cause_id),
            },
            claim_lag_seconds: self.claim_lag_seconds.max(0.0),
        })
    }
}

/// What a fenced failure did to the job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailOutcome {
    /// Back to `ready`, to run again after the backoff.
    Retried,
    /// Out of attempts.
    Dead,
    /// An erasure named the job while it ran, so it was deleted and not put back.
    Purged,
}

/// What a reaper pass did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Reaped {
    pub requeued: u64,
    pub dead: u64,
    /// Expired jobs an erasure had marked, deleted instead of being requeued or buried.
    pub purged: u64,
}

/// How many jobs of one kind are in one state.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct QueueDepth {
    pub kind: String,
    pub state: String,
    pub count: i64,
}

/// How many jobs are in each state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueueCounts {
    pub ready: i64,
    pub running: i64,
    pub dead: i64,
}

/// A recurring schedule.
#[derive(Clone, Debug, FromRow)]
pub struct Schedule {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub cron: String,
    pub timezone: String,
    pub next_run_at: OffsetDateTime,
    pub payload: Value,
    pub enabled: bool,
}

/// Enqueues a job; returns whether a row was inserted (false when its unique key is held).
///
/// Pass the caller's transaction so the job commits with the change that caused it. The
/// wake-up notification is sent in that transaction, so it is delivered on commit.
pub async fn enqueue<'e>(executor: impl PgExecutor<'e>, job: &NewJob) -> Result<bool, StoreError> {
    let notified = sqlx::query(
        "WITH inserted AS ( \
             INSERT INTO jobs \
                (id, workspace_id, kind, payload, priority, run_at, max_attempts, unique_key, \
                 trace_id, cause_kind, cause_id) \
             VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, $8, $9, $10, $11) \
             ON CONFLICT DO NOTHING \
             RETURNING kind) \
         SELECT pg_notify($12, kind) FROM inserted",
    )
    .bind(Uuid::new_v4())
    .bind(job.workspace_id.map(|id| id.as_uuid()))
    .bind(&job.kind)
    .bind(&job.payload)
    .bind(job.priority)
    .bind(job.run_at)
    .bind(job.max_attempts)
    .bind(job.unique_key.as_deref())
    .bind(job.trace.trace_id.as_uuid())
    .bind(job.trace.cause.kind.as_str())
    .bind(&job.trace.cause.id)
    .bind(NOTIFY_CHANNEL)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(notified.is_some())
}

/// Enqueues many jobs with one statement; returns how many rows were inserted (a job whose unique
/// key is held is skipped, as in [`enqueue`]). One wake-up notification goes out per kind, in the
/// caller's transaction.
pub async fn enqueue_many<'e>(
    executor: impl PgExecutor<'e>,
    jobs: &[NewJob],
) -> Result<u64, StoreError> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let ids: Vec<Uuid> = jobs.iter().map(|_| Uuid::new_v4()).collect();
    let workspaces: Vec<Option<Uuid>> = jobs
        .iter()
        .map(|job| job.workspace_id.map(|id| id.as_uuid()))
        .collect();
    let kinds: Vec<&str> = jobs.iter().map(|job| job.kind.as_str()).collect();
    let payloads: Vec<&Value> = jobs.iter().map(|job| &job.payload).collect();
    let priorities: Vec<i16> = jobs.iter().map(|job| job.priority).collect();
    let run_ats: Vec<Option<OffsetDateTime>> = jobs.iter().map(|job| job.run_at).collect();
    let attempts: Vec<i32> = jobs.iter().map(|job| job.max_attempts).collect();
    let keys: Vec<Option<&str>> = jobs.iter().map(|job| job.unique_key.as_deref()).collect();
    let traces: Vec<Uuid> = jobs
        .iter()
        .map(|job| job.trace.trace_id.as_uuid())
        .collect();
    let cause_kinds: Vec<&str> = jobs
        .iter()
        .map(|job| job.trace.cause.kind.as_str())
        .collect();
    let cause_ids: Vec<&str> = jobs.iter().map(|job| job.trace.cause.id.as_str()).collect();
    let inserted: Vec<String> = sqlx::query_scalar(
        "WITH inserted AS ( \
             INSERT INTO jobs \
                (id, workspace_id, kind, payload, priority, run_at, max_attempts, unique_key, \
                 trace_id, cause_kind, cause_id) \
             SELECT t.id, t.workspace_id, t.kind, t.payload, t.priority, COALESCE(t.run_at, now()), \
                    t.max_attempts, t.unique_key, t.trace_id, t.cause_kind, t.cause_id \
             FROM unnest($1::uuid[], $2::uuid[], $3::text[], $4::jsonb[], $5::int2[], \
                         $6::timestamptz[], $7::int4[], $8::text[], $9::uuid[], $10::text[], \
                         $11::text[]) \
                  AS t(id, workspace_id, kind, payload, priority, run_at, max_attempts, \
                       unique_key, trace_id, cause_kind, cause_id) \
             ON CONFLICT DO NOTHING \
             RETURNING kind), \
         notified AS (SELECT pg_notify($12, k.kind) FROM (SELECT DISTINCT kind FROM inserted) k) \
         SELECT kind FROM inserted, (SELECT count(*) FROM notified) AS n",
    )
    .bind(&ids)
    .bind(&workspaces)
    .bind(&kinds)
    .bind(&payloads)
    .bind(&priorities)
    .bind(&run_ats)
    .bind(&attempts)
    .bind(&keys)
    .bind(&traces)
    .bind(&cause_kinds)
    .bind(&cause_ids)
    .bind(NOTIFY_CHANNEL)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(u64::try_from(inserted.len()).unwrap_or(u64::MAX))
}

/// Claims the next runnable job of one of `kinds`, or none.
///
/// A short transaction of its own, committed before the handler runs. With
/// `max_running_per_workspace`, a workspace that already has that many running jobs is
/// skipped, and claims are serialised so the cap holds under concurrent workers.
pub async fn claim_job(
    pool: &PgPool,
    worker: &str,
    kinds: &[&str],
    lease: Duration,
    max_running_per_workspace: Option<u32>,
) -> Result<Option<ClaimedJob>, StoreError> {
    let cap = max_running_per_workspace.map(i64::from);
    // Without a cap one statement is enough. With one, claims queue up on a transaction
    // lock so the count each claim sees includes the claims before it.
    let mut tx = None;
    if cap.is_some() {
        let mut started = pool.begin().await.map_err(StoreError::query)?;
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CLAIM_LOCK_KEY)
            .execute(&mut *started)
            .await
            .map_err(StoreError::query)?;
        tx = Some(started);
    }
    let query = sqlx::query_as(
        "UPDATE jobs SET state = 'running', attempts = attempts + 1, locked_by = $1, \
            lease_token = gen_random_uuid(), \
            lease_expires_at = now() + make_interval(secs => $2) \
            WHERE id = (SELECT j.id FROM jobs j \
            WHERE j.state = 'ready' AND j.run_at <= now() AND j.kind = ANY($3) \
            AND ($4::bigint IS NULL OR j.workspace_id IS NULL \
            OR (SELECT count(*) FROM jobs r \
            WHERE r.workspace_id = j.workspace_id AND r.state = 'running') < $4) \
            ORDER BY j.priority, j.run_at \
            LIMIT 1 \
            FOR UPDATE SKIP LOCKED) \
            RETURNING id, workspace_id, kind, payload, attempts, max_attempts, lease_token, \
            unique_key, trace_id, cause_kind, cause_id, \
            EXTRACT(EPOCH FROM now() - run_at)::float8 AS claim_lag_seconds",
    )
    .bind(worker)
    .bind(lease.as_secs_f64())
    .bind(kinds)
    .bind(cap);
    let claimed: Option<ClaimedRow> = match tx {
        Some(mut tx) => {
            let claimed = query
                .fetch_optional(&mut *tx)
                .await
                .map_err(StoreError::query)?;
            tx.commit().await.map_err(StoreError::query)?;
            claimed
        }
        None => query
            .fetch_optional(pool)
            .await
            .map_err(StoreError::query)?,
    };
    claimed.map(ClaimedRow::into_claimed).transpose()
}

/// Locks the job row in the handler's transaction, to confirm the lease is still held.
///
/// False means the row is gone or carries another token: the lease was lost and the
/// transaction must be abandoned. The lock is a key-share lock, which still lets the
/// heartbeat extend the lease while the handler runs; completion is then fenced by
/// [`complete_job`] in the same transaction.
pub async fn lock_lease(
    connection: &mut PgConnection,
    id: Uuid,
    lease_token: Uuid,
) -> Result<bool, StoreError> {
    let row = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 AND lease_token = $2 FOR KEY SHARE")
        .bind(id)
        .bind(lease_token)
        .fetch_optional(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(row.is_some())
}

/// Deletes a finished job. False means the lease was lost, and the caller must roll back
/// the transaction it ran the handler in.
pub async fn complete_job<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    lease_token: Uuid,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM jobs WHERE id = $1 AND lease_token = $2")
        .bind(id)
        .bind(lease_token)
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Records a failed attempt: back to `ready` after `retry_in`, or `dead` when the attempts
/// are used up. `None` means the lease was lost and nothing changed.
///
/// `error` is truncated to 2 KB and must never contain payload values. For a workspace
/// job the same statement appends a `job.attempt` decision, `failed` or `exhausted`.
pub async fn fail_job<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    lease_token: Uuid,
    retry_in: Duration,
    error: &str,
) -> Result<Option<FailOutcome>, StoreError> {
    let state: Option<String> = sqlx::query_scalar(
        "WITH purged AS ( \
             DELETE FROM jobs WHERE id = $1 AND lease_token = $2 AND purge_on_release \
             RETURNING id), \
         updated AS ( \
             UPDATE jobs SET \
                state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'ready' END, \
                run_at = CASE WHEN attempts >= max_attempts THEN run_at \
                              ELSE now() + make_interval(secs => $3) END, \
                dead_at = CASE WHEN attempts >= max_attempts THEN now() END, \
                payload = CASE WHEN attempts >= max_attempts AND kind = 'destination.erase' \
                               THEN payload - 'external_key' ELSE payload END, \
                last_error = $4, lease_token = NULL, lease_expires_at = NULL, locked_by = NULL \
             WHERE id = $1 AND lease_token = $2 AND NOT purge_on_release \
             RETURNING id, workspace_id, kind, payload, state, attempts, trace_id, cause_kind, \
                       cause_id), \
         settled AS ( \
             UPDATE erasure_destination_calls c SET outcome = 'failed', error_code = 'job_dead', \
                attempts = updated.attempts, updated_at = now() FROM updated \
             WHERE updated.state = 'dead' AND updated.kind = 'destination.erase' \
               AND c.outcome = 'pending' AND c.workspace_id = updated.workspace_id \
               AND c.erasure_id::text = updated.payload ->> 'ledger_id' \
               AND c.destination_id::text = updated.payload ->> 'destination_id' \
             RETURNING c.erasure_id), \
         recorded AS ( \
             INSERT INTO decision_records \
                (id, workspace_id, trace_id, action, outcome, reason, cause_kind, cause_id, \
                 job_id, attempt) \
             SELECT gen_random_uuid(), workspace_id, trace_id, 'job.attempt', \
                    CASE WHEN state = 'dead' THEN 'exhausted' ELSE 'failed' END, \
                    $4, cause_kind, cause_id, id, attempts \
             FROM updated WHERE workspace_id IS NOT NULL) \
         SELECT state FROM updated UNION ALL SELECT 'purged' FROM purged",
    )
    .bind(id)
    .bind(lease_token)
    .bind(retry_in.as_secs_f64())
    .bind(truncate_error(error))
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(state.map(|state| match state.as_str() {
        "dead" => FailOutcome::Dead,
        "purged" => FailOutcome::Purged,
        _ => FailOutcome::Retried,
    }))
}

/// Extends the lease of a running job. False means the lease was lost.
pub async fn heartbeat_job<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    lease_token: Uuid,
    lease: Duration,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE jobs SET lease_expires_at = now() + make_interval(secs => $3) \
         WHERE id = $1 AND lease_token = $2",
    )
    .bind(id)
    .bind(lease_token)
    .bind(lease.as_secs_f64())
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Moves running jobs whose lease expired back to `ready`, to run now, or to `dead` when
/// their attempts are used up (the attempt was counted at claim time). Each reaped
/// workspace job gets a `job.attempt` decision, `lease_expired` or `exhausted`.
///
/// The expired rows are taken `FOR NO KEY UPDATE` in id order (the order an erasure marks them
/// in). That lock does not conflict with a zombie handler's `lock_lease` (`FOR KEY SHARE`), so a
/// job whose handler is still open is requeued all the same, and the late ack is refused.
///
/// A job an erasure marked `purge_on_release` is deleted, not requeued or buried. A `DELETE`
/// needs `FOR UPDATE`, which a handler's `KEY SHARE` blocks, so the rows to delete are taken
/// `FOR UPDATE OF j SKIP LOCKED` first and a marked row a handler still holds is left alone on
/// this pass: its handler's `complete_job` or `fail_job` deletes it, or a later reap does. The
/// reaper therefore never waits on a handler, which would stall every other expired job behind
/// one zombie and deadlock with a handler that completes.
pub async fn reap_expired<'e>(executor: impl PgExecutor<'e>) -> Result<Reaped, StoreError> {
    let states: Vec<String> = sqlx::query_scalar(
        "WITH locked AS ( \
             SELECT id, purge_on_release FROM jobs \
             WHERE state = 'running' AND lease_expires_at < now() \
             ORDER BY id FOR NO KEY UPDATE), \
         purgeable AS ( \
             SELECT j.id FROM jobs j \
             WHERE j.id IN (SELECT id FROM locked WHERE purge_on_release) \
             FOR UPDATE OF j SKIP LOCKED), \
         purged AS ( \
             DELETE FROM jobs WHERE id IN (SELECT id FROM purgeable) \
             RETURNING id), \
         updated AS ( \
             UPDATE jobs SET \
                state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'ready' END, \
                run_at = now(), last_error = 'lease expired', \
                dead_at = CASE WHEN attempts >= max_attempts THEN now() END, \
                payload = CASE WHEN attempts >= max_attempts AND kind = 'destination.erase' \
                               THEN payload - 'external_key' ELSE payload END, \
                lease_token = NULL, lease_expires_at = NULL, locked_by = NULL \
             WHERE id IN (SELECT id FROM locked WHERE NOT purge_on_release) \
             RETURNING id, workspace_id, kind, payload, state, attempts, trace_id, cause_kind, \
                       cause_id), \
         settled AS ( \
             UPDATE erasure_destination_calls c SET outcome = 'failed', error_code = 'job_dead', \
                attempts = updated.attempts, updated_at = now() FROM updated \
             WHERE updated.state = 'dead' AND updated.kind = 'destination.erase' \
               AND c.outcome = 'pending' AND c.workspace_id = updated.workspace_id \
               AND c.erasure_id::text = updated.payload ->> 'ledger_id' \
               AND c.destination_id::text = updated.payload ->> 'destination_id' \
             RETURNING c.erasure_id), \
         recorded AS ( \
             INSERT INTO decision_records \
                (id, workspace_id, trace_id, action, outcome, reason, cause_kind, cause_id, \
                 job_id, attempt) \
             SELECT gen_random_uuid(), workspace_id, trace_id, 'job.attempt', \
                    CASE WHEN state = 'dead' THEN 'exhausted' ELSE 'lease_expired' END, \
                    'lease expired', cause_kind, cause_id, id, attempts \
             FROM updated WHERE workspace_id IS NOT NULL) \
         SELECT state FROM updated UNION ALL SELECT 'purged' FROM purged",
    )
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let count = |wanted: &str| states.iter().filter(|state| *state == wanted).count() as u64;
    let (dead, purged) = (count("dead"), count("purged"));
    Ok(Reaped {
        requeued: states.len() as u64 - dead - purged,
        dead,
        purged,
    })
}

pub async fn find_job<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
) -> Result<Option<Job>, StoreError> {
    sqlx::query_as(
        "SELECT id, workspace_id, kind, payload, priority, run_at, state, attempts, \
                max_attempts, lease_token, lease_expires_at, locked_by, unique_key, \
                last_error, created_at, trace_id, cause_kind, cause_id, dead_at \
         FROM jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

pub async fn queue_counts<'e>(executor: impl PgExecutor<'e>) -> Result<QueueCounts, StoreError> {
    let (ready, running, dead): (i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'ready'), \
                count(*) FILTER (WHERE state = 'running'), \
                count(*) FILTER (WHERE state = 'dead') \
         FROM jobs",
    )
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(QueueCounts {
        ready,
        running,
        dead,
    })
}

/// Job counts by kind and state, for the queue-depth gauges.
pub async fn queue_depths<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<Vec<QueueDepth>, StoreError> {
    sqlx::query_as("SELECT kind, state, count(*) AS count FROM jobs GROUP BY kind, state")
        .fetch_all(executor)
        .await
        .map_err(StoreError::query)
}

/// The queue of one job kind, for the jobs-admin view.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct KindDepth {
    pub kind: String,
    pub ready: i64,
    pub running: i64,
    pub dead: i64,
    /// How long the longest-waiting runnable job has waited, by the database's clock.
    pub oldest_ready_seconds: Option<f64>,
}

/// One job without its payload, as the jobs-admin view shows it.
#[derive(Clone, Debug)]
pub struct JobSummary {
    pub id: Uuid,
    pub workspace_id: Option<Uuid>,
    pub kind: String,
    pub state: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: Option<String>,
    pub trace_id: TraceId,
    pub cause: Cause,
    pub dead_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct JobSummaryRow {
    id: Uuid,
    workspace_id: Option<Uuid>,
    kind: String,
    state: String,
    attempts: i32,
    max_attempts: i32,
    last_error: Option<String>,
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
    dead_at: Option<OffsetDateTime>,
}

impl JobSummaryRow {
    fn into_summary(self) -> Result<JobSummary, StoreError> {
        let kind: CauseKind = self
            .cause_kind
            .parse()
            .map_err(|error| StoreError::query(sqlx::Error::Decode(Box::new(error))))?;
        Ok(JobSummary {
            id: self.id,
            workspace_id: self.workspace_id,
            kind: self.kind,
            state: self.state,
            attempts: self.attempts,
            max_attempts: self.max_attempts,
            last_error: self.last_error,
            trace_id: TraceId::from_uuid(self.trace_id),
            cause: Cause::from_stored(kind, self.cause_id),
            dead_at: self.dead_at,
        })
    }
}

/// Where the next page of dead jobs starts: strictly before this `(dead_at, id)`.
pub type DeadCursor = (OffsetDateTime, Uuid);

/// Job counts per kind and state, and the age of the oldest runnable job, in one query.
/// Sorted by kind.
pub async fn kind_depths<'e>(executor: impl PgExecutor<'e>) -> Result<Vec<KindDepth>, StoreError> {
    sqlx::query_as(
        "SELECT kind, \
                count(*) FILTER (WHERE state = 'ready') AS ready, \
                count(*) FILTER (WHERE state = 'running') AS running, \
                count(*) FILTER (WHERE state = 'dead') AS dead, \
                EXTRACT(EPOCH FROM now() - min(run_at) \
                    FILTER (WHERE state = 'ready' AND run_at <= now()))::float8 \
                    AS oldest_ready_seconds \
         FROM jobs GROUP BY kind ORDER BY kind",
    )
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Dead jobs, newest first, at most `limit`, starting after `before`. The flag is true when
/// more exist. The payload is never read.
pub async fn list_dead_jobs<'e>(
    executor: impl PgExecutor<'e>,
    limit: u32,
    before: Option<DeadCursor>,
) -> Result<(Vec<JobSummary>, bool), StoreError> {
    let rows: Vec<JobSummaryRow> = sqlx::query_as(
        "SELECT id, workspace_id, kind, state, attempts, max_attempts, last_error, trace_id, \
                cause_kind, cause_id, dead_at \
         FROM jobs \
         WHERE state = 'dead' AND ($1::timestamptz IS NULL OR (dead_at, id) < ($1, $2)) \
         ORDER BY dead_at DESC, id DESC LIMIT $3",
    )
    .bind(before.map(|(dead_at, _)| dead_at))
    .bind(before.map(|(_, id)| id))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    let jobs = rows
        .into_iter()
        .take(limit as usize)
        .map(JobSummaryRow::into_summary)
        .collect::<Result<_, _>>()?;
    Ok((jobs, more))
}

/// Puts a dead job back to `ready` with fresh attempts, to run now, keeping its trace and
/// cause, and wakes the workers. `None` means there is no such dead job.
///
/// An error for which [`StoreError::is_conflict`] holds means another ready or running job
/// holds the same unique key; nothing changed, but the caller's transaction is aborted.
pub async fn retry_dead_job(
    connection: &mut PgConnection,
    id: Uuid,
) -> Result<Option<JobSummary>, StoreError> {
    let row: Option<JobSummaryRow> = sqlx::query_as(
        "WITH updated AS ( \
             UPDATE jobs SET state = 'ready', attempts = 0, run_at = now(), \
                lease_token = NULL, lease_expires_at = NULL, locked_by = NULL, \
                last_error = NULL, dead_at = NULL \
             WHERE id = $1 AND state = 'dead' \
             RETURNING id, workspace_id, kind, state, attempts, max_attempts, last_error, \
                       trace_id, cause_kind, cause_id, dead_at) \
         SELECT updated.*, pg_notify($2, kind) AS notified FROM updated",
    )
    .bind(id)
    .bind(NOTIFY_CHANNEL)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(JobSummaryRow::into_summary).transpose()
}

/// The database's clock; due times are compared against it, never the host's.
pub async fn database_now<'e>(executor: impl PgExecutor<'e>) -> Result<OffsetDateTime, StoreError> {
    sqlx::query_scalar("SELECT now()")
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

/// A schedule to create.
#[derive(Clone, Debug)]
pub struct NewSchedule {
    pub workspace_id: Option<WorkspaceId>,
    pub kind: String,
    pub cron: String,
    pub timezone: String,
    pub next_run_at: OffsetDateTime,
    pub payload: Value,
}

pub async fn create_schedule<'e>(
    executor: impl PgExecutor<'e>,
    schedule: &NewSchedule,
) -> Result<Uuid, StoreError> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(schedule.workspace_id.map(|id| id.as_uuid()))
    .bind(&schedule.kind)
    .bind(&schedule.cron)
    .bind(&schedule.timezone)
    .bind(schedule.next_run_at)
    .bind(&schedule.payload)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(id)
}

/// Enabled schedules whose next slot is at or before `now`, oldest first.
pub async fn due_schedules<'e>(
    executor: impl PgExecutor<'e>,
    now: OffsetDateTime,
    limit: i64,
) -> Result<Vec<Schedule>, StoreError> {
    sqlx::query_as(
        "SELECT id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled \
         FROM schedules WHERE enabled AND next_run_at <= $1 \
         ORDER BY next_run_at LIMIT $2",
    )
    .bind(now)
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// Compare-and-set: moves a schedule from the slot the caller read to `to`. False means
/// another scheduler already advanced it, so the caller must not enqueue that slot.
///
/// Call it in the same transaction as the enqueue; the advanced `next_run_at` is the
/// durable record that the slot fired, and survives the job completing and being deleted.
pub async fn advance_schedule<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<bool, StoreError> {
    let done =
        sqlx::query("UPDATE schedules SET next_run_at = $3 WHERE id = $1 AND next_run_at = $2")
            .bind(id)
            .bind(from)
            .bind(to)
            .execute(executor)
            .await
            .map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

/// Stops a schedule from firing, for one that can no longer be evaluated.
pub async fn disable_schedule<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE schedules SET enabled = false WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// A dedicated connection listening for new jobs.
pub struct JobListener {
    inner: PgListener,
}

/// Starts listening on [`NOTIFY_CHANNEL`] with a connection of its own.
pub async fn listen_for_jobs(pool: &PgPool) -> Result<JobListener, StoreError> {
    let mut inner = PgListener::connect_with(pool)
        .await
        .map_err(StoreError::query)?;
    inner
        .listen(NOTIFY_CHANNEL)
        .await
        .map_err(StoreError::query)?;
    Ok(JobListener { inner })
}

impl JobListener {
    /// Waits for a notification. `Ok(false)` means the connection was lost; it reconnects
    /// on the next call, and notifications sent meanwhile are lost, so callers fall back
    /// to polling.
    pub async fn wait(&mut self) -> Result<bool, StoreError> {
        self.inner
            .try_recv()
            .await
            .map(|notification| notification.is_some())
            .map_err(StoreError::query)
    }
}

/// The scheduler leader's hold on the advisory lock. The lock lives as long as this
/// connection does.
pub struct Leadership {
    connection: PgConnection,
}

/// Tries to become the scheduler leader. The lock is taken on a connection detached from
/// the pool, so it is never returned to the pool still holding the lock.
pub async fn try_lead(pool: &PgPool) -> Result<Option<Leadership>, StoreError> {
    let mut connection = pool.acquire().await.map_err(StoreError::query)?.detach();
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LEADER_LOCK_KEY)
        .fetch_one(&mut connection)
        .await
        .map_err(StoreError::query)?;
    Ok(acquired.then_some(Leadership { connection }))
}

impl Leadership {
    /// Whether the lock connection is still alive. A dead connection has lost the lock.
    pub async fn is_held(&mut self) -> bool {
        sqlx::query("SELECT 1")
            .execute(&mut self.connection)
            .await
            .is_ok()
    }

    /// Gives the lock up now rather than when the connection is dropped.
    pub async fn release(mut self) {
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(LEADER_LOCK_KEY)
            .execute(&mut self.connection)
            .await;
    }
}

/// Keeps `error` within [`MAX_ERROR_BYTES`], cutting on a character boundary.
fn truncate_error(error: &str) -> &str {
    if error.len() <= MAX_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    &error[..end]
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    const LEASE: Duration = Duration::from_secs(30);

    fn job(kind: &str) -> NewJob {
        NewJob::new(kind, TraceContext::root(Cause::api("test-request")))
    }

    async fn claim(pool: &PgPool, kinds: &[&str]) -> Option<ClaimedJob> {
        claim_job(pool, "test-worker", kinds, LEASE, None)
            .await
            .unwrap()
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn only_job(pool: &PgPool) -> Job {
        let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_one(pool)
            .await
            .unwrap();
        find_job(pool, id).await.unwrap().unwrap()
    }

    #[test]
    fn a_long_error_is_cut_to_two_kilobytes_on_a_character_boundary() {
        let long = "é".repeat(2000);

        let kept = truncate_error(&long);

        assert!(kept.len() <= MAX_ERROR_BYTES);
        assert!(kept.len() >= MAX_ERROR_BYTES - 1);
        assert_eq!(truncate_error("short"), "short");
    }

    #[test]
    fn a_new_job_defaults_to_an_empty_payload_and_five_attempts() {
        let job = job("maintenance.noop");

        assert_eq!(job.payload, json!({}));
        assert_eq!(job.max_attempts, DEFAULT_MAX_ATTEMPTS);
        assert_eq!(job.priority, 0);
        assert!(job.unique_key.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_enqueued_job_can_be_claimed_once(pool: PgPool) {
        assert!(enqueue(&pool, &job("a.kind")).await.unwrap());

        let claimed = claim(&pool, &["a.kind"]).await.unwrap();

        assert_eq!(claimed.kind, "a.kind");
        assert_eq!(claimed.attempts, 1);
        assert!(claim(&pool, &["a.kind"]).await.is_none());
        let stored = only_job(&pool).await;
        assert_eq!(stored.state, "running");
        assert_eq!(stored.lease_token, Some(claimed.lease_token));
        assert_eq!(stored.locked_by.as_deref(), Some("test-worker"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_trace_and_cause_survive_enqueue_and_claim(pool: PgPool) {
        let schedule = Uuid::new_v4();
        let trace = TraceContext::root(Cause::schedule(schedule));
        let new = NewJob::new("a.kind", trace.clone());
        enqueue(&pool, &new).await.unwrap();

        let stored = only_job(&pool).await;
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();

        assert_eq!(stored.trace_id, trace.trace_id.as_uuid());
        assert_eq!(stored.cause_kind, "schedule");
        assert_eq!(stored.cause_id, schedule.to_string());
        assert_eq!(claimed.trace, trace);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn claim_lag_is_how_long_the_job_waited_past_its_run_at(pool: PgPool) {
        let mut late = job("a.kind");
        late.run_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(90));
        enqueue(&pool, &late).await.unwrap();
        enqueue(&pool, &job("b.kind")).await.unwrap();

        let waited = claim(&pool, &["a.kind"]).await.unwrap();
        let immediate = claim(&pool, &["b.kind"]).await.unwrap();

        assert!(
            waited.claim_lag_seconds >= 89.0,
            "{}",
            waited.claim_lag_seconds
        );
        assert!(waited.claim_lag_seconds < 600.0);
        assert!(immediate.claim_lag_seconds >= 0.0);
        assert!(immediate.claim_lag_seconds < 30.0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn queue_depths_group_by_kind_and_state(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        enqueue(&pool, &job("a.kind")).await.unwrap();
        enqueue(&pool, &job("b.kind")).await.unwrap();
        claim(&pool, &["b.kind"]).await.unwrap();

        let mut depths = queue_depths(&pool).await.unwrap();
        depths.sort_by(|a, b| a.kind.cmp(&b.kind));

        let summary: Vec<(&str, &str, i64)> = depths
            .iter()
            .map(|d| (d.kind.as_str(), d.state.as_str(), d.count))
            .collect();
        assert_eq!(summary, [("a.kind", "ready", 2), ("b.kind", "running", 1)]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_workspace_job_leaves_a_job_attempt_decision_and_an_instance_job_none(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        let mut owned = job("a.kind");
        owned.workspace_id = Some(ws);
        owned.max_attempts = 2;
        let trace = owned.trace.clone();
        enqueue(&pool, &owned).await.unwrap();
        enqueue(&pool, &job("b.kind")).await.unwrap();
        let first = claim(&pool, &["a.kind"]).await.unwrap();
        fail_job(&pool, first.id, first.lease_token, Duration::ZERO, "boom")
            .await
            .unwrap();
        let second = claim(&pool, &["a.kind"]).await.unwrap();
        fail_job(&pool, second.id, second.lease_token, Duration::ZERO, "boom")
            .await
            .unwrap();
        let instance = claim(&pool, &["b.kind"]).await.unwrap();
        fail_job(
            &pool,
            instance.id,
            instance.lease_token,
            Duration::ZERO,
            "boom",
        )
        .await
        .unwrap();

        let (records, _) = crate::list_trace(&pool, ws, trace.trace_id, 10)
            .await
            .unwrap();

        let seen: Vec<(&str, &str, Option<&str>, Option<i32>)> = records
            .iter()
            .map(|r| {
                (
                    r.action.as_str(),
                    r.outcome.as_str(),
                    r.reason.as_deref(),
                    r.attempt,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("job.attempt", "failed", Some("boom"), Some(1)),
                ("job.attempt", "exhausted", Some("boom"), Some(2)),
            ]
        );
        assert_eq!(records[0].cause, trace.cause);
        assert_eq!(records[0].job_id, Some(first.id));
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(total, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reaped_workspace_job_leaves_a_lease_expired_or_exhausted_decision(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let mut retried = job("a.kind");
        retried.workspace_id = Some(ws);
        let mut last = job("b.kind");
        last.workspace_id = Some(ws);
        last.max_attempts = 1;
        let traces = [retried.trace.clone(), last.trace.clone()];
        enqueue(&pool, &retried).await.unwrap();
        enqueue(&pool, &last).await.unwrap();
        let first = claim_job(&pool, "w", &["a.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        let second = claim_job(&pool, "w", &["b.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        wait_until_expired(&pool, first.id).await;
        wait_until_expired(&pool, second.id).await;

        reap_expired(&pool).await.unwrap();

        let mut outcomes = Vec::new();
        for trace in &traces {
            let (records, _) = crate::list_trace(&pool, ws, trace.trace_id, 10)
                .await
                .unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].action, "job.attempt");
            assert_eq!(records[0].reason.as_deref(), Some("lease expired"));
            outcomes.push(records[0].outcome.clone());
        }
        assert_eq!(outcomes, ["lease_expired", "exhausted"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_job_with_a_held_unique_key_is_not_enqueued(pool: PgPool) {
        let mut keyed = job("a.kind");
        keyed.unique_key = Some("once".into());

        assert!(enqueue(&pool, &keyed).await.unwrap());
        assert!(!enqueue(&pool, &keyed).await.unwrap());
        // Still held while running.
        claim(&pool, &["a.kind"]).await.unwrap();
        assert!(!enqueue(&pool, &keyed).await.unwrap());

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_unique_key_is_free_again_once_the_job_is_gone_or_dead(pool: PgPool) {
        let mut keyed = job("a.kind");
        keyed.unique_key = Some("once".into());
        keyed.max_attempts = 1;
        enqueue(&pool, &keyed).await.unwrap();
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();
        fail_job(
            &pool,
            claimed.id,
            claimed.lease_token,
            Duration::ZERO,
            "boom",
        )
        .await
        .unwrap();

        assert!(enqueue(&pool, &keyed).await.unwrap());
        let second = claim(&pool, &["a.kind"]).await.unwrap();
        assert!(
            complete_job(&pool, second.id, second.lease_token)
                .await
                .unwrap()
        );
        assert!(enqueue(&pool, &keyed).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn claims_follow_priority_then_age_and_skip_other_kinds_and_future_jobs(pool: PgPool) {
        let mut low = job("a.kind");
        low.priority = 5;
        low.payload = json!({"n": "low"});
        let mut high = job("a.kind");
        high.priority = -1;
        high.payload = json!({"n": "high"});
        let mut other = job("b.kind");
        other.priority = -10;
        let mut later = job("a.kind");
        later.priority = -10;
        later.run_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        for new in [&low, &high, &other, &later] {
            enqueue(&pool, new).await.unwrap();
        }

        let first = claim(&pool, &["a.kind"]).await.unwrap();
        let second = claim(&pool, &["a.kind"]).await.unwrap();

        assert_eq!(first.payload, json!({"n": "high"}));
        assert_eq!(second.payload, json!({"n": "low"}));
        assert!(claim(&pool, &["a.kind"]).await.is_none());
        assert!(claim(&pool, &["b.kind"]).await.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_per_workspace_cap_keeps_a_busy_workspace_from_starving_the_rest(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        let quiet = workspace(&pool, "quiet").await;
        for _ in 0..3 {
            let mut new = job("a.kind");
            new.workspace_id = Some(busy);
            new.priority = 0;
            enqueue(&pool, &new).await.unwrap();
        }
        let mut other = job("a.kind");
        other.workspace_id = Some(quiet);
        other.priority = 1;
        enqueue(&pool, &other).await.unwrap();
        let claim_capped = || claim_job(&pool, "w", &["a.kind"], LEASE, Some(2));

        let first = claim_capped().await.unwrap().unwrap();
        let second = claim_capped().await.unwrap().unwrap();
        // The busy workspace is at its cap, so the next claim passes it over.
        let third = claim_capped().await.unwrap().unwrap();
        let fourth = claim_capped().await.unwrap();

        assert_eq!(first.workspace_id, Some(busy.as_uuid()));
        assert_eq!(second.workspace_id, Some(busy.as_uuid()));
        assert_eq!(third.workspace_id, Some(quiet.as_uuid()));
        assert!(fourth.is_none());
        // Finishing a busy job frees a slot.
        complete_job(&pool, first.id, first.lease_token)
            .await
            .unwrap();
        let fifth = claim_capped().await.unwrap().unwrap();
        assert_eq!(fifth.workspace_id, Some(busy.as_uuid()));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_cap_does_not_apply_to_instance_level_jobs(pool: PgPool) {
        for _ in 0..3 {
            enqueue(&pool, &job("a.kind")).await.unwrap();
        }

        for _ in 0..3 {
            let claimed = claim_job(&pool, "w", &["a.kind"], LEASE, Some(1))
                .await
                .unwrap();
            assert!(claimed.is_some());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn completing_deletes_the_job_and_a_stale_token_changes_nothing(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();

        let stale = Uuid::new_v4();
        assert!(!complete_job(&pool, claimed.id, stale).await.unwrap());
        assert!(
            fail_job(&pool, claimed.id, stale, Duration::ZERO, "boom")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !heartbeat_job(&pool, claimed.id, stale, LEASE)
                .await
                .unwrap()
        );
        assert_eq!(only_job(&pool).await.state, "running");

        assert!(
            complete_job(&pool, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        assert!(find_job(&pool, claimed.id).await.unwrap().is_none());
        assert!(
            !complete_job(&pool, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_retries_after_the_backoff_with_the_error_kept(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();

        let outcome = fail_job(
            &pool,
            claimed.id,
            claimed.lease_token,
            Duration::from_secs(3600),
            "it broke",
        )
        .await
        .unwrap();

        assert_eq!(outcome, Some(FailOutcome::Retried));
        let stored = only_job(&pool).await;
        assert_eq!(stored.state, "ready");
        assert_eq!(stored.last_error.as_deref(), Some("it broke"));
        assert!(stored.lease_token.is_none() && stored.locked_by.is_none());
        assert!(stored.run_at > OffsetDateTime::now_utc() + time::Duration::minutes(59));
        assert!(claim(&pool, &["a.kind"]).await.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_is_dead_once_its_attempts_are_used_up(pool: PgPool) {
        let mut new = job("a.kind");
        new.max_attempts = 2;
        enqueue(&pool, &new).await.unwrap();

        let first = claim(&pool, &["a.kind"]).await.unwrap();
        let retried = fail_job(&pool, first.id, first.lease_token, Duration::ZERO, "one")
            .await
            .unwrap();
        let second = claim(&pool, &["a.kind"]).await.unwrap();
        let dead = fail_job(&pool, second.id, second.lease_token, Duration::ZERO, "two")
            .await
            .unwrap();

        assert_eq!(retried, Some(FailOutcome::Retried));
        assert_eq!(second.attempts, 2);
        assert_eq!(dead, Some(FailOutcome::Dead));
        let stored = only_job(&pool).await;
        assert_eq!(stored.state, "dead");
        assert_eq!(stored.last_error.as_deref(), Some("two"));
        assert!(claim(&pool, &["a.kind"]).await.is_none());
        assert_eq!(
            queue_counts(&pool).await.unwrap(),
            QueueCounts {
                ready: 0,
                running: 0,
                dead: 1
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_requeues_an_expired_lease_and_fences_the_old_token(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim_job(&pool, "w", &["a.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        // A live lease is left alone.
        enqueue(&pool, &job("b.kind")).await.unwrap();
        claim(&pool, &["b.kind"]).await.unwrap();
        wait_until_expired(&pool, claimed.id).await;

        let reaped = reap_expired(&pool).await.unwrap();

        assert_eq!(
            reaped,
            Reaped {
                requeued: 1,
                dead: 0,
                purged: 0
            }
        );
        let again = claim(&pool, &["a.kind"]).await.unwrap();
        assert_eq!(again.id, claimed.id);
        assert_eq!(again.attempts, 2);
        assert_ne!(again.lease_token, claimed.lease_token);
        assert!(
            !complete_job(&pool, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        assert!(
            !lock_lease(
                &mut pool.acquire().await.unwrap(),
                claimed.id,
                claimed.lease_token
            )
            .await
            .unwrap()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_kills_an_expired_job_that_is_out_of_attempts(pool: PgPool) {
        let mut new = job("a.kind");
        new.max_attempts = 1;
        enqueue(&pool, &new).await.unwrap();
        let claimed = claim_job(&pool, "w", &["a.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        wait_until_expired(&pool, claimed.id).await;

        let reaped = reap_expired(&pool).await.unwrap();

        assert_eq!(
            reaped,
            Reaped {
                requeued: 0,
                dead: 1,
                purged: 0
            }
        );
        assert_eq!(only_job(&pool).await.state, "dead");
    }

    /// Waits for the lease to pass, by the database's clock.
    async fn wait_until_expired(pool: &PgPool, id: Uuid) {
        loop {
            let expired: bool =
                sqlx::query_scalar("SELECT lease_expires_at < now() FROM jobs WHERE id = $1")
                    .bind(id)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            if expired {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_heartbeat_extends_the_lease(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim_job(&pool, "w", &["a.kind"], Duration::from_secs(5), None)
            .await
            .unwrap()
            .unwrap();
        let before = only_job(&pool).await.lease_expires_at.unwrap();

        assert!(
            heartbeat_job(
                &pool,
                claimed.id,
                claimed.lease_token,
                Duration::from_secs(600)
            )
            .await
            .unwrap()
        );

        assert!(
            only_job(&pool).await.lease_expires_at.unwrap() > before + time::Duration::minutes(5)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lease_lock_still_lets_a_heartbeat_through(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();
        let mut tx = pool.begin().await.unwrap();

        assert!(
            lock_lease(&mut tx, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        // A different connection extends the lease without waiting for the transaction.
        assert!(
            heartbeat_job(&pool, claimed.id, claimed.lease_token, LEASE)
                .await
                .unwrap()
        );
        assert!(
            complete_job(&mut *tx, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        tx.commit().await.unwrap();

        assert!(find_job(&pool, claimed.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rolled_back_completion_leaves_the_job_running(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim(&pool, &["a.kind"]).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        complete_job(&mut *tx, claimed.id, claimed.lease_token)
            .await
            .unwrap();
        drop(tx);

        assert_eq!(only_job(&pool).await.state, "running");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_job_enqueued_in_a_rolled_back_transaction_never_exists(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        enqueue(&mut *tx, &job("a.kind")).await.unwrap();
        drop(tx);

        assert_eq!(queue_counts(&pool).await.unwrap(), QueueCounts::default());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_committed_enqueue_wakes_a_listener(pool: PgPool) {
        let mut listener = listen_for_jobs(&pool).await.unwrap();

        enqueue(&pool, &job("a.kind")).await.unwrap();

        assert!(listener.wait().await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_schedule_advances_only_from_the_slot_the_caller_read(pool: PgPool) {
        let slot =
            OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - time::Duration::hours(2);
        let id = create_schedule(
            &pool,
            &NewSchedule {
                workspace_id: None,
                kind: "a.kind".into(),
                cron: "0 * * * *".into(),
                timezone: "UTC".into(),
                next_run_at: slot,
                payload: json!({}),
            },
        )
        .await
        .unwrap();
        let now = database_now(&pool).await.unwrap();
        let next = slot + time::Duration::hours(1);

        let due = due_schedules(&pool, now, 10).await.unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].next_run_at, slot);
        assert!(advance_schedule(&pool, id, slot, next).await.unwrap());
        // A second scheduler holding the old slot loses the race.
        assert!(!advance_schedule(&pool, id, slot, next).await.unwrap());

        let due = due_schedules(&pool, now, 10).await.unwrap();
        assert_eq!(due[0].next_run_at, next);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_disabled_or_future_schedule_is_not_due(pool: PgPool) {
        let now = database_now(&pool).await.unwrap();
        let make = |next_run_at| NewSchedule {
            workspace_id: None,
            kind: "a.kind".into(),
            cron: "0 * * * *".into(),
            timezone: "UTC".into(),
            next_run_at,
            payload: json!({}),
        };
        let disabled = create_schedule(&pool, &make(now - time::Duration::hours(1)))
            .await
            .unwrap();
        create_schedule(&pool, &make(now + time::Duration::hours(1)))
            .await
            .unwrap();
        disable_schedule(&pool, disabled).await.unwrap();

        assert!(due_schedules(&pool, now, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_one_connection_leads_at_a_time(pool: PgPool) {
        let first = try_lead(&pool).await.unwrap();
        let mut leader = first.expect("the first caller leads");

        assert!(try_lead(&pool).await.unwrap().is_none());
        assert!(leader.is_held().await);

        leader.release().await;
        assert!(try_lead(&pool).await.unwrap().is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dropping_the_leaders_connection_hands_over_leadership(pool: PgPool) {
        let leader = try_lead(&pool).await.unwrap().unwrap();

        drop(leader);

        // The server notices the closed connection on its own schedule.
        loop {
            if try_lead(&pool).await.unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn dead_candidate(kind: &str) -> NewJob {
        let mut new = job(kind);
        new.max_attempts = 1;
        new
    }

    /// Enqueues a job with one attempt, claims it and fails it, leaving it dead.
    async fn make_dead(pool: &PgPool, new: NewJob) -> Uuid {
        let kind = new.kind.clone();
        enqueue(pool, &new).await.unwrap();
        let claimed = claim(pool, &[kind.as_str()]).await.unwrap();
        let outcome = fail_job(
            pool,
            claimed.id,
            claimed.lease_token,
            Duration::ZERO,
            "boom",
        )
        .await
        .unwrap();
        assert_eq!(outcome, Some(FailOutcome::Dead));
        claimed.id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn failing_a_job_out_of_attempts_stamps_dead_at_and_a_retried_failure_does_not(
        pool: PgPool,
    ) {
        enqueue(&pool, &job("retrying.kind")).await.unwrap();
        let claimed = claim(&pool, &["retrying.kind"]).await.unwrap();
        fail_job(&pool, claimed.id, claimed.lease_token, Duration::ZERO, "x")
            .await
            .unwrap();
        assert!(only_job(&pool).await.dead_at.is_none());
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        let id = make_dead(&pool, dead_candidate("dying.kind")).await;

        let stored = find_job(&pool, id).await.unwrap().unwrap();
        assert_eq!(stored.state, "dead");
        assert!(stored.dead_at.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn reaping_a_job_out_of_attempts_stamps_dead_at(pool: PgPool) {
        enqueue(&pool, &dead_candidate("reaped.kind"))
            .await
            .unwrap();
        let claimed = claim_job(&pool, "w", &["reaped.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let reaped = reap_expired(&pool).await.unwrap();

        assert_eq!(reaped.dead, 1);
        let stored = find_job(&pool, claimed.id).await.unwrap().unwrap();
        assert_eq!(stored.state, "dead");
        assert!(stored.dead_at.is_some());
    }

    /// An erase call job holding an external key, and a job of another kind holding the same
    /// field, each with one attempt.
    fn keyed(kind: &str) -> NewJob {
        let mut new = dead_candidate(kind);
        new.payload = serde_json::json!({"external_key": "SENTINEL-KEY", "ledger_id": "l-1"});
        new
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erase_call_that_dies_loses_its_external_key_and_other_kinds_keep_theirs(
        pool: PgPool,
    ) {
        let erase = make_dead(&pool, keyed(crate::ERASE_CALL_JOB_KIND)).await;
        let other = make_dead(&pool, keyed("other.kind")).await;

        let erase = find_job(&pool, erase).await.unwrap().unwrap();
        let other = find_job(&pool, other).await.unwrap().unwrap();

        assert_eq!(erase.payload, serde_json::json!({"ledger_id": "l-1"}));
        assert_eq!(other.payload["external_key"], "SENTINEL-KEY");
        assert_eq!(crate::EXTERNAL_KEY_FIELD, "external_key");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_erase_call_reaped_dead_loses_its_external_key(pool: PgPool) {
        enqueue(&pool, &keyed(crate::ERASE_CALL_JOB_KIND))
            .await
            .unwrap();
        let claimed = claim_job(
            &pool,
            "w",
            &[crate::ERASE_CALL_JOB_KIND],
            Duration::from_millis(1),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let reaped = reap_expired(&pool).await.unwrap();

        assert_eq!(reaped.dead, 1);
        let stored = find_job(&pool, claimed.id).await.unwrap().unwrap();
        assert_eq!(stored.payload, serde_json::json!({"ledger_id": "l-1"}));
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_dead_at_migration_applies_to_a_table_that_already_has_dead_rows(pool: PgPool) {
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 5)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        for (state, kind) in [("dead", "old.dead"), ("ready", "old.ready")] {
            sqlx::query(
                "INSERT INTO jobs (id, kind, state, trace_id, cause_kind, cause_id) \
                 VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), 'manual', '')",
            )
            .bind(kind)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
        }

        crate::migrate(&pool).await.unwrap();

        let rows: Vec<(String, Option<OffsetDateTime>)> =
            sqlx::query_as("SELECT state, dead_at FROM jobs ORDER BY state")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows[0].0, "dead");
        assert!(rows[0].1.is_some(), "a dead row is backfilled");
        assert_eq!(rows[1].0, "ready");
        assert!(rows[1].1.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn depths_count_each_state_per_kind_and_age_only_runnable_ready_jobs(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let mut later = job("a.kind");
        later.run_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        enqueue(&pool, &later).await.unwrap();
        make_dead(&pool, dead_candidate("b.kind")).await;
        enqueue(&pool, &job("c.kind")).await.unwrap();
        claim(&pool, &["c.kind"]).await.unwrap();
        sqlx::query(
            "UPDATE jobs SET run_at = now() - interval '90 seconds' \
             WHERE kind = 'a.kind' AND run_at <= now()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let depths = kind_depths(&pool).await.unwrap();

        let kinds: Vec<&str> = depths.iter().map(|depth| depth.kind.as_str()).collect();
        assert_eq!(kinds, ["a.kind", "b.kind", "c.kind"]);
        let counts = |depth: &KindDepth| (depth.ready, depth.running, depth.dead);
        assert_eq!(counts(&depths[0]), (3, 0, 0));
        assert_eq!(counts(&depths[1]), (0, 0, 1));
        assert_eq!(counts(&depths[2]), (0, 1, 0));
        let oldest = depths[0].oldest_ready_seconds.unwrap();
        assert!((90.0..120.0).contains(&oldest), "{oldest}");
        assert_eq!(depths[1].oldest_ready_seconds, None);
        assert_eq!(depths[2].oldest_ready_seconds, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_dead_list_is_newest_first_and_pages_without_gaps_or_duplicates(pool: PgPool) {
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(make_dead(&pool, dead_candidate("dead.kind")).await);
        }
        // Two rows share an instant, so the id must break the tie.
        sqlx::query("UPDATE jobs SET dead_at = '2030-01-01T00:00:00Z' WHERE id = ANY($1)")
            .bind(&ids[..2])
            .execute(&pool)
            .await
            .unwrap();

        let (first, more) = list_dead_jobs(&pool, 2, None).await.unwrap();
        assert!(more);
        let cursor = |job: &JobSummary| (job.dead_at.unwrap(), job.id);
        let (second, more) = list_dead_jobs(&pool, 2, Some(cursor(&first[1])))
            .await
            .unwrap();
        assert!(more);
        let (third, more) = list_dead_jobs(&pool, 2, Some(cursor(&second[1])))
            .await
            .unwrap();
        assert!(!more);

        let paged: Vec<Uuid> = first
            .iter()
            .chain(&second)
            .chain(&third)
            .map(|job| job.id)
            .collect();
        let (all, _) = list_dead_jobs(&pool, 50, None).await.unwrap();
        let everything: Vec<Uuid> = all.iter().map(|job| job.id).collect();
        assert_eq!(paged, everything);
        let mut unique = paged.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 5);
        let stamps: Vec<_> = all.iter().map(|job| (job.dead_at, job.id)).collect();
        assert!(stamps.windows(2).all(|pair| pair[0] > pair[1]));
        assert_eq!(first[0].last_error.as_deref(), Some("boom"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_a_dead_job_resets_it_and_keeps_its_trace(pool: PgPool) {
        let trace = TraceContext::root(Cause::schedule(Uuid::new_v4()));
        let mut new = dead_candidate("retry.kind");
        new.trace = trace.clone();
        let id = make_dead(&pool, new).await;
        let mut listener = listen_for_jobs(&pool).await.unwrap();

        let mut connection = pool.acquire().await.unwrap();
        let retried = retry_dead_job(&mut connection, id).await.unwrap().unwrap();
        drop(connection);

        assert_eq!(retried.state, "ready");
        assert_eq!(retried.attempts, 0);
        assert_eq!(retried.last_error, None);
        assert_eq!(retried.dead_at, None);
        assert_eq!(retried.trace_id, trace.trace_id);
        assert_eq!(retried.cause, trace.cause);
        assert!(listener.wait().await.unwrap(), "workers are woken");
        let claimed = claim(&pool, &["retry.kind"]).await.unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.attempts, 1);
        assert_eq!(claimed.trace, trace);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_a_dead_job_can_be_retried(pool: PgPool) {
        enqueue(&pool, &job("live.kind")).await.unwrap();
        let ready = only_job(&pool).await.id;
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            retry_dead_job(&mut connection, ready)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            retry_dead_job(&mut connection, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retrying_into_a_held_unique_key_is_a_conflict_and_changes_nothing(pool: PgPool) {
        let mut keyed = dead_candidate("keyed.kind");
        keyed.unique_key = Some("only-one".into());
        let dead = make_dead(&pool, keyed.clone()).await;
        keyed.max_attempts = 5;
        assert!(enqueue(&pool, &keyed).await.unwrap());

        let mut connection = pool.acquire().await.unwrap();
        let error = retry_dead_job(&mut connection, dead).await.unwrap_err();
        drop(connection);

        assert!(error.is_conflict());
        let stored = find_job(&pool, dead).await.unwrap().unwrap();
        assert_eq!(stored.state, "dead");
        assert!(stored.dead_at.is_some());
        assert_eq!(stored.attempts, 1);
    }

    async fn insert_expired(pool: &PgPool, id: u128, token: Uuid, marked: bool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, state, lease_token, lease_expires_at, \
                attempts, unique_key, cause_kind, cause_id, trace_id, purge_on_release) \
             VALUES ($1, 'a.kind', '{}', 'running', $2, now() - interval '1 minute', 1, \
                $1::text, 'api', 't', gen_random_uuid(), $3)",
        )
        .bind(Uuid::from_u128(id))
        .bind(token)
        .bind(marked)
        .execute(pool)
        .await
        .unwrap();
    }

    /// One zombie handler must not stall the whole reap: the reaper skips the marked row the
    /// handler still holds, reaps the free one, and purges the marked one on a later pass.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_skips_an_expired_job_a_handler_still_holds_and_purges_it_later(
        pool: PgPool,
    ) {
        let (free, held) = (Uuid::new_v4(), Uuid::new_v4());
        insert_expired(&pool, 1, free, false).await;
        insert_expired(&pool, 2, held, true).await;
        let mut handler = pool.begin().await.unwrap();
        assert!(
            lock_lease(&mut handler, Uuid::from_u128(2), held)
                .await
                .unwrap()
        );

        // The timeout is a hang guard only: the reaper has nothing to wait for.
        let reaped = tokio::time::timeout(Duration::from_secs(10), reap_expired(&pool))
            .await
            .expect("the reaper waited for a handler's lock")
            .unwrap();

        assert_eq!(
            reaped,
            Reaped {
                requeued: 1,
                dead: 0,
                purged: 0
            }
        );
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
            .bind(Uuid::from_u128(2))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "running", "the held job is left to its handler");
        handler.commit().await.unwrap();

        let reaped = reap_expired(&pool).await.unwrap();
        assert_eq!(
            reaped,
            Reaped {
                requeued: 0,
                dead: 0,
                purged: 1
            }
        );
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM jobs")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [Uuid::from_u128(1)]);
    }

    /// A handler holds a marked, expired job when the reaper runs, then completes. The reaper
    /// must not take the row and wait on the handler: the handler's `complete_job` would then
    /// wait on the reaper (40P01).
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_and_a_completing_handler_do_not_deadlock_on_a_marked_job(pool: PgPool) {
        use crate::privacy::testing::poll_until;
        let token = Uuid::new_v4();
        insert_expired(&pool, 1, token, true).await;
        let mut handler = pool.begin().await.unwrap();
        assert!(
            lock_lease(&mut handler, Uuid::from_u128(1), token)
                .await
                .unwrap()
        );

        let mut reaper_connection = pool.acquire().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *reaper_connection)
            .await
            .unwrap();
        let reaper = tokio::spawn(async move { reap_expired(&mut *reaper_connection).await });
        // The reaper either finishes (it skipped the row) or waits for the handler.
        let (probe, reaper_ref) = (&pool, &reaper);
        poll_until("the reaper to finish or wait", || async move {
            reaper_ref.is_finished() || {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
                     WHERE pid = $1 AND wait_event_type = 'Lock')",
                )
                .bind(pid)
                .fetch_one(probe)
                .await
                .unwrap();
                waiting
            }
        })
        .await;

        let completed = complete_job(&mut *handler, Uuid::from_u128(1), token)
            .await
            .expect("the handler's completion hit a deadlock");
        assert!(completed);
        handler.commit().await.unwrap();
        let reaped = tokio::time::timeout(Duration::from_secs(10), reaper)
            .await
            .expect("the reaper did not finish")
            .unwrap()
            .expect("the reaper hit a deadlock");

        assert_eq!(reaped.requeued + reaped.dead, 0);
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "the job ends up deleted");
    }

    /// A zombie handler still holds an unmarked expired job in an open transaction. The reaper
    /// must requeue it anyway, or no other worker could take the job over; the late ack is then
    /// refused.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reaper_requeues_an_unmarked_expired_job_a_zombie_handler_still_holds(
        pool: PgPool,
    ) {
        let token = Uuid::new_v4();
        insert_expired(&pool, 1, token, false).await;
        let mut zombie = pool.begin().await.unwrap();
        assert!(
            lock_lease(&mut zombie, Uuid::from_u128(1), token)
                .await
                .unwrap()
        );

        let reaped = tokio::time::timeout(Duration::from_secs(10), reap_expired(&pool))
            .await
            .expect("the reaper waited for the zombie handler")
            .unwrap();

        assert_eq!(
            reaped,
            Reaped {
                requeued: 1,
                dead: 0,
                purged: 0
            }
        );
        let acked = complete_job(&mut *zombie, Uuid::from_u128(1), token)
            .await
            .unwrap();
        assert!(!acked, "the late ack of the old lease is refused");
        zombie.commit().await.unwrap();
        let again = claim(&pool, &["a.kind"]).await.unwrap();
        assert_eq!(again.id, Uuid::from_u128(1));
    }
}
