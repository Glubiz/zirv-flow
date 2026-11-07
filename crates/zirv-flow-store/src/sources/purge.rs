//! The purge of a deleted source (architecture section 7.2): the live records of a source that no
//! longer exists are removed from the people they feed, a bounded batch per transaction.
//!
//! The delete request enqueues the first purge job in its own transaction; each run removes one
//! batch through `remove_contribution_in` (the person's traits are recomputed from the remaining
//! live records, the identities only that record held go, and the person is marked for segments)
//! and the job enqueues the next batch until none is left. A removed record is tombstoned, so a
//! run that dies and is retried finds only what is left and removes each record once.
//!
//! Nothing here holds or logs a record value, an identifier or a trait value.

use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId, decision_actions};

use crate::people::remove_contribution_in;
use crate::sync_runs::SYNC_SWEEP_BATCH;
use crate::{NewDecision, NewJob, StoreError, append_decisions};

/// The kind of the job that purges a deleted source.
pub const PURGE_JOB_KIND: &str = "sources.purge";

/// Why a record was removed because its source was deleted.
pub const DELETE_BY_SOURCE_DELETE: &str = "source_deleted";

/// The most records one purge batch removes.
pub const PURGE_BATCH: i64 = SYNC_SWEEP_BATCH;

/// The first purge job of a deleted source. One job per source is ready or running at a time, so
/// a second enqueue for the same source is a no-op.
pub fn purge_job(workspace: WorkspaceId, source_id: Uuid, trace: &TraceContext) -> NewJob {
    let mut job = NewJob::new(PURGE_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace, "source_id": source_id });
    job.unique_key = Some(format!("{PURGE_JOB_KIND}:{source_id}"));
    job
}

/// What a purge batch did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PurgeBatchOutcome {
    /// Records this batch removed.
    pub removed: i64,
    /// The batch was full, so another may find more.
    pub more: bool,
}

/// Removes up to [`PURGE_BATCH`] live records of the source in the caller's transaction, each with
/// a `deleted` decision on the job's trace, and asks for the segment re-evaluation the removals
/// marked. The caller commits.
pub async fn purge_source_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source_id: Uuid,
    trace: &TraceContext,
    job: Option<(Uuid, i32)>,
) -> Result<PurgeBatchOutcome, StoreError> {
    let source = source_id.to_string();
    let live: Vec<(String, String)> = sqlx::query_as(
        "SELECT stream, record_key FROM source_records \
         WHERE workspace_id = $1 AND source = $2 AND deleted_at IS NULL \
         ORDER BY id LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(&source)
    .bind(PURGE_BATCH)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    let mut decisions = Vec::new();
    for (stream, key) in &live {
        let removed =
            remove_contribution_in(&mut *connection, workspace, &source, stream, key, trace)
                .await?;
        if let Some(removed) = removed {
            decisions.push(NewDecision {
                graph: None,
                workspace_id: workspace,
                trace: trace.clone(),
                spec: &decision_actions::SYNC_RECORD_DELETED,
                detail: Some(DELETE_BY_SOURCE_DELETE),
                subject_ids: vec![("source_record", removed.record_id)],
                job,
            });
        }
    }
    let removed = i64::try_from(decisions.len()).unwrap_or(i64::MAX);
    append_decisions(&mut *connection, &decisions).await?;
    if removed > 0 {
        crate::changes::request_reevaluation(&mut *connection, workspace).await?;
    }
    Ok(PurgeBatchOutcome {
        removed,
        more: i64::try_from(live.len()).unwrap_or(i64::MAX) >= PURGE_BATCH,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value};
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::{Cause, Slug, WorkspaceName};

    use super::*;
    use crate::{
        ProfileRecord, apply_profile_batch, begin, commit, create_workspace, list_identities,
    };

    fn trace() -> TraceContext {
        TraceContext::root(Cause::api("purge"))
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn record(
        source: Uuid,
        key: &str,
        identifiers: &[(&str, &str)],
        traits: Value,
    ) -> ProfileRecord {
        ProfileRecord {
            source: source.to_string(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).to_owned(), (*value).to_owned()))
                .collect(),
            traits: traits.as_object().cloned().unwrap_or_else(Map::new),
            observed_at: OffsetDateTime::now_utc(),
            trace: trace(),
        }
    }

    async fn live(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM source_records WHERE deleted_at IS NULL")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn purge(pool: &PgPool, ws: WorkspaceId, source: Uuid) -> PurgeBatchOutcome {
        let mut tx = begin(pool).await.unwrap();
        let outcome = purge_source_batch(&mut tx, ws, source, &trace(), None)
            .await
            .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    #[test]
    fn the_first_job_is_unique_per_source_and_names_the_workspace_and_the_source() {
        let ws = WorkspaceId::new();
        let source = Uuid::now_v7();

        let job = purge_job(ws, source, &trace());

        assert_eq!(job.kind, PURGE_JOB_KIND);
        assert_eq!(job.workspace_id, Some(ws));
        assert_eq!(job.payload["source_id"], json!(source));
        assert_eq!(
            job.unique_key.as_deref(),
            Some(format!("sources.purge:{source}").as_str())
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_keeps_the_other_sources_traits_and_loses_the_purged_sources_alone(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::from_bits(63),
        )
        .await;
        let (gone, kept) = (Uuid::now_v7(), Uuid::now_v7());
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                record(
                    gone,
                    "1",
                    &[("email", "a@example.com"), ("phone", "+4512345678")],
                    json!({"plan": "pro"}),
                ),
                record(
                    kept,
                    "9",
                    &[("email", "a@example.com")],
                    json!({"tier": "gold"}),
                ),
            ],
        )
        .await
        .unwrap();
        let crate::ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        let outcome = purge(&pool, ws, gone).await;

        assert_eq!(
            outcome,
            PurgeBatchOutcome {
                removed: 1,
                more: false
            }
        );
        let person = crate::find_person(&pool, ws, person_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(person.traits.get("tier"), Some(&json!("gold")));
        assert_eq!(person.traits.get("plan"), None, "the purged source's trait");
        let identities = list_identities(&pool, ws, person_id).await.unwrap();
        let kinds: Vec<&str> = identities.iter().map(|i| i.kind.as_str()).collect();
        assert_eq!(kinds, ["email"], "the phone only the purged source held");
        let dirty: i64 =
            sqlx::query_scalar("SELECT count(*) FROM person_changes WHERE person_id = $1")
                .bind(person_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(dirty, 1, "segments recompute the person");
        assert_eq!(live(&pool).await, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_purge_over_one_batch_finishes_and_removes_each_record_once(pool: PgPool) {
        let ws = workspace(&pool).await;
        let (gone, kept) = (Uuid::now_v7(), Uuid::now_v7());
        let mut records: Vec<ProfileRecord> = (0..450)
            .map(|i| {
                record(
                    gone,
                    &format!("k{i}"),
                    &[("email", &format!("g{i}@example.com"))],
                    json!({}),
                )
            })
            .collect();
        records.push(record(
            kept,
            "k",
            &[("email", "keep@example.com")],
            json!({}),
        ));
        apply_profile_batch(&pool, ws, &records).await.unwrap();

        let mut sizes = Vec::new();
        loop {
            let outcome = purge(&pool, ws, gone).await;
            sizes.push((outcome.removed, outcome.more));
            if !outcome.more {
                break;
            }
        }

        assert_eq!(sizes, [(200, true), (200, true), (50, false)]);
        assert_eq!(live(&pool).await, 1, "the other source is untouched");
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'sync.record' \
             AND outcome = 'deleted' AND reason = 'source_deleted'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let retracted: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'contribution.removed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((decisions, retracted), (450, 450));
        // A run after the end finds nothing.
        assert_eq!(
            purge(&pool, ws, gone).await,
            PurgeBatchOutcome {
                removed: 0,
                more: false
            }
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_that_fails_leaves_nothing_removed_and_the_retry_removes_each_once(
        pool: PgPool,
    ) {
        let ws = workspace(&pool).await;
        let gone = Uuid::now_v7();
        let records: Vec<ProfileRecord> = (0..250)
            .map(|i| {
                record(
                    gone,
                    &format!("k{i}"),
                    &[("email", &format!("g{i}@example.com"))],
                    json!({}),
                )
            })
            .collect();
        apply_profile_batch(&pool, ws, &records).await.unwrap();
        // The first run does one batch, then its transaction is lost before the second.
        assert_eq!(purge(&pool, ws, gone).await.removed, 200);
        let mut tx = begin(&pool).await.unwrap();
        let lost = purge_source_batch(&mut tx, ws, gone, &trace(), None)
            .await
            .unwrap();
        assert_eq!(lost.removed, 50);
        drop(tx);
        assert_eq!(
            live(&pool).await,
            50,
            "a dropped transaction removes nothing"
        );

        let retry = purge(&pool, ws, gone).await;

        assert_eq!(
            retry,
            PurgeBatchOutcome {
                removed: 50,
                more: false
            }
        );
        assert_eq!(live(&pool).await, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_purge_touches_only_its_source_in_its_workspace(pool: PgPool) {
        let ws = workspace(&pool).await;
        let other = {
            let name = WorkspaceName::parse("theirs").unwrap();
            let slug = Slug::parse("theirs").unwrap();
            create_workspace(&pool, &name, &slug).await.unwrap().id
        };
        let gone = Uuid::now_v7();
        apply_profile_batch(
            &pool,
            ws,
            &[record(gone, "1", &[("email", "a@example.com")], json!({}))],
        )
        .await
        .unwrap();
        apply_profile_batch(
            &pool,
            other,
            &[record(gone, "1", &[("email", "a@example.com")], json!({}))],
        )
        .await
        .unwrap();

        purge(&pool, ws, gone).await;

        let left: Vec<(Uuid,)> =
            sqlx::query_as("SELECT workspace_id FROM source_records WHERE deleted_at IS NULL")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(left, [(other.as_uuid(),)]);
    }
}
