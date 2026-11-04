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
    ClaimedJob, Clock, DecisionBuffer, EffectError, Enrollment, EnrollmentRead, EnrollmentStatus,
    EnrollmentUpdate, FlowState, FlowTraitWrite, GraphPoint, NewDecision, NewJob, NewStepExecution,
    NewWait, SEND_JOB_KIND, STEP_JOB_KIND, SYNC_JOB_KIND, StepEnrollment, StepJobInput,
    StepJobPayload, StepOutcome, StepRecord, StoreError, StoredEvent, WEBHOOK_JOB_KIND,
    apply_flow_list, apply_flow_trait, clear_waits, enqueue, evaluate_conditions,
    find_event_of_person, find_flow_version, flow_state_for_step, flow_states_for_steps,
    hold_enrollment, lock_read_enrollment, read_enrollment, read_enrollments, record_steps,
    register_wait, release_enrollment, reopen_failed_step, save_enrollment, segment_catalog,
    settle_wait_step, unhealthy_inputs, wait_filters,
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
    const NEEDS_CHANGE_READERS: bool = true;

    async fn run(&self, ctx: &mut JobContext<'_>, payload: Value) -> Result<(), JobError> {
        let job = StepJobPayload::parse(&payload)
            .ok_or_else(|| JobError::new("the payload is not a flow step"))?;
        if ctx.workspace_id() != Some(job.workspace) {
            return Err(JobError::new(
                "the payload names another workspace than the job",
            ));
        }
        let clock = ctx.clock().clone();
        let attempt = (ctx.job_id(), ctx.attempts());
        let last_attempt = ctx.attempts() >= ctx.max_attempts();
        let wakes = ctx.take_wakes();
        // The worker took the person-change guard (`NEEDS_CHANGE_READERS`) before the lease, in
        // this transaction, for the workspace checked above.
        let conn = ctx.conn();
        let Some(batch) = wakes else {
            let now = clock.now(conn).await?;
            let Some(read) = read_enrollment(conn, job.workspace, job.enrollment).await? else {
                // Erased with the person: nothing is left to advance.
                return Ok(());
            };
            // After the segment guard, the flow's state is read and held until commit, so a
            // resume either sees the enrollment this step holds or the step sees the flow resumed.
            // The flow, then the person, then the enrollment: the order a resume locks them in.
            let flow_state = flow_state_for_step(conn, job.workspace, read.flow_id).await?;
            let run = StepRun {
                job: &job,
                at: attempt,
                last_attempt,
                now,
                alone: true,
            };
            let mut decisions = DecisionBuffer::default();
            step(conn, &run, read, flow_state, None, &mut decisions).await?;
            decisions.flush(conn).await?;
            return Ok(());
        };
        // A batch read the clock and every enrollment of its wakes before it locked anything
        // (#397). It locked the flows of all its wakes before any of them locked a person; no
        // pause, resume or archive can commit while it holds them, so the state it read is the one
        // at its commit. It appends the decisions of all its wakes before it commits.
        let Some(read) = batch.reads.get(&job.enrollment).cloned() else {
            return Ok(());
        };
        let flow_state = batch.flows.get(&read.flow_id).copied();
        let run = StepRun {
            job: &job,
            at: attempt,
            last_attempt,
            now: batch.now,
            alone: false,
        };
        step(
            conn,
            &run,
            read,
            flow_state,
            Some(&mut batch.graphs),
            &mut batch.decisions,
        )
        .await
    }
}

/// One attempt of a `flow.step` job, as its steps see it.
struct StepRun<'a> {
    job: &'a StepJobPayload,
    /// The job and its attempt, for the decisions it makes.
    at: (Uuid, i32),
    last_attempt: bool,
    /// The time the step runs at: its own read of the clock, or its batch's (#397).
    now: OffsetDateTime,
    /// Whether the job runs in a transaction of its own, which appends the decisions of its walk
    /// before the walk's savepoint is released; a batch appends them all before it commits.
    alone: bool,
}

/// Locks the enrollment `read` found and advances it as the job asks, or holds, releases or
/// defers it. Every decision goes to `decisions`, for the caller to append.
async fn step(
    conn: &mut PgConnection,
    run: &StepRun<'_>,
    read: EnrollmentRead,
    flow_state: Option<FlowState>,
    graphs: Option<&mut PinnedGraphs>,
    decisions: &mut DecisionBuffer,
) -> Result<(), JobError> {
    let (job, now) = (run.job, run.now);
    let Some(StepEnrollment {
        mut enrollment,
        workspace_zone,
        person_zone,
    }) = lock_read_enrollment(conn, job.workspace, job.enrollment, read).await?
    else {
        return Ok(());
    };
    if is_stale(&enrollment, job) {
        return Ok(());
    }
    match (flow_state, &job.input) {
        // An archived flow changes no more; its archive job exits what is in flight.
        (Some(FlowState::Archived) | None, _) => return Ok(()),
        // An operator's retry or skip is deliberate, whatever the flow is doing.
        (_, StepJobInput::Resume { .. }) => {}
        (Some(FlowState::Paused), _) => {
            return hold(conn, job, &enrollment, run.at, decisions).await;
        }
        // A held enrollment of a flow that is no longer paused, which its resume batch has not
        // reached yet, takes this job at once: an event must not lose to the stored timer.
        _ if enrollment.status == EnrollmentStatus::Held => {
            match release(conn, job, enrollment, decisions).await? {
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
            job.enrollment, job.state_version, run.at.0
        );
        let run_at = due.max(now + time::Duration::seconds(5));
        enqueue(conn, &job.job(&enrollment.trace, Some(run_at), key)).await?;
        return Ok(());
    }

    if let (StepJobInput::Wake, Some(due)) = (&job.input, enrollment.wake_at)
        && run.at.1 == 1
    {
        record_wake_lateness((now - due).as_seconds_f64());
    }

    let mark = decisions.mark();
    let mut savepoint = conn.begin().await?;
    let zones = (workspace_zone, person_zone);
    let walked = advance(&mut savepoint, run, &enrollment, zones, graphs, decisions).await;
    match walked {
        Ok(()) => {
            savepoint.commit().await?;
            Ok(())
        }
        Err(failure) => {
            savepoint.rollback().await?;
            // The walk's decisions go with its writes.
            decisions.discard_after(mark);
            if run.last_attempt {
                fail_enrollment(conn, run, &enrollment, &failure, decisions).await
            } else {
                Err(failure.error)
            }
        }
    }
}

/// What the `flow.step` jobs of one workspace that run in one transaction share (#397).
#[derive(Debug)]
pub(crate) struct WakeBatch {
    /// The time every wake of the batch runs at: one read of the clock, after the guard and before
    /// any lock. The claim found each wake due by then, so none runs early.
    now: OffsetDateTime,
    /// Each job's enrollment as one statement read it before any lock; one it did not find is
    /// absent.
    reads: HashMap<Uuid, EnrollmentRead>,
    /// The state of each of their flows, read under the lock the transaction holds until it ends;
    /// a flow that is gone is absent.
    flows: HashMap<Uuid, FlowState>,
    /// The pinned versions read so far.
    graphs: PinnedGraphs,
    /// The decisions of the wakes kept so far, in the order they were made, appended in one
    /// statement before the batch commits.
    decisions: DecisionBuffer,
}

impl WakeBatch {
    /// Where the batch's decisions stand before a wake, for [`WakeBatch::discard_decisions`].
    pub(crate) fn decisions_mark(&self) -> usize {
        self.decisions.mark()
    }

    /// Drops the decisions made since `mark`, for a wake whose savepoint rolled back.
    pub(crate) fn discard_decisions(&mut self, mark: usize) {
        self.decisions.discard_after(mark);
    }

    /// Appends the decisions of every wake kept, in one statement, in the order they were made.
    pub(crate) async fn flush_decisions(
        &mut self,
        conn: &mut PgConnection,
    ) -> Result<(), StoreError> {
        self.decisions.flush(conn).await
    }

    /// Where `job` comes in the order the batch runs its jobs in, the order their steps lock
    /// people in: by the lowest id of the group the step locks first, then by its person, the id
    /// order that merges, erasures and the lifecycle batches lock people in. A job whose
    /// enrollment was not found locks no person and sorts first (`None`); a stable sort keeps the
    /// claim's order between jobs that tie.
    pub(crate) fn lock_order(&self, job: &ClaimedJob) -> Option<(Uuid, Uuid)> {
        enrollment_of(job)
            .and_then(|enrollment| self.reads.get(&enrollment))
            .map(EnrollmentRead::lock_order)
    }
}

fn enrollment_of(job: &ClaimedJob) -> Option<Uuid> {
    StepJobPayload::parse(&job.payload).map(|payload| payload.enrollment)
}

/// Prepares the transaction that runs `jobs`, `flow.step` jobs of `workspace`, after its
/// person-change guard (#397). It reads `clock` once for all of them; one statement reads every
/// job's enrollment, and one locks all their flows `FOR KEY SHARE` in id order, before any job
/// locks a person; the transaction holds them until it ends. The caller runs the jobs in
/// [`WakeBatch::lock_order`] and appends their decisions with [`WakeBatch::flush_decisions`].
pub(crate) async fn plan_wakes(
    conn: &mut PgConnection,
    clock: &Clock,
    workspace: WorkspaceId,
    jobs: &[ClaimedJob],
) -> Result<WakeBatch, StoreError> {
    let now = clock.now(conn).await?;
    let enrollments: Vec<Uuid> = jobs.iter().filter_map(enrollment_of).collect();
    let reads: HashMap<Uuid, EnrollmentRead> = read_enrollments(conn, workspace, &enrollments)
        .await?
        .into_iter()
        .collect();
    // The statement orders and deduplicates the flows itself.
    let flows: Vec<Uuid> = reads.values().map(|read| read.flow_id).collect();
    let flows = flow_states_for_steps(conn, workspace, &flows)
        .await?
        .into_iter()
        .collect();
    Ok(WakeBatch {
        now,
        reads,
        flows,
        graphs: PinnedGraphs::new(),
        decisions: DecisionBuffer::default(),
    })
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
    decisions: &mut DecisionBuffer,
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
    decisions.push(NewDecision {
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
    });
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
    decisions: &mut DecisionBuffer,
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
        decisions.push(NewDecision {
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
        });
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
    /// Where the decisions of the walk go, in the order they were made: the job's own, appended
    /// in one statement when the walk ends, or its batch's (#397).
    decisions: &'a mut DecisionBuffer,
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
                admit_at: None,
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

/// The pinned versions a batch has read, by flow and version.
type PinnedGraphs = HashMap<(Uuid, i32), FlowGraph>;

/// The graph of the version an enrollment is pinned to. A version row never changes: the store
/// refuses an update, and a delete while its flow exists, and the step holds its flow
/// `FOR KEY SHARE`. A publish adds a version under another number. So a batch reads each pinned
/// version once and its later wakes take that graph from `graphs`.
async fn pinned_graph(
    conn: &mut PgConnection,
    graphs: Option<&mut PinnedGraphs>,
    workspace: WorkspaceId,
    enrollment: &Enrollment,
) -> Result<FlowGraph, JobError> {
    let key = (enrollment.flow_id, enrollment.flow_version);
    if let Some(graph) = graphs.as_deref().and_then(|graphs| graphs.get(&key)) {
        return Ok(graph.clone());
    }
    let version = find_flow_version(conn, workspace, key.0, key.1)
        .await?
        .ok_or_else(|| JobError::new("the pinned flow version is missing"))?;
    let graph = FlowGraph::from_value(version.graph)
        .map_err(|_| JobError::new("the pinned flow version does not parse"))?;
    if let Some(graphs) = graphs {
        graphs.insert(key, graph.clone());
    }
    Ok(graph)
}

/// Walks the enrollment from the job's input to the next wait, exit or failure. `zones` are the
/// workspace's zone and the person's `timezone` trait, as the enrollment's lock read them. The
/// walk's decisions go to `decisions`; a job alone appends them here, in the walk's savepoint.
async fn advance(
    conn: &mut PgConnection,
    run: &StepRun<'_>,
    enrollment: &Enrollment,
    (workspace_zone, person_zone): (String, Option<String>),
    graphs: Option<&mut PinnedGraphs>,
    decisions: &mut DecisionBuffer,
) -> Result<(), StepFailure> {
    let job = run.job;
    let graph = pinned_graph(&mut *conn, graphs, job.workspace, enrollment).await?;
    let zones = Zones {
        now: to_chrono(run.now).ok_or_else(|| JobError::new("the time does not fit the engine"))?,
        workspace: workspace_zone.parse().unwrap_or(Tz::UTC),
        person: person_zone,
    };
    let mut walk = Walk {
        workspace: job.workspace,
        enrollment,
        graph,
        zones,
        now: run.now,
        job: run.at,
        catalog: None,
        freshness: None,
        steps: HashMap::new(),
        decisions,
    };

    walk_steps(conn, &mut walk, job).await?;
    if run.alone {
        walk.decisions.flush(conn).await?;
    }
    Ok(())
}

/// The nodes of [`advance`], from the first input to where the walk ends.
async fn walk_steps(
    conn: &mut PgConnection,
    walk: &mut Walk<'_>,
    job: &StepJobPayload,
) -> Result<(), StepFailure> {
    let Some(mut input) = first_input(conn, walk, job).await? else {
        return Ok(());
    };
    let mut state = engine_state(walk.enrollment)?;
    let mut first = true;
    loop {
        let transition = engine_step(&walk.graph, &state, input, &walk.zones)
            .map_err(|_| JobError::new("the flow engine refused the step"))?;
        if transition.outcome == Outcome::Stale {
            return Ok(());
        }
        if first {
            wake(conn, walk, job).await?;
            first = false;
        }
        apply(conn, walk, &transition).await?;
        match &transition.outcome {
            Outcome::NeedsConditions { node, conditions } => {
                let results = branch(conn, walk, node, conditions).await?;
                input = StepInput::ConditionsEvaluated {
                    node: node.clone(),
                    results,
                };
                state = transition.state;
            }
            _ => return finish(conn, walk, &transition).await,
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
    walk.decisions.push(decision);
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
    walk.decisions.extend(decisions);

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
    walk.decisions.extend(decisions);
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
    walk.decisions.push(decision);
    Ok(())
}

/// The last attempt failed. The savepoint already took the step's effects and rows back, so the
/// enrollment keeps the state the job started from (its node, waits, timer and version) and is
/// marked `failed` with the node that failed and the error stored. No step row is written for
/// the failing node, so a retry runs the whole step again, effects included.
async fn fail_enrollment(
    conn: &mut PgConnection,
    run: &StepRun<'_>,
    enrollment: &Enrollment,
    failure: &StepFailure,
    decisions: &mut DecisionBuffer,
) -> Result<(), JobError> {
    let job = run.job;
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
            now: run.now,
        },
    )
    .await?;
    if !saved {
        return Err(JobError::new("the enrollment moved under its lock"));
    }
    decisions.push(NewDecision {
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
        job: Some(run.at),
    });
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

    /// The person-change guard the worker takes, before anything else in the job's transaction,
    /// for a kind that `needs` it.
    async fn guard(tx: &mut PgConnection, claimed: &ClaimedJob, needs: bool) {
        if needs && let Some(workspace) = claimed.workspace_id {
            zirv_flow_store::cache_change_readers(tx, WorkspaceId::from_uuid(workspace))
                .await
                .unwrap();
        }
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
                guard(&mut tx, &claimed, K::NEEDS_CHANGE_READERS).await;
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
            zirv_flow_store::resume_flow(
                &mut tx,
                &Clock::Manual(self.clock.clone()),
                self.ws,
                flow,
                &trace,
            )
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
            // The tests that use this are about the steps of a flow, not the ramp that admits people.
            zirv_flow_store::set_warmup(&mut tx, self.ws, id, false, 300)
                .await
                .unwrap();
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
            guard(&mut tx, &claimed, FlowStep::NEEDS_CHANGE_READERS).await;
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

            let step: Uuid = sqlx::query_scalar("SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = 'wait'")
                .bind(id).fetch_one(&env.pool).await.unwrap();
            let subjects = json!({"flow": flow, "enrollment": id, "person": ada, "version": 1, "node": "wait", "step_execution": step});
            zirv_flow_store::decision_testing::assert_fixture(&env.pool, env.ws, &done.trace,
                ("flow.wake", "event", None), subjects.clone()).await;
            zirv_flow_store::decision_testing::assert_fixture(&env.pool, env.ws, &done.trace,
                ("flow.wait", "waiting", Some("until_event")), subjects).await;
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

            let step: Uuid = sqlx::query_scalar("SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = 'wait'")
                .bind(id).fetch_one(&env.pool).await.unwrap();
            let subjects = json!({"flow": flow, "enrollment": id, "person": ada, "version": 1, "node": "wait", "step_execution": step});
            zirv_flow_store::decision_testing::assert_fixture(&env.pool, env.ws, &done.trace,
                ("flow.wake", "timeout", None), subjects.clone()).await;
            zirv_flow_store::decision_testing::assert_fixture(&env.pool, env.ws, &done.trace,
                ("flow.wait", "waiting", Some("until_event")), subjects).await;
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
    async fn a_wake_appends_its_decisions_in_one_statement_with_the_rows_it_always_wrote(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let ada = env.person(PERSONAL_DATA.email, json!({})).await;
            let graph = json!({"v": 1, "nodes": [
                {"id": "w1", "type": "wait.duration", "seconds": 60},
                {"id": "x1", "type": "exit"}
            ], "edges": [edge("e1", "w1", "next", "x1")]});
            let flow = env.flow("Timer", graph).await;
            let enrolled = env.enroll(flow, 1, &[ada]).await;
            let id = enrolled.enrolled[0].enrollment;
            let trace = enrolled.enrolled[0].trace.clone();
            env.run_due().await;
            env.advance(61);
            env.hold_others(Some("wake"), Some(id)).await;
            let wake = env.claim().await.unwrap();
            env.release().await;

            env.execute(wake.clone()).await.unwrap();

            let step_of = |node: &'static str| {
                let pool = env.pool.clone();
                async move {
                    sqlx::query_scalar::<_, Uuid>(
                        "SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = $2",
                    )
                    .bind(id)
                    .bind(node)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
                }
            };
            let (waited, exited) = (step_of("w1").await, step_of("x1").await);
            let row =
                |action: &str, outcome: &str, reason: Option<&str>, step: Uuid, node: &str| {
                    json!({"action": action, "outcome": outcome, "reason": reason, "attempt": 1,
                       "subjects": {"flow": flow, "enrollment": id, "person": ada,
                                    "step_execution": step, "version": 1, "node": node}})
                };
            let rows: Vec<Value> = sqlx::query_scalar(
                "SELECT jsonb_build_object('action', action, 'outcome', outcome, 'reason', reason, \
                     'attempt', attempt, 'subjects', subject_ids) \
                 FROM decision_records WHERE workspace_id = $1 AND trace_id = $2 AND job_id = $3 \
                 ORDER BY recorded_at, id",
            )
            .bind(env.ws.as_uuid())
            .bind(trace.trace_id.as_uuid())
            .bind(wake.id)
            .fetch_all(&env.pool)
            .await
            .unwrap();
            assert_eq!(
                rows,
                [
                    row("flow.wake", "timer", None, waited, "w1"),
                    row("flow.step", "done", Some("exit"), exited, "x1"),
                    row("flow.enrollment", "completed", None, exited, "x1"),
                ]
            );
            // Rows written by one statement share a command id.
            let statements: i64 = sqlx::query_scalar(
                "SELECT count(DISTINCT cmin::text) FROM decision_records WHERE job_id = $1",
            )
            .bind(wake.id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(
                statements, 1,
                "the wake appended its decisions in more than one insert"
            );
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

            let enrollment = env.enrollment(id).await;
            let step: Uuid = sqlx::query_scalar(
                "SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = 'until'",
            )
            .bind(id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            zirv_flow_store::decision_testing::assert_fixture(
                &env.pool,
                env.ws,
                &enrollment.trace,
                ("flow.wait", "waiting", Some("until_time")),
                json!({"flow": flow, "enrollment": id,
                    "person": ada, "version": 1, "node": "until", "step_execution": step}),
            )
            .await;
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

    async fn branch_fixture(pool: PgPool, selected: Option<usize>) {
        let env = Env::new(pool).await;
        env.define_trait("plan", TraitType::String).await;
        let person = env
            .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
            .await;
        let count = selected.map_or(10, |index| 10.max(index + 1));
        let conditions: Vec<_> = (0..count).map(|index| json!({
            "trait": "plan", "op": "eq", "value": if selected == Some(index) { "pro" } else { "free" }
        })).collect();
        let mut edges: Vec<_> = (0..count)
            .map(|index| {
                edge(
                    &format!("case-{index}"),
                    "pick",
                    &format!("case_{index}"),
                    "end",
                )
            })
            .collect();
        edges.push(edge("otherwise", "pick", "else", "end"));
        let flow = env
            .flow(
                "Branch fixture",
                json!({"v": 1, "nodes": [
            {"id": "pick", "type": "branch", "conditions": conditions},
            {"id": "end", "type": "exit"}
        ], "edges": edges}),
            )
            .await;
        let enrolled = env.enroll(flow, 1, &[person]).await;
        let enrollment = &enrolled.enrolled[0];
        env.run_due().await;
        let step: Uuid = sqlx::query_scalar(
            "SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = 'pick'",
        )
        .bind(enrollment.enrollment)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let subjects = json!({"flow": flow, "person": person,
            "enrollment": enrollment.enrollment, "version": 1, "node": "pick", "step_execution": step});
        let outcome = selected.map_or_else(
            || "else".to_owned(),
            |index| {
                if index >= 10 {
                    "case_more".to_owned()
                } else {
                    format!("case_{index}")
                }
            },
        );
        zirv_flow_store::decision_testing::assert_fixture(
            &env.pool,
            env.ws,
            &enrollment.trace,
            ("flow.branch", &outcome, None),
            subjects.clone(),
        )
        .await;
        for index in 0..count {
            let outcome = if selected == Some(index) {
                "matched"
            } else {
                "unmatched"
            };
            let reason = if index >= 10 {
                "case_more".to_owned()
            } else {
                format!("case_{index}")
            };
            zirv_flow_store::decision_testing::assert_fixture(
                &env.pool,
                env.ws,
                &enrollment.trace,
                ("flow.branch.condition", outcome, Some(&reason)),
                subjects.clone(),
            )
            .await;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_1(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(1)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_2(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(2)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_3(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(3)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_4(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(4)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_5(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(5)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_6(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(6)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_7(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(7)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_8(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(8)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_9(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, Some(9)).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_else(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            branch_fixture(pool, None).await;
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_step_failed(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let person = env.person(PERSONAL_DATA.email, json!({})).await;
            let flow = env.flow("Invalid stored time", json!({"v": 1, "nodes": [
                {"id": "wait", "type": "wait.until_time", "when": {"kind": "date_time", "at": "soon"}},
                {"id": "end", "type": "exit"}
            ], "edges": [edge("next", "wait", "next", "end")]})).await;
            let enrolled = env.enroll(flow, 1, &[person]).await;
            let enrollment = &enrolled.enrolled[0];
            env.run_due().await;
            let step: Uuid = sqlx::query_scalar("SELECT id FROM step_executions WHERE enrollment_id = $1 AND node_id = 'wait'")
                .bind(enrollment.enrollment).fetch_one(&env.pool).await.unwrap();
            let subjects = json!({"flow": flow, "enrollment": enrollment.enrollment, "person": person,
                "version": 1, "node": "wait", "step_execution": step});
            for action in ["flow.step", "flow.enrollment"] {
                zirv_flow_store::decision_testing::assert_fixture(&env.pool, env.ws, &enrollment.trace,
                    (action, "failed", Some("invalid_time")), subjects.clone()).await;
            }
        }).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn flow_branch_case_more(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            // The API rejects this size; the worker must still label an oversized stored graph.
            branch_fixture(pool, Some(10)).await;
        })
        .await;
    }

    // The wakes of one workspace that a slot claims together run in one transaction, a savepoint
    // each (#397).

    /// A wait of a minute, then one more visit, then the end: a wake applied twice shows as two.
    fn visit_graph() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "visit", "type": "action.update_trait", "key": "visits",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "visit"), edge("e2", "visit", "next", "end")]})
    }

    /// A wait of a minute, then an increment of the `plan` trait, which fails for a person whose
    /// plan is text.
    fn bump_plan_graph() -> Value {
        json!({"v": 1, "nodes": [
            {"id": "w", "type": "wait.duration", "seconds": 60},
            {"id": "bump", "type": "action.update_trait", "key": "plan",
             "change": {"op": "increment", "by": 1.0}},
            {"id": "end", "type": "exit"}
        ], "edges": [edge("e1", "w", "next", "bump"), edge("e2", "bump", "next", "end")]})
    }

    /// The people `visitor{n}@example.com` of `numbers`, with no visits yet, in the order of their
    /// ids. The `visits` trait must be defined.
    async fn visitors(env: &Env, numbers: std::ops::Range<usize>) -> Vec<Uuid> {
        let mut people = Vec::with_capacity(numbers.len());
        for n in numbers {
            let email = format!("visitor{n}@example.com");
            people.push(env.person(&email, json!({"visits": 0})).await);
        }
        people
    }

    /// `count` people with no visits yet, enrolled in a flow of [`visit_graph`] and asleep in its
    /// wait, sorted by person id; the clock is past the wait and their wakes are due.
    async fn due_visits(env: &Env, count: usize) -> Vec<zirv_flow_store::Enrolled> {
        env.define_trait("visits", TraitType::Number).await;
        let people = visitors(env, 0..count).await;
        enroll_visitors(env, &people).await
    }

    /// `people` enrolled in a flow of [`visit_graph`] and asleep in its wait, sorted by person id;
    /// the clock is past the wait and every due wake is due.
    async fn enroll_visitors(env: &Env, people: &[Uuid]) -> Vec<zirv_flow_store::Enrolled> {
        enroll_asleep(env, "Visits", visit_graph(), people).await
    }

    /// `people` enrolled in a flow `name` of `graph`, which starts with a wait of a minute, and
    /// asleep in it, sorted by person id; the clock is past the wait and every due wake is due.
    async fn enroll_asleep(
        env: &Env,
        name: &str,
        graph: Value,
        people: &[Uuid],
    ) -> Vec<zirv_flow_store::Enrolled> {
        let flow = env.flow(name, graph).await;
        let mut enrolled = env.enroll(flow, 1, people).await.enrolled;
        enrolled.sort_by_key(|enrolled| enrolled.person);
        env.run_due().await;
        env.advance(61);
        env.hold_others(Some("wake"), None).await;
        enrolled
    }

    /// The due wakes, claimed together as a slot claims them.
    async fn claim_wakes(env: &Env, count: usize) -> Vec<ClaimedJob> {
        let claimed = zirv_flow_store::claim_jobs(
            &env.pool,
            "test",
            &[STEP_JOB_KIND],
            Duration::from_secs(30),
            None,
            crate::worker::CLAIM_BATCH,
        )
        .await
        .unwrap();
        env.release().await;
        assert_eq!(claimed.len(), count, "the wakes that are due");
        claimed
    }

    /// A worker's configuration on the test's clock, with retries a few milliseconds apart.
    fn batch_config(env: &Env) -> crate::WorkerConfig {
        let mut config = crate::WorkerConfig::new(1);
        config.clock = Clock::Manual(env.clock.clone());
        config.poll_interval = Duration::from_millis(20);
        config.backoff_base = Duration::from_millis(1);
        config.backoff_cap = Duration::from_millis(5);
        config
    }

    /// Runs `jobs` in one batch transaction as a slot does, beside the test.
    fn spawn_batch(
        env: &Env,
        jobs: Vec<ClaimedJob>,
    ) -> tokio::task::JoinHandle<Vec<(Uuid, crate::metrics::Outcome)>> {
        let (pool, config, ws) = (env.pool.clone(), batch_config(env), env.ws);
        tokio::spawn(async move { crate::worker::run_wakes(&pool, config, ws, jobs).await })
    }

    /// The outcome of each job, in the order of `jobs`.
    fn outcomes_of(
        jobs: &[ClaimedJob],
        outcomes: &[(Uuid, crate::metrics::Outcome)],
    ) -> Vec<crate::metrics::Outcome> {
        jobs.iter()
            .map(|job| {
                outcomes
                    .iter()
                    .find(|(id, _)| *id == job.id)
                    .map(|(_, outcome)| *outcome)
                    .unwrap_or_else(|| panic!("job {} was not run", job.id))
            })
            .collect()
    }

    /// The wake job of each enrollment, in the order of `enrolled`.
    fn wakes_of(enrolled: &[zirv_flow_store::Enrolled], jobs: &[ClaimedJob]) -> Vec<ClaimedJob> {
        enrolled
            .iter()
            .map(|enrolled| {
                jobs.iter()
                    .find(|job| enrollment_of(job) == Some(enrolled.enrollment))
                    .cloned()
                    .unwrap()
            })
            .collect()
    }

    async fn visits(env: &Env, person: Uuid) -> Value {
        env.traits(person).await["visits"].clone()
    }

    /// Holds `person` locked, as a merge or an erasure does, until the transaction ends.
    async fn hold_person(env: &Env, person: Uuid) -> zirv_flow_store::Tx {
        let mut tx = zirv_flow_store::begin(&env.pool).await.unwrap();
        sqlx::query("SELECT id FROM people WHERE id = $1 FOR UPDATE")
            .bind(person)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx
    }

    /// How many connections of this database wait for a lock.
    async fn waiting(env: &Env) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND cardinality(pg_blocking_pids(pid)) > 0",
        )
        .fetch_one(&env.pool)
        .await
        .unwrap()
    }

    /// Waits until `count` connections of this database wait for a lock.
    async fn until_waiting(env: &Env, count: i64) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if waiting(env).await >= count {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} connections never waited for a lock"));
    }

    /// The decisions a job's attempt wrote, in the order they were recorded: action, outcome,
    /// reason, the names of their subjects and the attempt.
    async fn shapes_of(env: &Env, job: Uuid) -> Vec<(String, String, Option<String>, Value, i32)> {
        sqlx::query_as(
            "SELECT action, outcome, reason, \
                    COALESCE((SELECT jsonb_agg(k ORDER BY k) \
                              FROM jsonb_object_keys(subject_ids) k), '[]'), \
                    attempt \
             FROM decision_records WHERE workspace_id = $1 AND job_id = $2 \
             ORDER BY recorded_at, id",
        )
        .bind(env.ws.as_uuid())
        .bind(job)
        .fetch_all(&env.pool)
        .await
        .unwrap()
    }

    /// How many transactions wrote the decisions of `jobs`: their `recorded_at` is the start of
    /// the transaction that wrote them.
    async fn transactions_of(env: &Env, jobs: &[Uuid]) -> i64 {
        sqlx::query_scalar(
            "SELECT count(DISTINCT recorded_at) FROM decision_records \
             WHERE workspace_id = $1 AND job_id = ANY($2) AND action <> 'job.attempt'",
        )
        .bind(env.ws.as_uuid())
        .bind(jobs)
        .fetch_one(&env.pool)
        .await
        .unwrap()
    }

    /// Runs a worker of one slot until no `flow.step` job is left.
    async fn work_off_steps(env: &Env) {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let worker = crate::Worker::new(
            env.pool.clone(),
            crate::Registry::built_in(),
            batch_config(env),
        );
        let running = tokio::spawn(worker.run(stopped));
        tokio::time::timeout(Duration::from_secs(60), async {
            while env
                .count("SELECT count(*) FROM jobs WHERE kind = 'flow.step'")
                .await
                > 0
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the worker runs every step");
        stop.send(true).unwrap();
        running.await.unwrap();
    }

    /// The `job.attempt` decisions of a job, as (outcome, reason, attempt), in the order they were
    /// recorded.
    async fn attempts_of(env: &Env, job: Uuid) -> Vec<(String, Option<String>, i32)> {
        shapes_of(env, job)
            .await
            .into_iter()
            .filter(|(action, ..)| action == "job.attempt")
            .map(|(_, outcome, reason, _, attempt)| (outcome, reason, attempt))
            .collect()
    }

    /// Relays connections to the database and counts their round trips: every simple query and
    /// every `Sync` of the extended protocol, which the server answers with `ReadyForQuery`.
    struct RoundTrips {
        options: sqlx::postgres::PgConnectOptions,
        count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }

    impl RoundTrips {
        /// A relay to the database of `pool`, until the test's runtime ends.
        async fn to(pool: &PgPool) -> Self {
            let target = (*pool.connect_options()).clone();
            let address = format!("{}:{}", target.get_host(), target.get_port());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let count = std::sync::Arc::default();
            let counting = std::sync::Arc::clone(&count);
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let server = tokio::net::TcpStream::connect(&address).await.unwrap();
                    tokio::spawn(relay(client, server, std::sync::Arc::clone(&counting)));
                }
            });
            Self {
                options: target
                    .host("127.0.0.1")
                    .port(port)
                    .ssl_mode(sqlx::postgres::PgSslMode::Disable),
                count,
            }
        }

        /// A pool of one connection through the relay, so every statement of a test runs on it.
        async fn pool(&self) -> PgPool {
            sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect_with(self.options.clone())
                .await
                .unwrap()
        }

        fn count(&self) -> u64 {
            self.count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Copies one connection both ways, counting the client's simple queries and `Sync`s. The
    /// startup message carries no type byte; every message after it does.
    async fn relay(
        client: tokio::net::TcpStream,
        server: tokio::net::TcpStream,
        count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut from_client, mut to_client) = client.into_split();
        let (mut from_server, mut to_server) = server.into_split();
        tokio::spawn(async move { tokio::io::copy(&mut from_server, &mut to_client).await });
        let mut typed = false;
        loop {
            let mut header = [0_u8; 5];
            let start = usize::from(!typed);
            if from_client.read_exact(&mut header[start..]).await.is_err() {
                return;
            }
            let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
            let mut body = vec![0_u8; length as usize - 4];
            if from_client.read_exact(&mut body).await.is_err() {
                return;
            }
            if typed && matches!(header[0], b'Q' | b'S') {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            if to_server.write_all(&header[start..]).await.is_err()
                || to_server.write_all(&body).await.is_err()
            {
                return;
            }
            typed = true;
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_slot_runs_the_due_wakes_of_a_workspace_in_one_transaction_that_writes_what_a_lone_wake_writes(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
            let handle = recorder.handle();
            let _installed = ::metrics::set_default_local_recorder(&recorder);
            let env = Env::new(pool).await;
            let enrolled = due_visits(&env, crate::worker::CLAIM_BATCH as usize + 1).await;
            let wakes: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT id, (payload ->> 'enrollment_id')::uuid FROM jobs WHERE kind = 'flow.step'",
            )
            .fetch_all(&env.pool)
            .await
            .unwrap();
            let wake_of = |enrollment: Uuid| {
                wakes
                    .iter()
                    .find(|(_, of)| *of == enrollment)
                    .map(|(id, _)| *id)
                    .unwrap()
            };
            // One wake runs alone, as every wake did before; the others are a full batch, what a
            // slot claims at once.
            let lone = enrolled[4].enrollment;
            assert_eq!(env.run_matching(Some("wake"), Some(lone)).await, 1);
            assert_eq!(
                env.count("SELECT count(*) FROM jobs WHERE state = 'ready' AND run_at <= now()")
                    .await,
                i64::from(crate::worker::CLAIM_BATCH),
                "only the batch's wakes are due"
            );

            work_off_steps(&env).await;

            let batched: Vec<Uuid> = enrolled
                .iter()
                .filter(|enrolled| enrolled.enrollment != lone)
                .map(|enrolled| wake_of(enrolled.enrollment))
                .collect();
            assert_eq!(transactions_of(&env, &batched).await, 1);
            assert!(
                handle
                    .render()
                    .contains("zirv_flow_job_transactions_total{kind=\"flow.step\"} 1"),
                "{}",
                handle.render()
            );
            let alone = shapes_of(&env, wake_of(lone)).await;
            assert!(!alone.is_empty());
            for enrolled in &enrolled {
                assert_eq!(
                    env.enrollment(enrolled.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
                assert_eq!(visits(&env, enrolled.person).await, json!(1));
                assert_eq!(
                    shapes_of(&env, wake_of(enrolled.enrollment)).await,
                    alone,
                    "a batched wake records what a lone wake records"
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wake_that_fails_in_a_batch_rolls_back_to_its_savepoint_and_is_retried_while_the_others_commit(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            const MESSAGE: &str = "an increment needs a trait whose value is a number";
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let failing = env.flow("Failing", bump_plan_graph()).await;
            let doomed = env.enroll(failing, 1, &[ada]).await.enrolled[0].enrollment;
            // A full batch (ruling 48be9592).
            let enrolled = due_visits(&env, 31).await;
            let before = env.enrollment(doomed).await;
            let jobs = claim_wakes(&env, 32).await;
            let doomed_job = jobs
                .iter()
                .find(|job| enrollment_of(job) == Some(doomed))
                .unwrap()
                .clone();

            let outcomes = spawn_batch(&env, jobs.clone()).await.unwrap();

            let wakes = wakes_of(&enrolled, &jobs);
            assert_eq!(
                outcomes_of(&wakes, &outcomes),
                [crate::metrics::Outcome::Completed; 31]
            );
            assert_eq!(
                outcomes_of(std::slice::from_ref(&doomed_job), &outcomes),
                [crate::metrics::Outcome::Retried]
            );
            for enrolled in &enrolled {
                assert_eq!(visits(&env, enrolled.person).await, json!(1));
            }
            let ids: Vec<Uuid> = wakes.iter().map(|job| job.id).collect();
            assert_eq!(
                transactions_of(&env, &ids).await,
                1,
                "the others committed together"
            );
            let in_batch: bool = sqlx::query_scalar(
                "SELECT (SELECT recorded_at FROM decision_records WHERE job_id = $1) \
                      = (SELECT max(recorded_at) FROM decision_records WHERE job_id = ANY($2))",
            )
            .bind(doomed_job.id)
            .bind(&ids)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert!(in_batch, "the failure was written in the batch transaction");
            let after = env.enrollment(doomed).await;
            assert_eq!(
                (after.status, after.state_version, after.node.as_str()),
                (before.status, before.state_version, "w"),
                "the failed wake left its enrollment as it was"
            );
            let (state, attempts, error): (String, i32, Option<String>) =
                sqlx::query_as("SELECT state, attempts, last_error FROM jobs WHERE id = $1")
                    .bind(doomed_job.id)
                    .fetch_one(&env.pool)
                    .await
                    .unwrap();
            assert_eq!(
                (state.as_str(), attempts, error.as_deref()),
                ("ready", 1, Some(MESSAGE))
            );
            let recorded: Vec<(String, String, Option<String>, Value, i32)> =
                shapes_of(&env, doomed_job.id).await;
            assert_eq!(
                recorded
                    .iter()
                    .map(|(action, outcome, reason, _, attempt)| {
                        (
                            action.as_str(),
                            outcome.as_str(),
                            reason.as_deref(),
                            *attempt,
                        )
                    })
                    .collect::<Vec<_>>(),
                [("job.attempt", "failed", Some("job_error"), 1)],
                "only the failure is recorded for the failed wake"
            );
        })
        .await;
    }

    /// A batch appends the decisions of all its wakes in one statement before it commits (#397).
    /// A wake that fails in the middle of the batch rolls back to its savepoint, and the decisions
    /// it made go with it; the wakes before and after it keep theirs, in the order they made them.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_batch_appends_its_decisions_once_without_those_of_a_wake_that_rolled_back(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            env.define_trait("visits", TraitType::Number).await;
            let mut people = visitors(&env, 0..16).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            people.extend(visitors(&env, 16..31).await);
            let failing = env.flow("Failing", bump_plan_graph()).await;
            let doomed = env.enroll(failing, 1, &[ada]).await.enrolled[0].enrollment;
            let enrolled = enroll_visitors(&env, &people).await;
            assert!(
                enrolled[15].person < ada && ada < enrolled[16].person,
                "the failing wake runs in the middle of the batch"
            );
            let jobs = claim_wakes(&env, 32).await;
            let doomed_job = jobs
                .iter()
                .find(|job| enrollment_of(job) == Some(doomed))
                .unwrap()
                .clone();

            let outcomes = spawn_batch(&env, jobs.clone()).await.unwrap();

            use crate::metrics::Outcome::{Completed, Retried};
            let wakes = wakes_of(&enrolled, &jobs);
            assert_eq!(outcomes_of(&wakes, &outcomes), [Completed; 31]);
            assert_eq!(
                outcomes_of(std::slice::from_ref(&doomed_job), &outcomes),
                [Retried]
            );
            // Every flow decision of the batch, in the order it was recorded.
            let rows: Vec<(Uuid, String, String, i64)> = sqlx::query_as(
                "SELECT job_id, action, outcome, \
                        dense_rank() OVER (ORDER BY cmin::text::bigint) \
                 FROM decision_records WHERE workspace_id = $1 AND job_id = ANY($2) \
                   AND action LIKE 'flow.%' ORDER BY recorded_at, id",
            )
            .bind(env.ws.as_uuid())
            .bind(jobs.iter().map(|job| job.id).collect::<Vec<_>>())
            .fetch_all(&env.pool)
            .await
            .unwrap();
            assert!(
                rows.iter().all(|(.., statement)| *statement == 1),
                "the batch appended its decisions in more than one insert: {rows:?}"
            );
            assert!(
                rows.iter().all(|(job, ..)| *job != doomed_job.id),
                "the rolled-back wake kept a decision"
            );
            let made = [
                ("flow.wake", "timer"),
                ("flow.step", "done"),
                ("flow.step", "done"),
                ("flow.enrollment", "completed"),
            ];
            let expected: Vec<(Uuid, String, String)> = wakes
                .iter()
                .flat_map(|wake| {
                    made.iter().map(|(action, outcome)| {
                        (wake.id, (*action).to_owned(), (*outcome).to_owned())
                    })
                })
                .collect();
            let recorded: Vec<(Uuid, String, String)> = rows
                .into_iter()
                .map(|(job, action, outcome, _)| (job, action, outcome))
                .collect();
            assert_eq!(
                recorded, expected,
                "each wake's decisions, wake after wake in the order they ran"
            );
            assert_eq!(
                attempts_of(&env, doomed_job.id).await,
                [("failed".to_owned(), Some("job_error".to_owned()), 1)]
            );
        })
        .await;
    }

    /// The round trips of a wake of a full batch (#397), counted on the wire after a first batch
    /// prepared every statement on the connection, for the bench's timer: a wait, then the end.
    /// On 0.231.0 a batch of 32 made 490 of them, 15.3 a wake; one clock read per batch, one
    /// append of its decisions and one fenced completion of its wakes take three from each wake.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wake_of_a_full_batch_makes_three_round_trips_fewer(pool: PgPool) {
        /// The round trips of a batch of 32 such wakes on 0.231.0.
        const BEFORE: u64 = 490;
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let batch = crate::worker::CLAIM_BATCH as usize;
            env.define_trait("visits", TraitType::Number).await;
            let people = visitors(&env, 0..2 * batch).await;
            let timer = json!({"v": 1, "nodes": [
                {"id": "w", "type": "wait.duration", "seconds": 60},
                {"id": "end", "type": "exit"}
            ], "edges": [edge("e1", "w", "next", "end")]});
            let enrolled = enroll_asleep(&env, "Timer", timer, &people).await;
            // The database's clock, which the wakes read here, is past their timers too.
            sqlx::query("UPDATE enrollments SET wake_at = wake_at - interval '2 minutes'")
                .execute(&env.pool)
                .await
                .unwrap();
            let relay = RoundTrips::to(&env.pool).await;
            let through = relay.pool().await;
            let mut config = batch_config(&env);
            config.clock = Clock::Database;
            let mut trips = Vec::new();
            for _ in 0..2 {
                let jobs = claim_wakes(&env, batch).await;
                let before = relay.count();
                let outcomes =
                    crate::worker::run_wakes(&through, config.clone(), env.ws, jobs.clone()).await;
                trips.push(relay.count() - before);
                assert_eq!(
                    outcomes_of(&jobs, &outcomes),
                    vec![crate::metrics::Outcome::Completed; batch]
                );
            }
            for enrolled in &enrolled {
                assert_eq!(
                    env.enrollment(enrolled.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
            }
            let warm = trips[1];
            eprintln!(
                "round trips of a batch of {batch}: {trips:?}, {:.2} per wake warm",
                warm as f64 / batch as f64
            );
            assert!(
                warm + 3 * batch as u64 <= BEFORE + 3,
                "a warm batch of {batch} made {warm} round trips, against {BEFORE} on 0.231.0"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_wake_keeps_its_own_failure_when_its_lease_runs_out_before_the_batch_ends(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            const MESSAGE: &str = "an increment needs a trait whose value is a number";
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let failing = env.flow("Failing", bump_plan_graph()).await;
            let doomed = env.enroll(failing, 1, &[ada]).await.enrolled[0].enrollment;
            let enrolled = due_visits(&env, 31).await;
            assert!(ada < enrolled[0].person, "the failing wake runs first");
            // Leases of two seconds for the failing wake and the next one, which the failed wake
            // outlives while the next one waits. The rest of the full batch hold the usual lease,
            // so the reaper takes nothing but the failed wake.
            let lease = Duration::from_secs(2);
            sqlx::query(
                "UPDATE jobs SET run_at = run_at + interval '100 years' \
                 WHERE kind = 'flow.step' AND payload ->> 'enrollment_id' <> ALL($1)",
            )
            .bind(vec![doomed.to_string(), enrolled[0].enrollment.to_string()])
            .execute(&env.pool)
            .await
            .unwrap();
            let mut jobs = zirv_flow_store::claim_jobs(
                &env.pool,
                "test",
                &[STEP_JOB_KIND],
                lease,
                None,
                crate::worker::CLAIM_BATCH,
            )
            .await
            .unwrap();
            assert_eq!(jobs.len(), 2, "the two short leases");
            env.release().await;
            jobs.extend(claim_wakes(&env, 30).await);
            let doomed_job = jobs
                .iter()
                .find(|job| enrollment_of(job) == Some(doomed))
                .unwrap()
                .clone();
            let blocker = hold_person(&env, enrolled[0].person).await;
            let mut config = batch_config(&env);
            config.lease = lease;
            config.backoff_base = Duration::from_secs(600);
            config.backoff_cap = Duration::from_secs(600);
            let (pool, ws) = (env.pool.clone(), env.ws);
            let batch =
                tokio::spawn(
                    async move { crate::worker::run_wakes(&pool, config, ws, jobs).await },
                );
            until_waiting(&env, 1).await;
            // The failed wake's lease runs out while the next wake waits, and the reaper runs.
            tokio::time::sleep(lease + Duration::from_millis(500)).await;
            let pool = env.pool.clone();
            let reaper =
                tokio::spawn(async move { zirv_flow_store::reap_expired(&pool).await.unwrap() });
            for _ in 0..300 {
                if reaper.is_finished() || waiting(&env).await >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            blocker.rollback().await.unwrap();
            let outcomes = batch.await.unwrap();
            reaper.await.unwrap();

            assert_eq!(
                outcomes_of(std::slice::from_ref(&doomed_job), &outcomes),
                [crate::metrics::Outcome::Retried]
            );
            let (state, attempts, error, backed_off): (String, i32, Option<String>, bool) =
                sqlx::query_as(
                    "SELECT state, attempts, last_error, run_at > now() + interval '1 minute' \
                     FROM jobs WHERE id = $1",
                )
                .bind(doomed_job.id)
                .fetch_one(&env.pool)
                .await
                .unwrap();
            assert_eq!(
                (state.as_str(), attempts, error.as_deref(), backed_off),
                ("ready", 1, Some(MESSAGE), true),
                "the failure, not the reaper, settled the wake"
            );
            let recorded: Vec<(String, String, Option<String>, Value, i32)> =
                shapes_of(&env, doomed_job.id).await;
            assert_eq!(
                recorded
                    .iter()
                    .map(|(action, outcome, reason, _, attempt)| {
                        (
                            action.as_str(),
                            outcome.as_str(),
                            reason.as_deref(),
                            *attempt,
                        )
                    })
                    .collect::<Vec<_>>(),
                [("job.attempt", "failed", Some("job_error"), 1)]
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failure_written_in_a_batch_whose_commit_fails_is_settled_once_after_it(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            const MESSAGE: &str = "an increment needs a trait whose value is a number";
            let env = Env::new(pool).await;
            env.define_trait("plan", TraitType::String).await;
            let ada = env
                .person(PERSONAL_DATA.email, json!({"plan": "pro"}))
                .await;
            let failing = env.flow("Failing", bump_plan_graph()).await;
            let doomed = env.enroll(failing, 1, &[ada]).await.enrolled[0].enrollment;
            let enrolled = due_visits(&env, 31).await;
            // The next commit that writes a step row fails, at the commit itself.
            for statement in [
                "CREATE SEQUENCE commit_failures",
                "CREATE FUNCTION fail_first_commit() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     IF nextval('commit_failures') = 1 THEN \
                         RAISE EXCEPTION 'injected commit failure'; \
                     END IF; \
                     RETURN NULL; \
                 END $$",
                "CREATE CONSTRAINT TRIGGER fail_first_commit AFTER INSERT ON step_executions \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_first_commit()",
            ] {
                sqlx::query(statement).execute(&env.pool).await.unwrap();
            }
            let jobs = claim_wakes(&env, 32).await;
            let doomed_job = jobs
                .iter()
                .find(|job| enrollment_of(job) == Some(doomed))
                .unwrap()
                .clone();

            let outcomes = spawn_batch(&env, jobs.clone()).await.unwrap();

            assert_eq!(
                outcomes_of(&jobs, &outcomes),
                [crate::metrics::Outcome::Retried; 32]
            );
            let errors: Vec<(Uuid, String, i32, Option<String>)> = sqlx::query_as(
                "SELECT id, state, attempts, last_error FROM jobs WHERE id = ANY($1)",
            )
            .bind(jobs.iter().map(|job| job.id).collect::<Vec<_>>())
            .fetch_all(&env.pool)
            .await
            .unwrap();
            for wake in wakes_of(&enrolled, &jobs).iter().chain([&doomed_job]) {
                let error = if wake.id == doomed_job.id {
                    MESSAGE
                } else {
                    "could not commit the job transaction"
                };
                let (_, state, attempts, last_error) =
                    errors.iter().find(|(id, ..)| *id == wake.id).unwrap();
                assert_eq!(
                    (state.as_str(), *attempts, last_error.as_deref()),
                    ("ready", 1, Some(error)),
                    "job {}",
                    wake.id
                );
                let recorded: Vec<(String, String, Option<String>, Value, i32)> =
                    shapes_of(&env, wake.id).await;
                assert_eq!(
                    recorded
                        .iter()
                        .map(|(action, outcome, reason, _, attempt)| {
                            (
                                action.as_str(),
                                outcome.as_str(),
                                reason.as_deref(),
                                *attempt,
                            )
                        })
                        .collect::<Vec<_>>(),
                    [("job.attempt", "failed", Some("job_error"), 1)],
                    "job {} is settled once",
                    wake.id
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_failed_batch_commit_keeps_nothing_and_every_wake_runs_again_with_its_effect_once(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let enrolled = due_visits(&env, 32).await;
            let wakes: Vec<Uuid> =
                sqlx::query_scalar("SELECT id FROM jobs WHERE kind = 'flow.step'")
                    .fetch_all(&env.pool)
                    .await
                    .unwrap();
            assert_eq!(wakes.len(), 32);
            // The first commit that writes a step row fails, at the commit itself.
            for statement in [
                "CREATE SEQUENCE commit_failures",
                "CREATE FUNCTION fail_first_commit() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     IF nextval('commit_failures') = 1 THEN \
                         RAISE EXCEPTION 'injected commit failure'; \
                     END IF; \
                     RETURN NULL; \
                 END $$",
                "CREATE CONSTRAINT TRIGGER fail_first_commit AFTER INSERT ON step_executions \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION fail_first_commit()",
            ] {
                sqlx::query(statement).execute(&env.pool).await.unwrap();
            }

            work_off_steps(&env).await;

            assert!(
                env.count("SELECT last_value FROM commit_failures").await > 1,
                "the failed commit was not the last"
            );
            for enrolled in &enrolled {
                assert_eq!(
                    env.enrollment(enrolled.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
                assert_eq!(
                    visits(&env, enrolled.person).await,
                    json!(1),
                    "applied once"
                );
            }
            let steps: Vec<i64> = sqlx::query_scalar(
                "SELECT count(*) FROM step_executions WHERE enrollment_id = ANY($1) \
                 GROUP BY enrollment_id",
            )
            .bind(
                enrolled
                    .iter()
                    .map(|enrolled| enrolled.enrollment)
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&env.pool)
            .await
            .unwrap();
            assert_eq!(steps.len(), 32);
            assert!(steps.iter().all(|count| *count == steps[0]), "{steps:?}");
            // Every wake of the batch failed with it, and each ran again to its one completion.
            for wake in &wakes {
                let failed: Vec<(String, String, Option<String>, Value, i32)> =
                    shapes_of(&env, *wake)
                        .await
                        .into_iter()
                        .filter(|(action, ..)| action == "job.attempt")
                        .collect();
                assert_eq!(failed.len(), 1, "wake {wake}: {failed:?}");
                assert_eq!(
                    (failed[0].1.as_str(), failed[0].2.as_deref(), failed[0].4),
                    ("failed", Some("job_error"), 1)
                );
            }
        })
        .await;
    }

    /// A batch completes its wakes with one fenced delete before it commits (#397). A wake whose
    /// lease is lost after it began, here while it waits for its person, is missing from it, so the
    /// batch commits nothing: the lost wake is its new owner's, as a job alone's whose completion
    /// finds the lease gone, and every other wake retries as after a failed commit. Run again, each
    /// applies its effect once.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_wake_that_loses_its_lease_mid_batch_keeps_the_batch_from_committing_and_each_runs_once_after(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let enrolled = due_visits(&env, 32).await;
            let jobs = claim_wakes(&env, 32).await;
            let wakes = wakes_of(&enrolled, &jobs);
            // The second wake waits for its person after it took its lease lock.
            let blocker = hold_person(&env, enrolled[1].person).await;
            let batch = spawn_batch(&env, jobs.clone());
            until_waiting(&env, 1).await;
            // Its lease runs out and the reaper hands it back while it waits.
            sqlx::query(
                "UPDATE jobs SET lease_expires_at = now() - interval '1 second' WHERE id = $1",
            )
            .bind(wakes[1].id)
            .execute(&env.pool)
            .await
            .unwrap();
            let reaped = zirv_flow_store::reap_expired(&env.pool).await.unwrap();
            assert_eq!(reaped.requeued, 1);
            blocker.rollback().await.unwrap();
            let outcomes = batch.await.unwrap();

            use crate::metrics::Outcome::{LeaseLost, Retried};
            let mut expected = [Retried; 32];
            expected[1] = LeaseLost;
            assert_eq!(outcomes_of(&wakes, &outcomes), expected);
            for enrolled in &enrolled {
                assert_eq!(
                    visits(&env, enrolled.person).await,
                    json!(0),
                    "the batch committed nothing"
                );
            }

            work_off_steps(&env).await;

            for enrolled in &enrolled {
                assert_eq!(
                    visits(&env, enrolled.person).await,
                    json!(1),
                    "applied once"
                );
                assert_eq!(
                    env.enrollment(enrolled.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
            }
            let steps: Vec<i64> = sqlx::query_scalar(
                "SELECT count(*) FROM step_executions WHERE enrollment_id = ANY($1) \
                 GROUP BY enrollment_id",
            )
            .bind(
                enrolled
                    .iter()
                    .map(|enrolled| enrolled.enrollment)
                    .collect::<Vec<_>>(),
            )
            .fetch_all(&env.pool)
            .await
            .unwrap();
            assert_eq!(steps.len(), 32);
            assert!(steps.iter().all(|count| *count == steps[0]), "{steps:?}");
            for (index, wake) in wakes.iter().enumerate() {
                let (outcome, reason) = if index == 1 {
                    ("lease_expired", "lease_expired")
                } else {
                    ("failed", "job_error")
                };
                assert_eq!(
                    attempts_of(&env, wake.id).await,
                    [(outcome.to_owned(), Some(reason.to_owned()), 1)],
                    "wake {index}"
                );
            }
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_person_erased_while_a_batch_runs_has_its_wake_skipped_and_nothing_written_back(
        pool: PgPool,
    ) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let enrolled = due_visits(&env, 32).await;
            let jobs = claim_wakes(&env, 32).await;
            let wakes = wakes_of(&enrolled, &jobs);
            // The batch read every enrollment, then waits at its first person; the second person
            // is erased meanwhile.
            let blocker = hold_person(&env, enrolled[0].person).await;
            let batch = spawn_batch(&env, jobs.clone());
            until_waiting(&env, 1).await;
            let erased = &enrolled[1];
            let mut erasure = zirv_flow_store::begin(&env.pool).await.unwrap();
            assert!(
                zirv_flow_store::delete_person(
                    &mut erasure,
                    env.ws,
                    erased.person,
                    zirv_flow_store::DeleteReason::Requested,
                    &TraceContext::root(Cause::api("erase")),
                )
                .await
                .unwrap()
            );
            zirv_flow_store::commit(erasure).await.unwrap();
            blocker.rollback().await.unwrap();
            let outcomes = batch.await.unwrap();

            use crate::metrics::Outcome::Completed;
            assert_eq!(outcomes_of(&wakes, &outcomes), [Completed; 32]);
            for kept in enrolled.iter().filter(|kept| kept.person != erased.person) {
                assert_eq!(visits(&env, kept.person).await, json!(1));
                assert_eq!(
                    env.enrollment(kept.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
            }
            for (table, column) in [
                ("people", "id"),
                ("enrollments", "person_id"),
                ("enrollment_waits", "person_id"),
            ] {
                let left = sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table} WHERE {column} = $1"
                )))
                .bind(erased.person)
                .fetch_one(&env.pool)
                .await
                .unwrap();
                assert_eq!(left, 0, "{table} kept the erased person");
            }
            let written: i64 = sqlx::query_scalar(
                "SELECT (SELECT count(*) FROM step_executions WHERE enrollment_id = $1) \
                      + (SELECT count(*) FROM decision_records \
                         WHERE subject_ids ->> 'person' = $2::text OR job_id = $3) \
                      + (SELECT count(*) FROM jobs WHERE payload ->> 'enrollment_id' = $1::text)",
            )
            .bind(erased.enrollment)
            .bind(erased.person)
            .bind(wakes[1].id)
            .fetch_one(&env.pool)
            .await
            .unwrap();
            assert_eq!(
                written, 0,
                "the batch wrote nothing back for the erased person"
            );
        })
        .await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_merge_of_two_people_a_batch_holds_waits_for_it_and_neither_deadlocks(pool: PgPool) {
        zirv_flow_store::decision_testing::checked(pool, |pool| async move {
            let env = Env::new(pool).await;
            let enrolled = due_visits(&env, 32).await;
            let (low, high) = (&enrolled[0], &enrolled[1]);
            // Claimed higher id first, the order a batch that ran in claim order would lock in.
            sqlx::query(
                "UPDATE jobs SET run_at = now() - CASE WHEN payload ->> 'enrollment_id' = $1::text \
                     THEN interval '2 seconds' ELSE interval '1 second' END \
                 WHERE kind = 'flow.step'",
            )
            .bind(high.enrollment)
            .execute(&env.pool)
            .await
            .unwrap();
            let jobs = claim_wakes(&env, 32).await;
            assert_eq!(enrollment_of(&jobs[0]), Some(high.enrollment));
            let wakes = wakes_of(&enrolled, &jobs);
            // The batch takes the lower id, then waits for the higher; a merge of the two then
            // waits for the batch.
            let blocker = hold_person(&env, high.person).await;
            let batch = spawn_batch(&env, jobs.clone());
            until_waiting(&env, 1).await;
            let emails: Vec<String> = sqlx::query_scalar(
                "SELECT value FROM identities WHERE person_id = ANY($1) AND kind = 'email' \
                 ORDER BY person_id",
            )
            .bind(vec![low.person, high.person])
            .fetch_all(&env.pool)
            .await
            .unwrap();
            let joined = ProfileRecord {
                source: "crm".into(),
                stream: "contacts".into(),
                record_key: "joined".into(),
                identifiers: emails
                    .iter()
                    .map(|email| ("email".to_owned(), email.clone()))
                    .collect(),
                traits: serde_json::Map::new(),
                observed_at: env.clock.now(),
                trace: TraceContext::root(Cause::api("merge")),
            };
            let (pool, ws) = (env.pool.clone(), env.ws);
            let merge = tokio::spawn(async move {
                zirv_flow_store::apply_profile_batch(&pool, ws, &[joined]).await
            });
            until_waiting(&env, 2).await;
            blocker.rollback().await.unwrap();
            let outcomes = batch.await.unwrap();
            let merged = merge.await.unwrap().expect("the merge commits");

            use crate::metrics::Outcome::Completed;
            assert_eq!(outcomes_of(&wakes, &outcomes), [Completed; 32]);
            assert!(
                matches!(merged[0], zirv_flow_store::ProfileOutcome::Applied { .. }),
                "{:?}",
                merged[0]
            );
            let pointers: Vec<(Uuid, Option<Uuid>)> =
                sqlx::query_as("SELECT id, merged_into FROM people WHERE id = ANY($1) ORDER BY id")
                    .bind(vec![low.person, high.person])
                    .fetch_all(&env.pool)
                    .await
                    .unwrap();
            let merged_away: Vec<_> = pointers
                .iter()
                .filter_map(|(id, into)| into.map(|into| (*id, into)))
                .collect();
            assert_eq!(merged_away.len(), 1, "{pointers:?}");
            for enrolled in &enrolled {
                assert_eq!(
                    env.enrollment(enrolled.enrollment).await.status,
                    EnrollmentStatus::Completed
                );
            }
            assert_eq!(
                env.count("SELECT count(*) FROM decision_records WHERE action = 'job.attempt'")
                    .await,
                0,
                "no wake failed"
            );
        })
        .await;
    }
}
