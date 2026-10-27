//! `decisions.retention`: drops the decision-log partitions past the window, deletes the older
//! rows of the month that holds the cutoff, and refreshes the size gauges (architecture section
//! 16.3). An instance-level job that a daily schedule enqueues; it needs no workspace and no
//! master key.

use hashbrown::HashMap;
use serde_json::Value;
use tracing::{info, warn};
use zirv_flow_store::{
    DecisionRetentionReport, apply_decision_retention_progress, decision_log_size,
};

use crate::{JobContext, JobError, JobKind, metrics};

pub(crate) struct DecisionsRetention;

impl JobKind for DecisionsRetention {
    const KIND: &'static str = "decisions.retention";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new(
                "the worker has no pool to retain decisions on",
            ));
        };
        let settings = *ctx.decision_retention();
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        // Workspaces with a shorter window of their own arrive here with the per-workspace
        // setting; none have one yet.
        // What committed before a failure still counts, so the counter is recorded either way.
        let mut report = DecisionRetentionReport::default();
        let outcome =
            apply_decision_retention_progress(&pool, &settings, &HashMap::new(), now, &mut report)
                .await;
        metrics::record_retention(
            "decision_records",
            report.records_deleted,
            now.unix_timestamp() as f64,
        );
        for partition in &report.skipped_partitions {
            warn!(partition = %partition, "decision partition drop skipped: the table is locked");
        }
        outcome?;
        let size = decision_log_size(&pool).await?;
        metrics::record_decision_log_size(size.bytes, size.partitions);
        info!(
            records_deleted = report.records_deleted,
            partitions_dropped = report.partitions_dropped,
            partitions_skipped = report.skipped_partitions.len(),
            "decision retention ran"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_store::{Clock, DecisionRetention, ManualClock};

    use super::*;

    #[test]
    fn the_kind_name_is_decisions_retention() {
        assert_eq!(DecisionsRetention::KIND, "decisions.retention");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_deletes_what_is_past_the_window_at_the_clock_and_reports_counts_and_size(
        pool: PgPool,
    ) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let now = OffsetDateTime::now_utc();
        // The old row lands in the DEFAULT partition, so only the batched delete removes it.
        for (days, action) in [(1_i64, "fresh"), (500, "old")] {
            sqlx::query(
                "INSERT INTO decision_records \
                 (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id) \
                 VALUES (gen_random_uuid(), $1, gen_random_uuid(), $2, $3, 'done', 'api', 'x')",
            )
            .bind(ws.as_uuid())
            .bind(now - time::Duration::days(days))
            .bind(action)
            .execute(&pool)
            .await
            .unwrap();
        }
        // The migration's schedule fires on the leader's tick, as it would in a deployment.
        let mut leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        let fired = leader.tick_at(now + time::Duration::days(2)).await.unwrap();
        assert!(matches!(fired, crate::Tick::Led(report) if report.fired >= 1));
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[DecisionsRetention::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_ingest(crate::context::IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 1,
                shards: 1,
            })
            .with_clock(Clock::Manual(ManualClock::new(now)))
            .with_decision_retention(DecisionRetention::default());
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let guard = ::metrics::set_default_local_recorder(&recorder);
        DecisionsRetention
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        drop(guard);
        tx.rollback().await.unwrap();

        let left: Vec<String> = sqlx::query_scalar("SELECT action FROM decision_records")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, ["fresh"]);
        let size = decision_log_size(&pool).await.unwrap();
        let output = handle.render();
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"decision_records\"} 1"),
            "{output}"
        );
        assert!(
            output.contains(&format!("zirv_flow_decision_log_bytes {}", size.bytes)),
            "{output}"
        );
        assert!(
            output.contains(&format!(
                "zirv_flow_decision_log_partitions {}",
                size.partitions
            )),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_a_committed_batch_still_counts_that_batch(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let now = OffsetDateTime::now_utc();
        // One full batch of old rows, then a last one that a trigger refuses to delete.
        sqlx::query(
            "INSERT INTO decision_records \
             (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), \
                $2 - make_interval(days => 500) + make_interval(secs => n), \
                CASE WHEN n = 5001 THEN 'last' ELSE 'old' END, 'done', 'api', 'x' \
             FROM generate_series(1, 5001) n",
        )
        .bind(ws.as_uuid())
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE FUNCTION refuse_last() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF OLD.action = 'last' THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN OLD; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_last BEFORE DELETE ON decision_records_default \
             FOR EACH ROW EXECUTE FUNCTION refuse_last()",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        leader.tick_at(now + time::Duration::days(2)).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[DecisionsRetention::KIND],
            Duration::from_secs(30),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        let mut ctx = JobContext::new(&mut tx, &claimed)
            .with_ingest(crate::context::IngestSettings {
                pool: pool.clone(),
                redaction_key: None,
                batch_size: 1,
                shards: 1,
            })
            .with_clock(Clock::Manual(ManualClock::new(now)))
            .with_decision_retention(DecisionRetention::default());
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let guard = ::metrics::set_default_local_recorder(&recorder);
        let result = DecisionsRetention
            .run(&mut ctx, claimed.payload.clone())
            .await;
        drop(guard);
        tx.rollback().await.unwrap();

        assert!(result.is_err());
        let output = handle.render();
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"decision_records\"} 5000"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_daily_at_0353_utc(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'decisions.retention'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "53 3 * * *", true)
        );
    }
}
