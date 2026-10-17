//! The sweep of a sync run (architecture section 7.2): once a full-refresh or reconciliation run
//! has read its whole stream, the live records of that stream the run did not see are removed.
//! A unit is one bounded transaction, fenced on the job's lease and on the run's state exactly as
//! a batch is, so a lost lease, a cancel or a crash never leaves a half-removed unit and a
//! resumed run removes each record once.
//!
//! The guard: before the first unit the run counts what it would remove against what is live,
//! and a sweep over the stream's limit removes nothing (see [`sweep_exceeds_limit`]).
//!
//! Nothing here holds or logs a record value, an identifier or a trait value.

use sqlx::PgExecutor;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::batch::{DELETE_BY_SWEEP, Unfenced, fence_run};
use crate::people::remove_contribution_in;
use crate::{NewDecision, PgPool, StoreError, append_decisions, begin, commit};

/// The most records one sweep unit removes.
pub const SYNC_SWEEP_BATCH: i64 = 200;

/// One unit of a sweep.
pub struct SweepUnit<'a> {
    pub workspace: WorkspaceId,
    pub run_id: Uuid,
    /// The `source_records` keys of the run: the source id as text, and the stream name.
    pub source: &'a str,
    pub stream: &'a str,
    /// The job and lease the unit is fenced on, and the attempt for the decisions.
    pub job_id: Uuid,
    pub lease_token: Uuid,
    pub attempt: i32,
    pub trace: &'a TraceContext,
    /// Where the previous unit ended: only records after this id are looked at.
    pub after: Option<Uuid>,
}

/// What became of a unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SweepOutcome {
    Swept {
        /// Records this unit removed.
        removed: i64,
        /// The last record the unit looked at; pass it as `after` to the next unit.
        last: Option<Uuid>,
        /// The unit was full, so another may find more.
        more: bool,
    },
    /// The job's lease is gone: another worker owns the run now. Nothing was written.
    LeaseLost,
    /// The run is no longer running (cancelled, or ended): nothing was written.
    Cancelled,
}

/// Removes up to [`SYNC_SWEEP_BATCH`] live records of the stream that the run did not see, in one
/// transaction: each through `remove_contribution_in`, with a `deleted` decision under the run's
/// trace, and the run's `records_deleted` counter. Records already removed are not looked at
/// again, so a re-run of a partly done sweep removes each once.
pub async fn sweep_unreached(
    pool: &PgPool,
    unit: &SweepUnit<'_>,
) -> Result<SweepOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    match fence_run(
        &mut tx,
        unit.workspace,
        unit.run_id,
        unit.job_id,
        unit.lease_token,
    )
    .await?
    {
        Some(Unfenced::LeaseLost) => return Ok(SweepOutcome::LeaseLost),
        Some(Unfenced::Cancelled) => return Ok(SweepOutcome::Cancelled),
        None => {}
    }

    let unreached: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, record_key FROM source_records \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND deleted_at IS NULL \
           AND last_seen_run IS DISTINCT FROM $4 AND ($5::uuid IS NULL OR id > $5) \
         ORDER BY id LIMIT $6",
    )
    .bind(unit.workspace.as_uuid())
    .bind(unit.source)
    .bind(unit.stream)
    .bind(unit.run_id)
    .bind(unit.after)
    .bind(SYNC_SWEEP_BATCH)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::query)?;

    let mut decisions = Vec::new();
    for (_, key) in &unreached {
        let removed = remove_contribution_in(
            &mut tx,
            unit.workspace,
            unit.source,
            unit.stream,
            key,
            unit.trace,
        )
        .await?;
        if let Some(removed) = removed {
            decisions.push(NewDecision {
                workspace_id: unit.workspace,
                trace: unit.trace.clone(),
                action: "sync.record",
                outcome: "deleted",
                reason: Some(DELETE_BY_SWEEP),
                subject_ids: vec![("source_record", removed.record_id)],
                job: Some((unit.job_id, unit.attempt)),
            });
        }
    }
    let removed = i64::try_from(decisions.len()).unwrap_or(i64::MAX);
    append_decisions(&mut tx, &decisions).await?;
    if removed > 0 {
        sqlx::query(
            "UPDATE sync_runs SET records_deleted = records_deleted + $3, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(unit.workspace.as_uuid())
        .bind(unit.run_id)
        .bind(removed)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
        crate::changes::request_reevaluation(&mut tx, unit.workspace).await?;
    }
    commit(tx).await?;
    Ok(SweepOutcome::Swept {
        removed,
        last: unreached.last().map(|(id, _)| *id),
        more: i64::try_from(unreached.len()).unwrap_or(i64::MAX) >= SYNC_SWEEP_BATCH,
    })
}

/// Whether sweeping would remove more than `limit` of the stream's live records that existed
/// before the run: the share of them the run did not see is over the limit. Records the run
/// itself inserted are not counted, or a changed primary key (every record re-keyed, so as many
/// new rows as old ones) would halve the share and pass. A stream with no earlier live record is
/// never over. A read that returned nothing, a partial read and a changed primary key all leave
/// most earlier records unseen, so the guard stops the sweep before it deletes people whose state
/// a later read cannot bring back. The share equal to the limit sweeps.
pub async fn sweep_exceeds_limit<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    run_id: Uuid,
    source: &str,
    stream: &str,
    limit: f64,
) -> Result<bool, StoreError> {
    let (earlier, unseen): (i64, i64) = sqlx::query_as(
        "SELECT count(*), count(*) FILTER (WHERE last_seen_run IS DISTINCT FROM $4) \
         FROM source_records \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND deleted_at IS NULL \
           AND created_at < (SELECT created_at FROM sync_runs WHERE workspace_id = $1 AND id = $4)",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .bind(stream)
    .bind(run_id)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(earlier > 0 && (unseen as f64) / (earlier as f64) > limit)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use zirv_flow_core::OrphanPolicy;

    use super::super::batch::{BatchOutcome, SyncBatch, SyncReject, commit_sync_batch};
    use super::super::model::{cancel_run, find_run};
    use super::super::testing::{
        Fixture, batch, commit_records, fixture, next_run, started_run, upsert, workspace,
    };
    use super::*;
    use crate::people::apply_profile_batch;

    fn unit<'a>(fx: &'a Fixture, source: &'a str, after: Option<Uuid>) -> SweepUnit<'a> {
        SweepUnit {
            workspace: fx.ws,
            run_id: fx.run.id,
            source,
            stream: "contacts",
            job_id: fx.job.id,
            lease_token: fx.job.lease_token,
            attempt: fx.job.attempts,
            trace: &fx.trace,
            after,
        }
    }

    async fn live_keys(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT record_key FROM source_records WHERE deleted_at IS NULL ORDER BY record_key",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    async fn deleted_of(pool: &PgPool, fx: &Fixture) -> i64 {
        find_run(pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap()
            .records_deleted
    }

    /// Run 1 applied `n` records; run 2 is started and has seen none of them.
    async fn second_run(pool: &PgPool, n: usize) -> Fixture {
        let first = fixture(pool, "one", "full_refresh").await;
        commit_records(pool, &first, n).await;
        next_run(pool, &first).await
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_removes_the_unseen_live_records_and_only_those(pool: PgPool) {
        let fx = second_run(&pool, 3).await;
        let source = fx.source_id.to_string();
        // Run 2 reads k0 again.
        let seen = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &seen, &cp))
            .await
            .unwrap();
        assert!(matches!(outcome, BatchOutcome::Committed(_)));

        let swept = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        let SweepOutcome::Swept {
            removed,
            last,
            more,
        } = swept
        else {
            panic!("{swept:?}");
        };
        assert_eq!((removed, more), (2, false));
        assert!(last.is_some());
        assert_eq!(live_keys(&pool).await, ["k0"]);
        let tombstones = count(
            &pool,
            "SELECT count(*) FROM source_records WHERE deleted_at IS NOT NULL",
        )
        .await;
        assert_eq!(tombstones, 2, "the rows are tombstoned, not deleted");
        assert_eq!(deleted_of(&pool, &fx).await, 2);
        // The people stay under the default `keep` policy.
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_removal_is_a_deleted_decision_and_a_contribution_removed_under_the_run_trace(
        pool: PgPool,
    ) {
        let fx = second_run(&pool, 2).await;
        let source = fx.source_id.to_string();

        sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        type Row = (
            Uuid,
            String,
            String,
            String,
            String,
            Option<String>,
            Value,
            Option<Uuid>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, action, outcome, reason, subject_ids, job_id \
             FROM decision_records \
             WHERE outcome IN ('deleted', 'removed') ORDER BY recorded_at, id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let swept: Vec<&Row> = rows.iter().filter(|row| row.4 == "deleted").collect();
        assert_eq!(swept.len(), 2);
        for row in &swept {
            assert_eq!(
                (row.3.as_str(), row.5.as_deref()),
                ("sync.record", Some("swept"))
            );
            // The subject is the ledger row only: the person may go with it.
            assert_eq!(row.6.as_object().unwrap().len(), 1);
            assert!(row.6["source_record"].is_string());
            assert_eq!(row.7, Some(fx.job.id));
        }
        let removed: Vec<&Row> = rows.iter().filter(|row| row.4 == "removed").collect();
        assert_eq!(removed.len(), 2);
        for row in swept.iter().chain(&removed) {
            assert_eq!(row.0, fx.run.trace_id);
            assert_eq!(row.1, "sync_run");
            assert_eq!(row.2, fx.run.id.to_string());
        }
        let shown = format!("{rows:?}");
        assert!(!shown.contains("example.com"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_is_bounded_and_its_keyset_resumes_where_the_last_unit_ended(pool: PgPool) {
        let fx = second_run(&pool, 450).await;
        let source = fx.source_id.to_string();

        // A keyset past every id finds nothing, whatever is unseen.
        let beyond = sweep_unreached(&pool, &unit(&fx, &source, Some(Uuid::max())))
            .await
            .unwrap();
        assert_eq!(
            beyond,
            SweepOutcome::Swept {
                removed: 0,
                last: None,
                more: false
            }
        );

        let mut after = None;
        let mut sizes = Vec::new();
        loop {
            let outcome = sweep_unreached(&pool, &unit(&fx, &source, after))
                .await
                .unwrap();
            let SweepOutcome::Swept {
                removed,
                last,
                more,
            } = outcome
            else {
                panic!("{outcome:?}");
            };
            sizes.push((removed, more));
            if !more {
                break;
            }
            after = last;
        }

        assert_eq!(sizes, [(200, true), (200, true), (50, false)]);
        assert!(live_keys(&pool).await.is_empty());
        assert_eq!(deleted_of(&pool, &fx).await, 450);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_run_again_after_a_partial_one_removes_each_record_once(pool: PgPool) {
        let fx = second_run(&pool, 250).await;
        let source = fx.source_id.to_string();
        // The first attempt does one unit and dies.
        sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();
        assert_eq!(live_keys(&pool).await.len(), 50);

        // The next attempt starts from the beginning and finds only what is left.
        let outcome = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        let SweepOutcome::Swept { removed, more, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((removed, more), (50, false));
        assert_eq!(deleted_of(&pool, &fx).await, 250);
        let deleted = count(
            &pool,
            "SELECT count(*) FROM decision_records WHERE outcome = 'deleted'",
        )
        .await;
        let retracted = count(
            &pool,
            "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed'",
        )
        .await;
        assert_eq!((deleted, retracted), (250, 250));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_whose_lease_was_taken_over_sweeps_nothing(pool: PgPool) {
        let fx = second_run(&pool, 3).await;
        let source = fx.source_id.to_string();
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        assert_eq!(outcome, SweepOutcome::LeaseLost);
        assert_eq!(live_keys(&pool).await.len(), 3);
        assert_eq!(deleted_of(&pool, &fx).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_for_a_job_that_is_gone_sweeps_nothing(pool: PgPool) {
        let fx = second_run(&pool, 3).await;
        let source = fx.source_id.to_string();
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        assert_eq!(outcome, SweepOutcome::LeaseLost);
        assert_eq!(live_keys(&pool).await.len(), 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_that_is_cancelled_or_failed_sweeps_nothing(pool: PgPool) {
        let fx = second_run(&pool, 3).await;
        let source = fx.source_id.to_string();
        let mut connection = pool.acquire().await.unwrap();
        cancel_run(&mut connection, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap();
        drop(connection);

        let cancelled = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        assert_eq!(cancelled, SweepOutcome::Cancelled);
        assert_eq!(live_keys(&pool).await.len(), 3);

        // A run that failed (the reject threshold, say) takes no sweep either.
        let next = started_run(&pool, fx.ws, fx.source_id, fx.stream_id).await;
        sqlx::query("UPDATE sync_runs SET state = 'failed' WHERE id = $1")
            .bind(next.run.id)
            .execute(&pool)
            .await
            .unwrap();
        let failed = sweep_unreached(&pool, &unit(&next, &source, None))
            .await
            .unwrap();
        assert_eq!(failed, SweepOutcome::Cancelled);
        assert_eq!(live_keys(&pool).await.len(), 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_record_the_run_read_but_could_not_use_is_seen_and_not_swept(pool: PgPool) {
        let fx = second_run(&pool, 6).await;
        let source = fx.source_id.to_string();
        // Run 2 re-reads k0 (unchanged); the store refuses k1 (a bad email) and the mapping
        // rejects k2; k3 is only listed (a reconciliation reads keys); k4 and k5 are not read.
        let upserts = [
            upsert(&fx, "k0", "a0@example.com"),
            upsert(&fx, "k1", "not-an-email"),
        ];
        let rejected = [SyncReject {
            record_key: "k2".into(),
            reasons: vec![("email".into(), "invalid_identifier")],
            sample: json!({"properties": {}, "masked": []}),
        }];
        let listed = ["k3".to_owned()];
        let cp = json!({"offset": 4});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejected,
                seen: &listed,
                singly: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(progress.batch.read, 4);
        assert_eq!(progress.batch.rejected, 2);

        let outcome = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        let SweepOutcome::Swept { removed, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(removed, 2);
        assert_eq!(live_keys(&pool).await, ["k0", "k1", "k2", "k3"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn other_streams_sources_and_workspaces_are_not_swept(pool: PgPool) {
        let fx = second_run(&pool, 2).await;
        let source = fx.source_id.to_string();
        let other_ws = workspace(&pool, "two").await;
        for (ws, record_source, stream, key) in [
            (fx.ws, source.as_str(), "other_stream", "n0"),
            (fx.ws, "another-source", "contacts", "n1"),
            (other_ws, source.as_str(), "contacts", "n2"),
        ] {
            let mut record = upsert(&fx, key, &format!("{key}@example.com")).record;
            record.source = record_source.to_owned();
            record.stream = stream.to_owned();
            apply_profile_batch(&pool, ws, &[record]).await.unwrap();
        }

        let outcome = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        let SweepOutcome::Swept { removed, .. } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(removed, 2);
        assert_eq!(live_keys(&pool).await, ["n0", "n1", "n2"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_orphan_policy_decides_the_person_of_the_last_swept_record(pool: PgPool) {
        let kept = second_run(&pool, 1).await;
        let source = kept.source_id.to_string();

        // Under `keep` (the default) the person stays.
        sweep_unreached(&pool, &unit(&kept, &source, None))
            .await
            .unwrap();
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 1);

        // Under `delete` the person goes with the record.
        let first = fixture(&pool, "del", "full_refresh").await;
        commit_records(&pool, &first, 1).await;
        let deleting = next_run(&pool, &first).await;
        crate::set_orphan_policy(&pool, deleting.ws, OrphanPolicy::Delete)
            .await
            .unwrap();
        let source = deleting.source_id.to_string();
        sweep_unreached(&pool, &unit(&deleting, &source, None))
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM people WHERE workspace_id = $1")
            .bind(deleting.ws.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(count(&pool, "SELECT count(*) FROM people").await, 1);
    }

    async fn over(pool: &PgPool, fx: &Fixture, limit: f64) -> bool {
        let source = fx.source_id.to_string();
        sweep_exceeds_limit(pool, fx.ws, fx.run.id, &source, "contacts", limit)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_limit_refuses_a_sweep_over_it_and_allows_one_at_or_under_it(pool: PgPool) {
        let fx = second_run(&pool, 4).await;

        // All four are unseen: 100% is over 50%, and a limit of 1 allows it.
        assert!(over(&pool, &fx, 0.5).await);
        assert!(!over(&pool, &fx, 1.0).await);

        // Two of four seen leaves 50% unseen: exactly at the limit sweeps, under it does not.
        sqlx::query(
            "UPDATE source_records SET last_seen_run = $1 WHERE record_key IN ('k0', 'k1')",
        )
        .bind(fx.run.id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(!over(&pool, &fx, 0.5).await);
        assert!(over(&pool, &fx, 0.49).await);

        // Three of four seen: 25% unseen.
        sqlx::query("UPDATE source_records SET last_seen_run = $1 WHERE record_key = 'k2'")
            .bind(fx.run.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!over(&pool, &fx, 0.5).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_changed_primary_key_is_measured_against_the_records_before_the_run(pool: PgPool) {
        let fx = second_run(&pool, 4).await;
        let source = fx.source_id.to_string();
        // The source returns the same four records under new keys: four live rows are inserted
        // and seen, and the four old ones stay unseen. Against all eight that is 50%; against the
        // four that existed before the run it is 100%.
        let upserts: Vec<_> = (0..4)
            .map(|i| upsert(&fx, &format!("n{i}"), &format!("a{i}@example.com")))
            .collect();
        let cp = json!({"offset": 4});
        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &upserts, &cp))
            .await
            .unwrap();
        assert!(matches!(outcome, BatchOutcome::Committed(_)));

        assert!(over(&pool, &fx, 0.5).await);
        assert!(!over(&pool, &fx, 1.0).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_with_no_live_record_is_never_over_the_limit(pool: PgPool) {
        let fx = second_run(&pool, 0).await;

        assert!(!over(&pool, &fx, 0.01).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_limit_counts_only_live_records_of_this_stream_and_workspace(pool: PgPool) {
        let fx = second_run(&pool, 4).await;
        let source = fx.source_id.to_string();
        // Three of four are seen, so 25% are unseen. Tombstoned and foreign rows must not change
        // that: a tombstone is not live, and another workspace's rows are not this stream's.
        sqlx::query("UPDATE source_records SET last_seen_run = $1 WHERE record_key <> 'k3'")
            .bind(fx.run.id)
            .execute(&pool)
            .await
            .unwrap();
        let other_ws = workspace(&pool, "two").await;
        for n in 0..10 {
            let mut record = upsert(&fx, &format!("f{n}"), &format!("f{n}@example.com")).record;
            record.source = source.clone();
            apply_profile_batch(&pool, other_ws, &[record])
                .await
                .unwrap();
        }
        let stale = fixture(&pool, "old", "full_refresh").await;
        commit_records(&pool, &stale, 5).await;
        sqlx::query("UPDATE source_records SET deleted_at = now() WHERE workspace_id = $1")
            .bind(stale.ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        assert!(!over(&pool, &fx, 0.3).await);
        assert!(over(&pool, &fx, 0.2).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_adds_nullable_defaulted_columns_and_an_event_index_on_every_partition(
        pool: PgPool,
    ) {
        // Nothing is backfilled: the columns are null or take their default.
        let fx = fixture(&pool, "one", "incremental").await;
        let (kind, limit, schedule): (String, f64, Option<String>) = sqlx::query_as(
            "SELECT r.run_kind, s.sweep_limit, s.reconcile_schedule \
             FROM sync_runs r JOIN streams s ON s.id = r.stream_id WHERE r.id = $1",
        )
        .bind(fx.run.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((kind.as_str(), limit, schedule), ("sync", 0.5, None));

        // The index is on the partitioned parent, so a partition made later gets it too.
        crate::ensure_event_partitions(
            &pool,
            time::Date::from_calendar_date(2031, time::Month::March, 1).unwrap(),
        )
        .await
        .unwrap();
        let (partitions, indexed): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM pg_inherits WHERE inhparent = 'events'::regclass), \
                    (SELECT count(*) FROM pg_index i \
                     JOIN pg_inherits h ON h.inhrelid = i.indrelid \
                     JOIN pg_class ic ON ic.oid = i.indexrelid \
                     WHERE h.inhparent = 'events'::regclass \
                       AND ic.relname LIKE '%source_record_id%')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(partitions > 2);
        assert_eq!(partitions, indexed, "every partition has the record index");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sweep_that_removes_records_requests_the_reevaluation_it_marked(pool: PgPool) {
        let fx = second_run(&pool, 2).await;
        let source = fx.source_id.to_string();
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM jobs WHERE kind = 'segment.reevaluate'")
            .execute(&pool)
            .await
            .unwrap();

        let swept = sweep_unreached(&pool, &unit(&fx, &source, None))
            .await
            .unwrap();

        assert!(matches!(swept, SweepOutcome::Swept { removed: 2, .. }));
        assert!(count(&pool, "SELECT count(*) FROM person_changes").await > 0);
        let jobs = count(
            &pool,
            "SELECT count(*) FROM jobs WHERE kind = 'segment.reevaluate'",
        )
        .await;
        assert_eq!(jobs, 1);
    }
}
