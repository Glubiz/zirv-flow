//! What ends or pauses the enrollments of a flow from outside their own steps (architecture 9.3
//! and 9.4): the goal and exit criteria, a pause that holds due steps and a resume that runs them,
//! an archive, and the operator's retry, skip and bulk exit.
//!
//! The store never runs the pure engine. The goal and exit criteria are matched here, in
//! batched queries over the events a batch inserted and the membership changes a refresh wrote,
//! and handed to the `flow.settle` job (in `zirv-flow-jobs`), which applies `engine::settle`.
//! The archive, the resume and the bulk exit are batches of [`LIFECYCLE_BATCH`] enrollments,
//! each in one transaction, run by the `flow.lifecycle` job; the operator's retry and skip are
//! one enrollment, done in the caller's transaction.
//!
//! Lock order, as everywhere in the flow runtime: the person row before the enrollment, and the
//! people of a batch in id order (`FOR NO KEY UPDATE`, the lock [`super::lock_enrollment`] takes).

use std::fmt;

use serde_json::{Value, json};
use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Cause, CauseKind, TraceContext, TraceId, WorkspaceId};

use super::arrivals::Arrival;
use super::runtime::{
    EnrollmentStatus, StepJobInput, StepJobPayload, find_enrollment, lock_enrollment,
};
use super::state::FlowState;
use crate::{NewDecision, NewJob, StoreError, append_decisions, enqueue, enqueue_many};

/// The kind of the job that matches the goal and exit criteria of a batch of enrollments.
pub const SETTLE_JOB_KIND: &str = "flow.settle";
/// The kind of the job that archives, resumes or bulk-exits the enrollments of a flow in batches.
pub const LIFECYCLE_JOB_KIND: &str = "flow.lifecycle";
/// How many enrollments one `flow.lifecycle` run changes, and one `flow.settle` job names.
pub const LIFECYCLE_BATCH: usize = 500;

/// Why a held enrollment whose stored input or cause does not parse is failed by a resume: it
/// could not be run again, and a row left held would be read by every later batch.
const UNRESUMABLE: &str =
    "the held step could not be resumed: its stored input or cause does not parse";

/// The states in which an enrollment still holds its person.
const IN_FLIGHT: &str = "('active', 'waiting', 'held', 'failed')";

/// The state of the flow a step is about to run for, read with `FOR KEY SHARE`: a resume, which
/// locks the flow `FOR UPDATE`, waits for the step to commit, so a step that holds its enrollment
/// because the flow was paused is seen by the resume that follows it. The lock is taken before the
/// person and the enrollment are, as the resume takes the flow before them.
pub async fn flow_state_for_step(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
) -> Result<Option<FlowState>, StoreError> {
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM flows WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(state.as_deref().and_then(FlowState::parse))
}

/// Holds an enrollment whose step fell due while its flow was paused: `held`, with the input to
/// run again on resume. The caller holds the enrollment's lock.
pub async fn hold_enrollment(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    enrollment: Uuid,
    input: &StepJobInput,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE enrollments SET state = 'held', resume_input = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2 AND state IN ('active', 'waiting', 'held')",
    )
    .bind(workspace.as_uuid())
    .bind(enrollment)
    .bind(input.to_stored())
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Forgets the failed step row of a node, so a retry that runs the node again records it afresh
/// (and applies its effects) instead of finding the row of the failure.
pub async fn reopen_failed_step(
    connection: &mut PgConnection,
    enrollment: Uuid,
    node: &str,
) -> Result<(), StoreError> {
    sqlx::query(
        "DELETE FROM step_executions \
         WHERE enrollment_id = $1 AND node_id = $2 AND outcome = 'failed'",
    )
    .bind(enrollment)
    .bind(node)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// What one batch of a lifecycle action did. The next batch is due whenever this one read a full
/// page, whatever it changed: a page whose rows another transaction ended first, or that could not
/// be resumed, changes nothing and still leaves later rows behind.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleBatch {
    /// The enrollments the batch looked at. A full batch means there may be more.
    pub read: usize,
    /// The enrollments it changed: one that another transaction ended first is not counted.
    pub changed: usize,
    /// The person of the last enrollment read: the keyset cursor of the next batch.
    pub last: Option<Uuid>,
}

/// Locks the people of `found` (enrollment, person) in id order, and returns the enrollment ids.
async fn lock_people(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    found: &[(Uuid, Uuid)],
) -> Result<Vec<Uuid>, StoreError> {
    let people: Vec<Uuid> = found.iter().map(|(_, person)| *person).collect();
    sqlx::query(
        "SELECT id FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id \
         FOR NO KEY UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&people)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(found.iter().map(|(id, _)| *id).collect())
}

/// A batch of enrollments to end, and why.
#[derive(Clone, Debug)]
pub struct ExitBatch<'a> {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    /// Only the enrollments pinned to this version; every version when `None`.
    pub version: Option<i32>,
    /// The `exit_reason` stored with each enrollment and its decision.
    pub reason: &'static str,
    /// The trace of the action; every enrollment's decision is recorded under it.
    pub trace: &'a TraceContext,
    /// The keyset cursor: only the enrollments of people after this one, in id order.
    pub after: Option<Uuid>,
}

/// Exits up to `limit` of a flow's in-flight enrollments (active, waiting, held and failed) in
/// the caller's transaction: the people are locked in id order, then each enrollment is set
/// `exited` with its reason, its waits are removed and its `state_version` grows, so a timer or an
/// event for the older state finds it stale. Each gets a `flow.enrollment` decision. Returns how
/// many it looked at and how many it ended; call it again while it reads a full batch.
pub async fn exit_batch(
    connection: &mut PgConnection,
    batch: &ExitBatch<'_>,
    now: OffsetDateTime,
    limit: usize,
) -> Result<LifecycleBatch, StoreError> {
    let workspace = batch.workspace;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let found: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, person_id FROM enrollments \
         WHERE workspace_id = $1 AND flow_id = $2 AND state IN {IN_FLIGHT} \
           AND ($3::int IS NULL OR flow_version = $3) \
           AND ($5::uuid IS NULL OR person_id > $5) \
         ORDER BY person_id, id LIMIT $4"
    )))
    .bind(workspace.as_uuid())
    .bind(batch.flow)
    .bind(batch.version)
    .bind(limit)
    .bind(batch.after)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if found.is_empty() {
        return Ok(LifecycleBatch::default());
    }
    let last = found.last().map(|(_, person)| *person);
    let ids = lock_people(connection, workspace, &found).await?;
    let ended: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE enrollments SET state = 'exited', exit_reason = $3, \
             state_version = state_version + 1, wake_at = NULL, error = NULL, \
             failed_node = NULL, failure_placed = false, resume_input = NULL, ended_at = $4, updated_at = now() \
         WHERE workspace_id = $1 AND id = ANY($2) AND state IN {IN_FLIGHT} \
         RETURNING id, person_id"
    )))
    .bind(workspace.as_uuid())
    .bind(&ids)
    .bind(batch.reason)
    .bind(now)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let ended_ids: Vec<Uuid> = ended.iter().map(|(id, _)| *id).collect();
    sqlx::query("DELETE FROM enrollment_waits WHERE enrollment_id = ANY($1)")
        .bind(&ended_ids)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
    let decisions: Vec<NewDecision> = ended
        .iter()
        .map(|(id, person)| NewDecision {
            workspace_id: workspace,
            trace: batch.trace.clone(),
            action: "flow.enrollment",
            outcome: "exited",
            reason: Some(batch.reason),
            subject_ids: vec![
                ("flow", batch.flow),
                ("enrollment", *id),
                ("person", *person),
            ],
            job: None,
        })
        .collect();
    append_decisions(connection, &decisions).await?;
    Ok(LifecycleBatch {
        read: found.len(),
        changed: ended.len(),
        last,
    })
}

#[derive(FromRow)]
struct HeldRow {
    id: Uuid,
    person_id: Uuid,
    state_version: i64,
    resume_input: Value,
    trace_id: Uuid,
    cause_kind: String,
    cause_id: String,
}

/// Resumes up to `limit` held enrollments of a flow in the caller's transaction: each goes back
/// to the state it was held from and gets the `flow.step` job it was held for, under its own
/// trace, with a unique key that carries `trace`. A `flow.enrollment` decision under `trace`
/// (the resume's) records each. A job for a timer that is not due yet defers itself, as any
/// other. A held enrollment whose stored input or cause does not parse cannot be resumed: it is
/// moved to `failed` with an error (and a `failed` decision) so it is never read again, never left
/// held. Only the people after `after` are read (the keyset cursor: the `last` of the previous
/// batch). Returns how many it looked at and how many it resumed; call it again while it reads a
/// full batch.
pub async fn resume_batch(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    trace: &TraceContext,
    after: Option<Uuid>,
    limit: usize,
) -> Result<LifecycleBatch, StoreError> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let found: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, person_id FROM enrollments \
         WHERE workspace_id = $1 AND flow_id = $2 AND state = 'held' \
           AND ($4::uuid IS NULL OR person_id > $4) \
         ORDER BY person_id, id LIMIT $3",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(limit)
    .bind(after)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if found.is_empty() {
        return Ok(LifecycleBatch::default());
    }
    let last = found.last().map(|(_, person)| *person);
    let ids = lock_people(connection, workspace, &found).await?;
    let held: Vec<HeldRow> = sqlx::query_as(
        "SELECT id, person_id, state_version, resume_input, trace_id, cause_kind, cause_id \
         FROM enrollments WHERE workspace_id = $1 AND id = ANY($2) AND state = 'held' \
         ORDER BY id FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(&ids)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let mut jobs = Vec::with_capacity(held.len());
    let mut resumed = Vec::with_capacity(held.len());
    let mut decisions = Vec::with_capacity(held.len());
    let mut unresumable = Vec::new();
    for row in &held {
        let input = StepJobInput::from_stored(&row.resume_input);
        let kind = row.cause_kind.parse::<CauseKind>();
        let (Some(input), Ok(kind)) = (input, kind) else {
            unresumable.push(row);
            continue;
        };
        let enrollment_trace = TraceContext {
            trace_id: TraceId::from_uuid(row.trace_id),
            cause: Cause::from_stored(kind, row.cause_id.clone()),
        };
        let payload = StepJobPayload {
            workspace,
            enrollment: row.id,
            state_version: row.state_version,
            input: input.clone(),
        };
        let key = format!(
            "resume:{}:{}:{}",
            row.id,
            row.state_version,
            trace.trace_id.as_uuid()
        );
        jobs.push(payload.job(&enrollment_trace, None, key));
        resumed.push((row.id, input.status_before().as_str()));
        decisions.push(NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "flow.enrollment",
            outcome: "resumed",
            reason: Some("flow_resumed"),
            subject_ids: vec![
                ("flow", flow),
                ("enrollment", row.id),
                ("person", row.person_id),
            ],
            job: None,
        });
    }
    let (resumed_ids, states): (Vec<Uuid>, Vec<&str>) = resumed.into_iter().unzip();
    sqlx::query(
        "UPDATE enrollments e SET state = t.state, resume_input = NULL, updated_at = now() \
         FROM unnest($1::uuid[], $2::text[]) AS t(id, state) WHERE e.id = t.id",
    )
    .bind(&resumed_ids)
    .bind(&states)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if !unresumable.is_empty() {
        let broken: Vec<Uuid> = unresumable.iter().map(|row| row.id).collect();
        sqlx::query(
            "UPDATE enrollments SET state = 'failed', error = $2, failed_node = node, \n                 failure_placed = false, resume_input = NULL, updated_at = now() \n             WHERE id = ANY($1)",
        )
        .bind(&broken)
        .bind(UNRESUMABLE)
        .execute(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        decisions.extend(unresumable.iter().map(|row| NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "flow.enrollment",
            outcome: "failed",
            reason: Some(UNRESUMABLE),
            subject_ids: vec![
                ("flow", flow),
                ("enrollment", row.id),
                ("person", row.person_id),
            ],
            job: None,
        }));
    }
    enqueue_many(&mut *connection, &jobs).await?;
    append_decisions(connection, &decisions).await?;
    Ok(LifecycleBatch {
        read: found.len(),
        changed: resumed_ids.len(),
        last,
    })
}

/// What an operator's retry or skip was refused for.
#[derive(Debug)]
pub enum ControlError {
    /// No such enrollment in the flow.
    NotFound,
    /// The enrollment is not `failed`.
    NotFailed(EnrollmentStatus),
    /// The flow is archived: it changes no more, and a step job would do nothing.
    FlowArchived,
    /// The flow is paused: a step would be held. Resume the flow first.
    FlowPaused,
    /// The engine did not place the failure at the node a skip would leave along: a step that
    /// failed on a later node of its chain started from this one, and a failure with no node
    /// attribution (the pinned version is missing, a database error on the last attempt) stores
    /// the enrollment's own node. Only a retry makes sense.
    SkipUnavailable,
    Store(StoreError),
}

impl From<StoreError> for ControlError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("no such enrollment"),
            Self::NotFailed(status) => write!(
                formatter,
                "the enrollment is {} and only a failed one can be retried or skipped",
                status.as_str()
            ),
            Self::FlowArchived => formatter.write_str("the flow is archived"),
            Self::FlowPaused => {
                formatter.write_str("the flow is paused; resume it before retrying or skipping")
            }
            Self::SkipUnavailable => formatter.write_str(
                "the failure was not placed at the node the enrollment stopped at, so there is \
                 nothing to skip; retry it instead",
            ),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ControlError {}

/// An operator's retry (`skip: false`) or skip (`skip: true`) of a failed enrollment, in the
/// caller's transaction. The flow must be published: an archived or a paused flow refuses both
/// (an operator resumes a paused flow first), and writes nothing. The enrollment is locked (person
/// first), checked, and a decision under `trace` records the action.
///
/// A retry of a step that failed in the runtime puts the enrollment back in the state the step
/// started from (it kept its node, timer, waits and `state_version`) and enqueues the input that
/// failed again. A retry of a failure the engine reported, and every skip, enqueue the engine's
/// `Resume` for the failed enrollment. The job's own decisions are on the enrollment's trace.
pub async fn resume_failed(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    enrollment: Uuid,
    skip: bool,
    trace: &TraceContext,
) -> Result<(), ControlError> {
    let Some(known) = find_enrollment(connection, workspace, enrollment).await? else {
        return Err(ControlError::NotFound);
    };
    if known.flow_id != flow {
        return Err(ControlError::NotFound);
    }
    // The flow before the person and the enrollment, as every step takes them.
    match flow_state_for_step(connection, workspace, flow).await? {
        Some(FlowState::Archived) => return Err(ControlError::FlowArchived),
        Some(FlowState::Paused) => return Err(ControlError::FlowPaused),
        Some(FlowState::Draft | FlowState::Published) | None => {}
    }
    let Some(locked) = lock_enrollment(connection, workspace, enrollment).await? else {
        return Err(ControlError::NotFound);
    };
    if locked.status != EnrollmentStatus::Failed {
        return Err(ControlError::NotFailed(locked.status));
    }
    if skip
        && !(locked.failure_placed && locked.failed_node.as_deref() == Some(locked.node.as_str()))
    {
        return Err(ControlError::SkipUnavailable);
    }
    let stored = locked
        .resume_input
        .as_ref()
        .and_then(StepJobInput::from_stored);
    let input = match (skip, stored) {
        (false, Some(input)) => {
            sqlx::query(
                "UPDATE enrollments SET state = $2, error = NULL, failed_node = NULL, \
                     failure_placed = false, resume_input = NULL, updated_at = now() WHERE id = $1",
            )
            .bind(enrollment)
            .bind(input.status_before().as_str())
            .execute(&mut *connection)
            .await
            .map_err(StoreError::query)?;
            input
        }
        _ => StepJobInput::Resume { skip },
    };
    let payload = StepJobPayload {
        workspace,
        enrollment,
        state_version: locked.state_version,
        input,
    };
    let key = format!(
        "resume:{enrollment}:{}:{}",
        locked.state_version,
        if skip { "skip" } else { "retry" }
    );
    enqueue(&mut *connection, &payload.job(&locked.trace, None, key)).await?;
    append_decisions(
        connection,
        &[NewDecision {
            workspace_id: workspace,
            trace: trace.clone(),
            action: "flow.enrollment",
            outcome: if skip { "skipped" } else { "retried" },
            reason: None,
            subject_ids: vec![
                ("flow", flow),
                ("enrollment", enrollment),
                ("person", locked.person_id),
            ],
            job: None,
        }],
    )
    .await?;
    Ok(())
}

/// What the goal or the exit criteria of an enrollment's flow noticed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Signal {
    /// An event the flow's goal or an event exit criterion names; the job applies the property
    /// filters, which the store cannot read.
    Event {
        event_id: Uuid,
        name: String,
        goal: bool,
        exit: bool,
    },
    /// The person entered the segment that is the flow's goal.
    GoalSegment,
    /// The person left the trigger segment of a flow with a `leaves_segment` criterion.
    LeftSegment,
}

/// One enrollment a `flow.settle` job looks at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettleItem {
    pub enrollment: Uuid,
    pub person: Uuid,
    pub signal: Signal,
}

/// The payload of a `flow.settle` job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlePayload {
    pub workspace: WorkspaceId,
    pub items: Vec<SettleItem>,
}

impl SettleItem {
    fn to_value(&self) -> Value {
        let mut item = json!({"enrollment": self.enrollment, "person": self.person});
        match &self.signal {
            Signal::Event {
                event_id,
                name,
                goal,
                exit,
            } => {
                item["signal"] = json!("event");
                item["event_id"] = json!(event_id);
                item["name"] = json!(name);
                item["goal"] = json!(goal);
                item["exit"] = json!(exit);
            }
            Signal::GoalSegment => item["signal"] = json!("goal_segment"),
            Signal::LeftSegment => item["signal"] = json!("left_segment"),
        }
        item
    }

    fn parse(value: &Value) -> Option<Self> {
        let text = |key: &str| value.get(key).and_then(Value::as_str);
        let flag = |key: &str| value.get(key).and_then(Value::as_bool);
        let signal = match text("signal")? {
            "event" => Signal::Event {
                event_id: Uuid::parse_str(text("event_id")?).ok()?,
                name: text("name")?.to_owned(),
                goal: flag("goal")?,
                exit: flag("exit")?,
            },
            "goal_segment" => Signal::GoalSegment,
            "left_segment" => Signal::LeftSegment,
            _ => return None,
        };
        Some(Self {
            enrollment: Uuid::parse_str(text("enrollment")?).ok()?,
            person: Uuid::parse_str(text("person")?).ok()?,
            signal,
        })
    }
}

impl SettlePayload {
    pub fn to_value(&self) -> Value {
        json!({
            "workspace_id": self.workspace,
            "items": self.items.iter().map(SettleItem::to_value).collect::<Vec<_>>(),
        })
    }

    pub fn parse(payload: &Value) -> Option<Self> {
        let items = payload
            .get("items")?
            .as_array()?
            .iter()
            .map(SettleItem::parse)
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            workspace: payload.get("workspace_id")?.as_str()?.parse().ok()?,
            items,
        })
    }

    fn job(&self, trace: &TraceContext, unique_key: String) -> NewJob {
        let mut job = NewJob::new(SETTLE_JOB_KIND, trace.clone());
        job.workspace_id = Some(self.workspace);
        job.payload = self.to_value();
        job.unique_key = Some(unique_key);
        job
    }
}

/// Enqueues the `flow.settle` jobs for the events of a batch: one query finds the in-flight
/// enrollments of the people whose flow version names the event as its goal or as an event exit
/// criterion, by name. Property filters are not applied here (the store cannot read the segment
/// AST); the job applies them to the stored event. Returns the number of jobs enqueued.
pub(crate) async fn enqueue_event_settles(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    arrivals: &[Arrival],
) -> Result<usize, StoreError> {
    if arrivals.is_empty() {
        return Ok(0);
    }
    let events: Vec<Uuid> = arrivals.iter().map(|a| a.event_id).collect();
    let people: Vec<Uuid> = arrivals.iter().map(|a| a.person_id).collect();
    let names: Vec<&str> = arrivals.iter().map(|a| a.name.as_str()).collect();
    let rows: Vec<(Uuid, Uuid, Uuid, String, bool, bool)> = sqlx::query_as(
        "SELECT e.id, e.person_id, t.event_id, t.name, \
                COALESCE(v.settings -> 'goal' ->> 'type' = 'event' \
                         AND v.settings -> 'goal' ->> 'event' = t.name, false) AS goal, \
                COALESCE(v.settings -> 'exit_criteria' @> \
                         jsonb_build_array(jsonb_build_object('type', 'event', 'event', t.name)), \
                         false) AS exit \
         FROM unnest($2::uuid[], $3::uuid[], $4::text[]) AS t(event_id, person_id, name) \
         JOIN enrollments e ON e.workspace_id = $1 AND e.person_id = t.person_id \
                           AND e.state IN ('active', 'waiting', 'held') \
         JOIN flow_versions v ON v.flow_id = e.flow_id AND v.version = e.flow_version \
         WHERE (v.settings -> 'goal' ->> 'type' = 'event' \
                AND v.settings -> 'goal' ->> 'event' = t.name) \
            OR v.settings -> 'exit_criteria' @> \
               jsonb_build_array(jsonb_build_object('type', 'event', 'event', t.name)) \
         ORDER BY e.person_id, e.id, t.event_id",
    )
    .bind(workspace.as_uuid())
    .bind(&events)
    .bind(&people)
    .bind(&names)
    .fetch_all(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let items: Vec<SettleItem> = rows
        .into_iter()
        .map(
            |(enrollment, person, event_id, name, goal, exit)| SettleItem {
                enrollment,
                person,
                signal: Signal::Event {
                    event_id,
                    name,
                    goal,
                    exit,
                },
            },
        )
        .collect();
    let mut enqueued = 0;
    for chunk in items.chunks(LIFECYCLE_BATCH) {
        let Some(first) = chunk.first() else {
            continue;
        };
        let Signal::Event { event_id, .. } = first.signal else {
            continue;
        };
        let trace = TraceContext {
            trace_id: TraceId::new(),
            cause: Cause::event(event_id),
        };
        let payload = SettlePayload {
            workspace,
            items: chunk.to_vec(),
        };
        let unique = format!("flow.settle:event:{event_id}:{}", first.enrollment);
        if enqueue(&mut *connection, &payload.job(&trace, unique)).await? {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// Whether a published or paused flow, in any version, names `segment` as its goal or as the
/// trigger segment of a `leaves_segment` exit criterion. A refresh of such a segment must capture
/// its membership changes, as it does for a segment trigger.
pub async fn segment_is_watched(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
) -> Result<bool, StoreError> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM flows f \
             JOIN flow_versions v ON v.flow_id = f.id \
             WHERE f.workspace_id = $1 AND f.state IN ('published', 'paused') \
               AND ((v.settings -> 'goal' ->> 'type' = 'segment' \
                     AND v.settings -> 'goal' ->> 'segment_id' = $2) \
                 OR (v.settings -> 'trigger' ->> 'type' = 'segment' \
                     AND v.settings -> 'trigger' ->> 'segment_id' = $2 \
                     AND v.settings -> 'exit_criteria' @> '[{\"type\": \"leaves_segment\"}]'::jsonb)))",
    )
    .bind(workspace.as_uuid())
    .bind(segment.to_string())
    .fetch_one(connection)
    .await
    .map_err(StoreError::query)
}

/// Enqueues the `flow.settle` jobs for the membership changes of a segment: the in-flight
/// enrollments of the people who entered it whose version has it as the goal, and of the people
/// who left it whose version triggers on it and ends on `leaves_segment`. A change caused by the
/// segment's own definition settles nothing (ruling 336b80fa). Returns the number of jobs
/// enqueued.
pub(crate) async fn enqueue_segment_settles(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    segment: Uuid,
    changes: &crate::segments::SegmentMembershipChanges,
) -> Result<usize, StoreError> {
    let live = |list: &[crate::segments::SegmentMembershipChange]| -> Vec<(Uuid, Uuid)> {
        list.iter()
            .filter(|change| change.trace.cause.kind != CauseKind::DefinitionChange)
            .map(|change| (change.person, change.event))
            .collect()
    };
    let entered = live(&changes.entered);
    let exited = live(&changes.exited);
    let mut items = Vec::new();
    let mut first_trace: Option<(Uuid, TraceContext)> = None;
    for (list, source, signal) in [
        (&entered, &changes.entered, Signal::GoalSegment),
        (&exited, &changes.exited, Signal::LeftSegment),
    ] {
        if list.is_empty() {
            continue;
        }
        let people: Vec<Uuid> = list.iter().map(|(person, _)| *person).collect();
        let condition = match signal {
            Signal::GoalSegment => {
                "v.settings -> 'goal' ->> 'type' = 'segment' \
                 AND v.settings -> 'goal' ->> 'segment_id' = $3"
            }
            _ => {
                "v.settings -> 'trigger' ->> 'type' = 'segment' \
                 AND v.settings -> 'trigger' ->> 'segment_id' = $3 \
                 AND v.settings -> 'exit_criteria' @> '[{\"type\": \"leaves_segment\"}]'::jsonb"
            }
        };
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT e.id, e.person_id FROM unnest($2::uuid[]) AS t(person_id) \
             JOIN enrollments e ON e.workspace_id = $1 AND e.person_id = t.person_id \
                               AND e.state IN ('active', 'waiting', 'held') \
             JOIN flow_versions v ON v.flow_id = e.flow_id AND v.version = e.flow_version \
             WHERE {condition} ORDER BY e.person_id, e.id"
        )))
        .bind(workspace.as_uuid())
        .bind(&people)
        .bind(segment.to_string())
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::query)?;
        if first_trace.is_none()
            && !rows.is_empty()
            && let Some(change) = source
                .iter()
                .find(|change| change.trace.cause.kind != CauseKind::DefinitionChange)
        {
            first_trace = Some((change.event, change.trace.clone()));
        }
        items.extend(rows.into_iter().map(|(enrollment, person)| SettleItem {
            enrollment,
            person,
            signal: signal.clone(),
        }));
    }
    let Some((event, trace)) = first_trace else {
        return Ok(0);
    };
    let mut enqueued = 0;
    for chunk in items.chunks(LIFECYCLE_BATCH) {
        let Some(first) = chunk.first() else {
            continue;
        };
        let payload = SettlePayload {
            workspace,
            items: chunk.to_vec(),
        };
        let unique = format!("flow.settle:segment:{event}:{}", first.enrollment);
        if enqueue(&mut *connection, &payload.job(&trace, unique)).await? {
            enqueued += 1;
        }
    }
    Ok(enqueued)
}

/// What a `flow.lifecycle` job does to the enrollments of a flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleAction {
    /// Exit every in-flight enrollment with reason `flow_archived`.
    Archive,
    /// Run every held enrollment again.
    Resume,
    /// Exit the in-flight enrollments pinned to this version with reason `manual`.
    ExitVersion { version: i32 },
}

impl LifecycleAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Resume => "resume",
            Self::ExitVersion { .. } => "exit_version",
        }
    }
}

/// The payload of a `flow.lifecycle` job: one batch of the action, and the number of the batch,
/// which the next batch's unique key carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LifecyclePayload {
    pub workspace: WorkspaceId,
    pub flow: Uuid,
    pub action: LifecycleAction,
    pub round: u32,
    /// The keyset cursor: the person of the last enrollment the previous batch read. Each batch
    /// reads the enrollments of the people after it, so a page that changed nothing cannot be read
    /// again.
    pub after: Option<Uuid>,
}

impl LifecyclePayload {
    pub fn to_value(&self) -> Value {
        let mut payload = json!({
            "workspace_id": self.workspace,
            "flow_id": self.flow,
            "action": self.action.as_str(),
            "round": self.round,
        });
        if let LifecycleAction::ExitVersion { version } = self.action {
            payload["version"] = json!(version);
        }
        if let Some(after) = self.after {
            payload["after"] = json!(after);
        }
        payload
    }

    pub fn parse(payload: &Value) -> Option<Self> {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let action = match text("action")? {
            "archive" => LifecycleAction::Archive,
            "resume" => LifecycleAction::Resume,
            "exit_version" => LifecycleAction::ExitVersion {
                version: i32::try_from(payload.get("version")?.as_i64()?).ok()?,
            },
            _ => return None,
        };
        Some(Self {
            workspace: text("workspace_id")?.parse().ok()?,
            flow: Uuid::parse_str(text("flow_id")?).ok()?,
            action,
            round: u32::try_from(payload.get("round")?.as_u64()?).ok()?,
            after: match payload.get("after") {
                None | Some(Value::Null) => None,
                Some(after) => Some(Uuid::parse_str(after.as_str()?).ok()?),
            },
        })
    }

    /// The job for this batch, under the trace of the action. The unique key names the action's
    /// trace and the round, so the next round does not collide with the running one and a
    /// second request is a batch of its own.
    pub fn job(&self, trace: &TraceContext) -> NewJob {
        let mut job = NewJob::new(LIFECYCLE_JOB_KIND, trace.clone());
        job.workspace_id = Some(self.workspace);
        job.payload = self.to_value();
        job.unique_key = Some(format!(
            "flow.lifecycle:{}:{}:{}:{}",
            self.action.as_str(),
            self.flow,
            trace.trace_id.as_uuid(),
            self.round
        ));
        job
    }
}

/// Enqueues the first batch of a lifecycle action in the caller's transaction.
pub async fn enqueue_lifecycle(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    flow: Uuid,
    action: LifecycleAction,
    trace: &TraceContext,
) -> Result<bool, StoreError> {
    let payload = LifecyclePayload {
        workspace,
        flow,
        action,
        round: 0,
        after: None,
    };
    enqueue(connection, &payload.job(trace)).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;
    use crate::flows::tests_support::{create, publish, trace, waiting_enrollment, workspace};
    use crate::flows::{FlowPatch, update_flow_draft};

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

    /// `count` enrollments of a flow asleep at an `order` wait, at `state_version` 3.
    async fn sleepers(pool: &PgPool, ws: WorkspaceId, flow: Uuid, count: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for _ in 0..count {
            let person = person(pool, ws).await;
            ids.push(waiting_enrollment(pool, ws, flow, person, "order").await);
        }
        ids
    }

    async fn scalar(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[test]
    fn the_payloads_round_trip() {
        let ws = WorkspaceId::from_uuid(Uuid::from_u128(1));
        let settle = SettlePayload {
            workspace: ws,
            items: vec![
                SettleItem {
                    enrollment: Uuid::from_u128(2),
                    person: Uuid::from_u128(3),
                    signal: Signal::Event {
                        event_id: Uuid::from_u128(4),
                        name: "order".into(),
                        goal: true,
                        exit: false,
                    },
                },
                SettleItem {
                    enrollment: Uuid::from_u128(5),
                    person: Uuid::from_u128(6),
                    signal: Signal::GoalSegment,
                },
                SettleItem {
                    enrollment: Uuid::from_u128(7),
                    person: Uuid::from_u128(8),
                    signal: Signal::LeftSegment,
                },
            ],
        };
        assert_eq!(SettlePayload::parse(&settle.to_value()), Some(settle));
        for action in [
            LifecycleAction::Archive,
            LifecycleAction::Resume,
            LifecycleAction::ExitVersion { version: 3 },
        ] {
            let payload = LifecyclePayload {
                workspace: ws,
                flow: Uuid::from_u128(9),
                action,
                round: 2,
                after: Some(Uuid::from_u128(10)),
            };
            assert_eq!(LifecyclePayload::parse(&payload.to_value()), Some(payload));
        }
        assert_eq!(LifecyclePayload::parse(&json!({"action": "nap"})), None);
        assert_eq!(SettlePayload::parse(&json!({"workspace_id": ws})), None);
    }

    #[test]
    fn a_round_of_a_lifecycle_action_has_a_key_of_its_own() {
        let trace = trace();
        let first = LifecyclePayload {
            workspace: WorkspaceId::from_uuid(Uuid::from_u128(1)),
            flow: Uuid::from_u128(2),
            action: LifecycleAction::Archive,
            round: 0,
            after: None,
        };
        let next = LifecyclePayload { round: 1, ..first };
        assert_ne!(first.job(&trace).unique_key, next.job(&trace).unique_key);
        assert_eq!(first.job(&trace).kind, LIFECYCLE_JOB_KIND);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_exit_works_in_batches_and_leaves_ended_enrollments_alone(pool: PgPool) {
        let ws = workspace(&pool, "exits").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let ids = sleepers(&pool, ws, flow.id, 5).await;
        sqlx::query("UPDATE enrollments SET state = 'completed', ended_at = now() WHERE id = $1")
            .bind(ids[4])
            .execute(&pool)
            .await
            .unwrap();
        let action = trace();
        let batch = ExitBatch {
            workspace: ws,
            flow: flow.id,
            version: None,
            reason: "flow_archived",
            trace: &action,
            after: None,
        };
        let mut connection = pool.acquire().await.unwrap();
        let now = OffsetDateTime::now_utc();

        let first = exit_batch(&mut connection, &batch, now, 3).await.unwrap();
        let second = exit_batch(&mut connection, &batch, now, 3).await.unwrap();
        let third = exit_batch(&mut connection, &batch, now, 3).await.unwrap();

        assert_eq!((first.read, first.changed), (3, 3));
        assert_eq!(
            (second.read, second.changed),
            (1, 1),
            "a short batch is the last"
        );
        assert_eq!(third, LifecycleBatch::default());
        for id in &ids[..4] {
            let (state, reason, version, ended): (String, Option<String>, i64, bool) =
                sqlx::query_as(
                    "SELECT state, exit_reason, state_version, ended_at IS NOT NULL \
                     FROM enrollments WHERE id = $1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(
                (state.as_str(), reason.as_deref(), version, ended),
                ("exited", Some("flow_archived"), 4, true)
            );
            assert_eq!(
                scalar(
                    &pool,
                    "SELECT count(*) FROM enrollment_waits WHERE enrollment_id = $1",
                    *id
                )
                .await,
                0
            );
        }
        assert_eq!(
            scalar(
                &pool,
                "SELECT count(*) FROM enrollments WHERE id = $1 AND state = 'completed'",
                ids[4]
            )
            .await,
            1
        );
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE trace_id = $1 \
             AND action = 'flow.enrollment' AND outcome = 'exited' AND reason = 'flow_archived'",
        )
        .bind(action.trace_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 4, "one decision per enrollment");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_exit_by_version_takes_only_that_version(pool: PgPool) {
        let ws = workspace(&pool, "versions").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                graph: Some(json!({"v": 1, "nodes": [{"id": "later", "type": "exit"}],
                                   "edges": []})),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish(&pool, ws, flow.id).await.unwrap();
        let ids = sleepers(&pool, ws, flow.id, 2).await;
        sqlx::query("UPDATE enrollments SET flow_version = 2 WHERE id = $1")
            .bind(ids[1])
            .execute(&pool)
            .await
            .unwrap();
        let action = trace();
        let mut connection = pool.acquire().await.unwrap();

        let done = exit_batch(
            &mut connection,
            &ExitBatch {
                workspace: ws,
                flow: flow.id,
                version: Some(1),
                reason: "manual",
                trace: &action,
                after: None,
            },
            OffsetDateTime::now_utc(),
            10,
        )
        .await
        .unwrap();

        assert_eq!((done.read, done.changed), (1, 1));
        let kept: String = sqlx::query_scalar("SELECT state FROM enrollments WHERE id = $1")
            .bind(ids[1])
            .fetch_one(&pool)
            .await
            .unwrap();
        let ended: String = sqlx::query_scalar("SELECT state FROM enrollments WHERE id = $1")
            .bind(ids[0])
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((ended.as_str(), kept.as_str()), ("exited", "waiting"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_reads_only_the_people_after_its_cursor_whatever_the_earlier_pages_changed(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "cursor").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let ids = sleepers(&pool, ws, flow.id, 5).await;
        let mut connection = pool.acquire().await.unwrap();
        for id in &ids {
            hold_enrollment(&mut connection, ws, *id, &StepJobInput::Wake)
                .await
                .unwrap();
        }
        // Held rows that cannot be resumed fill the first page: the cursor moves past them.
        sqlx::query(
            "UPDATE enrollments SET resume_input = '{\"input\": \"bogus\"}'::jsonb \
             WHERE id IN (SELECT id FROM enrollments WHERE flow_id = $1 \
                          ORDER BY person_id, id LIMIT 2)",
        )
        .bind(flow.id)
        .execute(&pool)
        .await
        .unwrap();
        let action = trace();

        let first = resume_batch(&mut connection, ws, flow.id, &action, None, 2)
            .await
            .unwrap();
        let second = resume_batch(&mut connection, ws, flow.id, &action, first.last, 2)
            .await
            .unwrap();
        let third = resume_batch(&mut connection, ws, flow.id, &action, second.last, 2)
            .await
            .unwrap();

        assert_eq!((first.read, first.changed), (2, 0));
        assert_eq!((second.read, second.changed), (2, 2));
        assert_eq!((third.read, third.changed), (1, 1));
        assert!(first.last < second.last && second.last < third.last);
        let states: Vec<(String, bool)> = sqlx::query_as(
            "SELECT state, error IS NOT NULL FROM enrollments WHERE flow_id = $1 \
             ORDER BY person_id, id",
        )
        .bind(flow.id)
        .fetch_all(&pool)
        .await
        .unwrap();
        let states: Vec<(&str, bool)> = states.iter().map(|(s, e)| (s.as_str(), *e)).collect();
        assert_eq!(
            states,
            [
                ("failed", true),
                ("failed", true),
                ("waiting", false),
                ("waiting", false),
                ("waiting", false)
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_resume_runs_held_enrollments_in_batches_with_a_job_and_a_decision_each(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "resume").await;
        let flow = create(&pool, ws, "Flow").await;
        publish(&pool, ws, flow.id).await.unwrap();
        let ids = sleepers(&pool, ws, flow.id, 3).await;
        let mut connection = pool.acquire().await.unwrap();
        for id in &ids[..2] {
            hold_enrollment(&mut connection, ws, *id, &StepJobInput::Wake)
                .await
                .unwrap();
        }
        let action = trace();

        let first = resume_batch(&mut connection, ws, flow.id, &action, None, 1)
            .await
            .unwrap();
        let second = resume_batch(&mut connection, ws, flow.id, &action, None, 1)
            .await
            .unwrap();
        let third = resume_batch(&mut connection, ws, flow.id, &action, None, 1)
            .await
            .unwrap();

        assert_eq!((first.read, first.changed), (1, 1));
        assert_eq!((second.read, second.changed), (1, 1));
        assert_eq!(third, LifecycleBatch::default());
        let states: Vec<(String, bool)> = sqlx::query_as(
            "SELECT state, resume_input IS NULL FROM enrollments WHERE id = ANY($1) ORDER BY id",
        )
        .bind(&ids)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(
            states
                .iter()
                .all(|(state, clear)| state == "waiting" && *clear)
        );
        let jobs: Vec<(Value, Option<String>)> =
            sqlx::query_as("SELECT payload, unique_key FROM jobs WHERE kind = 'flow.step'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(jobs.len(), 2);
        for (payload, key) in jobs {
            let parsed = StepJobPayload::parse(&payload).unwrap();
            assert_eq!(
                (parsed.input, parsed.state_version),
                (StepJobInput::Wake, 3)
            );
            assert!(
                key.unwrap()
                    .ends_with(&action.trace_id.as_uuid().to_string())
            );
        }
        let decisions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM decision_records WHERE trace_id = $1 AND outcome = 'resumed'",
        )
        .bind(action.trace_id.as_uuid())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(decisions, 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_is_watched_while_a_live_flow_names_it_as_goal_or_leave_trigger(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "watched").await;
        let (goal, leaves, other) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        let mut connection = pool.acquire().await.unwrap();
        for segment in [goal, leaves, other] {
            assert!(
                !segment_is_watched(&mut connection, ws, segment)
                    .await
                    .unwrap()
            );
        }
        let flow = create(&pool, ws, "Goal").await;
        update_flow_draft(
            &mut connection,
            ws,
            flow.id,
            &FlowPatch {
                settings: Some(json!({
                    "trigger": {"type": "segment", "segment_id": leaves, "on": "entered"},
                    "reentry": {"mode": "once"},
                    "goal": {"type": "segment", "segment_id": goal},
                    "exit_criteria": [{"type": "leaves_segment"}]
                })),
                ..FlowPatch::default()
            },
        )
        .await
        .unwrap();
        drop(connection);
        publish(&pool, ws, flow.id).await.unwrap();
        let mut connection = pool.acquire().await.unwrap();

        assert!(segment_is_watched(&mut connection, ws, goal).await.unwrap());
        assert!(
            segment_is_watched(&mut connection, ws, leaves)
                .await
                .unwrap()
        );
        assert!(
            !segment_is_watched(&mut connection, ws, other)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE flows SET state = 'archived', archived_at = now() WHERE id = $1")
            .bind(flow.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(!segment_is_watched(&mut connection, ws, goal).await.unwrap());
    }
}
