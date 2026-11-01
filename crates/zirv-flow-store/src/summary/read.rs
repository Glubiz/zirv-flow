//! The read half of the summary: sums the rollup rows of the window's UTC days and adds the
//! per-flow snapshot. Nothing here touches a source table.

use std::time::Duration;

use hashbrown::HashMap;
use time::{Duration as Span, OffsetDateTime, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::rollup::{
    EVENTS_RECEIVED, FLOW_GOALS, FLOW_SENT, GOALS_REACHED, MESSAGES_SENT, PEOPLE_CREATED,
    WORKSPACE_WIDE,
};
use crate::StoreError;
use crate::bounded::begin_snapshot_for;
use crate::pool::PgPool;

/// How long the summary read may run; it touches at most a few hundred rollup rows.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What one published flow did in the window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowFigures {
    pub flow_id: Uuid,
    /// Enrollments in flight at the last rollup run: `active`, `waiting`, `held` or `failed`.
    pub people_in_flow: i64,
    /// Messages of the flow that ended `sent` in the window.
    pub sent: i64,
    /// Goal conversions of the flow in the window; `None` when its current version has no goal.
    pub goals_reached: Option<i64>,
}

/// The counts of a workspace over whole UTC days.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSummary {
    /// Midnight UTC of the first day counted.
    pub from: OffsetDateTime,
    /// When the rollup last finished a run; `None` before its first.
    pub computed_at: Option<OffsetDateTime>,
    /// Messages that ended `sent`, test sends excluded.
    pub messages_sent: i64,
    /// Goal conversions: a `goal` node that ended `done`, or a settings-goal step row.
    pub goals_reached: i64,
    pub people_created: i64,
    /// Events received, whatever their `occurred_at`.
    pub events_received: i64,
    /// Published flows only, ordered by flow id.
    pub flows: Vec<FlowFigures>,
}

/// The summary of `workspace` over the last `days` UTC days, today included, as of the last
/// rollup run.
pub async fn workspace_summary(
    pool: &PgPool,
    workspace: WorkspaceId,
    days: i64,
    now: OffsetDateTime,
) -> Result<WorkspaceSummary, StoreError> {
    let ws = workspace.as_uuid();
    let first_day = now.to_offset(UtcOffset::UTC).date() - Span::days(days - 1);
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    let sums: Vec<(String, Uuid, i64)> = sqlx::query_as(
        "SELECT metric, flow_id, sum(count)::bigint FROM summary_daily \
         WHERE workspace_id = $1 AND day >= $2 GROUP BY metric, flow_id",
    )
    .bind(ws)
    .bind(first_day)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let flows: Vec<(Uuid, i64, bool)> = sqlx::query_as(
        "SELECT flow_id, people_in_flow, has_goal FROM summary_flows \
         WHERE workspace_id = $1 ORDER BY flow_id",
    )
    .bind(ws)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let computed_at: Option<Option<OffsetDateTime>> =
        sqlx::query_scalar("SELECT computed_at FROM summary_state WHERE workspace_id = $1")
            .bind(ws)
            .fetch_optional(&mut *tx)
            .await
            .map_err(StoreError::bounded)?;
    drop(tx);
    let mut wide: HashMap<&str, i64> = HashMap::new();
    let mut sent: HashMap<Uuid, i64> = HashMap::new();
    let mut goals: HashMap<Uuid, i64> = HashMap::new();
    for (metric, flow, count) in &sums {
        match (metric.as_str(), *flow == WORKSPACE_WIDE) {
            (FLOW_SENT, false) => {
                sent.insert(*flow, *count);
            }
            (FLOW_GOALS, false) => {
                goals.insert(*flow, *count);
            }
            (name, true) => {
                wide.insert(name, *count);
            }
            _ => {}
        }
    }
    let total = |metric: &str| wide.get(metric).copied().unwrap_or(0);
    Ok(WorkspaceSummary {
        from: first_day.midnight().assume_utc(),
        computed_at: computed_at.flatten(),
        messages_sent: total(MESSAGES_SENT),
        goals_reached: total(GOALS_REACHED),
        people_created: total(PEOPLE_CREATED),
        events_received: total(EVENTS_RECEIVED),
        flows: flows
            .into_iter()
            .map(|(flow_id, people_in_flow, has_goal)| FlowFigures {
                flow_id,
                people_in_flow,
                sent: sent.get(&flow_id).copied().unwrap_or(0),
                goals_reached: has_goal.then(|| goals.get(&flow_id).copied().unwrap_or(0)),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::super::refresh_workspace_summary;
    use super::super::support::{enrollment, event, message, person, published, step};
    use super::*;
    use crate::flows::SETTINGS_GOAL_NODE;
    use crate::flows::tests_support::{create, workspace};

    const NOW: OffsetDateTime = datetime!(2030-01-15 12:00 UTC);

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn before_the_first_rollup_run_every_figure_is_zero_and_the_age_unknown(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;

        let summary = workspace_summary(&pool, ws, 7, NOW).await.unwrap();

        assert_eq!(summary.computed_at, None);
        assert_eq!(summary.from, datetime!(2030-01-09 00:00 UTC));
        assert_eq!(
            (
                summary.messages_sent,
                summary.people_created,
                summary.events_received
            ),
            (0, 0, 0)
        );
        assert!(summary.flows.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_window_sums_the_rollup_rows_of_its_days_only(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let someone = person(&pool, ws, datetime!(2030-01-02 01:00 UTC)).await;
        // People on the 8th (outside 7 days), the 9th (first day) and the 15th.
        person(&pool, ws, datetime!(2030-01-08 23:59 UTC)).await;
        person(&pool, ws, datetime!(2030-01-09 00:00 UTC)).await;
        person(&pool, ws, datetime!(2030-01-15 09:00 UTC)).await;
        for at in [
            datetime!(2030-01-08 10:00 UTC),
            datetime!(2030-01-12 10:00 UTC),
        ] {
            event(&pool, ws, someone, at).await;
            message(&pool, ws, someone, None, "sent", false, at).await;
        }
        let day12 = datetime!(2030-01-12 10:00 UTC);
        message(&pool, ws, someone, None, "sent", true, day12).await;
        message(&pool, ws, someone, None, "failed", false, day12).await;
        refresh_workspace_summary(&pool, ws, NOW).await.unwrap();

        let week = workspace_summary(&pool, ws, 7, NOW).await.unwrap();
        let day = workspace_summary(&pool, ws, 1, NOW).await.unwrap();

        assert_eq!(week.computed_at, Some(NOW));
        assert_eq!(week.people_created, 2);
        assert_eq!(week.events_received, 1);
        assert_eq!(week.messages_sent, 1);
        assert_eq!(day.people_created, 1);
        assert_eq!(day.events_received, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flows_lists_published_flows_only_with_their_own_figures(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let live = published(&pool, ws, "Live").await;
        let paused = published(&pool, ws, "Paused").await;
        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(paused)
            .execute(&pool)
            .await
            .unwrap();
        let plain = published(&pool, ws, "Plain").await;
        create(&pool, ws, "Draft").await;
        let inside = datetime!(2030-01-14 09:00 UTC);
        let one = person(&pool, ws, inside).await;
        let two = person(&pool, ws, inside).await;
        let three = person(&pool, ws, inside).await;
        let waiting = enrollment(&pool, ws, live, one, "waiting").await;
        let converted = enrollment(&pool, ws, live, two, "completed").await;
        let late = enrollment(&pool, ws, live, three, "completed").await;
        enrollment(&pool, ws, paused, one, "active").await;
        // A goal-node row and a settings-goal row inside the window count; another node and a
        // goal row after the rollup cut-off do not.
        step(&pool, ws, converted, "win", inside).await;
        step(&pool, ws, waiting, SETTINGS_GOAL_NODE, inside).await;
        step(&pool, ws, converted, "other", inside).await;
        step(&pool, ws, late, "win", NOW).await;
        message(&pool, ws, one, Some(live), "sent", false, inside).await;
        message(&pool, ws, one, Some(live), "sent", false, inside).await;
        message(&pool, ws, one, Some(live), "sent", false, NOW).await;
        refresh_workspace_summary(&pool, ws, NOW).await.unwrap();

        let summary = workspace_summary(&pool, ws, 7, NOW).await.unwrap();

        let mut expected = [
            FlowFigures {
                flow_id: live,
                people_in_flow: 1,
                sent: 2,
                goals_reached: Some(2),
            },
            FlowFigures {
                flow_id: plain,
                people_in_flow: 0,
                sent: 0,
                goals_reached: None,
            },
        ];
        expected.sort_by_key(|figures| figures.flow_id);
        assert_eq!(summary.flows, expected);
        assert_eq!(summary.goals_reached, 2);
        assert_eq!(summary.messages_sent, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_whose_only_enrollments_failed_still_has_people_in_flow(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let flow = published(&pool, ws, "Plain").await;
        let someone = person(&pool, ws, NOW).await;
        enrollment(&pool, ws, flow, someone, "failed").await;
        refresh_workspace_summary(&pool, ws, NOW).await.unwrap();

        let summary = workspace_summary(&pool, ws, 7, NOW).await.unwrap();

        assert_eq!(summary.flows.len(), 1);
        assert_eq!(summary.flows[0].people_in_flow, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_rows_are_not_counted(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        let flow = published(&pool, theirs, "Theirs").await;
        let inside = datetime!(2030-01-14 09:00 UTC);
        let stranger = person(&pool, theirs, inside).await;
        event(&pool, theirs, stranger, inside).await;
        message(&pool, theirs, stranger, Some(flow), "sent", false, inside).await;
        let enrolled = enrollment(&pool, theirs, flow, stranger, "active").await;
        step(&pool, theirs, enrolled, SETTINGS_GOAL_NODE, inside).await;
        refresh_workspace_summary(&pool, ours, NOW).await.unwrap();
        refresh_workspace_summary(&pool, theirs, NOW).await.unwrap();

        let summary = workspace_summary(&pool, ours, 7, NOW).await.unwrap();

        assert_eq!(
            summary,
            WorkspaceSummary {
                from: datetime!(2030-01-09 00:00 UTC),
                computed_at: Some(NOW),
                messages_sent: 0,
                goals_reached: 0,
                people_created: 0,
                events_received: 0,
                flows: Vec::new(),
            }
        );
        let theirs_summary = workspace_summary(&pool, theirs, 7, NOW).await.unwrap();
        assert_eq!(theirs_summary.messages_sent, 1);
    }
}
