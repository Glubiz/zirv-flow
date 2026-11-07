//! `sources.purge`: removes a deleted source's contribution to live data.
//!
//! The payload is `{workspace_id, source_id}`. Each run removes one batch of the source's live
//! records in its own transaction (the people are recomputed from their remaining records and
//! marked for segments) and enqueues the next batch on the same trace until none is left. A run
//! that fails is retried and finds only the records not yet removed.

use serde_json::{Value, json};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{NewJob, SOURCE_PURGE_JOB_KIND};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct SourcesPurge;

impl JobKind for SourcesPurge {
    const KIND: &'static str = SOURCE_PURGE_JOB_KIND;
    const NEEDS_CHANGE_READERS: bool = true;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let (workspace, source) = super::workspace_and_id(
            &payload,
            "source_id",
            "the payload needs a workspace_id and a source_id",
        )?;
        if ctx.workspace_id() != Some(workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let trace = ctx.trace().clone();
        let job = Some((ctx.job_id(), ctx.attempts()));
        let batch =
            zirv_flow_store::purge_source_batch(ctx.conn(), workspace, source, &trace, job).await?;
        crate::metrics::record_source_purge(batch.removed);
        if batch.more {
            ctx.enqueue(&follow_up(workspace, source, &trace)).await?;
        }
        Ok(())
    }
}

fn follow_up(workspace: WorkspaceId, source: Uuid, trace: &zirv_flow_core::TraceContext) -> NewJob {
    let mut next = NewJob::new(SOURCE_PURGE_JOB_KIND, trace.clone());
    next.workspace_id = Some(workspace);
    next.payload = json!({ "workspace_id": workspace, "source_id": source });
    next
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{ProfileRecord, apply_profile_batch};

    use super::*;

    #[test]
    fn the_kind_name_is_sources_purge() {
        assert_eq!(SourcesPurge::KIND, "sources.purge");
    }

    #[test]
    fn the_follow_up_carries_the_payload_and_no_unique_key() {
        let ws = WorkspaceId::new();
        let source = Uuid::now_v7();

        let next = follow_up(ws, source, &TraceContext::root(Cause::api("t")));

        assert_eq!(next.payload["source_id"], json!(source));
        assert!(next.unique_key.is_none(), "the running job holds the key");
    }

    async fn claim_and_run(pool: &PgPool) -> bool {
        let Some(claimed) = zirv_flow_store::claim_job(
            pool,
            "w",
            &[SourcesPurge::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap() else {
            return false;
        };
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed);
        SourcesPurge
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        sqlx::query("DELETE FROM jobs WHERE id = $1")
            .bind(claimed.id)
            .execute(pool)
            .await
            .unwrap();
        true
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_purges_in_batches_and_enqueues_its_own_follow_ups(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let source = Uuid::now_v7();
        let records: Vec<ProfileRecord> = (0..250)
            .map(|i| ProfileRecord {
                source: source.to_string(),
                stream: "s".into(),
                record_key: format!("k{i}"),
                identifiers: vec![("email".into(), format!("p{i}@example.com"))],
                traits: serde_json::Map::new(),
                observed_at: OffsetDateTime::now_utc(),
                trace: TraceContext::root(Cause::api("t")),
            })
            .collect();
        apply_profile_batch(&pool, ws, &records).await.unwrap();
        let trace = TraceContext::root(Cause::api("req"));
        let first = zirv_flow_store::source_purge_job(ws, source, &trace);
        assert!(zirv_flow_store::enqueue(&pool, &first).await.unwrap());
        assert!(
            !zirv_flow_store::enqueue(&pool, &first).await.unwrap(),
            "a second delete enqueues no second purge"
        );

        let mut runs = 0;
        while claim_and_run(&pool).await {
            runs += 1;
        }

        assert_eq!(runs, 2, "200 then 50");
        let live: i64 =
            sqlx::query_scalar("SELECT count(*) FROM source_records WHERE deleted_at IS NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(live, 0);
    }
}
