//! The append-only decision log (architecture section 12.4). This module offers appending
//! and reading only; nothing updates or deletes a row.
//!
//! The write API takes static strings and durable ids only, so personal data cannot reach
//! a record by accident.

use serde_json::{Map, Value};
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor, PgPool, Postgres, QueryBuilder};
use time::{Date, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use crate::StoreError;

/// Records per statement of [`append_decisions`]: 11 binds each stay far below PostgreSQL's limit.
const MAX_BATCH: usize = 1000;

/// The `bench-no-decisions` feature, a benchmark-only switch that makes [`append_decision`] and
/// [`append_decisions`] write nothing. The failed-job records written by the job queue's own SQL
/// are not affected. Never set in a release build.
const BENCH_NO_DECISIONS: bool = cfg!(feature = "bench-no-decisions");

// Makes the feature impossible to compile by accident: `--all-features`, an alias in a dependent or
// a default list all fail here unless the builder deliberately sets the variable.
#[cfg(feature = "bench-no-decisions")]
const _: () = assert!(
    option_env!("ZIRV_FLOW_BENCH_BUILD").is_some(),
    "bench-no-decisions is for benchmark builds only: set ZIRV_FLOW_BENCH_BUILD=1 to build it",
);

/// A record to append.
#[derive(Clone, Debug)]
pub struct NewDecision {
    pub workspace_id: WorkspaceId,
    pub trace: TraceContext,
    pub action: &'static str,
    pub outcome: &'static str,
    pub reason: Option<&'static str>,
    /// Durable domain ids by name.
    pub subject_ids: Vec<(&'static str, Uuid)>,
    /// The job and attempt number that made the decision, when a job did.
    pub job: Option<(Uuid, i32)>,
    /// Where in a flow's graph the decision was made, for the flow decisions the UI links from.
    pub graph: Option<GraphPoint>,
}

/// A point in a flow's published graph: the version an enrollment is pinned to and, when the
/// decision concerns one, the node. Both are ids the marketer chose, never personal data. They
/// are stored in `subject_ids` as `version` (a number) and `node` (text).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GraphPoint {
    pub version: i32,
    pub node: Option<String>,
}

/// The `subject_ids` document of a record: ids by name, then the graph point.
fn subjects_of(decision: &NewDecision) -> Value {
    let mut subjects: Map<String, Value> = decision
        .subject_ids
        .iter()
        .map(|(name, id)| ((*name).to_owned(), Value::String(id.to_string())))
        .collect();
    if let Some(point) = &decision.graph {
        subjects.insert("version".to_owned(), Value::from(point.version));
        if let Some(node) = &point.node {
            subjects.insert("node".to_owned(), Value::String(node.clone()));
        }
    }
    Value::Object(subjects)
}

/// A record as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub id: Uuid,
    pub trace_id: TraceId,
    pub recorded_at: OffsetDateTime,
    pub action: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub cause: Cause,
    pub subject_ids: Value,
    pub job_id: Option<Uuid>,
    pub attempt: Option<i32>,
}

#[derive(FromRow)]
struct DecisionRow {
    id: Uuid,
    trace_id: Uuid,
    recorded_at: OffsetDateTime,
    action: String,
    outcome: String,
    reason: Option<String>,
    cause_kind: String,
    cause_id: String,
    subject_ids: Value,
    job_id: Option<Uuid>,
    attempt: Option<i32>,
}

impl DecisionRow {
    fn into_decision(self) -> Result<Decision, StoreError> {
        let kind: CauseKind = self
            .cause_kind
            .parse()
            .map_err(|error| StoreError::query(sqlx::Error::Decode(Box::new(error))))?;
        Ok(Decision {
            id: self.id,
            trace_id: TraceId::from_uuid(self.trace_id),
            recorded_at: self.recorded_at,
            action: self.action,
            outcome: self.outcome,
            reason: self.reason,
            cause: Cause::from_stored(kind, self.cause_id),
            subject_ids: self.subject_ids,
            job_id: self.job_id,
            attempt: self.attempt,
        })
    }
}

/// Appends a record. Pass the caller's transaction so the record commits with the change
/// it describes.
pub async fn append_decision<'e>(
    executor: impl PgExecutor<'e>,
    decision: &NewDecision,
) -> Result<(), StoreError> {
    if BENCH_NO_DECISIONS {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO decision_records \
            (id, workspace_id, trace_id, action, outcome, reason, cause_kind, cause_id, \
             subject_ids, job_id, attempt) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(Uuid::new_v4())
    .bind(decision.workspace_id.as_uuid())
    .bind(decision.trace.trace_id.as_uuid())
    .bind(decision.action)
    .bind(decision.outcome)
    .bind(decision.reason)
    .bind(decision.trace.cause.kind.as_str())
    .bind(&decision.trace.cause.id)
    .bind(subjects_of(decision))
    .bind(decision.job.map(|(id, _)| id))
    .bind(decision.job.map(|(_, attempt)| attempt))
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Appends many records in one statement, at most [`MAX_BATCH`] at a time; the same rules as
/// [`append_decision`] apply to each.
pub async fn append_decisions(
    connection: &mut PgConnection,
    decisions: &[NewDecision],
) -> Result<(), StoreError> {
    if BENCH_NO_DECISIONS {
        return Ok(());
    }
    for chunk in decisions.chunks(MAX_BATCH) {
        let mut query = QueryBuilder::<Postgres>::new(
            "INSERT INTO decision_records                 (id, workspace_id, trace_id, action, outcome, reason, cause_kind, cause_id,                  subject_ids, job_id, attempt) ",
        );
        query.push_values(chunk, |mut row, decision| {
            row.push_bind(Uuid::new_v4())
                .push_bind(decision.workspace_id.as_uuid())
                .push_bind(decision.trace.trace_id.as_uuid())
                .push_bind(decision.action)
                .push_bind(decision.outcome)
                .push_bind(decision.reason)
                .push_bind(decision.trace.cause.kind.as_str())
                .push_bind(decision.trace.cause.id.clone())
                .push_bind(subjects_of(decision))
                .push_bind(decision.job.map(|(id, _)| id))
                .push_bind(decision.job.map(|(_, attempt)| attempt));
        });
        query
            .build()
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
    }
    Ok(())
}

/// The records of one trace in one workspace, oldest first, at most `limit`. The flag is
/// true when more existed.
pub async fn list_trace(
    pool: &PgPool,
    workspace: WorkspaceId,
    trace_id: TraceId,
    limit: u32,
) -> Result<(Vec<Decision>, bool), StoreError> {
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "SELECT id, trace_id, recorded_at, action, outcome, reason, cause_kind, cause_id, \
                subject_ids, job_id, attempt \
         FROM decision_records WHERE workspace_id = $1 AND trace_id = $2 \
         ORDER BY recorded_at, id LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(trace_id.as_uuid())
    .bind(i64::from(limit) + 1)
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    let truncated = rows.len() > limit as usize;
    let decisions = rows
        .into_iter()
        .take(limit as usize)
        .map(DecisionRow::into_decision)
        .collect::<Result<_, _>>()?;
    Ok((decisions, truncated))
}

/// Creates the monthly partitions for the month of `today` and the next one, if they are
/// missing. Fails when the DEFAULT partition already holds rows of a month, which cannot
/// then get a partition of its own; those rows stay readable. Every month is attempted
/// before the first error is returned.
pub async fn ensure_decision_partitions(pool: &PgPool, today: Date) -> Result<(), StoreError> {
    let first = today.replace_day(1).expect("day 1 exists in every month");
    let mut first_error = None;
    for start in [first, next_month(first)] {
        let end = next_month(start);
        // The names and bounds are built from integers only.
        let statement = format!(
            "CREATE TABLE IF NOT EXISTS decision_records_y{:04}m{:02} \
             PARTITION OF decision_records \
             FOR VALUES FROM ('{}') TO ('{}')",
            start.year(),
            u8::from(start.month()),
            bound(start),
            bound(end),
        );
        let created = sqlx::query(sqlx::AssertSqlSafe(statement))
            .execute(pool)
            .await
            .map_err(StoreError::query);
        if let Err(error) = created {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub(crate) fn next_month(first_of_month: Date) -> Date {
    let month = first_of_month.month().next();
    let year = first_of_month.year() + i32::from(u8::from(month) == 1);
    Date::from_calendar_date(year, month, 1).expect("the first of a month exists")
}

pub(crate) fn bound(date: Date) -> String {
    format!(
        "{:04}-{:02}-{:02} 00:00:00+00",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// The records that name `enrollment` as a subject, oldest first, at most `limit`, recorded at or after
/// `since` (the enrollment's creation, which prunes the partitions). The flag is true when more
/// existed. Errors are bounded-read errors, so a timeout is reported as one.
pub(crate) async fn list_enrollment_decisions(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    since: OffsetDateTime,
    limit: u32,
) -> Result<(Vec<Decision>, bool), StoreError> {
    let rows: Vec<DecisionRow> = sqlx::query_as(
        "SELECT id, trace_id, recorded_at, action, outcome, reason, cause_kind, cause_id, \r
                subject_ids, job_id, attempt \r
         FROM decision_records \r
         WHERE workspace_id = $1 AND subject_ids ? 'enrollment' \r
           AND (subject_ids ->> 'enrollment') = $2 AND recorded_at >= $3 \r
         ORDER BY recorded_at, id LIMIT $4",
    )
    .bind(workspace.as_uuid())
    .bind(enrollment.to_string())
    .bind(since)
    .bind(i64::from(limit) + 1)
    .fetch_all(connection)
    .await
    .map_err(StoreError::bounded)?;
    let truncated = rows.len() > limit as usize;
    let decisions = rows
        .into_iter()
        .take(limit as usize)
        .map(DecisionRow::into_decision)
        .collect::<Result<_, _>>()?;
    Ok((decisions, truncated))
}

#[cfg(test)]
mod tests {
    use time::macros::date;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn decision(
        workspace_id: WorkspaceId,
        trace: &TraceContext,
        action: &'static str,
    ) -> NewDecision {
        NewDecision {
            graph: None,
            workspace_id,
            trace: trace.clone(),
            action,
            outcome: "done",
            reason: None,
            subject_ids: Vec::new(),
            job: None,
        }
    }

    async fn only_partition(pool: &PgPool) -> String {
        sqlx::query_scalar("SELECT tableoid::regclass::text FROM decision_records")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn database_today(pool: &PgPool) -> Date {
        let now: OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(pool)
            .await
            .unwrap();
        now.to_offset(time::UtcOffset::UTC).date()
    }

    #[test]
    fn partitions_cover_the_month_and_roll_over_the_year() {
        assert_eq!(next_month(date!(2026 - 10 - 01)), date!(2026 - 11 - 01));
        assert_eq!(next_month(date!(2026 - 12 - 01)), date!(2027 - 01 - 01));
        assert_eq!(bound(date!(2027 - 01 - 01)), "2027-01-01 00:00:00+00");
    }

    /// Nearest-rank percentile (0 to 100) of an ascending slice.
    #[cfg(not(feature = "bench-no-decisions"))]
    fn percentile_ms(sorted: &[f64], p: f64) -> f64 {
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }

    /// Times the decision appends of one drain batch, the "added transaction time" of
    /// architecture section 19. Run by hand with `ZIRV_FLOW_BENCH_DECISIONS=<batches>` (at least
    /// 500 are run; `ZIRV_FLOW_BENCH_DRAIN_BATCH` sets the batch size, default 500, the shipped
    /// `ingest.drain_batch_size`); without the variable it does nothing. It appends what a drain
    /// writes, one `append_decision` per decision on the batch's transaction, and prints JSON
    /// with the p50, p95 and p99 per batch in milliseconds, with and without the commit. Progress goes to
    /// standard error every 50 batches; `ZIRV_FLOW_BENCH_DECISIONS_BUDGET_S` (default 600) caps the time per
    /// shape and a capped shape is reported with `partial_time_budget_hit` and the batches it ran.
    #[cfg(not(feature = "bench-no-decisions"))]
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL; set ZIRV_FLOW_BENCH_DECISIONS to measure"]
    async fn a_drain_batch_of_decisions_records_its_duration(pool: PgPool) {
        use std::time::Instant;

        fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
            std::env::var(name)
                .ok()
                .and_then(|value| value.parse().ok())
        }
        let Some(batches) = env_number::<usize>("ZIRV_FLOW_BENCH_DECISIONS") else {
            eprintln!("ZIRV_FLOW_BENCH_DECISIONS is not set: skipping the timing run");
            return;
        };
        let batches = batches.max(500);
        let size = env_number::<usize>("ZIRV_FLOW_BENCH_DRAIN_BATCH").unwrap_or(500);
        let budget = std::time::Duration::from_secs(
            env_number::<u64>("ZIRV_FLOW_BENCH_DECISIONS_BUDGET_S").unwrap_or(600),
        );
        let ws = workspace(&pool, "bench").await;

        // The shapes the drain writes (events/apply.rs, people/apply.rs): a stored event is one
        // `event.accepted`, a changed profile one `trait.change`; a new person adds a
        // `person.created`.
        type Decision = (&'static str, &'static str, &'static [&'static str]);
        let shapes: [(&str, &[Decision]); 3] = [
            (
                "track",
                &[("event.accepted", "valid", &["event", "person"])],
            ),
            (
                "identify",
                &[("trait.change", "changed", &["person", "source_record"])],
            ),
            (
                "track_new_person",
                &[
                    ("person.created", "created", &["person", "source_record"]),
                    ("event.accepted", "valid", &["event", "person"]),
                ],
            ),
        ];
        let mut report = Vec::new();
        for (name, per_item) in shapes {
            let mut inside = Vec::with_capacity(batches);
            let mut committed = Vec::with_capacity(batches);
            let shape_started = Instant::now();
            let mut partial = false;
            for number in 0..batches {
                if shape_started.elapsed() > budget {
                    partial = true;
                    break;
                }
                if number % 50 == 0 {
                    eprintln!("{name}: batch {number} of {batches}");
                }
                let trace = TraceContext::root(Cause::api("bench"));
                let started = Instant::now();
                let mut tx = crate::begin(&pool).await.unwrap();
                for _ in 0..size {
                    for &(action, outcome, subjects) in per_item {
                        append_decision(
                            &mut *tx,
                            &NewDecision {
                                graph: None,
                                workspace_id: ws,
                                trace: trace.clone(),
                                action,
                                outcome,
                                reason: None,
                                subject_ids: subjects
                                    .iter()
                                    .map(|name| (*name, Uuid::new_v4()))
                                    .collect(),
                                job: None,
                            },
                        )
                        .await
                        .unwrap();
                    }
                }
                inside.push(started.elapsed().as_secs_f64() * 1000.0);
                crate::commit(tx).await.unwrap();
                committed.push(started.elapsed().as_secs_f64() * 1000.0);
            }
            let batches_run = inside.len();
            let summary = |mut values: Vec<f64>| {
                values.sort_by(f64::total_cmp);
                serde_json::json!({
                    "p50_ms": percentile_ms(&values, 50.0),
                    "p95_ms": percentile_ms(&values, 95.0),
                    "p99_ms": percentile_ms(&values, 99.0),
                })
            };
            report.push(serde_json::json!({
                "shape": name,
                "decisions_per_batch": size * per_item.len(),
                "batches_planned": batches,
                "batches_run": batches_run,
                "partial_time_budget_hit": partial,
                "inside_transaction": summary(inside),
                "with_commit": summary(committed),
            }));
        }
        println!(
            "{}",
            serde_json::json!({ "benchmark": "decision_append", "drain_batch_size": size, "shapes": report })
        );
    }

    #[test]
    fn the_benchmark_switch_is_off_unless_the_feature_is_enabled() {
        #[cfg(not(feature = "bench-no-decisions"))]
        const {
            assert!(!BENCH_NO_DECISIONS)
        };
    }

    #[cfg(feature = "bench-no-decisions")]
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn with_the_benchmark_feature_appends_write_nothing(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req"));
        append_decision(&pool, &decision(ws, &trace, "step"))
            .await
            .unwrap();
        let mut connection = pool.acquire().await.unwrap();
        append_decisions(&mut connection, &[decision(ws, &trace, "step")])
            .await
            .unwrap();
        let (records, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        assert!(records.is_empty());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn records_come_back_oldest_first_and_only_for_their_workspace(pool: PgPool) {
        let ours = workspace(&pool, "ours").await;
        let theirs = workspace(&pool, "theirs").await;
        let trace = TraceContext::root(Cause::api("req-1"));
        let subject = Uuid::new_v4();
        let mut first = decision(ours, &trace, "first");
        first.reason = Some("because");
        first.subject_ids = vec![("segment_id", subject)];
        first.job = Some((Uuid::new_v4(), 2));
        append_decision(&pool, &first).await.unwrap();
        append_decision(&pool, &decision(ours, &trace, "second"))
            .await
            .unwrap();
        append_decision(&pool, &decision(theirs, &trace, "elsewhere"))
            .await
            .unwrap();
        let other = TraceContext::root(Cause::api("x"));
        append_decision(&pool, &decision(ours, &other, "other-trace"))
            .await
            .unwrap();

        let (records, truncated) = list_trace(&pool, ours, trace.trace_id, 10).await.unwrap();

        assert!(!truncated);
        let actions: Vec<&str> = records
            .iter()
            .map(|record| record.action.as_str())
            .collect();
        assert_eq!(actions, ["first", "second"]);
        assert_eq!(records[0].reason.as_deref(), Some("because"));
        assert_eq!(records[0].cause, trace.cause);
        assert_eq!(records[0].attempt, Some(2));
        assert_eq!(
            records[0].subject_ids,
            serde_json::json!({"segment_id": subject.to_string()})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_appends_every_record_with_its_own_subjects(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req-1"));
        let subjects: Vec<Uuid> = (0..MAX_BATCH + 5).map(|_| Uuid::new_v4()).collect();
        let batch: Vec<NewDecision> = subjects
            .iter()
            .map(|subject| {
                let mut record = decision(ws, &trace, "bulk");
                record.subject_ids = vec![("person", *subject)];
                record
            })
            .collect();
        let mut connection = pool.acquire().await.unwrap();

        append_decisions(&mut connection, &batch).await.unwrap();
        append_decisions(&mut connection, &[]).await.unwrap();

        let stored: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT subject_ids ->> 'person') FROM decision_records              WHERE workspace_id = $1 AND action = 'bulk'",
        )
        .bind(ws.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(stored, i64::try_from(subjects.len()).unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_long_trace_is_cut_at_the_limit_and_flagged(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let trace = TraceContext::root(Cause::api("req-1"));
        for _ in 0..3 {
            append_decision(&pool, &decision(ws, &trace, "step"))
                .await
                .unwrap();
        }

        let (cut, cut_flag) = list_trace(&pool, ws, trace.trace_id, 2).await.unwrap();
        let (all, all_flag) = list_trace(&pool, ws, trace.trace_id, 3).await.unwrap();

        assert_eq!((cut.len(), cut_flag), (2, true));
        assert_eq!((all.len(), all_flag), (3, false));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn partition_upkeep_is_idempotent_and_new_rows_leave_the_default_partition(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let today = database_today(&pool).await;
        let trace = TraceContext::root(Cause::api("req-1"));

        ensure_decision_partitions(&pool, today).await.unwrap();
        ensure_decision_partitions(&pool, today).await.unwrap();
        append_decision(&pool, &decision(ws, &trace, "step"))
            .await
            .unwrap();

        assert_eq!(
            only_partition(&pool).await,
            format!(
                "decision_records_y{:04}m{:02}",
                today.year(),
                u8::from(today.month())
            )
        );
    }

    async fn insert_at(pool: &PgPool, ws: WorkspaceId, recorded_at: &str) {
        sqlx::query(
            "INSERT INTO decision_records \
             (id, workspace_id, trace_id, recorded_at, action, outcome, cause_kind, cause_id) \
             VALUES ($1, $2, $3, $4::timestamptz, 'step', 'done', 'api', 'x')",
        )
        .bind(Uuid::new_v4())
        .bind(ws.as_uuid())
        .bind(Uuid::new_v4())
        .bind(recorded_at)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn far_month(pool: &PgPool) -> Date {
        let mut month = database_today(pool).await.replace_day(1).unwrap();
        for _ in 0..24 {
            month = next_month(month);
        }
        month
    }

    fn partition_name(month: Date) -> String {
        format!(
            "decision_records_y{:04}m{:02}",
            month.year(),
            u8::from(month.month())
        )
    }

    async fn partition_exists(pool: &PgPool, name: &str) -> bool {
        sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn migrating_creates_this_and_next_month_so_a_fresh_row_skips_the_default(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let today = database_today(&pool).await;
        let trace = TraceContext::root(Cause::api("req-1"));
        let next = next_month(today.replace_day(1).unwrap());

        append_decision(&pool, &decision(ws, &trace, "step"))
            .await
            .unwrap();

        let name = |date: Date| {
            format!(
                "decision_records_y{:04}m{:02}",
                date.year(),
                u8::from(date.month())
            )
        };
        assert_eq!(only_partition(&pool).await, name(today));
        assert!(partition_exists(&pool, &name(next)).await);
        ensure_decision_partitions(&pool, today).await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn without_upkeep_a_row_outside_the_precreated_months_lands_in_the_default_partition(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;

        let month = far_month(&pool).await;

        insert_at(&pool, ws, &format!("{}-15 00:00:00+00", &bound(month)[..7])).await;

        assert_eq!(only_partition(&pool).await, "decision_records_default");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn upkeep_fails_when_the_default_partition_holds_that_month_but_still_makes_the_next(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        let month = far_month(&pool).await;
        insert_at(&pool, ws, &format!("{}-15 00:00:00+00", &bound(month)[..7])).await;

        let result = ensure_decision_partitions(&pool, month.replace_day(10).unwrap()).await;

        assert!(result.is_err());
        assert!(partition_exists(&pool, &partition_name(next_month(month))).await);
        assert!(!partition_exists(&pool, &partition_name(month)).await);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }
}
