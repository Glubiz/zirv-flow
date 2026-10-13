//! The freshness of every enabled stream, computed on read.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{
    Freshness, FreshnessInput, Threshold, WorkspaceId, effective_threshold, freshness,
};

use crate::StoreError;

/// The verdict on one stream and what it was computed from.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamFreshness {
    pub workspace_id: Uuid,
    pub source_id: Uuid,
    pub stream_id: Uuid,
    pub state: Freshness,
    pub last_success_at: Option<OffsetDateTime>,
    /// Seconds since the stream last proved itself current (its last success, or when it was
    /// enabled if that is later).
    pub age_seconds: i64,
    /// `None` when the stream never turns stale.
    pub threshold_seconds: Option<i64>,
    /// Whether the threshold is the schedule default and not the stream's own.
    pub threshold_is_default: bool,
    /// The stream's checkpoint, opaque and possibly personal: the API shows it to admins only.
    pub high_water_mark: Option<Value>,
}

#[derive(FromRow)]
struct Row {
    workspace_id: Uuid,
    source_id: Uuid,
    stream_id: Uuid,
    enabled_at: OffsetDateTime,
    last_success_at: Option<OffsetDateTime>,
    last_run_failed: bool,
    staleness_threshold_seconds: Option<i32>,
    checkpoint: Option<Value>,
    schedule: Option<String>,
}

/// The state of the enabled streams of enabled sources, ordered by workspace, source and stream
/// name. `workspace: None` reads every workspace and is for the scheduler leader only; the API
/// always passes its workspace. `source` narrows to one source.
pub async fn stream_freshness<'e>(
    executor: impl PgExecutor<'e>,
    workspace: Option<WorkspaceId>,
    source: Option<Uuid>,
    now: OffsetDateTime,
) -> Result<Vec<StreamFreshness>, StoreError> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.workspace_id, s.source_id, s.id AS stream_id, \
                COALESCE(s.enabled_at, s.created_at) AS enabled_at, s.last_success_at, \
                s.last_run_failed, s.staleness_threshold_seconds, s.checkpoint, src.schedule \
         FROM streams s JOIN sources src ON src.id = s.source_id \
         WHERE s.enabled AND src.enabled \
           AND ($1::uuid IS NULL OR s.workspace_id = $1) \
           AND ($2::uuid IS NULL OR s.source_id = $2) \
         ORDER BY s.workspace_id, s.source_id, s.name",
    )
    .bind(workspace.map(|workspace| workspace.as_uuid()))
    .bind(source)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;

    // The default threshold costs several cron evaluations; streams of one source share it.
    let mut defaults: HashMap<String, Option<Threshold>> = HashMap::new();
    Ok(rows
        .into_iter()
        .map(|row| {
            let threshold = match (row.staleness_threshold_seconds, row.schedule.as_deref()) {
                (Some(seconds), _) => effective_threshold(Some(seconds), None, now),
                (None, Some(schedule)) => *defaults
                    .entry(schedule.to_owned())
                    .or_insert_with(|| effective_threshold(None, Some(schedule), now)),
                (None, None) => None,
            };
            let input = FreshnessInput {
                enabled_at: row.enabled_at,
                last_success_at: row.last_success_at,
                last_run_failed: row.last_run_failed,
                threshold: threshold.map(|threshold| threshold.duration),
            };
            StreamFreshness {
                workspace_id: row.workspace_id,
                source_id: row.source_id,
                stream_id: row.stream_id,
                state: freshness(&input, now),
                last_success_at: row.last_success_at,
                age_seconds: zirv_flow_core::freshness::age(&input, now).whole_seconds(),
                threshold_seconds: threshold.map(|threshold| threshold.duration.whole_seconds()),
                threshold_is_default: threshold.is_some_and(|threshold| threshold.is_default),
                high_water_mark: row.checkpoint,
            }
        })
        .collect())
}
