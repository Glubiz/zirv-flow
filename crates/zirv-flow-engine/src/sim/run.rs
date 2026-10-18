//! The driver: a virtual clock over a job queue, seeded events that race their timeouts, a seeded
//! branch evaluator, and crashes injected inside and after the commit of every step.
//!
//! One loop claims the earliest job (jobs for the same enrollment that are also due within a
//! couple of seconds are claimed together, as two workers would, and commit in a seeded order),
//! computes the step chain from the state it read, and commits it as one transaction. The
//! runtime re-reads the state on every claim and the commit is a compare-and-set on the version,
//! so a redelivered job finds its work done and is acknowledged without effect.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use chrono_tz::Tz;
use uuid::Uuid;
use zirv_flow_core::flow::FlowGraph;

use super::db::{Commit, Db, Job, JobKind, QueueKey, Tx};
use super::generate::{self, EVENTS};
use super::invariants::{self, World};
use super::rng::{Rng, mix};
use super::{Config, CrashMode, Defect, Invariant, Report, Violation};
use crate::{ArrivedEvent, EnrollmentState, Outcome, StepContext, StepInput, WaitChange, step};

/// The virtual days enrollments start over.
pub(super) const DAYS: i64 = 90;

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
            self.clock = now;
            // Every job of the group reads before any of them commits.
            let mut claimed = Vec::new();
            for mut job in group {
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
        let (expected, input) = match &job.kind {
            JobKind::Start => (0, StepInput::Start),
            JobKind::Wake { version } => (
                *version,
                StepInput::TimerDue {
                    wake_version: *version,
                },
            ),
            JobKind::Event { name } => match self.db.waits.get(&(job.enrollment, name.clone())) {
                None => return Prepared::Nothing,
                Some(row) => (
                    row.wake_version,
                    StepInput::EventArrived {
                        event: ArrivedEvent {
                            name: name.clone(),
                            wake_version: row.wake_version,
                        },
                    },
                ),
            },
        };
        if state.state_version != expected {
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
                kind: JobKind::Event { name: name.into() },
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
