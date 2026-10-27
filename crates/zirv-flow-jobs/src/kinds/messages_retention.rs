//! `messages.retention`: deletes ended messages past their window and writes the handled markers
//! of the rows it removes (architecture section 16.3). An instance-level job that a daily
//! schedule enqueues; it needs no workspace and no master key.

use serde_json::Value;
use tracing::info;
use zirv_flow_store::{
    MessageRetentionReport, apply_message_retention_progress, record_retention_run,
    workspace_windows,
};

use crate::{JobContext, JobError, JobKind, metrics};

/// The deletes commit batch by batch before the `retention.ran` row is written, so a failure
/// after them leaves no row.
pub(crate) struct MessagesRetention;

impl JobKind for MessagesRetention {
    const KIND: &'static str = "messages.retention";

    async fn run(&self, ctx: &mut JobContext<'_>, _payload: Value) -> Result<(), JobError> {
        let Some(pool) = ctx.pool().cloned() else {
            return Err(JobError::new(
                "the worker has no pool to retain messages on",
            ));
        };
        let settings = *ctx.message_retention();
        let clock = ctx.clock().clone();
        let now = clock.now(ctx.conn()).await?;
        // A workspace with a shorter window of its own is deleted at that window, and only it.
        let windows =
            workspace_windows(&pool, settings.days, ctx.decision_retention().days).await?;
        // What committed before a failure still counts, so the run and the counter are recorded
        // either way.
        let mut report = MessageRetentionReport::default();
        let outcome =
            apply_message_retention_progress(&pool, &settings, &windows.messages, now, &mut report)
                .await;
        record_retention_run(&pool, "messages", now, report.messages_deleted).await?;
        metrics::record_retention(
            "messages",
            report.messages_deleted,
            now.unix_timestamp() as f64,
        );
        outcome?;
        info!(
            messages_deleted = report.messages_deleted,
            "message retention ran"
        );
        ctx.audit_retention(
            Self::KIND,
            now - time::Duration::days(i64::from(settings.days)),
            &[("messages_deleted", report.messages_deleted)],
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use time::OffsetDateTime;
    use zirv_flow_store::{Clock, ManualClock, MessageRetention};

    use super::*;

    #[test]
    fn the_kind_name_is_messages_retention() {
        assert_eq!(MessagesRetention::KIND, "messages.retention");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_job_deletes_what_is_past_the_window_at_the_clock_and_reports_counts(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let ws = zirv_flow_store::create_workspace(
            &pool,
            &zirv_flow_core::WorkspaceName::parse("ours").unwrap(),
            &zirv_flow_core::Slug::parse("ours").unwrap(),
        )
        .await
        .unwrap()
        .id;
        let person = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        for (days, node) in [(1_i64, "fresh"), (500, "old")] {
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, created_at, completed_at) \
                 VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), $3, gen_random_uuid(), \
                    gen_random_uuid(), 'sent', $4, $4)",
            )
            .bind(ws.as_uuid())
            .bind(person)
            .bind(node)
            .bind(now - time::Duration::days(days))
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
            &[MessagesRetention::KIND],
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
            .with_message_retention(MessageRetention::default());
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let guard = ::metrics::set_default_local_recorder(&recorder);
        MessagesRetention
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        drop(guard);
        zirv_flow_store::commit(tx).await.unwrap();
        let audit = zirv_flow_store::list_audit(&pool, 10).await.unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, "retention.ran");
        assert_eq!(audit[0].actor_kind, "system");
        assert_eq!(audit[0].summary["job"], "messages.retention");
        assert_eq!(audit[0].summary["counts"]["messages_deleted"], 1);

        let left: Vec<String> = sqlx::query_scalar("SELECT node_id FROM messages")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, ["fresh"]);
        let output = handle.render();
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"messages\"} 1"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_window_deletes_that_workspaces_messages_only(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let now = OffsetDateTime::now_utc();
        let mut workspaces = Vec::new();
        for slug in ["short", "long"] {
            let ws = zirv_flow_store::create_workspace(
                &pool,
                &zirv_flow_core::WorkspaceName::parse(slug).unwrap(),
                &zirv_flow_core::Slug::parse(slug).unwrap(),
            )
            .await
            .unwrap()
            .id;
            let person = uuid::Uuid::now_v7();
            sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
                .bind(person)
                .bind(ws.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                    template_id, destination_id, status, created_at, completed_at) \
                 VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), 'aged', gen_random_uuid(), \
                    gen_random_uuid(), 'sent', $3, $3)",
            )
            .bind(ws.as_uuid())
            .bind(person)
            .bind(now - time::Duration::days(100))
            .execute(&pool)
            .await
            .unwrap();
            workspaces.push(ws);
        }
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();
        zirv_flow_store::set_workspace_retention(
            &mut tx,
            workspaces[0],
            Some(Some(60)),
            None,
            None,
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();
        let mut leader = crate::Scheduler::new(pool.clone(), crate::SchedulerConfig::default());
        leader.tick_at(now + time::Duration::days(2)).await.unwrap();
        let claimed = zirv_flow_store::claim_job(
            &pool,
            "w",
            &[MessagesRetention::KIND],
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
            .with_message_retention(MessageRetention::default());

        MessagesRetention
            .run(&mut ctx, claimed.payload.clone())
            .await
            .unwrap();
        tx.rollback().await.unwrap();

        let left: Vec<uuid::Uuid> = sqlx::query_scalar("SELECT workspace_id FROM messages")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(left, [workspaces[1].as_uuid()]);
        assert_eq!(super::super::recorded_run(&pool, "messages").await, Some(1));
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
        let person = uuid::Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        // One full batch of old messages, then a newest one that a trigger refuses to delete.
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, \
                template_id, destination_id, status, created_at, completed_at) \
             SELECT gen_random_uuid(), $1, $2, gen_random_uuid(), \
                CASE WHEN n = 5001 THEN 'last' ELSE 'old' END, gen_random_uuid(), \
                gen_random_uuid(), 'sent', \
                $3 - make_interval(days => 500) + make_interval(secs => n), \
                $3 - make_interval(days => 500) + make_interval(secs => n) \
             FROM generate_series(1, 5001) n",
        )
        .bind(ws.as_uuid())
        .bind(person)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE FUNCTION refuse_last() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN IF OLD.node_id = 'last' THEN RAISE EXCEPTION 'refused'; END IF; \
             RETURN OLD; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse_last BEFORE DELETE ON messages \
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
            &[MessagesRetention::KIND],
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
            .with_message_retention(MessageRetention::default());
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        let guard = ::metrics::set_default_local_recorder(&recorder);
        let result = MessagesRetention
            .run(&mut ctx, claimed.payload.clone())
            .await;
        drop(guard);
        tx.rollback().await.unwrap();

        assert!(result.is_err());
        assert_eq!(
            super::super::recorded_run(&pool, "messages").await,
            Some(5000)
        );
        let output = handle.render();
        assert!(
            output.contains("zirv_flow_retention_deleted_total{table=\"messages\"} 5000"),
            "{output}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_migration_schedules_the_job_daily_at_0341_utc(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();

        let (workspace, cron, enabled): (Option<uuid::Uuid>, String, bool) = sqlx::query_as(
            "SELECT workspace_id, cron, enabled FROM schedules WHERE kind = 'messages.retention'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(
            (workspace, cron.as_str(), enabled),
            (None, "41 3 * * *", true)
        );
    }
}
