//! The freshness of every enabled stream, computed on read.

use hashbrown::HashMap;

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
    /// For a stream-mode stream: how many messages it is behind over all partitions, from the
    /// watermarks its consumer last saw. `None` for the other modes and before the first fetch.
    pub lag: Option<i64>,
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
    lag: Option<i64>,
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
                s.last_run_failed, s.staleness_threshold_seconds, s.checkpoint, src.schedule, \
                CASE WHEN s.sync_mode = 'stream' THEN \
                    (SELECT sum(GREATEST(o.high_watermark - o.next_offset, 0))::int8 \
                     FROM stream_offsets o \
                     WHERE o.stream_id = s.id AND o.high_watermark IS NOT NULL) END AS lag \
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
                lag: row.lag,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;

    use super::*;
    use crate::stream_freshness::StreamOutcome;
    use crate::stream_freshness::testing::{outcome, report};
    use crate::sync_runs::testing::{source_with_stream, workspace};
    use crate::{StreamSettings, configure_stream, database_now};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_state_follows_the_threshold_the_schedule_and_the_last_outcome(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (source, stream) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let enabled_at: OffsetDateTime =
            sqlx::query_scalar("SELECT enabled_at FROM streams WHERE id = $1")
                .bind(stream)
                .fetch_one(&pool)
                .await
                .unwrap();
        let state = |now| {
            let pool = pool.clone();
            async move { report(&pool, Some(ws), Some(source), now).await.remove(0) }
        };

        // No schedule and no threshold: never stale.
        let none = state(enabled_at + Duration::days(400)).await;
        assert_eq!(none.state, Freshness::Fresh);
        assert_eq!(
            (none.threshold_seconds, none.threshold_is_default),
            (None, false)
        );

        // The schedule gives the default: twice the hourly interval.
        sqlx::query("UPDATE sources SET schedule = '0 * * * *'")
            .execute(&pool)
            .await
            .unwrap();
        let inside = state(enabled_at + Duration::hours(2)).await;
        assert_eq!(
            inside.state,
            Freshness::Fresh,
            "equal to the threshold is fresh"
        );
        assert_eq!(inside.threshold_seconds, Some(7200));
        assert!(inside.threshold_is_default);
        let past = state(enabled_at + Duration::hours(2) + Duration::seconds(1)).await;
        assert_eq!(past.state, Freshness::Stale);
        assert_eq!(past.last_success_at, None);
        assert_eq!(past.age_seconds, 7201);

        // An explicit threshold wins over the schedule.
        sqlx::query("UPDATE streams SET staleness_threshold_seconds = 60")
            .execute(&pool)
            .await
            .unwrap();
        let explicit = state(enabled_at + Duration::seconds(61)).await;
        assert_eq!(explicit.state, Freshness::Stale);
        assert_eq!(explicit.threshold_seconds, Some(60));
        assert!(!explicit.threshold_is_default);

        // A success restarts the age; a failure beats the age.
        let success = enabled_at + Duration::minutes(10);
        outcome(&pool, ws, stream, StreamOutcome::Succeeded, success).await;
        assert_eq!(
            state(success + Duration::seconds(60)).await.state,
            Freshness::Fresh
        );
        assert_eq!(
            state(success + Duration::seconds(61)).await.state,
            Freshness::Stale
        );
        outcome(&pool, ws, stream, StreamOutcome::Failed, success).await;
        assert_eq!(state(success).await.state, Freshness::Failing);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_disabled_stream_and_a_stream_of_a_disabled_source_are_not_reported(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let (disabled_source, _) = source_with_stream(&pool, ws, "off", "full_refresh").await;
        let (source, _) = source_with_stream(&pool, ws, "crm", "full_refresh").await;
        let now = database_now(&pool).await.unwrap();
        assert_eq!(report(&pool, Some(ws), None, now).await.len(), 2);
        sqlx::query("UPDATE sources SET enabled = false WHERE id = $1")
            .bind(disabled_source)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(report(&pool, Some(ws), None, now).await.len(), 1);

        configure_stream(
            &mut pool.acquire().await.unwrap(),
            ws,
            source,
            "contacts",
            &StreamSettings {
                reconcile_schedule: None,
                sweep_limit: 0.5,
                enabled: false,
                sync_mode: "full_refresh",
                cursor_field: None,
                primary_key: &[],
                staleness_threshold_seconds: None,
            },
        )
        .await
        .unwrap();
        assert!(report(&pool, Some(ws), None, now).await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_report_filters_by_workspace_and_source_and_carries_the_checkpoint(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (source_one, stream_one) = source_with_stream(&pool, one, "crm", "full_refresh").await;
        let (source_two, stream_two) = source_with_stream(&pool, two, "crm", "full_refresh").await;
        sqlx::query("UPDATE streams SET checkpoint = $2 WHERE id = $1")
            .bind(stream_one)
            .bind(json!({"offset": 5}))
            .execute(&pool)
            .await
            .unwrap();
        let now = database_now(&pool).await.unwrap();

        let all = report(&pool, None, None, now).await;
        let own = report(&pool, Some(one), None, now).await;
        let by_source = report(&pool, Some(one), Some(source_one), now).await;
        let foreign_source = report(&pool, Some(one), Some(source_two), now).await;
        let other = report(&pool, Some(two), None, now).await;

        assert_eq!(all.len(), 2);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].stream_id, stream_one);
        assert_eq!(own[0].workspace_id, one.as_uuid());
        assert_eq!(own[0].high_water_mark, Some(json!({"offset": 5})));
        assert_eq!(by_source, own);
        assert!(foreign_source.is_empty(), "a source of another workspace");
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].stream_id, stream_two);
        assert_eq!(other[0].high_water_mark, None);
    }
}
