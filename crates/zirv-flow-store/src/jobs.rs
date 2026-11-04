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
use zirv_flow_core::decision_actions;
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
    priority: i16,
    run_at: OffsetDateTime,
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

/// The CTEs of a single-job enqueue, `inserted` and `notified`, over the twelve parameters
/// [`bind_job`] binds as `$1` to `$12`. A statement that appends them to its own takes
/// parameters from `$13` and must read `notified`, or the wake-up is never sent.
macro_rules! enqueue_ctes {
    () => {
        "inserted AS ( \
             INSERT INTO jobs \
                (id, workspace_id, kind, payload, priority, run_at, max_attempts, unique_key, \
                 trace_id, cause_kind, cause_id) \
             VALUES ($1, $2, $3, $4, $5, COALESCE($6, now()), $7, $8, $9, $10, $11) \
             ON CONFLICT DO NOTHING \
             RETURNING kind), \
         notified AS ( \
             SELECT pg_notify($12, kind) FROM inserted \
             WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' AND kind = inserted.kind \
                               AND run_at <= now()))"
    };
}
pub(crate) use enqueue_ctes;

/// Binds `$1` to `$12` of [`enqueue_ctes`].
pub(crate) fn bind_job<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    job: &'q NewJob,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
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
}

/// Enqueues a job; returns whether a row was inserted (false when its unique key is held).
///
/// Pass the caller's transaction so the job commits with the change that caused it. The
/// wake-up notification is sent in that transaction, so it is delivered on commit.
pub async fn enqueue<'e>(executor: impl PgExecutor<'e>, job: &NewJob) -> Result<bool, StoreError> {
    let notified = bind_job(
        sqlx::query(concat!(
            "WITH ",
            enqueue_ctes!(),
            " SELECT kind FROM inserted, (SELECT count(*) FROM notified) AS n"
        )),
        job,
    )
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
         notified AS ( \
             SELECT pg_notify($12, k.kind) FROM (SELECT DISTINCT kind FROM inserted) k \
             WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' AND kind = k.kind \
                               AND run_at <= now())) \
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

/// The distinct priorities of the ready jobs, upwards, one `jobs_ready_idx` probe each.
macro_rules! claim_bands {
    () => {
        "bands (priority) AS ( \
            (SELECT priority FROM jobs WHERE state = 'ready' ORDER BY priority LIMIT 1) \
            UNION ALL \
            SELECT (SELECT j.priority FROM jobs j \
                    WHERE j.state = 'ready' AND j.priority > b.priority \
                    ORDER BY j.priority LIMIT 1) \
            FROM bands b WHERE b.priority IS NOT NULL) "
    };
}

macro_rules! claim_update {
    () => {
        "UPDATE jobs SET state = 'running', attempts = attempts + 1, locked_by = $1, \
            lease_token = gen_random_uuid(), \
            lease_expires_at = now() + make_interval(secs => $2) "
    };
}

/// The oldest due jobs of one of `$3`, up to `$4`, band by band upwards, locked.
macro_rules! claim_first_due {
    () => {
        "ARRAY(SELECT c.id FROM bands b CROSS JOIN LATERAL ( \
            SELECT j.id FROM jobs j \
            WHERE j.state = 'ready' AND j.priority = b.priority AND j.run_at <= now() \
            AND j.kind = ANY($3) \
            ORDER BY j.run_at \
            LIMIT $4 \
            FOR UPDATE SKIP LOCKED) c \
            LIMIT $4)"
    };
}

macro_rules! claim_returning {
    () => {
        " RETURNING id, workspace_id, kind, payload, priority, run_at, attempts, max_attempts, \
            lease_token, unique_key, trace_id, cause_kind, cause_id, \
            EXTRACT(EPOCH FROM now() - run_at)::float8 AS claim_lag_seconds"
    };
}

/// The claim: up to `$4` due jobs of one of `$3`, by priority band, then age.
///
/// `bands` walks the distinct priorities of the ready jobs upwards, one `jobs_ready_idx` probe
/// each, and the lateral takes the oldest due jobs of each band in turn: a bounded range of
/// `(priority, run_at)` per band. A claim's cost therefore does not grow with the due or the
/// future jobs, whatever the statistics say (#207, `docs/benchmarks/m5-flows.md`). The
/// recursion yields the bands in ascending order and the outer `LIMIT` stops at the band that
/// fills the batch, so a later band is read only while the bands before it held fewer than `$4`
/// due jobs. Due jobs of kinds outside `$3` in a band are stepped over. A row is locked as the
/// limit takes it, so a claim locks only the jobs it claims.
const CLAIM: &str = concat!(
    "WITH RECURSIVE ",
    claim_bands!(),
    claim_update!(),
    "WHERE id = ANY(",
    claim_first_due!(),
    ")",
    claim_returning!()
);

/// The claim under a per-workspace cap of `$5` running jobs; instance jobs are never capped.
///
/// `running` counts each workspace's running jobs once, from `jobs_workspace_running_idx`, and
/// `capped` lists the workspaces at their cap. While the cap is at least `$4` and no workspace
/// runs more than `$5 - $4` jobs, no batch can take one past its cap, and the claim is
/// [`CLAIM`]'s. Otherwise a band's due jobs are not read in `run_at` order, which would step
/// over a capped workspace's flood of due jobs row by row. `heads` instead walks the workspaces
/// upwards in `jobs_ready_workspace_idx` from below the lowest id (the nil one), one probe per
/// workspace and band (a skip scan): a capped workspace is passed over at once, and each other
/// workspace finds the first band that holds a due job of one of `$3`. From that band it offers
/// its oldest due jobs of those kinds, as many as its cap has room for (`$5` less its running
/// jobs), so a batch never takes a workspace past its cap, and at most `$4`. The instance jobs'
/// bands are walked in `jobs_ready_instance_idx` the way [`CLAIM`] walks them, and offer at most
/// `$4`. A claim's cost is therefore bounded by the workspaces with ready jobs, their bands and
/// `$4`, not by any workspace's backlog (#207); as in [`CLAIM`], due jobs of other kinds are
/// stepped over. Every workspace probe is a row comparison led by `workspace_id`, so no index
/// but that one can answer it, whatever the statistics.
///
/// A batch takes only the offers of the best band offered (`best`), workspace and instance
/// offers alike: a workspace offers from its own first band alone, so a later band's offers
/// could otherwise come before the same workspace's jobs of an earlier band, which wait for the
/// next claim (ruling a894b5c2). A batch is therefore smaller when the best band offers fewer
/// than `$4` jobs, and empty for one claim when other transactions lock every one of them. The
/// offers are read without locks and tried by age, `FOR UPDATE SKIP LOCKED`, so a claim locks
/// only the rows it takes; an offer that is locked is passed over until the next claim. Claims
/// under a cap are serialised by [`claim_jobs`], so no other claim moves a head or changes a
/// running count meanwhile.
const CLAIM_CAPPED: &str = concat!(
    "WITH RECURSIVE running AS MATERIALIZED ( \
        SELECT workspace_id, count(*) AS running FROM jobs \
        WHERE state = 'running' AND workspace_id IS NOT NULL GROUP BY workspace_id), \
    capped AS MATERIALIZED (SELECT workspace_id FROM running WHERE running >= $5), ",
    claim_bands!(),
    ", heads (workspace_id, priority, id, run_at) AS ( \
        SELECT '00000000-0000-0000-0000-000000000000'::uuid, 32767::smallint, NULL::uuid, \
            NULL::timestamptz \
        UNION ALL \
        SELECT n.workspace_id, n.priority, h.id, h.run_at FROM heads p \
        CROSS JOIN LATERAL ( \
            SELECT j.workspace_id, j.priority FROM jobs j \
            WHERE j.state = 'ready' AND j.workspace_id IS NOT NULL \
            AND ROW(j.workspace_id, j.priority) > ROW(p.workspace_id, \
                CASE WHEN p.id IS NULL AND p.workspace_id NOT IN (SELECT workspace_id FROM capped) \
                THEN p.priority ELSE 32767::smallint END) \
            ORDER BY j.workspace_id, j.priority LIMIT 1) n \
        LEFT JOIN LATERAL ( \
            SELECT j.id, j.run_at FROM jobs j \
            WHERE n.workspace_id NOT IN (SELECT workspace_id FROM capped) \
            AND j.state = 'ready' AND j.workspace_id IS NOT NULL \
            AND ROW(j.workspace_id, j.priority) >= ROW(n.workspace_id, n.priority) \
            AND ROW(j.workspace_id, j.priority, j.run_at) \
                <= ROW(n.workspace_id, n.priority, now()) \
            AND j.kind = ANY($3) \
            ORDER BY j.workspace_id, j.priority, j.run_at LIMIT 1) h ON true), \
    instance_bands (priority) AS ( \
        (SELECT priority FROM jobs WHERE state = 'ready' AND workspace_id IS NULL \
         ORDER BY priority LIMIT 1) \
        UNION ALL \
        SELECT (SELECT j.priority FROM jobs j \
                WHERE j.state = 'ready' AND j.workspace_id IS NULL AND j.priority > b.priority \
                ORDER BY j.priority LIMIT 1) \
        FROM instance_bands b WHERE b.priority IS NOT NULL) ",
    claim_update!(),
    "WHERE id = ANY(CASE WHEN $5 >= $4 \
        AND NOT EXISTS (SELECT FROM running WHERE running > $5 - $4) THEN ",
    claim_first_due!(),
    " ELSE ARRAY(SELECT l.id FROM ( \
        SELECT c.id, c.run_at FROM ( \
        SELECT c.id, c.priority, c.run_at, min(c.priority) OVER () AS best FROM ( \
            SELECT o.id, h.priority, o.run_at FROM heads h \
            LEFT JOIN running r ON r.workspace_id = h.workspace_id \
            CROSS JOIN LATERAL ( \
                SELECT j.id, j.run_at FROM jobs j \
                WHERE j.state = 'ready' AND j.workspace_id IS NOT NULL \
                AND ROW(j.workspace_id, j.priority) >= ROW(h.workspace_id, h.priority) \
                AND ROW(j.workspace_id, j.priority, j.run_at) \
                    <= ROW(h.workspace_id, h.priority, now()) \
                AND j.kind = ANY($3) \
                ORDER BY j.workspace_id, j.priority, j.run_at \
                LIMIT LEAST($4, $5 - COALESCE(r.running, 0))) o \
            WHERE h.id IS NOT NULL \
            UNION ALL \
            (SELECT i.id, b.priority, i.run_at FROM instance_bands b CROSS JOIN LATERAL ( \
                SELECT j.id, j.run_at FROM jobs j \
                WHERE j.state = 'ready' AND j.workspace_id IS NULL AND j.priority = b.priority \
                AND j.run_at <= now() AND j.kind = ANY($3) \
                ORDER BY j.run_at LIMIT $4) i \
             LIMIT $4)) c) c \
        WHERE c.priority = c.best \
        ORDER BY c.run_at) c \
    CROSS JOIN LATERAL ( \
        SELECT j.id FROM jobs j WHERE j.id = c.id AND j.state = 'ready' \
        FOR UPDATE SKIP LOCKED) l \
    LIMIT $4) END)",
    claim_returning!()
);

/// Claims up to `n` runnable jobs of one of `kinds`, by priority, then age; none when nothing is
/// due.
///
/// One short statement of its own, committed before any handler runs, so a batch costs one round
/// trip and one commit whatever its size. Each job gets its own lease token and lease, and its
/// attempt is counted. With `max_running_per_workspace`, a workspace that already has that many
/// running jobs is skipped, a batch takes no workspace past it (its running jobs and the batch's
/// counted together), and claims are serialised so the cap holds under concurrent workers. The
/// jobs come back in claim order.
pub async fn claim_jobs(
    pool: &PgPool,
    worker: &str,
    kinds: &[&str],
    lease: Duration,
    max_running_per_workspace: Option<u32>,
    n: u32,
) -> Result<Vec<ClaimedJob>, StoreError> {
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut claimed: Vec<ClaimedRow> = match max_running_per_workspace {
        // Without a cap one statement is enough.
        None => sqlx::query_as(CLAIM)
            .bind(worker)
            .bind(lease.as_secs_f64())
            .bind(kinds)
            .bind(i64::from(n))
            .fetch_all(pool)
            .await
            .map_err(StoreError::query)?,
        // With one, claims queue up on a transaction lock so the counts each claim sees
        // include the claims before it.
        Some(cap) => {
            let mut tx = pool.begin().await.map_err(StoreError::query)?;
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(CLAIM_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .map_err(StoreError::query)?;
            let claimed = sqlx::query_as(CLAIM_CAPPED)
                .bind(worker)
                .bind(lease.as_secs_f64())
                .bind(kinds)
                .bind(i64::from(n))
                .bind(i64::from(cap))
                .fetch_all(&mut *tx)
                .await
                .map_err(StoreError::query)?;
            tx.commit().await.map_err(StoreError::query)?;
            claimed
        }
    };
    // `RETURNING` follows the primary key, not the claim's order.
    claimed.sort_by_key(|row| (row.priority, row.run_at));
    claimed.into_iter().map(ClaimedRow::into_claimed).collect()
}

/// Claims the next runnable job of one of `kinds`, or none: [`claim_jobs`] of one.
pub async fn claim_job(
    pool: &PgPool,
    worker: &str,
    kinds: &[&str],
    lease: Duration,
    max_running_per_workspace: Option<u32>,
) -> Result<Option<ClaimedJob>, StoreError> {
    let claimed = claim_jobs(pool, worker, kinds, lease, max_running_per_workspace, 1).await?;
    Ok(claimed.into_iter().next())
}

/// Gives back claimed jobs whose handlers never started, each fenced on its lease token: back to
/// `ready` as before the claim, with its `run_at` kept and the claim's attempt no longer counted,
/// and the workers woken as an enqueue wakes them. A job an erasure marked `purge_on_release`
/// while it was held is deleted instead, as on every other release, and a job whose lease was
/// lost is left alone. Returns how many jobs went back to `ready`.
///
/// The rows are taken `FOR UPDATE` in id order, the order an erasure and the reaper lock in. No
/// handler holds them (a handler takes its `lock_lease` only once it starts), so this never
/// waits on one.
pub async fn release_jobs<'e>(
    executor: impl PgExecutor<'e>,
    jobs: &[(Uuid, Uuid)],
) -> Result<u64, StoreError> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let (ids, tokens): (Vec<Uuid>, Vec<Uuid>) = jobs.iter().copied().unzip();
    let released: Vec<String> = sqlx::query_scalar(
        "WITH held AS ( \
             SELECT j.id, j.purge_on_release FROM jobs j \
             JOIN unnest($1::uuid[], $2::uuid[]) AS t (id, lease_token) \
               ON j.id = t.id AND j.lease_token = t.lease_token \
             WHERE j.state = 'running' \
             ORDER BY j.id FOR UPDATE OF j), \
         purged AS ( \
             DELETE FROM jobs WHERE id IN (SELECT id FROM held WHERE purge_on_release)), \
         released AS ( \
             UPDATE jobs SET state = 'ready', attempts = attempts - 1, lease_token = NULL, \
                lease_expires_at = NULL, locked_by = NULL \
             WHERE id IN (SELECT id FROM held WHERE NOT purge_on_release) \
             RETURNING kind), \
         notified AS ( \
             SELECT pg_notify($3, k.kind) FROM (SELECT DISTINCT kind FROM released) k \
             WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' AND kind = k.kind \
                               AND run_at <= now())) \
         SELECT kind FROM released, (SELECT count(*) FROM notified) AS n",
    )
    .bind(&ids)
    .bind(&tokens)
    .bind(NOTIFY_CHANNEL)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(u64::try_from(released.len()).unwrap_or(u64::MAX))
}

/// Locks the job row in the handler's transaction, to confirm the lease is still held.
///
/// False means the row is gone or carries another token: the lease was lost and the
/// transaction must be abandoned. The lock is a key-share lock, which still lets the
/// heartbeat extend the lease while the handler runs; completion is then fenced by
/// [`complete_job`] in the same transaction.
///
/// The lock keeps the job from being claimed again while this transaction is open, since a claim
/// skips locked rows, and keeps the reaper from deleting it, since a delete needs a lock that
/// conflicts with this one. A row lock also gives the transaction an id, which holds the xmin
/// horizon back until it ends; a kind safe to overlap with a late attempt of itself confirms its
/// lease with [`confirm_lease`] instead (#397).
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

/// Confirms the lease is still held without locking the job row: false means the row is gone
/// or carries another token. The read takes no transaction id and keeps no snapshot once it
/// returns, so a transaction left open across a long handler does not hold the xmin horizon
/// back. Nothing keeps another worker from claiming the job once the reaper puts it back, so
/// only a kind that is safe to overlap with a late attempt of itself uses this; its completion
/// is still fenced by [`complete_job`] (#397).
pub async fn confirm_lease<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    lease_token: Uuid,
) -> Result<bool, StoreError> {
    let row = sqlx::query("SELECT 1 FROM jobs WHERE id = $1 AND lease_token = $2")
        .bind(id)
        .bind(lease_token)
        .fetch_optional(executor)
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
/// job the same statement appends a `job.attempt` decision, `failed` or `exhausted`,
/// with reason `job_error`; the diagnostic stays in `last_error`.
///
/// The `locked` CTE takes the row first: when an erasure's `purge_on_release` mark commits while
/// this waits, the lock rechecks against the newest row version, so the flag it returns (not the
/// statement snapshot's) picks between the delete and the requeue.
pub async fn fail_job<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
    lease_token: Uuid,
    retry_in: Duration,
    error: &str,
) -> Result<Option<FailOutcome>, StoreError> {
    let state: Option<String> = sqlx::query_scalar(
        "WITH locked AS ( \
             SELECT purge_on_release FROM jobs \
             WHERE id = $1 AND lease_token = $2 FOR NO KEY UPDATE), \
         purged AS ( \
             DELETE FROM jobs WHERE id = $1 AND lease_token = $2 \
               AND (SELECT purge_on_release FROM locked) \
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
             WHERE id = $1 AND lease_token = $2 AND NOT (SELECT purge_on_release FROM locked) \
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
             SELECT gen_random_uuid(), workspace_id, trace_id, $5::text, \
                    CASE WHEN state = 'dead' THEN $6::text ELSE $7::text END, \
                    'job_error', cause_kind, cause_id, id, attempts \
             FROM updated WHERE workspace_id IS NOT NULL) \
         SELECT state FROM updated UNION ALL SELECT 'purged' FROM purged",
    )
    .bind(id)
    .bind(lease_token)
    .bind(retry_in.as_secs_f64())
    .bind(truncate_error(error))
    .bind(decision_actions::JOB_ATTEMPT_FAILED.action())
    .bind(decision_actions::JOB_ATTEMPT_EXHAUSTED.outcome())
    .bind(decision_actions::JOB_ATTEMPT_FAILED.outcome())
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
/// reaper therefore never waits on a handler's lease lock, which would stall every other expired
/// job behind one zombie and deadlock with a handler that completes. It does wait for a worker's
/// batch transaction (#397) that already completed or failed a job whose lease ran out while the
/// batch's later jobs ran; once the batch commits, the row is gone or settled and is skipped.
/// A kind that confirms its lease with [`confirm_lease`] holds no row lock, so nothing here
/// waits on it or skips it, and its late ack is refused the same way.
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
             SELECT gen_random_uuid(), workspace_id, trace_id, $1::text, \
                    CASE WHEN state = 'dead' THEN $2::text ELSE $3::text END, \
                    'lease_expired', cause_kind, cause_id, id, attempts \
             FROM updated WHERE workspace_id IS NOT NULL) \
         SELECT state FROM updated UNION ALL SELECT 'purged' FROM purged",
    )
    .bind(decision_actions::JOB_ATTEMPT_LEASE_EXPIRED.action())
    .bind(decision_actions::JOB_ATTEMPT_EXHAUSTED.outcome())
    .bind(decision_actions::JOB_ATTEMPT_LEASE_EXPIRED.outcome())
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

/// Up to this many ready jobs of a kind are counted; above it the count is an estimate.
const DEPTH_EXACT: i64 = 10_000;

/// The planner's estimate of a kind's jobs in a state, for a ready count above [`DEPTH_EXACT`].
const DEPTH_ESTIMATE: &str = "SELECT 1 FROM jobs WHERE state = $1 AND kind = $2";

/// The depths with the ready jobs of each kind counted up to `$1`: a skip scan of
/// `jobs_ready_kind_idx` finds the kinds with ready jobs, one probe each, and each kind's count
/// reads at most `$1` entries of its range there. The running jobs are as many as the worker
/// slots hold and the dead ones as many as retention keeps, so both are counted in full from
/// their partial indexes. No part reads the whole table, however long the queue (#397).
const DEPTHS: &str = "WITH RECURSIVE kinds (kind) AS ( \
        (SELECT kind FROM jobs WHERE state = 'ready' ORDER BY kind LIMIT 1) \
        UNION ALL \
        SELECT (SELECT j.kind FROM jobs j WHERE j.state = 'ready' AND j.kind > k.kind \
                ORDER BY j.kind LIMIT 1) \
        FROM kinds k WHERE k.kind IS NOT NULL) \
    SELECT k.kind, 'ready' AS state, \
        (SELECT count(*) FROM (SELECT 1 FROM jobs j WHERE j.state = 'ready' AND j.kind = k.kind \
                               ORDER BY j.run_at LIMIT $1) c) AS count \
    FROM kinds k WHERE k.kind IS NOT NULL \
    UNION ALL \
    SELECT kind, 'running', count(*) FROM jobs WHERE state = 'running' GROUP BY kind \
    UNION ALL \
    SELECT kind, 'dead', count(*) FROM jobs WHERE state = 'dead' GROUP BY kind";

/// Job counts by kind and state, for the queue-depth gauges. The running and dead counts are
/// exact, and so is each kind's ready count up to 10,000; above that it is the planner's
/// estimate from the table statistics, and never below 10,001, so a long backlog costs no more to
/// gauge than a short one.
pub async fn queue_depths(connection: &mut PgConnection) -> Result<Vec<QueueDepth>, StoreError> {
    depths_up_to(connection, DEPTH_EXACT).await
}

async fn depths_up_to(
    connection: &mut PgConnection,
    exact: i64,
) -> Result<Vec<QueueDepth>, StoreError> {
    // Planned for the limit's value each time, never with a generic plan.
    let mut depths: Vec<QueueDepth> = sqlx::query_as(DEPTHS)
        .persistent(false)
        .bind(exact + 1)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    for depth in depths.iter_mut().filter(|depth| depth.state == "ready") {
        depth.count = crate::bounded::count_or_estimate(
            connection,
            depth.count,
            exact,
            DEPTH_ESTIMATE,
            &["ready", depth.kind.as_str()],
        )
        .await?;
    }
    Ok(depths)
}

/// The longest a queue-depth gauge statement may run.
const DEPTH_TIMEOUT: Duration = Duration::from_secs(1);

/// [`queue_depths`] under a statement timeout of a second, for the leader's gauges: the reads
/// are bounded, and the timeout keeps a slow database from holding the tick.
pub async fn sample_queue_depths(pool: &PgPool) -> Result<Vec<QueueDepth>, StoreError> {
    let mut tx = crate::bounded::begin_bounded_for(pool, DEPTH_TIMEOUT).await?;
    let depths = queue_depths(&mut tx).await?;
    tx.commit().await.map_err(StoreError::query)?;
    Ok(depths)
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
         SELECT updated.*, \
                CASE WHEN NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' \
                                      AND kind = updated.kind AND run_at <= now()) \
                     THEN pg_notify($2, kind) END AS notified \
         FROM updated",
    )
    .bind(id)
    .bind(NOTIFY_CHANNEL)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    row.map(JobSummaryRow::into_summary).transpose()
}

/// Deletes up to `batch` dead jobs whose `dead_at` is before `cutoff`, oldest first, in one
/// committed statement, and returns how many it deleted. `ready` and `running` jobs are never
/// touched. A caller loops until a batch comes back short.
pub async fn purge_dead_jobs<'e>(
    executor: impl PgExecutor<'e>,
    cutoff: OffsetDateTime,
    batch: i64,
) -> Result<i64, StoreError> {
    let deleted = sqlx::query(
        "DELETE FROM jobs WHERE id IN ( \
             SELECT id FROM jobs WHERE state = 'dead' AND dead_at < $1 \
             ORDER BY dead_at, id LIMIT $2 FOR UPDATE SKIP LOCKED)",
    )
    .bind(cutoff)
    .bind(batch)
    .execute(executor)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    Ok(i64::try_from(deleted).unwrap_or(i64::MAX))
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use serde_json::json;
    use sqlx::{PgPool, Row};
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

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn purging_dead_jobs_removes_only_old_dead_rows_in_batches(pool: PgPool) {
        for (state, age_days, kind) in [
            ("dead", 20_i32, "old.dead.1"),
            ("dead", 30, "old.dead.2"),
            ("dead", 40, "old.dead.3"),
            ("dead", 2, "young.dead"),
            ("ready", 40, "old.ready"),
            ("running", 40, "old.running"),
        ] {
            sqlx::query(
                "INSERT INTO jobs (id, kind, state, payload, trace_id, cause_kind, cause_id, \
                    dead_at, run_at) \
                 VALUES (gen_random_uuid(), $1, $2, '{}', gen_random_uuid(), 'manual', '', \
                         CASE WHEN $2 = 'dead' THEN now() - make_interval(days => $3) END, now())",
            )
            .bind(kind)
            .bind(state)
            .bind(age_days)
            .execute(&pool)
            .await
            .unwrap();
        }
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(14);

        assert_eq!(purge_dead_jobs(&pool, cutoff, 2).await.unwrap(), 2);
        assert_eq!(purge_dead_jobs(&pool, cutoff, 2).await.unwrap(), 1);
        assert_eq!(purge_dead_jobs(&pool, cutoff, 2).await.unwrap(), 0);

        let mut left: Vec<String> = sqlx::query_scalar("SELECT kind FROM jobs")
            .fetch_all(&pool)
            .await
            .unwrap();
        left.sort();
        assert_eq!(left, ["old.ready", "old.running", "young.dead"]);
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
        enqueue(&pool, &job("c.kind")).await.unwrap();
        claim(&pool, &["b.kind"]).await.unwrap();
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, state, dead_at, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), k, '{}', 'dead', now(), gen_random_uuid(), 'api', 'x' \
             FROM unnest(ARRAY['a.kind', 'c.kind', 'c.kind']) k",
        )
        .execute(&pool)
        .await
        .unwrap();

        let depths = sorted(
            queue_depths(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
        );

        let summary: Vec<(&str, &str, i64)> = depths
            .iter()
            .map(|d| (d.kind.as_str(), d.state.as_str(), d.count))
            .collect();
        assert_eq!(
            summary,
            [
                ("a.kind", "dead", 1),
                ("a.kind", "ready", 2),
                ("b.kind", "running", 1),
                ("c.kind", "dead", 2),
                ("c.kind", "ready", 1)
            ]
        );
        let full: Vec<QueueDepth> =
            sqlx::query_as("SELECT kind, state, count(*) AS count FROM jobs GROUP BY kind, state")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            depths,
            sorted(full),
            "below the limit, every count is exact"
        );
    }

    fn sorted(mut depths: Vec<QueueDepth>) -> Vec<QueueDepth> {
        depths.sort_by(|a, b| (&a.kind, &a.state).cmp(&(&b.kind, &b.state)));
        depths
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_ready_count_above_the_limit_is_the_planners_estimate(pool: PgPool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), CASE WHEN g <= 3 THEN 'b.kind' ELSE 'a.kind' END, '{}', \
                    gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, 403) g",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let depths = sorted(depths_up_to(&mut connection, 5).await.unwrap());

        let summary: Vec<(&str, i64)> = depths.iter().map(|d| (d.kind.as_str(), d.count)).collect();
        // ANALYZE reads every row of a table this small, so the estimate is the count.
        assert_eq!(summary, [("a.kind", 400), ("b.kind", 3)]);
        assert!(depths.iter().all(|d| d.state == "ready"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_queue_depths_read_a_bounded_part_of_a_long_queue(pool: PgPool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, run_at, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), CASE WHEN g % 1000 = 0 THEN 'b.kind' ELSE 'a.kind' END, \
                    '{}', now() - make_interval(secs => g), gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, 60000) g ORDER BY random()",
        )
        .execute(&pool)
        .await
        .unwrap();
        enqueue(&pool, &job("c.kind")).await.unwrap();
        claim(&pool, &["c.kind"]).await.unwrap();
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        let pages: i64 = sqlx::query_scalar(
            "SELECT pg_relation_size('jobs') / current_setting('block_size')::bigint",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // A limit of 100 here, so the bound shows against a table a test can fill.
        let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {DEPTHS}"
        )))
        .bind(101_i64)
        .fetch_one(&pool)
        .await
        .unwrap();
        let top = &plan[0]["Plan"];
        let buffers = top["Shared Hit Blocks"].as_i64().unwrap()
            + top["Shared Read Blocks"].as_i64().unwrap();

        let scanned: Vec<&Value> = plan_nodes(top)
            .into_iter()
            .filter(|node| node["Node Type"] == "Seq Scan")
            .collect();
        assert!(scanned.is_empty(), "{plan}");
        assert_eq!(top["Actual Rows"], 3, "{plan}");
        // The ready count of `a.kind` stops at the limit: about one heap page per entry it reads,
        // against the whole table's.
        assert!(pages > 800, "{pages}");
        assert!(buffers < 400, "{buffers} buffers for {pages} pages\n{plan}");
        let depths = sorted(
            queue_depths(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
        );
        assert_eq!(
            depths
                .iter()
                .map(|d| (d.kind.as_str(), d.state.as_str()))
                .collect::<Vec<_>>(),
            [
                ("a.kind", "ready"),
                ("b.kind", "ready"),
                ("c.kind", "running")
            ]
        );
        assert!(depths[0].count > DEPTH_EXACT, "{depths:?}");
        assert_eq!(depths[1].count, 60, "{depths:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_timed_queue_depths_match_the_plain_ones_and_fail_on_a_closed_pool(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        enqueue(&pool, &job("b.kind")).await.unwrap();
        claim(&pool, &["b.kind"]).await.unwrap();

        let timed = sorted(sample_queue_depths(&pool).await.unwrap());
        let plain = sorted(
            queue_depths(&mut pool.acquire().await.unwrap())
                .await
                .unwrap(),
        );

        assert_eq!(timed, plain);
        assert_eq!(timed.len(), 2);
        pool.close().await;
        assert!(sample_queue_depths(&pool).await.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_timed_queue_depths_give_up_after_a_second(pool: PgPool) {
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE jobs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let started = std::time::Instant::now();

        let blocked = sample_queue_depths(&pool).await;

        let waited = started.elapsed();
        blocker.rollback().await.unwrap();
        assert!(blocked.is_err());
        assert!(
            waited >= Duration::from_millis(900) && waited < Duration::from_secs(4),
            "{waited:?}"
        );
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
                ("job.attempt", "failed", Some("job_error"), Some(1)),
                ("job.attempt", "exhausted", Some("job_error"), Some(2)),
            ]
        );
        assert_eq!(records[0].cause, trace.cause);
        assert_eq!(records[0].job_id, Some(first.id));
        assert_eq!(
            find_job(&pool, second.id)
                .await
                .unwrap()
                .unwrap()
                .last_error
                .as_deref(),
            Some("boom")
        );
        crate::decision_testing::assert_decisions_hold_no_personal_data(&pool).await;
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
            assert_eq!(records[0].reason.as_deref(), Some("lease_expired"));
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
    async fn concurrent_claimers_respect_the_cap_and_claim_quiet_work_in_the_first_round(
        pool: PgPool,
    ) {
        crate::decision_testing::checked(pool, |pool| async move {
            const CLAIMERS: usize = 8;
            let busy = workspace(&pool, "busy").await;
            let quiet = workspace(&pool, "quiet").await;
            for _ in 0..50 {
                let mut new = job("a.kind");
                new.workspace_id = Some(busy);
                enqueue(&pool, &new).await.unwrap();
            }
            let mut other = job("a.kind");
            other.workspace_id = Some(quiet);
            other.priority = 1;
            enqueue(&pool, &other).await.unwrap();

            let barrier = Arc::new(tokio::sync::Barrier::new(CLAIMERS));
            let mut tasks = tokio::task::JoinSet::new();
            for worker in 0..CLAIMERS {
                let pool = pool.clone();
                let barrier = barrier.clone();
                tasks.spawn(async move {
                    let name = format!("claimer-{worker}");
                    let (mut completed, mut quiet_claims, mut max_running) = (0, 0, 0_i64);
                    // Each round holds its claims until all eight attempts have sampled the
                    // database. With no completions yet, at most two busy claims can precede
                    // the quiet job: it must be claimed within the first eight attempts.
                    // Two busy jobs per round drain the 50-job backlog in 25 rounds.
                    for round in 0..25 {
                        barrier.wait().await;
                        let claimed = claim_job(&pool, &name, &["a.kind"], LEASE, Some(2))
                            .await
                            .unwrap();
                        let running: i64 = sqlx::query_scalar(
                            "SELECT count(*) FROM jobs WHERE workspace_id = $1 AND state = 'running'",
                        )
                        .bind(busy.as_uuid())
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                        assert!(running <= 2, "round {round}: {running} busy jobs running");
                        max_running = max_running.max(running);
                        barrier.wait().await;
                        if let Some(claimed) = claimed {
                            if claimed.workspace_id == Some(quiet.as_uuid()) {
                                assert_eq!(round, 0, "quiet work missed the first eight attempts");
                                quiet_claims += 1;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            assert!(
                                complete_job(&pool, claimed.id, claimed.lease_token)
                                    .await
                                    .unwrap()
                            );
                            completed += 1;
                        }
                    }
                    (completed, quiet_claims, max_running)
                });
            }

            tokio::time::timeout(Duration::from_secs(30), async {
                let (mut completed, mut quiet_claims, mut max_running) = (0, 0, 0_i64);
                while let Some(result) = tasks.join_next().await {
                    let (done, quiet, running) = result.unwrap();
                    completed += done;
                    quiet_claims += quiet;
                    max_running = max_running.max(running);
                }
                assert_eq!(completed, 51);
                assert_eq!(quiet_claims, 1);
                assert_eq!(max_running, 2, "the claimers must overlap at the cap");
            })
            .await
            .expect("eight claimers did not drain the queue within 30 seconds");
            let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'a.kind'")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(left, 0);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn uncapped_claims_bypass_the_advisory_lock_while_capped_claims_wait(pool: PgPool) {
        crate::decision_testing::checked(pool, |pool| async move {
            let ws = workspace(&pool, "ours").await;
            let mut new = job("a.kind");
            new.workspace_id = Some(ws);
            for _ in 0..2 {
                enqueue(&pool, &new).await.unwrap();
            }
            let mut holder = pool.begin().await.unwrap();
            let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *holder)
                .await
                .unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(CLAIM_LOCK_KEY)
                .execute(&mut *holder)
                .await
                .unwrap();

            let uncapped = tokio::time::timeout(
                Duration::from_secs(10),
                claim_job(&pool, "uncapped", &["a.kind"], LEASE, None),
            )
            .await
            .expect("an uncapped claim waited for the advisory lock")
            .unwrap()
            .unwrap();
            assert!(
                complete_job(&pool, uncapped.id, uncapped.lease_token)
                    .await
                    .unwrap()
            );

            let mut tasks = tokio::task::JoinSet::new();
            let capped_pool = pool.clone();
            tasks.spawn(async move {
                claim_job(&capped_pool, "capped", &["a.kind"], LEASE, Some(2)).await
            });
            // Observe the lock wait before using a short timeout: a task that has not
            // connected yet is not evidence that capped claims take the advisory lock.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let waiting: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory' \
                         AND NOT granted AND $1 = ANY(pg_blocking_pids(pid)))",
                    )
                    .bind(holder_pid)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
                    if waiting {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the capped claim never waited for the held advisory lock");
            assert!(
                tokio::time::timeout(Duration::from_millis(200), tasks.join_next())
                    .await
                    .is_err(),
                "the capped claim completed while the advisory lock was held"
            );
            holder.rollback().await.unwrap();
            let capped = tokio::time::timeout(Duration::from_secs(10), tasks.join_next())
                .await
                .expect("the capped claim did not resume after the lock was released")
                .unwrap()
                .unwrap()
                .unwrap()
                .unwrap();
            assert_ne!(uncapped.id, capped.id);
            assert!(
                complete_job(&pool, capped.id, capped.lease_token)
                    .await
                    .unwrap()
            );
        })
        .await;
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
    async fn confirming_a_lease_takes_no_transaction_id_and_leaves_the_job_claimable(pool: PgPool) {
        enqueue(&pool, &job("a.kind")).await.unwrap();
        let claimed = claim_job(&pool, "w", &["a.kind"], Duration::from_millis(1), None)
            .await
            .unwrap()
            .unwrap();
        const ASSIGNED: &str = "SELECT pg_current_xact_id_if_assigned()::text";
        let mut locked = pool.begin().await.unwrap();
        assert!(
            lock_lease(&mut locked, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        let locked_xid: Option<String> = sqlx::query_scalar(ASSIGNED)
            .fetch_one(&mut *locked)
            .await
            .unwrap();
        assert!(locked_xid.is_some(), "a row lock takes a transaction id");
        locked.rollback().await.unwrap();
        let mut confirmed = pool.begin().await.unwrap();

        assert!(
            confirm_lease(&mut *confirmed, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        assert!(
            !confirm_lease(&mut *confirmed, claimed.id, Uuid::from_u128(7))
                .await
                .unwrap()
        );
        let confirmed_xid: Option<String> = sqlx::query_scalar(ASSIGNED)
            .fetch_one(&mut *confirmed)
            .await
            .unwrap();
        assert_eq!(confirmed_xid, None);
        // Nothing holds the row, so the reaper puts it back and another worker claims it while
        // the confirming transaction is still open; its late completion is then refused.
        wait_until_expired(&pool, claimed.id).await;
        assert_eq!(reap_expired(&pool).await.unwrap().requeued, 1);
        let again = claim(&pool, &["a.kind"]).await.unwrap();
        assert_eq!(again.id, claimed.id);
        assert!(
            !complete_job(&mut *confirmed, claimed.id, claimed.lease_token)
                .await
                .unwrap()
        );
        confirmed.rollback().await.unwrap();
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

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_live_enrollments_index_migration_accepts_the_index_built_by_hand(pool: PgPool) {
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 72)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        sqlx::query(
            "CREATE INDEX enrollments_live_idx ON enrollments (state, wake_at)              WHERE state IN ('active', 'waiting')",
        )
        .execute(&pool)
        .await
        .unwrap();

        crate::migrate(&pool).await.unwrap();

        let indexes: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE indexname = 'enrollments_live_idx'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(indexes, 1);
    }

    #[sqlx::test(migrations = false)]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_capped_claim_index_migration_accepts_the_indexes_built_by_hand(pool: PgPool) {
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = before
            .migrations
            .iter()
            .filter(|migration| migration.version < 88)
            .cloned()
            .collect();
        before.run(&pool).await.unwrap();
        for statement in [
            "CREATE INDEX CONCURRENTLY jobs_ready_workspace_idx \
             ON jobs (workspace_id, priority, run_at) \
             WHERE state = 'ready' AND workspace_id IS NOT NULL",
            "CREATE INDEX CONCURRENTLY jobs_ready_instance_idx ON jobs (priority, run_at) \
             WHERE state = 'ready' AND workspace_id IS NULL",
        ] {
            sqlx::raw_sql(statement).execute(&pool).await.unwrap();
        }

        crate::migrate(&pool).await.unwrap();

        let indexes: Vec<String> = sqlx::query_scalar(
            "SELECT indexname::text FROM pg_indexes \
             WHERE indexname IN ('jobs_ready_workspace_idx', 'jobs_ready_instance_idx') \
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            indexes,
            ["jobs_ready_instance_idx", "jobs_ready_workspace_idx"]
        );
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

    /// How a stress producer enqueues.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Notify {
        /// A plain insert, no `NOTIFY` at all (the control).
        Never,
        /// The pre-#298 statement: a `pg_notify` after every insert.
        Always,
        /// The production [`enqueue`].
        Production,
    }

    #[derive(Debug)]
    struct Stress {
        commits_per_second: f64,
        /// Share of samples in which at least one `COMMIT` waited on the database object lock.
        sample_share: f64,
        /// Share of in-flight `COMMIT` backends, over all samples, that were waiting.
        commit_share: f64,
        notifications: i64,
    }

    async fn stress(pool: &PgPool, mode: Notify, run_for: Duration) -> Stress {
        const PRODUCERS: usize = 6;
        const CONSUMERS: usize = 3;
        sqlx::query("TRUNCATE jobs").execute(pool).await.unwrap();
        let big = sqlx::postgres::PgPoolOptions::new()
            .max_connections(16)
            .connect_with((*pool.connect_options()).clone())
            .await
            .unwrap();
        let mut listener = PgListener::connect_with(&big).await.unwrap();
        listener.listen(NOTIFY_CHANNEL).await.unwrap();
        let counter = tokio::spawn(async move {
            let mut heard = 0_i64;
            while listener.try_recv().await.is_ok() {
                heard += 1;
            }
            heard
        });
        let stop = Arc::new(AtomicBool::new(false));
        let commits = Arc::new(AtomicU64::new(0));
        let mut tasks = Vec::new();
        for _ in 0..PRODUCERS {
            let (pool, stop, commits) = (big.clone(), stop.clone(), commits.clone());
            tasks.push(tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    let mut tx = pool.begin().await.unwrap();
                    match mode {
                        Notify::Production => {
                            enqueue(&mut *tx, &job("stress.kind")).await.unwrap();
                        }
                        Notify::Never | Notify::Always => {
                            sqlx::query(
                                "WITH inserted AS ( \
                                     INSERT INTO jobs (id, kind, payload, trace_id, cause_kind, cause_id) \
                                     VALUES (gen_random_uuid(), 'stress.kind', '{}', gen_random_uuid(), \
                                             'api', 'stress') RETURNING kind) \
                                 SELECT CASE WHEN $1 THEN pg_notify($2, kind) END FROM inserted",
                            )
                            .bind(mode == Notify::Always)
                            .bind(NOTIFY_CHANNEL)
                            .execute(&mut *tx)
                            .await
                            .unwrap();
                        }
                    }
                    tx.commit().await.unwrap();
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        for _ in 0..CONSUMERS {
            let (pool, stop) = (big.clone(), stop.clone());
            tasks.push(tokio::spawn(async move {
                while !stop.load(Ordering::Relaxed) {
                    match claim_job(&pool, "stress", &["stress.kind"], LEASE, None)
                        .await
                        .unwrap()
                    {
                        Some(claimed) => {
                            complete_job(&pool, claimed.id, claimed.lease_token)
                                .await
                                .unwrap();
                        }
                        None => tokio::time::sleep(Duration::from_millis(1)).await,
                    }
                }
            }));
        }
        let started = std::time::Instant::now();
        let (mut samples, mut waited, mut commit_backends, mut commit_waiters) =
            (0_u64, 0_u64, 0_i64, 0_i64);
        while started.elapsed() < run_for {
            let (in_commit, waiting): (i64, i64) = sqlx::query_as(
                "SELECT count(*) FILTER (WHERE a.query ILIKE 'COMMIT%'), \
                        count(*) FILTER (WHERE a.query ILIKE 'COMMIT%' AND a.wait_event_type = 'Lock' \
                                         AND a.wait_event = 'object') \
                 FROM pg_stat_activity a \
                 WHERE a.datname = current_database() AND a.pid <> pg_backend_pid() \
                   AND a.state = 'active'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            samples += 1;
            waited += u64::from(waiting > 0);
            commit_backends += in_commit;
            commit_waiters += waiting;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let elapsed = started.elapsed().as_secs_f64();
        stop.store(true, Ordering::Relaxed);
        for task in tasks {
            task.await.unwrap();
        }
        let done = commits.load(Ordering::Relaxed);
        big.close().await;
        let notifications = counter.await.unwrap_or(-1);
        let stress = Stress {
            commits_per_second: done as f64 / elapsed,
            sample_share: waited as f64 / samples.max(1) as f64,
            commit_share: commit_waiters as f64 / commit_backends.max(1) as f64,
            notifications,
        };
        eprintln!("stress {mode:?}: {stress:?} ({samples} samples)");
        stress
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_NOTIFY to run the 18 s stress"]
    async fn commits_do_not_wait_on_the_notify_lock_under_load(pool: PgPool) {
        if std::env::var_os("ZIRV_FLOW_BENCH_NOTIFY").is_none() {
            eprintln!("ZIRV_FLOW_BENCH_NOTIFY is not set: skipping the stress run");
            return;
        }
        let run_for = Duration::from_secs(6);
        let production = stress(&pool, Notify::Production, run_for).await;
        let never = stress(&pool, Notify::Never, run_for).await;
        let always = stress(&pool, Notify::Always, run_for).await;

        eprintln!(
            "RESULT never={:.0}/s share={:.3} | always={:.0}/s share={:.3} | production={:.0}/s share={:.3}",
            never.commits_per_second,
            never.commit_share,
            always.commits_per_second,
            always.commit_share,
            production.commits_per_second,
            production.commit_share,
        );
        assert!(never.sample_share < 0.02, "the control waits: {never:?}");
        assert!(
            always.commit_share > 0.2,
            "a notify per commit no longer serialises commits: {always:?}"
        );
        assert!(
            production.commit_share < 0.05,
            "enqueue still serialises commits: {production:?}"
        );
        assert!(production.notifications * 20 < always.notifications.max(1));
    }

    /// Compares the three enqueue variants fairly: one discarded warm-up each, then
    /// `ZIRV_FLOW_BENCH_NOTIFY_ROUNDS` rounds in a rotated order, `TRUNCATE jobs` before every run
    /// (done by `stress`), once with the ready-kind index and once without it. Prints the median
    /// and range of commits/s and the median lock-wait share; asserts nothing.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_NOTIFY_ROUNDS to measure"]
    async fn the_enqueue_variants_compared_over_rotated_rounds(pool: PgPool) {
        let Some(rounds) = std::env::var("ZIRV_FLOW_BENCH_NOTIFY_ROUNDS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|&rounds| rounds > 0)
        else {
            eprintln!(
                "ZIRV_FLOW_BENCH_NOTIFY_ROUNDS is not a positive count: skipping the comparison"
            );
            return;
        };
        let run_for = Duration::from_secs(6);
        let variants = [Notify::Never, Notify::Always, Notify::Production];
        for indexed in [true, false] {
            if !indexed {
                sqlx::query("DROP INDEX jobs_ready_kind_idx")
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            for variant in variants {
                stress(&pool, variant, run_for).await;
            }
            let mut results: Vec<(Notify, Vec<f64>, Vec<f64>)> = variants
                .iter()
                .map(|variant| (*variant, Vec::new(), Vec::new()))
                .collect();
            for round in 0..rounds {
                for step in 0..variants.len() {
                    let slot = (round + step) % variants.len();
                    let run = stress(&pool, variants[slot], run_for).await;
                    results[slot].1.push(run.commits_per_second);
                    results[slot].2.push(run.commit_share);
                }
            }
            for (variant, mut rates, mut shares) in results {
                rates.sort_by(f64::total_cmp);
                shares.sort_by(f64::total_cmp);
                eprintln!(
                    "SUMMARY index={indexed} {variant:?}: median {:.0}/s range {:.0}-{:.0} median share {:.3}",
                    rates[rates.len() / 2],
                    rates[0],
                    rates[rates.len() - 1],
                    shares[shares.len() / 2],
                );
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_wake_check_uses_the_ready_kind_index_despite_a_large_backlog(pool: PgPool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), 'big.kind', '{}', gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, 5000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();

        let plan: serde_json::Value = sqlx::query_scalar(
            "EXPLAIN (FORMAT JSON) \
             SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM jobs WHERE state = 'ready' \
                                        AND kind = 'idle.kind' AND run_at <= now())",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        let mut indexes = Vec::new();
        let mut stack = vec![&plan];
        while let Some(node) = stack.pop() {
            match node {
                Value::Object(map) => {
                    if let Some(Value::String(name)) = map.get("Index Name") {
                        indexes.push(name.clone());
                    }
                    stack.extend(map.values());
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        assert_eq!(indexes, ["jobs_ready_kind_idx"], "{plan}");
    }

    async fn wait_for_notification(listener: &mut PgListener) -> Duration {
        let started = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), listener.recv())
            .await
            .expect("no wake-up within 5 seconds")
            .unwrap();
        started.elapsed()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_idle_queue_is_woken_as_fast_as_before(pool: PgPool) {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(NOTIFY_CHANNEL).await.unwrap();

        let before = std::time::Instant::now();
        sqlx::query("SELECT pg_notify($1, 'a.kind')")
            .bind(NOTIFY_CHANNEL)
            .execute(&pool)
            .await
            .unwrap();
        wait_for_notification(&mut listener).await;
        let old = before.elapsed();

        let before = std::time::Instant::now();
        enqueue(&pool, &job("a.kind")).await.unwrap();
        wait_for_notification(&mut listener).await;
        let new = before.elapsed();

        eprintln!("idle wake latency: old {old:?}, new {new:?}");
        assert!(new < Duration::from_millis(500), "{new:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_busy_kind_skips_the_wake_but_other_kinds_and_idle_ones_still_wake(pool: PgPool) {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(NOTIFY_CHANNEL).await.unwrap();
        async fn heard(listener: &mut PgListener) -> bool {
            tokio::time::timeout(Duration::from_millis(300), listener.recv())
                .await
                .is_ok()
        }

        enqueue(&pool, &job("a.kind")).await.unwrap();
        assert!(heard(&mut listener).await, "the first job wakes");
        enqueue(&pool, &job("a.kind")).await.unwrap();
        assert!(
            !heard(&mut listener).await,
            "a backlog of the kind is already awake"
        );
        enqueue(&pool, &job("b.kind")).await.unwrap();
        assert!(heard(&mut listener).await, "another kind wakes");

        let mut later = job("c.kind");
        later.run_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        enqueue(&pool, &later).await.unwrap();
        let _ = heard(&mut listener).await;
        enqueue(&pool, &job("c.kind")).await.unwrap();
        assert!(heard(&mut listener).await, "a job due later is no backlog");

        let mut rolled_back = pool.begin().await.unwrap();
        enqueue(&mut *rolled_back, &job("d.kind")).await.unwrap();
        rolled_back.rollback().await.unwrap();
        assert!(!heard(&mut listener).await, "a rollback wakes nobody");
        enqueue(&pool, &job("d.kind")).await.unwrap();
        assert!(
            heard(&mut listener).await,
            "the retry after a rollback wakes"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enqueue_many_wakes_only_idle_kinds(pool: PgPool) {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(NOTIFY_CHANNEL).await.unwrap();
        enqueue(&pool, &job("a.kind")).await.unwrap();
        listener.recv().await.unwrap();

        let inserted = enqueue_many(&pool, &[job("a.kind"), job("b.kind")])
            .await
            .unwrap();

        assert_eq!(inserted, 2);
        let woken = tokio::time::timeout(Duration::from_secs(2), listener.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(woken.payload(), "b.kind");
        let more = tokio::time::timeout(Duration::from_millis(300), listener.recv()).await;
        assert!(more.is_err(), "a.kind is busy");
    }

    /// The claim statement before #207, the baseline of the claim benchmark and of the plan
    /// check. Its `ORDER BY priority, run_at` has two plans, both growing with the queue: a
    /// scan of `jobs_ready_idx` that only filters on `run_at` and so steps over every future job
    /// ahead of the first due one, or, when the statistics put almost nothing due,
    /// `jobs_ready_kind_idx` and a sort of every due job on each claim.
    const CLAIM_BEFORE_207: &str = "UPDATE jobs SET state = 'running', \
         attempts = attempts + 1, locked_by = $1, \
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
         EXTRACT(EPOCH FROM now() - run_at)::float8 AS claim_lag_seconds";

    /// The band claim before the cap path got its skip scan, the baseline of the capped claim
    /// benchmark and plan check: under a cap it reads each band in `run_at` order and counts the
    /// running jobs of each row's workspace, so it steps over a capped workspace's due jobs one
    /// by one.
    const CLAIM_WITH_PER_ROW_CAP: &str = "WITH RECURSIVE bands (priority) AS ( \
         (SELECT priority FROM jobs WHERE state = 'ready' ORDER BY priority LIMIT 1) \
         UNION ALL \
         SELECT (SELECT j.priority FROM jobs j WHERE j.state = 'ready' AND j.priority > b.priority \
         ORDER BY j.priority LIMIT 1) \
         FROM bands b WHERE b.priority IS NOT NULL) \
         UPDATE jobs SET state = 'running', attempts = attempts + 1, locked_by = $1, \
         lease_token = gen_random_uuid(), \
         lease_expires_at = now() + make_interval(secs => $2) \
         WHERE id = (SELECT c.id FROM bands b CROSS JOIN LATERAL ( \
         SELECT j.id FROM jobs j \
         WHERE j.state = 'ready' AND j.priority = b.priority AND j.run_at <= now() \
         AND j.kind = ANY($3) \
         AND ($4::bigint IS NULL OR j.workspace_id IS NULL \
         OR (SELECT count(*) FROM jobs r \
         WHERE r.workspace_id = j.workspace_id AND r.state = 'running') < $4) \
         ORDER BY j.run_at \
         LIMIT 1 \
         FOR UPDATE SKIP LOCKED) c \
         LIMIT 1) \
         RETURNING id, workspace_id, kind, payload, attempts, max_attempts, lease_token, \
         unique_key, trace_id, cause_kind, cause_id, \
         EXTRACT(EPOCH FROM now() - run_at)::float8 AS claim_lag_seconds";

    /// What a production worker claims: every kind in its registry.
    const WORKER_KINDS: &[&str] = &[
        "audit.retention",
        "decisions.retention",
        "delivery.send",
        "delivery.sync",
        "destination.erase",
        "event.forward",
        "events.retention",
        "flow.enroll",
        "flow.lifecycle",
        "flow.settle",
        "flow.step",
        "import.dry_run",
        "import.run",
        "ingest.drain",
        "ingest.replay",
        "jobs.retention",
        "kafka.consume",
        "maintenance.ai_transcripts",
        "maintenance.noop",
        "messages.retention",
        "person.merge.rewrite",
        "privacy.erase",
        "privacy.export",
        "segment.reevaluate",
        "segment.refresh",
        "sending_domain.verify",
        "summary.rollup",
        "sync.run",
        "template.test_send",
        "traits.recompute",
        "traits.slot_backfill",
        "traits.slot_clear",
        "webhook.call",
    ];

    /// Stops autovacuum so the statistics change only where a test says so.
    async fn freeze_statistics(pool: &PgPool) {
        sqlx::query("ALTER TABLE jobs SET (autovacuum_enabled = false)")
            .execute(pool)
            .await
            .unwrap();
    }

    async fn vacuum_and_analyze(pool: &PgPool) {
        sqlx::query("VACUUM (PARALLEL 0) jobs")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("ANALYZE jobs").execute(pool).await.unwrap();
    }

    /// Inserts `count` waiting `flow.step` wakes of `workspace`, due `from` to `to` seconds from
    /// now (negative is overdue), each with its wake key.
    async fn seed_wakes(
        pool: &PgPool,
        workspace: WorkspaceId,
        tag: &str,
        count: i64,
        from: f64,
        to: f64,
    ) {
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, priority, run_at, unique_key, \
                trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, 'flow.step', \
                    jsonb_build_object('enrollment_id', gen_random_uuid(), 'state_version', 1), 0, \
                    now() + make_interval(secs => $3 + ($4 - $3) * (g::float8 / $5)), \
                    'wake:' || $2 || ':' || g, gen_random_uuid(), 'event', 'bench' \
             FROM generate_series(1, $5) g ORDER BY random()",
        )
        .bind(workspace.as_uuid())
        .bind(tag)
        .bind(from)
        .bind(to)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Work of other kinds beside the timers.
    #[derive(Clone, Copy)]
    enum OtherWork {
        /// Refreshes due later, and maintenance at an earlier priority but not due for an hour.
        Later,
        /// Ingest drains due now, and a few running jobs.
        Now,
        /// Enrolment work due now, and webhook calls due at a later priority.
        Busy,
        /// The webhook calls alone.
        LaterPriority,
    }

    async fn seed_other_work(pool: &PgPool, work: OtherWork) {
        let rows: &[(&str, i32, i16, f64, &str)] = match work {
            OtherWork::Later => &[
                ("segment.refresh", 500, 0, 600.0, "ready"),
                ("maintenance.noop", 200, -1, 3_600.0, "ready"),
            ],
            OtherWork::Now => &[
                ("ingest.drain", 8, 0, -5.0, "ready"),
                ("delivery.send", 4, 0, -60.0, "running"),
            ],
            OtherWork::Busy => &[
                ("flow.enroll", 2_000, 0, -30.0, "ready"),
                ("webhook.call", 1_000, 5, -30.0, "ready"),
            ],
            OtherWork::LaterPriority => &[("webhook.call", 1_000, 5, -30.0, "ready")],
        };
        for (kind, count, priority, from, state) in rows {
            sqlx::query(
                "INSERT INTO jobs (id, kind, payload, priority, run_at, state, trace_id, \
                    cause_kind, cause_id) \
                 SELECT gen_random_uuid(), $1, '{}', $3, \
                        now() + make_interval(secs => $4 + g * 0.01), $5, gen_random_uuid(), \
                        'api', 'x' \
                 FROM generate_series(1, $2) g",
            )
            .bind(kind)
            .bind(count)
            .bind(priority)
            .bind(from)
            .bind(state)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    /// One claim by the baseline statement, run as [`claim_job`] ran it, or by [`claim_job`];
    /// the claimed id and token.
    async fn claim_by(pool: &PgPool, before: bool, cap: Option<i64>) -> Option<(Uuid, Uuid)> {
        if !before {
            let cap = cap.map(|cap| u32::try_from(cap).unwrap());
            return claim_job(pool, "bench", WORKER_KINDS, LEASE, cap)
                .await
                .unwrap()
                .map(|claimed| (claimed.id, claimed.lease_token));
        }
        let query = sqlx::query(if cap.is_some() {
            CLAIM_WITH_PER_ROW_CAP
        } else {
            CLAIM_BEFORE_207
        })
        .bind("bench")
        .bind(LEASE.as_secs_f64())
        .bind(WORKER_KINDS)
        .bind(cap);
        let row: Option<sqlx::postgres::PgRow> = if cap.is_some() {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(CLAIM_LOCK_KEY)
                .execute(&mut *tx)
                .await
                .unwrap();
            let row = query.fetch_optional(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            row
        } else {
            query.fetch_optional(pool).await.unwrap()
        };
        row.map(|row| (row.get("id"), row.get("lease_token")))
    }

    /// Four workers claim and complete for `run_for`: claims and empty claims per second.
    async fn claim_rate(
        pool: &PgPool,
        before: bool,
        cap: Option<i64>,
        run_for: Duration,
    ) -> (f64, f64) {
        let started = std::time::Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let pool = pool.clone();
            tasks.spawn(async move {
                let (mut claimed, mut empty) = (0_u64, 0_u64);
                while started.elapsed() < run_for {
                    match claim_by(&pool, before, cap).await {
                        Some((id, token)) => {
                            complete_job(&pool, id, token).await.unwrap();
                            claimed += 1;
                        }
                        None => empty += 1,
                    }
                }
                (claimed, empty)
            });
        }
        let (mut claimed, mut empty) = (0, 0);
        while let Some(result) = tasks.join_next().await {
            let (done, none) = result.unwrap();
            claimed += done;
            empty += none;
        }
        let elapsed = started.elapsed().as_secs_f64();
        (claimed as f64 / elapsed, empty as f64 / elapsed)
    }

    /// The plan a worker's connection runs: the statement prepared once and run six times,
    /// so the plan cache has made its generic-or-custom choice, then `EXPLAIN (ANALYZE,
    /// BUFFERS)` of the next execution. All inside a transaction that is rolled back. `binds`
    /// are the parameters after `$3`, as [`claim_plans`] takes them.
    async fn cached_claim_plan(pool: &PgPool, sql: &'static str, binds: &[Option<i64>]) -> String {
        let mut tx = pool.begin().await.unwrap();
        for _ in 0..6 {
            let mut query = sqlx::query(sql)
                .bind("bench")
                .bind(LEASE.as_secs_f64())
                .bind(WORKER_KINDS);
            for value in binds {
                query = query.bind(*value);
            }
            query.execute(&mut *tx).await.unwrap();
        }
        let (name, generic, custom): (String, i64, i64) = sqlx::query_as(
            "SELECT name, generic_plans, custom_plans FROM pg_prepared_statements \
             WHERE statement = $1",
        )
        .bind(sql)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        let kinds = WORKER_KINDS
            .iter()
            .map(|kind| format!("'{kind}'"))
            .collect::<Vec<_>>()
            .join(",");
        let rest: String = binds
            .iter()
            .map(|value| value.map_or_else(|| ", NULL".to_owned(), |value| format!(", {value}")))
            .collect();
        let lines: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, BUFFERS) EXECUTE {name}('bench', 30, ARRAY[{kinds}]::text[]{rest})"
        )))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        format!(
            "generic_plans={generic} custom_plans={custom}\n{}",
            lines.join("\n")
        )
    }

    /// Every node of an `EXPLAIN (FORMAT JSON)` plan, `plan` itself included if it is one.
    fn plan_nodes(plan: &Value) -> Vec<&Value> {
        let mut nodes = Vec::new();
        let mut stack = vec![plan];
        while let Some(node) = stack.pop() {
            match node {
                Value::Object(map) => {
                    if map.contains_key("Node Type") {
                        nodes.push(node);
                    }
                    stack.extend(map.values());
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        nodes
    }

    fn plan_text<'a>(node: &'a Value, key: &str) -> &'a str {
        node.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// Whether every read of `jobs` under `node` is a probe of `jobs_ready_workspace_idx` or
    /// `jobs_ready_instance_idx` (checked on its own by [`unbounded_step`]) or of the running
    /// jobs: a sort of such reads orders at most the heads of the workspaces, or the running set.
    fn reads_only_bounded_sets(node: &Value) -> bool {
        plan_nodes(node).into_iter().all(|read| {
            match (
                plan_text(read, "Index Name"),
                plan_text(read, "Relation Name"),
            ) {
                ("", "jobs") => plan_text(read, "Node Type") == "Bitmap Heap Scan",
                ("", _) => true,
                (index, _) => [
                    "jobs_ready_workspace_idx",
                    "jobs_ready_instance_idx",
                    "jobs_workspace_running_idx",
                ]
                .contains(&index),
            }
        })
    }

    /// The step of a claim plan whose cost grows with the queue, if any: a sort of anything but
    /// workspace heads, a sequential scan of `jobs`, the kind index (whose order is not the
    /// claim's), a scan of a band-ordered ready index that does not start at a priority and stop
    /// at the first entry, a scan of `jobs_ready_workspace_idx` not bounded by a workspace, and
    /// (the cap path) a ready index scan that filters on the workspace, which steps over a capped
    /// workspace's jobs row by row, or a running count per row.
    fn unbounded_step(plan: &Value) -> Option<String> {
        for node in plan_nodes(plan) {
            let text = |key: &str| plan_text(node, key);
            let step = text("Node Type");
            if (step.ends_with("Sort") && !reads_only_bounded_sets(node))
                || (step == "Seq Scan" && text("Relation Name") == "jobs")
            {
                return Some(step.to_owned());
            }
            let (bound, filter) = (text("Index Cond"), text("Filter"));
            let first_entry = bound.is_empty() && filter.is_empty();
            match text("Index Name") {
                "jobs_ready_kind_idx" => return Some("jobs_ready_kind_idx".to_owned()),
                "jobs_workspace_running_idx" if bound.contains("workspace_id =") => {
                    return Some(format!("a running count per row on {bound:?}"));
                }
                index @ ("jobs_ready_idx"
                | "jobs_ready_instance_idx"
                | "jobs_ready_workspace_idx") => {
                    if filter.contains("workspace_id") || filter.contains("SubPlan") {
                        return Some(format!("{index} filtered on {filter:?}"));
                    }
                    let bounded = if index == "jobs_ready_workspace_idx" {
                        bound.contains("workspace_id")
                    } else {
                        bound.contains("priority =") || bound.contains("priority >")
                    };
                    if !bounded && !first_entry {
                        return Some(format!("{index} on {bound:?}"));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The parameters a plan check binds after `$3`, one set per custom plan: the cap for the
    /// baselines, and the batch size, then the cap if it takes one, for the current claims.
    type Binds<'a> = &'a [&'a [Option<i64>]];

    /// The custom plan of `sql` for each set of `binds` and its generic plan.
    async fn claim_plans(
        pool: &PgPool,
        sql: &'static str,
        binds: Binds<'_>,
    ) -> Vec<(String, Value)> {
        let mut plans = Vec::new();
        for set in binds {
            let mut query =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
                    .bind("w")
                    .bind(LEASE.as_secs_f64())
                    .bind(WORKER_KINDS);
            for value in *set {
                query = query.bind(*value);
            }
            plans.push((
                format!("custom, binds {set:?}"),
                query.fetch_one(pool).await.unwrap(),
            ));
        }
        // Simple protocol: a prepared EXPLAIN would expect the placeholders bound.
        let generic: Value = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (FORMAT JSON, GENERIC_PLAN) {sql}"
        )))
        .fetch_one(pool)
        .await
        .unwrap()
        .get(0);
        plans.push(("generic".to_owned(), generic));
        plans
    }

    /// Asserts that each statement's plans are bounded, or (`caught`) that the check catches
    /// one of them, as custom plans for each set of binds and as the generic plan.
    async fn check_claim_plans(
        pool: &PgPool,
        stage: &str,
        statements: &[(&str, &'static str, Binds<'_>, bool)],
    ) {
        for (statement, sql, binds, caught) in statements {
            for (mode, plan) in claim_plans(pool, sql, binds).await {
                let step = unbounded_step(&plan);
                if *caught {
                    assert!(
                        step.is_some(),
                        "{statement}, {stage}, {mode}: not caught\n{plan}"
                    );
                } else {
                    assert!(
                        step.is_none(),
                        "{statement}, {stage}, {mode}: {step:?}\n{plan}"
                    );
                }
            }
        }
    }

    /// #207: the claim reads a bounded range per priority band whatever the statistics: none
    /// yet, taken while nothing was due (the old claim then sorted every due job), or fresh; as
    /// the custom plan for one job and for a batch (#397), with and without a cap, and as the
    /// generic plan a worker's prepared statement can settle on. The statement before #207 is
    /// checked the same way, so the check is known to catch it.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_claim_plan_stays_bounded_whatever_the_statistics(pool: PgPool) {
        freeze_statistics(&pool).await;
        let ws = workspace(&pool, "ours").await;
        seed_wakes(&pool, ws, "f", 20_000, 600.0, 2_400.0).await;
        seed_other_work(&pool, OtherWork::Later).await;
        let statements: &[(&str, &'static str, Binds<'_>, bool)] = &[
            ("current", CLAIM, &[&[Some(1)], &[Some(8)]], false),
            (
                "capped",
                CLAIM_CAPPED,
                &[
                    &[Some(1), Some(2)],
                    &[Some(8), Some(2)],
                    &[Some(8), Some(20)],
                ],
                false,
            ),
            (
                "before #207",
                CLAIM_BEFORE_207,
                &[&[None], &[Some(2)]],
                true,
            ),
        ];

        seed_wakes(&pool, ws, "b0", 5_000, -60.0, 0.0).await;
        seed_other_work(&pool, OtherWork::Busy).await;
        check_claim_plans(&pool, "no statistics", statements).await;

        sqlx::query("DELETE FROM jobs WHERE run_at <= now()")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        seed_wakes(&pool, ws, "b1", 5_000, -60.0, 0.0).await;
        seed_other_work(&pool, OtherWork::Busy).await;
        check_claim_plans(&pool, "statistics taken while nothing was due", statements).await;

        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        check_claim_plans(&pool, "fresh statistics", statements).await;
    }

    /// `count` running jobs of `workspace`, as other workers' claims leave them.
    async fn seed_running(pool: &PgPool, workspace: WorkspaceId, count: i64) {
        sqlx::query(
            "INSERT INTO jobs (id, workspace_id, kind, payload, state, trace_id, cause_kind, \
                cause_id) \
             SELECT gen_random_uuid(), $1, 'delivery.send', '{}', 'running', gen_random_uuid(), \
                    'api', 'x' \
             FROM generate_series(1, $2)",
        )
        .bind(workspace.as_uuid())
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// #207, the cap path: a workspace at its cap with a flood of due jobs in the first band,
    /// other workspaces' due jobs behind it and instance work beside it. The capped claim reads
    /// no ready index range filtered on the workspace (which steps over the flood row by row)
    /// and counts no running jobs per row, whatever the statistics, as the custom plan for one
    /// job and for a batch larger than the cap (#397), and as the generic plan. The claim before
    /// its skip scan is checked the same way and caught.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_capped_claim_plan_does_not_step_over_a_capped_workspace(pool: PgPool) {
        freeze_statistics(&pool).await;
        let flood = workspace(&pool, "flood").await;
        let others = [
            workspace(&pool, "quiet").await,
            workspace(&pool, "calm").await,
        ];
        seed_running(&pool, flood, 2).await;
        seed_wakes(&pool, flood, "f", 20_000, 600.0, 2_400.0).await;
        seed_other_work(&pool, OtherWork::Later).await;
        let burst = |tag: &'static str| {
            let pool = pool.clone();
            async move {
                seed_wakes(&pool, flood, tag, 5_000, -120.0, -60.0).await;
                for (n, other) in others.into_iter().enumerate() {
                    seed_wakes(&pool, other, &format!("{tag}-{n}"), 10, -60.0, 0.0).await;
                }
                seed_other_work(&pool, OtherWork::Busy).await;
            }
        };
        let statements: &[(&str, &'static str, Binds<'_>, bool)] = &[
            (
                "capped",
                CLAIM_CAPPED,
                &[&[Some(1), Some(2)], &[Some(8), Some(2)]],
                false,
            ),
            ("per-row cap", CLAIM_WITH_PER_ROW_CAP, &[&[Some(2)]], true),
        ];

        burst("b0").await;
        check_claim_plans(&pool, "no statistics", statements).await;

        sqlx::query("DELETE FROM jobs WHERE state = 'ready' AND run_at <= now()")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        burst("b1").await;
        check_claim_plans(&pool, "statistics taken while nothing was due", statements).await;

        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        check_claim_plans(&pool, "fresh statistics", statements).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_claim_skips_locked_jobs_within_a_band_and_falls_through_to_the_next(pool: PgPool) {
        for (name, priority, age) in [("first", 0_i16, 20), ("second", 0, 10), ("later", 3, 30)] {
            let mut new = job("a.kind");
            new.priority = priority;
            new.payload = json!({ "n": name });
            new.run_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(age));
            enqueue(&pool, &new).await.unwrap();
        }
        // Another claimer holds the oldest job of the first band.
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM jobs WHERE payload ->> 'n' = 'first' FOR UPDATE")
            .fetch_one(&mut *holder)
            .await
            .unwrap();

        let second = claim(&pool, &["a.kind"]).await.unwrap();
        let later = claim(&pool, &["a.kind"]).await.unwrap();
        holder.rollback().await.unwrap();
        let first = claim(&pool, &["a.kind"]).await.unwrap();

        assert_eq!(second.payload, json!({"n": "second"}));
        assert_eq!(later.payload, json!({"n": "later"}));
        assert_eq!(first.payload, json!({"n": "first"}));
    }

    /// Enqueues a due `flow.step` job named `name` of `workspace` (none: an instance job).
    async fn enqueue_named(
        pool: &PgPool,
        name: &str,
        workspace: Option<WorkspaceId>,
        priority: i16,
        age_seconds: i64,
    ) {
        let mut new = job("flow.step");
        new.workspace_id = workspace;
        new.priority = priority;
        new.payload = json!({ "n": name });
        new.run_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(age_seconds));
        enqueue(pool, &new).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_workspace_flood_does_not_hide_the_jobs_behind_it(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        let quiet = workspace(&pool, "quiet").await;
        let third = workspace(&pool, "third").await;
        seed_running(&pool, busy, 1).await;
        // Busy's due jobs are all older than quiet's, in the same band.
        seed_wakes(&pool, busy, "busy", 500, -180.0, -60.0).await;
        enqueue_named(&pool, "quiet", Some(quiet), 0, 5).await;
        // Older than everything, but at a later priority.
        enqueue_named(&pool, "third", Some(third), 1, 600).await;
        enqueue_named(&pool, "instance", None, 1, 1).await;
        let claim_capped = || claim_job(&pool, "w", &["flow.step"], LEASE, Some(1));

        let quiet_job = claim_capped().await.unwrap().unwrap();
        let third_job = claim_capped().await.unwrap().unwrap();
        // Every workspace is at its cap now; instance jobs are never capped.
        let instance_job = claim_capped().await.unwrap().unwrap();
        let none = claim_capped().await.unwrap();

        assert_eq!(quiet_job.payload, json!({"n": "quiet"}));
        assert_eq!(third_job.payload, json!({"n": "third"}));
        assert_eq!(instance_job.payload, json!({"n": "instance"}));
        assert!(none.is_none());
    }

    /// A locked offer is passed over within its band, but under a cap a claim never falls
    /// through to a later band while the best band offered is held (ruling a894b5c2).
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_claim_skips_a_locked_job_but_waits_for_its_band(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        seed_running(&pool, busy, 1).await;
        seed_wakes(&pool, busy, "busy", 50, -180.0, -60.0).await;
        for (name, priority, age) in [("first", 0_i16, 30), ("second", 0, 10), ("later", 3, 40)] {
            let ws = workspace(&pool, name).await;
            enqueue_named(&pool, name, Some(ws), priority, age).await;
        }
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM jobs WHERE payload ->> 'n' = 'first' FOR UPDATE")
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        let claim_capped = || claim_job(&pool, "w", &["flow.step"], LEASE, Some(1));

        let second = claim_capped().await.unwrap().unwrap();
        let held = claim_capped().await.unwrap();
        holder.rollback().await.unwrap();
        let first = claim_capped().await.unwrap().unwrap();
        let later = claim_capped().await.unwrap().unwrap();

        assert_eq!(second.payload, json!({"n": "second"}));
        assert!(held.is_none(), "the best band's only offer is held");
        assert_eq!(first.payload, json!({"n": "first"}));
        assert_eq!(later.payload, json!({"n": "later"}));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_claim_locks_only_the_jobs_it_claims(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        seed_running(&pool, busy, 1).await;
        seed_wakes(&pool, busy, "busy", 20, -180.0, -60.0).await;
        for n in 0..5 {
            let name = format!("other{n}");
            let ws = workspace(&pool, &name).await;
            enqueue_named(&pool, &name, Some(ws), 0, 10 - n).await;
        }
        let lockable = || async {
            let mut probe = pool.begin().await.unwrap();
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM (SELECT id FROM jobs WHERE state = 'ready' \
                 FOR UPDATE SKIP LOCKED) l",
            )
            .fetch_one(&mut *probe)
            .await
            .unwrap();
            probe.rollback().await.unwrap();
            count
        };
        assert_eq!(lockable().await, 25);

        for (n, names) in [
            (1_i64, &["other0"][..]),
            (3, &["other0", "other1", "other2"]),
        ] {
            let mut claimer = pool.begin().await.unwrap();
            let mut claimed: Vec<ClaimedRow> = sqlx::query_as(CLAIM_CAPPED)
                .bind("w")
                .bind(LEASE.as_secs_f64())
                .bind(&["flow.step"][..])
                .bind(n)
                .bind(1_i64)
                .fetch_all(&mut *claimer)
                .await
                .unwrap();
            claimed.sort_by_key(|row| row.run_at);

            // The five workspace heads were read, but only the claimed ones are locked.
            let payloads: Vec<&Value> = claimed.iter().map(|row| &row.payload).collect();
            let wanted: Vec<Value> = names.iter().map(|name| json!({ "n": name })).collect();
            assert_eq!(
                payloads,
                wanted.iter().collect::<Vec<_>>(),
                "a batch of {n}"
            );
            assert_eq!(lockable().await, 25 - n, "a batch of {n}");
            claimer.rollback().await.unwrap();
        }
    }

    fn names(jobs: &[ClaimedJob]) -> Vec<&str> {
        jobs.iter()
            .map(|job| job.payload["n"].as_str().unwrap_or(""))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_claim_takes_the_oldest_due_jobs_band_by_band_with_a_lease_each(pool: PgPool) {
        for (name, kind, priority, age) in [
            ("p0-new", "a.kind", 0_i16, 10_i64),
            ("p0-old", "a.kind", 0, 90),
            ("p1-old", "a.kind", 1, 300),
            ("p1-new", "a.kind", 1, 5),
            ("p-1", "a.kind", -1, 1),
            ("other", "b.kind", -5, 600),
            ("p2", "a.kind", 2, 50),
        ] {
            let mut new = job(kind);
            new.priority = priority;
            new.payload = json!({ "n": name });
            new.run_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(age));
            enqueue(&pool, &new).await.unwrap();
        }
        let mut later = job("a.kind");
        later.priority = -9;
        later.run_at = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
        enqueue(&pool, &later).await.unwrap();
        let batch = || claim_jobs(&pool, "w", &["a.kind"], LEASE, None, 4);

        let first = batch().await.unwrap();
        let second = batch().await.unwrap();

        assert_eq!(names(&first), ["p-1", "p0-old", "p0-new", "p1-old"]);
        assert_eq!(names(&second), ["p1-new", "p2"]);
        assert!(batch().await.unwrap().is_empty());
        let mut tokens: Vec<Uuid> = first
            .iter()
            .chain(&second)
            .map(|job| job.lease_token)
            .collect();
        tokens.sort();
        tokens.dedup();
        assert_eq!(tokens.len(), 6, "every job has a lease token of its own");
        for claimed in first.iter().chain(&second) {
            let stored = find_job(&pool, claimed.id).await.unwrap().unwrap();
            assert_eq!(stored.state, "running");
            assert_eq!(stored.lease_token, Some(claimed.lease_token));
            assert_eq!((stored.attempts, claimed.attempts), (1, 1));
            assert!(stored.lease_expires_at.is_some());
        }
        // Each job's lag is that of its own `run_at`.
        assert!((89.0..600.0).contains(&first[1].claim_lag_seconds));
        assert!((0.0..30.0).contains(&first[0].claim_lag_seconds));
    }

    /// A slot's claim (ruling 48be9592): thirty-two jobs, the oldest due, in age order, out of
    /// due jobs enqueued in shuffled order beside future ones and another kind.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_claim_of_thirty_two_takes_the_thirty_two_oldest_due_jobs_in_order(pool: PgPool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, run_at, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), CASE WHEN g % 10 = 0 THEN 'b.kind' ELSE 'a.kind' END, \
                    jsonb_build_object('n', g), now() - make_interval(secs => 100.5 - g), \
                    gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, 150) g ORDER BY random()",
        )
        .execute(&pool)
        .await
        .unwrap();
        // The jobs after the hundredth are not due yet; the lower the number, the older the job.
        let mut wanted: Vec<i64> = (1..=100).filter(|n| n % 10 != 0).collect();
        wanted.truncate(32);

        let claimed = claim_jobs(&pool, "w", &["a.kind"], LEASE, None, 32)
            .await
            .unwrap();

        let got: Vec<i64> = claimed
            .iter()
            .map(|job| job.payload["n"].as_i64().unwrap())
            .collect();
        assert_eq!(got, wanted);
    }

    /// `count` due `a.kind` jobs, a millisecond apart, stored in an order unrelated to their age, as
    /// timers set at random times are, so neighbours in `jobs_ready_idx` sit on different pages.
    async fn seed_due(pool: &PgPool, count: i64) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, run_at, trace_id, cause_kind, cause_id) \
             SELECT gen_random_uuid(), 'a.kind', '{}', \
                    now() - interval '1 hour' + make_interval(secs => g * 0.001), \
                    gen_random_uuid(), 'api', 'x' \
             FROM generate_series(1, $1) g ORDER BY random()",
        )
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Deletes the `count` oldest ready jobs, as claims and completions of them do: their entries
    /// stay at the head of `jobs_ready_idx` until nothing can still see the rows.
    async fn finish_oldest(pool: &PgPool, count: i64) {
        sqlx::query(
            "DELETE FROM jobs WHERE id IN (SELECT id FROM jobs WHERE state = 'ready' \
             ORDER BY priority, run_at LIMIT $1)",
        )
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    /// A transaction on a connection of its own holding a `REPEATABLE READ` snapshot, as a long
    /// report does, or a long transaction that holds a transaction id: a row deleted after it
    /// began cannot be cleaned up, from the heap or from an index, until it ends.
    async fn hold_snapshot(pool: &PgPool) -> sqlx::Transaction<'static, sqlx::Postgres> {
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("SELECT 1").execute(&mut *tx).await.unwrap();
        tx
    }

    /// The shared buffers one claim of up to `n` `a.kind` jobs touches, from `EXPLAIN (ANALYZE,
    /// BUFFERS)` of [`CLAIM`] in a transaction that is rolled back, so the jobs stay ready. The
    /// claim must have taken `n`.
    async fn claim_buffers(pool: &PgPool, n: i64) -> i64 {
        let mut tx = pool.begin().await.unwrap();
        let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {CLAIM}"
        )))
        .bind("w")
        .bind(LEASE.as_secs_f64())
        .bind(&["a.kind"][..])
        .bind(n)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        let top = &plan[0]["Plan"];
        assert_eq!(top["Actual Rows"].as_f64(), Some(n as f64), "{plan}");
        top["Shared Hit Blocks"].as_i64().unwrap() + top["Shared Read Blocks"].as_i64().unwrap()
    }

    /// Ruling 48be9592: most of a claim's cost is its walk over the dead entries at the head of
    /// `jobs_ready_idx`, which it pays whatever it takes, so a claim of 32 costs about what a
    /// claim of 8 does. The entries are 2,000 finished jobs a held snapshot keeps.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_claim_of_thirty_two_reads_about_the_buffers_of_a_claim_of_eight(pool: PgPool) {
        freeze_statistics(&pool).await;
        seed_due(&pool, 3_000).await;
        let snapshot = hold_snapshot(&pool).await;
        finish_oldest(&pool, 2_000).await;

        let eight = claim_buffers(&pool, 8).await;
        let thirty_two = claim_buffers(&pool, 32).await;
        snapshot.commit().await.unwrap();

        assert!(
            eight > 2_000,
            "the walk over the dead head: {eight} buffers"
        );
        assert!(
            thirty_two * 4 < eight * 5,
            "a claim of 32 read {thirty_two} buffers, one of 8 read {eight}"
        );
    }

    /// The xmin horizon (investigation G, candidate 2): while a snapshot older than the finished
    /// jobs is held, every claim walks their dead entries, row by row through the heap; once it
    /// ends, the first claim marks them dead in the index and the next skips them.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn claims_behind_a_held_snapshot_walk_the_finished_jobs_and_stop_once_it_ends(
        pool: PgPool,
    ) {
        freeze_statistics(&pool).await;
        seed_due(&pool, 6_000).await;
        let snapshot = hold_snapshot(&pool).await;
        finish_oldest(&pool, 5_000).await;

        let held = claim_buffers(&pool, 8).await;
        let again = claim_buffers(&pool, 8).await;
        snapshot.commit().await.unwrap();
        claim_buffers(&pool, 8).await;
        let released = claim_buffers(&pool, 8).await;

        assert!(held > 5_000, "{held} buffers behind the snapshot");
        assert!(
            again * 10 > held * 9,
            "a held snapshot keeps the walk: {held} then {again} buffers"
        );
        assert!(
            released * 10 < held,
            "{held} buffers behind the snapshot, {released} once it ended"
        );
    }

    /// Two claimers, each on a connection of its own, drain 400 due jobs of two workspaces and
    /// the instance in batches of eight, completing each batch before claiming the next: no job is
    /// claimed twice and every lease token is distinct, without a cap and under a cap of three,
    /// where no workspace ever runs more than three.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_batch_claims_never_take_a_job_twice(pool: PgPool) {
        let workspaces = vec![
            Some(workspace(&pool, "one").await.as_uuid()),
            Some(workspace(&pool, "two").await.as_uuid()),
            None,
        ];
        for cap in [None, Some(3_u32)] {
            sqlx::query(
                "INSERT INTO jobs (id, workspace_id, kind, payload, priority, run_at, trace_id, \
                    cause_kind, cause_id) \
                 SELECT gen_random_uuid(), ($1::uuid[])[1 + g % 3], 'a.kind', '{}', \
                        (g % 2)::int2, now() - make_interval(secs => g), gen_random_uuid(), \
                        'api', 'x' \
                 FROM generate_series(1, 400) g",
            )
            .bind(&workspaces)
            .execute(&pool)
            .await
            .unwrap();
            let start = Arc::new(tokio::sync::Barrier::new(2));
            let mut tasks = tokio::task::JoinSet::new();
            for claimer in 0..2 {
                let own = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect_with((*pool.connect_options()).clone())
                    .await
                    .unwrap();
                let start = start.clone();
                tasks.spawn(async move {
                    let name = format!("claimer-{claimer}");
                    let mut claimed = Vec::new();
                    start.wait().await;
                    loop {
                        let batch = claim_jobs(&own, &name, &["a.kind"], LEASE, cap, 8)
                            .await
                            .unwrap();
                        if batch.is_empty() {
                            return claimed;
                        }
                        if let Some(cap) = cap {
                            let most: Option<i64> = sqlx::query_scalar(
                                "SELECT max(n) FROM (SELECT count(*) AS n FROM jobs \
                                 WHERE state = 'running' AND workspace_id IS NOT NULL \
                                 GROUP BY workspace_id) r",
                            )
                            .fetch_one(&own)
                            .await
                            .unwrap();
                            assert!(most.unwrap_or(0) <= i64::from(cap), "{most:?} running");
                        }
                        for job in batch {
                            assert!(complete_job(&own, job.id, job.lease_token).await.unwrap());
                            claimed.push((job.id, job.lease_token));
                        }
                    }
                });
            }
            let mut claimed = Vec::new();
            while let Some(result) = tasks.join_next().await {
                claimed.extend(result.unwrap());
            }

            let mut ids: Vec<Uuid> = claimed.iter().map(|(id, _)| *id).collect();
            ids.sort();
            ids.dedup();
            let mut tokens: Vec<Uuid> = claimed.iter().map(|(_, token)| *token).collect();
            tokens.sort();
            tokens.dedup();
            assert_eq!(
                (claimed.len(), ids.len(), tokens.len()),
                (400, 400, 400),
                "cap {cap:?}"
            );
            assert_eq!(queue_counts(&pool).await.unwrap(), QueueCounts::default());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_batch_takes_no_workspace_past_its_cap(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        let fresh = workspace(&pool, "fresh").await;
        let quiet = workspace(&pool, "quiet").await;
        seed_running(&pool, busy, 1).await;
        seed_wakes(&pool, busy, "busy", 10, -180.0, -60.0).await;
        // No running jobs, but a cap below the batch size.
        seed_wakes(&pool, fresh, "fresh", 10, -120.0, -60.0).await;
        enqueue_named(&pool, "quiet", Some(quiet), 0, 5).await;
        enqueue_named(&pool, "instance-a", None, 0, 30).await;
        enqueue_named(&pool, "instance-b", None, 0, 20).await;
        let batch = || claim_jobs(&pool, "w", &["flow.step"], LEASE, Some(3), 8);

        let first = batch().await.unwrap();
        let second = batch().await.unwrap();

        let taken = |workspace: Option<WorkspaceId>| {
            let id = workspace.map(|ws| ws.as_uuid());
            first.iter().filter(|job| job.workspace_id == id).count()
        };
        assert_eq!(
            [
                taken(Some(busy)),
                taken(Some(fresh)),
                taken(Some(quiet)),
                taken(None)
            ],
            [2, 3, 1, 2]
        );
        assert!(second.is_empty(), "every workspace is at its cap");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_workspace_flood_does_not_hide_the_jobs_behind_it_from_a_batch(pool: PgPool) {
        let busy = workspace(&pool, "busy").await;
        let quiet = workspace(&pool, "quiet").await;
        let third = workspace(&pool, "third").await;
        seed_running(&pool, busy, 1).await;
        // Busy's due jobs are all older than quiet's, in the same band.
        seed_wakes(&pool, busy, "busy", 500, -180.0, -60.0).await;
        enqueue_named(&pool, "quiet", Some(quiet), 0, 5).await;
        // Older than everything, but at a later priority.
        enqueue_named(&pool, "third", Some(third), 1, 600).await;
        enqueue_named(&pool, "instance", None, 1, 1).await;
        let batch = || claim_jobs(&pool, "w", &["flow.step"], LEASE, Some(1), 8);

        let first = batch().await.unwrap();
        let second = batch().await.unwrap();
        let last = batch().await.unwrap();

        // Each batch takes one band only.
        assert_eq!(names(&first), ["quiet"]);
        assert_eq!(names(&second), ["third", "instance"]);
        assert!(last.is_empty(), "every workspace is at its cap");
    }

    /// Ruling a894b5c2: under a cap a batch takes only the best band offered, so a workspace's
    /// earlier band never waits behind another workspace's later band.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_capped_batch_takes_only_the_best_band_offered(pool: PgPool) {
        let a = workspace(&pool, "a").await;
        let b = workspace(&pool, "b").await;
        enqueue_named(&pool, "a-p0", Some(a), 0, 10).await;
        for n in 0..7 {
            enqueue_named(&pool, "a-p1", Some(a), 1, 20 + n).await;
            enqueue_named(&pool, "b-p2", Some(b), 2, 30 + n).await;
        }
        // A cap below the batch size takes the capped path.
        let batch = || claim_jobs(&pool, "w", &["flow.step"], LEASE, Some(7), 8);

        let first = batch().await.unwrap();
        let second = batch().await.unwrap();

        assert_eq!(names(&first), ["a-p0"]);
        // A runs one job, so its cap has room for six more.
        assert_eq!(names(&second), ["a-p1"; 6]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn batch_claimed_jobs_a_crash_left_run_again_once_their_leases_expire(pool: PgPool) {
        for _ in 0..3 {
            enqueue(&pool, &job("a.kind")).await.unwrap();
        }
        let crashed = claim_jobs(&pool, "w", &["a.kind"], Duration::from_millis(300), None, 8)
            .await
            .unwrap();
        assert_eq!(crashed.len(), 3);
        for job in &crashed {
            wait_until_expired(&pool, job.id).await;
        }

        let reaped = reap_expired(&pool).await.unwrap();
        let again = claim_jobs(&pool, "w", &["a.kind"], LEASE, None, 8)
            .await
            .unwrap();

        assert_eq!(reaped.requeued, 3);
        let mut before: Vec<Uuid> = crashed.iter().map(|job| job.id).collect();
        let mut after: Vec<Uuid> = again.iter().map(|job| job.id).collect();
        before.sort();
        after.sort();
        assert_eq!(before, after);
        assert!(again.iter().all(|job| job.attempts == 2));
        for job in &crashed {
            assert!(!complete_job(&pool, job.id, job.lease_token).await.unwrap());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_released_job_goes_back_as_it_was_and_a_marked_or_lost_one_does_not(pool: PgPool) {
        for (n, age) in [(0, 30_i64), (1, 20), (2, 10)] {
            let mut new = job("a.kind");
            new.payload = json!({ "n": n });
            new.run_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(age));
            enqueue(&pool, &new).await.unwrap();
        }
        let held = claim_jobs(&pool, "w", &["a.kind"], LEASE, None, 8)
            .await
            .unwrap();
        let [kept, marked, lost] = [&held[0], &held[1], &held[2]];
        let run_at = find_job(&pool, kept.id).await.unwrap().unwrap().run_at;
        sqlx::query("UPDATE jobs SET purge_on_release = true WHERE id = $1")
            .bind(marked.id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(lost.id)
            .execute(&pool)
            .await
            .unwrap();
        let mut listener = listen_for_jobs(&pool).await.unwrap();

        let tokens: Vec<(Uuid, Uuid)> = held.iter().map(|job| (job.id, job.lease_token)).collect();
        let released = release_jobs(&pool, &tokens).await.unwrap();

        assert_eq!(released, 1);
        let stored = find_job(&pool, kept.id).await.unwrap().unwrap();
        assert_eq!(stored.state, "ready");
        assert_eq!(stored.attempts, 0, "the attempt never started");
        assert_eq!(stored.run_at, run_at);
        assert!(stored.lease_token.is_none() && stored.lease_expires_at.is_none());
        assert!(stored.locked_by.is_none());
        assert!(find_job(&pool, marked.id).await.unwrap().is_none());
        let other = find_job(&pool, lost.id).await.unwrap().unwrap();
        assert_eq!(other.state, "running");
        assert_ne!(other.lease_token, Some(lost.lease_token));
        tokio::time::timeout(Duration::from_secs(5), listener.wait())
            .await
            .expect("a release wakes the workers")
            .unwrap();
        let again = claim_jobs(&pool, "w", &["a.kind"], LEASE, None, 8)
            .await
            .unwrap();
        assert_eq!(
            again.iter().map(|job| job.id).collect::<Vec<_>>(),
            [kept.id]
        );
        assert_eq!(again[0].attempts, 1);
    }

    /// Reproduces #207 at small scale: a million waiting timers and a burst of due ones, with
    /// other work beside them. The claim before #207 and the current one run on the same rows,
    /// with no statistics, with statistics taken while only a little other work was due, with
    /// statistics taken while nothing was due, with fresh ones, and then once the burst is gone,
    /// with only later-priority work due and with nothing due. Prints the plan each worker
    /// connection settles on and the claim rate of four workers; asserts nothing.
    /// `ZIRV_FLOW_BENCH_CLAIM_FUTURE` and `ZIRV_FLOW_BENCH_CLAIM_DUE` set the sizes; at 100,000
    /// waiting the old claim sorts once the statistics put nothing due.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_CLAIM to measure"]
    async fn the_claim_cost_against_a_timer_backlog(pool: PgPool) {
        if std::env::var_os("ZIRV_FLOW_BENCH_CLAIM").is_none() {
            eprintln!("ZIRV_FLOW_BENCH_CLAIM is not set: skipping the claim benchmark");
            return;
        }
        let setting = |name: &str, default: i64| {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default)
        };
        let future = setting("ZIRV_FLOW_BENCH_CLAIM_FUTURE", 1_000_000);
        let due = setting("ZIRV_FLOW_BENCH_CLAIM_DUE", 100_000);
        let run_for = Duration::from_secs(5);
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        eprintln!("BENCH {version}; {future} future wakes, {due} due");
        freeze_statistics(&pool).await;
        let ws = workspace(&pool, "bench").await;
        seed_wakes(&pool, ws, "f", future, 600.0, 2_400.0).await;
        seed_other_work(&pool, OtherWork::Later).await;
        seed_other_work(&pool, OtherWork::Now).await;
        let measure = |label: &'static str, before: bool| {
            let pool = pool.clone();
            async move {
                let due_now: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM jobs WHERE state = 'ready' AND run_at <= now()",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                let statement = if before { "before" } else { "after" };
                let sql = if before { CLAIM_BEFORE_207 } else { CLAIM };
                // The baseline's `$4` is the cap; the claim's is the batch size.
                let binds: &[Option<i64>] = if before { &[None] } else { &[Some(1)] };
                let plan = cached_claim_plan(&pool, sql, binds).await;
                eprintln!("PLAN {label} {statement} due={due_now}\n{plan}");
                let (claims, empty) = claim_rate(&pool, before, None, run_for).await;
                eprintln!(
                    "RATE {label} {statement} due={due_now}: {claims:.0} claims/s, \
                     {empty:.0} empty claims/s"
                );
            }
        };
        let report = |label: &'static str| async move {
            measure(label, true).await;
            measure(label, false).await;
        };
        let burst = |tag: &'static str| {
            let pool = pool.clone();
            async move {
                seed_wakes(&pool, ws, tag, due, -60.0, 0.0).await;
                seed_other_work(&pool, OtherWork::Busy).await;
            }
        };
        // The burst and the busy work leave (with `idle`, everything due goes), the table is
        // vacuumed and analysed, and what left comes back.
        let restage = |idle: bool| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "DELETE FROM jobs WHERE unique_key LIKE 'wake:b%' \
                     OR kind IN ('flow.enroll', 'webhook.call') OR ($1 AND run_at <= now())",
                )
                .bind(idle)
                .execute(&pool)
                .await
                .unwrap();
                vacuum_and_analyze(&pool).await;
                if idle {
                    seed_other_work(&pool, OtherWork::Now).await;
                }
            }
        };

        burst("b0").await;
        report("no-statistics").await;

        restage(false).await;
        burst("b1").await;
        report("stale-statistics-drains-due").await;

        restage(true).await;
        burst("b2").await;
        report("stale-statistics-nothing-due").await;

        sqlx::query("ANALYZE jobs").execute(&pool).await.unwrap();
        report("fresh-statistics").await;

        sqlx::query(
            "DELETE FROM jobs WHERE unique_key LIKE 'wake:b%' \
             OR kind IN ('flow.enroll', 'ingest.drain')",
        )
        .execute(&pool)
        .await
        .unwrap();
        vacuum_and_analyze(&pool).await;
        for before in [true, false] {
            sqlx::query("DELETE FROM jobs WHERE kind = 'webhook.call'")
                .execute(&pool)
                .await
                .unwrap();
            seed_other_work(&pool, OtherWork::LaterPriority).await;
            measure("only-later-priority-due", before).await;
        }
        sqlx::query("DELETE FROM jobs WHERE kind = 'webhook.call'")
            .execute(&pool)
            .await
            .unwrap();
        report("nothing-due").await;
    }

    /// The cap path of #207: a workspace at a cap of 10 running jobs with a flood of due jobs,
    /// older than eight other workspaces' 20,000 due jobs in the same band, with fresh
    /// statistics. The claim with a per-row cap count and the current one, each run as
    /// [`claim_job`] runs it under a cap, claim from the same rows (the other workspaces' jobs are
    /// seeded again before each run): the plan a worker connection settles on and the rate of
    /// four capped workers, then the same once only the flood is due, so every claim is empty,
    /// and once no workspace is at its cap, so the claim takes its uncapped branch and the rate
    /// is that of the cap's serialised claims. Asserts nothing. `ZIRV_FLOW_BENCH_CLAIM_FLOOD`
    /// sets the flood (default 100,000).
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_CLAIM to measure"]
    async fn the_capped_claim_cost_against_a_workspace_flood(pool: PgPool) {
        const CAP: i64 = 10;
        if std::env::var_os("ZIRV_FLOW_BENCH_CLAIM").is_none() {
            eprintln!("ZIRV_FLOW_BENCH_CLAIM is not set: skipping the claim benchmark");
            return;
        }
        let flood_size: i64 = std::env::var("ZIRV_FLOW_BENCH_CLAIM_FLOOD")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(100_000);
        let run_for = Duration::from_secs(5);
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        eprintln!("BENCH {version}; a flood of {flood_size} due jobs at a cap of {CAP}");
        freeze_statistics(&pool).await;
        let flood = workspace(&pool, "flood").await;
        let mut others = Vec::new();
        for n in 0..8 {
            others.push(workspace(&pool, &format!("other{n}")).await);
        }
        let other_ids: Vec<Uuid> = others.iter().map(|other| other.as_uuid()).collect();
        seed_running(&pool, flood, CAP).await;
        seed_wakes(&pool, flood, "flood", flood_size, -180.0, -120.0).await;
        seed_other_work(&pool, OtherWork::Later).await;

        for scenario in ["others-due", "only-the-flood-due", "none-at-cap"] {
            if scenario == "none-at-cap" {
                sqlx::query("DELETE FROM jobs WHERE workspace_id = $1 AND state = 'running'")
                    .bind(flood.as_uuid())
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            for (round, before) in [true, false].into_iter().enumerate() {
                sqlx::query("DELETE FROM jobs WHERE workspace_id = ANY($1)")
                    .bind(&other_ids)
                    .execute(&pool)
                    .await
                    .unwrap();
                if scenario != "only-the-flood-due" {
                    for (n, other) in others.iter().enumerate() {
                        let tag = format!("{scenario}-{round}-{n}");
                        seed_wakes(&pool, *other, &tag, 2_500, -60.0, 0.0).await;
                    }
                }
                vacuum_and_analyze(&pool).await;
                let due_now: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM jobs WHERE state = 'ready' AND run_at <= now()",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                let statement = if before { "before" } else { "after" };
                let sql = if before {
                    CLAIM_WITH_PER_ROW_CAP
                } else {
                    CLAIM_CAPPED
                };
                let binds: &[Option<i64>] = if before {
                    &[Some(CAP)]
                } else {
                    &[Some(1), Some(CAP)]
                };
                let plan = cached_claim_plan(&pool, sql, binds).await;
                eprintln!("PLAN {scenario} {statement} flood={flood_size} due={due_now}\n{plan}");
                let (claims, empty) = claim_rate(&pool, before, Some(CAP), run_for).await;
                eprintln!(
                    "RATE {scenario} {statement} flood={flood_size} due={due_now}: \
                     {claims:.1} claims/s, {empty:.1} empty claims/s"
                );
            }
        }
    }
}
