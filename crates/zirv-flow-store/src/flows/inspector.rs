//! The flow enrollment inspector's reads (architecture 12.3 and 12.4): a flow's enrollments, one
//! enrollment's path, and the per-node counts of a version. Every read runs in one
//! `REPEATABLE READ READ ONLY` snapshot under a statement timeout, so a slow scan is cancelled and
//! reported as a timeout (`StoreError::is_timeout`) and the parts of a response agree.

use std::time::Duration;

use hashbrown::HashMap;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};
use time::{Date, Duration as Span, OffsetDateTime, Time};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use super::runtime::EnrollmentStatus;
use crate::StoreError;
use crate::bounded::begin_snapshot_for;
use crate::decisions::{Decision, list_enrollment_decisions};

/// How long an inspector read may run.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The most decisions one enrollment's path carries.
pub const MAX_PATH_DECISIONS: u32 = 1000;

/// The step row of the flow's goal setting, which no node of the graph has.
pub const SETTINGS_GOAL_NODE: &str = "settings.goal";

/// Which enrollments to list.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnrollmentFilter {
    pub state: Option<EnrollmentStatus>,
    pub version: Option<i32>,
    /// Matches the node an enrollment is at or the node it failed at.
    pub node: Option<String>,
}

/// Where a page ended: the next page starts after this enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EnrollmentCursor {
    pub created_at: OffsetDateTime,
    pub id: Uuid,
}

/// An enrollment as the inspector shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct EnrollmentSummary {
    pub id: Uuid,
    pub person_id: Uuid,
    pub flow_version: i32,
    pub status: EnrollmentStatus,
    pub node: String,
    pub failed_node: Option<String>,
    pub exit_reason: Option<String>,
    pub error: Option<String>,
    pub wake_at: Option<OffsetDateTime>,
    pub trace_id: Uuid,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub ended_at: Option<OffsetDateTime>,
}

#[derive(FromRow)]
struct SummaryRow {
    id: Uuid,
    person_id: Uuid,
    flow_version: i32,
    state: String,
    node: String,
    failed_node: Option<String>,
    exit_reason: Option<String>,
    error: Option<String>,
    wake_at: Option<OffsetDateTime>,
    trace_id: Uuid,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
}

impl SummaryRow {
    fn into_summary(self) -> Result<EnrollmentSummary, StoreError> {
        let status = EnrollmentStatus::parse(&self.state)
            .ok_or_else(|| StoreError::query(sqlx::Error::Decode("unknown state".into())))?;
        Ok(EnrollmentSummary {
            id: self.id,
            person_id: self.person_id,
            flow_version: self.flow_version,
            status,
            node: self.node,
            failed_node: self.failed_node,
            exit_reason: self.exit_reason,
            error: self.error,
            wake_at: self.wake_at,
            trace_id: self.trace_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
            ended_at: self.ended_at,
        })
    }
}

const SUMMARY_COLUMNS: &str = "id, person_id, flow_version, state, node, failed_node, \
     exit_reason, error, wake_at, trace_id, created_at, updated_at, ended_at";

/// A page of a flow's enrollments, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct EnrollmentPage {
    pub enrollments: Vec<EnrollmentSummary>,
    /// Pass as `before` for the next page; absent on the last.
    pub next: Option<EnrollmentCursor>,
}

async fn flow_exists(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM flows WHERE workspace_id = $1 AND id = $2)")
        .bind(workspace.as_uuid())
        .bind(flow)
        .fetch_one(connection)
        .await
        .map_err(StoreError::bounded)
}

/// Appends the statement of [`list_flow_enrollments`]. Only the predicates asked for are in it, so
/// the planner can use the index that matches them.
fn list_query(
    workspace: WorkspaceId,
    flow: Uuid,
    filter: &EnrollmentFilter,
    limit: i64,
    before: Option<EnrollmentCursor>,
) -> QueryBuilder<Postgres> {
    let mut query = QueryBuilder::<Postgres>::new(format!(
        "SELECT {SUMMARY_COLUMNS} FROM enrollments WHERE workspace_id = "
    ));
    query.push_bind(workspace.as_uuid());
    query.push(" AND flow_id = ").push_bind(flow);
    if let Some(state) = filter.state {
        query.push(" AND state = ").push_bind(state.as_str());
    }
    if let Some(version) = filter.version {
        query.push(" AND flow_version = ").push_bind(version);
    }
    if let Some(node) = &filter.node {
        query.push(" AND (node = ").push_bind(node.clone());
        query.push(" OR failed_node = ").push_bind(node.clone());
        query.push(")");
    }
    if let Some(cursor) = before {
        query
            .push(" AND (created_at, id) < (")
            .push_bind(cursor.created_at);
        query.push(", ").push_bind(cursor.id);
        query.push(")");
    }
    query
        .push(" ORDER BY created_at DESC, id DESC LIMIT ")
        .push_bind(limit);
    query
}

/// The enrollments of a flow, newest first by `(created_at, id)`, at most `limit`, after `before`.
/// `None` when the workspace has no such flow.
pub async fn list_flow_enrollments(
    pool: &PgPool,
    workspace: WorkspaceId,
    flow: Uuid,
    filter: &EnrollmentFilter,
    limit: u32,
    before: Option<EnrollmentCursor>,
) -> Result<Option<EnrollmentPage>, StoreError> {
    let limit = i64::from(limit.max(1));
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    if !flow_exists(&mut tx, workspace, flow).await? {
        return Ok(None);
    }
    let rows: Vec<SummaryRow> = list_query(workspace, flow, filter, limit + 1, before)
        .build_query_as()
        .fetch_all(&mut *tx)
        .await
        .map_err(StoreError::bounded)?;
    drop(tx);
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let more = rows.len() > limit;
    let enrollments = rows
        .into_iter()
        .take(limit)
        .map(SummaryRow::into_summary)
        .collect::<Result<Vec<_>, _>>()?;
    let next = if more {
        enrollments.last().map(|last| EnrollmentCursor {
            created_at: last.created_at,
            id: last.id,
        })
    } else {
        None
    };
    Ok(Some(EnrollmentPage { enrollments, next }))
}

/// A node an enrollment has run.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct PathStep {
    pub id: Uuid,
    pub node_id: String,
    pub outcome: String,
    pub error: Option<String>,
    pub attempts: i32,
    pub started_at: OffsetDateTime,
}

/// The event an enrollment is asleep at, with the time it gives up.
#[derive(Clone, Debug, Eq, FromRow, PartialEq)]
pub struct PathEventWait {
    pub event_name: String,
    pub timeout_at: OffsetDateTime,
}

/// One enrollment's path: its steps, its waits and every decision that names it.
#[derive(Clone, Debug, PartialEq)]
pub struct EnrollmentPath {
    pub enrollment: EnrollmentSummary,
    pub steps: Vec<PathStep>,
    pub event_waits: Vec<PathEventWait>,
    pub decisions: Vec<Decision>,
    /// True when older decisions were left out because the enrollment has more than
    /// [`MAX_PATH_DECISIONS`]; the newest are kept.
    pub truncated: bool,
}

/// An enrollment of `flow` with its steps, waits and decisions, or `None` when the workspace has
/// no such enrollment of that flow.
pub async fn load_enrollment_path(
    pool: &PgPool,
    workspace: WorkspaceId,
    flow: Uuid,
    enrollment: Uuid,
) -> Result<Option<EnrollmentPath>, StoreError> {
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    let row: Option<SummaryRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SUMMARY_COLUMNS} FROM enrollments \
         WHERE workspace_id = $1 AND flow_id = $2 AND id = $3"
    )))
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(enrollment)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let summary = row.into_summary()?;
    let steps: Vec<PathStep> = sqlx::query_as(
        "SELECT id, node_id, outcome, error, attempts, started_at FROM step_executions \
         WHERE enrollment_id = $1 ORDER BY started_at, id",
    )
    .bind(enrollment)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let event_waits: Vec<PathEventWait> = sqlx::query_as(
        "SELECT event_name, timeout_at FROM enrollment_waits \
         WHERE enrollment_id = $1 ORDER BY event_name",
    )
    .bind(enrollment)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let (decisions, truncated) = list_enrollment_decisions(
        &mut tx,
        workspace,
        enrollment,
        summary.created_at,
        MAX_PATH_DECISIONS,
    )
    .await?;
    Ok(Some(EnrollmentPath {
        enrollment: summary,
        steps,
        event_waits,
        decisions,
        truncated,
    }))
}

/// What happened at one node of a version, counted from the rows.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodeCount {
    /// Step executions of the node.
    pub entered: i64,
    /// Step executions that ended `done`.
    pub completed: i64,
    /// Enrollments of the version waiting at the node now.
    pub waiting: i64,
    /// Step executions that ended `failed`.
    pub failed: i64,
    /// Messages of a send node that ended `sent`.
    pub sent: i64,
    /// Messages of a send node that ended `failed`.
    pub send_failed: i64,
    /// Messages of a send node that ended `skipped`, by the fixed reason code, sorted by reason.
    pub skipped: Vec<(String, i64)>,
}

/// The counts of every node that has rows, by node id (the settings goal as
/// [`SETTINGS_GOAL_NODE`]), for one version of a flow; computed on read. `None` when the workspace
/// has no such flow version.
pub async fn version_node_counts(
    pool: &PgPool,
    workspace: WorkspaceId,
    flow: Uuid,
    version: i32,
) -> Result<Option<HashMap<String, NodeCount>>, StoreError> {
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM flow_versions v JOIN flows f ON f.id = v.flow_id \
         WHERE f.workspace_id = $1 AND v.flow_id = $2 AND v.version = $3)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(version)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    if !known {
        return Ok(None);
    }
    let steps: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT s.node_id, count(*), count(*) FILTER (WHERE s.outcome = 'done'), \
                count(*) FILTER (WHERE s.outcome = 'failed') \
         FROM enrollments e JOIN step_executions s ON s.enrollment_id = e.id \
         WHERE e.flow_id = $1 AND e.flow_version = $2 AND e.workspace_id = $3 \
         GROUP BY s.node_id",
    )
    .bind(flow)
    .bind(version)
    .bind(workspace.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let waiting: Vec<(String, i64)> = sqlx::query_as(
        "SELECT node, count(*) FROM enrollments \
         WHERE flow_id = $1 AND state = 'waiting' AND flow_version = $2 AND workspace_id = $3 \
         GROUP BY node",
    )
    .bind(flow)
    .bind(version)
    .bind(workspace.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    // Delivery outcomes come from the message rows of this version, by node; test sends carry no
    // flow and a pending or sending message has no outcome yet.
    let deliveries: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT node_id, status, reason, count(*) FROM messages          WHERE workspace_id = $1 AND flow_id = $2 AND flow_version = $3 AND NOT test            AND status IN ('sent', 'skipped', 'failed')          GROUP BY node_id, status, reason ORDER BY node_id, reason",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(version)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    drop(tx);
    let mut counts: HashMap<String, NodeCount> = HashMap::new();
    for (node, status, reason, total) in deliveries {
        let count = counts.entry(node).or_default();
        match status.as_str() {
            "sent" => count.sent += total,
            "failed" => count.send_failed += total,
            _ => count
                .skipped
                .push((reason.unwrap_or_else(|| "unknown".to_owned()), total)),
        }
    }
    for (node, entered, completed, failed) in steps {
        let count = counts.entry(node).or_default();
        count.entered = entered;
        count.completed = completed;
        count.failed = failed;
    }
    for (node, waiting) in waiting {
        counts.entry(node).or_default().waiting = waiting;
    }
    Ok(Some(counts))
}

/// The most days one entries series covers.
pub const MAX_ENTRY_DAYS: u32 = 90;

/// How many people entered a flow on each UTC day of a window, and in the same number of days
/// before it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntriesPerDay {
    /// The first day of the window.
    pub from: Date,
    /// The last day of the window: the day the read ran on.
    pub to: Date,
    /// One item per day, oldest first, including the days nobody entered.
    pub days: Vec<(Date, i64)>,
    pub total: i64,
    pub previous_total: i64,
}

/// Enrollments created per UTC day over the last `days` days (1 to [`MAX_ENTRY_DAYS`]) ending
/// today, for every version of the flow or for one. `None` when the workspace has no such flow.
pub async fn flow_entries_per_day(
    pool: &PgPool,
    workspace: WorkspaceId,
    flow: Uuid,
    days: u32,
    version: Option<i32>,
) -> Result<Option<EntriesPerDay>, StoreError> {
    entries_per_day_until(
        pool,
        workspace,
        flow,
        days,
        version,
        OffsetDateTime::now_utc().date(),
    )
    .await
}

async fn entries_per_day_until(
    pool: &PgPool,
    workspace: WorkspaceId,
    flow: Uuid,
    days: u32,
    version: Option<i32>,
    today: Date,
) -> Result<Option<EntriesPerDay>, StoreError> {
    let span = Span::days(i64::from(days));
    let from = today - span + Span::days(1);
    let midnight = |day: Date| day.with_time(Time::MIDNIGHT).assume_utc();
    let (start, end) = (midnight(from), midnight(today + Span::days(1)));
    let before = midnight(from - span);
    let mut tx = begin_snapshot_for(pool, TIMEOUT).await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM flows WHERE workspace_id = $1 AND id = $2)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    if !known {
        return Ok(None);
    }
    let rows: Vec<(Date, i64)> = sqlx::query_as(
        "SELECT g.day::date, COALESCE(c.entered, 0) \
         FROM generate_series($3::date, $4::date, interval '1 day') AS g(day) \
         LEFT JOIN (SELECT (created_at AT TIME ZONE 'UTC')::date AS day, count(*) AS entered \
                    FROM enrollments \
                    WHERE workspace_id = $1 AND flow_id = $2 \
                      AND created_at >= $5 AND created_at < $6 \
                      AND ($7::int IS NULL OR flow_version = $7) \
                    GROUP BY 1) c ON c.day = g.day::date \
         ORDER BY g.day",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(from)
    .bind(today)
    .bind(start)
    .bind(end)
    .bind(version)
    .fetch_all(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    let previous_total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM enrollments \
         WHERE workspace_id = $1 AND flow_id = $2 AND created_at >= $3 AND created_at < $4 \
           AND ($5::int IS NULL OR flow_version = $5)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(before)
    .bind(start)
    .bind(version)
    .fetch_one(&mut *tx)
    .await
    .map_err(StoreError::bounded)?;
    drop(tx);
    let total = rows.iter().map(|(_, entered)| entered).sum();
    Ok(Some(EntriesPerDay {
        from,
        to: today,
        days: rows,
        total,
        previous_total,
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as Span;

    use super::*;
    use crate::flows::tests_support::{create, edit, publish, trace, workspace};
    use crate::{GraphPoint, NewDecision, append_decision};

    struct Seed {
        ws: WorkspaceId,
        flow: Uuid,
        version: i32,
        state: &'static str,
        node: &'static str,
        failed_node: Option<&'static str>,
        created_at: OffsetDateTime,
    }

    impl Seed {
        fn new(ws: WorkspaceId, flow: Uuid, state: &'static str, node: &'static str) -> Self {
            Self {
                ws,
                flow,
                version: 1,
                state,
                node,
                failed_node: None,
                created_at: OffsetDateTime::now_utc(),
            }
        }
    }

    /// An enrollment of a new person; returns its id.
    async fn seed(pool: &PgPool, seed: Seed) -> Uuid {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(seed.ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO enrollments \
                (id, workspace_id, flow_id, flow_version, person_id, state, node, occurrence_id, \
                 trace_id, cause_kind, cause_id, error, failed_node, created_at, updated_at, \
                 ended_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $1, $1, 'api', 't', \
                     CASE WHEN $6 = 'failed' THEN 'boom' END, $8, $9, $9, \
                     CASE WHEN $6 IN ('completed', 'exited') THEN $9 END)",
        )
        .bind(id)
        .bind(seed.ws.as_uuid())
        .bind(seed.flow)
        .bind(seed.version)
        .bind(person)
        .bind(seed.state)
        .bind(seed.node)
        .bind(seed.failed_node)
        .bind(seed.created_at)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    async fn step(pool: &PgPool, ws: WorkspaceId, enrollment: Uuid, node: &str, outcome: &str) {
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome, error) \
             VALUES ($1, $2, $3, $4, $5, CASE WHEN $5 = 'failed' THEN 'boom' END)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(enrollment)
        .bind(node)
        .bind(outcome)
        .execute(pool)
        .await
        .unwrap();
    }

    fn at(minutes: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + Span::days(20_000) + Span::minutes(minutes)
    }

    async fn published_flow(pool: &PgPool, slug: &str) -> (WorkspaceId, Uuid) {
        let ws = workspace(pool, slug).await;
        let flow = create(pool, ws, "Welcome").await;
        publish(pool, ws, flow.id).await.unwrap();
        edit(pool, ws, flow.id, "second").await;
        publish(pool, ws, flow.id).await.unwrap();
        (ws, flow.id)
    }

    fn ids(page: &EnrollmentPage) -> Vec<Uuid> {
        page.enrollments.iter().map(|e| e.id).collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pages_have_no_duplicates_or_gaps_and_the_filters_narrow_them(pool: PgPool) {
        let (ws, flow) = published_flow(&pool, "list").await;
        // Created at 0, 1, 2, 3, 3 minutes: the last two share a time, so the id orders them.
        let rows = [
            ("failed", "a", Some("b"), 1, 0),
            ("waiting", "b", None, 1, 1),
            ("active", "a", None, 1, 2),
            ("active", "a", None, 1, 3),
            ("active", "a", None, 2, 3),
        ];
        let mut all = Vec::new();
        for (state, node, failed_node, version, minute) in rows {
            let mut one = Seed::new(ws, flow, state, node);
            one.failed_node = failed_node;
            one.version = version;
            one.created_at = at(minute);
            all.push((minute, seed(&pool, one).await));
        }
        all.sort_by_key(|entry| std::cmp::Reverse(*entry));
        let newest_first: Vec<Uuid> = all.iter().map(|(_, id)| *id).collect();
        let none = EnrollmentFilter::default();

        let first = list_flow_enrollments(&pool, ws, flow, &none, 2, None)
            .await
            .unwrap()
            .unwrap();
        let second = list_flow_enrollments(&pool, ws, flow, &none, 2, first.next)
            .await
            .unwrap()
            .unwrap();
        let third = list_flow_enrollments(&pool, ws, flow, &none, 2, second.next)
            .await
            .unwrap()
            .unwrap();

        let mut seen = ids(&first);
        seen.extend(ids(&second));
        seen.extend(ids(&third));
        assert!(first.next.is_some() && second.next.is_some() && third.next.is_none());
        assert_eq!(seen, newest_first, "newest first, no duplicate, no gap");

        let by_minute = |minute: i64| all.iter().find(|(m, _)| *m == minute).unwrap().1;
        let waiting = EnrollmentFilter {
            state: Some(EnrollmentStatus::Waiting),
            ..none.clone()
        };
        let page = list_flow_enrollments(&pool, ws, flow, &waiting, 50, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ids(&page), [by_minute(1)]);
        let v2 = EnrollmentFilter {
            version: Some(2),
            ..none.clone()
        };
        let page = list_flow_enrollments(&pool, ws, flow, &v2, 50, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(page.enrollments.len(), 1);
        assert_eq!(page.enrollments[0].flow_version, 2);
        let at_b = EnrollmentFilter {
            node: Some("b".into()),
            ..none.clone()
        };
        let page = list_flow_enrollments(&pool, ws, flow, &at_b, 50, None)
            .await
            .unwrap()
            .unwrap();
        let mut found = ids(&page);
        found.sort();
        let mut want = vec![by_minute(0), by_minute(1)];
        want.sort();
        assert_eq!(
            found, want,
            "a node filter matches the node and the failed node"
        );

        let other = workspace(&pool, "elsewhere").await;
        let elsewhere = list_flow_enrollments(&pool, other, flow, &none, 5, None).await;
        assert!(elsewhere.unwrap().is_none());
        let unknown = list_flow_enrollments(&pool, ws, Uuid::now_v7(), &none, 5, None).await;
        assert!(unknown.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_counts_of_a_version_match_a_hand_count(pool: PgPool) {
        let (ws, flow) = published_flow(&pool, "counts").await;
        let done = seed(&pool, Seed::new(ws, flow, "completed", "c")).await;
        let waiting = seed(&pool, Seed::new(ws, flow, "waiting", "b")).await;
        let mut stuck = Seed::new(ws, flow, "failed", "b");
        stuck.failed_node = Some("b");
        let failed = seed(&pool, stuck).await;
        let mut later = Seed::new(ws, flow, "waiting", "b");
        later.version = 2;
        let other_version = seed(&pool, later).await;
        for (enrollment, node, outcome) in [
            (done, "a", "done"),
            (done, "b", "done"),
            (done, "c", "done"),
            (done, SETTINGS_GOAL_NODE, "done"),
            (waiting, "a", "done"),
            (waiting, "b", "waiting"),
            (failed, "a", "done"),
            (failed, "b", "failed"),
            (other_version, "a", "done"),
        ] {
            step(&pool, ws, enrollment, node, outcome).await;
        }

        let counts = version_node_counts(&pool, ws, flow, 1)
            .await
            .unwrap()
            .unwrap();

        let count = |node: &str| counts.get(node).cloned().unwrap_or_default();
        let expect = |entered, completed, waiting, failed| NodeCount {
            entered,
            completed,
            waiting,
            failed,
            ..NodeCount::default()
        };
        assert_eq!(count("a"), expect(3, 3, 0, 0));
        assert_eq!(count("b"), expect(3, 1, 1, 1));
        assert_eq!(count("c"), expect(1, 1, 0, 0));
        assert_eq!(count(SETTINGS_GOAL_NODE), expect(1, 1, 0, 0));
        assert_eq!(count("nowhere"), NodeCount::default());
        let missing = version_node_counts(&pool, ws, flow, 9).await.unwrap();
        assert!(missing.is_none());
        let other = workspace(&pool, "elsewhere").await;
        let elsewhere = version_node_counts(&pool, other, flow, 1).await.unwrap();
        assert!(elsewhere.is_none());
    }

    async fn message(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Option<(Uuid, i32)>,
        node: &str,
        status: &str,
        reason: Option<&str>,
        test: bool,
    ) {
        let person = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(person)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, flow_id,                  flow_version, node_id, template_id, destination_id, status, reason, test,                  completed_at)              VALUES ($1, $2, $3, $1, $4, $5, $6, $1, $1, $7, $8, $9,                      CASE WHEN $7 IN ('pending', 'sending') THEN NULL ELSE now() END)",
        )
        .bind(Uuid::now_v7())
        .bind(ws.as_uuid())
        .bind(person)
        .bind(flow.map(|f| f.0))
        .bind(flow.map(|f| f.1))
        .bind(node)
        .bind(status)
        .bind(reason)
        .bind(test)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_delivery_counts_of_a_node_come_from_its_message_rows(pool: PgPool) {
        let (ws, flow) = published_flow(&pool, "deliveries").await;
        let v1 = Some((flow, 1));
        for (node, status, reason) in [
            ("mail", "sent", None),
            ("mail", "sent", None),
            ("mail", "skipped", Some("no_consent")),
            ("mail", "skipped", Some("no_consent")),
            ("mail", "skipped", Some("suppressed")),
            ("mail", "failed", Some("rejected")),
            ("mail", "pending", None),
            ("other", "sent", None),
        ] {
            message(&pool, ws, v1, node, status, reason, false).await;
        }
        // Not counted: a test message, another version, a test send with no flow.
        message(&pool, ws, v1, "mail", "sent", None, true).await;
        message(&pool, ws, Some((flow, 2)), "mail", "sent", None, false).await;
        message(&pool, ws, None, "mail", "sent", None, false).await;

        let counts = version_node_counts(&pool, ws, flow, 1)
            .await
            .unwrap()
            .unwrap();

        let mail = counts.get("mail").cloned().unwrap();
        assert_eq!((mail.sent, mail.send_failed), (2, 1));
        assert_eq!(
            mail.skipped,
            vec![("no_consent".to_owned(), 2), ("suppressed".to_owned(), 1)]
        );
        assert_eq!(counts.get("other").map(|c| c.sent), Some(1));
        assert_eq!(mail.entered, 0, "enrollment counts are independent");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_steps_of_one_job_come_back_in_the_order_they_ran(pool: PgPool) {
        use crate::{NewStepExecution, StepOutcome, record_steps};

        let (ws, flow) = published_flow(&pool, "order").await;
        let id = seed(&pool, Seed::new(ws, flow, "active", "a")).await;
        // One transaction, so every step shares one `started_at`; the names are not in order.
        let nodes = ["zeta", "mid", "alpha", "queue", "beta", "omega"];
        let steps: Vec<NewStepExecution> = nodes
            .iter()
            .map(|node| NewStepExecution {
                node_id: node,
                outcome: StepOutcome::Done,
                error: None,
                attempt: 1,
            })
            .collect();
        let mut tx = crate::begin(&pool).await.unwrap();
        record_steps(&mut tx, ws, id, &steps).await.unwrap();
        crate::commit(tx).await.unwrap();

        let path = load_enrollment_path(&pool, ws, flow, id)
            .await
            .unwrap()
            .unwrap();

        let ran: Vec<&str> = path.steps.iter().map(|s| s.node_id.as_str()).collect();
        assert_eq!(ran, nodes);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_path_has_the_steps_waits_and_decisions_of_that_enrollment_only(pool: PgPool) {
        let (ws, flow) = published_flow(&pool, "path").await;
        let mine = seed(&pool, Seed::new(ws, flow, "waiting", "b")).await;
        let theirs = seed(&pool, Seed::new(ws, flow, "active", "a")).await;
        step(&pool, ws, mine, "a", "done").await;
        step(&pool, ws, theirs, "a", "done").await;
        sqlx::query(
            "INSERT INTO enrollment_waits (workspace_id, enrollment_id, person_id, event_name, \
                wake_version, timeout_at) \
             SELECT workspace_id, id, person_id, 'opened', 0, now() FROM enrollments WHERE id = $1",
        )
        .bind(mine)
        .execute(&pool)
        .await
        .unwrap();
        for (enrollment, spec) in [
            (mine, &zirv_flow_core::decision_actions::FLOW_STEP_DONE),
            (theirs, &zirv_flow_core::decision_actions::FLOW_STEP_DONE),
            (mine, &zirv_flow_core::decision_actions::FLOW_STEP_WAITING),
        ] {
            append_decision(
                &pool,
                &NewDecision {
                    graph: Some(GraphPoint {
                        version: 1,
                        node: Some("a".into()),
                        admit_at: None,
                    }),
                    workspace_id: ws,
                    trace: trace(),
                    spec,
                    detail: None,
                    subject_ids: vec![("flow", flow), ("enrollment", enrollment)],
                    job: None,
                },
            )
            .await
            .unwrap();
        }

        let path = load_enrollment_path(&pool, ws, flow, mine)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(path.enrollment.id, mine);
        assert_eq!(path.steps.len(), 1);
        assert_eq!(path.event_waits.len(), 1);
        assert_eq!(path.event_waits[0].event_name, "opened");
        assert_eq!(path.decisions.len(), 2);
        assert!(!path.truncated);
        assert_eq!(path.decisions[0].subject_ids["version"], json!(1));
        assert_eq!(path.decisions[0].subject_ids["node"], "a");
        // Another flow's or workspace's enrollment is not found.
        let wrong_flow = load_enrollment_path(&pool, ws, Uuid::now_v7(), mine).await;
        assert!(wrong_flow.unwrap().is_none());
        let other = workspace(&pool, "elsewhere").await;
        let elsewhere = load_enrollment_path(&pool, other, flow, mine).await;
        assert!(elsewhere.unwrap().is_none());
    }

    /// The plan of `sql` mentions `index`.
    async fn plan_uses(pool: &PgPool, sql: &str, index: &str) -> bool {
        let mut tx = pool.begin().await.unwrap();
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        let text = plan.join("\n");
        eprintln!("{sql}\n{text}");
        text.contains(index)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_read_uses_its_index(pool: PgPool) {
        let (ws, flow) = published_flow(&pool, "explain").await;
        sqlx::query(
            "INSERT INTO people (id, workspace_id) \
             SELECT gen_random_uuid(), $1 FROM generate_series(1, 20000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, state, \
                node, occurrence_id, trace_id, cause_kind, cause_id, created_at, ended_at) \
             SELECT gen_random_uuid(), $1, $2, 1 + (p.n % 100 = 0)::int, p.id, \
                    CASE WHEN p.n % 50 = 0 THEN 'completed' ELSE 'exited' END, 'a', \
                    gen_random_uuid(), gen_random_uuid(), 'api', 't', \
                    now() - make_interval(secs => p.n), now() \
             FROM (SELECT id, row_number() OVER () AS n FROM people WHERE workspace_id = $1) p",
        )
        .bind(ws.as_uuid())
        .bind(flow)
        .execute(&pool)
        .await
        .unwrap();
        // One step row per enrollment, so the counts join reads a table of a realistic size.
        sqlx::query(
            "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome) \
             SELECT gen_random_uuid(), workspace_id, id, 'a', 'done' FROM enrollments",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO decision_records (id, workspace_id, trace_id, action, outcome, \
                cause_kind, cause_id, subject_ids) \
             SELECT gen_random_uuid(), $1, gen_random_uuid(), 'flow.step', 'done', 'api', 't', \
                jsonb_build_object('enrollment', gen_random_uuid()::text) \
             FROM generate_series(1, 20000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        for table in [
            "people",
            "enrollments",
            "step_executions",
            "decision_records",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("ANALYZE {table}")))
                .execute(&pool)
                .await
                .unwrap();
        }
        let (w, f) = (ws.as_uuid(), flow);
        let base = format!("FROM enrollments WHERE workspace_id = '{w}' AND flow_id = '{f}'");
        let order = "ORDER BY created_at DESC, id DESC LIMIT 51";
        let e = Uuid::now_v7();

        let checks = [
            (
                format!("SELECT id {base} {order}"),
                "enrollments_flow_list_idx".to_owned(),
            ),
            (
                format!("SELECT id {base} AND state = 'completed' {order}"),
                "enrollments_flow_state_list_idx".to_owned(),
            ),
            (
                format!("SELECT id {base} AND flow_version = 2 {order}"),
                "enrollments_flow_version_list_idx".to_owned(),
            ),
            (
                format!(
                    "SELECT s.node_id, count(*) FROM enrollments e \
                     JOIN step_executions s ON s.enrollment_id = e.id \
                     WHERE e.flow_id = '{f}' AND e.flow_version = 2 AND e.workspace_id = '{w}' \
                     GROUP BY s.node_id"
                ),
                // `enrollments_flow_version_list_idx` serves it, or on PostgreSQL 18 a skip scan of
                // `enrollments_flow_state_idx`; either way the version is an index condition.
                format!("Index Cond: ((flow_id = '{f}'::uuid) AND (flow_version = 2))"),
            ),
            (
                format!(
                    "SELECT node, count(*) FROM enrollments WHERE flow_id = '{f}' \
                     AND state = 'completed' AND flow_version = 2 AND workspace_id = '{w}' \
                     GROUP BY node"
                ),
                "enrollments_flow_state_idx".to_owned(),
            ),
            (
                format!(
                    "SELECT id FROM decision_records WHERE workspace_id = '{w}' \
                     AND subject_ids ? 'enrollment' AND (subject_ids ->> 'enrollment') = '{e}' \
                     AND recorded_at >= now() - interval '1 day' \
                     ORDER BY recorded_at, id LIMIT 1001"
                ),
                // The per-partition index has a generated name; its condition is what identifies it.
                format!(
                    "Index Cond: ((workspace_id = '{w}'::uuid) AND ((subject_ids ->> 'enrollment'::text) = "
                ),
            ),
        ];
        for (sql, index) in checks {
            assert!(plan_uses(&pool, &sql, &index).await, "{index}: {sql}");
        }
    }

    fn day(month: time::Month, day: u8) -> Date {
        Date::from_calendar_date(2026, month, day).unwrap()
    }

    fn utc(day: Date, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
        day.with_time(Time::from_hms(hour, minute, second).unwrap())
            .assume_utc()
    }

    async fn enter(pool: &PgPool, ws: WorkspaceId, flow: Uuid, version: i32, at: OffsetDateTime) {
        let mut entry = Seed::new(ws, flow, "active", "a");
        entry.version = version;
        entry.created_at = at;
        seed(pool, entry).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn entries_are_bucketed_by_utc_day_with_zero_days_and_a_previous_total(pool: PgPool) {
        use time::Month::{October, September};
        let (ws, flow) = published_flow(&pool, "entries").await;
        let today = day(October, 10);
        // 7 days: 4 to 10 October. The last second of the 3rd and the first of the 4th differ.
        enter(&pool, ws, flow, 1, utc(day(October, 3), 23, 59, 59)).await;
        enter(&pool, ws, flow, 1, utc(day(October, 4), 0, 0, 0)).await;
        enter(&pool, ws, flow, 2, utc(day(October, 4), 23, 59, 59)).await;
        enter(&pool, ws, flow, 1, utc(day(October, 10), 12, 0, 0)).await;
        // After the window, and before the previous window.
        enter(&pool, ws, flow, 1, utc(day(October, 11), 0, 0, 0)).await;
        enter(&pool, ws, flow, 1, utc(day(September, 26), 23, 59, 59)).await;

        let series = entries_per_day_until(&pool, ws, flow, 7, None, today)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((series.from, series.to), (day(October, 4), today));
        let counts: Vec<i64> = series.days.iter().map(|(_, n)| *n).collect();
        assert_eq!(
            counts,
            [2, 0, 0, 0, 0, 0, 1],
            "oldest first, zero days kept"
        );
        assert_eq!(series.days[0].0, day(October, 4));
        assert_eq!(series.total, 3);
        // 27 September to 3 October holds the entry of the 3rd at 23:59:59.
        assert_eq!(series.previous_total, 1);

        let only_v2 = entries_per_day_until(&pool, ws, flow, 7, Some(2), today)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((only_v2.total, only_v2.previous_total), (1, 0));
        let one = entries_per_day_until(&pool, ws, flow, 1, None, today)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(one.days, [(today, 1)]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn entries_do_not_cross_flows_or_workspaces(pool: PgPool) {
        use time::Month::October;
        let (ws, flow) = published_flow(&pool, "entries-a").await;
        let (other_ws, other_flow) = published_flow(&pool, "entries-b").await;
        let today = day(October, 10);
        enter(&pool, ws, flow, 1, utc(today, 8, 0, 0)).await;
        enter(&pool, other_ws, other_flow, 1, utc(today, 9, 0, 0)).await;
        enter(&pool, other_ws, other_flow, 1, utc(today, 9, 30, 0)).await;

        let mine = entries_per_day_until(&pool, ws, flow, 3, None, today)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mine.total, 1);
        let theirs = entries_per_day_until(&pool, other_ws, other_flow, 3, None, today)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(theirs.total, 2);
        let wrong_workspace = entries_per_day_until(&pool, other_ws, flow, 3, None, today).await;
        assert!(wrong_workspace.unwrap().is_none());
        let unknown = entries_per_day_until(&pool, ws, Uuid::now_v7(), 3, None, today).await;
        assert!(unknown.unwrap().is_none());
    }
}
