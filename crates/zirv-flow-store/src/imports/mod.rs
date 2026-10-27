//! CSV imports (architecture sections 7.1, 7.2 and 16.3): the uploaded file kept in chunks,
//! the mapping, the state changes the API makes, the batches the jobs commit, the refused rows
//! and their retention.

mod batch;
mod job;
mod model;
mod rejects;
mod retention;
mod upload;

pub use batch::{
    BatchOutcome, BatchProgress, DryRunBatch, ImportBatch, REJECT_CHECK_MIN_ROWS, RowDetail,
    commit_dry_run_batch, commit_import_batch,
};
pub use job::{DRY_RUN_JOB_KIND, RUN_JOB_KIND, enqueue_import_job};
pub use model::{
    Import, ImportStatus, TransitionError, cancel_import, fail_import, find_import,
    finish_import_dry_run, finish_import_run, list_imports, set_import_mapping,
    start_import_dry_run, start_import_run,
};
pub use rejects::{
    ImportPhase, ImportReject, RejectReason, RejectedRow, StoredReason, list_import_rejects,
};
pub use retention::{
    ABANDONED_AFTER_DAYS, CHUNK_RETENTION_DAYS, ImportErasure, ImportRetention,
    REJECT_RETENTION_DAYS, UPLOAD_ABANDONED_AFTER_DAYS, delete_expired_import_data,
    erase_import_values,
};
pub use upload::{CHUNK_BYTES, ImportUpload, UploadScan, UploadStart, read_import_chunk};

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use sqlx::PgPool;
    use time::macros::datetime;
    use uuid::Uuid;
    use zirv_flow_core::mapping::{ColumnRule, Mapping, Target};
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceId, WorkspaceName};

    use super::*;
    use crate::people::ProfileRecord;
    use crate::{ListSource, add_members, begin, commit, create_list, create_workspace};

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("t"))
    }

    fn mapping() -> Mapping {
        Mapping {
            delete_when: None,
            key_column: None,
            event: None,
            columns: vec![ColumnRule {
                column: "email".into(),
                target: Target::Identifier {
                    kind: "email".into(),
                },
                transforms: vec![],
            }],
        }
    }

    /// Uploads `bytes` as an import with an `email` header.
    async fn upload(pool: &PgPool, ws: WorkspaceId, bytes: &[u8], rows: i64) -> Import {
        let header = vec!["email".to_owned()];
        let trace = trace();
        ImportUpload::store(
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
                rows_total: rows,
            },
        )
        .await
        .unwrap()
    }

    async fn mapped(pool: &PgPool, ws: WorkspaceId, bytes: &[u8], rows: i64) -> Import {
        let import = upload(pool, ws, bytes, rows).await;
        let mut tx = begin(pool).await.unwrap();
        let import = set_import_mapping(&mut tx, ws, import.id, &mapping())
            .await
            .unwrap();
        commit(tx).await.unwrap();
        import
    }

    async fn dry_running(pool: &PgPool, ws: WorkspaceId, rows: i64) -> Import {
        let import = mapped(pool, ws, b"email\n", rows).await;
        let mut tx = begin(pool).await.unwrap();
        let import = start_import_dry_run(&mut tx, ws, import.id).await.unwrap();
        commit(tx).await.unwrap();
        import
    }

    async fn running(pool: &PgPool, ws: WorkspaceId, rows: i64) -> Import {
        let import = mapped(pool, ws, b"email\n", rows).await;
        let mut tx = begin(pool).await.unwrap();
        let import = start_import_run(&mut tx, ws, import.id).await.unwrap();
        commit(tx).await.unwrap();
        import
    }

    fn record(n: usize) -> ProfileRecord {
        ProfileRecord {
            source: "csv".into(),
            stream: "people".into(),
            record_key: format!("k{n}"),
            identifiers: vec![("email".into(), format!("u{n}@example.com"))],
            traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: trace(),
        }
    }

    fn detail(n: usize, lists: Vec<Uuid>) -> RowDetail {
        RowDetail {
            row_number: i64::try_from(n).unwrap() + 2,
            lists,
            raw: vec![format!("u{n}@example.com")],
        }
    }

    fn refused(n: usize) -> RejectedRow {
        RejectedRow {
            row_number: i64::try_from(n).unwrap() + 2,
            reasons: vec![RejectReason {
                column: "email".into(),
                reason: "invalid_identifier",
            }],
            raw: vec!["secret-cell".into()],
        }
    }

    async fn run_batch(
        pool: &PgPool,
        import: &Import,
        expected_done: i64,
        range: std::ops::Range<usize>,
        rejected: &[RejectedRow],
        lists: &[Uuid],
    ) -> BatchOutcome {
        let records: Vec<ProfileRecord> = range.clone().map(record).collect();
        let details: Vec<RowDetail> = range.map(|n| detail(n, lists.to_vec())).collect();
        let trace = trace();
        commit_import_batch(
            pool,
            &ImportBatch {
                import_id: import.id,
                workspace: WorkspaceId::from_uuid(import.workspace_id),
                expected_done,
                records: &records,
                details: &details,
                rejected,
                trace: &trace,
            },
            false,
        )
        .await
        .unwrap()
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn reread(pool: &PgPool, ws: WorkspaceId, import: &Import) -> Import {
        find_import(pool, ws, import.id).await.unwrap().unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_file_round_trips_through_its_chunks_with_its_hash(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let bytes: Vec<u8> = (0..(CHUNK_BYTES * 5 / 2))
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();

        let import = upload(&pool, ws, &bytes, 0).await;

        let mut stored = Vec::new();
        let mut seq = 0;
        while let Some(chunk) = read_import_chunk(&pool, import.id, seq).await.unwrap() {
            stored.extend_from_slice(&chunk);
            seq += 1;
        }
        assert_eq!(seq, 3);
        assert_eq!(stored, bytes);
        assert_eq!(
            import.file_sha256.as_slice(),
            Sha256::digest(&bytes).as_slice()
        );
        assert_eq!(import.size_bytes, i64::try_from(bytes.len()).unwrap());
        assert_eq!(import.status, ImportStatus::Uploaded);
        assert_eq!(import.header, ["email"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_that_is_dropped_leaves_nothing_behind(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let trace = trace();
        let mut upload = ImportUpload::begin(&pool, ws, &start(&trace))
            .await
            .unwrap();
        upload.write(&vec![b'a'; CHUNK_BYTES + 5]).await.unwrap();
        assert_eq!(count(&pool, "import_chunks").await, 1);

        upload.abort().await;

        assert_eq!(count(&pool, "import_chunks").await, 0);
        assert_eq!(count(&pool, "imports").await, 0);
    }

    fn start(trace: &TraceContext) -> UploadStart<'_> {
        UploadStart {
            name: "people",
            source: "csv",
            stream: "people",
            file_name: "people.csv",
            created_by: None,
            trace,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_that_is_dropped_mid_body_deletes_its_import_and_chunks(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let trace = trace();
        let mut upload = ImportUpload::begin(&pool, ws, &start(&trace))
            .await
            .unwrap();
        upload.write(&vec![b'a'; CHUNK_BYTES * 2]).await.unwrap();
        assert_eq!(count(&pool, "import_chunks").await, 2);

        drop(upload);

        for _ in 0..50 {
            if count(&pool, "imports").await == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(count(&pool, "imports").await, 0);
        assert_eq!(count(&pool, "import_chunks").await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn mid_upload_the_import_is_visible_as_uploading_hidden_from_the_history_with_no_open_transaction(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let trace = trace();
        let mut upload = ImportUpload::begin(&pool, ws, &start(&trace))
            .await
            .unwrap();
        upload.write(&vec![b'a'; CHUNK_BYTES + 10]).await.unwrap();

        // Seen from other connections, between two chunks.
        let seen = find_import(&pool, ws, upload.id()).await.unwrap().unwrap();
        let history = list_imports(&pool, ws, None, 10).await.unwrap().0;
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid() \
               AND state IN ('idle in transaction', 'idle in transaction (aborted)')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(seen.status, ImportStatus::Uploading);
        assert!(history.is_empty());
        assert_eq!(open, 0, "no transaction stays open between chunks");
        assert_eq!(count(&pool, "import_chunks").await, 1);
        let header = vec!["email".to_owned()];
        let (finished, tx) = upload
            .finish(&UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: 0,
            })
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        assert_eq!(finished.status, ImportStatus::Uploaded);
        assert_eq!(list_imports(&pool, ws, None, 10).await.unwrap().0.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_empty_file_has_no_chunks_and_a_hash(pool: PgPool) {
        let ws = workspace(&pool, "one").await;

        let import = upload(&pool, ws, b"", 0).await;

        assert_eq!(count(&pool, "import_chunks").await, 0);
        assert_eq!(
            import.file_sha256.as_slice(),
            Sha256::digest(b"").as_slice()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_states_follow_the_life_of_an_import(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = upload(&pool, ws, b"email\n", 0).await;
        let id = import.id;
        let mut tx = begin(&pool).await.unwrap();

        // No mapping yet: neither pass may start.
        for result in [
            start_import_dry_run(&mut tx, ws, id).await,
            start_import_run(&mut tx, ws, id).await,
        ] {
            assert!(matches!(
                result,
                Err(TransitionError::WrongState(ImportStatus::Uploaded))
            ));
        }
        let saved = set_import_mapping(&mut tx, ws, id, &mapping())
            .await
            .unwrap();
        assert_eq!(saved.status, ImportStatus::Mapped);
        assert_eq!(saved.mapping, Some(mapping()));
        let dry = start_import_dry_run(&mut tx, ws, id).await.unwrap();
        assert_eq!(dry.status, ImportStatus::DryRunning);
        assert!(matches!(
            start_import_run(&mut tx, ws, id).await,
            Err(TransitionError::WrongState(ImportStatus::DryRunning))
        ));
        assert!(matches!(
            set_import_mapping(&mut tx, ws, id, &mapping()).await,
            Err(TransitionError::WrongState(ImportStatus::DryRunning))
        ));
        let done = finish_import_dry_run(&mut tx, ws, id).await.unwrap();
        assert_eq!(done.status, ImportStatus::DryRunDone);
        let run = start_import_run(&mut tx, ws, id).await.unwrap();
        assert_eq!(run.status, ImportStatus::Running);
        assert!(run.started_at.is_some());
        let finished = finish_import_run(&mut tx, ws, id).await.unwrap();
        assert_eq!(finished.status, ImportStatus::Succeeded);
        assert!(finished.finished_at.is_some());
        assert!(matches!(
            cancel_import(&mut tx, ws, id).await,
            Err(TransitionError::WrongState(ImportStatus::Succeeded))
        ));
        assert!(matches!(
            cancel_import(&mut tx, ws, Uuid::now_v7()).await,
            Err(TransitionError::NotFound)
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_can_be_cancelled_before_it_is_run_and_a_failure_keeps_its_code(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let idle = upload(&pool, ws, b"email\n", 0).await;
        let busy = running(&pool, ws, 0).await;
        let mut tx = begin(&pool).await.unwrap();

        let cancelled = cancel_import(&mut tx, ws, idle.id).await.unwrap();
        let failed = fail_import(&mut tx, ws, busy.id, "list_not_found")
            .await
            .unwrap();

        assert_eq!(cancelled.status, ImportStatus::Cancelled);
        assert!(cancelled.finished_at.is_some());
        assert_eq!(failed.status, ImportStatus::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("list_not_found"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_history_lists_newest_first_in_pages(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(upload(&pool, ws, b"email\n", 0).await.id);
        }

        let (first, more) = list_imports(&pool, ws, None, 2).await.unwrap();
        let (second, more_after) = list_imports(&pool, ws, Some(first[1].id), 2).await.unwrap();

        assert!(more);
        assert!(!more_after);
        let listed: Vec<Uuid> = first.iter().chain(&second).map(|i| i.id).collect();
        ids.reverse();
        assert_eq!(listed, ids);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_applies_the_records_and_moves_the_checkpoint_together(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 5).await;

        let first = run_batch(&pool, &import, 0, 0..3, &[refused(3)], &[]).await;
        let second = run_batch(&pool, &import, 4, 4..5, &[], &[]).await;

        assert_eq!(
            first,
            BatchOutcome::Committed(BatchProgress {
                rows_done: 4,
                rows_rejected: 1,
                threshold_exceeded: false
            })
        );
        assert_eq!(
            second,
            BatchOutcome::Committed(BatchProgress {
                rows_done: 5,
                rows_rejected: 1,
                threshold_exceeded: false
            })
        );
        let now = reread(&pool, ws, &import).await;
        assert_eq!(
            (
                now.rows_done,
                now.rows_applied,
                now.rows_unchanged,
                now.rows_rejected
            ),
            (5, 4, 0, 1)
        );
        assert_eq!(count(&pool, "people").await, 4);
        assert_eq!(count(&pool, "import_rejects").await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_repeated_at_a_stale_checkpoint_writes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 4).await;
        run_batch(&pool, &import, 0, 0..2, &[], &[]).await;

        let again = run_batch(&pool, &import, 0, 0..2, &[], &[]).await;
        let ahead = run_batch(&pool, &import, 3, 2..4, &[], &[]).await;

        assert_eq!(again, BatchOutcome::Superseded);
        assert_eq!(ahead, BatchOutcome::Superseded);
        assert_eq!(reread(&pool, ws, &import).await.rows_done, 2);
        assert_eq!(count(&pool, "people").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancelled_import_stops_at_the_next_batch(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 4).await;
        run_batch(&pool, &import, 0, 0..2, &[], &[]).await;
        let mut tx = begin(&pool).await.unwrap();
        cancel_import(&mut tx, ws, import.id).await.unwrap();
        commit(tx).await.unwrap();

        let next = run_batch(&pool, &import, 2, 2..4, &[], &[]).await;

        assert_eq!(next, BatchOutcome::Superseded);
        assert_eq!(count(&pool, "people").await, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_repeated_record_is_unchanged_and_writes_no_new_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let first = running(&pool, ws, 3).await;
        run_batch(&pool, &first, 0, 0..3, &[], &[]).await;
        let source_records = count(&pool, "source_records").await;
        let again = running(&pool, ws, 3).await;

        run_batch(&pool, &again, 0, 0..3, &[], &[]).await;

        let now = reread(&pool, ws, &again).await;
        assert_eq!((now.rows_applied, now.rows_unchanged), (0, 3));
        assert_eq!(count(&pool, "people").await, 3);
        assert_eq!(count(&pool, "source_records").await, source_records);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_target_adds_the_batchs_people_as_import_members(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "News", "", None).await.unwrap();
        let import = running(&pool, ws, 3).await;

        run_batch(&pool, &import, 0, 0..3, &[], &[list.id]).await;
        let repeat = running(&pool, ws, 3).await;
        run_batch(&pool, &repeat, 0, 0..3, &[], &[list.id]).await;

        let sources: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT source FROM list_members WHERE list_id = $1")
                .bind(list.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(sources, ["import"]);
        let list = crate::find_list(&pool, ws, list.id).await.unwrap().unwrap();
        assert_eq!(list.member_count, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_list_that_vanished_writes_nothing_and_says_so(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 2).await;

        let outcome = run_batch(&pool, &import, 0, 0..2, &[], &[Uuid::now_v7()]).await;

        assert_eq!(outcome, BatchOutcome::ListMissing);
        assert_eq!(count(&pool, "people").await, 0);
        assert_eq!(reread(&pool, ws, &import).await.rows_done, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reject_threshold_fails_the_import_after_a_thousand_rows(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 3000).await;
        let rejected: Vec<RejectedRow> = (0..100).map(|n| refused(1000 + n)).collect();
        let first = run_batch(&pool, &import, 0, 0..1000, &[], &[]).await;

        // 100 of 1,100 is over 5%.
        let second = run_batch(&pool, &import, 1000, 1000..1000, &rejected, &[]).await;

        assert!(matches!(
            first,
            BatchOutcome::Committed(BatchProgress {
                threshold_exceeded: false,
                ..
            })
        ));
        assert_eq!(
            second,
            BatchOutcome::Committed(BatchProgress {
                rows_done: 1100,
                rows_rejected: 100,
                threshold_exceeded: true
            })
        );
        let now = reread(&pool, ws, &import).await;
        assert_eq!(now.status, ImportStatus::Failed);
        assert_eq!(now.error_code.as_deref(), Some("reject_threshold"));
        assert_eq!(
            count(&pool, "people").await,
            1000,
            "applied batches stay applied"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_few_rejects_in_a_small_file_never_fail_it(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 10).await;
        let rejected: Vec<RejectedRow> = (0..5).map(refused).collect();

        let outcome = run_batch(&pool, &import, 0, 5..10, &rejected, &[]).await;

        assert!(matches!(
            outcome,
            BatchOutcome::Committed(BatchProgress {
                threshold_exceeded: false,
                ..
            })
        ));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_poison_record_becomes_a_reject_when_applied_singly(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 3).await;
        for statement in [
            "CREATE FUNCTION poison() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
               IF NEW.record_key = 'k1' THEN RAISE EXCEPTION 'poison'; END IF; \
               RETURN NEW; END $$",
            "CREATE TRIGGER poison BEFORE INSERT ON source_records \
             FOR EACH ROW EXECUTE FUNCTION poison()",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        let records: Vec<ProfileRecord> = (0..3).map(record).collect();
        let details: Vec<RowDetail> = (0..3).map(|n| detail(n, vec![])).collect();
        let trace = trace();
        let batch = ImportBatch {
            import_id: import.id,
            workspace: ws,
            expected_done: 0,
            records: &records,
            details: &details,
            rejected: &[],
            trace: &trace,
        };

        assert!(commit_import_batch(&pool, &batch, false).await.is_err());
        let outcome = commit_import_batch(&pool, &batch, true).await.unwrap();

        assert_eq!(
            outcome,
            BatchOutcome::Committed(BatchProgress {
                rows_done: 3,
                rows_rejected: 1,
                threshold_exceeded: false
            })
        );
        assert_eq!(count(&pool, "people").await, 2);
        let (rejects, _) = list_import_rejects(&pool, ws, import.id, None, None, 10, false)
            .await
            .unwrap();
        assert_eq!(rejects.len(), 1);
        assert_eq!(rejects[0].row_number, 3);
        assert_eq!(rejects[0].errors[0].reason, "apply_failed");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dry_run_batch_writes_only_rejects_and_counters(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = dry_running(&pool, ws, 6).await;
        let id = import.id;
        let rejected = [refused(1), refused(2)];
        let batch = DryRunBatch {
            import_id: id,
            workspace: ws,
            expected_done: 0,
            accepted: 4,
            rejected: &rejected,
        };

        let kept = commit_dry_run_batch(&pool, &batch).await.unwrap();
        let stale = commit_dry_run_batch(&pool, &batch).await.unwrap();

        assert!(kept);
        assert!(!stale);
        let now = reread(&pool, ws, &import).await;
        assert_eq!(
            (now.rows_done, now.rows_applied, now.rows_rejected),
            (6, 4, 2)
        );
        for table in ["people", "source_records", "identities"] {
            assert_eq!(count(&pool, table).await, 0, "{table}");
        }
        let (rejects, _) =
            list_import_rejects(&pool, ws, id, Some(ImportPhase::DryRun), None, 10, false)
                .await
                .unwrap();
        assert_eq!(rejects.len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rejects_hold_codes_and_show_the_cells_only_when_asked(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = running(&pool, ws, 5).await;
        let rejected = [refused(1), refused(2), refused(3)];
        run_batch(&pool, &import, 0, 0..0, &rejected, &[]).await;

        let (plain, more) = list_import_rejects(&pool, ws, import.id, None, None, 2, false)
            .await
            .unwrap();
        let (with_cells, _) = list_import_rejects(&pool, ws, import.id, None, Some(3), 2, true)
            .await
            .unwrap();

        assert!(more);
        assert_eq!(
            plain.iter().map(|r| r.row_number).collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(plain.iter().all(|r| r.raw.is_none()));
        assert_eq!(plain[0].errors[0].column, "email");
        assert_eq!(plain[0].errors[0].reason, "invalid_identifier");
        assert_eq!(with_cells.len(), 2);
        assert_eq!(
            with_cells[0].raw.as_deref(),
            Some(&["secret-cell".to_owned()][..])
        );
        let stored: String = sqlx::query_scalar("SELECT errors::text FROM import_rejects LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!stored.contains("secret"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn starting_a_run_clears_the_dry_runs_rejects(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = dry_running(&pool, ws, 2).await;
        let id = import.id;
        commit_dry_run_batch(
            &pool,
            &DryRunBatch {
                import_id: id,
                workspace: ws,
                expected_done: 0,
                accepted: 1,
                rejected: &[refused(1)],
            },
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        finish_import_dry_run(&mut tx, ws, id).await.unwrap();
        commit(tx).await.unwrap();
        assert_eq!(count(&pool, "import_rejects").await, 1);

        let mut tx = begin(&pool).await.unwrap();
        let started = start_import_run(&mut tx, ws, id).await.unwrap();
        commit(tx).await.unwrap();

        assert_eq!(count(&pool, "import_rejects").await, 0);
        assert_eq!((started.rows_done, started.rows_rejected), (0, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn retention_deletes_files_a_week_after_the_end_and_rejects_after_a_month(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let ended = upload(&pool, ws, b"email\n", 0).await;
        let recent = upload(&pool, ws, b"email\n", 0).await;
        let open = upload(&pool, ws, b"email\n", 0).await;
        let busy = running(&pool, ws, 3).await;
        run_batch(&pool, &busy, 0, 0..0, &[refused(1), refused(2)], &[]).await;
        sqlx::query(
            "UPDATE imports SET status = 'succeeded', \
                finished_at = CASE WHEN id = $1 THEN now() - interval '8 days' \
                                   ELSE now() - interval '6 days' END \
             WHERE id = ANY($2)",
        )
        .bind(ended.id)
        .bind(vec![ended.id, recent.id])
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE import_rejects SET created_at = now() - interval '31 days' \
             WHERE row_number = 3",
        )
        .execute(&pool)
        .await
        .unwrap();

        let report = delete_expired_import_data(&pool).await.unwrap();

        assert_eq!(
            report,
            ImportRetention {
                chunks: 1,
                rejects: 1,
                ..ImportRetention::default()
            }
        );
        let left: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT import_id FROM import_chunks")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(!left.contains(&ended.id));
        assert!(left.contains(&recent.id) && left.contains(&open.id));
        assert_eq!(count(&pool, "import_rejects").await, 1);
        assert_eq!(count(&pool, "imports").await, 4, "the status is kept");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_import_nobody_touched_for_a_week_is_cancelled_as_abandoned_and_loses_its_file(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let stale_uploaded = upload(&pool, ws, b"email\n", 0).await;
        let stale_mapped = mapped(&pool, ws, b"email\n", 0).await;
        let fresh = mapped(&pool, ws, b"email\n", 0).await;
        let busy = running(&pool, ws, 0).await;
        sqlx::query("UPDATE imports SET updated_at = now() - interval '8 days' WHERE id = ANY($1)")
            .bind(vec![stale_uploaded.id, stale_mapped.id, busy.id])
            .execute(&pool)
            .await
            .unwrap();

        let report = delete_expired_import_data(&pool).await.unwrap();

        assert_eq!((report.abandoned, report.chunks), (2, 2));
        for id in [stale_uploaded.id, stale_mapped.id] {
            let import = find_import(&pool, ws, id).await.unwrap().unwrap();
            assert_eq!(import.status, ImportStatus::Cancelled);
            assert_eq!(import.error_code.as_deref(), Some("abandoned"));
            assert!(import.finished_at.is_some());
            assert!(read_import_chunk(&pool, id, 0).await.unwrap().is_none());
        }
        let kept = find_import(&pool, ws, fresh.id).await.unwrap().unwrap();
        assert_eq!(kept.status, ImportStatus::Mapped);
        assert!(
            read_import_chunk(&pool, fresh.id, 0)
                .await
                .unwrap()
                .is_some()
        );
        let running_still = find_import(&pool, ws, busy.id).await.unwrap().unwrap();
        assert_eq!(running_still.status, ImportStatus::Running);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_upload_untouched_for_a_day_is_deleted_with_its_chunks(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let trace = trace();
        let mut crashed = ImportUpload::begin(&pool, ws, &start(&trace))
            .await
            .unwrap();
        crashed.write(&vec![b'a'; CHUNK_BYTES]).await.unwrap();
        let mut current = ImportUpload::begin(&pool, ws, &start(&trace))
            .await
            .unwrap();
        current.write(&vec![b'b'; CHUNK_BYTES]).await.unwrap();
        sqlx::query("UPDATE imports SET created_at = now() - interval '2 days' WHERE id = $1")
            .bind(crashed.id())
            .execute(&pool)
            .await
            .unwrap();

        let report = delete_expired_import_data(&pool).await.unwrap();

        assert_eq!(report.uploads, 1);
        assert!(
            find_import(&pool, ws, crashed.id())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_import(&pool, ws, current.id())
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(count(&pool, "import_chunks").await, 1);
        // The upload in progress is not disturbed: it still finishes.
        let header = vec!["email".to_owned()];
        let (finished, tx) = current
            .finish(&UploadScan {
                delimiter: ',',
                header: &header,
                rows_total: 0,
            })
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        assert_eq!(finished.status, ImportStatus::Uploaded);
        drop(crashed);
    }
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_import_read_and_change_stays_inside_its_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let import = running(&pool, one, 3).await;
        run_batch(&pool, &import, 0, 0..0, &[refused(1)], &[]).await;
        let mut tx = begin(&pool).await.unwrap();

        assert!(find_import(&pool, two, import.id).await.unwrap().is_none());
        assert!(
            list_imports(&pool, two, None, 10)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(
            list_import_rejects(&pool, two, import.id, None, None, 10, true)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert!(matches!(
            cancel_import(&mut tx, two, import.id).await,
            Err(TransitionError::NotFound)
        ));
        assert!(matches!(
            set_import_mapping(&mut tx, two, import.id, &mapping()).await,
            Err(TransitionError::NotFound)
        ));
        let trace = trace();
        let foreign = commit_import_batch(
            &pool,
            &ImportBatch {
                import_id: import.id,
                workspace: two,
                expected_done: 0,
                records: &[],
                details: &[],
                rejected: &[],
                trace: &trace,
            },
            false,
        )
        .await
        .unwrap();
        assert_eq!(foreign, BatchOutcome::Superseded);
        let still = find_import(&pool, one, import.id).await.unwrap().unwrap();
        assert_eq!(still.status, ImportStatus::Running);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_import_tables_hold_no_person_id_column_for_the_erasure_scan(pool: PgPool) {
        let columns: Vec<String> = sqlx::query_scalar(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() \
               AND table_name IN ('imports', 'import_chunks', 'import_rejects') \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id' \
                    OR column_name LIKE '%person%')",
        )
        .fetch_all(&pool)
        .await
        .unwrap();

        assert!(columns.is_empty(), "{columns:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_member_added_outside_an_import_keeps_its_source(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let list = create_list(&pool, ws, "News", "", None).await.unwrap();
        let import = running(&pool, ws, 2).await;
        run_batch(&pool, &import, 0, 0..1, &[], &[]).await;
        let person: Uuid = sqlx::query_scalar("SELECT id FROM people LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        add_members(
            &mut tx,
            ws,
            list.id,
            &[person],
            ListSource::Ui,
            None,
            &trace(),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();

        run_batch(&pool, &import, 1, 0..1, &[], &[list.id]).await;

        let sources: Vec<String> =
            sqlx::query_scalar("SELECT source FROM list_members WHERE list_id = $1")
                .bind(list.id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(sources, ["ui"]);
    }

    async fn stored_file(pool: &PgPool, ws: WorkspaceId, bytes: &[u8]) -> Import {
        upload(pool, ws, bytes, 1).await
    }

    async fn reject_with(pool: &PgPool, ws: WorkspaceId, import: Uuid, row: i64, cell: &str) {
        sqlx::query(
            "INSERT INTO import_rejects (import_id, row_number, workspace_id, phase, errors, raw) \
             VALUES ($1, $2, $3, 'run', '[]', $4)",
        )
        .bind(import)
        .bind(row)
        .bind(ws.as_uuid())
        .bind(json!({"email": cell}))
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_value_deletes_the_file_that_holds_it_and_keeps_the_others(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let hit = stored_file(&pool, ws, b"email\nZed.Quill@Example.com\n").await;
        let kept = stored_file(&pool, ws, b"email\ngrace@example.org\n").await;
        let elsewhere = stored_file(&pool, other, b"email\nzed.quill@example.com\n").await;
        let mut connection = pool.acquire().await.unwrap();

        let erased = erase_import_values(&mut connection, ws, &["zed.quill@example.com".into()])
            .await
            .unwrap();
        let blank = erase_import_values(&mut connection, ws, &[String::new()])
            .await
            .unwrap();

        assert_eq!(
            erased,
            ImportErasure {
                chunks: 1,
                rejects: 0
            }
        );
        assert_eq!(blank, ImportErasure::default());
        assert!(read_import_chunk(&pool, hit.id, 0).await.unwrap().is_none());
        for id in [kept.id, elsewhere.id] {
            assert!(read_import_chunk(&pool, id, 0).await.unwrap().is_some());
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_value_split_across_two_chunks_still_deletes_the_whole_file(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let import = stored_file(&pool, ws, b"email\nhead").await;
        sqlx::query(
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) VALUES ($1, 1, $2, $3)",
        )
        .bind(import.id)
        .bind(ws.as_uuid())
        .bind(b"er@example.com\n".as_slice())
        .execute(&pool)
        .await
        .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let erased = erase_import_values(&mut connection, ws, &["header@example.com".into()])
            .await
            .unwrap();

        assert_eq!(erased.chunks, 2);
        assert!(
            read_import_chunk(&pool, import.id, 1)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_value_deletes_the_rejected_rows_that_hold_it(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let import = stored_file(&pool, ws, b"email\n").await;
        let foreign = stored_file(&pool, other, b"email\n").await;
        reject_with(&pool, ws, import.id, 2, "Zed.Quill@Example.com").await;
        reject_with(&pool, ws, import.id, 3, "grace@example.org").await;
        reject_with(&pool, other, foreign.id, 2, "zed.quill@example.com").await;
        let mut connection = pool.acquire().await.unwrap();

        let erased = erase_import_values(&mut connection, ws, &["zed.quill@example.com".into()])
            .await
            .unwrap();

        assert_eq!(erased.rejects, 1);
        let mut left: Vec<(Uuid, i64)> =
            sqlx::query_as("SELECT import_id, row_number FROM import_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        left.sort();
        let mut expected = vec![(import.id, 3), (foreign.id, 2)];
        expected.sort();
        assert_eq!(left, expected);
    }

    async fn second_chunk(pool: &PgPool, ws: WorkspaceId, import: Uuid, data: &[u8]) {
        sqlx::query(
            "INSERT INTO import_chunks (import_id, seq, workspace_id, data) VALUES ($1, 1, $2, $3)",
        )
        .bind(import)
        .bind(ws.as_uuid())
        .bind(data)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_short_id_deletes_only_the_whole_field_and_an_email_in_a_notes_cell(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let hit = stored_file(&pool, ws, b"id,notes\n42,hello\n").await;
        let quoted = stored_file(&pool, ws, b"id\n\"42\"\n").await;
        let email = stored_file(&pool, ws, b"id,notes\n9,\"mail Zed@Example.com now\"\n").await;
        let bystanders = [
            stored_file(&pool, ws, b"id\n1427\n").await,
            stored_file(&pool, ws, b"id,notes\n7,x42y\n").await,
            stored_file(&pool, ws, b"at\n2026-10-27T11:42\n").await,
        ];
        let kept_reject = stored_file(&pool, ws, b"id\n").await;
        reject_with(&pool, ws, kept_reject.id, 2, "1427").await;
        reject_with(&pool, ws, kept_reject.id, 3, "x42y").await;
        reject_with(&pool, ws, kept_reject.id, 4, "42").await;
        let mut connection = pool.acquire().await.unwrap();

        let erased = erase_import_values(
            &mut connection,
            ws,
            &["42".into(), "zed@example.com".into()],
        )
        .await
        .unwrap();

        assert_eq!(
            erased,
            ImportErasure {
                chunks: 3,
                rejects: 1
            }
        );
        for gone in [hit.id, quoted.id, email.id] {
            assert!(read_import_chunk(&pool, gone, 0).await.unwrap().is_none());
        }
        for kept in bystanders
            .iter()
            .map(|import| import.id)
            .chain([kept_reject.id])
        {
            assert!(read_import_chunk(&pool, kept, 0).await.unwrap().is_some());
        }
        let rows: Vec<i64> =
            sqlx::query_scalar("SELECT row_number FROM import_rejects ORDER BY row_number")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, [2, 3]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_field_cut_by_the_chunk_boundary_matches_as_a_whole_not_as_a_part(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let whole = stored_file(&pool, ws, b"id\n4").await;
        second_chunk(&pool, ws, whole.id, b"2\n").await;
        let part = stored_file(&pool, ws, b"id\nx4").await;
        second_chunk(&pool, ws, part.id, b"2\n").await;
        let mut connection = pool.acquire().await.unwrap();

        let erased = erase_import_values(&mut connection, ws, &["42".into()])
            .await
            .unwrap();

        assert_eq!(erased.chunks, 2);
        assert!(
            read_import_chunk(&pool, whole.id, 0)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            read_import_chunk(&pool, part.id, 0)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn erasing_a_file_fails_the_imports_that_have_not_ended_with_erased(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let running = stored_file(&pool, ws, b"id\n42\n").await;
        let done = stored_file(&pool, ws, b"id\n42\n").await;
        let bystander = stored_file(&pool, ws, b"id\n1427\n").await;
        for (id, status) in [
            (running.id, "running"),
            (done.id, "succeeded"),
            (bystander.id, "running"),
        ] {
            sqlx::query("UPDATE imports SET status = $2 WHERE id = $1")
                .bind(id)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
        let mut connection = pool.acquire().await.unwrap();

        erase_import_values(&mut connection, ws, &["42".into()])
            .await
            .unwrap();

        let states: Vec<(Uuid, String, Option<String>)> =
            sqlx::query_as("SELECT id, status, error_code FROM imports")
                .fetch_all(&pool)
                .await
                .unwrap();
        let of = |id: Uuid| states.iter().find(|row| row.0 == id).cloned().unwrap();
        assert_eq!(of(running.id).1, "failed");
        assert_eq!(of(running.id).2.as_deref(), Some("erased"));
        assert_eq!(of(done.id).1, "succeeded");
        assert_eq!(of(bystander.id).1, "running");
    }
}
