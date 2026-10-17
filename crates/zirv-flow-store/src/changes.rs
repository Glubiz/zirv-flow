//! The dirty set (architecture 7.1 and 8.4): `person_changes`, one row per person whose data
//! changed since a pass last looked. A writer marks the people it changed in its own
//! transaction, with what changed and the trace and cause of the change, and asks for a
//! `segment.reevaluate` job. The job claims the marks with [`claim_dirty`].

use serde_json::json;
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use crate::{NewJob, StoreError, enqueue};

/// The kind of the job that re-evaluates the segments for a workspace's dirty people.
pub const REEVALUATE_JOB_KIND: &str = "segment.reevaluate";

/// One claimed mark.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirtyPerson {
    pub person_id: Uuid,
    /// Every class that changed since the person was last evaluated.
    pub classes: ChangeClasses,
    /// The trace and cause of the latest change; none on a mark that predates them.
    pub trace: Option<TraceContext>,
    /// The earliest `occurred_at` of a late event, when one was among the changes.
    pub late_from: Option<OffsetDateTime>,
}

/// Marks `people` changed in `classes`, in the caller's transaction. Marks coalesce per person:
/// the classes are OR-ed, the latest trace and cause win, `late_from` takes the earliest value
/// and `changed_at` keeps the first mark's time. It only upserts; the transaction's owner calls
/// [`enqueue_reevaluate`] once before it commits.
pub(crate) async fn mark_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
    classes: ChangeClasses,
    trace: &TraceContext,
    late_from: Option<OffsetDateTime>,
) -> Result<(), StoreError> {
    if people.is_empty() {
        return Ok(());
    }
    // Sorted, so two writers marking overlapping people take the rows in one order.
    let mut sorted = people.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    sqlx::query(
        "INSERT INTO person_changes \
            (workspace_id, person_id, classes, trace_id, cause_kind, cause_id, late_from) \
         SELECT $1, p.person_id, $3, $4, $5, $6, $7 \
         FROM unnest($2::uuid[]) WITH ORDINALITY AS p(person_id, n) ORDER BY p.n \
         ON CONFLICT (workspace_id, person_id) DO UPDATE SET \
             classes = person_changes.classes | EXCLUDED.classes, \
             trace_id = EXCLUDED.trace_id, cause_kind = EXCLUDED.cause_kind, \
             cause_id = EXCLUDED.cause_id, \
             late_from = LEAST(person_changes.late_from, EXCLUDED.late_from)",
    )
    .bind(workspace.as_uuid())
    .bind(&sorted)
    .bind(i16::from(classes.bits()))
    .bind(trace.trace_id.as_uuid())
    .bind(trace.cause.kind.as_str())
    .bind(&trace.cause.id)
    .bind(late_from)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// [`mark_people`] for one person.
pub(crate) async fn mark_person(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    person: Uuid,
    classes: ChangeClasses,
    trace: &TraceContext,
    late_from: Option<OffsetDateTime>,
) -> Result<(), StoreError> {
    mark_people(connection, workspace, &[person], classes, trace, late_from).await
}

/// Enqueues the workspace's `segment.reevaluate`, once per transaction that marked anyone. A
/// no-op while one is ready or running (its unique key).
///
/// This only buys latency. Correctness never depends on it: a mark that no request follows (a
/// path that missed the call, a request that rolled back, a mark made after the job's last
/// claim) is found by the leader's safety net, which enqueues for any mark older than 2 s.
pub async fn enqueue_reevaluate<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    workspace: WorkspaceId,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let mut job = NewJob::new(REEVALUATE_JOB_KIND, trace.clone());
    job.workspace_id = Some(workspace);
    job.payload = json!({ "workspace_id": workspace.as_uuid() });
    job.unique_key = Some(unique_key(workspace));
    enqueue(executor, &job).await
}

fn unique_key(workspace: WorkspaceId) -> String {
    format!("{REEVALUATE_JOB_KIND}:{}", workspace.as_uuid())
}

/// [`enqueue_reevaluate`] for a transaction that marked people, on a trace of its own: the
/// job's trace only names the decisions the job records itself. Call it once, before the
/// transaction commits.
pub(crate) async fn request_reevaluation(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<(), StoreError> {
    let trace = TraceContext::root(Cause::from_stored(
        CauseKind::Api,
        "segment.reevaluate".to_owned(),
    ));
    enqueue_reevaluate(connection, workspace, &trace).await?;
    Ok(())
}

type ClaimedRow = (
    Uuid,
    i16,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<OffsetDateTime>,
);

/// Claims up to `limit` of the workspace's oldest marks and returns them in person order.
/// Rows another claim holds are skipped, so concurrent claims take disjoint people. The marks
/// stay deleted only if the caller's transaction commits; a mark made after the claim inserts
/// its row afresh.
pub async fn claim_dirty(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    limit: i64,
) -> Result<Vec<DirtyPerson>, StoreError> {
    let rows: Vec<ClaimedRow> = sqlx::query_as(
        "DELETE FROM person_changes WHERE (workspace_id, person_id) IN ( \
             SELECT workspace_id, person_id FROM person_changes WHERE workspace_id = $1 \
             ORDER BY changed_at, person_id FOR UPDATE SKIP LOCKED LIMIT $2) \
         RETURNING person_id, classes, trace_id, cause_kind, cause_id, late_from",
    )
    .bind(workspace.as_uuid())
    .bind(limit)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    let mut claimed: Vec<DirtyPerson> = rows
        .into_iter()
        .map(
            |(person_id, classes, trace, kind, id, late_from)| DirtyPerson {
                person_id,
                classes: ChangeClasses::from_bits(u8::try_from(classes).unwrap_or(u8::MAX)),
                trace: trace_of(trace, kind, id),
                late_from,
            },
        )
        .collect();
    claimed.sort_by_key(|person| person.person_id);
    Ok(claimed)
}

fn trace_of(trace: Option<Uuid>, kind: Option<String>, id: Option<String>) -> Option<TraceContext> {
    let kind = kind?.parse().ok()?;
    Some(TraceContext {
        trace_id: TraceId::from_uuid(trace?),
        cause: Cause::from_stored(kind, id?),
    })
}

/// A workspace's oldest dirty mark.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirtyHead {
    pub workspace_id: WorkspaceId,
    /// How long the oldest mark has waited, by the database's clock.
    pub age_seconds: f64,
}

/// The oldest mark of every workspace that has any: one index probe per workspace on
/// `(workspace_id, changed_at)`, never a scan of the set.
pub async fn dirty_heads(pool: &PgPool) -> Result<Vec<DirtyHead>, StoreError> {
    let rows: Vec<(Uuid, f64)> = sqlx::query_as(
        "SELECT w.id, EXTRACT(EPOCH FROM clock_timestamp() - head.changed_at)::float8 \
         FROM workspaces w \
         CROSS JOIN LATERAL ( \
             SELECT changed_at FROM person_changes WHERE workspace_id = w.id \
             ORDER BY changed_at LIMIT 1) head",
    )
    .fetch_all(pool)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(workspace, age_seconds)| DirtyHead {
            workspace_id: WorkspaceId::from_uuid(workspace),
            age_seconds,
        })
        .collect())
}

/// How many people are marked, over all workspaces: the dirty backlog.
pub async fn dirty_backlog(pool: &PgPool) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT count(*) FROM person_changes")
        .fetch_one(pool)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::{begin, commit, create_workspace};

    fn trace(cause: Cause) -> TraceContext {
        TraceContext::root(cause)
    }

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn person(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO people (id, workspace_id) VALUES ($1, $2)")
            .bind(id)
            .bind(ws.as_uuid())
            .execute(pool)
            .await
            .unwrap();
        id
    }

    async fn mark(
        pool: &PgPool,
        ws: WorkspaceId,
        people: &[Uuid],
        classes: ChangeClasses,
        trace: &TraceContext,
        late: Option<OffsetDateTime>,
    ) {
        let mut tx = begin(pool).await.unwrap();
        mark_people(&mut tx, ws, people, classes, trace, late)
            .await
            .unwrap();
        commit(tx).await.unwrap();
    }

    async fn claim(pool: &PgPool, ws: WorkspaceId, limit: i64) -> Vec<DirtyPerson> {
        let mut tx = begin(pool).await.unwrap();
        let claimed = claim_dirty(&mut tx, ws, limit).await.unwrap();
        commit(tx).await.unwrap();
        claimed
    }

    #[test]
    fn a_stored_cause_that_is_incomplete_or_unknown_has_no_trace() {
        let id = Uuid::from_u128(1);
        assert!(trace_of(Some(id), Some("event".into()), None).is_none());
        assert!(trace_of(None, Some("event".into()), Some("e".into())).is_none());
        assert!(trace_of(Some(id), Some("nonsense".into()), Some("e".into())).is_none());
        let found = trace_of(Some(id), Some("sync_run".into()), Some("r".into())).unwrap();
        assert_eq!(found.trace_id.as_uuid(), id);
        assert_eq!(found.cause.kind, CauseKind::SyncRun);
        assert_eq!(found.cause.id, "r");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn marks_coalesce_by_oring_classes_and_keeping_the_latest_trace_and_earliest_times(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        let one = person(&pool, ws).await;
        let first = trace(Cause::api("first"));
        let second = trace(Cause::event(Uuid::from_u128(9)));

        mark(
            &pool,
            ws,
            &[one],
            ChangeClasses::TRAITS,
            &first,
            Some(datetime!(2030-01-02 00:00 UTC)),
        )
        .await;
        mark(
            &pool,
            ws,
            &[one],
            ChangeClasses::LISTS,
            &second,
            Some(datetime!(2030-01-01 00:00 UTC)),
        )
        .await;
        mark(&pool, ws, &[one], ChangeClasses::EVENTS, &second, None).await;

        let claimed = claim(&pool, ws, 10).await;
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            claimed[0].classes,
            ChangeClasses::TRAITS | ChangeClasses::LISTS | ChangeClasses::EVENTS
        );
        assert_eq!(claimed[0].trace.as_ref(), Some(&second));
        assert_eq!(claimed[0].late_from, Some(datetime!(2030-01-01 00:00 UTC)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn re_marking_keeps_the_first_changed_at(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let one = person(&pool, ws).await;
        mark(
            &pool,
            ws,
            &[one],
            ChangeClasses::TRAITS,
            &trace(Cause::api("a")),
            None,
        )
        .await;
        sqlx::query("UPDATE person_changes SET changed_at = changed_at - interval '1 hour'")
            .execute(&pool)
            .await
            .unwrap();
        let before: OffsetDateTime = sqlx::query_scalar("SELECT changed_at FROM person_changes")
            .fetch_one(&pool)
            .await
            .unwrap();

        mark(
            &pool,
            ws,
            &[one],
            ChangeClasses::LISTS,
            &trace(Cause::api("b")),
            None,
        )
        .await;

        let after: OffsetDateTime = sqlx::query_scalar("SELECT changed_at FROM person_changes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(before, after);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mark_made_after_a_claim_survives_it_and_a_rolled_back_claim_restores_its_batch(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "ours").await;
        let (one, two) = (person(&pool, ws).await, person(&pool, ws).await);
        let a = trace(Cause::api("a"));
        mark(&pool, ws, &[one, two], ChangeClasses::TRAITS, &a, None).await;

        // A failed pass restores its batch.
        let mut tx = begin(&pool).await.unwrap();
        assert_eq!(claim_dirty(&mut tx, ws, 10).await.unwrap().len(), 2);
        drop(tx);
        assert_eq!(dirty_backlog(&pool).await.unwrap(), 2);

        // A mark made after the claim re-inserts its row, and the claim does not remove it.
        let mut tx = begin(&pool).await.unwrap();
        assert_eq!(claim_dirty(&mut tx, ws, 10).await.unwrap().len(), 2);
        commit(tx).await.unwrap();
        mark(&pool, ws, &[one], ChangeClasses::LISTS, &a, None).await;
        let left = claim(&pool, ws, 10).await;
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].person_id, one);
        assert_eq!(left[0].classes, ChangeClasses::LISTS);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_claims_take_disjoint_people_oldest_first(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let mut people = Vec::new();
        for _ in 0..6 {
            people.push(person(&pool, ws).await);
        }
        mark(
            &pool,
            ws,
            &people,
            ChangeClasses::TRAITS,
            &trace(Cause::api("a")),
            None,
        )
        .await;
        let mut first = begin(&pool).await.unwrap();
        let mut second = begin(&pool).await.unwrap();

        let one = claim_dirty(&mut first, ws, 4).await.unwrap();
        let two = claim_dirty(&mut second, ws, 4).await.unwrap();

        assert_eq!((one.len(), two.len()), (4, 2));
        let mut all: Vec<Uuid> = one.iter().chain(&two).map(|p| p.person_id).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 6);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_heads_name_each_workspace_with_marks_and_the_backlog_counts_them(pool: PgPool) {
        let (busy, idle) = (
            workspace(&pool, "busy").await,
            workspace(&pool, "idle").await,
        );
        let (one, two) = (person(&pool, busy).await, person(&pool, busy).await);
        mark(
            &pool,
            busy,
            &[one, two],
            ChangeClasses::TRAITS,
            &trace(Cause::api("a")),
            None,
        )
        .await;
        sqlx::query("UPDATE person_changes SET changed_at = now() - interval '30 seconds'")
            .execute(&pool)
            .await
            .unwrap();

        let heads = dirty_heads(&pool).await.unwrap();

        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].workspace_id, busy);
        assert!((29.0..120.0).contains(&heads[0].age_seconds));
        assert_eq!(dirty_backlog(&pool).await.unwrap(), 2);
        assert!(heads.iter().all(|head| head.workspace_id != idle));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_mark_enqueues_nothing_and_a_request_is_one_job_per_workspace(pool: PgPool) {
        let ws = workspace(&pool, "ours").await;
        let (one, two) = (person(&pool, ws).await, person(&pool, ws).await);
        let a = trace(Cause::api("a"));
        mark(&pool, ws, &[one, two], ChangeClasses::TRAITS, &a, None).await;
        let jobs = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM jobs WHERE kind = 'segment.reevaluate'",
            )
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert_eq!(jobs(pool.clone()).await, 0);

        assert!(enqueue_reevaluate(&pool, ws, &a).await.unwrap());
        assert!(!enqueue_reevaluate(&pool, ws, &a).await.unwrap());

        assert_eq!(jobs(pool.clone()).await, 1);
    }
}
