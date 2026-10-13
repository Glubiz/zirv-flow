//! `import.run`: applies an uploaded CSV file (architecture sections 7.1 and 7.2). The file is
//! read from its stored chunks in batches of 500 rows. Each batch is ONE transaction of its
//! own that applies the profile records, adds the list members, writes the rejects and
//! advances the checkpoint, so the job's transaction only holds the lease. The worker
//! heartbeats the lease on its own pool, so a long run is one job: a crash or a lost lease
//! resumes from the checkpoint (`rows_done`) when the job is retried.
//!
//! A run fails with `reject_threshold` once more than 1,000 rows are done and the rejected
//! share is over the import's threshold; the batches already applied stay applied. Cancelling
//! the import stops the job at its next batch.
//!
//! Nothing here logs a cell, an identifier or a trait value.

use serde_json::Value;
use tracing::info;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_core::mapping::Header;
use zirv_flow_store::{
    BatchOutcome, ImportBatch, ImportStatus, PgPool, RUN_JOB_KIND, TransitionError,
    commit_import_batch, fail_import, find_import, finish_import_run,
};

use super::import_rows::{BATCH_ROWS, Planner, Probe, ReadError, RowReader, hex, load_catalog};
use super::ingest_drain::whole_then_singly;
use crate::{JobContext, JobError, JobKind};

pub(crate) struct ImportRun;

impl JobKind for ImportRun {
    const KIND: &'static str = RUN_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        run_import(ctx, &payload, None).await
    }
}

const CONFLICTED: JobError = JobError::new("the import batch kept conflicting with another");

/// The id the payload names.
pub(super) fn import_id(payload: &Value) -> Result<Uuid, JobError> {
    payload
        .get("import_id")
        .and_then(Value::as_str)
        .and_then(|text| Uuid::parse_str(text).ok())
        .ok_or(JobError::new("the payload has no import_id"))
}

/// Ends the import with `code`, unless it ended already.
pub(super) async fn fail(
    pool: &PgPool,
    workspace: WorkspaceId,
    import: Uuid,
    code: &'static str,
) -> Result<(), JobError> {
    let mut connection = pool.acquire().await?;
    match fail_import(&mut connection, workspace, import, code).await {
        Ok(_) | Err(TransitionError::WrongState(_) | TransitionError::NotFound) => Ok(()),
        Err(TransitionError::Store(error)) => Err(error.into()),
    }
}

/// Ends the import for a file that cannot be read, or fails the attempt for a database error.
pub(super) async fn unreadable(
    pool: &PgPool,
    workspace: WorkspaceId,
    import: Uuid,
    error: ReadError,
) -> Result<(), JobError> {
    match error.code() {
        Some(code) => fail(pool, workspace, import, code).await,
        None => Err(JobError::new("database query failed")),
    }
}

/// One batch: whole when it can be, record by record when a record is poison.
async fn commit_batch(pool: &PgPool, batch: &ImportBatch<'_>) -> Result<BatchOutcome, JobError> {
    whole_then_singly(
        CONFLICTED,
        "the import batch could not be applied",
        |singly| commit_import_batch(pool, batch, singly),
    )
    .await
}

/// Runs the import. With `stop_after`, a test simulates a crash after that many batches.
pub(super) async fn run_import(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    stop_after: Option<usize>,
) -> Result<(), JobError> {
    run_import_probed(ctx, payload, &Probe::stopping_after(stop_after)).await
}

/// [`run_import`] with the test hooks of `probe`. When the attempt fails and no attempt is left,
/// the import is marked `failed` with `job_failed`, so it never stays `running` without a job.
pub(super) async fn run_import_probed(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    probe: &Probe,
) -> Result<(), JobError> {
    let last = ctx.attempts() >= ctx.max_attempts();
    let (pool, workspace) = (ctx.pool().cloned(), ctx.workspace_id());
    let result = run_import_pass(ctx, payload, probe).await;
    if result.is_err() && last {
        fail_last_attempt(pool, workspace, payload).await;
    }
    result
}

/// Ends the import of a job that has no attempt left; best effort, since the job is dead anyway.
pub(super) async fn fail_last_attempt(
    pool: Option<PgPool>,
    workspace: Option<WorkspaceId>,
    payload: &Value,
) {
    if let (Some(pool), Some(workspace), Ok(id)) = (pool, workspace, import_id(payload)) {
        let _ = fail(&pool, workspace, id, "job_failed").await;
    }
}

async fn run_import_pass(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    probe: &Probe,
) -> Result<(), JobError> {
    let id = import_id(payload)?;
    let workspace = ctx
        .workspace_id()
        .ok_or(JobError::new("the import job has no workspace"))?;
    let pool = ctx
        .pool()
        .ok_or(JobError::new("the worker has no pool for imports"))?
        .clone();
    let Some(import) = find_import(&pool, workspace, id).await? else {
        return Ok(());
    };
    // Cancelled, or finished by an earlier attempt: nothing to do.
    if import.status != ImportStatus::Running {
        return Ok(());
    }
    let Some(mapping) = &import.mapping else {
        return fail(&pool, workspace, id, "no_mapping").await;
    };
    let Ok(header) = Header::new(import.header.clone()) else {
        return fail(&pool, workspace, id, "bad_header").await;
    };
    let catalog = load_catalog(&pool, workspace).await?;
    let file_hash = hex(&import.file_sha256);
    let trace = ctx.trace().clone();
    let planner = Planner {
        mapping,
        header: &header,
        catalog: &catalog,
        source: &import.source,
        stream: &import.stream,
        file_hash: &file_hash,
        // The time the run was started, so a resumed run observes what the first attempt did.
        observed_at: import.started_at.unwrap_or(import.created_at),
        trace: &trace,
    };

    let mut reader = RowReader::new(&pool, &import);
    if let Err(error) = reader.skip(import.rows_done).await {
        return unreadable(&pool, workspace, id, error).await;
    }
    let mut done = import.rows_done;
    let mut batches = 0_usize;
    loop {
        let rows = match reader.next_rows(BATCH_ROWS).await {
            Ok(rows) => rows,
            Err(error) => return unreadable(&pool, workspace, id, error).await,
        };
        if rows.is_empty() {
            break;
        }
        let planned = planner.plan(rows);
        let batch = ImportBatch {
            import_id: id,
            workspace,
            expected_done: done,
            records: &planned.records,
            details: &planned.details,
            rejected: &planned.rejected,
            trace: &trace,
        };
        probe.before_commit().await;
        match commit_batch(&pool, &batch).await? {
            BatchOutcome::Committed(progress) => {
                done = progress.rows_done;
                if progress.threshold_exceeded {
                    info!(import_id = %id, rows_done = done, "import stopped by the reject threshold");
                    return Ok(());
                }
            }
            // The checkpoint moved: a cancel, or an attempt that got further (or a commit this
            // one never heard of). Go on from where the import now stands, unless it ended.
            BatchOutcome::Superseded => {
                let Some(now) = find_import(&pool, workspace, id).await? else {
                    return Ok(());
                };
                if now.status != ImportStatus::Running {
                    return Ok(());
                }
                if now.rows_done == done {
                    return Err(CONFLICTED);
                }
                reader = RowReader::new(&pool, &now);
                if let Err(error) = reader.skip(now.rows_done).await {
                    return unreadable(&pool, workspace, id, error).await;
                }
                done = now.rows_done;
                continue;
            }
            BatchOutcome::ListMissing => {
                return fail(&pool, workspace, id, "list_not_found").await;
            }
        }
        batches += 1;
        if probe.stop_after == Some(batches) {
            return Err(JobError::new("stopped for a test"));
        }
    }

    let mut connection = pool.acquire().await?;
    match finish_import_run(&mut connection, workspace, id).await {
        Ok(_) | Err(TransitionError::WrongState(_) | TransitionError::NotFound) => {
            info!(import_id = %id, rows_done = done, "import run finished");
            Ok(())
        }
        Err(TransitionError::Store(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::mapping::{ColumnRule, Mapping, Target, Transform};
    use zirv_flow_core::people::TraitType;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};
    use zirv_flow_store::{
        Import, ImportUpload, NewTraitDefinition, UploadScan, UploadStart, cancel_import,
        create_list, create_trait_definition, find_import, list_import_rejects, set_import_mapping,
        start_import_run,
    };

    use super::*;
    use crate::context::IngestSettings;

    #[test]
    fn the_kind_name_is_import_run() {
        assert_eq!(ImportRun::KIND, "import.run");
    }

    #[test]
    fn the_payload_names_the_import() {
        let id = Uuid::now_v7();

        assert_eq!(import_id(&json!({"import_id": id.to_string()})), Ok(id));
        assert!(import_id(&json!({})).is_err());
        assert!(import_id(&json!({"import_id": "nope"})).is_err());
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id
    }

    async fn plan_trait(pool: &PgPool, ws: WorkspaceId) {
        create_trait_definition(
            pool,
            ws,
            &NewTraitDefinition {
                key: "age".into(),
                trait_type: TraitType::Number,
                precedence: zirv_flow_core::people::Precedence::Latest,
                source_order: Vec::new(),
                pii: false,
                channel_address: false,
                inferred: false,
                description: String::new(),
            },
        )
        .await
        .unwrap();
    }

    fn rule(column: &str, target: Target, transforms: Vec<Transform>) -> ColumnRule {
        ColumnRule {
            column: column.into(),
            target,
            transforms,
        }
    }

    fn mapping(list: Option<Uuid>) -> Mapping {
        let mut columns = vec![
            rule(
                "email",
                Target::Identifier {
                    kind: "email".into(),
                },
                vec![Transform::Trim, Transform::Lower],
            ),
            rule("age", Target::Trait { key: "age".into() }, vec![]),
        ];
        if let Some(list_id) = list {
            columns.push(rule("news", Target::List { list_id }, vec![]));
        }
        Mapping {
            delete_when: None,
            key_column: None,
            columns,
        }
    }

    /// A file of `rows` people; every `bad_every`-th row has an email that is not one.
    fn file(rows: usize, bad_every: usize) -> Vec<u8> {
        let mut text = String::from("email,age,news\n");
        for n in 0..rows {
            if bad_every > 0 && n % bad_every == bad_every - 1 {
                text.push_str(&format!("not-an-email-{n},{},yes\n", n % 90));
            } else {
                text.push_str(&format!("User{n}@Example.com,{},yes\n", n % 90));
            }
        }
        text.into_bytes()
    }

    async fn start(
        pool: &PgPool,
        ws: WorkspaceId,
        bytes: &[u8],
        rows: usize,
        list: Option<Uuid>,
    ) -> Import {
        let header = vec!["email".to_owned(), "age".to_owned(), "news".to_owned()];
        let trace = TraceContext::root(Cause::api("t"));
        let import = ImportUpload::store(
            pool,
            ws,
            &UploadStart {
                name: "people",
                source: "csv",
                stream: "people",
                file_name: "people.csv",
                created_by: None,
                trace: &trace,
            },
            bytes,
            &UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: i64::try_from(rows).unwrap(),
            },
        )
        .await
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        set_import_mapping(&mut tx, ws, import.id, &mapping(list))
            .await
            .unwrap();
        let started = start_import_run(&mut tx, ws, import.id).await.unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        started
    }

    /// Runs the handler once on the import's job, in a transaction of its own that is dropped
    /// afterwards, as a worker's would be when the attempt fails.
    async fn attempt(
        pool: &PgPool,
        import: &Import,
        stop_after: Option<usize>,
    ) -> Result<(), JobError> {
        attempt_probed(pool, import, &Probe::stopping_after(stop_after)).await
    }

    async fn attempt_probed(pool: &PgPool, import: &Import, probe: &Probe) -> Result<(), JobError> {
        attempt_number(pool, import, probe, 1).await
    }

    /// Like [`attempt_probed`], as attempt `attempts` of 5.
    async fn attempt_number(
        pool: &PgPool,
        import: &Import,
        probe: &Probe,
        attempts: i32,
    ) -> Result<(), JobError> {
        let claimed = zirv_flow_store::ClaimedJob {
            id: Uuid::now_v7(),
            workspace_id: Some(import.workspace_id),
            kind: ImportRun::KIND.to_owned(),
            payload: json!({ "import_id": import.id }),
            attempts,
            max_attempts: 5,
            lease_token: Uuid::now_v7(),
            unique_key: None,
            trace: TraceContext::root(Cause::api("t")),
            claim_lag_seconds: 0.0,
        };
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed).with_ingest(IngestSettings {
            pool: pool.clone(),
            redaction_key: None,
            batch_size: 500,
            shards: 1,
        });
        run_import_probed(&mut ctx, &claimed.payload.clone(), probe).await
    }

    async fn reread(pool: &PgPool, import: &Import) -> Import {
        find_import(pool, WorkspaceId::from_uuid(import.workspace_id), import.id)
            .await
            .unwrap()
            .unwrap()
    }

    /// The people an import left behind, comparable between workspaces: record keys with
    /// their content hashes, the person count and the list sizes.
    async fn snapshot(pool: &PgPool, ws: WorkspaceId) -> (Vec<(String, Vec<u8>)>, i64, Vec<i64>) {
        let records = sqlx::query_as(
            "SELECT record_key, content_hash FROM source_records WHERE workspace_id = $1 \
             ORDER BY record_key",
        )
        .bind(ws.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap();
        let people = sqlx::query_scalar("SELECT count(*) FROM people WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(pool)
            .await
            .unwrap();
        let lists = sqlx::query_scalar(
            "SELECT member_count FROM lists WHERE workspace_id = $1 ORDER BY lower(name)",
        )
        .bind(ws.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap();
        (records, people, lists)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_applies_every_row_in_batches_and_succeeds(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let list = create_list(&pool, ws, "News", "", None).await.unwrap();
        let import = start(&pool, ws, &file(1200, 0), 1200, Some(list.id)).await;

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Succeeded);
        assert_eq!(
            (
                done.rows_done,
                done.rows_applied,
                done.rows_unchanged,
                done.rows_rejected
            ),
            (1200, 1200, 0, 0)
        );
        let (_, people, lists) = snapshot(&pool, ws).await;
        assert_eq!((people, lists), (1200, vec![1200]));
        let traits: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM people WHERE workspace_id = $1 AND traits ? 'age'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(traits, 1200);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resumed_run_ends_in_the_state_of_a_clean_run(pool: PgPool) {
        let clean = workspace(&pool, "clean").await;
        let crashed = workspace(&pool, "crashed").await;
        let bytes = file(1700, 40);
        let mut imports = Vec::new();
        for ws in [clean, crashed] {
            plan_trait(&pool, ws).await;
            let list = create_list(&pool, ws, "News", "", None).await.unwrap();
            imports.push(start(&pool, ws, &bytes, 1700, Some(list.id)).await);
        }

        attempt(&pool, &imports[0], None).await.unwrap();
        // The crash: the first attempt dies after its second batch.
        assert!(attempt(&pool, &imports[1], Some(2)).await.is_err());
        let midway = reread(&pool, &imports[1]).await;
        assert_eq!(
            (midway.status, midway.rows_done),
            (ImportStatus::Running, 1000)
        );
        attempt(&pool, &imports[1], None).await.unwrap();

        let (clean_import, resumed) = (
            reread(&pool, &imports[0]).await,
            reread(&pool, &imports[1]).await,
        );
        assert_eq!(resumed.status, ImportStatus::Succeeded);
        assert_eq!(
            (
                resumed.rows_done,
                resumed.rows_applied,
                resumed.rows_unchanged,
                resumed.rows_rejected
            ),
            (
                clean_import.rows_done,
                clean_import.rows_applied,
                clean_import.rows_unchanged,
                clean_import.rows_rejected
            )
        );
        assert_eq!(snapshot(&pool, clean).await, snapshot(&pool, crashed).await);
        let rejects = |ws: WorkspaceId, id: Uuid| {
            let pool = pool.clone();
            async move {
                list_import_rejects(&pool, ws, id, None, None, 1000, false)
                    .await
                    .unwrap()
                    .0
            }
        };
        assert_eq!(
            rejects(clean, imports[0].id).await,
            rejects(crashed, imports[1].id).await
        );
        assert_eq!(rejects(clean, imports[0].id).await.len(), 42);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn running_the_same_file_again_changes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let list = create_list(&pool, ws, "News", "", None).await.unwrap();
        let bytes = file(700, 0);
        let first = start(&pool, ws, &bytes, 700, Some(list.id)).await;
        attempt(&pool, &first, None).await.unwrap();
        let before: Vec<(Uuid, Vec<u8>, time::OffsetDateTime)> = sqlx::query_as(
            "SELECT id, content_hash, updated_at FROM source_records ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let changes_before: i64 = sqlx::query_scalar("SELECT count(*) FROM list_member_changes")
            .fetch_one(&pool)
            .await
            .unwrap();

        let second = start(&pool, ws, &bytes, 700, Some(list.id)).await;
        attempt(&pool, &second, None).await.unwrap();

        let done = reread(&pool, &second).await;
        assert_eq!(done.status, ImportStatus::Succeeded);
        assert_eq!((done.rows_applied, done.rows_unchanged), (0, 700));
        let after: Vec<(Uuid, Vec<u8>, time::OffsetDateTime)> = sqlx::query_as(
            "SELECT id, content_hash, updated_at FROM source_records ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(before, after);
        let changes_after: i64 = sqlx::query_scalar("SELECT count(*) FROM list_member_changes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(changes_before, changes_after);
        assert_eq!(snapshot(&pool, ws).await.1, 700);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_bad_file_fails_at_the_reject_threshold_and_keeps_the_applied_batches(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(3000, 5), 3000, None).await;

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Failed);
        assert_eq!(done.error_code.as_deref(), Some("reject_threshold"));
        assert_eq!(
            done.rows_done, 1500,
            "stopped at the first batch past 1,000 rows"
        );
        assert_eq!(done.rows_rejected, 300);
        assert_eq!(snapshot(&pool, ws).await.1, 1200, "the applied rows stay");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_fenced_out_by_an_attempt_that_got_further_goes_on_and_applies_every_row_once(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(1200, 0), 1200, None).await;
        let behind = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Probe {
            stop_after: None,
            before_commit: Some(Box::new({
                let (pool, import, behind) = (pool.clone(), import.clone(), behind.clone());
                move || {
                    let (pool, import, behind) = (pool.clone(), import.clone(), behind.clone());
                    Box::pin(async move {
                        // Once, another attempt commits the first batch before this one does.
                        if !behind.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            assert!(attempt(&pool, &import, Some(1)).await.is_err());
                        }
                    })
                }
            })),
        };

        attempt_probed(&pool, &import, &probe).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Succeeded);
        assert_eq!(
            (done.rows_done, done.rows_applied, done.rows_unchanged),
            (1200, 1200, 0),
            "every row was applied exactly once"
        );
        assert_eq!(snapshot(&pool, ws).await.1, 1200);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_whose_last_attempt_fails_is_marked_failed_instead_of_left_running(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(3000, 0), 3000, None).await;
        let crash = Probe::stopping_after(Some(1));

        // Attempts 1 to 4 fail and leave the import to the retry.
        for attempts in 1..5 {
            assert!(
                attempt_number(&pool, &import, &crash, attempts)
                    .await
                    .is_err()
            );
            assert_eq!(reread(&pool, &import).await.status, ImportStatus::Running);
        }
        assert!(attempt_number(&pool, &import, &crash, 5).await.is_err());

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Failed);
        assert_eq!(done.error_code.as_deref(), Some("job_failed"));
        assert!(done.finished_at.is_some());
        assert_eq!(done.rows_done, 2500, "the applied batches stay");
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancelled_run_stops_at_the_next_batch_boundary(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(1200, 0), 1200, None).await;
        assert!(attempt(&pool, &import, Some(1)).await.is_err());
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        cancel_import(&mut tx, ws, import.id).await.unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Cancelled);
        assert_eq!(done.rows_done, 500);
        assert_eq!(snapshot(&pool, ws).await.1, 500);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_that_was_deleted_fails_the_import_with_a_code(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(10, 0), 10, Some(Uuid::now_v7())).await;

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Failed);
        assert_eq!(done.error_code.as_deref(), Some("list_not_found"));
        assert_eq!(snapshot(&pool, ws).await.1, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_file_whose_chunks_were_deleted_fails_the_import_with_a_code(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let import = start(&pool, ws, &file(10, 0), 10, None).await;
        sqlx::query("DELETE FROM import_chunks")
            .execute(&pool)
            .await
            .unwrap();

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Failed);
        assert_eq!(done.error_code.as_deref(), Some("file_missing"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn ten_thousand_rows_run_to_the_end(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let list = create_list(&pool, ws, "News", "", None).await.unwrap();
        let import = start(&pool, ws, &file(10_000, 0), 10_000, Some(list.id)).await;

        attempt(&pool, &import, None).await.unwrap();

        let done = reread(&pool, &import).await;
        assert_eq!(done.status, ImportStatus::Succeeded);
        assert_eq!(done.rows_applied, 10_000);
        assert_eq!(snapshot(&pool, ws).await.1, 10_000);
    }

    /// Records how long a big file takes. Run by hand, or in the load-test issue, with
    /// `ZIRV_FLOW_BENCH_ROWS=1000000`; without that variable it does nothing.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_ROWS to measure"]
    async fn a_big_file_records_its_duration(pool: PgPool) {
        let Some(rows) = std::env::var("ZIRV_FLOW_BENCH_ROWS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
        else {
            eprintln!("ZIRV_FLOW_BENCH_ROWS is not set: skipping the timing run");
            return;
        };
        let ws = workspace(&pool, "one").await;
        plan_trait(&pool, ws).await;
        let bytes = file(rows, 0);
        let import = start(&pool, ws, &bytes, rows, None).await;

        let started = Instant::now();
        attempt(&pool, &import, None).await.unwrap();
        let elapsed = started.elapsed();

        let done = reread(&pool, &import).await;
        eprintln!(
            "import of {rows} rows ({} MiB): {:.1} s, {:.0} rows/s",
            bytes.len() >> 20,
            elapsed.as_secs_f64(),
            rows as f64 / elapsed.as_secs_f64()
        );
        assert_eq!(done.status, ImportStatus::Succeeded);
        assert_eq!(done.rows_done, i64::try_from(rows).unwrap());
    }
}
