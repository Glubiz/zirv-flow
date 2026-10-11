//! CSV imports (architecture sections 7.1, 7.2 and 16.3): the uploaded file kept in chunks,
//! the mapping, the state changes the API makes, the batches the jobs commit, the refused rows
//! and their retention.

mod batch;
mod model;
mod rejects;
mod retention;
mod upload;

pub use batch::{
    BatchOutcome, BatchProgress, DryRunBatch, ImportBatch, REJECT_CHECK_MIN_ROWS, RowDetail,
    commit_dry_run_batch, commit_import_batch,
};
pub use model::{
    Import, ImportStatus, TransitionError, cancel_import, fail_import, find_import,
    finish_import_dry_run, finish_import_run, list_imports, set_import_mapping,
    start_import_dry_run, start_import_run,
};
pub use rejects::{
    ImportPhase, ImportReject, RejectReason, RejectedRow, StoredReason, list_import_rejects,
};
pub use retention::{
    CHUNK_RETENTION_DAYS, ImportRetention, REJECT_RETENTION_DAYS, delete_expired_import_data,
};
pub use upload::{CHUNK_BYTES, ImportUpload, NewImport, read_import_chunk};

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
            key_column: None,
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
        let mut upload = ImportUpload::begin(pool, ws).await.unwrap();
        upload.write(bytes).await.unwrap();
        let import = upload
            .insert(&NewImport {
                name: "people",
                source: "csv",
                stream: "people",
                file_name: "people.csv",
                delimiter: ',',
                header: &header,
                rows_total: rows,
                created_by: None,
                trace: &trace,
            })
            .await
            .unwrap();
        upload.commit().await.unwrap();
        import
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
        let mut upload = ImportUpload::begin(&pool, ws).await.unwrap();
        upload.write(&vec![b'a'; CHUNK_BYTES + 5]).await.unwrap();
        drop(upload);

        assert_eq!(count(&pool, "import_chunks").await, 0);
        assert_eq!(count(&pool, "imports").await, 0);
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
                rejects: 1
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
}
