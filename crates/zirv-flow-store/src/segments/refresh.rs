//! The full refresh of a segment's members (architecture 8.3 and 8.4): the lock that serializes
//! refreshes of one segment, the jobs that ask for one, the cause and trace of what a refresh
//! changes, and the set-based statement that diffs the new set against the open rows. This
//! crate never sees the compiler: the job compiles the definition and passes in the SQL.

use serde_json::json;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use super::preview::{SqlQuery, bind_all};
use crate::{NewJob, StoreError, enqueue};

/// The kind of the job that refreshes one segment.
pub const REFRESH_JOB_KIND: &str = "segment.refresh";

/// The class of the advisory lock that serializes refreshes of one segment. The incremental
/// path takes the same lock through [`lock_segment_refresh`].
pub const REFRESH_LOCK_CLASS: &str = "segments.refresh";

/// What asked for a refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshTrigger {
    /// A create or a save of a version.
    Save,
    /// The segment's schedule.
    Schedule,
    /// An editor's "refresh now".
    Manual,
    /// A segment this one references changed.
    Dependent,
}

impl RefreshTrigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Save => "save",
            Self::Schedule => "schedule",
            Self::Manual => "manual",
            Self::Dependent => "dependent",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "save" => Some(Self::Save),
            "schedule" => Some(Self::Schedule),
            "manual" => Some(Self::Manual),
            "dependent" => Some(Self::Dependent),
            _ => None,
        }
    }
}

/// A refresh to enqueue, with what makes its unique key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshRequest {
    /// Saving `version`.
    Save { version: i32 },
    /// "Refresh now".
    Manual,
    /// After the refresh job `triggering_job` changed a segment this one references.
    Dependent { triggering_job: Uuid },
}

impl RefreshRequest {
    const fn trigger(self) -> RefreshTrigger {
        match self {
            Self::Save { .. } => RefreshTrigger::Save,
            Self::Manual => RefreshTrigger::Manual,
            Self::Dependent { .. } => RefreshTrigger::Dependent,
        }
    }

    /// One queued or running job holds each key (ruling 1c758fe9). A scheduled refresh has no
    /// request: the scheduler supplies the key of the slot.
    fn unique_key(self, segment: Uuid) -> String {
        match self {
            Self::Save { version } => format!("{REFRESH_JOB_KIND}:{segment}:v{version}"),
            Self::Manual => format!("{REFRESH_JOB_KIND}:{segment}:manual"),
            Self::Dependent { triggering_job } => {
                format!("{REFRESH_JOB_KIND}:{segment}:after:{triggering_job}")
            }
        }
    }
}

/// Enqueues a refresh of `segment` in the caller's transaction, on `trace`. False when a job
/// with the same key is queued or running already.
pub async fn enqueue_refresh(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    request: RefreshRequest,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(REFRESH_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({
        "workspace_id": workspace,
        "segment_id": segment,
        "trigger": request.trigger().as_str(),
    });
    job.unique_key = Some(request.unique_key(segment));
    enqueue(connection, &job).await
}

/// Takes the segment's refresh lock for the rest of the transaction, waiting for a refresh that
/// holds it. It serializes refreshes of one segment without blocking a save.
pub async fn lock_segment_refresh(
    connection: &mut PgConnection,
    segment: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1 || '/' || $2::text, 0))")
        .bind(REFRESH_LOCK_CLASS)
        .bind(segment)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Why a person entered or left a segment (architecture 8.3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipCause {
    /// The first computation of the segment.
    Initial,
    /// The definition changed.
    DefinitionChange,
    /// Data changed: a person's data or a segment this one references.
    DataChange,
    /// Time passed: a relative window moved, or a scheduled recompute found the change.
    Time,
    /// An editor asked for a refresh.
    Manual,
}

impl MembershipCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::DefinitionChange => "definition_change",
            Self::DataChange => "data_change",
            Self::Time => "time",
            Self::Manual => "manual",
        }
    }
}

/// The cause of what a refresh changes, from the segment's state and what asked for it. A
/// segment never computed is `initial`, and one computed from an older version is
/// `definition_change`, whoever asked: a scheduled refresh that applies a save's version
/// attributes it to the save. Otherwise the trigger decides, and a `save` whose version another
/// refresh already applied is `time`.
pub fn membership_cause(
    computed_version: Option<i32>,
    current_version: i32,
    trigger: RefreshTrigger,
) -> MembershipCause {
    match computed_version {
        None => MembershipCause::Initial,
        Some(computed) if computed != current_version => MembershipCause::DefinitionChange,
        Some(_) => match trigger {
            RefreshTrigger::Schedule | RefreshTrigger::Save => MembershipCause::Time,
            RefreshTrigger::Manual => MembershipCause::Manual,
            RefreshTrigger::Dependent => MembershipCause::DataChange,
        },
    }
}

/// The trace of what a refresh changes. A change caused by the definition keeps the trace of the
/// request that saved the version, with a `definition_change` cause; every other change stays on
/// the job's trace and cause.
pub fn membership_trace(
    cause: MembershipCause,
    job_trace: &TraceContext,
    segment: Uuid,
    version_trace: Uuid,
) -> TraceContext {
    match cause {
        MembershipCause::Initial | MembershipCause::DefinitionChange => TraceContext {
            trace_id: TraceId::from_uuid(version_trace),
            cause: Cause::definition_change(segment),
        },
        _ => job_trace.clone(),
    }
}

/// One refresh to apply.
#[derive(Clone, Debug)]
pub struct RefreshApply<'a> {
    pub workspace: WorkspaceId,
    pub segment: Uuid,
    /// The version the set was compiled from.
    pub version: i32,
    /// The compiled members query, selecting `p.id`.
    pub members: &'a SqlQuery,
    /// The clock time the set was compiled at.
    pub now: OffsetDateTime,
    pub cause: MembershipCause,
    /// The trace and cause the changes are recorded under, from [`membership_trace`].
    pub trace: &'a TraceContext,
    /// The job and attempt that refresh.
    pub job: (Uuid, i32),
    /// The job's own trace, which the refreshes of dependent segments continue.
    pub job_trace: &'a TraceContext,
}

/// What a refresh changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshOutcome {
    pub entered: i64,
    pub exited: i64,
}

/// The diff statement for the compiled `members` query. The extra values bind after the compiled
/// ones: `$1` is the workspace, the compiled values take `$2` to `$k + 1` and these follow in
/// order: the segment, the version, the time, the trace id, the cause, the decision's cause kind
/// and id, the job id and the attempt.
fn refresh_statement(members: &SqlQuery) -> String {
    let base = members.params.len() + 1;
    let segment = format!("${}::uuid", base + 1);
    let version = format!("${}::int4", base + 2);
    let now = format!("${}::timestamptz", base + 3);
    let trace = format!("${}::uuid", base + 4);
    let cause = format!("${}::text", base + 5);
    let cause_kind = format!("${}::text", base + 6);
    let cause_id = format!("${}::text", base + 7);
    let job = format!("${}::uuid", base + 8);
    let attempt = format!("${}::int4", base + 9);
    // The event and the decision of each change; `kind` is `entered` or `exited` and `from` the
    // CTE that lists the people.
    let recorded = |kind: &str, from: &str| {
        format!(
            "{kind}_events AS ( \
                 INSERT INTO events (id, workspace_id, person_id, name, occurred_at, \
                     received_at, properties, source, trace_id, validation_state) \
                 SELECT gen_random_uuid(), $1, person_id, 'segment.{kind}', {now}, {now}, \
                     jsonb_build_object('segment_id', {segment}::text, 'version', {version}, \
                                        'cause', {cause}), \
                     'segments', {trace}, 'valid' FROM {from} RETURNING 1), \
             {kind}_decisions AS ( \
                 INSERT INTO decision_records (id, workspace_id, trace_id, action, outcome, \
                     reason, cause_kind, cause_id, subject_ids, job_id, attempt) \
                 SELECT gen_random_uuid(), $1, {trace}, 'segment.{kind}', '{kind}', {cause}, \
                     {cause_kind}, {cause_id}, \
                     jsonb_build_object('segment', {segment}::text, 'person', person_id::text), \
                     {job}, {attempt} FROM {from} RETURNING 1)"
        )
    };
    let entered = recorded("entered", "inserted");
    let exited = recorded("exited", "closed");
    format!(
        "WITH target AS ({members_sql}), \
         entering AS ( \
             SELECT t.id AS person_id FROM target t \
             WHERE NOT EXISTS (SELECT 1 FROM segment_members m \
                 WHERE m.workspace_id = $1 AND m.segment_id = {segment} \
                   AND m.person_id = t.id AND m.exited_at IS NULL)), \
         leaving AS ( \
             SELECT m.id AS row_id FROM segment_members m \
             WHERE m.workspace_id = $1 AND m.segment_id = {segment} AND m.exited_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM target t WHERE t.id = m.person_id)), \
         inserted AS ( \
             INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                 entered_at, entered_trace_id, entered_cause) \
             SELECT $1, {segment}, person_id, {version}, {now}, {trace}, {cause} FROM entering \
             RETURNING person_id), \
         closed AS ( \
             UPDATE segment_members m SET exited_version = {version}, exited_at = {now}, \
                 exited_trace_id = {trace}, exited_cause = {cause} \
             FROM leaving l WHERE m.id = l.row_id AND m.exited_at IS NULL \
             RETURNING m.person_id), \
         {entered}, {exited} \
         SELECT ARRAY[ \
             (SELECT count(*) FROM inserted), (SELECT count(*) FROM closed), \
             (SELECT count(*) FROM entered_events) + (SELECT count(*) FROM exited_events), \
             (SELECT count(*) FROM entered_decisions) + (SELECT count(*) FROM exited_decisions) \
         ]::bigint[]",
        members_sql = members.sql,
    )
}

/// Diffs the compiled set against the segment's open rows and applies the difference, all in the
/// caller's transaction, after [`lock_segment_refresh`]:
///
/// - a person in the set with no open row gets one and a `segment.entered` event;
/// - an open row whose person is not in the set is closed with a `segment.exited` event;
/// - each change appends a decision under the trace;
/// - the segment's `computed_version`, `computed_at` and `member_count` are written;
/// - when anything changed, a `dependent` refresh is enqueued for each live segment whose current
///   version references this one, on the job's trace. It commits with the membership.
///
/// The events are written here and never through the ingest path, so they never mark a person
/// changed (ruling 6645b860). The person ids never leave the database.
pub async fn apply_refresh(
    connection: &mut PgConnection,
    apply: &RefreshApply<'_>,
) -> Result<RefreshOutcome, StoreError> {
    let statement = refresh_statement(apply.members);
    let query = bind_all(
        sqlx::query_scalar::<_, Vec<i64>>(sqlx::AssertSqlSafe(statement)),
        apply.workspace,
        &apply.members.params,
    )
    .bind(apply.segment)
    .bind(apply.version)
    .bind(apply.now)
    .bind(apply.trace.trace_id.as_uuid())
    .bind(apply.cause.as_str())
    .bind(apply.trace.cause.kind.as_str())
    .bind(apply.trace.cause.id.as_str())
    .bind(apply.job.0)
    .bind(apply.job.1);
    let counts = query
        .fetch_one(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let outcome = RefreshOutcome {
        entered: counts[0],
        exited: counts[1],
    };

    sqlx::query(
        "UPDATE segments SET computed_version = $3, computed_at = $4, \
            member_count = member_count + $5 - $6 \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(apply.workspace.as_uuid())
    .bind(apply.segment)
    .bind(apply.version)
    .bind(apply.now)
    .bind(outcome.entered)
    .bind(outcome.exited)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;

    if outcome.entered + outcome.exited > 0 {
        let dependents: Vec<Uuid> = sqlx::query_scalar(
            "SELECT s.id FROM segments s \
             JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
             WHERE s.workspace_id = $1 AND s.archived_at IS NULL AND s.id <> $2 \
               AND v.deps -> 'segments' ? $3 \
             ORDER BY s.id",
        )
        .bind(apply.workspace.as_uuid())
        .bind(apply.segment)
        .bind(apply.segment.to_string())
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        for dependent in dependents {
            enqueue_refresh(
                connection,
                apply.workspace,
                dependent,
                RefreshRequest::Dependent {
                    triggering_job: apply.job.0,
                },
                apply.job_trace,
            )
            .await?;
        }
    }
    Ok(outcome)
}

/// The cause kind a decision of `cause` carries: `definition_change` for the two causes the
/// definition drives, else the job trace's own.
pub fn decision_cause_kind(cause: MembershipCause, job_trace: &TraceContext) -> CauseKind {
    match cause {
        MembershipCause::Initial | MembershipCause::DefinitionChange => CauseKind::DefinitionChange,
        _ => job_trace.cause.kind,
    }
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;
    use serde_json::Value;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::UserId;

    use super::*;
    use crate::segments::SqlParam;
    use crate::segments::tests_support::{person, segment, segment_referencing, workspace};
    use crate::{begin, commit, list_trace};

    const NOW: OffsetDateTime = datetime!(2030-01-01 12:00 UTC);

    fn plan_query(plan: &str) -> SqlQuery {
        SqlQuery {
            sql: "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND p.merged_into IS NULL \
                  AND (p.traits ->> 'plan') = $2::text"
                .into(),
            params: vec![SqlParam::Text(plan.into())],
        }
    }

    fn job_trace() -> TraceContext {
        TraceContext::root(Cause::schedule(Uuid::new_v4()))
    }

    #[test]
    fn a_trigger_round_trips_through_its_name() {
        for trigger in [
            RefreshTrigger::Save,
            RefreshTrigger::Schedule,
            RefreshTrigger::Manual,
            RefreshTrigger::Dependent,
        ] {
            assert_eq!(RefreshTrigger::parse(trigger.as_str()), Some(trigger));
        }
        assert_eq!(RefreshTrigger::parse("now"), None);
    }

    #[test]
    fn each_request_has_its_own_unique_key() {
        let segment = Uuid::from_u128(7);
        let job = Uuid::from_u128(9);

        assert_eq!(
            RefreshRequest::Save { version: 3 }.unique_key(segment),
            format!("segment.refresh:{segment}:v3")
        );
        assert_eq!(
            RefreshRequest::Manual.unique_key(segment),
            format!("segment.refresh:{segment}:manual")
        );
        assert_eq!(
            RefreshRequest::Dependent {
                triggering_job: job
            }
            .unique_key(segment),
            format!("segment.refresh:{segment}:after:{job}")
        );
    }

    #[test]
    fn the_cause_comes_from_the_state_first_and_the_trigger_second() {
        use RefreshTrigger::{Dependent, Manual, Save, Schedule};
        for trigger in [Save, Schedule, Manual, Dependent] {
            assert_eq!(membership_cause(None, 1, trigger), MembershipCause::Initial);
            assert_eq!(
                membership_cause(Some(1), 2, trigger),
                MembershipCause::DefinitionChange
            );
        }
        assert_eq!(
            membership_cause(Some(2), 2, Schedule),
            MembershipCause::Time
        );
        assert_eq!(membership_cause(Some(2), 2, Save), MembershipCause::Time);
        assert_eq!(
            membership_cause(Some(2), 2, Manual),
            MembershipCause::Manual
        );
        assert_eq!(
            membership_cause(Some(2), 2, Dependent),
            MembershipCause::DataChange
        );
    }

    #[test]
    fn a_definition_driven_change_keeps_the_saves_trace_and_the_rest_the_jobs() {
        let job = job_trace();
        let segment = Uuid::from_u128(5);
        let saved = Uuid::from_u128(77);

        for cause in [MembershipCause::Initial, MembershipCause::DefinitionChange] {
            let trace = membership_trace(cause, &job, segment, saved);
            assert_eq!(trace.trace_id.as_uuid(), saved);
            assert_eq!(trace.cause, Cause::definition_change(segment));
            assert_eq!(
                decision_cause_kind(cause, &job),
                CauseKind::DefinitionChange
            );
        }
        for cause in [
            MembershipCause::Time,
            MembershipCause::Manual,
            MembershipCause::DataChange,
        ] {
            assert_eq!(membership_trace(cause, &job, segment, saved), job);
            assert_eq!(decision_cause_kind(cause, &job), CauseKind::Schedule);
        }
        let manual = TraceContext::root(Cause::manual(UserId::from_uuid(Uuid::nil())));
        assert_eq!(
            decision_cause_kind(MembershipCause::Manual, &manual),
            CauseKind::Manual
        );
    }

    #[test]
    fn the_extra_values_are_numbered_after_the_compiled_ones() {
        let one = refresh_statement(&plan_query("pro"));
        let none = refresh_statement(&SqlQuery {
            sql: "SELECT p.id FROM people p WHERE p.workspace_id = $1".into(),
            params: Vec::new(),
        });

        // One compiled value: the segment is $3 and the attempt is $11.
        assert!(one.contains("segment_id = $3::uuid"));
        assert!(one.contains("$11::int4"));
        assert!(!one.contains("$12"));
        // None: the segment is $2 and the attempt is $10.
        assert!(none.contains("segment_id = $2::uuid"));
        assert!(none.contains("$10::int4"));
        assert!(!none.contains("$11"));
        assert!(one.starts_with("WITH target AS (SELECT p.id FROM people p"));
    }

    async fn refresh(
        pool: &PgPool,
        ws: WorkspaceId,
        segment: Uuid,
        plan: &str,
        cause: MembershipCause,
        at: OffsetDateTime,
    ) -> (RefreshOutcome, TraceContext) {
        let job = job_trace();
        let trace = membership_trace(cause, &job, segment, Uuid::new_v4());
        let mut tx = begin(pool).await.unwrap();
        lock_segment_refresh(&mut tx, segment).await.unwrap();
        let outcome = apply_refresh(
            &mut tx,
            &RefreshApply {
                workspace: ws,
                segment,
                version: 1,
                members: &plan_query(plan),
                now: at,
                cause,
                trace: &trace,
                job: (Uuid::new_v4(), 1),
                job_trace: &job,
            },
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        (outcome, trace)
    }

    async fn open_people(pool: &PgPool, segment: Uuid) -> HashSet<Uuid> {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT person_id FROM segment_members WHERE segment_id = $1 AND exited_at IS NULL",
        )
        .bind(segment)
        .fetch_all(pool)
        .await
        .unwrap()
        .into_iter()
        .collect()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_refresh_enters_and_exits_people_with_events_decisions_and_counts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        let cy = person(&pool, ws, "cy@example.com", "free").await;

        let (first, trace) = refresh(&pool, ws, seg.id, "pro", MembershipCause::Initial, NOW).await;

        assert_eq!(
            first,
            RefreshOutcome {
                entered: 2,
                exited: 0
            }
        );
        assert_eq!(open_people(&pool, seg.id).await, HashSet::from([ada, bob]));
        let (computed_version, computed_at, count): (Option<i32>, Option<OffsetDateTime>, i32) =
            sqlx::query_as(
                "SELECT computed_version, computed_at, member_count FROM segments WHERE id = $1",
            )
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (computed_version, computed_at, count),
            (Some(1), Some(NOW), 2)
        );
        let events: Vec<(String, String, Value, Uuid, OffsetDateTime)> = sqlx::query_as(
            "SELECT name, source, properties, trace_id, occurred_at FROM events \
             WHERE name LIKE 'segment.%' ORDER BY person_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(events.len(), 2);
        for (name, source, properties, trace_id, occurred_at) in &events {
            assert_eq!(
                (name.as_str(), source.as_str()),
                ("segment.entered", "segments")
            );
            assert_eq!(
                *properties,
                serde_json::json!({"segment_id": seg.id, "version": 1, "cause": "initial"})
            );
            assert_eq!(*trace_id, trace.trace_id.as_uuid());
            assert_eq!(*occurred_at, NOW);
        }

        // A repeat changes nothing. Bob leaves the plan and Cy joins it.
        let (repeat, _) = refresh(&pool, ws, seg.id, "pro", MembershipCause::Time, NOW).await;
        assert_eq!(
            repeat,
            RefreshOutcome {
                entered: 0,
                exited: 0
            }
        );
        sqlx::query("UPDATE people SET traits = '{\"plan\":\"free\"}' WHERE id = $1")
            .bind(bob)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET traits = '{\"plan\":\"pro\"}' WHERE id = $1")
            .bind(cy)
            .execute(&pool)
            .await
            .unwrap();
        let later = NOW + time::Duration::hours(1);
        let (change, trace) =
            refresh(&pool, ws, seg.id, "pro", MembershipCause::DataChange, later).await;

        assert_eq!(
            change,
            RefreshOutcome {
                entered: 1,
                exited: 1
            }
        );
        assert_eq!(open_people(&pool, seg.id).await, HashSet::from([ada, cy]));
        let count: i32 = sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        let closed: (OffsetDateTime, Uuid, String, i32) = sqlx::query_as(
            "SELECT exited_at, exited_trace_id, exited_cause, exited_version FROM segment_members \
             WHERE person_id = $1 AND segment_id = $2",
        )
        .bind(bob)
        .bind(seg.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            closed,
            (later, trace.trace_id.as_uuid(), "data_change".into(), 1)
        );
        let (records, _) = list_trace(&pool, ws, trace.trace_id, 10).await.unwrap();
        let mut actions: Vec<(String, String, Option<String>)> = records
            .iter()
            .map(|r| (r.action.clone(), r.outcome.clone(), r.reason.clone()))
            .collect();
        actions.sort();
        assert_eq!(
            actions,
            [
                (
                    "segment.entered".into(),
                    "entered".into(),
                    Some("data_change".into())
                ),
                (
                    "segment.exited".into(),
                    "exited".into(),
                    Some("data_change".into())
                ),
            ]
        );
        assert!(
            records
                .iter()
                .all(|r| r.cause == trace.cause && r.attempt == Some(1))
        );
        let exited_event: String = sqlx::query_scalar(
            "SELECT properties ->> 'cause' FROM events WHERE name = 'segment.exited'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(exited_event, "data_change");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_segment_events_never_mark_a_person_changed(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        person(&pool, ws, "ada@example.com", "pro").await;
        sqlx::query("DELETE FROM person_changes")
            .execute(&pool)
            .await
            .unwrap();

        refresh(&pool, ws, seg.id, "pro", MembershipCause::Initial, NOW).await;
        sqlx::query("UPDATE people SET traits = '{}'")
            .execute(&pool)
            .await
            .unwrap();
        refresh(&pool, ws, seg.id, "pro", MembershipCause::Time, NOW).await;

        let events: i64 =
            sqlx::query_scalar("SELECT count(*) FROM events WHERE name LIKE 'segment.%'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM person_changes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((events, changes), (2, 0));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn another_workspaces_people_and_rows_are_never_touched(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let seg = segment(&pool, one, "Pros", 60).await;
        let other_seg = segment(&pool, two, "Pros", 60).await;
        person(&pool, one, "ada@example.com", "pro").await;
        let foreign = person(&pool, two, "ada@example.com", "pro").await;
        refresh(
            &pool,
            two,
            other_seg.id,
            "pro",
            MembershipCause::Initial,
            NOW,
        )
        .await;

        let (outcome, _) = refresh(&pool, one, seg.id, "pro", MembershipCause::Initial, NOW).await;

        assert_eq!(outcome.entered, 1);
        assert_eq!(
            open_people(&pool, other_seg.id).await,
            HashSet::from([foreign])
        );
        assert!(!open_people(&pool, seg.id).await.contains(&foreign));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_change_enqueues_a_dependent_refresh_in_the_same_transaction_and_a_rollback_none(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let base = segment(&pool, ws, "B", 60).await;
        let above = segment_referencing(&pool, ws, "A", base.id).await;
        person(&pool, ws, "ada@example.com", "pro").await;
        // The saves queued their own refreshes; only the dependent ones are looked at here.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();

        let job = job_trace();
        let trace = membership_trace(MembershipCause::Time, &job, base.id, Uuid::new_v4());
        let job_id = Uuid::new_v4();
        let query = plan_query("pro");
        let apply = RefreshApply {
            workspace: ws,
            segment: base.id,
            version: 1,
            members: &query,
            now: NOW,
            cause: MembershipCause::Time,
            trace: &trace,
            job: (job_id, 1),
            job_trace: &job,
        };

        let mut tx = begin(&pool).await.unwrap();
        apply_refresh(&mut tx, &apply).await.unwrap();
        drop(tx);
        let none: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'segment.refresh'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(none, 0);

        let mut tx = begin(&pool).await.unwrap();
        apply_refresh(&mut tx, &apply).await.unwrap();
        commit(tx).await.unwrap();
        let jobs: Vec<(Option<String>, Value, Uuid)> = sqlx::query_as(
            "SELECT unique_key, payload, trace_id FROM jobs WHERE kind = 'segment.refresh'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].0.as_deref(),
            Some(format!("segment.refresh:{}:after:{job_id}", above.id).as_str())
        );
        assert_eq!(
            jobs[0].1,
            serde_json::json!({"workspace_id": ws, "segment_id": above.id, "trigger": "dependent"})
        );
        assert_eq!(jobs[0].2, job.trace_id.as_uuid());

        // No change, no cascade.
        sqlx::query("DELETE FROM jobs")
            .execute(&pool)
            .await
            .unwrap();
        let mut tx = begin(&pool).await.unwrap();
        apply_refresh(&mut tx, &apply).await.unwrap();
        commit(tx).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, 0);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_refreshes_of_one_segment_serialize_on_its_lock(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let other = segment(&pool, ws, "Other", 60).await;
        person(&pool, ws, "ada@example.com", "pro").await;

        let mut first = begin(&pool).await.unwrap();
        lock_segment_refresh(&mut first, seg.id).await.unwrap();
        // Another segment's refresh is not held up.
        let mut unrelated = begin(&pool).await.unwrap();
        lock_segment_refresh(&mut unrelated, other.id)
            .await
            .unwrap();
        drop(unrelated);

        let second_pool = pool.clone();
        let id = seg.id;
        let second = tokio::spawn(async move {
            let mut tx = begin(&second_pool).await.unwrap();
            lock_segment_refresh(&mut tx, id).await.unwrap();
            // Past the lock it sees what the first refresh committed.
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM segment_members WHERE segment_id = $1 AND exited_at IS NULL",
            )
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            open
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !second.is_finished(),
            "the second refresh must wait for the first"
        );

        let job = job_trace();
        let trace = membership_trace(MembershipCause::Initial, &job, seg.id, Uuid::new_v4());
        apply_refresh(
            &mut first,
            &RefreshApply {
                workspace: ws,
                segment: seg.id,
                version: 1,
                members: &plan_query("pro"),
                now: NOW,
                cause: MembershipCause::Initial,
                trace: &trace,
                job: (Uuid::new_v4(), 1),
                job_trace: &job,
            },
        )
        .await
        .unwrap();
        commit(first).await.unwrap();

        assert_eq!(second.await.unwrap(), 1);
    }
}
