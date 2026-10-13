//! A sync run row, its state machine and the reads the API and the jobs make.

use serde_json::{Value, json};
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use super::job::{RUN_JOB_KIND, run_trace};
use crate::{NewJob, PgPool, StoreError, begin, commit, enqueue, lock_lease};

/// What started a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunTrigger {
    Manual,
    Schedule,
}

impl RunTrigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Schedule => "schedule",
        }
    }
}

/// A sync run: one read of one stream. It holds counters and codes, never a record.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct SyncRun {
    pub id: Uuid,
    pub source_id: Uuid,
    pub stream_id: Uuid,
    pub stream_name: String,
    /// `queued`, `running`, `succeeded`, `failed` or `cancelled`.
    pub state: String,
    /// `manual` or `schedule`.
    pub trigger: String,
    /// The stream's sync mode when the run was queued.
    pub sync_mode: String,
    pub records_read: i64,
    pub records_applied: i64,
    pub records_unchanged: i64,
    pub records_rejected: i64,
    pub records_deleted: i64,
    pub reject_threshold: f64,
    /// Where this run resumes; opaque.
    pub checkpoint: Option<Value>,
    pub error_code: Option<String>,
    /// How many times a worker started the run.
    pub attempt: i32,
    pub trace_id: Uuid,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub finished_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

impl SyncRun {
    /// Whether the run can still read: `queued` or `running`.
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "running")
    }
}

/// The columns of [`SyncRun`], read from the row aliased `$a` of `sync_runs`, with the stream's
/// name read from the alias `s`.
macro_rules! run_columns {
    ($a:literal) => {
        concat!(
            $a,
            ".id, ",
            $a,
            ".source_id, ",
            $a,
            ".stream_id, s.name AS stream_name, ",
            $a,
            ".state, ",
            $a,
            ".trigger, ",
            $a,
            ".sync_mode, ",
            $a,
            ".records_read, ",
            $a,
            ".records_applied, ",
            $a,
            ".records_unchanged, ",
            $a,
            ".records_rejected, ",
            $a,
            ".records_deleted, ",
            $a,
            ".reject_threshold, ",
            $a,
            ".checkpoint, ",
            $a,
            ".error_code, ",
            $a,
            ".attempt, ",
            $a,
            ".trace_id, ",
            $a,
            ".created_by, ",
            $a,
            ".created_at, ",
            $a,
            ".started_at, ",
            $a,
            ".finished_at, ",
            $a,
            ".updated_at"
        )
    };
}

/// What a fan-out queued.
#[derive(Debug, Default, PartialEq)]
pub struct FanOut {
    pub created: Vec<SyncRun>,
    /// Streams left alone because they already have an active run.
    pub skipped_active: usize,
}

/// Queues a run of one stream of a source, with the job that works it, in the caller's
/// transaction. `None` when the stream has an active run already (or there is no such stream).
/// The job carries the run's trace and the `sync_run` cause.
pub async fn queue_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream_id: Uuid,
    trigger: RunTrigger,
    created_by: Option<UserId>,
) -> Result<Option<SyncRun>, StoreError> {
    // The source before the stream, whatever order the foreign-key checks run in: a writer
    // holding the source `FOR UPDATE` that then locks its streams (`update_source`, a cascading
    // delete) must not find a stream key-share held here.
    sqlx::query("SELECT 1 FROM sources WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE")
        .bind(workspace.as_uuid())
        .bind(source_id)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let run: Option<SyncRun> = sqlx::query_as(concat!(
        "WITH r AS ( \
            INSERT INTO sync_runs \
                (id, workspace_id, source_id, stream_id, trigger, sync_mode, trace_id, created_by) \
            SELECT $1, $2, $3, s.id, $5, s.sync_mode, $6, $7 FROM streams s \
            WHERE s.workspace_id = $2 AND s.source_id = $3 AND s.id = $4 \
            ON CONFLICT (stream_id) WHERE state IN ('queued', 'running') DO NOTHING \
            RETURNING *) \
         SELECT ",
        run_columns!("r"),
        " FROM r JOIN streams s ON s.id = r.stream_id"
    ))
    .bind(Uuid::now_v7())
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(stream_id)
    .bind(trigger.as_str())
    .bind(Uuid::new_v4())
    .bind(created_by.map(|user| user.as_uuid()))
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(run) = run else {
        return Ok(None);
    };
    let mut job = NewJob::new(RUN_JOB_KIND, run_trace(&run));
    job.workspace_id = Some(workspace);
    job.payload = json!({
        "source_id": source_id,
        "stream_id": run.stream_id,
        "run_id": run.id,
    });
    // One key per run: a retried fan-out cannot queue a run twice, and the next occurrence of
    // a schedule gets a key of its own.
    job.unique_key = Some(format!("sync:run:{}", run.id));
    enqueue(&mut *connection, &job).await?;
    Ok(Some(run))
}

/// Queues a run for every enabled stream of the source that has a mapping, and counts the ones
/// skipped for having an active run. `None` when the workspace has no such source, or when
/// `only_enabled` is set and the source is disabled.
pub async fn queue_runs_for_source(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    trigger: RunTrigger,
    created_by: Option<UserId>,
    only_enabled: bool,
) -> Result<Option<FanOut>, StoreError> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM sources WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    match enabled {
        None => return Ok(None),
        Some(false) if only_enabled => return Ok(None),
        Some(_) => {}
    }
    let streams: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM streams \
         WHERE workspace_id = $1 AND source_id = $2 AND enabled AND mapping IS NOT NULL \
         ORDER BY name",
    )
    .bind(workspace.as_uuid())
    .bind(source_id)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut fan = FanOut::default();
    for stream in streams {
        match queue_run(
            connection, workspace, source_id, stream, trigger, created_by,
        )
        .await?
        {
            Some(run) => fan.created.push(run),
            None => fan.skipped_active += 1,
        }
    }
    Ok(Some(fan))
}

pub async fn find_run<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    run_id: Uuid,
) -> Result<Option<SyncRun>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        run_columns!("r"),
        " FROM sync_runs r JOIN streams s ON s.id = r.stream_id \
          WHERE r.workspace_id = $1 AND r.source_id = $2 AND r.id = $3"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(run_id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Where a page of runs ends: the `created_at` and id of its last run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunCursor {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

impl RunCursor {
    pub fn of(run: &SyncRun) -> Self {
        Self {
            created_at: run.created_at,
            id: run.id,
        }
    }
}

/// The runs of a source, newest first, at most `limit`, after `before` when given, for one
/// stream when `stream` is. The flag is true when more existed.
pub async fn list_runs<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    source_id: Uuid,
    stream: Option<&str>,
    before: Option<RunCursor>,
    limit: u32,
) -> Result<(Vec<SyncRun>, bool), StoreError> {
    let mut runs: Vec<SyncRun> = sqlx::query_as(concat!(
        "SELECT ",
        run_columns!("r"),
        " FROM sync_runs r JOIN streams s ON s.id = r.stream_id \
          WHERE r.workspace_id = $1 AND r.source_id = $2 \
            AND ($3::text IS NULL OR s.name = $3) \
            AND ($4::timestamptz IS NULL OR (r.created_at, r.id) < ($4, $5)) \
          ORDER BY r.created_at DESC, r.id DESC LIMIT $6"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(stream)
    .bind(before.map(|cursor| cursor.created_at))
    .bind(before.map(|cursor| cursor.id))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let more = runs.len() > limit as usize;
    runs.truncate(limit as usize);
    Ok((runs, more))
}

/// Moves a `queued` or `running` run to `running`, counts the attempt and stamps the first
/// start. `None` when the run is finished, cancelled or gone: the job has nothing to do.
pub async fn start_run<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    run_id: Uuid,
) -> Result<Option<SyncRun>, StoreError> {
    sqlx::query_as(concat!(
        "WITH r AS ( \
            UPDATE sync_runs SET state = 'running', attempt = attempt + 1, \
                started_at = COALESCE(started_at, now()), updated_at = now() \
            WHERE workspace_id = $1 AND id = $2 AND state IN ('queued', 'running') \
            RETURNING *) \
         SELECT ",
        run_columns!("r"),
        " FROM r JOIN streams s ON s.id = r.stream_id"
    ))
    .bind(workspace.as_uuid())
    .bind(run_id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// What cancelling a run did.
#[derive(Debug, PartialEq)]
pub enum CancelOutcome {
    Cancelled(Box<SyncRun>),
    /// The run is finished already.
    NotActive,
    NotFound,
}

/// Cancels an active run. A running job notices at its next batch boundary: the batch in
/// flight is not applied.
pub async fn cancel_run(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    run_id: Uuid,
) -> Result<CancelOutcome, StoreError> {
    let cancelled: Option<SyncRun> = sqlx::query_as(concat!(
        "WITH r AS ( \
            UPDATE sync_runs SET state = 'cancelled', finished_at = now(), updated_at = now() \
            WHERE workspace_id = $1 AND source_id = $2 AND id = $3 \
              AND state IN ('queued', 'running') \
            RETURNING *) \
         SELECT ",
        run_columns!("r"),
        " FROM r JOIN streams s ON s.id = r.stream_id"
    ))
    .bind(workspace.as_uuid())
    .bind(source_id)
    .bind(run_id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if let Some(run) = cancelled {
        return Ok(CancelOutcome::Cancelled(Box::new(run)));
    }
    let exists = find_run(&mut *connection, workspace, source_id, run_id)
        .await?
        .is_some();
    Ok(if exists {
        CancelOutcome::NotActive
    } else {
        CancelOutcome::NotFound
    })
}

/// The job and lease token a write of a worker is fenced on.
#[derive(Clone, Copy, Debug)]
pub struct Lease {
    pub job_id: Uuid,
    pub token: Uuid,
}

/// What an end-of-run write did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunEnd {
    /// The run is over as asked.
    Ended,
    /// The run had ended already (cancelled, say): nothing was written.
    Over,
    /// The job's lease is gone: another worker owns the run now. Nothing was written.
    LeaseLost,
}

/// Runs `end` in a transaction that first locks the job's lease, as a batch does, so a worker
/// that lost its lease cannot end the run.
async fn fenced_end(
    pool: &PgPool,
    lease: Lease,
    end: sqlx::query::Query<'_, sqlx::Postgres, sqlx::postgres::PgArguments>,
) -> Result<RunEnd, StoreError> {
    let mut tx = begin(pool).await?;
    if !lock_lease(&mut tx, lease.job_id, lease.token).await? {
        return Ok(RunEnd::LeaseLost);
    }
    let done = end.execute(&mut *tx).await.map_err(StoreError::query)?;
    commit(tx).await?;
    Ok(if done.rows_affected() == 1 {
        RunEnd::Ended
    } else {
        RunEnd::Over
    })
}

/// Ends an active run as `failed` with a code (never a value), fenced on the job's lease.
pub async fn fail_run(
    pool: &PgPool,
    workspace: WorkspaceId,
    run_id: Uuid,
    lease: Lease,
    code: &str,
) -> Result<RunEnd, StoreError> {
    let end = sqlx::query(
        "UPDATE sync_runs SET state = 'failed', error_code = $3, finished_at = now(), \
            updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 AND state IN ('queued', 'running')",
    )
    .bind(workspace.as_uuid())
    .bind(run_id)
    .bind(code);
    fenced_end(pool, lease, end).await
}

/// Ends a running run as `succeeded`, fenced on the job's lease.
pub async fn finish_run(
    pool: &PgPool,
    workspace: WorkspaceId,
    run_id: Uuid,
    lease: Lease,
) -> Result<RunEnd, StoreError> {
    let end = sqlx::query(
        "UPDATE sync_runs SET state = 'succeeded', finished_at = now(), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 AND state = 'running'",
    )
    .bind(workspace.as_uuid())
    .bind(run_id);
    fenced_end(pool, lease, end).await
}

/// Fails every active run whose job is dead or gone, with `job_failed`, and returns the ids.
///
/// A job the reaper buried after its last lease expiry, or that was deleted, never runs its
/// handler again, so nothing else would end its run, and the run would keep the stream's one
/// active-run slot for good. The job is found by the key `queue_run` gave it. A run and its job
/// are inserted in one transaction, so a run without a job row is never one still being
/// queued.
pub async fn fail_orphaned_runs(pool: &PgPool) -> Result<Vec<Uuid>, StoreError> {
    sqlx::query_scalar(
        "UPDATE sync_runs r SET state = 'failed', error_code = 'job_failed', \
            finished_at = now(), updated_at = now() \
         WHERE r.state IN ('queued', 'running') \
           AND NOT EXISTS (SELECT 1 FROM jobs j \
                           WHERE j.unique_key = 'sync:run:' || r.id::text AND j.state <> 'dead') \
         RETURNING r.id",
    )
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)
}

/// A record a run refused, as the API shows it: codes and a masked sample.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct RunReject {
    pub id: Uuid,
    pub received_at: OffsetDateTime,
    pub reason: String,
    /// `[{property, reason}]`.
    pub reasons: Value,
    /// `{properties, masked}`: only values the stream's mapping sends to non-PII traits.
    pub sample: Value,
}

/// The first `limit` rejects of a run of the workspace, oldest first.
pub async fn list_run_rejects<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    run_id: Uuid,
    limit: u32,
) -> Result<Vec<RunReject>, StoreError> {
    sqlx::query_as(
        "SELECT id, received_at, reason, reasons, sample FROM ingest_rejects \
         WHERE workspace_id = $1 AND sync_run_id = $2 \
         ORDER BY received_at, id LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(run_id)
    .bind(i64::from(limit))
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::sync_runs) fn sample_run() -> SyncRun {
        SyncRun {
            id: Uuid::now_v7(),
            source_id: Uuid::now_v7(),
            stream_id: Uuid::now_v7(),
            stream_name: "contacts".into(),
            state: "queued".into(),
            trigger: "manual".into(),
            sync_mode: "full_refresh".into(),
            records_read: 0,
            records_applied: 0,
            records_unchanged: 0,
            records_rejected: 0,
            records_deleted: 0,
            reject_threshold: 0.05,
            checkpoint: None,
            error_code: None,
            attempt: 0,
            trace_id: Uuid::new_v4(),
            created_by: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            finished_at: None,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn only_a_queued_or_running_run_is_active() {
        for (state, active) in [
            ("queued", true),
            ("running", true),
            ("succeeded", false),
            ("failed", false),
            ("cancelled", false),
        ] {
            let mut run = sample_run();
            run.state = state.into();
            assert_eq!(run.is_active(), active, "{state}");
        }
    }

    #[test]
    fn a_trigger_is_stored_as_its_name() {
        assert_eq!(RunTrigger::Manual.as_str(), "manual");
        assert_eq!(RunTrigger::Schedule.as_str(), "schedule");
    }

    use serde_json::json;
    use sqlx::PgPool;

    use crate::sync_runs::testing::{claim_lease, fixture, source_with_stream, workspace};
    use crate::{StreamSettings, configure_stream};

    async fn queue(
        pool: &PgPool,
        fx_ws: WorkspaceId,
        source: Uuid,
        stream: Uuid,
    ) -> Option<SyncRun> {
        let mut connection = pool.acquire().await.unwrap();
        queue_run(
            &mut connection,
            fx_ws,
            source,
            stream,
            RunTrigger::Schedule,
            None,
        )
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_queued_run_has_a_job_under_the_runs_trace_and_cause_and_one_active_run_per_stream(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;

        let run = queue(&pool, ws, source, stream).await.unwrap();
        let second = queue(&pool, ws, source, stream).await;

        assert_eq!(run.state, "queued");
        assert_eq!(run.trigger, "schedule");
        assert_eq!(run.stream_name, "contacts");
        assert_eq!(run.attempt, 0);
        assert!(second.is_none(), "one active run per stream");
        let job: (String, Value, String, String, uuid::Uuid, Option<String>) = sqlx::query_as(
            "SELECT kind, payload, cause_kind, cause_id, trace_id, unique_key FROM jobs",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(job.0, "sync.run");
        assert_eq!(job.1["run_id"], json!(run.id));
        assert_eq!(job.1["stream_id"], json!(stream));
        assert_eq!(job.2, "sync_run");
        assert_eq!(job.3, run.id.to_string());
        assert_eq!(job.4, run.trace_id);
        assert_eq!(job.5, Some(format!("sync:run:{}", run.id)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_finished_run_frees_the_stream_for_the_next_one(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let first = queue(&pool, ws, source, stream).await.unwrap();
        start_run(&pool, ws, first.id).await.unwrap().unwrap();
        let lease = claim_lease(&pool).await;

        assert_eq!(
            finish_run(&pool, ws, first.id, lease).await.unwrap(),
            RunEnd::Ended
        );
        let next = queue(&pool, ws, source, stream).await;

        assert!(next.is_some());
        assert_eq!(
            finish_run(&pool, ws, first.id, lease).await.unwrap(),
            RunEnd::Over
        );
        assert_eq!(
            fail_run(&pool, ws, first.id, lease, "x").await.unwrap(),
            RunEnd::Over
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn starting_counts_attempts_and_stops_once_the_run_is_over(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let run = queue(&pool, ws, source, stream).await.unwrap();

        let first = start_run(&pool, ws, run.id).await.unwrap().unwrap();
        let again = start_run(&pool, ws, run.id).await.unwrap().unwrap();

        assert_eq!((first.state.as_str(), first.attempt), ("running", 1));
        assert_eq!(again.attempt, 2);
        assert_eq!(again.started_at, first.started_at);
        let lease = claim_lease(&pool).await;
        assert_eq!(
            fail_run(&pool, ws, run.id, lease, "source_error")
                .await
                .unwrap(),
            RunEnd::Ended
        );
        assert!(start_run(&pool, ws, run.id).await.unwrap().is_none());
        let failed = find_run(&pool, ws, source, run.id).await.unwrap().unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error_code.as_deref(), Some("source_error"));
        assert!(failed.finished_at.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn cancelling_moves_an_active_run_and_refuses_the_rest(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let run = queue(&pool, ws, source, stream).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let foreign = cancel_run(&mut connection, other, source, run.id)
            .await
            .unwrap();
        let wrong_source = cancel_run(&mut connection, ws, Uuid::now_v7(), run.id)
            .await
            .unwrap();
        let cancelled = cancel_run(&mut connection, ws, source, run.id)
            .await
            .unwrap();
        let again = cancel_run(&mut connection, ws, source, run.id)
            .await
            .unwrap();

        assert_eq!(foreign, CancelOutcome::NotFound);
        assert_eq!(wrong_source, CancelOutcome::NotFound);
        let CancelOutcome::Cancelled(run) = cancelled else {
            panic!("{cancelled:?}");
        };
        assert_eq!(run.state, "cancelled");
        assert!(run.finished_at.is_some());
        assert_eq!(again, CancelOutcome::NotActive);
        assert!(start_run(&pool, ws, run.id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_fan_out_queues_enabled_mapped_streams_and_counts_the_active_ones(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let descriptor = json!({});
        let mut tx = crate::begin(&pool).await.unwrap();
        crate::upsert_discovered_streams(
            &mut tx,
            ws,
            source,
            &[
                crate::DiscoveredStream {
                    name: "disabled",
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &[],
                    descriptor: &descriptor,
                },
                crate::DiscoveredStream {
                    name: "unmapped",
                    sync_mode: "full_refresh",
                    cursor_field: None,
                    primary_key: &[],
                    descriptor: &descriptor,
                },
            ],
        )
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();
        // `unmapped` is enabled without a mapping; `disabled` has a mapping and is disabled.
        let enable = StreamSettings {
            enabled: true,
            sync_mode: "full_refresh",
            cursor_field: None,
            primary_key: &[],
            staleness_threshold_seconds: None,
        };
        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source,
            "unmapped",
            &enable,
        )
        .await
        .unwrap();
        crate::set_stream_mapping(&pool, ws, source, "disabled", &json!({"columns": []}))
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let first = queue_runs_for_source(
            &mut connection,
            ws,
            source,
            RunTrigger::Schedule,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();
        let second = queue_runs_for_source(
            &mut connection,
            ws,
            source,
            RunTrigger::Schedule,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(first.created.len(), 1);
        assert_eq!(first.created[0].stream_id, stream);
        assert_eq!(first.skipped_active, 0);
        assert!(second.created.is_empty());
        assert_eq!(second.skipped_active, 1);
        let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(jobs, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_fan_out_skips_a_disabled_or_missing_source_when_asked(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, _) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        sqlx::query("UPDATE sources SET enabled = false")
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let strict = queue_runs_for_source(
            &mut connection,
            ws,
            source,
            RunTrigger::Schedule,
            None,
            true,
        )
        .await
        .unwrap();
        let manual =
            queue_runs_for_source(&mut connection, ws, source, RunTrigger::Manual, None, false)
                .await
                .unwrap();
        let missing = queue_runs_for_source(
            &mut connection,
            ws,
            Uuid::now_v7(),
            RunTrigger::Manual,
            None,
            false,
        )
        .await
        .unwrap();

        assert!(strict.is_none());
        assert_eq!(manual.unwrap().created.len(), 1);
        assert!(missing.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn runs_are_listed_newest_first_by_page_and_by_stream_inside_one_workspace(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let (other_source, other_stream) =
            source_with_stream(&pool, other, "crm", "full_refresh").await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            let run = queue(&pool, ws, source, stream).await.unwrap();
            let lease = claim_lease(&pool).await;
            fail_run(&pool, ws, run.id, lease, "x").await.unwrap();
            ids.push(run.id);
        }
        queue(&pool, other, other_source, other_stream)
            .await
            .unwrap();

        let (first, more) = list_runs(&pool, ws, source, None, None, 2).await.unwrap();
        let (second, more_after) = list_runs(
            &pool,
            ws,
            source,
            None,
            Some(RunCursor::of(first.last().unwrap())),
            2,
        )
        .await
        .unwrap();
        let (named, _) = list_runs(&pool, ws, source, Some("contacts"), None, 10)
            .await
            .unwrap();
        let (none, _) = list_runs(&pool, ws, source, Some("nope"), None, 10)
            .await
            .unwrap();
        let (foreign, _) = list_runs(&pool, other, source, None, None, 10)
            .await
            .unwrap();

        assert!(more && !more_after);
        assert_eq!(
            first.iter().map(|r| r.id).collect::<Vec<_>>(),
            [ids[2], ids[1]]
        );
        assert_eq!(second.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[0]]);
        assert_eq!(named.len(), 3);
        assert!(none.is_empty() && foreign.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_rejects_of_a_run_are_read_back_by_workspace_and_run(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, name, reason, \
                reasons, sample, trace_id, sync_run_id) \
             VALUES ($1, $2, now(), 's', 'record', 'contacts', 'invalid_email', '[]', '{}', $3, $4)",
        )
        .bind(Uuid::now_v7())
        .bind(fx.ws.as_uuid())
        .bind(fx.run.trace_id)
        .bind(fx.run.id)
        .execute(&pool)
        .await
        .unwrap();

        let own = list_run_rejects(&pool, fx.ws, fx.run.id, 10).await.unwrap();
        let none = list_run_rejects(&pool, fx.ws, Uuid::now_v7(), 10)
            .await
            .unwrap();

        assert_eq!(own.len(), 1);
        assert_eq!(own[0].reason, "invalid_email");
        assert!(none.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_that_lost_its_lease_can_neither_finish_nor_fail_the_run(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let held = Lease {
            job_id: fx.job.id,
            token: fx.job.lease_token,
        };
        let stale = Lease {
            token: Uuid::new_v4(),
            ..held
        };

        let finished = finish_run(&pool, fx.ws, fx.run.id, stale).await.unwrap();
        let failed = fail_run(&pool, fx.ws, fx.run.id, stale, "job_failed")
            .await
            .unwrap();

        assert_eq!((finished, failed), (RunEnd::LeaseLost, RunEnd::LeaseLost));
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "running");
        assert_eq!(run.error_code, None);
        assert_eq!(
            finish_run(&pool, fx.ws, fx.run.id, held).await.unwrap(),
            RunEnd::Ended
        );
    }

    async fn state_of(
        pool: &PgPool,
        ws: WorkspaceId,
        source: Uuid,
        run: Uuid,
    ) -> (String, Option<String>) {
        let run = find_run(pool, ws, source, run).await.unwrap().unwrap();
        (run.state, run.error_code)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_whose_job_is_dead_or_gone_is_failed_and_the_rest_are_left_alone(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, _) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let descriptor = json!({});
        let mut tx = crate::begin(&pool).await.unwrap();
        let names = ["dead", "gone", "live", "running", "done"];
        let streams: Vec<crate::DiscoveredStream<'_>> = names
            .iter()
            .map(|name| crate::DiscoveredStream {
                name,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                descriptor: &descriptor,
            })
            .collect();
        let found = crate::upsert_discovered_streams(&mut tx, ws, source, &streams)
            .await
            .unwrap()
            .unwrap();
        crate::commit(tx).await.unwrap();
        let mut runs = std::collections::HashMap::new();
        for name in names {
            let stream = found.iter().find(|stream| stream.name == name).unwrap();
            runs.insert(name, queue(&pool, ws, source, stream.id).await.unwrap());
        }
        let job_of = |name: &str| format!("sync:run:{}", runs[name].id);
        sqlx::query("UPDATE jobs SET state = 'dead' WHERE unique_key = $1")
            .bind(job_of("dead"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM jobs WHERE unique_key = $1")
            .bind(job_of("gone"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE sync_runs SET state = 'running' WHERE id = $1")
            .bind(runs["running"].id)
            .execute(&pool)
            .await
            .unwrap();
        // A finished run has no use for its job.
        sqlx::query("UPDATE sync_runs SET state = 'succeeded' WHERE id = $1")
            .bind(runs["done"].id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM jobs WHERE unique_key = $1")
            .bind(job_of("done"))
            .execute(&pool)
            .await
            .unwrap();

        let mut failed = fail_orphaned_runs(&pool).await.unwrap();
        failed.sort();

        let mut expected = vec![runs["dead"].id, runs["gone"].id];
        expected.sort();
        assert_eq!(failed, expected);
        for (name, state, code) in [
            ("dead", "failed", Some("job_failed")),
            ("gone", "failed", Some("job_failed")),
            ("live", "queued", None),
            ("running", "running", None),
            ("done", "succeeded", None),
        ] {
            let found = state_of(&pool, ws, source, runs[name].id).await;
            assert_eq!(found, (state.to_owned(), code.map(str::to_owned)), "{name}");
        }
        // The stream is free again, and a second pass finds nothing.
        let stream = found.iter().find(|stream| stream.name == "dead").unwrap();
        assert!(queue(&pool, ws, source, stream.id).await.is_some());
        assert!(fail_orphaned_runs(&pool).await.unwrap().is_empty());
    }
}
