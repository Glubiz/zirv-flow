//! The daily rollup behind the Home summary. A scheduled job calls [`refresh_workspace_summary`]
//! for each workspace: it adds the rows written since the last run to their UTC day, recounts a
//! day exactly once it has closed, backfills the last 30 days and refreshes the per-flow
//! snapshot. The endpoint reads only what this module writes (`0085_summary_rollup.sql`).
//!
//! Every count names the workspace and a time slice with an index that carries what the count
//! reads, so a run costs the slice and not the history, and each UTC day-slice commits on its own:
//! people, messages and goal step rows by their time column (index-only), events by id range on
//! `events_workspace_id_id_idx` (ids are minted on receipt, so an id range is a received-time
//! range).

use std::time::Duration;

use sqlx::PgConnection;
use time::{Date, Duration as Span, OffsetDateTime, UtcOffset};
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

use crate::bounded::begin_bounded_for;
use crate::flows::SETTINGS_GOAL_NODE;
use crate::pool::PgPool;
use crate::{StoreError, commit};

/// Source rows younger than this are left for the next run: a transaction that started before
/// them may still commit, and the time columns are stamped at its start.
pub const COMMIT_MARGIN: Span = Span::minutes(2);

/// How many days the rollup keeps and backfills.
const KEPT_DAYS: i64 = 30;

/// How long one day-slice transaction may take.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The day-slices of each kind one run commits (today and the incremental catch-up, closed-day
/// recounts, backfill). A slice is a handful of index-only range counts, so three keep a run well
/// inside the five-minute period even on a large workspace, and the 30-day backfill still
/// finishes in ten runs, under an hour.
const DAYS_PER_RUN: usize = 3;

/// The `flow_id` of the workspace-wide metrics.
pub(super) const WORKSPACE_WIDE: Uuid = Uuid::nil();

pub(super) const MESSAGES_SENT: &str = "messages_sent";
pub(super) const GOALS_REACHED: &str = "goals_reached";
pub(super) const PEOPLE_CREATED: &str = "people_created";
pub(super) const EVENTS_RECEIVED: &str = "events_received";
pub(super) const FLOW_SENT: &str = "flow_sent";
pub(super) const FLOW_GOALS: &str = "flow_goals";

/// What one time slice holds.
#[derive(Debug, Default)]
struct Counts {
    messages: i64,
    people: i64,
    events: i64,
    flow_sent: Vec<(Uuid, i64)>,
    flow_goals: Vec<(Uuid, i64)>,
}

impl Counts {
    /// The non-zero `(metric, flow, count)` rows of the slice.
    fn rows(&self) -> Vec<(&'static str, Uuid, i64)> {
        let goals: i64 = self.flow_goals.iter().map(|(_, count)| count).sum();
        let wide = [
            (MESSAGES_SENT, self.messages),
            (GOALS_REACHED, goals),
            (PEOPLE_CREATED, self.people),
            (EVENTS_RECEIVED, self.events),
        ];
        let per_flow = |metric, rows: &[(Uuid, i64)]| {
            rows.iter()
                .map(|(flow, count)| (metric, *flow, *count))
                .collect::<Vec<_>>()
        };
        wide.into_iter()
            .map(|(metric, count)| (metric, WORKSPACE_WIDE, count))
            .chain(per_flow(FLOW_SENT, &self.flow_sent))
            .chain(per_flow(FLOW_GOALS, &self.flow_goals))
            .filter(|(_, _, count)| *count > 0)
            .collect()
    }
}

/// The goal nodes of every flow version of a workspace, resolved once per run and only when a
/// slice has step rows at all.
struct GoalSets {
    /// Every goal node id, plus the settings goal.
    node_ids: Vec<String>,
    /// Parallel arrays: `nodes[i]` is a goal node of `(flows[i], versions[i])`.
    flows: Vec<Uuid>,
    versions: Vec<i32>,
    nodes: Vec<String>,
}

async fn load_goal_sets(conn: &mut PgConnection, workspace: Uuid) -> Result<GoalSets, StoreError> {
    let versions: Vec<(Uuid, i32, serde_json::Value)> =
        sqlx::query_as("SELECT flow_id, version, graph FROM flow_versions WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_all(conn)
            .await
            .map_err(StoreError::query)?;
    let mut sets = GoalSets {
        node_ids: vec![SETTINGS_GOAL_NODE.to_owned()],
        flows: Vec::new(),
        versions: Vec::new(),
        nodes: Vec::new(),
    };
    for (flow, version, graph) in versions {
        let nodes = graph.get("nodes").and_then(|nodes| nodes.as_array());
        for node in nodes.into_iter().flatten() {
            let kind = node.get("type").and_then(|kind| kind.as_str());
            let id = node.get("id").and_then(|id| id.as_str());
            if let (Some("goal"), Some(id)) = (kind, id) {
                sets.flows.push(flow);
                sets.versions.push(version);
                sets.nodes.push(id.to_owned());
                if !sets.node_ids.iter().any(|known| known == id) {
                    sets.node_ids.push(id.to_owned());
                }
            }
        }
    }
    Ok(sets)
}

/// The smallest UUIDv7 minted at `at` (millisecond precision), so `[floor(a), floor(b))` holds
/// the ids minted in `[a, b)`.
fn v7_floor(at: OffsetDateTime) -> Uuid {
    let millis = (at.unix_timestamp_nanos() / 1_000_000).max(0) as u128;
    Uuid::from_u128(millis << 80)
}

/// One slice of one UTC day.
struct Slice {
    day: Date,
    from: OffsetDateTime,
    to: OffsetDateTime,
}

/// `[from, to)` cut at UTC midnights.
fn day_slices(from: OffsetDateTime, to: OffsetDateTime) -> Vec<Slice> {
    let mut slices = Vec::new();
    let mut at = from.to_offset(UtcOffset::UTC);
    let to = to.to_offset(UtcOffset::UTC);
    while at < to {
        let day = at.date();
        let end = day
            .next_day()
            .map_or(to, |next| next.midnight().assume_utc())
            .min(to);
        slices.push(Slice {
            day,
            from: at,
            to: end,
        });
        at = end;
    }
    slices
}

async fn count_slice(
    conn: &mut PgConnection,
    workspace: Uuid,
    slice: &Slice,
    goals: &mut Option<GoalSets>,
) -> Result<Counts, StoreError> {
    let (from, to) = (slice.from, slice.to);
    let sent: Vec<(Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT flow_id, count(*) FROM messages \
         WHERE workspace_id = $1 AND status = 'sent' AND NOT test \
           AND completed_at >= $2 AND completed_at < $3 GROUP BY flow_id",
    )
    .bind(workspace)
    .bind(from)
    .bind(to)
    .fetch_all(&mut *conn)
    .await
    .map_err(StoreError::query)?;
    let people: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM people \
         WHERE workspace_id = $1 AND created_at >= $2 AND created_at < $3",
    )
    .bind(workspace)
    .bind(from)
    .bind(to)
    .fetch_one(&mut *conn)
    .await
    .map_err(StoreError::query)?;
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM events WHERE workspace_id = $1 AND id >= $2 AND id < $3",
    )
    .bind(workspace)
    .bind(v7_floor(from))
    .bind(v7_floor(to))
    .fetch_one(&mut *conn)
    .await
    .map_err(StoreError::query)?;
    if goals.is_none() {
        *goals = Some(load_goal_sets(conn, workspace).await?);
    }
    let Some(sets) = goals.as_ref() else {
        return Err(StoreError::query(sqlx::Error::RowNotFound));
    };
    let flow_goals = sqlx::query_as(GOALS_BY_FLOW)
        .bind(workspace)
        .bind(from)
        .bind(to)
        .bind(&sets.node_ids)
        .bind(SETTINGS_GOAL_NODE)
        .bind(&sets.flows)
        .bind(&sets.versions)
        .bind(&sets.nodes)
        .fetch_all(&mut *conn)
        .await
        .map_err(StoreError::query)?;
    Ok(Counts {
        messages: sent.iter().map(|(_, count)| count).sum(),
        people,
        events,
        flow_sent: sent
            .into_iter()
            .filter_map(|(flow, count)| flow.map(|flow| (flow, count)))
            .collect(),
        flow_goals,
    })
}

/// The goal step rows of a slice, by flow. Only rows of a goal node id are read, inside the
/// slice's time range, from `step_executions_workspace_node_started_idx` (which carries the
/// enrollment and outcome, so the rows come from the index alone); the enrollment is read for the
/// matches, for its flow and version. A graph goal counts when it ended `done` and the node is a
/// goal of the version the enrollment entered by; the settings goal counts whatever the version.
const GOALS_BY_FLOW: &str = "SELECT e.flow_id, count(*) \
     FROM step_executions s JOIN enrollments e ON e.id = s.enrollment_id \
     WHERE s.workspace_id = $1 AND s.started_at >= $2 AND s.started_at < $3 \
       AND s.node_id = ANY($4) \
       AND (s.node_id = $5 \
            OR (s.outcome = 'done' \
                AND EXISTS (SELECT 1 FROM unnest($6::uuid[], $7::int[], $8::text[]) \
                                AS g (flow_id, version, node_id) \
                            WHERE g.flow_id = e.flow_id AND g.version = e.flow_version \
                              AND g.node_id = s.node_id))) \
     GROUP BY e.flow_id";

async fn write_rows(
    conn: &mut PgConnection,
    workspace: Uuid,
    day: Date,
    counts: &Counts,
    replace: bool,
) -> Result<(), StoreError> {
    if replace {
        sqlx::query("DELETE FROM summary_daily WHERE workspace_id = $1 AND day = $2")
            .bind(workspace)
            .bind(day)
            .execute(&mut *conn)
            .await
            .map_err(StoreError::query)?;
    }
    let rows = counts.rows();
    let metrics: Vec<&str> = rows.iter().map(|(metric, _, _)| *metric).collect();
    let flows: Vec<Uuid> = rows.iter().map(|(_, flow, _)| *flow).collect();
    let amounts: Vec<i64> = rows.iter().map(|(_, _, count)| *count).collect();
    sqlx::query(
        "INSERT INTO summary_daily (workspace_id, day, metric, flow_id, count) \
         SELECT $1, $2, t.metric, t.flow_id, t.count \
         FROM unnest($3::text[], $4::uuid[], $5::bigint[]) AS t (metric, flow_id, count) \
         ON CONFLICT (workspace_id, day, metric, flow_id) \
         DO UPDATE SET count = summary_daily.count + EXCLUDED.count",
    )
    .bind(workspace)
    .bind(day)
    .bind(&metrics)
    .bind(&flows)
    .bind(&amounts)
    .execute(conn)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The published flows of a workspace and whether the version each runs has a goal.
const PUBLISHED_FLOWS: &str = "SELECT f.id, \
            (v.settings ->> 'goal') IS NOT NULL \
            OR (jsonb_typeof(v.graph -> 'nodes') = 'array' \
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(v.graph -> 'nodes') n \
                            WHERE n ->> 'type' = 'goal')) \
     FROM flows f \
     JOIN flow_versions v ON v.flow_id = f.id AND v.version = f.current_version \
     WHERE f.workspace_id = $1 AND f.state = 'published'";

/// Replaces the snapshot of the published flows: enrollments in flight now (the states of the
/// enroll in-flight rule, `failed` included, as a failed one is retryable) and whether the
/// current version has a goal.
async fn refresh_flows(conn: &mut PgConnection, workspace: Uuid) -> Result<(), StoreError> {
    let published: Vec<(Uuid, bool)> = sqlx::query_as(PUBLISHED_FLOWS)
        .bind(workspace)
        .fetch_all(&mut *conn)
        .await
        .map_err(StoreError::query)?;
    let ids: Vec<Uuid> = published.iter().map(|(id, _)| *id).collect();
    let goal: Vec<bool> = published.iter().map(|(_, goal)| *goal).collect();
    sqlx::query("DELETE FROM summary_flows WHERE workspace_id = $1 AND NOT (flow_id = ANY($2))")
        .bind(workspace)
        .bind(&ids)
        .execute(&mut *conn)
        .await
        .map_err(StoreError::query)?;
    sqlx::query(IN_FLOW_SNAPSHOT)
        .bind(workspace)
        .bind(&ids)
        .bind(&goal)
        .execute(conn)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

const IN_FLOW_SNAPSHOT: &str = "INSERT INTO summary_flows (workspace_id, flow_id, people_in_flow, has_goal) \
     SELECT $1, p.flow_id, \
            (SELECT count(*) FROM enrollments e \
             WHERE e.flow_id = p.flow_id AND e.workspace_id = $1 \
               AND e.state IN ('active', 'waiting', 'held', 'failed')), \
            p.has_goal \
     FROM unnest($2::uuid[], $3::boolean[]) AS p (flow_id, has_goal) \
     ON CONFLICT (workspace_id, flow_id) DO UPDATE \
     SET people_in_flow = EXCLUDED.people_in_flow, has_goal = EXCLUDED.has_goal";

/// Where a workspace's rollup stands; see `0085_summary_rollup.sql`.
#[derive(Debug, Clone, Copy)]
struct State {
    counted_to: Option<OffsetDateTime>,
    closed_through: Option<Date>,
    backfilled_from: Option<Date>,
}

/// What a slice does to its day's rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Write {
    /// Add the slice to what the day holds.
    Add,
    /// Make the day hold exactly the slice.
    Replace,
}

fn full_day(day: Date) -> Slice {
    Slice {
        day,
        from: day.midnight().assume_utc(),
        to: day.midnight().assume_utc() + Span::days(1),
    }
}

/// Counts `slice`, writes it and moves the marks to `state`, in one transaction: the slice is
/// either all in the rollup with its marks or not at all, so a later failure loses no progress.
async fn commit_slice(
    pool: &PgPool,
    workspace: Uuid,
    slice: &Slice,
    write: Write,
    goals: &mut Option<GoalSets>,
    state: &State,
) -> Result<(), StoreError> {
    let mut tx = begin_bounded_for(pool, TIMEOUT).await?;
    let counts = count_slice(&mut tx, workspace, slice, goals).await?;
    let replace = write == Write::Replace;
    write_rows(&mut tx, workspace, slice.day, &counts, replace).await?;
    sqlx::query(
        "UPDATE summary_state \
         SET counted_to = $2, closed_through = $3, backfilled_from = $4 WHERE workspace_id = $1",
    )
    .bind(workspace)
    .bind(state.counted_to)
    .bind(state.closed_through)
    .bind(state.backfilled_from)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    commit(tx).await
}

/// The name a workspace's run locks on.
fn lock_name(workspace: Uuid) -> String {
    format!("summary.rollup:{workspace}")
}

/// Brings the rollup of `workspace` toward `now - COMMIT_MARGIN`, a bounded amount per call.
///
/// Every slice commits on its own, so progress survives a failure, and no call does more than
/// [`DAYS_PER_RUN`] day-slices of each kind:
/// 1. A first run (or one after more than 30 days idle) counts today so far and starts a
///    backfill; otherwise `[counted_to, now - margin)` is counted into its days.
/// 2. When a day has closed, it and the day before it are recounted exactly, which fixes a row
///    that committed late (the second recount catches one that missed the first).
/// 3. The backfill recounts days newest first, back to 30 days ago.
///
/// A workspace another run holds is skipped quietly: the holder is already doing this work.
pub async fn refresh_workspace_summary(
    pool: &PgPool,
    workspace: WorkspaceId,
    now: OffsetDateTime,
) -> Result<(), StoreError> {
    let ws = workspace.as_uuid();
    sqlx::query("INSERT INTO summary_state (workspace_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(ws)
        .execute(pool)
        .await
        .map_err(StoreError::query)?;
    // A session lock, as the run spans many transactions. If it cannot be released the
    // connection is dropped, which ends the session and the lock with it.
    let mut lock = pool.acquire().await.map_err(StoreError::query)?;
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(lock_name(ws))
        .fetch_one(&mut *lock)
        .await
        .map_err(StoreError::query)?;
    if !held {
        return Ok(());
    }
    let outcome = advance(pool, ws, now).await;
    let released = sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
        .bind(lock_name(ws))
        .execute(&mut *lock)
        .await;
    if released.is_err() {
        drop(lock.detach());
    }
    outcome
}

async fn advance(pool: &PgPool, ws: Uuid, now: OffsetDateTime) -> Result<(), StoreError> {
    let (counted_to, closed_through, backfilled_from) = sqlx::query_as(
        "SELECT counted_to, closed_through, backfilled_from FROM summary_state \
         WHERE workspace_id = $1",
    )
    .bind(ws)
    .fetch_one(pool)
    .await
    .map_err(StoreError::query)?;
    let mut state = State {
        counted_to,
        closed_through,
        backfilled_from,
    };
    let upper = (now - COMMIT_MARGIN).to_offset(UtcOffset::UTC);
    let upper_day = upper.date();
    let floor = upper_day - Span::days(KEPT_DAYS - 1);
    let mut goals = None;

    let stale = state
        .counted_to
        .is_none_or(|counted| upper - counted > Span::days(KEPT_DAYS));
    if stale {
        let today = Slice {
            to: upper,
            ..full_day(upper_day)
        };
        state = State {
            counted_to: Some(upper),
            closed_through: Some(upper_day - Span::days(1)),
            backfilled_from: Some(upper_day),
        };
        commit_slice(pool, ws, &today, Write::Replace, &mut goals, &state).await?;
    } else if let Some(from) = state.counted_to {
        for slice in day_slices(from, upper).into_iter().take(DAYS_PER_RUN) {
            state.counted_to = Some(slice.to);
            commit_slice(pool, ws, &slice, Write::Add, &mut goals, &state).await?;
        }
    }

    let newest_closed = upper_day - Span::days(1);
    if let (Some(closed), Some(counted)) = (state.closed_through, state.counted_to) {
        let mut day = closed.max(floor);
        for _ in 0..DAYS_PER_RUN {
            let slice = full_day(day);
            // A day the count has not reached is still being added to.
            if closed >= newest_closed || day > newest_closed || slice.to > counted {
                break;
            }
            state.closed_through = state.closed_through.max(Some(day));
            commit_slice(pool, ws, &slice, Write::Replace, &mut goals, &state).await?;
            day += Span::days(1);
        }
    }

    if let Some(mut oldest) = state.backfilled_from {
        for _ in 0..DAYS_PER_RUN {
            if oldest <= floor {
                break;
            }
            oldest -= Span::days(1);
            state.backfilled_from = Some(oldest);
            let slice = full_day(oldest);
            commit_slice(pool, ws, &slice, Write::Replace, &mut goals, &state).await?;
        }
    }

    let mut tx = begin_bounded_for(pool, TIMEOUT).await?;
    sqlx::query("DELETE FROM summary_daily WHERE workspace_id = $1 AND day < $2")
        .bind(ws)
        .bind(upper_day - Span::days(KEPT_DAYS + 5))
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    refresh_flows(&mut tx, ws).await?;
    sqlx::query("UPDATE summary_state SET computed_at = $2 WHERE workspace_id = $1")
        .bind(ws)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::query)?;
    commit(tx).await
}

/// The ids of every workspace, for the job to walk.
pub async fn summary_workspaces(pool: &PgPool) -> Result<Vec<WorkspaceId>, StoreError> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM workspaces ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(StoreError::query)?;
    Ok(ids.into_iter().map(WorkspaceId::from_uuid).collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::{date, datetime};

    use super::super::support::{enrollment, event, message, person, published, step};
    use super::*;
    use crate::flows::tests_support::{create, publish, workspace};
    use crate::flows::{FlowPatch, update_flow_draft};

    const T0: OffsetDateTime = datetime!(2030-01-15 12:00 UTC);

    async fn total(pool: &PgPool, ws: WorkspaceId, day: Date, metric: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(sum(count), 0)::bigint FROM summary_daily \
             WHERE workspace_id = $1 AND day = $2 AND metric = $3 AND flow_id = $4",
        )
        .bind(ws.as_uuid())
        .bind(day)
        .bind(metric)
        .bind(WORKSPACE_WIDE)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn an_event_id_range_is_a_received_time_range() {
        let at = datetime!(2030-01-15 12:00:00.5 UTC);
        let stamp =
            uuid::Timestamp::from_unix(uuid::NoContext, at.unix_timestamp() as u64, 500_000_000);
        let minted = Uuid::new_v7(stamp);

        assert!(v7_floor(at) <= minted);
        assert!(minted < v7_floor(at + Span::milliseconds(1)));
        assert!(v7_floor(at - Span::milliseconds(1)) < v7_floor(at));
    }

    #[test]
    fn slices_are_cut_at_utc_midnight() {
        let slices = day_slices(
            datetime!(2030-01-14 23:00 UTC),
            datetime!(2030-01-15 01:00 UTC),
        );

        let cuts: Vec<_> = slices.iter().map(|s| (s.day, s.from, s.to)).collect();
        assert_eq!(
            cuts,
            [
                (
                    date!(2030 - 01 - 14),
                    datetime!(2030-01-14 23:00 UTC),
                    datetime!(2030-01-15 00:00 UTC)
                ),
                (
                    date!(2030 - 01 - 15),
                    datetime!(2030-01-15 00:00 UTC),
                    datetime!(2030-01-15 01:00 UTC)
                ),
            ]
        );
        assert!(day_slices(T0, T0).is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn new_rows_are_counted_into_their_utc_day(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        let someone = person(&pool, ws, datetime!(2030-01-15 11:00 UTC)).await;
        // Either side of midnight, counted by one run at 00:30 on the 16th.
        for at in [
            datetime!(2030-01-15 23:50 UTC),
            datetime!(2030-01-16 00:10 UTC),
        ] {
            person(&pool, ws, at).await;
            event(&pool, ws, someone, at).await;
            message(&pool, ws, someone, None, "sent", false, at).await;
        }

        refresh_workspace_summary(&pool, ws, datetime!(2030-01-16 00:30 UTC))
            .await
            .unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            2
        );
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 16), PEOPLE_CREATED).await,
            1
        );
        for day in [date!(2030 - 01 - 15), date!(2030 - 01 - 16)] {
            assert_eq!(total(&pool, ws, day, EVENTS_RECEIVED).await, 1);
            assert_eq!(total(&pool, ws, day, MESSAGES_SENT).await, 1);
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_high_water_mark_keeps_a_second_run_from_counting_the_same_rows(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        person(&pool, ws, datetime!(2030-01-15 12:03 UTC)).await;
        let next = T0 + Span::minutes(10);

        refresh_workspace_summary(&pool, ws, next).await.unwrap();
        refresh_workspace_summary(&pool, ws, next).await.unwrap();
        refresh_workspace_summary(&pool, ws, next + Span::minutes(5))
            .await
            .unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_row_inside_the_commit_margin_is_counted_by_the_next_run(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        // 12:09 is younger than 12:10 minus the 2 minute margin.
        person(&pool, ws, datetime!(2030-01-15 12:09 UTC)).await;

        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(10))
            .await
            .unwrap();
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            0
        );

        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(15))
            .await
            .unwrap();
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_closed_day_correction_fixes_a_row_that_committed_late(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(10))
            .await
            .unwrap();
        // Stamped at 10:00 but committed after the mark had passed 12:08.
        person(&pool, ws, datetime!(2030-01-15 10:00 UTC)).await;
        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(20))
            .await
            .unwrap();
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            0
        );

        refresh_workspace_summary(&pool, ws, datetime!(2030-01-16 00:03 UTC))
            .await
            .unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_first_run_backfills_the_last_thirty_days_and_no_more(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        person(&pool, ws, datetime!(2029-12-17 00:00 UTC)).await;
        person(&pool, ws, datetime!(2029-12-16 23:59 UTC)).await;
        person(&pool, ws, datetime!(2030-01-05 08:00 UTC)).await;

        // Ten runs of three days each.
        for _ in 0..10 {
            refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        }

        assert_eq!(
            total(&pool, ws, date!(2029 - 12 - 17), PEOPLE_CREATED).await,
            1
        );
        assert_eq!(
            total(&pool, ws, date!(2029 - 12 - 16), PEOPLE_CREATED).await,
            0
        );
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 05), PEOPLE_CREATED).await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn only_goal_nodes_of_the_entered_version_count_as_goals(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let live = published(&pool, ws, "Live").await;
        // Another flow where `win` is an ordinary node.
        let other = create(&pool, ws, "Other").await.id;
        let patch = FlowPatch {
            graph: Some(json!({"v": 1, "nodes": [{"id": "win", "type": "exit"}], "edges": []})),
            ..FlowPatch::default()
        };
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(&mut connection, ws, other, &patch)
            .await
            .unwrap();
        drop(connection);
        publish(&pool, ws, other).await.unwrap();
        let at = datetime!(2030-01-15 09:00 UTC);
        let one = person(&pool, ws, at).await;
        let two = person(&pool, ws, at).await;
        let goal_run = enrollment(&pool, ws, live, one, "active").await;
        let plain_run = enrollment(&pool, ws, other, two, "active").await;
        step(&pool, ws, goal_run, "win", at).await;
        step(&pool, ws, goal_run, "other", at).await;
        step(&pool, ws, plain_run, "win", at).await;

        refresh_workspace_summary(&pool, ws, T0).await.unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), GOALS_REACHED).await,
            1
        );
        let per_flow: Vec<(Uuid, i64)> =
            sqlx::query_as("SELECT flow_id, count FROM summary_daily WHERE metric = 'flow_goals'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(per_flow, [(live, 1)]);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_flow_snapshot_follows_the_published_flows_and_their_enrollments(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        let live = published(&pool, ws, "Live").await;
        let someone = person(&pool, ws, T0).await;
        let enrolled = enrollment(&pool, ws, live, someone, "active").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        let read = || async {
            sqlx::query_as::<_, (Uuid, i64, bool)>(
                "SELECT flow_id, people_in_flow, has_goal FROM summary_flows",
            )
            .fetch_all(&pool)
            .await
            .unwrap()
        };
        assert_eq!(read().await, [(live, 1, true)]);

        sqlx::query("UPDATE enrollments SET state = 'completed', ended_at = now() WHERE id = $1")
            .bind(enrolled)
            .execute(&pool)
            .await
            .unwrap();
        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(5))
            .await
            .unwrap();
        assert_eq!(read().await, [(live, 0, true)]);

        sqlx::query("UPDATE flows SET state = 'paused' WHERE id = $1")
            .bind(live)
            .execute(&pool)
            .await
            .unwrap();
        refresh_workspace_summary(&pool, ws, T0 + Span::minutes(10))
            .await
            .unwrap();
        assert!(read().await.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_counts_one_workspace_and_leaves_the_other_alone(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        person(&pool, ours, datetime!(2030-01-15 09:00 UTC)).await;
        person(&pool, theirs, datetime!(2030-01-15 09:00 UTC)).await;
        person(&pool, theirs, datetime!(2030-01-15 09:01 UTC)).await;

        refresh_workspace_summary(&pool, ours, T0).await.unwrap();

        assert_eq!(
            total(&pool, ours, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
        assert_eq!(
            total(&pool, theirs, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            0
        );
        let states: i64 = sqlx::query_scalar("SELECT count(*) FROM summary_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(states, 1);
        assert_eq!(summary_workspaces(&pool).await.unwrap().len(), 2);
    }

    async fn people_days(pool: &PgPool, ws: WorkspaceId) -> Vec<Date> {
        sqlx::query_scalar(
            "SELECT day FROM summary_daily WHERE workspace_id = $1 AND metric = 'people_created' \
             ORDER BY day DESC",
        )
        .bind(ws.as_uuid())
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_run_is_bounded_and_the_backfill_walks_newest_day_first(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        for day in 1..=14 {
            person(
                &pool,
                ws,
                datetime!(2030-01-01 08:00 UTC) + Span::days(day - 1),
            )
            .await;
        }

        refresh_workspace_summary(&pool, ws, T0).await.unwrap();

        // Today, then the three days before it; nothing older yet.
        assert_eq!(
            people_days(&pool, ws).await,
            [
                date!(2030 - 01 - 14),
                date!(2030 - 01 - 13),
                date!(2030 - 01 - 12),
            ]
        );
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        assert_eq!(people_days(&pool, ws).await.len(), 6);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn progress_persists_when_a_run_fails_midway(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        for day in [14, 13, 12] {
            person(
                &pool,
                ws,
                datetime!(2030-01-01 08:00 UTC) + Span::days(day - 1),
            )
            .await;
        }
        sqlx::query(
            "CREATE FUNCTION refuse_day() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER refuse BEFORE INSERT ON summary_daily FOR EACH ROW \
             WHEN (NEW.day = '2030-01-13') EXECUTE FUNCTION refuse_day()",
        )
        .execute(&pool)
        .await
        .unwrap();

        let failed = refresh_workspace_summary(&pool, ws, T0).await;

        assert!(failed.is_err());
        assert_eq!(people_days(&pool, ws).await, [date!(2030 - 01 - 14)]);
        let (counted_to, backfilled_from): (Option<OffsetDateTime>, Option<Date>) =
            sqlx::query_as("SELECT counted_to, backfilled_from FROM summary_state")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(counted_to.is_some());
        assert_eq!(backfilled_from, Some(date!(2030 - 01 - 14)));

        sqlx::query("DROP TRIGGER refuse ON summary_daily")
            .execute(&pool)
            .await
            .unwrap();
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();

        assert_eq!(
            people_days(&pool, ws).await,
            [
                date!(2030 - 01 - 14),
                date!(2030 - 01 - 13),
                date!(2030 - 01 - 12),
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_another_run_holds_is_skipped_and_the_run_succeeds(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        person(&pool, ws, datetime!(2030-01-15 09:00 UTC)).await;
        let mut holder = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(hashtextextended($1, 0))")
            .bind(lock_name(ws.as_uuid()))
            .execute(&mut *holder)
            .await
            .unwrap();

        refresh_workspace_summary(&pool, ws, T0).await.unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            0
        );
        sqlx::query("SELECT pg_advisory_unlock(hashtextextended($1, 0))")
            .bind(lock_name(ws.as_uuid()))
            .execute(&mut *holder)
            .await
            .unwrap();
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_last_two_closed_days_are_recounted(pool: PgPool) {
        crate::migrate(&pool).await.unwrap();
        let ws = workspace(&pool, "one").await;
        refresh_workspace_summary(&pool, ws, T0).await.unwrap();
        // The 15th closes and is recounted at 00:03 on the 16th ...
        refresh_workspace_summary(&pool, ws, datetime!(2030-01-16 00:03 UTC))
            .await
            .unwrap();
        // ... and a row that commits into it after that, stamped 10:00, is still lost to it.
        person(&pool, ws, datetime!(2030-01-15 10:00 UTC)).await;
        refresh_workspace_summary(&pool, ws, datetime!(2030-01-16 12:00 UTC))
            .await
            .unwrap();
        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            0
        );

        // The 16th closes: the 16th and the 15th, once more, are recounted.
        refresh_workspace_summary(&pool, ws, datetime!(2030-01-17 00:03 UTC))
            .await
            .unwrap();

        assert_eq!(
            total(&pool, ws, date!(2030 - 01 - 15), PEOPLE_CREATED).await,
            1
        );
    }
}
