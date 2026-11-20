//! The driver: a virtual clock over a job queue, seeded events that race their timeouts, a seeded
//! branch evaluator, and crashes injected inside and after the commit of every step.
//!
//! One loop claims the earliest job (jobs for the same enrollment that are also due within a
//! couple of seconds are claimed together, as two workers would, and commit in a seeded order),
//! computes the step chain from the state it read, and commits it as one transaction. The
//! runtime re-reads the state on every claim and the commit is a compare-and-set on the version,
//! so a redelivered job finds its work done and is acknowledged without effect. Half the claims
//! that find other jobs due take up to [`BATCH`] of them, as a slot does, and run them in one
//! batch transaction (`Sim::run_batch`).

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use uuid::Uuid;
use zirv_flow_core::flow::FlowGraph;

use super::db::{Commit, Db, Job, JobKind, Journal, QueueKey, Tx};
use super::generate::{self, EVENTS};
use super::invariants::{self, World};
use super::rng::{Rng, mix};
use super::{Config, CrashMode, Defect, Invariant, Report, Violation};
use crate::{ArrivedEvent, EnrollmentState, Outcome, StepContext, StepInput, WaitChange, step};

/// The virtual days enrollments start over.
pub(super) const DAYS: i64 = 90;

/// The most jobs one claim takes, and so one batch transaction runs: the runtime's `CLAIM_BATCH`.
const BATCH: usize = 32;

/// One step in this many of a batch fails in its handler after writing, and rolls back to its
/// savepoint alone.
const STEP_FAILURE_ONE_IN: u64 = 12;

/// The zones of the people; the last is not a zone, which the engine must fall back from.
const ZONES: [Option<&str>; 6] = [
    None,
    Some("Europe/Copenhagen"),
    Some("America/New_York"),
    Some("Asia/Kolkata"),
    Some("Pacific/Auckland"),
    Some("Not/AZone"),
];

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

fn seconds(count: u64) -> TimeDelta {
    TimeDelta::seconds(i64::try_from(count).unwrap_or(i64::MAX / 4))
}

struct Ctx {
    now: DateTime<Utc>,
    person: Option<String>,
}

impl StepContext for Ctx {
    fn now(&self) -> DateTime<Utc> {
        self.now
    }
    fn workspace_timezone(&self) -> Tz {
        Tz::Europe__Copenhagen
    }
    fn person_timezone(&self) -> Option<String> {
        self.person.clone()
    }
}

/// What a claimed job came to, before the commit.
enum Prepared {
    /// Nothing to commit: the job is stale or has nothing to do.
    Nothing,
    Run(Box<Tx>),
}

struct Sim<'a> {
    config: &'a Config,
    graphs: Vec<FlowGraph>,
    graph_of: BTreeMap<Uuid, usize>,
    zones: BTreeMap<Uuid, Option<String>>,
    db: Db,
    rng: Rng,
    clock: DateTime<Utc>,
    report: Report,
    checked_horizon: bool,
}

/// Runs one population to the end and checks the invariants.
pub(super) fn run(config: &Config) -> Report {
    let mut sim = Sim {
        config,
        graphs: Vec::new(),
        graph_of: BTreeMap::new(),
        zones: BTreeMap::new(),
        db: Db::default(),
        rng: Rng::derive(config.seed, &[1]),
        clock: start(),
        report: Report::default(),
        checked_horizon: false,
    };
    sim.populate();
    sim.drive();
    sim.finish()
}

impl Sim<'_> {
    fn violate(&mut self, invariant: Invariant, detail: String) {
        self.report.push(Violation { invariant, detail });
    }

    fn record(&mut self, found: Vec<Violation>) {
        for violation in found {
            self.report.push(violation);
        }
    }

    fn populate(&mut self) {
        let graph_count = (self.config.enrollments / 150).clamp(6, 60);
        let mut graph_rng = Rng::derive(self.config.seed, &[2]);
        for index in 0..graph_count {
            let size = usize::try_from(graph_rng.range(6, 28)).unwrap_or(10);
            self.graphs
                .push(generate::graph(&mut graph_rng, size, index % 7 == 0));
        }
        for _ in 0..self.config.enrollments {
            let id = self.rng.uuid();
            let graph = self.rng.index(self.graphs.len());
            let zone = self.rng.pick(&ZONES).map(str::to_owned);
            let Ok(state) = EnrollmentState::new(id, &self.graphs[graph]) else {
                self.violate(Invariant::Engine, "a generated graph has no entry".into());
                continue;
            };
            let at = start() + seconds(self.rng.below(u64::try_from(DAYS).unwrap_or(1) * 86_400));
            self.graph_of.insert(id, graph);
            self.zones.insert(id, zone);
            self.db.enrollments.insert(id, state);
            let tie = self.rng.next_u64();
            self.db.enqueue(
                Job {
                    enrollment: id,
                    kind: JobKind::Start,
                    run_at: at,
                    tie,
                    crashes: 0,
                    snapshot: None,
                },
                Some(&format!("start:{id}")),
            );
        }
    }

    fn drive(&mut self) {
        let horizon = start() + TimeDelta::days(DAYS);
        while let Some(&first) = self.db.queue.keys().next() {
            let mut now = self.clock.max(first.0);
            if !self.checked_horizon && now >= horizon {
                self.checked_horizon = true;
                let mut found = Vec::new();
                invariants::liveness(&self.world(), false, &mut found);
                self.record(found);
            }
            let Some(job) = self.db.queue.remove(&first) else {
                break;
            };
            let mut group = vec![job];
            if self.config.races {
                let until = now + seconds(self.rng.below(3));
                let enrollment = group[0].enrollment;
                let partners: Vec<QueueKey> = self
                    .db
                    .queue
                    .range(..=(until, u64::MAX, u64::MAX))
                    .filter(|(_, job)| job.enrollment == enrollment)
                    .map(|(key, _)| *key)
                    .collect();
                if !partners.is_empty() {
                    now = until;
                    self.report.stats.races += 1;
                    group.extend(partners.iter().filter_map(|key| self.db.queue.remove(key)));
                    self.rng.shuffle(&mut group);
                }
            }
            if group.len() == 1 && self.batching() {
                // The other jobs due now, which one claim of a slot takes with this one.
                let due: Vec<QueueKey> = self
                    .db
                    .queue
                    .range(..=(now, u64::MAX, u64::MAX))
                    .take(BATCH - 1)
                    .map(|(key, _)| *key)
                    .collect();
                if !due.is_empty() && self.rng.chance(50) {
                    group.extend(due.iter().filter_map(|key| self.db.queue.remove(key)));
                    self.clock = now;
                    let group = self.claim(group);
                    self.run_batch(group, now);
                    continue;
                }
            }
            self.clock = now;
            // Every job of the group reads before any of them commits.
            let mut claimed = Vec::new();
            for mut job in self.claim(group) {
                let prepared = self.prepare(&mut job, now);
                claimed.push((job, prepared));
            }
            for (job, prepared) in claimed {
                self.complete(job, prepared, now);
            }
        }
    }

    fn world(&self) -> World<'_> {
        World {
            db: &self.db,
            graphs: &self.graphs,
            graph_of: &self.graph_of,
        }
    }

    /// Takes the jobs a claim got: each event job that has not been matched yet matches the
    /// COMMITTED wait of its name now, before the claim's first step runs, and keeps that version
    /// for good. This is the point that mirrors the runtime's `enqueue_arrivals`, which matches an
    /// arriving event once, against committed `enrollment_waits`, and fixes the wait's version in
    /// the job; between claims every row of the harness's db is committed, and no step of this
    /// claim has written yet. An event that finds no wait for its name enqueues nothing in the
    /// runtime, so its job is consumed here. A redelivered job already has its version and is
    /// never matched again.
    fn claim(&self, jobs: Vec<Job>) -> Vec<Job> {
        jobs.into_iter()
            .filter_map(|mut job| {
                if let JobKind::Event { name, matched } = &mut job.kind
                    && matched.is_none()
                {
                    let row = self.db.waits.get(&(job.enrollment, name.clone()))?;
                    *matched = Some(row.wake_version);
                }
                Some(job)
            })
            .collect()
    }

    /// Reads the state and computes what the job would commit.
    fn prepare(&mut self, job: &mut Job, now: DateTime<Utc>) -> Prepared {
        let Some(current) = self.db.enrollments.get(&job.enrollment).cloned() else {
            return Prepared::Nothing;
        };
        // A runtime that does not re-read keeps working from what its first claim saw.
        let state = if self.config.defect == Some(Defect::NoIdempotency) {
            job.snapshot.get_or_insert(current).clone()
        } else {
            current
        };
        let (expected, input) = match &mut job.kind {
            JobKind::Start => (0, StepInput::Start),
            JobKind::Wake { version } => (
                *version,
                StepInput::TimerDue {
                    wake_version: *version,
                },
            ),
            JobKind::Event { name, matched } => {
                // Matched when the claim took the job (`Sim::claim`), never here.
                let Some(version) = *matched else {
                    return Prepared::Nothing;
                };
                (
                    version,
                    StepInput::EventArrived {
                        event: ArrivedEvent {
                            name: name.clone(),
                            wake_version: version,
                        },
                    },
                )
            }
        };
        if state.state_version != expected {
            self.report.stats.stale += 1;
            return Prepared::Nothing;
        }
        // The runtime's `first_input` drops an event for a name the state does not await, so the
        // engine is never sent one.
        if let JobKind::Event { name, .. } = &job.kind
            && state.awaiting.as_deref() != Some(name.as_str())
        {
            self.report.stats.stale += 1;
            return Prepared::Nothing;
        }
        let graph = &self.graphs[self.graph_of[&job.enrollment]];
        let ctx = Ctx {
            now,
            person: self.zones.get(&job.enrollment).cloned().flatten(),
        };
        match chain(graph, &state, input, &ctx, self.config.seed) {
            Ok(Some(tx)) => Prepared::Run(Box::new(tx)),
            Ok(None) => {
                self.report.stats.stale += 1;
                Prepared::Nothing
            }
            Err(detail) => {
                self.violate(Invariant::Engine, detail);
                Prepared::Nothing
            }
        }
    }

    /// Where the process dies on this attempt: inside the transaction after that many effects
    /// (`0..=effects + 1`), or after the commit and before the acknowledgement (`effects + 2`).
    fn crash_point(&mut self, job: &Job, effects: usize) -> Option<usize> {
        let points = effects + 3;
        match self.config.crash {
            CrashMode::Never => None,
            CrashMode::Random { one_in } => {
                (self.rng.below(one_in) == 0).then(|| self.rng.index(points))
            }
            // Attempt n of a job dies at point n, so every point of every step is hit once.
            CrashMode::EveryEffect => {
                let attempt = usize::try_from(job.crashes).unwrap_or(usize::MAX);
                (attempt < points).then_some(attempt)
            }
        }
    }

    fn complete(&mut self, mut job: Job, prepared: Prepared, now: DateTime<Utc>) {
        let Prepared::Run(tx) = prepared else {
            return;
        };
        let effects = tx.effects.len();
        let point = self.crash_point(&job, effects);
        let in_tx = point.filter(|point| *point <= effects + 1);
        let tie = self.rng.next_u64();
        match self.db.commit(&tx, in_tx, self.config.defect, tie) {
            Commit::Stale => self.report.stats.stale += 1,
            Commit::RolledBack => {
                self.report.stats.crashes_in_tx += 1;
                job.crashes += 1;
                self.redeliver(job, now);
            }
            Commit::Committed => {
                if self.config.crash == CrashMode::EveryEffect
                    && usize::try_from(job.crashes) != Ok(effects + 2)
                {
                    self.report.stats.partial_sweeps += 1;
                }
                self.after_commit(&tx, now);
                if point == Some(effects + 2) {
                    self.report.stats.crashes_after_commit += 1;
                    job.crashes += 1;
                    self.redeliver(job, now);
                }
            }
        }
    }

    /// Whether claims run batch transactions: not while every effect of every job is a crash
    /// point, which is counted per job.
    fn batching(&self) -> bool {
        self.config.batches && self.config.crash != CrashMode::EveryEffect
    }

    /// Runs the jobs one claim took in one transaction, as a slot runs the steps of a workspace
    /// it claimed together (#397). The jobs run in lock order (by enrollment, ties in claim
    /// order). Each reads the state when it starts, so it sees what the jobs before it wrote, and
    /// writes after a savepoint of its own. A job whose handler fails after writing rolls back to
    /// its savepoint and is redelivered alone, and the others go on. A crash inside any job, or
    /// at the commit, discards every job of the transaction and redelivers them all; a crash
    /// after the commit redelivers them all, to find their work done.
    fn run_batch(&mut self, mut jobs: Vec<Job>, now: DateTime<Utc>) {
        self.report.stats.batches += 1;
        jobs.sort_by_key(|job| job.enrollment);
        let defect = self.config.defect;
        let mut journal = Journal::default();
        let mut held = jobs.into_iter();
        let mut ran: Vec<(Job, Option<Box<Tx>>)> = Vec::new();
        let mut crash_after_commit = false;
        while let Some(mut job) = held.next() {
            let Prepared::Run(tx) = self.prepare(&mut job, now) else {
                ran.push((job, None));
                continue;
            };
            let effects = tx.effects.len();
            let point = self.crash_point(&job, effects);
            crash_after_commit |= point == Some(effects + 2);
            let in_tx = point.filter(|point| *point <= effects + 1);
            let savepoint = journal.mark();
            let tie = self.rng.next_u64();
            match self.db.write(&tx, in_tx, defect, tie, &mut journal) {
                None => {
                    // The process died: the transaction is gone with every job it held.
                    self.db.roll_back(journal, defect);
                    self.report.stats.crashes_in_tx += 1;
                    let lost = ran
                        .into_iter()
                        .map(|(job, _)| job)
                        .chain(std::iter::once(job))
                        .chain(held);
                    for mut job in lost {
                        job.crashes += 1;
                        self.redeliver(job, now);
                    }
                    return;
                }
                Some(Commit::Committed) if self.rng.below(STEP_FAILURE_ONE_IN) == 0 => {
                    let to = if defect == Some(Defect::WideSavepoint) {
                        0
                    } else {
                        savepoint
                    };
                    self.db.roll_back_to(&mut journal, to, defect);
                    self.report.stats.savepoint_rollbacks += 1;
                    self.redeliver(job, now);
                }
                Some(Commit::Committed) => ran.push((job, Some(tx))),
                Some(_) => {
                    self.report.stats.stale += 1;
                    ran.push((job, None));
                }
            }
        }
        let committed = ran.iter().filter(|(_, tx)| tx.is_some()).count();
        if committed > 1 {
            self.report.stats.shared_commits += 1;
        }
        if crash_after_commit {
            self.report.stats.crashes_after_commit += 1;
        }
        for (mut job, tx) in ran {
            if let Some(tx) = tx {
                self.after_commit(&tx, now);
            }
            if crash_after_commit {
                job.crashes += 1;
                self.redeliver(job, now);
            }
        }
    }

    /// The claim times out and another worker claims the job again, a little later.
    fn redeliver(&mut self, job: Job, now: DateTime<Utc>) {
        let delay = seconds(self.rng.range(1, 30));
        let tie = self.rng.next_u64();
        self.db.requeue(job, now, delay, tie);
    }

    fn after_commit(&mut self, tx: &Tx, now: DateTime<Utc>) {
        self.report.stats.commits += 1;
        if let Some(wake_at) = tx.timer_for
            && now < wake_at
        {
            self.violate(
                Invariant::TimerNotEarly,
                format!(
                    "the timer of enrollment {} fired at {now}, before its wake_at {wake_at}",
                    tx.enrollment
                ),
            );
        }
        if let Some(graph) = self.graph_of.get(&tx.enrollment).map(|g| &self.graphs[*g]) {
            for visit in &tx.visits {
                if let Some(node) = graph.node(&visit.node) {
                    self.report.stats.kinds.insert(node.kind.type_name());
                }
            }
        }
        self.schedule_events(tx, now);
    }

    /// Events arrive for a registered wait: before its timeout, at it, or around it (the race),
    /// sometimes twice, sometimes with noise for an event nobody awaits, sometimes never.
    fn schedule_events(&mut self, tx: &Tx, now: DateTime<Utc>) {
        for change in &tx.waits {
            let WaitChange::Register { event, timeout_at } = change else {
                continue;
            };
            let id = tx.enrollment.as_u128();
            let mut rng = Rng::derive(
                self.config.seed,
                &[id as u64, (id >> 64) as u64, tx.state.state_version, 3],
            );
            let span = u64::try_from((*timeout_at - now).num_seconds())
                .unwrap_or(0)
                .max(1);
            let arrival = match rng.below(100) {
                0..=54 => Some(now + seconds(1 + rng.below(span))),
                55..=64 => Some(*timeout_at),
                65..=74 => Some((*timeout_at + seconds(rng.below(5)) - seconds(2)).max(now)),
                _ => None,
            };
            if let Some(at) = arrival {
                self.event_job(tx.enrollment, &event.name, at, &mut rng);
                if rng.chance(10) {
                    let later = at + seconds(rng.range(1, 60));
                    self.event_job(tx.enrollment, &event.name, later, &mut rng);
                }
            }
            if rng.chance(30) {
                let other = *rng.pick(&EVENTS);
                if other != event.name {
                    let at = now + seconds(rng.below(span));
                    self.event_job(tx.enrollment, other, at, &mut rng);
                }
            }
        }
    }

    fn event_job(&mut self, enrollment: Uuid, name: &str, at: DateTime<Utc>, rng: &mut Rng) {
        self.db.enqueue(
            Job {
                enrollment,
                kind: JobKind::Event {
                    name: name.into(),
                    matched: None,
                },
                run_at: at,
                tie: rng.next_u64(),
                crashes: 0,
                snapshot: None,
            },
            None,
        );
    }

    fn finish(mut self) -> Report {
        let mut found = Vec::new();
        let world = self.world();
        invariants::internal_effects_once(&world, &mut found);
        invariants::external_effects_once(&world, &mut found);
        invariants::liveness(&world, true, &mut found);
        self.record(found);
        let mut fingerprint = 0_u64;
        for (id, state) in &self.db.enrollments {
            fingerprint = mix(fingerprint
                ^ mix(id.as_u128() as u64)
                ^ mix(state.state_version)
                ^ mix(state.status as u64 + 1));
        }
        for key in &self.db.applied {
            fingerprint =
                mix(fingerprint ^ key.bytes().fold(0_u64, |acc, b| mix(acc ^ u64::from(b))));
        }
        self.report.fingerprint = mix(fingerprint ^ self.db.outbox.len() as u64);
        self.report.stats.enrollments = self.db.enrollments.len();
        self.report.stats.queue_left = self.db.queue.len();
        self.report
    }
}

/// Runs a step and, while it yields at a branch, answers with the seeded evaluator and steps
/// again: one chain, committed as one transaction. `None` is a stale input.
fn chain(
    graph: &FlowGraph,
    state: &EnrollmentState,
    input: StepInput,
    ctx: &Ctx,
    seed: u64,
) -> Result<Option<Tx>, String> {
    let mut tx = Tx {
        enrollment: state.enrollment_id,
        base_version: state.state_version,
        state: state.clone(),
        visits: Vec::new(),
        effects: Vec::new(),
        wake: None,
        waits: Vec::new(),
        decisions: Vec::new(),
        timer_for: matches!(input, StepInput::TimerDue { .. })
            .then_some(state.wake_at)
            .flatten(),
    };
    let mut current = state.clone();
    let mut input = input;
    for _ in 0..=graph.nodes.len() + 1 {
        let transition = step(graph, &current, input, ctx)
            .map_err(|error| format!("the engine refused a valid graph: {error:?}"))?;
        if transition.outcome == Outcome::Stale {
            return Ok(None);
        }
        tx.visits.extend(transition.visited);
        tx.effects.extend(transition.effects);
        tx.waits.extend(transition.waits);
        tx.decisions.extend(transition.zone_fallback);
        if transition.wake.is_some() {
            tx.wake = transition.wake;
        }
        match transition.outcome {
            Outcome::NeedsConditions { node, conditions } => {
                let results = evaluate(seed, state.enrollment_id, &node, conditions.len());
                input = StepInput::ConditionsEvaluated { node, results };
                current = transition.state;
            }
            Outcome::Failed { node, failure } => {
                return Err(format!("node {node} failed on a valid graph: {failure:?}"));
            }
            _ => {
                tx.state = transition.state;
                return Ok(Some(tx));
            }
        }
    }
    Err("a chain of branches did not end".into())
}

/// The seeded test evaluator: each condition of a branch is true about a third of the time, and
/// the answer depends only on the seed, the enrollment, the node and the condition, so a retry
/// of the same step answers the same.
fn evaluate(seed: u64, enrollment: Uuid, node: &str, count: usize) -> Vec<bool> {
    let id = enrollment.as_u128();
    let base = node.bytes().fold(
        mix(seed ^ mix(id as u64) ^ mix((id >> 64) as u64)),
        |acc, b| mix(acc ^ u64::from(b)),
    );
    (0..count)
        .map(|index| mix(base ^ index as u64) % 100 < 35)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zirv_flow_core::flow::{Edge, EventMatch, FlowNode, NodeKind};

    fn edge(id: &str, from: &str, port: &str, to: &str) -> Edge {
        Edge {
            id: id.into(),
            from: from.into(),
            port: port.into(),
            to: to.into(),
        }
    }

    fn await_event(id: &str, name: &str, timeout_seconds: u64) -> FlowNode {
        FlowNode {
            id: id.into(),
            kind: NodeKind::WaitUntilEvent {
                event: EventMatch {
                    name: name.into(),
                    filters: Vec::new(),
                },
                timeout_seconds,
            },
        }
    }

    /// `w1` awaits `order` and on its timeout falls into `w3`, which awaits `order` again; on the
    /// event it goes to `w2`, which awaits `visit`.
    fn two_orders() -> FlowGraph {
        FlowGraph {
            v: 1,
            nodes: vec![
                await_event("w1", "order", 3_600),
                await_event("w2", "visit", 7_200),
                await_event("w3", "order", 3_600),
                FlowNode {
                    id: "x".into(),
                    kind: NodeKind::Exit {},
                },
            ],
            edges: vec![
                edge("e0", "w1", "event", "w2"),
                edge("e1", "w1", "timeout", "w3"),
                edge("e2", "w2", "event", "x"),
                edge("e3", "w2", "timeout", "x"),
                edge("e4", "w3", "event", "x"),
                edge("e5", "w3", "timeout", "x"),
            ],
        }
    }

    fn job(enrollment: Uuid, kind: JobKind, at: DateTime<Utc>) -> Job {
        Job {
            enrollment,
            kind,
            run_at: at,
            tie: 0,
            crashes: 0,
            snapshot: None,
        }
    }

    fn order_event(matched: Option<u64>) -> JobKind {
        JobKind::Event {
            name: "order".into(),
            matched,
        }
    }

    /// Two enrollments on `two_orders`: `a` has started and sleeps at `w1` (version 1, awaiting
    /// `order`); `b` (the larger id, so it runs last in a batch) has not started.
    fn sim_at_w1(calm: &Config) -> (Sim<'_>, Uuid, Uuid, DateTime<Utc>) {
        let graph = two_orders();
        let (a, b) = (Uuid::from_u128(7), Uuid::from_u128(8));
        let mut sim = Sim {
            config: calm,
            graphs: vec![graph.clone()],
            graph_of: BTreeMap::from([(a, 0), (b, 0)]),
            zones: BTreeMap::from([(a, None), (b, None)]),
            db: Db::default(),
            rng: Rng::derive(calm.seed, &[1]),
            clock: start(),
            report: Report::default(),
            checked_horizon: false,
        };
        for id in [a, b] {
            let state = EnrollmentState::new(id, &graph).expect("the graph has an entry");
            sim.db.enrollments.insert(id, state);
        }
        let mut first = job(a, JobKind::Start, start());
        let prepared = sim.prepare(&mut first, start());
        sim.complete(first, prepared, start());
        sim.db.queue.clear();
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (1, Some("order"))
        );
        assert_eq!(sim.db.waits[&(a, "order".into())].wake_version, 1);
        let wake = state.wake_at.expect("w1 sleeps");
        (sim, a, b, wake)
    }

    /// Claims `jobs` and runs them in one batch with a job of `b` last that dies at its first
    /// write: the whole transaction rolls back and every job is redelivered with
    /// `crashes > 0`. Jobs of `a` carry enough crashes not to die themselves.
    fn crashed_batch<'a>(
        sim: &mut Sim<'a>,
        crashing: &'a Config,
        b: Uuid,
        mut jobs: Vec<Job>,
        at: DateTime<Utc>,
    ) {
        for job in &mut jobs {
            job.crashes = 100;
        }
        jobs.push(job(b, JobKind::Start, at));
        let calm = sim.config;
        sim.config = crashing;
        let jobs = sim.claim(jobs);
        sim.run_batch(jobs, at);
        sim.config = calm;
        assert_eq!(sim.report.stats.crashes_in_tx, 1);
        assert!(sim.db.queue.values().all(|queued| queued.crashes > 0));
    }

    /// Claims `jobs` as one claim of their own and commits each, with no crash.
    fn run_claim(sim: &mut Sim<'_>, jobs: Vec<Job>, at: DateTime<Utc>) {
        for mut one in sim.claim(jobs) {
            let prepared = sim.prepare(&mut one, at);
            sim.complete(one, prepared, at);
        }
    }

    /// Commits `kind` for `a` at `at` as its own claim, with no crash.
    fn run_alone(sim: &mut Sim<'_>, a: Uuid, kind: JobKind, at: DateTime<Utc>) {
        run_claim(sim, vec![job(a, kind, at)], at);
    }

    /// Redelivers every queued redelivery, in queue order, each as its own claim.
    fn redeliver_all(sim: &mut Sim<'_>, at: DateTime<Utc>) {
        let keys: Vec<QueueKey> = sim
            .db
            .queue
            .iter()
            .filter(|(_, queued)| queued.crashes > 0)
            .map(|(key, _)| *key)
            .collect();
        for key in keys {
            let queued = sim.db.queue.remove(&key).expect("queued");
            run_claim(sim, vec![queued], at);
        }
    }

    fn configs() -> (Config, Config) {
        let calm = Config {
            crash: CrashMode::Never,
            races: false,
            batches: false,
            ..Config::new(1, 0)
        };
        let crashing = Config {
            crash: CrashMode::EveryEffect,
            ..calm.clone()
        };
        (calm, crashing)
    }

    /// Seed 483383881: a timer moved `w1` to `w3` (version 2) and an `order` event of the same
    /// batch, matched lazily against that uncommitted wait, took version 2. The crash undid
    /// both, another `order` event then took `w1` to `w2` (also version 2), and the redelivered
    /// one was applied to a state awaiting `visit`: "the enrollment does not await `order`". The
    /// event matches the committed version 1 wait at its claim, keeps it, and is dropped as
    /// stale.
    #[test]
    fn a_crashed_batch_does_not_send_a_redelivered_event_to_a_state_that_awaits_another() {
        let (calm, crashing) = configs();
        let (mut sim, a, b, wake) = sim_at_w1(&calm);
        let jobs = vec![
            job(a, JobKind::Wake { version: 1 }, wake),
            job(a, order_event(None), wake),
        ];
        crashed_batch(&mut sim, &crashing, b, jobs, wake);
        let redelivered = sim
            .db
            .queue
            .values()
            .find(|queued| matches!(queued.kind, JobKind::Event { .. }))
            .expect("the event is redelivered");
        assert!(matches!(
            redelivered.kind,
            JobKind::Event {
                matched: Some(1),
                ..
            }
        ));
        // Another `order` event takes w1 to w2, at version 2 as the timer's step would have.
        let later = wake + TimeDelta::seconds(10);
        run_alone(&mut sim, a, order_event(Some(1)), later);
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (2, Some("visit"))
        );
        let stale = sim.report.stats.stale;
        redeliver_all(&mut sim, later + TimeDelta::minutes(1));
        assert!(
            sim.report.violations.is_empty(),
            "{:?}",
            sim.report.violations
        );
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (2, Some("visit"))
        );
        assert!(
            sim.report.stats.stale >= stale + 2,
            "the timer and the event are stale"
        );
    }

    /// An event that matched the committed wait at version 1 keeps it through a crash. When the
    /// enrollment has moved to a new `order` wait (version 2) before the redelivery, the
    /// redelivered event is stale, as the runtime's `is_stale` finds it; it is not applied to
    /// the version 2 wait.
    #[test]
    fn a_committed_match_survives_a_crash_and_goes_stale() {
        let (calm, crashing) = configs();
        let (mut sim, a, b, wake) = sim_at_w1(&calm);
        crashed_batch(
            &mut sim,
            &crashing,
            b,
            vec![job(a, order_event(None), wake)],
            wake,
        );
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (1, Some("order"))
        );
        // The timer takes w1 to w3: a new `order` wait at version 2.
        let later = wake + TimeDelta::seconds(10);
        run_alone(&mut sim, a, JobKind::Wake { version: 1 }, later);
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (2, Some("order"))
        );
        assert_eq!(sim.db.waits[&(a, "order".into())].wake_version, 2);
        let stale = sim.report.stats.stale;
        redeliver_all(&mut sim, later + TimeDelta::minutes(1));
        let state = &sim.db.enrollments[&a];
        assert_eq!(
            (state.state_version, state.awaiting.as_deref()),
            (2, Some("order")),
            "the version 1 event must not move the version 2 wait"
        );
        assert_eq!(sim.report.stats.stale, stale + 1);
        assert!(
            sim.report.violations.is_empty(),
            "{:?}",
            sim.report.violations
        );
    }

    #[test]
    fn an_event_that_finds_no_committed_wait_is_consumed() {
        let (calm, _) = configs();
        let (sim, a, _, wake) = sim_at_w1(&calm);
        let visit = JobKind::Event {
            name: "visit".into(),
            matched: None,
        };
        assert!(sim.claim(vec![job(a, visit, wake)]).is_empty());
        let claimed = sim.claim(vec![job(a, order_event(None), wake)]);
        assert!(matches!(
            claimed[0].kind,
            JobKind::Event {
                matched: Some(1),
                ..
            }
        ));
    }

    #[test]
    fn the_evaluator_depends_only_on_its_inputs() {
        let id = Uuid::from_u128(5);
        let a = evaluate(1, id, "n3", 6);
        assert_eq!(a, evaluate(1, id, "n3", 6));
        assert_ne!(evaluate(1, id, "n3", 64), evaluate(2, id, "n3", 64));
        assert_ne!(evaluate(1, id, "n3", 64), evaluate(1, id, "n4", 64));
        let truthy = evaluate(9, id, "n1", 2000).iter().filter(|b| **b).count();
        assert!((500..900).contains(&truthy), "{truthy} of 2000");
    }

    #[test]
    fn the_start_is_a_tuesday_in_january() {
        assert_eq!(start().to_rfc3339(), "2030-01-01T00:00:00+00:00");
    }
}
