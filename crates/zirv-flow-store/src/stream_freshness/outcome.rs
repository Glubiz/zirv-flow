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
