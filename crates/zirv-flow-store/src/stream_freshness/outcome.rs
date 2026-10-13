//! The one writer of `streams.last_success_at` and `streams.last_run_failed`.

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;

/// What a source reports about a stream. A pull run reports it when it ends; a webhook source
/// reports `Succeeded` with the time of the last record it received; a stream consumer reports
/// `Succeeded` while its lag is within bound and `Failed` when it is not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamOutcome {
    Succeeded,
    Failed,
}

/// Records an outcome for a stream of the workspace. Use the caller's transaction so the
/// report commits with whatever ended the run.
///
/// `Succeeded` sets `last_success_at = GREATEST(last_success_at, at)`, so it never moves
/// backwards, and clears `last_run_failed`. `Failed` sets `last_run_failed` and leaves
/// `last_success_at`. Returns whether the workspace has such a stream.
pub async fn record_stream_outcome(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    stream_id: Uuid,
    outcome: StreamOutcome,
    at: OffsetDateTime,
) -> Result<bool, StoreError> {
    let sql = match outcome {
        StreamOutcome::Succeeded => {
            "UPDATE streams SET last_success_at = GREATEST(last_success_at, $3), \
                last_run_failed = false \
             WHERE workspace_id = $1 AND id = $2"
        }
        StreamOutcome::Failed => {
            "UPDATE streams SET last_run_failed = true WHERE workspace_id = $1 AND id = $2"
        }
    };
    let mut query = sqlx::query(sql).bind(workspace.as_uuid()).bind(stream_id);
    if outcome == StreamOutcome::Succeeded {
        query = query.bind(at);
    }
    let done = query.execute(connection).await.map_err(StoreError::query)?;
    Ok(done.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::Duration;

    use super::*;
    use crate::stream_freshness::testing::outcome;
    use crate::sync_runs::testing::{source_with_stream, workspace};
    use crate::{database_now, find_stream_by_id};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_outcome_succeeds_fails_recovers_and_never_moves_the_success_backwards(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let at = database_now(&pool).await.unwrap();
        let read = || async {
            let found = find_stream_by_id(&pool, ws, source, stream)
                .await
                .unwrap()
                .unwrap();
            (found.last_success_at, found.last_run_failed)
        };

        assert!(outcome(&pool, ws, stream, StreamOutcome::Failed, at).await);
        assert_eq!(read().await, (None, true));
        assert!(outcome(&pool, ws, stream, StreamOutcome::Succeeded, at).await);
        assert_eq!(read().await, (Some(at), false), "a success recovers");
        let earlier = at - Duration::hours(1);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, earlier).await;
        assert_eq!(read().await, (Some(at), false), "never backwards");
        let later = at + Duration::hours(1);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, later).await;
        assert_eq!(read().await, (Some(later), false));
        outcome(&pool, ws, stream, StreamOutcome::Failed, later).await;
        assert_eq!(
            read().await,
            (Some(later), true),
            "a failure keeps the success"
        );
        assert!(
            !outcome(&pool, other, stream, StreamOutcome::Succeeded, later).await,
            "another workspace reports on nothing"
        );
    }
}
