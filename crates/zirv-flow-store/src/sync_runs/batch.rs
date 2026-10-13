//! One batch of a sync run, in ONE transaction (architecture sections 7.1 and 7.2): the profile
//! records, the retracted contributions, the rejects, the decisions, the counters and the
//! checkpoint commit together or not at all, fenced on the job's lease and on the run's state.
//! A crash, a lost lease or a cancel therefore never leaves a half-applied batch, and a resumed
//! run neither skips nor re-applies a committed one.
//!
//! Nothing here logs a record value, an identifier or a trait value.

use std::collections::HashMap;

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::people::{
    ProfileOutcome, ProfileRecord, apply_profile_records, apply_profile_records_singly,
    remove_contribution_in,
};
use crate::{NewDecision, PgPool, StoreError, append_decisions, begin, commit, lock_lease};

/// A run is judged on its reject rate only once this many records have been read. Smaller than
/// the 1,000 rows of a CSV import because incremental runs are usually small.
pub const SYNC_REJECT_CHECK_MIN_RECORDS: i64 = 100;

/// A record the mapping accepted, with the masked sample to keep should the store refuse it.
#[derive(Clone, Debug)]
pub struct SyncUpsert {
    pub record: ProfileRecord,
    pub sample: Value,
}

/// A record the mapping refused: codes and a masked sample, never a value.
#[derive(Clone, Debug)]
pub struct SyncReject {
    /// `(field, code)` pairs; the field is empty when no one field is at fault.
    pub reasons: Vec<(String, &'static str)>,
    /// `{properties, masked}`.
    pub sample: Value,
}

/// A batch of a run: the records gathered from the source and where to resume after them.
pub struct SyncBatch<'a> {
    pub workspace: WorkspaceId,
    pub run_id: Uuid,
    pub stream_id: Uuid,
    /// The `source_records` keys of the run: the source id as text, and the stream name.
    pub source: &'a str,
    pub stream: &'a str,
    /// The job and lease the batch is fenced on, and the attempt for the decisions.
    pub job_id: Uuid,
    pub lease_token: Uuid,
    pub attempt: i32,
    pub trace: &'a TraceContext,
    pub upserts: &'a [SyncUpsert],
    /// The keys of the records the source reports as deleted.
    pub deleted: &'a [String],
    pub rejected: &'a [SyncReject],
    /// Where the source resumes after this batch.
    pub checkpoint: &'a Value,
    /// Also write the checkpoint to the stream, for the next run.
    pub incremental: bool,
    /// Apply each record in a savepoint of its own, so one poison record is rejected as
    /// `apply_failed` instead of stopping the run.
    pub singly: bool,
}

/// Record counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counts {
    pub read: i64,
    pub applied: i64,
    pub unchanged: i64,
    pub rejected: i64,
    pub deleted: i64,
}

/// Where a committed batch left the run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BatchProgress {
    /// What this batch added.
    pub batch: Counts,
    /// The run's totals after it.
    pub run: Counts,
    /// The reject share went over the run's threshold: the batch is committed and the run is
    /// `failed` with `reject_threshold`.
    pub threshold_exceeded: bool,
}

/// What became of a batch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BatchOutcome {
    Committed(BatchProgress),
    /// The job's lease is gone: another worker owns the run now. Nothing was written.
    LeaseLost,
    /// The run is no longer running (cancelled, or ended): nothing was written.
    Cancelled,
}

/// A reject to store, with the code that heads it.
struct Refusal {
    id: Uuid,
    reasons: Vec<(String, &'static str)>,
    sample: Value,
}

/// Commits one batch of a run.
pub async fn commit_sync_batch(
    pool: &PgPool,
    batch: &SyncBatch<'_>,
) -> Result<BatchOutcome, StoreError> {
    let mut tx = begin(pool).await?;
    if !lock_lease(&mut tx, batch.job_id, batch.lease_token).await? {
        return Ok(BatchOutcome::LeaseLost);
    }
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM sync_runs WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.run_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    if state.as_deref() != Some("running") {
        return Ok(BatchOutcome::Cancelled);
    }

    let results = apply_upserts(&mut tx, batch).await?;

    // The ledger rows of the records that are now current, marked as seen by this run.
    let seen_keys: Vec<&str> = batch
        .upserts
        .iter()
        .zip(&results)
        .filter(|(_, result)| {
            matches!(
                result,
                Some(ProfileOutcome::Applied { .. } | ProfileOutcome::Unchanged { .. })
            )
        })
        .map(|(upsert, _)| upsert.record.record_key.as_str())
        .collect();
    let seen = mark_seen(&mut tx, batch, &seen_keys).await?;

    let mut counts = Counts {
        read: i64::try_from(batch.upserts.len() + batch.deleted.len() + batch.rejected.len())
            .unwrap_or(i64::MAX),
        ..Counts::default()
    };
    let mut decisions = Vec::new();
    let mut refusals: Vec<Refusal> = batch
        .rejected
        .iter()
        .map(|reject| Refusal {
            id: Uuid::now_v7(),
            reasons: reject.reasons.clone(),
            sample: reject.sample.clone(),
        })
        .collect();
    for (upsert, result) in batch.upserts.iter().zip(results) {
        let reason: &'static str = match result {
            Some(ProfileOutcome::Applied { person_id, .. }) => {
                counts.applied += 1;
                let mut subjects = vec![("person", person_id)];
                subjects.extend(
                    seen.get(upsert.record.record_key.as_str())
                        .map(|id| ("source_record", *id)),
                );
                decisions.push(decision(batch, "applied", None, subjects));
                continue;
            }
            Some(ProfileOutcome::Unchanged { .. }) => {
                counts.unchanged += 1;
                continue;
            }
            Some(ProfileOutcome::Rejected { reason }) => reason,
            None => "apply_failed",
        };
        refusals.push(Refusal {
            id: Uuid::now_v7(),
            reasons: vec![(String::new(), reason)],
            sample: upsert.sample.clone(),
        });
    }

    // The store's own `contribution.removed` decision says what happened to a deleted record.
    for key in batch.deleted {
        let removed = remove_contribution_in(
            &mut tx,
            batch.workspace,
            batch.source,
            batch.stream,
            key,
            batch.trace,
        )
        .await?;
        if removed {
            counts.deleted += 1;
        } else {
            counts.unchanged += 1;
        }
    }

    counts.rejected = i64::try_from(refusals.len()).unwrap_or(i64::MAX);
    insert_rejects(&mut tx, batch, &refusals).await?;
    for refusal in &refusals {
        let code = refusal
            .reasons
            .first()
            .map_or("rejected", |(_, code)| *code);
        decisions.push(decision(
            batch,
            "rejected",
            Some(code),
            vec![("ingest_reject", refusal.id)],
        ));
    }
    append_decisions(&mut tx, &decisions).await?;

    let (read, applied, unchanged, rejected, deleted, threshold): (i64, i64, i64, i64, i64, f64) =
        sqlx::query_as(
            "UPDATE sync_runs SET records_read = records_read + $3, \
                records_applied = records_applied + $4, \
                records_unchanged = records_unchanged + $5, \
                records_rejected = records_rejected + $6, \
                records_deleted = records_deleted + $7, \
                checkpoint = $8, updated_at = now() \
             WHERE workspace_id = $1 AND id = $2 \
             RETURNING records_read, records_applied, records_unchanged, records_rejected, \
                       records_deleted, reject_threshold",
        )
        .bind(batch.workspace.as_uuid())
        .bind(batch.run_id)
        .bind(counts.read)
        .bind(counts.applied)
        .bind(counts.unchanged)
        .bind(counts.rejected)
        .bind(counts.deleted)
        .bind(batch.checkpoint)
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::query)?;

    let exceeded = threshold_exceeded(read, rejected, threshold);
    if exceeded {
        sqlx::query(
            "UPDATE sync_runs SET state = 'failed', error_code = 'reject_threshold', \
                finished_at = now() WHERE workspace_id = $1 AND id = $2",
        )
        .bind(batch.workspace.as_uuid())
        .bind(batch.run_id)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    } else if batch.incremental {
        // A run that failed on its rejects leaves the stream's cursor where it was, so the
        // next run reads those records again.
        sqlx::query("UPDATE streams SET checkpoint = $3 WHERE workspace_id = $1 AND id = $2")
            .bind(batch.workspace.as_uuid())
            .bind(batch.stream_id)
            .bind(batch.checkpoint)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::query)?;
    }
    commit(tx).await?;
    Ok(BatchOutcome::Committed(BatchProgress {
        batch: counts,
        run: Counts {
            read,
            applied,
            unchanged,
            rejected,
            deleted,
        },
        threshold_exceeded: exceeded,
    }))
}

/// Whether the reject share is over the run's threshold: only once at least
/// [`SYNC_REJECT_CHECK_MIN_RECORDS`] records are read.
fn threshold_exceeded(read: i64, rejected: i64, threshold: f64) -> bool {
    read >= SYNC_REJECT_CHECK_MIN_RECORDS && (rejected as f64) / (read as f64) > threshold
}

fn decision(
    batch: &SyncBatch<'_>,
    outcome: &'static str,
    reason: Option<&'static str>,
    subject_ids: Vec<(&'static str, Uuid)>,
) -> NewDecision {
    NewDecision {
        workspace_id: batch.workspace,
        trace: batch.trace.clone(),
        action: "sync.record",
        outcome,
        reason,
        subject_ids,
        job: Some((batch.job_id, batch.attempt)),
    }
}

/// Applies the accepted records, all together or, with `singly`, each in a savepoint. A record
/// the store cannot apply is `None`.
async fn apply_upserts(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
) -> Result<Vec<Option<ProfileOutcome>>, StoreError> {
    let records: Vec<ProfileRecord> = batch
        .upserts
        .iter()
        .map(|upsert| upsert.record.clone())
        .collect();
    if batch.singly {
        return apply_profile_records_singly(tx, batch.workspace, &records).await;
    }
    let outcomes = apply_profile_records(tx, batch.workspace, &records).await?;
    Ok(outcomes.into_iter().map(Some).collect())
}

/// Sets `last_seen_run` on the live ledger rows of `keys` and returns their ids by key.
async fn mark_seen(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    keys: &[&str],
) -> Result<HashMap<String, Uuid>, StoreError> {
    if keys.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, Uuid)> = sqlx::query_as(
        "UPDATE source_records SET last_seen_run = $5 \
         WHERE workspace_id = $1 AND source = $2 AND stream = $3 AND record_key = ANY($4) \
           AND deleted_at IS NULL \
         RETURNING record_key, id",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.source)
    .bind(batch.stream)
    .bind(keys)
    .bind(batch.run_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

/// Keeps the refused records of the batch, tied to the run.
async fn insert_rejects(
    tx: &mut crate::Tx,
    batch: &SyncBatch<'_>,
    refusals: &[Refusal],
) -> Result<(), StoreError> {
    if refusals.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = refusals.iter().map(|refusal| refusal.id).collect();
    let codes: Vec<&str> = refusals
        .iter()
        .map(|refusal| {
            refusal
                .reasons
                .first()
                .map_or("rejected", |(_, code)| *code)
        })
        .collect();
    let reasons: Vec<Value> = refusals
        .iter()
        .map(|refusal| {
            Value::Array(
                refusal
                    .reasons
                    .iter()
                    .map(|(field, code)| json!({"property": field, "reason": code}))
                    .collect(),
            )
        })
        .collect();
    let samples: Vec<Value> = refusals
        .iter()
        .map(|refusal| refusal.sample.clone())
        .collect();
    sqlx::query(
        "INSERT INTO ingest_rejects \
            (id, workspace_id, received_at, source, kind, name, reason, reasons, sample, \
             payload, trace_id, sync_run_id) \
         SELECT t.id, $1, now(), $2, 'record', $3, t.reason, t.reasons, t.sample, \
                '{}'::jsonb, $4, $5 \
         FROM unnest($6::uuid[], $7::text[], $8::jsonb[], $9::jsonb[]) \
              AS t(id, reason, reasons, sample)",
    )
    .bind(batch.workspace.as_uuid())
    .bind(batch.source)
    .bind(batch.stream)
    .bind(batch.trace.trace_id.as_uuid())
    .bind(batch.run_id)
    .bind(ids)
    .bind(codes)
    .bind(reasons)
    .bind(samples)
    .execute(&mut **tx)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::super::model::{cancel_run, find_run};
    use super::super::testing::{Fixture, batch, fixture, upsert};
    use super::*;

    type DecisionTuple = (
        uuid::Uuid,
        String,
        String,
        String,
        String,
        Option<String>,
        Value,
    );
    type RejectTuple = (
        uuid::Uuid,
        String,
        Option<String>,
        Option<uuid::Uuid>,
        String,
    );

    fn reject() -> SyncReject {
        SyncReject {
            reasons: vec![("email".into(), "invalid_identifier")],
            sample: json!({"properties": {"email": null}, "masked": ["email"]}),
        }
    }

    async fn count(pool: &PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    /// The run's counters, checkpoint and state, and how much was written around it.
    async fn nothing_written(pool: &PgPool, fx: &Fixture) {
        let run = find_run(pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                run.records_read,
                run.records_applied,
                run.records_rejected,
                run.records_deleted
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(run.checkpoint, None);
        assert_eq!(count(pool, "SELECT count(*) FROM source_records").await, 0);
        assert_eq!(count(pool, "SELECT count(*) FROM ingest_rejects").await, 0);
        assert_eq!(
            count(pool, "SELECT count(*) FROM decision_records").await,
            0
        );
        assert_eq!(count(pool, "SELECT count(*) FROM people").await, 0);
    }

    #[test]
    fn the_threshold_applies_only_from_the_minimum_record_count() {
        assert!(!threshold_exceeded(99, 99, 0.05));
        assert!(threshold_exceeded(100, 6, 0.05));
        assert!(!threshold_exceeded(100, 5, 0.05));
        assert!(!threshold_exceeded(1000, 50, 0.05));
        assert!(threshold_exceeded(1000, 51, 0.05));
        assert!(!threshold_exceeded(100, 100, 1.0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_applies_deletes_and_rejects_and_records_each_under_the_run_trace(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let first = [
            upsert(&fx, "k0", "a0@example.com"),
            upsert(&fx, "k1", "a1@example.com"),
            upsert(&fx, "k2", "a2@example.com"),
        ];
        let cp1 = json!({"offset": 3});
        let one = commit_sync_batch(&pool, &batch(&fx, &source, &first, &cp1))
            .await
            .unwrap();
        let BatchOutcome::Committed(one) = one else {
            panic!("{one:?}");
        };
        assert_eq!((one.batch.read, one.batch.applied), (3, 3));

        let second = [
            upsert(&fx, "k3", "a3@example.com"),
            upsert(&fx, "k1", "a1@example.com"),
            upsert(&fx, "kbad", "not-an-email"),
        ];
        let deleted = ["k0".to_owned()];
        let rejected = [reject()];
        let cp2 = json!({"offset": 6});
        let two = commit_sync_batch(
            &pool,
            &SyncBatch {
                deleted: &deleted,
                rejected: &rejected,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(two) = two else {
            panic!("{two:?}");
        };

        assert_eq!(
            (
                two.batch.read,
                two.batch.applied,
                two.batch.unchanged,
                two.batch.rejected,
                two.batch.deleted
            ),
            (5, 1, 1, 2, 1)
        );
        assert_eq!(
            two.run,
            Counts {
                read: 8,
                applied: 4,
                unchanged: 1,
                rejected: 2,
                deleted: 1
            }
        );
        assert!(!two.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.checkpoint, Some(cp2));
        assert_eq!(run.state, "running");

        // Every decision of the run is under its one trace, caused by the run.
        let decisions: Vec<DecisionTuple> = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, action, outcome, reason, subject_ids \
                 FROM decision_records ORDER BY recorded_at, id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(!decisions.is_empty());
        for decision in &decisions {
            assert_eq!(decision.0, fx.run.trace_id);
            assert_eq!(decision.1, "sync_run");
            assert_eq!(decision.2, fx.run.id.to_string());
        }
        let of = |action: &str, outcome: &str| {
            decisions
                .iter()
                .filter(|d| d.3 == action && d.4 == outcome)
                .collect::<Vec<_>>()
        };
        let applied = of("sync.record", "applied");
        assert_eq!(applied.len(), 4);
        for decision in &applied {
            assert!(decision.6["person"].is_string() && decision.6["source_record"].is_string());
        }
        let rejects = of("sync.record", "rejected");
        assert_eq!(rejects.len(), 2);
        let reject_ids: Vec<String> = sqlx::query_scalar("SELECT id::text FROM ingest_rejects")
            .fetch_all(&pool)
            .await
            .unwrap();
        for decision in &rejects {
            let id = decision.6["ingest_reject"].as_str().unwrap();
            assert!(reject_ids.iter().any(|known| known == id));
            assert_eq!(decision.6.as_object().unwrap().len(), 1);
        }
        let mut codes: Vec<_> = rejects.iter().map(|d| d.5.clone().unwrap()).collect();
        codes.sort();
        assert_eq!(codes, ["invalid_email", "invalid_identifier"]);
        assert_eq!(of("contribution.removed", "removed").len(), 1);
        let shown = format!("{decisions:?}");
        assert!(!shown.contains("example.com") && !shown.contains("not-an-email"));

        // The rejects name the run and hold no person.
        let rows: Vec<RejectTuple> =
            sqlx::query_as("SELECT sync_run_id, kind, name, person_id, source FROM ingest_rejects")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.0, fx.run.id);
            assert_eq!(row.1, "record");
            assert_eq!(row.2.as_deref(), Some("contacts"));
            assert_eq!(row.3, None);
            assert_eq!(row.4, source);
        }
        // Applied and unchanged records are marked seen by the run; a full refresh keeps no
        // cursor on the stream.
        let seen: Vec<(String, Option<uuid::Uuid>)> = sqlx::query_as(
            "SELECT record_key, last_seen_run FROM source_records \
             WHERE deleted_at IS NULL ORDER BY record_key",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let keys: Vec<_> = seen.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["k1", "k2", "k3"]);
        assert!(seen.iter().all(|(_, run)| *run == Some(fx.run.id)));
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_fails_midway_writes_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        // PostgreSQL refuses a NUL in jsonb, after the record was applied and before the commit.
        let poison = [SyncReject {
            reasons: vec![("\u{0}".into(), "invalid_identifier")],
            sample: json!({}),
        }];
        let cp = json!({"offset": 1});

        let result = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &poison,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await;

        assert!(result.is_err());
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_worker_whose_lease_was_taken_over_commits_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        sqlx::query("UPDATE jobs SET lease_token = gen_random_uuid() WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::LeaseLost));
        nothing_written(&pool, &fx).await;
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_for_a_job_that_is_gone_commits_nothing(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [upsert(&fx, "k0", "a0@example.com")];
        let cp = json!({"offset": 1});
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(fx.job.id)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &upserts, &cp))
            .await
            .unwrap();

        assert!(matches!(outcome, BatchOutcome::LeaseLost));
        nothing_written(&pool, &fx).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cancel_stops_the_run_at_the_batch_boundary(pool: PgPool) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();
        let first = [upsert(&fx, "k0", "a0@example.com")];
        let cp1 = json!({"offset": 1});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &first, &cp1)
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, BatchOutcome::Committed(_)));
        let mut connection = pool.acquire().await.unwrap();
        cancel_run(&mut connection, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap();
        drop(connection);

        let second = [upsert(&fx, "k1", "a1@example.com")];
        let cp2 = json!({"offset": 2});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                incremental: true,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, BatchOutcome::Cancelled));
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "cancelled");
        assert_eq!((run.records_read, run.records_applied), (1, 1));
        assert_eq!(run.checkpoint, Some(cp1.clone()));
        let keys: Vec<String> = sqlx::query_scalar("SELECT record_key FROM source_records")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(keys, ["k0"]);
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, Some(cp1));
    }

    fn many(fx: &Fixture, from: usize, to: usize) -> Vec<SyncUpsert> {
        (from..to)
            .map(|n| upsert(fx, &format!("k{n}"), &format!("a{n}@example.com")))
            .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_reject_check_waits_for_the_minimum_then_fails_the_run_and_keeps_earlier_batches(
        pool: PgPool,
    ) {
        let fx = fixture(&pool, "one", "incremental").await;
        let source = fx.source_id.to_string();

        // 10 applied and 89 rejected: 99 records read, over the share but under the minimum.
        let first = many(&fx, 0, 10);
        let rejects: Vec<SyncReject> = (0..89).map(|_| reject()).collect();
        let cp1 = json!({"offset": 99});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &rejects,
                incremental: true,
                ..batch(&fx, &source, &first, &cp1)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(progress.run.read, 99);
        assert!(!progress.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "running");

        // The hundredth record, a reject, puts it over.
        let second = many(&fx, 10, 11);
        let one = [reject()];
        let cp2 = json!({"offset": 100});
        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                rejected: &one,
                incremental: true,
                ..batch(&fx, &source, &second, &cp2)
            },
        )
        .await
        .unwrap();
        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };

        assert!(progress.threshold_exceeded);
        let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.state, "failed");
        assert_eq!(run.error_code.as_deref(), Some("reject_threshold"));
        assert!(run.finished_at.is_some());
        // Earlier batches stay applied; nothing is swept.
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM source_records WHERE deleted_at IS NULL"
            )
            .await,
            11
        );
        // The stream's cursor stays at the last batch that did not fail the run.
        let stream_checkpoint: Option<Value> =
            sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                .bind(fx.stream_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stream_checkpoint, Some(cp1));
        // A failed run takes no more batches.
        let late = many(&fx, 11, 12);
        let cp3 = json!({"offset": 101});
        let outcome = commit_sync_batch(&pool, &batch(&fx, &source, &late, &cp3))
            .await
            .unwrap();
        assert!(matches!(outcome, BatchOutcome::Cancelled));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_an_incremental_run_moves_the_streams_checkpoint(pool: PgPool) {
        let incremental = fixture(&pool, "one", "incremental").await;
        let full = fixture(&pool, "two", "full_refresh").await;
        for (fx, is_incremental) in [(&incremental, true), (&full, false)] {
            let source = fx.source_id.to_string();
            let upserts = [upsert(fx, "k0", "a0@example.com")];
            let cp = json!({"offset": 1});
            let outcome = commit_sync_batch(
                &pool,
                &SyncBatch {
                    incremental: is_incremental,
                    ..batch(fx, &source, &upserts, &cp)
                },
            )
            .await
            .unwrap();
            assert!(matches!(outcome, BatchOutcome::Committed(_)));
        }

        for (fx, expected) in [(&incremental, Some(json!({"offset": 1}))), (&full, None)] {
            let stream: Option<Value> =
                sqlx::query_scalar("SELECT checkpoint FROM streams WHERE id = $1")
                    .bind(fx.stream_id)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(stream, expected);
            let run = find_run(&pool, fx.ws, fx.source_id, fx.run.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.checkpoint, Some(json!({"offset": 1})));
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn applying_record_by_record_gives_the_same_result(pool: PgPool) {
        let fx = fixture(&pool, "one", "full_refresh").await;
        let source = fx.source_id.to_string();
        let upserts = [
            upsert(&fx, "k0", "a0@example.com"),
            upsert(&fx, "kbad", "not-an-email"),
        ];
        let cp = json!({"offset": 2});

        let outcome = commit_sync_batch(
            &pool,
            &SyncBatch {
                singly: true,
                ..batch(&fx, &source, &upserts, &cp)
            },
        )
        .await
        .unwrap();

        let BatchOutcome::Committed(progress) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!((progress.batch.applied, progress.batch.rejected), (1, 1));
    }
}
