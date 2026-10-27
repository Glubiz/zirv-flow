//! `maintenance.ai_transcripts`: deletes AI transcripts past their expiry (ruling OQ-26), and the
//! assistant's drafts past theirs (seven days after they were made). An instance-level job that an
//! hourly schedule enqueues. It deletes in bounded batches and a rerun finds nothing left for the
//! batches that completed, so it is idempotent.

use serde_json::Value;
use tracing::info;
use zirv_flow_store::{
    DRAFT_PURGE_BATCH, TRANSCRIPT_PURGE_BATCH, database_now, purge_expired_ai_drafts,
    purge_expired_ai_transcripts, record_retention_run,
};

use crate::{JobContext, JobError, JobKind};

/// The most batches one run deletes; a backlog beyond it waits for the next run.
const MAX_BATCHES: u32 = 100;

pub(crate) struct AiTranscripts;

impl JobKind for AiTranscripts {
    const KIND: &'static str = "maintenance.ai_transcripts";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let now = database_now(&mut *ctx.conn()).await?;
        let mut deleted = 0;
        for _ in 0..MAX_BATCHES {
            let batch =
                purge_expired_ai_transcripts(ctx.conn(), now, TRANSCRIPT_PURGE_BATCH).await?;
            deleted += batch;
            if batch < TRANSCRIPT_PURGE_BATCH as u64 {
                break;
            }
        }
        let mut drafts = 0;
        for _ in 0..MAX_BATCHES {
            let batch = purge_expired_ai_drafts(ctx.conn(), now, DRAFT_PURGE_BATCH).await?;
            drafts += batch;
            if batch < DRAFT_PURGE_BATCH as u64 {
                break;
            }
        }
        record_retention_run(&mut *ctx.conn(), "assistant_transcripts", now, deleted).await?;
        info!(deleted, drafts, "AI transcript and draft retention ran");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};
    use zirv_flow_store::{CallOutcome, NewAiCall, NewJob, insert_ai_transcript, record_ai_call};

    use super::*;

    #[test]
    fn the_kind_name_is_maintenance_ai_transcripts() {
        assert_eq!(AiTranscripts::KIND, "maintenance.ai_transcripts");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_purges_only_expired_transcripts_and_a_rerun_changes_nothing(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let now = OffsetDateTime::now_utc();
        let mut connection = pool.acquire().await.unwrap();
        for (n, offset) in [(1, -1_i64), (2, 1)] {
            let call = Uuid::now_v7();
            record_ai_call(
                &mut connection,
                &NewAiCall {
                    id: call,
                    workspace_id: ws,
                    user_id: None,
                    feature: "mapping",
                    provider_id: "p",
                    model: "m",
                    input_tokens: 1,
                    output_tokens: 1,
                    estimated_cost_micros: 1,
                    latency_ms: 1,
                    outcome: CallOutcome::Ok,
                    draft_id: None,
                },
            )
            .await
            .unwrap();
            let expires = now + time::Duration::days(offset);
            insert_ai_transcript(&mut connection, ws, call, None, &json!({"n": n}), expires)
                .await
                .unwrap();
        }
        drop(connection);

        for _ in 0..2 {
            let mut new = NewJob::new(AiTranscripts::KIND, TraceContext::root(Cause::api("req")));
            new.payload = json!({});
            zirv_flow_store::enqueue(&pool, &new).await.unwrap();
            let claimed = zirv_flow_store::claim_job(
                &pool,
                "w",
                &[AiTranscripts::KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed);
            AiTranscripts
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        let left: Vec<serde_json::Value> = sqlx::query_scalar("SELECT content FROM ai_transcripts")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [json!({"n": 2})]);
        assert!(
            super::super::recorded_run(&pool, "assistant_transcripts")
                .await
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_hourly_run_also_purges_expired_drafts(pool: PgPool) {
        use zirv_flow_core::Email;
        use zirv_flow_store::{
            DraftKind, DraftStatus, NewAiDraft, NewSource, create_source, create_user,
            insert_ai_draft,
        };

        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = WorkspaceName::parse("ours").unwrap();
        let slug = Slug::parse("ours").unwrap();
        let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let user = create_user(
            &pool,
            &Email::parse("ada@example.com").unwrap(),
            None,
            false,
        )
        .await
        .unwrap()
        .id;
        let config = json!({});
        let source = Uuid::now_v7();
        create_source(
            &pool,
            ws,
            &NewSource {
                id: source,
                kind: "fake",
                name: "crm",
                config: &config,
                credential_id: None,
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        let now = OffsetDateTime::now_utc();
        let content = json!({"columns": []});
        let mut connection = pool.acquire().await.unwrap();
        let mut ids = Vec::new();
        for offset in [-1_i64, 7] {
            let id = Uuid::now_v7();
            ids.push(id);
            insert_ai_draft(
                &mut connection,
                &NewAiDraft {
                    id,
                    workspace_id: ws,
                    user_id: user,
                    kind: DraftKind::Mapping,
                    source_id: Some(source),
                    stream_name: Some("contacts"),
                    segment_id: None,
                    flow_id: None,
                    template_id: None,
                    status: DraftStatus::Valid,
                    content: Some(&content),
                    base_digest: None,
                    expires_at: now + time::Duration::days(offset),
                },
            )
            .await
            .unwrap();
        }
        drop(connection);

        for _ in 0..2 {
            let mut new = NewJob::new(AiTranscripts::KIND, TraceContext::root(Cause::api("req")));
            new.payload = json!({});
            zirv_flow_store::enqueue(&pool, &new).await.unwrap();
            let claimed = zirv_flow_store::claim_job(
                &pool,
                "w",
                &[AiTranscripts::KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
            let mut ctx = JobContext::new(&mut tx, &claimed);
            AiTranscripts
                .run(&mut ctx, claimed.payload.clone())
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        let left: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM ai_drafts")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [ids[1]], "only the draft that expired is gone");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_hourly_at_minute_23(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules \
             WHERE kind = 'maintenance.ai_transcripts'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "23 * * * *", true)
        );
    }
}
