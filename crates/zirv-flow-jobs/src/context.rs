//! What a running handler sees of its job.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};
use zirv_flow_store::{ClaimedJob, NewDecision, NewJob, PgConnection};

use crate::JobError;

/// The handler's view of one claimed job and the transaction it runs in.
///
/// Everything the handler writes through [`conn`](Self::conn), and every job it enqueues,
/// commits together with the job's completion or not at all. A handler that returns an
/// error, or loses its lease, leaves no trace.
pub struct JobContext<'a> {
    connection: &'a mut PgConnection,
    job: &'a ClaimedJob,
}

impl<'a> JobContext<'a> {
    pub(crate) fn new(connection: &'a mut PgConnection, job: &'a ClaimedJob) -> Self {
        Self { connection, job }
    }

    /// The handler's transaction.
    pub fn conn(&mut self) -> &mut PgConnection {
        self.connection
    }

    pub fn job_id(&self) -> Uuid {
        self.job.id
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        self.job.workspace_id.map(WorkspaceId::from_uuid)
    }

    /// The attempt number, starting at 1.
    pub fn attempts(&self) -> i32 {
        self.job.attempts
    }

    pub fn unique_key(&self) -> Option<&str> {
        self.job.unique_key.as_deref()
    }

    /// The payload the job was enqueued with, for handlers that need to pass it on.
    pub fn payload(&self) -> &Value {
        &self.job.payload
    }

    /// The trace this job belongs to. Pass it on when enqueueing follow-up work, so the
    /// work stays on one trace.
    pub fn trace(&self) -> &TraceContext {
        &self.job.trace
    }

    /// Appends a decision to the workspace's log, in the handler's transaction, under this
    /// job's trace and cause. Only durable ids go in `subject_ids`; an instance-level job
    /// has no workspace to record in and gets an error.
    pub async fn record_decision(
        &mut self,
        action: &'static str,
        outcome: &'static str,
        reason: Option<&'static str>,
        subject_ids: Vec<(&'static str, Uuid)>,
    ) -> Result<(), JobError> {
        let Some(workspace_id) = self.workspace_id() else {
            return Err(JobError::new(
                "an instance-level job has no workspace to record a decision in",
            ));
        };
        let decision = NewDecision {
            workspace_id,
            trace: self.job.trace.clone(),
            action,
            outcome,
            reason,
            subject_ids,
            job: Some((self.job.id, self.job.attempts)),
        };
        Ok(zirv_flow_store::append_decision(&mut *self.connection, &decision).await?)
    }

    /// Enqueues a follow-up job in the handler's transaction. False when its unique key is
    /// already held.
    pub async fn enqueue(&mut self, job: &NewJob) -> Result<bool, JobError> {
        Ok(zirv_flow_store::enqueue(&mut *self.connection, job).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;
    use zirv_flow_core::Cause;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_sees_its_job_and_enqueues_in_its_own_transaction(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let trace = TraceContext::root(Cause::api("req-1"));
        let mut first = NewJob::new("a.kind", trace.clone());
        first.payload = serde_json::json!({"n": 1});
        first.unique_key = Some("k".into());
        zirv_flow_store::enqueue(&pool, &first).await.unwrap();
        let claimed =
            zirv_flow_store::claim_job(&pool, "w", &["a.kind"], Duration::from_secs(30), None)
                .await
                .unwrap()
                .unwrap();
        let mut tx = zirv_flow_store::begin(&pool).await.unwrap();

        let mut ctx = JobContext::new(&mut tx, &claimed);
        assert_eq!(ctx.job_id(), claimed.id);
        assert_eq!(ctx.attempts(), 1);
        assert_eq!(ctx.unique_key(), Some("k"));
        assert_eq!(ctx.workspace_id(), None);
        assert_eq!(ctx.payload(), &serde_json::json!({"n": 1}));
        assert_eq!(ctx.trace(), &trace);
        assert!(
            ctx.enqueue(&NewJob::new("b.kind", ctx.trace().clone()))
                .await
                .unwrap()
        );
        let in_tx = zirv_flow_store::queue_counts(ctx.conn()).await.unwrap();
        assert_eq!(in_tx.ready, 1);
        drop(tx);

        // Rolled back with the transaction.
        let after = zirv_flow_store::queue_counts(&pool).await.unwrap();
        assert_eq!((after.ready, after.running), (0, 1));
    }

    async fn claimed_in_transaction(
        pool: &PgPool,
        workspace: Option<WorkspaceId>,
    ) -> (ClaimedJob, zirv_flow_store::Tx) {
        let mut new = NewJob::new("a.kind", TraceContext::root(Cause::api("req-2")));
        new.workspace_id = workspace;
        zirv_flow_store::enqueue(pool, &new).await.unwrap();
        let claimed =
            zirv_flow_store::claim_job(pool, "w", &["a.kind"], Duration::from_secs(30), None)
                .await
                .unwrap()
                .unwrap();
        (claimed, zirv_flow_store::begin(pool).await.unwrap())
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_recorded_decision_carries_the_jobs_workspace_trace_cause_and_attempt(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let name = zirv_flow_core::WorkspaceName::parse("ours").unwrap();
        let slug = zirv_flow_core::Slug::parse("ours").unwrap();
        let workspace = zirv_flow_store::create_workspace(&pool, &name, &slug)
            .await
            .unwrap()
            .id;
        let (claimed, mut tx) = claimed_in_transaction(&pool, Some(workspace)).await;
        let subject = Uuid::new_v4();

        let mut ctx = JobContext::new(&mut tx, &claimed);
        ctx.record_decision(
            "segment.evaluated",
            "matched",
            None,
            vec![("segment_id", subject)],
        )
        .await
        .unwrap();
        zirv_flow_store::commit(tx).await.unwrap();

        let (records, _) =
            zirv_flow_store::list_trace(&pool, workspace, claimed.trace.trace_id, 10)
                .await
                .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].action, "segment.evaluated");
        assert_eq!(records[0].outcome, "matched");
        assert_eq!(records[0].cause, claimed.trace.cause);
        assert_eq!(records[0].job_id, Some(claimed.id));
        assert_eq!(records[0].attempt, Some(1));
        assert_eq!(
            records[0].subject_ids,
            serde_json::json!({"segment_id": subject.to_string()})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_instance_level_job_cannot_record_a_decision(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let (claimed, mut tx) = claimed_in_transaction(&pool, None).await;

        let mut ctx = JobContext::new(&mut tx, &claimed);
        let error = ctx
            .record_decision("anything", "done", None, Vec::new())
            .await
            .unwrap_err();

        assert_eq!(
            error.message(),
            "an instance-level job has no workspace to record a decision in"
        );
    }
}
