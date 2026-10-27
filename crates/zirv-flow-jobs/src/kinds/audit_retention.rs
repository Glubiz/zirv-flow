//! `audit.retention`: deletes audit rows past the window through the database's purge function,
//! the only deleter the append-only guard allows, then records the purge as one `audit.purged`
//! row (architecture section 16.3). An instance-level job that a daily schedule enqueues; it
//! needs no workspace and no master key.

use serde_json::{Value, json};
use time::Duration;
use time::format_description::well_known::Rfc3339;
use tracing::info;
use zirv_flow_store::{Actor, NewAuditEntry, append_audit, purge_audit, record_retention_run};

use crate::{JobContext, JobError, JobKind, metrics};

/// Rows deleted per committed statement.
const BATCH: i32 = 5000;

pub(crate) struct AuditRetention;

impl JobKind for AuditRetention {
    const KIND: &'static str = "audit.retention";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new(
                "the worker has no pool to retain the audit log on",
            ));
        };
        let days = ctx.audit_days();
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        let cutoff = now - Duration::days(i64::from(days));
        // Each batch commits on its own, so a failure keeps the earlier ones; they still count.
        let mut deleted = 0_u64;
        let outcome = loop {
            match purge_audit(&pool, cutoff, BATCH).await {
                Ok(batch) => {
                    deleted += u64::try_from(batch).unwrap_or(0);
                    if batch < i64::from(BATCH) {
                        break Ok(());
                    }
                }
                Err(error) => break Err(error),
            }
        };
        metrics::record_retention("audit_log", deleted, now.unix_timestamp() as f64);
        record_retention_run(&pool, "audit", now, deleted).await?;
        // What was purged is recorded even when a later batch failed.
        if deleted > 0 {
            let cutoff_text = cutoff
                .format(&Rfc3339)
                .map_err(|_| JobError::new("the cutoff has no text form"))?;
            append_audit(
                &pool,
                &NewAuditEntry {
                    workspace_id: None,
                    actor: Actor::system(),
                    action: "audit.purged",
                    object_kind: None,
                    object_id: None,
                    request_id: None,
                    trace_id: None,
                    summary: json!({"count": deleted, "cutoff": cutoff_text}),
                },
            )
            .await?;
        }
        outcome?;
        info!(audit_deleted = deleted, days, "audit retention ran");
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
    fn the_kind_name_is_audit_retention() {
        assert_eq!(AuditRetention::KIND, "audit.retention");
    }

    async fn insert_rows(pool: &PgPool, action: &str, age_days: i32, count: i32) {
        sqlx::query(
            "INSERT INTO audit_log (actor_kind, action, occurred_at) \
             SELECT 'system', $1, now() - make_interval(days => $2) + make_interval(secs => n) \
             FROM generate_series(1, $3) n",
        )
        .bind(action)
        .bind(age_days)
        .bind(count)
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
            &[AuditRetention::KIND],
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
        let result = AuditRetention.run(&mut ctx, claimed.payload.clone()).await;
        drop(guard);
        tx.rollback().await.unwrap();
        (result, handle.render())
    }

    async fn actions(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM audit_log ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_scheduled_run_purges_old_rows_and_records_one_purge_row(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        insert_rows(&pool, "session.signed_in", 800, 3).await;
        insert_rows(&pool, "flow.created", 5, 2).await;

        let (result, output) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;

        result.unwrap();
        assert_eq!(
            actions(&pool).await,
            ["flow.created", "flow.created", "audit.purged"]
        );
        let (kind, summary): (String, Value) = sqlx::query_as(
            "SELECT actor_kind, summary FROM audit_log WHERE action = 'audit.purged'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kind, "system");
        let object = summary.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["count", "cutoff"]);
        assert_eq!(object["count"], 3);
        assert_eq!(super::super::recorded_run(&pool, "audit").await, Some(3));
        let cutoff = object["cutoff"].as_str().unwrap();
        assert!(OffsetDateTime::parse(cutoff, &Rfc3339).is_ok(), "{cutoff}");
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"audit_log\"} 3"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_that_deletes_nothing_writes_no_purge_row(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        insert_rows(&pool, "flow.created", 5, 2).await;

        let (result, _) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;

        result.unwrap();
        assert_eq!(actions(&pool).await, ["flow.created", "flow.created"]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_direct_delete_or_update_still_fails_after_a_run(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        insert_rows(&pool, "session.signed_in", 800, 1).await;
        insert_rows(&pool, "flow.created", 5, 1).await;
        let (result, _) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;
        result.unwrap();

        let delete = sqlx::query("DELETE FROM audit_log").execute(&pool).await;
        let update = sqlx::query("UPDATE audit_log SET action = 'x'")
            .execute(&pool)
            .await;

        assert!(delete.is_err() && update.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_after_a_committed_batch_counts_it_and_the_next_run_continues(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        insert_rows(&pool, "old", 800, 5000).await;
        insert_rows(&pool, "last", 799, 1).await;
        sqlx::query(
            "CREATE FUNCTION refuse_last() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF OLD.action = 'last' THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN OLD; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER aaa_refuse_last BEFORE DELETE ON audit_log \
             FOR EACH ROW EXECUTE FUNCTION refuse_last()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let (result, output) = run_scheduled(&pool, OffsetDateTime::now_utc()).await;

        assert!(result.is_err());
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"audit_log\"} 5000"),
            "{output}"
        );
        let purged: i64 =
            sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'audit.purged'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(purged, 1, "the committed batches are recorded");

        sqlx::query("DROP TRIGGER aaa_refuse_last ON audit_log")
            .execute(&pool)
            .await
            .unwrap();
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(730);
        assert_eq!(purge_audit(&pool, cutoff, 5000).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_daily_at_0419_utc(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'audit.retention'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "19 4 * * *", true)
        );
    }
}
