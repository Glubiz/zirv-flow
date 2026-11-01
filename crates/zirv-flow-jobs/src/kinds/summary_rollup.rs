//! `summary.rollup`: brings the daily rollup behind the Home figures up to date, workspace by
//! workspace. An instance-level job that a five-minute schedule enqueues; it needs no payload and
//! no master key. A workspace that fails is logged and skipped, so one cannot stop the others;
//! the job then fails so the failure is visible, and the next run counts what was missed.

use serde_json::Value;
use tracing::{info, warn};
use zirv_flow_store::{refresh_workspace_summary, summary_workspaces};

use crate::{JobContext, JobError, JobKind};

pub(crate) struct SummaryRollup;

impl JobKind for SummaryRollup {
    const KIND: &'static str = "summary.rollup";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new(
                "the worker has no pool to roll the summary up on",
            ));
        };
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let workspaces = summary_workspaces(&pool).await?;
        let mut failed = 0_usize;
        for workspace in &workspaces {
            if let Err(error) = refresh_workspace_summary(&pool, *workspace, now).await {
                failed += 1;
                warn!(%workspace, %error, "summary rollup failed for a workspace");
            }
        }
        info!(workspaces = workspaces.len(), failed, "summary rollup ran");
        if failed > 0 {
            return Err(JobError::new("the summary rollup failed for a workspace"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};
    use zirv_flow_store::{Clock, ManualClock};

    use super::*;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        zirv_flow_store::create_workspace(
            pool,
            &WorkspaceName::parse(slug).unwrap(),
            &Slug::parse(slug).unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    /// Fires the migration's schedule and runs the claimed job at `now`.
    async fn run_once(pool: &PgPool, now: OffsetDateTime) -> Result<(), JobError> {
        let mut leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        leader
            .tick_at(now + time::Duration::hours(1))
            .await
            .unwrap();
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[SummaryRollup::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_ingest(crate::context::IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 1,
                shards: 1,
            })
            .with_clock(Clock::Manual(ManualClock::new(now)));
        let result = SummaryRollup.run(&mut ctx, claimed.payload.clone()).await;
        tx.rollback().await.unwrap();
        result
    }

    async fn rolled_up(pool: &PgPool, ws: WorkspaceId) -> bool {
        sqlx::query_scalar(
            "SELECT computed_at IS NOT NULL FROM summary_state WHERE workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .fetch_optional(pool)
        .await
        .unwrap()
        .unwrap_or(false)
    }

    #[test]
    fn the_kind_name_is_summary_rollup() {
        assert_eq!(SummaryRollup::KIND, "summary.rollup");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_every_five_minutes(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'summary.rollup'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "*/5 * * * *", true)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_rolls_every_workspace_up(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);

        run_once(&pool, OffsetDateTime::now_utc()).await.unwrap();

        assert!(rolled_up(&pool, one).await);
        assert!(rolled_up(&pool, two).await);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failing_workspace_does_not_stop_the_others_and_fails_the_job(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (bad, good) = (
            workspace(&pool, "bad").await,
            workspace(&pool, "good").await,
        );
        sqlx::query(
            "CREATE FUNCTION refuse_state() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TRIGGER refuse_state BEFORE INSERT ON summary_state \
             FOR EACH ROW WHEN (NEW.workspace_id = '{}') EXECUTE FUNCTION refuse_state()",
            bad.as_uuid()
        )))
        .execute(&pool)
        .await
        .unwrap();

        let result = run_once(&pool, OffsetDateTime::now_utc()).await;

        assert!(result.is_err());
        assert!(rolled_up(&pool, good).await);
        assert!(!rolled_up(&pool, bad).await);
    }
}
