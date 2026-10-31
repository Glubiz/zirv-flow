//! The workspace summary behind the Home figures: aggregate counts over a window, with no person
//! data. Every count names the workspace and a time column that has an index leading with both
//! (`0085_summary_indexes.sql`), so the cost follows the window and not the history. All reads
//! share one `REPEATABLE READ READ ONLY` snapshot under a statement timeout.

use std::time::Duration;

use hashbrown::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::StoreError;
use crate::bounded::begin_snapshot_for;
use crate::flows::SETTINGS_GOAL_NODE;
use crate::pool::PgPool;

/// How long the summary reads may run.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What one published flow did in the window.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlowFigures {
    pub flow_id: Uuid,
    /// Enrollments in flight now: `active`, `waiting` or `held`.
    pub people_in_flow: i64,
    /// Messages of the flow that ended `sent` in the window.
    pub sent: i64,
    /// Goal conversions of the flow in the window; `None` when its current version has no goal.
    pub goals_reached: Option<i64>,
}

/// The counts of a workspace over `[from, to)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSummary {
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

/// The goal step rows of a workspace in a window, by flow. A `goal` node is found in the graph
/// of the version the enrollment entered by.
const GOALS_BY_FLOW: &str = "SELECT e.flow_id, count(*) \
     FROM step_executions s \
     JOIN enrollments e ON e.id = s.enrollment_id \
     JOIN flow_versions v ON v.flow_id = e.flow_id AND v.version = e.flow_version \
     WHERE s.workspace_id = $1 AND s.started_at >= $2 AND s.started_at < $3 \
       AND (s.node_id = $4 \
            OR (s.outcome = 'done' AND jsonb_typeof(v.graph -> 'nodes') = 'array' \
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(v.graph -> 'nodes') n \
                            WHERE n ->> 'id' = s.node_id AND n ->> 'type' = 'goal'))) \
     GROUP BY e.flow_id";

/// The published flows of a workspace and whether the version each runs has a goal.
const PUBLISHED_FLOWS: &str = "SELECT f.id, \
            (v.settings ->> 'goal') IS NOT NULL \
            OR (jsonb_typeof(v.graph -> 'nodes') = 'array' \
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(v.graph -> 'nodes') n \
                            WHERE n ->> 'type' = 'goal')) \
     FROM flows f \
     JOIN flow_versions v ON v.flow_id = f.id AND v.version = f.current_version \
     WHERE f.workspace_id = $1 AND f.state = 'published' ORDER BY f.id";

/// The summary of `workspace` over `[from, to)`.
pub async fn workspace_summary(
    pool: &PgPool,
    workspace: WorkspaceId,
    from: OffsetDateTime,
    to: OffsetDateTime,
) -> Result<WorkspaceSummary, StoreError> {
    let ws = workspace.as_uuid();
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    let messages_sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages \
         WHERE workspace_id = $1 AND status = 'sent' AND NOT test \
           AND completed_at >= $2 AND completed_at < $3",
    )
    .bind(ws)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let people_created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM people \
         WHERE workspace_id = $1 AND created_at >= $2 AND created_at < $3",
    )
    .bind(ws)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let events_received: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events \
         WHERE workspace_id = $1 AND received_at >= $2 AND received_at < $3",
    )
    .bind(ws)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let goals: Vec<(Uuid, i64)> = sqlx::query_as(GOALS_BY_FLOW)
        .bind(ws)
        .bind(from)
        .bind(to)
        .bind(SETTINGS_GOAL_NODE)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::bounded)?;
    let published: Vec<(Uuid, bool)> = sqlx::query_as(PUBLISHED_FLOWS)
        .bind(ws)
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::bounded)?;
    let ids: Vec<Uuid> = published.iter().map(|(id, _)| *id).collect();
    let in_flow: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT flow_id, count(*) FROM enrollments \
         WHERE flow_id = ANY($1) AND workspace_id = $2 AND state IN ('active', 'waiting', 'held') \
         GROUP BY flow_id",
    )
    .bind(&ids)
    .bind(ws)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let sent: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT flow_id, count(*) FROM messages \
         WHERE workspace_id = $1 AND status = 'sent' AND NOT test \
           AND completed_at >= $2 AND completed_at < $3 AND flow_id = ANY($4) \
         GROUP BY flow_id",
    )
    .bind(ws)
    .bind(from)
    .bind(to)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    drop(tx);
    let by_flow = |rows: &[(Uuid, i64)]| -> HashMap<Uuid, i64> { rows.iter().copied().collect() };
    let (in_flow, sent_by, goals_by) = (by_flow(&in_flow), by_flow(&sent), by_flow(&goals));
    let figure = |map: &HashMap<Uuid, i64>, flow: Uuid| map.get(&flow).copied().unwrap_or(0);
    Ok(WorkspaceSummary {
        messages_sent,
        goals_reached: goals.iter().map(|(_, count)| count).sum(),
        people_created,
        events_received,
        flows: published
            .into_iter()
            .map(|(flow_id, has_goal)| FlowFigures {
                flow_id,
                people_in_flow: figure(&in_flow, flow_id),
                sent: figure(&sent_by, flow_id),
                goals_reached: has_goal.then(|| figure(&goals_by, flow_id)),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as Span;
    use time::macros::datetime;

    use super::*;
    use crate::flows::tests_support::{create, publish, workspace};
    use crate::flows::{FlowPatch, update_flow_draft};

    const FROM: OffsetDateTime = datetime!(2030-01-08 00:00 UTC);
    const TO: OffsetDateTime = datetime!(2030-01-15 00:00 UTC);

    async fn person(pool: &PgPool, ws: WorkspaceId, created_at: OffsetDateTime) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id, created_at) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(ws.as_uuid())
            .bind(created_at)
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn event(pool: &PgPool, ws: WorkspaceId, person: Uuid, received_at: OffsetDateTime) {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO events (id, workspace_id, person_id, name, occurred_at, received_at, \
                                 source, trace_id, validation_state) \
             VALUES ($1, $2, $3, 'order', $4, $4, 'api', $1, 'valid')",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(received_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn message(
        pool: &PgPool,
        ws: WorkspaceId,
        person: Uuid,
        flow: Option<Uuid>,
        status: &str,
        test: bool,
        completed_at: OffsetDateTime,
    ) {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id, \
                                   flow_version, node_id, template_id, destination_id, status, \
                                   reason, test, created_at, completed_at) \
             VALUES ($1, $2, $3, $1, $4, 1, 'send', $1, $1, $5, \
                     CASE WHEN $5 = 'sent' THEN NULL ELSE 'x' END, $6, $7, $7)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(person)
        .bind(flow)
        .bind(status)
        .bind(test)
        .bind(completed_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn enrollment(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        person: Uuid,
        state: &str,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, ended_at) \
             VALUES ($1, $2, $3, 1, $4, $5, 'draft', $1, $1, 'api', 't', \
                     CASE WHEN $5 = 'failed' THEN 'boom' END, \
                     CASE WHEN $5 = 'failed' THEN 'draft' END, \
                     CASE WHEN $5 IN ('completed', 'exited') THEN now() END)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(flow)
        .bind(person)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn step(
        pool: &PgPool,
        ws: WorkspaceId,
        enrollment: Uuid,
        node: &str,
        started_at: OffsetDateTime,
    ) {
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome, \
                                          started_at) \
             VALUES ($1, $2, $3, $4, 'done', $5)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(enrollment)
        .bind(node)
        .bind(started_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn published(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let flow = create(pool, ws, name).await.id;
        if name == "Live" {
            // Version 1 holds a `goal` node named `win`.
            let patch = FlowPatch {
                graph: Some(json!({"v": 1, "nodes": [{"id": "win", "type": "goal"}], "edges": []})),
                ..FlowPatch::default()
            };
            let mut connection = pool.acquire().await.unwrap();
            update_flow_draft(&mut connection, ws, flow, &patch)
                .await
                .unwrap();
        }
        publish(pool, ws, flow).await.unwrap();
        flow
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn counts_include_the_start_of_the_window_and_exclude_its_end(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let before = FROM - Span::microseconds(1);
        let inside = person(&pool, ws, FROM).await;
        person(&pool, ws, before).await;
        person(&pool, ws, TO).await;
        for at in [FROM, TO - Span::microseconds(1), before, TO] {
            event(&pool, ws, inside, at).await;
            message(&pool, ws, inside, None, "sent", false, at).await;
        }
        // Not sent, or a test send.
        message(&pool, ws, inside, None, "failed", false, FROM).await;
        message(&pool, ws, inside, None, "sent", true, FROM).await;

        let summary = workspace_summary(&pool, ws, FROM, TO).await.unwrap();

        assert_eq!(summary.people_created, 1);
        assert_eq!(summary.events_received, 2);
        assert_eq!(summary.messages_sent, 2);
        assert_eq!(summary.goals_reached, 0);
        assert!(summary.flows.is_empty());
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
        let inside = FROM + Span::days(1);
        let one = person(&pool, ws, inside).await;
        let two = person(&pool, ws, inside).await;
        let three = person(&pool, ws, inside).await;
        let waiting = enrollment(&pool, ws, live, one, "waiting").await;
        let converted = enrollment(&pool, ws, live, two, "completed").await;
        let late = enrollment(&pool, ws, live, three, "completed").await;
        enrollment(&pool, ws, paused, one, "active").await;
        // A goal-node row and a settings-goal row inside the window count; another node, and a
        // goal row at the end of the window, do not.
        step(&pool, ws, converted, "win", inside).await;
        step(&pool, ws, waiting, SETTINGS_GOAL_NODE, inside).await;
        step(&pool, ws, converted, "other", inside).await;
        step(&pool, ws, late, "win", TO).await;
        message(&pool, ws, one, Some(live), "sent", false, inside).await;
        message(&pool, ws, one, Some(live), "sent", false, inside).await;
        message(&pool, ws, one, Some(live), "sent", false, TO).await;

        let summary = workspace_summary(&pool, ws, FROM, TO).await.unwrap();

        assert_eq!(
            summary.flows,
            [
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
            ]
        );
        assert_eq!(summary.goals_reached, 2);
        assert_eq!(summary.messages_sent, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_rows_are_not_counted(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        let flow = published(&pool, theirs, "Theirs").await;
        let inside = FROM + Span::days(1);
        let stranger = person(&pool, theirs, inside).await;
        event(&pool, theirs, stranger, inside).await;
        message(&pool, theirs, stranger, Some(flow), "sent", false, inside).await;
        let enrolled = enrollment(&pool, theirs, flow, stranger, "active").await;
        step(&pool, theirs, enrolled, SETTINGS_GOAL_NODE, inside).await;

        let summary = workspace_summary(&pool, ours, FROM, TO).await.unwrap();

        assert_eq!(
            summary,
            WorkspaceSummary {
                messages_sent: 0,
                goals_reached: 0,
                people_created: 0,
                events_received: 0,
                flows: Vec::new(),
            }
        );
    }
}
