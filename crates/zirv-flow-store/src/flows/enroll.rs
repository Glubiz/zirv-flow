//! Enrolling people in a flow (architecture 9.4): the re-entry rule, the enrollments pinned to
//! the version they enter by, and the `flow.step` job that starts each of them, all in the
//! caller's transaction.
//!
//! Enrolls of one flow run one after another (a transaction advisory lock), so the checks below
//! and the writes that follow cannot interleave with another enroll of the flow. The partial
//! unique index on in-flight enrollments is the backstop.

use std::fmt;

use hashbrown::{HashMap, HashSet};
use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::flow::{FlowGraph, FlowSettings, Reentry};
use zirv_flow_core::{Cause, TraceContext, TraceId, WorkspaceId};

use super::runtime::{StepJobInput, StepJobPayload};
use super::state::FlowState;
use crate::{GraphPoint, NewDecision, NewJob, StoreError, append_decisions, enqueue_many};

/// The most people one enroll takes (the size of a `flow.enroll` job).
pub const MAX_ENROLL_PEOPLE: usize = 1000;

/// The reason a person with no row is refused for. It has no decision: the person is gone, and a
/// decision would write their id back.
const NO_SUCH_PERSON: &str = "no_such_person";

/// What made the enrollment: the trigger kind and the id of its occurrence (an event id, a
/// segment event id, a schedule slot or the id of a manual or API request). Enrolling the same
/// person for the same occurrence twice changes nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Occurrence {
    pub trigger: &'static str,
    pub id: Uuid,
}

/// Who to enroll in which version of which flow.
#[derive(Clone, Debug)]
pub struct EnrollRequest<'a> {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    pub version: i32,
    pub people: &'a [Uuid],
    pub occurrence: Occurrence,
    /// The re-entry interval is measured against this instant.
    pub now: OffsetDateTime,
    /// The trace of the work that enrolls. A refused person's decision is on it; each enrollment
    /// starts a trace of its own with the same cause.
    pub trace: &'a TraceContext,
}

/// A person who entered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Enrolled {
    pub person: Uuid,
    pub enrollment: Uuid,
    /// The enrollment's trace.
    pub trace: TraceContext,
}

/// What an enroll did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnrollOutcome {
    pub enrolled: Vec<Enrolled>,
    /// Refused people with the reason of their `flow.enroll` decision.
    pub skipped: Vec<(Uuid, &'static str)>,
    /// People this occurrence had already enrolled: nothing was written for them.
    pub replayed: usize,
}

/// Why an enroll was refused as a whole.
#[derive(Debug)]
pub enum EnrollError {
    /// No such flow, or no such version of it.
    NoSuchVersion,
    /// More than [`MAX_ENROLL_PEOPLE`] people.
    TooMany,
    /// A stored graph or settings that do not parse, or a graph with no entry.
    Corrupt(&'static str),
    Store(StoreError),
}

impl fmt::Display for EnrollError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchVersion => formatter.write_str("no such flow version"),
            Self::TooMany => write!(
                formatter,
                "an enroll takes at most {MAX_ENROLL_PEOPLE} people"
            ),
            Self::Corrupt(what) => write!(formatter, "the stored flow version is unusable: {what}"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for EnrollError {}

impl From<StoreError> for EnrollError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Enrolls `request.people` in the version, or says why not, person by person. Every person
/// enters for the request's one occurrence; see [`enroll_entries`] for people who each have an
/// occurrence of their own.
pub async fn enroll(
    connection: &mut PgConnection,
    request: &EnrollRequest<'_>,
) -> Result<EnrollOutcome, EnrollError> {
    let entries: Vec<EnrollEntry> = request
        .people
        .iter()
        .map(|person| EnrollEntry {
            person: *person,
            occurrence: request.occurrence.id,
            cause: None,
        })
        .collect();
    enroll_entries(
        connection,
        &EnrollEntries {
            workspace: request.workspace,
            flow: request.flow,
            version: request.version,
            trigger: request.occurrence.trigger,
            entries: &entries,
            now: request.now,
            trace: request.trace,
        },
    )
    .await
}

/// One person to enroll for one occurrence.
#[derive(Clone, Debug)]
pub struct EnrollEntry {
    pub person: Uuid,
    /// The occurrence this person enters for: an event id, a segment event id, a schedule slot or
    /// the id of a request.
    pub occurrence: Uuid,
    /// The cause of this enrollment when it is not the request trace's (an event trigger is
    /// caused by its own event).
    pub cause: Option<Cause>,
}

/// Who to enroll in which version of which flow, each person for an occurrence of their own.
#[derive(Clone, Debug)]
pub struct EnrollEntries<'a> {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    pub version: i32,
    /// The trigger kind every entry came from.
    pub trigger: &'static str,
    pub entries: &'a [EnrollEntry],
    /// The re-entry interval is measured against this instant.
    pub now: OffsetDateTime,
    /// The trace of the work that enrolls, as for [`EnrollRequest`].
    pub trace: &'a TraceContext,
}

/// Enrolls the people of `request.entries`, or says why not, person by person.
///
/// Each enrolled person gets an `active` enrollment pinned to `request.version` and a `flow.step`
/// job that starts it. The re-entry rule of the version's settings applies: `once` refuses a
/// person with a `flow_entries` row, `once_at_a_time` refuses while the person has an in-flight
/// enrollment, `always` refuses until the minimum interval since the last entry has passed. A
/// person with an in-flight enrollment is refused in every mode. A flow that is not published
/// enrolls nobody. A refusal is a `flow.enroll` decision with outcome `skipped` and its reason.
///
/// An entry whose `(flow, person, occurrence)` already enrolled is a replay and changes nothing.
/// A person who appears twice enters once, by their first entry. People are written in id order,
/// so two enrolls that touch the same people take their rows in the same order. The people are
/// locked `FOR KEY SHARE` first (see [`lock_people`]), so a merge or an erasure cannot commit
/// between the checks and the writes: a person merged meanwhile enters as the survivor and one
/// erased meanwhile is refused as `no_such_person`.
pub async fn enroll_entries(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
) -> Result<EnrollOutcome, EnrollError> {
    if request.entries.len() > MAX_ENROLL_PEOPLE {
        return Err(EnrollError::TooMany);
    }
    lock_enrolls(connection, request.flow).await?;
    let (state, graph, settings) = read_version(connection, request).await?;
    let entry = graph
        .entry()
        .ok_or(EnrollError::Corrupt("the graph has no entry node"))?
        .id
        .clone();

    // A person merged into another since the work was queued enters as the survivor, so the
    // re-entry rule decides for the one person they now are. The people are locked before
    // anything is read about them or written for them; a person with no row (erased) keeps the
    // id they were named by and is refused below.
    let (resolved, existing) = lock_people(connection, request).await?;

    let mut seen = HashSet::new();
    let mut by_person: Vec<&(EnrollEntry, Uuid)> = resolved
        .iter()
        .filter(|(entry, _)| seen.insert(entry.person))
        .collect();
    by_person.sort_unstable_by_key(|(entry, _)| entry.person);
    let entries: Vec<&EnrollEntry> = by_person.iter().map(|(entry, _)| entry).collect();
    let people: Vec<Uuid> = entries.iter().map(|entry| entry.person).collect();
    let entered = entries_of(connection, request, &people).await?;
    let in_flight = in_flight_of(connection, request, &people).await?;
    // An enrollment made before the merge sits under the id the entry was named by.
    let pairs: Vec<(Uuid, Uuid)> = by_person
        .iter()
        .flat_map(|(entry, named)| [(entry.person, entry.occurrence), (*named, entry.occurrence)])
        .collect();
    let replayed_pairs = replayed_of(connection, request, &pairs).await?;
    let replayed: HashSet<Uuid> = by_person
        .iter()
        .filter(|(entry, named)| {
            replayed_pairs.contains(&(entry.person, entry.occurrence))
                || replayed_pairs.contains(&(*named, entry.occurrence))
        })
        .map(|(entry, _)| entry.person)
        .collect();

    let mut outcome = EnrollOutcome::default();
    let mut accepted = Vec::new();
    for entry in entries {
        let person = entry.person;
        if replayed.contains(&person) {
            outcome.replayed += 1;
            continue;
        }
        let refusal = refusal(
            state,
            &settings.reentry,
            existing.contains(&person),
            in_flight.contains(&person),
            entered.get(&person).copied(),
            request.now,
        );
        match refusal {
            Some(reason) => outcome.skipped.push((person, reason)),
            None => accepted.push(entry),
        }
    }

    // A person with no row was erased: nothing is written that names them. The skip is still in
    // `outcome.skipped`, where the caller counts it.
    let mut decisions: Vec<NewDecision> = outcome
        .skipped
        .iter()
        .filter(|(_, reason)| *reason != NO_SUCH_PERSON)
        .map(|(person, reason)| NewDecision {
            graph: Some(GraphPoint {
                version: request.version,
                node: None,
            }),
            workspace_id: request.workspace,
            trace: request.trace.clone(),
            action: "flow.enroll",
            outcome: "skipped",
            reason: Some(reason),
            subject_ids: vec![("flow", request.flow), ("person", *person)],
            job: None,
        })
        .collect();
    let mut occurrence_of = Vec::with_capacity(accepted.len());
    for entry in accepted {
        let cause = entry
            .cause
            .clone()
            .unwrap_or_else(|| request.trace.cause.clone());
        outcome.enrolled.push(Enrolled {
            person: entry.person,
            enrollment: Uuid::now_v7(),
            trace: TraceContext {
                trace_id: TraceId::new(),
                cause,
            },
        });
        occurrence_of.push(entry.occurrence);
    }
    write_enrollments(
        connection,
        request,
        &entry,
        &outcome.enrolled,
        &occurrence_of,
    )
    .await?;
    // The start jobs go in with one statement: a fan-out of a thousand people is one round trip.
    let starts: Vec<NewJob> = outcome
        .enrolled
        .iter()
        .map(|enrolled| {
            let start = StepJobPayload {
                workspace: request.workspace,
                enrollment: enrolled.enrollment,
                state_version: 0,
                input: StepJobInput::Start,
            };
            let key = format!("start:{}", enrolled.enrollment);
            start.job(&enrolled.trace, None, key)
        })
        .collect();
    enqueue_many(&mut *connection, &starts).await?;
    for enrolled in &outcome.enrolled {
        decisions.push(NewDecision {
            graph: Some(GraphPoint {
                version: request.version,
                node: None,
            }),
            workspace_id: request.workspace,
            trace: enrolled.trace.clone(),
            action: "flow.enroll",
            outcome: "enrolled",
            reason: None,
            subject_ids: vec![
                ("flow", request.flow),
                ("person", enrolled.person),
                ("enrollment", enrolled.enrollment),
            ],
            job: None,
        });
    }
    append_decisions(connection, &decisions).await?;
    Ok(outcome)
}

/// The reason a person is refused, or `None` when they may enter.
fn refusal(
    state: FlowState,
    reentry: &Reentry,
    exists: bool,
    in_flight: bool,
    entries: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<&'static str> {
    match state {
        FlowState::Published => {}
        FlowState::Paused => return Some("flow_paused"),
        FlowState::Draft | FlowState::Archived => return Some("flow_not_live"),
    }
    if !exists {
        return Some(NO_SUCH_PERSON);
    }
    if in_flight {
        return Some("already_active");
    }
    match (reentry, entries) {
        (Reentry::Once {}, Some(_)) => Some("already_entered"),
        (
            Reentry::Always {
                min_interval_seconds,
            },
            Some(last),
        ) => {
            let wait = Duration::seconds(i64::try_from(*min_interval_seconds).unwrap_or(i64::MAX));
            let due = last.checked_add(wait);
            due.is_none_or(|due| due > now)
                .then_some("reentry_interval")
        }
        _ => None,
    }
}

pub(super) async fn lock_enrolls(
    connection: &mut PgConnection,
    flow: Uuid,
) -> Result<(), StoreError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('flow.enroll:' || $1::text, 0))")
        .bind(flow)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

async fn read_version(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
) -> Result<(FlowState, FlowGraph, FlowSettings), EnrollError> {
    let row: Option<(String, serde_json::Value, serde_json::Value)> = sqlx::query_as(
        "SELECT f.state, v.graph, v.settings FROM flows f \
         JOIN flow_versions v ON v.flow_id = f.id AND v.version = $3 \
         WHERE f.workspace_id = $1 AND f.id = $2",
    )
    .bind(request.workspace.as_uuid())
    .bind(request.flow)
    .bind(request.version)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    let (state, graph, settings) = row.ok_or(EnrollError::NoSuchVersion)?;
    let state = FlowState::parse(&state).ok_or(EnrollError::Corrupt("an unknown flow state"))?;
    let graph = FlowGraph::from_value(graph).map_err(|_| EnrollError::Corrupt("the graph"))?;
    let settings =
        serde_json::from_value(settings).map_err(|_| EnrollError::Corrupt("the settings"))?;
    Ok((state, graph, settings))
}

/// When each of `people` last entered the flow.
async fn entries_of(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
    people: &[Uuid],
) -> Result<HashMap<Uuid, OffsetDateTime>, StoreError> {
    let rows: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "SELECT person_id, last_entered_at FROM flow_entries \
         WHERE flow_id = $1 AND person_id = ANY($2)",
    )
    .bind(request.flow)
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

async fn in_flight_of(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
    people: &[Uuid],
) -> Result<HashSet<Uuid>, StoreError> {
    let rows: Vec<Uuid> = sqlx::query_scalar(
        "SELECT person_id FROM enrollments \
         WHERE flow_id = $1 AND person_id = ANY($2) AND state IN ('active', 'waiting', 'held', 'failed')",
    )
    .bind(request.flow)
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

/// The `(person, occurrence)` pairs of `pairs` that already have an enrollment.
async fn replayed_of(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
    pairs: &[(Uuid, Uuid)],
) -> Result<HashSet<(Uuid, Uuid)>, StoreError> {
    let people: Vec<Uuid> = pairs.iter().map(|(person, _)| *person).collect();
    let occurrences: Vec<Uuid> = pairs.iter().map(|(_, occurrence)| *occurrence).collect();
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT e.person_id, e.occurrence_id FROM enrollments e \
         JOIN unnest($2::uuid[], $3::uuid[]) AS t(person_id, occurrence_id) \
           ON e.person_id = t.person_id AND e.occurrence_id = t.occurrence_id \
         WHERE e.flow_id = $1",
    )
    .bind(request.flow)
    .bind(&people)
    .bind(&occurrences)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

/// How many times [`lock_people`] resolves the people again because a merge committed while it
/// waited for their locks, before it gives the enroll back as a retryable race.
pub(super) const LOCK_ROUNDS: usize = 4;

/// Resolves the entries' people to the people they are now and locks those rows `FOR KEY SHARE`,
/// in id order: a merge or an erasure, which lock a merged group `FOR UPDATE` in id order, then
/// wait for this transaction or are waited for, and the checks and the insert that follow see
/// the people they were made for. Returns each entry resolved (with the id it was named by) and
/// the people that exist and are not merged away.
///
/// A person merged (or merged again) while the lock was waited for resolves to the survivor in
/// the next round. The round's locks are released first, with a savepoint rollback, so the
/// survivor is never locked after a higher id. A person erased meanwhile has no row and is not in
/// the returned set. After [`LOCK_ROUNDS`] rounds the enroll is a retryable race.
///
/// An audience page (`lock_audience_page`) has locked its people and their survivors already, in
/// one id-ordered statement, so for those people this takes rows the transaction holds; only a
/// merge that committed after the page was locked can name a survivor not held yet.
async fn lock_people(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
) -> Result<(Vec<(EnrollEntry, Uuid)>, HashSet<Uuid>), EnrollError> {
    let named: Vec<Uuid> = request.entries.iter().map(|entry| entry.person).collect();
    for _ in 0..LOCK_ROUNDS {
        let survivors = resolve_survivors(connection, request.workspace, &named).await?;
        let resolved: Vec<(EnrollEntry, Uuid)> = request
            .entries
            .iter()
            .map(|entry| {
                let person = survivors
                    .get(&entry.person)
                    .copied()
                    .unwrap_or(entry.person);
                (
                    EnrollEntry {
                        person,
                        ..entry.clone()
                    },
                    entry.person,
                )
            })
            .collect();
        let mut people: Vec<Uuid> = resolved.iter().map(|(entry, _)| entry.person).collect();
        people.sort_unstable();
        people.dedup();
        sqlx::query("SAVEPOINT enroll_people")
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        let locked: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT id, merged_into FROM people WHERE workspace_id = $1 AND id = ANY($2) \
             ORDER BY id FOR KEY SHARE",
        )
        .bind(request.workspace.as_uuid())
        .bind(&people)
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if locked.iter().any(|(_, merged_into)| merged_into.is_some()) {
            sqlx::query("ROLLBACK TO SAVEPOINT enroll_people")
                .execute(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            continue;
        }
        sqlx::query("RELEASE SAVEPOINT enroll_people")
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
        let existing = locked.into_iter().map(|(id, _)| id).collect();
        return Ok((resolved, existing));
    }
    Err(StoreError::retryable_race().into())
}

/// The person each of `people` currently is: the survivor of one merged into another (merges are
/// compressed, so one hop is the whole chain). A person who is not merged, and one with no row,
/// are not in the map.
pub async fn resolve_survivors(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    people: &[Uuid],
) -> Result<HashMap<Uuid, Uuid>, StoreError> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, merged_into FROM people \
         WHERE workspace_id = $1 AND id = ANY($2) AND merged_into IS NOT NULL",
    )
    .bind(workspace.as_uuid())
    .bind(people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().collect())
}

/// Writes the enrollments and the entries. `occurrences` is parallel to `enrolled`.
async fn write_enrollments(
    connection: &mut PgConnection,
    request: &EnrollEntries<'_>,
    entry: &str,
    enrolled: &[Enrolled],
    occurrences: &[Uuid],
) -> Result<(), StoreError> {
    if enrolled.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = enrolled.iter().map(|e| e.enrollment).collect();
    let people: Vec<Uuid> = enrolled.iter().map(|e| e.person).collect();
    let traces: Vec<Uuid> = enrolled
        .iter()
        .map(|e| e.trace.trace_id.as_uuid())
        .collect();
    let kinds: Vec<&str> = enrolled
        .iter()
        .map(|e| e.trace.cause.kind.as_str())
        .collect();
    let causes: Vec<&str> = enrolled.iter().map(|e| e.trace.cause.id.as_str()).collect();
    // The context holds ids and the trigger kind only, never a property or a trait value.
    sqlx::query(
        "INSERT INTO enrollments \
            (id, workspace_id, flow_id, flow_version, person_id, state, node, context, \
             occurrence_id, state_version, trace_id, cause_kind, cause_id) \
         SELECT t.id, $1, $2, $3, t.person_id, 'active', $4, \
                jsonb_build_object('trigger', $5::text, 'occurrence', t.occurrence_id), \
                t.occurrence_id, 0, t.trace_id, t.cause_kind, t.cause_id \
         FROM unnest($6::uuid[], $7::uuid[], $8::uuid[], $9::uuid[], $10::text[], $11::text[]) \
              AS t(id, person_id, trace_id, occurrence_id, cause_kind, cause_id)",
    )
    .bind(request.workspace.as_uuid())
    .bind(request.flow)
    .bind(request.version)
    .bind(entry)
    .bind(request.trigger)
    .bind(&ids)
    .bind(&people)
    .bind(&traces)
    .bind(occurrences)
    .bind(&kinds)
    .bind(&causes)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, last_entered_at) \
         SELECT $1, $2, t.person_id, 1, $3 FROM unnest($4::uuid[]) AS t(person_id) \
         ON CONFLICT (flow_id, person_id) DO UPDATE SET \
             entries = flow_entries.entries + 1, last_entered_at = EXCLUDED.last_entered_at",
    )
    .bind(request.workspace.as_uuid())
    .bind(request.flow)
    .bind(request.now)
    .bind(&people)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::Cause;

    use super::*;
    use crate::flows::tests_support::{create, publish, workspace};
    use crate::flows::{FlowPatch, update_flow_draft};

    const NOW: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);

    #[test]
    fn the_reentry_rules_refuse_exactly_what_they_name() {
        let once = Reentry::Once {};
        let at_a_time = Reentry::OnceAtATime {};
        let always = Reentry::Always {
            min_interval_seconds: 60,
        };
        let live = FlowState::Published;
        let past = Some(NOW - Duration::seconds(30));
        let long_ago = Some(NOW - Duration::seconds(61));

        assert_eq!(refusal(live, &once, true, false, None, NOW), None);
        assert_eq!(
            refusal(live, &once, true, false, long_ago, NOW),
            Some("already_entered")
        );
        assert_eq!(refusal(live, &at_a_time, true, false, long_ago, NOW), None);
        assert_eq!(
            refusal(live, &at_a_time, true, true, None, NOW),
            Some("already_active")
        );
        assert_eq!(
            refusal(live, &always, true, false, past, NOW),
            Some("reentry_interval")
        );
        assert_eq!(refusal(live, &always, true, false, long_ago, NOW), None);
        assert_eq!(
            refusal(live, &always, true, true, long_ago, NOW),
            Some("already_active")
        );
        assert_eq!(
            refusal(live, &once, false, false, None, NOW),
            Some("no_such_person")
        );
        assert_eq!(
            refusal(FlowState::Paused, &once, true, false, None, NOW),
            Some("flow_paused")
        );
        assert_eq!(
            refusal(FlowState::Archived, &once, true, false, None, NOW),
            Some("flow_not_live")
        );
    }

    #[test]
    fn a_zero_interval_never_refuses_an_entry_in_the_past() {
        let always = Reentry::Always {
            min_interval_seconds: 0,
        };
        assert_eq!(
            refusal(FlowState::Published, &always, true, false, Some(NOW), NOW),
            None
        );
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

    /// A published flow of one `exit` node with the given re-entry mode.
    async fn flow(pool: &PgPool, ws: WorkspaceId, reentry: Value) -> Uuid {
        let flow = create(pool, ws, "Welcome").await;
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                settings: Some(json!({"trigger": {"type": "manual"}, "reentry": reentry})),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        publish(pool, ws, flow.id).await.unwrap();
        flow.id
    }

    async fn run(
        pool: &PgPool,
        ws: WorkspaceId,
        flow: Uuid,
        people: &[Uuid],
        occurrence: Uuid,
        now: OffsetDateTime,
    ) -> EnrollOutcome {
        let mut tx = crate::begin(pool).await.unwrap();
        let outcome = enroll(
            &mut tx,
            &EnrollRequest {
                workspace: ws,
                flow,
                version: 1,
                people,
                occurrence: Occurrence {
                    trigger: "manual",
                    id: occurrence,
                },
                now,
                trace: &TraceContext::root(Cause::api("enroll")),
            },
        )
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();
        outcome
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn enrolling_creates_pinned_enrollments_and_start_jobs(pool: PgPool) {
        let ws = workspace(&pool, "enroll").await;
        let flow = flow(&pool, ws, json!({"mode": "once"})).await;
        let (ada, bob) = (person(&pool, ws).await, person(&pool, ws).await);

        let outcome = run(&pool, ws, flow, &[ada, bob, ada], Uuid::now_v7(), NOW).await;

        assert_eq!(outcome.enrolled.len(), 2);
        assert!(outcome.skipped.is_empty());
        let rows: Vec<(String, String, i32, i64)> =
            sqlx::query_as("SELECT state, node, flow_version, state_version FROM enrollments")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| *row == ("active".into(), "draft".into(), 1, 0))
        );
        let jobs: Vec<(String, Option<String>, Value)> =
            sqlx::query_as("SELECT kind, unique_key, payload FROM jobs WHERE kind = 'flow.step'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(jobs.len(), 2);
        for enrolled in &outcome.enrolled {
            let key = format!("start:{}", enrolled.enrollment);
            assert!(jobs.iter().any(|job| job.1.as_deref() == Some(&key)));
        }
        let entries: i64 = sqlx::query_scalar("SELECT count(*) FROM flow_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(entries, 2);
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'flow.enroll' \
             AND outcome = 'enrolled'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn once_refuses_a_person_who_entered_before_even_after_the_enrollment_ended(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "once").await;
        let flow = flow(&pool, ws, json!({"mode": "once"})).await;
        let ada = person(&pool, ws).await;
        run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;
        sqlx::query(
            "UPDATE enrollments SET state = 'completed', ended_at = now(), state_version = 1",
        )
        .execute(&pool)
        .await
        .unwrap();

        let again = run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;

        assert!(again.enrolled.is_empty());
        assert_eq!(again.skipped, [(ada, "already_entered")]);
        let skipped: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE action = 'flow.enroll' \
             AND outcome = 'skipped' AND reason = 'already_entered'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(skipped, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn once_at_a_time_refuses_while_active_and_allows_a_new_entry_after(pool: PgPool) {
        let ws = workspace(&pool, "at-a-time").await;
        let flow = flow(&pool, ws, json!({"mode": "once_at_a_time"})).await;
        let ada = person(&pool, ws).await;
        run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;

        let busy = run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;
        sqlx::query(
            "UPDATE enrollments SET state = 'completed', ended_at = now(), state_version = 1",
        )
        .execute(&pool)
        .await
        .unwrap();
        let free = run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;

        assert_eq!(busy.skipped, [(ada, "already_active")]);
        assert_eq!(free.enrolled.len(), 1);
        let entries: i32 = sqlx::query_scalar("SELECT entries FROM flow_entries")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(entries, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn always_waits_out_its_minimum_interval(pool: PgPool) {
        let ws = workspace(&pool, "always").await;
        let flow = flow(
            &pool,
            ws,
            json!({"mode": "always", "min_interval_seconds": 3600}),
        )
        .await;
        let ada = person(&pool, ws).await;
        run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;
        sqlx::query(
            "UPDATE enrollments SET state = 'completed', ended_at = now(), state_version = 1",
        )
        .execute(&pool)
        .await
        .unwrap();

        let early = run(
            &pool,
            ws,
            flow,
            &[ada],
            Uuid::now_v7(),
            NOW + Duration::minutes(59),
        )
        .await;
        let later = run(
            &pool,
            ws,
            flow,
            &[ada],
            Uuid::now_v7(),
            NOW + Duration::minutes(61),
        )
        .await;

        assert_eq!(early.skipped, [(ada, "reentry_interval")]);
        assert_eq!(later.enrolled.len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_same_occurrence_enrolls_nobody_twice_and_unknown_people_are_skipped(pool: PgPool) {
        let ws = workspace(&pool, "replay").await;
        let flow = flow(&pool, ws, json!({"mode": "always"})).await;
        let ada = person(&pool, ws).await;
        let ghost = Uuid::now_v7();
        let occurrence = Uuid::now_v7();
        run(&pool, ws, flow, &[ada], occurrence, NOW).await;
        sqlx::query(
            "UPDATE enrollments SET state = 'completed', ended_at = now(), state_version = 1",
        )
        .execute(&pool)
        .await
        .unwrap();

        let again = run(&pool, ws, flow, &[ada, ghost], occurrence, NOW).await;

        assert_eq!(again.replayed, 1);
        assert!(again.enrolled.is_empty());
        assert_eq!(again.skipped, [(ghost, "no_such_person")]);
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM enrollments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_paused_flow_enrolls_nobody_and_a_missing_version_is_refused(pool: PgPool) {
        let ws = workspace(&pool, "paused").await;
        let flow = flow(&pool, ws, json!({"mode": "once"})).await;
        let ada = person(&pool, ws).await;
        crate::flows::tests_support::lifecycle(&pool, ws, flow, crate::FlowAction::Pause)
            .await
            .unwrap();

        let paused = run(&pool, ws, flow, &[ada], Uuid::now_v7(), NOW).await;
        let mut tx = crate::begin(&pool).await.unwrap();
        let missing = enroll(
            &mut tx,
            &EnrollRequest {
                workspace: ws,
                flow,
                version: 9,
                people: &[ada],
                occurrence: Occurrence {
                    trigger: "manual",
                    id: Uuid::now_v7(),
                },
                now: NOW,
                trace: &TraceContext::root(Cause::api("enroll")),
            },
        )
        .await;

        assert_eq!(paused.skipped, [(ada, "flow_paused")]);
        assert!(matches!(missing, Err(EnrollError::NoSuchVersion)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn two_concurrent_enrolls_of_one_person_enroll_them_once(pool: PgPool) {
        let ws = workspace(&pool, "race").await;
        let flow = flow(&pool, ws, json!({"mode": "once_at_a_time"})).await;
        let ada = person(&pool, ws).await;

        let people = [ada];
        let (one, two) = tokio::join!(
            run(&pool, ws, flow, &people, Uuid::now_v7(), NOW),
            run(&pool, ws, flow, &people, Uuid::now_v7(), NOW)
        );

        assert_eq!(one.enrolled.len() + two.enrolled.len(), 1);
        assert_eq!(one.skipped.len() + two.skipped.len(), 1);
    }

    /// Waits until the backend `pid` is blocked on a lock, by polling `pg_blocking_pids`.
    async fn wait_until_blocked(pool: &PgPool, pid: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1)) > 0")
                .bind(pid)
                .fetch_one(pool)
                .await
                .unwrap();
            if blocked {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the enroll never waited for the merge"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_enroll_that_resolved_a_person_merged_before_its_insert_enrolls_the_survivor(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "merge-race").await;
        let flow = flow(&pool, ws, json!({"mode": "once_at_a_time"})).await;
        let (loser, survivor) = (person(&pool, ws).await, person(&pool, ws).await);

        // The merge holds the loser while it points it at the survivor, as `merge` does.
        let mut merge = crate::begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(vec![loser, survivor])
            .execute(&mut *merge)
            .await
            .unwrap();
        sqlx::query("UPDATE people SET merged_into = $2 WHERE id = $1")
            .bind(loser)
            .bind(survivor)
            .execute(&mut *merge)
            .await
            .unwrap();

        // The enroll resolves the loser from committed rows: not merged yet.
        let mut tx = crate::begin(&pool).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let enroller = tokio::spawn(async move {
            let outcome = enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: ws,
                    flow,
                    version: 1,
                    people: &[loser],
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: NOW,
                    trace: &TraceContext::root(Cause::api("enroll")),
                },
            )
            .await
            .unwrap();
            crate::commit(tx).await.unwrap();
            outcome
        });
        wait_until_blocked(&pool, pid).await;
        crate::commit(merge).await.unwrap();
        let outcome = enroller.await.unwrap();

        let on_loser: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM enrollments WHERE person_id = $1 \
               AND state IN ('active', 'waiting', 'held', 'failed')",
        )
        .bind(loser)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(on_loser, 0, "nothing in flight on the merged-away person");
        let entries_on_loser: i64 =
            sqlx::query_scalar("SELECT count(*) FROM flow_entries WHERE person_id = $1")
                .bind(loser)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entries_on_loser, 0);
        let on_survivor: i64 =
            sqlx::query_scalar("SELECT count(*) FROM enrollments WHERE person_id = $1")
                .bind(survivor)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(on_survivor, 1, "the survivor is enrolled once");
        assert_eq!(
            outcome
                .enrolled
                .iter()
                .map(|e| e.person)
                .collect::<Vec<_>>(),
            [survivor]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_enroll_that_resolved_a_person_erased_before_its_insert_skips_them(pool: PgPool) {
        let ws = workspace(&pool, "erase-race").await;
        let flow = flow(&pool, ws, json!({"mode": "once_at_a_time"})).await;
        let (gone, stays) = (person(&pool, ws).await, person(&pool, ws).await);

        let mut erase = crate::begin(&pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(gone)
            .execute(&mut *erase)
            .await
            .unwrap();
        sqlx::query("DELETE FROM people WHERE id = $1")
            .bind(gone)
            .execute(&mut *erase)
            .await
            .unwrap();

        let mut tx = crate::begin(&pool).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let enroller = tokio::spawn(async move {
            let outcome = enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: ws,
                    flow,
                    version: 1,
                    people: &[gone, stays],
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: NOW,
                    trace: &TraceContext::root(Cause::api("enroll")),
                },
            )
            .await
            .unwrap();
            crate::commit(tx).await.unwrap();
            outcome
        });
        wait_until_blocked(&pool, pid).await;
        crate::commit(erase).await.unwrap();
        let outcome = enroller.await.unwrap();

        assert_eq!(outcome.skipped, [(gone, NO_SUCH_PERSON)]);
        assert_eq!(
            outcome
                .enrolled
                .iter()
                .map(|e| e.person)
                .collect::<Vec<_>>(),
            [stays]
        );
    }
}
