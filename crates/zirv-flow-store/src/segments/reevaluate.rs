//! The incremental re-evaluation of segments for a batch of dirty people (architecture 8.4):
//! which segments a batch reaches and in what order, and the batch-restricted diff. This crate
//! never sees the compiler: the job compiles each definition and passes in the SQL.

use std::collections::BTreeSet;

use hashbrown::{HashMap, HashSet};
use serde_json::Value;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::segment::ChangeClasses;
use zirv_flow_core::{TraceContext, WorkspaceId};

use super::preview::{SqlQuery, bind_all};
use super::refresh::{
    MembershipCause, RefreshOutcome, Scope, lock_segment_refresh, refresh_statement,
};
use crate::{DirtyPerson, StoreError};

/// A claimed batch as the parallel arrays the diff statement unnests: each person with the
/// trace and cause of the latest change that marked it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirtyBatch {
    people: Vec<Uuid>,
    traces: Vec<Uuid>,
    cause_kinds: Vec<String>,
    cause_ids: Vec<String>,
    /// Every class that changed for anyone in the batch.
    classes: ChangeClasses,
}

impl DirtyBatch {
    /// The batch of `claimed`. A mark that predates traces falls back to `fallback`, the job's.
    pub fn new(claimed: &[DirtyPerson], fallback: &TraceContext) -> Self {
        let mut batch = Self::default();
        for person in claimed {
            let trace = person.trace.as_ref().unwrap_or(fallback);
            batch.people.push(person.person_id);
            batch.traces.push(trace.trace_id.as_uuid());
            batch.cause_kinds.push(trace.cause.kind.as_str().to_owned());
            batch.cause_ids.push(trace.cause.id.clone());
            batch.classes = batch.classes | person.classes;
        }
        batch
    }

    /// The people of the batch, in the order of the arrays.
    pub fn people(&self) -> &[Uuid] {
        &self.people
    }

    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
    }

    /// The union of the classes the batch's marks carry.
    pub const fn classes(&self) -> ChangeClasses {
        self.classes
    }
}

/// The one total order every writer that touches several segments' rows locks them in: a
/// segment comes after every segment it references, ties go by id, and archived segments are
/// ordered with the rest. The passes, merges and erasures all lock through it, so no two of
/// them can wait on each other.
async fn lock_order(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<(Uuid, Deps)>, StoreError> {
    let rows: Vec<(Uuid, Value, bool)> = sqlx::query_as(
        "SELECT s.id, v.deps, s.archived_at IS NOT NULL FROM segments s \
         JOIN segment_versions v ON v.segment_id = s.id AND v.version = s.current_version \
         WHERE s.workspace_id = $1",
    )
    .bind(workspace.as_uuid())
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows
        .into_iter()
        .map(|(id, deps, archived)| {
            let mut deps = Deps::parse(&deps);
            deps.archived = archived;
            (id, deps)
        })
        .collect())
}

/// Takes the refresh lock of each segment in `segments`, which are already in lock order.
async fn acquire(connection: &mut PgConnection, segments: &[Uuid]) -> Result<(), StoreError> {
    for &segment in segments {
        lock_segment_refresh(connection, segment).await?;
    }
    Ok(())
}

/// The live segments one batch reaches, in the order to evaluate them: every segment whose
/// current version's dependency classes intersect `classes`, then the segments that reference
/// those, and so on. A segment comes after every segment it references; ties go by id, so
/// every pass locks and evaluates in one order. The segments are read now and judged again
/// once their lock is held.
pub async fn reevaluation_order(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    classes: ChangeClasses,
) -> Result<Vec<Uuid>, StoreError> {
    let all = lock_order(connection, workspace).await?;
    Ok(order_segments(all, classes))
}

/// [`reevaluation_order`], with each of its segments' refresh locks taken, in that order, for
/// the rest of the transaction. Read nothing about the segments before this returns.
pub async fn lock_reevaluation(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    classes: ChangeClasses,
) -> Result<Vec<Uuid>, StoreError> {
    let order = reevaluation_order(connection, workspace, classes).await?;
    acquire(connection, &order).await?;
    Ok(order)
}

/// Takes, in the same order as a pass, the refresh lock of every segment in which one of
/// `people` has an open row, for the rest of the transaction. A merge or an erasure calls this
/// before it writes any segment row, so none of them waits on a pass that waits on it.
pub(crate) async fn lock_segments_of_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<(), StoreError> {
    let touched: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT segment_id FROM segment_members \
         WHERE workspace_id = $1 AND person_id = ANY($2) AND exited_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if touched.is_empty() {
        return Ok(());
    }
    let position: HashMap<Uuid, usize> = global_order(lock_order(connection, workspace).await?)
        .into_iter()
        .enumerate()
        .map(|(at, id)| (id, at))
        .collect();
    let mut ordered = touched;
    ordered.sort_unstable_by_key(|id| (position.get(id).copied().unwrap_or(usize::MAX), *id));
    acquire(connection, &ordered).await
}

/// The parts of a version's `deps` the pass reads.
#[derive(Clone, Debug, Default)]
struct Deps {
    classes: ChangeClasses,
    segments: BTreeSet<Uuid>,
    archived: bool,
}

impl Deps {
    fn parse(deps: &Value) -> Self {
        let classes = deps
            .get("classes")
            .and_then(Value::as_u64)
            .and_then(|bits| u8::try_from(bits).ok())
            .map_or(ChangeClasses::NONE, ChangeClasses::from_bits);
        let segments = deps
            .get("segments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().and_then(|id| Uuid::parse_str(id).ok()))
            .collect();
        Self {
            classes,
            segments,
            archived: false,
        }
    }
}

/// Every segment, each after the segments it references, the smallest id first among those
/// that are ready (Kahn's algorithm). Segments on a reference cycle, which cannot be saved,
/// come last by id.
fn global_order(segments: Vec<(Uuid, Deps)>) -> Vec<Uuid> {
    let deps: HashMap<Uuid, Deps> = segments.into_iter().collect();
    let mut referenced_by: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (id, own) in &deps {
        for referenced in own.segments.iter().filter(|r| deps.contains_key(*r)) {
            referenced_by.entry(*referenced).or_default().push(*id);
        }
    }
    let mut waiting: HashMap<Uuid, usize> = deps
        .iter()
        .map(|(id, own)| {
            let needs = own
                .segments
                .iter()
                .filter(|r| deps.contains_key(*r))
                .count();
            (*id, needs)
        })
        .collect();
    let mut ready: BTreeSet<Uuid> = waiting
        .iter()
        .filter(|(_, needs)| **needs == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order = Vec::with_capacity(deps.len());
    while let Some(id) = ready.pop_first() {
        waiting.remove(&id);
        order.push(id);
        for dependent in referenced_by.get(&id).into_iter().flatten() {
            if let Some(needs) = waiting.get_mut(dependent) {
                *needs -= 1;
                if *needs == 0 {
                    ready.insert(*dependent);
                }
            }
        }
    }
    let mut left: Vec<Uuid> = waiting.into_keys().collect();
    left.sort_unstable();
    order.extend(left);
    order
}

/// The live segments `classes` reach and everything that references them, in [`global_order`].
fn order_segments(segments: Vec<(Uuid, Deps)>, classes: ChangeClasses) -> Vec<Uuid> {
    let mut referenced_by: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for (id, own) in &segments {
        for referenced in &own.segments {
            referenced_by.entry(*referenced).or_default().push(*id);
        }
    }
    let live: HashSet<Uuid> = segments
        .iter()
        .filter(|(_, own)| !own.archived)
        .map(|(id, _)| *id)
        .collect();
    let mut chosen: HashSet<Uuid> = HashSet::new();
    let mut pending: Vec<Uuid> = segments
        .iter()
        .filter(|(id, own)| live.contains(id) && own.classes.intersects(classes))
        .map(|(id, _)| *id)
        .collect();
    while let Some(id) = pending.pop() {
        if chosen.insert(id) {
            pending.extend(
                referenced_by
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|dependent| live.contains(dependent)),
            );
        }
    }
    if chosen.is_empty() {
        return Vec::new();
    }
    global_order(segments)
        .into_iter()
        .filter(|id| chosen.contains(id))
        .collect()
}

/// One batch-restricted diff to apply.
#[derive(Clone, Debug)]
pub struct ReevaluateApply<'a> {
    pub workspace: WorkspaceId,
    pub segment: Uuid,
    /// The version the batch query was compiled from.
    pub version: i32,
    /// The compiled batch query, selecting `p.id` and reading the batch's people as its last
    /// value, `$k + 2`: the compiler's `batch_sql`. This holds the compiled values only.
    pub members: &'a SqlQuery,
    /// The clock time the query was compiled at.
    pub now: OffsetDateTime,
    pub batch: &'a DirtyBatch,
    /// The job and attempt that re-evaluate.
    pub job: (Uuid, i32),
}

/// Diffs the segment's membership of the batch's people against the compiled query and applies
/// the difference, all in the caller's transaction, after [`lock_segment_refresh`]:
///
/// - a person in the set with no open row gets one and a `segment.entered` event;
/// - an open row of a batch person who is not in the set is closed with a `segment.exited` event;
/// - each change appends a decision, and its event and row carry the trace of the person's mark
///   and, in the decision, its cause;
/// - the segment's `member_count` follows. `computed_version` and `computed_at` are left: a
///   pass looks at some people, not the segment.
///
/// Nothing is enqueued for the segments that reference this one: the pass evaluates them next,
/// on the membership this wrote.
///
/// [`lock_segment_refresh`]: super::lock_segment_refresh
pub async fn apply_reevaluation(
    connection: &mut PgConnection,
    apply: &ReevaluateApply<'_>,
) -> Result<RefreshOutcome, StoreError> {
    let statement = refresh_statement(apply.members, Scope::Batch);
    let batch = apply.batch;
    // A custom plan for a segment that is missing from the `segment_members` statistics (new, or
    // not yet analyzed) estimates one row for its id, so the diff scans every open row of the
    // segment for each batch person. The generic plan probes `segment_members_open_idx` by
    // (segment_id, person_id) instead. The setting is local to the caller's transaction and is
    // restored right after, so its other statements keep custom plans.
    sqlx::query("SET LOCAL plan_cache_mode = force_generic_plan")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let counts = bind_all(
        sqlx::query_scalar::<_, Vec<i64>>(sqlx::AssertSqlSafe(statement)),
        apply.workspace,
        &apply.members.params,
    )
    .persistent(false)
    .bind(&batch.people)
    .bind(apply.segment)
    .bind(apply.version)
    .bind(apply.now)
    .bind(MembershipCause::DataChange.as_str())
    .bind(&batch.traces)
    .bind(&batch.cause_kinds)
    .bind(&batch.cause_ids)
    .bind(apply.job.0)
    .bind(apply.job.1)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query("SET LOCAL plan_cache_mode = DEFAULT")
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let outcome = RefreshOutcome {
        entered: counts[0],
        exited: counts[1],
    };
    if outcome.entered != outcome.exited {
        sqlx::query(
            "UPDATE segments SET member_count = member_count + $3 - $4 \
             WHERE workspace_id = $1 AND id = $2",
        )
        .bind(apply.workspace.as_uuid())
        .bind(apply.segment)
        .bind(outcome.entered)
        .bind(outcome.exited)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::Cause;

    use super::*;
    use crate::segments::tests_support::{person, segment, workspace};
    use crate::segments::{SqlParam, lock_segment_refresh};
    use crate::{begin, commit};

    const NOW: OffsetDateTime = datetime!(2030-01-01 12:00 UTC);

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn deps(classes: ChangeClasses, references: &[Uuid]) -> Deps {
        Deps::parse(&json!({
            "classes": classes.bits(),
            "segments": references.iter().map(Uuid::to_string).collect::<Vec<_>>(),
        }))
    }

    #[test]
    fn a_version_without_deps_reaches_nothing() {
        let parsed = Deps::parse(&json!({}));
        assert_eq!(parsed.classes, ChangeClasses::NONE);
        assert!(parsed.segments.is_empty());
        assert!(order_segments(vec![(id(1), parsed)], ChangeClasses::TRAITS).is_empty());
    }

    #[test]
    fn only_segments_whose_classes_intersect_are_reached() {
        let rows = vec![
            (id(1), deps(ChangeClasses::TRAITS, &[])),
            (
                id(2),
                deps(ChangeClasses::LISTS | ChangeClasses::MERGE, &[]),
            ),
            (id(3), deps(ChangeClasses::EVENTS, &[])),
        ];

        assert_eq!(order_segments(rows.clone(), ChangeClasses::TRAITS), [id(1)]);
        assert_eq!(
            order_segments(rows.clone(), ChangeClasses::LISTS | ChangeClasses::EVENTS),
            [id(2), id(3)]
        );
        assert_eq!(
            order_segments(rows, ChangeClasses::CONSENT),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn the_dependents_of_a_reached_segment_follow_it_transitively() {
        // 9 reads 5 reads 7; only 7 reads traits. 4 reads 7 too. 6 is unrelated.
        let rows = vec![
            (id(9), deps(ChangeClasses::NONE, &[id(5)])),
            (id(5), deps(ChangeClasses::NONE, &[id(7)])),
            (id(7), deps(ChangeClasses::TRAITS, &[])),
            (id(4), deps(ChangeClasses::NONE, &[id(7)])),
            (id(6), deps(ChangeClasses::EVENTS, &[])),
        ];

        let order = order_segments(rows, ChangeClasses::TRAITS);

        // A segment comes after the ones it references; the smallest id goes first among
        // those that are ready.
        assert_eq!(order, [id(7), id(4), id(5), id(9)]);
    }

    #[test]
    fn a_segment_waits_for_every_segment_it_references_even_when_its_id_is_smaller() {
        let rows = vec![
            (id(1), deps(ChangeClasses::NONE, &[id(8), id(2)])),
            (id(2), deps(ChangeClasses::NONE, &[id(8)])),
            (id(8), deps(ChangeClasses::TRAITS, &[])),
        ];

        assert_eq!(
            order_segments(rows, ChangeClasses::TRAITS),
            [id(8), id(2), id(1)]
        );
    }

    #[test]
    fn a_reference_cycle_is_still_evaluated_after_the_rest() {
        let rows = vec![
            (id(1), deps(ChangeClasses::TRAITS, &[id(2)])),
            (id(2), deps(ChangeClasses::NONE, &[id(1)])),
            (id(3), deps(ChangeClasses::TRAITS, &[])),
        ];

        assert_eq!(
            order_segments(rows, ChangeClasses::TRAITS),
            [id(3), id(1), id(2)]
        );
    }

    #[test]
    fn the_lock_order_is_one_total_order_that_a_subset_follows_and_an_archived_segment_joins() {
        // The chosen segments come out in the order the whole set has, so a merge that locks
        // some of them and a pass that locks others agree on who goes first.
        let mut archived = deps(ChangeClasses::TRAITS, &[id(9)]);
        archived.archived = true;
        let rows = vec![
            (id(9), deps(ChangeClasses::NONE, &[])),
            (id(1), deps(ChangeClasses::NONE, &[id(9)])),
            (id(5), deps(ChangeClasses::TRAITS, &[])),
            (id(3), archived),
        ];

        assert_eq!(global_order(rows.clone()), [id(5), id(9), id(1), id(3)]);
        // An archived segment is never chosen, and does not carry its reach to others.
        assert_eq!(order_segments(rows, ChangeClasses::TRAITS), [id(5)]);
    }
    #[test]
    fn a_batch_carries_the_union_of_classes_and_each_persons_own_trace() {
        let fallback = TraceContext::root(Cause::schedule(id(100)));
        let marked = TraceContext::root(Cause::event(id(7)));
        let claimed = [
            DirtyPerson {
                person_id: id(1),
                classes: ChangeClasses::TRAITS,
                trace: Some(marked.clone()),
                late_from: None,
            },
            DirtyPerson {
                person_id: id(2),
                classes: ChangeClasses::LISTS,
                trace: None,
                late_from: None,
            },
        ];

        let batch = DirtyBatch::new(&claimed, &fallback);

        assert_eq!(batch.people(), [id(1), id(2)]);
        assert_eq!(
            batch.classes(),
            ChangeClasses::TRAITS | ChangeClasses::LISTS
        );
        assert_eq!(
            batch.traces,
            [marked.trace_id.as_uuid(), fallback.trace_id.as_uuid()]
        );
        assert_eq!(batch.cause_kinds, ["event", "schedule"]);
        assert_eq!(batch.cause_ids, [id(7).to_string(), id(100).to_string()]);
        assert!(!batch.is_empty());
        assert!(DirtyBatch::default().is_empty());
    }

    #[test]
    fn the_batch_values_are_numbered_after_the_compiled_ones() {
        let members = SqlQuery {
            sql: "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND p.id = ANY($3::uuid[])"
                .into(),
            params: vec![SqlParam::Text("pro".into())],
        };

        let statement = refresh_statement(&members, Scope::Batch);

        // One compiled value ($2), the people $3, then segment $4, version $5, time $6, cause
        // $7, the three parallel arrays $8 to $10, the job $11 and the attempt $12.
        assert!(statement.starts_with(
            "WITH batch AS (SELECT * FROM unnest($3::uuid[], $8::uuid[], $9::text[], $10::text[])"
        ));
        assert!(statement.contains("segment_id = $4::uuid"));
        assert!(statement.contains("$11::uuid, $12::int4"));
        assert!(statement.contains("m.person_id = ANY($3::uuid[])"));
        assert!(!statement.contains("$13"));
    }

    fn pro_batch(plan: &str) -> SqlQuery {
        SqlQuery {
            sql: "SELECT p.id FROM people p WHERE p.workspace_id = $1 AND p.merged_into IS NULL \
                  AND (p.traits ->> 'plan') = $2::text AND p.id = ANY($3::uuid[])"
                .into(),
            params: vec![SqlParam::Text(plan.into())],
        }
    }

    async fn reevaluate(
        pool: &PgPool,
        ws: WorkspaceId,
        segment: Uuid,
        plan: &str,
        batch: &DirtyBatch,
    ) -> RefreshOutcome {
        let mut tx = begin(pool).await.unwrap();
        lock_segment_refresh(&mut tx, segment).await.unwrap();
        let outcome = apply_reevaluation(
            &mut tx,
            &ReevaluateApply {
                workspace: ws,
                segment,
                version: 1,
                members: &pro_batch(plan),
                now: NOW,
                batch,
                job: (Uuid::new_v4(), 1),
            },
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        outcome
    }

    fn dirty(person: Uuid, trace: &TraceContext) -> DirtyPerson {
        DirtyPerson {
            person_id: person,
            classes: ChangeClasses::TRAITS,
            trace: Some(trace.clone()),
            late_from: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_diff_touches_only_its_people_and_records_each_under_its_own_trace(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let bob = person(&pool, ws, "bob@example.com", "pro").await;
        let cy = person(&pool, ws, "cy@example.com", "free").await;
        let fallback = TraceContext::root(Cause::schedule(id(1)));
        let event = Uuid::now_v7();
        let ada_trace = TraceContext::root(Cause::event(event));
        let cy_trace = TraceContext::root(Cause::api("req"));

        // Ada and Cy are in the batch; Bob is a pro nobody has looked at yet.
        let batch = DirtyBatch::new(&[dirty(ada, &ada_trace), dirty(cy, &cy_trace)], &fallback);
        let first = reevaluate(&pool, ws, seg.id, "pro", &batch).await;

        assert_eq!(
            first,
            RefreshOutcome {
                entered: 1,
                exited: 0
            }
        );
        let open: Vec<Uuid> = sqlx::query_scalar(
            "SELECT person_id FROM segment_members WHERE segment_id = $1 AND exited_at IS NULL",
        )
        .bind(seg.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(open, [ada]);
        let (trace, cause): (Uuid, String) = sqlx::query_as(
            "SELECT entered_trace_id, entered_cause FROM segment_members WHERE person_id = $1",
        )
        .bind(ada)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (trace, cause.as_str()),
            (ada_trace.trace_id.as_uuid(), "data_change")
        );
        let event_row: (Uuid, Value) = sqlx::query_as(
            "SELECT trace_id, properties FROM events WHERE person_id = $1 AND name = 'segment.entered'",
        )
        .bind(ada)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(event_row.0, ada_trace.trace_id.as_uuid());
        assert_eq!(event_row.1["cause"], "data_change");
        let decision: (Uuid, String, String, Option<String>) = sqlx::query_as(
            "SELECT trace_id, cause_kind, cause_id, reason FROM decision_records \
             WHERE action = 'segment.entered' AND subject_ids ->> 'person' = $1",
        )
        .bind(ada.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            decision,
            (
                ada_trace.trace_id.as_uuid(),
                "event".to_owned(),
                event.to_string(),
                Some("data_change".to_owned())
            )
        );
        let count: i32 = sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        // A pass leaves the segment's full-refresh bookkeeping alone.
        let computed: (Option<i32>, Option<OffsetDateTime>) =
            sqlx::query_as("SELECT computed_version, computed_at FROM segments WHERE id = $1")
                .bind(seg.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(computed, (None, None));

        // Ada drops to free: only she is in the batch, so only she exits; Bob stays unseen.
        sqlx::query("UPDATE people SET traits = '{\"plan\": \"free\"}' WHERE id = $1")
            .bind(ada)
            .execute(&pool)
            .await
            .unwrap();
        let batch = DirtyBatch::new(&[dirty(ada, &ada_trace)], &fallback);
        let second = reevaluate(&pool, ws, seg.id, "pro", &batch).await;

        assert_eq!(
            second,
            RefreshOutcome {
                entered: 0,
                exited: 1
            }
        );
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM segment_members WHERE segment_id = $1 AND exited_at IS NULL",
        )
        .bind(seg.id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(open, 0);
        let count: i32 = sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM segment_members WHERE person_id = $1"
            )
            .bind(bob)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unchanged_batch_writes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let trace = TraceContext::root(Cause::api("req"));
        let batch = DirtyBatch::new(&[dirty(ada, &trace)], &trace);
        reevaluate(&pool, ws, seg.id, "pro", &batch).await;
        let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();

        let again = reevaluate(&pool, ws, seg.id, "pro", &batch).await;

        assert_eq!(
            again,
            RefreshOutcome {
                entered: 0,
                exited: 0
            }
        );
        let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events_before, events_after);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merged_person_in_the_batch_leaves_every_segment(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let seg = segment(&pool, ws, "Pros", 60).await;
        let ada = person(&pool, ws, "ada@example.com", "pro").await;
        let trace = TraceContext::root(Cause::api("req"));
        let batch = DirtyBatch::new(&[dirty(ada, &trace)], &trace);
        reevaluate(&pool, ws, seg.id, "pro", &batch).await;
        // A refresh that ran concurrently with a merge opened a row after the merge closed
        // them: the loser is merged but has an open row.
        let survivor = person(&pool, ws, "ada2@example.com", "pro").await;
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(ada)
            .bind(survivor)
            .execute(&pool)
            .await
            .unwrap();

        let outcome = reevaluate(&pool, ws, seg.id, "pro", &batch).await;

        assert_eq!(
            outcome,
            RefreshOutcome {
                entered: 0,
                exited: 1
            }
        );
        let count: i32 = sqlx::query_scalar("SELECT member_count FROM segments WHERE id = $1")
            .bind(seg.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    /// Opens `count` people of the plan and a row for each in `segment`.
    async fn fill(pool: &PgPool, ws: WorkspaceId, segment: Uuid, count: i32) {
        sqlx::query(
            "WITH made AS ( \
                 INSERT INTO people (id, workspace_id, traits) \
                 SELECT gen_random_uuid(), $1, '{\"plan\": \"pro\"}' FROM generate_series(1, $3) \
                 RETURNING id) \
             INSERT INTO segment_members \
                 (workspace_id, segment_id, person_id, version, entered_at, entered_trace_id, \
                  entered_cause) \
             SELECT $1, $2, id, 1, now(), gen_random_uuid(), 'initial' FROM made",
        )
        .bind(ws.as_uuid())
        .bind(segment)
        .bind(count)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_diff_of_a_segment_missing_from_the_statistics_probes_by_person(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let known = segment(&pool, ws, "Known", 60).await;
        fill(&pool, ws, known.id, 300).await;
        sqlx::query("ANALYZE segment_members")
            .execute(&pool)
            .await
            .unwrap();
        // The statistics know only the first segment when this one fills.
        let fresh = segment(&pool, ws, "Fresh", 60).await;
        fill(&pool, ws, fresh.id, 20_000).await;
        let people: Vec<Uuid> = sqlx::query_scalar(
            "SELECT person_id FROM segment_members WHERE segment_id = $1 LIMIT 3",
        )
        .bind(fresh.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        let trace = TraceContext::root(Cause::api("req"));
        let dirty_people: Vec<DirtyPerson> = people.iter().map(|p| dirty(*p, &trace)).collect();
        let batch = DirtyBatch::new(&dirty_people, &trace);

        let mut tx = begin(&pool).await.unwrap();
        lock_segment_refresh(&mut tx, fresh.id).await.unwrap();
        let read = "SELECT COALESCE(sum(seq_tup_read + idx_tup_fetch), 0)::bigint \
                    FROM pg_stat_xact_user_tables WHERE relname = 'segment_members'";
        let before: i64 = sqlx::query_scalar(read).fetch_one(&mut *tx).await.unwrap();
        let outcome = apply_reevaluation(
            &mut tx,
            &ReevaluateApply {
                workspace: ws,
                segment: fresh.id,
                version: 1,
                members: &pro_batch("pro"),
                now: NOW,
                batch: &batch,
                job: (Uuid::new_v4(), 1),
            },
        )
        .await
        .unwrap();
        let after: i64 = sqlx::query_scalar(read).fetch_one(&mut *tx).await.unwrap();
        let mode: String = sqlx::query_scalar("SHOW plan_cache_mode")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert_eq!(
            outcome,
            RefreshOutcome {
                entered: 0,
                exited: 0
            }
        );
        // The rows read follow the batch, not the segment's 20 000 open rows.
        assert!(after - before < 100, "read {} rows", after - before);
        // The setting does not leak into the rest of the transaction.
        assert_eq!(mode, "auto");
    }
}
