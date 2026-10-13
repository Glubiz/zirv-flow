//! `import.dry_run`: maps an uploaded CSV file without applying it (architecture section 7.2).
//! It reads the stored file in batches of 500 rows and writes nothing but the rejects (phase
//! `dry_run`) and the counters, which a batch advances together. A crashed attempt resumes
//! from the checkpoint like a run.
//!
//! Nothing here logs a cell, an identifier or a trait value.

use serde_json::Value;
use tracing::info;
use zirv_flow_core::mapping::Header;
use zirv_flow_store::{
    DRY_RUN_JOB_KIND, DryRunBatch, ImportStatus, TransitionError, commit_dry_run_batch,
    find_import, finish_import_dry_run,
};

use super::import_rows::{BATCH_ROWS, Planner, Probe, RowReader, hex, load_catalog};
use super::import_run::{fail, fail_last_attempt, import_id, unreadable};
use crate::{JobContext, JobError, JobKind};

pub(crate) struct ImportDryRun;

impl JobKind for ImportDryRun {
    const KIND: &'static str = DRY_RUN_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        dry_run_import(ctx, &payload, None).await
    }
}

/// Dry-runs the import. With `stop_after`, a test simulates a crash after that many batches.
pub(super) async fn dry_run_import(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    stop_after: Option<usize>,
) -> Result<(), JobError> {
    dry_run_import_probed(ctx, payload, &Probe::stopping_after(stop_after)).await
}

/// [`dry_run_import`] with the test hooks of `probe`. When the attempt fails and no attempt is
/// left, the import is marked `failed` with `job_failed`.
pub(super) async fn dry_run_import_probed(
    ctx: &mut JobContext<'_>,
    payload: &Value,
    probe: &Probe,
) -> Result<(), JobError> {
    let last = ctx.attempts() >= ctx.max_attempts();
    let (pool, workspace) = (ctx.pool().cloned(), ctx.workspace_id());
    let result = dry_run_import_pass(ctx, payload, probe).await;
    if result.is_err() && last {
        fail_last_attempt(pool, workspace, payload).await;
    }
    result
}

async fn dry_run_import_pass(
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
    if import.status != ImportStatus::DryRunning {
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
        observed_at: import.created_at,
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
        let count = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let planned = planner.plan(rows);
        let batch = DryRunBatch {
            import_id: id,
            workspace,
            expected_done: done,
            accepted: i64::try_from(planned.records.len()).unwrap_or(i64::MAX),
            rejected: &planned.rejected,
        };
        probe.before_commit().await;
        if !commit_dry_run_batch(&pool, &batch).await? {
            // The checkpoint moved: a cancel, or an attempt that got further. Go on from where
            // the import now stands, unless it ended.
            let Some(now) = find_import(&pool, workspace, id).await? else {
                return Ok(());
            };
            if now.status != ImportStatus::DryRunning {
                return Ok(());
            }
            if now.rows_done == done {
                return Err(JobError::new("the dry run batch kept conflicting"));
            }
            reader = RowReader::new(&pool, &now);
            if let Err(error) = reader.skip(now.rows_done).await {
                return unreadable(&pool, workspace, id, error).await;
            }
            done = now.rows_done;
            continue;
        }
        done += count;
        batches += 1;
        if probe.stop_after == Some(batches) {
            return Err(JobError::new("stopped for a test"));
        }
    }

    let mut connection = pool.acquire().await?;
    match finish_import_dry_run(&mut connection, workspace, id).await {
        Ok(_) | Err(TransitionError::WrongState(_) | TransitionError::NotFound) => {
            info!(import_id = %id, rows_done = done, "import dry run finished");
            Ok(())
        }
        Err(TransitionError::Store(error)) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;
    use zirv_flow_core::mapping::{ColumnRule, Mapping, Target, Transform};
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};
    use zirv_flow_store::{
        Import, ImportPhase, ImportUpload, UploadScan, UploadStart, list_import_rejects,
        set_import_mapping, start_import_dry_run,
    };

    use super::*;
    use crate::context::IngestSettings;

    #[test]
    fn the_kind_name_is_import_dry_run() {
        assert_eq!(ImportDryRun::KIND, "import.dry_run");
    }

    async fn dry_running(pool: &PgPool, rows: usize, bad_every: usize) -> (WorkspaceId, Import) {
        let mut text = String::from("email\n");
        for n in 0..rows {
            if n % bad_every == bad_every - 1 {
                text.push_str("nope\n");
            } else {
                text.push_str(&format!("u{n}@example.com\n"));
            }
        }
        dry_running_text(pool, &text, rows).await
    }

    /// A dry-running import of `text`, which holds `rows` data rows.
    async fn dry_running_text(pool: &PgPool, text: &str, rows: usize) -> (WorkspaceId, Import) {
        zirv_flow_store::migrate(pool).await.unwrap();
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = zirv_flow_store::create_workspace(pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let header = vec!["email".to_owned()];
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
            text.as_bytes(),
            &UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: i64::try_from(rows).unwrap(),
            },
        )
        .await
        .unwrap();
        let mapping = Mapping {
            key_column: None,
            event: None,
            columns: vec![ColumnRule {
                column: "email".into(),
                target: Target::Identifier {
                    kind: "email".into(),
                },
                transforms: vec![Transform::Trim],
            }],
        };
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        set_import_mapping(&mut tx, ws, import.id, &mapping)
            .await
            .unwrap();
        let started = start_import_dry_run(&mut tx, ws, import.id).await.unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        (ws, started)
    }

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
            kind: ImportDryRun::KIND.to_owned(),
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
        dry_run_import_probed(&mut ctx, &claimed.payload.clone(), probe).await
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_writes_no_profile_rows_only_rejects_and_counters(pool: PgPool) {
        let (ws, import) = dry_running(&pool, 1100, 10).await;

        attempt(&pool, &import, None).await.unwrap();

        let done = find_import(&pool, ws, import.id).await.unwrap().unwrap();
        assert_eq!(done.status, ImportStatus::DryRunDone);
        assert_eq!(
            (done.rows_done, done.rows_applied, done.rows_rejected),
            (1100, 990, 110)
        );
        for table in [
            "people",
            "identities",
            "source_records",
            "person_changes",
            "list_members",
        ] {
            assert_eq!(count(&pool, table).await, 0, "{table}");
        }
        let (rejects, more) = list_import_rejects(
            &pool,
            ws,
            import.id,
            Some(ImportPhase::DryRun),
            None,
            50,
            false,
        )
        .await
        .unwrap();
        assert!(more);
        assert_eq!(rejects[0].row_number, 11);
        assert_eq!(rejects[0].errors[0].reason, "invalid_identifier");
        assert_eq!(count(&pool, "import_rejects").await, 110);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_crashed_dry_run_resumes_to_the_same_counts(pool: PgPool) {
        let (ws, import) = dry_running(&pool, 1100, 10).await;

        assert!(attempt(&pool, &import, Some(1)).await.is_err());
        attempt(&pool, &import, None).await.unwrap();

        let done = find_import(&pool, ws, import.id).await.unwrap().unwrap();
        assert_eq!(done.status, ImportStatus::DryRunDone);
        assert_eq!(
            (done.rows_done, done.rows_applied, done.rows_rejected),
            (1100, 990, 110)
        );
        assert_eq!(count(&pool, "import_rejects").await, 110);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_batch_fenced_out_goes_on_from_the_checkpoint_to_the_end(pool: PgPool) {
        let (ws, import) = dry_running(&pool, 1100, 10).await;
        let behind = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = Probe {
            stop_after: None,
            before_commit: Some(Box::new({
                let (pool, import, behind) = (pool.clone(), import.clone(), behind.clone());
                move || {
                    let (pool, import, behind) = (pool.clone(), import.clone(), behind.clone());
                    Box::pin(async move {
                        if !behind.swap(true, std::sync::atomic::Ordering::SeqCst) {
                            assert!(attempt(&pool, &import, Some(1)).await.is_err());
                        }
                    })
                }
            })),
        };

        attempt_probed(&pool, &import, &probe).await.unwrap();

        let done = find_import(&pool, ws, import.id).await.unwrap().unwrap();
        assert_eq!(done.status, ImportStatus::DryRunDone);
        assert_eq!(
            (done.rows_done, done.rows_applied, done.rows_rejected),
            (1100, 990, 110)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rejects_carry_the_row_a_spreadsheet_shows_with_blank_lines_and_multi_line_values_counted(
        pool: PgPool,
    ) {
        // Rows: header 1, blank 2, "nope" 3, one record over two lines 4, blank 5, "bad" 6.
        let text = "email\n\nnope\n\"ada@example.com\n\"\n\nbad\nok@example.com\n";
        let (ws, import) = dry_running_text(&pool, text, 4).await;

        attempt(&pool, &import, None).await.unwrap();

        let (rejects, _) = list_import_rejects(&pool, ws, import.id, None, None, 10, false)
            .await
            .unwrap();
        let rows: Vec<i64> = rejects.iter().map(|reject| reject.row_number).collect();
        assert_eq!(rows, [3, 6]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_whose_last_attempt_fails_is_marked_failed(pool: PgPool) {
        let (ws, import) = dry_running(&pool, 1100, 10).await;
        let crash = Probe::stopping_after(Some(1));

        assert!(attempt_number(&pool, &import, &crash, 4).await.is_err());
        assert_eq!(
            find_import(&pool, ws, import.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            ImportStatus::DryRunning
        );
        assert!(attempt_number(&pool, &import, &crash, 5).await.is_err());

        let done = find_import(&pool, ws, import.id).await.unwrap().unwrap();
        assert_eq!(done.status, ImportStatus::Failed);
        assert_eq!(done.error_code.as_deref(), Some("job_failed"));
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_that_is_not_running_does_nothing(pool: PgPool) {
        let (ws, import) = dry_running(&pool, 20, 10).await;
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::cancel_import(&mut tx, ws, import.id)
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        attempt(&pool, &import, None).await.unwrap();

        let after = find_import(&pool, ws, import.id).await.unwrap().unwrap();
        assert_eq!(
            (after.status, after.rows_done),
            (ImportStatus::Cancelled, 0)
        );
        assert_eq!(count(&pool, "import_rejects").await, 0);
    }
}
