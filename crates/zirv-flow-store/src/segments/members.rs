//! Reading a segment's members and their history, and keeping the rows right when a person is
//! merged or deleted. The refresh writes the rows (see `refresh`).

use sqlx::PgConnection;
use time::{Date, Duration, OffsetDateTime, Time};
use uuid::Uuid;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::StoreError;
use crate::people::strongest_identifiers;

/// A current member.
#[derive(Clone, PartialEq)]
pub struct MemberRow {
    pub person_id: Uuid,
    pub entered_at: OffsetDateTime,
    /// The strongest identifier as `(kind, value)`; the caller masks the value by role.
    pub display: Option<(String, String)>,
}

// The display value is personal data.
impl std::fmt::Debug for MemberRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemberRow")
            .field("person_id", &self.person_id)
            .field("entered_at", &self.entered_at)
            .finish_non_exhaustive()
    }
}

/// One entry or exit.
#[derive(Clone, PartialEq)]
pub struct HistoryRow {
    pub person_id: Uuid,
    /// `entered` or `exited`.
    pub kind: &'static str,
    pub at: OffsetDateTime,
    pub cause: String,
    /// The version that entered the person, or whose refresh closed the row.
    pub version: i32,
    /// The member row, which with `kind` and `at` orders the history.
    pub row_id: i64,
    pub display: Option<(String, String)>,
}

impl std::fmt::Debug for HistoryRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HistoryRow")
            .field("person_id", &self.person_id)
            .field("kind", &self.kind)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

/// Where the next history page starts: the last row of the previous page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryCursor {
    pub at: OffsetDateTime,
    pub row_id: i64,
    /// `entered` is 0 and `exited` is 1.
    pub kind: i16,
}

impl HistoryCursor {
    pub fn of(row: &HistoryRow) -> Self {
        Self {
            at: row.at,
            row_id: row.row_id,
            kind: i16::from(row.kind == "exited"),
        }
    }
}

/// A page of the current members by person id, after `after`. The flag is true when more
/// follow. Reads in the caller's connection, scoped to the workspace.
pub async fn segment_members_page(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    after: Option<Uuid>,
    limit: u32,
) -> Result<(Vec<MemberRow>, bool), StoreError> {
    let mut rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT person_id, entered_at FROM segment_members \
         WHERE workspace_id = $1 AND segment_id = $2 AND exited_at IS NULL \
           AND ($3::uuid IS NULL OR person_id > $3) \
         ORDER BY person_id LIMIT $4",
    )
    .bind(workspace.as_uuid())
    .bind(segment)
    .bind(after)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let mut display = strongest_identifiers(connection, workspace, &ids).await?;
    let members = rows
        .into_iter()
        .map(|(person_id, entered_at)| MemberRow {
            display: display.remove(&person_id),
            person_id,
            entered_at,
        })
        .collect();
    Ok((members, more))
}

/// A page of entries and exits, newest first, after `after`. The flag is true when more follow.
pub async fn segment_history_page(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    after: Option<HistoryCursor>,
    limit: u32,
) -> Result<(Vec<HistoryRow>, bool), StoreError> {
    let (at, row_id, kind) = after.map_or((None, None, None), |cursor| {
        (Some(cursor.at), Some(cursor.row_id), Some(cursor.kind))
    });
    let mut rows: Vec<(Uuid, i16, OffsetDateTime, String, i32, i64)> = sqlx::query_as(
        "SELECT person_id, kind, at, cause, version, id FROM ( \
             SELECT person_id, 0::int2 AS kind, entered_at AS at, entered_cause AS cause, \
                    version, id \
             FROM segment_members WHERE workspace_id = $1 AND segment_id = $2 \
             UNION ALL \
             SELECT person_id, 1::int2, exited_at, exited_cause, exited_version, id \
             FROM segment_members \
             WHERE workspace_id = $1 AND segment_id = $2 AND exited_at IS NOT NULL \
         ) h \
         WHERE $3::timestamptz IS NULL OR (at, id, kind) < ($3, $4::bigint, $5::int2) \
         ORDER BY at DESC, id DESC, kind DESC LIMIT $6",
    )
    .bind(workspace.as_uuid())
    .bind(segment)
    .bind(at)
    .bind(row_id)
    .bind(kind)
    .bind(i64::from(limit) + 1)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let ids: Vec<Uuid> = rows.iter().map(|row| row.0).collect();
    let display = strongest_identifiers(connection, workspace, &ids).await?;
    let history = rows
        .into_iter()
        .map(|(person_id, kind, at, cause, version, row_id)| HistoryRow {
            display: display.get(&person_id).cloned(),
            person_id,
            kind: if kind == 0 { "entered" } else { "exited" },
            at,
            cause,
            version,
            row_id,
        })
        .collect();
    Ok((history, more))
}

/// The most days one size series covers.
pub const MAX_SIZE_DAYS: u32 = 90;

/// A segment's size at the end of each UTC day of a window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SizePerDay {
    /// The first day of the window.
    pub from: Date,
    /// The last day of the window: the day the read ran on.
    pub to: Date,
    /// One item per day, oldest first, including the days nothing changed.
    pub days: Vec<(Date, i64)>,
    /// The member count now, which is the size at the end of the last day.
    pub current: i64,
}

/// The size at the end of each UTC day of the last `days` days (1 to [`MAX_SIZE_DAYS`]) ending
/// today, worked back from the current member count through the entries and exits after each
/// day. Reads in the caller's connection; `None` when the workspace has no such segment.
pub async fn segment_size_per_day(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    days: u32,
) -> Result<Option<SizePerDay>, StoreError> {
    size_per_day_until(
        connection,
        workspace,
        segment,
        days,
        OffsetDateTime::now_utc().date(),
    )
    .await
}

async fn size_per_day_until(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    days: u32,
    today: Date,
) -> Result<Option<SizePerDay>, StoreError> {
    let from = today - Duration::days(i64::from(days) - 1);
    let midnight = |day: Date| day.with_time(Time::MIDNIGHT).assume_utc();
    let (start, end) = (midnight(from), midnight(today + Duration::days(1)));
    // One statement reads the count and the changes, so they agree. A day's size is the count
    // less the net entries of every later day.
    let rows: Vec<(Date, i64, i64)> = sqlx::query_as(
        "SELECT g.day::date,                 (s.member_count - COALESCE(SUM(COALESCE(e.n, 0) - COALESCE(x.n, 0))                     OVER (ORDER BY g.day ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING), 0))::bigint,                 s.member_count::bigint          FROM segments s          CROSS JOIN generate_series($3::date, $4::date, interval '1 day') AS g(day)          LEFT JOIN (SELECT (entered_at AT TIME ZONE 'UTC')::date AS day, count(*) AS n                     FROM segment_members                     WHERE workspace_id = $1 AND segment_id = $2                       AND entered_at >= $5 AND entered_at < $6                     GROUP BY 1) e ON e.day = g.day::date          LEFT JOIN (SELECT (exited_at AT TIME ZONE 'UTC')::date AS day, count(*) AS n                     FROM segment_members                     WHERE workspace_id = $1 AND segment_id = $2                       AND exited_at >= $5 AND exited_at < $6                     GROUP BY 1) x ON x.day = g.day::date          WHERE s.workspace_id = $1 AND s.id = $2          ORDER BY g.day",
    )
    .bind(workspace.as_uuid())
    .bind(segment)
    .bind(from)
    .bind(today)
    .bind(start)
    .bind(end)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(current) = rows.first().map(|row| row.2) else {
        return Ok(None);
    };
    Ok(Some(SizePerDay {
        from,
        to: today,
        days: rows.into_iter().map(|(day, size, _)| (day, size)).collect(),
        current,
    }))
}

/// Closes the open rows of a merged person, in the caller's transaction: `exited_at` is the
/// merge time, the cause is `data_change` and the trace is the merge's. No event is written; the
/// survivor enters at the next refresh if they belong. The segments' counts drop with the rows.
pub(crate) async fn close_on_merge(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    loser: Uuid,
    trace: &TraceContext,
) -> Result<(), StoreError> {
    sqlx::query(
        "WITH closed AS ( \
             UPDATE segment_members SET exited_version = version, exited_at = now(), \
                 exited_trace_id = $3, exited_cause = 'data_change' \
             WHERE workspace_id = $1 AND person_id = $2 AND exited_at IS NULL \
             RETURNING segment_id), \
         counts AS (SELECT segment_id, count(*) AS n FROM closed GROUP BY segment_id) \
         UPDATE segments s SET member_count = s.member_count - counts.n \
         FROM counts WHERE s.id = counts.segment_id AND s.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .bind(loser)
    .bind(trace.trace_id.as_uuid())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Takes the people's open rows out of the segments' counts. Deleting the rows themselves is the
/// `DELETIONS` list's job; run this first.
/// The segments are locked first, in the order of a pass.
pub(crate) async fn forget_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<(), StoreError> {
    super::lock_segments_of_people(connection, workspace, people).await?;
    sqlx::query(
        "WITH counts AS ( \
             SELECT segment_id, count(*) AS n FROM segment_members \
             WHERE workspace_id = $1 AND person_id = ANY($2) AND exited_at IS NULL \
             GROUP BY segment_id) \
         UPDATE segments s SET member_count = s.member_count - counts.n \
         FROM counts WHERE s.id = counts.segment_id AND s.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::Duration;
    use zirv_flow_core::Cause;

    use super::*;
    use crate::segments::tests_support::{person, segment, workspace};
    use crate::{begin, commit};

    async fn enter(pool: &PgPool, ws: WorkspaceId, seg: Uuid, person: Uuid, at: OffsetDateTime) {
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause) \
             VALUES ($1, $2, $3, 1, $4, gen_random_uuid(), 'initial')",
        )
        .bind(ws.as_uuid())
        .bind(seg)
        .bind(person)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("UPDATE segments SET member_count = member_count + 1 WHERE id = $1")
            .bind(seg)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn leave(pool: &PgPool, seg: Uuid, person: Uuid, at: OffsetDateTime) {
        sqlx::query(
            "UPDATE segment_members SET exited_at = $3, exited_trace_id = gen_random_uuid(), \
                exited_cause = 'time', exited_version = 1 \
             WHERE segment_id = $1 AND person_id = $2",
        )
        .bind(seg)
        .bind(person)
        .bind(at)
        .execute(pool)
        .await
        .unwrap();
    }

    #[test]
    fn a_cursor_names_the_row_the_kind_and_the_time() {
        let at = OffsetDateTime::UNIX_EPOCH;
        let row = |kind| HistoryRow {
            person_id: Uuid::nil(),
            kind,
            at,
            cause: "time".into(),
            version: 1,
            row_id: 9,
            display: None,
        };

        assert_eq!(HistoryCursor::of(&row("entered")).kind, 0);
        assert_eq!(HistoryCursor::of(&row("exited")).kind, 1);
        assert_eq!(HistoryCursor::of(&row("exited")).row_id, 9);
        assert!(!format!("{:?}", row("entered")).contains("display"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn members_page_by_person_id_and_only_the_open_rows_show(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let start = OffsetDateTime::now_utc();
        let mut ids = Vec::new();
        for n in 0..5 {
            let id = person(&pool, ws, &format!("p{n}@example.com"), "pro").await;
            enter(&pool, ws, seg.id, id, start).await;
            ids.push(id);
        }
        leave(&pool, seg.id, ids[0], start).await;
        ids.remove(0);
        ids.sort();
        let mut connection = pool.acquire().await.unwrap();

        let (first, more) = segment_members_page(&mut connection, ws, seg.id, None, 3)
            .await
            .unwrap();
        let (second, more_after) =
            segment_members_page(&mut connection, ws, seg.id, Some(first[2].person_id), 3)
                .await
                .unwrap();
        let (foreign, _) = segment_members_page(&mut connection, other, seg.id, None, 3)
            .await
            .unwrap();

        assert!(more);
        assert!(!more_after);
        let seen: Vec<Uuid> = first
            .iter()
            .chain(&second)
            .map(|row| row.person_id)
            .collect();
        assert_eq!(seen, ids);
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|row| row.display.is_some()));
        assert!(foreign.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn history_lists_entries_and_exits_newest_first_and_pages_without_gaps(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let start = OffsetDateTime::now_utc() - Duration::days(1);
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        enter(&pool, ws, seg.id, ada, start).await;
        enter(&pool, ws, seg.id, bob, start + Duration::hours(1)).await;
        leave(&pool, seg.id, ada, start + Duration::hours(2)).await;
        let mut connection = pool.acquire().await.unwrap();

        let (all, more) = segment_history_page(&mut connection, ws, seg.id, None, 10)
            .await
            .unwrap();
        assert!(!more);
        let kinds: Vec<(&str, Uuid)> = all.iter().map(|row| (row.kind, row.person_id)).collect();
        assert_eq!(kinds, [("exited", ada), ("entered", bob), ("entered", ada)]);
        assert_eq!(all[0].cause, "time");

        let (page, more) = segment_history_page(&mut connection, ws, seg.id, None, 2)
            .await
            .unwrap();
        assert!(more);
        let (rest, more) = segment_history_page(
            &mut connection,
            ws,
            seg.id,
            Some(HistoryCursor::of(&page[1])),
            2,
        )
        .await
        .unwrap();
        assert!(!more);
        assert_eq!(page.len() + rest.len(), 3);
        assert_eq!((rest[0].kind, rest[0].person_id), ("entered", ada));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_size_is_worked_back_from_the_count_by_utc_day_with_quiet_days_kept(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let today = time::macros::date!(2026 - 10 - 10);
        let at = |day: u8, hour: u8, minute: u8, second: u8| {
            time::macros::datetime!(2026 - 10 - 01 00:00 UTC)
                .replace_day(day)
                .unwrap()
                .replace_time(Time::from_hms(hour, minute, second).unwrap())
        };
        let (ada, bob, cy, dee) = (
            person(&pool, ws, "ada@example.com", "pro").await,
            person(&pool, ws, "bob@example.com", "pro").await,
            person(&pool, ws, "cy@example.com", "pro").await,
            person(&pool, ws, "dee@example.com", "pro").await,
        );
        // Ada is in from before the window. Cy enters in the last second of the 8th and leaves
        // at midnight. Dee enters, leaves and enters again, so the count now is three.
        enter(&pool, ws, seg.id, ada, at(1, 12, 0, 0)).await;
        enter(&pool, ws, seg.id, bob, at(6, 0, 0, 0)).await;
        enter(&pool, ws, seg.id, dee, at(6, 10, 0, 0)).await;
        leave(&pool, seg.id, dee, at(7, 10, 0, 0)).await;
        enter(&pool, ws, seg.id, cy, at(8, 23, 59, 59)).await;
        leave(&pool, seg.id, cy, at(9, 0, 0, 0)).await;
        enter(&pool, ws, seg.id, dee, at(9, 9, 0, 0)).await;
        sqlx::query("UPDATE segments SET member_count = 3 WHERE id = $1")
            .bind(seg.id)
            .execute(&pool)
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();

        let series = size_per_day_until(&mut connection, ws, seg.id, 6, today)
            .await
            .unwrap()
            .unwrap();

        let sizes: Vec<i64> = series.days.iter().map(|(_, size)| *size).collect();
        assert_eq!(series.from, time::macros::date!(2026 - 10 - 05));
        assert_eq!(series.to, today);
        // The 5th to the 10th: before Bob and Dee, both in, Dee out, Cy in, Cy out and Dee back
        // in, and a quiet day.
        assert_eq!(sizes, [1, 3, 2, 3, 3, 3]);
        assert_eq!(series.current, 3);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_one_day_window_is_the_current_count_and_other_workspaces_have_no_series(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let today = OffsetDateTime::now_utc().date();
        enter(&pool, ws, seg.id, ada, OffsetDateTime::now_utc()).await;
        let mut connection = pool.acquire().await.unwrap();

        let one = size_per_day_until(&mut connection, ws, seg.id, 1, today)
            .await
            .unwrap()
            .unwrap();
        let foreign = size_per_day_until(&mut connection, other, seg.id, 1, today)
            .await
            .unwrap();
        let unknown = size_per_day_until(&mut connection, ws, Uuid::now_v7(), 1, today)
            .await
            .unwrap();

        assert_eq!(one.days, [(today, 1)]);
        assert_eq!((one.from, one.to, one.current), (today, today, 1));
        assert!(foreign.is_none() && unknown.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_closes_the_losers_open_rows_with_the_merges_trace_and_drops_the_count(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        enter(&pool, ws, seg.id, ada, OffsetDateTime::now_utc()).await;
        enter(&pool, ws, seg.id, bob, OffsetDateTime::now_utc()).await;
        let trace = TraceContext::root(Cause::api("merge"));

        let mut tx = begin(&pool).await.unwrap();
        close_on_merge(&mut tx, ws, ada, &trace).await.unwrap();
        commit(tx).await.unwrap();

        let (cause, trace_id, count): (String, Uuid, i32) = sqlx::query_as(
            "SELECT m.exited_cause, m.exited_trace_id, s.member_count FROM segment_members m \
             JOIN segments s ON s.id = m.segment_id WHERE m.person_id = $1",
        )
        .bind(ada)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (cause.as_str(), trace_id, count),
            ("data_change", trace.trace_id.as_uuid(), 1)
        );
        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE name LIKE 'segment.%'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn forgetting_people_drops_only_their_open_rows_from_the_counts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        let now = OffsetDateTime::now_utc();
        enter(&pool, ws, seg.id, ada, now).await;
        enter(&pool, ws, seg.id, bob, now).await;
        leave(&pool, seg.id, bob, now).await;
        sqlx::query("UPDATE segments SET member_count = 1 WHERE id = $1")
            .bind(seg.id)
            .execute(&pool)
            .await
            .unwrap();

        let mut tx = begin(&pool).await.unwrap();
        forget_people(&mut tx, ws, &[ada, bob]).await.unwrap();
        commit(tx).await.unwrap();

        let count: i32 = sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    /// The SQL of the compiler's `051-segment` golden fixture.
    const SEGMENT_NODE_SQL: &str = "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND \
        p.merged_into IS NULL AND (EXISTS (SELECT 1 FROM segment_members sm WHERE \
        sm.workspace_id = p.workspace_id AND sm.person_id = p.id AND sm.segment_id = $2::uuid \
        AND sm.exited_at IS NULL))";

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_compiled_segment_node_probes_a_batch_through_the_open_member_index(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let other = segment(&pool, ws, "Other", 60).await;
        sqlx::query(
            "INSERT INTO people (id, workspace_id) \
             SELECT gen_random_uuid(), $1 FROM generate_series(1, 3000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause) \
             SELECT $1, s, p.id, 1, now(), gen_random_uuid(), 'initial' \
             FROM people p, unnest(ARRAY[$2::uuid, $3::uuid]) s WHERE p.workspace_id = $1",
        )
        .bind(ws.as_uuid())
        .bind(seg.id)
        .bind(other.id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE people, segment_members")
            .execute(&pool)
            .await
            .unwrap();

        let batch: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM people WHERE workspace_id = $1 LIMIT 10")
                .bind(ws.as_uuid())
                .fetch_all(&pool)
                .await
                .unwrap();

        // The incremental path asks about a batch of people (`batch_sql`): one probe each.
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (COSTS OFF) {SEGMENT_NODE_SQL} AND p.id = ANY($3::uuid[])"
        )))
        .bind(ws.as_uuid())
        .bind(seg.id)
        .bind(&batch)
        .fetch_all(&pool)
        .await
        .unwrap();

        let plan = plan.join("\n");
        eprintln!("{plan}");
        assert!(plan.contains("segment_members_open_idx"), "{plan}");
    }
}
