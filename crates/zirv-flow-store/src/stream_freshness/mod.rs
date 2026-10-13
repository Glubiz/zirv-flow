//! Source freshness (architecture 7.5): the values a stream keeps (`last_success_at`,
//! `last_run_failed`, `enabled_at`), the one writer of them and the one read that turns them
//! into a verdict.
//!
//! The state is derived on every read, never stored. The API and the scheduler leader both call
//! [`stream_freshness`], so they cannot disagree. A record value never appears here.

mod outcome;
mod report;

pub use outcome::{StreamOutcome, record_stream_outcome};
pub use report::{StreamFreshness, stream_freshness};

#[cfg(test)]
mod testing {
    use sqlx::PgPool;
    use time::OffsetDateTime;
    use uuid::Uuid;
    use zirv_flow_core::WorkspaceId;

    use super::*;

    pub(super) async fn report(
        pool: &PgPool,
        ws: Option<WorkspaceId>,
        source: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Vec<StreamFreshness> {
        stream_freshness(pool, ws, source, now).await.unwrap()
    }

    pub(super) async fn outcome(
        pool: &PgPool,
        ws: WorkspaceId,
        stream: Uuid,
        outcome: StreamOutcome,
        at: OffsetDateTime,
    ) -> bool {
        record_stream_outcome(&mut pool.acquire().await.unwrap(), ws, stream, outcome, at)
            .await
            .unwrap()
    }
}
