//! `flow.step`: advances one enrollment (architecture 9.2 and 9.3).
//!
//! The payload names an enrollment, the `state_version` the job expects it to be at, and what
//! happened: it was enrolled (`start`), its timer is due (`wake`), or an event arrived that its
//! wait names (`event`). The job locks the enrollment row, so two jobs of one enrollment run one
//! after the other, and a job whose version is no longer the enrollment's is a no-op: the first
//! commit wins, which is how an awaited event and its timeout race to exactly one path.
//!
//! The pure engine walks the nodes. At a branch it yields; the conditions are evaluated for this
//! one person with the compiled SQL in the same transaction, after the internal effects of the
//! nodes before the branch were applied, so the branch sees them. Everything a step does (the
//! step rows, the effects, the decisions, the wait rows, the timer job and the new state)
//! commits with the job or not at all. A failed attempt rolls back to a savepoint and is retried
//! with backoff; on the last attempt the enrollment becomes `failed` with the error stored.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use hashbrown::HashMap;
use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::decision_actions::{self, DecisionSpec};
use zirv_flow_core::flow::{FlowGraph, MAX_CONDITIONS, NodeKind};
use zirv_flow_core::{Freshness, WorkspaceId};
use zirv_flow_engine::{
    ArrivedEvent, Effect, EffectKind, EnrollmentState, ExitReason, Failure, Outcome, Status,
    StepContext, StepInput, Transition, WaitChange, step as engine_step,
};
use zirv_flow_segments::{
    Catalog, Outcome as Verdict, PersonSnapshot, SnapshotEvent, TraitStorage, compile_condition,
    evaluate, parse_and_validate,
};
use zirv_flow_store::{
    EffectError, Enrollment, EnrollmentStatus, EnrollmentUpdate, FlowState, FlowTraitWrite,
    GraphPoint, NewDecision, NewJob, NewStepExecution, NewWait, SEND_JOB_KIND, STEP_JOB_KIND,
    SYNC_JOB_KIND, StepJobInput, StepJobPayload, StepOutcome, StepRecord, StoredEvent,
    WEBHOOK_JOB_KIND, append_decisions, apply_flow_list, apply_flow_trait, clear_waits, enqueue,
    evaluate_conditions, find_enrollment, find_event_of_person, find_flow_version,
    flow_state_for_step, hold_enrollment, lock_enrollment, record_steps, register_wait,
    release_enrollment, reopen_failed_step, save_enrollment, segment_catalog, settle_wait_step,
    step_zones, unhealthy_inputs, wait_filters,
};

use super::delivery_send::Queued;
use super::segment_refresh::{catalog_of, sql_query};
use crate::metrics::record_wake_lateness;
use crate::{JobContext, JobError, JobKind};

pub(crate) struct FlowStep;

/// The ports a branch takes, by the index of the condition that matched.
const CASES: [&str; MAX_CONDITIONS] = [
    "case_0", "case_1", "case_2", "case_3", "case_4", "case_5", "case_6", "case_7", "case_8",
    "case_9",
];

impl JobKind for FlowStep {
    const KIND: &'static str = STEP_JOB_KIND;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = StepJobPayload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a flow step"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let at = (ctx.job_id(), ctx.attempts());
        let job_id = ctx.job_id();
        let last_attempt = ctx.attempts() >= ctx.max_attempts();
        let conn = ctx.conn();
        let now = clock.now(conn).await?;
        let Some(known) = find_enrollment(conn, job.workspace, job.enrollment).await? else {
            // Erased with the person: nothing is left to advance.
            return Ok(());
        };
        // The flow's state is read first, and held until this transaction ends, so a resume
        // either sees the enrollment this step holds or the step sees the flow resumed. The
        // flow, then the person, then the enrollment: the order a resume locks them in.
        let flow_state = flow_state_for_step(conn, job.workspace, known.flow_id).await?;
        let Some(mut enrollment) = lock_enrollment(conn, job.workspace, job.enrollment).await?
        else {
            return Ok(());
        };
        if is_stale(&enrollment, &job) {
            return Ok(());
        }
        match (flow_state, &job.input) {
            // An archived flow changes no more; its archive job exits what is in flight.
            (Some(FlowState::Archived) | None, _) => return Ok(()),
            // An operator's retry or skip is deliberate, whatever the flow is doing.
            (_, StepJobInput::Resume { .. }) => {}
            (Some(FlowState::Paused), _) => {
                return hold(conn, &job, &enrollment, at).await;
            }
            // A held enrollment of a flow that is no longer paused, which its resume batch has not
            // reached yet, takes this job at once: an event must not lose to the stored timer.
            _ if enrollment.status == EnrollmentStatus::Held => {
                match release(conn, &job, enrollment).await? {
                    Some(released) => enrollment = released,
                    None => return Ok(()),
                }
            }
            _ => {}
        }
        if let (StepJobInput::Wake, Some(due)) = (&job.input, enrollment.wake_at)
            && due > now
        {
            // This clock is behind the timer. Retrying would burn attempts and could lose the timer
            // on the last one, so the job ends cleanly and a fresh wake takes its place. The running
            // job holds the wake key, so the new key carries this job's id; it runs no sooner than
            // the timer and no sooner than a few seconds from now.
            let key = format!(
                "wake:{}:{}:early:{}",
                job.enrollment, job.state_version, job_id
            );
            let run_at = due.max(now + time::Duration::seconds(5));
            enqueue(conn, &job.job(&enrollment.trace, Some(run_at), key)).await?;
            return Ok(());
        }

        if let (StepJobInput::Wake, Some(due)) = (&job.input, enrollment.wake_at)
            && at.1 == 1
        {
            record_wake_lateness((now - due).as_seconds_f64());
        }

        let mut savepoint = conn.begin().await?;
        let walked = advance(&mut savepoint, &job, &enrollment, now, at).await;
        match walked {
            Ok(()) => {
                savepoint.commit().await?;
                Ok(())
            }
            Err(failure) => {
                savepoint.rollback().await?;
                if last_attempt {
                    fail_enrollment(conn, &job, &enrollment, &failure, now, at).await
                } else {
                    Err(failure.error)
                }
            }
        }
    }
}

/// A job whose version is not the enrollment's, or that asks for what the enrollment is no
/// longer doing, is stale: the first commit won.
fn is_stale(enrollment: &Enrollment, job: &StepJobPayload) -> bool {
    let wanted = job.input.status_before();
    let held = enrollment.status == EnrollmentStatus::Held
        && !matches!(job.input, StepJobInput::Resume { .. });
    enrollment.state_version != job.state_version || (enrollment.status != wanted && !held)
}

/// A job found its enrollment held on a flow that is not paused: the flow was resumed and its
/// batch has not reached the enrollment. The enrollment goes back from `held` to the state it was
/// held from and the job runs its own input (the caller checked the `state_version`); the batch
/// then finds it no longer held. The release records the `resumed` decision the batch would have.
/// `None` leaves it held for the batch: the job is not for the state the enrollment was held
/// from, or it is a timer and an event is stored (a timer never replaces an event).
async fn release(
    conn: &mut PgConnection,
    job: &StepJobPayload,
    enrollment: Enrollment,
) -> Result<Option<Enrollment>, JobError> {
    let stored = enrollment
        .resume_input
        .as_ref()
        .and_then(StepJobInput::from_stored);
    let Some(stored) = stored else {
        return Ok(None);
    };
    let before = stored.status_before();
    let timer_over_event = matches!(stored, StepJobInput::Event { .. })
        && !matches!(job.input, StepJobInput::Event { .. });
    if before != job.input.status_before() || timer_over_event {
        return Ok(None);
    }
    release_enrollment(conn, job.workspace, enrollment.id, before).await?;
    // The decision the resume batch would have written, under the enrollment's own trace (the
    // batch's is not known here) and in this transaction, so the trace reads held, resumed, steps.
    let decision = NewDecision {
        graph: Some(GraphPoint::at(enrollment.flow_version, &enrollment.node)),
        workspace_id: job.workspace,
        trace: enrollment.trace.clone(),
        spec: &decision_actions::FLOW_ENROLLMENT_RESUMED,
        detail: None,
        subject_ids: vec![
            ("flow", enrollment.flow_id),
            ("enrollment", enrollment.id),
            ("person", enrollment.person_id),
        ],
        job: None,
    };
    append_decisions(conn, &[decision]).await?;
    Ok(Some(Enrollment {
        status: before,
        resume_input: None,
        ..enrollment
    }))
}

/// The flow is paused: the step that fell due is held, not run. The enrollment keeps its node,
/// timer, waits and version, and the input it is held for; the flow's resume runs it. An event
/// replaces a held timer (it would have won the race), and a timer never replaces an event.
async fn hold(
    conn: &mut PgConnection,
    job: &StepJobPayload,
    enrollment: &Enrollment,
    at: (Uuid, i32),
) -> Result<(), JobError> {
    let already = enrollment.status == EnrollmentStatus::Held;
    let stored = enrollment
        .resume_input
        .as_ref()
        .and_then(StepJobInput::from_stored);
    let replaces = matches!(job.input, StepJobInput::Event { .. })
        && !matches!(stored, Some(StepJobInput::Event { .. }));
    if already && !replaces {
        return Ok(());
    }
    hold_enrollment(conn, job.workspace, enrollment.id, &job.input).await?;
    if !already {
        let decision = NewDecision {
            graph: Some(GraphPoint::at(enrollment.flow_version, &enrollment.node)),
            workspace_id: job.workspace,
            trace: enrollment.trace.clone(),
            spec: &decision_actions::FLOW_ENROLLMENT_HELD,
            detail: None,
            subject_ids: vec![
                ("flow", enrollment.flow_id),
                ("enrollment", enrollment.id),
                ("person", enrollment.person_id),
            ],
            job: Some(at),
        };
        append_decisions(conn, &[decision]).await?;
    }
    Ok(())
}

/// Why a step failed, and the node it failed at when that is known.
struct StepFailure {
    node: Option<String>,
    error: JobError,
}

impl StepFailure {
    fn at(node: &str, error: impl Into<JobError>) -> Self {
        Self {
            node: Some(node.to_owned()),
            error: error.into(),
        }
    }
}

impl<E: Into<JobError>> From<E> for StepFailure {
    fn from(error: E) -> Self {
        Self {
            node: None,
            error: error.into(),
        }
    }
}

/// The inputs of the engine that are not the state: the time and the zones.
struct Zones {
    now: DateTime<Utc>,
    workspace: Tz,
    person: Option<String>,
}

impl StepContext for Zones {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }

    fn workspace_timezone(&self) -> Tz {
        self.workspace
    }

    fn person_timezone(&self) -> Option<String> {
        self.person.clone()
    }
}

fn to_chrono(at: OffsetDateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(at.unix_timestamp(), at.nanosecond())
}

fn from_chrono(at: DateTime<Utc>) -> Result<OffsetDateTime, JobError> {
    OffsetDateTime::from_unix_timestamp(at.timestamp())
        .and_then(|seconds| seconds.replace_nanosecond(at.timestamp_subsec_nanos()))
        .map_err(|_| JobError::new("a time does not fit the clock"))
}

/// What one `flow.step` knows while it walks.
struct Walk<'a> {
    workspace: WorkspaceId,
    enrollment: &'a Enrollment,
    graph: FlowGraph,
    zones: Zones,
    now: OffsetDateTime,
    /// The job and its attempt, for the decisions it makes.
    job: (Uuid, i32),
    /// What the loaded catalog and the input freshness cost to read are read at most once.
    catalog: Option<(Catalog, TraitStorage)>,
    freshness: Option<&'static str>,
    /// The step execution of each node this job has run.
    steps: HashMap<String, Uuid>,
}

impl Walk<'_> {
    fn decision(
        &self,
        spec: &'static DecisionSpec,
        detail: Option<&'static str>,
        step: Option<Uuid>,
        node: Option<&str>,
    ) -> NewDecision {
        let enrollment = self.enrollment;
        let mut subject_ids = vec![
            ("flow", enrollment.flow_id),
            ("enrollment", enrollment.id),
            ("person", enrollment.person_id),
        ];
        subject_ids.extend(step.map(|id| ("step_execution", id)));
        NewDecision {
            graph: Some(GraphPoint {
                version: enrollment.flow_version,
                node: node.map(str::to_owned),
            }),
            workspace_id: self.workspace,
            trace: enrollment.trace.clone(),
            spec,
            detail,
            subject_ids,
            job: Some(self.job),
        }
    }

    fn step_of(&self, node: &str) -> Option<Uuid> {
        self.steps.get(node).copied()
    }

    async fn catalog(
        &mut self,
        conn: &mut PgConnection,
    ) -> Result<&(Catalog, TraitStorage), JobError> {
        if self.catalog.is_none() {
            let snapshot = segment_catalog(conn, self.workspace).await?;
            self.catalog = Some(catalog_of(snapshot));
        }
        self.catalog
            .as_ref()
            .ok_or_else(|| JobError::new("the catalog could not be read"))
    }
}

/// Walks the enrollment from the job's input to the next wait, exit or failure.
async fn advance(
    conn: &mut PgConnection,
    job: &StepJobPayload,
    enrollment: &Enrollment,
    now: OffsetDateTime,
    at: (Uuid, i32),
) -> Result<(), StepFailure> {
    let version = find_flow_version(
        &mut *conn,
        job.workspace,
        enrollment.flow_id,
        enrollment.flow_version,
    )
    .await?
    .ok_or_else(|| JobError::new("the pinned flow version is missing"))?;
    let graph = FlowGraph::from_value(version.graph)
        .map_err(|_| JobError::new("the pinned flow version does not parse"))?;
    let (workspace_zone, person_zone) =
        step_zones(&mut *conn, job.workspace, enrollment.person_id).await?;
    let zones = Zones {
        now: to_chrono(now).ok_or_else(|| JobError::new("the time does not fit the engine"))?,
        workspace: workspace_zone.parse().unwrap_or(Tz::UTC),
        person: person_zone,
    };
    let mut walk = Walk {
        workspace: job.workspace,
        enrollment,
        graph,
        zones,
        now,
        job: at,
        catalog: None,
        freshness: None,
        steps: HashMap::new(),
    };

    let Some(mut input) = first_input(conn, &mut walk, job).await? else {
        return Ok(());
    };
    let mut state = engine_state(enrollment)?;
    let mut first = true;
    loop {
        let transition = engine_step(&walk.graph, &state, input, &walk.zones)
            .map_err(|_| JobError::new("the flow engine refused the step"))?;
        if transition.outcome == Outcome::Stale {
            return Ok(());
        }
        if first {
            wake(conn, &mut walk, job).await?;
            first = false;
        }
        apply(conn, &mut walk, &transition).await?;
        match &transition.outcome {
            Outcome::NeedsConditions { node, conditions } => {
                let results = branch(conn, &mut walk, node, conditions).await?;
                input = StepInput::ConditionsEvaluated {
                    node: node.clone(),
                    results,
                };
                state = transition.state;
            }
            _ => return finish(conn, &mut walk, &transition).await,
        }
    }
}

/// The engine's input for the job; `None` when an event does not match its wait's filters, so
/// the enrollment stays asleep.
async fn first_input(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    job: &StepJobPayload,
) -> Result<Option<StepInput>, StepFailure> {
    let version = u64::try_from(walk.enrollment.state_version)
        .map_err(|_| JobError::new("the state version is negative"))?;
    Ok(Some(match &job.input {
        StepJobInput::Start => StepInput::Start,
        StepJobInput::Wake => StepInput::TimerDue {
            wake_version: version,
        },
        StepJobInput::Resume { skip } => StepInput::Resume { skip: *skip },
        StepJobInput::Event { event_id, name } => {
            if walk.enrollment.awaiting.as_deref() != Some(name.as_str()) {
                return Ok(None);
            }
            let Some(filters) = wait_filters(&mut *conn, walk.enrollment.id, name).await? else {
                return Ok(None);
            };
            let person = walk.enrollment.person_id;
            let Some(event) =
                find_event_of_person(&mut *conn, walk.workspace, person, *event_id).await?
            else {
                return Ok(None);
            };
            if !event_matches(conn, walk, name, &filters, &event).await? {
                return Ok(None);
            }
            StepInput::EventArrived {
                event: ArrivedEvent {
                    name: name.clone(),
                    wake_version: version,
                },
            }
        }
    }))
}

/// Whether a stored event satisfies the property filters of a wait, by the reference evaluator
/// over a snapshot that holds just that event.
async fn event_matches(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    name: &str,
    filters: &[Value],
    event: &StoredEvent,
) -> Result<bool, JobError> {
    if filters.is_empty() {
        return Ok(true);
    }
    let now = walk.now;
    let (catalog, _) = walk.catalog(conn).await?;
    Ok(filters_pass(catalog, name, filters, event, now))
}

/// Whether `event` satisfies the property filters named `name` carries, over a snapshot that
/// holds just that event. Something the filters read that was archived since the publish cannot
/// match. The goal and exit criteria of a flow (`flow.settle`) are matched the same way.
pub(super) fn filters_pass(
    catalog: &Catalog,
    name: &str,
    filters: &[Value],
    event: &StoredEvent,
    now: OffsetDateTime,
) -> bool {
    if filters.is_empty() {
        return true;
    }
    let wire = json!({"v": 1, "event": name, "where": filters});
    let Ok(definition) = parse_and_validate(&wire, catalog) else {
        return false;
    };
    let snapshot = PersonSnapshot {
        events: vec![SnapshotEvent {
            name: event.name.clone(),
            occurred_at: event.occurred_at,
            properties: event.properties.as_object().cloned().unwrap_or_default(),
        }],
        ..PersonSnapshot::default()
    };
    evaluate(&definition, catalog, &snapshot, now).outcome == Verdict::Pass
}

/// The engine's view of a stored enrollment.
fn engine_state(enrollment: &Enrollment) -> Result<EnrollmentState, JobError> {
    let status = match enrollment.status {
        EnrollmentStatus::Active => Status::Active,
        EnrollmentStatus::Waiting => Status::Waiting,
        EnrollmentStatus::Failed => Status::Failed,
        EnrollmentStatus::Held => return Err(JobError::new("the enrollment is held")),
        EnrollmentStatus::Completed | EnrollmentStatus::Exited => {
            return Err(JobError::new("the enrollment has ended"));
        }
    };
    Ok(EnrollmentState {
        enrollment_id: enrollment.id,
        node: enrollment.node.clone(),
        state_version: u64::try_from(enrollment.state_version)
            .map_err(|_| JobError::new("the state version is negative"))?,
        status,
        wake_at: enrollment.wake_at.and_then(to_chrono),
        awaiting: enrollment.awaiting.clone(),
    })
}

/// A timer or an event ended the wait the enrollment slept at.
async fn wake(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    job: &StepJobPayload,
) -> Result<(), StepFailure> {
    let outcome = match (&job.input, node_of(walk, &walk.enrollment.node)) {
        (StepJobInput::Start, _) => return Ok(()),
        (StepJobInput::Resume { skip }, _) => {
            // A retry runs the failed node again, so its row of the failure must not hold it.
            if let (false, Some(node)) = (skip, &walk.enrollment.failed_node) {
                reopen_failed_step(&mut *conn, walk.enrollment.id, node).await?;
            }
            return Ok(());
        }
        (StepJobInput::Event { .. }, _) => &decision_actions::FLOW_WAKE_EVENT,
        (StepJobInput::Wake, Some(NodeKind::WaitUntilEvent { .. })) => {
            &decision_actions::FLOW_WAKE_TIMEOUT
        }
        (StepJobInput::Wake, _) => &decision_actions::FLOW_WAKE_TIMER,
    };
    let node = walk.enrollment.node.clone();
    let step = settle_wait_step(&mut *conn, walk.enrollment.id, &node).await?;
    let decision = walk.decision(outcome, None, step, Some(&node));
    append_decisions(conn, &[decision]).await?;
    Ok(())
}

fn node_of<'g>(walk: &'g Walk<'_>, id: &str) -> Option<&'g NodeKind> {
    walk.graph.node(id).map(|node| &node.kind)
}

/// Commits the nodes a transition visited: their step rows, their effects (only the ones whose
/// row this transaction wrote), their decisions, the wait rows and the timer.
async fn apply(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    transition: &Transition,
) -> Result<(), StepFailure> {
    let last = transition.visited.len().saturating_sub(1);
    let failure_text = match &transition.outcome {
        Outcome::Failed { failure, .. } => Some(describe(*failure)),
        _ => None,
    };
    let steps: Vec<NewStepExecution<'_>> = transition
        .visited
        .iter()
        .enumerate()
        .map(|(index, visit)| {
            let ends_here = index == last;
            let (outcome, error) = match &transition.outcome {
                Outcome::Waiting if ends_here => (StepOutcome::Waiting, None),
                Outcome::Failed { .. } if ends_here => (StepOutcome::Failed, failure_text),
                _ => (StepOutcome::Done, None),
            };
            NewStepExecution {
                node_id: &visit.node,
                outcome,
                error,
                attempt: walk.job.1,
            }
        })
        .collect();
    let records = record_steps(&mut *conn, walk.workspace, walk.enrollment.id, &steps).await?;
    let fresh: HashMap<&str, StepRecord> = transition
        .visited
        .iter()
        .zip(&records)
        .map(|(visit, record)| (visit.node.as_str(), *record))
        .collect();
    for (visit, record) in transition.visited.iter().zip(&records) {
        walk.steps.insert(visit.node.clone(), record.id);
    }

    let mut enqueued: HashMap<String, &'static str> = HashMap::new();
    let mut decisions = Vec::new();
    for effect in &transition.effects {
        if !fresh.get(effect.node.as_str()).is_some_and(|r| r.fresh) {
            continue;
        }
        let step = walk.step_of(&effect.node);
        if let Some(label) = apply_effect(conn, walk, effect, step, &mut decisions)
            .await
            .map_err(|error| StepFailure::at(&effect.node, error))?
        {
            enqueued.insert(effect.node.clone(), label);
        }
    }
    for (visit, record) in transition.visited.iter().zip(&records) {
        if !record.fresh {
            continue;
        }
        let kind = node_of(walk, &visit.node).map_or("node", NodeKind::type_name);
        let ends_here = visit.node == transition.state.node;
        let (spec, reason) = match (enqueued.get(&visit.node), &transition.outcome) {
            (Some(label), _) => (&decision_actions::FLOW_STEP_ENQUEUED, Some(*label)),
            (None, Outcome::Failed { failure, .. }) if ends_here => (
                &decision_actions::FLOW_STEP_FAILED,
                Some(failure_code(*failure)),
            ),
            (None, Outcome::Waiting) if ends_here => (
                &decision_actions::FLOW_STEP_WAITING,
                Some(if transition.zone_fallback.is_some() {
                    "timezone_fallback"
                } else {
                    kind
                }),
            ),
            (None, _) => (&decision_actions::FLOW_STEP_DONE, Some(kind)),
        };
        decisions.push(walk.decision(spec, reason, Some(record.id), Some(&visit.node)));
    }
    append_decisions(&mut *conn, &decisions).await?;

    for change in &transition.waits {
        match change {
            WaitChange::Register { event, timeout_at } => {
                let wake_version = i64::try_from(transition.state.state_version)
                    .map_err(|_| JobError::new("the state version does not fit"))?;
                register_wait(
                    &mut *conn,
                    walk.workspace,
                    walk.enrollment.id,
                    &NewWait {
                        person: walk.enrollment.person_id,
                        event_name: &event.name,
                        filters: &event.filters,
                        wake_version,
                        timeout_at: from_chrono(*timeout_at)?,
                    },
                )
                .await?;
            }
            WaitChange::Clear { event_name } => {
                clear_waits(&mut *conn, walk.enrollment.id, Some(event_name)).await?;
            }
        }
    }
    if let Some(timer) = &transition.wake {
        let payload = StepJobPayload {
            workspace: walk.workspace,
            enrollment: walk.enrollment.id,
            state_version: i64::try_from(transition.state.state_version)
                .map_err(|_| JobError::new("the state version does not fit"))?,
            input: StepJobInput::Wake,
        };
        let job = payload.job(
            &walk.enrollment.trace,
            Some(from_chrono(timer.at)?),
            timer.key.clone(),
        );
        enqueue(&mut *conn, &job).await?;
    }
    Ok(())
}

/// Applies one effect of a step whose row this transaction wrote. An external effect becomes a
/// job under its stable key and returns the freshness label its step decision records.
async fn apply_effect(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    effect: &Effect,
    step: Option<Uuid>,
    decisions: &mut Vec<NewDecision>,
) -> Result<Option<&'static str>, JobError> {
    let enrollment = walk.enrollment;
    match &effect.kind {
        EffectKind::UpdateTrait { key, change } => {
            apply_flow_trait(
                conn,
                &FlowTraitWrite {
                    workspace: walk.workspace,
                    flow: enrollment.flow_id,
                    person: enrollment.person_id,
                    key,
                    change,
                    now: walk.now,
                    trace: &enrollment.trace,
                },
            )
            .await
            .map_err(effect_error)?;
            Ok(None)
        }
        EffectKind::List { list_id, change } => {
            apply_flow_list(
                conn,
                walk.workspace,
                *list_id,
                enrollment.person_id,
                *change,
                &enrollment.trace,
            )
            .await
            .map_err(effect_error)?;
            Ok(None)
        }
        EffectKind::GoalConversion => {
            decisions.push(walk.decision(
                &decision_actions::FLOW_GOAL_CONVERTED,
                None,
                step,
                Some(&effect.node),
            ));
            Ok(None)
        }
        EffectKind::Send {
            template,
            destination,
            topic,
            dimensions,
        } => {
            let label = input_freshness(conn, walk).await?;
            let key = effect
                .effect_key
                .clone()
                .ok_or_else(|| JobError::new("an external effect has no key"))?;
            let mut job = NewJob::new(SEND_JOB_KIND, enrollment.trace.clone());
            job.workspace_id = Some(walk.workspace);
            job.unique_key = Some(key.clone());
            // The message id is minted here, in the step's transaction: it is the idempotency
            // key at the destination, so every attempt of the send carries the same one.
            job.payload = Queued {
                workspace: walk.workspace,
                message: Uuid::now_v7(),
                key: &key,
                flow: enrollment.flow_id,
                version: enrollment.flow_version,
                node: &effect.node,
                enrollment: enrollment.id,
                person: enrollment.person_id,
                template: *template,
                destination: *destination,
                topic: *topic,
                dimensions,
                queued_at: walk.now,
            }
            .payload()
            .ok_or_else(|| JobError::new("the time cannot be formatted"))?;
            enqueue(&mut *conn, &job).await?;
            Ok(Some(label))
        }
        EffectKind::Sync {
            destination,
            operation,
            trait_keys,
        } => {
            let label = input_freshness(conn, walk).await?;
            let key = effect
                .effect_key
                .clone()
                .ok_or_else(|| JobError::new("an external effect has no key"))?;
            let mut job = NewJob::new(SYNC_JOB_KIND, enrollment.trace.clone());
            job.workspace_id = Some(walk.workspace);
            job.unique_key = Some(key.clone());
            job.payload = json!({
                "workspace_id": walk.workspace,
                "flow_id": enrollment.flow_id,
                "version": enrollment.flow_version,
                "node_id": effect.node,
                "enrollment_id": enrollment.id,
                "person_id": enrollment.person_id,
                "trace_id": enrollment.trace.trace_id,
                "idempotency_key": key,
                // The subject of the delivery attempts, minted in the step's transaction.
                "sync_id": Uuid::now_v7(),
                "destination_id": destination,
                "operation": operation.as_str(),
                "trait_keys": trait_keys,
            });
            enqueue(&mut *conn, &job).await?;
            Ok(Some(label))
        }
        EffectKind::Webhook {
            url,
            trait_keys,
            template,
        } => {
            let label = input_freshness(conn, walk).await?;
            let key = effect
                .effect_key
                .clone()
                .ok_or_else(|| JobError::new("an external effect has no key"))?;
            let occurred_at = walk
                .now
                .format(&Rfc3339)
                .map_err(|_| JobError::new("the time cannot be formatted"))?;
            let mut job = NewJob::new(WEBHOOK_JOB_KIND, enrollment.trace.clone());
            job.workspace_id = Some(walk.workspace);
            job.unique_key = Some(key.clone());
            job.payload = json!({
                "workspace_id": walk.workspace,
                "flow_id": enrollment.flow_id,
                "version": enrollment.flow_version,
                "node_id": effect.node,
                "enrollment_id": enrollment.id,
                "person_id": enrollment.person_id,
                "trace_id": enrollment.trace.trace_id,
                "idempotency_key": key,
                "occurred_at": occurred_at,
                "url": url,
                "trait_keys": trait_keys,
            });
            // Only a templated node names one, so the payload of every other node is unchanged.
            if let (Some(template), Some(payload)) = (template, job.payload.as_object_mut()) {
                payload.insert("template".to_owned(), json!(template));
            }
            enqueue(&mut *conn, &job).await?;
            Ok(Some(label))
        }
    }
}

fn effect_error(error: EffectError) -> JobError {
    match error {
        EffectError::Store(error) => error.into(),
        other => JobError::new(other.describe()),
    }
}

/// How current the data behind the flow is, for the decision of an external effect (ruling
/// 5c679ecb): recorded, and never a reason to pause.
async fn input_freshness(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
) -> Result<&'static str, JobError> {
    if let Some(label) = walk.freshness {
        return Ok(label);
    }
    let reads = flow_reads(conn, walk).await?;
    let now = walk.now;
    let unhealthy = unhealthy_inputs(conn, walk.workspace, &reads, now).await?;
    let worst = unhealthy
        .iter()
        .map(|input| input.state)
        .max()
        .unwrap_or(Freshness::Fresh);
    let label = match worst {
        Freshness::Fresh => "inputs_fresh",
        Freshness::Stale => "inputs_stale",
        Freshness::Failing => "inputs_failing",
    };
    walk.freshness = Some(label);
    Ok(label)
}

/// What the flow reads, in the shape of a segment version's `deps`: every branch condition that
/// still compiles, and the events its waits name.
async fn flow_reads(conn: &mut PgConnection, walk: &mut Walk<'_>) -> Result<Value, JobError> {
    let now = walk.now;
    let nodes: Vec<NodeKind> = walk
        .graph
        .nodes
        .iter()
        .map(|node| node.kind.clone())
        .collect();
    let (catalog, storage) = walk.catalog(conn).await?;
    let mut reads: [Vec<String>; 4] = Default::default();
    let keys = ["traits", "events", "lists", "segments"];
    for kind in &nodes {
        match kind {
            NodeKind::Branch { conditions } => {
                for condition in conditions {
                    let Some(deps) = condition_deps(condition, catalog, storage, now) else {
                        continue;
                    };
                    for (slot, key) in reads.iter_mut().zip(keys) {
                        let items = deps.get(key).and_then(Value::as_array);
                        slot.extend(
                            items
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str)
                                .map(str::to_owned),
                        );
                    }
                }
            }
            NodeKind::WaitUntilEvent { event, .. } => reads[1].push(event.name.clone()),
            _ => {}
        }
    }
    let [traits, events, lists, segments] = reads;
    Ok(json!({"traits": traits, "events": events, "lists": lists, "segments": segments}))
}

fn condition_deps(
    condition: &Value,
    catalog: &Catalog,
    storage: &TraitStorage,
    now: OffsetDateTime,
) -> Option<Value> {
    let compiled = compile_condition(condition, catalog, storage, now).ok()?;
    Some(compiled.deps.to_value())
}

/// Evaluates the conditions of a branch for the enrollment's person, in this transaction, and
/// records the results (never the values they were computed from).
async fn branch(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    node: &str,
    conditions: &[Value],
) -> Result<Vec<bool>, StepFailure> {
    let now = walk.now;
    let person = walk.enrollment.person_id;
    let workspace = walk.workspace;
    let mut compiled = Vec::with_capacity(conditions.len());
    for condition in conditions {
        let (catalog, storage) = walk
            .catalog(conn)
            .await
            .map_err(|error| StepFailure::at(node, error))?;
        let query = compile_condition(condition, catalog, storage, now).map_err(|_| {
            StepFailure::at(node, JobError::new("a branch condition no longer compiles"))
        })?;
        compiled.push(sql_query(query.batch_sql()));
    }
    let results = evaluate_conditions(&mut *conn, workspace, person, &compiled)
        .await
        .map_err(|error| StepFailure::at(node, error))?;
    let taken = results.iter().position(|matched| *matched).map_or(
        &decision_actions::FLOW_BRANCH_ELSE,
        |index| {
            decision_actions::BRANCH_CASES
                .get(index)
                .copied()
                .unwrap_or(&decision_actions::FLOW_BRANCH_CASE_MORE)
        },
    );
    let step = walk.step_of(node);
    let mut decisions = vec![walk.decision(taken, None, step, Some(node))];
    for (index, matched) in results.iter().enumerate() {
        decisions.push(walk.decision(
            if *matched {
                &decision_actions::FLOW_BRANCH_CONDITION_MATCHED
            } else {
                &decision_actions::FLOW_BRANCH_CONDITION_UNMATCHED
            },
            Some(CASES.get(index).copied().unwrap_or("case_more")),
            step,
            Some(node),
        ));
    }
    append_decisions(&mut *conn, &decisions)
        .await
        .map_err(|error| StepFailure::at(node, error))?;
    Ok(results)
}

/// Writes the state the walk ended in, and the decision that says how.
async fn finish(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    transition: &Transition,
) -> Result<(), StepFailure> {
    let state = &transition.state;
    let (status, exit_reason, error) = match &transition.outcome {
        Outcome::Waiting => (EnrollmentStatus::Waiting, None, None),
        Outcome::Completed => (EnrollmentStatus::Completed, None, None),
        Outcome::Exited { reason } => (
            EnrollmentStatus::Exited,
            Some(exit_reason_text(*reason)),
            None,
        ),
        Outcome::Failed { failure, .. } => {
            (EnrollmentStatus::Failed, None, Some(describe(*failure)))
        }
        Outcome::NeedsConditions { .. } | Outcome::Converted | Outcome::Stale => {
            return Err(JobError::new("the walk did not end").into());
        }
    };
    let version = i64::try_from(state.state_version)
        .map_err(|_| JobError::new("the state version does not fit"))?;
    let wake_at = state.wake_at.map(from_chrono).transpose()?;
    let saved = save_enrollment(
        &mut *conn,
        walk.workspace,
        walk.enrollment.id,
        walk.enrollment.state_version,
        &EnrollmentUpdate {
            status,
            node: &state.node,
            state_version: version,
            wake_at,
            exit_reason,
            error,
            failed_node: error.map(|_| state.node.as_str()),
            failure_placed: error.is_some(),
            resume_input: None,
            now: walk.now,
        },
    )
    .await?;
    if !saved {
        // The row is locked for this transaction, so another commit cannot have moved it.
        return Err(JobError::new("the enrollment moved under its lock").into());
    }
    let step = walk.step_of(&state.node);
    let decision = match &transition.outcome {
        Outcome::Waiting => {
            let spec = match node_of(walk, &state.node) {
                Some(NodeKind::WaitUntilEvent { .. }) => &decision_actions::FLOW_WAIT_UNTIL_EVENT,
                Some(NodeKind::WaitUntilTime { .. }) => &decision_actions::FLOW_WAIT_UNTIL_TIME,
                _ => &decision_actions::FLOW_WAIT_DURATION,
            };
            walk.decision(spec, None, step, Some(&state.node))
        }
        Outcome::Completed => walk.decision(
            &decision_actions::FLOW_ENROLLMENT_COMPLETED,
            None,
            step,
            Some(&state.node),
        ),
        Outcome::Exited { .. } => walk.decision(
            &decision_actions::FLOW_ENROLLMENT_EXITED,
            exit_reason,
            step,
            Some(&state.node),
        ),
        Outcome::Failed { failure, .. } => walk.decision(
            &decision_actions::FLOW_ENROLLMENT_FAILED,
            Some(failure_code(*failure)),
            step,
            Some(&state.node),
        ),
        _ => unreachable!("the walk ended above"),
    };
    append_decisions(&mut *conn, &[decision]).await?;
    Ok(())
}

/// The last attempt failed. The savepoint already took the step's effects and rows back, so the
/// enrollment keeps the state the job started from (its node, waits, timer and version) and is
/// marked `failed` with the node that failed and the error stored. No step row is written for
/// the failing node, so a retry runs the whole step again, effects included.
async fn fail_enrollment(
    conn: &mut PgConnection,
    job: &StepJobPayload,
    enrollment: &Enrollment,
    failure: &StepFailure,
    now: OffsetDateTime,
    at: (Uuid, i32),
) -> Result<(), JobError> {
    let failed_node = failure.node.as_deref().unwrap_or(&enrollment.node);
    let message = failure.error.message();
    // The input to run again on a retry. A failed retry keeps what the enrollment stopped on.
    let resume_input = match job.input {
        StepJobInput::Resume { .. } => enrollment.resume_input.clone(),
        _ => Some(job.input.to_stored()),
    };
    let saved = save_enrollment(
        &mut *conn,
        job.workspace,
        enrollment.id,
        enrollment.state_version,
        &EnrollmentUpdate {
            status: EnrollmentStatus::Failed,
            node: &enrollment.node,
            state_version: enrollment.state_version,
            wake_at: enrollment.wake_at,
            exit_reason: None,
            error: Some(message),
            failed_node: Some(failed_node),
            failure_placed: failure.node.is_some(),
            resume_input: resume_input.as_ref(),
            now,
        },
    )
    .await?;
    if !saved {
        return Err(JobError::new("the enrollment moved under its lock"));
    }
    let decision = NewDecision {
        graph: Some(GraphPoint::at(enrollment.flow_version, failed_node)),
        workspace_id: job.workspace,
        trace: enrollment.trace.clone(),
        spec: &decision_actions::FLOW_ENROLLMENT_FAILED,
        detail: Some("step_failed"),
        subject_ids: vec![
            ("flow", enrollment.flow_id),
            ("enrollment", enrollment.id),
            ("person", enrollment.person_id),
        ],
        job: Some(at),
    };
    append_decisions(&mut *conn, &[decision]).await?;
    Ok(())
}

/// The text an enrollment stores for the reason it exited.
pub(super) const fn exit_reason_text(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::Goal => "goal",
        ExitReason::GoalCriterion => "goal_criterion",
        ExitReason::ExitEvent => "exit_event",
        ExitReason::LeftSegment => "left_segment",
    }
}

const fn failure_code(failure: Failure) -> &'static str {
    match failure {
        Failure::InvalidTime => "invalid_time",
        Failure::OutOfRange => "duration_out_of_range",
        Failure::EmptySplit => "empty_split",
    }
}

/// What an operator reads of a failure the engine reported.
const fn describe(failure: Failure) -> &'static str {
    match failure {
        Failure::InvalidTime => "the wait time is invalid",
        Failure::OutOfRange => "a duration does not fit the clock",
        Failure::EmptySplit => "a split has weights that sum to zero",
    }
}

#[cfg(test)]
pub(super) mod tests {
    use zirv_flow_store::decision_testing::PERSONAL_DATA;

    use std::time::Duration;

    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration as Span;
    use zirv_flow_core::events::EventMode;
    use zirv_flow_core::people::{Precedence, TraitType};
    use zirv_flow_core::{
        Cause, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceName,
    };
    use zirv_flow_store::{
        ClaimedJob, Clock, EnrollOutcome, EnrollRequest, EventRecord, FlowPatch, ManualClock,
        NewEventSchema, NewFlow, NewTraitDefinition, Occurrence, ProfileRecord, find_enrollment,
    };

    use super::*;

    #[test]
    fn the_kind_name_is_flow_step_and_it_is_registered() {
        assert_eq!(FlowStep::KIND, "flow.step");
        assert!(crate::Registry::built_in().get("flow.step").is_some());
    }

    #[test]
    fn every_branch_port_is_named_and_the_names_match_the_graph() {
        for (index, port) in CASES.iter().enumerate() {
            assert_eq!(*port, zirv_flow_core::flow::case_port(index));
        }
    }

    #[test]
    fn times_cross_between_the_clocks_without_losing_a_microsecond() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_900_000_000_123_456_000).unwrap();
        let there = to_chrono(at).unwrap();
        assert_eq!(from_chrono(there).unwrap(), at);
    }

    #[test]
    fn every_failure_has_a_message() {
        for failure in [
            Failure::InvalidTime,
            Failure::OutOfRange,
            Failure::EmptySplit,
        ] {
            assert!(!describe(failure).is_empty());
        }
    }

    fn stale(
        status: EnrollmentStatus,
        version: i64,
        input: StepJobInput,
        job_version: i64,
    ) -> bool {
        let enrollment = Enrollment {
            id: Uuid::nil(),
            flow_id: Uuid::nil(),
            flow_version: 1,
            person_id: Uuid::nil(),
            status,
            node: "n".into(),
            context: json!({}),
            wake_at: None,
            state_version: version,
            awaiting: None,
            exit_reason: None,
            error: None,
            failed_node: None,
            failure_placed: false,
            resume_input: None,
            trace: TraceContext::root(Cause::api("t")),
            created_at: OffsetDateTime::UNIX_EPOCH,
            ended_at: None,
        };
        is_stale(
            &enrollment,
            &StepJobPayload {
                workspace: WorkspaceId::from_uuid(Uuid::nil()),
                enrollment: Uuid::nil(),
                state_version: job_version,
                input,
            },
        )
    }

    #[test]
    fn a_job_is_stale_unless_the_version_and_the_status_are_what_it_expects() {
        use EnrollmentStatus::{Active, Completed, Waiting};
        assert!(!stale(Active, 0, StepJobInput::Start, 0));
        assert!(stale(Waiting, 1, StepJobInput::Start, 0));
        assert!(!stale(Waiting, 1, StepJobInput::Wake, 1));
        assert!(stale(Waiting, 2, StepJobInput::Wake, 1), "an older timer");
        assert!(
            stale(Completed, 2, StepJobInput::Wake, 2),
            "an ended enrollment"
        );
        assert!(stale(Active, 1, StepJobInput::Wake, 1), "the wrong status");
    }

    /// One workspace, one clock and a worker that runs `flow.step` jobs by hand.
    pub(crate) struct Env {
        pub(crate) pool: PgPool,
        pub(crate) ws: WorkspaceId,
        pub(crate) clock: ManualClock,
    }

    impl Env {
        pub(crate) async fn new(pool: PgPool) -> Self {
            zirv_flow_store::migrate(&pool).await.unwrap();
            let name = WorkspaceName::parse("ours").unwrap();
            let slug = Slug::parse("ours").unwrap();
            let ws = zirv_flow_store::create_workspace(&pool, &name, &slug)
                .await
                .unwrap()
                .id;
            let now = OffsetDateTime::now_utc();
            let now = now.replace_nanosecond(now.microsecond() * 1000).unwrap();
            Self {
                pool,
                ws,
                clock: ManualClock::new(now),
            }
        }

        pub(crate) fn advance(&self, seconds: i64) {
            self.clock.advance(Span::seconds(seconds));
        }

        pub(crate) async fn define_trait(&self, key: &str, kind: TraitType) {
            zirv_flow_store::create_trait_definition(
                &self.pool,
                self.ws,
                &NewTraitDefinition {
                    key: key.into(),
                    trait_type: kind,
                    precedence: Precedence::Latest,
                    source_order: Vec::new(),
                    pii: false,
                    channel_address: false,
                    inferred: false,
                    description: String::new(),
                },
            )
            .await
            .unwrap();
        }

        pub(crate) async fn define_event(&self, name: &str) {
            zirv_flow_store::create_event_schema(
                &self.pool,
                self.ws,
                &NewEventSchema {
                    name: name.into(),
                    mode: EventMode::Warn,
                    description: String::new(),
                    properties: serde_json::from_value(
                        json!([{"name": "total", "type": "number"}]),
                    )
                    .unwrap(),
                },
            )
            .await
            .unwrap();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::publish_schema_draft(&mut connection, self.ws, name, None)
                .await
                .unwrap();
        }

        pub(crate) async fn person(&self, email: &str, traits: Value) -> Uuid {
            let record = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: email.into(),
                identifiers: vec![("email".into(), email.into())],
                traits: traits.as_object().cloned().unwrap(),
                observed_at: self.clock.now() - Span::days(1),
                trace: TraceContext::root(Cause::api("t")),
            };
            match &zirv_flow_store::apply_profile_batch(&self.pool, self.ws, &[record])
                .await
                .unwrap()[0]
            {
                zirv_flow_store::ProfileOutcome::Applied { person_id, .. }
                | zirv_flow_store::ProfileOutcome::Unchanged { person_id, .. } => *person_id,
                zirv_flow_store::ProfileOutcome::Rejected { reason } => {
                    panic!("rejected: {reason}")
                }
            }
        }

        pub(crate) async fn list(&self, name: &str) -> Uuid {
            zirv_flow_store::create_list(&self.pool, self.ws, name, "", None)
                .await
                .unwrap()
                .id
        }

        /// Creates a flow and publishes its first version.
        pub(crate) async fn flow(&self, name: &str, graph: Value) -> Uuid {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id,
                    name: name.into(),
                    graph,
                    settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            self.publish(id).await;
            id
        }

        /// Creates a flow with these settings and publishes its first version.
        pub(crate) async fn flow_with(&self, name: &str, graph: Value, settings: Value) -> Uuid {
            let id = Uuid::now_v7();
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::create_flow(
                &mut connection,
                self.ws,
                &NewFlow {
                    id,
                    name: name.into(),
                    graph,
                    settings,
                },
                None,
            )
            .await
            .unwrap();
            drop(connection);
            self.publish(id).await;
            id
        }

        /// Runs every ready job of one kind, as a worker does, and returns how many ran.
        pub(crate) async fn run_kind<K: JobKind>(&self, kind: &'static str, handler: &K) -> usize {
            let mut ran = 0;
            while let Some(claimed) = zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[kind],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
            {
                let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
                let result = {
                    let mut ctx = JobContext::new(&mut tx, &claimed)
                        .with_clock(Clock::Manual(self.clock.clone()));
                    handler.run(&mut ctx, claimed.payload.clone()).await
                };
                result.unwrap();
                zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                    .await
                    .unwrap();
                zirv_flow_store::commit(tx).await.unwrap();
                ran += 1;
            }
            ran
        }

        pub(crate) async fn pause(&self, flow: Uuid) {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let trace = TraceContext::root(Cause::api("pause"));
            zirv_flow_store::pause_flow(&mut tx, self.ws, flow, &trace)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// Resumes the flow under a trace caused by `manual`, as the API does for a user.
        pub(crate) async fn resume(&self, flow: Uuid) -> TraceContext {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let trace = TraceContext::root(Cause::manual(zirv_flow_core::UserId::from_uuid(
                Uuid::new_v4(),
            )));
            zirv_flow_store::resume_flow(&mut tx, self.ws, flow, &trace)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            trace
        }

        pub(crate) async fn archive(&self, flow: Uuid) -> TraceContext {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let trace = TraceContext::root(Cause::manual(zirv_flow_core::UserId::from_uuid(
                Uuid::new_v4(),
            )));
            zirv_flow_store::archive_flow(&mut tx, self.ws, flow, &trace)
                .await
                .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            trace
        }

        /// The saved content of a segment of the people whose `plan` trait is `plan`.
        async fn plan_version(&self, plan: &str) -> zirv_flow_store::NewSegmentVersion {
            let definition = json!({"op": "eq", "trait": "plan", "value": plan, "v": 1});
            let mut connection = self.pool.acquire().await.unwrap();
            let (catalog, storage) =
                catalog_of(segment_catalog(&mut connection, self.ws).await.unwrap());
            let parsed = parse_and_validate(&definition, &catalog).unwrap();
            let compiled =
                zirv_flow_segments::compile(&parsed, &catalog, &storage, OffsetDateTime::now_utc())
                    .unwrap();
            zirv_flow_store::NewSegmentVersion {
                definition,
                normalized: zirv_flow_segments::normalize(parsed).to_value(),
                deps: compiled.deps.to_value(),
                time_sensitive: compiled.time_sensitive,
            }
        }

        /// A segment of the people whose `plan` trait is `plan`.
        pub(crate) async fn plan_segment(&self, name: &str, plan: &str) -> Uuid {
            let content = self.plan_version(plan).await;
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::lock_segments(&mut tx, self.ws)
                .await
                .unwrap();
            let (segment, _) = zirv_flow_store::create_segment(
                &mut tx,
                self.ws,
                &zirv_flow_store::NewSegment {
                    id: Uuid::now_v7(),
                    name: name.into(),
                    description: String::new(),
                    cadence_minutes: 60,
                },
                &content,
                None,
                &TraceContext::root(Cause::api("save")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            segment.id
        }

        /// Saves a new definition of the segment: the people with this `plan`.
        pub(crate) async fn resave_plan_segment(&self, segment: Uuid, plan: &str) {
            let content = self.plan_version(plan).await;
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            zirv_flow_store::lock_segments(&mut tx, self.ws)
                .await
                .unwrap();
            zirv_flow_store::update_segment(
                &mut tx,
                self.ws,
                segment,
                &zirv_flow_store::SegmentPatch {
                    version: Some(content),
                    ..zirv_flow_store::SegmentPatch::default()
                },
                None,
                &TraceContext::root(Cause::api("save")),
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
        }

        /// Refreshes a segment as its job does, under `trace`.
        pub(crate) async fn refresh(&self, segment: Uuid, trigger: &str, trace: TraceContext) {
            sqlx::query("DELETE FROM jobs WHERE kind = 'segment.refresh'")
                .execute(&self.pool)
                .await
                .unwrap();
            let mut job = NewJob::new(zirv_flow_store::REFRESH_JOB_KIND, trace);
            job.workspace_id = Some(self.ws);
            job.payload =
                json!({"workspace_id": self.ws, "segment_id": segment, "trigger": trigger});
            job.unique_key = Some(format!("test:{}", Uuid::new_v4()));
            enqueue(&self.pool, &job).await.unwrap();
            self.run_kind(
                zirv_flow_store::REFRESH_JOB_KIND,
                &crate::kinds::segment_refresh::SegmentRefresh,
            )
            .await;
        }

        /// Sets a person's `plan` trait directly, as a data change the next refresh sees.
        pub(crate) async fn set_plan(&self, person: Uuid, plan: &str) {
            sqlx::query(
                "UPDATE people SET traits = jsonb_build_object('plan', $2::text) WHERE id = $1",
            )
            .bind(person)
            .bind(plan)
            .execute(&self.pool)
            .await
            .unwrap();
        }

        pub(crate) async fn publish(&self, id: Uuid) -> i32 {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let locked = zirv_flow_store::lock_flow(&mut tx, self.ws, id)
                .await
                .unwrap();
            let trace = TraceContext::root(Cause::api("publish"));
            let (_, version) = zirv_flow_store::publish_flow(
                &mut tx,
                &zirv_flow_store::Clock::default(),
                self.ws,
                &locked,
                None,
                &trace,
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            version.version
        }

        /// Replaces the draft graph and publishes it as the next version.
        pub(crate) async fn republish(&self, id: Uuid, graph: Value) -> i32 {
            let mut connection = self.pool.acquire().await.unwrap();
            zirv_flow_store::update_flow_draft(
                &mut connection,
                self.ws,
                id,
                &FlowPatch {
                    graph: Some(graph),
                    ..FlowPatch::default()
                },
            )
            .await
            .unwrap();
            drop(connection);
            self.publish(id).await
        }

        pub(crate) async fn enroll(
            &self,
            flow: Uuid,
            version: i32,
            people: &[Uuid],
        ) -> EnrollOutcome {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let outcome = zirv_flow_store::enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: self.ws,
                    flow,
                    version,
                    people,
                    occurrence: Occurrence {
                        trigger: "manual",
                        id: Uuid::now_v7(),
                    },
                    now: self.clock.now(),
                    trace: &TraceContext::root(Cause::api("enroll")),
                },
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            outcome
        }

        /// Pushes the jobs that do not match far into the future and makes the ones that do
        /// due: a timer once the clock reached its time, a start or an event at once.
        pub(crate) async fn hold_others(&self, input: Option<&str>, enrollment: Option<Uuid>) {
            sqlx::query(
                "UPDATE jobs SET run_at = run_at + interval '100 years' \
                 WHERE kind = 'flow.step' AND state = 'ready' \
                   AND NOT (($1::text IS NULL OR payload ->> 'input' = $1) \
                        AND ($2::uuid IS NULL OR payload ->> 'enrollment_id' = $2::text))",
            )
            .bind(input)
            .bind(enrollment)
            .execute(&self.pool)
            .await
            .unwrap();
            sqlx::query(
                "UPDATE jobs SET run_at = now() - interval '1 second' \
                 WHERE kind = 'flow.step' AND state = 'ready' \
                   AND run_at < now() + interval '50 years' \
                   AND (payload ->> 'input' <> 'wake' OR run_at <= $1)",
            )
            .bind(self.clock.now())
            .execute(&self.pool)
            .await
            .unwrap();
        }

        pub(crate) async fn release(&self) {
            sqlx::query(
                "UPDATE jobs SET run_at = run_at - interval '100 years' \
                 WHERE kind = 'flow.step' AND run_at > now() + interval '50 years'",
            )
            .execute(&self.pool)
            .await
            .unwrap();
        }

        pub(crate) async fn claim(&self) -> Option<ClaimedJob> {
            zirv_flow_store::claim_job(
                &self.pool,
                "test",
                &[STEP_JOB_KIND],
                Duration::from_secs(30),
                None,
            )
            .await
            .unwrap()
        }

        /// Runs one claimed job as a worker does: its work commits with its completion, and an
        /// error rolls it back and puts the job up for another attempt.
        pub(crate) async fn execute(&self, claimed: ClaimedJob) -> Result<(), JobError> {
            let mut tx = zirv_flow_store::begin(&self.pool).await.unwrap();
            let result = {
                let mut ctx = JobContext::new(&mut tx, &claimed)
                    .with_clock(Clock::Manual(self.clock.clone()));
                FlowStep.run(&mut ctx, claimed.payload.clone()).await
            };
            match result {
                Ok(()) => {
                    zirv_flow_store::complete_job(&mut *tx, claimed.id, claimed.lease_token)
                        .await
                        .unwrap();
                    zirv_flow_store::commit(tx).await.unwrap();
                    Ok(())
                }
                Err(error) => {
                    drop(tx);
                    zirv_flow_store::fail_job(
                        &self.pool,
                        claimed.id,
                        claimed.lease_token,
                        Duration::ZERO,
                        error.message(),
                    )
                    .await
                    .unwrap();
                    Err(error)
                }
            }
        }

        /// Claims the due jobs that match and runs them, until none is left.
        pub(crate) async fn run_matching(
            &self,
            input: Option<&str>,
            enrollment: Option<Uuid>,
        ) -> usize {
            let mut ran = 0;
            loop {
                self.hold_others(input, enrollment).await;
                let claimed = self.claim().await;
                self.release().await;
                let Some(claimed) = claimed else {
                    return ran;
                };
                let _ = self.execute(claimed).await;
                ran += 1;
            }
        }

        /// Runs every job that is due, including the ones those jobs make due.
        pub(crate) async fn run_due(&self) -> usize {
            self.run_matching(None, None).await
        }

        pub(crate) async fn enrollment(&self, id: Uuid) -> Enrollment {
            let mut connection = self.pool.acquire().await.unwrap();
            find_enrollment(&mut connection, self.ws, id)
                .await
                .unwrap()
                .unwrap()
        }

        pub(crate) async fn traits(&self, person: Uuid) -> Value {
            sqlx::query_scalar("SELECT traits FROM people WHERE id = $1")
                .bind(person)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        /// The (action, outcome) of every decision on a trace, sorted.
        pub(crate) async fn decisions(&self, trace: &TraceContext) -> Vec<(String, String)> {
            let mut rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT action, outcome FROM decision_records \
                 WHERE workspace_id = $1 AND trace_id = $2",
            )
            .bind(self.ws.as_uuid())
            .bind(trace.trace_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .unwrap();
            rows.sort();
            rows
        }

        pub(crate) async fn count(&self, sql: &str) -> i64 {
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
                .fetch_one(&self.pool)
                .await
                .unwrap()
        }

        pub(crate) async fn apply_event(&self, email: &str, name: &str, properties: Value) {
            let key =
                HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
            let record = EventRecord {
                source_record_id: None,
                source: "web".into(),
                identifiers: vec![("email".into(), email.into())],
                name: name.into(),
                properties: properties.as_object().cloned().unwrap(),
                timestamp: None,
                received_at: self.clock.now(),
                message_id: None,
                trace: TraceContext::root(Cause::api("event")),
            };
            zirv_flow_store::apply_event_batch(&self.pool, self.ws, &key, &[record])
                .await
                .unwrap();
        }
    }

    pub(crate) fn edge(id: &str, from: &str, port: &str, to: &str) -> Value {
        json!({"id": id, "from": from, "port": port, "to": to})
    }

    /// wait, set a trait, branch on it, add to a list on the first case.
    fn walk_graph(list: Uuid) -> Value {
        json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "t", "type": "action.update_trait", "key": "tier",
             "change": {"op": "set", "value": "gold"}},
            {"id": "b", "type": "branch",
             "conditions": [{"trait": "tier", "op": "eq", "value": "gold"}]},
            {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
            {"id": "won", "type": "exit"},
            {"id": "lost", "type": "exit"}
        ], "edges": [
            edge("e1", "w", "next", "t"),
            edge("e2", "t", "next", "b"),
            edge("e3", "b", "case_0", "l"),
            edge("e4", "b", "else", "lost"),
            edge("e5", "l", "next", "won")
        ]})
    }

    /// A wait for an `order` event, an hour at most, then a trait that says which path ran.
    fn race_graph(filters: Value) -> Value {
        json!({"v": 1, "nodes": [
            {"id": "wait", "type": "wait.until_event", "event": "order",
             "filters": filters, "timeout_seconds": 3600},
            {"id": "by_event", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "event"}},
            {"id": "by_timeout", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "timeout"}},
            {"id": "end_event", "type": "exit"},
            {"id": "end_timeout", "type": "exit"}
        ], "edges": [
            edge("e1", "wait", "event", "by_event"),
            edge("e2", "wait", "timeout", "by_timeout"),
            edge("e3", "by_event", "next", "end_event"),
            edge("e4", "by_timeout", "next", "end_timeout")
        ]})
    }

    async fn path_of(env: &Env, person: Uuid) -> Option<String> {
        env.traits(person).await["path"].as_str().map(str::to_owned)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_published_flow_enrolls_a_person_who_walks_a_wait_a_branch_and_an_internal_action_to_the_end(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("tier", TraitType::String).await;
            let list = env.list("Golden").await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let flow = env.flow("Walk", walk_graph(list)).await;

            let enrolled = env.enroll(flow, 1, &[ada]).await;
            let id = enrolled.enrolled[0].enrollment;
            env.run_due().await;
            let asleep = env.enrollment(id).await;
            env.advance(61);
            env.run_due().await;
            let done = env.enrollment(id).await;

            assert_eq!(asleep.status, EnrollmentStatus::Waiting);
            assert_eq!(asleep.node, "w");
            assert_eq!(asleep.state_version, 1);
            assert_eq!(done.status, EnrollmentStatus::Completed);
            assert_eq!(done.node, "won");
            assert_eq!(done.flow_version, 1);
            assert!(done.ended_at.is_some());
            assert_eq!(env.traits(ada).await["tier"], "gold");
            let sources: Vec<String> = sqlx::query_scalar("SELECT source FROM list_members")
                .fetch_all(&env.pool)
                .await
                .unwrap();
            assert_eq!(sources, ["flow"]);
            let mut nodes: Vec<String> = sqlx::query_scalar(
                "SELECT node_id FROM step_executions WHERE enrollment_id = $1 AND outcome = 'done'",
            )
            .bind(id)
            .fetch_all(&env.pool)
            .await
            .unwrap();
            nodes.sort();
            assert_eq!(nodes, ["b", "l", "t", "w", "won"]);
            let source: String = sqlx::query_scalar(
                "SELECT source FROM source_records WHERE person_id = $1 AND source LIKE 'flow:%'",
            )
            .bind(ada)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(source, format!("flow:{flow}"));
            assert_eq!(
                env.count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                    .await,
                0
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wake_records_how_late_it_ran_and_the_start_records_nothing(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            let env = Env::new(pool).await;
            env.define_trait("tier", TraitType::String).await;
            let list = env.list("Golden").await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let flow = env.flow("Late", walk_graph(list)).await;
            env.enroll(flow, 1, &[ada]).await;

            env.run_due().await;
            let after_start = handle.render();
            env.advance(100);
            env.run_due().await;

            assert!(
                !after_start.contains("zirv_flow_timer_wake_lateness_seconds"),
                "{after_start}"
            );
            let output = handle.render();
            let sample = |name: &str| {
                output.lines().find_map(|line| {
                    line.strip_prefix(name)?
                        .strip_prefix(' ')?
                        .parse::<f64>()
                        .ok()
                })
            };
            assert_eq!(
                sample("zirv_flow_timer_wake_lateness_seconds_count"),
                Some(1.0),
                "{output}"
            );
            let late = sample("zirv_flow_timer_wake_lateness_seconds_sum").unwrap();
            assert!((39.0..41.0).contains(&late), "{output}");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_retried_wake_records_its_lateness_once(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "bump", "type": "action.update_trait", "key": "plan",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "bump"), edge("e2", "bump", "next", "end")]});
            let flow = env.flow("Retried", graph).await;
            env.enroll(flow, 1, &[ada]).await;
            env.run_due().await;
            sqlx::query("UPDATE jobs SET max_attempts = 2 WHERE kind = 'flow.step'")
                .execute(&env.pool)
                .await
                .unwrap();
            env.advance(100);

            let mut attempts = Vec::new();
            for _ in 0..2 {
                env.hold_others(None, None).await;
                let claimed = env.claim().await.unwrap();
                env.release().await;
                attempts.push(claimed.attempts);
                let _ = env.execute(claimed).await;
            }

            assert_eq!(attempts, [1, 2]);
            let output = handle.render();
            let count = output
                .lines()
                .find_map(|line| line.strip_prefix("zirv_flow_timer_wake_lateness_seconds_count "));
            assert_eq!(count, Some("1"), "{output}");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_leader_publishes_the_enrollment_gauges_and_the_lateness_of_a_seeded_timer(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let _guard = ::metrics::set_default_local_recorder(&recorder);
        let env = Env::new(pool.clone()).await;
        env.define_trait("tier", TraitType::String).await;
        let list = env.list("Golden").await;
        let flow = env.flow("Sampled", walk_graph(list)).await;
        for email in [PERSONAL_DATA.email, "bob@example.com"] {
            let person = env.person(email, json!({})).await;
            env.enroll(flow, 1, &[person]).await;
        }
        env.run_due().await;
        sqlx::query(
            "UPDATE enrollments SET wake_at = now() - interval '90 seconds'              WHERE id = (SELECT id FROM enrollments ORDER BY id LIMIT 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut leader = crate::scheduler::Scheduler::new(
            pool.clone(),
            crate::scheduler::SchedulerConfig::default(),
        );

        leader.tick().await.unwrap();

        let output = handle.render();
        let gauge = |series: &str| {
            output.lines().find_map(|line| {
                line.strip_prefix(series)?
                    .strip_prefix(' ')?
                    .parse::<f64>()
                    .ok()
            })
        };
        assert_eq!(
            gauge("zirv_flow_enrollments{state=\"waiting\"}"),
            Some(2.0),
            "{output}"
        );
        assert_eq!(
            gauge("zirv_flow_enrollments{state=\"active\"}"),
            Some(0.0),
            "{output}"
        );
        let late = gauge("zirv_flow_timer_lateness_seconds").unwrap();
        assert!((90.0..100.0).contains(&late), "{output}");
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_branch_sees_the_internal_effect_applied_earlier_in_the_same_step(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("tier", TraitType::String).await;
            let list = env.list("Golden").await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let graph = json!({"v": 1, "nodes": [
                {"id": "t", "type": "action.update_trait", "key": "tier",
                 "change": {"op": "set", "value": "gold"}},
                {"id": "b", "type": "branch",
                 "conditions": [{"trait": "tier", "op": "eq", "value": "gold"}]},
                {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
                {"id": "won", "type": "exit"},
                {"id": "lost", "type": "exit"}
            ], "edges": [
                edge("e1", "t", "next", "b"),
                edge("e2", "b", "case_0", "l"),
                edge("e3", "b", "else", "lost"),
                edge("e4", "l", "next", "won")
            ]});
            let flow = env.flow("Same step", graph).await;
            let enrolled = env.enroll(flow, 1, &[ada]).await;

            let ran = env.run_due().await;

            let done = env.enrollment(enrolled.enrolled[0].enrollment).await;
            assert_eq!(ran, 1, "one job walked the whole flow");
            assert_eq!(
                (done.status, done.node.as_str()),
                (EnrollmentStatus::Completed, "won")
            );
            let decisions = env.decisions(&enrolled.enrolled[0].trace).await;
            assert!(decisions.contains(&("flow.branch".into(), "case_0".into())));
            assert!(decisions.contains(&("flow.branch.condition".into(), "matched".into())));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn publishing_v2_leaves_a_v1_enrollment_on_v1_while_new_enrollments_use_v2(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let bob = env.person("bob@example.com", json!({})).await;
            let v1 = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "end")]});
            let v2 = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "t", "type": "action.update_trait", "key": "path",
             "change": {"op": "set", "value": "v2"}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "t"), edge("e2", "t", "next", "end")]});
            let flow = env.flow("Versions", v1).await;
            let first = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;
            env.run_due().await;
            let in_flight = {
                let mut connection = env.pool.acquire().await.unwrap();
                zirv_flow_store::count_in_flight_enrollments(&mut connection, env.ws, flow)
                    .await
                    .unwrap()
            };

            let version = env.republish(flow, v2).await;
            let second = env.enroll(flow, version, &[bob]).await.enrolled[0].enrollment;
            env.run_due().await;
            env.advance(61);
            env.run_due().await;

            assert_eq!((version, in_flight), (2, 1));
            let (one, two) = (env.enrollment(first).await, env.enrollment(second).await);
            assert_eq!(
                (one.flow_version, one.status),
                (1, EnrollmentStatus::Completed)
            );
            assert_eq!(
                (two.flow_version, two.status),
                (2, EnrollmentStatus::Completed)
            );
            assert_eq!(path_of(&env, ada).await, None, "v1 has no trait step");
            assert_eq!(path_of(&env, bob).await.as_deref(), Some("v2"));
        })
        .await;
    }

    /// Enrolls a person and runs the start, so the enrollment waits for an order.
    async fn waiting_for_an_order(env: &Env, email: &str, flow: Uuid) -> (Uuid, Uuid) {
        let person = env.person(email, json!({})).await;
        let id = env.enroll(flow, 1, &[person]).await.enrolled[0].enrollment;
        env.run_matching(Some("start"), Some(id)).await;
        let waiting = env.enrollment(id).await;
        assert_eq!(waiting.status, EnrollmentStatus::Waiting);
        assert_eq!(waiting.awaiting.as_deref(), Some("order"));
        (id, person)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_that_commits_before_the_timeout_takes_the_event_path_and_the_timer_is_stale(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            let flow = env.flow("Race", race_graph(json!([]))).await;
            let (id, ada) = waiting_for_an_order(&env, PERSONAL_DATA.email, flow).await;
            env.apply_event(PERSONAL_DATA.email, "order", json!({}))
                .await;

            env.run_matching(Some("event"), Some(id)).await;
            env.advance(3601);
            env.run_matching(Some("wake"), Some(id)).await;

            let done = env.enrollment(id).await;
            assert_eq!(path_of(&env, ada).await.as_deref(), Some("event"));
            assert_eq!(
                (done.status, done.node.as_str()),
                (EnrollmentStatus::Completed, "end_event")
            );
            assert_eq!(done.state_version, 2);
            assert_eq!(
                env.count("SELECT count(*) FROM decision_records WHERE action = 'flow.wake'")
                    .await,
                1
            );
            assert_eq!(env.count("SELECT count(*) FROM enrollment_waits").await, 0);
            assert_eq!(
                env.count("SELECT count(*) FROM step_executions WHERE node_id = 'by_timeout'")
                    .await,
                0
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_timeout_that_commits_before_the_event_takes_the_timeout_path_and_the_event_is_stale(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            let flow = env.flow("Race", race_graph(json!([]))).await;
            let (id, ada) = waiting_for_an_order(&env, PERSONAL_DATA.email, flow).await;
            env.apply_event(PERSONAL_DATA.email, "order", json!({}))
                .await;
            env.advance(3601);

            env.run_matching(Some("wake"), Some(id)).await;
            let after_timeout = env.enrollment(id).await;
            env.run_matching(Some("event"), Some(id)).await;

            let done = env.enrollment(id).await;
            assert_eq!(path_of(&env, ada).await.as_deref(), Some("timeout"));
            assert_eq!(done, after_timeout, "the late event changed nothing");
            assert_eq!(
                (done.status, done.node.as_str()),
                (EnrollmentStatus::Completed, "end_timeout")
            );
            assert_eq!(
                env.count("SELECT count(*) FROM step_executions WHERE node_id = 'by_event'")
                    .await,
                0
            );
            assert_eq!(
                env.count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                    .await,
                0
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_and_its_timeout_running_at_the_same_moment_take_exactly_one_path(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            let flow = env.flow("Race", race_graph(json!([]))).await;
            for round in 0..6 {
                let email = format!("p{round}@example.com");
                let (id, person) = waiting_for_an_order(&env, &email, flow).await;
                env.apply_event(&email, "order", json!({})).await;
                env.advance(3601);
                // Both jobs are claimed before either runs.
                env.hold_others(None, Some(id)).await;
                let first = env.claim().await.unwrap();
                let second = env.claim().await.unwrap();
                env.release().await;

                let (one, two) = tokio::join!(env.execute(first), env.execute(second));

                assert!(one.is_ok() && two.is_ok());
                let done = env.enrollment(id).await;
                let path = path_of(&env, person).await.unwrap();
                assert!(path == "event" || path == "timeout", "{path}");
                assert_eq!(done.status, EnrollmentStatus::Completed);
                assert_eq!(done.state_version, 2, "one transition after the wait");
                let ran: Vec<String> = sqlx::query_scalar(
                    "SELECT node_id FROM step_executions WHERE enrollment_id = $1 \
                 AND node_id IN ('by_event', 'by_timeout')",
                )
                .bind(id)
                .fetch_all(&env.pool)
                .await
                .unwrap();
                assert_eq!(ran.len(), 1, "{ran:?}");
                assert!(ran[0].ends_with(path.as_str()), "{ran:?} {path}");
                let wakes: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM decision_records WHERE action = 'flow.wake' \
                 AND subject_ids ->> 'enrollment' = $1",
                )
                .bind(id.to_string())
                .fetch_one(&env.pool)
                .await
                .unwrap();
                assert_eq!(wakes, 1);
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_event_that_fails_the_waits_filters_leaves_the_enrollment_asleep(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            env.define_event("order").await;
            let filters = json!([{"prop": "total", "op": "gt", "value": 100}]);
            let flow = env.flow("Filtered", race_graph(filters)).await;
            let (id, ada) = waiting_for_an_order(&env, PERSONAL_DATA.email, flow).await;

            env.apply_event(PERSONAL_DATA.email, "order", json!({"total": 5}))
                .await;
            let ran = env.run_matching(Some("event"), Some(id)).await;
            let still = env.enrollment(id).await;
            env.apply_event(PERSONAL_DATA.email, "order", json!({"total": 500}))
                .await;
            env.run_matching(Some("event"), Some(id)).await;
            let woken = env.enrollment(id).await;

            assert_eq!(ran, 1);
            assert_eq!(
                (still.status, still.state_version),
                (EnrollmentStatus::Waiting, 1)
            );
            assert_eq!(still.awaiting.as_deref(), Some("order"));
            assert_eq!(woken.status, EnrollmentStatus::Completed);
            assert_eq!(path_of(&env, ada).await.as_deref(), Some("event"));
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn stale_and_orphaned_jobs_change_nothing_at_all(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("path", TraitType::String).await;
            let flow = env.flow("Race", race_graph(json!([]))).await;
            let (id, _) = waiting_for_an_order(&env, PERSONAL_DATA.email, flow).await;
            let before = env.enrollment(id).await;
            let decisions = env.count("SELECT count(*) FROM decision_records").await;
            let steps = env.count("SELECT count(*) FROM step_executions").await;
            sqlx::query("DELETE FROM jobs")
                .execute(&env.pool)
                .await
                .unwrap();
            let mut payloads = vec![
                (id, 0, StepJobInput::Start),
                (id, 0, StepJobInput::Wake),
                (id, 7, StepJobInput::Wake),
                (Uuid::now_v7(), 0, StepJobInput::Start),
            ];
            payloads.push((
                id,
                1,
                StepJobInput::Event {
                    event_id: Uuid::now_v7(),
                    name: "refund".into(),
                },
            ));
            for (enrollment, version, input) in payloads {
                let payload = StepJobPayload {
                    workspace: env.ws,
                    enrollment,
                    state_version: version,
                    input,
                };
                let key = format!("test:{}", Uuid::now_v7());
                zirv_flow_store::enqueue(&env.pool, &payload.job(&before.trace, None, key))
                    .await
                    .unwrap();
            }
            env.advance(10_000);

            let ran = env.run_due().await;

            assert_eq!(ran, 5);
            assert_eq!(env.enrollment(id).await, before);
            assert_eq!(
                env.count("SELECT count(*) FROM decision_records").await,
                decisions
            );
            assert_eq!(
                env.count("SELECT count(*) FROM step_executions").await,
                steps
            );
            assert_eq!(env.count("SELECT count(*) FROM jobs").await, 0);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_step_that_keeps_failing_is_retried_and_ends_failed_with_its_error_visible(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            const MESSAGE: &str = "an increment needs a trait whose value is a number";
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "bump", "type": "action.update_trait", "key": "plan",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "bump", "next", "end")]});
            let flow = env.flow("Failing", graph).await;
            let enrolled = env.enroll(flow, 1, &[ada]).await;
            let id = enrolled.enrolled[0].enrollment;
            sqlx::query("UPDATE jobs SET max_attempts = 2 WHERE kind = 'flow.step'")
                .execute(&env.pool)
                .await
                .unwrap();

            let first = env.claim().await.unwrap();
            let first = env.execute(first).await;
            let after_first = env.enrollment(id).await;
            let steps_after_first = env.count("SELECT count(*) FROM step_executions").await;
            let second = env.claim().await.unwrap();
            let attempts = second.attempts;
            let second = env.execute(second).await;

            assert_eq!(first.unwrap_err().message(), MESSAGE);
            assert_eq!(
                (after_first.status, after_first.state_version),
                (EnrollmentStatus::Active, 0)
            );
            assert_eq!(steps_after_first, 0, "the first attempt left nothing");
            assert_eq!(attempts, 2);
            assert!(
                second.is_ok(),
                "the last attempt records the failure: {second:?}"
            );
            let failed = env.enrollment(id).await;
            assert_eq!(failed.status, EnrollmentStatus::Failed);
            assert_eq!(failed.node, "bump", "the node the step started from");
            assert_eq!(failed.failed_node.as_deref(), Some("bump"));
            assert_eq!(failed.state_version, 0, "the version the job started from");
            assert_eq!(failed.error.as_deref(), Some(MESSAGE));
            assert_eq!(env.count("SELECT count(*) FROM step_executions").await, 0);
            let decisions = env.decisions(&enrolled.enrolled[0].trace).await;
            assert!(decisions.contains(&("flow.enrollment".into(), "failed".into())));
            assert_eq!(
                env.count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                    .await,
                0
            );
            let again = env.enroll(flow, 1, &[ada]).await;
            assert_eq!(again.skipped, [(ada, "already_active")]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_late_in_a_chain_keeps_the_state_the_step_started_from(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            env.define_trait("tier", TraitType::String).await;
            let list = env.list("Golden").await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let graph = json!({"v": 1, "nodes": [
                {"id": "a", "type": "action.update_trait", "key": "tier",
                 "change": {"op": "set", "value": "gold"}},
                {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
                {"id": "inc", "type": "action.update_trait", "key": "plan",
                 "change": {"op": "increment", "by": 1.0}},
                {"id": "end", "type": "exit"}
            ], "edges": [
                edge("e1", "a", "next", "l"),
                edge("e2", "l", "next", "inc"),
                edge("e3", "inc", "next", "end")
            ]});
            let flow = env.flow("Chain", graph).await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;
            sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'flow.step'")
                .execute(&env.pool)
                .await
                .unwrap();

            env.run_due().await;

            let failed = env.enrollment(id).await;
            assert_eq!(failed.status, EnrollmentStatus::Failed);
            assert_eq!(failed.node, "a", "the step started at the entry node");
            assert_eq!(failed.failed_node.as_deref(), Some("inc"));
            assert_eq!(failed.state_version, 0);
            assert_eq!(env.count("SELECT count(*) FROM step_executions").await, 0);
            assert_eq!(env.count("SELECT count(*) FROM list_members").await, 0);
            assert_eq!(env.traits(ada).await["tier"], Value::Null);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_timer_that_fires_before_this_clock_reaches_it_is_deferred_not_failed_or_lost(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "end")]});
            let flow = env.flow("Early", graph).await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;
            env.run_due().await;
            let asleep = env.enrollment(id).await;
            sqlx::query("UPDATE jobs SET run_at = now() - interval '1 second', max_attempts = 1")
                .execute(&env.pool)
                .await
                .unwrap();

            let early = env.claim().await.unwrap();
            let early = env.execute(early).await;

            assert!(early.is_ok(), "an early wake ends cleanly");
            assert_eq!(env.enrollment(id).await, asleep, "nothing moved");
            let (jobs, attempts, due): (i64, i32, OffsetDateTime) = sqlx::query_as(
                "SELECT count(*), max(attempts), min(run_at) FROM jobs WHERE kind = 'flow.step'",
            )
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(
                (jobs, attempts),
                (1, 0),
                "a fresh wake replaces the old one"
            );
            assert!(due >= asleep.wake_at.unwrap());
            env.advance(61);
            env.run_due().await;
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_decisions_of_a_three_node_path_are_complete_and_on_the_enrollments_trace(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let list = env.list(PERSONAL_DATA.list_name).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!(PERSONAL_DATA.traits()))
                .await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 30},
            {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "l"), edge("e2", "l", "next", "end")]});
            let flow = env.flow("Three", graph).await;
            let enrolled = env.enroll(flow, 1, &[ada]).await;
            let trace = enrolled.enrolled[0].trace.clone();

            env.run_due().await;
            env.advance(31);
            env.run_due().await;

            let expected: Vec<(String, String)> = [
                ("flow.enroll", "enrolled"),
                ("flow.enrollment", "completed"),
                ("flow.step", "done"),
                ("flow.step", "done"),
                ("flow.step", "waiting"),
                ("flow.wait", "waiting"),
                ("flow.wake", "timer"),
                ("list.membership", "added"),
            ]
            .into_iter()
            .map(|(action, outcome)| (action.to_owned(), outcome.to_owned()))
            .collect();
            assert_eq!(env.decisions(&trace).await, expected);
            let causes: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM decision_records WHERE trace_id = $1 AND cause_kind = 'api'",
            )
            .bind(trace.trace_id.as_uuid())
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(
                causes, 8,
                "every decision carries the cause that enrolled the person"
            );
            let linked: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM decision_records WHERE trace_id = $1 \
             AND action LIKE 'flow.%' AND subject_ids ? 'enrollment' AND subject_ids ? 'person'",
            )
            .bind(trace.trace_id.as_uuid())
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(linked, 7);
        })
        .await;
    }

    fn webhook_graph() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "b", "type": "branch",
             "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
            {"id": "hook", "type": "action.webhook",
             "url": "https://hooks.example.com/pro", "trait_keys": ["plan"]},
            {"id": "end", "type": "exit"},
            {"id": "other", "type": "exit"}
        ], "edges": [
            edge("e1", "b", "case_0", "hook"),
            edge("e2", "b", "else", "other"),
            edge("e3", "hook", "next", "end")
        ]})
    }

    /// The reason of the step decision of the webhook node, and the enqueued jobs.
    async fn webhook_outcome(
        env: &Env,
        enrollment: Uuid,
    ) -> (Option<String>, Vec<(String, Value)>) {
        let reason: Option<String> = sqlx::query_scalar(
            "SELECT reason FROM decision_records WHERE action = 'flow.step' \
             AND outcome = 'enqueued' AND subject_ids ->> 'enrollment' = $1",
        )
        .bind(enrollment.to_string())
        .fetch_optional(&env.pool)
        .await
        .unwrap()
        .flatten();
        let jobs =
            sqlx::query_as("SELECT unique_key, payload FROM jobs WHERE kind = 'webhook.call'")
                .fetch_all(&env.pool)
                .await
                .unwrap();
        (reason, jobs)
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_step_enqueues_its_job_under_the_stable_key_and_records_fresh_inputs(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let flow = env.flow("Hook", webhook_graph()).await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;

            env.run_due().await;

            let (reason, jobs) = webhook_outcome(&env, id).await;
            assert_eq!(reason.as_deref(), Some("inputs_fresh"));
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].0, format!("{id}:hook:webhook"));
            assert_eq!(jobs[0].1["url"], "https://hooks.example.com/pro");
            assert_eq!(jobs[0].1["node_id"], "hook");
            assert_eq!(jobs[0].1["person_id"], ada.to_string());
            assert_eq!(jobs[0].1["idempotency_key"], format!("{id}:hook:webhook"));
            assert_eq!(jobs[0].1["trait_keys"], json!(["plan"]));
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sync_step_enqueues_its_job_under_the_stable_key_with_ids_and_keys_only(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "SENTINEL"}))
                .await;
            let destination = Uuid::from_u128(500);
            let flow = env
                .flow(
                    "Sync",
                    json!({"v": 1, "nodes": [
                        {"id": "push", "type": "action.sync", "destination": destination,
                         "operation": "upsert", "trait_keys": ["plan"]},
                        {"id": "drop", "type": "action.sync", "destination": destination,
                         "operation": "remove"},
                        {"id": "end", "type": "exit"}
                    ], "edges": [
                        edge("e1", "push", "next", "drop"),
                        edge("e2", "drop", "next", "end")
                    ]}),
                )
                .await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;

            env.run_due().await;

            let mut jobs: Vec<(String, Value)> = sqlx::query_as(
            "SELECT unique_key, payload FROM jobs WHERE kind = 'delivery.sync' ORDER BY unique_key",
        )
        .fetch_all(&env.pool)
        .await
        .unwrap();
            assert_eq!(jobs.len(), 2);
            let (drop_key, drop_payload) = jobs.remove(0);
            let (push_key, push_payload) = jobs.remove(0);
            assert_eq!(drop_key, format!("{id}:drop:sync"));
            assert_eq!(push_key, format!("{id}:push:sync"));
            assert_eq!(push_payload["operation"], "upsert");
            assert_eq!(push_payload["trait_keys"], json!(["plan"]));
            assert_eq!(push_payload["destination_id"], destination.to_string());
            assert_eq!(push_payload["idempotency_key"], push_key);
            assert_eq!(push_payload["enrollment_id"], id.to_string());
            assert_eq!(drop_payload["operation"], "remove");
            assert_eq!(drop_payload["trait_keys"], json!([]));
            assert_ne!(push_payload["sync_id"], drop_payload["sync_id"]);
            let queued = format!("{push_payload}{drop_payload}");
            assert!(
                !queued.contains("SENTINEL") && !queued.contains("ada@"),
                "{queued}"
            );
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_webhook_step_records_that_an_input_is_failing_and_still_runs(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let source = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO sources (id, workspace_id, name, kind, config, enabled) \
             VALUES ($1, $2, 'CRM', 'postgres', '{}', true)",
            )
            .bind(source)
            .bind(env.ws.as_uuid())
            .execute(&env.pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO streams (id, workspace_id, source_id, name, enabled, sync_mode, \
                mapping, discovered, enabled_at, last_success_at, last_run_failed) \
             VALUES ($1, $2, $3, 'contacts', true, 'full_refresh', $4, '{}', $5, $5, true)",
            )
            .bind(Uuid::now_v7())
            .bind(env.ws.as_uuid())
            .bind(source)
            .bind(json!({"columns": [
                {"column": "e", "target": {"type": "identifier", "kind": "email"}},
                {"column": "p", "target": {"type": "trait", "key": "plan"}}
            ]}))
            .bind(env.clock.now())
            .execute(&env.pool)
            .await
            .unwrap();
            let flow = env.flow("Hook", webhook_graph()).await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;

            env.run_due().await;

            let (reason, jobs) = webhook_outcome(&env, id).await;
            assert_eq!(reason.as_deref(), Some("inputs_failing"));
            assert_eq!(jobs.len(), 1, "a failing input never pauses the flow");
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wait_until_a_time_falls_back_to_the_workspace_zone_and_says_so(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            sqlx::query("UPDATE people SET traits = '{\"timezone\": \"Not/AZone\"}' WHERE id = $1")
                .bind(ada)
                .execute(&env.pool)
                .await
                .unwrap();
            let graph = json!({"v": 1, "nodes": [
            {"id": "until", "type": "wait.until_time",
             "when": {"kind": "date_time", "at": "2999-01-01T09:00:00"}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "until", "next", "end")]});
            let flow = env.flow("Zones", graph).await;
            let id = env.enroll(flow, 1, &[ada]).await.enrolled[0].enrollment;

            env.run_due().await;

            let reason: String = sqlx::query_scalar(
                "SELECT reason FROM decision_records WHERE action = 'flow.step' \
             AND outcome = 'waiting' AND subject_ids ->> 'enrollment' = $1",
            )
            .bind(id.to_string())
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(reason, "timezone_fallback");
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Waiting);
        })
        .await;
    }

    /// The subject ids of every flow decision on `trace`, with the action and outcome.
    async fn flow_subjects(env: &Env, trace: &TraceContext) -> Vec<(String, String, Value)> {
        sqlx::query_as(
            "SELECT action, outcome, subject_ids FROM decision_records \
             WHERE workspace_id = $1 AND trace_id = $2 AND action LIKE 'flow.%' \
             ORDER BY recorded_at, id",
        )
        .bind(env.ws.as_uuid())
        .bind(trace.trace_id.as_uuid())
        .fetch_all(&env.pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn every_flow_decision_of_a_path_names_the_flow_version_node_enrollment_and_person(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let list = env.list(PERSONAL_DATA.list_name).await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 30},
            {"id": "l", "type": "action.list", "list_id": list, "change": "add"},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "l"), edge("e2", "l", "next", "end")]});
            let flow = env.flow("Three", graph).await;
            let enrolled = env.enroll(flow, 1, &[ada]).await;
            let id = enrolled.enrolled[0].enrollment;
            let trace = enrolled.enrolled[0].trace.clone();
            env.run_due().await;
            env.advance(31);
            env.run_due().await;

            let decisions = flow_subjects(&env, &trace).await;

            assert!(decisions.len() >= 7, "{decisions:?}");
            for (action, outcome, subjects) in &decisions {
                let label = format!("{action}/{outcome}: {subjects}");
                assert_eq!(subjects["flow"], json!(flow.to_string()), "{label}");
                assert_eq!(subjects["version"], json!(1), "{label}");
                assert_eq!(subjects["enrollment"], json!(id.to_string()), "{label}");
                assert_eq!(subjects["person"], json!(ada.to_string()), "{label}");
                if action == "flow.enroll" {
                    assert!(subjects.get("node").is_none(), "{label}");
                } else {
                    assert!(subjects["node"].is_string(), "{label}");
                }
            }
            let node_of = |action: &str, outcome: &str| -> Vec<String> {
                decisions
                    .iter()
                    .filter(|(a, o, _)| a == action && o == outcome)
                    .map(|(_, _, subjects)| subjects["node"].as_str().unwrap().to_owned())
                    .collect()
            };
            assert_eq!(node_of("flow.wait", "waiting"), ["w"]);
            assert_eq!(node_of("flow.wake", "timer"), ["w"]);
            assert_eq!(node_of("flow.enrollment", "completed"), ["end"]);
            let mut done = node_of("flow.step", "done");
            done.sort();
            assert_eq!(done, ["end", "l"]);
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_branch_decision_names_the_branch_node(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "pick", "type": "branch",
             "conditions": [{"trait": "plan", "op": "eq", "value": "pro"}]},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "pick", "case_0", "end"), edge("e2", "pick", "else", "end")]});
            let flow = env.flow("Branching", graph).await;
            let trace = env.enroll(flow, 1, &[ada]).await.enrolled[0].trace.clone();

            env.run_due().await;

            let decisions = flow_subjects(&env, &trace).await;
            let branches: Vec<_> = decisions
                .iter()
                .filter(|(action, _, _)| action.starts_with("flow.branch"))
                .collect();
            assert!(!branches.is_empty(), "{decisions:?}");
            for (action, _, subjects) in branches {
                assert_eq!(subjects["node"], "pick", "{action}");
                assert_eq!(subjects["version"], json!(1), "{action}");
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_triggered_enrollment_that_fails_is_retried_and_skipped_on_one_trace(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            const MESSAGE: &str = "an increment needs a trait whose value is a number";
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let graph = json!({"v": 1, "nodes": [
            {"id": "bump", "type": "action.update_trait", "key": "plan",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "bump", "next", "end")]});
            let flow = env.flow("Failing", graph).await;
            // The segment change that triggers the enrollment: its cause is the upstream event.
            let change = Cause::event(Uuid::now_v7());
            let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
            let enrolled = zirv_flow_store::enroll(
                &mut tx,
                &EnrollRequest {
                    workspace: env.ws,
                    flow,
                    version: 1,
                    people: &[ada],
                    occurrence: Occurrence {
                        trigger: "segment",
                        id: Uuid::now_v7(),
                    },
                    now: env.clock.now(),
                    trace: &TraceContext::root(change.clone()),
                },
            )
            .await
            .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            let id = enrolled.enrolled[0].enrollment;
            let trace = enrolled.enrolled[0].trace.clone();
            let one_attempt = || async {
                sqlx::query("UPDATE jobs SET max_attempts = 1 WHERE kind = 'flow.step'")
                    .execute(&env.pool)
                    .await
                    .unwrap();
            };
            one_attempt().await;
            env.run_due().await;
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Failed);
            assert_eq!(env.enrollment(id).await.error.as_deref(), Some(MESSAGE));

            let operator = TraceContext::root(Cause::manual(zirv_flow_core::UserId::from_uuid(
                Uuid::new_v4(),
            )));
            let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
            let retried =
                zirv_flow_store::resume_failed(&mut tx, env.ws, flow, id, false, &operator)
                    .await
                    .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            one_attempt().await;
            env.run_due().await;
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Failed);
            let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
            let skipped =
                zirv_flow_store::resume_failed(&mut tx, env.ws, flow, id, true, &operator)
                    .await
                    .unwrap();
            zirv_flow_store::commit(tx).await.unwrap();
            one_attempt().await;
            env.run_due().await;

            assert_eq!(
                retried, trace.trace_id,
                "the retry is on the enrollment's trace"
            );
            assert_eq!(skipped, trace.trace_id, "so is the skip");
            assert_eq!(env.enrollment(id).await.status, EnrollmentStatus::Completed);
            // Nothing of the enrollment is on any other trace.
            let elsewhere: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM decision_records \
             WHERE subject_ids ->> 'enrollment' = $1 AND trace_id <> $2",
            )
            .bind(id.to_string())
            .bind(trace.trace_id.as_uuid())
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(elsewhere, 0);
            // The trace read, as the trace endpoint reads it.
            let (records, truncated) =
                zirv_flow_store::list_trace(&env.pool, env.ws, trace.trace_id, 1000)
                    .await
                    .unwrap();
            assert!(!truncated);
            let flow_records: Vec<_> = records
                .iter()
                .filter(|record| record.action.starts_with("flow."))
                .collect();
            for record in &flow_records {
                for key in ["flow", "version", "enrollment", "person"] {
                    assert!(
                        record.subject_ids.get(key).is_some(),
                        "{} {} lacks {key}",
                        record.action,
                        record.outcome
                    );
                }
            }
            let of = |outcome: &str| -> Vec<_> {
                flow_records
                    .iter()
                    .filter(|record| {
                        record.action == "flow.enrollment" && record.outcome == outcome
                    })
                    .collect()
            };
            let failures = of("failed");
            assert_eq!(
                failures.len(),
                2,
                "the step failed, and failed again on the retry"
            );
            for failure in failures {
                assert_eq!(failure.reason.as_deref(), Some("step_failed"));
                assert_eq!(failure.subject_ids["node"], "bump");
            }
            for outcome in ["retried", "skipped"] {
                let records = of(outcome);
                assert_eq!(records.len(), 1, "{outcome}");
                assert_eq!(records[0].reason.as_deref(), Some("operator"));
                assert_eq!(records[0].subject_ids["node"], "bump");
                assert_eq!(
                    records[0].cause, operator.cause,
                    "the operator is the cause"
                );
            }
            assert!(
                flow_records
                    .iter()
                    .any(|r| r.cause == Cause::from_stored(change.kind, change.id.clone()))
            );
            // The enrollment's path carries the same decisions.
            let path = zirv_flow_store::load_enrollment_path(&env.pool, env.ws, flow, id)
                .await
                .unwrap()
                .unwrap();
            let on_trace: Vec<Uuid> = flow_records.iter().map(|r| r.id).collect();
            let on_path: Vec<Uuid> = path
                .decisions
                .iter()
                .filter(|r| r.action.starts_with("flow."))
                .map(|r| r.id)
                .collect();
            assert_eq!(on_path, on_trace);
            assert_eq!(path.enrollment.trace_id, trace.trace_id.as_uuid());
            // Both reads agree, but they share an ORDER BY, so check the order itself: it is the
            // order the walk wrote the records in, which the ids follow.
            let mut by_id = on_path.clone();
            by_id.sort();
            assert_eq!(on_path, by_id, "the path is in the order the walk wrote");
            let walk: Vec<&str> = path
                .decisions
                .iter()
                .filter(|r| r.action == "flow.enrollment" && r.outcome != "enrolled")
                .map(|r| r.outcome.as_str())
                .collect();
            assert_eq!(
                walk,
                ["failed", "retried", "failed", "skipped", "completed"]
            );
            // The person's timeline shows the same flow decisions, each with the version and node
            // the walk wrote.
            let timeline = zirv_flow_store::load_timeline(
                &env.pool,
                env.ws,
                ada,
                zirv_flow_store::TimelineFilter::default(),
                100,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            let mut on_timeline: Vec<(Uuid, Option<i64>, Option<String>)> = timeline
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    zirv_flow_store::TimelineEntry::Decision(d)
                        if d.action.starts_with("flow.") =>
                    {
                        Some((
                            d.id,
                            d.subject_ids["version"].as_i64(),
                            d.subject_ids["node"].as_str().map(str::to_owned),
                        ))
                    }
                    _ => None,
                })
                .collect();
            on_timeline.sort();
            let mut written: Vec<(Uuid, Option<i64>, Option<String>)> = flow_records
                .iter()
                .map(|r| {
                    (
                        r.id,
                        r.subject_ids["version"].as_i64(),
                        r.subject_ids["node"].as_str().map(str::to_owned),
                    )
                })
                .collect();
            written.sort();
            assert_eq!(on_timeline, written);
            // Each carries the enrollment's trace and names the enrollment.
            let timeline_flow: Vec<_> = timeline
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    zirv_flow_store::TimelineEntry::Decision(d)
                        if d.action.starts_with("flow.") =>
                    {
                        Some(d)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(timeline_flow.len(), flow_records.len());
            for d in timeline_flow {
                assert_eq!(
                    d.trace_id,
                    trace.trace_id.as_uuid(),
                    "{} {} is on the enrollment's trace",
                    d.action,
                    d.outcome
                );
                assert_eq!(
                    d.subject_ids["enrollment"].as_str(),
                    Some(id.to_string().as_str()),
                    "{} {} names the enrollment",
                    d.action,
                    d.outcome
                );
            }
            assert!(
                on_timeline
                    .iter()
                    .all(|(_, version, _)| *version == Some(1)),
                "every flow decision carries the pinned version"
            );
            assert!(
                on_timeline
                    .iter()
                    .any(|(_, _, node)| node.as_deref() == Some("bump")),
                "step decisions carry their node"
            );
        })
        .await;
    }
}
