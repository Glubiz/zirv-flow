//! `messages.retention`: deletes ended messages past their window and writes the handled markers
//! of the rows it removes (architecture section 16.3). An instance-level job that a daily
//! schedule enqueues; it needs no workspace and no master key.

use hashbrown::HashMap;
use serde_json::Value;
use tracing::info;
use zirv_flow_store::apply_message_retention_for;

use crate::{JobContext, JobError, JobKind, metrics};

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
        // Workspaces with a shorter window of their own arrive here with the per-workspace
        // setting; none have one yet.
        let report = apply_message_retention_for(&pool, &settings, &HashMap::new(), now).await?;
        metrics::record_retention(
            "messages",
            report.messages_deleted,
            now.unix_timestamp() as f64,
        );
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
