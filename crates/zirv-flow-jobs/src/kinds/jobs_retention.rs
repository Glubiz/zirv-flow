//! `jobs.retention`: deletes dead jobs past the window (architecture section 16.3). An
//! instance-level job that a daily schedule enqueues; it needs no workspace and no master key.
//! Dead jobs are the job history: finished jobs are removed when they complete.

use serde_json::Value;
use time::Duration;
use tracing::info;
use zirv_flow_store::purge_dead_jobs;

use crate::{JobContext, JobError, JobKind, metrics};

/// Rows deleted per committed statement.
const BATCH: i64 = 5000;

pub(crate) struct JobsRetention;

impl JobKind for JobsRetention {
    const KIND: &'static str = "jobs.retention";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new("the worker has no pool to retain jobs on"));
        };
        let days = ctx.job_history_days();
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let cutoff = now - Duration::days(i64::from(days));
        // Each batch commits on its own, so a failure keeps the earlier ones; they still count.
        let mut deleted = 0_u64;
        let outcome = loop {
            match purge_dead_jobs(&pool, cutoff, BATCH).await {
                Ok(batch) => {
                    deleted += u64::try_from(batch).unwrap_or(0);
                    if batch < BATCH {
                        break Ok(());
                    }
                }
                Err(error) => break Err(error),
            }
        };
        metrics::record_retention("jobs", deleted, now.unix_timestamp() as f64);
        outcome?;
        info!(jobs_deleted = deleted, days, "job history retention ran");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_store::{Clock, ManualClock};

    use super::*;

    #[test]
    fn the_kind_name_is_jobs_retention() {
        assert_eq!(JobsRetention::KIND, "jobs.retention");
    }

    async fn insert_job(pool: &PgPool, kind: &str, state: &str, age_days: i32) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, state, payload, trace_id, cause_kind, cause_id, dead_at) \
             VALUES (gen_random_uuid(), $1, $2, '{}', gen_random_uuid(), 'manual', '', \
                     CASE WHEN $2 = 'dead' THEN now() - make_interval(days => $3) END)",
        )
        .bind(kind)
        .bind(state)
        .bind(age_days)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Fires the migration's schedule on the leader's tick and runs the claimed job at `now`.
    async fn run_scheduled(pool: &PgPool, now: OffsetDateTime) -> (Result<(), JobError>, String) {
        let mut leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        let fired = leader.tick_at(now + time::Duration::days(2)).await.unwrap();
        assert!(matches!(fired, crate::Tick::Led(report) if report.fired >= 1));
        let claimed = zirv_flow_store::claim_job(
            pool,
            "w",
            &[JobsRetention::KIND],
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
            .with_clock(Clock::Manual(ManualClock::new(now)))
            .with_log_retention_days(14, 730);
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let guard = ::metrics::set_default_local_recorder(&recorder);
        let result = JobsRetention.run(&mut ctx, claimed.payload.clone()).await;
        drop(guard);
        tx.rollback().await.unwrap();
        (result, handle.render())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_scheduled_run_removes_dead_jobs_past_14_days_and_keeps_the_rest(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        for (kind, state, age) in [
            ("old.dead", "dead", 15),
            ("young.dead", "dead", 13),
            ("old.ready", "ready", 40),
            ("old.running", "running", 40),
        ] {
            insert_job(&pool, kind, state, age).await;
        }

        let (result, output) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;

        result.unwrap();
        let mut left: Vec<String> = sqlx::query_scalar(
            "SELECT kind FROM jobs WHERE kind LIKE 'old.%' OR kind LIKE 'young.%'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        left.sort();
        assert_eq!(left, ["old.ready", "old.running", "young.dead"]);
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"jobs\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_retention_last_run_timestamp_seconds{table=\"jobs\"}"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_a_committed_batch_counts_it_and_the_next_run_continues(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        // One full batch of old dead jobs, then a last one that a trigger refuses to delete.
        sqlx::query(
            "INSERT INTO jobs (id, kind, state, payload, trace_id, cause_kind, cause_id, dead_at) \
             SELECT gen_random_uuid(), CASE WHEN n = 5001 THEN 'last' ELSE 'old' END, 'dead', \
                    '{}', gen_random_uuid(), 'manual', '', now() - make_interval(days => 30) + make_interval(secs => n) \
             FROM generate_series(1, 5001) n",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE FUNCTION refuse_last() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF OLD.kind = 'last' THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN OLD; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_last BEFORE DELETE ON jobs \
             FOR EACH ROW EXECUTE FUNCTION refuse_last()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (result, output) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;

        assert!(result.is_err());
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"jobs\"} 5000"),
            "{output}"
        );
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'last'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 1);

        sqlx::query("DROP TRIGGER refuse_last ON jobs")
            .execute(&pool)
            .await
            .unwrap();
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(14);
        assert_eq!(purge_dead_jobs(&pool, cutoff, 5000).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_daily_at_0407_utc(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'jobs.retention'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "7 4 * * *", true)
        );
    }
}
