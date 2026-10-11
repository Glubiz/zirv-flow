//! What a running handler sees of its job.

use serde_json::Value;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;
use zirv_flow_store::{ClaimedJob, NewJob, PgConnection};

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

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_handler_sees_its_job_and_enqueues_in_its_own_transaction(pool: PgPool) {
        zirv_flow_store::migrate(&pool).await.unwrap();
        let mut first = NewJob::new("a.kind");
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
        assert!(ctx.enqueue(&NewJob::new("b.kind")).await.unwrap());
        let in_tx = zirv_flow_store::queue_counts(ctx.conn()).await.unwrap();
        assert_eq!(in_tx.ready, 1);
        drop(tx);

        // Rolled back with the transaction.
        let after = zirv_flow_store::queue_counts(&pool).await.unwrap();
        assert_eq!((after.ready, after.running), (0, 1));
    }
}
